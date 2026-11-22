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

/// Howling Abyss casts its final-room cue (cinemas 53 `id_265_01` and 133 `id_280_01`) on an
/// NPC. Play it for characters in the same room on this instance.
pub(super) fn broadcasts_room_cinema(cinema_id: u32) -> bool {
    matches!(cinema_id, 53 | 133)
}

/// The Howling Abyss hazard doodads: their clouts take a casting NPC as the trigger caster.
pub(super) fn is_hazard_clout(template_id: u32) -> bool {
    matches!(template_id, 5170 | 5383 | 5706 | 5708 | 6261 | 6273)
}

/// The floor spear windows. Their injuries last beyond the contact that applies them, and a
/// unit inside a spear window keeps its buff.
pub(super) fn clout_reapplies_contact(clout_id: u32) -> bool {
    matches!(clout_id, 1295 | 1334 | 1336)
}

/// Calleil's Hellfire clouts carry their payload without an inside buff.
pub(super) fn clout_payload_without_inside_buff(clout_id: u32) -> bool {
    matches!(clout_id, 1415 | 1542)
}

/// The thrown traps: the Bomber keg (49) and the Triste family's secret weapons (64, 66). They
/// fly ballistically, and their offset turns with the thrower.
pub(super) fn is_thrown_trap(template_id: u32) -> bool {
    matches!(template_id, 49 | 64 | 66)
}
