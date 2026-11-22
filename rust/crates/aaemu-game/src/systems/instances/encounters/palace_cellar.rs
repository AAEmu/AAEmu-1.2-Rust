//! Palace Cellar (Sal temple): Melkina, Tushan, the flame braziers, the boulder and the doors.

// ---- keys

/// Palace Cellar's world template.
pub const WORLD: &str = "instance_sal_temple";
/// Palace Cellar's zone.
pub const ZONE: u32 = 240;

// ---- tables

/// The raised passages have another walkable floor only a few meters underneath.
pub(super) fn raised_floor_drop(zone_id: u32) -> Option<f32> {
    (zone_id == 240).then_some(2.5)
}

/// Akmit (cinema 9, `id_240_01`) reaches the players of the room through an area skill that
/// needs a playback cue.
pub(super) fn plays_cinema_directly(cinema_id: u32) -> bool {
    cinema_id == 9
}

/// The Cellar flame follows the brazier's phase, even when the bucket is used
/// beside a different prop. Phase changes must remove the previous fire area.
pub(super) fn is_flame_clout(template_id: u32, buff_id: u32) -> bool {
    matches!(template_id, 4109 | 4741) && buff_id == 2783
}

/// The burning boulder that rolls down the Cellar ramp.
pub const ROLLING_BOULDER: u32 = 42;

/// Both rooms' no-attack areas accept Melkina's original open cue.
pub(super) fn doodad_hit_skill(
    skill_id: u32,
    doodad_template_id: impl FnOnce() -> u32,
) -> Option<u32> {
    (skill_id == 20615 && doodad_template_id() == 5947).then_some(17422)
}

/// Tushan's reward is tied to his death. Require neither another unit within
/// the 2 m plot shape nor a height lookup that can place it beneath the floor.
pub(super) fn plot_targets_caster(skill_id: u32, event_id: u32) -> bool {
    skill_id == 20615 && event_id == 5347
}

/// The Sal temple's pools are indoor water volumes.
pub(super) fn has_indoor_pools(template_name: &str) -> bool {
    template_name == WORLD
}

/// The Palace Cellar doors reuse the closed mesh for the open animation.
pub(super) fn door_reuses_closed_mesh(template_id: u32, func_group: u32) -> bool {
    matches!(
        (template_id, func_group),
        (4104, 9965 | 10027) | (4106, 10005 | 10006) | (4128, 10018 | 10019) | (4577, 11228)
    )
}

// ---- hooks

/// Palace Cellar's encounter hooks.
pub struct PalaceCellar;

impl super::hooks::Encounter for PalaceCellar {}
