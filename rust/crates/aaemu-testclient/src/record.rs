//! Packet capture: every frame a bot sends or receives, in order, as JSON lines.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Which socket a frame travelled on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Login,
    Game,
    Stream,
}

/// Direction of a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dir {
    C2S,
    S2C,
}

/// One captured frame. `level` is the game frame level (1 = game, 2 = proxy, 0 = login/stream).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub t_ms: u64,
    pub channel: Channel,
    pub dir: Dir,
    pub level: u8,
    pub opcode: u16,
    pub name: String,
    /// Scenario step this frame belongs to (the last [`Recorder::step`] marker).
    pub step: String,
    /// Body after the opcode, hex encoded.
    pub body: String,
}

impl Event {
    pub fn body_bytes(&self) -> Vec<u8> {
        hex::decode(&self.body).unwrap_or_default()
    }
}

#[derive(Debug)]
struct Inner {
    start: Instant,
    step: String,
    events: Vec<Event>,
    enabled: bool,
    live: Option<LiveCapture>,
}

/// Control mode drains this bounded queue to stdout; ordinary recorders still keep a Vec.
const MAX_PENDING_EVENTS: usize = 8192;
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;

struct LiveCapture {
    sink: Box<dyn Write + Send>,
    pending: Vec<Event>,
    pending_bytes: usize,
    failed: bool,
    game_ready: bool,
}

impl std::fmt::Debug for LiveCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveCapture")
            .field("pending", &self.pending.len())
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

fn public_game_event(channel: Channel, dir: Dir, level: u8, opcode: u16) -> bool {
    use aaemu_game::opcodes::{cs, sc};
    // Credential-bearing game transitions in the 1.2 tables: enter-world cookie (C2S),
    // stream token (S2C), and the reconnect token delivered after character selection.
    // Login/stream channels are never public, regardless of session readiness.
    channel == Channel::Game
        && !(level == 1
            && match dir {
                Dir::C2S => opcode == cs::X2EnterWorldPacket,
                Dir::S2C => matches!(
                    opcode,
                    sc::X2EnterWorldResponsePacket | sc::SCReconnectAuthPacket
                ),
            })
}

/// Shared, cheaply cloneable capture buffer.
#[derive(Clone, Debug)]
pub struct Recorder(Arc<Mutex<Inner>>);

impl Default for Recorder {
    fn default() -> Self {
        Self::new(true)
    }
}

impl Recorder {
    pub fn new(enabled: bool) -> Self {
        Self(Arc::new(Mutex::new(Inner {
            start: Instant::now(),
            step: "connect".into(),
            events: Vec::new(),
            enabled,
            live: None,
        })))
    }

    /// Incremental, game-only capture for control mode. Refuses to overwrite evidence.
    /// Login/stream authentication and game cookie/token packets are never retained here.
    /// Unlike `new`, this recorder does not retain the entire run in memory.
    pub fn incremental_game(path: &Path) -> anyhow::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Self::incremental_game_to(std::io::BufWriter::new(file)))
    }

    /// The same bounded incremental capture with a caller-owned sink, e.g. an in-memory
    /// writer for deterministic transport/loop tests. Authentication policy is identical.
    pub fn incremental_game_to(sink: impl Write + Send + 'static) -> Self {
        Self::with_live_sink(Box::new(sink))
    }

    fn with_live_sink(sink: Box<dyn Write + Send>) -> Self {
        let rec = Self::new(true);
        rec.inner().live = Some(LiveCapture {
            sink,
            pending: Vec::new(),
            pending_bytes: 0,
            failed: false,
            game_ready: false,
        });
        rec
    }

    /// Omit the authentication/lobby burst, including any compressed token-bearing frames.
    /// Select/spawn and all subsequent gameplay remain available to the controller.
    pub fn control_game_ready(&self, ready: bool) {
        if let Some(live) = self.inner().live.as_mut() {
            live.game_ready = ready;
        }
    }

    /// Drain independent game events, after checking for capture/queue failures.
    pub fn take_live_events(&self) -> anyhow::Result<Vec<Event>> {
        let mut i = self.inner();
        let live = i
            .live
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("not a live recorder"))?;
        anyhow::ensure!(!live.failed, "incremental capture failed or queue exceeded");
        live.pending_bytes = 0;
        Ok(std::mem::take(&mut live.pending))
    }

    pub fn flush_live(&self) -> anyhow::Result<()> {
        if let Some(live) = self.inner().live.as_mut() {
            anyhow::ensure!(!live.failed, "incremental capture failed or queue exceeded");
            if live.sink.flush().is_err() {
                live.failed = true;
                anyhow::bail!("incremental capture flush failed");
            }
        }
        Ok(())
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn is_incremental(&self) -> bool {
        self.inner().live.is_some()
    }

    /// Starts a new scenario step; later frames are tagged with it.
    pub fn step(&self, name: &str) {
        self.inner().step = name.to_string();
    }

    pub fn current_step(&self) -> String {
        self.inner().step.clone()
    }

    pub fn push(
        &self,
        channel: Channel,
        dir: Dir,
        level: u8,
        opcode: u16,
        name: &str,
        body: &[u8],
    ) {
        if channel == Channel::Game
            && level == crate::game::LEVEL_COMPRESSED
            && self.is_incremental()
        {
            if !self
                .inner()
                .live
                .as_ref()
                .is_some_and(|live| live.game_ready)
            {
                return;
            }
            // The caller supplies the inflated native body, just as an ordinary aggregate
            // capture does. Share GameClient's atomic splitter, then apply the same auth
            // policy to each entry. Never publish an outer batch containing a token.
            match crate::game::split_game_batch(dir, opcode, body) {
                Ok(packets) => {
                    for packet in packets {
                        self.push(
                            channel,
                            dir,
                            packet.level,
                            packet.opcode,
                            crate::game::packet_name(dir, packet.level, packet.opcode),
                            &packet.body,
                        );
                    }
                }
                Err(_) => {
                    if let Some(live) = self.inner().live.as_mut() {
                        live.failed = true;
                    }
                }
            }
            return;
        }
        let mut i = self.inner();
        if !i.enabled {
            return;
        }
        if i.live
            .as_ref()
            .is_some_and(|live| !live.game_ready || !public_game_event(channel, dir, level, opcode))
        {
            return;
        }
        // Unknown levels have no validated public framing. DD04 is handled above.
        if channel == Channel::Game && !matches!(level, 1 | 2) {
            if let Some(live) = i.live.as_mut() {
                live.failed = true;
                return;
            }
        }
        let ev = Event {
            t_ms: i.start.elapsed().as_millis() as u64,
            channel,
            dir,
            level,
            opcode,
            name: name.to_string(),
            step: i.step.clone(),
            body: hex::encode(body),
        };
        if let Some(live) = i.live.as_mut() {
            if live.failed {
                return;
            }
            let result = (|| -> anyhow::Result<()> {
                let mut line = serde_json::to_vec(&ev)?;
                line.push(b'\n');
                anyhow::ensure!(
                    live.pending.len() < MAX_PENDING_EVENTS
                        && line.len() <= MAX_PENDING_BYTES.saturating_sub(live.pending_bytes),
                    "live event queue exceeded"
                );
                live.sink.write_all(&line)?;
                // A hard socket close or process kill must preserve earlier accepted frames.
                live.sink.flush()?;
                live.pending_bytes += line.len();
                live.pending.push(ev);
                Ok(())
            })();
            live.failed = result.is_err();
        } else {
            i.events.push(ev);
        }
    }

    pub fn events(&self) -> Vec<Event> {
        self.inner().events.clone()
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
        for e in self.inner().events.iter() {
            serde_json::to_writer(&mut f, e)?;
            f.write_all(b"\n")?;
        }
        f.flush()?;
        Ok(())
    }
}

/// Reads a capture written by [`Recorder::save`].
pub fn load(path: &Path) -> anyhow::Result<Vec<Event>> {
    let text = std::fs::read_to_string(path)?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| Ok(serde_json::from_str(l)?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaemu_game::opcodes::{cs, sc};

    #[derive(Default)]
    struct Written {
        bytes: Vec<u8>,
        flushes: usize,
        fail_flush: bool,
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Written>>);

    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            let mut written = self.0.lock().unwrap();
            written.flushes += 1;
            if written.fail_flush {
                return Err(std::io::Error::other("injected flush failure"));
            }
            Ok(())
        }
    }

    fn push(rec: &Recorder) {
        rec.push(
            Channel::Game,
            Dir::S2C,
            1,
            sc::SCErrorMsgPacket,
            "SCErrorMsgPacket",
            &[7, 0],
        );
    }

    #[test]
    fn live_capture_flushes_before_drain_and_omits_all_authentication_channels() {
        let sink = Sink::default();
        let rec = Recorder::with_live_sink(Box::new(sink.clone()));
        rec.push(
            Channel::Game,
            Dir::S2C,
            4,
            1,
            "CompressedGamePackets",
            b"lobby-token",
        );
        assert!(sink.0.lock().unwrap().bytes.is_empty());
        rec.control_game_ready(true);
        for (channel, dir, opcode) in [
            (Channel::Login, Dir::C2S, 4),
            (Channel::Stream, Dir::C2S, 1),
            (Channel::Game, Dir::C2S, cs::X2EnterWorldPacket),
            (Channel::Game, Dir::S2C, sc::X2EnterWorldResponsePacket),
            (Channel::Game, Dir::S2C, sc::SCReconnectAuthPacket),
        ] {
            rec.push(channel, dir, 1, opcode, "auth", b"private");
        }
        push(&rec);
        let persisted = sink.0.lock().unwrap();
        assert_eq!(persisted.flushes, 1);
        let event: Event = serde_json::from_slice(&persisted.bytes).unwrap();
        assert_eq!(event.body_bytes(), [7, 0]);
        assert!(!String::from_utf8_lossy(&persisted.bytes).contains(&hex::encode(b"private")));
        drop(persisted);
        assert_eq!(rec.take_live_events().unwrap(), vec![event]);
        assert!(rec.take_live_events().unwrap().is_empty());
        assert!(
            rec.events().is_empty(),
            "control must not retain the whole run"
        );
        rec.control_game_ready(false);
        push(&rec);
        assert_eq!(sink.0.lock().unwrap().flushes, 1);
    }

    #[test]
    fn reconnect_token_after_readiness_is_absent_from_capture_and_stdout_events() {
        let sink = Sink::default();
        let rec = Recorder::with_live_sink(Box::new(sink.clone()));
        rec.control_game_ready(true);
        rec.push(
            Channel::Game,
            Dir::S2C,
            1,
            sc::SCReconnectAuthPacket,
            "SCReconnectAuthPacket",
            &[0x78, 0x56, 0x34, 0x12],
        );
        push(&rec);
        let stdout_events = rec.take_live_events().unwrap();
        assert_eq!(stdout_events.len(), 1);
        assert_eq!(stdout_events[0].opcode, sc::SCErrorMsgPacket);
        let persisted = sink.0.lock().unwrap();
        let capture_events: Vec<Event> = String::from_utf8_lossy(&persisted.bytes)
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(capture_events, stdout_events);
        assert!(!String::from_utf8_lossy(&persisted.bytes).contains("78563412"));
    }

    #[test]
    fn auth_only_post_ready_batch_never_retains_outer_or_inner_secret_bytes() {
        let sink = Sink::default();
        let rec = Recorder::with_live_sink(Box::new(sink.clone()));
        rec.control_game_ready(true);
        push(&rec);
        let before = sink.0.lock().unwrap().bytes.clone();
        // A valid native reconnect-only batch must be filtered, not retained as raw DD04.
        rec.push(
            Channel::Game,
            Dir::S2C,
            4,
            1,
            "CompressedGamePackets",
            &[0, 0, 1, 0, 9, 8, 7, 6],
        );
        assert_eq!(sink.0.lock().unwrap().bytes, before);
        let events = rec.take_live_events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].opcode, sc::SCErrorMsgPacket);
    }

    #[test]
    fn malformed_batch_is_rejected_before_any_partial_event_or_secret_is_published() {
        let sink = Sink::default();
        let rec = Recorder::with_live_sink(Box::new(sink.clone()));
        rec.control_game_ready(true);
        rec.push(
            Channel::Game,
            Dir::S2C,
            4,
            1,
            "CompressedGamePackets",
            &[0, 0, 1, 0, 9, 8, 7, 6, 0xff],
        );
        assert!(sink.0.lock().unwrap().bytes.is_empty());
        assert!(rec.take_live_events().is_err());
    }

    #[test]
    fn queue_or_sink_failure_is_observable_and_never_silently_resumes() {
        let sink = Sink::default();
        let rec = Recorder::with_live_sink(Box::new(sink.clone()));
        rec.control_game_ready(true);
        sink.0.lock().unwrap().fail_flush = true;
        push(&rec);
        assert!(rec.take_live_events().is_err());
        sink.0.lock().unwrap().fail_flush = false;
        assert!(rec.flush_live().is_err());

        let rec = Recorder::with_live_sink(Box::new(Sink::default()));
        rec.control_game_ready(true);
        for _ in 0..=MAX_PENDING_EVENTS {
            push(&rec);
        }
        assert!(rec.take_live_events().is_err());
    }

    #[test]
    fn ordinary_recorders_still_buffer_every_channel() {
        let rec = Recorder::new(true);
        rec.push(Channel::Login, Dir::C2S, 0, 4, "auth", b"ticket");
        rec.push(Channel::Stream, Dir::C2S, 0, 1, "join", b"token");
        push(&rec);
        assert_eq!(rec.events().len(), 3);
        assert_eq!(rec.events()[0].body_bytes(), b"ticket");
    }
}
