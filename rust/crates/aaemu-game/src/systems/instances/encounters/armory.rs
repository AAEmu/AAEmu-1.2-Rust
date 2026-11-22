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

/// Earthen Grip is marked Others, which the area filter interprets as neutral only.
/// Both it and Sorvell's barrel barrage select enemies that can receive their effects.
pub(super) fn hostile_area_skill(skill_id: u32) -> bool {
    matches!(skill_id, 15592 | 15668)
}

/// Hieronimus casts Howl while a separate effect places the ward: this cue runs once from the
/// caster.
pub(super) fn applies_to_source(skill_id: u32, effect_id: u32) -> bool {
    matches!((skill_id, effect_id), (17635, 19275))
}

/// Compact skill 15667 is shared by the two 1.2 Armory exit doodads (groups 8794 and 12257).
pub const ARMORY_EXIT_SKILL: u32 = 15667;

/// Leave Burnt Castle Armory also applies Return directly. It is offered on the
/// entrance exit and on the rear exit only after skill 18024 reveals it.
pub(super) fn is_exit_doodad(template_id: u32, func_group_id: u32) -> bool {
    matches!((template_id, func_group_id), (3673, 8794) | (4923, 12257))
}

/// Hell Lightning's initial target has only aimed the spell. Its placement step can
/// pick that player, then the event's hit history excludes them on later ticks.
pub(super) fn keeps_first_random_target(event_id: u32) -> bool {
    event_id == 1452
}

/// Thundering Strike's 24-second stack window is the length of four authored
/// cooldowns. Renew the window on a hit so the fifth one can petrify the target.
pub(super) fn refreshes_stack_window(buff_id: u32) -> bool {
    buff_id == 2407
}
