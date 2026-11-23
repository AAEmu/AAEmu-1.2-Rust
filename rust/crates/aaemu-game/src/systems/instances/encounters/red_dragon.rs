//! Red Dragon Egg summon.

// ---- keys

pub(crate) const RED_DRAGON_EGG_SKILL: u32 = 23_150;

// ---- tables

pub(super) fn is_red_dragon_egg_skill(skill_id: u32) -> bool {
    skill_id == RED_DRAGON_EGG_SKILL
}

// ---- hooks

pub(super) struct RedDragon;

impl super::hooks::Encounter for RedDragon {}
