//! Hadir Farm: Velarre, the poison puddles and tombstones, and the Rotten Flesh Golem.

// ---- keys

/// Hadir Farm's world template.
pub const WORLD: &str = "instance_hadir_farm";
/// Hadir Farm's zone.
pub const ZONE: u32 = 241;

// ---- tables

/// The Hadir command sets that play an encounter intro (65, 79..=81, 126).
pub(super) fn is_encounter_intro(cmd_set_id: u32) -> bool {
    matches!(cmd_set_id, 65 | 79..=81 | 126)
}

/// Keep incidental assistance near the pull.
pub(super) fn helps_only_near_the_pull(world: &str) -> bool {
    world == WORLD
}

/// The poison puddles and tombstones are anchored at the encounter's
/// summoned doodad. Timed phases can arrive without an explicit caster.
pub(super) fn is_hazard_clout(template_id: u32) -> bool {
    matches!(template_id, 3874 | 3956)
}
