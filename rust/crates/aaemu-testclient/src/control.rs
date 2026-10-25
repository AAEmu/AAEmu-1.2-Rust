//! Bounded stdio controller, using the same session and packet paths as scenarios.
//!
//! First send `{"protocol":"aaemu-control/1","id":1,"method":"hello","params":{}}`.
//! Its response is the ready boundary. Every subsequent request has the same envelope and
//! a unique string (1..64 bytes) or unsigned integer id. All params objects are strict.
//! Stdout contains only versioned responses, independent game events and lifecycle records;
//! tracing goes to stderr. Events preceding a response have `seq <= response.event_seq`.
//! This boundary orders observations; it does not imply that a server action has settled.

use std::collections::HashSet;
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
use tokio::sync::{mpsc, oneshot};
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
            "event_queue_bytes": 8 * 1024 * 1024, "event_queue_count": 2048
        },
        "incremental_game_capture": true,
        "capture_create_new": true,
        "auth_frames_omitted": true,
        "capture_start": "post-lobby character select/spawn (each session)",
        "handshake_before_network": true,
        "initial_character": "existing configured name, never first/create",
        "state_source": "client_observation",
        "automatic_retry": false,
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

enum Input {
    Line(Vec<u8>),
    End,
    Fault(&'static str),
}

struct Output {
    bytes: Vec<u8>,
    ack: oneshot::Sender<std::io::Result<()>>,
}

/// Detached, bounded stdio workers avoid Tokio's uncancellable blocking stdin read keeping
/// the runtime alive after deadline. They own no sockets and are never joined on shutdown.
fn stdio() -> anyhow::Result<(mpsc::Receiver<Input>, mpsc::Sender<Output>)> {
    let (input_tx, input_rx) = mpsc::channel(1);
    std::thread::Builder::new()
        .name("control-input".into())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            loop {
                let message = match read_line(&mut stdin) {
                    Ok(Some(line)) => Input::Line(line),
                    Ok(None) => Input::End,
                    Err(code) => Input::Fault(code),
                };
                let terminal = !matches!(&message, Input::Line(_));
                if input_tx.blocking_send(message).is_err() || terminal {
                    break;
                }
            }
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
    Ok((input_rx, output_tx))
}

struct Reporter {
    output: mpsc::Sender<Output>,
    deadline: Instant,
    seq: u64,
}

impl Reporter {
    async fn write(&mut self, mut value: Value) -> anyhow::Result<()> {
        value["protocol"] = PROTOCOL.into();
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        let (ack, completed) = oneshot::channel();
        // A blocked/broken output must not hold game sockets indefinitely. Allow a small
        // terminal reporting window after the network deadline; no gameplay runs then.
        let until = (Instant::now() + Duration::from_secs(1))
            .min(self.deadline + Duration::from_millis(200));
        tokio::time::timeout_at(until, async {
            self.output
                .send(Output { bytes, ack })
                .await
                .context("stdout worker closed")?;
            completed.await.context("stdout worker stopped")??;
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("stdout deadline")??;
        Ok(())
    }

    async fn events(&mut self, rec: &Recorder) -> anyhow::Result<()> {
        for event in rec.take_live_events()? {
            self.seq += 1;
            self.write(json!({"type": "event", "seq": self.seq, "event": event}))
                .await?;
        }
        Ok(())
    }

    async fn response(
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
        self.write(value).await
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
        if let Some(ControlSession { game, stream, .. }) = old {
            // StreamClient owns one unsplit socket and no background task.
            drop(stream);
            game.disconnect_hard().await;
        }
        json!({"reason": reason, "t_ms": self.started.elapsed().as_millis(),
               "session_generation": self.generation, "old_self_obj": old_self_obj,
               "had_session": had_session, "game_read_closed": had_session,
               "game_write_closed": had_session, "stream_closed": had_session,
               "closure_scope": "local socket handles", "sent_leave_world": false})
    }

    async fn execute(&mut self, cmd: Command) -> anyhow::Result<Value> {
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
                        let until = Instant::now() + Duration::from_millis(p.ms);
                        while Instant::now() < until {
                            game.recv_within(until.saturating_duration_since(Instant::now()))
                                .await?;
                        }
                    }
                    Command::Gm(p) => scenario::control_gm(game, &p.command).await?,
                    Command::Tp(p) => {
                        self.clock = self.clock.wrapping_add(1500);
                        scenario::control_tp(game, p.x, p.y, p.z, self.clock).await?;
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

/// Keep capture/event delivery and EOF detection alive while the shared session code runs.
/// One look-ahead command is allowed; subsequent input remains backpressured.
async fn drive(
    future: impl Future<Output = anyhow::Result<Value>>,
    until: Instant,
    rec: &Recorder,
    report: &mut Reporter,
    input: &mut mpsc::Receiver<Input>,
    pending: &mut Option<Input>,
) -> anyhow::Result<Completion> {
    tokio::pin!(future);
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    loop {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(until) => return Ok(Completion::Stopped("command_deadline")),
            result = &mut future => return Ok(Completion::Done(result)),
            message = input.recv(), if pending.is_none() => match message {
                Some(line @ Input::Line(_)) => *pending = Some(line),
                Some(Input::Fault(code)) => return Ok(Completion::Stopped(code)),
                Some(Input::End) | None => return Ok(Completion::Stopped("eof")),
            },
            _ = tick.tick() => report.events(rec).await?,
        }
    }
}

async fn serve(
    controller: &mut Controller,
    report: &mut Reporter,
    input: &mut mpsc::Receiver<Input>,
) -> anyhow::Result<&'static str> {
    let mut pending = None;
    let mut ids = HashSet::new();
    let mut count = 0;
    let mut next_poll = Instant::now();
    report
        .write(json!({"type": "capabilities", "capabilities": describe()}))
        .await?;
    loop {
        if Instant::now() >= report.deadline {
            return Ok("deadline");
        }
        // Poll independently of stdin readiness: a stream of state/rawcs requests must
        // not starve receive/keep-alive work, even with one look-ahead request queued.
        if Instant::now() >= next_poll {
            let idle = tokio::time::timeout_at(report.deadline, controller.idle()).await;
            if !matches!(idle, Ok(Ok(()))) {
                let closed = controller.disconnect("session_error").await;
                report
                    .write(json!({"type": "lifecycle", "closed": closed}))
                    .await?;
            }
            report.events(&controller.rec).await?;
            next_poll = Instant::now() + Duration::from_millis(50);
        }
        let message = if let Some(message) = pending.take() {
            Some(message)
        } else {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(report.deadline) => return Ok("deadline"),
                _ = tokio::time::sleep_until(next_poll) => continue,
                message = input.recv() => message,
            }
        };
        let line = match message {
            Some(Input::Line(line)) => line,
            Some(Input::Fault(code)) => {
                report.response(None, Err(code)).await?;
                return Ok(code);
            }
            Some(Input::End) | None => return Ok("eof"),
        };
        if Instant::now() >= report.deadline {
            return Ok("deadline");
        }
        count += 1;
        if count > MAX_COMMANDS {
            report.response(None, Err("command_limit")).await?;
            return Ok("command_limit");
        }
        let (id, cmd) = match parse(&line) {
            Ok(parsed) => parsed,
            Err(e) => {
                if let Some(id) = &e.id {
                    if !ids.insert(id.clone()) {
                        report.response(None, Err("duplicate_id")).await?;
                        return Ok("duplicate_id");
                    }
                }
                report.response(e.id, Err(e.code)).await?;
                if e.fatal {
                    return Ok(e.code);
                }
                continue;
            }
        };
        if !ids.insert(id.clone()) {
            report.response(None, Err("duplicate_id")).await?;
            return Ok("duplicate_id");
        }
        if !controller.handshaken && !matches!(&cmd, Command::Hello) {
            report.response(Some(id), Err("handshake_required")).await?;
            return Ok("handshake_required");
        }
        if (controller.handshaken && matches!(&cmd, Command::Hello))
            || matches!(&cmd, Command::Reconnect(id)
                if !reconnect_allowed(controller.session.is_some(), controller.character_id, *id))
        {
            report
                .response(Some(id), Err("invalid_session_state"))
                .await?;
            continue;
        }
        let closing = matches!(&cmd, Command::Close);
        let connecting = matches!(&cmd, Command::Hello | Command::Reconnect(_));
        let allowance = if connecting {
            CONNECT_TIMEOUT
        } else {
            COMMAND_TIMEOUT
        };
        // Pump's requested maximum may equal the ordinary action ceiling.
        let allowance = if matches!(&cmd, Command::Pump(_)) {
            allowance + Duration::from_millis(100)
        } else {
            allowance
        };
        let until = (Instant::now() + allowance).min(report.deadline);
        controller.rec.step(&format!("control.{count}"));
        let rec = controller.rec.clone();
        let done = drive(
            controller.execute(cmd),
            until,
            &rec,
            report,
            input,
            &mut pending,
        )
        .await?;
        let mut stop = None;
        let result = match done {
            Completion::Done(Ok(value)) => Ok(value),
            Completion::Done(Err(error)) => {
                tracing::debug!("control action failed: {error:#}");
                let closed = controller.disconnect("session_error").await;
                report
                    .write(json!({"type": "lifecycle", "closed": closed}))
                    .await?;
                Err("session_error")
            }
            Completion::Stopped(code) => {
                let closed = controller.disconnect(code).await;
                report
                    .write(json!({"type": "lifecycle", "closed": closed}))
                    .await?;
                if code != "command_deadline" || Instant::now() >= report.deadline {
                    stop = Some(code);
                }
                Err(code)
            }
        };
        report.events(&rec).await?;
        report.response(Some(id), result).await?;
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
    let mut report = Reporter {
        output,
        deadline: started + Duration::from_millis(deadline_ms),
        seq: 0,
    };
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
        report.events(&rec).await?;
        report
            .write(json!({"type": "closed", "closed": closed}))
            .await?;
    }
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

    #[tokio::test]
    async fn eof_cancels_an_active_action_without_waiting_for_its_timeout() {
        let rec = Recorder::new(false);
        let (output, _reader) = mpsc::channel(1);
        let mut report = Reporter {
            output,
            deadline: Instant::now() + CONNECT_TIMEOUT,
            seq: 0,
        };
        let (sender, mut input) = mpsc::channel(1);
        sender.send(Input::End).await.unwrap();
        let released = Arc::new(AtomicBool::new(false));
        let resource = Released(released.clone());
        let result = drive(
            async move {
                let _resource = resource;
                std::future::pending().await
            },
            report.deadline,
            &rec,
            &mut report,
            &mut input,
            &mut None,
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
        let mut report = Reporter {
            output,
            deadline: Instant::now(),
            seq: 0,
        };
        let (_sender, mut input) = mpsc::channel(1);
        let released = Arc::new(AtomicBool::new(false));
        let resource = Released(released.clone());
        let result = drive(
            async move {
                let _resource = resource;
                std::future::pending().await
            },
            report.deadline,
            &rec,
            &mut report,
            &mut input,
            &mut None,
        )
        .await
        .unwrap();
        assert!(matches!(result, Completion::Stopped("command_deadline")));
        assert!(released.load(Ordering::SeqCst));
    }
}
