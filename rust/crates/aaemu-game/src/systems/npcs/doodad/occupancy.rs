//! The 1.2 single-user rule of `doodad_almighties.once_one_man`.
//!
//! `once_one_interaction` (7 034 of 7 124 templates) is loaded but deliberately NOT enforced:
//! it is set on dungeon exits, respawn-bind records, feast / buff tables, statues and
//! trade-pack boxes, so it cannot mean "one user at a time" (unknown; see `PORTING.md`).
//!
//! Enforced only for `once_one_man` (984 templates, always together with the other column):
//!
//! * a timed interaction cast whose matched func is holdable (`DoodadFuncUse`,
//!   `DoodadFuncRemoveItem`, `DoodadFuncOpenPaper`, ...) holds the doodad while the cast runs
//!   and while its delayed effects are still pending;
//! * a seat / instrument (`DoodadFuncAttachment`) with `space <= 1` holds it for the whole
//!   occupation (the character's `Bonding`).
//!
//! Never held nor refused: multi-seat attachments (`space > 1`: the passenger code's
//! `load_passenger` capacity decides), and the func kinds that legitimately coexist with other
//! users ([`NON_HOLDING_FUNCS`]). Instant interactions never take an occupancy.
//!
//! A second unit's start is refused with the 1.2 error 4 `ALREADY_INTERACTING_SOMEONE_ELSE`
//! before any cast, labor or item cost. The occupancy is dropped when the cast ends (with its
//! pending effects), the seat is unbound, the occupant dies / logs out, or the doodad
//! despawns. The seat part follows the character's `bonding` (set by the attachment func,
//! cleared by `unbond_doodad` when the client sends `CSUnbondDoodad`), never the occupant's
//! movement: a seated client keeps sending its position, and a move frees neither the seat nor
//! `VehicleSeat`. [`permits`] also reaps an entry whose occupant is no longer in the doodad's
//! world, is dead, is no longer in a `Cast` task on this doodad and has no pending effect, so a
//! missed release cannot block a doodad forever.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::server::GameServer;
use crate::systems::npcs::doodad::data::DoodadData;
use crate::systems::npcs::doodad::templates::{DoodadFunc, FuncTemplate};
use crate::systems::skills::data::{EffectTemplate, SkillTemplate};
use crate::systems::skills::state::{SkillRun, SkillTaskKind};
use crate::world::{now_ms, DoodadRef, WorldObject};

/// Upper bound of tracked doodads (ids are reused on respawn, so a missed release must not grow
/// the map without bound). At the cap the check fails open: never block a doodad on a full map.
const MAX_OCCUPANCIES: usize = 4096;

/// A fresh entry without a cast task yet is still being accepted (the occupancy is taken
/// before the task slot is set); keep it live for this long so a second unit cannot drop it in
/// the window.
const START_GRACE_MS: i64 = 2_000;

/// A pending interaction effect keeps the hold at most this long after `mark_effects_pending`
/// (the skill's own delay + the grace, capped). If the `ApplySkillTask` never runs (its timeline
/// id changed), the entry must expire instead of holding the doodad while the occupant lives.
pub(crate) const PENDING_HOLD_MAX_MS: i64 = 30_000;

/// Func kinds that never make a `once_one_man` doodad single-user: instance exits, respawn-bind
/// records, buff / feast tables, counters, recovery, and the instant interaction kinds (loot,
/// fake use, purchase, skill hits).
const NON_HOLDING_FUNCS: &[&str] = &[
    "DoodadFuncExitIndun",
    "DoodadFuncBinding",
    "DoodadFuncBuff",
    "DoodadFuncFeast",
    "DoodadFuncInsertCounter",
    "DoodadFuncRecoverItem",
    "DoodadFuncLootItem",
    "DoodadFuncFakeUse",
    "DoodadFuncPurchase",
    "DoodadFuncSkillHit",
];

/// The unit holding a doodad (`once_one_man`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Occupancy {
    /// The occupant unit's object id.
    pub occupant_obj_id: u32,
    /// A timed use / gather cast is running (or was accepted and has not ended yet).
    pub casting: bool,
    /// The occupant sits / lies on the doodad (`DoodadFuncAttachment` bond).
    pub bonded: bool,
    /// The cast slot is already cleared but the interaction effect is still delayed
    /// (`Skill.ScheduleEffects` with a delay); the hold expires at this unix ms (0 = none).
    pub pending_until_ms: i64,
    /// Unix ms the entry was last taken / refreshed.
    pub since_ms: i64,
}

/// The occupancy entries of a `DoodadManager`.
#[derive(Debug, Default)]
pub struct Occupancies {
    entries: Mutex<HashMap<u32, Occupancy>>,
}

impl Occupancies {
    fn lock(&self) -> MutexGuard<'_, HashMap<u32, Occupancy>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Number of tracked doodads (tests / diagnostics).
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Test-only: age an entry's dated state by `ms` (the grace / pending-cap paths).
    #[cfg(test)]
    pub fn age_entry(&self, doodad_obj_id: u32, ms: i64) {
        if let Some(entry) = self.lock().get_mut(&doodad_obj_id) {
            entry.since_ms -= ms;
            if entry.pending_until_ms > 0 {
                entry.pending_until_ms -= ms;
            }
        }
    }
}

fn manager(server: &GameServer) -> &Occupancies {
    &server.systems.npcs.doodads.occupancy
}

/// The template's `once_one_man` flag (the only column this rule enforces).
fn once_one_man(data: &DoodadData, template_id: u32) -> bool {
    data.template(template_id).is_some_and(|t| t.once_one_man)
}

/// A template with any attachment seat of more than one place is not single-user: the seat's
/// `load_passenger` capacity decides who may sit where.
fn multi_seat(data: &DoodadData, template_id: u32) -> bool {
    data.template(template_id).is_some_and(|t| {
        t.func_groups.iter().any(|g| {
            data.funcs_for_group(g.id).iter().any(|f| {
                matches!(
                    data.func_template(f.func_id, &f.func_type),
                    Some(FuncTemplate::Attachment(a)) if a.space > 1
                )
            })
        })
    })
}

/// Whether a matched func makes the doodad single-user.
fn func_holds(data: &DoodadData, func: &DoodadFunc) -> bool {
    if NON_HOLDING_FUNCS.contains(&func.func_type.as_str()) {
        return false;
    }
    match data.func_template(func.func_id, &func.func_type) {
        Some(FuncTemplate::Attachment(a)) => a.space <= 1,
        _ => true,
    }
}

/// Whether a skill runs a doodad interaction (`InteractionEffect`), the only skills the rule
/// applies to. Unrelated combat skills that happen to target a doodad are not interactions.
pub fn is_interaction_skill(server: &GameServer, skill: &SkillTemplate) -> bool {
    let data = &server.game_data.systems.skills;
    skill.effects.iter().any(|effect| {
        effect
            .template
            .and_then(|(kind, id)| data.effect(kind, id))
            .is_some_and(|effect| matches!(effect, EffectTemplate::Interaction { .. }))
    })
}

/// Whether the `once_one_man` rule applies to an interaction of `skill` with `doodad` (the
/// template is flagged, the template has no multi-seat attachment, and the func the skill would
/// run is holdable).
pub fn holds_for_skill(server: &GameServer, doodad: &DoodadRef, skill: &SkillTemplate) -> bool {
    let template_id = doodad.data().template_id;
    let data = &server.game_data.systems.npcs.doodad;
    if !once_one_man(data, template_id) || multi_seat(data, template_id) {
        return false;
    }
    let group = doodad.data().func_group_id;
    data.func_for_skill(group, skill.id)
        .is_some_and(|func| func_holds(data, func))
}

/// Which hold a skill start takes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StartHold {
    /// Instant non-seat interaction: never blocked, never held.
    None,
    /// Timed interaction cast: held while the cast (and its delayed effects) runs.
    Cast,
    /// Instant seat / instrument use: not held by the cast, but refused while another unit
    /// occupies the seat (the bond acquires the occupancy when it runs).
    Seat,
}

fn start_hold(
    server: &GameServer,
    doodad: &DoodadRef,
    caster: &WorldObject,
    skill: &SkillRun,
) -> StartHold {
    if !is_interaction_skill(server, &skill.template) {
        return StartHold::None;
    }
    let template_id = doodad.data().template_id;
    let data = &server.game_data.systems.npcs.doodad;
    if !once_one_man(data, template_id) || multi_seat(data, template_id) {
        return StartHold::None;
    }
    let group = doodad.data().func_group_id;
    let Some(func) = data.func_for_skill(group, skill.template.id) else {
        return StartHold::None;
    };
    if !func_holds(data, func) {
        return StartHold::None;
    }
    if crate::systems::skills::skill::cast_time_ms(server, skill, caster) > 0 {
        return StartHold::Cast;
    }
    if matches!(
        data.func_template(func.func_id, &func.func_type),
        Some(FuncTemplate::Attachment(_))
    ) {
        return StartHold::Seat;
    }
    StartHold::None
}

/// Whether a skill start must be refused before any side effect (the in-use error is sent by
/// the client-facing caller).
pub fn refuses_start(
    server: &GameServer,
    doodad: &DoodadRef,
    caster: &WorldObject,
    skill: &SkillRun,
) -> bool {
    start_hold(server, doodad, caster, skill) != StartHold::None && !permits(server, doodad, caster)
}

/// Takes the hold of a skill start ([`StartHold::Cast`]); instant non-seat starts take none
/// and are always allowed while the doodad is free. `false` = another unit holds it.
///
/// A taken hold is recorded in the run ([`SkillRunState::holds_doodad`]), so `end_skill` only
/// visits the occupancy map for the skills that took one.
///
/// [`SkillRunState::holds_doodad`]: crate::systems::skills::state::SkillRunState::holds_doodad
pub fn begin_skill(
    server: &GameServer,
    doodad: &DoodadRef,
    caster: &WorldObject,
    skill: &SkillRun,
) -> bool {
    match start_hold(server, doodad, caster, skill) {
        StartHold::None => true,
        StartHold::Seat => permits(server, doodad, caster),
        StartHold::Cast => {
            let taken = begin(server, doodad, caster, false);
            if taken {
                skill.state().holds_doodad = true;
            }
            taken
        }
    }
}

/// Whether a *new timed* interaction of `skill_id` by `unit` must be refused at the execution
/// funnel. Loot / fake use / purchase / skill hits and every instant interaction pass; the
/// occupant's own pending effect also passes.
///
/// Every skill-bound `Doodad.Use` comes through here, so the cheap checks run first (the
/// template flag, the matched func, then whether another unit holds the doodad at all); the
/// classification of the skill, which builds a `SkillRun` and its effective cast time, only runs
/// when the unit would actually be refused.
pub fn refuses_new_timed(
    server: &GameServer,
    doodad: &DoodadRef,
    unit: &WorldObject,
    skill_id: u32,
) -> bool {
    if skill_id == 0 {
        return false;
    }
    let Some(template) = server.game_data.systems.skills.skill(skill_id).cloned() else {
        return false;
    };
    if !holds_for_skill(server, doodad, &template) || permits(server, doodad, unit) {
        return false;
    }
    if !is_interaction_skill(server, &template) {
        return false;
    }
    // The same timed predicate as `start_hold`: the effective cast time with the caster's real
    // skill level, stat and skill modifiers, so a cast reduced to 0 is instant in both places.
    let run = crate::systems::skills::skill::new_skill_for(server, template, Some(unit));
    crate::systems::skills::skill::cast_time_ms(server, &run, unit) > 0
}

/// Whether `unit` may start an interaction with `doodad` now. Stale entries are dropped first.
pub fn permits(server: &GameServer, doodad: &DoodadRef, unit: &WorldObject) -> bool {
    let template_id = doodad.data().template_id;
    let data = &server.game_data.systems.npcs.doodad;
    if !once_one_man(data, template_id) || multi_seat(data, template_id) {
        return true;
    }
    let mut map = manager(server).lock();
    let Some(entry) = map.get(&doodad.obj_id) else {
        return true;
    };
    if entry.occupant_obj_id == unit.obj_id() {
        return true;
    }
    if entry_holds(server, doodad.obj_id, entry) {
        return false;
    }
    map.remove(&doodad.obj_id);
    true
}

/// Takes the timed-cast occupancy unless another unit holds it (or a live seat occupation).
pub fn begin_cast(
    server: &GameServer,
    doodad: &DoodadRef,
    unit: &WorldObject,
    skill: &SkillRun,
) -> bool {
    if !holds_for_skill(server, doodad, &skill.template) {
        return true;
    }
    let taken = begin(server, doodad, unit, false);
    if taken {
        skill.state().holds_doodad = true;
    }
    taken
}

/// Takes / merges the seat occupation (`DoodadFuncAttachment`). Multi-seat attachments
/// (`space > 1`) take none: their seat capacity decides.
pub fn begin_bond(server: &GameServer, doodad: &DoodadRef, unit: &WorldObject, space: i32) -> bool {
    if space > 1 {
        return true;
    }
    let template_id = doodad.data().template_id;
    if !once_one_man(&server.game_data.systems.npcs.doodad, template_id) {
        return true;
    }
    begin(server, doodad, unit, true)
}

/// Marks that this cast's interaction effect is still to run after the cast slot was cleared
/// (`Skill.ScheduleEffects`), so the hold survives until `end_skill`. `delay_ms` is the whole
/// delay of the effect (animation time included); an instant effect is marked too, because
/// `cast` clears the slot before the effect runs and `end_skill` releases the hold, and
/// another unit must not take the doodad in that window.
pub fn mark_effects_pending(
    server: &GameServer,
    doodad: &DoodadRef,
    caster: &WorldObject,
    delay_ms: i64,
) {
    let map = manager(server);
    if map.is_empty() {
        return;
    }
    let now = now_ms();
    let mut entries = map.lock();
    if let Some(entry) = entries.get_mut(&doodad.obj_id) {
        if entry.occupant_obj_id == caster.obj_id() {
            entry.casting = true;
            entry.since_ms = now;
            entry.pending_until_ms = now
                + delay_ms
                    .saturating_add(START_GRACE_MS)
                    .min(PENDING_HOLD_MAX_MS);
        }
    }
}

/// Releases the cast part of the occupancy (the seat part stays while the unit is bonded).
///
/// A `DoodadFuncUse` starts the doodad's own skill cast, which can begin before the cast that
/// ran the func ends (`DoodadFuncUse` -> scheduled `UseSkill`). The unit is then still in a
/// `Cast` task on the same doodad, so the hold stays for that cast. The cheap empty check and
/// the unit read both run before the map lock: `entry_holds` takes the map first, and the two
/// orders must not cross.
pub fn end_cast(server: &GameServer, doodad_obj_id: u32, caster: &WorldObject) {
    let map = manager(server);
    if map.is_empty() {
        return;
    }
    let still_casting = caster
        .with_unit(|u| {
            u.unit()
                .skill_state
                .skill_task
                .as_ref()
                .is_some_and(|task| {
                    task.kind == SkillTaskKind::Cast
                        && task.skill.state().initial_target == doodad_obj_id
                })
        })
        .unwrap_or(false);
    let mut entries = map.lock();
    let remove = match entries.get_mut(&doodad_obj_id) {
        Some(entry) if entry.occupant_obj_id == caster.obj_id() => {
            entry.casting = still_casting;
            entry.pending_until_ms = 0;
            !entry.casting && !entry.bonded
        }
        _ => false,
    };
    if remove {
        entries.remove(&doodad_obj_id);
    }
}

/// Releases the seat / instrument part of the occupancy.
pub fn end_bond(server: &GameServer, doodad_obj_id: u32, unit_obj_id: u32) {
    let map = manager(server);
    if map.is_empty() {
        return;
    }
    let mut entries = map.lock();
    let remove = match entries.get_mut(&doodad_obj_id) {
        Some(entry) if entry.occupant_obj_id == unit_obj_id => {
            entry.bonded = false;
            !entry.casting
        }
        _ => false,
    };
    if remove {
        entries.remove(&doodad_obj_id);
    }
}

/// Drops every occupation of `unit_obj_id` (death, logout).
pub fn release_all_of(server: &GameServer, unit_obj_id: u32) {
    let map = manager(server);
    if map.is_empty() {
        return;
    }
    map.lock()
        .retain(|_, entry| entry.occupant_obj_id != unit_obj_id);
}

/// Drops a doodad's entry (despawn / delete; the object id may be reused).
pub fn forget(server: &GameServer, doodad_obj_id: u32) {
    let map = manager(server);
    if map.is_empty() {
        return;
    }
    map.lock().remove(&doodad_obj_id);
}

/// `SCErrorMsg(ALREADY_INTERACTING_SOMEONE_ELSE)`; characters only.
pub fn send_already_interacting(unit: &WorldObject) {
    if let Some(player) = unit.as_player() {
        crate::systems::economy::inventory::send_error_message(
            player,
            crate::systems::economy::error_message::ALREADY_INTERACTING_SOMEONE_ELSE,
            0,
            true,
        );
    }
}

/// The single-lock acquire: refuses while another unit holds a live entry, otherwise records
/// (or merges into) the caller's entry.
fn begin(server: &GameServer, doodad: &DoodadRef, unit: &WorldObject, bond: bool) -> bool {
    let obj_id = doodad.obj_id;
    let mut entries = manager(server).lock();
    if let Some(entry) = entries.get(&obj_id) {
        if entry.occupant_obj_id != unit.obj_id() {
            if entry_holds(server, obj_id, entry) {
                return false;
            }
            entries.remove(&obj_id);
        }
    }
    if entries.len() >= MAX_OCCUPANCIES && !entries.contains_key(&obj_id) {
        entries.retain(|id, entry| entry_holds(server, *id, entry));
        if entries.len() >= MAX_OCCUPANCIES {
            // Fail open: a full map must never block an interaction.
            return true;
        }
    }
    let entry = entries.entry(obj_id).or_insert(Occupancy {
        occupant_obj_id: unit.obj_id(),
        ..Occupancy::default()
    });
    if entry.occupant_obj_id != unit.obj_id() {
        return true; // cannot happen after the checks above; never block on it
    }
    entry.since_ms = now_ms();
    if bond {
        entry.bonded = true;
    } else {
        entry.casting = true;
    }
    true
}

/// Whether an entry still describes a real occupation. Called with the occupancy map locked;
/// it reads the doodad's template flag-free state and the occupant's data, never a doodad data
/// lock. A unit that left the world, died, is no longer casting on this doodad and has no
/// pending effect, and is not bonded to it, is stale.
fn entry_holds(server: &GameServer, doodad_obj_id: u32, entry: &Occupancy) -> bool {
    let Some(doodad) = server.world.object(doodad_obj_id) else {
        return false;
    };
    if doodad.as_doodad().is_none() {
        return false;
    }
    let Some(world) = server.world.world_of(&doodad) else {
        return false;
    };
    let Some(occupant) = world.object(entry.occupant_obj_id) else {
        return false;
    };
    let alive = occupant.with_unit(|u| u.unit().hp > 0).unwrap_or(true);
    if !alive {
        return false;
    }
    let now = now_ms();
    if entry.pending_until_ms > now {
        return true;
    }
    if now - entry.since_ms < START_GRACE_MS {
        return true;
    }
    let casting = entry.casting
        && occupant
            .with_unit(|u| {
                u.unit()
                    .skill_state
                    .skill_task
                    .as_ref()
                    .is_some_and(|task| {
                        task.kind == SkillTaskKind::Cast
                            && task.skill.state().initial_target == doodad_obj_id
                    })
            })
            .unwrap_or(false);
    let bonded = entry.bonded
        && occupant.as_player().is_some_and(|p| {
            p.data()
                .bonding
                .is_some_and(|b| b.owner_obj_id == doodad_obj_id)
        });
    casting || bonded
}
