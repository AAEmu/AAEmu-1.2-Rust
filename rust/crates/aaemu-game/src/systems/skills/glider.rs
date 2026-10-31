//! 1.2 glider rules shared by the skill, interaction and item-move entry points.
//!
//! Evidence:
//! * A glider is a back-slot item with `item_backpacks.backpack_type_id = 2` whose
//!   `items.use_skill_id` applies a `buffs.gliding = 't'` buff (S14 research, data check).
//! * The client's skill start resolves a `link_equip_slot_id` skill through the equipped item
//!   and requires the item's backpack type to equal `link_backpack_type_id`
//!   (`FUN_393925c0`); a failure raises `must_equip_proper_item` (326) before any packet.
//! * The client refuses to change the equipped backpack while gliding
//!   (`FUN_394e41f0`, error 543 `cannot_change_backpack_in_gliding`) and refuses interactions
//!   while gliding (`FUN_393b3d50`, error 334 `cannot_interact_while_gliding`).
//!
//! Not implemented here: an airborne deploy requirement. The 1.2 skill has only the authored
//! `unit_reqs` NotUnderWater and NoBuffTag 306 (`unit_reqs` 28592/28593), the client start path
//! `FUN_393989e0` checks only the linked item, and the startup lift is designed to take off from
//! the ground; the double-jump height / freefall gate remains capture-pending (S14-C4).

use crate::data::items::{backpack_type, TemplateKind};
use crate::models::container::equip_slot;
use crate::models::item::Item;
use crate::server::GameServer;
use crate::systems::economy::inventory::Inv;
use crate::world::{PlayerRef, WorldObject};

use super::data::SkillTemplate;

/// `skills.skill_controller_id` of every 1.2 glider deploy skill (31 rows; no
/// `skill_controllers` row 2687 exists, so the server never runs a controller for the player).
pub const GLIDER_SKILL_CONTROLLER_ID: u32 = 2687;

/// Whether `skill` is a glider skill: the deploy skill (`skill_controller_id` 2687) or one of
/// the linked glider UI skills (`link_backpack_type_id` 2, e.g. 14901 use / 17657 fold).
pub fn is_glider_skill(skill: &SkillTemplate) -> bool {
    skill.skill_controller_id == GLIDER_SKILL_CONTROLLER_ID
        || skill.link_backpack_type_id == backpack_type::GLIDER
}

/// Whether `skill` is a glider fold skill: linked to the glider type but with no back-slot
/// requirement of its own (`link_equip_slot_id` -1; 17657 "glider fold" and the later fold
/// variants). A fold ends the current flight.
pub fn is_glider_fold_skill(skill: &SkillTemplate) -> bool {
    skill.link_backpack_type_id == backpack_type::GLIDER && skill.link_equip_slot_id < 0
}

/// The character's equipped glider (`item_backpacks.backpack_type_id = 2` in the back slot).
pub fn equipped_glider(server: &GameServer, player: &PlayerRef) -> Option<Item> {
    let item = Inv::of(player).equipped(equip_slot::BACKPACK)?;
    let template = server.game_data.items.template(item.template_id)?;
    matches!(
        &template.kind,
        TemplateKind::Backpack {
            backpack_type: kind
        } if *kind == backpack_type::GLIDER
    )
    .then_some(item)
}

/// Whether the unit has an active buff with `buffs.gliding = 't'`, regardless of its movement
/// state. Buffs 7431 and 1675 (test / indun flight skills) are not `remove_on_land`, so the buff
/// alone does not mean the unit is still flying.
pub fn has_gliding_buff(server: &GameServer, unit: &WorldObject) -> bool {
    let data = &server.game_data.systems.skills;
    unit.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .any(|e| data.buff(e.template.id).is_some_and(|t| t.gliding))
    })
}

/// The 1.2 client's gliding state (stance 4 / `FUN_39357bc0`): an active gliding buff **while
/// airborne**. The movement state ends it; without that a lingering non-`remove_on_land` buff
/// (7431 / 1675) would keep the interaction and back-slot rules blocking a landed player.
pub fn is_gliding(server: &GameServer, unit: &WorldObject) -> bool {
    let data = &server.game_data.systems.skills;
    unit.with_unit(|u| {
        let u = u.unit();
        u.skill_state.airborne
            && u.buffs
                .effects
                .iter()
                .any(|e| data.buff(e.template.id).is_some_and(|t| t.gliding))
    })
    .unwrap_or(false)
}

/// Ends every active gliding buff on `unit`: the flight stops and the client falls (the buff
/// removal is the stop path, `FUN_39389d60`). Used when the glider item is gone but the buff is
/// still active, so the fold still ends the flight instead of being refused.
pub fn end_flight(server: &GameServer, unit: &WorldObject) -> bool {
    let data = &server.game_data.systems.skills;
    let indices: Vec<u32> = unit.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .filter(|e| data.buff(e.template.id).is_some_and(|t| t.gliding))
            .map(|e| e.index)
            .collect()
    });
    for index in &indices {
        super::buffs::exit(server, unit, *index, false);
    }
    !indices.is_empty()
}

/// The shared 334 check for every interaction entry path: a gliding player is refused and gets
/// `cannot_interact_while_gliding` (the client's `FUN_393b3d50` gate). Non-player casters (NPC
/// effects, area triggers) are left alone.
pub fn refuse_interaction(server: &GameServer, unit: &WorldObject) -> bool {
    let Some(player) = unit.as_player() else {
        return false;
    };
    if !is_gliding(server, unit) {
        return false;
    }
    crate::systems::economy::inventory::send_error_message(
        player,
        crate::systems::economy::error_message::CANNOT_INTERACT_WHILE_GLIDING,
        0,
        true,
    );
    true
}
