//! Plateau Earth Elemental.

use crate::systems::ai::behaviors::TemplateScalars;

// ---- keys
// The original rule keys on NPC stats and the computed range.

// ---- tables

/// Chase range after the correction for weaponless melee NPCs.
pub(super) fn melee_chase_range(t: &TemplateScalars, mut range: f32) -> f32 {
    if t.base_skill_id == 2 && t.skills_count == 0 && range == 4.0 {
        range -= 1.0; // Fix that ID=7927, Plateau Earth Elemental can hit with a melee attack
    }
    range
}

// ---- hooks

pub(super) struct Plateau;

impl super::hooks::Encounter for Plateau {}
