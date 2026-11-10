//! The 1.2 single-user rule of `doodad_almighties.once_one_interaction` / `once_one_man`.
//!
//! Both columns are loaded ([`super::data`], [`super::templates::DoodadTemplate`]) but neither
//! the C# port nor this one ever read them. The 1.2 client carries the matching message
//! `ui_text` 5 `ALREADY_INTERACTING_SOMEONE_ELSE` ("Target in-use by someone else.",
//! `error_message::ALREADY_INTERACTING_SOMEONE_ELSE` = 4). Research (medium-low confidence on
//! the exact split between the two columns; see
//! `D:/aaemu-audits/gap-audit-2026-11-10/research-data-rules.md` section 3):
//!
//! * `once_one_interaction` (7034 of 7124 templates): a *timed* use / gather cast holds the
//!   doodad while it runs; an instant interaction neither holds nor is blocked by its own kind.
//! * `once_one_man` (984 templates, always together with `once_one_interaction`): the same
//!   occupancy, held for the whole occupation of a seat / instrument
//!   (`DoodadFuncAttachment`, the character's `Bonding`) and across the phase change that follows
//!   a successful pick.
//!
//! One entry per doodad object id, holding at most one unit. [`permits`] instant interactions
//! are also refused while another unit holds the doodad: "another unit's attempt to start an
//! interaction with that doodad is refused". Every exit releases ([`end_cast`] from
//! `Skill.EndSkill`, [`end_bond`] from the unbind / move paths, [`release_all_of`] on death and
//! logout, [`forget`] when the doodad despawns). On top of that [`permits`] drops an entry whose
//! occupant is no longer in the doodad's world, is dead, or no longer in the state the entry
//! records, so a missed release cannot block a doodad forever.
//!
//! The map is a leaf lock: it is never held while a doodad / unit data lock is held by a caller,
//! and a map entry is validated by looking the occupant up in its world (no doodad data lock is
//! held; the flags are read and the guard dropped first).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::server::GameServer;
use crate::systems::skills::data::{EffectTemplate, SkillTemplate};
use crate::systems::skills::state::{SkillRun, SkillTaskKind};
use crate::world::{DoodadRef, WorldObject};

/// Upper bound of tracked doodads (ids are reused on respawn, so a missed release must not grow
/// the map without bound). At the cap the check fails open: never block a doodad on a full map.
const MAX_OCCUPANCIES: usize = 4096;

/// The unit holding a doodad (`once_one_interaction` / `once_one_man`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Occupancy {
    /// The occupant unit's object id.
    pub occupant_obj_id: u32,
    /// A timed use / gather cast is running (or was accepted and has not ended yet).
    pub casting: bool,
    /// The occupant sits / lies on the doodad (`DoodadFuncAttachment` bond).
    pub bonded: bool,
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
}

fn manager(server: &GameServer) -> &Occupancies {
    &server.systems.npcs.doodads.occupancy
}

/// The two single-user flags of a doodad's template.
fn flags(server: &GameServer, doodad: &DoodadRef) -> (bool, bool) {
    let template_id = doodad.data().template_id;
    server
        .game_data
        .systems
        .npcs
        .doodad
        .template(template_id)
        .map_or((false, false), |t| (t.once_one_interaction, t.once_one_man))
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

/// [`permits`] for a skill cast: refuses when another unit holds the doodad. Errors are sent to
/// a player caster only. Taking the occupancy of a timed cast is [`begin_cast`]'s job, so a
/// refusal here happens before any cast state changes.
pub fn permits_interaction(
    server: &GameServer,
    doodad: &DoodadRef,
    caster: &WorldObject,
    skill: &SkillRun,
) -> bool {
    if !is_interaction_skill(server, &skill.template) {
        return true;
    }
    if permits(server, doodad, caster) {
        return true;
    }
    send_already_interacting(caster);
    false
}

/// Whether `unit` may start an interaction with `doodad` now. Stale entries (occupant gone,
/// dead, no longer casting / bonded) are dropped first.
pub fn permits(server: &GameServer, doodad: &DoodadRef, unit: &WorldObject) -> bool {
    let (once_one_interaction, once_one_man) = flags(server, doodad);
    if !once_one_interaction && !once_one_man {
        return true;
    }
    let mut map = manager(server).lock();
    let Some(entry) = map.get(&doodad.obj_id) else {
        return true;
    };
    if entry.occupant_obj_id == unit.obj_id() {
        return true;
    }
    if entry_is_live(server, doodad.obj_id, entry) {
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
    if !is_interaction_skill(server, &skill.template) {
        return true;
    }
    let (once_one_interaction, _) = flags(server, doodad);
    !once_one_interaction || begin(server, doodad, unit, false)
}

/// Takes / merges the seat occupation (`DoodadFuncAttachment`).
pub fn begin_bond(server: &GameServer, doodad: &DoodadRef, unit: &WorldObject) -> bool {
    let (_, once_one_man) = flags(server, doodad);
    !once_one_man || begin(server, doodad, unit, true)
}

/// Releases the cast part of the occupancy (the seat part stays while the unit is bonded).
///
/// A `DoodadFuncUse` starts the doodad's own skill cast, which can begin before the cast that
/// ran the func ends (`DoodadFuncUse` -> scheduled `UseSkill`). The unit is then still in a
/// `Cast` task on the same doodad, so the hold stays for that cast. The unit check runs before
/// the map lock: `entry_is_live` takes the map first, and the two orders must not cross.
pub fn end_cast(server: &GameServer, doodad_obj_id: u32, caster: &WorldObject) {
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
    let map = manager(server);
    if map.is_empty() {
        return;
    }
    let mut entries = map.lock();
    let remove = match entries.get_mut(&doodad_obj_id) {
        Some(entry) if entry.occupant_obj_id == caster.obj_id() => {
            entry.casting = still_casting;
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
            if entry_is_live(server, obj_id, entry) {
                return false;
            }
            entries.remove(&obj_id);
        }
    }
    if entries.len() >= MAX_OCCUPANCIES && !entries.contains_key(&obj_id) {
        entries.retain(|id, entry| entry_is_live(server, *id, entry));
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
    if bond {
        entry.bonded = true;
    } else {
        entry.casting = true;
    }
    true
}

/// Whether an entry still describes a real occupation. Called with the occupancy map locked;
/// it reads the doodad's template flag-free state and the occupant's data, never a doodad data
/// lock. A unit that left the world, died, stopped its cast or lost the seat is stale.
fn entry_is_live(server: &GameServer, doodad_obj_id: u32, entry: &Occupancy) -> bool {
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
    let casting = entry.casting
        && occupant
            .with_unit(|u| {
                u.unit()
                    .skill_state
                    .skill_task
                    .as_ref()
                    .is_some_and(|task| task.kind == SkillTaskKind::Cast)
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
