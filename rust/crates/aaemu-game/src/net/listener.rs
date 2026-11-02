//! `GameNetwork.Start` + the per-session parts of `GameProtocolHandler`
//! (`OnConnect` / `OnReceive` / `OnDisconnect`).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aaemu_commons::net::FrameCodec;
use bytes::Bytes;
use futures::{Sink, SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::net::connection::{ConnCommand, GameConnection};
use crate::net::outbox::{outbox, OutFrame, OutboxReceiver};
use crate::net::reply_delay::ReplyDelayConn;
use crate::server::GameServer;

/// Frames written to the socket buffer before one flush.
const WRITE_BATCH_FRAMES: usize = 256;
/// A batch the client does not accept within this time means it stopped reading.
const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(20);

/// Binds `Network.Host:Port` (`"*"` = any) and serves connections until the task is dropped.
pub async fn run(server: Arc<GameServer>) -> std::io::Result<()> {
    let cfg = &server.config.network;
    let host = if cfg.host == "*" {
        "0.0.0.0"
    } else {
        cfg.host.as_str()
    };
    let listener = TcpListener::bind((host, cfg.port)).await?;
    tracing::info!("Network started on {}", listener.local_addr()?);
    loop {
        let (socket, peer) = listener.accept().await?;
        let server = server.clone();
        tokio::spawn(async move { serve(server, socket, peer).await });
    }
}

/// Drives one client connection: a reader/dispatcher loop owning the [`GameConnection`] plus a
/// writer task fed by its outbox ([`write_frames`]).
pub async fn serve(server: Arc<GameServer>, socket: TcpStream, peer: SocketAddr) {
    let id = server.next_connection_id();
    tracing::info!("Connect from {} established, session id: {id}", peer.ip());
    let _ = socket.set_nodelay(true);
    let (rd, wr) = socket.into_split();
    let mut reader = FramedRead::new(rd, FrameCodec);

    let (out_tx, out_rx) = outbox();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ConnCommand>();
    let (mut conn, _) = GameConnection::new(id, peer, out_tx, cmd_tx.clone());
    // Before the table entry and any `Player` handle: `Debug:ReplyDelay` (`None` when off).
    conn.set_reply_delay(server.reply_delay.clone());
    let handle = conn.handle();
    // The writer sees the strict latch so a frame that raced the selected one cannot overtake it.
    let delay = conn.reply_delay_state();
    let mut writer = tokio::spawn(write_frames(
        id,
        FramedWrite::new(wr, FrameCodec),
        out_rx,
        cmd_tx,
        delay,
    ));
    server.connections.add(handle.clone());
    // Created before the packet loop: a panic in a packet handler or in `leave_world_task` too
    // must release the session's reservation, remove a half-left world entry and free the
    // account (see `DisconnectCleanup`). It is disarmed once `on_disconnect` returned.
    let mut cleanup = DisconnectCleanup::new(&server, handle);

    while !conn.is_closing() {
        tokio::select! {
            frame = reader.next() => match frame {
                Some(Ok(payload)) => {
                    if let Err(e) = server.handle_frame(&mut conn, &payload).await {
                        // GameProtocolHandler.OnReceive catch-all: connection.Shutdown()
                        tracing::error!("session {id}: {e}");
                        break;
                    }
                }
                Some(Err(e)) => {
                    tracing::debug!("session {id}: read error: {e}");
                    break;
                }
                None => break,
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(ConnCommand::Send(packet)) => conn.send(packet.as_ref()),
                Some(ConnCommand::LeaveWorld(target)) => {
                    let active = conn.active_char.clone();
                    crate::world::session::leave_world_task(&server, Some(&mut conn), target, active).await;
                }
                Some(ConnCommand::ReturnToWorldList) => {
                    crate::world::session::return_to_world_list(&server, &mut conn).await;
                }
                Some(ConnCommand::DetachAndClose) => {
                    conn.active_char = None;
                    break;
                }
                Some(ConnCommand::Close) | None => break,
            },
        }
    }

    on_disconnect(&server, &mut conn).await;
    cleanup.complete();
    drop(cleanup);
    drop(conn); // closes the writer channel; queued frames are flushed first
    if tokio::time::timeout(Duration::from_secs(5), &mut writer)
        .await
        .is_err()
    {
        // The client is not reading: drop what is still queued with the task.
        writer.abort();
    }
    tracing::info!("Client from {} disconnected", peer.ip());
}

/// The writer task: drains the outbox into the socket, up to [`WRITE_BATCH_FRAMES`] frames per
/// flush (one write for many small packets instead of one per packet). A write error or a batch
/// stuck for [`WRITE_STALL_TIMEOUT`] closes the connection.
///
/// Frames with a `release_at` (`Debug:ReplyDelay`) wait in `pending`, in queue order, until their
/// deadline; an immediate frame queued meanwhile is written first. `DelayEverythingAfter` makes
/// every later frame of the connection delayed too, so nothing overtakes a held frame.
async fn write_frames<S>(
    id: u32,
    mut sink: S,
    mut frames: OutboxReceiver,
    commands: mpsc::UnboundedSender<ConnCommand>,
    delay: Option<Arc<ReplyDelayConn>>,
) where
    S: Sink<Bytes> + Unpin + Send + 'static,
    S::Error: std::fmt::Debug,
{
    let mut pending: VecDeque<OutFrame> = VecDeque::new();
    // The outbox is closed once the connection task dropped it; held frames are still flushed.
    let mut closed = false;
    'write: loop {
        // Held frames whose deadline passed, in queue order.
        while pending.front().is_some_and(is_due) {
            let mut batch = Vec::with_capacity(pending.len().min(WRITE_BATCH_FRAMES));
            while pending.front().is_some_and(is_due) && batch.len() < WRITE_BATCH_FRAMES {
                batch.push(pending.pop_front().expect("front checked").bytes);
            }
            if !write_batch(id, &mut sink, batch).await {
                break 'write;
            }
        }

        let frame = match pending.front() {
            Some(front) => {
                let deadline = front.release_at.expect("pending frames are held");
                if closed {
                    tokio::time::sleep_until(deadline).await;
                    continue 'write;
                }
                tokio::select! {
                    frame = frames.recv() => frame,
                    _ = tokio::time::sleep_until(deadline) => continue 'write,
                }
            }
            None if closed => break,
            None => frames.recv().await,
        };
        let Some(frame) = frame else {
            closed = true;
            continue;
        };

        if frame.release_at.is_some() {
            pending.push_back(frame);
            continue;
        }

        // Strict mode race: a frame queued by another thread at the same moment as the selected
        // one can carry no deadline. Behind held frames it must wait, not overtake them.
        if !pending.is_empty() {
            if let Some(delay) = &delay {
                if delay.strict() {
                    let release_at = tokio::time::Instant::now() + delay.delay();
                    pending.push_back(OutFrame::delayed(frame.bytes, release_at));
                    continue;
                }
            }
        }

        // An immediate frame is not held; batch the immediate frames queued behind it and keep
        // the first held one for its deadline. The common (switch off) path feeds straight into
        // the sink, as before.
        let write = async {
            sink.feed(frame.bytes).await?;
            let mut written = 1;
            while written < WRITE_BATCH_FRAMES {
                match frames.try_recv_frame() {
                    Ok(next) if next.release_at.is_none() => {
                        sink.feed(next.bytes).await?;
                        written += 1;
                    }
                    Ok(next) => {
                        pending.push_back(next);
                        break;
                    }
                    Err(_) => break,
                }
            }
            SinkExt::<Bytes>::flush(&mut sink).await
        };
        if !write_or_close(id, write).await {
            break;
        }
    }
    let _ = commands.send(ConnCommand::Close);
}

/// `true` once a held frame's deadline has passed (`tokio::time` so paused tests can drive it).
fn is_due(frame: &OutFrame) -> bool {
    frame
        .release_at
        .is_some_and(|at| at <= tokio::time::Instant::now())
}

/// Feeds one batch of held frames and flushes it; `false` closes the connection.
async fn write_batch<S>(id: u32, sink: &mut S, batch: Vec<Bytes>) -> bool
where
    S: Sink<Bytes> + Unpin,
    S::Error: std::fmt::Debug,
{
    let write = async {
        for frame in batch {
            sink.feed(frame).await?;
        }
        SinkExt::<Bytes>::flush(sink).await
    };
    write_or_close(id, write).await
}

/// Runs one write with [`WRITE_STALL_TIMEOUT`]; `false` closes the connection (write error or a
/// client that accepts no data).
async fn write_or_close<F, E>(id: u32, write: F) -> bool
where
    F: std::future::Future<Output = Result<(), E>>,
    E: std::fmt::Debug,
{
    match tokio::time::timeout(WRITE_STALL_TIMEOUT, write).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::debug!("write to session {id} failed: {e:?}");
            false
        }
        Err(_) => {
            tracing::warn!(
                "session {id}: client accepted no data for {} s, closing the connection",
                WRITE_STALL_TIMEOUT.as_secs()
            );
            false
        }
    }
}

/// `GameProtocolHandler.OnDisconnect` + `GameConnection.OnDisconnect`.
pub async fn on_disconnect(server: &Arc<GameServer>, conn: &mut GameConnection) {
    let account_id = conn.account_id();
    // The departure save runs before the account is freed: meanwhile `EnterWorldManager`
    // refuses a second login of the account, so a reconnect cannot load the character while its
    // previous session is still writing it. Deviation from the C# (`Remove` first), see
    // PORTING.md.
    crate::world::session::on_disconnect(server, conn).await;
    server.accounts.remove_if_owned(account_id, conn.id);
    server.streams.remove_token(conn.id);
    server.connections.remove(conn.id);
    // AccountManager.UpdateLoginTime(AccountId, UtcNow), fire and forget. Deviation: C# also runs
    // it for connections that never entered (AccountId 0), which inserts an `accounts` row with
    // id 0 via GetAccountDetailsInternal; that is skipped here.
    if account_id != 0 {
        let server = server.clone();
        tokio::spawn(async move {
            crate::db::update_login_time(
                &server.db,
                &server.config,
                account_id,
                chrono::Utc::now().naive_utc(),
            )
            .await;
        });
    }
}

/// Best-effort cleanup after the connection task ends, including a panic anywhere in its packet
/// loop or in [`on_disconnect`] (Server-Backlog: "Disconnect cleanup: no unwind guard if cleanup
/// panics"). It is disarmed by [`DisconnectCleanup::complete`] once the normal disconnect finished;
/// every step is then idempotent anyway. A panic in the reservation release or in the world
/// removal must not abort the unwind, and the reservation is released before the world part runs.
struct DisconnectCleanup {
    server: Arc<GameServer>,
    /// Live account id: `X2EnterWorld` sets it after the connection was accepted.
    connection: crate::net::connection::ConnectionHandle,
    completed: bool,
}

impl DisconnectCleanup {
    fn new(server: &Arc<GameServer>, connection: crate::net::connection::ConnectionHandle) -> Self {
        Self {
            server: server.clone(),
            connection,
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for DisconnectCleanup {
    fn drop(&mut self) {
        if !self.completed {
            let world_cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::world::session::force_release_connection(&self.server, self.connection.id);
            }));
            if world_cleanup.is_err() {
                tracing::error!(
                    "session {}: the fallback world cleanup panicked; the reservation was released first",
                    self.connection.id
                );
            }
        }
        self.server
            .accounts
            .remove_if_owned(self.connection.account_id(), self.connection.id);
        self.server.streams.remove_token(self.connection.id);
        self.server.connections.remove(self.connection.id);
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use std::time::Duration;

    use bytes::Bytes;
    use futures::Sink;
    use tokio::sync::mpsc;

    use crate::config::ReplyDelayConfig;
    use crate::net::outbox::{outbox, OutFrame};
    use crate::net::packet::encode_raw;
    use crate::net::reply_delay::{ReplyDelay, ReplyDelayConn};
    use crate::opcodes::{cs, sc};
    use crate::test_support::{spawn_player, test_character, test_server, TestClient};

    /// A sink that records each frame with the (possibly paused) time it was written.
    #[derive(Clone, Default)]
    struct RecordingSink {
        frames: Arc<Mutex<Vec<Bytes>>>,
        at: Arc<Mutex<Vec<tokio::time::Instant>>>,
    }

    impl RecordingSink {
        fn frames(&self) -> Vec<Vec<u8>> {
            self.frames
                .lock()
                .unwrap()
                .iter()
                .map(|f| f.to_vec())
                .collect()
        }

        fn times(&self) -> Vec<tokio::time::Instant> {
            self.at.lock().unwrap().clone()
        }
    }

    impl Sink<Bytes> for RecordingSink {
        type Error = std::io::Error;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
            self.frames.lock().unwrap().push(item);
            self.at.lock().unwrap().push(tokio::time::Instant::now());
            Ok(())
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    fn reply_delay(
        packets: &[&str],
        replies_to: &[&str],
        delay_everything_after: bool,
    ) -> Arc<ReplyDelay> {
        ReplyDelay::resolve(&ReplyDelayConfig {
            delay_ms: 600,
            packets: packets.iter().map(|s| (*s).to_string()).collect(),
            replies_to: replies_to.iter().map(|s| (*s).to_string()).collect(),
            character_name: None,
            delay_everything_after,
        })
        .expect("a selector resolves")
    }

    fn frame(opcode: u16) -> Bytes {
        Bytes::from(encode_raw(1, opcode, &[]))
    }

    /// Off: no deadline, no timer; the frames go out in queue order as before.
    #[tokio::test(start_paused = true)]
    async fn undelayed_frames_keep_their_order_and_are_written_without_waiting() {
        let (tx, rx) = outbox();
        let (commands, _commands_rx) = mpsc::unbounded_channel();
        let sink = RecordingSink::default();
        let start = tokio::time::Instant::now();
        let writer = tokio::spawn(super::write_frames(1, sink.clone(), rx, commands, None));

        for byte in [b"a".as_slice(), b"b", b"c"] {
            tx.push(OutFrame::now(Bytes::copy_from_slice(byte)));
        }
        drop(tx);
        writer.await.unwrap();

        assert_eq!(sink.frames(), [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
        assert!(sink.times().iter().all(|at| *at == start), "no time passed");
    }

    /// On: a held frame is written at its deadline, in queue order, and not before.
    #[tokio::test(start_paused = true)]
    async fn held_frames_arrive_after_the_delay_in_order() {
        let (tx, rx) = outbox();
        let (commands, _commands_rx) = mpsc::unbounded_channel();
        let sink = RecordingSink::default();
        let release = tokio::time::Instant::now() + Duration::from_millis(600);
        let writer = tokio::spawn(super::write_frames(1, sink.clone(), rx, commands, None));

        for byte in [b"a".as_slice(), b"b", b"c"] {
            tx.push(OutFrame::delayed(Bytes::copy_from_slice(byte), release));
        }
        drop(tx);
        tokio::task::yield_now().await;
        assert!(sink.frames().is_empty(), "nothing before the delay");
        tokio::time::advance(Duration::from_millis(599)).await;
        tokio::task::yield_now().await;
        assert!(sink.frames().is_empty(), "still nothing 1 ms before");
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;

        assert_eq!(sink.frames(), [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
        assert!(sink.times().iter().all(|at| *at >= release));
        writer.await.unwrap();
    }

    /// The connection state selects frames by S->C name and by the C->S packet being handled.
    #[tokio::test(start_paused = true)]
    async fn selection_and_strict_mode_hold_the_full_order() {
        let delay =
            ReplyDelayConn::new(1, reply_delay(&["SCSkillStarted"], &["CSStartSkill"], true));
        let started = delay.frame(frame(sc::SCSkillStartedPacket));
        let strict_after = delay.frame(frame(sc::SCErrorMsgPacket));
        assert!(started.release_at.is_some());
        assert!(strict_after.release_at.is_some());
        assert!(strict_after.release_at >= started.release_at);

        // "Replies to CSStartSkill": only while that handler runs.
        let reply = ReplyDelayConn::new(1, reply_delay(&[], &["CSStartSkill"], false));
        assert!(reply
            .frame(frame(sc::SCSkillFiredPacket))
            .release_at
            .is_none());
        crate::net::reply_delay::with_reply_context(1, 1, cs::CSStartSkillPacket, async {
            assert!(reply
                .frame(frame(sc::SCSkillFiredPacket))
                .release_at
                .is_some());
        })
        .await;
        assert!(reply
            .frame(frame(sc::SCSkillFiredPacket))
            .release_at
            .is_none());
    }

    /// Strict mode race: a frame queued with no deadline while a held one waits must go behind it.
    #[tokio::test(start_paused = true)]
    async fn strict_mode_queues_a_racing_immediate_frame_behind_the_held_one() {
        let (tx, rx) = outbox();
        let (commands, _commands_rx) = mpsc::unbounded_channel();
        let sink = RecordingSink::default();
        let delay = ReplyDelayConn::new(1, reply_delay(&["SCSkillStarted"], &[], true));
        let writer = tokio::spawn(super::write_frames(
            1,
            sink.clone(),
            rx,
            commands,
            Some(delay.clone()),
        ));

        // The selected frame latches strict mode and is held; the second frame raced it and has
        // no deadline (another thread queued it without seeing the latch).
        tx.push(delay.frame(frame(sc::SCSkillStartedPacket)));
        tx.push(OutFrame::now(frame(sc::SCErrorMsgPacket)));
        drop(tx);
        tokio::task::yield_now().await;
        assert!(sink.frames().is_empty(), "nothing before the delay");
        tokio::time::advance(Duration::from_millis(600)).await;
        tokio::task::yield_now().await;
        writer.await.unwrap();

        let opcodes: Vec<u16> = sink
            .frames()
            .iter()
            .map(|f| u16::from_le_bytes([f[4], f[5]]))
            .collect();
        assert_eq!(opcodes, [sc::SCSkillStartedPacket, sc::SCErrorMsgPacket]);
        assert_eq!(
            sink.times()[0],
            sink.times()[1],
            "both written when the held frame was due, in order"
        );
    }

    /// Strict mode through the writer: the unselected frame after the held one cannot overtake it.
    #[tokio::test(start_paused = true)]
    async fn strict_mode_keeps_the_full_order_through_the_writer() {
        let (tx, rx) = outbox();
        let (commands, _commands_rx) = mpsc::unbounded_channel();
        let sink = RecordingSink::default();
        let delay = ReplyDelayConn::new(1, reply_delay(&["SCSkillStarted"], &[], true));
        let writer = tokio::spawn(super::write_frames(
            1,
            sink.clone(),
            rx,
            commands,
            Some(delay.clone()),
        ));

        tx.push(delay.frame(frame(sc::SCErrorMsgPacket)));
        tx.push(delay.frame(frame(sc::SCSkillStartedPacket)));
        tx.push(delay.frame(frame(sc::SCErrorMsgPacket)));
        drop(tx);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(600)).await;
        tokio::task::yield_now().await;
        writer.await.unwrap();

        let opcodes: Vec<u16> = sink
            .frames()
            .iter()
            .map(|f| u16::from_le_bytes([f[4], f[5]]))
            .collect();
        assert_eq!(
            opcodes,
            [
                sc::SCErrorMsgPacket,
                sc::SCSkillStartedPacket,
                sc::SCErrorMsgPacket
            ]
        );
        // The first frame was immediate, the selected one and everything after it held.
        assert!(sink.times()[0] < sink.times()[1]);
        assert_eq!(sink.times()[1], sink.times()[2]);
    }

    /// A panic in the disconnect cleanup must not leave the account registered, the character's
    /// world entry behind or its session reservation held. The guard is dropped while the panic
    /// unwinds, exactly like `serve` drops it after `on_disconnect`.
    #[tokio::test]
    async fn a_panicking_disconnect_cleanup_still_releases_the_session() {
        let server = Arc::new(test_server());
        let client = TestClient::new(&server, 1, 1000);
        let player = spawn_player(
            &server,
            &client,
            test_character(7, "Ghost", 100.0, 100.0),
            0x700,
        );
        assert!(server.accounts.try_add(1000, 1));
        assert!(server.enter_world.reserve_character(7, 1).is_ok());

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _cleanup = super::DisconnectCleanup::new(&server, client.conn.handle());
            panic!("disconnect cleanup panicked");
        }));
        assert!(result.is_err());
        assert_eq!(server.enter_world.character_owner(7), None);
        assert!(server.world.character_by_id(7).is_none());
        assert!(!server.accounts.contains(1000));
        assert!(server.connections.get(1).is_none());
        assert_eq!(server.streams.account_for(1), None);
        drop(player);
    }
}
