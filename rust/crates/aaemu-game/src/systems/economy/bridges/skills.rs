use std::collections::HashSet;

use crate::data::items::TemplateKind;
use crate::models::container::{equip_slot, slot_type};
use crate::models::item::{
    EquipStat, Item, ItemClass, BIND_ON_UNPACK, ITEM_FLAG_SKINIZED, ITEM_FLAG_UNPACKED,
};
use crate::models::item_task::{task_type, ItemFx, ItemFxList, ItemTask};
use crate::server::GameServer;
use crate::systems::skills::data::{EffectKind, EffectTemplate};
use crate::systems::skills::effects::EffectArgs;
use crate::systems::skills::ext::{
    GearStats, GradeEnchantRequest, HoldableInfo, ItemActionResult, ItemSkillHooks, TakenCastItem,
};
use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
use crate::world::{PlayerRef, WorldObject};

use super::super::inventory::{send_error_message_to, Inv, ItemTxn, Out};
use super::super::item_effects::{Outcome, SkillItem};
use super::super::loot::SharedRandom;
use super::super::{error_message, inventory, item_effects};
use super::{lock, put_restored_item, task_packet, EconomySheetMusic, CATEGORY_DYE};

/// `SkillItem` of the caster object.
fn skill_item(c: &SkillCaster) -> Option<SkillItem> {
    match *c {
        SkillCaster::Item {
            item_id,
            item_template_id,
            ..
        } => Some(SkillItem {
            item_id,
            item_template_id,
        }),
        _ => None,
    }
}

/// `SkillCastItemTarget` `(Id, ObjId)`.
fn item_target(t: &SkillCastTarget) -> Option<(u64, u32)> {
    match *t {
        SkillCastTarget::Item { id, obj_id, .. } => Some((id, obj_id)),
        _ => None,
    }
}

fn player_caster<'a>(args: &'a EffectArgs<'_>) -> Option<&'a PlayerRef> {
    args.caster.as_ref().and_then(WorldObject::as_player)
}

fn player_target<'a>(args: &'a EffectArgs<'_>) -> Option<&'a PlayerRef> {
    args.target.as_obj().and_then(WorldObject::as_player)
}

/// `skill.Cancelled = true`.
fn cancel_skill(args: &EffectArgs<'_>) {
    if let Some(s) = args.source.skill() {
        s.state().cancelled = true;
        s.state().effect_cancelled = true;
    }
}

/// The equipment items of a unit (`Equipment.Items`).
fn equipment_items(unit: &WorldObject) -> Vec<Item> {
    match unit {
        WorldObject::Player(p) => lock(&Inv::of(p).equipment()).items.clone(),
        WorldObject::Npc(n) => n.data().equipment.iter().flatten().cloned().collect(),
        WorldObject::Mate(m) => m.data().equipment.iter().flatten().cloned().collect(),
        WorldObject::Slave(s) => s.data().equipment.iter().flatten().cloned().collect(),
        _ => Vec::new(),
    }
}

/// `Equipment.GetItemBySlot(slot)` of a unit.
fn equipped_by_slot(unit: &WorldObject, slot: i32) -> Option<Item> {
    match unit {
        WorldObject::Player(p) => Inv::of(p).equipped(slot),
        _ => equipment_items(unit).into_iter().find(|i| i.slot == slot),
    }
}

fn holdable_info(server: &GameServer, item: &Item) -> Option<HoldableInfo> {
    let data = &server.game_data.items;
    let t = data.template(item.template_id)?;
    let h = data.holdable_of(t)?;
    Some(HoldableInfo {
        holdable_id: h.id,
        slot_type_id: h.slot_type_id,
        min_range: h.min_range as f32,
        max_range: h.max_range as f32,
        damage_scale: h.damage_scale as f32,
        speed: h.speed as f32,
    })
}

// ---------------------------------------------------------------------------------------------
// skills: ItemSkillHooks
// ---------------------------------------------------------------------------------------------

/// The skills subsystem's view of items (`ItemSkillHooks`).
pub struct EconomyForSkills;

/// `BuffConstants` used by `UpdateGearBonuses`.
mod buff_constants {
    pub const ARMOR_BUFF_TAG: u32 = 145;
    pub const EQUIPMENT_BUFF_TAG: u32 = 156;
    pub const CLOTH_4P: u32 = 713;
    pub const CLOTH_7P: u32 = 714;
    pub const LEATHER_4P: u32 = 715;
    pub const LEATHER_7P: u32 = 716;
    pub const PLATE_4P: u32 = 717;
    pub const PLATE_7P: u32 = 740;
    pub const EQUIP_DUALWIELD: u32 = 4899;
    pub const EQUIP_SHIELD: u32 = 8226;
    pub const EQUIP_TWO_HANDED: u32 = 8227;
}

/// `EquipmentItemSlotType` values the gear buffs check.
mod gear_slot_type {
    pub const BACK: u32 = 9;
    pub const MAINHAND: u32 = 14;
    pub const OFFHAND: u32 = 15;
    pub const TWO_HANDED: u32 = 16;
    pub const ONE_HANDED: u32 = 17;
    pub const SHIELD: u32 = 20;
}

/// `ArmorType`.
mod armor_type {
    pub const CLOTH: u32 = 1;
    pub const LEATHER: u32 = 2;
    pub const METAL: u32 = 3;
}

/// `ItemGrade.Arcane`.
const ITEM_GRADE_ARCANE: u8 = 4;

impl EconomyForSkills {
    /// `new Buff(this, this, SkillCasterUnit(..), template, null, now) { AbLevel }` +
    /// `Buffs.AddBuff`.
    /// `Buffs.AddBuff(new Buff(this, this, skillCaster, template, ...) { AbLevel })` of the gear
    /// code; `caster_obj` is the `SkillCasterUnit` object id (`new SkillCasterUnit()` = 0 for
    /// equip effects and armor buffs, `SkillCasterUnit(ObjId)` for wield and item-set buffs).
    fn add_gear_buff(
        server: &GameServer,
        unit: &WorldObject,
        buff_id: u32,
        ab_level: u32,
        caster_obj: u32,
    ) {
        use crate::systems::skills::buffs;
        let Some(t) = server.game_data.systems.skills.buff(buff_id).cloned() else {
            return;
        };
        let mut b = buffs::new_buff(
            server,
            &t,
            Some(unit),
            &SkillCaster::Unit(caster_obj),
            None,
            crate::world::now_ms(),
        );
        if ab_level > 0 {
            b.ab_level = ab_level;
        }
        buffs::add_buff(server, unit, b, 0, 0);
    }

    fn check_buff(unit: &WorldObject, buff_id: u32) -> bool {
        unit.with_base_unit(|b| b.buffs().check_buff(buff_id))
    }

    /// `ItemGameData.GetItemBuff(templateId, grade) ?? GetBuffTemplate(Template.BuffId)`.
    fn item_buff(server: &GameServer, item: &Item) -> Option<u32> {
        let econ = &server.game_data.systems.economy.item_effects;
        econ.item_grade_buff(item.template_id, item.grade)
            .or_else(|| {
                server
                    .game_data
                    .items
                    .template(item.template_id)
                    .map(|t| t.buff_id)
            })
            .filter(|id| *id != 0 && server.game_data.systems.skills.buff(*id).is_some())
    }

    /// The charged buff part of `ApplyEquipEffects` (`RechargeBuffId`).
    fn charge_buff_active(server: &GameServer, item: &Item) -> Option<u32> {
        let t = server.game_data.items.template(item.template_id)?;
        let e = t.equip()?;
        if e.recharge_buff_id == 0 {
            return None;
        }
        let bind_on_unpack = t.bind_type & BIND_ON_UNPACK != 0;
        let mut check_expire = if bind_on_unpack {
            item.unpack_time
        } else {
            item.charge_start_time
        };
        check_expire += i64::from(e.charge_lifetime) * 60;
        let now = chrono::Utc::now().timestamp();
        let mut add = false;
        if e.charge_lifetime > 0 && check_expire > now {
            add = true;
        }
        if e.charge_count > 0 && item.charge_count > 0 {
            add = true;
        }
        if bind_on_unpack && !item.has_flag(ITEM_FLAG_UNPACKED) {
            add = false;
        }
        add.then_some(e.recharge_buff_id)
    }

    fn is_destroyed(server: &GameServer, item: &Item) -> bool {
        item.is_equip() && !item.is_not_destroyed(&server.game_data.items)
    }

    /// `Unit.ApplyEquipEffects(itemAdded, itemRemoved)`.
    fn apply_equip_effects(
        server: &GameServer,
        unit: &WorldObject,
        items: &[Item],
        added: Option<&Item>,
        removed: Option<&Item>,
    ) {
        use crate::systems::skills::buffs;
        let data = &server.game_data.items;
        if let Some(r) = removed {
            if let Some(id) = Self::item_buff(server, r) {
                if Self::check_buff(unit, id) {
                    buffs::remove_buff(server, unit, id);
                }
            }
            if let Some(e) = data.template(r.template_id).and_then(|t| t.equip()) {
                if e.recharge_buff_id > 0 && Self::check_buff(unit, e.recharge_buff_id) {
                    buffs::remove_buff(server, unit, e.recharge_buff_id);
                }
            }
        }
        if let Some(a) = added {
            if !Self::is_destroyed(server, a) {
                let level = data.template(a.template_id).map_or(0, |t| t.level) as u32;
                if let Some(id) = Self::item_buff(server, a) {
                    Self::add_gear_buff(server, unit, id, level, 0);
                }
                if let Some(id) = Self::charge_buff_active(server, a) {
                    Self::add_gear_buff(server, unit, id, level, 0);
                }
            }
        }
        if added.is_none() && removed.is_none() {
            // This is the first load check to apply buffs for equipped items.
            buffs::remove_buffs_by_tag(server, unit, buff_constants::EQUIPMENT_BUFF_TAG, 20);
            for item in items {
                let Some(t) = data.template(item.template_id) else {
                    continue;
                };
                let level = t.level as u32;
                if t.buff_id != 0 {
                    if let Some(id) = Self::item_buff(server, item) {
                        Self::add_gear_buff(server, unit, id, level, 0);
                    }
                }
                if let Some(id) = Self::charge_buff_active(server, item) {
                    Self::add_gear_buff(server, unit, id, level, 0);
                }
            }
        }
    }

    /// `Unit.GetWeaponWieldKind()` + `ApplyWeaponWieldBuff()`.
    fn apply_weapon_wield_buff(server: &GameServer, unit: &WorldObject) {
        use crate::systems::skills::buffs;
        buffs::remove_buff(server, unit, buff_constants::EQUIP_DUALWIELD);
        buffs::remove_buff(server, unit, buff_constants::EQUIP_SHIELD);
        buffs::remove_buff(server, unit, buff_constants::EQUIP_TWO_HANDED);
        let slot_type_of = |slot: i32| {
            equipped_by_slot(unit, slot)
                .and_then(|i| holdable_info(server, &i))
                .map(|h| h.slot_type_id)
        };
        let main = slot_type_of(equip_slot::MAINHAND);
        let off = slot_type_of(equip_slot::OFFHAND);
        let buff = match main {
            Some(gear_slot_type::TWO_HANDED) => Some(buff_constants::EQUIP_TWO_HANDED),
            Some(gear_slot_type::ONE_HANDED | gear_slot_type::MAINHAND)
                if matches!(
                    off,
                    Some(gear_slot_type::ONE_HANDED | gear_slot_type::OFFHAND)
                ) =>
            {
                Some(buff_constants::EQUIP_DUALWIELD)
            }
            // None / OneHanded: a shield in the offhand
            _ => (off == Some(gear_slot_type::SHIELD)).then_some(buff_constants::EQUIP_SHIELD),
        };
        if let Some(id) = buff {
            Self::add_gear_buff(server, unit, id, 0, unit.obj_id());
        }
    }

    /// `Unit.ApplyArmorGradeBuff(itemAdded, itemRemoved)`.
    fn apply_armor_grade_buff(
        server: &GameServer,
        unit: &WorldObject,
        items: &[Item],
        added: Option<&Item>,
        removed: Option<&Item>,
    ) {
        use crate::systems::skills::buffs;
        let data = &server.game_data.items;
        let is_armor = |i: Option<&Item>| i.is_some_and(|i| i.class == ItemClass::Armor);
        if (added.is_some() || removed.is_some()) && !is_armor(added) && !is_armor(removed) {
            return;
        }
        if added.is_some_and(|a| Self::is_destroyed(server, a)) {
            // Destroyed item, ignore
            return;
        }
        // Clear any existing armor grade buffs
        buffs::remove_buffs_by_tag(server, unit, buff_constants::ARMOR_BUFF_TAG, 10);
        // Get armor pieces by kind (insertion order of the kinds, as the C# dictionary)
        let mut kinds: Vec<(u32, Vec<&Item>)> = Vec::new();
        for item in items {
            if item.class != ItemClass::Armor {
                continue;
            }
            let Some(t) = data.template(item.template_id) else {
                continue;
            };
            if !matches!(t.kind, TemplateKind::Armor(_)) {
                continue;
            }
            // armorTemplate.SlotTemplate.SlotTypeId == Back
            if t.equip()
                .is_some_and(|e| e.slot_type_id == gear_slot_type::BACK)
            {
                continue;
            }
            let Some(kind) = data.wearable_kind_of(t).map(|k| k.type_id) else {
                continue;
            };
            match kinds.iter_mut().find(|(k, _)| *k == kind) {
                Some((_, v)) => v.push(item),
                None => kinds.push((kind, vec![item])),
            }
        }
        // Get kind with most pieces
        let Some(mut best) = kinds.first() else {
            return;
        };
        for k in &kinds {
            if k.1.len() > best.1.len() {
                best = k;
            }
        }
        let pieces = &best.1;
        if pieces.len() < 4 {
            return;
        }
        // finalArmorTemplate.WearableTemplate.TypeId
        let Some(armor_type) = data
            .template(pieces[0].template_id)
            .and_then(|t| data.wearable_of(t))
            .map(|k| k.type_id)
        else {
            return;
        };
        let set_buff = match (pieces.len() == 7, armor_type) {
            (true, armor_type::CLOTH) => Some(buff_constants::CLOTH_7P),
            (true, armor_type::LEATHER) => Some(buff_constants::LEATHER_7P),
            (true, armor_type::METAL) => Some(buff_constants::PLATE_7P),
            (false, armor_type::CLOTH) => Some(buff_constants::CLOTH_4P),
            (false, armor_type::LEATHER) => Some(buff_constants::LEATHER_4P),
            (false, armor_type::METAL) => Some(buff_constants::PLATE_4P),
            _ => None,
        };
        if let Some(id) = set_buff {
            Self::add_gear_buff(server, unit, id, 0, 0);
        }
        // Get only pieces >= arcane
        let above: Vec<&&Item> = pieces
            .iter()
            .filter(|p| p.grade >= ITEM_GRADE_ARCANE)
            .collect();
        if above.len() < 4 {
            return;
        }
        let total_level: i32 = above
            .iter()
            .map(|p| data.template(p.template_id).map_or(0, |t| t.level))
            .sum();
        // This const was calculated by hand, it might make no sense.
        let ab_level = total_level as f32 * 0.406_705_54_f32;
        let grade_buff_ab_level = ab_level * ab_level / 15.0 + 30.0;
        let lowest_grade = above.iter().map(|p| p.grade).min().unwrap_or(0);
        let Some(grade_buff) = data.armor_grade_buff(armor_type, u32::from(lowest_grade)) else {
            // C#: NullReferenceException on GetArmorGradeBuff(..).BuffId
            tracing::error!(
                "ApplyArmorGradeBuff: no armor grade buff for type {armor_type} grade {lowest_grade}"
            );
            return;
        };
        Self::add_gear_buff(
            server,
            unit,
            grade_buff.buff_id,
            grade_buff_ab_level as u32,
            0,
        );
    }

    /// `Unit.ApplyEquipItemSetBonuses()`. The set procs are collected by
    /// [`super::procs::refresh`] in the gear-bonus pass; this function keeps the buff part.
    fn apply_equip_item_set_bonuses(server: &GameServer, unit: &WorldObject, items: &[Item]) {
        use crate::systems::skills::buffs;
        let data = &server.game_data.items;
        let mut set_pieces: Vec<(u32, i32, u32)> = Vec::new(); // (set, pieces, min level)
        for item in items {
            let Some(t) = data.template(item.template_id) else {
                continue;
            };
            let Some(e) = t.equip() else { continue };
            if e.equip_item_set_id == 0 {
                continue;
            }
            match set_pieces
                .iter_mut()
                .find(|(s, _, _)| *s == e.equip_item_set_id)
            {
                Some((_, n, lvl)) => {
                    *n += 1;
                    if (t.level as u32) < *lvl {
                        *lvl = t.level as u32;
                    }
                }
                None => set_pieces.push((e.equip_item_set_id, 1, t.level as u32)),
            }
        }
        let mut applied: HashSet<u32> = HashSet::new();
        for (set_id, count, level) in set_pieces {
            let Some(set) = data.equip_item_set(set_id) else {
                // C#: NullReferenceException
                tracing::error!("ApplyEquipItemSetBonuses: unknown equip item set {set_id}");
                continue;
            };
            for bonus in &set.bonuses {
                if count >= bonus.num_pieces {
                    if bonus.buff_id != 0 {
                        if Self::check_buff(unit, bonus.buff_id) {
                            applied.insert(bonus.buff_id);
                            continue;
                        }
                        Self::add_gear_buff(server, unit, bonus.buff_id, level, unit.obj_id());
                        applied.insert(bonus.buff_id);
                    }
                } else if bonus.buff_id != 0
                    && Self::check_buff(unit, bonus.buff_id)
                    && !applied.contains(&bonus.buff_id)
                {
                    buffs::remove_buff(server, unit, bonus.buff_id);
                }
            }
        }
    }

    /// `ItemConversion` / `GradeEnchant` ... `SpecialEffect` bodies (`item_effects`), `true` =
    /// handled.
    fn special(server: &GameServer, special_type: i32, args: &EffectArgs<'_>, v: [i32; 4]) -> bool {
        /// `SpecialType` values whose `Execute` body is the economy's.
        mod special {
            pub const GAIN_ITEM: i32 = 27;
            pub const GAIN_ITEM_WITH_POS_IMPRINT: i32 = 32;
            pub const GAIN_ITEM_WITH_EMBLEM_IMPRINT: i32 = 45;
            pub const ADD_CHARACTER_SLOT: i32 = 91;
            pub const ITEM_CONVERSION: i32 = 49;
            pub const FISHING_LOOT: i32 = 79;
            pub const GRADE_ENCHANT: i32 = 92;
            pub const RECHARGE_ITEM_BUFF: i32 = 95;
            pub const EXP_TO_ITEM: i32 = 96;
            pub const CLEANUP_LOOK_CONVERT: i32 = 103;
            pub const ITEM_SOCKETING: i32 = 106;
            pub const ITEM_CAP_SCALE_RESET: i32 = 111;
            pub const ITEM_CAP_SCALE: i32 = 112;
            pub const AUCTION_POST_AUTHORITY: i32 = 117;
        }
        let caster_item = skill_item(&args.caster_caster);
        let target_item = item_target(&args.target_caster).map(|(id, _)| id);
        let tl = args.source.skill().map_or(0, |s| s.tl_id());
        let mut rng = SharedRandom;
        match special_type {
            special::GAIN_ITEM => {
                if let Some(p) = player_caster(args) {
                    item_effects::gain_item(server, p, v);
                }
            }
            special::GAIN_ITEM_WITH_POS_IMPRINT => {
                if let Some(p) = player_caster(args) {
                    item_effects::gain_item_with_pos_imprint(server, p, v);
                }
            }
            special::GAIN_ITEM_WITH_EMBLEM_IMPRINT => {
                item_effects::gain_item_with_emblem_imprint(
                    server,
                    player_target(args),
                    caster_item,
                    args.source.source_item.as_deref(),
                    v,
                );
            }
            special::ITEM_CONVERSION => {
                let out = match player_caster(args) {
                    Some(p) => item_effects::item_conversion(server, p, target_item, v, &mut rng),
                    None => Outcome::Cancelled,
                };
                if out == Outcome::Cancelled {
                    cancel_skill(args);
                }
            }
            special::FISHING_LOOT => {
                if let Some(p) = player_caster(args) {
                    item_effects::fishing_loot(server, p, &args.target, v, &mut rng);
                }
            }
            special::ADD_CHARACTER_SLOT => {
                if let Some(p) = player_caster(args) {
                    super::character_slots::apply(server, p);
                }
            }
            special::GRADE_ENCHANT => {
                if let Some(p) = player_caster(args) {
                    let support = match args.skill_object {
                        SkillObject::ItemGradeEnchantingSupport {
                            support_item_id, ..
                        } if *support_item_id != 0 => Some(*support_item_id),
                        _ => None,
                    };
                    item_effects::grade_enchant(
                        server,
                        p,
                        caster_item,
                        args.source.source_item.as_deref(),
                        target_item,
                        support,
                        v,
                        tl,
                        &mut rng,
                    );
                }
            }
            special::EXP_TO_ITEM => {
                let out = match player_caster(args) {
                    Some(p) => item_effects::exp_to_item(server, p, v),
                    None => Outcome::Cancelled,
                };
                if out == Outcome::Cancelled {
                    cancel_skill(args);
                }
            }
            special::RECHARGE_ITEM_BUFF => {
                let out = match player_caster(args) {
                    Some(p) => item_effects::recharge_item_buff(server, p, target_item, v),
                    None => Outcome::Cancelled,
                };
                if out == Outcome::Cancelled {
                    cancel_skill(args);
                }
            }
            special::CLEANUP_LOOK_CONVERT => {
                let out = match player_caster(args) {
                    Some(p) => item_effects::cleanup_look_convert(
                        server,
                        p,
                        caster_item,
                        args.source.source_item.as_deref(),
                        args.source.source_item_debited,
                        target_item,
                        v,
                    ),
                    None => Outcome::Cancelled,
                };
                if out == Outcome::Cancelled {
                    cancel_skill(args);
                }
            }
            special::ITEM_SOCKETING => {
                if let Some(p) = player_caster(args) {
                    item_effects::item_socketing(
                        server,
                        p,
                        caster_item,
                        args.source.source_item.as_deref(),
                        target_item,
                        v,
                        tl,
                        &mut rng,
                    );
                }
            }
            special::ITEM_CAP_SCALE_RESET => {
                if let Some(p) = player_caster(args) {
                    item_effects::item_cap_scale_reset(server, p, caster_item, target_item, v);
                }
            }
            special::ITEM_CAP_SCALE => {
                if let Some(p) = player_caster(args) {
                    let skill_id = args.source.skill().map_or(0, |s| s.template.id);
                    item_effects::item_cap_scale(
                        server,
                        p,
                        caster_item,
                        target_item,
                        skill_id,
                        v,
                        &mut rng,
                    );
                }
            }
            special::AUCTION_POST_AUTHORITY => {
                if let Some(p) = player_caster(args) {
                    let (account_id, world_id) = {
                        let c = p.data();
                        (c.account_id, c.unit.transform.world_id)
                    };
                    super::account_attributes::apply_effect(
                        server,
                        account_id,
                        world_id,
                        super::account_attributes::AccountAttributeGrant {
                            kind_id: super::account_attributes::KIND_AUCTION_POST_AUTHORITY,
                            bind_world: false,
                            is_add: true,
                            count: 0,
                            time: 0,
                        },
                    );
                }
            }
            _ => return false,
        }
        true
    }

    /// `RecoverExpEffect.Apply`.
    fn recover_exp(
        server: &GameServer,
        e: &crate::systems::skills::data::effects::RecoverExpEffect,
        args: &EffectArgs<'_>,
    ) {
        let Some(player) = player_caster(args) else {
            return;
        };
        let (name, level, recoverable, labor) = {
            let c = player.data();
            (
                c.unit.name.clone(),
                c.unit.level,
                c.recoverable_exp,
                c.labor_power,
            )
        };
        tracing::debug!("Player {name}");
        let mut out = Out::Direct(server);
        if recoverable <= 0 {
            // Is this one correct?
            send_error_message_to(
                &mut out,
                player,
                error_message::CANNOT_RECOVER_ALL_NOT_ENOUGH,
            );
            return;
        }
        // TODO (C#): Verify this formula
        let level = i32::from(level);
        let needed = if level <= 50 {
            level
        } else {
            50 + (level - 50) * 20
        };
        if e.need_labor_power && i32::from(labor) < needed {
            send_error_message_to(&mut out, player, error_message::NOT_ENOUGH_LABOR_POWER);
            return;
        }
        // NeedMoney: not used in 1.2
        if e.need_priest {
            let me = WorldObject::from(player);
            let found = server.world.world_of(&me).is_some_and(|w| {
                w.around(&me, |o| o.as_npc().cloned())
                    .into_iter()
                    .filter(|n| {
                        use crate::systems::skills::geometry::{distance_to, Target};
                        let t = Target::Obj(WorldObject::from(n));
                        distance_to(server, &Target::Obj(me.clone()), &t, false) <= 10.0
                    })
                    .any(|n| {
                        let tid = n.data().unit.template_id;
                        server
                            .game_data
                            .systems
                            .npcs
                            .npc
                            .template(tid)
                            .is_some_and(|t| t.priest)
                    })
            });
            if !found {
                send_error_message_to(&mut out, player, error_message::TOO_FAR_AWAY);
                return;
            }
        }
        // Use labor and recover exp (not ChangeLabor: no labor exp). Effects without a labor
        // cost (the scrolls' 26611) restore only. The checks above ran on a snapshot, so the
        // labor and the recoverable exp are re-checked, spent and taken under one lock: a
        // concurrent change can neither make the recovery cheaper nor credit it twice.
        let recoverable = {
            let mut c = player.data();
            if c.recoverable_exp <= 0 {
                None
            } else if e.need_labor_power && i32::from(c.labor_power) < needed {
                Some(Err(()))
            } else {
                if e.need_labor_power {
                    c.change_labor_power(-(needed as i16));
                }
                Some(Ok(std::mem::take(&mut c.recoverable_exp)))
            }
        };
        let recoverable = match recoverable {
            None => {
                send_error_message_to(
                    &mut out,
                    player,
                    error_message::CANNOT_RECOVER_ALL_NOT_ENOUGH,
                );
                return;
            }
            Some(Err(())) => {
                send_error_message_to(&mut out, player, error_message::NOT_ENOUGH_LABOR_POWER);
                return;
            }
            Some(Ok(recoverable)) => recoverable,
        };
        if e.need_labor_power {
            player.send(
                crate::systems::skills::progress::SCCharacterLaborPowerChangedPacket {
                    amount: -needed,
                    action: 0,
                    point: 0,
                    step: 0,
                },
            );
        }
        player.send(crate::systems::skills::progress::SCRecoverableExpPacket {
            obj_id: player.obj_id,
            recoverable_exp: 0,
            penaltied_exp: 0,
            reason: 1,
        });
        crate::systems::skills::progress::add_exp(server, player, recoverable, false);
    }

    /// The non-house, non-shipyard part of `CraftEffect.Apply` (housing and vehicles take their
    /// targets first): `Craft` / `Collect` / default -> `Craft.EndCraft()`, then
    /// `DoDoodadInteractionEvents(character, character, target.TemplateId)`.
    fn craft_effect(server: &GameServer, world_interaction: u32, args: &EffectArgs<'_>) -> bool {
        use crate::systems::npcs::area_trigger::world_interaction_group as g;
        let Some(character) = player_caster(args) else {
            return false;
        };
        let Some(target) = args.target.as_obj() else {
            return false;
        };
        tracing::debug!("CraftEffect, {world_interaction}");
        let wi_group = server
            .game_data
            .systems
            .npcs
            .area_trigger
            .get_world_interaction_group(world_interaction);
        match wi_group {
            Some(g::CRAFT | g::COLLECT) => super::craft::end_craft(server, character),
            _ => {
                tracing::warn!(
                    "CraftEffect, {world_interaction} does not have a wi group ({wi_group:?})"
                );
                super::craft::end_craft(server, character);
            }
        }
        let template_id = target.with_base_unit(|b| b.template_id());
        crate::systems::quests::events::on_doodad_interaction(
            server,
            character,
            character,
            template_id,
        );
        true
    }
}

impl ItemSkillHooks for EconomyForSkills {
    fn craft_cast_failed(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        skill_id: u32,
        result: u8,
    ) {
        super::craft::cast_failed(server, player, skill_id, result);
    }

    fn drop_trade_pack_to_floor(&self, server: &GameServer, player: &PlayerRef) {
        item_effects::drop_trade_pack_to_floor(server, player);
    }

    fn gear_stats(&self, server: &GameServer, player: &PlayerRef) -> GearStats {
        let data = &server.game_data.items;
        let formulas = &server.game_data.formulas;
        let items = lock(&Inv::of(player).equipment()).items.clone();
        let mut g = GearStats::default();
        for item in &items {
            if item.is_equip() && item.is_not_destroyed(data) {
                let st = |w| f64::from(item.stat(data, w).unwrap_or(0));
                g.str_ += st(EquipStat::Str);
                g.dex += st(EquipStat::Dex);
                g.sta += st(EquipStat::Sta);
                g.int += st(EquipStat::Int);
                g.spi += st(EquipStat::Spi);
            }
            match item.class {
                ItemClass::Armor | ItemClass::Accessory => {
                    g.armor += item.base_armor(data, formulas).unwrap_or(0);
                    g.magic_resistance += item.base_magic_resistance(data, formulas).unwrap_or(0);
                }
                ItemClass::Weapon => g.armor += item.weapon_armor(data).unwrap_or(0),
                _ => {}
            }
        }
        let weapon = |slot: i32| {
            items
                .iter()
                .find(|i| i.slot == slot && i.class == ItemClass::Weapon)
        };
        if let Some(w) = weapon(equip_slot::MAINHAND) {
            g.mainhand_dps = w.dps(data).unwrap_or(0.0);
            g.mainhand_mdps = w.mdps(data).unwrap_or(0.0) as f32;
            g.mainhand_hdps = w.hdps(data).unwrap_or(0.0) as f32;
        }
        if let Some(w) = weapon(equip_slot::OFFHAND) {
            g.offhand_dps = w.dps(data).unwrap_or(0.0);
        }
        if let Some(w) = weapon(equip_slot::RANGED) {
            g.ranged_dps = w.dps(data).unwrap_or(0.0);
        }
        g
    }

    fn weapon_holdable(
        &self,
        server: &GameServer,
        unit: &WorldObject,
        slot: i32,
    ) -> Option<HoldableInfo> {
        let item = equipped_by_slot(unit, slot)?;
        holdable_info(server, &item)
    }

    fn weapon_attack_anim(
        &self,
        server: &GameServer,
        holdable_id: u32,
        index: i32,
        left_hand: bool,
    ) -> u32 {
        server
            .game_data
            .items
            .holdable(holdable_id)
            .map_or(crate::data::items::Holdable::DEFAULT_FIST_ANIM_ID, |h| {
                h.attack_anim_id(index, left_hand)
            })
    }

    fn has_equipped(&self, _server: &GameServer, player: &PlayerRef, slot: i32) -> bool {
        Inv::of(player).equipped(slot).is_some()
    }

    fn item_count(&self, _server: &GameServer, player: &PlayerRef, template_id: u32) -> i32 {
        Inv::of(player).items_count_unlocked(template_id, -1)
    }

    fn owned_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        item_id: u64,
    ) -> Option<(u32, i32)> {
        server
            .items
            .item_by_id(item_id)
            .filter(|i| i.owner_id == u64::from(player.character_id))
            .map(|i| (i.template_id, i.count))
    }

    fn use_skill_as_reagent(&self, server: &GameServer, template_id: u32) -> bool {
        server
            .game_data
            .items
            .template(template_id)
            .is_some_and(|t| t.use_skill_as_reagent)
    }

    fn item_use_skill(&self, server: &GameServer, template_id: u32) -> Option<(u32, bool)> {
        server.game_data.items.template(template_id).map(|t| {
            (
                t.use_skill_id,
                t.bind_type == crate::data::items::bind_type::BIND_ON_PICKUP,
            )
        })
    }

    fn consume_owned_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        item_id: u64,
        expected_template_id: u32,
        amount: i32,
    ) -> Option<bool> {
        let Some(container) = server.items.holding_container(item_id) else {
            return Some(false);
        };
        let mut txn = ItemTxn::begin(server);
        let mut guard = lock(&container);
        let matches = guard.get_item_by_id(item_id).is_some_and(|item| {
            item.template_id == expected_template_id
                && item.owner_id == u64::from(player.character_id)
        });
        if !matches {
            return Some(false);
        }
        if amount > 0 {
            let consumed = guard.consume_item(
                task_type::SKILL_REAGENTS,
                expected_template_id,
                amount,
                Some(item_id),
                &mut txn.env,
            );
            if consumed != amount {
                return Some(false);
            }
        }
        Some(true)
    }

    fn take_cast_source_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        item_id: u64,
        expected_template_id: u32,
        amount: i32,
    ) -> Option<Result<TakenCastItem, ()>> {
        let amount = amount.max(0);
        let inv = Inv::of(player);
        let mut txn = ItemTxn::begin(server);
        // Only the caster's bag or equipment may hold the cast item: a trade, mail, auction,
        // bank or coffer copy is not a usable cast source.
        for st in [slot_type::INVENTORY, slot_type::EQUIPMENT] {
            let Some(container) = inv.get(st) else {
                continue;
            };
            let mut guard = lock(&container);
            let Some(item) = guard.get_item_by_id(item_id).cloned() else {
                continue;
            };
            if item.template_id != expected_template_id
                || item.owner_id != u64::from(player.character_id)
                || item.count < amount
            {
                return Some(Err(()));
            }
            if amount == 0 {
                let slot = item.slot;
                drop(guard);
                return Some(Ok(TakenCastItem {
                    item,
                    container,
                    slot,
                    amount,
                    removed: false,
                }));
            }
            if !txn.env.rules.can_destroy(&item) {
                return Some(Err(()));
            }
            if amount >= item.count {
                // Full consume: the item leaves its container now, so the client removal comes
                // before any product can take the freed slot.
                let Some(removed) = guard.remove_item(item_id) else {
                    return Some(Err(()));
                };
                let slot = removed.slot;
                guard.notify_leave(&removed, removed.slot as u8, &mut txn.env.fx);
                txn.env.fx.tasks(
                    player.character_id,
                    task_type::SKILL_REAGENTS,
                    vec![ItemTask::remove_slot(&removed)],
                );
                drop(guard);
                return Some(Ok(TakenCastItem {
                    item: removed,
                    container,
                    slot,
                    amount,
                    removed: true,
                }));
            }
            // Partial consume: debit in place, keeping the stack in its slot so a product or
            // loot drop can never be placed into the same slot.
            let after = {
                let Some(live) = guard.get_item_by_id_mut(item_id) else {
                    return Some(Err(()));
                };
                live.count -= amount;
                live.dirty = true;
                live.clone()
            };
            guard.update_free_slot_count();
            txn.env.fx.tasks(
                player.character_id,
                task_type::SKILL_REAGENTS,
                vec![ItemTask::count_update(&after, -amount)],
            );
            drop(guard);
            return Some(Ok(TakenCastItem {
                item,
                container,
                slot: after.slot,
                amount,
                removed: false,
            }));
        }
        Some(Err(()))
    }

    fn commit_cast_source_item(&self, server: &GameServer, taken: TakenCastItem) {
        if taken.amount == 0 {
            return;
        }
        let owner = taken.item.owner_id as u32;
        let mut txn = ItemTxn::begin(server);
        // A partial debit drops the same count; the client already got its count update.
        txn.env.fx.push(ItemFx::Consumed {
            owner,
            template_id: taken.item.template_id,
            count: taken.amount,
        });
        if taken.removed {
            // The client already got the removal at take time.
            txn.env.fx.push(ItemFx::ReleaseId(taken.item.id));
        }
    }

    fn refund_cast_source_item(&self, server: &GameServer, taken: TakenCastItem) {
        if taken.amount == 0 {
            return;
        }
        let owner = taken.item.owner_id as u32;
        let mut txn = ItemTxn::begin(server);
        if !taken.removed {
            // Partial debit: put the units back into the same stack (same id and slot).
            let container = taken.container.clone();
            let mut guard = lock(&container);
            let max_count = server
                .game_data
                .items
                .template(taken.item.template_id)
                .map_or(0, |t| t.max_count);
            if let Some(live) = guard
                .get_item_by_id_mut(taken.item.id)
                .filter(|live| max_count <= 0 || live.count + taken.amount <= max_count)
            {
                live.count += taken.amount;
                live.dirty = true;
                let after = live.clone();
                drop(guard);
                txn.env.fx.tasks(
                    owner,
                    task_type::SKILL_REAGENTS,
                    vec![ItemTask::count_update(&after, taken.amount)],
                );
                return;
            }
            drop(guard);
            // The kept remainder moved or was consumed while the effects ran: return the
            // debited units as a fresh stack with the snapshot's template and properties.
            let Some(mut restored) = server.items.create(
                taken.item.template_id,
                taken.amount,
                taken.item.grade,
                taken.item.world_id,
                &server.game_data.items,
                &server.ids.item,
                chrono::Utc::now().timestamp(),
            ) else {
                tracing::error!(
                    item_id = taken.item.id,
                    template_id = taken.item.template_id,
                    "cancelled cast could not recreate its debited source units"
                );
                return;
            };
            restored.flags = taken.item.flags;
            restored.ucc_id = taken.item.ucc_id;
            restored.details = taken.item.details.clone();
            restored.lifespan_mins = taken.item.lifespan_mins;
            restored.made_unit_id = taken.item.made_unit_id;
            restored.crafter_id = taken.item.crafter_id;
            restored.create_time = taken.item.create_time;
            restored.unsecure_time = taken.item.unsecure_time;
            restored.unpack_time = taken.item.unpack_time;
            restored.charge_start_time = taken.item.charge_start_time;
            restored.bind_on_unpack = taken.item.bind_on_unpack;
            restored.expiration_time = taken.item.expiration_time;
            restored.expiration_online_minutes_left = taken.item.expiration_online_minutes_left;
            restored.charge_count = taken.item.charge_count;
            restored.owner_id = taken.item.owner_id;
            restored.dirty = true;
            put_restored_item(server, &mut txn, &container, taken.slot, restored, owner);
            return;
        }
        // Full consume: restore the whole item to its slot if it is still free.
        let container = taken.container.clone();
        let mut item = taken.item;
        item.dirty = true;
        put_restored_item(server, &mut txn, &container, taken.slot, item, owner);
    }

    fn take_item_template(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        template_id: u32,
        amount: i32,
    ) -> Option<bool> {
        if amount <= 0 {
            return Some(true);
        }
        let mut txn = ItemTxn::begin(server);
        let inv = Inv::of(player);
        // The economy operation lock serialises item moves, so this count cannot change before
        // the debit below: a missing reagent consumes nothing.
        if inv.items_count_unlocked(template_id, -1) < amount {
            return Some(false);
        }
        let consumed = inventory::consume_item(
            &mut txn,
            &inv,
            None,
            task_type::SKILL_EFFECT_CONSUMPTION,
            template_id,
            amount,
            None,
        );
        Some(consumed == amount)
    }

    fn consume_item_by_id(
        &self,
        server: &GameServer,
        _player: &PlayerRef,
        item_id: u64,
        amount: i32,
    ) {
        let Some(c) = server.items.holding_container(item_id) else {
            return;
        };
        let mut txn = ItemTxn::begin(server);
        let template_id = lock(&c).get_item_by_id(item_id).map(|i| i.template_id);
        if let Some(t) = template_id {
            lock(&c).consume_item(
                task_type::SKILL_REAGENTS,
                t,
                amount,
                Some(item_id),
                &mut txn.env,
            );
        }
    }

    fn consume_item_template(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        template_id: u32,
        amount: i32,
    ) {
        let mut txn = ItemTxn::begin(server);
        inventory::consume_item(
            &mut txn,
            &Inv::of(player),
            None,
            task_type::SKILL_EFFECT_CONSUMPTION,
            template_id,
            amount,
            None,
        );
    }

    fn acquire_item(&self, server: &GameServer, player: &PlayerRef, template_id: u32, amount: i32) {
        let mut txn = ItemTxn::begin(server);
        lock(&Inv::of(player).bag()).acquire_default_item(
            task_type::SKILL_EFFECT_GAIN_ITEM,
            template_id,
            amount,
            -1,
            0,
            &mut txn.env,
        );
    }

    fn pay_building_material(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        template_id: u32,
        amount: i32,
    ) -> Option<ItemFxList> {
        if template_id == 0 || amount <= 0 {
            return Some(ItemFxList::default());
        }
        let mut txn = ItemTxn::begin(server);
        let inv = Inv::of(player);
        // Match both the availability check and Inventory.ConsumeItem's container set.
        if inv.items_count_unlocked(template_id, -1) < amount {
            return None;
        }
        let snapshots: Vec<_> = [slot_type::INVENTORY, slot_type::EQUIPMENT, slot_type::BANK]
            .into_iter()
            .filter_map(|st| inv.get(st))
            .map(|container| {
                let snapshot = lock(&container).clone();
                (container, snapshot)
            })
            .collect();
        let consumed = inventory::consume_item(
            &mut txn,
            &inv,
            None,
            task_type::SKILL_EFFECT_CONSUMPTION,
            template_id,
            amount,
            None,
        );
        if consumed != amount {
            // ItemTxn drops by COMMITTING its effects. Restore the exact items (ids, slots,
            // flags and container state) and discard every queued effect before it drops.
            for (container, snapshot) in snapshots {
                *lock(&container) = snapshot;
            }
            txn.env.take_fx();
            return None;
        }
        Some(txn.into_fx())
    }

    fn item_set(&self, server: &GameServer, item_set_id: u32) -> Vec<(u32, i32)> {
        server
            .game_data
            .items
            .item_set(item_set_id)
            .map(|s| s.items.iter().map(|i| (i.item_id, i.count)).collect())
            .unwrap_or_default()
    }

    fn durability_loss(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        amount: i32,
        targets: u8,
        rate: f32,
    ) {
        use super::repair::DurabilityLossTargets as T;
        let t = match targets {
            0 => T::All,
            1 => T::AllArmor,
            2 => T::AllWeapons,
            3 => T::AllMainWeapons,
            4 => T::PrimaryWeapon,
            5 => T::SecondaryWeapon,
            6 => T::RangedWeapon,
            7 => T::Shield,
            _ => T::Feet,
        };
        super::repair::apply_durability_loss_to_equipment(
            server,
            player,
            amount.clamp(0, i32::from(u8::MAX)) as u8,
            t,
            rate,
            &mut SharedRandom,
        );
    }

    fn unlock_item(&self, server: &GameServer, player: &PlayerRef, item_id: u64) -> bool {
        // SCItemTaskSuccessPacket(ItemUnlock, ItemUpdate(skillItem.SkillSourceItem)). The C#
        // keeps the item object of the cast; here a source item the plot consumed is gone.
        let Some(item) = server.items.item_by_id(item_id) else {
            return false;
        };
        player.send(task_packet(
            task_type::ITEM_UNLOCK,
            vec![ItemTask::update(&item)],
        ));
        true
    }

    fn apply_gear_buffs(
        &self,
        server: &GameServer,
        unit: &WorldObject,
        added: Option<&Item>,
        removed: Option<&Item>,
    ) {
        let items = equipment_items(unit);
        Self::apply_equip_effects(server, unit, &items, added, removed);
        Self::apply_weapon_wield_buff(server, unit);
        Self::apply_armor_grade_buff(server, unit, &items, added, removed);
        Self::apply_equip_item_set_bonuses(server, unit, &items);
        // Set, item-binding and holdable procs of the current equipment. Rebuilt on every gear
        // change (equip / unequip, gear break, repair, login, weapon-set swap, gem change), so
        // the active set always matches the equipped items.
        super::procs::refresh(server, unit, &items);
    }

    fn apply_effect(
        &self,
        server: &GameServer,
        kind: EffectKind,
        template: &EffectTemplate,
        args: &EffectArgs<'_>,
    ) -> bool {
        match (kind, template) {
            (EffectKind::GainLootPackItemEffect, EffectTemplate::GainLootPackItem(e)) => {
                // Ignore if not a player
                if let Some(p) = player_caster(args) {
                    let effect = item_effects::GainLootPackItemEffect {
                        loot_pack_id: e.loot_pack_id,
                        consume_source_item: e.consume_source_item,
                        consume_item_id: e.consume_item_id,
                        consume_count: e.consume_count,
                        inherit_grade: e.inherit_grade,
                    };
                    item_effects::gain_loot_pack_item(
                        server,
                        p,
                        &effect,
                        skill_item(&args.caster_caster),
                        args.source.source_item.as_deref(),
                        args.source.source_item_debited,
                        &mut SharedRandom,
                    );
                }
                true
            }
            (EffectKind::ImprintUccEffect, EffectTemplate::ImprintUcc { .. }) => {
                tracing::trace!("ImprintUccEffect");
                let out = match player_caster(args).filter(|caster| {
                    player_target(args).is_some_and(|target| target.obj_id == caster.obj_id)
                }) {
                    Some(player) => item_effects::imprint_ucc(
                        server,
                        Some(player),
                        skill_item(&args.caster_caster),
                        args.source.source_item.as_deref(),
                        item_target(&args.target_caster).map(|(id, _)| id),
                    ),
                    None => Outcome::Cancelled,
                };
                if out == Outcome::Cancelled {
                    cancel_skill(args);
                }
                true
            }
            (EffectKind::CleanupUccEffect, EffectTemplate::CleanupUcc { .. }) => {
                tracing::trace!("CleanupUccEffect");
                let out = match player_caster(args).filter(|caster| {
                    player_target(args).is_some_and(|target| target.obj_id == caster.obj_id)
                }) {
                    Some(player) => item_effects::cleanup_ucc(
                        server,
                        Some(player),
                        skill_item(&args.caster_caster),
                        args.source.source_item.as_deref(),
                        item_target(&args.target_caster).map(|(id, _)| id),
                    ),
                    None => Outcome::Cancelled,
                };
                if out == Outcome::Cancelled {
                    cancel_skill(args);
                }
                true
            }
            (EffectKind::ConversionEffect, EffectTemplate::Conversion(_)) => {
                item_effects::conversion_effect();
                true
            }
            (
                EffectKind::PutDownBackpackEffect,
                EffectTemplate::PutDownBackpack {
                    backpack_doodad_id, ..
                },
            ) => {
                if let Some(p) = player_caster(args) {
                    item_effects::put_down_backpack(
                        server,
                        p,
                        *backpack_doodad_id,
                        skill_item(&args.caster_caster),
                    );
                }
                true
            }
            (EffectKind::RecoverExpEffect, EffectTemplate::RecoverExp(e)) => {
                Self::recover_exp(server, e, args);
                true
            }
            (
                EffectKind::CraftEffect,
                EffectTemplate::Craft {
                    world_interaction, ..
                },
            ) => Self::craft_effect(server, *world_interaction, args),
            (EffectKind::TrainCraftEffect, EffectTemplate::TrainCraft { craft_id, .. }) => {
                let Some(player) = player_caster(args) else {
                    return true;
                };
                // An item cast names its template on the caster; the source item may already
                // have been debited out of its container. Only a non-item cast uses the
                // effect's own craft.
                let item_template_id = match args.caster_caster {
                    SkillCaster::Item {
                        item_template_id, ..
                    } => Some(item_template_id),
                    _ => None,
                };
                match super::craft_learning::train_craft(
                    server,
                    player,
                    *craft_id,
                    item_template_id,
                ) {
                    super::craft_learning::TrainOutcome::Learned(crafts) => {
                        tracing::debug!(
                            character = player.character_id,
                            ?crafts,
                            "TrainCraftEffect: learned craft"
                        );
                    }
                    super::craft_learning::TrainOutcome::AlreadyLearned => {
                        tracing::debug!(
                            character = player.character_id,
                            craft_id,
                            "TrainCraftEffect: craft already learned, item kept"
                        );
                        // `craft_already_learned` (client error id 0x81 = 129, X2G:1136026).
                        inventory::send_error_message(
                            player,
                            error_message::CRAFT_ALREADY_LEARNED,
                            0,
                            true,
                        );
                        // `Skill.Apply` refunds its pre-debited source item unless the cast is
                        // cancelled, as the dye path does.
                        cancel_skill(args);
                    }
                    super::craft_learning::TrainOutcome::NoRecipe { item_template_id } => {
                        tracing::warn!(
                            character = player.character_id,
                            ?item_template_id,
                            effect_craft_id = craft_id,
                            "TrainCraftEffect: no learnable craft in the data, item kept"
                        );
                        cancel_skill(args);
                    }
                }
                true
            }
            (EffectKind::AccountAttributeEffect, EffectTemplate::AccountAttribute(e)) => {
                if let Some(p) = player_caster(args) {
                    let (account_id, world_id) = {
                        let c = p.data();
                        (c.account_id, c.unit.transform.world_id)
                    };
                    super::account_attributes::apply_effect(
                        server,
                        account_id,
                        world_id,
                        super::account_attributes::AccountAttributeGrant {
                            kind_id: e.kind_id.min(i32::MAX as u32) as i32,
                            bind_world: e.bind_world,
                            is_add: e.is_add,
                            count: e.count.min(i32::MAX as u32) as i32,
                            time: e.time.min(i32::MAX as u32) as i32,
                        },
                    );
                }
                true
            }
            _ => false,
        }
    }

    fn try_add_new_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        task_type: u8,
        template_id: u32,
        count: i32,
        grade: u8,
    ) -> bool {
        let mut txn = ItemTxn::begin(server);
        inventory::try_add_new_item(
            &mut txn,
            &Inv::of(player),
            task_type,
            template_id,
            count,
            i32::from(grade),
            0,
        )
    }

    fn item_template_exists(&self, server: &GameServer, template_id: u32) -> bool {
        server.game_data.items.template(template_id).is_some()
    }

    fn acquire_default_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        task_type: u8,
        template_id: u32,
        amount: i32,
    ) -> bool {
        let mut txn = ItemTxn::begin(server);
        let result = lock(&Inv::of(player).bag()).acquire_default_item(
            task_type,
            template_id,
            amount,
            -1,
            0,
            &mut txn.env,
        );
        result
    }

    fn consume_bag_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        task_type: u8,
        template_id: u32,
        amount: i32,
    ) -> i32 {
        let mut txn = ItemTxn::begin(server);
        let result = lock(&Inv::of(player).bag()).consume_item(
            task_type,
            template_id,
            amount,
            None,
            &mut txn.env,
        );
        result
    }

    fn consume_bag_item_by_id(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        task_type: u8,
        item_id: u64,
        expected_template_id: u32,
        amount: i32,
    ) -> Option<bool> {
        let container = Inv::of(player).bag();
        let mut txn = ItemTxn::begin(server);
        let mut guard = lock(&container);
        let matches = guard.get_item_by_id(item_id).is_some_and(|item| {
            item.template_id == expected_template_id
                && item.owner_id == u64::from(player.character_id)
        });
        if !matches || amount <= 0 {
            return Some(false);
        }
        let consumed = guard.consume_item(
            task_type,
            expected_template_id,
            amount,
            Some(item_id),
            &mut txn.env,
        );
        Some(consumed == amount)
    }

    fn ucc_create_stamp(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        source_item_id: u64,
        source_item: Option<&Item>,
    ) {
        let source = source_item
            .cloned()
            .or_else(|| server.items.item_by_id(source_item_id));
        if let Some(source) = source {
            item_effects::create_stamp(server, player, &source);
        }
    }

    fn item_conversion(&self, server: &GameServer, player: &PlayerRef, item_id: u64) -> bool {
        item_effects::item_conversion(server, player, Some(item_id), [0; 4], &mut SharedRandom)
            == Outcome::Cancelled
    }

    fn dye_item(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        dye_item_id: u64,
        dye_template_id: u32,
        target_item_id: u64,
        source_item: Option<&Item>,
    ) {
        let mut txn = ItemTxn::begin(server);
        let inv = Inv::of(player);
        // Skill.Apply refunds its pre-debited source item unless this flag cancels it. Reject
        // before editing equipment so an invalid target also keeps its dye.
        let reject = || player.data().skill_char.skill_cancelled = true;
        let dye = match source_item {
            Some(item) => Some(item.clone()),
            None => inv
                .holder_of(dye_item_id)
                .and_then(|holder| lock(&holder).get_item_by_id(dye_item_id).cloned()),
        };
        if !dye.is_some_and(|i| i.template_id == dye_template_id && i.count > 0) {
            reject();
            return;
        }
        let Some(holder) = inv.holder_of(target_item_id) else {
            tracing::error!(
                "Dyeing: target item {target_item_id} not found (NullReferenceException)"
            );
            reject();
            return;
        };
        // Check the item used is in the Dye (33) category.
        let category = server
            .game_data
            .items
            .template(dye_template_id)
            .map_or(0, |t| t.category_id);
        if category != CATEGORY_DYE {
            tracing::warn!(
                "{} tried to dye with a non-dye item - item id {dye_item_id} and template id {dye_template_id}",
                player.data().unit.name
            );
            reject();
            return;
        }
        let target = lock(&holder).get_item_by_id(target_item_id).cloned();
        if !target.is_some_and(|i| {
            i.is_equip()
                && server
                    .game_data
                    .items
                    .default_dye_ids
                    .contains_key(&i.template_id)
        }) {
            reject();
            return;
        }
        let after = {
            let mut g = lock(&holder);
            let Some(it) = g.get_item_by_id_mut(target_item_id) else {
                drop(g);
                reject();
                return;
            };
            it.details.dye_item_id = dye_template_id;
            it.dirty = true;
            it.clone()
        };
        txn.out().send(
            player,
            task_packet(task_type::DYEING, vec![ItemTask::update(&after)]),
        );
    }

    fn renew_equipment(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        source_item_id: u64,
        source_template_id: u32,
        source_slot: u32,
        target_item_id: u64,
        target_slot: u32,
    ) -> bool {
        super::renew::renew_equipment(
            server,
            player,
            source_item_id,
            source_template_id,
            source_slot,
            target_item_id,
            target_slot,
        )
    }

    fn physical_enchant(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        item_id: u64,
        armor: bool,
        npc_template_id: Option<u32>,
    ) -> bool {
        super::renew::physical_enchant(
            server,
            player,
            item_id,
            armor,
            npc_template_id,
            &mut SharedRandom,
        )
    }

    fn give_fishing_loot(
        &self,
        _server: &GameServer,
        _player: &PlayerRef,
        _zone_group_id: u32,
        _land: bool,
    ) -> Option<bool> {
        // `FishingLoot` is handled whole by `apply_special_effect`.
        None
    }

    fn cleanup_look_convert(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        target_item_id: u64,
        powder_item_id: Option<u64>,
        powder_item: Option<&Item>,
        powder_debited: bool,
    ) -> ItemActionResult {
        let powder = match powder_item {
            Some(p) => Some(SkillItem {
                item_id: p.id,
                item_template_id: p.template_id,
            }),
            None => powder_item_id.and_then(|id| {
                server.items.item_by_id(id).map(|i| SkillItem {
                    item_id: id,
                    item_template_id: i.template_id,
                })
            }),
        };
        match item_effects::cleanup_look_convert(
            server,
            player,
            powder,
            powder_item,
            powder_debited,
            Some(target_item_id),
            [0; 4],
        ) {
            Outcome::Done => ItemActionResult::Done,
            Outcome::Cancelled => ItemActionResult::Cancel,
        }
    }

    fn skinize(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        target_item_id: u64,
        powder_item_id: Option<u64>,
        powder_item: Option<&Item>,
        powder_debited: bool,
    ) -> ItemActionResult {
        let mut txn = ItemTxn::begin(server);
        let inv = Inv::of(player);
        let Some(holder) = inv.holder_of(target_item_id) else {
            return ItemActionResult::Cancel;
        };
        let skinized = lock(&holder)
            .get_item_by_id(target_item_id)
            .is_some_and(|i| i.has_flag(ITEM_FLAG_SKINIZED));
        if skinized {
            // Already an image item
            // TODO (C#): Get the correct error as this is likely not correct
            return ItemActionResult::CancelWithError(
                error_message::ITEM_LOOK_CONVERT_AS_NOT_USE_AS_SKIN,
            );
        }
        let powder = match powder_item {
            Some(p) => p.clone(),
            None => {
                let Some(powder_id) = powder_item_id else {
                    return ItemActionResult::Cancel;
                };
                let Some(powder) = inv.item_by_id(powder_id) else {
                    return ItemActionResult::Cancel;
                };
                powder
            }
        };
        if powder.count < 1 {
            return ItemActionResult::Cancel;
        }
        let after = {
            let mut g = lock(&holder);
            let Some(it) = g.get_item_by_id_mut(target_item_id) else {
                return ItemActionResult::Cancel;
            };
            it.flags |= ITEM_FLAG_SKINIZED;
            it.dirty = true;
            it.clone()
        };
        txn.out().send(
            player,
            task_packet(task_type::SKINIZE, vec![ItemTask::update_bits(&after)]),
        );
        // The cast's debit already consumed the powder; only the fallback pays for it here.
        if !powder_debited {
            if let Some(c) = inv.holder_of_any(powder.id) {
                lock(&c).consume_item(
                    task_type::SKINIZE,
                    powder.template_id,
                    1,
                    Some(powder.id),
                    &mut txn.env,
                );
            }
        }
        ItemActionResult::Done
    }

    fn item_socketing(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        gem_item_id: u64,
        target_item_id: u64,
        gem_item: Option<&Item>,
    ) -> bool {
        // Handled whole by `apply_special_effect` (which sends the skill end); this path only
        // runs without the special-effect routing.
        let gem = match gem_item {
            Some(i) => Some(SkillItem {
                item_id: i.id,
                item_template_id: i.template_id,
            }),
            None => server.items.item_by_id(gem_item_id).map(|i| SkillItem {
                item_id: gem_item_id,
                item_template_id: i.template_id,
            }),
        };
        item_effects::item_socketing(
            server,
            player,
            gem,
            gem_item,
            Some(target_item_id),
            [0; 4],
            0,
            &mut SharedRandom,
        );
        false
    }

    fn grade_enchant(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        request: &GradeEnchantRequest,
        scroll_item: Option<&Item>,
    ) -> bool {
        item_effects::grade_enchant(
            server,
            player,
            Some(SkillItem {
                item_id: request.scroll_item_id,
                item_template_id: request.scroll_template_id,
            }),
            scroll_item,
            Some(request.target_item_id),
            (request.support_item_id != 0).then_some(request.support_item_id),
            [i32::from(request.is_lucky), 0, request.item_type, 0],
            0,
            &mut SharedRandom,
        );
        false
    }

    fn equipped_item_category(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        slot: i32,
    ) -> Option<u32> {
        let item = Inv::of(player).equipped(slot)?;
        server
            .game_data
            .items
            .template(item.template_id)
            .map(|t| t.category_id as u32)
    }

    fn create_sheet_music(&self, server: &GameServer, player: &PlayerRef, item_id: u64) -> bool {
        // MusicManager.CreateSheetMusic is a DB call: it runs as a task here (deviation: the
        // C# blocks and returns its result; failures are logged by the music manager).
        let (Some(srv), Ok(handle)) = (server.self_arc(), tokio::runtime::Handle::try_current())
        else {
            return false;
        };
        let player = player.clone();
        handle.spawn(async move {
            let ok = srv
                .systems
                .services
                .music
                .create_sheet_music(&srv, &player, item_id, &EconomySheetMusic { server: &srv })
                .await;
            if !ok {
                tracing::error!(
                    "Special effects: UserMusicSaveNotes - Error saving music for {} !",
                    player.data().unit.name
                );
            }
        });
        true
    }

    fn apply_special_effect(
        &self,
        server: &GameServer,
        special_type: i32,
        args: &EffectArgs<'_>,
        values: [i32; 4],
    ) -> bool {
        Self::special(server, special_type, args, values)
    }
}
