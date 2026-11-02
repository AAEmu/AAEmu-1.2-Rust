//! The outgoing frame queue of a game connection: encoded frame payloads waiting for the
//! connection's writer task.
//!
//! Frames are [`Bytes`], so one broadcast is encoded once and the same buffer is queued for
//! every recipient. The queue tracks how many bytes wait for the socket; a client that stops
//! reading would otherwise grow it without bound, so past [`MAX_QUEUED_BYTES`] new frames are
//! refused and the sender closes the connection.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// Queued bytes after which a connection is treated as not reading and closed.
pub const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;

/// One encoded frame waiting for the connection's writer.
#[derive(Debug, Clone)]
pub struct OutFrame {
    pub bytes: Bytes,
    /// `Debug:ReplyDelay` holds the frame until this instant; `None` = write as soon as possible.
    pub release_at: Option<Instant>,
}

impl OutFrame {
    pub fn now(bytes: Bytes) -> Self {
        Self {
            bytes,
            release_at: None,
        }
    }

    pub fn delayed(bytes: Bytes, release_at: Instant) -> Self {
        Self {
            bytes,
            release_at: Some(release_at),
        }
    }
}

/// Result of [`Outbox::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    Queued,
    /// The writer is gone (connection closed).
    Closed,
    /// The queue is over [`MAX_QUEUED_BYTES`]; `first` is set for the first refused frame only,
    /// so the caller closes the connection once.
    Overflow {
        first: bool,
    },
}

#[derive(Debug, Default)]
struct Counters {
    queued: AtomicUsize,
    overflowed: AtomicBool,
}

/// Sending half (cloned into every handle of the connection).
#[derive(Debug, Clone)]
pub struct Outbox {
    tx: mpsc::UnboundedSender<OutFrame>,
    counters: Arc<Counters>,
}

/// Receiving half, owned by the writer task.
#[derive(Debug)]
pub struct OutboxReceiver {
    rx: mpsc::UnboundedReceiver<OutFrame>,
    counters: Arc<Counters>,
}

/// A new connection queue.
pub fn outbox() -> (Outbox, OutboxReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    let counters = Arc::new(Counters::default());
    (
        Outbox {
            tx,
            counters: counters.clone(),
        },
        OutboxReceiver { rx, counters },
    )
}

impl Outbox {
    pub fn push(&self, frame: OutFrame) -> Push {
        let len = frame.bytes.len();
        let queued = self.counters.queued.fetch_add(len, Ordering::Relaxed) + len;
        if queued > MAX_QUEUED_BYTES {
            self.counters.queued.fetch_sub(len, Ordering::Relaxed);
            let first = !self.counters.overflowed.swap(true, Ordering::Relaxed);
            return Push::Overflow { first };
        }
        if self.tx.send(frame).is_err() {
            self.counters.queued.fetch_sub(len, Ordering::Relaxed);
            return Push::Closed;
        }
        Push::Queued
    }

    /// Bytes waiting for the writer.
    pub fn queued_bytes(&self) -> usize {
        self.counters.queued.load(Ordering::Relaxed)
    }
}

impl OutboxReceiver {
    pub async fn recv(&mut self) -> Option<OutFrame> {
        let frame = self.rx.recv().await?;
        self.counters
            .queued
            .fetch_sub(frame.bytes.len(), Ordering::Relaxed);
        Some(frame)
    }

    /// The next frame's bytes, ignoring a reply-delay deadline (tests inspect what was queued).
    pub fn try_recv(&mut self) -> Result<Bytes, mpsc::error::TryRecvError> {
        self.try_recv_frame().map(|frame| frame.bytes)
    }

    pub fn try_recv_frame(&mut self) -> Result<OutFrame, mpsc::error::TryRecvError> {
        let frame = self.rx.try_recv()?;
        self.counters
            .queued
            .fetch_sub(frame.bytes.len(), Ordering::Relaxed);
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now(bytes: &'static [u8]) -> OutFrame {
        OutFrame::now(Bytes::from_static(bytes))
    }

    #[test]
    fn counts_queued_bytes_and_refuses_past_the_limit() {
        let (tx, mut rx) = outbox();
        assert_eq!(tx.push(now(b"abc")), Push::Queued);
        assert_eq!(tx.queued_bytes(), 3);
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"abc"));
        assert_eq!(tx.queued_bytes(), 0);

        let big = OutFrame::now(Bytes::from(vec![0u8; MAX_QUEUED_BYTES]));
        assert_eq!(tx.push(big), Push::Queued);
        assert_eq!(tx.push(now(b"x")), Push::Overflow { first: true });
        assert_eq!(tx.push(now(b"y")), Push::Overflow { first: false });
        assert_eq!(tx.queued_bytes(), MAX_QUEUED_BYTES);
        assert_eq!(rx.try_recv().unwrap().len(), MAX_QUEUED_BYTES);
        assert_eq!(tx.queued_bytes(), 0);
    }

    #[test]
    fn closed_writer_reports_closed() {
        let (tx, rx) = outbox();
        drop(rx);
        assert_eq!(tx.push(now(b"abc")), Push::Closed);
        assert_eq!(tx.queued_bytes(), 0);
    }
}
