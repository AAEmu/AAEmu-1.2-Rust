use super::item_impl;
use crate::data::formulas::Formula;
use crate::models::item::ItemClass;

/// `ItemConfig` (`item_configs`, first row).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ItemConfig {
    pub durability_decrement_chance: f32,
    pub durability_repair_cost_factor: f32,
    pub durability_const: f32,
    pub holdable_durability_const: f32,
    pub wearable_durability_const: f32,
    pub death_durability_loss_ratio: u8,
    pub item_stat_const: i32,
    pub holdable_stat_const: i32,
    pub wearable_stat_const: i32,
    pub stat_value_const: i32,
}

/// `GradeTemplate` (the columns the item model needs everywhere; every column is in
/// [`FullGradeTemplate`]).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GradeTemplate {
    pub grade: i32,
    pub grade_order: i32,
    pub durability: f32,
}

/// `GradeTemplate` with every `item_grades` column.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FullGradeTemplate {
    pub grade: i32,
    pub grade_order: i32,
    pub holdable_dps: f32,
    pub holdable_armor: f32,
    pub holdable_magic_dps: f32,
    pub holdable_heal_dps: f32,
    pub wearable_armor: f32,
    pub wearable_magic_resistance: f32,
    pub durability: f32,
    pub upgrade_ratio: i32,
    pub stat_multiplier: i32,
    pub refund_multiplier: i32,
    pub enchant_success_ratio: i32,
    pub enchant_great_success_ratio: i32,
    pub enchant_break_ratio: i32,
    pub enchant_downgrade_ratio: i32,
    pub enchant_downgrade_min: i32,
    pub enchant_downgrade_max: i32,
    pub enchant_cost: i32,
    pub currency_id: i32,
}

impl FullGradeTemplate {
    /// The [`GradeTemplate`] subset.
    pub fn short(&self) -> GradeTemplate {
        GradeTemplate {
            grade: self.grade,
            grade_order: self.grade_order,
            durability: self.durability,
        }
    }
}

/// `Holdable` (`holdables`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Holdable {
    pub id: u32,
    pub kind_id: u32,
    pub speed: i32,
    pub extra_damage_pierce_factor: i32,
    pub extra_damage_slash_factor: i32,
    pub extra_damage_blunt_factor: i32,
    pub max_range: i32,
    pub angle: i32,
    pub enchanted_dps1000: i32,
    pub slot_type_id: u32,
    pub damage_scale: i32,
    pub formula_dps: Formula,
    pub formula_mdps: Formula,
    pub formula_armor: Formula,
    pub formula_hdps: Formula,
    pub min_range: i32,
    pub sheathe_priority: i32,
    pub durability_ratio: f32,
    pub renew_category: i32,
    pub item_proc_id: i32,
    pub stat_multiplier: i32,
    pub anim_r1_id: u32,
    pub anim_l1_id: u32,
    pub anim_r2_id: u32,
    pub anim_l2_id: u32,
    pub anim_r3_id: u32,
    pub anim_l3_id: u32,
}

impl Holdable {
    /// `Holdable.DefaultFistAnimId`.
    pub const DEFAULT_FIST_ANIM_ID: u32 = 2;

    /// `Holdable.GetAttackAnimId(attackIndex, leftHand)`.
    pub fn attack_anim_id(&self, attack_index: i32, left_hand: bool) -> u32 {
        let (a1, a2, a3) = if left_hand {
            (self.anim_l1_id, self.anim_l2_id, self.anim_l3_id)
        } else {
            (self.anim_r1_id, self.anim_r2_id, self.anim_r3_id)
        };
        let valid: Vec<u32> = [a1, a2, a3].into_iter().filter(|a| *a > 0).collect();
        if valid.is_empty() {
            return if left_hand {
                self.anim_r1_id
            } else {
                Self::DEFAULT_FIST_ANIM_ID
            };
        }
        // Math.Abs(int.MinValue) throws in C#; unsigned_abs keeps the same index otherwise.
        valid[(attack_index.unsigned_abs() as usize) % valid.len()]
    }
}

/// `Wearable`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Wearable {
    pub type_id: u32,
    pub slot_type_id: u32,
    pub armor_bp: i32,
    pub magic_resistance_bp: i32,
}

/// `WearableKind` (`wearable_kinds`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WearableKind {
    pub type_id: u32,
    pub armor_ratio: i32,
    pub magic_resistance_ratio: i32,
    pub full_buf_id: u32,
    pub half_buf_id: u32,
    pub extra_damage_pierce: i32,
    pub extra_damage_slash: i32,
    pub extra_damage_blunt: i32,
    pub durability_ratio: f32,
}

/// `WearableSlot` (`wearable_slots`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WearableSlot {
    pub slot_type_id: u32,
    pub coverage: i32,
}

/// `AttributeModifiers` (`equip_item_attr_modifiers`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttributeModifiers {
    pub id: u32,
    pub str_weight: i32,
    pub dex_weight: i32,
    pub sta_weight: i32,
    pub int_weight: i32,
    pub spi_weight: i32,
}

impl AttributeModifiers {
    /// `AllWeight`.
    pub fn all_weight(&self) -> i32 {
        self.str_weight + self.dex_weight + self.sta_weight + self.int_weight + self.spi_weight
    }

    /// `Count` (number of positive weights).
    pub fn count(&self) -> i32 {
        [
            self.str_weight,
            self.dex_weight,
            self.sta_weight,
            self.int_weight,
            self.spi_weight,
        ]
        .iter()
        .filter(|w| **w > 0)
        .count() as i32
    }
}

/// `ItemProcTemplate` (`item_procs`). `SkillTemplate` is resolved by the skills subsystem from
/// `skill_id` (`ItemManager.OnSkillsLoaded`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemProcTemplate {
    pub id: u32,
    pub skill_id: u32,
    /// `ProcChanceKind` (see [`proc_chance_kind`](super::proc_chance_kind)).
    pub chance_kind: u32,
    pub chance_rate: u32,
    pub chance_param: u32,
    pub cooldown_sec: u32,
    pub finisher: bool,
    pub item_level_based_chance_bonus: u32,
}

/// `EquipItemSetBonus`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EquipItemSetBonus {
    pub num_pieces: i32,
    pub buff_id: u32,
    pub item_proc_id: u32,
}

/// `EquipItemSet` (`equip_item_set_bonuses` grouped by set).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EquipItemSet {
    pub id: u32,
    pub bonuses: Vec<EquipItemSetBonus>,
}

/// `ItemLookConvert` (`item_look_convert_required_items`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemLookConvert {
    pub id: u32,
    pub slot_id: u32,
    pub required_item_id: u32,
    pub required_item_count: i32,
}

/// `EquipSlotEnchantingCost`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EquipSlotEnchantingCost {
    pub id: u32,
    pub slot_type_id: u32,
    pub cost: i32,
}

/// `ItemGradeEnchantingSupport`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemGradeEnchantingSupport {
    pub id: u32,
    pub item_id: u32,
    pub require_grade_min: i32,
    pub require_grade_max: i32,
    pub add_success_ratio: i32,
    pub add_success_mul: i32,
    pub add_great_success_ratio: i32,
    pub add_great_success_mul: i32,
    pub add_break_ratio: i32,
    pub add_break_mul: i32,
    pub add_downgrade_ratio: i32,
    pub add_downgrade_mul: i32,
    pub add_great_success_grade: i32,
}

/// `ItemCapScale`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemCapScale {
    pub id: u32,
    pub skill_id: u32,
    pub scale_min: i32,
    pub scale_max: i32,
}

/// `item_socket_num_limits`: max sockets of an item by equip slot type and grade.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemSocketNumLimit {
    pub id: u32,
    pub slot_id: u32,
    pub grade_id: i32,
    pub num_socket: u8,
}

/// `item_socket_level_limits`: the target item's level requirement for a lunagem.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemSocketLevelLimit {
    pub id: u32,
    pub item_id: u32,
    pub level: i32,
}

/// `item_sockets`: the equip slot group a lunagem fits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemSocket {
    pub id: u32,
    pub item_id: u32,
    pub equip_slot_group_id: u32,
}

/// `equip_slot_group_maps`: the equip slot types a slot group covers (one row per pair).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EquipSlotGroupMap {
    pub id: u32,
    pub equip_slot_group_id: u32,
    pub equip_slot_type_id: u32,
}

/// `equip_slot_groups`: the named groups `equip_slot_group_maps` joins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EquipSlotGroup {
    pub id: u32,
    pub name: String,
}

/// `GradeDistributions` (`item_grade_distributions`; `weights[i]` = `Weight{i}`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GradeDistributions {
    pub id: i32,
    pub name: String,
    pub weights: [i32; 12],
}

/// `LootPackDroppingNpc`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LootPackDroppingNpc {
    pub id: u32,
    pub npc_id: u32,
    pub loot_pack_id: u32,
    pub default_pack: bool,
}

/// `LootPackConvertFish` (`doodad_func_convert_fish_items`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LootPackConvertFish {
    pub id: u32,
    pub item_id: u32,
    pub loot_pack_id: u32,
    pub doodad_func_convert_fish_id: u32,
}

/// `ArmorGradeBuff`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArmorGradeBuff {
    pub id: u8,
    pub armor_type: u32,
    pub item_grade: u32,
    pub buff_id: u32,
}

/// `ItemSetItem`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemSetItem {
    pub id: u32,
    pub item_set_id: u32,
    pub item_id: u32,
    pub count: i32,
}

/// `ItemSet` (`Items` in insertion order).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemSet {
    pub id: u32,
    pub kind_id: u32,
    pub name: String,
    pub items: Vec<ItemSetItem>,
}

/// `RuneTemplate` specifics (`item_enchanting_gems`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuneTemplate {
    pub equip_slot_group_id: u32,
    pub equip_level: u8,
    pub item_grade_id: u8,
}

/// `BackpackTemplate` specifics (`item_backpacks`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackpackTemplate {
    pub asset_id: u32,
    pub backpack_type: u32,
    pub declare_siege_zone_group_id: u32,
    pub heavy: bool,
    pub asset2_id: u32,
    pub normal_speciality: bool,
    pub use_as_stat: bool,
    pub skin_kind_id: u32,
}

/// `BodyPartTemplate` specifics (`item_body_parts`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BodyPartTemplate {
    pub item_id: u32,
    pub model_id: u32,
    pub npc_only: bool,
    pub slot_type_id: u32,
    pub beauty_shop_only: bool,
}

/// The `ItemTemplate` subclass and its class-specific fields.
#[derive(Debug, Clone, PartialEq)]
pub enum TemplateKind {
    Item,
    Armor(EquipTemplate),
    Weapon(EquipTemplate),
    Accessory(EquipTemplate),
    SummonMate { npc_id: u32 },
    SummonSlave { slave_id: u32 },
    BodyPart { slot_type_id: u32, model_id: u32 },
    Rune,
    Backpack { backpack_type: u32 },
    Ucc,
    MusicSheet,
    TreasureMap,
}

/// `EquipItemTemplate` + the armor/weapon/accessory specifics.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EquipTemplate {
    /// Wearable / holdable `slot_type_id` (`EquipmentItemSlotType`).
    pub slot_type_id: u32,
    /// `SlotTemplate.Coverage` (armor / accessory).
    pub coverage: i32,
    /// `KindTemplate.DurabilityRatio` (armor / accessory) or `HoldableTemplate.DurabilityRatio`.
    pub durability_ratio: f32,
    pub durability_multiplier: i32,
    pub charge_lifetime: i32,
    pub charge_count: i32,
    pub default_dye_item_id: u32,
    /// `ModSetId` (`equip_item_attr_modifiers` id, 0 = none).
    pub mod_set_id: u32,
    pub repairable: bool,
    /// `BaseEnchantable` (armor / weapon only).
    pub base_enchantable: bool,
    /// `BaseEquipment` (armor / weapon only).
    pub base_equipment: bool,
    pub recharge_buff_id: u32,
    /// `EquipItemSetId` (`eiset_id`).
    pub equip_item_set_id: u32,
    /// `ItemLookConvert` (its id; armor: by wearable slot, weapon: by holdable).
    pub item_look_convert_id: Option<u32>,
    /// `WeaponTemplate.HoldableTemplate` (holdable id; weapons only).
    pub holdable_id: u32,
    /// `WearableTemplate` / `KindTemplate` type (`ArmorType`; armor / accessory only).
    pub armor_type_id: u32,
}

/// `ItemTemplate` (subset).
#[derive(Debug, Clone, PartialEq)]
pub struct ItemTemplate {
    pub id: u32,
    pub kind: TemplateKind,
    pub name: String,
    pub bind_type: u32,
    pub max_count: i32,
    pub gradable: bool,
    /// `items.grade_enchantable`: the item may be the target of a regrade (`GradeEnchant`).
    pub grade_enchantable: bool,
    pub fixed_grade: i32,
    pub exp_abs_lifetime: i32,
    pub exp_online_lifetime: i32,
    /// `ExpDate` as unix seconds (`None` = `DateTime.MinValue`).
    pub exp_date: Option<i64>,
    pub category_id: i32,
    pub level: i32,
    pub price: i32,
    pub refund: i32,
    pub pickup_limit: i32,
    pub sellable: bool,
    pub use_skill_id: u32,
    pub use_skill_as_reagent: bool,
    /// `ImplId` (`ItemImplEnum`).
    pub impl_id: i32,
    pub buff_id: u32,
    pub loot_multi: bool,
    pub loot_quest_id: u32,
    pub honor_price: i32,
    pub level_requirement: i32,
    /// `items.actability_group_id`: the actability group of `actability_requirement`.
    pub actability_group_id: i32,
    /// `items.actability_requirement`: actability points needed to use / equip the item
    /// (1.2 client `FUN_394e1a90`, `actability_not_enough_point` 508; 0 = no requirement).
    pub actability_requirement: i32,
    pub auction_category_a: i32,
    pub auction_category_b: i32,
    pub auction_category_c: i32,
    pub level_limit: i32,
    pub disenchantable: bool,
    pub living_point_price: i32,
    pub char_gender: u8,
    pub specialty_zone_id: u32,
    /// `items.limited_sale_count`: how many of this template one account may buy
    /// (0 = unlimited).
    pub limited_sale_count: i32,
    /// `items.one_time_sale`: one account may buy this template once.
    pub one_time_sale: bool,
}

impl ItemTemplate {
    pub fn new(id: u32, kind: TemplateKind) -> Self {
        Self {
            id,
            kind,
            name: String::new(),
            bind_type: 0,
            max_count: 0,
            gradable: false,
            grade_enchantable: false,
            fixed_grade: 0,
            exp_abs_lifetime: 0,
            exp_online_lifetime: 0,
            exp_date: None,
            category_id: 0,
            level: 0,
            price: 0,
            refund: 0,
            pickup_limit: 0,
            sellable: false,
            use_skill_id: 0,
            use_skill_as_reagent: false,
            impl_id: 0,
            buff_id: 0,
            loot_multi: false,
            loot_quest_id: 0,
            honor_price: 0,
            level_requirement: 0,
            actability_group_id: 0,
            actability_requirement: 0,
            auction_category_a: 0,
            auction_category_b: 0,
            auction_category_c: 0,
            level_limit: 0,
            disenchantable: false,
            living_point_price: 0,
            char_gender: 0,
            specialty_zone_id: 0,
            limited_sale_count: 0,
            one_time_sale: false,
        }
    }

    /// The per-account purchase limit of `items.one_time_sale` / `items.limited_sale_count`
    /// (`None` = unlimited). A one-time sale is a limit of one; when both columns are set the
    /// smaller limit wins.
    pub fn purchase_limit(&self) -> Option<i32> {
        let limited = (self.limited_sale_count > 0).then_some(self.limited_sale_count);
        let one_time = self.one_time_sale.then_some(1);
        match (limited, one_time) {
            (Some(limited), Some(one_time)) => Some(limited.min(one_time)),
            (Some(limited), None) => Some(limited),
            (None, Some(one_time)) => Some(one_time),
            (None, None) => None,
        }
    }

    /// A plain `ItemTemplate` (tests / synthetic data).
    pub fn plain(id: u32, max_count: i32) -> Self {
        Self {
            max_count,
            fixed_grade: -1,
            ..Self::new(id, TemplateKind::Item)
        }
    }

    /// `ItemTemplate.ClassType`.
    pub fn class_type(&self) -> ItemClass {
        if self.impl_id == item_impl::SLAVE_EQUIPMENT {
            return ItemClass::SlaveEquipment;
        }
        match self.kind {
            TemplateKind::Item => ItemClass::Item,
            TemplateKind::Armor(_) => ItemClass::Armor,
            TemplateKind::Weapon(_) => ItemClass::Weapon,
            TemplateKind::Accessory(_) => ItemClass::Accessory,
            TemplateKind::SummonMate { .. } => ItemClass::SummonMate,
            TemplateKind::SummonSlave { .. } => ItemClass::SummonSlave,
            TemplateKind::BodyPart { .. } => ItemClass::BodyPart,
            TemplateKind::Rune => ItemClass::Rune,
            TemplateKind::Backpack { .. } => ItemClass::Backpack,
            TemplateKind::Ucc => ItemClass::UccItem,
            TemplateKind::MusicSheet => ItemClass::MusicSheetItem,
            TemplateKind::TreasureMap => ItemClass::TreasureMap,
        }
    }

    pub fn equip(&self) -> Option<&EquipTemplate> {
        match &self.kind {
            TemplateKind::Armor(e) | TemplateKind::Weapon(e) | TemplateKind::Accessory(e) => {
                Some(e)
            }
            _ => None,
        }
    }

    /// `template is EquipItemTemplate`.
    pub fn is_equip(&self) -> bool {
        self.equip().is_some()
    }

    /// The `EquipmentItemSlotType` used by `EquipmentContainer.GetAllowedGearSlots` / `CanAccept`
    /// (`None` = not equippable).
    pub fn gear_slot_type(&self) -> Option<u32> {
        match &self.kind {
            TemplateKind::BodyPart { slot_type_id, .. } => Some(*slot_type_id),
            TemplateKind::Armor(e) | TemplateKind::Weapon(e) | TemplateKind::Accessory(e) => {
                Some(e.slot_type_id)
            }
            TemplateKind::Backpack { .. } => Some(EQUIPMENT_ITEM_SLOT_TYPE_BACKPACK),
            _ => None,
        }
    }

    /// `template.BindType.HasFlag(flag)` (the C# enum is not a flags enum, so this is a bit
    /// test of the raw value).
    pub fn bind_type_has_flag(&self, flag: u32) -> bool {
        self.bind_type & flag == flag
    }
}

/// `EquipmentItemSlotType.Backpack`.
pub const EQUIPMENT_ITEM_SLOT_TYPE_BACKPACK: u32 = 30;
