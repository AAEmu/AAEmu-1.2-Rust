//! `ItemManager.Load` (item templates and every item table it reads): `item_configs`,
//! `item_look_convert_*`, `item_grades`, `holdables`, `wearables`, `wearable_kinds`,
//! `wearable_slots`, `dyeable_items`, `equip_item_attr_modifiers`, `item_procs`,
//! `item_proc_bindings`, `equip_item_set_bonuses`, the class tables (`item_armors`, `item_weapons`,
//! `item_accessories`, `item_summon_mates`, `item_summon_slaves`, `item_body_parts`,
//! `item_enchanting_gems`, `item_backpacks`), the hard-coded UCC / music / treasure templates, the
//! base `items` table, `equip_slot_enchanting_costs`, `item_grade_enchanting_supports`,
//! `item_socket_chances`, `item_cap_scales`, `item_cap_scale_forbids`, `item_grade_distributions`,
//! `loot_pack_dropping_npcs`, `doodad_func_convert_fish_items`, `item_spawn_doodads`,
//! `unit_modifiers` (items), `armor_grade_buffs`, `item_sets` / `item_set_items`, and the
//! "invalid_item_" names. The localized search strings live in `systems::economy::items_data`
//! (they need the localization tables).

use std::collections::{HashMap, HashSet};

use super::sqlite;
use crate::models::unit::BonusTemplate;

mod ids;
mod load;
mod templates;
pub use ids::{
    armor_type, backpack_kind, backpack_type, bind_type, equipment_item_slot_type, item_category,
    item_grade, item_impl, proc_chance_kind, shop_currency_type, ITEM_APPRAISAL_CERTIFICATE,
    ITEM_BOUND_TAX_CERTIFICATE, ITEM_COINS, ITEM_CREST_INK, ITEM_CREST_STAMP, ITEM_DAWN_STONE,
    ITEM_SALON_CERTIFICATE, ITEM_SHEET_MUSIC, ITEM_TAX_CERTIFICATE,
    ITEM_TREASURE_MAP_WITH_COORDINATES,
};
pub use templates::{
    ArmorGradeBuff, AttributeModifiers, BackpackTemplate, BodyPartTemplate, EquipItemSet,
    EquipItemSetBonus, EquipSlotEnchantingCost, EquipSlotGroup, EquipSlotGroupMap, EquipTemplate,
    FullGradeTemplate, GradeDistributions, GradeTemplate, Holdable, ItemCapScale, ItemConfig,
    ItemGradeEnchantingSupport, ItemLookConvert, ItemProcTemplate, ItemSet, ItemSetItem,
    ItemSocket, ItemSocketLevelLimit, ItemSocketNumLimit, ItemTemplate, LootPackConvertFish,
    LootPackDroppingNpc, RuneTemplate, TemplateKind, Wearable, WearableKind, WearableSlot,
    EQUIPMENT_ITEM_SLOT_TYPE_BACKPACK,
};

/// Item game data (`ItemManager`'s template tables).
#[derive(Debug, Clone, Default)]
pub struct ItemData {
    pub config: ItemConfig,
    /// `_grades` (the short form; see [`ItemData::full_grades`]).
    pub grades: HashMap<i32, GradeTemplate>,
    pub templates: HashMap<u32, ItemTemplate>,
    /// `_templates` insertion order (template ids; filled by [`ItemData::load`]).
    pub template_order: Vec<u32>,
    /// `_grades` with every column.
    pub full_grades: HashMap<i32, FullGradeTemplate>,
    /// `_gradesOrdered` (grade order -> grade id).
    pub grades_ordered: HashMap<i32, i32>,
    /// `ItemManager.MaxGradeId` / `MaxGradeValue`.
    pub max_grade_id: i32,
    pub max_grade_value: i32,
    /// `_holdables`.
    pub holdables: HashMap<u32, Holdable>,
    /// `_wearables` (key `type_id * 128 + slot_type_id`).
    pub wearables: HashMap<u32, Wearable>,
    /// `_wearableKinds`.
    pub wearable_kinds: HashMap<u32, WearableKind>,
    /// `_wearableSlots`.
    pub wearable_slots: HashMap<u32, WearableSlot>,
    /// `_defaultDyeIds`.
    pub default_dye_ids: HashMap<u32, u32>,
    /// `_modifiers`.
    pub modifiers: HashMap<u32, AttributeModifiers>,
    /// `_itemProcTemplates`.
    pub item_procs: HashMap<u32, ItemProcTemplate>,
    /// `item_proc_bindings` grouped by item id (the C# never loaded this table).
    pub item_proc_bindings: HashMap<u32, Vec<u32>>,
    /// `_equipItemSets`.
    pub equip_item_sets: HashMap<u32, EquipItemSet>,
    /// `_itemLookConverts`, `_holdableItemLookConverts`, `_wearableItemLookConverts`.
    pub item_look_converts: HashMap<u32, ItemLookConvert>,
    pub holdable_item_look_converts: HashMap<u32, u32>,
    pub wearable_item_look_converts: HashMap<u32, u32>,
    /// `RuneTemplate` / `BackpackTemplate` / `BodyPartTemplate` specifics by item id.
    pub runes: HashMap<u32, RuneTemplate>,
    pub backpacks: HashMap<u32, BackpackTemplate>,
    pub body_parts: HashMap<u32, BodyPartTemplate>,
    /// `_enchantingCosts` (by slot type).
    pub enchanting_costs: HashMap<u32, EquipSlotEnchantingCost>,
    /// `_enchantingSupports` (by item id).
    pub enchanting_supports: HashMap<u32, ItemGradeEnchantingSupport>,
    /// `_socketChance`.
    pub socket_chance: HashMap<u32, u32>,
    /// `item_socket_num_limits` (by equip slot type and grade).
    pub item_socket_num_limits: HashMap<(u32, i32), ItemSocketNumLimit>,
    /// `item_socket_level_limits` (by lunagem item id).
    pub item_socket_level_limits: HashMap<u32, ItemSocketLevelLimit>,
    /// `item_sockets` (by lunagem item id).
    pub item_sockets: HashMap<u32, ItemSocket>,
    /// `equip_slot_groups` (by id).
    pub equip_slot_groups: HashMap<u32, EquipSlotGroup>,
    /// `equip_slot_group_maps` (by equip slot group id, in table order).
    pub equip_slot_group_maps: HashMap<u32, Vec<EquipSlotGroupMap>>,
    /// `_itemCapScales` (by skill id).
    pub item_cap_scales: HashMap<u32, ItemCapScale>,
    /// `item_cap_scale_forbids.item_id` (the 1.2 client `FUN_395a72d0` set): item templates
    /// that cannot be the target of `ItemCapScale` / its reset.
    pub item_cap_scale_forbids: HashSet<u32>,
    /// `_itemGradeDistributions`.
    pub grade_distributions: HashMap<i32, GradeDistributions>,
    /// `_lootPackDroppingNpc` (by npc template id, rows in table order).
    pub loot_pack_dropping_npc: HashMap<u32, Vec<LootPackDroppingNpc>>,
    /// `_lootPackConvertFish` (by item id).
    pub loot_pack_convert_fish: HashMap<u32, Vec<LootPackConvertFish>>,
    /// `_itemDoodadTemplates` (doodad id -> item ids).
    pub item_doodads: HashMap<u32, Vec<u32>>,
    /// `_itemUnitModifiers`.
    pub item_unit_modifiers: HashMap<u32, Vec<BonusTemplate>>,
    /// `_armorGradeBuffs` (armor type -> grade -> buff).
    pub armor_grade_buffs: HashMap<u32, HashMap<u32, ArmorGradeBuff>>,
    /// `_itemSets`.
    pub item_sets: HashMap<u32, ItemSet>,
}

impl ItemData {
    pub fn template(&self, id: u32) -> Option<&ItemTemplate> {
        self.templates.get(&id)
    }

    /// `item_body_parts` row of an item (the `BodyPartTemplate` specifics).
    pub fn body_part(&self, id: u32) -> Option<&BodyPartTemplate> {
        self.body_parts.get(&id)
    }

    /// `GetGradeTemplate`.
    pub fn grade(&self, grade: i32) -> Option<&GradeTemplate> {
        self.grades.get(&grade)
    }

    /// `GetGradeTemplate` with every column.
    pub fn full_grade(&self, grade: i32) -> Option<&FullGradeTemplate> {
        self.full_grades.get(&grade)
    }

    /// Adds a grade to both grade tables (tests / synthetic data).
    pub fn insert_grade(&mut self, g: FullGradeTemplate) {
        self.grades.insert(g.grade, g.short());
        self.grades_ordered.insert(g.grade_order, g.grade);
        self.full_grades.insert(g.grade, g);
    }

    /// `GetGradeTemplateByOrder`.
    pub fn grade_by_order(&self, order: i32) -> Option<&FullGradeTemplate> {
        self.grades_ordered
            .get(&order)
            .and_then(|g| self.full_grades.get(g))
    }

    /// `GetEquippedItemSet`.
    pub fn equip_item_set(&self, id: u32) -> Option<&EquipItemSet> {
        self.equip_item_sets.get(&id)
    }

    /// `GetHoldable`.
    pub fn holdable(&self, id: u32) -> Option<&Holdable> {
        self.holdables.get(&id)
    }

    /// `WeaponTemplate.HoldableTemplate` of `template` (`None` for non-weapons).
    pub fn holdable_of(&self, template: &ItemTemplate) -> Option<&Holdable> {
        match &template.kind {
            TemplateKind::Weapon(e) => self.holdables.get(&e.holdable_id),
            _ => None,
        }
    }

    /// `ArmorTemplate/AccessoryTemplate.KindTemplate`.
    pub fn wearable_kind_of(&self, template: &ItemTemplate) -> Option<&WearableKind> {
        match &template.kind {
            TemplateKind::Armor(e) | TemplateKind::Accessory(e) => {
                self.wearable_kinds.get(&e.armor_type_id)
            }
            _ => None,
        }
    }

    /// `ArmorTemplate/AccessoryTemplate.WearableTemplate`.
    pub fn wearable_of(&self, template: &ItemTemplate) -> Option<&Wearable> {
        match &template.kind {
            TemplateKind::Armor(e) | TemplateKind::Accessory(e) => self
                .wearables
                .get(&(e.armor_type_id * 128 + e.slot_type_id)),
            _ => None,
        }
    }

    /// `EquipItemTemplate.ItemLookConvert`.
    pub fn item_look_convert_of(&self, template: &ItemTemplate) -> Option<&ItemLookConvert> {
        template
            .equip()?
            .item_look_convert_id
            .and_then(|id| self.item_look_converts.get(&id))
    }

    /// `GetEquipSlotEnchantingCost`.
    pub fn equip_slot_enchanting_cost(
        &self,
        slot_type_id: u32,
    ) -> Option<&EquipSlotEnchantingCost> {
        self.enchanting_costs.get(&slot_type_id)
    }

    /// `GetItemGradEnchantingSupportByItemId`.
    pub fn enchanting_support(&self, item_id: u32) -> Option<&ItemGradeEnchantingSupport> {
        self.enchanting_supports.get(&item_id)
    }

    /// `GetLootPackIdByNpcId` (empty when none).
    pub fn loot_packs_by_npc(&self, npc_id: u32) -> &[LootPackDroppingNpc] {
        self.loot_pack_dropping_npc
            .get(&npc_id)
            .map_or(&[], Vec::as_slice)
    }

    /// `GetLootPackIdByItemId` (fish -> trophy loot packs).
    pub fn loot_packs_by_fish(&self, item_id: u32) -> &[LootPackConvertFish] {
        self.loot_pack_convert_fish
            .get(&item_id)
            .map_or(&[], Vec::as_slice)
    }

    /// `GetGradeDistributions`.
    pub fn grade_distributions(&self, id: u8) -> Option<&GradeDistributions> {
        self.grade_distributions.get(&i32::from(id))
    }

    /// `GetSocketChance(numSockets)` (chance of the *next* socket).
    pub fn socket_chance(&self, num_sockets: u32) -> u32 {
        self.socket_chance
            .get(&num_sockets.wrapping_add(1))
            .copied()
            .unwrap_or(0)
    }

    /// Max gem sockets for an equip slot type and grade. A missing row means no socket
    /// (`FUN_395aca00` returns 0), so installing is impossible there.
    pub fn item_socket_num_limit(&self, slot_type_id: u32, grade: u8) -> u8 {
        self.item_socket_num_limits
            .get(&(slot_type_id, i32::from(grade)))
            .map_or(0, |l| l.num_socket)
    }

    /// The lunagem's minimum target level requirement (`None` = no limit, as the client's
    /// `FUN_395aca70` returns `INT_MAX` for a missing row).
    pub fn item_socket_level_limit(&self, item_id: u32) -> Option<i32> {
        self.item_socket_level_limits.get(&item_id).map(|l| l.level)
    }

    /// The equip slot group a lunagem fits (`None` = the gem declares no socket group).
    pub fn item_socket_group(&self, item_id: u32) -> Option<u32> {
        self.item_sockets
            .get(&item_id)
            .map(|s| s.equip_slot_group_id)
    }

    /// The `equip_slot_groups` row (`None` = a map / socket row names an unknown group).
    pub fn equip_slot_group(&self, id: u32) -> Option<&EquipSlotGroup> {
        self.equip_slot_groups.get(&id)
    }

    /// Whether the gem's `equip_slot_group_id` covers `slot_type_id` (`equip_slot_groups` +
    /// `equip_slot_group_maps`). A group that is not in the parent table covers nothing.
    pub fn socket_group_allows(&self, group_id: u32, slot_type_id: u32) -> bool {
        self.equip_slot_groups.contains_key(&group_id)
            && self
                .equip_slot_group_maps
                .get(&group_id)
                .is_some_and(|maps| maps.iter().any(|m| m.equip_slot_type_id == slot_type_id))
    }

    /// `GetItemCapScale(skillId)`.
    pub fn item_cap_scale(&self, skill_id: u32) -> Option<&ItemCapScale> {
        self.item_cap_scales.get(&skill_id)
    }

    /// 1.2 client `FUN_395a72d0`: whether `item_cap_scale_forbids` lists this template.
    pub fn cap_scale_forbidden(&self, item_id: u32) -> bool {
        self.item_cap_scale_forbids.contains(&item_id)
    }

    /// `GetAttributeModifiers(id)` (`None` where the C# indexer throws).
    pub fn attribute_modifiers(&self, id: u32) -> Option<&AttributeModifiers> {
        self.modifiers.get(&id)
    }

    /// `GetItemIdsFromDoodad`.
    pub fn item_ids_from_doodad(&self, doodad_id: u32) -> &[u32] {
        self.item_doodads.get(&doodad_id).map_or(&[], Vec::as_slice)
    }

    /// `GetItemProcTemplate`.
    pub fn item_proc(&self, id: u32) -> Option<&ItemProcTemplate> {
        self.item_procs.get(&id)
    }

    /// The `item_proc_bindings` proc ids of an item template (the C# never read the table).
    pub fn item_proc_bindings(&self, item_id: u32) -> &[u32] {
        self.item_proc_bindings
            .get(&item_id)
            .map_or(&[], Vec::as_slice)
    }

    /// `GetUnitModifiers(itemId)`.
    pub fn unit_modifiers(&self, item_id: u32) -> &[BonusTemplate] {
        self.item_unit_modifiers
            .get(&item_id)
            .map_or(&[], Vec::as_slice)
    }

    /// `GetArmorGradeBuff(type, grade)`.
    pub fn armor_grade_buff(&self, armor_type: u32, grade: u32) -> Option<&ArmorGradeBuff> {
        self.armor_grade_buffs.get(&armor_type)?.get(&grade)
    }

    /// `GetItemSet`.
    pub fn item_set(&self, id: u32) -> Option<&ItemSet> {
        self.item_sets.get(&id)
    }

    /// `GetAllItems`.
    pub fn all_templates(&self) -> impl Iterator<Item = &ItemTemplate> {
        self.templates.values()
    }

    /// `IsAutoEquipTradePack(itemTemplateId)`: a backpack template that does not bind on equip.
    pub fn is_auto_equip_trade_pack(&self, template_id: u32) -> bool {
        self.template(template_id).is_some_and(|t| {
            matches!(t.kind, TemplateKind::Backpack { .. })
                && !t.bind_type_has_flag(bind_type::BIND_ON_EQUIP)
        })
    }

    /// `Armor/Accessory/Weapon.MaxDurability` for an item of `template` with `grade`. `None` when
    /// the grade template is missing (a `NullReferenceException` in C#) or for non-equipment.
    #[allow(clippy::excessive_precision)] // the C# float literal 0.0099999998f
    pub fn max_durability(&self, template: &ItemTemplate, grade: i32) -> Option<u8> {
        let g = self.grade(grade)?;
        let durability_const = self.config.durability_const;
        let (first, e) = match &template.kind {
            TemplateKind::Armor(e) | TemplateKind::Accessory(e) => {
                // (int)(template.SlotTemplate.Coverage * 100 + 0.5f)
                (((e.coverage * 100) as f32 + 0.5f32) as i32, e)
            }
            TemplateKind::Weapon(e) => (
                // (int)(GetHoldableDurabilityConst() * 100 + 0.5f)
                (self.config.holdable_durability_const * 100.0 + 0.5f32) as i32,
                e,
            ),
            _ => return None,
        };
        let ratio = (e.durability_ratio * 1000.0 + 0.5f32) as i32;
        // (int)(a * b * grade.Durability * 1000 * 0.00000001f) * DurabilityConst
        let inner =
            (first.wrapping_mul(ratio) as f32 * g.durability * 1000.0 * 0.000_000_01f32) as i32;
        let durability = inner as f32 * durability_const;
        // (float)Math.Round(durability * DurabilityMultiplier * 0.0099999998f)  (ToEven)
        let v = f64::from(durability * e.durability_multiplier as f32 * 0.009_999_999_8f32)
            .round_ties_even() as f32;
        Some(v as u8)
    }
}

/// `SqliteDataReader.GetDateTime` for the text dates used in compact.sqlite3.
pub fn parse_sqlite_datetime(s: &str) -> Option<i64> {
    let s = s.trim();
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt.and_utc().timestamp());
        }
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc().timestamp())
}

#[cfg(test)]
mod tests;
