//! `DoodadManager.Load` (`Core/Managers/UnitManagers/DoodadManager.cs`): doodad templates
//! (`doodad_almighties`, `doodad_groups`), func groups, funcs, phase funcs, every
//! `doodad_func_*` template table, the consume changer items and the template caches, plus the
//! read-only getters of `DoodadManager` (`GetTemplate`, `GetFunc`, `GetFuncsForGroup`,
//! `GetPhaseFunc`, `GetFuncTemplate`, `GetPhaseFuncTemplate`, `GetDoodadFuncGroups(Id)`,
//! `GetDoodadFuncs`, `GetDoodadPhaseFuncs`, `GetDoodadFuncConsumeChangerItemList`,
//! `GetTreasureChestTemplateIds`).
//!
//! C# behaviour mirrored:
//! * The first `doodad_func_groups` pass (`ORDER BY doodad_almighty_id, ...`) runs before any
//!   template exists, so it adds nothing (its column reads still run). The groups come from the
//!   second, unordered pass. The server retains the model for fish school phases so the
//!   fish finder can exclude invisible setup phases.
//! * `doodad_funcs` and `doodad_phase_funcs` keep their
//!   `ORDER BY doodad_func_group_id, actual_func_id` order (the C# order).
//!   **Deviation (2026-11-11):** the C# order runs a `DoodadFuncPulseTrigger` (274) before the
//!   same phase's `DoodadFuncPulse` (287) and stops the sequence, so Sharpwind's switches never
//!   pulsed their logic network. The executor (`phase::do_phase_funcs`) therefore runs the
//!   `DoodadFuncPulseTrigger` rows of a group after its other phase funcs; the loader order
//!   itself is unchanged. The 10.x zone loader (`LoadDoodadPhaseFuncDescs`, `sub_39BE6B30`)
//!   selects `doodad_phase_funcs` without an `ORDER BY`, i.e. in row (= authoring) order, which
//!   also runs the pulse first. `FuncGroups` of a template and `IsCofferTemplate` follow the
//!   insertion order of `_allFuncGroups` (plain `doodad_func_groups` row order).
//! * Template tables go to `_funcTemplates` or `_phaseFuncTemplates` by the base class of their
//!   C# type (dictionaries created by reflection). Item / child tables whose classes derive from
//!   `DoodadPhaseFuncTemplate` therefore live with the phase func templates:
//!   `DoodadFuncBuyFishItem`, `DoodadFuncBuyFishModel`, `DoodadFuncConsumeChangerModel(Item)`,
//!   `DoodadFuncConvertFishItem`, `DoodadFuncCraftStartCraft`, `DoodadFuncCoffer`, ...
//! * Duplicate ids in a template table, in `doodad_funcs` or `doodad_almighties` fail the load
//!   (`Dictionary.Add` throws); duplicate func groups are logged and skipped (`TryAdd` +
//!   `Logger.Fatal`); duplicate `doodad_groups` / consume changer items keep the first row.
//! * A `doodad_func_clout_effects` row for an unknown clout, or a template with an unknown
//!   `group_id`, fails the load.
//! * `CreateTemplateCaches` sums the `Delay` of `DoodadFuncGrowth` templates looked up through
//!   the *funcs* of each group (`GetFuncsForGroup`, not the phase funcs) with
//!   `GetPhaseFuncTemplate`; `TotalDoodadGrowthTime` falls back to `GrowthTime` when <= 0.
//! * `GetBoolean(col, true)` columns use the `"t"`/`"1"` string rule, `GetBoolean(col)` columns
//!   (`doodad_groups.is_export`, `removed_by_house`) are `GetInt64 != 0`.

mod loading;

#[cfg(test)]
use self::loading::load_templates;

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use super::templates::*;
use crate::data::sqlite::{for_each_row, SqlRow};
use crate::data::GameDataError;
use crate::systems::DataLoadCtx;

pub use super::templates::DoodadTemplate;

/// One template dictionary per C# class name (`Dictionary<string, Dictionary<uint, T>>`).
pub type FuncTemplateMap = HashMap<&'static str, HashMap<u32, FuncTemplate>>;

/// `DoodadManager` game data.
#[derive(Debug, Default)]
pub struct DoodadData {
    /// `_templates`.
    pub templates: HashMap<u32, DoodadTemplate>,
    /// `_templates` insertion order (`doodad_almighties` row order; `Values` enumeration order).
    pub template_order: Vec<u32>,
    /// `_allFuncGroups` in insertion order.
    pub all_func_groups: Vec<DoodadFuncGroups>,
    /// `_allFuncGroups` keys -> index in [`Self::all_func_groups`].
    pub all_func_groups_index: HashMap<u32, usize>,
    /// `_funcsByGroups`.
    pub funcs_by_groups: HashMap<u32, Vec<DoodadFunc>>,
    /// `_funcsById` (by `FuncKey`).
    pub funcs_by_id: HashMap<u32, DoodadFunc>,
    /// `_phaseFuncs`.
    pub phase_funcs: HashMap<u32, Vec<DoodadPhaseFunc>>,
    /// `_funcTemplates` (classes deriving from `DoodadFuncTemplate`).
    pub func_templates: FuncTemplateMap,
    /// `_phaseFuncTemplates` (classes deriving from `DoodadPhaseFuncTemplate`).
    pub phase_func_templates: FuncTemplateMap,
    /// `_doodadFuncConsumeChangerItem` values in insertion order (ids unique, first row wins).
    pub consume_changer_items: Vec<DoodadFuncConsumeChangerItem>,
    /// `_doodadGroups`.
    pub doodad_groups: HashMap<u32, DoodadGroups>,
    /// `doodad_place_skins` by doodad template id: the place kinds the 1.2 client's place-skin
    /// rule accepts (`doodad_place_skin_kind_id`; 2,361 rows on the 1.2 data). Duplicate rows are
    /// kept, like the client's vector.
    pub place_skins: HashMap<u32, Vec<u32>>,
}

fn data_err(msg: String) -> GameDataError {
    GameDataError::File(msg)
}

/// `DoodadFuncTod.TodAsHours` as computed by `DoodadManager.Load`.
pub fn tod_as_hours(id: u32, tod_value: u32) -> f32 {
    let mut tod = tod_value;
    // Correction for typos in the compact DB
    while tod >= 2400 {
        tod /= 10;
    }
    let hh = tod / 100;
    let mut mm = tod % 100;
    if mm >= 60 {
        tracing::warn!("DoodadFuncToD has invalid value for minutes, Id {id}, ToD {tod_value}");
    }
    mm %= 60;
    hh as f32 * 1f32 + mm as f32 / 60f32
}

impl DoodadData {
    /// `DoodadManager.Load`.
    pub fn load(ctx: &DataLoadCtx<'_>) -> Result<Self, GameDataError> {
        Self::load_from(ctx.conn)
    }

    /// First `doodad_func_groups` pass: `GetTemplate` finds nothing yet (`_templates` is empty),
    /// so only the column reads have an effect (errors).
    fn load_func_groups_discarded(&mut self, conn: &Connection) -> Result<(), GameDataError> {
        for_each_row(
            conn,
            "SELECT * FROM doodad_func_groups ORDER BY doodad_almighty_id, doodad_func_group_kind_id",
            [],
            |r| {
                let group = DoodadFuncGroups {
                    id: r.u32("id")?,
                    almighty: r.u32("doodad_almighty_id")?,
                    group_kind_id: r.u32("doodad_func_group_kind_id")?,
                    sound_id: r.u32_or("sound_id", 0)?,
                    phase_msg: r.string_or("phase_msg", "")?,
                    is_msg_to_zone: r.bool_str("is_msg_to_zone")?,
                    model: Some(r.string_or("model", "")?),
                };
                if let Some(template) = self.templates.get_mut(&group.almighty) {
                    template.func_groups.push(group);
                }
                Ok(())
            },
        )?;
        Ok(())
    }

    /// `doodad_funcs`.
    fn load_funcs(&mut self, conn: &Connection) -> Result<(), GameDataError> {
        let mut funcs = Vec::new();
        for_each_row(
            conn,
            "SELECT * FROM doodad_funcs ORDER BY doodad_func_group_id, actual_func_id",
            [],
            |r| {
                funcs.push(DoodadFunc {
                    func_key: r.u32("id")?,
                    group_id: r.u32("doodad_func_group_id")?,
                    func_id: r.u32("actual_func_id")?,
                    func_type: r.string("actual_func_type")?,
                    next_phase: r.i32_or("next_phase", -1)?,
                    sound_id: r.u32_or("sound_id", 0)?,
                    skill_id: r.u32_or("func_skill_id", 0)?,
                    perm_id: r.u32("perm_id")?,
                    count: r.i32_or("act_count", 0)?,
                });
                Ok(())
            },
        )?;
        for func in funcs {
            self.funcs_by_groups
                .entry(func.group_id)
                .or_default()
                .push(func.clone());
            let key = func.func_key;
            if self.funcs_by_id.insert(key, func).is_some() {
                return Err(data_err(format!(
                    "doodad: duplicate doodad_funcs id {key}: An item with the same key has already been added"
                )));
            }
        }
        Ok(())
    }

    /// `doodad_phase_funcs`.
    fn load_phase_funcs(&mut self, conn: &Connection) -> Result<(), GameDataError> {
        for_each_row(
            conn,
            "SELECT * FROM doodad_phase_funcs ORDER BY doodad_func_group_id, actual_func_id",
            [],
            |r| {
                let func = DoodadPhaseFunc {
                    group_id: r.u32("doodad_func_group_id")?,
                    func_id: r.u32("actual_func_id")?,
                    func_type: r.string("actual_func_type")?,
                };
                self.phase_funcs
                    .entry(func.group_id)
                    .or_default()
                    .push(func);
                Ok(())
            },
        )?;
        Ok(())
    }

    /// `doodad_func_clout_effects` -> `DoodadFuncClout.Effects` (unknown clout: KeyNotFound).
    fn load_clout_effects(&mut self, conn: &Connection) -> Result<(), GameDataError> {
        let mut rows = Vec::new();
        for_each_row(conn, "SELECT * FROM doodad_func_clout_effects", [], |r| {
            let clout_id = r.u32("doodad_func_clout_id")?;
            rows.push((clout_id, r.u32("effect_id")?));
            Ok(())
        })?;
        let clouts = self
            .phase_func_templates
            .get_mut("DoodadFuncClout")
            .ok_or_else(|| data_err("doodad: no DoodadFuncClout dictionary".into()))?;
        for (clout_id, effect_id) in rows {
            match clouts.get_mut(&clout_id) {
                Some(FuncTemplate::Clout(c)) => c.effects.push(effect_id),
                _ => {
                    return Err(data_err(format!(
                        "doodad: doodad_func_clout_effects references unknown clout {clout_id}"
                    )))
                }
            }
        }
        Ok(())
    }

    /// `doodad_func_consume_changer_items` (`TryAdd`: the first row of an id wins).
    fn load_consume_changer_items(&mut self, conn: &Connection) -> Result<(), GameDataError> {
        let mut seen: HashSet<u32> = self.consume_changer_items.iter().map(|e| e.id).collect();
        for_each_row(
            conn,
            "SELECT * FROM doodad_func_consume_changer_items",
            [],
            |r| {
                let entry = DoodadFuncConsumeChangerItem {
                    id: r.u32("id")?,
                    doodad_func_consume_changer_id: r.u32("doodad_func_consume_changer_id")?,
                    item_id: r.u32("item_id")?,
                };
                if seen.insert(entry.id) {
                    self.consume_changer_items.push(entry);
                }
                Ok(())
            },
        )?;
        Ok(())
    }

    /// Second `doodad_func_groups` pass (`_allFuncGroups`).
    fn load_all_func_groups(&mut self, conn: &Connection) -> Result<(), GameDataError> {
        for_each_row(conn, "SELECT * FROM doodad_func_groups", [], |r| {
            let almighty = r.u32("doodad_almighty_id")?;
            let model = if super::super::fish_school::is_school_template(almighty) {
                Some(r.string_or("model", "")?)
            } else {
                None
            };
            let group = DoodadFuncGroups {
                id: r.u32("id")?,
                almighty,
                group_kind_id: r.u32("doodad_func_group_kind_id")?,
                sound_id: r.u32_or("sound_id", 0)?,
                phase_msg: r.string_or("phase_msg", "")?,
                is_msg_to_zone: r.bool_str("is_msg_to_zone")?,
                model,
            };
            if self.all_func_groups_index.contains_key(&group.id) {
                tracing::error!("Failed to add FuncGroups: {}", group.id);
            } else {
                self.all_func_groups_index
                    .insert(group.id, self.all_func_groups.len());
                self.all_func_groups.push(group);
            }
            Ok(())
        })?;
        Ok(())
    }

    /// `LoadDoodadTemplates`: `doodad_groups`, `doodad_almighties`, func groups binding.
    fn load_doodad_templates(&mut self, conn: &Connection) -> Result<(), GameDataError> {
        tracing::info!("Loading doodad templates...");
        self.doodad_groups.clear();
        for_each_row(conn, "SELECT * FROM doodad_groups", [], |r| {
            let group = DoodadGroups {
                id: r.u32("id")?,
                guard_on_field_time: r.u32("guard_on_field_time")?,
                is_export: r.bool("is_export")?,
                removed_by_house: r.bool("removed_by_house")?,
            };
            self.doodad_groups.entry(group.id).or_insert(group);
            Ok(())
        })?;

        // `IsCofferTemplate` for every template at once (same result, one pass).
        let coffer_capacities = self.coffer_capacities();
        let mut rows = Vec::new();
        for_each_row(conn, "SELECT * from doodad_almighties", [], |r| {
            let template_id = r.u32("id")?;
            let coffer_capacity = if template_id == 0 {
                -1
            } else {
                coffer_capacities.get(&template_id).copied().unwrap_or(-1)
            };
            let group_id = r.u32("group_id")?;
            let template = DoodadTemplate {
                id: template_id,
                once_one_man: r.bool_str("once_one_man")?,
                once_one_interaction: r.bool_str("once_one_interaction")?,
                mgmt_spawn: r.bool_str("mgmt_spawn")?,
                percent: r.i32_or("percent", 0)?,
                min_time: r.i32_or("min_time", 0)?,
                max_time: r.i32_or("max_time", 0)?,
                model_kind_id: r.u32("model_kind_id")?,
                use_creator_faction: r.bool_str("use_creator_faction")?,
                force_tod_top_priority: r.bool_str("force_tod_top_priority")?,
                milestone_id: r.u32_or("milestone_id", 0)?,
                group_id,
                // Filled below (the C# throws right after reading group_id).
                group: DoodadGroups::default(),
                use_target_decal: r.bool_str("use_target_decal")?,
                use_target_silhouette: r.bool_str("use_target_silhouette")?,
                use_target_highlight: r.bool_str("use_target_highlight")?,
                target_decal_size: r.f32_or("target_decal_size", 0.0)?,
                sim_radius: r.i32_or("sim_radius", 0)?,
                collide_ship: r.bool_str("collide_ship")?,
                collide_vehicle: r.bool_str("collide_vehicle")?,
                climate_id: r.u32_or("climate_id", 0)? as i32,
                save_indun: r.bool_str("save_indun")?,
                force_up_action: r.bool_str("force_up_action")?,
                parentable: r.bool_str("parentable")?,
                childable: r.bool_str("childable")?,
                faction_id: r.u32("faction_id")?,
                growth_time: r.i32_or("growth_time", 0)?,
                despawn_on_collision: r.bool_str("despawn_on_collision")?,
                no_collision: r.bool_str("no_collision")?,
                restrict_zone_id: r.u32_or("restrict_zone_id", 0)?,
                func_groups: Vec::new(),
                total_doodad_growth_time: 0,
                coffer_capacity: (coffer_capacity > 0).then_some(coffer_capacity),
            };
            rows.push((template, group_id));
            Ok(())
        })?;
        for (mut template, group_id) in rows {
            let Some(group) = self.doodad_groups.get(&group_id) else {
                return Err(data_err(format!(
                    "Invalid doodad group {group_id} for doodad {}",
                    template.id
                )));
            };
            template.group = *group;
            let id = template.id;
            if self.templates.insert(id, template).is_some() {
                return Err(data_err(format!(
                    "doodad: duplicate doodad_almighties id {id}: An item with the same key has already been added"
                )));
            }
            self.template_order.push(id);
        }

        // Bind FuncGroups to Template
        for group in &self.all_func_groups {
            if let Some(template) = self.templates.get_mut(&group.almighty) {
                template.func_groups.push(group.clone());
            }
        }
        Ok(())
    }

    /// `doodad_place_skins`: the place kinds the 1.2 client's place-skin rule accepts for a
    /// doodad template. The client keys them by `doodad_almighty_id` and keeps every row; a
    /// template without a row cannot be planted on terrain (outside the house-box skip).
    fn load_place_skins(&mut self, conn: &Connection) -> Result<(), GameDataError> {
        tracing::info!("Loading doodad place skins ...");
        let mut rows = 0usize;
        for_each_row(conn, "SELECT * FROM doodad_place_skins", [], |r| {
            let almighty = r.u32_or("doodad_almighty_id", 0)?;
            let kind = r.u32_or("doodad_place_skin_kind_id", 0)?;
            if almighty == 0 || !(1..=4).contains(&kind) {
                // A bad row must not fail the whole catalog (LESSONS.md).
                tracing::warn!("doodad_place_skins: skipping invalid kind {kind} for {almighty}");
                return Ok(());
            }
            self.place_skins.entry(almighty).or_default().push(kind);
            rows += 1;
            Ok(())
        })?;
        tracing::info!("Loaded {rows} doodad place skins");
        Ok(())
    }

    /// `IsCofferTemplate` of every template id that has a coffer: walks `_allFuncGroups` in
    /// order and keeps the first `DoodadFuncCoffer` capacity found per template, exactly what
    /// `IsCofferTemplate(id)` returns for each id (-1 / absent otherwise).
    fn coffer_capacities(&self) -> HashMap<u32, i32> {
        let mut out = HashMap::new();
        for group in &self.all_func_groups {
            if out.contains_key(&group.almighty) {
                continue;
            }
            if let Some(capacity) = self.first_coffer_capacity_of_group(group.id) {
                out.insert(group.almighty, capacity);
            }
        }
        out
    }

    fn first_coffer_capacity_of_group(&self, group_id: u32) -> Option<i32> {
        let funcs = self.funcs_by_groups.get(&group_id)?;
        funcs.iter().find_map(|func| {
            match self.phase_func_template(func.func_id, &func.func_type) {
                Some(FuncTemplate::Coffer(c)) => Some(c.capacity),
                _ => None,
            }
        })
    }

    /// `IsCofferTemplate(templateId)`: the coffer capacity of the first `DoodadFuncCoffer`
    /// reachable from the template's func groups (through `doodad_funcs`), else -1.
    pub fn is_coffer_template(&self, template_id: u32) -> i32 {
        if template_id == 0 {
            return -1;
        }
        for group in &self.all_func_groups {
            if group.almighty != template_id {
                continue;
            }
            if let Some(capacity) = self.first_coffer_capacity_of_group(group.id) {
                return capacity;
            }
        }
        -1
    }

    /// `CreateTemplateCaches`: `TotalDoodadGrowthTime` of every template.
    pub fn create_template_caches(&mut self) {
        let mut totals = Vec::with_capacity(self.templates.len());
        for template in self.templates.values() {
            let mut total: i32 = 0;
            for group in &template.func_groups {
                for func in self.funcs_for_group(group.id) {
                    if let Some(FuncTemplate::Growth(g)) =
                        self.phase_func_template(func.func_id, &func.func_type)
                    {
                        total = total.wrapping_add(g.delay);
                    }
                }
            }
            if total <= 0 {
                total = template.growth_time;
            }
            totals.push((template.id, total));
        }
        for (id, total) in totals {
            if let Some(t) = self.templates.get_mut(&id) {
                t.total_doodad_growth_time = total;
            }
        }
    }

    /// `DoodadManager.Exist`.
    pub fn exist(&self, template_id: u32) -> bool {
        self.templates.contains_key(&template_id)
    }

    /// `DoodadManager.GetTemplate`.
    pub fn template(&self, template_id: u32) -> Option<&DoodadTemplate> {
        self.templates.get(&template_id)
    }

    /// `GetFunc(funcId)` (by `FuncKey`, the `doodad_funcs` row id).
    pub fn func(&self, func_key: u32) -> Option<&DoodadFunc> {
        self.funcs_by_id.get(&func_key)
    }

    /// `GetFunc(funcGroupId, skillId)`: the first func of the group whose `SkillId` is the
    /// skill, including the skill named by a FakeUse, Use or SkillHit template.
    /// Falls back to a legacy gathering skill, then a skill-less func, preferring
    /// one with `NextPhase != -1`.
    pub fn func_for_skill(&self, func_group_id: u32, skill_id: u32) -> Option<&DoodadFunc> {
        let funcs = self.funcs_by_groups.get(&func_group_id)?;
        for func in funcs {
            if func.skill_id == skill_id {
                return Some(func);
            }
            match self.func_template(func.func_id, &func.func_type) {
                // Special handler for fake use skill id
                Some(FuncTemplate::FakeUse(t))
                    if t.fake_skill_id > 0 && t.fake_skill_id == skill_id =>
                {
                    return Some(func)
                }
                // Special handler for use (func) skill id
                Some(FuncTemplate::Use(t)) if t.skill_id > 0 && t.skill_id == skill_id => {
                    return Some(func)
                }
                // A phase can respond to several skills without a func_skill_id.
                // Select the SkillHit row that names this spell before the fallback
                // picks the first skill-less row (the Cradle mushroom accepts two).
                Some(FuncTemplate::SkillHit(t)) if t.skill_id > 0 && t.skill_id == skill_id => {
                    return Some(func)
                }
                _ => {}
            }
        }
        // The legacy gathering compatibility adapter supplies a skill when func_skill_id
        // is absent. Resolve it before the unchanged generic skill-less fallback.
        if let Some(func) = funcs.iter().find(|f| {
            f.skill_id == 0
                && self
                    .func_template(f.func_id, &f.func_type)
                    .and_then(super::gathering::legacy_skill)
                    == Some(skill_id)
        }) {
            return Some(func);
        }
        // First we skip functions with NextPhase = -1
        if let Some(func) = funcs.iter().find(|f| f.skill_id == 0 && f.next_phase != -1) {
            return Some(func);
        }
        // Then we search with NextPhase = -1
        funcs.iter().find(|f| f.skill_id == 0)
    }

    pub fn accepts_skill_hit(&self, func_group_id: u32, skill_id: u32) -> bool {
        self.funcs_for_group(func_group_id).iter().any(|func| {
            matches!(
                self.func_template(func.func_id, &func.func_type),
                Some(FuncTemplate::SkillHit(hit)) if hit.skill_id == skill_id
            )
        })
    }

    /// `GetFuncsForGroup` (empty when the group has no funcs).
    pub fn funcs_for_group(&self, func_group_id: u32) -> &[DoodadFunc] {
        self.funcs_by_groups
            .get(&func_group_id)
            .map_or(&[], Vec::as_slice)
    }

    /// `GetDoodadFuncs` (same as [`Self::funcs_for_group`]).
    pub fn doodad_funcs(&self, func_group_id: u32) -> &[DoodadFunc] {
        self.funcs_for_group(func_group_id)
    }

    /// `GetPhaseFunc` / `GetDoodadPhaseFuncs` (empty when the group has no phase funcs).
    pub fn phase_funcs(&self, func_group_id: u32) -> &[DoodadPhaseFunc] {
        self.phase_funcs
            .get(&func_group_id)
            .map_or(&[], Vec::as_slice)
    }

    /// `GetFuncTemplate(funcId, funcType)`: only classes deriving from `DoodadFuncTemplate`.
    pub fn func_template(&self, func_id: u32, func_type: &str) -> Option<&FuncTemplate> {
        let func_type = if func_type == "DoodadFuncTreeByproductsCollect" {
            "DoodadFuncTreeByProductsCollect"
        } else {
            func_type
        };
        self.func_templates.get(func_type)?.get(&func_id)
    }

    /// `GetPhaseFuncTemplate(funcId, funcType)`: only classes deriving from
    /// `DoodadPhaseFuncTemplate`.
    pub fn phase_func_template(&self, func_id: u32, func_type: &str) -> Option<&FuncTemplate> {
        self.phase_func_templates.get(func_type)?.get(&func_id)
    }

    /// `GetDoodadFuncGroups(templateId)` (empty for an unknown template).
    pub fn doodad_func_groups(&self, template_id: u32) -> &[DoodadFuncGroups] {
        self.templates
            .get(&template_id)
            .map_or(&[], |t| t.func_groups.as_slice())
    }

    /// `GetDoodadFuncGroupsId(templateId)`.
    pub fn func_groups_ids(&self, template_id: u32) -> Vec<u32> {
        self.doodad_func_groups(template_id)
            .iter()
            .map(|g| g.id)
            .collect()
    }

    /// `_allFuncGroups[id]`.
    pub fn func_group(&self, func_group_id: u32) -> Option<&DoodadFuncGroups> {
        self.all_func_groups_index
            .get(&func_group_id)
            .map(|&i| &self.all_func_groups[i])
    }

    /// `GetDoodadFuncConsumeChangerItemList(doodadFuncConsumeChangerId)`: item ids in
    /// insertion order.
    pub fn consume_changer_items(&self, doodad_func_consume_changer_id: u32) -> Vec<u32> {
        self.consume_changer_items
            .iter()
            .filter(|e| e.doodad_func_consume_changer_id == doodad_func_consume_changer_id)
            .map(|e| e.item_id)
            .collect()
    }

    /// `GetTreasureChestTemplateIds`: templates of groups 55..=59, in template order.
    pub fn treasure_chest_template_ids(&self) -> Vec<u32> {
        self.template_order
            .iter()
            .filter(|id| {
                self.templates
                    .get(id)
                    .is_some_and(|t| (55..=59).contains(&t.group_id))
            })
            .copied()
            .collect()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod manager_tests;

#[cfg(test)]
mod group_loading_tests;
