use super::{
    equipment_item_slot_type, item_grade, parse_sqlite_datetime, ArmorGradeBuff,
    AttributeModifiers, BackpackTemplate, BodyPartTemplate, EquipItemSet, EquipItemSetBonus,
    EquipSlotEnchantingCost, EquipSlotGroup, EquipSlotGroupMap, EquipTemplate, FullGradeTemplate,
    GradeDistributions, Holdable, ItemCapScale, ItemConfig, ItemData, ItemGradeEnchantingSupport,
    ItemLookConvert, ItemProcTemplate, ItemSet, ItemSetItem, ItemSocket, ItemSocketLevelLimit,
    ItemSocketNumLimit, ItemTemplate, LootPackConvertFish, LootPackDroppingNpc, RuneTemplate,
    TemplateKind, Wearable, WearableKind, WearableSlot, ITEM_CREST_INK, ITEM_CREST_STAMP,
    ITEM_SHEET_MUSIC, ITEM_TREASURE_MAP_WITH_COORDINATES,
};
use crate::data::formulas::Formula;
use crate::data::sqlite::for_each_row;
use crate::models::unit::{BonusTemplate, UnitAttribute, UnitModifierType};
use rusqlite::Connection;
use std::collections::HashMap;

fn load_err(msg: String) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(msg)
}

impl ItemData {
    /// `ItemManager.Load`. As in C#, an empty `item_configs` table aborts the whole load
    /// (`if (!reader.Read()) return;`) and leaves every table empty.
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let mut data = ItemData::default();
        let mut has_config = false;
        {
            let mut stmt = conn.prepare("SELECT * FROM item_configs")?;
            let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
            let mut rows = stmt.query([])?;
            if let Some(row) = rows.next()? {
                let r = super::sqlite::SqlRow::new(row, &names);
                data.config = ItemConfig {
                    durability_decrement_chance: r.f32("durability_decrement_chance")?,
                    durability_repair_cost_factor: r.f32("durability_repair_cost_factor")?,
                    durability_const: r.f32("durability_const")?,
                    holdable_durability_const: r.f32("holdable_durability_const")?,
                    wearable_durability_const: r.f32("wearable_durability_const")?,
                    death_durability_loss_ratio: r.u8("death_durability_loss_ratio")?,
                    item_stat_const: r.i32("item_stat_const")?,
                    holdable_stat_const: r.i32("holdable_stat_const")?,
                    wearable_stat_const: r.i32("wearable_stat_const")?,
                    stat_value_const: r.i32("stat_value_const")?,
                };
                has_config = true;
            }
        }
        if !has_config {
            tracing::warn!("item_configs is empty: item templates not loaded (as in C#)");
            return Ok(data);
        }
        tracing::info!("Loading item templates ...");

        for_each_row(
            conn,
            "SELECT * FROM item_look_convert_required_items",
            [],
            |r| {
                let t = ItemLookConvert {
                    id: r.u32("item_look_convert_id")?,
                    slot_id: 0,
                    required_item_id: r.u32("item_id")?,
                    required_item_count: r.i32("item_count")?,
                };
                data.item_look_converts.entry(t.id).or_insert(t);
                Ok(())
            },
        )?;
        for_each_row(conn, "SELECT * FROM item_look_convert_holdables", [], |r| {
            let convert = r.u32("item_look_convert_id")?;
            data.holdable_item_look_converts
                .entry(r.u32("holdable_id")?)
                .or_insert(convert);
            Ok(())
        })?;
        for_each_row(conn, "SELECT * FROM item_look_convert_wearables", [], |r| {
            let convert = r.u32("item_look_convert_id")?;
            data.wearable_item_look_converts
                .entry(r.u32("wearable_slot_id")?)
                .or_insert(convert);
            Ok(())
        })?;

        for_each_row(conn, "SELECT * FROM item_grades", [], |r| {
            let g = FullGradeTemplate {
                grade: r.i32("id")?,
                grade_order: r.i32("grade_order")?,
                holdable_dps: r.f32("var_holdable_dps")?,
                holdable_armor: r.f32("var_holdable_armor")?,
                holdable_magic_dps: r.f32("var_holdable_magic_dps")?,
                wearable_armor: r.f32("var_wearable_armor")?,
                wearable_magic_resistance: r.f32("var_wearable_magic_resistance")?,
                durability: r.f32("durability_value")?,
                upgrade_ratio: r.i32("upgrade_ratio")?,
                stat_multiplier: r.i32("stat_multiplier")?,
                refund_multiplier: r.i32("refund_multiplier")?,
                enchant_success_ratio: r.i32("grade_enchant_success_ratio")?,
                enchant_great_success_ratio: r.i32("grade_enchant_great_success_ratio")?,
                enchant_break_ratio: r.i32("grade_enchant_break_ratio")?,
                enchant_downgrade_ratio: r.i32("grade_enchant_downgrade_ratio")?,
                enchant_cost: r.i32("grade_enchant_cost")?,
                holdable_heal_dps: r.f32("var_holdable_heal_dps")?,
                enchant_downgrade_min: r.i32("grade_enchant_downgrade_min")?,
                enchant_downgrade_max: r.i32("grade_enchant_downgrade_max")?,
                currency_id: r.i32("currency_id")?,
            };
            if data.full_grades.contains_key(&g.grade)
                || data.grades_ordered.contains_key(&g.grade_order)
            {
                return Err(load_err(format!(
                    "duplicate item grade {} / order {} (Dictionary.Add)",
                    g.grade, g.grade_order
                )));
            }
            data.insert_grade(g);
            Ok(())
        })?;
        // MaxGradeId = _grades.Keys.Max() (throws on an empty table).
        data.max_grade_id = data
            .full_grades
            .keys()
            .copied()
            .max()
            .ok_or_else(|| load_err("item_grades is empty (Max of empty sequence)".into()))?;
        data.max_grade_value = data.grades_ordered.keys().copied().max().unwrap_or(0);

        for_each_row(conn, "SELECT * FROM holdables", [], |r| {
            let h = Holdable {
                id: r.u32("id")?,
                kind_id: r.u32("kind_id")?,
                speed: r.i32("speed")?,
                extra_damage_pierce_factor: r.i32("extra_damage_pierce_factor")?,
                extra_damage_slash_factor: r.i32("extra_damage_slash_factor")?,
                extra_damage_blunt_factor: r.i32("extra_damage_blunt_factor")?,
                max_range: r.i32("max_range")?,
                angle: r.i32("angle")?,
                enchanted_dps1000: r.i32("enchanted_dps1000")?,
                slot_type_id: r.u32("slot_type_id")?,
                damage_scale: r.i32("damage_scale")?,
                formula_dps: Formula::new(r.string("formula_dps")?),
                formula_mdps: Formula::new(r.string("formula_mdps")?),
                formula_armor: Formula::new(r.string("formula_armor")?),
                min_range: r.i32("min_range")?,
                sheathe_priority: r.i32("sheathe_priority")?,
                durability_ratio: r.f32("durability_ratio")?,
                renew_category: r.i32("renew_category")?,
                item_proc_id: r.i32("item_proc_id")?,
                stat_multiplier: r.i32("stat_multiplier")?,
                formula_hdps: Formula::new(r.string("formula_hdps")?),
                anim_r1_id: r.u32_or("anim_r1_id", 0)?,
                anim_l1_id: r.u32_or("anim_l1_id", 0)?,
                anim_r2_id: r.u32_or("anim_r2_id", 0)?,
                anim_l2_id: r.u32_or("anim_l2_id", 0)?,
                anim_r3_id: r.u32_or("anim_r3_id", 0)?,
                anim_l3_id: r.u32_or("anim_l3_id", 0)?,
            };
            if data.holdables.insert(h.id, h).is_some() {
                return Err(load_err("duplicate holdable (Dictionary.Add)".into()));
            }
            Ok(())
        })?;

        for_each_row(conn, "SELECT * FROM wearables", [], |r| {
            let w = Wearable {
                type_id: r.u32("armor_type_id")?,
                slot_type_id: r.u32("slot_type_id")?,
                armor_bp: r.i32("armor_bp")?,
                magic_resistance_bp: r.i32("magic_resistance_bp")?,
            };
            let key = w.type_id.wrapping_mul(128).wrapping_add(w.slot_type_id);
            if data.wearables.insert(key, w).is_some() {
                return Err(load_err("duplicate wearable (Dictionary.Add)".into()));
            }
            Ok(())
        })?;

        for_each_row(conn, "SELECT * FROM wearable_kinds", [], |r| {
            let k = WearableKind {
                type_id: r.u32("armor_type_id")?,
                armor_ratio: r.i32("armor_ratio")?,
                magic_resistance_ratio: r.i32("magic_resistance_ratio")?,
                full_buf_id: r.u32("full_buff_id")?,
                half_buf_id: r.u32("half_buff_id")?,
                extra_damage_pierce: r.i32("extra_damage_pierce")?,
                extra_damage_slash: r.i32("extra_damage_slash")?,
                extra_damage_blunt: r.i32("extra_damage_blunt")?,
                durability_ratio: r.f32("durability_ratio")?,
            };
            if data.wearable_kinds.insert(k.type_id, k).is_some() {
                return Err(load_err("duplicate wearable kind (Dictionary.Add)".into()));
            }
            Ok(())
        })?;

        for_each_row(conn, "SELECT * FROM wearable_slots", [], |r| {
            let s = WearableSlot {
                slot_type_id: r.u32("slot_type_id")?,
                coverage: r.i32("coverage")?,
            };
            if data.wearable_slots.insert(s.slot_type_id, s).is_some() {
                return Err(load_err("duplicate wearable slot (Dictionary.Add)".into()));
            }
            Ok(())
        })?;

        for_each_row(conn, "SELECT * FROM dyeable_items", [], |r| {
            if data
                .default_dye_ids
                .insert(r.u32("item_id")?, r.u32("default_dyeing_item_id")?)
                .is_some()
            {
                return Err(load_err("duplicate dyeable item (Dictionary.Add)".into()));
            }
            Ok(())
        })?;

        for_each_row(conn, "SELECT * FROM equip_item_attr_modifiers", [], |r| {
            let m = AttributeModifiers {
                id: r.u32("id")?,
                str_weight: r.i32("str_weight")?,
                dex_weight: r.i32("dex_weight")?,
                sta_weight: r.i32("sta_weight")?,
                int_weight: r.i32("int_weight")?,
                spi_weight: r.i32("spi_weight")?,
            };
            if data.modifiers.insert(m.id, m).is_some() {
                return Err(load_err("duplicate attr modifier (Dictionary.Add)".into()));
            }
            Ok(())
        })?;

        for_each_row(conn, "SELECT * FROM item_procs", [], |r| {
            let p = ItemProcTemplate {
                id: r.u32("id")?,
                skill_id: r.u32("skill_id")?,
                chance_kind: r.u32("chance_kind_id")?,
                chance_rate: r.u32("chance_rate")?,
                chance_param: r.u32("chance_param")?,
                cooldown_sec: r.u32("cooldown_sec")?,
                finisher: r.bool_str("finisher")?,
                item_level_based_chance_bonus: r.u32("item_level_based_chance_bonus")?,
            };
            if data.item_procs.insert(p.id, p).is_some() {
                return Err(load_err("duplicate item proc (Dictionary.Add)".into()));
            }
            Ok(())
        })?;

        // Not loaded by the C#; the item procs bound to a template (`item_proc_bindings`).
        for_each_row(conn, "SELECT * FROM item_proc_bindings", [], |r| {
            let item_id = r.u32("item_id")?;
            let proc_id = r.u32("proc_id")?;
            let list = data.item_proc_bindings.entry(item_id).or_default();
            if !list.contains(&proc_id) {
                list.push(proc_id);
            }
            Ok(())
        })?;

        for_each_row(conn, "SELECT * FROM equip_item_set_bonuses", [], |r| {
            let id = r.u32("equip_item_set_id")?;
            let set = data
                .equip_item_sets
                .entry(id)
                .or_insert_with(|| EquipItemSet {
                    id,
                    bonuses: Vec::new(),
                });
            let bonus = EquipItemSetBonus {
                num_pieces: r.i32("num_pieces")?,
                buff_id: r.u32_or("buff_id", 0)?,
                item_proc_id: r.u32_or("proc_id", 0)?,
            };
            if bonus.buff_id != 0 || bonus.item_proc_id != 0 {
                set.bonuses.push(bonus);
            }
            Ok(())
        })?;

        let missing = |what: &str, k: u32| load_err(format!("{what} {k} missing (KeyNotFound)"));
        let look_convert = |map: &HashMap<u32, u32>,
                            converts: &HashMap<u32, ItemLookConvert>,
                            key: u32|
         -> rusqlite::Result<Option<u32>> {
            match map.get(&key) {
                None => Ok(None),
                Some(c) if converts.contains_key(c) => Ok(Some(*c)),
                Some(c) => Err(missing("item_look_convert", *c)),
            }
        };

        let mut tmp = Vec::new();
        for_each_row(conn, "SELECT * FROM item_armors", [], |r| {
            let id = r.u32("item_id")?;
            let slot = r.u32("slot_type_id")?;
            let ty = r.u32("type_id")?;
            let key = ty.wrapping_mul(128).wrapping_add(slot);
            let w = data
                .wearables
                .get(&key)
                .ok_or_else(|| missing("wearable", key))?;
            let kind = data
                .wearable_kinds
                .get(&ty)
                .ok_or_else(|| missing("wearable_kind", ty))?;
            let coverage = data
                .wearable_slots
                .get(&slot)
                .ok_or_else(|| missing("wearable_slot", slot))?
                .coverage;
            tmp.push(ItemTemplate::new(
                id,
                TemplateKind::Armor(EquipTemplate {
                    slot_type_id: w.slot_type_id,
                    coverage,
                    durability_ratio: kind.durability_ratio,
                    durability_multiplier: r.i32("durability_multiplier")?,
                    charge_lifetime: r.i32_or("charge_lifetime", 0)?,
                    charge_count: r.i32_or("charge_count", 0)?,
                    default_dye_item_id: data.default_dye_ids.get(&id).copied().unwrap_or(0),
                    mod_set_id: r.u32_or("mod_set_id", 0)?,
                    repairable: r.bool_str("repairable")?,
                    base_enchantable: r.bool_str("base_enchantable")?,
                    base_equipment: r.bool_str("base_equipment")?,
                    recharge_buff_id: r.u32_or("recharge_buff_id", 0)?,
                    equip_item_set_id: r.u32_or("eiset_id", 0)?,
                    item_look_convert_id: look_convert(
                        &data.wearable_item_look_converts,
                        &data.item_look_converts,
                        slot,
                    )?,
                    holdable_id: 0,
                    armor_type_id: ty,
                }),
            ));
            Ok(())
        })?;
        for_each_row(conn, "SELECT * FROM item_weapons", [], |r| {
            let hid = r.u32("holdable_id")?;
            let h = data
                .holdables
                .get(&hid)
                .ok_or_else(|| missing("holdable", hid))?;
            tmp.push(ItemTemplate::new(
                r.u32("item_id")?,
                TemplateKind::Weapon(EquipTemplate {
                    slot_type_id: h.slot_type_id,
                    coverage: 0,
                    durability_ratio: h.durability_ratio,
                    durability_multiplier: r.i32("durability_multiplier")?,
                    charge_lifetime: r.i32_or("charge_lifetime", 0)?,
                    charge_count: r.i32_or("charge_count", 0)?,
                    default_dye_item_id: 0,
                    mod_set_id: r.u32_or("mod_set_id", 0)?,
                    repairable: r.bool_str("repairable")?,
                    // reader.GetBoolean("base_enchantable") (numeric): 't' -> false.
                    base_enchantable: r.bool("base_enchantable")?,
                    base_equipment: r.bool_str("base_equipment")?,
                    recharge_buff_id: r.u32_or("recharge_buff_id", 0)?,
                    equip_item_set_id: r.u32_or("eiset_id", 0)?,
                    item_look_convert_id: look_convert(
                        &data.holdable_item_look_converts,
                        &data.item_look_converts,
                        hid,
                    )?,
                    holdable_id: hid,
                    armor_type_id: 0,
                }),
            ));
            Ok(())
        })?;
        for_each_row(conn, "SELECT * FROM item_accessories", [], |r| {
            let slot = r.u32("slot_type_id")?;
            let ty = r.u32("type_id")?;
            let key = ty.wrapping_mul(128).wrapping_add(slot);
            let w = data
                .wearables
                .get(&key)
                .ok_or_else(|| missing("wearable", key))?;
            let kind = data
                .wearable_kinds
                .get(&ty)
                .ok_or_else(|| missing("wearable_kind", ty))?;
            let coverage = data
                .wearable_slots
                .get(&slot)
                .ok_or_else(|| missing("wearable_slot", slot))?
                .coverage;
            tmp.push(ItemTemplate::new(
                r.u32("item_id")?,
                TemplateKind::Accessory(EquipTemplate {
                    slot_type_id: w.slot_type_id,
                    coverage,
                    durability_ratio: kind.durability_ratio,
                    durability_multiplier: r.i32("durability_multiplier")?,
                    charge_lifetime: r.i32_or("charge_lifetime", 0)?,
                    charge_count: r.i32_or("charge_count", 0)?,
                    default_dye_item_id: 0,
                    mod_set_id: r.u32_or("mod_set_id", 0)?,
                    repairable: r.bool_str("repairable")?,
                    base_enchantable: false,
                    base_equipment: false,
                    recharge_buff_id: r.u32_or("recharge_buff_id", 0)?,
                    equip_item_set_id: r.u32_or("eiset_id", 0)?,
                    item_look_convert_id: None,
                    holdable_id: 0,
                    armor_type_id: ty,
                }),
            ));
            Ok(())
        })?;
        for_each_row(conn, "SELECT * FROM item_summon_mates", [], |r| {
            tmp.push(ItemTemplate::new(
                r.u32("item_id")?,
                TemplateKind::SummonMate {
                    npc_id: r.u32("npc_id")?,
                },
            ));
            Ok(())
        })?;
        for_each_row(conn, "SELECT * FROM item_summon_slaves", [], |r| {
            tmp.push(ItemTemplate::new(
                r.u32("item_id")?,
                TemplateKind::SummonSlave {
                    slave_id: r.u32("slave_id")?,
                },
            ));
            Ok(())
        })?;
        let mut body_parts = Vec::new();
        for_each_row(conn, "SELECT * FROM item_body_parts ORDER BY id", [], |r| {
            if r.is_null("item_id")? {
                return Ok(());
            }
            let b = BodyPartTemplate {
                item_id: r.u32("item_id")?,
                model_id: r.u32("model_id")?,
                npc_only: r.bool_str("npc_only")?,
                slot_type_id: r.u32("slot_type_id")?,
                beauty_shop_only: r.bool_str("beautyshop_only")?,
            };
            body_parts.push(b);
            tmp.push(ItemTemplate::new(
                b.item_id,
                TemplateKind::BodyPart {
                    slot_type_id: b.slot_type_id,
                    model_id: b.model_id,
                },
            ));
            Ok(())
        })?;
        let mut runes = Vec::new();
        for_each_row(conn, "SELECT * FROM item_enchanting_gems", [], |r| {
            let id = r.u32("item_id")?;
            runes.push((
                id,
                RuneTemplate {
                    equip_slot_group_id: r.u32_or("equip_slot_group_id", 0)?,
                    equip_level: r.u8_or("equip_level", 0)?,
                    item_grade_id: r.u8_or("item_grade_id", 0)?,
                },
            ));
            tmp.push(ItemTemplate::new(id, TemplateKind::Rune));
            Ok(())
        })?;
        let mut backpacks = Vec::new();
        for_each_row(conn, "SELECT * FROM item_backpacks", [], |r| {
            let id = r.u32("item_id")?;
            let b = BackpackTemplate {
                asset_id: r.u32("asset_id")?,
                backpack_type: r.u32("backpack_type_id")?,
                declare_siege_zone_group_id: r.u32("declare_siege_zone_group_id")?,
                // reader.GetBoolean(column) (numeric): 't' -> false.
                heavy: r.bool("heavy")?,
                asset2_id: r.u32("asset2_id")?,
                normal_speciality: r.bool("normal_specialty")?,
                use_as_stat: r.bool("use_as_stat")?,
                skin_kind_id: r.u32("skin_kind_id")?,
            };
            backpacks.push((id, b));
            tmp.push(ItemTemplate::new(
                id,
                TemplateKind::Backpack {
                    backpack_type: b.backpack_type,
                },
            ));
            Ok(())
        })?;
        // TODO (C#): HACK-FIX FOR CREST INK/STAMP/MUSIC
        tmp.push(ItemTemplate::new(ITEM_CREST_INK, TemplateKind::Ucc));
        tmp.push(ItemTemplate::new(ITEM_CREST_STAMP, TemplateKind::Ucc));
        tmp.push(ItemTemplate::new(
            ITEM_SHEET_MUSIC,
            TemplateKind::MusicSheet,
        ));
        tmp.push(ItemTemplate::new(
            ITEM_TREASURE_MAP_WITH_COORDINATES,
            TemplateKind::TreasureMap,
        ));
        for t in tmp {
            let id = t.id;
            data.template_order.push(id);
            if data.templates.insert(id, t).is_some() {
                return Err(load_err(format!(
                    "duplicate item template {id} (Dictionary.Add)"
                )));
            }
        }
        // The class-specific tables keep the last row of an item id, like the template objects.
        for b in body_parts {
            data.body_parts.insert(b.item_id, b);
        }
        for (id, r) in runes {
            data.runes.insert(id, r);
        }
        for (id, b) in backpacks {
            data.backpacks.insert(id, b);
        }

        // Templates the `items` table names (the others keep a null Name -> "invalid_item_").
        let mut named = std::collections::HashSet::new();
        for_each_row(conn, "SELECT * FROM items", [], |r| {
            let id = r.u32("id")?;
            named.insert(id);
            if !data.templates.contains_key(&id) {
                data.template_order.push(id);
            }
            let t = data
                .templates
                .entry(id)
                .or_insert_with(|| ItemTemplate::new(id, TemplateKind::Item));
            t.name = r.string_or("name", "")?;
            t.category_id = r.i32("category_id")?;
            t.level = r.i32("level")?;
            t.price = r.i32("price")?;
            t.refund = r.i32("refund")?;
            t.bind_type = r.u32("bind_id")?;
            t.pickup_limit = r.i32("pickup_limit")?;
            t.max_count = r.i32("max_stack_size")?;
            t.sellable = r.bool_str("sellable")?;
            t.use_skill_id = r.u32("use_skill_id")?;
            t.use_skill_as_reagent = r.bool_str("use_skill_as_reagent")?;
            t.impl_id = r.i32("impl_id")?;
            t.buff_id = r.u32("buff_id")?;
            t.gradable = r.bool_str("gradable")?;
            t.grade_enchantable = r.bool_str("grade_enchantable")?;
            t.loot_multi = r.bool_str("loot_multi")?;
            t.loot_quest_id = r.u32("loot_quest_id")?;
            t.honor_price = r.i32("honor_price")?;
            t.exp_abs_lifetime = r.i32("exp_abs_lifetime")?;
            t.exp_online_lifetime = r.i32("exp_online_lifetime")?;
            t.exp_date = if r.is_null("exp_date")? {
                None
            } else {
                parse_sqlite_datetime(&r.string("exp_date")?)
            };
            t.specialty_zone_id = if r.is_null("specialty_zone_id")? {
                0
            } else {
                r.u32("specialty_zone_id")?
            };
            t.level_requirement = r.i32("level_requirement")?;
            t.actability_group_id = r.i32_or("actability_group_id", 0)?;
            t.actability_requirement = r.i32_or("actability_requirement", 0)?;
            t.auction_category_a = r.i32_or("auction_a_category_id", 0)?;
            t.auction_category_b = r.i32_or("auction_b_category_id", 0)?;
            t.auction_category_c = r.i32_or("auction_c_category_id", 0)?;
            t.level_limit = r.i32("level_limit")?;
            t.fixed_grade = r.i32("fixed_grade")?;
            t.disenchantable = r.bool_str("disenchantable")?;
            t.living_point_price = r.i32("living_point_price")?;
            t.char_gender = r.u8("char_gender_id")?;
            t.limited_sale_count = r.i32_or("limited_sale_count", 0)?;
            t.one_time_sale = r.bool_str("one_time_sale")?;
            Ok(())
        })?;

        for_each_row(conn, "SELECT * FROM equip_slot_enchanting_costs", [], |r| {
            let c = EquipSlotEnchantingCost {
                id: r.u32("id")?,
                slot_type_id: r.u32("slot_type_id")?,
                cost: r.i32("cost")?,
            };
            data.enchanting_costs.entry(c.slot_type_id).or_insert(c);
            Ok(())
        })?;

        for_each_row(
            conn,
            "SELECT * FROM item_grade_enchanting_supports",
            [],
            |r| {
                let s = ItemGradeEnchantingSupport {
                    id: r.u32("id")?,
                    item_id: r.u32("item_id")?,
                    require_grade_min: r.i32("require_grade_min")?,
                    require_grade_max: r.i32("require_grade_max")?,
                    add_success_ratio: r.i32("add_success_ratio")?,
                    add_success_mul: r.i32("add_success_mul")?,
                    add_great_success_ratio: r.i32("add_great_success_ratio")?,
                    add_great_success_mul: r.i32("add_great_success_mul")?,
                    add_break_ratio: r.i32("add_break_ratio")?,
                    add_break_mul: r.i32("add_break_mul")?,
                    add_downgrade_ratio: r.i32("add_downgrade_ratio")?,
                    add_downgrade_mul: r.i32("add_downgrade_mul")?,
                    add_great_success_grade: r.i32("add_great_success_grade")?,
                };
                data.enchanting_supports.entry(s.item_id).or_insert(s);
                Ok(())
            },
        )?;

        for_each_row(conn, "SELECT * FROM item_socket_chances", [], |r| {
            let n = r.u32("num_sockets")?;
            let chance = r.u32("success_ratio")?;
            data.socket_chance.entry(n).or_insert(chance);
            Ok(())
        })?;

        // `slot_id` is an `EquipmentItemSlotType` id (the client feeds the target template's
        // slot type straight into this table's lookup, `FUN_397cc290` -> `FUN_395aca00`).
        for_each_row(conn, "SELECT * FROM item_socket_num_limits", [], |r| {
            let limit = ItemSocketNumLimit {
                id: r.u32("id")?,
                slot_id: r.u32("slot_id")?,
                grade_id: r.i32("grade_id")?,
                num_socket: r.u8("num_socket")?,
            };
            // Skip rows that cannot describe a real item (bad slot / grade / > 7 gem array).
            if !(1..=equipment_item_slot_type::COSPLAY).contains(&limit.slot_id)
                || !(0..=i32::from(item_grade::MYTHIC)).contains(&limit.grade_id)
                || limit.num_socket > 7
            {
                tracing::warn!(
                    "item_socket_num_limits {}: bad slot {} / grade {} / sockets {}",
                    limit.id,
                    limit.slot_id,
                    limit.grade_id,
                    limit.num_socket
                );
                return Ok(());
            }
            data.item_socket_num_limits
                .entry((limit.slot_id, limit.grade_id))
                .or_insert(limit);
            Ok(())
        })?;

        for_each_row(conn, "SELECT * FROM item_socket_level_limits", [], |r| {
            let limit = ItemSocketLevelLimit {
                id: r.u32("id")?,
                item_id: r.u32("item_id")?,
                level: r.i32("level")?,
            };
            if !data.templates.contains_key(&limit.item_id) {
                tracing::warn!(
                    "item_socket_level_limits {}: item {} is not in the items table",
                    limit.id,
                    limit.item_id
                );
                return Ok(());
            }
            data.item_socket_level_limits
                .entry(limit.item_id)
                .or_insert(limit);
            Ok(())
        })?;

        // The 1.2 table has 15 rows for test/removed items (no `items` row); they cannot be
        // used by any real item, so skip them instead of failing the catalog.
        for_each_row(conn, "SELECT * FROM item_sockets", [], |r| {
            let socket = ItemSocket {
                id: r.u32("id")?,
                item_id: r.u32("item_id")?,
                equip_slot_group_id: r.u32_or("equip_slot_group_id", 0)?,
            };
            // Rows left over from removed/test items have no items row (and often no group);
            // they are expected in the shipped 1.2 data and cannot be used by any real item.
            if socket.equip_slot_group_id == 0 || !data.templates.contains_key(&socket.item_id) {
                tracing::debug!(
                    "item_sockets {}: item {} / equip slot group {} is not loadable",
                    socket.id,
                    socket.item_id,
                    socket.equip_slot_group_id
                );
                return Ok(());
            }
            data.item_sockets.entry(socket.item_id).or_insert(socket);
            Ok(())
        })?;

        for_each_row(conn, "SELECT * FROM equip_slot_groups", [], |r| {
            let group = EquipSlotGroup {
                id: r.u32("id")?,
                name: r.string("name")?,
            };
            data.equip_slot_groups.insert(group.id, group);
            Ok(())
        })?;

        for_each_row(conn, "SELECT * FROM equip_slot_group_maps", [], |r| {
            let map = EquipSlotGroupMap {
                id: r.u32("id")?,
                equip_slot_group_id: r.u32("equip_slot_group_id")?,
                equip_slot_type_id: r.u32("equip_slot_type_id")?,
            };
            if !(1..=equipment_item_slot_type::COSPLAY).contains(&map.equip_slot_type_id) {
                tracing::warn!(
                    "equip_slot_group_maps {}: bad slot type {}",
                    map.id,
                    map.equip_slot_type_id
                );
                return Ok(());
            }
            if !data
                .equip_slot_groups
                .contains_key(&map.equip_slot_group_id)
            {
                tracing::warn!(
                    "equip_slot_group_maps {}: unknown equip slot group {}",
                    map.id,
                    map.equip_slot_group_id
                );
                return Ok(());
            }
            data.equip_slot_group_maps
                .entry(map.equip_slot_group_id)
                .or_default()
                .push(map);
            Ok(())
        })?;

        for_each_row(conn, "SELECT * FROM item_cap_scales", [], |r| {
            let c = ItemCapScale {
                id: r.u32("id")?,
                skill_id: r.u32("skill_id")?,
                scale_min: r.i32("scale_min")?,
                scale_max: r.i32("scale_max")?,
            };
            data.item_cap_scales.entry(c.skill_id).or_insert(c);
            Ok(())
        })?;

        for_each_row(
            conn,
            "SELECT item_id FROM item_cap_scale_forbids",
            [],
            |r| {
                data.item_cap_scale_forbids.insert(r.u32("item_id")?);
                Ok(())
            },
        )?;

        for_each_row(conn, "SELECT * FROM item_grade_distributions", [], |r| {
            let mut weights = [0i32; 12];
            for (i, w) in weights.iter_mut().enumerate() {
                *w = r.i32(&format!("weight_{i}"))?;
            }
            let d = GradeDistributions {
                id: r.i32("id")?,
                name: r.string("name")?,
                weights,
            };
            if data.grade_distributions.insert(d.id, d).is_some() {
                return Err(load_err(
                    "duplicate grade distribution (Dictionary.Add)".into(),
                ));
            }
            Ok(())
        })?;

        for_each_row(conn, "SELECT * FROM loot_pack_dropping_npcs", [], |r| {
            let t = LootPackDroppingNpc {
                id: r.u32("id")?,
                npc_id: r.u32("npc_id")?,
                loot_pack_id: r.u32("loot_pack_id")?,
                // reader.GetBoolean("default_pack") (numeric): 't' -> false.
                default_pack: r.bool("default_pack")?,
            };
            data.loot_pack_dropping_npc
                .entry(t.npc_id)
                .or_default()
                .push(t);
            Ok(())
        })?;

        for_each_row(
            conn,
            "SELECT * FROM doodad_func_convert_fish_items",
            [],
            |r| {
                let t = LootPackConvertFish {
                    id: r.u32("id")?,
                    item_id: r.u32("item_id")?,
                    loot_pack_id: r.u32("loot_pack_id")?,
                    doodad_func_convert_fish_id: r.u32("doodad_func_convert_fish_id")?,
                };
                data.loot_pack_convert_fish
                    .entry(t.item_id)
                    .or_default()
                    .push(t);
                Ok(())
            },
        )?;

        for_each_row(conn, "SELECT * FROM item_spawn_doodads", [], |r| {
            let doodad_id = r.u32("doodad_id")?;
            let item_id = r.u32("item_id")?;
            data.item_doodads
                .entry(doodad_id)
                .or_default()
                .push(item_id);
            Ok(())
        })?;

        for_each_row(
            conn,
            "SELECT * FROM unit_modifiers WHERE owner_type='Item'",
            [],
            |r| {
                let item_id = r.u32("owner_id")?;
                let raw = r.u8("unit_attribute_id")?;
                let Some(attribute) = UnitAttribute::from_u8(raw) else {
                    // Deviation: the C# keeps the raw enum value; our UnitAttribute cannot.
                    tracing::warn!("unit_modifiers: item {item_id} has unknown attribute {raw}");
                    return Ok(());
                };
                data.item_unit_modifiers
                    .entry(item_id)
                    .or_default()
                    .push(BonusTemplate {
                        attribute,
                        modifier_type: UnitModifierType::from_u8(r.u8("unit_modifier_type_id")?),
                        value: r.i32("value")?,
                        linear_level_bonus: r.i32("linear_level_bonus")?,
                    });
                Ok(())
            },
        )?;

        for_each_row(conn, "SELECT * FROM armor_grade_buffs", [], |r| {
            let b = ArmorGradeBuff {
                id: r.u8("id")?,
                armor_type: r.u32("armor_type_id")?,
                item_grade: r.u32("item_grade_id")?,
                buff_id: r.u32("buff_id")?,
            };
            data.armor_grade_buffs
                .entry(b.armor_type)
                .or_default()
                .entry(b.item_grade)
                .or_insert(b);
            Ok(())
        })?;

        for_each_row(conn, "SELECT * FROM item_sets", [], |r| {
            let s = ItemSet {
                id: r.u32("id")?,
                kind_id: r.u32("kind_id")?,
                name: r.string("name")?,
                items: Vec::new(),
            };
            match data.item_sets.entry(s.id) {
                std::collections::hash_map::Entry::Occupied(_) => {
                    tracing::warn!("Duplicate entry for item_sets {}", s.id);
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(s);
                }
            }
            Ok(())
        })?;

        for_each_row(conn, "SELECT * FROM item_set_items", [], |r| {
            let e = ItemSetItem {
                id: r.u32("id")?,
                item_set_id: r.u32("item_set_id")?,
                item_id: r.u32("item_id")?,
                count: r.i32("count")?,
            };
            match data.item_sets.get_mut(&e.item_set_id) {
                Some(set) => {
                    if !set.items.iter().any(|i| i.id == e.id) {
                        set.items.push(e);
                    }
                }
                None => tracing::warn!("Missing item set entry for item_set_items {}", e.id),
            }
            Ok(())
        })?;

        // Naming missing items names (has other templates, but not in items).
        let mut invalid = 0;
        for t in data.templates.values_mut() {
            if !named.contains(&t.id) {
                invalid += 1;
                t.name = format!("invalid_item_{}", t.id);
            }
        }
        tracing::info!(
            "Loaded {} item templates (with {invalid} unused) ...",
            data.templates.len()
        );
        Ok(data)
    }
}
