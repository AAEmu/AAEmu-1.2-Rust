use std::time::{Duration, Instant};

use aaemu_commons::PacketStream;
use aaemu_game::opcodes::sc;
use aaemu_game::systems::quests::model::status;
use anyhow::{bail, ensure, Context};

use super::{packet_obj_id, pump, wait_for_giver, BotConfig, Runner, QUIET};
use crate::game::{GameClient, Packet};

/// `npc-return`: accept the configured quest at its resident NPC giver, leave the giver beyond
/// the warm/trigger horizon for longer than the warm-retention window, then walk back to the
/// spawn and require the same live giver instance to be announced again while the quest is
/// still active.
///
/// The residency check (same object id + template, fresh `SCUnitStatePacket`) stays on one
/// connection. The quest-state readback re-selects the character on that same connection
/// (logout to character select, then select again): the `SCQuestsPacket` snapshot then proves
/// the quest survived the away/return cycle.
pub(super) async fn npc_return(
    game: &mut GameClient,
    cfg: &BotConfig,
    r: &mut Runner<'_>,
) -> anyhow::Result<()> {
    let p0 = game.state.self_pos;
    let self_obj = game.state.self_obj;
    let char_id = game.state.selected_char;

    // ---- accept the quest at the configured resident giver ----
    let s = r.begin("game.npc-return.accept");
    let giver = wait_for_giver(game, cfg.quest_npc_template, Duration::from_secs(10)).await?;
    let giver_obj = giver.obj_id;
    game.move_to((giver.pos.0 + 1.0, giver.pos.1, giver.pos.2), 3000)
        .await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;
    game.start_quest(cfg.quest_id, giver_obj).await?;
    // No answer at all when the quest cannot be started (e.g. already taken).
    let answers = game
        .drain(Duration::from_millis(800), Duration::from_secs(4))
        .await?;
    if let Some(err) = answers.iter().find(|p| p.is_sc(sc::SCErrorMsgPacket)) {
        let code = err.stream().read_i16().unwrap_or(0);
        bail!(
            "start_quest({}) on giver {giver_obj} was rejected (SCErrorMsg {code})",
            cfg.quest_id
        );
    }
    let confirmed = answers.iter().any(|p| {
        (p.is_sc(sc::SCQuestContextStartedPacket) || p.is_sc(sc::SCQuestContextUpdatedPacket))
            && quest_template_id(p).ok() == Some(cfg.quest_id)
    });
    ensure!(
        confirmed,
        "start_quest({}) on giver {giver_obj} got no SCQuestContextStarted/Updated carrying it: \
         this character probably already has quest {}; run npc-return with a fresh disposable \
         character",
        cfg.quest_id,
        cfg.quest_id
    );
    r.var("giver_obj", giver_obj);
    r.var("giver_template", giver.id);
    r.end(
        s,
        format!(
            "giver {giver_obj} (template {}); quest {} started (SCQuestContextStarted/Updated carried it)",
            giver.id, cfg.quest_id
        ),
    );

    // ---- leave beyond the warm/trigger horizon and outlast the warm retention ----
    let s = r.begin("game.npc-return.away");
    ensure!(
        cfg.npc_return_distance > 0.0,
        "npc_return_distance must be positive"
    );
    let far = (p0.0 + cfg.npc_return_distance, p0.1, p0.2);
    game.move_to(far, 60_000).await?;
    // The server moves our unit and echoes the movement to us; the echoed transform is the
    // evidence that the leave actually happened server-side.
    game.wait_for("own SCOneUnitMovementPacket (away)", |p| {
        p.is_sc(sc::SCOneUnitMovementPacket) && packet_obj_id(p).ok() == Some(self_obj)
    })
    .await?;
    game.drain(Duration::from_millis(500), Duration::from_secs(3))
        .await?;
    let at = game.state.units.get(&self_obj).map(|u| u.pos);
    ensure!(
        at.is_some_and(|u| dist3(u, far) <= 5.0),
        "server did not apply the away move to {far:?}: own unit state {at:?}"
    );
    ensure!(
        !game.state.units.contains_key(&giver_obj),
        "giver {giver_obj} is still visible {} m away (warm horizon not left)",
        cfg.npc_return_distance
    );
    let pumped = pump(game, cfg.npc_return_away_secs).await?;
    ensure!(!game.state.disconnected, "connection lost while away");
    r.var("npc_return_distance", cfg.npc_return_distance as u64);
    r.end(
        s,
        format!(
            "{:.0} m away at {far:?}, {} s away, {} packets pumped",
            cfg.npc_return_distance,
            cfg.npc_return_away_secs,
            pumped.len()
        ),
    );

    // ---- come back: the resident giver must be announced again, same live instance ----
    let s = r.begin("game.npc-return.return");
    game.move_to(p0, 60_000).await?;
    // The giver's SCUnitStatePacket can arrive before our own movement echo, so packet order
    // cannot be relied on (a waiting scan would consume the announcement first). `observe`
    // updates the unit map for every received packet regardless of order: the echo moves our
    // own entry back to the spawn, and the announcement re-inserts the giver (which was
    // asserted gone while away). Wait for both, then validate the entries below.
    let return_deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let at = game.state.units.get(&self_obj).map(|u| u.pos);
        let giver_back = game.state.units.contains_key(&giver_obj);
        if at.is_some_and(|u| dist3(u, p0) <= 5.0) && giver_back {
            break;
        }
        if Instant::now() >= return_deadline {
            bail!(
                "no return within 20 s: own transform {at:?} (expected near {p0:?}), \
                 giver {giver_obj} visible: {giver_back}"
            );
        }
        game.drain(Duration::from_millis(500), Duration::from_secs(1))
            .await?;
    }
    let unit = game
        .state
        .units
        .get(&giver_obj)
        .context("giver missing from unit state after its SCUnitStatePacket")?;
    ensure!(
        unit.unit_type == 1 && unit.id == cfg.quest_npc_template,
        "returned unit {giver_obj} has type {} / template {}, expected NPC template {}",
        unit.unit_type,
        unit.id,
        cfg.quest_npc_template
    );
    let pos = unit.pos;
    r.end(
        s,
        format!(
            "same giver {giver_obj} (template {}) re-announced at {pos:?}",
            unit.id
        ),
    );

    // ---- the quest is still active on a fresh character snapshot ----
    let s = r.begin("game.npc-return.quest");
    game.leave_world(1).await?;
    game.wait_sc(sc::SCPrepareLeaveWorldPacket).await?;
    game.wait_sc(sc::SCLeaveWorldGrantedPacket).await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;
    game.list_characters().await?;
    game.wait_for("SCCharacterListPacket", |p| {
        p.is_sc(sc::SCCharacterListPacket) && p.body.first() == Some(&1)
    })
    .await?;
    game.select_character(char_id).await?;
    // Collect the whole select snapshot until a quiet gap after SCCharacterStatePacket: waiting
    // for that packet alone could swallow an SCQuestsPacket that arrived just before it.
    let mut snapshot = Vec::new();
    let snapshot_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let batch = game.drain(QUIET, Duration::from_secs(10)).await?;
        let quiet = batch.is_empty();
        snapshot.extend(batch);
        let state_seen = snapshot.iter().any(|p| p.is_sc(sc::SCCharacterStatePacket));
        if state_seen && quiet {
            break;
        }
        if Instant::now() >= snapshot_deadline {
            bail!(
                "no complete character snapshot after re-selecting character {char_id} \
                 (SCCharacterStatePacket seen: {state_seen})"
            );
        }
    }
    let row = snapshot
        .iter()
        .filter(|p| p.is_sc(sc::SCQuestsPacket))
        .filter_map(|p| quest_rows(&p.body).ok())
        .flatten()
        .find(|(template, _)| *template == cfg.quest_id);
    let (template, st) = row.with_context(|| {
        format!(
            "quest {} is not in SCQuestsPacket after the away/return cycle",
            cfg.quest_id
        )
    })?;
    ensure!(
        matches!(st, status::PROGRESS | status::READY),
        "quest {template} is no longer active after the away/return cycle (status {st})"
    );
    r.end(s, format!("quest {template} still active (status {st})"));

    // The re-select snapshot leaves the connection in the character-select lobby, where the
    // server ignores `CSLeaveWorldPacket` (`EnterWorldManager.Leave` only acts in
    // `GameState.World`). Complete the same spawn -> notify handshake as the initial spawn
    // before the final leave.
    let s = r.begin("game.npc-return.respawn");
    game.spawn_character().await?;
    game.wait_for("own SCUnitStatePacket (re-select)", |p| {
        p.is_sc(sc::SCUnitStatePacket)
    })
    .await?;
    game.notify_in_game().await?;
    game.notify_in_game_completed().await?;
    game.drain(QUIET, Duration::from_secs(15)).await?;
    ensure!(
        game.state.self_obj == self_obj,
        "re-select spawn announced obj {} instead of the original {self_obj}",
        game.state.self_obj
    );
    r.end(
        s,
        format!("obj {self_obj} spawned again at {:?}", game.state.self_pos),
    );

    let s = r.begin("game.leave");
    game.leave_world(0).await?;
    let prep = game.wait_sc(sc::SCPrepareLeaveWorldPacket).await?;
    let logout_ms = prep.stream().read_i32().unwrap_or(0);
    if cfg.wait_leave_granted && logout_ms <= 10_000 {
        game.wait_sc(sc::SCLeaveWorldGrantedPacket).await?;
    }
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await
        .ok();
    r.end(s, "left");
    Ok(())
}

fn dist3(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    let (dx, dy, dz) = (a.0 - b.0, a.1 - b.1, a.2 - b.2);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// Quest template id of an `SCQuestContextStartedPacket` / `SCQuestContextUpdatedPacket` body
/// (the second field after the quest instance id written by `Quest.Write`).
pub(super) fn quest_template_id(p: &Packet) -> anyhow::Result<u32> {
    let mut s = p.stream();
    s.read_i64()?;
    Ok(s.read_u32()?)
}

/// One `ActiveQuest` row as written by `Quest.Write` (`systems/quests/packets.rs`), reduced to
/// the fields the readback asserts: `(template_id, status)`.
pub(super) fn read_quest_row(s: &mut PacketStream) -> anyhow::Result<(u32, u8)> {
    s.read_i64()?; // quest instance id (long)
    let template = s.read_u32()?;
    let status = s.read_u8()?;
    for _ in 0..5 {
        s.read_i32()?; // objectives
    }
    s.read_bool()?; // isCheckSet
    s.read_bc()?; // obj id
    s.read_u32()?; // type id
    s.read_bc()?; // obj id
    s.read_bc()?; // obj id
    s.read_i32()?; // left time
    s.read_u32()?; // component id (0 without a time limit)
    s.read_i64()?; // doodad id
    s.read_i64()?; // accept time (seconds)
    s.read_u8()?; // acceptor type
    s.read_u32()?; // acceptor id
    Ok((template, status))
}

/// `SCQuestsPacket` body (`[i32 count][ActiveQuest...]`) as `(template_id, status)` rows. The
/// count is untrusted: negatives are rejected and the rows are parsed incrementally (no
/// capacity reservation from the wire value).
pub(super) fn quest_rows(body: &[u8]) -> anyhow::Result<Vec<(u32, u8)>> {
    let mut s = PacketStream::from_bytes(body.to_vec());
    let count = s.read_i32()?;
    ensure!(count >= 0, "negative SCQuestsPacket quest count {count}");
    let mut rows = Vec::new();
    for _ in 0..count {
        rows.push(read_quest_row(&mut s)?);
    }
    Ok(rows)
}
