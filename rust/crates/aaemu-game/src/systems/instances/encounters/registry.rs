//! The encounter registry: the one place engine code asks about encounter-specific ids.
//!
//! Each function delegates to the tables of the encounters it concerns. A lookup that more than
//! one encounter answers asks them in the order of the id list it replaced; the lists do not
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
    armory::is_wave_add(zone_id, template_id)
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

// ---- skills, plots, buffs and controllers

/// `(skill, effect)` pairs applied to the caster whatever their authored application method:
/// scripted cues that run once from the caster (Burnt Castle Armory, Howling Abyss, Sharpwind
/// Mines).
pub fn applies_to_source(skill_id: u32, effect_id: u32) -> bool {
    armory::applies_to_source(skill_id, effect_id)
        || howling_abyss::applies_to_source(skill_id, effect_id)
        || sharpwind::applies_to_source(skill_id, effect_id)
}

/// The skill a doodad's `SkillHit` receives instead of the cast skill (Palace Cellar).
/// `doodad_template_id` is only asked for a skill that has one.
pub fn doodad_hit_skill(skill_id: u32, doodad_template_id: impl FnOnce() -> u32) -> Option<u32> {
    palace_cellar::doodad_hit_skill(skill_id, doodad_template_id)
}

/// NPC area skills that select hostile units whatever their authored relation (Burnt Castle
/// Armory).
pub fn hostile_area_skill(skill_id: u32) -> bool {
    armory::hostile_area_skill(skill_id)
}

/// Skills whose targets are only the doodads that accept their `SkillHit` (Kroloal Cradle).
pub fn hits_only_accepting_doodads(skill_id: u32) -> bool {
    cradle::hits_only_accepting_doodads(skill_id)
}

/// Plot events that target and affect their caster alone (Palace Cellar).
pub fn plot_targets_caster(skill_id: u32, event_id: u32) -> bool {
    palace_cellar::plot_targets_caster(skill_id, event_id)
}

/// `hit_once` plot events that may pick their previous target again (Burnt Castle Armory).
pub fn keeps_first_random_target(event_id: u32) -> bool {
    armory::keeps_first_random_target(event_id)
}

/// Buffs whose stack timers are renewed by a new stack (Burnt Castle Armory).
pub fn refreshes_stack_window(buff_id: u32) -> bool {
    armory::refreshes_stack_window(buff_id)
}

/// A Nerta Bomb Drop buff on Nerta herself (Sharpwind Mines). `npc_template_id` (the buff
/// owner's NPC template, `None` for another unit) is only asked for a Bomb Drop buff.
pub fn is_nerta_bomb_drop(buff_id: u32, npc_template_id: impl FnOnce() -> Option<u32>) -> bool {
    sharpwind::is_nerta_bomb_drop(buff_id, npc_template_id)
}

/// Nerta's Bomb Drop buffs (Sharpwind Mines).
pub use super::sharpwind::{NERTA_BOMB_BUFF, NERTA_BOMB_BUFF_2};

/// Buffs whose skill controller the server runs on a character too (Hadir Farm).
pub fn server_runs_buff_controller(buff_id: u32) -> bool {
    hadir::server_runs_buff_controller(buff_id)
}

/// The caster a buff trigger takes as its source instead of the buff's owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerCaster {
    /// A floor spear doodad applied the Injured buff (Howling Abyss; Kroloal Cradle's gate
    /// traps reuse spear 5516).
    FloorSpear,
    /// Lady Velarre applied the tombstone buff (Hadir Farm).
    VelarreTombstone,
}

/// Buff triggers whose source is the caster that applied the buff, not its owner (Howling
/// Abyss floor spears, Hadir Farm's Velarre). The caster's doodad and NPC templates (`None`
/// for another kind) are only asked for those buffs. The two have different buffs, so at
/// most one applies.
pub fn trigger_source_is_caster(
    buff_id: u32,
    caster_doodad: impl FnOnce() -> Option<u32>,
    caster_npc: impl FnOnce() -> Option<u32>,
) -> Option<TriggerCaster> {
    if howling_abyss::floor_spear(buff_id, caster_doodad) {
        Some(TriggerCaster::FloorSpear)
    } else if hadir::velarre_tombstone(buff_id, caster_npc) {
        Some(TriggerCaster::VelarreTombstone)
    } else {
        None
    }
}

/// Dash controllers that charge to the selected victim (Howling Abyss).
pub fn dashes_to_victim(controller_id: u32) -> bool {
    howling_abyss::dashes_to_victim(controller_id)
}

/// Zones whose controller and knock-back ground probes are bounded by the walker drop
/// (Howling Abyss).
pub fn probes_raised_floors(zone_id: u32) -> bool {
    howling_abyss::probes_raised_floors(zone_id)
}

/// Leave Burnt Castle Armory, the skill of both exit doodads.
pub use super::armory::ARMORY_EXIT_SKILL;
/// The powder keg skill (Sharpwind Mines).
pub use super::sharpwind::POWDER_KEG_SKILL;

/// The two Burnt Castle Armory exit doodads `(template, func group)` that offer
/// [`ARMORY_EXIT_SKILL`].
pub fn is_armory_exit_doodad(template_id: u32, func_group_id: u32) -> bool {
    armory::is_exit_doodad(template_id, func_group_id)
}

/// The Kroloal Cradle destruction device skill that unlocks the hidden-room chests.
pub use super::cradle::CRADLE_CHEST_SKILL;

// ---- plots

/// How a plot event's area target finds its ground, for the encounter events that do not use
/// the generic `Max(previous Z + offset, ground)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlotGround {
    /// The Colossus's hand smash (Sharpwind Mines); `near` for the near circle's events.
    ColossusFist { near: bool },
    /// Hieronimus's meteor launch event (Burnt Castle Armory).
    MeteorLaunch,
    /// Hieronimus's meteor impact event (Burnt Castle Armory).
    MeteorImpact,
    /// Calleil's falling fireball marker events (Howling Abyss).
    CalleilFireball,
    /// Calleil's fireball impact events (Howling Abyss).
    CalleilImpact,
}

/// The encounter ground rule of a plot event. The caster's `zone_id` is only asked for the
/// Colossus skills. The dungeons' skills differ, so at most one rule applies.
pub fn plot_ground(
    skill_id: u32,
    event_id: u32,
    zone_id: impl FnOnce() -> u32,
) -> Option<PlotGround> {
    armory::plot_ground(skill_id, event_id)
        .or_else(|| howling_abyss::plot_ground(skill_id, event_id))
        .or_else(|| sharpwind::plot_ground(skill_id, event_id, zone_id))
}

// ---- spawn effects

/// The boss whose aggro target a `SpawnEffect` summon of this template inherits (Howling
/// Abyss).
pub fn summon_boss(sub_type: u32) -> Option<u32> {
    howling_abyss::summon_boss(sub_type)
}

/// `NpcSpawnerSpawnEffect`s whose adds attack the caster's aggro target (Burnt Castle Armory,
/// Howling Abyss).
pub fn is_wave_spawn(
    caster_template: u32,
    spawner_id: u32,
    use_summoner_aggro_target: bool,
) -> bool {
    armory::is_wave_spawn(caster_template, spawner_id, use_summoner_aggro_target)
        || howling_abyss::is_wave_spawn(caster_template, spawner_id, use_summoner_aggro_target)
}

// ---- physics and collision

/// Worlds whose water bodies are kept per instance instead of per world template (Howling
/// Abyss).
pub fn instance_scoped_water(world: &str) -> bool {
    howling_abyss::instance_scoped_water(world)
}

/// How a world's water volumes are read from its cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaterCells {
    /// Indoor pools are kept (Kroloal Cradle, Palace Cellar).
    pub indoor_pools: bool,
    /// The pools are river sectors (Howling Abyss).
    pub river_sectors: bool,
    /// The heightmap cell (0, 0) is loaded before any player enters (Kroloal Cradle).
    pub preload_origin: bool,
}

/// The water cell rules of a world template.
pub fn water_cells(template_name: &str) -> WaterCells {
    WaterCells {
        indoor_pools: cradle::has_indoor_pools(template_name)
            || palace_cellar::has_indoor_pools(template_name),
        river_sectors: howling_abyss::has_river_sectors(template_name),
        preload_origin: cradle::preloads_origin_cell(template_name),
    }
}

/// Doodads `(template, func group)` that are not world colliders because they reuse their
/// closed mesh for the open animation (Palace Cellar).
pub fn door_reuses_closed_mesh(template_id: u32, func_group: u32) -> bool {
    palace_cellar::door_reuses_closed_mesh(template_id, func_group)
}

// ---- World content

/// Adjust the computed chase range for the melee stats used by Plateau Earth Elemental.
pub(crate) fn melee_chase_range(
    t: &crate::systems::ai::behaviors::TemplateScalars,
    range: f32,
) -> f32 {
    super::plateau::melee_chase_range(t, range)
}

/// Whether a world with the instance prefix keeps its inactive-at-start spawners.
pub fn keeps_inactive_spawners(world_name: &str) -> bool {
    super::library::keeps_inactive_spawners(world_name)
}
