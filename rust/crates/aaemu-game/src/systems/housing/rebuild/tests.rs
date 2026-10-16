//! Rebuild tests: the data loader, the in-place swap (materials, labor, furniture, packets to
//! viewers), the validation failures, the preview and the extension channels. Child module of
//! `housing::tests`, so it reuses that file's synthetic world and economy.

use std::collections::BTreeMap;

use rusqlite::Connection;
use serde_json::{json, Value};

use super::*;
use crate::models::house::housing_permission;
use crate::systems::ext;
use crate::systems::housing::rebuild::data::{RebuildData, RebuildMaterial, Rebuilding};
use crate::systems::housing::rebuild::ops as rops;
use crate::systems::housing::rebuild::ops::RebuildError;
use crate::systems::housing::rebuild::preview;
use crate::systems::housing::rebuild::PREVIEW_TIMEOUT_MS;
use crate::systems::npcs::doodad::manager as doodads;

const TIER2: u32 = 302;
const TIER2_DOODAD: u32 = 4002;
const MAT_A: u32 = 8101;
const MAT_B: u32 = 8102;
const REBUILD_TIER2: u32 = 70;
const REBUILD_FARM: u32 = 71;
const REBUILD_MISSING: u32 = 72;

/// The housing fixture data plus a tier-2 template (one construction step, like the 10.x
/// `_rebuild` targets, and its own bound doodad) and rebuilding options for [`DESIGN`].
fn rebuild_data() -> GameData {
    let mut data = housing_data(false);
    let taxation = Taxation {
        id: 3,
        tax: 250_000,
        show: true,
    };
    let h = &mut data.systems.housing;
    h.taxations.insert(3, taxation);
    let mut build_steps = BTreeMap::new();
    build_steps.insert(
        0,
        HousingBuildStep {
            id: 900,
            housing_id: TIER2,
            step: 0,
            model_id: 1085,
            skill_id: 29291,
            num_actions: 1,
        },
    );
    h.add_template(HousingTemplate {
        id: TIER2,
        name: "Tier Two Farm".into(),
        main_model_id: 900,
        hp: 1500,
        garden_radius: 11.0,
        taxation: Some(taxation),
        taxation_id: 3,
        build_steps,
        binding_doodads: vec![HousingBindingDoodad {
            attach_point_id: 12,
            doodad_id: TIER2_DOODAD,
            position: WorldSpawnPosition::default(),
        }],
        ..Default::default()
    });
    h.rebuild.add(
        DESIGN,
        7,
        vec![
            Rebuilding {
                id: REBUILD_TIER2,
                name: "Upgrade".into(),
                skill_id: 28829,
                housing_id: TIER2,
                materials: vec![
                    RebuildMaterial {
                        item_id: MAT_A,
                        count: 5,
                    },
                    RebuildMaterial {
                        item_id: MAT_B,
                        count: 2,
                    },
                ],
                ..Default::default()
            },
            Rebuilding {
                id: REBUILD_FARM,
                name: String::new(),
                skill_id: 28828,
                housing_id: FARM,
                labor_power: 10,
                ..Default::default()
            },
            Rebuilding {
                id: REBUILD_MISSING,
                housing_id: 999,
                ..Default::default()
            },
        ],
    );
    h.rebuild
        .set_skill_name(28829, "Extend Building Performance");
    h.set_house_name(DESIGN, "Design House");
    h.set_house_name(TIER2, "Tier Two Farm");
    data.items.templates.insert(
        MAT_A,
        crate::data::items::ItemTemplate {
            name: "Mat A".into(),
            ..crate::data::items::ItemTemplate::plain(MAT_A, 1000)
        },
    );
    data.systems.npcs.doodad.templates.insert(
        TIER2_DOODAD,
        DoodadTemplate {
            id: TIER2_DOODAD,
            ..Default::default()
        },
    );
    data
}

fn rebuild_fixture() -> Fx {
    let fx = fixture_with(rebuild_data());
    fx.eco.item_counts.lock().unwrap().insert(MAT_A, 12);
    fx.eco.item_counts.lock().unwrap().insert(MAT_B, 2);
    fx.pa.data().labor_power = 50;
    fx
}

/// A finished [`DESIGN`] house of Alpha at (105, 100) with a chair as furniture.
fn house_with_chair(fx: &Fx) -> (HouseRef, crate::world::DoodadRef) {
    let house = place_house(fx, DESIGN, &fx.pa, 105.0, 100.0, -1);
    let world = fx.server.world.world(0).unwrap();
    let house_obj = WorldObject::from(&house);
    let chair = doodads::create(&fx.server, &world, 0, CHAIR_DOODAD, Some(&house_obj), true)
        .expect("chair");
    {
        let mut d = chair.data();
        d.owner_db_id = house.data().id;
        d.parent_obj_id = house.obj_id;
        d.attach_point = crate::models::unit::attach_point::NONE;
        d.owner_type = crate::models::doodad::doodad_owner_type::HOUSING;
        d.transform.set_position(1.0, 2.0, 3.0, 0.0, 0.0, 0.0);
    }
    world.attach_child(&house_obj, &WorldObject::from(&chair));
    doodads::spawn(&fx.server, &world, &chair);
    (house, chair)
}

fn bodies(frames: &[(u16, Vec<u8>)], op: u16) -> Vec<Vec<u8>> {
    frames
        .iter()
        .filter(|f| f.0 == op)
        .map(|f| f.1.clone())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// data
// ---------------------------------------------------------------------------------------------

#[test]
fn missing_tables_and_columns_yield_an_empty_rebuild_set() {
    // Vanilla 1.2: no rebuild tables, no pack column.
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch("CREATE TABLE housings (id INT, name TEXT);")
        .unwrap();
    let d = RebuildData::load(&c, "en_us").unwrap();
    assert!(d.is_empty());
    assert!(d.options_for(171).is_empty());
    assert_eq!(d.pack_of(171), None);
    // Not even a housings table.
    let d = RebuildData::load(&Connection::open_in_memory().unwrap(), "en_us").unwrap();
    assert!(d.is_empty());
    // The pack column without any rebuild table: the mapping exists, no options.
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE housings (id INT, housing_rebuilding_pack_id INT);
         INSERT INTO housings VALUES (171, 2), (172, NULL), (173, 0);",
    )
    .unwrap();
    let d = RebuildData::load(&c, "en_us").unwrap();
    assert_eq!(d.pack_of(171), Some(2));
    assert_eq!(d.pack_of(172), None);
    assert_eq!(d.pack_of(173), None);
    assert!(d.options_for(171).is_empty());
}

#[test]
fn present_tables_are_parsed_in_pack_order() {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE housings (id INT, housing_rebuilding_pack_id INT);
         CREATE TABLE housing_rebuilding_packs (id INT, name TEXT);
         CREATE TABLE housing_rebuilding_pack_rebuildings
            (id INT, housing_rebuilding_pack_id INT, housing_rebuilding_id INT, position INT);
         CREATE TABLE housing_rebuildings (id INT, name TEXT, skill_id INT, housing_id INT,
            labor_power INT, actability_group_id INT, change_point_desc TEXT);
         CREATE TABLE housing_rebuilding_materials
            (id INT, housing_rebuilding_id INT, item_id INT, count INT);
         CREATE TABLE localized_texts (id INT, tbl_name TEXT, tbl_column_name TEXT, idx INT,
            ko TEXT, en_us TEXT);
         INSERT INTO housings VALUES (171, 2), (330, 2), (432, 88);
         INSERT INTO housing_rebuilding_packs VALUES (2, 'farm'), (88, 'tier2');
         INSERT INTO housing_rebuilding_pack_rebuildings VALUES
            (7, 2, 5, 3), (5, 2, 3, 1), (6, 2, 4, 2), (8, 88, 33, 1), (9, 2, 404, 4);
         INSERT INTO housing_rebuildings VALUES
            (3, 'ko three', 28829, 432, 0, NULL, 'desc3'),
            (4, 'ko four', 28829, 433, 0, NULL, ''),
            (5, 'ko five', 28829, 434, 7, 12, ''),
            (33, 'side', 28828, 433, 0, NULL, '');
         INSERT INTO housing_rebuilding_materials VALUES
            (1, 5, 34983, 15), (2, 5, 47083, 6), (3, 3, 34983, 15), (4, 99, 1, 1);
         INSERT INTO localized_texts VALUES
            (1, 'housing_rebuilding', 'name', 3, '', 'wrong table'),
            (2, 'housing_rebuildings', 'name', 3, '', 'Wilderness Farm'),
            (3, 'housing_rebuildings', 'name', 4, '', ''),
            (4, 'skills', 'name', 28829, '', 'Extend Building Performance'),
            (5, 'skills', 'name', 28828, '', ''),
            (6, 'skills', 'name', 1, '', 'Unrelated skill'),
            (7, 'skills', 'desc', 28829, '', 'not a name');",
    )
    .unwrap();
    let d = RebuildData::load(&c, "en_us").unwrap();
    assert_eq!(d.rebuilding_count(), 4);
    assert_eq!(d.pack_of(171), Some(2));
    assert_eq!(d.pack_of(330), Some(2));
    let ids: Vec<u32> = d.options_for(171).iter().map(|b| b.id).collect();
    // By `position`; the entry pointing at a missing housing_rebuildings row is skipped.
    assert_eq!(ids, [3, 4, 5]);
    assert_eq!(
        d.options_for(432).iter().map(|b| b.id).collect::<Vec<_>>(),
        [33]
    );
    assert!(d.options_for(999).is_empty());
    let r3 = d.rebuilding(3).unwrap();
    assert_eq!(
        (r3.skill_id, r3.housing_id, r3.labor_power, r3.name.as_str()),
        (28829, 432, 0, "Wilderness Farm")
    );
    assert_eq!(r3.change_point_desc, "desc3");
    assert_eq!(
        r3.materials,
        [RebuildMaterial {
            item_id: 34983,
            count: 15
        }]
    );
    let r5 = d.rebuilding(5).unwrap();
    assert_eq!((r5.labor_power, r5.actability_group_id), (7, 12));
    assert_eq!(r5.materials.len(), 2);
    // An empty translation keeps the original name.
    assert_eq!(d.rebuilding(4).unwrap().name, "ko four");
    // Skill names are loaded for the skills the rebuildings use, and only `name` texts.
    assert_eq!(d.skill_name(28829), "Extend Building Performance");
    assert_eq!(d.skill_name(28828), "");
    assert_eq!(d.skill_name(1), "");
    assert!(d.option_for(171, 33).is_none());
    assert!(d.option_for(171, 5).is_some());
}

// ---------------------------------------------------------------------------------------------
// the swap
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn rebuild_swaps_the_template_in_place_and_redraws() {
    let mut fx = rebuild_fixture();
    let (house, chair) = house_with_chair(&fx);
    let world = fx.server.world.world(0).unwrap();
    let (obj_id, db_id, tl) = {
        let h = house.data();
        (house.obj_id, h.id, h.unit.tl_id)
    };
    house.data().permission = housing_permission::GUILD;
    house.data().unit.name = "My Home".into();
    let old_bound = fx.server.systems.housing.manager.attached(obj_id);
    assert_eq!(old_bound.len(), 1);
    let (old_bound_id, protection, place) = {
        let h = house.data();
        (old_bound[0].obj_id, h.protection_end_date, h.place_date)
    };
    let pos_before = house.data().unit.transform;
    fx.a.drain();
    fx.b.drain();
    fx.eco.calls();

    let target = rops::rebuild(&fx.server, &fx.pa, &house, REBUILD_TIER2).unwrap();
    assert_eq!(target, TIER2);

    // Materials consumed through the economy hook (nothing else).
    assert_eq!(
        fx.eco.calls(),
        [
            format!("consume 33 {MAT_A} 5 None"),
            format!("consume 33 {MAT_B} 2 None")
        ]
    );
    assert_eq!(fx.eco.item_counts.lock().unwrap()[&MAT_A], 7);
    assert_eq!(fx.pa.data().labor_power, 50); // this option costs no labor

    // The SAME entity: identity, owner, position, permissions, dates and name are kept.
    let same = fx.server.systems.housing.manager.by_id(db_id).unwrap();
    assert!(std::sync::Arc::ptr_eq(&same, &house));
    {
        let h = house.data();
        assert_eq!((h.id, h.unit.tl_id), (db_id, tl));
        assert_eq!(house.obj_id, obj_id);
        assert_eq!(h.unit.owner_id, 1);
        assert_eq!(h.permission, housing_permission::GUILD);
        assert_eq!(h.unit.name, "My Home"); // a custom name is kept
        assert_eq!((h.protection_end_date, h.place_date), (protection, place));
        assert_eq!(h.unit.transform.x, pos_before.x);
        assert_eq!(h.unit.transform.rot_z, pos_before.rot_z);
        // Template-derived fields follow the new template; the construction step is open.
        assert_eq!(h.unit.template_id, TIER2);
        assert_eq!(h.current_step, 0);
        assert_eq!(h.build_step_count, 1);
        assert_eq!(h.all_action, 1);
        assert_eq!((h.base_action, h.num_action), (0, 0));
        assert_eq!(h.tax, 250_000);
        assert_eq!((h.unit.hp, h.unit.max_hp), (1500, 1500));
        assert_eq!(h.unit.model_id, 1085);
        assert!(h.is_dirty);
        assert!(h.attached_doodads.is_empty());
    }
    // Old bindings are gone, furniture stays parented to the house.
    assert!(fx
        .server
        .systems
        .housing
        .manager
        .attached(obj_id)
        .is_empty());
    assert!(world.doodad(old_bound_id).is_none());
    assert!(world.doodad(chair.obj_id).is_some());
    assert!(house
        .core()
        .children()
        .iter()
        .any(|c| c.obj_id() == chair.obj_id));
    assert_eq!(chair.data().parent_obj_id, obj_id);

    // The viewer (Beta) saw the house vanish and reappear as the new template; so did the owner.
    let frames = fx.b.drain();
    let ops_seen = ops_of(&frames);
    let removed = ops_seen
        .iter()
        .position(|o| *o == sc::SCUnitsRemovedPacket)
        .expect("house removed");
    let state = ops_seen
        .iter()
        .rposition(|o| *o == sc::SCHouseStatePacket)
        .expect("house shown again");
    assert!(removed < state, "{ops_seen:x?}");
    assert_eq!(
        bodies(&frames, sc::SCHouseStatePacket).last().unwrap(),
        &ops::house_body(&fx.server, &house)
    );
    // The furniture is re-created for the viewer.
    assert!(ops_seen.contains(&sc::SCDoodadCreatedPacket));
    let owner_frames = fx.a.drain();
    assert_eq!(
        body_of(&owner_frames, sc::SCMyHousePacket),
        ops::house_body(&fx.server, &house)
    );

    // The owner finishes the construction step through the normal build path.
    assert!(ops::craft_building(&fx.server, &fx.pa, &house, 29291));
    assert_eq!(house.data().current_step, -1);
    let bound = fx.server.systems.housing.manager.attached(obj_id);
    assert_eq!(bound.len(), 1);
    assert_eq!(bound[0].data().template_id, TIER2_DOODAD);
    assert_eq!(house.data().unit.model_id, 900);
}

#[tokio::test]
async fn rebuild_into_a_finished_target_pays_labor_and_forces_public() {
    let fx = rebuild_fixture();
    let (house, chair) = house_with_chair(&fx);
    house.data().permission = housing_permission::PRIVATE;
    fx.eco.calls();
    assert_eq!(
        rops::rebuild(&fx.server, &fx.pa, &house, REBUILD_FARM),
        Ok(FARM)
    );
    // No materials, labor 10.
    assert!(fx.eco.calls().is_empty());
    assert_eq!(fx.pa.data().labor_power, 40);
    let h = house.data();
    assert_eq!((h.unit.template_id, h.current_step), (FARM, -1));
    assert_eq!(h.permission, housing_permission::PUBLIC); // always_public
    assert_eq!(h.all_action, 0);
    assert_eq!(h.unit.model_id, 800);
    assert!(h.attached_doodads.is_empty()); // the old template's binding is gone
    drop(h);
    assert!(fx
        .server
        .world
        .world(0)
        .unwrap()
        .doodad(chair.obj_id)
        .is_some());
}

#[tokio::test]
async fn gm_force_rebuild_needs_no_materials_or_ownership() {
    let fx = rebuild_fixture();
    let house = place_house(&fx, DESIGN, &fx.pb, 105.0, 100.0, 1); // unfinished, Beta's
    fx.eco.item_counts.lock().unwrap().clear();
    fx.pa.data().labor_power = 0;
    assert_eq!(rops::force_rebuild(&fx.server, &house, TIER2), Ok(()));
    assert_eq!(house.data().unit.template_id, TIER2);
    assert_eq!(house.data().current_step, 0);
    assert!(fx.eco.calls().is_empty());
    // Nonexistent targets are refused and change nothing.
    assert_eq!(
        rops::force_rebuild(&fx.server, &house, 12345),
        Err(RebuildError::NoTarget)
    );
    assert_eq!(house.data().unit.template_id, TIER2);
    // Forts and lodestones are never rebuild targets (and never rebuilt themselves).
    let mut data = rebuild_data();
    data.systems.housing.add_template(HousingTemplate {
        id: 500,
        guard_tower_setting_id: 6,
        ..Default::default()
    });
    let fx2 = fixture_with(data);
    let h2 = place_house(&fx2, DESIGN, &fx2.pa, 105.0, 100.0, -1);
    assert_eq!(
        rops::force_rebuild(&fx2.server, &h2, 500),
        Err(RebuildError::NoTarget)
    );
    assert_eq!(h2.data().unit.template_id, DESIGN);
}

#[tokio::test]
async fn rebuild_validation_failures_change_nothing() {
    let fx = rebuild_fixture();
    let (house, _) = house_with_chair(&fx);
    fx.eco.calls();
    let before = house.data().clone();
    let try_rebuild = |player: &PlayerRef, id: u32| rops::rebuild(&fx.server, player, &house, id);
    let assert_untouched = || {
        assert!(fx.eco.calls().is_empty(), "nothing may be consumed");
        assert_eq!(house.data().unit.template_id, before.unit.template_id);
        assert_eq!(house.data().current_step, before.current_step);
        assert_eq!(fx.pa.data().labor_power, 50);
    };

    // Not the owner.
    assert_eq!(
        try_rebuild(&fx.pb, REBUILD_TIER2),
        Err(RebuildError::NotOwner)
    );
    assert_untouched();
    // Not an option of this house / missing target.
    assert_eq!(try_rebuild(&fx.pa, 3), Err(RebuildError::UnknownRebuilding));
    assert_eq!(
        try_rebuild(&fx.pa, REBUILD_MISSING),
        Err(RebuildError::NoTarget)
    );
    assert_untouched();
    // Too few materials (nothing is partially consumed).
    fx.eco.item_counts.lock().unwrap().insert(MAT_B, 1);
    assert_eq!(
        try_rebuild(&fx.pa, REBUILD_TIER2),
        Err(RebuildError::Materials)
    );
    assert_untouched();
    assert_eq!(fx.eco.item_counts.lock().unwrap()[&MAT_A], 12);
    fx.eco.item_counts.lock().unwrap().insert(MAT_B, 2);
    // Too little labor.
    fx.pa.data().labor_power = 5;
    assert_eq!(try_rebuild(&fx.pa, REBUILD_FARM), Err(RebuildError::Labor));
    fx.pa.data().labor_power = 50;
    assert_untouched();
    // Too far away.
    fx.pa.data().unit.transform.set_xyz(400.0, 100.0, 100.0);
    assert_eq!(
        try_rebuild(&fx.pa, REBUILD_TIER2),
        Err(RebuildError::TooFar)
    );
    fx.pa.data().unit.transform.set_xyz(100.0, 100.0, 100.0);
    assert_untouched();
    // For sale.
    house.data().sell_price = 5000;
    assert_eq!(
        try_rebuild(&fx.pa, REBUILD_TIER2),
        Err(RebuildError::ForSale)
    );
    house.data().sell_price = 0;
    // Tax due (and overdue).
    let protection = house.data().protection_end_date;
    house.data().protection_end_date = manager::now() + 3600;
    assert_eq!(
        try_rebuild(&fx.pa, REBUILD_TIER2),
        Err(RebuildError::TaxDue)
    );
    house.data().protection_end_date = manager::now() - 10;
    assert_eq!(
        try_rebuild(&fx.pa, REBUILD_TIER2),
        Err(RebuildError::TaxDue)
    );
    house.data().protection_end_date = protection;
    assert_untouched();
    // Siege in the house's territory (zone group 25).
    install_siege(&fx, 25, true);
    assert_eq!(
        try_rebuild(&fx.pa, REBUILD_TIER2),
        Err(RebuildError::InSiege)
    );
    assert_untouched();
    install_siege(&fx, 25, false); // a finished siege does not block
    assert_eq!(try_rebuild(&fx.pa, REBUILD_TIER2), Ok(TIER2));
    // The rebuilt house is under construction (the target's open step): not finished, no
    // second rebuild.
    assert_eq!(house.data().current_step, 0);
    assert_eq!(
        try_rebuild(&fx.pa, REBUILD_TIER2),
        Err(RebuildError::NotFinished)
    );
}

/// Registers a dominion in zone group `group` whose siege is active (`active`) or finished.
fn install_siege(fx: &Fx, group: u32, active: bool) {
    use crate::systems::housing::dominion::state::{
        DominionState, SiegeOutcome, SiegeState, SiegeStatus, TowerIdentity,
    };
    let now = manager::now();
    let mut state = DominionState::unowned(
        group,
        TowerIdentity {
            house_id: 900,
            instance_id: 0,
            guard_tower_setting_id: 6,
        },
    );
    state.owner_expedition_id = 10;
    state.ownership_generation = 1;
    state.last_siege_id = 1;
    state.siege = Some(SiegeState {
        siege_id: 1,
        attacker_expedition_id: 20,
        defender_expedition_id: 10,
        ownership_generation: 1,
        status: if active {
            SiegeStatus::Active
        } else {
            SiegeStatus::Finished
        },
        outcome: if active {
            SiegeOutcome::Pending
        } else {
            SiegeOutcome::DefenderHeld
        },
        scheduled_at: now - 2000,
        starts_at: now - 1000,
        ends_at: now + 1000,
        ended_at: if active { 0 } else { now - 10 },
        participants: Default::default(),
    });
    fx.server
        .systems
        .housing
        .dominions
        .restore(vec![state])
        .expect("dominion state");
}

#[tokio::test]
async fn view_lists_options_with_inventory_counts_and_reasons() {
    let fx = rebuild_fixture();
    let (house, _) = house_with_chair(&fx);
    fx.eco.item_counts.lock().unwrap().insert(MAT_A, 3);
    fx.pa.data().labor_power = 4;
    let view = rops::view(&fx.server, &fx.pa, &house).unwrap();
    assert_eq!(view.house_id, house.data().id);
    assert_eq!(
        (view.template, view.tax, view.blocked),
        (DESIGN, 100_000, None)
    );
    assert_eq!(view.options.len(), 3);
    let tier2 = &view.options[0];
    assert_eq!((tier2.rebuilding, tier2.target), (REBUILD_TIER2, TIER2));
    assert_eq!(tier2.name, "Upgrade");
    assert_eq!(tier2.skill_name, "Extend Building Performance");
    assert_eq!(tier2.tax, 250_000);
    assert!(!tier2.can);
    assert_eq!(tier2.reason, Some(RebuildError::Materials));
    assert_eq!(
        tier2
            .materials
            .iter()
            .map(|m| (m.item, m.count, m.have))
            .collect::<Vec<_>>(),
        [(MAT_A, 5, 3), (MAT_B, 2, 2)]
    );
    let farm = &view.options[1];
    assert_eq!(farm.name, "Farm"); // no rebuilding name: the target's name
    assert_eq!(farm.skill_name, ""); // unknown skill text: the UI shows no skill line
    assert_eq!(farm.reason, Some(RebuildError::Labor));
    assert_eq!(view.options[2].reason, Some(RebuildError::NoTarget));
    fx.pa.data().labor_power = 50;
    assert!(rops::view(&fx.server, &fx.pa, &house).unwrap().options[1].can);
    // Only the owner sees them.
    assert_eq!(
        rops::view(&fx.server, &fx.pb, &house),
        Err(RebuildError::NotOwner)
    );
    // A house-level block applies to every option.
    house.data().sell_price = 1;
    let view = rops::view(&fx.server, &fx.pa, &house).unwrap();
    assert_eq!(view.blocked, Some(RebuildError::ForSale));
    assert!(view
        .options
        .iter()
        .all(|o| o.reason == Some(RebuildError::ForSale)));
}

// ---------------------------------------------------------------------------------------------
// preview
// ---------------------------------------------------------------------------------------------

fn house_state_ids(frames: &[(u16, Vec<u8>)]) -> usize {
    frames
        .iter()
        .filter(|f| f.0 == sc::SCHouseStatePacket)
        .count()
}

#[tokio::test]
async fn preview_shows_a_phantom_only_to_the_requester_and_restores_the_house() {
    let mut fx = rebuild_fixture();
    let (house, _) = house_with_chair(&fx);
    let world = fx.server.world.world(0).unwrap();
    let objects_before = world.object_count();
    let conn_id = fx.pa.conn.id;
    fx.a.drain();
    fx.b.drain();

    preview::start(&fx.server, &fx.pa, &house, REBUILD_TIER2).unwrap();
    let a_frames = fx.a.drain();
    // The real house (and children) leave Alpha's view, a house with a fresh object id and the
    // target template appears.
    let ops_seen = ops_of(&a_frames);
    assert!(
        ops_seen.contains(&sc::SCUnitsRemovedPacket),
        "{ops_seen:x?}"
    );
    assert_eq!(house_state_ids(&a_frames), 1);
    let state = body_of(&a_frames, sc::SCHouseStatePacket);
    let mut s = PacketStream::from_bytes(state.clone());
    let _tl = s.read_u16().unwrap();
    let db_id = s.read_u32().unwrap();
    let phantom_obj = s.read_bc().unwrap();
    let template = s.read_u32().unwrap();
    assert_eq!(db_id, house.data().id);
    assert_eq!(template, TIER2);
    // The nameplate carries the target template's name, not the old one.
    let contains = |name: &str| state.windows(name.len()).any(|w| w == name.as_bytes());
    assert!(contains("Tier Two Farm") && !contains("Design House"));
    assert_ne!(phantom_obj, house.obj_id);
    // The phantom never entered the world: no other object table entry, nobody else sees it.
    assert_eq!(world.object_count(), objects_before);
    assert!(world.house(phantom_obj).is_none());
    assert!(fx.b.drain().is_empty());
    // The real house is untouched.
    assert_eq!(house.data().unit.template_id, DESIGN);
    assert_eq!(
        fx.server.systems.housing.rebuild_previews.active(conn_id),
        Some((house.data().id, REBUILD_TIER2))
    );

    // Ending restores the real house for Alpha only and frees the phantom id.
    assert!(preview::end(&fx.server, conn_id, "ended"));
    let a_frames = fx.a.drain();
    let ops_seen = ops_of(&a_frames);
    assert!(ops_seen.contains(&sc::SCUnitsRemovedPacket)); // phantom removed
    assert_eq!(house_state_ids(&a_frames), 1); // the real house again
    let real = body_of(&a_frames, sc::SCHouseStatePacket);
    assert_eq!(real, ops::house_body(&fx.server, &house));
    assert!(fx.b.drain().is_empty());
    assert!(fx.server.systems.housing.rebuild_previews.is_empty());
    // The phantom id is released: the next allocation can reuse it.
    let next = fx.server.ids.object.get_next_id().unwrap();
    fx.server.ids.object.release_id(next);
    assert!(!preview::end(&fx.server, conn_id, "ended"), "no second end");
}

#[test]
fn a_retargeted_house_keeps_a_custom_name_and_follows_a_default_one() {
    let data = rebuild_data();
    let h = &data.systems.housing;
    let name = |current: &str| rops::retargeted_name(h, DESIGN, current, TIER2);
    assert_eq!(name("").as_deref(), Some("Tier Two Farm"));
    assert_eq!(name("Design House").as_deref(), Some("Tier Two Farm"));
    assert_eq!(name("My Home"), None);
    // A target without a localized name changes nothing.
    assert_eq!(rops::retargeted_name(h, DESIGN, "", FARM), None);
}

#[tokio::test]
async fn preview_ends_on_another_preview_timeout_distance_rebuild_and_disconnect() {
    let mut fx = rebuild_fixture();
    let (house, _) = house_with_chair(&fx);
    let conn_id = fx.pa.conn.id;
    let id = house.data().id;
    let active = |fx: &Fx| fx.server.systems.housing.rebuild_previews.active(conn_id);

    // At most one preview per connection: the second replaces the first.
    preview::start(&fx.server, &fx.pa, &house, REBUILD_TIER2).unwrap();
    preview::start(&fx.server, &fx.pa, &house, REBUILD_FARM).unwrap();
    assert_eq!(fx.server.systems.housing.rebuild_previews.len(), 1);
    assert_eq!(active(&fx), Some((id, REBUILD_FARM)));
    fx.a.drain();

    // Timeout.
    let started = crate::world::now_ms();
    preview::tick(&fx.server, started + 1000);
    assert_eq!(active(&fx), Some((id, REBUILD_FARM)));
    preview::tick(&fx.server, started + PREVIEW_TIMEOUT_MS + 1000);
    assert_eq!(active(&fx), None);
    let frames = fx.a.drain();
    assert_eq!(house_state_ids(&frames), 1); // the real house is back

    // Walking away.
    preview::start(&fx.server, &fx.pa, &house, REBUILD_TIER2).unwrap();
    fx.pa
        .data()
        .unit
        .transform
        .set_xyz(105.0 + 70.0, 100.0, 100.0);
    preview::tick(&fx.server, crate::world::now_ms());
    assert_eq!(active(&fx), None);
    fx.pa.data().unit.transform.set_xyz(100.0, 100.0, 100.0);

    // A rebuild of the house ends everyone's preview of it.
    preview::start(&fx.server, &fx.pa, &house, REBUILD_TIER2).unwrap();
    preview::start(&fx.server, &fx.pb, &house, REBUILD_TIER2).unwrap_err(); // Beta is no owner
    rops::rebuild(&fx.server, &fx.pa, &house, REBUILD_TIER2).unwrap();
    assert_eq!(active(&fx), None);
    fx.a.drain();
    fx.b.drain();

    // Disconnect: the entry is dropped silently, the id released.
    house.data().unit.template_id = DESIGN;
    house.data().current_step = -1;
    fx.eco.item_counts.lock().unwrap().insert(MAT_A, 5);
    preview::start(&fx.server, &fx.pa, &house, REBUILD_FARM).unwrap();
    fx.a.drain();
    ext::on_disconnect(&fx.server, conn_id);
    assert_eq!(active(&fx), None);
    assert!(fx.a.drain().is_empty());
}

#[tokio::test]
async fn preview_validates_and_follows_the_house() {
    let mut fx = rebuild_fixture();
    let (house, _) = house_with_chair(&fx);
    assert_eq!(
        preview::start(&fx.server, &fx.pb, &house, REBUILD_TIER2),
        Err(RebuildError::NotOwner)
    );
    assert_eq!(
        preview::start(&fx.server, &fx.pa, &house, 3),
        Err(RebuildError::UnknownRebuilding)
    );
    assert_eq!(
        preview::start(&fx.server, &fx.pa, &house, REBUILD_MISSING),
        Err(RebuildError::NoTarget)
    );
    fx.pa.data().unit.transform.set_xyz(400.0, 100.0, 100.0);
    assert_eq!(
        preview::start(&fx.server, &fx.pa, &house, REBUILD_TIER2),
        Err(RebuildError::TooFar)
    );
    fx.pa.data().unit.transform.set_xyz(100.0, 100.0, 100.0);
    fx.a.drain();
    // A preview does not need materials; a demolished house takes its previews with it.
    preview::start(&fx.server, &fx.pa, &house, REBUILD_TIER2).unwrap();
    fx.a.drain();
    ops::demolish(&fx.server, None, &house, true, false);
    fx.a.drain();
    preview::tick(&fx.server, crate::world::now_ms());
    assert!(fx.server.systems.housing.rebuild_previews.is_empty());
}

// ---------------------------------------------------------------------------------------------
// extension channels
// ---------------------------------------------------------------------------------------------

fn ext_frame(channel: &str, payload: &Value) -> Vec<u8> {
    let body = ext::packets::encode_body(channel, payload.to_string().as_bytes()).unwrap();
    crate::net::packet::encode_raw(crate::net::packet::LEVEL_PLAIN, ext::CS_AAEMU_EXT, &body)
}

fn ext_out(frames: Vec<(u16, Vec<u8>)>) -> Vec<(String, Value)> {
    frames
        .into_iter()
        .filter(|f| f.0 == ext::SC_AAEMU_EXT)
        .map(|(_, body)| {
            let m = ext::packets::parse_body(&body).unwrap();
            (m.channel, serde_json::from_slice(&m.payload).unwrap())
        })
        .collect()
}

async fn ext_call(fx: &mut Fx, channel: &str, payload: Value) -> Vec<(String, Value)> {
    fx.server
        .handle_frame(&mut fx.a.conn, &ext_frame(channel, &payload))
        .await
        .unwrap();
    ext_out(fx.a.drain())
}

async fn greet(fx: &mut Fx) {
    let out = ext_call(
        fx,
        "ext.hello",
        json!({ "proto": 1, "driver": "test", "features": ["house.rebuild"] }),
    )
    .await;
    assert_eq!(out[0].0, "ext.welcome");
    assert_eq!(out[0].1["features"], json!(["house.rebuild"]));
}

#[tokio::test]
async fn channels_info_start_and_preview_round_trip() {
    let mut fx = rebuild_fixture();
    // The conn.active_char of the test client (the extension handlers use it).
    fx.a.conn.active_char = Some(fx.pa.clone());
    let (house, _) = house_with_chair(&fx);
    let db_id = house.data().id;

    // Without the handshake nothing is answered.
    assert!(ext_call(&mut fx, "house.rebuild.info", json!({}))
        .await
        .is_empty());
    greet(&mut fx).await;

    // Info: the house is found by proximity (no id) or by id (string or number).
    for payload in [
        json!({}),
        json!({ "house": db_id.to_string() }),
        json!({ "house": db_id }),
    ] {
        let out = ext_call(&mut fx, "house.rebuild.info", payload).await;
        assert_eq!(out.len(), 1);
        let (channel, v) = &out[0];
        assert_eq!(channel, "house.rebuild.info");
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["house"], json!(db_id.to_string()));
        assert_eq!(v["template"], json!(DESIGN));
        assert_eq!(v["name"], json!(house.data().unit.name));
        let options = v["options"].as_array().unwrap();
        assert_eq!(options.len(), 3);
        assert_eq!(options[0]["rebuilding"], json!(REBUILD_TIER2));
        assert_eq!(options[0]["target"], json!(TIER2));
        assert_eq!(options[0]["skill"], json!(28829));
        assert_eq!(
            options[0]["skill_name"],
            json!("Extend Building Performance")
        );
        assert_eq!(options[1]["skill_name"], json!(""));
        assert_eq!(options[0]["can"], json!(true));
        assert_eq!(options[0]["reason"], json!(""));
        assert_eq!(
            options[0]["materials"][0],
            json!({ "item": MAT_A, "name": "Mat A", "count": 5, "have": 12 })
        );
        assert_eq!(options[2]["can"], json!(false));
        assert_eq!(options[2]["reason"], json!("no_target"));
        assert_eq!(
            options[2]["reason_message"],
            json!(RebuildError::NoTarget.message())
        );
        assert_eq!(options[0]["reason_message"], json!(""));
        assert!(v.get("blocked").is_none());
    }
    // Missing materials and a house-level block come with human-readable messages.
    fx.eco.item_counts.lock().unwrap().insert(MAT_A, 1);
    let out = ext_call(&mut fx, "house.rebuild.info", json!({})).await;
    assert_eq!(out[0].1["options"][0]["reason"], json!("materials"));
    assert_eq!(
        out[0].1["options"][0]["reason_message"],
        json!("Not enough materials.")
    );
    fx.eco.item_counts.lock().unwrap().insert(MAT_A, 12);
    house.data().sell_price = 1;
    let out = ext_call(&mut fx, "house.rebuild.info", json!({})).await;
    assert_eq!(out[0].1["blocked"], json!("for_sale"));
    assert_eq!(
        out[0].1["blocked_message"],
        json!(RebuildError::ForSale.message())
    );
    assert_eq!(
        out[0].1["options"][0]["reason_message"],
        json!(RebuildError::ForSale.message())
    );
    house.data().sell_price = 0;
    // Errors.
    let out = ext_call(&mut fx, "house.rebuild.info", json!({ "house": "99999" })).await;
    assert_eq!(out[0].1["ok"], json!(false));
    assert_eq!(out[0].1["error"], json!("no_house"));
    fx.pa.data().unit.transform.set_xyz(300.0, 300.0, 100.0);
    let out = ext_call(&mut fx, "house.rebuild.info", json!({})).await;
    assert_eq!(out[0].1["error"], json!("no_house")); // nothing within 30 m
    fx.pa.data().unit.transform.set_xyz(100.0, 100.0, 100.0);

    // Preview start / end.
    fx.b.drain();
    let out = ext_call(
        &mut fx,
        "house.rebuild.preview",
        json!({ "house": db_id.to_string(), "rebuilding": REBUILD_TIER2 }),
    )
    .await;
    assert_eq!(
        out,
        [(
            "house.rebuild.preview".to_string(),
            json!({ "ok": true, "active": true })
        )]
    );
    assert!(fx
        .server
        .systems
        .housing
        .rebuild_previews
        .active(fx.a.conn.id)
        .is_some());
    assert!(fx.b.drain().is_empty());
    let out = ext_call(&mut fx, "house.rebuild.preview_end", json!({})).await;
    assert_eq!(out[0].1["active"], json!(false));
    assert!(fx.server.systems.housing.rebuild_previews.is_empty());
    // Ending with no preview still answers.
    let out = ext_call(&mut fx, "house.rebuild.preview_end", json!({})).await;
    assert_eq!(out[0].1, json!({ "ok": true, "active": false }));
    let out = ext_call(
        &mut fx,
        "house.rebuild.preview",
        json!({ "house": db_id.to_string(), "rebuilding": 3 }),
    )
    .await;
    assert_eq!(out[0].1["ok"], json!(false));
    assert_eq!(out[0].1["error"], json!("unknown_rebuilding"));
    assert_eq!(out[0].1["active"], json!(false));

    // Start: failure first (a missing rebuilding id is a bad payload), then success.
    let out = ext_call(
        &mut fx,
        "house.rebuild.start",
        json!({ "house": db_id.to_string() }),
    )
    .await;
    assert_eq!(out[0].0, "ext.error");
    assert_eq!(out[0].1["error"], json!("bad_payload"));
    fx.eco.item_counts.lock().unwrap().insert(MAT_A, 1);
    let out = ext_call(
        &mut fx,
        "house.rebuild.start",
        json!({ "house": db_id.to_string(), "rebuilding": REBUILD_TIER2 }),
    )
    .await;
    assert_eq!(
        out,
        [(
            "house.rebuild.result".to_string(),
            json!({ "ok": false, "error": "materials", "message": RebuildError::Materials.message(), "house": db_id.to_string() })
        )]
    );
    fx.eco.item_counts.lock().unwrap().insert(MAT_A, 5);
    let out = ext_call(
        &mut fx,
        "house.rebuild.start",
        json!({ "house": db_id.to_string(), "rebuilding": REBUILD_TIER2.to_string() }),
    )
    .await;
    assert_eq!(
        out,
        [(
            "house.rebuild.result".to_string(),
            json!({ "ok": true, "house": db_id.to_string(), "target": TIER2 })
        )]
    );
    assert_eq!(house.data().unit.template_id, TIER2);
}

#[tokio::test]
async fn channels_require_the_negotiated_feature() {
    let mut fx = rebuild_fixture();
    fx.a.conn.active_char = Some(fx.pa.clone());
    let _ = house_with_chair(&fx);
    // A client that said hello without the feature gets a refusal, not an answer with data.
    let out = ext_call(&mut fx, "ext.hello", json!({ "proto": 1, "features": [] })).await;
    assert_eq!(out[0].1["features"], json!([]));
    let out = ext_call(&mut fx, "house.rebuild.info", json!({})).await;
    assert_eq!(out[0].1, json!({ "ok": false, "error": "not_available" }));
    let out = ext_call(&mut fx, "house.rebuild.start", json!({ "rebuilding": 70 })).await;
    assert_eq!(out[0].1["error"], json!("not_available"));
}
