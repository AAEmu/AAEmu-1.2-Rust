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
