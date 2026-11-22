//! Howling Abyss, Normal and Greater: the Medusa rooms under the walkways, Viboros, Calleil and
//! the floor spears.

// ---- keys

/// Howling Abyss (Normal)'s world template.
pub const WORLD: &str = "instance_howling_abyss";
/// Greater Howling Abyss' world template.
pub const GREATER_WORLD: &str = "instance_howling_abyss_2";
/// Howling Abyss (Normal)'s zone.
pub const ZONE: u32 = 265;
/// Greater Howling Abyss' zone.
pub const GREATER_ZONE: u32 = 280;

// ---- tables

/// Howling Abyss NPCs whose rooms sit under a raised walkway: the Medusa packs, their
/// Guardians and the Knights on the ledges above them (Normal 265 and Greater 280). They only
/// notice, or answer a help call against, a unit on their own floor.
pub(super) fn howling_floor_bound(zone_id: u32, template_id: u32) -> bool {
    matches!(zone_id, 265 | 280)
        && matches!(
            template_id,
            11258 | 11264 | 11757 | 11759 | 12765 | 12784 | 12785
        )
}

/// The height difference (m) at which [`howling_floor_bound`] NPCs treat a unit as being on
/// another floor.
pub const HOWLING_FLOOR_GAP: f32 = 3.0;

/// The raised passages have another walkable floor only a few meters underneath.
pub(super) fn raised_floor_drop(zone_id: u32) -> Option<f32> {
    matches!(zone_id, 265 | 280).then_some(2.5)
}
