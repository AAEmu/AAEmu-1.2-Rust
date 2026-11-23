//! Windscour's Combuster.

// ---- keys

const SKILL_ID: u32 = 16_444;

// ---- tables

pub(super) fn is_combuster_self_destruct_skill(skill_id: u32) -> bool {
    skill_id == SKILL_ID
}

// ---- hooks

use crate::systems::skills::data::{EffectKind, SkillTemplate};
use crate::systems::skills::geometry::Target;
use crate::world::WorldObject;

fn deferred_effect_replacement(
    t: &SkillTemplate,
    kind: EffectKind,
    id: u32,
    caster: &WorldObject,
    target: &Target,
) -> Option<(EffectKind, u32)> {
    // The Combuster actually casts Self-Destruct 16444. Its source-only damage (2729)
    // can leave it alive, so use the corpse-free removal authored for the same NPC
    // on Self-Destruct 15992. The explosion's damage and buff on nearby targets are
    // separate effects.
    let combuster_self_destruct = is_combuster_self_destruct_skill(t.id)
        && kind == EffectKind::DamageEffect
        && id == 2729
        && target.obj_id() == caster.obj_id()
        && caster
            .as_npc()
            .is_some_and(|npc| npc.data().unit.template_id == 986);
    // Broadcast the explosion from the caster's region before it disappears.
    combuster_self_destruct.then_some((EffectKind::KillNpcWithoutCorpseEffect, 647))
}

pub(super) struct Windscour;

impl super::hooks::Encounter for Windscour {
    fn deferred_effect_replacement(
        &self,
        t: &SkillTemplate,
        kind: EffectKind,
        id: u32,
        caster: &WorldObject,
        target: &Target,
    ) -> Option<(EffectKind, u32)> {
        deferred_effect_replacement(t, kind, id, caster, target)
    }
}
