//! Bounded stdio controller, using the same session and packet paths as scenarios.
//!
//! First send `{"protocol":"aaemu-control/1","id":1,"method":"hello","params":{}}`.
//! Its response is the ready boundary. Every subsequent request has the same envelope and
//! a unique string (1..64 bytes) or unsigned integer id. All params objects are strict.
//! Stdout contains only versioned responses, independent game events and lifecycle records;
//! tracing goes to stderr. Events preceding a response have `seq <= response.event_seq`.
//! This boundary orders observations; it does not imply that a server action has settled.

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use aaemu_game::opcodes::cs;
use aaemu_testclient::record::Recorder;
use aaemu_testclient::scenario::{self, BotConfig, ControlSession};
use anyhow::{bail, ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

const PROTOCOL: &str = "aaemu-control/1";
const MAX_LINE: usize = 16 * 1024;
const MAX_BODY: usize = 4096;
const MAX_GM: usize = 1024;
const MAX_COMMANDS: usize = 4096;
const MAX_PUMP_MS: u64 = 5000;
const MAX_DEADLINE_MS: u64 = 600_000;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
// Included in (never added to) command/connect/global ceilings. Socket cleanup and one
// completion retain this slice even when the gameplay action never receives an answer.
const REPORT_RESERVE: Duration = Duration::from_millis(250);
const INPUT_QUEUE: usize = 16;
const OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const OUTPUT_COUNT: usize = 8192;

pub(crate) fn describe() -> Value {
    json!({
        "protocol": PROTOCOL,
        "methods": ["hello", "pump", "gm", "tp", "rawcs", "state",
                    "disconnect_hard", "reconnect", "close"],
        "request": {"protocol": PROTOCOL, "id": 1, "method": "hello", "params": {}},
        "params": {
            "hello": {}, "pump": {"ms": "u64 <= 5000"},
            "gm": {"command": "GM command without leading slash"},
            "tp": {"x": "finite f32", "y": "finite f32", "z": "finite f32"},
            "rawcs": {"opcode": "literal u16", "body_hex": "contiguous hex"},
            "state": {}, "disconnect_hard": {},
            "reconnect": {"character_id": "previous ready character_id (u32)"}, "close": {}
        },
        "limits": {
            "line_bytes": MAX_LINE, "body_bytes": MAX_BODY, "gm_bytes": MAX_GM,
            "id_bytes": 64, "commands": MAX_COMMANDS, "pump_ms": MAX_PUMP_MS,
            "command_ms": COMMAND_TIMEOUT.as_millis(),
            "connect_ms": CONNECT_TIMEOUT.as_millis(), "deadline_ms": MAX_DEADLINE_MS,
            "cleanup_report_ms": REPORT_RESERVE.as_millis(),
            "event_queue_bytes": 8 * 1024 * 1024, "event_queue_count": 8192,
            "input_queue_count": INPUT_QUEUE, "output_queue_bytes": OUTPUT_BYTES,
            "output_queue_count": OUTPUT_COUNT
        },
        "incremental_game_capture": true,
        "capture_create_new": true,
        "auth_frames_omitted": true,
        "opaque_game_batches": "unsupported structures rejected; supported native DD04 expanded",
        "game_batches": "validated native DD04 expanded into ordinary level-1 events; auth entries omitted; unknown/nested structures rejected",
        "batch_limits": {"frame_bytes": aaemu_testclient::game::MAX_GAME_FRAME,
            "inflated_bytes": aaemu_testclient::game::MAX_BATCH_BYTES,
            "packets": aaemu_testclient::game::MAX_BATCH_PACKETS},
        "eof": "cancels active action and discards queued commands",
        "capture_start": "post-lobby character select/spawn (each session)",
        "handshake_before_network": true,
        "initial_character": "existing configured name, never first/create",
        "state_source": "client_observation",
        "automatic_retry": false,
        "command_timeout": "one command_deadline response after socket closure; control remains disconnected for state, explicit same-character reconnect, or close",
        "hard_close": "local game write/read halves and stream; no CSLeaveWorld; FIN/RST, not logout",
        "completion": "one response per input line; event_seq is an observation boundary",
        "cleanup": "close, EOF, input/output failure or deadline closes only this session"
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(untagged)]
enum RequestId {
    Number(u64),
    Text(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    protocol: String,
    id: RequestId,
    method: String,
    #[serde(deserialize_with = "unique_params")]
    params: Value,
}

fn unique_params<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
    struct Params;
    impl<'de> serde::de::Visitor<'de> for Params {
        type Value = Value;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a params object with unique keys")
        }

        fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<Value, M::Error> {
            let mut values = serde_json::Map::new();
            while let Some(key) = map.next_key::<String>()? {
                if values.contains_key(&key) {
                    return Err(serde::de::Error::custom("duplicate param"));
                }
                values.insert(key, map.next_value()?);
            }
            Ok(Value::Object(values))
        }
    }
    deserializer.deserialize_map(Params)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pump {
    ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Gm {
    command: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tp {
    x: f32,
    y: f32,
    z: f32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCs {
    opcode: u16,
    body_hex: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reconnect {
    character_id: u32,
}

enum Command {
    Hello,
    Pump(Pump),
    Gm(Gm),
    Tp(Tp),
    RawCs(u16, Vec<u8>),
    State,
    DisconnectHard,
    Reconnect(u32),
    Close,
}

struct Rejected {
    id: Option<RequestId>,
    code: &'static str,
    fatal: bool,
}

fn command(request: &Request) -> anyhow::Result<Command> {
    let params = request.params.clone();
    Ok(match request.method.as_str() {
        "hello" | "state" | "disconnect_hard" | "close" => {
            serde_json::from_value::<Empty>(params)?;
            match request.method.as_str() {
                "hello" => Command::Hello,
                "state" => Command::State,
                "disconnect_hard" => Command::DisconnectHard,
                _ => Command::Close,
            }
        }
        "pump" => {
            let p: Pump = serde_json::from_value(params)?;
            ensure!(p.ms <= MAX_PUMP_MS, "pump limit");
            Command::Pump(p)
        }
        "gm" => {
            let p: Gm = serde_json::from_value(params)?;
            ensure!(
                !p.command.trim().is_empty()
                    && p.command.len() <= MAX_GM
                    && !p.command.starts_with('/')
                    && !p.command.chars().any(char::is_control),
                "GM command limit/format"
            );
            Command::Gm(p)
        }
        "tp" => {
            let p: Tp = serde_json::from_value(params)?;
            ensure!(
                p.x.is_finite() && p.y.is_finite() && p.z.is_finite(),
                "position"
            );
            Command::Tp(p)
        }
        "rawcs" => {
            let p: RawCs = serde_json::from_value(params)?;
            ensure!(p.body_hex.len() <= MAX_BODY * 2, "raw body limit");
            // Authentication belongs exclusively to the shared session implementation.
            ensure!(p.opcode != cs::X2EnterWorldPacket, "authentication packet");
            Command::RawCs(p.opcode, hex::decode(p.body_hex)?)
        }
        "reconnect" => {
            let p: Reconnect = serde_json::from_value(params)?;
            ensure!(p.character_id != 0, "character id 0");
            Command::Reconnect(p.character_id)
        }
        _ => bail!("unknown method"),
    })
}

/// Errors deliberately contain no request body, config value, credential or packet bytes.
fn parse(line: &[u8]) -> Result<(RequestId, Command), Rejected> {
    if line.len() > MAX_LINE {
        return Err(Rejected {
            id: None,
            code: "line_limit",
            fatal: true,
        });
    }
    let r: Request = serde_json::from_slice(line).map_err(|_| Rejected {
        id: None,
        code: "invalid_request",
        fatal: false,
    })?;
    let reject = |code, fatal| Rejected {
        id: Some(r.id.clone()),
        code,
        fatal,
    };
    if r.protocol != PROTOCOL {
        return Err(reject("protocol_mismatch", true));
    }
    if matches!(&r.id, RequestId::Text(s) if s.is_empty() || s.len() > 64) {
        return Err(Rejected {
            id: None,
            code: "invalid_id",
            fatal: false,
        });
    }
    let cmd = command(&r).map_err(|_| reject("invalid_method_or_params", false))?;
    Ok((r.id, cmd))
}

/// Bounded even if the sender never terminates an oversized line. A partial final line is
/// rejected, so EOF cannot turn a truncated gameplay request into a command.
fn read_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, &'static str> {
    let mut line = Vec::new();
    loop {
        let bytes = reader.fill_buf().map_err(|_| "input_io")?;
        if bytes.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err("truncated_line")
            };
        }
        let newline = bytes.iter().position(|&b| b == b'\n');
        let len = newline.unwrap_or(bytes.len());
        if len > MAX_LINE.saturating_sub(line.len()) {
            return Err("line_limit");
        }
        line.extend_from_slice(&bytes[..len]);
        reader.consume(len + usize::from(newline.is_some()));
        if newline.is_some() {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

/// Terminal notification cannot sit behind queued gameplay. In particular EOF cancels
/// the active action and invalidates every queued line before another action is polled.
struct Input {
    lines: mpsc::Receiver<Vec<u8>>,
    terminal: watch::Receiver<Option<&'static str>>,
}

fn read_input(
    mut reader: impl BufRead,
    lines: mpsc::Sender<Vec<u8>>,
    terminal: watch::Sender<Option<&'static str>>,
) {
    let reason = loop {
        match read_line(&mut reader) {
            Ok(Some(line)) => match lines.try_send(line) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => break "input_queue_limit",
                Err(mpsc::error::TrySendError::Closed(_)) => return,
            },
            Ok(None) => break "eof",
            Err(code) => break code,
        }
    };
    terminal.send_replace(Some(reason));
}

async fn termination(terminal: &mut watch::Receiver<Option<&'static str>>) -> &'static str {
    loop {
        let reason = *terminal.borrow_and_update();
        if let Some(reason) = reason {
            return reason;
        }
        if terminal.changed().await.is_err() {
            return "eof";
        }
    }
}

/// Timer futures provide wakeups, but an already-expired timer can initially poll Pending.
/// Check synchronously before polling gameplay or consuming another command. EOF retains
/// priority, including a closed input worker that has not published a terminal value.
fn check_stop(
    terminal: &watch::Receiver<Option<&'static str>>,
    until: Instant,
    output_until: Option<Instant>,
    deadline_reason: &'static str,
) -> anyhow::Result<Option<&'static str>> {
    if let Some(reason) = *terminal.borrow() {
        return Ok(Some(reason));
    }
    if terminal.has_changed().is_err() {
        return Ok(Some("eof"));
    }
    let now = Instant::now();
    if now >= until {
        return Ok(Some(deadline_reason));
    }
    if let Some(until) = output_until {
        ensure!(now < until, "stdout budget exhausted");
    }
    Ok(None)
}

struct Output {
    bytes: Vec<u8>,
    ack: oneshot::Sender<std::io::Result<()>>,
}

/// Detached, bounded stdio workers avoid Tokio's uncancellable blocking stdin read keeping
/// the runtime alive after deadline. They own no sockets and are never joined on shutdown.
fn stdio() -> anyhow::Result<(Input, mpsc::Sender<Output>)> {
    let (input_tx, input_rx) = mpsc::channel(INPUT_QUEUE);
    let (terminal_tx, terminal_rx) = watch::channel(None);
    std::thread::Builder::new()
        .name("control-input".into())
        .spawn(move || {
            read_input(std::io::stdin().lock(), input_tx, terminal_tx);
        })?;
    let (output_tx, mut output_rx) = mpsc::channel::<Output>(1);
    // Use an owned OS handle rather than holding Rust's global stdout lock. A blocked
    // writer must not prevent runtime/process cleanup after its acknowledgement times out.
    #[cfg(windows)]
    let mut stdout = {
        use std::os::windows::io::AsHandle;
        std::fs::File::from(std::io::stdout().as_handle().try_clone_to_owned()?)
    };
    #[cfg(unix)]
    let mut stdout = {
        use std::os::fd::AsFd;
        std::fs::File::from(std::io::stdout().as_fd().try_clone_to_owned()?)
    };
    #[cfg(not(any(windows, unix)))]
    let mut stdout = std::io::stdout();
    std::thread::Builder::new()
        .name("control-output".into())
        .spawn(move || {
            while let Some(out) = output_rx.blocking_recv() {
                let result = stdout.write_all(&out.bytes).and_then(|()| stdout.flush());
                let failed = result.is_err();
                let _ = out.ack.send(result);
                if failed {
                    break;
                }
            }
        })?;
    Ok((
        Input {
            lines: input_rx,
            terminal: terminal_rx,
        },
        output_tx,
    ))
}

struct QueuedOutput {
    bytes: Vec<u8>,
    until: Instant,
}

struct InFlight {
    ack: oneshot::Receiver<std::io::Result<()>>,
    until: Instant,
    len: usize,
}

struct Reporter {
    output: mpsc::Sender<Output>,
    deadline: Instant,
    budget: Instant,
    seq: u64,
    queue: VecDeque<QueuedOutput>,
    queued_bytes: usize,
    flight: Option<InFlight>,
}

impl Reporter {
    fn new(output: mpsc::Sender<Output>, deadline: Instant) -> Self {
        Self {
            output,
            deadline,
            budget: deadline,
            seq: 0,
            queue: VecDeque::new(),
            queued_bytes: 0,
            flight: None,
        }
    }

    fn write(&mut self, mut value: Value) -> anyhow::Result<()> {
        value["protocol"] = PROTOCOL.into();
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        ensure!(
            self.queue.len() + usize::from(self.flight.is_some()) < OUTPUT_COUNT
                && bytes.len() <= OUTPUT_BYTES.saturating_sub(self.queued_bytes),
            "stdout queue limit"
        );
        self.queued_bytes += bytes.len();
        self.queue.push_back(QueuedOutput {
            bytes,
            until: self.budget,
        });
        Ok(())
    }

    fn events(&mut self, rec: &Recorder) -> anyhow::Result<()> {
        for event in rec.take_live_events()? {
            self.seq += 1;
            self.write(json!({"type": "event", "seq": self.seq, "event": event}))?;
        }
        Ok(())
    }

    fn response(
        &mut self,
        id: Option<RequestId>,
        result: Result<Value, &str>,
    ) -> anyhow::Result<()> {
        let value = match result {
            Ok(value) => json!({"type": "response", "id": id, "ok": true,
                               "result": value, "event_seq": self.seq}),
            Err(code) => json!({"type": "response", "id": id, "ok": false,
                              "error": {"code": code}, "event_seq": self.seq}),
        };
        self.write(value)
    }

    fn has_work(&self) -> bool {
        self.flight.is_some() || !self.queue.is_empty()
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.queue
            .iter()
            .map(|queued| queued.until)
            .chain(self.flight.as_ref().map(|flight| flight.until))
            .min()
    }

    /// One cancellation-safe output step, polled alongside action/EOF/deadline futures.
    /// The ack and queued bytes live on Reporter, so losing a select race never loses them.
    /// Every frame retains the shared action budget at enqueue time; acknowledgments cannot
    /// restart a command's deadline. A one-second stalled-writer ceiling is additional.
    async fn progress(&mut self) -> anyhow::Result<()> {
        if let Some(until) = self.next_deadline() {
            ensure!(Instant::now() < until, "stdout budget exhausted");
        }
        if self.flight.is_none() {
            let Some(front) = self.queue.front() else {
                return std::future::pending().await;
            };
            let until = self
                .next_deadline()
                .unwrap_or(front.until)
                .min(Instant::now() + Duration::from_secs(1));
            ensure!(Instant::now() < until, "stdout budget exhausted");
            let permit = tokio::time::timeout_at(until, self.output.reserve())
                .await
                .context("stdout budget")?
                .context("stdout worker closed")?;
            ensure!(Instant::now() < until, "stdout budget exhausted");
            let queued = self.queue.pop_front().context("stdout queue empty")?;
            let len = queued.bytes.len();
            let (ack, completed) = oneshot::channel();
            permit.send(Output {
                bytes: queued.bytes,
                ack,
            });
            self.flight = Some(InFlight {
                ack: completed,
                until,
                len,
            });
            // Starting a write can introduce an earlier stalled-writer ceiling. Return
            // control so drive/serve recompute it before any further action poll.
            return Ok(());
        }
        let flight = self.flight.as_mut().context("stdout ack missing")?;
        ensure!(Instant::now() < flight.until, "stdout budget exhausted");
        tokio::time::timeout_at(flight.until, &mut flight.ack)
            .await
            .context("stdout budget")?
            .context("stdout worker stopped")??;
        ensure!(Instant::now() < flight.until, "stdout budget exhausted");
        let flight = self.flight.take().context("stdout ack missing")?;
        self.queued_bytes -= flight.len;
        Ok(())
    }

    /// Called only after sockets are closed. Earlier command budgets remain in force.
    async fn finish(&mut self) -> anyhow::Result<()> {
        let until = (Instant::now() + Duration::from_secs(1))
            .min(self.deadline + Duration::from_millis(200));
        tokio::time::timeout_at(until, async {
            while self.has_work() {
                self.progress().await?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("terminal stdout budget")??;
        Ok(())
    }
}

struct Controller {
    cfg: BotConfig,
    rec: Recorder,
    session: Option<ControlSession>,
    handshaken: bool,
    character_id: Option<u32>,
    generation: u32,
    started: Instant,
    clock: u32,
}

fn reconnect_allowed(connected: bool, previous: Option<u32>, requested: u32) -> bool {
    !connected && requested != 0 && previous == Some(requested)
}

impl Controller {
    fn state(&self) -> Value {
        let observed = self.session.as_ref().map(|session| {
            let game = &session.game;
            json!({
                "account_id": game.account_id, "character_id": game.state.selected_char,
                "self_obj": game.state.self_obj, "position": game.state.observed_self_pos,
                "position_source": "SCUnitState/SCOneUnitMovement/SCTeleportUnit",
                "observation_seq": game.state.observation_seq,
                "observation_errors": game.state.observation_errors,
                "units_seen": game.state.units.len(), "doodads_seen": game.state.doodads.len()
            })
        });
        json!({"source": "client_observation", "authoritative": false,
               "connected": self.session.is_some(), "character_id": self.character_id,
               "session_generation": self.generation,
               "sampled_t_ms": self.started.elapsed().as_millis(), "observed": observed})
    }

    async fn disconnect(&mut self, reason: &str) -> Value {
        let old = self.session.take();
        let old_self_obj = old.as_ref().map(|s| s.game.state.self_obj);
        let had_session = old.is_some();
        if let Some(session) = old {
            session.disconnect_hard().await;
        }
        json!({"reason": reason, "t_ms": self.started.elapsed().as_millis(),
               "session_generation": self.generation, "old_self_obj": old_self_obj,
               "had_session": had_session, "game_read_closed": had_session,
               "game_write_closed": had_session, "stream_closed": had_session,
               "closure_scope": "local socket handles", "sent_leave_world": false})
    }

    async fn execute(&mut self, cmd: Command, until: Instant) -> anyhow::Result<Value> {
        match cmd {
            Command::Hello => {
                ensure!(!self.handshaken, "already handshaken");
                self.handshaken = true;
                let session = scenario::connect_control(&self.cfg, &self.rec, None).await?;
                self.character_id = Some(session.game.state.selected_char);
                self.generation += 1;
                self.session = Some(session);
                Ok(json!({"ready": true, "state": self.state()}))
            }
            Command::Reconnect(id) => {
                ensure!(
                    reconnect_allowed(self.session.is_some(), self.character_id, id),
                    "reconnect identity/session"
                );
                let session = scenario::connect_control(&self.cfg, &self.rec, Some(id)).await?;
                self.generation += 1;
                self.session = Some(session);
                Ok(json!({"ready": true, "state": self.state()}))
            }
            Command::State => Ok(self.state()),
            Command::DisconnectHard => {
                self.rec.flush_live()?;
                Ok(self.disconnect("disconnect_hard").await)
            }
            Command::Close => {
                let closed = self.disconnect("close").await;
                self.rec.flush_live()?;
                Ok(closed)
            }
            command => {
                let game = &mut self.session.as_mut().context("no connected session")?.game;
                match command {
                    Command::Pump(p) => {
                        let until = (Instant::now() + Duration::from_millis(p.ms)).min(
                            until
                                .checked_sub(Duration::from_millis(20))
                                .unwrap_or(until),
                        );
                        while Instant::now() < until {
                            game.recv_within(until.saturating_duration_since(Instant::now()))
                                .await?;
                        }
                    }
                    Command::Gm(p) => scenario::control_gm(game, &p.command).await?,
                    Command::Tp(p) => {
                        self.clock = self.clock.wrapping_add(1500);
                        scenario::control_tp(game, p.x, p.y, p.z, self.clock, until).await?;
                    }
                    Command::RawCs(opcode, body) => game.send_cs(opcode, &body).await?,
                    _ => unreachable!("handled lifecycle method"),
                }
                // Control consumers use raw events, not scenario-specific accumulated histories.
                game.state.events.clear();
                game.state.social.clear();
                Ok(self.state())
            }
        }
    }

    async fn idle(&mut self) -> anyhow::Result<()> {
        if let Some(session) = self.session.as_mut() {
            for _ in 0..128 {
                if session
                    .game
                    .recv_within(Duration::from_millis(1))
                    .await?
                    .is_none()
                {
                    break;
                }
            }
            session.game.state.events.clear();
            session.game.state.social.clear();
        }
        Ok(())
    }
}

enum Completion {
    Done(anyhow::Result<Value>),
    Stopped(&'static str),
}

/// No output branch awaits a batch. Action receive/keep-alive, terminal input, and
/// command/global timers remain independently pollable while an output ack is delayed.
async fn drive(
    future: impl Future<Output = anyhow::Result<Value>>,
    until: Instant,
    rec: &Recorder,
    report: &mut Reporter,
    input: &mut Input,
) -> anyhow::Result<Completion> {
    tokio::pin!(future);
    let until = until.min(report.deadline);
    let terminal = input.terminal.clone();
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    loop {
        let output_until = report.next_deadline();
        if let Some(reason) = check_stop(&terminal, until, output_until, "command_deadline")? {
            return Ok(Completion::Stopped(reason));
        }
        tokio::select! {
            biased;
            reason = termination(&mut input.terminal) => return Ok(Completion::Stopped(reason)),
            _ = tokio::time::sleep_until(until) => return Ok(Completion::Stopped("command_deadline")),
            _ = tokio::time::sleep_until(output_until.unwrap_or(until)), if output_until.is_some() => bail!("stdout budget exhausted"),
            result = std::future::poll_fn(|cx| {
                // A select can remain suspended across many executor polls. The loop
                // check alone does not guard a ready action on a later wakeup.
                match check_stop(&terminal, until, output_until, "command_deadline") {
                    Ok(Some(reason)) => std::task::Poll::Ready(Ok(Completion::Stopped(reason))),
                    Err(error) => std::task::Poll::Ready(Err(error)),
                    Ok(None) => future.as_mut().poll(cx).map(|result| Ok(Completion::Done(result))),
                }
            }) => return result,
            result = report.progress(), if output_until.is_some() => { result?; },
            _ = tick.tick() => report.events(rec)?,
        }
    }
}

async fn serve(
    controller: &mut Controller,
    report: &mut Reporter,
    input: &mut Input,
) -> anyhow::Result<&'static str> {
    let mut ids = HashSet::new();
    let mut count = 0;
    let mut next_poll = Instant::now();
    report.write(json!({"type": "capabilities", "capabilities": describe()}))?;
    loop {
        if let Some(reason) = check_stop(
            &input.terminal,
            report.deadline,
            report.next_deadline(),
            "deadline",
        )? {
            return Ok(reason);
        }
        // Poll receive/keep-alive even if stdin continuously supplies commands.
        if Instant::now() >= next_poll {
            let rec = controller.rec.clone();
            let idle = async {
                controller.idle().await?;
                Ok(Value::Null)
            };
            match drive(idle, report.deadline, &rec, report, input).await? {
                Completion::Done(Ok(_)) => {}
                Completion::Done(Err(_)) => {
                    let closed = controller.disconnect("session_error").await;
                    report.write(json!({"type": "lifecycle", "closed": closed}))?;
                }
                Completion::Stopped("command_deadline") => return Ok("deadline"),
                Completion::Stopped(reason) => return Ok(reason),
            }
            report.events(&rec)?;
            next_poll = Instant::now() + Duration::from_millis(50);
        }
        let output_until = report.next_deadline();
        if let Some(reason) =
            check_stop(&input.terminal, report.deadline, output_until, "deadline")?
        {
            return Ok(reason);
        }
        let line = tokio::select! {
            biased;
            reason = termination(&mut input.terminal) => return Ok(reason),
            _ = tokio::time::sleep_until(report.deadline) => return Ok("deadline"),
            _ = tokio::time::sleep_until(output_until.unwrap_or(report.deadline)), if output_until.is_some() => bail!("stdout budget exhausted"),
            _ = tokio::time::sleep_until(next_poll) => continue,
            result = report.progress(), if output_until.is_some() => { result?; continue; },
            line = input.lines.recv() => match line { Some(line) => line, None => return Ok("eof") },
        };
        if let Some(reason) = check_stop(
            &input.terminal,
            report.deadline,
            report.next_deadline(),
            "deadline",
        )? {
            return Ok(reason);
        }
        count += 1;
        if count > MAX_COMMANDS {
            report.response(None, Err("command_limit"))?;
            return Ok("command_limit");
        }
        let (id, cmd) = match parse(&line) {
            Ok(parsed) => parsed,
            Err(e) => {
                if let Some(id) = &e.id {
                    if !ids.insert(id.clone()) {
                        report.response(None, Err("duplicate_id"))?;
                        return Ok("duplicate_id");
                    }
                }
                report.response(e.id, Err(e.code))?;
                if e.fatal {
                    return Ok(e.code);
                }
                continue;
            }
        };
        if !ids.insert(id.clone()) {
            report.response(None, Err("duplicate_id"))?;
            return Ok("duplicate_id");
        }
        if !controller.handshaken && !matches!(&cmd, Command::Hello) {
            report.response(Some(id), Err("handshake_required"))?;
            return Ok("handshake_required");
        }
        if (controller.handshaken && matches!(&cmd, Command::Hello))
            || matches!(&cmd, Command::Reconnect(id)
                if !reconnect_allowed(controller.session.is_some(), controller.character_id, *id))
        {
            report.response(Some(id), Err("invalid_session_state"))?;
            continue;
        }
        let closing = matches!(&cmd, Command::Close);
        let connecting = matches!(&cmd, Command::Hello | Command::Reconnect(_));
        let allowance = if connecting {
            CONNECT_TIMEOUT
        } else {
            COMMAND_TIMEOUT
        };
        let until = (Instant::now() + allowance).min(report.deadline);
        // Local state/cleanup remain usable in the global reporting slice; they do not
        // wait for a gameplay acknowledgment. Network actions stop before that slice.
        let work_until = if matches!(
            &cmd,
            Command::State | Command::Close | Command::DisconnectHard
        ) {
            until
        } else {
            until
                .checked_sub(REPORT_RESERVE)
                .unwrap_or_else(Instant::now)
                .max(Instant::now())
        };
        report.budget = until;
        controller.rec.step(&format!("control.{count}"));
        let rec = controller.rec.clone();
        let done = drive(
            controller.execute(cmd, work_until),
            work_until,
            &rec,
            report,
            input,
        )
        .await?;
        let mut stop = None;
        let result = match done {
            Completion::Done(Ok(value)) => Ok(value),
            Completion::Done(Err(_)) => {
                // Native parser failures can retain input. No Display/Debug/cause logging.
                tracing::debug!("control action failed");
                let code = if Instant::now() >= work_until {
                    "command_deadline"
                } else {
                    "session_error"
                };
                let closed = controller.disconnect(code).await;
                report.write(json!({"type": "lifecycle", "closed": closed}))?;
                Err(code)
            }
            Completion::Stopped(code) => {
                let closed = controller.disconnect(code).await;
                report.write(json!({"type": "lifecycle", "closed": closed}))?;
                if code != "command_deadline" || Instant::now() >= report.deadline {
                    stop = Some(code);
                }
                Err(code)
            }
        };
        report.events(&rec)?;
        report.response(Some(id), result)?;
        report.budget = report.deadline;
        if closing {
            return Ok("close");
        }
        if let Some(reason) = stop {
            return Ok(reason);
        }
    }
}

pub(crate) async fn run(cfg: BotConfig, capture: &Path, deadline_ms: u64) -> anyhow::Result<()> {
    ensure!(
        (1..=MAX_DEADLINE_MS).contains(&deadline_ms),
        "deadline-ms must be 1..=600000"
    );
    ensure!(
        !cfg.create_new_character,
        "control cannot create characters"
    );
    let started = Instant::now();
    let rec = Recorder::incremental_game(capture)?;
    let (mut input, output) = stdio()?;
    let mut report = Reporter::new(output, started + Duration::from_millis(deadline_ms));
    let mut controller = Controller {
        cfg,
        rec: rec.clone(),
        session: None,
        handshaken: false,
        character_id: None,
        generation: 0,
        started,
        clock: 40_000,
    };
    let outcome = match tokio::time::timeout_at(
        report.deadline,
        serve(&mut controller, &mut report, &mut input),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Ok("deadline"),
    };
    let reason = outcome.as_ref().copied().unwrap_or("control_error");
    // Always clean up before attempting terminal output, including a broken stdout/capture.
    let closed = controller.disconnect(reason).await;
    let flushed = rec.flush_live();
    if outcome.is_ok() {
        report.budget = report.deadline + Duration::from_millis(200);
        report.events(&rec)?;
        report.write(json!({"type": "closed", "closed": closed}))?;
    }
    report.finish().await?;
    flushed?;
    let reason = outcome?;
    ensure!(
        matches!(reason, "close" | "eof"),
        "control stopped: {reason}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    struct Released(Arc<AtomicBool>);

    impl Drop for Released {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn request(method: &str, params: Value) -> Vec<u8> {
        serde_json::to_vec(
            &json!({"protocol": PROTOCOL, "id": "probe-1", "method": method, "params": params}),
        )
        .unwrap()
    }

    #[test]
    fn strict_envelope_rejects_incompatible_or_ambiguous_commands() {
        for input in [
            br#"{"id":1,"method":"hello","params":{}}"#.as_slice(),
            br#"{"protocol":"aaemu-control/2","id":1,"method":"hello","params":{}}"#,
            br#"{"protocol":"aaemu-control/1","id":null,"method":"hello","params":{}}"#,
            br#"{"protocol":"aaemu-control/1","id":1,"method":"hello","params":{},"extra":1}"#,
            br#"{"protocol":"aaemu-control/1","id":1,"id":2,"method":"hello","params":{}}"#,
            br#"{"protocol":"aaemu-control/1","id":1,"method":"pump","params":{"ms":0,"ms":1}}"#,
            br#"{"protocol":"aaemu-control/1","id":1,"method":"hello","params":null}"#,
            b"\xff",
            b"",
        ] {
            assert!(parse(input).is_err(), "accepted incompatible request");
        }
        assert!(matches!(
            parse(&request("hello", json!({}))),
            Ok((_, Command::Hello))
        ));
        let wrong = parse(br#"{"protocol":"wrong","id":7,"method":"hello","params":{}}"#)
            .err()
            .unwrap();
        assert!(wrong.fatal);
        assert_eq!(wrong.id, Some(RequestId::Number(7)));
    }

    #[test]
    fn literal_packet_and_action_limits_do_not_coerce_or_truncate() {
        for (method, params) in [
            ("pump", json!({"ms": MAX_PUMP_MS + 1})),
            ("pump", json!({"ms": -1})),
            ("state", json!({"authoritative": true})),
            ("gm", json!({"command": "x".repeat(MAX_GM + 1)})),
            ("gm", json!({"command": "move 1 2 3\nitem add self 1"})),
            ("rawcs", json!({"opcode": "0x03a", "body_hex": "00"})),
            ("rawcs", json!({"opcode": 65536, "body_hex": "00"})),
            ("rawcs", json!({"opcode": 58, "body_hex": "0"})),
            ("rawcs", json!({"opcode": 58, "body_hex": "00 01"})),
            (
                "rawcs",
                json!({"opcode": 58, "body_hex": "00".repeat(MAX_BODY + 1)}),
            ),
            (
                "rawcs",
                json!({"opcode": cs::X2EnterWorldPacket, "body_hex": "00"}),
            ),
            ("reconnect", json!({"character_id": 0})),
        ] {
            assert!(
                parse(&request(method, params)).is_err(),
                "accepted invalid {method}"
            );
        }
        let (_, cmd) = parse(&request("rawcs", json!({"opcode": 58, "body_hex": "abcd"})))
            .ok()
            .unwrap();
        assert!(matches!(cmd, Command::RawCs(58, body) if body == [0xab, 0xcd]));
        assert!(parse(&request("pump", json!({"ms": MAX_PUMP_MS}))).is_ok());
        assert!(parse(&request(
            "rawcs",
            json!({"opcode": 58, "body_hex": "00".repeat(MAX_BODY)})
        ))
        .is_ok());
    }

    #[test]
    fn framing_handles_split_crlf_and_rejects_truncation_or_unterminated_overflow() {
        let mut reader = BufReader::with_capacity(3, Cursor::new(b"{}\r\nsecond\n"));
        assert_eq!(read_line(&mut reader).unwrap(), Some(b"{}".to_vec()));
        assert_eq!(read_line(&mut reader).unwrap(), Some(b"second".to_vec()));
        assert_eq!(read_line(&mut reader).unwrap(), None);
        assert_eq!(read_line(&mut Cursor::new(b"{}")), Err("truncated_line"));
        assert_eq!(
            read_line(&mut Cursor::new(vec![b'a'; MAX_LINE + 1])),
            Err("line_limit")
        );
        let mut exact = vec![b'a'; MAX_LINE];
        exact.push(b'\n');
        assert_eq!(
            read_line(&mut Cursor::new(exact)).unwrap().unwrap().len(),
            MAX_LINE
        );
    }

    #[test]
    fn reconnect_requires_disconnected_session_and_the_exact_prior_character() {
        assert!(!reconnect_allowed(false, None, 42));
        assert!(!reconnect_allowed(true, Some(42), 42));
        assert!(!reconnect_allowed(false, Some(42), 43));
        assert!(!reconnect_allowed(false, Some(0), 0));
        assert!(reconnect_allowed(false, Some(42), 42));
    }

    fn input_pair() -> (
        Input,
        mpsc::Sender<Vec<u8>>,
        watch::Sender<Option<&'static str>>,
    ) {
        let (lines, rx) = mpsc::channel(INPUT_QUEUE);
        let (terminal, stopped) = watch::channel(None);
        (
            Input {
                lines: rx,
                terminal: stopped,
            },
            lines,
            terminal,
        )
    }

    #[tokio::test]
    async fn eof_cancels_an_active_action_without_waiting_for_its_timeout() {
        let rec = Recorder::new(false);
        let (output, _reader) = mpsc::channel(1);
        let until = Instant::now() + CONNECT_TIMEOUT;
        let mut report = Reporter::new(output, until);
        let (mut input, _lines, terminal) = input_pair();
        terminal.send_replace(Some("eof"));
        let released = Arc::new(AtomicBool::new(false));
        let resource = Released(released.clone());
        let result = drive(
            async move {
                let _resource = resource;
                std::future::pending().await
            },
            until,
            &rec,
            &mut report,
            &mut input,
        )
        .await
        .unwrap();
        assert!(matches!(result, Completion::Stopped("eof")));
        assert!(released.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn expired_deadline_cancels_even_when_input_is_idle() {
        let rec = Recorder::new(false);
        let (output, _reader) = mpsc::channel(1);
        let until = Instant::now();
        let mut report = Reporter::new(output, until);
        let (mut input, _lines, _terminal) = input_pair();
        let released = Arc::new(AtomicBool::new(false));
        let resource = Released(released.clone());
        let result = drive(
            async move {
                let _resource = resource;
                std::future::pending().await
            },
            until,
            &rec,
            &mut report,
            &mut input,
        )
        .await
        .unwrap();
        assert!(matches!(result, Completion::Stopped("command_deadline")));
        assert!(released.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn already_expired_command_or_global_deadline_never_polls_ready_action() {
        for global_expired in [false, true] {
            let rec = Recorder::new(false);
            let (output, _consumer) = mpsc::channel(1);
            let expired = Instant::now();
            let later = expired + CONNECT_TIMEOUT;
            let (command_until, global_until) = if global_expired {
                (later, expired)
            } else {
                (expired, later)
            };
            let mut report = Reporter::new(output, global_until);
            let (mut input, _lines, _terminal) = input_pair();
            let polled = AtomicBool::new(false);
            let action = async {
                polled.store(true, Ordering::SeqCst);
                Ok(Value::Null)
            };
            let result = drive(action, command_until, &rec, &mut report, &mut input)
                .await
                .unwrap();
            assert!(matches!(result, Completion::Stopped("command_deadline")));
            assert!(
                !polled.load(Ordering::SeqCst),
                "an already-expired ceiling allowed ready gameplay"
            );
        }
    }

    #[tokio::test]
    async fn eof_precedes_expired_budgets_without_polling_ready_action() {
        for published in [false, true] {
            let rec = Recorder::new(false);
            let (output, _consumer) = mpsc::channel(1);
            let until = Instant::now();
            let mut report = Reporter::new(output, until);
            report.write(json!({"type":"capabilities"})).unwrap();
            let (mut input, _lines, terminal) = input_pair();
            if published {
                terminal.send_replace(Some("eof"));
            }
            drop(terminal);
            let polled = AtomicBool::new(false);
            let action = async {
                polled.store(true, Ordering::SeqCst);
                Ok(Value::Null)
            };
            let result = drive(action, until, &rec, &mut report, &mut input)
                .await
                .unwrap();
            assert!(matches!(result, Completion::Stopped("eof")));
            assert!(!polled.load(Ordering::SeqCst));
        }
    }

    fn live_recorder() -> Recorder {
        let rec = Recorder::incremental_game_to(std::io::sink());
        rec.control_game_ready(true);
        rec
    }

    #[tokio::test]
    async fn delayed_output_acks_do_not_suspend_game_keepalive_or_command_deadline() {
        use aaemu_commons::net::FrameCodec;
        use aaemu_game::opcodes::{pp, sc};
        use aaemu_testclient::game::GameClient;
        use aaemu_testclient::record::{Channel, Dir};
        use futures::StreamExt;
        use std::sync::atomic::AtomicUsize;
        use tokio_util::codec::FramedRead;

        let rec = live_recorder();
        let (io, peer) = tokio::io::duplex(64 * 1024);
        let mut game = GameClient::from_io(io, rec.clone());
        game.account_id = 1;
        game.ping_every = Duration::from_millis(20);
        let peer = tokio::spawn(async move {
            let mut peer = FramedRead::new(peer, FrameCodec);
            let mut pings = 0;
            while let Some(packet) = peer.next().await {
                let packet = packet.unwrap();
                assert_eq!(packet[1], 2);
                assert_eq!(u16::from_le_bytes([packet[2], packet[3]]), pp::PingPacket);
                pings += 1;
            }
            pings
        });
        let (output, mut consumed) = mpsc::channel::<Output>(1);
        let acknowledgments = Arc::new(AtomicUsize::new(0));
        let counted = acknowledgments.clone();
        let consumer = tokio::spawn(async move {
            while let Some(packet) = consumed.recv().await {
                tokio::time::sleep(Duration::from_millis(25)).await;
                if packet.ack.send(Ok(())).is_err() {
                    break;
                }
                counted.fetch_add(1, Ordering::SeqCst);
            }
        });
        let until = Instant::now() + Duration::from_millis(300);
        let mut report = Reporter::new(output, until + Duration::from_secs(1));
        report.budget = until;
        // This batch needs at least 800ms of acknowledgments. The action has 300ms.
        for _ in 0..32 {
            rec.push(
                Channel::Game,
                Dir::S2C,
                1,
                sc::SCErrorMsgPacket,
                "ambient",
                &[7, 0],
            );
        }
        report.events(&rec).unwrap();
        let (mut input, _lines, _terminal) = input_pair();
        let action = async {
            loop {
                game.recv_within(Duration::from_millis(5)).await?;
            }
        };
        let done = tokio::time::timeout(
            Duration::from_secs(2),
            drive(action, until, &rec, &mut report, &mut input),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(done, Completion::Stopped("command_deadline")));
        assert!(acknowledgments.load(Ordering::SeqCst) >= 2);
        assert!(acknowledgments.load(Ordering::SeqCst) < 32);
        assert!(report.has_work());
        game.disconnect_hard().await;
        let pings = tokio::time::timeout(Duration::from_secs(1), peer)
            .await
            .unwrap()
            .unwrap();
        assert!(
            pings >= 2,
            "slow output starved the actual GameClient keep-alive path"
        );
        consumer.abort();
        let _ = consumer.await;
    }

    #[tokio::test]
    async fn active_action_plus_pending_rawcs_and_eof_never_sends_queued_gameplay() {
        use aaemu_commons::net::FrameCodec;
        use aaemu_testclient::game::GameClient;
        use futures::StreamExt;
        use tokio_util::codec::FramedRead;

        let rec = live_recorder();
        let (io, peer) = tokio::io::duplex(64 * 1024);
        let mut game = GameClient::from_io(io, rec.clone());
        game.account_id = 1;
        let peer = tokio::spawn(async move {
            let mut peer = FramedRead::new(peer, FrameCodec);
            let mut packets = Vec::new();
            while let Some(packet) = peer.next().await {
                packets.push(packet.unwrap());
            }
            packets
        });
        let (output, _consumer) = mpsc::channel(1);
        let until = Instant::now() + Duration::from_secs(1);
        let mut report = Reporter::new(output, until);
        let (mut input, lines, terminal) = input_pair();
        let (started, ready) = oneshot::channel();
        let producer = tokio::spawn(async move {
            ready.await.unwrap();
            let mut raw = request("rawcs", json!({"opcode":58,"body_hex":"abcd"}));
            raw.push(b'\n');
            // The real bounded input worker sees a queued rawcs followed by EOF.
            read_input(Cursor::new(raw), lines, terminal);
        });
        let action = async {
            let _ = started.send(());
            loop {
                game.recv_within(Duration::from_millis(10)).await?;
            }
        };
        let done = drive(action, until, &rec, &mut report, &mut input)
            .await
            .unwrap();
        assert!(matches!(done, Completion::Stopped("eof")));
        producer.await.unwrap();
        let queued = input.lines.try_recv().unwrap();
        assert!(matches!(parse(&queued), Ok((_, Command::RawCs(58, _)))));
        game.disconnect_hard().await;
        let packets = tokio::time::timeout(Duration::from_secs(1), peer)
            .await
            .unwrap()
            .unwrap();
        assert!(!packets.is_empty());
        assert!(
            packets.iter().all(|packet| packet[1] == 2),
            "queued rawcs or CSLeaveWorld escaped after EOF"
        );
    }

    #[test]
    fn input_queue_overflow_signals_terminal_without_blocking_the_reader() {
        let (mut input, lines, terminal) = input_pair();
        read_input(
            Cursor::new(b"{}\n".repeat(INPUT_QUEUE + 1)),
            lines,
            terminal,
        );
        assert_eq!(*input.terminal.borrow(), Some("input_queue_limit"));
        let mut count = 0;
        while input.lines.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, INPUT_QUEUE);
    }

    #[tokio::test]
    async fn earlier_command_output_budget_applies_even_behind_a_longer_lived_head() {
        let rec = live_recorder();
        let (output, _consumer) = mpsc::channel(1);
        let until = Instant::now() + CONNECT_TIMEOUT;
        let mut report = Reporter::new(output, until);
        report.write(json!({"type":"capabilities"})).unwrap();
        report.budget = Instant::now();
        report
            .response(Some(RequestId::Number(1)), Ok(Value::Null))
            .unwrap();
        let (mut input, _lines, _terminal) = input_pair();
        let polled = Arc::new(AtomicBool::new(false));
        let observed = polled.clone();
        let action = async move {
            observed.store(true, Ordering::SeqCst);
            Ok(Value::Null)
        };
        let result = drive(action, until, &rec, &mut report, &mut input).await;
        assert!(result.is_err());
        assert!(
            !polled.load(Ordering::SeqCst),
            "expired output budget allowed later gameplay"
        );
    }

    #[tokio::test]
    async fn expired_output_budget_blocks_ready_action_with_an_ack_in_flight() {
        for expired_flight in [false, true] {
            let rec = live_recorder();
            let (output, mut consumer) = mpsc::channel(1);
            let until = Instant::now() + CONNECT_TIMEOUT;
            let mut report = Reporter::new(output, until);
            report.write(json!({"type":"capabilities"})).unwrap();
            // Start a real output write, but withhold its acknowledgment. No sleep or
            // timer-driver behavior is needed to inject an expired queue/ack budget.
            report.progress().await.unwrap();
            let unacknowledged = consumer.try_recv().unwrap();
            assert!(report.flight.is_some());
            if expired_flight {
                report.flight.as_mut().unwrap().until = Instant::now();
            } else {
                report.budget = Instant::now();
            }
            report
                .response(Some(RequestId::Number(1)), Ok(Value::Null))
                .unwrap();
            let (mut input, _lines, _terminal) = input_pair();
            let polled = AtomicBool::new(false);
            let action = async {
                polled.store(true, Ordering::SeqCst);
                Ok(Value::Null)
            };
            let result = drive(action, until, &rec, &mut report, &mut input).await;
            assert!(result.is_err());
            assert!(!polled.load(Ordering::SeqCst));
            // Terminal draining must also honor the earlier queued budget while a
            // longer-lived ack is in flight, rather than consuming that ack first.
            assert!(report.progress().await.is_err());
            assert!(consumer.try_recv().is_err());
            drop(unacknowledged);
        }
    }

    #[tokio::test]
    async fn serve_checks_expired_budgets_before_consuming_queued_gameplay() {
        for (expired_flight, eof) in [(false, false), (true, false), (false, true)] {
            let rec = live_recorder();
            let cfg = serde_json::from_value(json!({"login":"127.0.0.1:1","user":"fixture",
                "password":"fixture-only","character":"Owner"}))
            .unwrap();
            // No sockets: any attempted rawcs would produce a session error. Leaving
            // the request queued proves serve stops before parsing or executing it.
            let mut controller = Controller {
                cfg,
                rec,
                session: None,
                handshaken: true,
                character_id: Some(42),
                generation: 1,
                started: Instant::now(),
                clock: 40000,
            };
            let until = Instant::now() + CONNECT_TIMEOUT;
            let (output, mut consumer) = mpsc::channel(1);
            let mut report = Reporter::new(output, until);
            report.write(json!({"type":"capabilities"})).unwrap();
            let unacknowledged = if expired_flight {
                report.progress().await.unwrap();
                report.flight.as_mut().unwrap().until = Instant::now();
                Some(consumer.try_recv().unwrap())
            } else {
                None
            };
            report.budget = if expired_flight {
                until
            } else {
                Instant::now()
            };
            report
                .response(Some(RequestId::Number(1)), Ok(Value::Null))
                .unwrap();
            let (mut input, lines, terminal) = input_pair();
            let rawcs = request("rawcs", json!({"opcode":58,"body_hex":"abcd"}));
            lines.try_send(rawcs.clone()).unwrap();
            if eof {
                terminal.send_replace(Some("eof"));
            }
            let queued = report.queue.len();
            let result = serve(&mut controller, &mut report, &mut input).await;
            if eof {
                assert_eq!(result.unwrap(), "eof");
            } else {
                assert!(result.is_err());
            }
            assert_eq!(input.lines.try_recv().unwrap(), rawcs);
            assert_eq!(report.queue.len(), queued + 1); // Only serve's capabilities.
            assert!(consumer.try_recv().is_err());
            assert!(controller.session.is_none());
            drop(unacknowledged);
        }
    }

    async fn timeout_serve_case(end_with_eof: bool) {
        use aaemu_testclient::game::GameClient;
        use aaemu_testclient::login::LoginClient;
        use aaemu_testclient::stream::StreamClient;
        use std::sync::Mutex;
        use tokio::io::AsyncReadExt;
        use tokio::net::TcpListener;

        // Source-only fixture: ephemeral loopback transports, no emulator/auth/DB. Start
        // with an established session so the full serve path executes the real tp method.
        let rec = live_recorder();
        let game_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let login_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut game = GameClient::connect(
            &game_listener.local_addr().unwrap().to_string(),
            rec.clone(),
        )
        .await
        .unwrap();
        let game_peer = game_listener.accept().await.unwrap().0;
        let stream = StreamClient::connect(
            &stream_listener.local_addr().unwrap().to_string(),
            rec.clone(),
        )
        .await
        .unwrap();
        let stream_peer = stream_listener.accept().await.unwrap().0;
        let login = LoginClient::connect(
            &login_listener.local_addr().unwrap().to_string(),
            rec.clone(),
        )
        .await
        .unwrap();
        let login_peer = login_listener.accept().await.unwrap().0;
        game.account_id = 1;
        game.state.selected_char = 42;
        game.state.self_obj = 77;
        game.state.observed_self_pos = Some((1.0, 2.0, 3.0));
        let peers = [game_peer, stream_peer, login_peer].map(|mut peer| {
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                peer.read_to_end(&mut bytes).await.unwrap();
                bytes
            })
        });
        let cfg: BotConfig = serde_json::from_value(json!({"login":"127.0.0.1:1","user":"fixture",
            "password":"fixture-only","character":"Owner"}))
        .unwrap();
        let started = Instant::now();
        let mut controller = Controller {
            cfg,
            rec: rec.clone(),
            session: Some(ControlSession::new(game, stream, login)),
            handshaken: true,
            character_id: Some(42),
            generation: 1,
            started,
            clock: 40000,
        };
        let until = started + Duration::from_millis(800);
        let (output, mut consumed) = mpsc::channel::<Output>(1);
        let records = Arc::new(Mutex::new(Vec::<Value>::new()));
        let saved = records.clone();
        let (observed, mut responses) = mpsc::channel::<Value>(4);
        let consumer = tokio::spawn(async move {
            while let Some(frame) = consumed.recv().await {
                let value: Value = serde_json::from_slice(&frame.bytes).unwrap();
                saved.lock().unwrap().push(value.clone());
                frame.ack.send(Ok(())).unwrap();
                if value["type"] == "response" {
                    observed.send(value).await.unwrap();
                }
            }
        });
        let mut report = Reporter::new(output, until);
        let (mut input, lines, terminal) = input_pair();
        let tp = json!({"protocol":PROTOCOL,"id":1,"method":"tp","params":{"x":10,"y":20,"z":30}});
        lines.send(serde_json::to_vec(&tp).unwrap()).await.unwrap();
        let (finished, done) = oneshot::channel();
        let serving = async {
            let result = serve(&mut controller, &mut report, &mut input).await;
            let _ = finished.send(());
            result
        };
        let script = async move {
            let timeout = responses.recv().await.unwrap();
            assert_eq!(timeout["id"], 1);
            assert_eq!(timeout["ok"], false);
            assert_eq!(timeout["error"]["code"], "command_deadline");
            // All three peer read tasks must observe EOF before any follow-up gameplay.
            let mut evidence = Vec::new();
            for peer in peers {
                evidence.push(
                    tokio::time::timeout(Duration::from_secs(1), peer)
                        .await
                        .unwrap()
                        .unwrap(),
                );
            }
            let state = json!({"protocol":PROTOCOL,"id":2,"method":"state","params":{}});
            lines
                .send(serde_json::to_vec(&state).unwrap())
                .await
                .unwrap();
            let state = responses.recv().await.unwrap();
            assert_eq!(state["id"], 2);
            assert_eq!(state["ok"], true);
            assert_eq!(state["result"]["connected"], false);
            assert_eq!(state["result"]["character_id"], 42);
            if end_with_eof {
                terminal.send_replace(Some("eof"));
            }
            // Keep stdin open in the deadline case until serve itself returns.
            let keep_input = (lines, terminal);
            let _ = done.await;
            drop(keep_input);
            evidence
        };
        let (result, evidence) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(serving, script)
        })
        .await
        .unwrap();
        assert_eq!(
            result.unwrap(),
            if end_with_eof { "eof" } else { "deadline" }
        );
        assert!(controller.session.is_none());
        report.finish().await.unwrap();
        {
            let records = records.lock().unwrap();
            assert_eq!(
                records
                    .iter()
                    .filter(|r| r["type"] == "response" && r["id"] == 1)
                    .count(),
                1
            );
            assert_eq!(
                records
                    .iter()
                    .filter(|r| r["type"] == "lifecycle"
                        && r["closed"]["reason"] == "command_deadline"
                        && r["closed"]["game_read_closed"] == true
                        && r["closed"]["game_write_closed"] == true
                        && r["closed"]["stream_closed"] == true)
                    .count(),
                1
            );
        }
        assert!(evidence[1].is_empty() && evidence[2].is_empty());
        let mut wire = evidence[0].as_slice();
        let mut chats = 0;
        while !wire.is_empty() {
            let len = usize::from(u16::from_le_bytes([wire[0], wire[1]]));
            let frame = &wire[2..2 + len];
            if frame[1] == 1 {
                let opcode = u16::from_le_bytes([frame[4], frame[5]]);
                assert_ne!(opcode, aaemu_game::opcodes::cs::CSLeaveWorldPacket);
                if opcode == aaemu_game::opcodes::cs::CSSendChatMessagePacket {
                    chats += 1;
                }
            }
            wire = &wire[2 + len..];
        }
        assert_eq!(chats, 1, "tp must be attempted once, never retried");
        consumer.abort();
        let _ = consumer.await;
    }

    #[tokio::test]
    async fn full_serve_timeout_reports_once_closes_sockets_and_remains_controllable() {
        timeout_serve_case(true).await;
        timeout_serve_case(false).await;
    }
}
