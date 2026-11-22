//! The encounter registry: the one place engine code asks about dungeon-specific ids.
//!
//! Each function delegates to the tables of the dungeons it concerns. A lookup that more than
//! one dungeon answers asks them in the order of the id list it replaced; the lists do not
//! overlap, so the order never changes a result. Arguments that cost a lock are passed as
//! closures and are only called where the replaced code read them.

use super::{armory, cradle, hadir, howling_abyss, palace_cellar, sharpwind};

// ---- AI and command sets

/// Command sets whose `FollowPath` routes are walked at the actor's own walking pace
/// (Kroloal Cradle). See [`cradle::walks_scene_routes`].
pub fn walks_scene_routes(cmd_set_id: u32) -> bool {
    cradle::walks_scene_routes(cmd_set_id)
}

/// Command sets that play an encounter intro: while one runs or is queued, an aggro target
/// change does not interrupt it (Hadir Farm, Burnt Castle Armory).
pub fn is_encounter_intro(cmd_set_id: u32) -> bool {
    hadir::is_encounter_intro(cmd_set_id) || armory::is_encounter_intro(cmd_set_id)
}

/// The actor flags of a `FollowPath` route that is run rather than walked (Sharpwind Mines).
pub fn route_runs_with_flags(route: &str) -> Option<u8> {
    sharpwind::route_runs_with_flags(route)
}

/// A skill that a `RunCommandSet` appends to its command set for the target template
/// (Burnt Castle Armory). `template_id` is only asked when the command set has one.
pub fn appended_intro_skill(cmd_set_id: u32, template_id: impl FnOnce() -> u32) -> Option<u32> {
    armory::appended_intro_skill(cmd_set_id, template_id)
}

/// Templates whose spawn events run as soon as their AI is set up (Sharpwind Mines).
pub fn runs_spawn_events_on_spawn(template_id: u32) -> bool {
    sharpwind::runs_spawn_events_on_spawn(template_id)
}

/// NPC skills that wait until the casting unit is visible in the world (Sharpwind Mines).
pub fn waits_for_visible_caster(skill_id: u32) -> bool {
    sharpwind::waits_for_visible_caster(skill_id)
}

/// Self-target skills that aim, and check their cast path, like targeted ones (Sharpwind
/// Mines).
pub fn is_directional_self_skill(skill_id: u32) -> bool {
    sharpwind::is_directional_self_skill(skill_id)
}

/// A big monster's combat skill that is picked before a random one whenever it is available
/// (Sharpwind Mines).
pub fn is_priority_combat_skill(skill_type: u32) -> bool {
    sharpwind::is_priority_combat_skill(skill_type)
}

/// NPCs that only notice, or answer a help call against, a unit on their own floor (Howling
/// Abyss). See [`howling_abyss::howling_floor_bound`].
pub fn floor_bound(zone_id: u32, template_id: u32) -> bool {
    howling_abyss::howling_floor_bound(zone_id, template_id)
}

/// The height difference (m) at which [`floor_bound`] NPCs treat a unit as being on another
/// floor.
pub const FLOOR_GAP: f32 = howling_abyss::HOWLING_FLOOR_GAP;

/// Worlds whose incidental aggro help stays near the pull (Sharpwind Mines, Hadir Farm).
pub fn helps_only_near_the_pull(world: &str) -> bool {
    sharpwind::helps_only_near_the_pull(world) || hadir::helps_only_near_the_pull(world)
}

/// Hieronimus's portal adds (Burnt Castle Armory).
pub fn is_wave_add(zone_id: u32, template_id: u32) -> bool {
    armory::is_armory_wave_add(zone_id, template_id)
}

/// The deepest floor (m) under a walker in a zone with raised passages (Palace Cellar, Howling
/// Abyss); `None` elsewhere.
pub fn raised_floor_drop(zone_id: u32) -> Option<f32> {
    palace_cellar::raised_floor_drop(zone_id).or_else(|| howling_abyss::raised_floor_drop(zone_id))
}

// ---- cinemas

/// Final-room cinemas cast on an NPC and played for the characters in the same room of the
/// instance (Sharpwind Mines, Howling Abyss).
pub fn broadcasts_room_cinema(cinema_id: u32) -> bool {
    sharpwind::broadcasts_room_cinema(cinema_id) || howling_abyss::broadcasts_room_cinema(cinema_id)
}

/// Cinemas a `CinemaEffect` plays for the character it hits instead of only remembering them
/// for the quests (Burnt Castle Armory, Palace Cellar, Kroloal Cradle).
pub fn plays_cinema_directly(cinema_id: u32) -> bool {
    armory::plays_cinema_directly(cinema_id)
        || palace_cellar::plays_cinema_directly(cinema_id)
        || cradle::plays_cinema_directly(cinema_id)
}

// ---- doodads and clouts

/// The encounter areas of a doodad's clout that keep their own caster and payload rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloutClass {
    /// The Burnt Castle Howl ward (Armory).
    HowlWard,
    /// A Howling Abyss hazard doodad.
    HowlingHazard,
    /// A Hadir Farm poison puddle or tombstone.
    HadirHazard,
    /// The Illusionist's frozen zone (Sharpwind Mines).
    FrozenZone,
    /// The Palace Cellar brazier flame.
    PalaceFlame,
}

/// The encounter class of a doodad's clout, from the doodad template, the clout id and its
/// buff. The classes have no template in common, so at most one applies.
pub fn clout_class(template_id: u32, clout_id: u32, buff_id: u32) -> Option<CloutClass> {
    if armory::is_howl_ward(template_id, buff_id) {
        Some(CloutClass::HowlWard)
    } else if howling_abyss::is_hazard_clout(template_id) {
        Some(CloutClass::HowlingHazard)
    } else if hadir::is_hazard_clout(template_id) {
        Some(CloutClass::HadirHazard)
    } else if sharpwind::is_frozen_zone(template_id, clout_id) {
        Some(CloutClass::FrozenZone)
    } else if palace_cellar::is_flame_clout(template_id, buff_id) {
        Some(CloutClass::PalaceFlame)
    } else {
        None
    }
}

/// Clouts that keep applying their contact buff while a unit stays inside (Kroloal Cradle,
/// Howling Abyss).
pub fn clout_reapplies_contact(clout_id: u32) -> bool {
    cradle::clout_reapplies_contact(clout_id) || howling_abyss::clout_reapplies_contact(clout_id)
}

/// Clout buffs that are a contact hit: not removed when the unit leaves (Kroloal Cradle).
pub fn is_contact_buff(buff_id: u32) -> bool {
    cradle::is_contact_buff(buff_id)
}

/// Clout buffs that only reach a unit under the water surface in this world (Kroloal Cradle).
pub fn clout_requires_submersion(world: &str, buff_id: u32) -> bool {
    cradle::clout_requires_submersion(world, buff_id)
}

/// Clouts without an inside buff that still apply all their effects (Howling Abyss).
pub fn clout_payload_without_inside_buff(clout_id: u32) -> bool {
    howling_abyss::clout_payload_without_inside_buff(clout_id)
}

/// The powder keg doodad (Sharpwind Mines).
pub use super::sharpwind::POWDER_KEG_DOODAD;
/// The powder keg item (Sharpwind Mines).
pub use super::sharpwind::POWDER_KEG_ITEM;

// ---- gimmicks

/// Sorvell's barrel, the gimmick that falls from the ceiling (Burnt Castle Armory).
pub use super::armory::FALLING_BARREL;
/// The rolling boulder (Palace Cellar).
pub use super::palace_cellar::ROLLING_BOULDER;

/// Thrown trap gimmicks: ballistic, with an offset that turns with the thrower (Howling Abyss).
pub fn is_thrown_trap(template_id: u32) -> bool {
    howling_abyss::is_thrown_trap(template_id)
}
