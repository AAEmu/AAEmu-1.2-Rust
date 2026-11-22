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
