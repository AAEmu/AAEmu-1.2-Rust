//! Doodad func item requirements checked before the cast instead of only in the func body.
//!
//! The 1.2 client sends `CSStartSkill` for every doodad use and has no pre-check for these
//! skills (`link_equip_slot_id` is -1); it stops the cast only when `SCSkillStarted` carries a
//! non-zero result. The 10.x zone decompile (`x2game-dev_dedicate.dll.c`) has only the
//! `DoodadFuncConsumeChanger` descriptor loader/factory (2841893-2842045, 2605824-2606182) and
//! no use or can-use check, so the result codes are inferred (see `PORTING.md`).

use std::sync::PoisonError;

use crate::models::container::{equip_slot, slot_type};
use crate::server::GameServer;
use crate::systems::economy::inventory::Inv;
use crate::systems::skills::data::SkillTemplate;
use crate::systems::skills::types::skill_result;
use crate::world::{DoodadRef, WorldObject};

use super::funcs::error_message;
use super::templates::FuncTemplate;

/// Why a doodad-use skill is refused before the cast: the `SCSkillStarted` result and value,
/// plus the `SCErrorMsg` the func body answers with (when it has one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillItemRefusal {
    pub result: u8,
    pub value: u32,
    pub error: Option<i16>,
}

impl SkillItemRefusal {
    fn new(result: u8, value: u32, error: Option<i16>) -> Self {
        Self {
            result,
            value,
            error,
        }
    }
}

/// The item requirement of the func the skill dispatches in the doodad's current phase.
///
/// Returns `None` when the caster is not a character (NPC / script paths keep their effect-time
/// behaviour) or when the func's requirement is met. The effect-time checks stay in place: the
/// phase, the equipped item or the bag can change while a cast is running.
pub fn skill_item_refusal(
    server: &GameServer,
    doodad: &DoodadRef,
    skill: &SkillTemplate,
    caster: &WorldObject,
) -> Option<SkillItemRefusal> {
    let player = caster.as_player()?;
    let data = &server.game_data.systems.npcs.doodad;
    let group = doodad.data().func_group_id;
    let func = data.func_for_skill(group, skill.id)?;
    let template = data.func_template(func.func_id, &func.func_type)?;
    let need_reagent = |value, error| {
        Some(SkillItemRefusal::new(
            skill_result::NEED_REAGENT,
            value,
            error,
        ))
    };
    match template {
        FuncTemplate::ConsumeChanger(f) => {
            let equipment = player.data().inventory.equipment().clone();
            let source = equipment
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_item_by_slot(f.slot_id as i32)
                .cloned();
            let Some(source) = source else {
                return need_reagent(0, Some(error_message::UNKNOWN_ITEM));
            };
            if !data
                .consume_changer_items(f.id)
                .contains(&source.template_id)
            {
                return need_reagent(0, Some(error_message::STORE_INVALID_ITEM));
            }
            None
        }
        FuncTemplate::RemoveItem(f) => {
            if !Inv::of(player).check_items(slot_type::INVENTORY, f.item_id, f.count) {
                return need_reagent(f.item_id, None);
            }
            None
        }
        FuncTemplate::BuyFish(_) | FuncTemplate::ConvertFish(_) => {
            if Inv::of(player).equipped(equip_slot::BACKPACK).is_none() {
                return need_reagent(0, Some(error_message::STORE_BACKPACK_NOGOODS));
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "item_gate_tests.rs"]
mod tests;
