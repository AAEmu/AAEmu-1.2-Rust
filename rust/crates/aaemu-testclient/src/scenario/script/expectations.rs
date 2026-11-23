use std::collections::HashSet;
use std::time::{Duration, Instant};

use aaemu_game::opcodes::sc;
use anyhow::{bail, Context};

use super::{peer, read_log_since, split_quoted};
use crate::game::{GameClient, Packet};
use crate::social::SocialEvent;

/// Variant name of a social event (`TeamJoined { .. }` -> `TeamJoined`).
fn event_kind(e: &SocialEvent) -> String {
    let d = format!("{e:?}");
    d.split([' ', '{', '(']).next().unwrap_or("").to_string()
}

/// Waits for the first not yet consumed social event named `kind` (case-insensitive),
/// reading packets for at most `secs`.
pub(super) async fn await_event(
    game: &mut GameClient,
    consumed: &mut HashSet<usize>,
    kind: &str,
    secs: u64,
) -> anyhow::Result<SocialEvent> {
    let want = kind.to_ascii_lowercase();
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let found = game
            .state
            .social
            .iter()
            .enumerate()
            .find(|(i, e)| !consumed.contains(i) && event_kind(e).to_ascii_lowercase() == want)
            .map(|(i, e)| (i, e.clone()));
        if let Some((i, e)) = found {
            consumed.insert(i);
            return Ok(e);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("timed out ({secs} s) waiting for social event '{kind}'");
        }
        game.recv_within(left.min(Duration::from_millis(500)))
            .await?;
    }
}

/// The object a buff expectation watches (`self`, `peer` or `player:NAME`).
fn buff_target(game: &GameClient, spec: Option<&str>) -> anyhow::Result<u32> {
    match spec.unwrap_or("self") {
        "self" => Ok(game.state.self_obj),
        "peer" => Ok(peer(game, None)?.obj_id),
        other => match other.strip_prefix("player:") {
            Some(name) => Ok(peer(game, Some(name))?.obj_id),
            None => bail!("buff target must be self, peer or player:NAME (got '{other}')"),
        },
    }
}

/// `expectbuff <buff> [self|peer|player:NAME] [secs]`.
pub(super) async fn expect_buff(
    game: &mut GameClient,
    buff: u32,
    spec: Option<&str>,
    secs: u64,
) -> anyhow::Result<()> {
    let target = buff_target(game, spec)?;
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        // The state tracks every buff packet, including those an earlier command drained.
        if let Some(indices) = game.state.buffs.get(&(target, buff)) {
            let indices: Vec<u32> = indices.iter().copied().collect();
            if !indices.is_empty() {
                println!("    buff {buff} on {target} present (indices {indices:?})");
                return Ok(());
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("expected buff {buff} on {target} within {secs} s; it did not arrive");
        }
        game.recv_within(left.min(Duration::from_millis(250)))
            .await?;
    }
}

/// `expectbuffremoved <buff> [self|peer|player:NAME] [secs]`.
pub(super) async fn expect_buff_removed(
    game: &mut GameClient,
    buff: u32,
    spec: Option<&str>,
    secs: u64,
) -> anyhow::Result<()> {
    let target = buff_target(game, spec)?;
    anyhow::ensure!(
        game.state
            .buffs
            .get(&(target, buff))
            .is_some_and(|indices| !indices.is_empty()),
        "expected buff {buff} on {target} to be present before waiting for its removal"
    );
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if game
            .state
            .buffs
            .get(&(target, buff))
            .is_none_or(|indices| indices.is_empty())
        {
            println!("    buff {buff} on {target} removed");
            return Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("expected buff {buff} on {target} to be removed within {secs} s; it was not");
        }
        game.recv_within(left.min(Duration::from_millis(250)))
            .await?;
    }
}

/// `expecthpdrop` / `expectnohpdrop`: watch the unit's last `SCUnitPointsPacket` HP against
/// the value seen when the command starts. `expectnohpdrop` fails as soon as the HP falls inside
/// the window; `expecthpdrop` fails when it does not. The server sends no `SCUnitPointsPacket`
/// at spawn, so a unit whose HP was never updated is "unknown": run a `gm heal` on it first.
/// Natural regeneration can refill a hit before the next packet, so a very small hit may go
/// unseen.
pub(super) async fn expect_hp_drop(
    game: &mut GameClient,
    spec: Option<&str>,
    secs: u64,
    want_drop: bool,
) -> anyhow::Result<()> {
    let target = buff_target(game, spec)?;
    let deadline = Instant::now() + Duration::from_secs(secs);
    let baseline = game.state.unit_points.get(&target).map(|p| p.0);
    anyhow::ensure!(
        baseline.is_some(),
        "no SCUnitPointsPacket for unit {target} yet (hp unknown)"
    );
    loop {
        let hp = game.state.unit_points.get(&target).map(|p| p.0);
        let dropped = matches!((baseline, hp), (Some(b), Some(n)) if n < b);
        if !want_drop && dropped {
            bail!("hp of {target} fell {baseline:?} -> {hp:?} (expected no HP drop)");
        }
        if want_drop && dropped {
            println!("    hp of {target}: {baseline:?} -> {hp:?}");
            return Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            if want_drop {
                bail!("no HP drop for unit {target} within {secs} s (hp {hp:?})");
            }
            println!("    no HP drop on {target} in {secs} s");
            return Ok(());
        }
        let wait = left.min(Duration::from_millis(250));
        game.recv_within(wait).await?;
    }
}

/// `expectpkt <PacketName> [secs]`.
pub(super) async fn expect_packet(
    game: &mut GameClient,
    want: &str,
    secs: u64,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("expected packet {want} within {secs} s; it did not arrive");
        }
        if let Some(p) = game
            .recv_within(left.min(Duration::from_millis(250)))
            .await?
        {
            if p.name().eq_ignore_ascii_case(want) {
                println!("    got {want}");
                return Ok(());
            }
        }
    }
}

/// `expectnopkt <PacketName> [secs]`.
pub(super) async fn expect_no_packet(
    game: &mut GameClient,
    want: &str,
    secs: u64,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            println!("    no {want} in {secs} s");
            return Ok(());
        }
        if let Some(p) = game
            .recv_within(left.min(Duration::from_millis(250)))
            .await?
        {
            if p.name().eq_ignore_ascii_case(want) {
                bail!("unexpected packet {want} within {secs} s");
            }
        }
    }
}

/// `(chat type, message)` of an `SCChatMessagePacket`.
pub(super) fn chat_type_and_text(p: &Packet) -> Option<(i16, String)> {
    if !p.is_sc(sc::SCChatMessagePacket) {
        return None;
    }
    let mut s = p.stream();
    let chat_type = s.read_i16().ok()?;
    s.read_i16().ok()?;
    s.read_u32().ok()?;
    s.read_bc().ok()?;
    s.read_u32().ok()?;
    s.read_u8().ok()?;
    s.read_u8().ok()?;
    s.read_u32().ok()?;
    s.read_string().ok()?;
    Some((chat_type, s.read_string().ok()?))
}

/// `expectchat <chat type> "<text>" [secs]`: an `SCChatMessagePacket` with that chat type and
/// message text (the sender block is skipped like `questitem::chat_text`).
pub(super) async fn expect_chat(
    game: &mut GameClient,
    chat_type: i16,
    text: &str,
    secs: u64,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("expected chat type {chat_type} '{text}' within {secs} s; it did not arrive");
        }
        let Some(p) = game
            .recv_within(left.min(Duration::from_millis(250)))
            .await?
        else {
            continue;
        };
        if chat_type_and_text(&p).is_some_and(|(t, m)| t == chat_type && m == text) {
            println!("    got chat type {chat_type} '{text}'");
            return Ok(());
        }
    }
}

/// `(reason, causedByMe)` of an `SCTradeCanceledPacket`.
pub(super) fn trade_cancel_reason(p: &Packet) -> Option<(i32, bool)> {
    if !p.is_sc(sc::SCTradeCanceledPacket) {
        return None;
    }
    let mut s = p.stream();
    Some((s.read_i32().ok()?, s.read_bool().ok()?))
}

/// `expecttradecancel <reason> <caused 0|1> [secs]`: an `SCTradeCanceledPacket` with exactly
/// that reason and causedByMe flag (`i32 reason, bool causedByMe`).
pub(super) async fn expect_trade_canceled(
    game: &mut GameClient,
    reason: i32,
    caused: bool,
    secs: u64,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!(
                "expected SCTradeCanceled(reason {reason}, caused {caused}) within {secs} s; it \
                 did not arrive"
            );
        }
        let Some(p) = game
            .recv_within(left.min(Duration::from_millis(250)))
            .await?
        else {
            continue;
        };
        let Some((got_reason, got_caused)) = trade_cancel_reason(&p) else {
            continue;
        };
        if got_reason == reason && got_caused == caused {
            println!("    got SCTradeCanceled(reason {got_reason}, caused {got_caused})");
            return Ok(());
        }
        bail!(
            "SCTradeCanceled reason {got_reason} caused {got_caused}, expected reason {reason} \
             caused {caused}"
        );
    }
}

/// `expectlog <regex> [secs]`: a line written after the command started must match.
pub(super) async fn expect_log(
    game: &mut GameClient,
    pattern: &str,
    secs: u64,
) -> anyhow::Result<()> {
    let path = std::env::var("AAEMU_VERIFY_GAME_LOG")
        .context("set AAEMU_VERIFY_GAME_LOG to the game server log")?;
    let path = std::path::Path::new(&path);
    let re = regex::Regex::new(pattern).with_context(|| format!("bad regex {pattern:?}"))?;
    let mut offset = std::fs::metadata(path)
        .with_context(|| format!("reading game log {path:?}"))?
        .len();
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let text = read_log_since(path, &mut offset)?;
        if let Some(line) = text.lines().find(|l| re.is_match(l)) {
            println!("    log: {line}");
            return Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("no new line matching /{pattern}/ within {secs} s in {path:?}");
        }
        // Keep the session alive (and the packet state fresh) while polling.
        game.recv_within(left.min(Duration::from_millis(200)))
            .await?;
    }
}

/// `expectdb "<sql>" <expected> [secs]`: the SQL's single value must equal `expected`
/// (re-checked every 250 ms when `secs` is given).
pub(super) async fn expect_db(sql: &str, expected: &str, secs: u64) -> anyhow::Result<()> {
    let spec = std::env::var("AAEMU_VERIFY_MYSQL")
        .context("set AAEMU_VERIFY_MYSQL to 'mysql.exe <args...> <database>'")?;
    let mut parts = split_quoted(&spec)?;
    anyhow::ensure!(!parts.is_empty(), "AAEMU_VERIFY_MYSQL is empty");
    let exe = parts.remove(0);
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        // mysql.exe blocks: run it off the async runtime so the bot's connection keeps going.
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(&parts).args(["-N", "-B", "-e", sql]);
        let out = tokio::task::spawn_blocking(move || cmd.output())
            .await?
            .with_context(|| format!("running {exe}"))?;
        if !out.status.success() {
            bail!(
                "mysql failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let got = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if got == expected {
            println!("    db {sql:?} = {got}");
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("expected {expected:?} from {sql:?}; got {got:?}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
