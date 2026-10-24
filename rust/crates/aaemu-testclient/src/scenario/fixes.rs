//! `stonethrow` and `ghost`: live checks of two server log-noise fixes (GM account needed).
//!
//! * `stonethrow`: casts the stone-throwing skill 11268 (plot 349) from item 4056 / 24210, a
//!   stackable `use_skill_as_reagent` item the plot's effects consume. With a stack of 2 the cast
//!   ends with an `ItemUnlock` (`SCItemTaskSuccessPacket` task type 95) for the remaining stack;
//!   with the last stone the item is gone when the plot ends and the server must skip the unlock
//!   (it used to log `Plot: SkillSourceItem <id> no longer exists`).
//! * `ghost`: kills the ghost (NPC template 3483, spawner 3380 at Solzreed peninsula) with `/kill`,
//!   deletes the corpse with `/despawn npc <objId>` before its corpse timer runs out, and waits
//!   past the corpse timer and the 50 s respawn: exactly one ghost must come back, and the
//!   server must not log `not found in SpawnedNpcs` for the stale corpse timer.
//!
//! Both need access level 100 on the bot's account (see `rust/LIVE-TESTING.md` section 3). The
//! server-side assertions (log lines) are checked by whoever runs the scenario; the bot asserts
//! what the client can see.

use std::time::{Duration, Instant};

use aaemu_game::opcodes::sc;
use anyhow::{bail, ensure, Context};

use super::questitem::{chat_text, gm_command, stack_id};
use super::{own_skill_response, skill_response_result, Runner, QUIET};
use crate::game::{GameClient, GameEvent, UnitInfo};

/// The stone-throwing skill (plot 349) and the two stackable items that use it.
const STONE_SKILL: u32 = 11268;
const STONES: [u32; 2] = [4056, 24210];
/// `ItemTaskType.ItemUnlock`.
const ITEM_UNLOCK: u8 = 95;
/// The skill's range is 20 m.
const CAST_RANGE: f32 = 19.0;

/// Ghost NPC template 3483 of spawner 3380 and its placement in main_world.
const GHOST: u32 = 3483;
const GHOST_POS: [f32; 3] = [15149.0, 14959.0, 137.0];
/// Units of the ghost template within this many metres of [`GHOST_POS`] belong to spawner 3380.
const GHOST_SPOT: f32 = 4.0;
/// The respawn delay is 50 s; the whole watch lasts this long after the kill.
const GHOST_WATCH: Duration = Duration::from_secs(130);

pub(super) async fn gm_access(game: &mut GameClient) -> anyhow::Result<()> {
    let reply = gm_command(game, "quest list").await?;
    let texts: Vec<String> = reply.iter().filter_map(chat_text).collect();
    ensure!(
        !texts.iter().any(|t| t.contains("Insufficient privileges")),
        "GM commands are refused: account {} needs access level 100",
        game.account_id
    );
    ensure!(!texts.is_empty(), "no reply to the GM command /quest list");
    gm_command(game, "revive").await?;
    Ok(())
}

fn dist(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt()
}

/// The `ItemUnlock` actions among the item tasks that arrived after `events_before`.
fn unlocks_since(game: &GameClient, events_before: usize) -> usize {
    game.state.events[events_before..]
        .iter()
        .filter(|e| matches!(e, GameEvent::ItemTask(t) if t.action == ITEM_UNLOCK))
        .count()
}

pub(super) async fn stonethrow(game: &mut GameClient, r: &mut Runner<'_>) -> anyhow::Result<()> {
    let s = r.begin("game.stonethrow.gm");
    gm_access(game).await?;
    r.end(s, "GM commands accepted");

    // ---- a target in range ----
    let s = r.begin("game.stonethrow.target");
    let nearest = |game: &GameClient| -> Option<UnitInfo> {
        let me = game.state.self_pos;
        game.state
            .units
            .values()
            .filter(|u| u.unit_type == 1 && !game.state.dead_units.contains(&u.obj_id))
            .min_by(|a, b| dist(a.pos, me).total_cmp(&dist(b.pos, me)))
            .cloned()
    };
    // Teleport next to the nearest visible NPC when none is in range.
    let first = nearest(game).context("no NPC in sight")?;
    if dist(first.pos, game.state.self_pos) > CAST_RANGE {
        let (x, y, z) = first.pos;
        game.chat(0, &format!("/move {} {y} {z}", x + 3.0)).await?;
        game.wait_for("SCTeleportUnitPacket", |p| {
            p.is_sc(sc::SCTeleportUnitPacket)
        })
        .await?;
        let at = game.state.self_pos;
        game.teleport_ended(at).await?;
        game.move_to(at, 5000).await?;
        game.drain(QUIET, Duration::from_secs(5)).await?;
    }
    let me = game.state.self_pos;
    let npc: UnitInfo = nearest(game)
        .filter(|u| dist(u.pos, me) <= CAST_RANGE)
        .with_context(|| format!("no NPC within {CAST_RANGE} m of {me:?}"))?;
    r.end(
        s,
        format!(
            "npc {} (template {}) {:.1} m away",
            npc.obj_id,
            npc.id,
            dist(npc.pos, me)
        ),
    );

    for (round, template) in STONES.into_iter().enumerate() {
        // Round 0: 2 stones, the first cast leaves 1 (ItemUnlock expected), the second uses up
        // the last one (no ItemUnlock). Round 1: exactly 1 stone of the other item.
        let first_count: u32 = if round == 0 { 2 } else { 1 };
        let s = r.begin(&format!("game.stonethrow.item{template}"));
        let reply = gm_command(game, &format!("item add self {template} {first_count}")).await?;
        let stack = stack_id(&reply, template).with_context(|| {
            format!(
                "no item {template} was added (GM replies: {:?})",
                reply.iter().filter_map(chat_text).collect::<Vec<_>>()
            )
        })?;
        r.end(
            s,
            format!("{first_count} x item {template}, stack id {stack}"),
        );

        for cast in 1..=first_count {
            let last = cast == first_count;
            let s = r.begin(&format!("game.stonethrow.cast{template}.{cast}"));
            let before = game.state.events.len();
            // The skill is on a short cooldown: let the previous cast settle.
            tokio::time::sleep(Duration::from_secs(3)).await;
            game.change_target(npc.obj_id).await?;
            game.start_item_skill(STONE_SKILL, stack, template, npc.obj_id)
                .await?;
            let caster = game.state.self_obj;
            let saved = game.timeout;
            game.timeout = Duration::from_secs(8);
            let answer = game
                .wait_for("skill response", |p| {
                    own_skill_response(p, STONE_SKILL, caster) || p.is_sc(sc::SCErrorMsgPacket)
                })
                .await;
            game.timeout = saved;
            let answer = answer.with_context(|| {
                format!("cast {cast} of skill {STONE_SKILL} from item {stack} got no answer")
            })?;
            ensure!(
                !answer.is_sc(sc::SCErrorMsgPacket),
                "cast {cast} rejected with error message {:?}",
                answer.body
            );
            let result = skill_response_result(&answer)?;
            ensure!(
                result == 0,
                "cast {cast} answered with SkillResult {result}"
            );
            // The plot runs on; wait for its end and the item task that follows.
            game.timeout = Duration::from_secs(10);
            let ended = game
                .wait_for("plot end", |p| p.is_sc(sc::SCPlotEndedPacket))
                .await;
            game.timeout = saved;
            ended.context("no SCPlotEnded after the stone cast")?;
            game.drain(QUIET, Duration::from_secs(4)).await?;
            let unlocks = unlocks_since(game, before);
            let tasks: Vec<u8> = game.state.events[before..]
                .iter()
                .filter_map(|e| match e {
                    GameEvent::ItemTask(t) => Some(t.action),
                    GameEvent::ErrorMsg(_) => None,
                })
                .collect();
            if last {
                ensure!(
                    unlocks == 0,
                    "last stone: {unlocks} ItemUnlock(s) sent for a consumed item (item tasks {tasks:?})"
                );
            } else {
                ensure!(
                    unlocks == 1,
                    "stack of {first_count}: expected 1 ItemUnlock, saw {unlocks} (item tasks {tasks:?})"
                );
            }
            r.end(
                s,
                format!(
                    "SkillResult {result}, item task types {tasks:?}, ItemUnlock count {unlocks} \
                     ({})",
                    if last {
                        "expected 0: last stone"
                    } else {
                        "expected 1"
                    }
                ),
            );
        }
    }
    Ok(())
}

pub(super) async fn ghost(game: &mut GameClient, r: &mut Runner<'_>) -> anyhow::Result<()> {
    let spot = (GHOST_POS[0], GHOST_POS[1], GHOST_POS[2]);
    // Ghosts of the spawner 3380 placement, nearest first.
    let spawner_ghosts = |game: &GameClient| -> Vec<UnitInfo> {
        let mut v: Vec<UnitInfo> = game
            .state
            .units
            .values()
            .filter(|u| u.unit_type == 1 && u.id == GHOST && dist(u.pos, spot) <= GHOST_SPOT)
            .cloned()
            .collect();
        v.sort_by(|a, b| dist(a.pos, spot).total_cmp(&dist(b.pos, spot)));
        v
    };

    let s = r.begin("game.ghost.gm");
    gm_access(game).await?;
    r.end(s, "GM commands accepted");

    let s = r.begin("game.ghost.teleport");
    let [x, y, z] = GHOST_POS;
    game.chat(0, &format!("/move {x} {y} {z}")).await?;
    game.wait_for("SCTeleportUnitPacket", |p| {
        p.is_sc(sc::SCTeleportUnitPacket)
    })
    .await?;
    let at = game.state.self_pos;
    game.teleport_ended(at).await?;
    game.move_to(at, 5000).await?;
    let mut waited = 0;
    while spawner_ghosts(game).is_empty() {
        ensure!(
            waited < 30,
            "no ghost (template {GHOST}) within {GHOST_SPOT} m of {spot:?} became visible"
        );
        game.drain(Duration::from_millis(500), Duration::from_secs(1))
            .await?;
        waited += 1;
    }
    game.drain(QUIET, Duration::from_secs(4)).await?;
    let ghost = spawner_ghosts(game)
        .into_iter()
        .next()
        .context("the ghost left the view")?;
    r.end(
        s,
        format!("at {at:?}, ghost obj {} at {:?}", ghost.obj_id, ghost.pos),
    );

    // ---- kill, then delete the corpse before its timer fires ----
    let s = r.begin("game.ghost.kill");
    game.change_target(ghost.obj_id).await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(1))
        .await?;
    game.chat(0, "/kill").await?;
    let t_kill = Instant::now();
    let mut waited = 0;
    while !game.state.dead_units.contains(&ghost.obj_id) {
        ensure!(
            waited < 20,
            "ghost {} did not die after /kill",
            ghost.obj_id
        );
        game.drain(Duration::from_millis(500), Duration::from_secs(1))
            .await?;
        waited += 1;
    }
    r.end(s, format!("ghost {} dead", ghost.obj_id));

    let s = r.begin("game.ghost.despawn");
    let reply = gm_command(game, &format!("despawn npc {}", ghost.obj_id)).await?;
    let texts: Vec<String> = reply.iter().filter_map(chat_text).collect();
    ensure!(
        texts.iter().any(|t| t.contains("Removing NPC")),
        "/despawn npc {} was not accepted: {texts:?}",
        ghost.obj_id
    );
    let mut waited = 0;
    while game.state.units.contains_key(&ghost.obj_id) {
        ensure!(
            waited < 10,
            "corpse {} still in view after /despawn",
            ghost.obj_id
        );
        game.drain(Duration::from_millis(300), Duration::from_secs(1))
            .await?;
        waited += 1;
    }
    r.end(
        s,
        format!(
            "corpse {} deleted {} s after the kill",
            ghost.obj_id,
            t_kill.elapsed().as_secs()
        ),
    );

    // ---- watch the respawn ----
    let s = r.begin("game.ghost.respawn");
    let mut respawned_at = None;
    let mut max_ghosts = 0;
    let mut dead_self = 0;
    while t_kill.elapsed() < GHOST_WATCH {
        game.drain(Duration::from_millis(500), Duration::from_secs(2))
            .await?;
        let n = spawner_ghosts(game).len();
        max_ghosts = max_ghosts.max(n);
        if n > 0 && respawned_at.is_none() {
            respawned_at = Some(t_kill.elapsed());
        }
        if game.state.dead_units.contains(&game.state.self_obj) {
            // The respawned ghost killed the bot: stand up again, it is a GM test character.
            dead_self += 1;
            gm_command(game, "revive").await?;
        }
    }
    let Some(first) = respawned_at else {
        bail!(
            "no ghost respawned within {} s of the kill",
            GHOST_WATCH.as_secs()
        );
    };
    let now = spawner_ghosts(game);
    ensure!(
        now.len() == 1 && max_ghosts == 1,
        "expected exactly one ghost, saw {} now and at most {max_ghosts}",
        now.len()
    );
    r.end(
        s,
        format!(
            "one ghost (obj {}) respawned {} s after the kill; still exactly one after {} s \
             (bot revived {dead_self}x)",
            now[0].obj_id,
            first.as_secs(),
            t_kill.elapsed().as_secs()
        ),
    );
    Ok(())
}
