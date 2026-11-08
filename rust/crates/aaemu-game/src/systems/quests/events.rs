//! The public quest event API (`QuestManagerEvents.cs` + the other C# entry points into the
//! quest system) and the operation runner that locks a character for quest work.
//!
//! Every function takes the player handle and must be called **without** holding that
//! player's data lock. A call made while a quest operation of the same character is running
//! on this thread (from a [`super::hooks::QuestHooks`] method) is queued and runs before that
//! operation ends.

use std::cell::RefCell;

use super::engine::{Ctx, Deferred, KillUnitInfo, QuestEnv, UnitSnapshot};
use super::model::acceptor;
use super::packets::{quest_error, SCDoodadCompleteQuestPacket, SCQuestContextFailedPacket};
use super::spheres::SphereQuest;
use super::state::{CinemaMark, EventKind};
use crate::models::character::Character;
use crate::server::GameServer;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::world::{NpcRef, PlayerRef, WorldObject};

/// A unit taking part in a kill (`OnKillArgs` / `OnZoneKillArgs` members).
pub type KillUnit = KillUnitInfo;

/// The quest `UnitEvents` with their arguments.
#[derive(Debug, Clone, PartialEq)]
pub enum QuestEvent {
    MonsterHunt {
        npc_id: u32,
        count: u32,
    },
    /// `OnMonsterGroupHuntArgs` (`NpcId` holds the group id).
    MonsterGroupHunt {
        group_id: u32,
        count: u32,
    },
    TalkMade {
        quest_id: u32,
        npc_id: u32,
        component_id: u32,
        act_id: u32,
        source_player_id: u32,
    },
    TalkNpcGroupMade {
        quest_id: u32,
        npc_group_id: u32,
        npc_id: u32,
        component_id: u32,
        act_id: u32,
    },
    Aggro {
        npc_id: u32,
    },
    ItemGather {
        item_id: u32,
        count: i32,
    },
    ItemGroupGather {
        item_id: u32,
        count: i32,
        group_id: u32,
    },
    ItemUse {
        item_id: u32,
    },
    Interaction {
        doodad_id: u32,
        source_player_id: u32,
    },
    ExpressFire {
        npc_id: u32,
        emotion_id: u32,
    },
    LevelUp,
    MateLevelUp,
    AbilityLevelUp,
    EnterSphere {
        sphere: SphereQuest,
    },
    ExitSphere {
        sphere: SphereQuest,
    },
    Craft {
        craft_id: u32,
    },
    ZoneKill {
        zone_group_id: u32,
        killer: KillUnit,
        victim: KillUnit,
    },
    CinemaStarted {
        cinema_id: u32,
    },
    CinemaEnded {
        cinema_id: u32,
    },
    ReportNpc {
        quest_id: u32,
        npc_id: u32,
        selected: i32,
    },
    ReportDoodad {
        quest_id: u32,
        doodad_id: u32,
        selected: i32,
    },
    ReportJournal,
    QuestComplete {
        quest_id: u32,
    },
    GuardNpcDied {
        npc_id: u32,
    },
    /// `OnKillArgs` (`Target`, `Killer`, `Victim`; the C# fills either `Target` or the pair).
    Kill {
        target: Option<KillUnit>,
        killer: Option<KillUnit>,
        victim: Option<KillUnit>,
    },
    TimerExpired {
        quest_id: u32,
        /// Runtime quest id of the run the timer was armed for. A timer task that fires after
        /// the quest was restarted must not fail the fresh run.
        run_id: u32,
    },
    QuestStepChanged {
        quest_id: u32,
        step: u8,
    },
}

impl QuestEvent {
    pub fn kind(&self) -> EventKind {
        match self {
            QuestEvent::MonsterHunt { .. } => EventKind::MonsterHunt,
            QuestEvent::MonsterGroupHunt { .. } => EventKind::MonsterGroupHunt,
            QuestEvent::TalkMade { .. } => EventKind::TalkMade,
            QuestEvent::TalkNpcGroupMade { .. } => EventKind::TalkNpcGroupMade,
            QuestEvent::Aggro { .. } => EventKind::Aggro,
            QuestEvent::ItemGather { .. } => EventKind::ItemGather,
            QuestEvent::ItemGroupGather { .. } => EventKind::ItemGroupGather,
            QuestEvent::ItemUse { .. } => EventKind::ItemUse,
            QuestEvent::Interaction { .. } => EventKind::Interaction,
            QuestEvent::ExpressFire { .. } => EventKind::ExpressFire,
            QuestEvent::LevelUp => EventKind::LevelUp,
            QuestEvent::MateLevelUp => EventKind::MateLevelUp,
            QuestEvent::AbilityLevelUp => EventKind::AbilityLevelUp,
            QuestEvent::EnterSphere { .. } => EventKind::EnterSphere,
            QuestEvent::ExitSphere { .. } => EventKind::ExitSphere,
            QuestEvent::Craft { .. } => EventKind::Craft,
            QuestEvent::ZoneKill { .. } => EventKind::ZoneKill,
            QuestEvent::CinemaStarted { .. } => EventKind::CinemaStarted,
            QuestEvent::CinemaEnded { .. } => EventKind::CinemaEnded,
            QuestEvent::ReportNpc { .. } => EventKind::ReportNpc,
            QuestEvent::ReportDoodad { .. } => EventKind::ReportDoodad,
            QuestEvent::ReportJournal => EventKind::ReportJournal,
            QuestEvent::QuestComplete { .. } => EventKind::QuestComplete,
            QuestEvent::GuardNpcDied { .. } => EventKind::GuardNpcDied,
            QuestEvent::Kill { .. } => EventKind::Kill,
            QuestEvent::TimerExpired { .. } => EventKind::TimerExpired,
            QuestEvent::QuestStepChanged { .. } => EventKind::QuestStepChanged,
        }
    }
}

// ----- operation runner -----

type PendingOp = Box<dyn FnOnce(&mut Ctx<'_>) + Send>;

thread_local! {
    /// Characters with a quest operation running on this thread, with the operations queued
    /// by nested calls.
    static ACTIVE: RefCell<Vec<(u32, Vec<PendingOp>)>> = const { RefCell::new(Vec::new()) };
}

/// Work queued by a locked [`super::hooks::QuestHooks`] method that must run once the
/// character's lock is released (world broadcasts, stat refreshes, follow-up events).
type WorldOp = Box<dyn FnOnce(&GameServer)>;

thread_local! {
    /// (character id, op) queued by [`after_unlock`] while that character's operation runs.
    static AFTER_UNLOCK: RefCell<Vec<(u32, WorldOp)>> = const { RefCell::new(Vec::new()) };
}

/// Runs `op` once the quest operation of `character_id` running on this thread has released
/// the character's lock (immediately when no such operation runs). Locked quest hooks use it
/// for the parts of a C# call that touch the world (`Character.AddExp` level-up broadcast,
/// `DoOnLevelUpEvents`, buff dispatch, ...).
pub fn after_unlock(
    server: &GameServer,
    character_id: u32,
    op: impl FnOnce(&GameServer) + 'static,
) {
    let active = ACTIVE.with(|a| a.borrow().iter().any(|(id, _)| *id == character_id));
    if active {
        AFTER_UNLOCK.with(|q| q.borrow_mut().push((character_id, Box::new(op))));
    } else {
        op(server);
    }
}

/// Runs the [`after_unlock`] work queued for `character_id`.
fn run_after_unlock(server: &GameServer, character_id: u32) {
    loop {
        let ops: Vec<WorldOp> = AFTER_UNLOCK.with(|q| {
            let mut q = q.borrow_mut();
            let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut *q)
                .into_iter()
                .partition(|(id, _)| *id == character_id);
            *q = rest;
            mine.into_iter().map(|(_, op)| op).collect()
        });
        if ops.is_empty() {
            break;
        }
        for op in ops {
            op(server);
        }
    }
}

fn nested_push(character_id: u32, op: PendingOp) -> Result<(), PendingOp> {
    ACTIVE.with(|a| {
        let mut a = a.borrow_mut();
        match a.iter_mut().find(|(id, _)| *id == character_id) {
            Some((_, q)) => {
                q.push(op);
                Ok(())
            }
            None => Err(op),
        }
    })
}

fn take_pending(character_id: u32) -> Vec<PendingOp> {
    ACTIVE.with(|a| {
        let mut a = a.borrow_mut();
        a.iter_mut()
            .find(|(id, _)| *id == character_id)
            .map(|(_, q)| std::mem::take(q))
            .unwrap_or_default()
    })
}

/// Captures the world facts an operation on `player` may need (current target, NPCs around
/// when an active quest has a distance act, position, time of day).
pub fn capture_env(server: &GameServer, player: &PlayerRef) -> QuestEnv {
    let data = &server.game_data.systems.quests;
    let (target_id, needs_npcs) = {
        let c = player.data();
        let needs = c.quest_state.quests.values().any(|q| {
            data.template(q.template_id).is_some_and(|t| {
                t.components.iter().any(|cid| {
                    data.acts_in_component(*cid).iter().any(|&a| {
                        matches!(
                            data.acts[a].kind,
                            super::templates::ActKind::ObjDistance { .. }
                                | super::templates::ActKind::CheckDistance { .. }
                        )
                    })
                })
            })
        });
        (c.unit.current_target, needs)
    };
    let mut env = QuestEnv {
        position: world_position(server, &WorldObject::from(player.clone())),
        time_of_day: server.time.get_time(),
        ..QuestEnv::default()
    };
    if target_id != 0 {
        env.target = server.world.object(target_id).map(|o| snapshot(server, &o));
    }
    if needs_npcs {
        env.nearby_npcs = nearby_npcs(server, player);
    }
    env
}

/// The NPCs of the player's neighbourhood: (template id, x, y, z).
pub fn nearby_npcs(server: &GameServer, player: &PlayerRef) -> Vec<(u32, f32, f32, f32)> {
    let me = WorldObject::from(player.clone());
    let Some(world) = server.world.world_of(&me) else {
        return Vec::new();
    };
    let npcs = world.around(&me, |o| o.as_npc().cloned());
    npcs.iter()
        .map(|n| {
            let template_id = n.data().unit.template_id;
            let (x, y, z) = world_position(server, &WorldObject::from(n.clone()));
            (template_id, x, y, z)
        })
        .collect()
}

/// `Transform.World.Position` of an object: the local position, composed with the parent chain
/// when the object is attached (a mounted rider or a ship passenger has a parent-relative
/// transform). Must be called without holding the object's data lock.
pub fn world_position(server: &GameServer, o: &WorldObject) -> (f32, f32, f32) {
    let t = o.transform();
    if t.parent_obj_id != 0 {
        if let Some(w) = server.world.world_of(o) {
            let p = w.world_pos_rot(o);
            return (p.x, p.y, p.z);
        }
    }
    (t.x, t.y, t.z)
}

/// A [`UnitSnapshot`] of a world object.
pub fn snapshot(server: &GameServer, o: &WorldObject) -> UnitSnapshot {
    let mut s = UnitSnapshot {
        obj_id: o.obj_id(),
        is_unit: o.is_unit(),
        position: world_position(server, o),
        ..UnitSnapshot::default()
    };
    if let Some(d) = o.as_doodad() {
        s.doodad_template_id = Some(d.data().template_id);
        s.base_unit_type = 4;
    }
    if let Some(n) = o.as_npc() {
        s.npc_template_id = Some(n.data().unit.template_id);
        s.base_unit_type = 1;
    }
    o.with_unit(|u| {
        let b = u.unit();
        s.level = b.level;
        s.hpp = b.hpp();
        s.in_battle = b.is_in_battle;
        s.under_water = b.is_under_water;
        s.faction_id = b.faction_id;
        s.buff_ids = b.buffs.effects.iter().map(|e| e.template.id).collect();
        s.zone_key = b.transform.zone_id;
    });
    s.mother_faction_id = server
        .game_data
        .factions
        .get(s.faction_id)
        .map_or(0, |f| f.mother_id);
    s
}

/// Runs one quest operation on `player`: locks the character, takes its quest state, runs
/// `f`, then the queued nested operations; afterwards re-derives the quest rows, releases the
/// lock and executes the deferred world effects. Returns `None` when called nested inside an
/// operation of the same character on this thread (use the event functions there: they queue).
pub fn with_quests<R>(
    server: &GameServer,
    player: &PlayerRef,
    env: Option<QuestEnv>,
    f: impl FnOnce(&mut Ctx<'_>) -> R,
) -> Option<R> {
    let character_id = player.character_id;
    let nested = ACTIVE.with(|a| a.borrow().iter().any(|(id, _)| *id == character_id));
    if nested {
        tracing::error!("with_quests: nested quest operation for character {character_id}");
        return None;
    }
    if crate::systems::economy::System::ops_held_on_this_thread() {
        // The ops lock below is not re-entrant: waiting for it here would hang this thread
        // forever and, with it, every other economy operation. Callers raise quest events
        // after the `ItemTxn` is released (queue the event in the `ItemFx` list).
        tracing::error!(
            "with_quests: the economy ops lock is held on this thread (character {character_id})"
        );
        return None;
    }
    let env = env.unwrap_or_else(|| capture_env(server, player));
    // Quest rewards and the saved quest row must share the item/wallet snapshot boundary.
    let economy = server.systems.economy.ops_lock();
    ACTIVE.with(|a| a.borrow_mut().push((character_id, Vec::new())));
    let system = &server.systems.quests;
    let hooks = system.hooks();
    // A panic in the operation or in a queued one is caught here, so that the quest state
    // taken out of the character below is put back and the ACTIVE entry is removed before the
    // panic resumes: otherwise every later event of the character on this thread would be
    // queued forever and its quests lost. Catching it while `guard` is held also keeps the
    // character mutex from being poisoned.
    let outcome = {
        let mut guard = player.data();
        let mut st = std::mem::take(&mut guard.quest_state);
        let c: &mut Character = &mut guard;
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut ctx = Ctx {
                server,
                data: &server.game_data.systems.quests,
                hooks,
                player: Some(player.clone()),
                c,
                st: &mut st,
                env,
                now_ms: chrono::Utc::now().timestamp_millis(),
                deferred: Vec::new(),
                buffs_changed: false,
                sent: Vec::new(),
                report_reply: None,
                report_refused: Vec::new(),
                answer_reports: true,
                answer_start_refusals: false,
            };
            if !ctx.st.loaded {
                ctx.load_from_rows();
            }
            let r = f(&mut ctx);
            loop {
                let pending = take_pending(character_id);
                if pending.is_empty() {
                    break;
                }
                for op in pending {
                    op(&mut ctx);
                }
            }
            let keep = system.queued_handles(character_id);
            ctx.st.collect_garbage(&keep);
            ctx.sync_rows();
            let deferred = std::mem::take(&mut ctx.deferred);
            let buffs_changed = ctx.buffs_changed;
            (r, deferred, buffs_changed)
        }));
        if run.is_err() {
            guard.quests.active = st.rows();
        }
        guard.quest_state = st;
        run
    };
    ACTIVE.with(|a| {
        let mut a = a.borrow_mut();
        if let Some(i) = a.iter().rposition(|(id, _)| *id == character_id) {
            a.remove(i);
        }
    });
    drop(economy);
    let (r, deferred, buffs_changed) = match outcome {
        Ok(done) => done,
        Err(payload) => std::panic::resume_unwind(payload),
    };
    crate::systems::economy::flush_deferred_glider_reequips(server);
    crate::systems::flush_achievements(server);
    if buffs_changed {
        crate::systems::dispatch_buff_events(server, &WorldObject::from(player.clone()));
    }
    run_deferred(server, player, deferred);
    run_after_unlock(server, character_id);
    Some(r)
}

/// Runs `op` as a quest operation, or queues it when nested in one of the same character.
pub(super) fn run_or_queue(
    server: &GameServer,
    player: &PlayerRef,
    op: impl FnOnce(&mut Ctx<'_>) + Send + 'static,
) {
    let op: PendingOp = Box::new(op);
    match nested_push(player.character_id, op) {
        Ok(()) => {}
        Err(op) => {
            with_quests(server, player, None, op);
        }
    }
}

fn run_deferred(server: &GameServer, player: &PlayerRef, deferred: Vec<Deferred>) {
    let hooks = server.systems.quests.hooks();
    let me = WorldObject::from(player.clone());
    let world = server.world.world_of(&me);
    for d in deferred {
        match d {
            Deferred::SystemMessage { text } => crate::world::session::send_message(
                player,
                crate::managers::chat::chat_type::SYSTEM,
                &text,
            ),
            Deferred::UseSkillSelf { skill_id } => hooks.use_skill(server, &me, skill_id, &me),
            Deferred::NpcUseSkill {
                npc_template_id,
                skill_id,
            } => {
                if let Some(npc) = world
                    .as_ref()
                    .and_then(|w| w.npc_by_template_id(npc_template_id))
                {
                    let o = WorldObject::from(npc);
                    hooks.use_skill(server, &o, skill_id, &o);
                }
            }
            Deferred::NpcSetFaction {
                npc_template_id,
                faction_id,
            } => {
                if let Some(npc) = world
                    .as_ref()
                    .and_then(|w| w.npc_by_template_id(npc_template_id))
                {
                    hooks.set_npc_faction(server, &npc, faction_id);
                }
            }
            Deferred::TeamEvent { event } => {
                for member in hooks.team_members(server, player) {
                    // Skip self
                    if member.character_id == player.character_id {
                        continue;
                    }
                    // Directly call OnX on team members to avoid loops/duplicates
                    let ev = event.clone();
                    with_quests(server, &member, None, move |ctx| {
                        let _ = ctx.dispatch(&ev);
                    });
                }
            }
            Deferred::NpcComponent { component_id } => {
                if let Some(component) = server.game_data.systems.quests.component(component_id) {
                    super::bridges::run_npc_component(server, player, component);
                }
            }
        }
    }
}

/// Dispatches one event to the player's quest subscribers (logs a C# throw).
fn dispatch_logged(ctx: &mut Ctx<'_>, ev: &QuestEvent) {
    if let Err(e) = ctx.dispatch(ev) {
        tracing::error!("Quest event {:?} threw: {}", ev.kind(), e.0);
    }
}

/// One WARN for a turn-in the server dropped before any quest act saw it (a bug report of
/// "cannot turn in" otherwise leaves no trace in the log), and the `SCQuestContextFailed`
/// answer that lets the client leave quest directing mode.
fn warn_report_refused(player: &PlayerRef, quest_id: u32, err: u8, reason: &str, answer: bool) {
    let (name, id) = {
        let c = player.data();
        (c.unit.name.clone(), c.id)
    };
    tracing::warn!("Report refused, {name} ({id}), Quest {quest_id}: {reason}");
    if answer {
        player.send(SCQuestContextFailedPacket { quest_id, err });
    }
}

/// Logs why a turn-in at `kind` (`NPC` / `doodad`) template `target_id` cannot reach a report
/// act: the quest is not active, or it has no report act of that kind at all (a mismatching
/// target and unfinished objectives are logged by the act itself). The event is dispatched
/// regardless; it is a no-op in both cases. The refusal is answered here, and `true` tells
/// the caller not to answer it a second time after the dispatch.
fn log_unreachable_report(ctx: &mut Ctx<'_>, quest_id: u32, kind: &str, target_id: u32) -> bool {
    if ctx.st.active_handle(quest_id).is_none() {
        ctx.log_refusal(
            "Report",
            quest_id,
            &format!("{kind} {target_id}: the quest is not active"),
        );
        ctx.send_report_refused(quest_id, quest_error::INVALID_QUEST);
        return true;
    }
    let class = if kind == "NPC" {
        "QuestActConReportNpc"
    } else {
        "QuestActConReportDoodad"
    };
    if !ctx
        .data
        .acts_of_type(class)
        .any(|(_, a)| a.quest_id == quest_id)
    {
        ctx.log_refusal(
            "Report",
            quest_id,
            &format!("{kind} {target_id}: the quest has no {kind} report act"),
        );
        ctx.send_report_refused(quest_id, quest_error::INVALID_NPC_OR_QUEST);
        return true;
    }
    false
}

// ----- QuestManagerEvents -----

/// `DoReportEvents(owner, questContextId, npcObjId, doodadObjId, selected)` for a report the
/// client asked for (`CSCompleteQuestContextPacket`): a refusal is answered with
/// `SCQuestContextFailedPacket` so the client leaves quest directing mode.
pub fn on_report(
    server: &GameServer,
    player: &PlayerRef,
    quest_id: u32,
    npc_obj_id: u32,
    doodad_obj_id: u32,
    selected: i32,
) {
    report(
        server,
        player,
        quest_id,
        npc_obj_id,
        doodad_obj_id,
        selected,
        true,
    );
}

/// `DoReportEvents` started by the server (`DoodadFuncQuest`, a doodad used on an owned
/// quest): the client is not in quest directing mode, so a refusal is only logged. A doodad
/// report that can be handed in sends [`SCDoodadCompleteQuestPacket`] instead (the client
/// then requests the completion with `CSCompleteQuestContextPacket`).
pub fn on_server_report(
    server: &GameServer,
    player: &PlayerRef,
    quest_id: u32,
    npc_obj_id: u32,
    doodad_obj_id: u32,
    selected: i32,
) {
    report(
        server,
        player,
        quest_id,
        npc_obj_id,
        doodad_obj_id,
        selected,
        false,
    );
}

fn report(
    server: &GameServer,
    player: &PlayerRef,
    quest_id: u32,
    npc_obj_id: u32,
    doodad_obj_id: u32,
    selected: i32,
    answer: bool,
) {
    let me = WorldObject::from(player.clone());
    let world = server.world.world_of(&me);
    if npc_obj_id > 0 {
        // Turning in at a NPC?
        let Some(npc) = world.as_ref().and_then(|w| w.npc(npc_obj_id)) else {
            warn_report_refused(
                player,
                quest_id,
                quest_error::INVALID_NPC_OR_QUEST,
                &format!("NPC object {npc_obj_id} is not in the character's world"),
                answer,
            );
            return;
        };
        let npc_id = npc.data().unit.template_id;
        run_or_queue(server, player, move |ctx| {
            let outer = std::mem::replace(&mut ctx.answer_reports, answer);
            let answered = log_unreachable_report(ctx, quest_id, "NPC", npc_id);
            ctx.report_reply = None;
            dispatch_logged(
                ctx,
                &QuestEvent::ReportNpc {
                    quest_id,
                    npc_id,
                    selected,
                },
            );
            if !answered {
                ctx.finish_report(quest_id, quest_error::INVALID_NPC_OR_QUEST);
            }
            ctx.answer_reports = outer;
        });
    } else if doodad_obj_id > 0 {
        // Turning in at a Doodad?
        let Some(doodad) = world.as_ref().and_then(|w| w.doodad(doodad_obj_id)) else {
            warn_report_refused(
                player,
                quest_id,
                quest_error::INVALID_DOODAD,
                &format!("doodad object {doodad_obj_id} is not in the character's world"),
                answer,
            );
            return;
        };
        let doodad_id = doodad.data().template_id;
        run_or_queue(server, player, move |ctx| {
            if !answer && ctx.report_doodad_ready(quest_id, doodad_id) {
                // `DoodadFuncQuest` on a report doodad that can be handed in: the 1.2
                // client starts its completion flow from `SCDoodadCompleteQuestPacket`
                // (`COMPLETE_QUEST_CONTEXT_DOODAD`) and answers with
                // `CSCompleteQuestContextPacket` for this doodad object id. Do not report
                // here, or that request would arrive after the quest already completed.
                ctx.send(SCDoodadCompleteQuestPacket {
                    doodad_obj_id,
                    quest_id,
                });
                return;
            }
            let outer = std::mem::replace(&mut ctx.answer_reports, answer);
            let answered = log_unreachable_report(ctx, quest_id, "doodad", doodad_id);
            ctx.report_reply = None;
            dispatch_logged(
                ctx,
                &QuestEvent::ReportDoodad {
                    quest_id,
                    doodad_id,
                    selected,
                },
            );
            if !answered {
                ctx.finish_report(quest_id, quest_error::INVALID_DOODAD);
            }
            ctx.answer_reports = outer;
        });
    } else {
        // Object-free reporting must not skip objectives or an NPC/doodad report act.
        run_or_queue(server, player, move |ctx| {
            let Some(h) = ctx.st.active_handle(quest_id) else {
                ctx.log_refusal("Report", quest_id, "no target: the quest is not active");
                ctx.send_report_refused(quest_id, quest_error::INVALID_QUEST);
                return;
            };
            let step = ctx.st.quest(h).map(|q| q.step);
            if step == Some(super::model::kind::PROGRESS) {
                // The existing early-completion path checks both the template flag and
                // the objective threshold before granting any reward.
                ctx.try_complete_quest_as_let_it_done(quest_id, selected);
                return;
            }
            if step != Some(super::model::kind::READY) {
                ctx.log_refusal(
                    "Report",
                    quest_id,
                    &format!("no target: the quest is at step {step:?}, not ready"),
                );
                ctx.send_report_refused(quest_id, quest_error::INVALID_QUEST_STATUS);
                return;
            }
            let automatic_report = ctx.data.template(quest_id).is_some_and(|t| {
                t.components_of(&ctx.data.components, super::model::kind::READY)
                    .into_iter()
                    .flat_map(|c| &c.acts)
                    .any(|&a| {
                        matches!(
                            ctx.data.acts[a].kind,
                            super::templates::ActKind::ConAutoComplete
                                | super::templates::ActKind::ConReportJournal
                        )
                    })
            });
            if !automatic_report {
                ctx.log_refusal(
                    "Report",
                    quest_id,
                    "no target: the ready step needs a report to an NPC or doodad",
                );
                ctx.send_report_refused(quest_id, quest_error::INVALID_QUEST_STATUS);
                return;
            }
            if let Some(q) = ctx.st.quest_mut(h) {
                q.selected_reward_index = selected;
            }
            ctx.run_current_step(h);
        });
    }
}

/// `DoItemsConsumedEvents(owner, templateId, count)`.
pub fn on_items_consumed(server: &GameServer, player: &PlayerRef, template_id: u32, count: i32) {
    on_items_acquired(server, player, template_id, count.wrapping_neg());
}

/// `DoItemsAcquiredEvents(owner, templateId, count)` (`Inventory` item add / remove).
pub fn on_items_acquired(server: &GameServer, player: &PlayerRef, template_id: u32, count: i32) {
    run_or_queue(server, player, move |ctx| {
        if count > 0 {
            ctx.start_quests_from_item_gain(template_id);
        }
        dispatch_logged(
            ctx,
            &QuestEvent::ItemGather {
                item_id: template_id,
                count,
            },
        );
        for group in ctx.data.group_items.groups_of(template_id) {
            dispatch_logged(
                ctx,
                &QuestEvent::ItemGroupGather {
                    item_id: template_id,
                    count,
                    group_id: group,
                },
            );
        }
    });
}

/// `DoDoodadInteractionEvents(sourcePlayer, targetPlayer, doodadTemplateId)`
/// (`InteractionEffect`, `CraftEffect`).
pub fn on_doodad_interaction(
    server: &GameServer,
    source: &PlayerRef,
    target: &PlayerRef,
    doodad_template_id: u32,
) {
    let source_player_id = source.character_id;
    run_or_queue(server, target, move |ctx| {
        dispatch_logged(
            ctx,
            &QuestEvent::Interaction {
                doodad_id: doodad_template_id,
                source_player_id,
            },
        );
    });
}

/// `DoTalkMadeEvents(sourcePlayer, targetPlayer, npcObjId, questContextId, questComponentId,
/// questActId)` (`CSQuestTalkMadePacket`).
pub fn on_talk_made(
    server: &GameServer,
    source: &PlayerRef,
    target: &PlayerRef,
    npc_obj_id: u32,
    quest_id: u32,
    component_id: u32,
    act_id: u32,
) {
    let me = WorldObject::from(source.clone());
    let Some(npc) = server.world.world_of(&me).and_then(|w| w.npc(npc_obj_id)) else {
        return;
    };
    let npc_id = npc.data().unit.template_id;
    let source_player_id = source.character_id;
    run_or_queue(server, target, move |ctx| {
        dispatch_logged(
            ctx,
            &QuestEvent::TalkMade {
                quest_id,
                npc_id,
                component_id,
                act_id,
                source_player_id,
            },
        );
        for group in ctx.data.group_npcs.groups_of(npc_id) {
            dispatch_logged(
                ctx,
                &QuestEvent::TalkNpcGroupMade {
                    quest_id,
                    npc_group_id: group,
                    npc_id,
                    component_id,
                    act_id,
                },
            );
        }
    });
}

/// A [`KillUnit`] for a world object (`aggro_for` = the player whose aggro rating is read
/// for `QuestActObjAggro`).
pub fn kill_unit(server: &GameServer, o: &WorldObject, aggro_for: Option<u32>) -> KillUnit {
    let s = snapshot(server, o);
    let aggro_percent = match (o.as_npc(), aggro_for) {
        (Some(npc), Some(obj)) => server
            .systems
            .quests
            .hooks()
            .npc_aggro_rating_percent(server, npc, obj),
        _ => 100.0,
    };
    KillUnit {
        obj_id: s.obj_id,
        character_id: o.as_player().map(|p| p.character_id),
        npc_template_id: s.npc_template_id,
        faction_id: s.faction_id,
        level: s.level,
        aggro_percent,
    }
}

/// The body of `DoOnMonsterHuntEvents(owner, npc)`: the monster hunt, monster group hunt and
/// zone kill quest events. `killer`/`victim` feed `ZoneKill`; a scripted removal has no real
/// killer, so its `Kill` event is skipped (C# #1792 calls only `DoOnMonsterHuntEvents`).
fn dispatch_monster_hunt_events(
    ctx: &mut Ctx<'_>,
    npc_id: u32,
    zone_group_id: u32,
    killer: KillUnit,
    victim: KillUnit,
    with_kill: bool,
) {
    if with_kill {
        // Credit every eligible contributor while the NPC's aggro table still exists.
        dispatch_logged(
            ctx,
            &QuestEvent::Kill {
                target: None,
                killer: Some(killer.clone()),
                victim: Some(victim.clone()),
            },
        );
    }
    // Individual monster kill
    dispatch_logged(ctx, &QuestEvent::MonsterHunt { npc_id, count: 1 });
    // Trigger NPC Group kills
    for group in ctx.data.group_npcs.groups_of(npc_id) {
        dispatch_logged(
            ctx,
            &QuestEvent::MonsterGroupHunt {
                group_id: group,
                count: 1,
            },
        );
    }
    // Zone kills (also handles the zone monster hunts)
    dispatch_logged(
        ctx,
        &QuestEvent::ZoneKill {
            zone_group_id,
            killer,
            victim,
        },
    );
}

/// `DoOnMonsterHuntEvents(owner, npc)` (`Npc.DoDie` for every eligible character).
pub fn on_npc_killed(server: &GameServer, player: &PlayerRef, npc: &NpcRef) {
    let (npc_id, zone_key) = {
        let d = npc.data();
        (d.unit.template_id, d.unit.transform.zone_id)
    };
    let zone_group_id = server
        .game_data
        .zones
        .zone_by_key(zone_key)
        .map_or(0, |z| z.group_id);
    let killer = kill_unit(server, &WorldObject::from(player.clone()), None);
    let victim = kill_unit(server, &WorldObject::from(npc.clone()), Some(player.obj_id));
    run_or_queue(server, player, move |ctx| {
        dispatch_monster_hunt_events(ctx, npc_id, zone_group_id, killer, victim, true);
    });
}

/// `DoOnMonsterHuntEvents(owner, npc)` for a scripted removal (`KillNpcWithoutCorpseEffect`,
/// C# 10.x PR #1792). The C# calls only this method there, so no `OnKill` quest event is
/// raised; the monster hunt / group hunt / zone kill objectives still count.
pub fn on_npc_scripted_kill(server: &GameServer, player: &PlayerRef, npc: &NpcRef) {
    let (npc_id, zone_key) = {
        let d = npc.data();
        (d.unit.template_id, d.unit.transform.zone_id)
    };
    let zone_group_id = server
        .game_data
        .zones
        .zone_by_key(zone_key)
        .map_or(0, |z| z.group_id);
    let killer = kill_unit(server, &WorldObject::from(player.clone()), None);
    let victim = kill_unit(server, &WorldObject::from(npc.clone()), Some(player.obj_id));
    run_or_queue(server, player, move |ctx| {
        dispatch_monster_hunt_events(ctx, npc_id, zone_group_id, killer, victim, false);
    });
}

/// `DoOnAggroEvents(owner, npc)` (`Unit.OnAggro` of a player against an NPC).
pub fn on_aggro(server: &GameServer, player: &PlayerRef, npc: &NpcRef) {
    // Runs on every damage hit: skip the quest operation (character lock, state take, queue)
    // unless an active quest act subscribes to `Aggro`. A taken-out state (`loaded` false: we
    // are nested in an operation of this character) is not trusted and falls through.
    {
        let c = player.data();
        let st = &c.quest_state;
        if st.loaded && !st.subs.iter().any(|s| s.event == EventKind::Aggro) {
            return;
        }
    }
    let npc_id = npc.data().unit.template_id;
    run_or_queue(server, player, move |ctx| {
        dispatch_logged(ctx, &QuestEvent::Aggro { npc_id });
    });
}

/// `DoOnExpressFireEvents(owner, emotionId, characterObjId, npcObjId)`
/// (`CSExpressEmotionPacket`, with `ExpressTextManager.GetExpressAnimId(emotionId)`).
pub fn on_express_fire(
    server: &GameServer,
    player: &PlayerRef,
    emotion_id: u32,
    character_obj_id: u32,
    npc_obj_id: u32,
) {
    if player.obj_id != character_obj_id {
        tracing::warn!(
            "DoOnExpressFireEvents seems to have a invalid characterObjId referenced, Got:{character_obj_id}, Expected:{} ({})",
            player.obj_id,
            player.data().unit.name
        );
        return;
    }
    let me = WorldObject::from(player.clone());
    let Some(npc) = server.world.world_of(&me).and_then(|w| w.npc(npc_obj_id)) else {
        return;
    };
    let npc_id = npc.data().unit.template_id;
    run_or_queue(server, player, move |ctx| {
        dispatch_logged(ctx, &QuestEvent::ExpressFire { npc_id, emotion_id });
    });
}

/// `DoOnLevelUpEvents(owner)` (`Character.AddExp`).
pub fn on_level_up(server: &GameServer, player: &PlayerRef) {
    run_or_queue(server, player, |ctx| {
        dispatch_logged(ctx, &QuestEvent::LevelUp);
        // Added for quest In the Footsteps of Gods and Heroes ( 5967 )
        dispatch_logged(ctx, &QuestEvent::AbilityLevelUp);
        // Also handle Level-based (character main level) quest starters
        let data = ctx.data;
        let starters: Vec<(u8, u32)> = data
            .acts_of_type("QuestActConAcceptLevelUp")
            .filter_map(|(_, a)| match a.kind {
                super::templates::ActKind::ConAcceptLevelUp { level } => Some((level, a.quest_id)),
                _ => None,
            })
            .collect();
        for (level, quest_id) in starters {
            if ctx.c.unit.level >= level
                && !ctx.has_quest_completed(quest_id)
                && !ctx.has_quest(quest_id)
            {
                ctx.add_quest(quest_id, false, acceptor::UNKNOWN, 0);
            }
        }
    });
}

/// `DoOnCraftEvents(owner, craftId)` (`CharacterCraft` craft success).
pub fn on_craft(server: &GameServer, player: &PlayerRef, craft_id: u32) {
    run_or_queue(server, player, move |ctx| {
        dispatch_logged(ctx, &QuestEvent::Craft { craft_id });
    });
}

/// `Character.ItemUse(item)` / `ItemUseByTemplate(template)` (`Events.OnItemUse`).
pub fn on_item_use(server: &GameServer, player: &PlayerRef, item_template_id: u32) {
    if item_template_id == 0 {
        return;
    }
    // USE_ITEM achievement credit (one per successful use).
    crate::systems::report_achievement(
        server,
        player,
        AchievementInput::Increment {
            kind_id: char_record_kind::USE_ITEM,
            amount: 1,
            matcher: RecordMatch::Value1(item_template_id),
        },
    );
    run_or_queue(server, player, move |ctx| {
        dispatch_logged(
            ctx,
            &QuestEvent::ItemUse {
                item_id: item_template_id,
            },
        );
    });
}

/// `owner.Events.OnMateLevelUp` (`Mate.AddExp` level up).
pub fn on_mate_level_up(server: &GameServer, player: &PlayerRef) {
    run_or_queue(server, player, |ctx| {
        dispatch_logged(ctx, &QuestEvent::MateLevelUp);
    });
}

/// Re-checks the mate objectives after a mate is spawned: `QuestActObjMateLevel` reads the
/// summon item's level (refreshed by `Mates.SpawnMount`), and the C# only re-checked it on a
/// level-up, so summoning a mate at the required level never completed the objective.
pub fn on_mate_spawned(server: &GameServer, player: &PlayerRef) {
    run_or_queue(server, player, |ctx| {
        dispatch_logged(ctx, &QuestEvent::MateLevelUp);
    });
}

/// `killerUnit.Events.OnKill(sender, args)` for a player killer (the quest subscribers:
/// `QuestActObjAggro`). The C# raises it twice per kill: `Unit.ReduceCurrentHp` with
/// `Target = attacker` (sic) and `Unit.DoDie` with `Killer` / `Victim`; pass the same.
pub fn on_kill(
    server: &GameServer,
    player: &PlayerRef,
    target: Option<&WorldObject>,
    killer: Option<&WorldObject>,
    victim: Option<&WorldObject>,
) {
    let me = Some(player.obj_id);
    let target = target.map(|o| kill_unit(server, o, me));
    let killer = killer.map(|o| kill_unit(server, o, me));
    let victim = victim.map(|o| kill_unit(server, o, me));
    run_or_queue(server, player, move |ctx| {
        dispatch_logged(
            ctx,
            &QuestEvent::Kill {
                target,
                killer,
                victim,
            },
        );
    });
}

/// `DoOnEnterSphereEvents(owner, sphereQuest, oldPosition)`.
pub fn on_enter_sphere(server: &GameServer, player: &PlayerRef, sphere: SphereQuest) {
    run_or_queue(server, player, move |ctx| {
        dispatch_logged(ctx, &QuestEvent::EnterSphere { sphere });
    });
}

/// `DoOnExitSphereEvents(owner, sphereQuest, oldPosition)`.
pub fn on_exit_sphere(server: &GameServer, player: &PlayerRef, sphere: SphereQuest) {
    run_or_queue(server, player, move |ctx| {
        dispatch_logged(ctx, &QuestEvent::ExitSphere { sphere });
    });
}

/// `QuestManager.OnTimerExpired(owner, questId)` (`QuestTimeoutTask`).
pub fn on_timer_expired(server: &GameServer, player: &PlayerRef, quest_id: u32, run_id: u32) {
    run_or_queue(server, player, move |ctx| {
        dispatch_logged(ctx, &QuestEvent::TimerExpired { quest_id, run_id });
    });
}

/// `DoOnEnterQuestStarterSphere(player, sphereQuestStarter, oldPosition)`.
pub fn on_enter_quest_starter_sphere(
    server: &GameServer,
    player: &PlayerRef,
    quest_template_id: u32,
    sphere_id: u32,
) {
    run_or_queue(server, player, move |ctx| {
        if ctx.has_quest_completed(quest_template_id) || ctx.has_quest(quest_template_id) {
            return;
        }
        ctx.add_quest(quest_template_id, false, acceptor::SPHERE, sphere_id);
    });
}

/// The cinema a `CSCompletedCinemaPacket` ends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CinemaEnd {
    /// `cinemas.id`, 0 when it is unknown.
    pub cinema_id: u32,
    /// Milliseconds since its `CSStartedCinemaPacket`; `None` when none was seen.
    pub played_ms: Option<i64>,
}

/// The cinema that ends now: the playing one, else the one a `QuestActObjCinema` claimed, else
/// unknown.
fn cinema_end(st: &super::state::QuestState, now_ms: i64) -> CinemaEnd {
    let playing = st.playing_cinema;
    let cinema_id = playing
        .map(|m| m.cinema_id)
        .filter(|id| *id != 0)
        .unwrap_or(st.currently_playing_cinema_id);
    CinemaEnd {
        cinema_id,
        played_ms: playing.map(|m| (now_ms - m.at_ms).max(0)),
    }
}

/// Rust addition: the client is about to play cinema `cinema_id`, announced by
/// `CSRequestPermissionToPlayCinemaForDirectingMode` or started by a `CinemaEffect`. The next
/// `CSStartedCinemaPacket` is that cinema. 0 forgets an earlier announcement (the new request
/// has no known cinema).
pub fn request_cinema(server: &GameServer, player: &PlayerRef, cinema_id: u32) {
    run_or_queue(server, player, move |ctx| {
        let at_ms = ctx.now_ms;
        ctx.st.requested_cinema = (cinema_id != 0).then_some(CinemaMark { cinema_id, at_ms });
    });
}

/// `Events.OnCinemaStarted` (`CSStartedCinemaPacket`). The event carries the announced cinema,
/// 0 when the client played one without announcing it (a `QuestActObjCinema` objective on
/// quest start).
pub fn on_cinema_started(server: &GameServer, player: &PlayerRef) {
    run_or_queue(server, player, |ctx| {
        let now_ms = ctx.now_ms;
        let cinema_id = ctx
            .st
            .requested_cinema
            .take()
            .filter(|r| (now_ms - r.at_ms).abs() <= super::cinema::REQUEST_TTL_MS)
            .map_or(0, |r| r.cinema_id);
        ctx.st.playing_cinema = Some(CinemaMark {
            cinema_id,
            at_ms: now_ms,
        });
        tracing::info!(
            "StartedCinema, {} ({}), Cinema {}",
            ctx.c.unit.name,
            ctx.c.id,
            ctx.data.cinema_label(cinema_id)
        );
        dispatch_logged(ctx, &QuestEvent::CinemaStarted { cinema_id });
    });
}

/// Which cinema `CSCompletedCinemaPacket` ends and for how long it played; call before
/// [`on_cinema_ended`] (which forgets it). Not for use inside a quest operation.
pub fn cinema_ending(player: &PlayerRef) -> CinemaEnd {
    let now_ms = chrono::Utc::now().timestamp_millis();
    cinema_end(&player.data().quest_state, now_ms)
}

/// `Events.OnCinemaEnded` (`CSCompletedCinemaPacket`).
pub fn on_cinema_ended(server: &GameServer, player: &PlayerRef) {
    run_or_queue(server, player, |ctx| {
        let CinemaEnd { cinema_id, .. } = cinema_end(ctx.st, ctx.now_ms);
        ctx.st.playing_cinema = None;
        ctx.st.requested_cinema = None;
        dispatch_logged(ctx, &QuestEvent::CinemaEnded { cinema_id });
    });
}

/// `Events.OnReportJournal` (nothing in the 1.2 C# raises it; for completeness).
pub fn on_report_journal(server: &GameServer, player: &PlayerRef) {
    run_or_queue(server, player, |ctx| {
        dispatch_logged(ctx, &QuestEvent::ReportJournal);
    });
}

/// `Character.Quests.OnQuestItemManuallyDestroyed(item)` (`Inventory` destroy of an item whose
/// template has a `LootQuestId` the player has).
pub fn on_quest_item_manually_destroyed(
    server: &GameServer,
    player: &PlayerRef,
    item_template_id: u32,
    loot_quest_id: u32,
) {
    run_or_queue(server, player, move |ctx| {
        ctx.on_quest_item_manually_destroyed(item_template_id, loot_quest_id);
    });
}

// ----- CharacterQuests entry points used by other subsystems -----

/// `CSStartQuestContextPacket`: the player asked to start `quest_id` at the NPC, doodad or
/// sphere context (or without one). Only these client requests answer a start-component unit
/// requirement refusal with `SCQuestUnitReqFailedPacket`; every other starter stays silent.
pub fn start_quest_context(
    server: &GameServer,
    player: &PlayerRef,
    quest_id: u32,
    npc_obj_id: u32,
    doodad_obj_id: u32,
    sphere_id: u32,
) {
    if npc_obj_id > 0 {
        add_quest_from_npc_impl(server, player, quest_id, npc_obj_id, true);
    } else if doodad_obj_id > 0 {
        add_quest_from_doodad_impl(server, player, quest_id, doodad_obj_id, true);
    } else if sphere_id > 0 {
        add_quest_from_sphere_impl(server, player, quest_id, sphere_id, true);
    } else {
        add_quest_impl(server, player, quest_id, false, acceptor::UNKNOWN, 0, true);
    }
}

/// `Quests.AddQuest(questId, forcibly)` (`AcceptQuestEffect`, GM commands). Returns the C#
/// result; `None` when nested in a quest operation of the same character (queued, result
/// unknown).
pub fn add_quest(
    server: &GameServer,
    player: &PlayerRef,
    quest_id: u32,
    forcibly: bool,
) -> Option<bool> {
    add_quest_impl(
        server,
        player,
        quest_id,
        forcibly,
        acceptor::UNKNOWN,
        0,
        false,
    )
}

fn add_quest_impl(
    server: &GameServer,
    player: &PlayerRef,
    quest_id: u32,
    forcibly: bool,
    acceptor_type: u8,
    acceptor_id: u32,
    answer_start_refusals: bool,
) -> Option<bool> {
    with_quests(server, player, None, move |ctx| {
        ctx.answer_start_refusals = answer_start_refusals;
        ctx.add_quest(quest_id, forcibly, acceptor_type, acceptor_id)
    })
}

/// `Quests.AddQuestFromNpc(questId, npcObjId)` (`CSStartQuestContextPacket`,
/// `Unit` engage-combat quests).
pub fn add_quest_from_npc(
    server: &GameServer,
    player: &PlayerRef,
    quest_id: u32,
    npc_obj_id: u32,
) -> Option<bool> {
    add_quest_from_npc_impl(server, player, quest_id, npc_obj_id, false)
}

fn add_quest_from_npc_impl(
    server: &GameServer,
    player: &PlayerRef,
    quest_id: u32,
    npc_obj_id: u32,
    answer_start_refusals: bool,
) -> Option<bool> {
    let me = WorldObject::from(player.clone());
    let npc = server.world.world_of(&me).and_then(|w| w.npc(npc_obj_id));
    let npc_info = npc.as_ref().map(|n| (n.obj_id, n.data().unit.template_id));
    let mut env = capture_env(server, player);
    env.target = npc.map(|n| snapshot(server, &WorldObject::from(n)));
    with_quests(server, player, Some(env), move |ctx| {
        ctx.answer_start_refusals = answer_start_refusals;
        // Owner.CurrentTarget = npc
        ctx.c.unit.current_target = npc_info.map_or(0, |(o, _)| o);
        match npc_info {
            Some((_, template_id)) => ctx.add_quest(quest_id, false, acceptor::NPC, template_id),
            None => {
                // C#: NullReferenceException on npc.TemplateId
                tracing::error!("AddQuestFromNpc: NPC {npc_obj_id} not found");
                false
            }
        }
    })
}

/// `Quests.AddQuestFromDoodad(questId, doodadObjId)`.
pub fn add_quest_from_doodad(
    server: &GameServer,
    player: &PlayerRef,
    quest_id: u32,
    doodad_obj_id: u32,
) -> Option<bool> {
    add_quest_from_doodad_impl(server, player, quest_id, doodad_obj_id, false)
}

fn add_quest_from_doodad_impl(
    server: &GameServer,
    player: &PlayerRef,
    quest_id: u32,
    doodad_obj_id: u32,
    answer_start_refusals: bool,
) -> Option<bool> {
    let me = WorldObject::from(player.clone());
    let doodad = server
        .world
        .world_of(&me)
        .and_then(|w| w.doodad(doodad_obj_id))
        .map(|d| d.data().template_id);
    with_quests(server, player, None, move |ctx| {
        ctx.answer_start_refusals = answer_start_refusals;
        match doodad {
            Some(t) => ctx.add_quest(quest_id, false, acceptor::DOODAD, t),
            None => {
                tracing::error!("AddQuestFromDoodad: doodad {doodad_obj_id} not found");
                false
            }
        }
    })
}

/// `Quests.AddQuestFromSphere(questId, sphereId)`.
pub fn add_quest_from_sphere(
    server: &GameServer,
    player: &PlayerRef,
    quest_id: u32,
    sphere_id: u32,
) -> Option<bool> {
    add_quest_from_sphere_impl(server, player, quest_id, sphere_id, false)
}

fn add_quest_from_sphere_impl(
    server: &GameServer,
    player: &PlayerRef,
    quest_id: u32,
    sphere_id: u32,
    answer_start_refusals: bool,
) -> Option<bool> {
    // Root-cause guard: the client names both the quest and the sphere, and the start-act check
    // (`starter_accepts`) only compares them with each other, so without this a client could
    // start any `QuestActConAcceptSphere` quest from anywhere. The character must be inside the
    // named sphere now, using the same area / sign-sphere geometry the sphere tick uses.
    // Automatic starters call `ctx.add_quest` directly and do not pass through here.
    let me = WorldObject::from(player.clone());
    let world = server.world.world_of(&me).map(|w| w.template.name.clone());
    let pos = world_position(server, &me);
    let inside = world.is_some_and(|world| {
        server.systems.quests.spheres.is_inside_sphere(
            &world,
            sphere_id,
            pos,
            super::spheres::START_TOLERANCE,
        )
    });
    if !inside {
        tracing::warn!(
            character = %player.data().unit.name,
            quest_id,
            sphere_id,
            "Client sphere quest start refused: the character is outside the sphere"
        );
        return Some(false);
    }
    with_quests(server, player, None, move |ctx| {
        ctx.answer_start_refusals = answer_start_refusals;
        ctx.add_quest(quest_id, false, acceptor::SPHERE, sphere_id)
    })
}

/// `Quests.AddQuestFromItem(questId, itemTemplateId)` (`AcceptQuestEffect`).
pub fn add_quest_from_item(
    server: &GameServer,
    player: &PlayerRef,
    quest_id: u32,
    item_template_id: u32,
) -> Option<bool> {
    with_quests(server, player, None, move |ctx| {
        ctx.add_quest(quest_id, false, acceptor::ITEM, item_template_id)
    })
}

/// `Quests.AddQuestFromSkill(questId, skillTemplateId)`.
pub fn add_quest_from_skill(
    server: &GameServer,
    player: &PlayerRef,
    quest_id: u32,
    skill_id: u32,
) -> Option<bool> {
    with_quests(server, player, None, move |ctx| {
        ctx.add_quest(quest_id, false, acceptor::SKILL, skill_id)
    })
}

/// `Quests.AddQuestFromBuff(questId, buffTemplateId)`.
pub fn add_quest_from_buff(
    server: &GameServer,
    player: &PlayerRef,
    quest_id: u32,
    buff_id: u32,
) -> Option<bool> {
    with_quests(server, player, None, move |ctx| {
        ctx.add_quest(quest_id, false, acceptor::BUFF, buff_id)
    })
}

/// `Quests.DropQuest(questId, update)` (`CSDropQuestContextPacket`).
pub fn drop_quest(server: &GameServer, player: &PlayerRef, quest_id: u32, update: bool) {
    run_or_queue(server, player, move |ctx| {
        ctx.drop_quest(quest_id, update, false)
    });
}

/// `AcceptQuestEffect.Apply` for a character target: `item` = the `SkillItem` caster's
/// (item template id, item template `ImplId == AcceptQuest`).
pub fn accept_quest_effect(
    server: &GameServer,
    player: &PlayerRef,
    quest_id: u32,
    item: Option<(u32, bool)>,
) {
    if let Some((item_template_id, is_accept_quest_item)) = item {
        if is_accept_quest_item {
            let item_quest_id = server
                .game_data
                .systems
                .quests
                .quest_id_from_starter_item(item_template_id);
            if item_quest_id > 0 {
                if add_quest_from_item(server, player, item_quest_id, item_template_id)
                    != Some(true)
                {
                    tracing::debug!(
                        "Failed to add Quest:{item_quest_id} from Item:{item_template_id}"
                    );
                }
                return;
            }
        }
    }
    add_quest(server, player, quest_id, false);
}

/// `DoodadFuncQuest.Use(caster, owner, skillId)` for a player caster.
pub fn doodad_func_quest(
    server: &GameServer,
    player: &PlayerRef,
    doodad_obj_id: u32,
    quest_id: u32,
) {
    if !has_quest(player, quest_id) {
        // A scheduled quest is offered (marker packet) only while its window is in progress.
        if !super::quest_schedule_allows(
            &server.game_data.systems.npcs.schedule,
            quest_id,
            crate::systems::npcs::spawn_manager::unix_now(),
        ) {
            return;
        }
        player.send(super::packets::SCDoodadQuestAcceptPacket {
            doodad_obj_id,
            quest_id,
        });
    } else {
        on_server_report(server, player, quest_id, 0, doodad_obj_id, 0);
    }
}

// ----- read-only helpers (take the player's data lock briefly) -----

/// `Quests.HasQuest(questId)`.
pub fn has_quest(player: &PlayerRef, quest_id: u32) -> bool {
    has_quest_in(&player.data(), quest_id)
}

/// `Quests.HasQuest(questId)` on already-locked character data (loot, doodad funcs).
pub fn has_quest_in(c: &Character, quest_id: u32) -> bool {
    if c.quest_state.loaded {
        c.quest_state.has_quest(quest_id)
    } else {
        c.quests.active.iter().any(|q| q.template_id == quest_id)
    }
}

/// `Quests.HasQuestCompleted(questId)` / `IsQuestComplete` on locked character data.
pub fn has_quest_completed_in(c: &Character, quest_id: u32) -> bool {
    super::engine::has_quest_completed(&c.quests.completed, quest_id)
}

/// `Quests.GetActiveActsWithUseItem(itemTemplateId)`: the parent component ids of the
/// matching acts (`UnitRequirementsGameData.CanUseSkill`'s area sphere special case).
pub fn active_components_with_use_item(
    c: &Character,
    data: &super::data::QuestData,
    item_template_id: u32,
) -> Vec<u32> {
    let mut out = Vec::new();
    let st = &c.quest_state;
    for h in st.active.values() {
        let Some(q) = st.quest(*h) else {
            continue;
        };
        let Some(si) = q.step_index(q.step) else {
            continue;
        };
        for comp in &q.steps[si].components {
            for a in &comp.acts {
                let at = &data.acts[a.template];
                if matches!(at.kind, super::templates::ActKind::ObjItemUse { item_id, .. } if item_id == item_template_id)
                    && !out.contains(&at.component_id)
                {
                    out.push(at.component_id);
                }
            }
        }
    }
    out
}

/// `Quests.Send()`: the active quests, 20 per `SCQuestsPacket` (GM commands; the login sends
/// them through the character snapshot). Uses the rows re-derived by the last operation.
pub fn send_quests(player: &PlayerRef) {
    const MAX_ENTRIES_PER_PACKET: usize = 20;
    let quests = player.data().quests.active.clone();
    let now = chrono::Utc::now().timestamp_millis();
    if quests.len() <= MAX_ENTRIES_PER_PACKET {
        player.send(crate::packets::world::SCQuestsPacket {
            quests,
            now_ms: now,
        });
        return;
    }
    for chunk in quests.chunks(MAX_ENTRIES_PER_PACKET) {
        player.send(crate::packets::world::SCQuestsPacket {
            quests: chunk.to_vec(),
            now_ms: now,
        });
    }
}

/// `Quests.SendCompleted()`: the completed quest blocks, 200 per `SCCompletedQuestsPacket`.
pub fn send_completed_quests(player: &PlayerRef) {
    const MAX_ENTRIES_PER_PACKET: usize = 200;
    let completed = player.data().quests.completed.clone();
    if completed.len() <= MAX_ENTRIES_PER_PACKET {
        player.send(crate::packets::world::SCCompletedQuestsPacket { quests: completed });
        return;
    }
    for chunk in completed.chunks(MAX_ENTRIES_PER_PACKET) {
        player.send(crate::packets::world::SCCompletedQuestsPacket {
            quests: chunk.to_vec(),
        });
    }
}
