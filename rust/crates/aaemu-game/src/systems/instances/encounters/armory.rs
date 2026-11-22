//! Burnt Castle Armory: Sorvell, Hieronimus and the portal waves.

// ---- keys

/// The Armory's world template.
pub const WORLD: &str = "instance_burntcastle_armory";
/// The Armory's zone.
pub const ZONE: u32 = 236;

// ---- tables

/// Hieronimus's portal adds (8964..=8969) in the Armory.
pub(super) fn is_armory_wave_add(zone_id: u32, template_id: u32) -> bool {
    zone_id == 236 && (8964..=8969).contains(&template_id)
}

/// The Armory command sets that play an encounter intro (103, 106, 107).
pub(super) fn is_encounter_intro(cmd_set_id: u32) -> bool {
    matches!(cmd_set_id, 103 | 106 | 107)
}

/// Command set 103 uses the low-health staging actor for its opening lines.
/// Finish with its authored vanish skill once the combat boss has appeared.
///
/// `template_id` is only asked for command set 103.
pub(super) fn appended_intro_skill(
    cmd_set_id: u32,
    template_id: impl FnOnce() -> u32,
) -> Option<u32> {
    (cmd_set_id == 103 && template_id() == 10518).then_some(17196)
}

/// Hieronimus (cinema 8, `id_236_01`) reaches the players of his room through an area skill
/// that needs a playback cue.
pub(super) fn plays_cinema_directly(cinema_id: u32) -> bool {
    cinema_id == 8
}

/// The Burnt Castle Howl ward: summoned by its marker NPC, its protection runs as the doodad.
pub(super) fn is_howl_ward(template_id: u32, buff_id: u32) -> bool {
    template_id == 4626 && buff_id == 3615
}

/// Sorvell's barrel bomb, the gimmick that falls from the ceiling.
pub const FALLING_BARREL: u32 = 39;
