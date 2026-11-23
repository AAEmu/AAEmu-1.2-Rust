//! Halcyona's Tosare portal.

// ---- keys

const SKILL_ID: u32 = 17_292;

// ---- tables

pub(super) fn is_tosare_portal_skill(skill_id: u32) -> bool {
    skill_id == SKILL_ID
}

// ---- hooks

pub(super) struct Halcyona;

impl super::hooks::Encounter for Halcyona {}
