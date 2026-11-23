//! Halcyona's Tosare portal.

// ---- keys

const SKILL_ID: u32 = 17_292;

// ---- tables

pub(super) fn is_tosare_portal_skill(skill_id: u32) -> bool {
    skill_id == SKILL_ID
}

// ---- hooks

use crate::systems::skills::data::SkillTemplate;
use crate::systems::skills::geometry::Target;
use crate::systems::skills::validation::fail;
use crate::world::WorldObject;

fn cast_target_ready(template: &SkillTemplate, target: &Target) -> Result<(), (u8, u32)> {
    // Tosare's portal skill casts a Return effect directly, so it never runs Doodad.Use.
    // Require the machinery's open phase, where the client gets this skill.
    if is_tosare_portal_skill(template.id)
        && !target
            .as_obj()
            .and_then(WorldObject::as_doodad)
            .is_some_and(|doodad| {
                let d = doodad.data();
                d.template_id == 4499 && d.func_group_id == 10_996
            })
    {
        return fail("skill_invalid_target");
    }
    Ok(())
}

pub(super) struct Halcyona;

impl super::hooks::Encounter for Halcyona {
    fn cast_target_ready(
        &self,
        template: &SkillTemplate,
        target: &Target,
    ) -> Result<(), (u8, u32)> {
        cast_target_ready(template, target)
    }
}
