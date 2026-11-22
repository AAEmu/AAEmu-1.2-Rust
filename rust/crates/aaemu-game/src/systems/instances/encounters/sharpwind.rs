//! Sharpwind Mines: the tornadoes, Okape, the Illusionist, Nerta, the Stone Brother (Colossus)
//! and the powder kegs.

// ---- keys

/// Sharpwind Mines' world template.
pub const WORLD: &str = "instance_cuttingwind_deadmine";
/// Sharpwind Mines' zone.
pub const ZONE: u32 = 262;

// ---- tables

/// These self-target plots project their strikes from the caster's heading.
pub(super) fn is_directional_self_skill(skill_id: u32) -> bool {
    matches!(skill_id, 15935 | 19980 | 18374 | 18401 | 18406 | 19248)
}

/// Keep incidental assistance near the pull.
pub(super) fn helps_only_near_the_pull(world: &str) -> bool {
    world == WORLD
}

/// Alistair's routes are run, with actor flags 4.
pub(super) fn route_runs_with_flags(route: &str) -> Option<u8> {
    route.starts_with("aipath_alistair").then_some(4)
}

/// At half health the Illusionist splits once the authored cooldown permits it.
pub(super) fn is_priority_combat_skill(skill_type: u32) -> bool {
    skill_type == 18485
}

/// The tornado (12014) and Okape (12188) run their spawn events as soon as they spawn.
pub(super) fn runs_spawn_events_on_spawn(template_id: u32) -> bool {
    matches!(template_id, 12014 | 12188)
}

/// The arena cue finds its audience and the tornado starts its hazard
/// buff once the casting unit is registered in the world.
pub(super) fn waits_for_visible_caster(skill_id: u32) -> bool {
    matches!(skill_id, 18983 | 19535)
}

/// Sharpwind Mines casts its final-room cue (cinema 51, `id_262_01`) on an NPC.
/// Play it for characters in the same room on this instance.
pub(super) fn broadcasts_room_cinema(cinema_id: u32) -> bool {
    cinema_id == 51
}

/// The Illusionist's frozen zone: its clout is cast by the NPC that summoned the doodad.
pub(super) fn is_frozen_zone(template_id: u32, clout_id: u32) -> bool {
    template_id == 5150 && clout_id == 1152
}

/// The powder keg, a worn pack, and the doodad it is dropped as.
pub const POWDER_KEG_DOODAD: u32 = 5951;
/// The powder keg item (equipped in the backpack slot).
pub const POWDER_KEG_ITEM: u32 = 25002;

/// The final-room cinematic timer belongs to the invisible cue actor, and the bandits' staff
/// pickups run once from the caster.
pub(super) fn applies_to_source(skill_id: u32, effect_id: u32) -> bool {
    matches!(
        (skill_id, effect_id),
        (19535, 24195) | (19019, 24232) | (19022, 24231)
    )
}

/// Nerta's Bomb Drop buff.
pub const NERTA_BOMB_BUFF: u32 = 4099;
/// The second Bomb Drop buff of the same name.
pub const NERTA_BOMB_BUFF_2: u32 = 4631;

/// Nerta (11362) under one of her Bomb Drop buffs. `npc_template_id` (the buff owner's NPC
/// template, `None` for another unit) is only asked for those buffs.
pub(super) fn is_nerta_bomb_drop(
    buff_id: u32,
    npc_template_id: impl FnOnce() -> Option<u32>,
) -> bool {
    matches!(buff_id, 4099 | 4631) && npc_template_id().is_some_and(|id| id == 11362)
}

/// The powder keg skill: the summon interaction consumes the actual equipped powder keg.
pub const POWDER_KEG_SKILL: u32 = 18715;
