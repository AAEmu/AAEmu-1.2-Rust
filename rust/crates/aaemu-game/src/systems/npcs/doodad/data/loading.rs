use super::*;

/// `reader.GetInt32(col, -1) >= 0 ? reader.GetInt32(col) : -1`.
fn next_phase_non_negative(r: &SqlRow<'_, '_>, col: &str) -> rusqlite::Result<i32> {
    let v = r.i32_or(col, -1)?;
    Ok(if v >= 0 { v } else { -1 })
}

/// One `doodad_func_*` table: `SELECT *`, build, `dict[class].Add(func.Id, func)`.
pub(super) fn load_templates(
    conn: &Connection,
    dict: &mut FuncTemplateMap,
    sql: &str,
    mut build: impl FnMut(&SqlRow<'_, '_>) -> rusqlite::Result<FuncTemplate>,
) -> Result<(), GameDataError> {
    let mut rows = Vec::new();
    for_each_row(conn, sql, [], |r| {
        rows.push(build(r)?);
        Ok(())
    })?;
    for func in rows {
        let class = func.func_type();
        let map = dict
            .get_mut(class)
            .ok_or_else(|| data_err(format!("doodad: no template dictionary for {class}")))?;
        let id = func.id();
        if map.contains_key(&id) {
            return Err(data_err(format!(
                "doodad: duplicate {class} id {id} ({sql}): An item with the same key has already been added"
            )));
        }
        map.insert(id, func);
    }
    Ok(())
}

impl DoodadData {
    /// `DoodadManager.Load` on a connection.
    pub fn load_from(conn: &Connection) -> Result<Self, GameDataError> {
        let mut d = Self::default();
        for (class, phase) in FUNC_TEMPLATE_CLASSES {
            if phase {
                d.phase_func_templates.insert(class, HashMap::new());
            } else {
                d.func_templates.insert(class, HashMap::new());
            }
        }

        tracing::info!("Loading doodad functions ...");
        d.load_func_groups_discarded(conn)?;
        d.load_funcs(conn)?;
        d.load_phase_funcs(conn)?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_animates",
            |r| {
                Ok(FuncTemplate::Animate(DoodadFuncAnimate {
                    id: r.u32("id")?,
                    name: r.string("name")?,
                    play_once: r.bool_str("play_once")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_area_triggers",
            |r| {
                Ok(FuncTemplate::AreaTrigger(DoodadFuncAreaTrigger {
                    id: r.u32("id")?,
                    npc_id: r.u32_or("npc_id", 0)?,
                    is_enter: r.bool_str("is_enter")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_attachments",
            |r| {
                Ok(FuncTemplate::Attachment(DoodadFuncAttachment {
                    id: r.u32("id")?,
                    attach_point_id: r.u8("attach_point_id")?,
                    space: r.i32("space")?,
                    bond_kind_id: u32::from(r.u8("bond_kind_id")?),
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_bindings",
            |r| {
                Ok(FuncTemplate::Binding(DoodadFuncBinding {
                    id: r.u32("id")?,
                    district_id: r.u32("district_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_bubbles",
            |r| {
                Ok(FuncTemplate::Bubble(DoodadFuncBubble {
                    id: r.u32("id")?,
                    bubble_id: r.u32("bubble_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_buffs",
            |r| {
                Ok(FuncTemplate::Buff(DoodadFuncBuff {
                    id: r.u32("id")?,
                    buff_id: r.u32("buff_id")?,
                    radius: r.f32("radius")?,
                    count: r.i32("count")?,
                    perm_id: r.u32("perm_id")?,
                    relationship_id: r.u32("relationship_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_butchers",
            |r| {
                Ok(FuncTemplate::Butcher(DoodadFuncButcher {
                    id: r.u32("id")?,
                    corpse_model: r.string("corpse_model")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_buy_fish_items",
            |r| {
                Ok(FuncTemplate::BuyFishItem(DoodadFuncBuyFishItem {
                    id: r.u32("id")?,
                    doodad_func_buy_fish_id: r.u32("doodad_func_buy_fish_id")?,
                    item_id: r.u32("item_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_buy_fish_models",
            |r| {
                Ok(FuncTemplate::BuyFishModel(DoodadFuncBuyFishModel {
                    id: r.u32("id")?,
                    name: r.string("name")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_buy_fishes",
            |r| {
                Ok(FuncTemplate::BuyFish(DoodadFuncBuyFish {
                    id: r.u32("id")?,
                    item_id: r.u32_or("item_id", 0)?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_catches",
            |r| Ok(FuncTemplate::Catch(DoodadFuncCatch { id: r.u32("id")? })),
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_cereal_harvests",
            |r| {
                Ok(FuncTemplate::CerealHarvest(DoodadFuncCerealHarvest {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_cleanup_logic_links",
            |r| {
                Ok(FuncTemplate::CleanupLogicLink(DoodadFuncCleanupLogicLink {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_climate_reacts",
            |r| {
                Ok(FuncTemplate::ClimateReact(DoodadFuncClimateReact {
                    id: r.u32("id")?,
                    next_phase: r.i32_or("next_phase", -1)?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_climbs",
            |r| {
                Ok(FuncTemplate::Climb(DoodadFuncClimb {
                    id: r.u32("id")?,
                    climb_type_id: r.u32("climb_type_id")?,
                    allow_horizontal_multi_hanger: r.bool_str("allow_horizontal_multi_hanger")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_clouts",
            |r| {
                Ok(FuncTemplate::Clout(DoodadFuncClout {
                    id: r.u32("id")?,
                    fx_group_id: r.u32_or("fx_group_id", 0)?,
                    duration: r.i32("duration")?,
                    tick: r.i32("tick")?,
                    target_relation: r.u32("target_relation_id")? as u8,
                    buff_id: r.u32_or("buff_id", 0)?,
                    projectile_id: r.u32_or("projectile_id", 0)?,
                    show_to_friendly_only: r.bool_str("show_to_friendly_only")?,
                    next_phase: r.i32_or("next_phase", -1)?,
                    aoe_shape_id: r.u32("aoe_shape_id")?,
                    target_buff_tag_id: r.u32_or("target_buff_tag_id", 0)?,
                    target_no_buff_tag_id: r.u32_or("target_no_buff_tag_id", 0)?,
                    use_origin_source: r.bool_str("use_origin_source")?,
                    effects: Vec::new(),
                }))
            },
        )?;
        d.load_clout_effects(conn)?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_coffer_perms",
            |r| {
                Ok(FuncTemplate::CofferPerm(DoodadFuncCofferPerm {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_coffers",
            |r| {
                Ok(FuncTemplate::Coffer(DoodadFuncCoffer {
                    id: r.u32("id")?,
                    capacity: r.i32("capacity")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_conditional_uses",
            |r| {
                Ok(FuncTemplate::ConditionalUse(DoodadFuncConditionalUse {
                    id: r.u32("id")?,
                    skill_id: r.u32_or("skill_id", 0)?,
                    fake_skill_id: r.u32_or("fake_skill_id", 0)?,
                    quest_id: r.u32_or("quest_id", 0)?,
                    quest_trigger_phase: r.u32_or("quest_trigger_phase", 0)?,
                    item_id: r.u32_or("item_id", 0)?,
                    item_trigger_phase: r.u32_or("item_trigger_phase", 0)?,
                }))
            },
        )?;
        d.load_consume_changer_items(conn)?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_consume_changer_model_items",
            |r| {
                Ok(FuncTemplate::ConsumeChangerModelItem(
                    DoodadFuncConsumeChangerModelItem {
                        id: r.u32("id")?,
                        doodad_func_consume_changer_model_id: r
                            .u32("doodad_func_consume_changer_model_id")?,
                        item_id: r.u32("item_id")?,
                    },
                ))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_consume_changer_models",
            |r| {
                Ok(FuncTemplate::ConsumeChangerModel(
                    DoodadFuncConsumeChangerModel {
                        id: r.u32("id")?,
                        name: r.string("name")?,
                    },
                ))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_consume_changers",
            |r| {
                Ok(FuncTemplate::ConsumeChanger(DoodadFuncConsumeChanger {
                    id: r.u32("id")?,
                    slot_id: r.u32("slot_id")?,
                    count: r.i32("count")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_consume_items",
            |r| {
                Ok(FuncTemplate::ConsumeItem(DoodadFuncConsumeItem {
                    id: r.u32("id")?,
                    item_id: r.u32("item_id")?,
                    count: r.i32("count")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_convert_fish_items",
            |r| {
                Ok(FuncTemplate::ConvertFishItem(DoodadFuncConvertFishItem {
                    id: r.u32("id")?,
                    doodad_func_convert_fish_id: r.u32("doodad_func_convert_fish_id")?,
                    item_id: r.u32("item_id")?,
                    loot_pack_id: r.u32("loot_pack_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_convert_fishes",
            |r| {
                Ok(FuncTemplate::ConvertFish(DoodadFuncConvertFish {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_craft_acts",
            |r| {
                Ok(FuncTemplate::CraftAct(DoodadFuncCraftAct {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_craft_cancels",
            |r| {
                Ok(FuncTemplate::CraftCancel(DoodadFuncCraftCancel {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_craft_directs",
            |r| {
                Ok(FuncTemplate::CraftDirect(DoodadFuncCraftDirect {
                    id: r.u32("id")?,
                    next_phase: r.i32_or("next_phase", -1)?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_craft_get_items",
            |r| {
                Ok(FuncTemplate::CraftGetItem(DoodadFuncCraftGetItem {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_craft_infos",
            |r| {
                Ok(FuncTemplate::CraftInfo(DoodadFuncCraftInfo {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_craft_packs",
            |r| {
                Ok(FuncTemplate::CraftPack(DoodadFuncCraftPack {
                    id: r.u32("id")?,
                    craft_pack_id: r.u32("craft_pack_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_craft_start_crafts",
            |r| {
                Ok(FuncTemplate::CraftStartCraft(DoodadFuncCraftStartCraft {
                    id: r.u32("id")?,
                    doodad_func_craft_start_id: r.u32("doodad_func_craft_start_id")?,
                    craft_id: r.u32("craft_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_craft_starts",
            |r| {
                Ok(FuncTemplate::CraftStart(DoodadFuncCraftStart {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_crop_harvests",
            |r| {
                Ok(FuncTemplate::CropHarvest(DoodadFuncCropHarvest {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_crystal_collects",
            |r| {
                Ok(FuncTemplate::CrystalCollect(DoodadFuncCrystalCollect {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_cutdownings",
            |r| {
                Ok(FuncTemplate::Cutdowning(DoodadFuncCutdowning {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_cutdowns",
            |r| {
                Ok(FuncTemplate::Cutdown(DoodadFuncCutdown {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_dairy_collects",
            |r| {
                Ok(FuncTemplate::DairyCollect(DoodadFuncDairyCollect {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_declare_sieges",
            |r| {
                Ok(FuncTemplate::DeclareSiege(DoodadFuncDeclareSiege {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_digs",
            |r| Ok(FuncTemplate::Dig(DoodadFuncDig { id: r.u32("id")? })),
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_dig_terrains",
            |r| {
                Ok(FuncTemplate::DigTerrain(DoodadFuncDigTerrain {
                    id: r.u32("id")?,
                    radius: r.i32("radius")?,
                    life: r.i32("life")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_dyeingredient_collects",
            |r| {
                Ok(FuncTemplate::DyeingredientCollect(
                    DoodadFuncDyeingredientCollect { id: r.u32("id")? },
                ))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_enter_instances",
            |r| {
                Ok(FuncTemplate::EnterInstance(DoodadFuncEnterInstance {
                    id: r.u32("id")?,
                    zone_id: r.u32("zone_id")?,
                    item_id: r.u32_or("item_id", 0)?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_enter_sys_instances",
            |r| {
                Ok(FuncTemplate::EnterSysInstance(DoodadFuncEnterSysInstance {
                    id: r.u32("id")?,
                    zone_id: r.u32("zone_id")?,
                    faction_id: r.u32_or("faction_id", 0)?,
                    selective: r.bool_str("selective")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_evidence_item_loots",
            |r| {
                Ok(FuncTemplate::EvidenceItemLoot(DoodadFuncEvidenceItemLoot {
                    id: r.u32("id")?,
                    skill_id: r.u32("skill_id")?,
                    crime_value: r.i16("crime_value")?,
                    crime_kind_id: r.u32("crime_kind_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_exchanges",
            |r| {
                Ok(FuncTemplate::Exchange(DoodadFuncExchange {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_exit_induns",
            |r| {
                Ok(FuncTemplate::ExitIndun(DoodadFuncExitIndun {
                    id: r.u32("id")?,
                    return_point_id: r.u32_or("return_point_id", 0)?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_fake_uses",
            |r| {
                Ok(FuncTemplate::FakeUse(DoodadFuncFakeUse {
                    id: r.u32("id")?,
                    skill_id: r.u32_or("skill_id", 0)?,
                    fake_skill_id: r.u32_or("fake_skill_id", 0)?,
                    target_parent: r.bool_str("target_parent")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_feeds",
            |r| {
                Ok(FuncTemplate::Feed(DoodadFuncFeed {
                    id: r.u32("id")?,
                    item_id: r.u32("item_id")?,
                    count: r.i32("count")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_fiber_collects",
            |r| {
                Ok(FuncTemplate::FiberCollect(DoodadFuncFiberCollect {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_finals",
            |r| {
                Ok(FuncTemplate::Final(DoodadFuncFinal {
                    id: r.u32("id")?,
                    after: r.i32_or("after", 0)?,
                    respawn: r.bool_str("respawn")?,
                    min_time: r.i32_or("min_time", 0)?,
                    max_time: r.i32_or("max_time", 0)?,
                    show_tip: r.bool_str("show_tip")?,
                    show_end_time: r.bool_str("show_end_time")?,
                    tip: r.string("tip")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_fish_schools",
            |r| {
                Ok(FuncTemplate::FishSchool(DoodadFuncFishSchool {
                    id: r.u32("id")?,
                    npc_spawner_id: r.u32("npc_spawner_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_fruit_picks",
            |r| {
                Ok(FuncTemplate::FruitPick(DoodadFuncFruitPick {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_gass_extracts",
            |r| {
                Ok(FuncTemplate::GassExtract(DoodadFuncGassExtract {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_growths",
            |r| {
                Ok(FuncTemplate::Growth(DoodadFuncGrowth {
                    id: r.u32("id")?,
                    delay: r.i32("delay")?,
                    start_scale: r.i32("start_scale")?,
                    end_scale: r.i32("end_scale")?,
                    next_phase: r.i32_or("next_phase", -1)?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_harvests",
            |r| {
                Ok(FuncTemplate::Harvest(DoodadFuncHarvest {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_house_farms",
            |r| {
                Ok(FuncTemplate::HouseFarm(DoodadFuncHouseFarm {
                    id: r.u32("id")?,
                    item_category_id: r.u32("item_category_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_housing_areas",
            |r| {
                Ok(FuncTemplate::HousingArea(DoodadFuncHousingArea {
                    id: r.u32("id")?,
                    faction_id: r.u32("faction_id")?,
                    radius: r.i32("radius")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_hungers",
            |r| {
                Ok(FuncTemplate::Hunger(DoodadFuncHunger {
                    id: r.i32("id")? as u32,
                    hungry_term: r.i32("hungry_term")?,
                    full_step: r.i32("full_step")?,
                    phase_change_limit: r.i32("phase_change_limit")?,
                    next_phase: next_phase_non_negative(r, "next_phase")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_insert_counters",
            |r| {
                Ok(FuncTemplate::InsertCounter(DoodadFuncInsertCounter {
                    id: r.u32("id")?,
                    count: r.i32("count")?,
                    item_id: r.u32("item_id")?,
                    item_count: r.i32("item_count")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_logics",
            |r| {
                Ok(FuncTemplate::Logic(DoodadFuncLogic {
                    id: r.u32("id")?,
                    operation_id: r.u32("operation_id")?,
                    delay_id: r.u32("delay_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_logic_family_providers",
            |r| {
                Ok(FuncTemplate::LogicFamilyProvider(
                    DoodadFuncLogicFamilyProvider {
                        id: r.u32("id")?,
                        family_id: r.u32("family_id")?,
                    },
                ))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_logic_family_subscribers",
            |r| {
                Ok(FuncTemplate::LogicFamilySubscriber(
                    DoodadFuncLogicFamilySubscriber {
                        id: r.u32("id")?,
                        family_id: r.u32("family_id")?,
                    },
                ))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_loot_items",
            |r| {
                Ok(FuncTemplate::LootItem(DoodadFuncLootItem {
                    id: r.u32("id")?,
                    world_interaction_id: r.u32("wi_id")? as i32,
                    item_id: r.u32("item_id")?,
                    count_min: r.i32("count_min")?,
                    count_max: r.i32("count_max")?,
                    percent: r.i32("percent")?,
                    remain_time: r.i32("remain_time")?,
                    group_id: r.u32("group_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_loot_packs",
            |r| {
                Ok(FuncTemplate::LootPack(DoodadFuncLootPack {
                    id: r.u32("id")?,
                    loot_pack_id: r.u32("loot_pack_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_machine_parts_collects",
            |r| {
                Ok(FuncTemplate::MachinePartsCollect(
                    DoodadFuncMachinePartsCollect { id: r.u32("id")? },
                ))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_medicalingredient_mines",
            |r| {
                Ok(FuncTemplate::MedicalingredientMine(
                    DoodadFuncMedicalingredientMine { id: r.u32("id")? },
                ))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_mows",
            |r| Ok(FuncTemplate::Mow(DoodadFuncMow { id: r.u32("id")? })),
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_navi_mark_pos_to_maps",
            |r| {
                Ok(FuncTemplate::NaviMarkPosToMap(DoodadFuncNaviMarkPosToMap {
                    id: r.u32("id")?,
                    x: r.i32("x")?,
                    y: r.i32("y")?,
                    z: r.i32("z")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_navi_namings",
            |r| {
                Ok(FuncTemplate::NaviNaming(DoodadFuncNaviNaming {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_navi_open_bounties",
            |r| {
                Ok(FuncTemplate::NaviOpenBounty(DoodadFuncNaviOpenBounty {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_navi_open_mailboxes",
            |r| {
                Ok(FuncTemplate::NaviOpenMailbox(DoodadFuncNaviOpenMailbox {
                    id: r.u32("id")?,
                    duration: r.i32("duration")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_navi_open_portals",
            |r| {
                Ok(FuncTemplate::NaviOpenPortal(DoodadFuncNaviOpenPortal {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_navi_remove_timers",
            |r| {
                Ok(FuncTemplate::NaviRemoveTimer(DoodadFuncNaviRemoveTimer {
                    id: r.u32("id")?,
                    after: r.i32("after")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_navi_removes",
            |r| {
                Ok(FuncTemplate::NaviRemove(DoodadFuncNaviRemove {
                    id: r.u32("id")?,
                    req_labor_power: r.i32("req_lp")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_navi_teleports",
            |r| {
                Ok(FuncTemplate::NaviTeleport(DoodadFuncNaviTeleport {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_open_farm_infos",
            |r| {
                Ok(FuncTemplate::OpenFarmInfo(DoodadFuncOpenFarmInfo {
                    id: r.u32("id")?,
                    farm_id: r.u32("farm_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_open_papers",
            |r| {
                Ok(FuncTemplate::OpenPaper(DoodadFuncOpenPaper {
                    id: r.u32("id")?,
                    book_page_id: r.u32_or("book_page_id", 0)?,
                    book_id: r.u32_or("book_id", 0)?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_ore_mines",
            |r| {
                Ok(FuncTemplate::OreMine(DoodadFuncOreMine {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_parent_infos",
            |r| {
                Ok(FuncTemplate::ParentInfo(DoodadFuncParentInfo {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_parrots",
            |r| Ok(FuncTemplate::Parrot(DoodadFuncParrot { id: r.u32("id")? })),
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_plant_collects",
            |r| {
                Ok(FuncTemplate::PlantCollect(DoodadFuncPlantCollect {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_play_flow_graphs",
            |r| {
                Ok(FuncTemplate::PlayFlowGraph(DoodadFuncPlayFlowGraph {
                    id: r.u32("id")?,
                    event_on_phase_change_id: r.u32("event_on_phase_change_id")?,
                    event_on_visible_id: r.u32("event_on_visible_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_pulse_triggers",
            |r| {
                Ok(FuncTemplate::PulseTrigger(DoodadFuncPulseTrigger {
                    id: r.u32("id")?,
                    flag: r.bool_str("flag")?,
                    next_phase: next_phase_non_negative(r, "next_phase")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_pulses",
            |r| {
                Ok(FuncTemplate::Pulse(DoodadFuncPulse {
                    id: r.u32("id")?,
                    flag: r.bool_str("flag")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_purchases",
            |r| {
                Ok(FuncTemplate::Purchase(DoodadFuncPurchase {
                    id: r.u32("id")?,
                    item_id: r.u32_or("item_id", 0)?,
                    count: r.i32("count")?,
                    coin_item_id: r.u32_or("coin_item_id", 0)?,
                    coin_count: r.i32_or("coin_count", 0)?,
                    currency_id: r.u32("currency_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_puzzle_ins",
            |r| {
                Ok(FuncTemplate::PuzzleIn(DoodadFuncPuzzleIn {
                    id: r.u32("id")?,
                    group_id: r.u32("group_id")?,
                    ratio: r.f32("ratio")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_puzzle_outs",
            |r| {
                Ok(FuncTemplate::PuzzleOut(DoodadFuncPuzzleOut {
                    id: r.u32("id")?,
                    group_id: r.u32("group_id")?,
                    ratio: r.f32("ratio")?,
                    anim: r.string("anim")?,
                    projectile_id: r.u32_or("projectile_id", 0)?,
                    loot_pack_id: r.u32_or("loot_pack_id", 0)?,
                    delay: r.i32("delay")?,
                    next_phase: r.i32_or("next_phase", -1)?,
                    projectile_delay: r.i32("projectile_delay")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_puzzle_rolls",
            |r| {
                Ok(FuncTemplate::PuzzleRoll(DoodadFuncPuzzleRoll {
                    id: r.u32("id")?,
                    item_id: r.u32("item_id")?,
                    count: r.i32("count")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_quests",
            |r| {
                Ok(FuncTemplate::Quest(DoodadFuncQuest {
                    id: r.u32("id")?,
                    quest_kind_id: r.u32("quest_kind_id")?,
                    quest_id: r.u32("quest_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_ratio_changes",
            |r| {
                Ok(FuncTemplate::RatioChange(DoodadFuncRatioChange {
                    id: r.u32("id")?,
                    ratio: r.i32("ratio")?,
                    next_phase: next_phase_non_negative(r, "next_phase")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_ratio_respawns",
            |r| {
                Ok(FuncTemplate::RatioRespawn(DoodadFuncRatioRespawn {
                    id: r.u32("id")?,
                    ratio: r.i32("ratio")?,
                    spawn_doodad_id: r.u32("spawn_doodad_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_recover_items",
            |r| {
                Ok(FuncTemplate::RecoverItem(DoodadFuncRecoverItem {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_remove_instances",
            |r| {
                Ok(FuncTemplate::RemoveInstance(DoodadFuncRemoveInstance {
                    id: r.u32("id")?,
                    zone_id: r.u32("zone_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_remove_items",
            |r| {
                Ok(FuncTemplate::RemoveItem(DoodadFuncRemoveItem {
                    id: r.u32("id")?,
                    item_id: r.u32("item_id")?,
                    count: r.i32("count")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_renew_items",
            |r| {
                Ok(FuncTemplate::RenewItem(DoodadFuncRenewItem {
                    id: r.u32("id")?,
                    skill_id: r.u32("skill_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_require_items",
            |r| {
                Ok(FuncTemplate::RequireItem(DoodadFuncRequireItem {
                    id: r.u32("id")?,
                    world_interaction_id: r.u32("wi_id")? as i32,
                    item_id: r.u32("item_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_require_quests",
            |r| {
                Ok(FuncTemplate::RequireQuest(DoodadFuncRequireQuest {
                    id: r.u32("id")?,
                    world_interaction_id: r.u32("wi_id")? as i32,
                    quest_id: r.u32("quest_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_respawns",
            |r| {
                Ok(FuncTemplate::Respawn(DoodadFuncRespawn {
                    id: r.u32("id")?,
                    min_time: r.i32("min_time")?,
                    max_time: r.i32("max_time")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_rock_mines",
            |r| {
                Ok(FuncTemplate::RockMine(DoodadFuncRockMine {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_seed_collects",
            |r| {
                Ok(FuncTemplate::SeedCollect(DoodadFuncSeedCollect {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_shears",
            |r| {
                Ok(FuncTemplate::Shear(DoodadFuncShear {
                    id: r.u32("id")?,
                    shear_type_id: r.u32("shear_type_id")?,
                    shear_term: r.i32("shear_term")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_siege_periods",
            |r| {
                Ok(FuncTemplate::SiegePeriod(DoodadFuncSiegePeriod {
                    id: r.u32("id")?,
                    siege_period_id: r.u32("siege_period_id")?,
                    next_phase: r.i32_or("next_phase", -1)?,
                    defense: r.bool_str("defense")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_signs",
            |r| {
                Ok(FuncTemplate::Sign(DoodadFuncSign {
                    id: r.u32("id")?,
                    name: r.string("name")?,
                    pick_num: r.i32("pick_num")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_skill_hits",
            |r| {
                Ok(FuncTemplate::SkillHit(DoodadFuncSkillHit {
                    id: r.u32("id")?,
                    skill_id: r.u32("skill_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_skin_offs",
            |r| {
                Ok(FuncTemplate::SkinOff(DoodadFuncSkinOff {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_soil_collects",
            |r| {
                Ok(FuncTemplate::SoilCollect(DoodadFuncSoilCollect {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_spawn_gimmicks",
            |r| {
                Ok(FuncTemplate::SpawnGimmick(DoodadFuncSpawnGimmick {
                    id: r.u32("id")?,
                    gimmick_id: r.u32("gimmick_id")?,
                    faction_id: r.u32("faction_id")?,
                    scale: r.f32("scale")?,
                    offset_x: r.f32("offset_x")?,
                    offset_y: r.f32("offset_y")?,
                    offset_z: r.f32("offset_z")?,
                    velocity_x: r.f32("velocity_x")?,
                    velocity_y: r.f32("velocity_y")?,
                    velocity_z: r.f32("velocity_z")?,
                    angle_x: r.f32("angle_x")?,
                    angle_y: r.f32("angle_y")?,
                    angle_z: r.f32("angle_z")?,
                    next_phase: r.i32_or("next_phase", -1)?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_spawn_mgmts",
            |r| {
                Ok(FuncTemplate::SpawnMgmt(DoodadFuncSpawnMgmt {
                    id: r.u32("id")?,
                    group_id: r.u32("group_id")?,
                    spawn: r.bool_str("spawn")?,
                    zone_id: r.u32("zone_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_spice_collects",
            |r| {
                Ok(FuncTemplate::SpiceCollect(DoodadFuncSpiceCollect {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_stamp_makers",
            |r| {
                Ok(FuncTemplate::StampMaker(DoodadFuncStampMaker {
                    id: r.u32("id")?,
                    consume_money: r.i32("consume_money")?,
                    item_id: r.u32("item_id")?,
                    consume_item_id: r.u32("consume_item_id")?,
                    consume_count: r.i32("consume_count")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_store_uis",
            |r| {
                Ok(FuncTemplate::StoreUi(DoodadFuncStoreUi {
                    id: r.u32("id")?,
                    merchant_pack_id: r.u32("merchant_pack_id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_timers",
            |r| {
                Ok(FuncTemplate::Timer(DoodadFuncTimer {
                    id: r.u32("id")?,
                    delay: r.i32("delay")?,
                    next_phase: r.i32_or("next_phase", -1)?,
                    keep_requester: r.bool_str("keep_requester")?,
                    show_tip: r.bool_str("show_tip")?,
                    show_end_time: r.bool_str("show_end_time")?,
                    tip: r.string("tip")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_tods",
            |r| {
                let mut func = DoodadFuncTod {
                    id: r.u32("id")?,
                    tod: r.u32("tod")?,
                    next_phase: next_phase_non_negative(r, "next_phase")?,
                    tod_as_hours: 0.0,
                };
                func.tod_as_hours = tod_as_hours(func.id, func.tod);
                Ok(FuncTemplate::Tod(func))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_tree_byproducts_collects",
            |r| {
                Ok(FuncTemplate::TreeByProductsCollect(
                    DoodadFuncTreeByProductsCollect { id: r.u32("id")? },
                ))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_ucc_imprints",
            |r| {
                Ok(FuncTemplate::UccImprint(DoodadFuncUccImprint {
                    id: r.u32("id")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.func_templates,
            "SELECT * FROM doodad_func_uses",
            |r| {
                Ok(FuncTemplate::Use(DoodadFuncUse {
                    id: r.u32("id")?,
                    skill_id: r.u32_or("skill_id", 0)?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_water_volumes",
            |r| {
                Ok(FuncTemplate::WaterVolume(DoodadFuncWaterVolume {
                    id: r.u32("id")?,
                    level_change: r.f32("levelChange")?,
                    duration: r.f32("duration")?,
                }))
            },
        )?;
        load_templates(
            conn,
            &mut d.phase_func_templates,
            "SELECT * FROM doodad_func_zone_reacts",
            |r| {
                Ok(FuncTemplate::ZoneReact(DoodadFuncZoneReact {
                    id: r.u32("id")?,
                    zone_group_id: r.u32("zone_group_id")?,
                    next_phase: r.i32_or("next_phase", -1)?,
                }))
            },
        )?;
        tracing::info!("Finished loading doodad functions ...");
        d.load_all_func_groups(conn)?;
        d.load_doodad_templates(conn)?;
        d.load_place_skins(conn)?;
        tracing::info!("Loaded {} doodad templates", d.templates.len());

        d.create_template_caches();
        Ok(d)
    }
}
