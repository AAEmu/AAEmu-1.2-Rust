//! Windscour's Combuster.

// ---- keys

const SKILL_ID: u32 = 16_444;

// ---- tables

pub(super) fn is_combuster_self_destruct_skill(skill_id: u32) -> bool {
    skill_id == SKILL_ID
}

// ---- hooks

pub(super) struct Windscour;

impl super::hooks::Encounter for Windscour {}
