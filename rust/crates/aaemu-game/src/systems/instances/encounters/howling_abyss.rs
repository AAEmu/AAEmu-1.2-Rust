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

/// Godfrey's timed dialogue continuation would multiply across his audience, and the
/// final-room cinematic timer belongs to the invisible cue actor: these cues run once from the
/// caster.
pub(super) fn applies_to_source(skill_id: u32, effect_id: u32) -> bool {
    matches!(
        (skill_id, effect_id),
        (19372, 23742)
            | (21422, 29282)
            | (21423, 29284)
            | (21426, 29291)
            | (19442, 23953)
            | (21477, 29402)
    )
}

/// The floor spear's Injured trigger is authored with the victim as its
/// source. Use the trap that applied it so self-attack rules do not stop
/// the environmental damage. Kroloal Cradle's gate traps reuse spear 5516.
pub(super) fn floor_spear(buff_id: u32, caster_doodad: impl FnOnce() -> Option<u32>) -> bool {
    buff_id == 3999 && caster_doodad().is_some_and(|id| matches!(id, 5516 | 5706 | 5708))
}

/// These boss charge rows use value1 as travel time and no fixed displacement.
/// The selected victim supplies the endpoint for that data shape.
pub(super) fn dashes_to_victim(controller_id: u32) -> bool {
    matches!(controller_id, 6640 | 6683 | 7130 | 7149)
}

/// The zones whose ground probes stay within the walker drop, so a raised floor is not left
/// for the floor underneath.
pub(super) fn probes_raised_floors(zone_id: u32) -> bool {
    matches!(zone_id, 265 | 280)
}
