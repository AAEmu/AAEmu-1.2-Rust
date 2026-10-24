//! `craft`: a craft batch that runs out of labor must end with the error the 1.2 client needs to
//! unlock its craft window (GM account needed, see `rust/LIVE-TESTING.md`).
//!
//! The bot spawns an Archeum Workbench (doodad 566) next to itself with `/spawn doodad`, adds
//! Gilda Stars (item 23633), tops its labor up by [`LABOR_TOP_UP`] (`/labor`; a GM command can
//! only add labor, so the batch is sized to run past whatever the character has) and sends
//! `CSExecuteCraft` for craft 8000001 "Disintegration: Gilda Stars" (skill 8000016, 3 labor per
//! craft) with a count larger than the labor allows. The client resets its batch (greyed-out Craft
//! button, `IsWorkingCraft`) only on `SCSkillStarted` with a nonzero result or `SCErrorMsg`
//! 148 (`CraftCantActAnyMore`) with a nonzero second code and `isNotify == false`.
//!
//! Asserted: at least [`MIN_CRAFTS`] crafts complete (`SCSkillStarted` + `SCSkillEnded`), the last
//! client-visible stop is that `SCErrorMsg` (148, reason `NotEnoughLaborPower` 29, not notify), and
//! no cast starts after it. The server log must show `craft 8000001 refused by skill 8000016`.

use std::time::{Duration, Instant};

use aaemu_commons::PacketStream;
use aaemu_game::opcodes::{cs, sc};
use anyhow::{ensure, Context};

use super::questitem::{chat_text, gm_command};
use super::{Runner, QUIET};
use crate::game::{GameClient, Packet};

/// Craft 8000001 "Disintegration: Gilda Stars" (1 Gilda Star -> 4 Gilda Dust).
const CRAFT: u32 = 8_000_001;
/// Item 23633 "Gilda Star".
const MATERIAL: u32 = 23_633;
/// Doodad 566 "Archeum Workbench".
const BENCH: u32 = 566;
/// Labor added before the batch so at least [`MIN_CRAFTS`] crafts are affordable (3 labor each).
const LABOR_TOP_UP: i32 = 12;
/// Batch size asked for (and Gilda Stars given): more crafts than a new character has labor for.
const COUNT: i32 = 100;
/// Longest the batch may take (about 1.5 s per craft).
const BATCH_TIMEOUT: Duration = Duration::from_secs(240);
/// Crafts that must succeed before the labor runs out.
const MIN_CRAFTS: usize = 2;
/// `ErrorMessageType.CraftCantActAnyMore` and `NotEnoughLaborPower`.
const CRAFT_CANT_ACT_ANY_MORE: i16 = 148;
const NOT_ENOUGH_LABOR_POWER: i16 = 29;

/// One craft-relevant packet the client saw during the batch.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Seen {
    SkillStarted,
    SkillEnded,
    /// `SCErrorMsg`: error1, error2, notify.
    Error(i16, i16, bool),
}

/// `SCErrorMsgPacket`: i16 error1, i16 error2, u32 kind, bool notify.
pub(super) fn parse_error(p: &Packet) -> Option<(i16, i16, bool)> {
    let mut s = p.stream();
    let e1 = s.read_i16().ok()?;
    let e2 = s.read_i16().ok()?;
    s.read_u32().ok()?;
    let notify = s.read_u8().ok()? != 0;
    Some((e1, e2, notify))
}

/// The craft-relevant packets of `packets`, in arrival order.
pub(super) fn classify(packets: &[Packet]) -> Vec<Seen> {
    packets
        .iter()
        .filter_map(|p| {
            if p.is_sc(sc::SCSkillStartedPacket) {
                Some(Seen::SkillStarted)
            } else if p.is_sc(sc::SCSkillEndedPacket) {
                Some(Seen::SkillEnded)
            } else if p.is_sc(sc::SCErrorMsgPacket) {
                parse_error(p).map(|(a, b, c)| Seen::Error(a, b, c))
            } else {
                None
            }
        })
        .collect()
}

/// What the batch must look like to the client: `min_crafts` completed casts, then the
/// unlocking error (148, `reason`, not notify) as the last error, and no cast after it.
pub(super) fn check_batch(seen: &[Seen], min_crafts: usize, reason: i16) -> Result<usize, String> {
    let Some(stop) = seen.iter().rposition(|s| matches!(s, Seen::Error(..))) else {
        return Err(format!("no SCErrorMsg ended the batch: {seen:?}"));
    };
    if let Seen::Error(e1, e2, notify) = seen[stop] {
        if e1 != CRAFT_CANT_ACT_ANY_MORE || e2 != reason || notify {
            return Err(format!(
                "last SCErrorMsg is ({e1}, {e2}, notify {notify}), want (148, {reason}, notify false)"
            ));
        }
    }
    if let Some(after) = seen[stop + 1..]
        .iter()
        .find(|s| matches!(s, Seen::SkillStarted))
    {
        return Err(format!("a cast started after the batch ended: {after:?}"));
    }
    let crafts = seen[..stop]
        .iter()
        .filter(|s| matches!(s, Seen::SkillEnded))
        .count();
    if crafts < min_crafts {
        return Err(format!(
            "only {crafts} crafts completed, want {min_crafts}: {seen:?}"
        ));
    }
    Ok(crafts)
}

pub(super) async fn run(game: &mut GameClient, r: &mut Runner<'_>) -> anyhow::Result<()> {
    let s = r.begin("game.craft.gm");
    let reply = gm_command(game, "quest list").await?;
    let texts: Vec<String> = reply.iter().filter_map(chat_text).collect();
    ensure!(
        !texts.iter().any(|t| t.contains("Insufficient privileges")),
        "GM commands are refused: account {} needs access level 100",
        game.account_id
    );
    gm_command(game, "revive").await?;
    r.end(s, "GM commands accepted");

    let s = r.begin("game.craft.setup");
    let before: Vec<u32> = game.state.doodads.keys().copied().collect();
    let reply = gm_command(game, &format!("spawn doodad {BENCH}")).await?;
    println!(
        "[craft] /spawn doodad {BENCH}: {:?}",
        reply.iter().filter_map(chat_text).collect::<Vec<_>>()
    );
    game.drain(QUIET, Duration::from_secs(4)).await?;
    let me = game.state.self_pos;
    let bench = game
        .state
        .doodads
        .values()
        .filter(|d| d.template_id == BENCH && !before.contains(&d.obj_id))
        .min_by(|a, b| dist(me, a.pos).total_cmp(&dist(me, b.pos)))
        .cloned()
        .context("the spawned Archeum Workbench was not announced")?;
    gm_command(game, &format!("item add self {MATERIAL} {COUNT}")).await?;
    gm_command(game, &format!("labor {LABOR_TOP_UP}")).await?;
    r.end(
        s,
        format!(
            "bench obj {} at {:?} ({:.1} m), {COUNT} Gilda Stars, labor +{LABOR_TOP_UP}",
            bench.obj_id,
            bench.pos,
            dist(me, bench.pos)
        ),
    );

    let s = r.begin("game.craft.batch");
    let mut body = PacketStream::new();
    body.write_u32(CRAFT)
        .write_bc(bench.obj_id)
        .write_i32(COUNT);
    game.send_cs(cs::CSExecuteCraft, body.as_bytes()).await?;
    let started = Instant::now();
    let mut packets = Vec::new();
    // The batch (about 1.5 s per craft), then a quiet period that must stay free of casts.
    let mut last_craft_packet = Instant::now();
    while started.elapsed() < BATCH_TIMEOUT {
        if let Some(p) = game.recv_within(Duration::from_millis(250)).await? {
            let is_error = p.is_sc(sc::SCErrorMsgPacket);
            if p.is_sc(sc::SCSkillStartedPacket) || p.is_sc(sc::SCSkillEndedPacket) || is_error {
                println!(
                    "[craft] +{:>5} ms {}{}",
                    started.elapsed().as_millis(),
                    p.name(),
                    parse_error(&p)
                        .filter(|_| is_error)
                        .map_or(String::new(), |e| format!(" {e:?}"))
                );
                last_craft_packet = Instant::now();
            }
            packets.push(p);
        }
        let stopped = classify(&packets)
            .iter()
            .any(|s| matches!(s, Seen::Error(CRAFT_CANT_ACT_ANY_MORE, ..)));
        if stopped && last_craft_packet.elapsed() > Duration::from_secs(8) {
            break;
        }
    }
    match check_batch(&classify(&packets), MIN_CRAFTS, NOT_ENOUGH_LABOR_POWER) {
        Ok(crafts) => r.end(
            s,
            format!(
                "{crafts} crafts completed, then SCErrorMsg 148/29 notify=false, no cast after"
            ),
        ),
        Err(why) => r.fail(s, why),
    }

    let s = r.begin("game.leave");
    game.leave_world(0).await?;
    game.wait_sc(sc::SCPrepareLeaveWorldPacket).await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await
        .ok();
    r.end(s, "left");
    Ok(())
}

fn dist(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    let (dx, dy, dz) = (a.0 - b.0, a.1 - b.1, a.2 - b.2);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    use Seen::*;

    #[test]
    fn batch_ending_in_the_unlocking_error_passes() {
        let seen = [
            SkillStarted,
            SkillEnded,
            SkillStarted,
            SkillEnded,
            Error(148, 29, false),
        ];
        assert_eq!(check_batch(&seen, 2, 29), Ok(2));
    }

    #[test]
    fn silent_stop_notify_zero_reason_and_later_casts_fail() {
        let prefix = || vec![SkillStarted, SkillEnded, SkillStarted, SkillEnded];
        assert!(check_batch(&prefix(), 2, 29).is_err(), "silent stop");
        for bad in [
            Error(148, 29, true),
            Error(148, 0, false),
            Error(147, 29, false),
        ] {
            let mut seen = prefix();
            seen.push(bad);
            assert!(check_batch(&seen, 2, 29).is_err());
        }
        let mut later = prefix();
        later.extend([Error(148, 29, false), SkillStarted]);
        assert!(check_batch(&later, 2, 29).is_err());
        let few = [SkillStarted, SkillEnded, Error(148, 29, false)];
        assert!(check_batch(&few, 2, 29).is_err());
    }

    #[test]
    fn error_packets_are_parsed() {
        let mut s = PacketStream::new();
        s.write_i16(148).write_i16(29).write_u32(0).write_u8(0);
        let p = Packet {
            level: 1,
            opcode: sc::SCErrorMsgPacket,
            body: s.into_bytes(),
        };
        assert_eq!(parse_error(&p), Some((148, 29, false)));
        assert_eq!(classify(&[p]), vec![Error(148, 29, false)]);
    }
}
