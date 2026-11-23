//! Wind Sealstone quest launch.

// ---- keys

const SKILL_ID: u32 = 17_539;

// ---- tables

pub(super) fn is_wind_sealstone_launch_skill(skill_id: u32) -> bool {
    skill_id == SKILL_ID
}

// ---- hooks

pub(super) struct WindSealstone;

impl super::hooks::Encounter for WindSealstone {}
