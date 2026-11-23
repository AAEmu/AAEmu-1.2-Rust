use std::time::{Duration, Instant};

use aaemu_commons::PacketStream;
use aaemu_game::opcodes::sc;
use anyhow::{bail, ensure, Context};

use super::{packet_obj_id, packet_summary, wait_for_giver, BotConfig, ReviveMethod, Runner};
use crate::game::{GameClient, GameEvent, ItemTaskInfo};

/// A mate as seen by its owner after it spawned.
#[derive(Clone, Copy, Debug)]
pub(super) struct MateView {
    pub(super) obj_id: u32,
    pub(super) tl_id: u16,
    pub(super) hp: i32,
    dead: bool,
}

/// `mate-revive`: the stablehand dead-mount flow (summon, kill, dismiss, corpse summon refused,
/// interact with the stablehand, revive request, re-summon). Checks that do not stop the flow
/// (item detail dead byte, refused summon, revive answers, re-summon alive) are recorded as
/// failed steps and the run continues to the clean-up, so one run reports everything a server
/// gets wrong. Needs a GM account (`/item add`, `/gold add`: access level 100, `/kill`: 50).
pub(super) async fn mate_revive(
    game: &mut GameClient,
    cfg: &BotConfig,
    r: &mut Runner<'_>,
) -> anyhow::Result<()> {
    let p0 = game.state.self_pos;
    let default_timeout = game.timeout;

    // ---- a summon item and some money for the revive fee ----
    let s = r.begin("game.mate-revive.item");
    game.timeout = Duration::from_secs(8);
    game.chat(0, &format!("/item add self {} 1", cfg.mate_item_template))
        .await?;
    let added = game
        .wait_for("SCItemTaskSuccessPacket for /item add", |p| {
            p.is_sc(sc::SCItemTaskSuccessPacket)
                && added_items(&p.body)
                    .iter()
                    .any(|(tpl, _)| *tpl == cfg.mate_item_template)
        })
        .await
        .with_context(|| {
            format!(
                "`/item add self {}` did not add the item: the account needs GM access \
                 (accounts.access_level >= 100 in the game database)",
                cfg.mate_item_template
            )
        })?;
    game.timeout = default_timeout;
    let item_id = added_items(&added.body)
        .into_iter()
        .find(|(tpl, _)| *tpl == cfg.mate_item_template)
        .map(|(_, id)| id)
        .context("item id missing")?;
    // 100 gold for the stablehand fee; the answer (if any) is not needed.
    game.chat(0, "/gold add self 100").await?;
    game.drain(Duration::from_millis(500), Duration::from_secs(3))
        .await?;
    r.var("mate_item_id", item_id);
    r.end(
        s,
        format!("item {item_id} (template {})", cfg.mate_item_template),
    );

    // ---- summon, expect a live mate ----
    let s = r.begin("game.mate-revive.summon");
    let first = summon_mate(game, cfg, item_id, None).await?;
    ensure!(
        !first.dead && first.hp > 0,
        "first summon already dead: {first:?}"
    );
    r.var("mate_obj", first.obj_id);
    r.var("mate_tl", first.tl_id);
    r.end(
        s,
        format!(
            "mate obj {} tl {} hp {} (SCMateSpawned + SCUnitState + SCUnitPoints)",
            first.obj_id, first.tl_id, first.hp
        ),
    );

    // ---- kill it with the GM command ----
    let s = r.begin("game.mate-revive.kill");
    let mark = game.state.events.len();
    game.change_target(first.obj_id).await?;
    game.chat(0, "/kill").await?;
    game.wait_for("SCUnitDeathPacket for the mate", |p| {
        p.is_sc(sc::SCUnitDeathPacket) && packet_obj_id(p).ok() == Some(first.obj_id)
    })
    .await
    .context("/kill did not kill the mate (GM access level 50 needed)")?;
    game.drain(Duration::from_millis(500), Duration::from_secs(3))
        .await?;
    ensure!(
        game.state.dead_units.contains(&first.obj_id),
        "mate {} not marked dead",
        first.obj_id
    );
    let hp = game.state.unit_points.get(&first.obj_id).map(|p| p.0);
    r.end(
        s,
        format!("mate {} dead, last SCUnitPoints hp {hp:?}", first.obj_id),
    );

    // ---- dismiss the corpse like the client does ----
    let s = r.begin("game.mate-revive.dismiss");
    game.remove_mate(first.tl_id).await?;
    let answers = game
        .drain(Duration::from_millis(1500), Duration::from_secs(5))
        .await?;
    // The server does not tell the owner about the removal (only other viewers get
    // SCUnitsRemoved), so the real check is the next summon: `spawn_mount` despawns a
    // still-active mate instead of spawning.
    game.state.units.remove(&first.obj_id);
    r.end(
        s,
        format!(
            "CSRemoveMate(tl {}) for corpse {}; answers: {}",
            first.tl_id,
            first.obj_id,
            packet_summary(&answers)
        ),
    );

    // ---- the item now carries the dead flag ----
    let s = r.begin("game.mate-revive.dead-flag");
    let flags = mate_flags(&game.state.events[mark..], item_id);
    let actions: Vec<u8> = item_tasks(&game.state.events[mark..])
        .map(|t| t.action)
        .collect();
    if flags.last().is_some_and(|(_, dead, _)| *dead != 0) {
        r.end(
            s,
            format!("item {item_id} detail (exp, dead, level) {flags:?}, task actions {actions:?}"),
        );
    } else {
        r.fail(
            s,
            format!(
                "item {item_id} never showed a non-zero mate dead flag after the death \
                 (details seen {flags:?}, SCItemTaskSuccess actions {actions:?}; MateDeath is {})",
                task_type::MATE_DEATH
            ),
        );
    }

    // ---- summoning the corpse item must be refused ----
    let s = r.begin("game.mate-revive.corpse-summon");
    let mark = game.state.events.len();
    let spawns = game.state.mate_spawn_count;
    game.use_item_skill(cfg.mate_item_skill_id, item_id, cfg.mate_item_template)
        .await?;
    game.drain(Duration::from_millis(1500), Duration::from_secs(6))
        .await?;
    let errors = error_codes(&game.state.events[mark..]);
    let spawned = game.state.mate_spawn_count > spawns;
    if errors.contains(&error_msg::MATE_CANNOT_SPAWN_WHILE_CORPSE) && !spawned {
        r.end(
            s,
            format!(
                "refused with SCErrorMsg {} (errors {errors:?}), no mate spawned",
                error_msg::MATE_CANNOT_SPAWN_WHILE_CORPSE
            ),
        );
    } else {
        r.fail(
            s,
            format!(
                "dead mate item summoned: SCErrorMsg codes {errors:?} (want {}), mate spawned: \
                 {spawned}",
                error_msg::MATE_CANNOT_SPAWN_WHILE_CORPSE
            ),
        );
    }
    if spawned {
        // Do not leave the (dead) mate out for the rest of the flow.
        if let Some(m) = game.state.mate_spawned {
            game.remove_mate(m.tl_id).await?;
            game.drain(Duration::from_millis(500), Duration::from_secs(3))
                .await?;
        }
    }

    // ---- walk to the stablehand and interact ----
    let s = r.begin("game.mate-revive.stablehand");
    let (sx, sy, sz) = cfg.stablehand_pos;
    game.move_to((sx + 1.0, sy, sz), 5000).await?;
    let stablehand = wait_for_giver(game, cfg.stablehand_npc_template, Duration::from_secs(30))
        .await
        .with_context(|| {
            format!(
                "stablehand NPC {} not announced near {:?}",
                cfg.stablehand_npc_template, cfg.stablehand_pos
            )
        })?;
    game.move_to(
        (stablehand.pos.0 + 1.0, stablehand.pos.1, stablehand.pos.2),
        5500,
    )
    .await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;
    game.interact_npc(stablehand.obj_id, true).await?;
    game.start_interaction(stablehand.obj_id).await?;
    let list = game
        .wait_for("SCNpcInteractionSkillListPacket", |p| {
            p.is_sc(sc::SCNpcInteractionSkillListPacket)
                && interaction_skill_list(&p.body).is_ok_and(|(npc, _)| npc == stablehand.obj_id)
        })
        .await?;
    let (_, skills) = interaction_skill_list(&list.body)?;
    r.var("stablehand_obj", stablehand.obj_id);
    let note = format!(
        "stablehand obj {} (template {}) offers {skills:?}",
        stablehand.obj_id, stablehand.id
    );
    // Informational only: the 1.2 client builds the stabler dialog from its own NPC data and
    // sends `CSRepairPetItems` regardless of this list (a merchant-stabler offers the merchant
    // skill here).
    if skills.contains(&cfg.revive_skill_id) {
        r.end(s, note);
    } else {
        r.end(
            s,
            format!(
                "{note} (no stabler skill {}; not used by the client)",
                cfg.revive_skill_id
            ),
        );
    }

    // ---- the revive request (isolated: see `send_revive_request`) ----
    let s = r.begin("game.mate-revive.revive");
    let mark = game.state.events.len();
    let sent = send_revive_request(game, cfg, stablehand.obj_id).await?;
    let answers = game
        .drain(Duration::from_millis(1500), Duration::from_secs(6))
        .await?;
    let events = &game.state.events[mark..];
    let errors = error_codes(events);
    let repair = item_tasks(events).find(|t| t.action == task_type::REPAIR_PETS);
    let cleared = mate_flags(events, item_id)
        .last()
        .is_some_and(|(_, dead, _)| *dead == 0);
    let money: i64 = item_tasks(events).map(|t| t.money).sum();
    let mut missing = Vec::new();
    if repair.is_none() {
        missing.push(format!(
            "SCItemTaskSuccess action {} (RepairPets)",
            task_type::REPAIR_PETS
        ));
    }
    if !cleared {
        missing.push("item detail dead byte 0".to_string());
    }
    if money >= 0 {
        missing.push("money deducted".to_string());
    }
    if !errors.contains(&error_msg::MATE_RESURRECTED) {
        missing.push(format!(
            "SCErrorMsg {} (MateResurrected)",
            error_msg::MATE_RESURRECTED
        ));
    }
    let note = format!(
        "{sent}; money change {money}, errors {errors:?}, answers: {}",
        packet_summary(&answers)
    );
    if missing.is_empty() {
        r.end(s, note);
    } else {
        r.fail(s, format!("{note}; missing: {}", missing.join(", ")));
    }

    // ---- re-summon: the mate must be alive ----
    let s = r.begin("game.mate-revive.resummon");
    let second = summon_mate(game, cfg, item_id, Some(first.obj_id)).await;
    match &second {
        Ok(m) if !m.dead && m.hp > 0 => r.end(
            s,
            format!(
                "mate obj {} tl {} spawned ALIVE, hp {}",
                m.obj_id, m.tl_id, m.hp
            ),
        ),
        Ok(m) => r.fail(
            s,
            format!(
                "mate obj {} tl {} spawned DEAD (hp {}, SCUnitDeath seen: {}): the stablehand \
                 revive did not restore it",
                m.obj_id, m.tl_id, m.hp, m.dead
            ),
        ),
        Err(e) => r.fail(s, format!("re-summon failed: {e:#}")),
    }

    // ---- clean up: dismiss whatever is out, walk back, log out ----
    let s = r.begin("game.mate-revive.cleanup");
    if let Ok(m) = second {
        game.remove_mate(m.tl_id).await?;
        game.drain(Duration::from_millis(500), Duration::from_secs(3))
            .await?;
    }
    game.move_to(p0, 60_000).await?;
    game.drain(Duration::from_millis(500), Duration::from_secs(3))
        .await?;
    r.end(s, "mate dismissed, back at the spawn");
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

/// `ItemTaskType` / `SCErrorMsg` codes the scenario asserts on.
mod task_type {
    pub const REPAIR_PETS: u8 = 58;
    pub const MATE_DEATH: u8 = 59;
}
mod error_msg {
    pub const MATE_CANNOT_SPAWN_WHILE_CORPSE: i16 = 106;
    pub const MATE_RESURRECTED: i16 = 111;
}

/// The `SCItemTaskSuccessPacket`s among `events`.
pub(super) fn item_tasks(events: &[GameEvent]) -> impl Iterator<Item = &ItemTaskInfo> {
    events.iter().filter_map(|e| match e {
        GameEvent::ItemTask(t) => Some(t),
        GameEvent::ErrorMsg(_) => None,
    })
}

/// The `SCErrorMsgPacket` codes among `events`.
pub(super) fn error_codes(events: &[GameEvent]) -> Vec<i16> {
    events
        .iter()
        .filter_map(|e| match e {
            GameEvent::ErrorMsg(c) => Some(*c),
            GameEvent::ItemTask(_) => None,
        })
        .collect()
}

/// `(exp, dead flag, level)` of every `SummonMate` detail block of item `item_id` in `events`,
/// in arrival order.
pub(super) fn mate_flags(events: &[GameEvent], item_id: u64) -> Vec<(i32, u8, u8)> {
    item_tasks(events)
        .flat_map(|t| t.items.iter())
        .filter(|i| i.id == item_id)
        .filter_map(|i| i.mate)
        .collect()
}

/// The revive request for the dead mate, sent while next to the stablehand. Returns a
/// description for the step note. Swap the body here once the real client packet is known.
async fn send_revive_request(
    game: &mut GameClient,
    cfg: &BotConfig,
    stablehand_obj: u32,
) -> anyhow::Result<String> {
    match cfg.revive_method {
        ReviveMethod::Repair => {
            game.repair_pet_items(stablehand_obj).await?;
            Ok(format!("CSRepairPetItems({stablehand_obj})"))
        }
        ReviveMethod::Skill => {
            game.start_skill(cfg.revive_skill_id, stablehand_obj)
                .await?;
            Ok(format!(
                "CSStartSkill({}) on stablehand {stablehand_obj}",
                cfg.revive_skill_id
            ))
        }
        ReviveMethod::None => Ok("no revive request sent".into()),
    }
}

/// Summons the mate of `item_id` through the item's use skill and waits until its unit state and
/// points arrived. `previous_obj` is a corpse that may still be in the unit map.
pub(super) async fn summon_mate(
    game: &mut GameClient,
    cfg: &BotConfig,
    item_id: u64,
    previous_obj: Option<u32>,
) -> anyhow::Result<MateView> {
    // Using the summon item of the mate that is already out dismisses it (the 1.2 toggle), so
    // a leftover active mate makes the first use spawn nothing; repeat once when no
    // SCMateSpawned follows.
    let spawned_before = game.state.mate_spawn_count;
    let default_timeout = game.timeout;
    game.timeout = Duration::from_secs(6);
    let mut got = None;
    for _attempt in 0..2 {
        game.use_item_skill(cfg.mate_item_skill_id, item_id, cfg.mate_item_template)
            .await?;
        let r = game
            .wait_for("SCMateSpawnedPacket", |p| {
                p.is_sc(sc::SCMateSpawnedPacket) || p.is_sc(sc::SCErrorMsgPacket)
            })
            .await;
        match r {
            Ok(p) => {
                got = Some(p);
                break;
            }
            Err(e) if game.state.disconnected => return Err(e),
            Err(_) => {}
        }
    }
    game.timeout = default_timeout;
    let got = got.context("no SCMateSpawnedPacket after two item uses")?;
    if got.is_sc(sc::SCErrorMsgPacket) {
        bail!(
            "summon rejected with SCErrorMsg {}",
            got.stream().read_i16().unwrap_or(0)
        );
    }
    ensure!(
        game.state.mate_spawn_count > spawned_before,
        "no new SCMateSpawned"
    );
    let spawn = game
        .state
        .mate_spawned
        .context("SCMateSpawned not parsed")?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let unit = game
            .state
            .units
            .values()
            .find(|u| u.unit_type == 5 && u.tl_id == spawn.tl_id && Some(u.obj_id) != previous_obj)
            .cloned();
        if let Some(u) = unit {
            if let Some(&(hp, _)) = game.state.unit_points.get(&u.obj_id) {
                // Let a trailing SCUnitDeath / points update land before judging.
                game.drain(Duration::from_millis(500), Duration::from_secs(2))
                    .await?;
                let hp = game.state.unit_points.get(&u.obj_id).map_or(hp, |p| p.0);
                return Ok(MateView {
                    obj_id: u.obj_id,
                    tl_id: spawn.tl_id,
                    hp,
                    dead: game.state.dead_units.contains(&u.obj_id),
                });
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "mate tl {} spawned but its unit state / points never arrived",
                spawn.tl_id
            );
        }
        game.drain(Duration::from_millis(300), Duration::from_secs(1))
            .await?;
    }
}

/// `(template id, item id)` of the `ItemAction.Create` items of an `SCItemTaskSuccessPacket`.
pub(super) fn added_items(body: &[u8]) -> Vec<(u32, u64)> {
    crate::game::parse_item_task(body)
        .items
        .into_iter()
        .filter(|i| i.task == 5)
        .map(|i| (i.template, i.id))
        .collect()
}

/// `(npc obj id, skill list)` of a `SCNpcInteractionSkillListPacket` body.
pub(super) fn interaction_skill_list(body: &[u8]) -> anyhow::Result<(u32, Vec<u32>)> {
    let mut s = PacketStream::from_bytes(body.to_vec());
    let npc = s.read_bc()?;
    s.read_bc()?; // obj id
    s.read_i32()?; // extra info
    s.read_i32()?; // pick id
    s.read_u8()?; // mouse button
    let count = s.read_i32()?;
    ensure!(
        (0..=256).contains(&count),
        "implausible skill count {count}"
    );
    let mut skills = Vec::new();
    for _ in 0..count {
        skills.push(s.read_u32()?);
    }
    Ok((npc, skills))
}
