//! Game-data tables of the skills subsystem (`server.game_data.systems.skills`): the full
//! `SkillManager.Load`, `PlotManager.Load`, `AnimationManager.Load`, `BuffGameData`, the
//! `unit_reqs`, `aoe_shapes`, actor model radii and npc tags.

pub mod effects;
pub mod misc;
pub mod plots;
pub mod templates;
pub mod zone_rules;

#[cfg(test)]
mod buff_game_data_tests;
#[cfg(test)]
mod skill_manager_tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use rusqlite::Connection;

use crate::data::sqlite::for_each_row;
use crate::models::unit::{
    BuffModifier, CombatBuffTemplate, DynamicBonusTemplate, LinearFuncTemplate, SkillAttribute,
    SkillModifier, UnitModifierType,
};
use crate::systems::DataLoadCtx;

pub use effects::{EffectKind, EffectTemplate, EffectType};
pub use misc::{AnimData, AreaShape, BuffGameData, BuffTolerance, UnitReq};
pub use plots::{PlotData, PlotEventTemplate, PlotNextEvent, PlotTree};
pub use templates::{
    BuffTemplate, SkillControllerTemplate, SkillEffect, SkillTemplate, TickEffect,
};

/// `BuffTriggerTemplate`.
#[derive(Debug, Clone, PartialEq)]
pub struct BuffTriggerTemplate {
    pub id: u32,
    /// `BuffEventTriggerKind` (raw `event_id`).
    pub kind: u16,
    pub effect_on_source: bool,
    /// `Effect` (resolved through `effects`).
    pub effect: (EffectKind, u32),
    pub use_damage_amount: bool,
    pub use_original_source: bool,
    pub target_buff_tag_id: u32,
    pub target_no_buff_tag_id: u32,
    pub synergy: bool,
}

/// `SkillReagent` / `SkillProduct`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillItemAmount {
    pub id: u32,
    pub skill_id: u32,
    pub item_id: u32,
    pub amount: i16,
}

/// `resurrection_waiting_times`: the escalating respawn waits by consecutive-death step.
///
/// The 1.2 client loads the same table (`LoadResurrectionWaitingTimeDescs`, `x2game.dll.c`
/// 0x39620CE0) and stores `[penalty_duration, siege_waiting_time, waiting_time]` by `id`, but
/// only displays the `resurrectWaitTime` the server sends in `SCUnitDeathPacket`. The values
/// are the only server-side evidence: a 600 s `penalty_duration` on every row, the normal
/// waits rising 0, 5, 45, 90, 180 s (the documented "up to 180 seconds"), and the siege
/// alternative falling 20, 15, 10, 5, 0 s. `id` is therefore the 1-based death step; a death
/// while the previous one is within `penalty_duration` advances the step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResurrectionWaitingTime {
    pub id: u32,
    /// Seconds a death keeps the step alive (the "penalty duration"); 600 on every 1.2 row.
    pub penalty_duration: i32,
    /// Seconds to wait before resurrecting in a normal zone.
    pub waiting_time: i32,
    /// Seconds to wait before resurrecting while the death zone's siege is active.
    pub siege_waiting_time: i32,
}

/// The skills subsystem's tables.
#[derive(Debug, Default)]
pub struct Data {
    /// `_skills`.
    pub skills: HashMap<u32, Arc<SkillTemplate>>,
    /// `_buffs`.
    pub buffs: HashMap<u32, Arc<BuffTemplate>>,
    /// `_types` (effects table).
    pub effect_types: HashMap<u32, EffectType>,
    /// `_effects`.
    pub effects: HashMap<EffectKind, HashMap<u32, EffectTemplate>>,
    /// `_buffTags`: buff id -> tag ids.
    pub buff_tags: HashMap<u32, Vec<u32>>,
    /// `_taggedBuffs`: tag id -> buff ids.
    pub tagged_buffs: HashMap<u32, Vec<u32>>,
    /// `buff_breakers`: buff id -> tags on the owner that end that buff.
    pub buff_breakers: HashMap<u32, Vec<u32>>,
    /// `_skillTags`.
    pub skill_tags: HashMap<u32, Vec<u32>>,
    /// `_taggedSkills`.
    pub tagged_skills: HashMap<u32, Vec<u32>>,
    /// Skills whose 1.2 `skills.desc` tooltip names the close-range damage penalty of
    /// `formulas` row 12 (`damage_multiplier_by_range`); built from the en_us text at load
    /// (see [`load_range_penalty_skills`]).
    pub range_penalty_skills: HashSet<u32>,
    /// Authored zone-group restrictions and item tag membership.
    pub zone_rules: zone_rules::ZoneRules,
    /// `_skillModifiers` by owner id.
    pub skill_modifiers: HashMap<u32, Vec<SkillModifier>>,
    /// `_buffTriggers` by buff id.
    pub buff_triggers: HashMap<u32, Vec<BuffTriggerTemplate>>,
    /// `_combatBuffs` by `ReqBuffId`.
    pub combat_buffs: HashMap<u32, Vec<CombatBuffTemplate>>,
    /// `_linearFuncs`.
    pub linear_funcs: HashMap<u32, LinearFuncTemplate>,
    /// `manual_funcs`: per-tick values for dynamic buff modifiers.
    pub manual_funcs: HashMap<u32, Vec<i32>>,
    /// `_skillReagents` (table order).
    pub skill_reagents: Vec<SkillItemAmount>,
    /// `_skillProducts` (table order).
    pub skill_products: Vec<SkillItemAmount>,
    /// `unit_modifiers WHERE owner_type='Item'` (`ItemManager.GetUnitModifiers`).
    pub item_unit_modifiers: HashMap<u32, Vec<crate::models::unit::BonusTemplate>>,
    pub plots: PlotData,
    pub anims: AnimData,
    pub area_shapes: HashMap<u32, AreaShape>,
    /// `ModelManager.GetActorModel(id).Radius`.
    pub model_radii: HashMap<u32, f32>,
    /// `UnitRequirementsGameData` by owner type.
    pub unit_reqs: HashMap<String, Vec<UnitReq>>,
    /// Buff-dependent caster/target restrictions (`skill_reqs` and skill/tag bindings).
    pub buff_skill_requirements: Vec<super::buff_reqs::BuffSkillRequirement>,
    pub buff_game_data: BuffGameData,
    /// `TagsGameData` npc tags.
    pub npc_tags: HashMap<u32, Vec<u32>>,
    /// `ZoneGroup.BuffId`.
    pub zone_group_buffs: HashMap<u32, u32>,
    /// Blessings sold through the 1.2 priest interaction.
    pub priest_buffs: HashMap<u32, super::priest::PriestBuff>,
    /// `content_configs` id 39 (`ability_change_cost`): copper per character level charged by
    /// the 1.2 ability change UI (`X2Ability.GetAbilityChangeCost`). 0 when the row is absent.
    pub ability_change_cost: i32,
    /// `resurrection_waiting_times` by death step (see [`ResurrectionWaitingTime`]).
    pub resurrection_waiting_times: BTreeMap<u32, ResurrectionWaitingTime>,
    /// `blocked_child_doodads` (see [`BlockedChildDoodads`]).
    pub blocked_child_doodads: BlockedChildDoodads,
}

/// `blocked_child_doodads`: while a buff (`owner_type = 'Buff'`, `owner_id` = buff id) is active
/// on a child doodad's parent unit, a player cannot use the child doodad template
/// (`doodad_id`). The 1.2 client loads the same table (`dll.c` strings `39a0c158` / `39a0c460`:
/// `SELECT owner_type, owner_id, doodad_id FROM blocked_child_doodads`); the two rows are buff
/// 6064 -> doodad 6560 and buff 6245 -> doodad 3446.
#[derive(Debug, Default)]
pub struct BlockedChildDoodads {
    /// Buff id -> child doodad template ids (each doodad id once, sorted).
    by_buff: HashMap<u32, Vec<u32>>,
}

impl BlockedChildDoodads {
    /// The child doodad template ids blocked while `buff_id` is active (empty when none).
    pub fn doodads(&self, buff_id: u32) -> &[u32] {
        self.by_buff.get(&buff_id).map_or(&[], Vec::as_slice)
    }

    /// Whether `buff_id` blocks the child doodad template `doodad_id`.
    pub fn blocks(&self, buff_id: u32, doodad_id: u32) -> bool {
        self.doodads(buff_id).contains(&doodad_id)
    }

    /// Adds one row (duplicate buff/doodad pairs are kept once).
    pub fn add(&mut self, buff_id: u32, doodad_id: u32) {
        let doodads = self.by_buff.entry(buff_id).or_default();
        if !doodads.contains(&doodad_id) {
            doodads.push(doodad_id);
        }
    }

    /// Number of buffs that block at least one child doodad.
    pub fn len(&self) -> usize {
        self.by_buff.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_buff.is_empty()
    }
}

fn dup(what: &str, id: u32) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(format!("duplicate {what} {id} (Dictionary.Add)"))
}

fn missing(what: &str, id: u32) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(format!("{what} {id} not found (KeyNotFound)"))
}

impl Data {
    /// Loads every table (`PlotManager` and `AnimationManager` first, as the C# skill loader
    /// resolves `Plot` / `FireAnim` while reading the skills).
    pub fn load(ctx: &DataLoadCtx<'_>) -> Result<Self, crate::data::GameDataError> {
        Ok(Self::load_from(ctx.conn, ctx.files)?)
    }

    pub fn load_from(
        conn: &Connection,
        files: &crate::data::client_files::ClientFileManager,
    ) -> rusqlite::Result<Self> {
        let mut d = Data {
            plots: PlotData::load(conn)?,
            anims: AnimData::load(conn, files)?,
            zone_rules: zone_rules::ZoneRules::load(conn)?,
            ..Data::default()
        };
        // skills
        {
            let plots = &d.plots;
            let anims = &d.anims;
            let skills = templates::load_skills(conn, &|p| plots.plot(p).is_some(), &|a| {
                anims.get(a).is_some()
            })?;
            for s in skills {
                if d.skills.contains_key(&s.id) {
                    return Err(dup("skill", s.id));
                }
                d.skills.insert(s.id, Arc::new(s));
            }
        }
        // buffs (+ buff_effects / tick effects / unit modifiers / dynamic modifiers)
        let mut buffs: HashMap<u32, BuffTemplate> = HashMap::new();
        for b in templates::load_buffs(conn)? {
            if buffs.contains_key(&b.id) {
                return Err(dup("buff", b.id));
            }
            buffs.insert(b.id, b);
        }
        for_each_row(conn, "SELECT * FROM buff_tick_effects", [], |r| {
            let buff_id = r.u32("buff_id")?;
            let t = buffs
                .get_mut(&buff_id)
                .ok_or_else(|| missing("buff", buff_id))?;
            t.tick_effects.push(TickEffect {
                effect_id: r.u32("effect_id")?,
                target_buff_tag_id: r.u32_or("target_buff_tag_id", 0)?,
                target_no_buff_tag_id: r.u32_or("target_nobuff_tag_id", 0)?,
            });
            Ok(())
        })?;
        for_each_row(
            conn,
            "SELECT * FROM unit_modifiers WHERE owner_type='Buff'",
            [],
            |r| {
                let buff_id = r.u32("owner_id")?;
                if let Some(t) = buffs.get_mut(&buff_id) {
                    if let Some(b) = templates::read_bonus(r)? {
                        t.bonuses.push(b);
                    }
                }
                Ok(())
            },
        )?;
        for_each_row(conn, "SELECT * FROM linear_funcs", [], |r| {
            let f = LinearFuncTemplate {
                id: r.u32("id")?,
                start_value: r.i32("start_value")?,
                end_value: r.i32("end_value")?,
            };
            d.linear_funcs.insert(f.id, f);
            Ok(())
        })?;
        d.manual_funcs = misc::load_manual_funcs(conn)?;
        for_each_row(conn, "SELECT * FROM dynamic_unit_modifiers", [], |r| {
            let buff_id = r.u32("buff_id")?;
            if let Some(t) = buffs.get_mut(&buff_id) {
                if let Some(attribute) = templates::unit_attribute(r.u8("unit_attribute_id")?) {
                    t.dynamic_bonuses.push(DynamicBonusTemplate {
                        attribute,
                        modifier_type: UnitModifierType::from_u8(r.u8("unit_modifier_type_id")?),
                        func_id: r.u32("func_id")?,
                        func_type: r.string("func_type")?,
                    });
                }
            }
            Ok(())
        })?;
        effects::load_effect_tables(conn, &|id| buffs.contains_key(&id), &mut d.effects)?;
        // Buffs whose SkillControllerId has no `skill_controllers` row (a gap of the 1.2 data;
        // `BuffTemplate.Start` skips the controller for them).
        {
            let controllers = d.effects.get(&EffectKind::SkillController);
            let mut absent: Vec<(u32, u32)> = buffs
                .values()
                .filter(|b| b.skill_controller_id > 0)
                .filter(|b| !controllers.is_some_and(|c| c.contains_key(&b.skill_controller_id)))
                .map(|b| (b.id, b.skill_controller_id))
                .collect();
            if !absent.is_empty() {
                absent.sort_unstable();
                let mut ids: Vec<u32> = absent.iter().map(|(_, sc)| *sc).collect();
                ids.sort_unstable();
                ids.dedup();
                tracing::warn!(
                    buffs = absent.len(),
                    controllers = ids.len(),
                    sample = ?&absent[..absent.len().min(5)],
                    "buffs reference skill controllers absent from the static data; their controller is skipped"
                );
            }
        }
        d.effect_types = effects::load_effect_types(conn)?;
        // skill_effects
        {
            let effect_types = &d.effect_types;
            let all_effects = &d.effects;
            let skills = &mut d.skills;
            for_each_row(conn, "SELECT * FROM skill_effects", [], |r| {
                let skill_id = r.u32("skill_id")?;
                if !skills.contains_key(&skill_id) {
                    return Ok(());
                }
                let effect_id = r.u32("effect_id")?;
                let ty = effect_types
                    .get(&effect_id)
                    .ok_or_else(|| missing("effect", effect_id))?;
                let template = match EffectKind::from_name(&ty.actual_type) {
                    Some(kind) => {
                        if !all_effects
                            .get(&kind)
                            .is_some_and(|m| m.contains_key(&ty.actual_id))
                        {
                            return Err(missing(
                                &format!("{} effect", ty.actual_type),
                                ty.actual_id,
                            ));
                        }
                        Some((kind, ty.actual_id))
                    }
                    None => None,
                };
                let e = SkillEffect {
                    effect_id,
                    template,
                    weight: r.i32("weight")?,
                    start_level: r.u8("start_level")?,
                    end_level: r.u8("end_level")?,
                    friendly: r.bool_str("friendly")?,
                    non_friendly: r.bool_str("non_friendly")?,
                    target_buff_tag_id: r.u32_or("target_buff_tag_id", 0)?,
                    target_no_buff_tag_id: r.u32_or("target_nobuff_tag_id", 0)?,
                    source_buff_tag_id: r.u32_or("source_buff_tag_id", 0)?,
                    source_no_buff_tag_id: r.u32_or("source_nobuff_tag_id", 0)?,
                    chance: r.i32("chance")?,
                    front: r.bool_str("front")?,
                    back: r.bool_str("back")?,
                    target_npc_tag_id: r.u32_or("target_npc_tag_id", 0)?,
                    application_method: r.u32("application_method_id")?,
                    consume_source_item: r.bool_str("consume_source_item")?,
                    consume_item_id: r.u32_or("consume_item_id", 0)?,
                    consume_item_count: r.i32("consume_item_count")?,
                    always_hit: r.bool_str("always_hit")?,
                    item_set_id: r.u32_or("item_set_id", 0)?,
                    interaction_success_hit: r.bool_str("interaction_success_hit")?,
                };
                if let Some(s) = skills.get_mut(&skill_id) {
                    Arc::make_mut(s).effects.push(e);
                }
                Ok(())
            })?;
        }
        for_each_row(conn, "SELECT * FROM tagged_buffs", [], |r| {
            let tag = r.u32("tag_id")?;
            let buff = r.u32("buff_id")?;
            d.buff_tags.entry(buff).or_default().push(tag);
            d.tagged_buffs.entry(tag).or_default().push(buff);
            Ok(())
        })?;
        d.buff_breakers = load_buff_breakers(conn)?;
        d.buff_breakers.retain(|id, _| buffs.contains_key(id));
        for_each_row(conn, "SELECT * FROM skill_modifiers", [], |r| {
            let attr = r.u32("skill_attribute_id")? as u8;
            let Some(skill_attribute) = SkillAttribute::from_u8(attr) else {
                tracing::warn!("skill_modifiers: unknown skill attribute {attr}");
                return Ok(());
            };
            let m = SkillModifier {
                id: r.u32("id")?,
                owner_id: r.u32("owner_id")?,
                tag_id: r.u32_or("tag_id", 0)?,
                skill_attribute,
                unit_modifier_type: UnitModifierType::from_u8(r.u32("unit_modifier_type_id")? as u8),
                value: r.i32("value")?,
                skill_id: r.u32_or("skill_id", 0)?,
                synergy: r.bool_str("synergy")?,
            };
            d.skill_modifiers.entry(m.owner_id).or_default().push(m);
            Ok(())
        })?;
        for_each_row(conn, "SELECT * FROM tagged_skills", [], |r| {
            let tag = r.u32("tag_id")?;
            let skill = r.u32("skill_id")?;
            d.skill_tags.entry(skill).or_default().push(tag);
            d.tagged_skills.entry(tag).or_default().push(skill);
            Ok(())
        })?;
        for_each_row(conn, "SELECT * FROM combat_buffs", [], |r| {
            let c = CombatBuffTemplate {
                id: r.u32("id")?,
                hit_skill_id: r.u32_or("hit_skill_id", 0)?,
                hit_type: r.u32("hit_type_id")? as u8,
                req_skill_id: r.u32_or("req_skill_id", 0)?,
                buff_id: r.u32("buff_id")?,
                buff_from_source: r.bool_str("buff_from_source")?,
                buff_to_source: r.bool_str("buff_to_source")?,
                req_buff_id: r.u32("req_buff_id")?,
                is_heal_spell: r.bool_str("is_heal_spell")?,
            };
            d.combat_buffs.entry(c.req_buff_id).or_default().push(c);
            Ok(())
        })?;
        {
            let effect_types = &d.effect_types;
            let all_effects = &d.effects;
            let triggers = &mut d.buff_triggers;
            for_each_row(conn, "SELECT * FROM buff_triggers", [], |r| {
                let buff_id = r.u32("buff_id")?;
                let list = triggers.entry(buff_id).or_default();
                let effect_id = r.u32("effect_id")?;
                // GetEffectTemplate(id): unknown type id -> null; unknown actual id -> throws.
                let effect = match effect_types.get(&effect_id) {
                    None => None,
                    Some(ty) => match EffectKind::from_name(&ty.actual_type) {
                        None => None,
                        Some(kind) => {
                            if !all_effects
                                .get(&kind)
                                .is_some_and(|m| m.contains_key(&ty.actual_id))
                            {
                                return Err(missing(
                                    &format!("{} effect", ty.actual_type),
                                    ty.actual_id,
                                ));
                            }
                            Some((kind, ty.actual_id))
                        }
                    },
                };
                if let Some(effect) = effect {
                    list.push(BuffTriggerTemplate {
                        id: r.u32("id")?,
                        kind: r.i64("event_id")? as u16,
                        effect_on_source: r.bool_str("effect_on_source")?,
                        effect,
                        use_damage_amount: r.bool_str("use_damage_amount")?,
                        use_original_source: r.bool_str("use_original_source")?,
                        target_buff_tag_id: r.u32_or("target_buff_tag_id", 0)?,
                        target_no_buff_tag_id: r.u32_or("target_no_buff_tag_id", 0)?,
                        synergy: r.bool_str("synergy")?,
                    });
                }
                Ok(())
            })?;
        }
        let mut reagent_ids = std::collections::HashSet::new();
        for_each_row(conn, "SELECT * from skill_reagents", [], |r| {
            let t = SkillItemAmount {
                id: r.u32("id")?,
                skill_id: r.u32("skill_id")?,
                item_id: r.u32("item_id")?,
                amount: r.i16("amount")?,
            };
            if !reagent_ids.insert(t.id) {
                return Err(dup("skill reagent", t.id));
            }
            d.skill_reagents.push(t);
            Ok(())
        })?;
        let mut product_ids = std::collections::HashSet::new();
        for_each_row(conn, "SELECT * from skill_products", [], |r| {
            let t = SkillItemAmount {
                id: r.u32("id")?,
                skill_id: r.u32("skill_id")?,
                item_id: r.u32("item_id")?,
                amount: r.i16("amount")?,
            };
            if !product_ids.insert(t.id) {
                return Err(dup("skill product", t.id));
            }
            d.skill_products.push(t);
            Ok(())
        })?;
        for_each_row(
            conn,
            "SELECT * FROM unit_modifiers WHERE owner_type='Item'",
            [],
            |r| {
                if let Some(b) = templates::read_bonus(r)? {
                    d.item_unit_modifiers
                        .entry(r.u32("owner_id")?)
                        .or_default()
                        .push(b);
                }
                Ok(())
            },
        )?;
        super::aura::warn_catalog(&buffs);
        // `transform_buff_id` replaces the buff's stacks when they reach `max_stack`. A row
        // whose target is absent keeps today's refresh-at-cap behaviour; warn once for the
        // whole catalog.
        {
            let mut absent: Vec<(u32, u32)> = buffs
                .values()
                .filter(|b| b.transform_buff_id != 0 && !buffs.contains_key(&b.transform_buff_id))
                .map(|b| (b.id, b.transform_buff_id))
                .collect();
            if !absent.is_empty() {
                absent.sort_unstable();
                for (id, _) in &absent {
                    if let Some(b) = buffs.get_mut(id) {
                        b.transform_buff_id = 0;
                    }
                }
                tracing::warn!(
                    buffs = absent.len(),
                    sample = ?&absent[..absent.len().min(5)],
                    "buffs reference transform buffs absent from the static data; their transform is skipped"
                );
            }
        }
        d.buffs = buffs.into_iter().map(|(k, v)| (k, Arc::new(v))).collect();
        d.area_shapes = misc::load_area_shapes(conn)?;
        d.model_radii = misc::load_actor_model_radii(conn)?;
        d.unit_reqs = misc::load_unit_reqs(conn)?;
        d.buff_skill_requirements = super::buff_reqs::load(conn)?;
        d.buff_game_data = BuffGameData::load(conn)?;
        d.npc_tags = misc::load_npc_tags(conn)?;
        d.zone_group_buffs = misc::load_zone_group_buffs(conn)?;
        d.priest_buffs = super::priest::load(conn)?;
        d.ability_change_cost = misc::load_content_config(conn, 39)?;
        d.resurrection_waiting_times = load_resurrection_waiting_times(conn)?;
        d.blocked_child_doodads = load_blocked_child_doodads(conn)?;
        d.range_penalty_skills = load_range_penalty_skills(conn);
        Ok(d)
    }

    // ---------------------------------------------------------------- SkillManager getters

    /// `GetSkillTemplate(id)`.
    pub fn skill(&self, id: u32) -> Option<&Arc<SkillTemplate>> {
        self.skills.get(&id)
    }

    /// `GetBuffTemplate(id)`.
    pub fn buff(&self, id: u32) -> Option<&Arc<BuffTemplate>> {
        self.buffs.get(&id)
    }

    /// `resurrection_waiting_times` row for a 1-based consecutive-death step. The real rows
    /// are contiguous; a gap falls back to the highest lower row (below the lowest row, to the
    /// first one), and a step past the last row uses the last row.
    pub fn resurrection_waiting_time(&self, step: u32) -> Option<&ResurrectionWaitingTime> {
        let step = step.max(1);
        self.resurrection_waiting_times
            .get(&step)
            .or_else(|| {
                self.resurrection_waiting_times
                    .range(..=step)
                    .next_back()
                    .map(|(_, row)| row)
            })
            .or_else(|| self.resurrection_waiting_times.values().next())
    }

    /// `GetEffectTemplate(id, type)`.
    pub fn effect(&self, kind: EffectKind, id: u32) -> Option<&EffectTemplate> {
        self.effects.get(&kind)?.get(&id)
    }

    /// `GetEffectTemplate(id, typeName)`.
    pub fn effect_by_name(&self, id: u32, type_name: &str) -> Option<&EffectTemplate> {
        self.effect(EffectKind::from_name(type_name)?, id)
    }

    /// `GetEffectTemplate(id)` (through the effects table).
    pub fn effect_by_type_id(&self, id: u32) -> Option<(EffectKind, &EffectTemplate)> {
        let ty = self.effect_types.get(&id)?;
        let kind = EffectKind::from_name(&ty.actual_type)?;
        Some((kind, self.effect(kind, ty.actual_id)?))
    }

    /// `GetBuffTags(buffId)`.
    pub fn buff_tags(&self, buff_id: u32) -> &[u32] {
        self.buff_tags.get(&buff_id).map_or(&[], Vec::as_slice)
    }

    /// `GetBuffsByTagId(tagId)` (`None` = null in C#).
    pub fn buffs_by_tag(&self, tag_id: u32) -> Option<&[u32]> {
        self.tagged_buffs.get(&tag_id).map(Vec::as_slice)
    }

    /// `GetSkillTags(skillId)`.
    pub fn skill_tags(&self, skill_id: u32) -> &[u32] {
        self.skill_tags.get(&skill_id).map_or(&[], Vec::as_slice)
    }

    /// `GetSkillsByTag(tagId)`.
    pub fn skills_by_tag(&self, tag_id: u32) -> &[u32] {
        self.tagged_skills.get(&tag_id).map_or(&[], Vec::as_slice)
    }

    /// `GetModifiersByOwnerId(id)`.
    pub fn skill_modifiers_by_owner(&self, id: u32) -> &[SkillModifier] {
        self.skill_modifiers.get(&id).map_or(&[], Vec::as_slice)
    }

    /// `BuffGameData.GetModifiersForBuff(ownerId)`.
    pub fn buff_modifiers_for_buff(&self, id: u32) -> &[BuffModifier] {
        self.buff_game_data
            .buff_modifiers
            .get(&id)
            .map_or(&[], Vec::as_slice)
    }

    /// `GetCombatBuffs(reqBuffId)`.
    pub fn combat_buffs_for(&self, req_buff_id: u32) -> &[CombatBuffTemplate] {
        self.combat_buffs
            .get(&req_buff_id)
            .map_or(&[], Vec::as_slice)
    }

    /// `GetBuffTriggerTemplates(buffId)`.
    pub fn buff_triggers_for(&self, buff_id: u32) -> &[BuffTriggerTemplate] {
        self.buff_triggers.get(&buff_id).map_or(&[], Vec::as_slice)
    }

    /// `GetLinearFunc(id)`.
    pub fn linear_func(&self, id: u32) -> Option<LinearFuncTemplate> {
        self.linear_funcs.get(&id).copied()
    }

    /// `GetSkillReagentsBySkillId(id)`.
    pub fn reagents_for(&self, skill_id: u32) -> Vec<SkillItemAmount> {
        self.skill_reagents
            .iter()
            .filter(|r| r.skill_id == skill_id)
            .copied()
            .collect()
    }

    /// `GetSkillProductsBySkillId(id)`.
    pub fn products_for(&self, skill_id: u32) -> Vec<SkillItemAmount> {
        self.skill_products
            .iter()
            .filter(|r| r.skill_id == skill_id)
            .copied()
            .collect()
    }

    /// `GetSkillControllerTemplate` (`GetEffectTemplate(id, "SkillController")`).
    pub fn skill_controller(&self, id: u32) -> Option<&SkillControllerTemplate> {
        match self.effect(EffectKind::SkillController, id)? {
            EffectTemplate::SkillController(t) => Some(t),
            _ => None,
        }
    }

    /// `UnitRequirementsGameData.GetSkillRequirements(skillId)`.
    pub fn skill_requirements(&self, skill_id: u32) -> Vec<&UnitReq> {
        self.unit_reqs
            .get("Skill")
            .map(|l| l.iter().filter(|u| u.owner_id == skill_id).collect())
            .unwrap_or_default()
    }

    /// `BuffGameData.GetBuffToleranceForBuffTag(tag)`.
    pub fn tolerance_for_tag(&self, tag: u32) -> Option<&BuffTolerance> {
        self.buff_game_data.tolerances.get(&tag)
    }

    /// `ModelManager.GetActorModel(modelId)?.Radius ?? 0`.
    pub fn model_radius(&self, model_id: u32) -> f32 {
        self.model_radii.get(&model_id).copied().unwrap_or(0.0)
    }

    /// `TagsGameData.GetIdsByTagId(Npcs, tag)`.
    pub fn npcs_by_tag(&self, tag: u32) -> &[u32] {
        self.npc_tags.get(&tag).map_or(&[], Vec::as_slice)
    }

    /// `SkillManager.GetNpSkillTemplate(npcSkill)`: the template of an NPC skill, `None` for an
    /// unknown skill or (the dungeon `InCombat` filter, `SkillUseConditionKind.InCombat = 0`) a
    /// skill that respects the global cooldown or has a plot.
    pub fn np_skill(&self, skill_id: u32, use_condition: i32) -> Option<&Arc<SkillTemplate>> {
        let t = self.skill(skill_id);
        if use_condition == 0 {
            if let Some(t) = t {
                if !t.ignore_global_cooldown || t.plot_id.is_some() {
                    return None;
                }
            }
        }
        t
    }

    /// `SkillManager.GetSkillActAbility(skillId)` (`ActabilityType`, 0 = None).
    pub fn skill_actability(&self, skill_id: u32) -> i32 {
        self.skill(skill_id).map_or(0, |t| t.actability_group_id)
    }

    /// `SkillManager.GetSpawnGimmickEffect(gimmickTemplateId)`: the first spawn effect of a
    /// gimmick (dictionary order: by effect id here).
    pub fn spawn_gimmick_effect(
        &self,
        gimmick_template_id: u32,
    ) -> Option<&effects::SpawnGimmickEffect> {
        let map = self.effects.get(&EffectKind::SpawnGimmickEffect)?;
        let mut ids: Vec<&u32> = map.keys().collect();
        ids.sort();
        ids.into_iter().find_map(|id| match map.get(id) {
            Some(EffectTemplate::SpawnGimmick(e)) if e.gimmick_id == gimmick_template_id => Some(e),
            _ => None,
        })
    }

    /// `IsDefaultSkill` / `IsCommonSkill` live in `data::skills::SkillData` (phase 2).
    pub fn is_loaded(&self) -> bool {
        !self.skills.is_empty()
    }

    /// The ids of all skill templates (tests / synthetic data).
    pub fn skill_ids(&self) -> Vec<u32> {
        self.skills.keys().copied().collect()
    }

    /// Adds a skill template (tests / synthetic data).
    pub fn add_skill(&mut self, t: SkillTemplate) {
        self.skills.insert(t.id, Arc::new(t));
    }

    /// Adds a buff template (tests / synthetic data).
    pub fn add_buff(&mut self, t: BuffTemplate) {
        self.buffs.insert(t.id, Arc::new(t));
    }

    /// Adds an effect template (tests / synthetic data); also registers an `effects` row with
    /// id `type_id` pointing at it when `type_id != 0`.
    pub fn add_effect(&mut self, kind: EffectKind, t: EffectTemplate, type_id: u32) {
        let id = t.id();
        self.effects.entry(kind).or_default().insert(id, t);
        if type_id != 0 {
            self.effect_types.insert(
                type_id,
                EffectType {
                    id: type_id,
                    actual_id: id,
                    actual_type: kind.name().to_string(),
                },
            );
        }
    }
}

/// `SELECT id, penalty_duration, waiting_time, siege_waiting_time FROM resurrection_waiting_times`
/// (the 10.x server query adds `conquest_waiting_time` and `special_waiting_time`). A row with
/// negative times or a duplicate id is skipped with a warning so one bad row does not abort the
/// whole catalog.
pub(super) fn load_resurrection_waiting_times(
    conn: &Connection,
) -> rusqlite::Result<BTreeMap<u32, ResurrectionWaitingTime>> {
    let mut rows = BTreeMap::new();
    for_each_row(conn, "SELECT * FROM resurrection_waiting_times", [], |r| {
        let row = ResurrectionWaitingTime {
            id: r.u32("id")?,
            penalty_duration: r.i32("penalty_duration")?,
            waiting_time: r.i32("waiting_time")?,
            siege_waiting_time: r.i32("siege_waiting_time")?,
        };
        if row.id == 0
            || row.penalty_duration < 0
            || row.waiting_time < 0
            || row.siege_waiting_time < 0
        {
            tracing::warn!("resurrection_waiting_times row {row:?} is invalid; skipped");
            return Ok(());
        }
        if rows.insert(row.id, row).is_some() {
            tracing::warn!(
                "resurrection_waiting_times id {} appears twice; the last row wins",
                row.id
            );
        }
        Ok(())
    })?;
    Ok(rows)
}

/// `SELECT owner_type, owner_id, doodad_id FROM blocked_child_doodads` (the 1.2 client loads
/// the same table, `dll.c` strings `39a0c158` / `39a0c460`). Only the evidenced owner type
/// `Buff` is handled; other / unknown owner types are skipped at debug. NULL columns and zero
/// ids are skipped so one bad row cannot abort the catalog.
pub(super) fn load_blocked_child_doodads(
    conn: &Connection,
) -> rusqlite::Result<BlockedChildDoodads> {
    let mut blocked = BlockedChildDoodads::default();
    for_each_row(conn, "SELECT * FROM blocked_child_doodads", [], |r| {
        let owner_type = r.string_or("owner_type", "")?;
        let owner_id = r.u32_or("owner_id", 0)?;
        let doodad_id = r.u32_or("doodad_id", 0)?;
        if owner_id == 0 || doodad_id == 0 {
            tracing::debug!(
                "blocked_child_doodads row with NULL/zero owner_id or doodad_id is skipped"
            );
            return Ok(());
        }
        if !owner_type.eq_ignore_ascii_case("Buff") {
            tracing::debug!(
                "blocked_child_doodads owner_type {owner_type:?} (owner {owner_id}) is not handled; skipped"
            );
            return Ok(());
        }
        blocked.add(owner_id, doodad_id);
        Ok(())
    })?;
    for doodads in blocked.by_buff.values_mut() {
        doodads.sort_unstable();
    }
    Ok(blocked)
}

/// Skill descriptions whose 1.2 tooltip names the close-range damage penalty of `formulas`
/// row 12 (`damage_multiplier_by_range`, `if_negative(range - 8, 0.8, 1)` = x0.8 under 8 m).
///
/// The 1.2 `damage_effects` table has no flag that scopes the row, but the client tooltips
/// name the skills: "Damage ... reduced/decreases -20% if [the] target is within 8m"
/// (Endless Arrows, Charged Bolt, Piercing Shot, Toxic Shot, Venom Arrow, Shoot Arrow, Mark,
/// Stalker's Mark; `D:/aaemu-audits/gap-audit-2026-11-10/verify-claims-10x.md` 11a). The set
/// is built from the en_us descriptions at load instead of a hard-coded id list, so a data
/// change is followed; the count and the ids are logged once.
pub(super) fn load_range_penalty_skills(conn: &Connection) -> HashSet<u32> {
    let mut skills = HashSet::new();
    let sql = "SELECT idx, en_us FROM localized_texts \
               WHERE tbl_name = 'skills' AND tbl_column_name = 'desc' \
                 AND en_us LIKE '%20%%' AND en_us LIKE '%within%8m%'";
    let loaded = for_each_row(conn, sql, [], |r| {
        let idx = r.i64("idx")?;
        let normalized = strip_tooltip_markup(&r.string_or("en_us", "")?).to_lowercase();
        if normalized.contains("20%")
            && normalized.contains("within 8m")
            && (normalized.contains("reduc") || normalized.contains("decreas"))
        {
            if let Ok(id) = u32::try_from(idx) {
                skills.insert(id);
            }
        }
        Ok(())
    });
    if let Err(e) = loaded {
        // An older compact without the table / column only narrows the set.
        tracing::warn!("range penalty skill descriptions not loaded: {e}");
        return HashSet::new();
    }
    let mut ids: Vec<u32> = skills.iter().copied().collect();
    ids.sort_unstable();
    tracing::info!(
        count = ids.len(),
        skills = ?ids,
        "skills with the close-range (-20% within 8 m) damage penalty"
    );
    skills
}

/// Removes the 1.2 client tooltip colour markup (`|cAARRGGBB` ... `|r`) so the text can be
/// matched without depending on the colour codes the data puts around "-20%" and "8m".
fn strip_tooltip_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('|') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let bytes = after.as_bytes();
        if let Some(after) = after.strip_prefix('r') {
            rest = after;
        } else if bytes.len() >= 9
            && bytes[0] == b'c'
            && bytes[1..9].iter().all(u8::is_ascii_hexdigit)
        {
            rest = &after[9..];
        } else {
            out.push('|');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// The 1.2 loader (`395D17B0`) attaches each breaker tag to its `buff_id`, rather than
/// treating `buff_id` as the buff to apply. The 10.x loader (`39B41DE0`) uses a set.
pub(super) fn load_buff_breakers(conn: &Connection) -> rusqlite::Result<HashMap<u32, Vec<u32>>> {
    let mut breakers: HashMap<u32, Vec<u32>> = HashMap::new();
    for_each_row(
        conn,
        "SELECT buff_id, buff_tag_id FROM buff_breakers",
        [],
        |r| {
            breakers
                .entry(r.u32("buff_id")?)
                .or_default()
                .push(r.u32("buff_tag_id")?);
            Ok(())
        },
    )?;
    for tags in breakers.values_mut() {
        tags.sort_unstable();
        tags.dedup();
    }
    Ok(breakers)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loader handles NULL columns and only the evidenced `Buff` owner type.
    #[test]
    fn blocked_child_doodads_are_null_tolerant_and_buff_only() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE blocked_child_doodads (id INT, owner_id INT, owner_type TEXT, doodad_id INT); \
             INSERT INTO blocked_child_doodads VALUES \
               (1, 6064, 'Buff', 6560), \
               (2, 6245, 'Buff', 3446), \
               (3, NULL, 'Buff', 6560), \
               (4, 6064, NULL, 6560), \
               (5, 6064, 'Buff', NULL), \
               (6, 7000, 'Slave', 6560);",
        )
        .unwrap();
        let blocked = load_blocked_child_doodads(&c).unwrap();
        assert_eq!(blocked.len(), 2);
        assert!(blocked.blocks(6064, 6560));
        assert!(blocked.blocks(6245, 3446));
        assert_eq!(blocked.doodads(6064), &[6560]);
        assert!(!blocked.blocks(7000, 6560), "only Buff rows are handled");
        assert!(!blocked.blocks(6064, 3446));
    }

    /// The two real rows: buff 6064 -> doodad 6560 (fishing boat crane), buff 6245 -> doodad
    /// 3446 (backpack storage box).
    #[test]
    #[ignore = "needs AAEMU_COMPACT_DB"]
    fn blocked_child_doodads_real_compact_db() {
        let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
            return;
        };
        let conn =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let blocked = load_blocked_child_doodads(&conn).unwrap();
        assert_eq!(blocked.len(), 2);
        assert_eq!(blocked.doodads(6064), &[6560]);
        assert_eq!(blocked.doodads(6245), &[3446]);
    }

    /// Loads every table of the real 1.2 compact.sqlite3 and reports row counts
    /// (`AAEMU_COMPACT_DB=... cargo test -p aaemu-game skills_real_compact_db -- --ignored`).
    #[test]
    #[ignore = "needs AAEMU_COMPACT_DB"]
    fn skills_real_compact_db() {
        let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
            return;
        };
        let conn =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut files = crate::data::client_files::ClientFileManager::new();
        if let Ok(pak) = std::env::var("AAEMU_GAME_PAK") {
            files.add_source(&pak, std::path::Path::new("."));
        }
        let d = Data::load_from(&conn, &files).unwrap();
        assert_eq!(d.manual_funcs.len(), 3);
        assert_eq!(d.manual_funcs[&11], vec![-1000, -1000, -700, -350]);
        assert_eq!(d.manual_funcs[&15], vec![500, 150]);
        let effect_counts: Vec<String> = EffectKind::ALL
            .iter()
            .map(|k| format!("{}={}", k.name(), d.effects.get(k).map_or(0, |m| m.len())))
            .collect();
        let skill_effects: usize = d.skills.values().map(|s| s.effects.len()).sum();
        let tick_effects: usize = d.buffs.values().map(|b| b.tick_effects.len()).sum();
        let bonuses: usize = d.buffs.values().map(|b| b.bonuses.len()).sum();
        let dyn_bonuses: usize = d.buffs.values().map(|b| b.dynamic_bonuses.len()).sum();
        let triggers: usize = d.buff_triggers.values().map(Vec::len).sum();
        let anims_with_sync = d.anims.anims.values().filter(|a| a.duration > 0).count();
        eprintln!(
            "skills {} skill_effects {} buffs {} tick_effects {} buff bonuses {} dynamic {} \
             effect types {} buff tags {} tagged skills {} skill modifiers {} buff modifiers {} \
             tolerances {} combat buffs {} triggers {} reagents {} products {} linear funcs {} \
             item modifiers {} plots {} plot events {} plot conditions {} trees {} anims {} \
             (with client timing {}) aoe shapes {} model radii {} unit reqs {} npc tags {} \
             zone group buffs {}\n{}",
            d.skills.len(),
            skill_effects,
            d.buffs.len(),
            tick_effects,
            bonuses,
            dyn_bonuses,
            d.effect_types.len(),
            d.tagged_buffs.len(),
            d.tagged_skills.len(),
            d.skill_modifiers.values().map(Vec::len).sum::<usize>(),
            d.buff_game_data
                .buff_modifiers
                .values()
                .map(Vec::len)
                .sum::<usize>(),
            d.buff_game_data.tolerances.len(),
            d.combat_buffs.values().map(Vec::len).sum::<usize>(),
            triggers,
            d.skill_reagents.len(),
            d.skill_products.len(),
            d.linear_funcs.len(),
            d.item_unit_modifiers.values().map(Vec::len).sum::<usize>(),
            d.plots.plots.len(),
            d.plots.events.len(),
            d.plots.conditions.len(),
            d.plots.trees.len(),
            d.anims.anims.len(),
            anims_with_sync,
            d.area_shapes.len(),
            d.model_radii.len(),
            d.unit_reqs.values().map(Vec::len).sum::<usize>(),
            d.npc_tags.len(),
            d.zone_group_buffs.len(),
            effect_counts.join(" "),
        );
        assert!(d.skills.len() > 10_000);
        assert!(d.buffs.len() > 5_000);
        assert!(d.plots.trees.len() > 1_000);
        // `content_configs` 39 (`X2Ability.GetAbilityChangeCost` reads it as `0x27`).
        assert_eq!(d.ability_change_cost, 200);
        // `resurrection_waiting_times`: ten 1-based death steps, 600 s penalty window, normal
        // waits 0/5/45/90/180 s and siege waits 20/15/10/5/0 s.
        assert_eq!(d.resurrection_waiting_times.len(), 10);
        assert!(d
            .resurrection_waiting_times
            .values()
            .all(|r| r.penalty_duration == 600));
        assert_eq!(
            d.resurrection_waiting_time(1)
                .map(|r| (r.waiting_time, r.siege_waiting_time)),
            Some((0, 20))
        );
        assert_eq!(
            d.resurrection_waiting_time(3)
                .map(|r| (r.waiting_time, r.siege_waiting_time)),
            Some((45, 10))
        );
        assert_eq!(
            d.resurrection_waiting_time(5)
                .map(|r| (r.waiting_time, r.siege_waiting_time)),
            Some((180, 0))
        );
        assert_eq!(d.resurrection_waiting_time(99).map(|r| r.id), Some(10));
        // Every skill effect / trigger points at a loaded template.
        for s in d.skills.values() {
            for e in &s.effects {
                if let Some((k, id)) = e.template {
                    assert!(d.effect(k, id).is_some());
                }
            }
        }
        // Melee auto attack exists.
        assert!(d.skill(2).is_some());
        for (column, loaded) in [
            (
                "source_alive",
                d.skills.values().filter(|s| s.source_alive).count(),
            ),
            (
                "check_obstacle",
                d.skills.values().filter(|s| s.check_obstacle).count(),
            ),
            (
                "knockback_immune",
                d.buffs.values().filter(|b| b.knockback_immune).count(),
            ),
        ] {
            let table = if column == "knockback_immune" {
                "buffs"
            } else {
                "skills"
            };
            let expected: usize = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE {column}='t'"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(expected > 0);
            assert_eq!(loaded, expected, "{column}");
        }
        for id in [3, 4, 5] {
            assert!(matches!(
                d.effect(EffectKind::PlayLogEffect, id),
                Some(EffectTemplate::PlayLog { .. })
            ));
        }
    }

    /// The spec's transform rows: all 66 `transform_buff_id` buffs are stack rule 4 with
    /// `max_stack >= 2`, every target exists, and the two authored chains are present.
    #[test]
    #[ignore = "needs AAEMU_COMPACT_DB"]
    fn skills_real_compact_db_transform_buffs() {
        let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
            return;
        };
        let conn =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut files = crate::data::client_files::ClientFileManager::new();
        if let Ok(pak) = std::env::var("AAEMU_GAME_PAK") {
            files.add_source(&pak, std::path::Path::new("."));
        }
        let d = Data::load_from(&conn, &files).unwrap();
        let mut rows: Vec<(u32, u32)> = Vec::new();
        for_each_row(
            &conn,
            "SELECT id, transform_buff_id FROM buffs WHERE transform_buff_id != 0",
            [],
            |r| {
                rows.push((r.u32("id")?, r.u32("transform_buff_id")?));
                Ok(())
            },
        )
        .unwrap();
        rows.sort_unstable();
        assert_eq!(rows.len(), 66);
        for (id, transform_id) in rows {
            let t = d.buff(id).unwrap_or_else(|| panic!("buff {id} not loaded"));
            assert_eq!(t.transform_buff_id, transform_id, "buff {id}");
            assert_eq!(t.stack_rule, 4, "buff {id} stack rule");
            assert!(t.max_stack >= 2, "buff {id} max_stack {}", t.max_stack);
            assert!(
                d.buff(transform_id).is_some(),
                "buff {id} transform {transform_id} not loaded"
            );
        }
        for (a, b, c) in [(4152, 4752, 4101), (5378, 5379, 5380)] {
            assert_eq!(d.buff(a).unwrap().transform_buff_id, b);
            assert_eq!(d.buff(b).unwrap().transform_buff_id, c);
        }
    }
}
