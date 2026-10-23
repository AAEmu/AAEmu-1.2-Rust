//! Gathering authorization is separate from `func_for_skill`'s skill-less chaining fallback.

use super::data::DoodadData;
use super::templates::{DoodadFunc, FuncTemplate};
use crate::server::GameServer;
use crate::systems::skills::data::{EffectTemplate, SkillTemplate};
use crate::world::{DoodadRef, WorldObject};

/// Intentional server compatibility policy for legacy funcs with no `func_skill_id`.
/// The shipped basic skills match their names, tool assets and InteractionEffects: ore
/// 11106 costs 3 LP, rock 11107 costs 0 LP, shear 11642 costs 0 LP and requires quest 266.
/// Local 1.2 UI/native selection of these skills was NOT verified. This adapter defines
/// server behavior rather than claiming native dispatch parity. Explicit bindings win;
/// the normal skill engine still enforces the selected template's costs/requirements.
#[derive(Clone, Copy)]
#[repr(u32)]
enum LegacyGatheringSkill {
    OreMine = 11106,
    RockMine = 11107,
    Shear = 11642,
}

pub(super) fn legacy_skill(template: &FuncTemplate) -> Option<u32> {
    Some(match template {
        FuncTemplate::OreMine(_) => LegacyGatheringSkill::OreMine,
        FuncTemplate::RockMine(_) => LegacyGatheringSkill::RockMine,
        FuncTemplate::Shear(_) => LegacyGatheringSkill::Shear,
        _ => return None,
    } as u32)
}

fn is_gathering_func(template: &FuncTemplate) -> bool {
    legacy_skill(template).is_some()
}

fn is_gathering_skill(server: &GameServer, id: u32) -> bool {
    let skills = &server.game_data.systems.skills;
    let Some(skill) = skills.skill(id) else {
        return false;
    };
    // Tool ids here identify the authored skill, not an equipped-item requirement. The
    // 1.2 pickaxe/shears assets are not inventory item templates. In particular, shearing
    // uses both Use (19) and SkinOff (67); interaction 19 alone is not authorization.
    skill.effects.iter().any(|effect| {
        let Some(EffectTemplate::Interaction {
            world_interaction, ..
        }) = effect
            .template
            .and_then(|(kind, id)| skills.effect(kind, id))
        else {
            return false;
        };
        match skill.mainhand_tool_id {
            2654 => *world_interaction == 73,
            15607 => matches!(world_interaction, 19 | 67),
            _ => false,
        }
    })
}

fn bound_skills(data: &DoodadData, func: &DoodadFunc) -> [u32; 2] {
    let template_skill = match data.func_template(func.func_id, &func.func_type) {
        Some(FuncTemplate::Use(t)) => t.skill_id,
        Some(FuncTemplate::FakeUse(t)) => t.fake_skill_id,
        _ => 0,
    };
    [func.skill_id, template_skill]
}

/// Check only gathering funcs and gathering Use/FakeUse dispatch. All other skill-less
/// fallback behavior, including automatic loot rows, retains its existing semantics.
pub(super) fn allows_func(
    server: &GameServer,
    func: &DoodadFunc,
    skill_id: u32,
    caster: Option<&WorldObject>,
) -> bool {
    let data = &server.game_data.systems.npcs.doodad;
    let Some(template) = data.func_template(func.func_id, &func.func_type) else {
        return true;
    };
    // Only characters pay labor and receive gathering loot. A Mount caster naming an
    // owned mate/slave must not advance a paid gathering phase on the character's behalf.
    let player_caster = caster.is_some_and(|caster| caster.as_player().is_some());
    if let Some(implicit) = legacy_skill(template) {
        let expected = if func.skill_id == 0 {
            implicit
        } else {
            func.skill_id
        };
        return player_caster && skill_id != 0 && skill_id == expected;
    }
    let bound = bound_skills(data, func);
    let dispatches_gathering = matches!(template, FuncTemplate::Use(_) | FuncTemplate::FakeUse(_))
        && (bound
            .iter()
            .any(|&id| id != 0 && is_gathering_skill(server, id))
            || (func.next_phase > 0
                && data
                    .funcs_for_group(func.next_phase as u32)
                    .iter()
                    .any(|next| {
                        data.func_template(next.func_id, &next.func_type)
                            .is_some_and(is_gathering_func)
                    })));
    !dispatches_gathering || (player_caster && skill_id != 0 && bound.contains(&skill_id))
}

/// Some interactions call the template directly rather than `DoFunc`.
pub(super) fn allows_template(
    server: &GameServer,
    doodad: &DoodadRef,
    template: &FuncTemplate,
    skill_id: u32,
    caster: Option<&WorldObject>,
) -> bool {
    if !is_gathering_func(template) {
        return true;
    }
    let group = doodad.data().func_group_id;
    server
        .game_data
        .systems
        .npcs
        .doodad
        .funcs_for_group(group)
        .iter()
        .any(|func| {
            func.func_type == template.func_type()
                && func.func_id == template.id()
                && allows_func(server, func, skill_id, caster)
        })
}

/// Resolve against the current phase before cast resources, buffs or effects change. The
/// phase executor checks again because a cast can finish after the phase has changed.
pub fn allows_skill(
    server: &GameServer,
    doodad: &DoodadRef,
    skill: &SkillTemplate,
    caster: &WorldObject,
) -> bool {
    let data = &server.game_data.systems.npcs.doodad;
    let group = doodad.data().func_group_id;
    // Restrict only skills that actually dispatch a doodad interaction. Unrelated combat
    // skills targeting a doodad must not acquire new gathering restrictions.
    let interacts = skill.effects.iter().any(|effect| {
        effect
            .template
            .and_then(|(kind, id)| server.game_data.systems.skills.effect(kind, id))
            .is_some_and(|effect| matches!(effect, EffectTemplate::Interaction { .. }))
    });
    if !interacts {
        return true;
    }
    match data.func_for_skill(group, skill.id) {
        Some(func) => allows_func(server, func, skill.id, Some(caster)),
        // No fallback row exists when the phase has only explicitly bound funcs. Refuse
        // a wrong gathering skill here too, before it can spend cast resources.
        None => data
            .funcs_for_group(group)
            .iter()
            .all(|func| allows_func(server, func, skill.id, Some(caster))),
    }
}
