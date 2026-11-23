use super::*;

/// `(table, class)` of every `doodad_func_*` template table `DoodadManager.Load` reads.
const TEMPLATE_TABLES: [(&str, &str); 119] = [
    ("doodad_func_animates", "DoodadFuncAnimate"),
    ("doodad_func_area_triggers", "DoodadFuncAreaTrigger"),
    ("doodad_func_attachments", "DoodadFuncAttachment"),
    ("doodad_func_bindings", "DoodadFuncBinding"),
    ("doodad_func_bubbles", "DoodadFuncBubble"),
    ("doodad_func_buffs", "DoodadFuncBuff"),
    ("doodad_func_butchers", "DoodadFuncButcher"),
    ("doodad_func_buy_fish_items", "DoodadFuncBuyFishItem"),
    ("doodad_func_buy_fish_models", "DoodadFuncBuyFishModel"),
    ("doodad_func_buy_fishes", "DoodadFuncBuyFish"),
    ("doodad_func_catches", "DoodadFuncCatch"),
    ("doodad_func_cereal_harvests", "DoodadFuncCerealHarvest"),
    (
        "doodad_func_cleanup_logic_links",
        "DoodadFuncCleanupLogicLink",
    ),
    ("doodad_func_climate_reacts", "DoodadFuncClimateReact"),
    ("doodad_func_climbs", "DoodadFuncClimb"),
    ("doodad_func_clouts", "DoodadFuncClout"),
    ("doodad_func_coffer_perms", "DoodadFuncCofferPerm"),
    ("doodad_func_coffers", "DoodadFuncCoffer"),
    ("doodad_func_conditional_uses", "DoodadFuncConditionalUse"),
    (
        "doodad_func_consume_changer_model_items",
        "DoodadFuncConsumeChangerModelItem",
    ),
    (
        "doodad_func_consume_changer_models",
        "DoodadFuncConsumeChangerModel",
    ),
    ("doodad_func_consume_changers", "DoodadFuncConsumeChanger"),
    ("doodad_func_consume_items", "DoodadFuncConsumeItem"),
    (
        "doodad_func_convert_fish_items",
        "DoodadFuncConvertFishItem",
    ),
    ("doodad_func_convert_fishes", "DoodadFuncConvertFish"),
    ("doodad_func_craft_acts", "DoodadFuncCraftAct"),
    ("doodad_func_craft_cancels", "DoodadFuncCraftCancel"),
    ("doodad_func_craft_directs", "DoodadFuncCraftDirect"),
    ("doodad_func_craft_get_items", "DoodadFuncCraftGetItem"),
    ("doodad_func_craft_infos", "DoodadFuncCraftInfo"),
    ("doodad_func_craft_packs", "DoodadFuncCraftPack"),
    (
        "doodad_func_craft_start_crafts",
        "DoodadFuncCraftStartCraft",
    ),
    ("doodad_func_craft_starts", "DoodadFuncCraftStart"),
    ("doodad_func_crop_harvests", "DoodadFuncCropHarvest"),
    ("doodad_func_crystal_collects", "DoodadFuncCrystalCollect"),
    ("doodad_func_cutdownings", "DoodadFuncCutdowning"),
    ("doodad_func_cutdowns", "DoodadFuncCutdown"),
    ("doodad_func_dairy_collects", "DoodadFuncDairyCollect"),
    ("doodad_func_declare_sieges", "DoodadFuncDeclareSiege"),
    ("doodad_func_digs", "DoodadFuncDig"),
    ("doodad_func_dig_terrains", "DoodadFuncDigTerrain"),
    (
        "doodad_func_dyeingredient_collects",
        "DoodadFuncDyeingredientCollect",
    ),
    ("doodad_func_enter_instances", "DoodadFuncEnterInstance"),
    (
        "doodad_func_enter_sys_instances",
        "DoodadFuncEnterSysInstance",
    ),
    (
        "doodad_func_evidence_item_loots",
        "DoodadFuncEvidenceItemLoot",
    ),
    ("doodad_func_exchanges", "DoodadFuncExchange"),
    ("doodad_func_exit_induns", "DoodadFuncExitIndun"),
    ("doodad_func_fake_uses", "DoodadFuncFakeUse"),
    ("doodad_func_feeds", "DoodadFuncFeed"),
    ("doodad_func_fiber_collects", "DoodadFuncFiberCollect"),
    ("doodad_func_finals", "DoodadFuncFinal"),
    ("doodad_func_fish_schools", "DoodadFuncFishSchool"),
    ("doodad_func_fruit_picks", "DoodadFuncFruitPick"),
    ("doodad_func_gass_extracts", "DoodadFuncGassExtract"),
    ("doodad_func_growths", "DoodadFuncGrowth"),
    ("doodad_func_harvests", "DoodadFuncHarvest"),
    ("doodad_func_house_farms", "DoodadFuncHouseFarm"),
    ("doodad_func_housing_areas", "DoodadFuncHousingArea"),
    ("doodad_func_hungers", "DoodadFuncHunger"),
    ("doodad_func_insert_counters", "DoodadFuncInsertCounter"),
    ("doodad_func_logics", "DoodadFuncLogic"),
    (
        "doodad_func_logic_family_providers",
        "DoodadFuncLogicFamilyProvider",
    ),
    (
        "doodad_func_logic_family_subscribers",
        "DoodadFuncLogicFamilySubscriber",
    ),
    ("doodad_func_loot_items", "DoodadFuncLootItem"),
    ("doodad_func_loot_packs", "DoodadFuncLootPack"),
    (
        "doodad_func_machine_parts_collects",
        "DoodadFuncMachinePartsCollect",
    ),
    (
        "doodad_func_medicalingredient_mines",
        "DoodadFuncMedicalingredientMine",
    ),
    ("doodad_func_mows", "DoodadFuncMow"),
    (
        "doodad_func_navi_mark_pos_to_maps",
        "DoodadFuncNaviMarkPosToMap",
    ),
    ("doodad_func_navi_namings", "DoodadFuncNaviNaming"),
    ("doodad_func_navi_open_bounties", "DoodadFuncNaviOpenBounty"),
    (
        "doodad_func_navi_open_mailboxes",
        "DoodadFuncNaviOpenMailbox",
    ),
    ("doodad_func_navi_open_portals", "DoodadFuncNaviOpenPortal"),
    (
        "doodad_func_navi_remove_timers",
        "DoodadFuncNaviRemoveTimer",
    ),
    ("doodad_func_navi_removes", "DoodadFuncNaviRemove"),
    ("doodad_func_navi_teleports", "DoodadFuncNaviTeleport"),
    ("doodad_func_open_farm_infos", "DoodadFuncOpenFarmInfo"),
    ("doodad_func_open_papers", "DoodadFuncOpenPaper"),
    ("doodad_func_ore_mines", "DoodadFuncOreMine"),
    ("doodad_func_parent_infos", "DoodadFuncParentInfo"),
    ("doodad_func_parrots", "DoodadFuncParrot"),
    ("doodad_func_plant_collects", "DoodadFuncPlantCollect"),
    ("doodad_func_play_flow_graphs", "DoodadFuncPlayFlowGraph"),
    ("doodad_func_pulse_triggers", "DoodadFuncPulseTrigger"),
    ("doodad_func_pulses", "DoodadFuncPulse"),
    ("doodad_func_purchases", "DoodadFuncPurchase"),
    ("doodad_func_puzzle_ins", "DoodadFuncPuzzleIn"),
    ("doodad_func_puzzle_outs", "DoodadFuncPuzzleOut"),
    ("doodad_func_puzzle_rolls", "DoodadFuncPuzzleRoll"),
    ("doodad_func_quests", "DoodadFuncQuest"),
    ("doodad_func_ratio_changes", "DoodadFuncRatioChange"),
    ("doodad_func_ratio_respawns", "DoodadFuncRatioRespawn"),
    ("doodad_func_recover_items", "DoodadFuncRecoverItem"),
    ("doodad_func_remove_instances", "DoodadFuncRemoveInstance"),
    ("doodad_func_remove_items", "DoodadFuncRemoveItem"),
    ("doodad_func_renew_items", "DoodadFuncRenewItem"),
    ("doodad_func_require_items", "DoodadFuncRequireItem"),
    ("doodad_func_require_quests", "DoodadFuncRequireQuest"),
    ("doodad_func_respawns", "DoodadFuncRespawn"),
    ("doodad_func_rock_mines", "DoodadFuncRockMine"),
    ("doodad_func_seed_collects", "DoodadFuncSeedCollect"),
    ("doodad_func_shears", "DoodadFuncShear"),
    ("doodad_func_siege_periods", "DoodadFuncSiegePeriod"),
    ("doodad_func_signs", "DoodadFuncSign"),
    ("doodad_func_skill_hits", "DoodadFuncSkillHit"),
    ("doodad_func_skin_offs", "DoodadFuncSkinOff"),
    ("doodad_func_soil_collects", "DoodadFuncSoilCollect"),
    ("doodad_func_spawn_gimmicks", "DoodadFuncSpawnGimmick"),
    ("doodad_func_spawn_mgmts", "DoodadFuncSpawnMgmt"),
    ("doodad_func_spice_collects", "DoodadFuncSpiceCollect"),
    ("doodad_func_stamp_makers", "DoodadFuncStampMaker"),
    ("doodad_func_store_uis", "DoodadFuncStoreUi"),
    ("doodad_func_timers", "DoodadFuncTimer"),
    ("doodad_func_tods", "DoodadFuncTod"),
    (
        "doodad_func_tree_byproducts_collects",
        "DoodadFuncTreeByProductsCollect",
    ),
    ("doodad_func_ucc_imprints", "DoodadFuncUccImprint"),
    ("doodad_func_uses", "DoodadFuncUse"),
    ("doodad_func_water_volumes", "DoodadFuncWaterVolume"),
    ("doodad_func_zone_reacts", "DoodadFuncZoneReact"),
];

fn func(key: u32, group: u32, func_id: u32, ty: &str, next: i32, skill: u32) -> DoodadFunc {
    DoodadFunc {
        group_id: group,
        func_id,
        func_key: key,
        func_type: ty.to_string(),
        next_phase: next,
        skill_id: skill,
        ..Default::default()
    }
}

fn with_classes() -> DoodadData {
    let mut d = DoodadData::default();
    for (class, phase) in FUNC_TEMPLATE_CLASSES {
        if phase {
            d.phase_func_templates.insert(class, HashMap::new());
        } else {
            d.func_templates.insert(class, HashMap::new());
        }
    }
    d
}

fn add(d: &mut DoodadData, t: FuncTemplate) {
    let dict = if t.is_phase_func() {
        &mut d.phase_func_templates
    } else {
        &mut d.func_templates
    };
    dict.get_mut(t.func_type()).unwrap().insert(t.id(), t);
}

#[test]
fn func_template_classes_are_complete() {
    assert_eq!(FUNC_TEMPLATE_CLASSES.len(), 121);
    assert_eq!(FUNC_TEMPLATE_CLASSES.iter().filter(|c| c.1).count(), 42);
    let t = FuncTemplate::Tod(DoodadFuncTod {
        id: 7,
        ..Default::default()
    });
    assert_eq!(
        (t.func_type(), t.id(), t.is_phase_func()),
        ("DoodadFuncTod", 7, true)
    );
    let t = FuncTemplate::BuyFishItem(DoodadFuncBuyFishItem::default());
    assert!(t.is_phase_func());
    let t = FuncTemplate::FakeUse(DoodadFuncFakeUse::default());
    assert!(!t.is_phase_func());
    assert_eq!(t.func_type(), "DoodadFuncFakeUse");
}

#[test]
fn get_func_by_group_and_skill() {
    let mut d = with_classes();
    add(
        &mut d,
        FuncTemplate::FakeUse(DoodadFuncFakeUse {
            id: 1,
            skill_id: 0,
            fake_skill_id: 500,
            target_parent: false,
        }),
    );
    add(
        &mut d,
        FuncTemplate::Use(DoodadFuncUse {
            id: 2,
            skill_id: 600,
        }),
    );
    add(
        &mut d,
        FuncTemplate::Coffer(DoodadFuncCoffer {
            id: 3,
            capacity: 10,
        }),
    );
    let funcs = vec![
        func(10, 1, 3, "DoodadFuncCoffer", -1, 0),
        func(11, 1, 1, "DoodadFuncFakeUse", 5, 0),
        func(12, 1, 2, "DoodadFuncUse", -1, 0),
        func(13, 1, 9, "DoodadFuncLootItem", 7, 42),
    ];
    d.funcs_by_groups.insert(1, funcs);
    let key = |d: &DoodadData, skill| d.func_for_skill(1, skill).map(|f| f.func_key);
    // SkillId match first, in list order (func 10 has SkillId 0 == skill 0).
    assert_eq!(key(&d, 0), Some(10));
    assert_eq!(key(&d, 42), Some(13));
    assert_eq!(key(&d, 500), Some(11));
    assert_eq!(key(&d, 600), Some(12));
    // Fallback: first SkillId == 0 with NextPhase != -1, then first SkillId == 0.
    assert_eq!(key(&d, 777), Some(11));
    d.funcs_by_groups.get_mut(&1).unwrap()[1].next_phase = -1;
    assert_eq!(key(&d, 777), Some(10));
    // No skill-less func.
    d.funcs_by_groups.insert(2, vec![func(20, 2, 9, "X", 1, 5)]);
    assert_eq!(d.func_for_skill(2, 6), None);
    assert_eq!(d.func_for_skill(3, 0), None);
    // FakeSkillId 0 never matches through the template.
    add(
        &mut d,
        FuncTemplate::FakeUse(DoodadFuncFakeUse {
            id: 4,
            ..Default::default()
        }),
    );
    d.funcs_by_groups
        .insert(4, vec![func(30, 4, 4, "DoodadFuncFakeUse", 1, 9)]);
    assert_eq!(d.func_for_skill(4, 0), None);
    // A phase template class in doodad_funcs is not found by GetFuncTemplate.
    add(
        &mut d,
        FuncTemplate::DeclareSiege(DoodadFuncDeclareSiege { id: 5 }),
    );
    assert!(d.func_template(5, "DoodadFuncDeclareSiege").is_none());
    assert!(d.phase_func_template(5, "DoodadFuncDeclareSiege").is_some());
}

#[test]
fn template_caches_and_coffers() {
    let mut d = with_classes();
    add(
        &mut d,
        FuncTemplate::Growth(DoodadFuncGrowth {
            id: 1,
            delay: 1000,
            ..Default::default()
        }),
    );
    add(
        &mut d,
        FuncTemplate::Growth(DoodadFuncGrowth {
            id: 2,
            delay: 500,
            ..Default::default()
        }),
    );
    add(
        &mut d,
        FuncTemplate::Coffer(DoodadFuncCoffer {
            id: 1,
            capacity: 50,
        }),
    );
    add(
        &mut d,
        FuncTemplate::Coffer(DoodadFuncCoffer { id: 2, capacity: 0 }),
    );
    d.funcs_by_groups
        .insert(100, vec![func(1, 100, 1, "DoodadFuncGrowth", -1, 0)]);
    d.funcs_by_groups
        .insert(101, vec![func(2, 101, 2, "DoodadFuncGrowth", -1, 0)]);
    d.funcs_by_groups
        .insert(200, vec![func(3, 200, 2, "DoodadFuncCoffer", -1, 0)]);
    d.funcs_by_groups
        .insert(201, vec![func(4, 201, 1, "DoodadFuncCoffer", -1, 0)]);
    let g = |id, almighty| DoodadFuncGroups {
        id,
        almighty,
        ..Default::default()
    };
    d.all_func_groups = vec![g(100, 1), g(101, 1), g(200, 2), g(201, 2), g(300, 3)];
    for t in [1, 2, 3] {
        let func_groups = d
            .all_func_groups
            .iter()
            .filter(|x| x.almighty == t)
            .cloned()
            .collect();
        d.templates.insert(
            t,
            DoodadTemplate {
                id: t,
                growth_time: 77,
                func_groups,
                ..Default::default()
            },
        );
    }
    d.create_template_caches();
    assert_eq!(d.templates[&1].total_doodad_growth_time, 1500);
    assert_eq!(d.templates[&2].total_doodad_growth_time, 77);
    // First coffer in group order wins, even with capacity 0.
    assert_eq!(d.is_coffer_template(2), 0);
    assert_eq!(d.is_coffer_template(1), -1);
    assert_eq!(d.is_coffer_template(0), -1);
    let caps = d.coffer_capacities();
    for t in [1, 2, 3] {
        assert_eq!(caps.get(&t).copied().unwrap_or(-1), d.is_coffer_template(t));
    }
}

#[test]
fn tod_hours() {
    assert_eq!(tod_as_hours(1, 2000), 20.0);
    assert_eq!(tod_as_hours(1, 930), 9.5);
    // 24000 -> 2400 -> 240: 02:40
    assert_eq!(tod_as_hours(1, 24000), 2.0 + 40.0 / 60.0);
    assert_eq!(tod_as_hours(1, 175), 1.25); // 01:75, minutes 75 % 60 = 15
}

#[test]
fn partial_load_from_sqlite() {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch(
            "CREATE TABLE doodad_func_groups (id INT, model TEXT, doodad_almighty_id INT, doodad_func_group_kind_id INT, sound_id INT, phase_msg TEXT, is_msg_to_zone TEXT);
             INSERT INTO doodad_func_groups VALUES (11, 'm', 2, 2, NULL, NULL, 'f'), (10, 'm', 2, 1, 5, 'door opened', 't'), (12, 'm', 1, 1, NULL, '  ', NULL), (10, 'dup', 1, 1, NULL, NULL, NULL),
                  (17231, 'prefab://prefabs/fish.xml/fish_school.ocean_inactive', 6448, 2, NULL, NULL, 'f');
             CREATE TABLE doodad_funcs (id INT, doodad_func_group_id INT, actual_func_id INT, actual_func_type TEXT, next_phase INT, sound_id INT, func_skill_id INT, perm_id INT, act_count INT);
             INSERT INTO doodad_funcs VALUES (2, 10, 9, 'DoodadFuncFakeUse', NULL, NULL, NULL, 1, NULL), (1, 10, 3, 'DoodadFuncCoffer', 11, 4, 77, 0, 2);
             CREATE TABLE doodad_func_coffers (id INT, capacity INT);
             INSERT INTO doodad_func_coffers VALUES (3, 150);
             CREATE TABLE doodad_func_fake_uses (id INT, skill_id INT, fake_skill_id INT, target_parent TEXT);
             INSERT INTO doodad_func_fake_uses VALUES (9, NULL, 13162, 't');
             CREATE TABLE doodad_groups (id INT, guard_on_field_time INT, is_export TEXT, removed_by_house TEXT);
             INSERT INTO doodad_groups VALUES (24, 0, 't', 1), (24, 5, 'f', 0);
             CREATE TABLE doodad_almighties (id INT, once_one_man TEXT, once_one_interaction TEXT, mgmt_spawn TEXT, percent INT, min_time INT, max_time INT, model_kind_id INT, use_creator_faction TEXT, force_tod_top_priority TEXT, milestone_id INT, group_id INT, use_target_decal TEXT, use_target_silhouette TEXT, use_target_highlight TEXT, target_decal_size REAL, sim_radius INT, collide_ship TEXT, collide_vehicle TEXT, climate_id INT, save_indun TEXT, force_up_action TEXT, parentable TEXT, childable TEXT, faction_id INT, growth_time INT, despawn_on_collision TEXT, no_collision TEXT, restrict_zone_id INT);
             INSERT INTO doodad_almighties VALUES (2, 't', 'f', 'f', NULL, NULL, NULL, 1, 'f', 'f', NULL, 24, 't', 'f', 'f', NULL, NULL, 'f', 'f', NULL, 'f', 'f', 'f', 'f', 0, 3000, 'f', 'f', NULL),
                                                  (1, 'f', 'f', 'f', 5, 1, 2, 1, 'f', 'f', NULL, 24, 't', 'f', 'f', 1.5, 3, 'f', 'f', 2, 'f', 'f', 'f', 'f', 101, NULL, 'f', 'f', 7);",
        )
        .unwrap();
    let mut d = with_classes();
    d.load_func_groups_discarded(&c).unwrap();
    d.load_funcs(&c).unwrap();
    load_templates(
        &c,
        &mut d.phase_func_templates,
        "SELECT * FROM doodad_func_coffers",
        |r| {
            Ok(FuncTemplate::Coffer(DoodadFuncCoffer {
                id: r.u32("id")?,
                capacity: r.i32("capacity")?,
            }))
        },
    )
    .unwrap();
    load_templates(
        &c,
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
    )
    .unwrap();
    // Duplicate template id: Dictionary.Add throws.
    assert!(load_templates(
        &c,
        &mut d.phase_func_templates,
        "SELECT * FROM doodad_func_coffers",
        |r| {
            Ok(FuncTemplate::Coffer(DoodadFuncCoffer {
                id: r.u32("id")?,
                capacity: 0,
            }))
        }
    )
    .is_err());
    d.load_all_func_groups(&c).unwrap();
    d.load_doodad_templates(&c).unwrap();
    d.create_template_caches();

    // ORDER BY doodad_func_group_id, actual_func_id.
    let keys: Vec<u32> = d.funcs_for_group(10).iter().map(|f| f.func_key).collect();
    assert_eq!(keys, [1, 2]);
    let f = d.func(1).unwrap();
    assert_eq!(
        (f.next_phase, f.sound_id, f.skill_id, f.count),
        (11, 4, 77, 2)
    );
    let f = d.func(2).unwrap();
    assert_eq!(
        (f.next_phase, f.sound_id, f.skill_id, f.count),
        (-1, 0, 0, 0)
    );
    // Func groups: row order, duplicate id skipped, fish models kept.
    let ids: Vec<u32> = d.all_func_groups.iter().map(|g| g.id).collect();
    assert_eq!(ids, [11, 10, 12, 17231]);
    assert_eq!(d.func_groups_ids(2), [11, 10]);
    assert!(d.doodad_func_groups(2).iter().all(|g| g.model.is_none()));
    assert_eq!(
        d.func_group(17231).unwrap().model.as_deref(),
        Some("prefab://prefabs/fish.xml/fish_school.ocean_inactive")
    );
    assert_eq!(d.func_group(10).unwrap().sound_id, 5);
    // phase_msg / is_msg_to_zone: NULL tolerant, `'t'`/`'f'` decoded like the other text flags.
    let g10 = d.func_group(10).unwrap();
    assert_eq!(g10.phase_msg, "door opened");
    assert!(g10.is_msg_to_zone);
    assert_eq!(d.func_group(11).unwrap().phase_msg, "");
    assert!(!d.func_group(11).unwrap().is_msg_to_zone);
    assert_eq!(d.func_group(12).unwrap().phase_msg, "  ");
    assert!(!d.func_group(12).unwrap().is_msg_to_zone);
    // Coffer template through doodad_funcs -> phase template.
    let t2 = d.template(2).unwrap();
    assert_eq!(t2.coffer_capacity, Some(150));
    assert!(t2.once_one_man && t2.use_target_decal && !t2.once_one_interaction);
    // doodad_groups: TryAdd keeps the first row; GetBoolean(col) on 't' is false.
    assert_eq!(
        t2.group,
        DoodadGroups {
            id: 24,
            guard_on_field_time: 0,
            is_export: false,
            removed_by_house: true
        }
    );
    assert_eq!(t2.total_doodad_growth_time, 3000);
    let t1 = d.template(1).unwrap();
    assert_eq!(t1.coffer_capacity, None);
    assert_eq!(
        (
            t1.percent,
            t1.min_time,
            t1.max_time,
            t1.sim_radius,
            t1.climate_id
        ),
        (5, 1, 2, 3, 2)
    );
    assert_eq!(
        (t1.faction_id, t1.growth_time, t1.restrict_zone_id),
        (101, 0, 7)
    );
    assert_eq!(t1.target_decal_size, 1.5);
    assert_eq!(d.template_order, [2, 1]);
    assert_eq!(d.func_for_skill(10, 13162).map(|f| f.func_key), Some(2));
    assert_eq!(d.func_for_skill(10, 77).map(|f| f.func_key), Some(1));
    // Missing doodad group: GameException.
    c.execute_batch("UPDATE doodad_almighties SET group_id = 99 WHERE id = 1")
        .unwrap();
    let mut d2 = with_classes();
    assert!(d2.load_doodad_templates(&c).is_err());
}

fn count(c: &Connection, table: &str) -> usize {
    c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap() as usize
}

#[test]
#[ignore]
fn real_compact_db_doodads() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let c = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let start = std::time::Instant::now();
    let d = DoodadData::load_from(&c).unwrap();
    eprintln!("doodad data loaded in {:?}", start.elapsed());

    for (table, class) in TEMPLATE_TABLES {
        let dict = if d.func_templates.contains_key(class) {
            &d.func_templates
        } else {
            &d.phase_func_templates
        };
        assert_eq!(dict[class].len(), count(&c, table), "{table}");
    }
    assert!(d.func_templates["DoodadFuncAuctionUi"].is_empty());
    assert!(d.func_templates["DoodadFuncBankUi"].is_empty());
    // ... but their func rows are loaded: the bank / auction checks match them by type
    // (`economy::store::near_service`). Group 22065 = the Fellowship Plaza's
    // "Auctioneer/Warehouse" (7983).
    let plaza: Vec<&str> = d
        .funcs_for_group(22065)
        .iter()
        .map(|f| f.func_type.as_str())
        .collect();
    assert!(plaza.contains(&"DoodadFuncBankUi"), "{plaza:?}");
    assert!(plaza.contains(&"DoodadFuncAuctionUi"), "{plaza:?}");
    assert_eq!(d.templates.len(), count(&c, "doodad_almighties"));
    assert_eq!(d.all_func_groups.len(), count(&c, "doodad_func_groups"));
    // WP-UI1: the authored phase messages (research evidence: 160 groups, 29 of them
    // zone-wide on 5 templates).
    let phase_msgs = d
        .all_func_groups
        .iter()
        .filter(|g| !g.phase_msg.trim().is_empty())
        .count();
    assert_eq!(phase_msgs, 160);
    let zone_msgs = d
        .all_func_groups
        .iter()
        .filter(|g| g.is_msg_to_zone && !g.phase_msg.trim().is_empty())
        .count();
    assert_eq!(zone_msgs, 29);
    assert_eq!(d.funcs_by_id.len(), count(&c, "doodad_funcs"));
    let n_funcs: usize = d.funcs_by_groups.values().map(Vec::len).sum();
    assert_eq!(n_funcs, count(&c, "doodad_funcs"));
    let n_phase: usize = d.phase_funcs.values().map(Vec::len).sum();
    assert_eq!(n_phase, count(&c, "doodad_phase_funcs"));
    assert_eq!(d.doodad_groups.len(), count(&c, "doodad_groups"));
    // WP-W1: `doodad_place_skins` (2,361 rows, 755 doodads; 2 duplicate almighty+kind rows).
    let place_skin_rows: usize = d.place_skins.values().map(Vec::len).sum();
    assert_eq!(place_skin_rows, 2_361);
    assert_eq!(place_skin_rows, count(&c, "doodad_place_skins"));
    assert_eq!(d.place_skins.len(), 755);
    let kind_count = |kind: u32| {
        d.place_skins
            .values()
            .flatten()
            .filter(|&&value| value == kind)
            .count()
    };
    assert_eq!(kind_count(1), 748);
    assert_eq!(kind_count(2), 584);
    assert_eq!(kind_count(3), 585);
    assert_eq!(kind_count(4), 444);
    let kinds = d.place_skins.get(&1677).cloned().unwrap_or_default();
    assert_eq!(kinds, vec![1, 2, 3, 4]);
    assert_eq!(
        d.consume_changer_items.len(),
        count(&c, "doodad_func_consume_changer_items")
    );
    let effects: usize = d.phase_func_templates["DoodadFuncClout"]
        .values()
        .map(|t| match t {
            FuncTemplate::Clout(c) => c.effects.len(),
            _ => unreachable!(),
        })
        .sum();
    assert_eq!(effects, count(&c, "doodad_func_clout_effects"));
    eprintln!(
            "templates {}, func groups {}, funcs {}, phase funcs {}, func templates {}, phase func templates {}, clout effects {}",
            d.templates.len(),
            d.all_func_groups.len(),
            d.funcs_by_id.len(),
            n_phase,
            d.func_templates.values().map(HashMap::len).sum::<usize>(),
            d.phase_func_templates.values().map(HashMap::len).sum::<usize>(),
            effects,
        );

    // Template 2336 (water the flowerbed), see the comment at the top of Doodad.cs.
    let t = d.template(2336).unwrap();
    assert_eq!(t.group_id, 24);
    assert_eq!(d.func_groups_ids(2336), [4651, 4652, 5136, 5137]);
    let start_group = d.func_group(4651).unwrap();
    assert_eq!(start_group.group_kind_id, doodad_func_group_kind::START);
    let pf = d.phase_funcs(4651);
    assert_eq!(pf.len(), 1);
    assert_eq!(
        (pf[0].func_id, pf[0].func_type.as_str()),
        (250, "DoodadFuncTod")
    );
    match d.phase_func_template(250, "DoodadFuncTod") {
        Some(FuncTemplate::Tod(tod)) => {
            assert_eq!(
                (tod.tod, tod.next_phase, tod.tod_as_hours),
                (2000, 5136, 20.0)
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(d.func_template(250, "DoodadFuncTod").is_none());
    let funcs = d.funcs_for_group(4651);
    assert_eq!(funcs.len(), 1);
    let f = &funcs[0];
    assert_eq!(
        (
            f.func_key,
            f.func_id,
            f.func_type.as_str(),
            f.next_phase,
            f.skill_id
        ),
        (3914, 543, "DoodadFuncFakeUse", 4652, 0)
    );
    match d.func_template(543, "DoodadFuncFakeUse") {
        Some(FuncTemplate::FakeUse(u)) => {
            assert_eq!(
                (u.skill_id, u.fake_skill_id, u.target_parent),
                (0, 13162, false)
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        d.func_for_skill(4651, 13162).map(|f| f.func_key),
        Some(3914)
    );
    assert_eq!(d.func_for_skill(4651, 1).map(|f| f.func_key), Some(3914));
    let p = d.phase_funcs(4652).first().expect("phase func of 4652");
    assert_eq!((p.func_id, p.func_type.as_str()), (822, "DoodadFuncTimer"));
    // "DoodadFuncTreeByproductsCollect" (DB spelling) misses the C# class name.
    assert!(d
        .func_templates
        .contains_key("DoodadFuncTreeByProductsCollect"));
    assert!(!d
        .func_templates
        .contains_key("DoodadFuncTreeByproductsCollect"));

    // Coffers: templates with a DoodadFuncCoffer reachable through doodad_funcs.
    let coffers: Vec<_> = d.templates.values().filter(|t| t.is_coffer()).collect();
    assert!(!coffers.is_empty());
    for t in &coffers {
        assert_eq!(Some(d.is_coffer_template(t.id)), t.coffer_capacity);
    }
    let mut list: Vec<_> = coffers.iter().map(|t| (t.id, t.coffer_capacity)).collect();
    list.sort_unstable();
    eprintln!("coffer templates: {list:?}");
    assert_eq!(d.treasure_chest_template_ids().len(), 12);
    // No DoodadFuncGrowth in doodad_funcs: TotalDoodadGrowthTime is always GrowthTime.
    assert!(d
        .templates
        .values()
        .all(|t| t.total_doodad_growth_time == t.growth_time));
}
