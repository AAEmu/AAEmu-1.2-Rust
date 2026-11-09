//! `RenewEquipment` (57) and `PhysicalEnchantWeapon` / `PhysicalEnchantArmor` (20 / 21),
//! stage 1.
//!
//! Evidence (1.2 client, `D:/aa-re/client12/out-x2game/x2game.dll.c`):
//! * `X2RenewItem.Renew` (`FUN_394a95b0`) converts the two 1-based bag slots, range-checks
//!   them (toast `0x14`), then `FUN_394e92d0` reads both items from the bag (toast `0x1b` when
//!   one is missing, `0xce` when the operand is not a bag item), validates the pair with
//!   `FUN_397cca90` (toast `1`), and submits a normal `CSStartSkill` with the source item as
//!   `SkillItem` and the target item as `SkillCastItemTarget`. Renew is therefore not a
//!   dedicated packet.
//! * `FUN_397cca90`: armor -> source `base_equipment` true / target false and equal armor-type
//!   record; weapon -> source `base_equipment` true / target false, equal holdable slot type
//!   and renew category, renew category `1..3` (`FUN_397cc9e0`).
//! * `physical_enchant_abilities(id, npc_id, armor, enchant_level, min_friendship,
//!   success_ratio)` is loaded by `LoadPhysicalEnchantAbilityDescs` and attached to each NPC
//!   (`FUN_396ce260`, `npc_id` -> the NPC's list).
//! * Physical enchant is a **single-item** cast, not a renew pair: skills 10521 / 10522
//!   (`기본강화-무기` / `기본강화-방어구`) have `target_type_id 9` (Item), and the NPC dialog
//!   texts (ui_texts 1428-1463) describe one item, money and archium, with a 1x / 2x / 4x
//!   archium choice. They are ordinary common skills (auto-learn, no learn requirement,
//!   ability 0), so the client sends a plain `CSStartSkill` (caster = the player,
//!   target = `SkillCastItemTarget`) - not the renew `SkillItem` + target pair. The renew pair
//!   predicate is therefore not applied to 20 / 21.
//!
//! The 1.2 client and data do not say what a success does to the items (consumption,
//! transformation, cost); that is stage 2 and needs a capture (`IT2-STAGE2`). The money /
//! archium formula, the multiplier effect, the great-success chance and where "enchant
//! durability" is stored are also capture-blocked (`IT2-COST`). Stage 1 therefore validates,
//! checks the enchanter's row, rolls `success_ratio` and logs the outcome without mutating any
//! item.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::data::items::{ItemData, ItemTemplate, TemplateKind};
use crate::data::sqlite::for_each_row;
use crate::models::container::slot_type;
use crate::models::item::Item;
use crate::server::GameServer;
use crate::systems::economy::error_message;
use crate::systems::economy::inventory::{send_error_message, Inv};
use crate::systems::economy::loot::LootRng;
use crate::world::PlayerRef;

/// `physical_enchant_abilities`: one NPC's chance to bring an item to `enchant_level`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhysicalEnchantAbility {
    pub id: u32,
    pub npc_id: u32,
    /// `armor` (`'t'` = armor, `'f'` = weapon).
    pub armor: bool,
    pub enchant_level: i32,
    pub min_friendship: i32,
    pub success_ratio: i32,
}

/// The economy game data of the renew / physical-enchant feature.
#[derive(Debug, Default)]
pub struct RenewData {
    /// `physical_enchant_abilities` by NPC template id, in table order.
    pub physical_enchant_abilities: HashMap<u32, Vec<PhysicalEnchantAbility>>,
}

impl RenewData {
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let mut data = Self::default();
        for_each_row(conn, "SELECT * FROM physical_enchant_abilities", [], |r| {
            let a = PhysicalEnchantAbility {
                id: r.u32("id")?,
                npc_id: r.u32("npc_id")?,
                armor: r.bool_str("armor")?,
                enchant_level: r.i32("enchant_level")?,
                min_friendship: r.i32("min_friendship")?,
                success_ratio: r.i32("success_ratio")?,
            };
            data.physical_enchant_abilities
                .entry(a.npc_id)
                .or_default()
                .push(a);
            Ok(())
        })?;
        Ok(data)
    }

    /// The row an enchanter offers for `armor` at `enchant_level` (first row wins, as the
    /// client's list is keyed by `(npc_id, armor, enchant_level)` in the shipped data).
    pub fn ability(
        &self,
        npc_id: u32,
        armor: bool,
        enchant_level: i32,
    ) -> Option<&PhysicalEnchantAbility> {
        self.physical_enchant_abilities
            .get(&npc_id)
            .and_then(|rows| {
                rows.iter()
                    .find(|a| a.armor == armor && a.enchant_level == enchant_level)
            })
    }
}

/// The `PhysicalEnchantWeapon` (20) / `PhysicalEnchantArmor` (21) effect name for logs.
fn physical_enchant_name(armor: bool) -> &'static str {
    if armor {
        "PhysicalEnchantArmor"
    } else {
        "PhysicalEnchantWeapon"
    }
}

/// The 1.2 client's pair predicate `FUN_397cca90` (`X2Item.IsRenewableItem`).
///
/// Armor: the source is `base_equipment` and the target is not, with the same armor-type
/// record. Weapon: the source is `base_equipment` and the target is not, with the same
/// holdable slot type and renew category, and the renew category is `1..3`.
pub fn renewable_pair(data: &ItemData, source: &ItemTemplate, target: &ItemTemplate) -> bool {
    match (&source.kind, &target.kind) {
        (TemplateKind::Armor(s), TemplateKind::Armor(t)) => {
            s.base_equipment
                && !t.base_equipment
                && s.armor_type_id == t.armor_type_id
                && data.wearable_kind_of(source).is_some()
                && data.wearable_kind_of(target).is_some()
        }
        (TemplateKind::Weapon(s), TemplateKind::Weapon(t)) => {
            let (Some(sh), Some(th)) = (data.holdable_of(source), data.holdable_of(target)) else {
                return false;
            };
            s.base_equipment
                && !t.base_equipment
                && sh.slot_type_id == th.slot_type_id
                && sh.renew_category == th.renew_category
                && (1..=3).contains(&sh.renew_category)
        }
        _ => false,
    }
}

/// The item's current physical-enchant step.
///
/// INFERRED (`IT2-ENCHANT-LEVEL`): the 1.2 item detail carries the result in
/// `temper_physical`, whose shipped `item_cap_scales` rows roll `101..115`; the 10.x ladder
/// maps step `N` to `100 + N` (`EquipItem.ScaleMultiplier`). A value `<= 100` is step 0
/// (untempered), so the next attempt is step `1`. Stage 2 must confirm this from a capture.
pub fn enchant_step(item: &Item) -> i32 {
    i32::from(item.details.temper_physical)
        .saturating_sub(100)
        .max(0)
}

fn reject(player: &PlayerRef, error: i16) {
    send_error_message(player, error, 0, true);
    player.data().skill_char.skill_cancelled = true;
}

/// The enchanter NPC: the cast itself carries no NPC, so the 1.2 client keeps the blacksmith
/// interaction open (`CSInteractNPC` sets `current_interaction_object`). A unit caster / target
/// that is an NPC is accepted as well.
fn enchanter_template_id(
    server: &GameServer,
    player: &PlayerRef,
    explicit: Option<u32>,
) -> Option<u32> {
    if explicit.is_some() {
        return explicit;
    }
    let obj_id = player.data().unit.current_interaction_object;
    if obj_id == 0 {
        return None;
    }
    server
        .world
        .object(obj_id)
        .and_then(|o| o.as_npc().cloned())
        .map(|n| n.data().unit.template_id)
}

/// Stage 1 of `RenewEquipment` (57).
///
/// `source_slot` / `target_slot` are the bag slot indices the client put in the `SkillItem`
/// and `SkillCastItemTarget` (the client converts them from its 1-based UI slots). Returns
/// whether the cast may continue; on `false` an `SCErrorMsg` was sent and the skill cancelled.
#[allow(clippy::too_many_arguments)]
pub fn renew_equipment(
    server: &GameServer,
    player: &PlayerRef,
    source_item_id: u64,
    source_template_id: u32,
    source_slot: u32,
    target_item_id: u64,
    target_slot: u32,
) -> bool {
    let inv = Inv::of(player);
    let bag = inv.bag();
    let (bag_size, source, target) = {
        let g = bag.lock().expect("container poisoned");
        (
            g.size(),
            g.get_item_by_id(source_item_id).cloned(),
            g.get_item_by_id(target_item_id).cloned(),
        )
    };
    // `FUN_394a95b0`: both 0-based bag indices must be inside the bag (toast 0x14).
    if source_slot >= bag_size.max(0) as u32 || target_slot >= bag_size.max(0) as u32 {
        tracing::warn!(
            "RenewEquipment: {} operand slot out of range (source {source_slot}, target {target_slot}, bag {bag_size})",
            player.data().unit.name
        );
        reject(player, error_message::MONEY_LOCKED);
        return false;
    }
    // `FUN_397ccec0` reads the item at the slot from the bag; a different item at that slot is
    // the same "item missing" toast as an empty one. An item that exists in another container
    // (equipment / bank) is the client's wrong-slot-type toast (0xce) instead.
    let exists =
        inv.holder_of_any(source_item_id).is_some() && inv.holder_of_any(target_item_id).is_some();
    if !exists {
        tracing::warn!(
            "RenewEquipment: operand item missing (source {source_item_id}, target {target_item_id})"
        );
        reject(player, error_message::NOT_ENOUGH_ITEM);
        return false;
    }
    if source.is_none() || target.is_none() {
        tracing::warn!(
            "RenewEquipment: operand item is not in the bag (source {source_item_id}, target {target_item_id})"
        );
        reject(player, error_message::NOT_READY);
        return false;
    }
    let source_at_slot = inv.item_at(slot_type::INVENTORY, source_slot as u8);
    let target_at_slot = inv.item_at(slot_type::INVENTORY, target_slot as u8);
    if !source_at_slot.is_some_and(|i| i.id == source_item_id)
        || !target_at_slot.is_some_and(|i| i.id == target_item_id)
    {
        tracing::warn!(
            "RenewEquipment: operand item missing at its slot (source {source_item_id}, target {target_item_id})"
        );
        reject(player, error_message::NOT_ENOUGH_ITEM);
        return false;
    }
    let (Some(source), Some(target)) = (source, target) else {
        return false;
    };
    let data = &server.game_data.items;
    let (Some(source_template), Some(target_template)) = (
        data.template(source.template_id),
        data.template(target.template_id),
    ) else {
        tracing::warn!(
            "RenewEquipment: operand template missing (source {}, target {})",
            source.template_id,
            target.template_id
        );
        reject(player, error_message::NOT_ENOUGH_ITEM);
        return false;
    };
    // The client sends the template id in the `SkillItem`; a forged mismatch is the same
    // invalid item.
    if source.template_id != source_template_id {
        tracing::warn!(
            "RenewEquipment: source template mismatch ({} != {source_template_id})",
            source.template_id
        );
        reject(player, error_message::NOT_ENOUGH_ITEM);
        return false;
    }
    // `FUN_397cca90` (toast 1).
    if !renewable_pair(data, source_template, target_template) {
        tracing::info!(
            "RenewEquipment: pair {} / {} is not renewable",
            source.template_id,
            target.template_id
        );
        reject(player, error_message::INTERNAL_ERROR);
        return false;
    }
    // The pair is valid; the result needs a capture.
    tracing::info!(
        "renew: outcome pending capture (IT2-STAGE2) kind=renew source={source_item_id}/{} target={target_item_id}/{}",
        source.template_id,
        target.template_id
    );
    true
}

/// Stage 1 of `PhysicalEnchantWeapon` (20) / `PhysicalEnchantArmor` (21).
///
/// One item, taken from the cast's `SkillCastItemTarget`: the 1.2 dialog enchants a single
/// weapon / armor for money and archium (ui_texts 1428-1463), not the renew
/// base-equipment + material pair. The dialog selects a bag item (`X2Item.IsEnchantable` takes
/// a bag slot); the item is looked up in the bag by id. Returns whether the cast may continue;
/// on `false` an `SCErrorMsg` was sent and the skill cancelled.
///
/// **HARD RULE: stage 2 (any change to the item: grade, durability, destruction) must not land
/// until the `IT2-COST` rule (money + archium, the 1x / 2x / 4x multiplier) is settled and
/// enforced; otherwise it is a free enchant.**
pub fn physical_enchant(
    server: &GameServer,
    player: &PlayerRef,
    item_id: u64,
    armor: bool,
    npc_template_id: Option<u32>,
    rng: &mut dyn LootRng,
) -> bool {
    let name = physical_enchant_name(armor);
    let inv = Inv::of(player);
    let Some(item) = inv
        .bag()
        .lock()
        .expect("container poisoned")
        .get_item_by_id(item_id)
        .cloned()
    else {
        tracing::warn!("{name}: operand item missing from the bag ({item_id})");
        reject(player, error_message::NOT_ENOUGH_ITEM);
        return false;
    };
    let data = &server.game_data.items;
    let Some(template) = data.template(item.template_id) else {
        tracing::warn!("{name}: operand template missing ({})", item.template_id);
        reject(player, error_message::NOT_ENOUGH_ITEM);
        return false;
    };
    // ui_text 1436 `invalid_item_msg`: the dialog enchants weapons (20) or armor (21) only.
    let arm_matches = matches!(
        (armor, &template.kind),
        (false, TemplateKind::Weapon(_)) | (true, TemplateKind::Armor(_))
    );
    if !arm_matches {
        tracing::info!(
            "{name}: item {} is not the class of this enchant arm",
            item.template_id
        );
        reject(player, error_message::ITEM_CANNOT_USE);
        return false;
    }
    let Some(npc_id) = enchanter_template_id(server, player, npc_template_id) else {
        tracing::info!("{name}: no enchanter NPC for item {item_id}");
        reject(player, error_message::NOT_READY);
        return false;
    };
    let level = enchant_step(&item) + 1;
    let Some(ability) = server
        .game_data
        .systems
        .economy
        .renew
        .ability(npc_id, armor, level)
    else {
        // ui_text 1438 `lack_ability_msg`: the NPC's ladder ends above this step.
        tracing::info!(
            "{name}: no physical_enchant_abilities row for npc {npc_id} armor {armor} level {level}"
        );
        reject(player, error_message::NOT_READY);
        return false;
    };
    // Nothing grants NPC friendship in 1.2 (`track_friendship` is false for every NPC), so a
    // row above 0 can never be met. EVIDENCED data + column, rule INFERRED (`IT2-FRIENDSHIP`).
    if ability.min_friendship > 0 {
        tracing::info!(
            "{name}: npc {npc_id} level {level} needs friendship {} (unobtainable in 1.2)",
            ability.min_friendship
        );
        reject(player, error_message::NOT_ENOUGH_FRIENDSHIP);
        return false;
    }
    // ui_texts 1442-1446: the dialog needs money + archium and offers a 1x / 2x / 4x archium
    // investment. The formula is not in the 1.2 data (`skills.cost` 0, no reagents for 10521 /
    // 10522), so the money / archium gate stays capture-pending (`IT2-COST`): log only, never
    // refuse on a guessed cost.
    tracing::info!(
        "{name}: money/archium gate capture-pending (IT2-COST) npc={npc_id} level={level} item={item_id}"
    );
    let roll = rng.next_below(100);
    let success = roll < ability.success_ratio;
    tracing::info!(
        "{name}: outcome pending capture (IT2-STAGE2) npc={npc_id} level={level} ratio={} roll={roll} success={success} item={item_id}/{}",
        ability.success_ratio,
        item.template_id
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::item::ItemClass;

    fn item(temper_physical: u16) -> Item {
        Item {
            class: ItemClass::Armor,
            id: 1,
            template_id: 1,
            slot: 0,
            grade: 0,
            flags: 0,
            count: 1,
            lifespan_mins: 0,
            made_unit_id: 0,
            crafter_id: 0,
            world_id: 0,
            create_time: 0,
            unsecure_time: 0,
            unpack_time: 0,
            charge_start_time: 0,
            ucc_id: 0,
            details: crate::models::item::ItemDetails {
                temper_physical,
                ..Default::default()
            },
            bind_on_unpack: false,
            owner_id: 0,
            slot_type: slot_type::INVENTORY,
            expiration_time: 0,
            expiration_online_minutes_left: 0.0,
            charge_count: 0,
            dirty: false,
        }
    }

    #[test]
    fn enchant_step_reads_the_temper_above_100() {
        assert_eq!(enchant_step(&item(0)), 0);
        assert_eq!(enchant_step(&item(100)), 0);
        assert_eq!(enchant_step(&item(101)), 1);
        assert_eq!(enchant_step(&item(115)), 15);
    }
}
