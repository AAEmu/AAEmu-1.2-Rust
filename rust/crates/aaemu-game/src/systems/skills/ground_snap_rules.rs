//! `GroundSnapRules` (special type 73): whether there is a floor to move a unit onto and what Z
//! that floor is.
//!
//! Ported from commit `c78b3a659`
//! (`AAEmu.Game/Models/Game/Skills/GroundSnapRules.cs`, `Models/Game/World/TerrainFloor.cs` and
//! their `AAEmu.UnitTests` cases). The C# samples the heightmap and snaps with lift / drop caps;
//! the sample itself comes from the collision service here, so this module only holds the policy.

/// Floor height the unit is put on (`TerrainFloor.ClearanceMetres`).
pub const CLEARANCE_METRES: f32 = 0.4;

/// Within this band of the sampled floor the unit sits on it (`NearBandMetres`).
pub const NEAR_BAND_METRES: f32 = 3.0;

/// Largest lift onto a sampled floor; a larger gap is a cliff or a bad sample
/// (`MaxUpwardSnapMetres`).
pub const MAX_UPWARD_SNAP_METRES: f32 = 8.0;

/// Largest drop onto a sampled floor; a deeper drop keeps the raw Z (`MaxDownwardSnapMetres`).
pub const MAX_DOWNWARD_SNAP_METRES: f32 = 12.0;

/// Water has to clear the seabed by this much before its surface wins (`WaterAboveSeabedMetres`).
pub const WATER_ABOVE_SEABED_METRES: f32 = 1.0;

/// The landing height, or `None` when there is no floor. A zero ground sample means the heightmap
/// is off or the cell is missing; moving the unit to Z 0 would drop it into the sea, so the effect
/// does nothing.
pub fn try_get_floor_z(
    raw_z: f32,
    ground: f32,
    over_water: bool,
    water_surface_z: f32,
) -> Option<f32> {
    if ground <= 0.0 {
        return None;
    }
    Some(choose_unit_floor_z(
        raw_z,
        ground,
        over_water,
        water_surface_z,
    ))
}

/// The snap itself: water surface over the seabed, the near band, a mild lift, a short drop, else
/// the raw Z is kept (a cliff or a bad sample).
pub fn choose_unit_floor_z(raw_z: f32, ground: f32, over_water: bool, water_surface_z: f32) -> f32 {
    if ground <= 0.0 {
        return raw_z;
    }
    if over_water && water_surface_z > ground + WATER_ABOVE_SEABED_METRES {
        return on_floor(water_surface_z);
    }
    if raw_z < ground - NEAR_BAND_METRES {
        // C# `OnFloor(rawZ)` on this branch, i.e. the raw Z plus the clearance; the raw Z is kept,
        // which is what the C# log line says it does, and a bad sample cannot lift a unit into a
        // cliff.
        if ground - raw_z > MAX_UPWARD_SNAP_METRES {
            return raw_z;
        }
        return on_floor(ground);
    }
    let drop = raw_z - ground;
    if drop <= MAX_DOWNWARD_SNAP_METRES {
        return on_floor(ground);
    }
    raw_z
}

fn on_floor(ground: f32) -> f32 {
    ground + CLEARANCE_METRES
}

#[cfg(test)]
#[path = "ground_snap_rules_tests.rs"]
mod ground_snap_rules_tests;
