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
