//! Merchant cargo regressions through the production economy bridge and authored phase dispatch.

use std::sync::Arc;

use crate::data::items::{backpack_type, ItemTemplate, TemplateKind};
use crate::data::GameData;
use crate::models::character::Inventory;
use crate::models::container::{equip_slot, slot_type};
use crate::models::doodad::{doodad_owner_type, Doodad};
use crate::models::item::Item;
use crate::models::item_task::task_type;
use crate::models::slave::Slave;
use crate::opcodes::sc;
use crate::systems::economy::inventory::{Inv, ItemTxn};
use crate::systems::economy::tests::{give, test_data, World, SINGLE};
use crate::systems::npcs::doodad::runtime::DoodadCtx;
use crate::systems::npcs::doodad::templates::*;
use crate::systems::npcs::doodad::{funcs::items, interactions, manager, phase};
use crate::systems::npcs::hooks::EconomyBridge;
use crate::systems::vehicles::data::{SlaveDoodadBinding, SlaveTemplate};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{DoodadRef, Entity, SlaveRef, WorldObject};

use super::EconomyForNpcs;

const HULL: u32 = 75;
const RACK: u32 = 3446;
const EMPTY: u32 = 8264;
const LOADED: u32 = 8265;
const STORE_SKILL: u32 = 15307;
const RECOVER_SKILL: u32 = 15309;
const PACK: u32 = 18469;
const GLIDER: u32 = 99991;

fn authored_data() -> GameData {
    let mut data = test_data();
    for (id, kind) in [(PACK, 1), (GLIDER, backpack_type::GLIDER)] {
        data.items.templates.insert(
            id,
            ItemTemplate {
                kind: TemplateKind::Backpack {
                    backpack_type: kind,
                },
                ..ItemTemplate::plain(id, 1)
            },
        );
    }
    let groups = vec![
        DoodadFuncGroups {
            id: EMPTY,
            almighty: RACK,
            group_kind_id: doodad_func_group_kind::START,
            ..Default::default()
        },
        DoodadFuncGroups {
            id: LOADED,
            almighty: RACK,
            group_kind_id: doodad_func_group_kind::NORMAL,
            ..Default::default()
        },
    ];
    let doodads = &mut data.systems.npcs.doodad;
    doodads.templates.insert(
        RACK,
        DoodadTemplate {
            id: RACK,
            func_groups: groups,
            ..Default::default()
        },
    );
    for (group, key, id, kind, skill, next) in [
        (
            EMPTY,
            7481,
            2,
            "DoodadFuncConsumeChanger",
            STORE_SKILL,
            LOADED,
        ),
        (
            LOADED,
            9998,
            393,
            "DoodadFuncRecoverItem",
            RECOVER_SKILL,
            EMPTY,
        ),
    ] {
        doodads.funcs_by_groups.insert(
            group,
            vec![DoodadFunc {
                group_id: group,
                func_id: id,
                func_key: key,
                func_type: kind.into(),
                next_phase: next as i32,
                skill_id: skill,
                perm_id: 5,
                ..Default::default()
            }],
        );
    }
    doodads
        .func_templates
        .entry("DoodadFuncConsumeChanger")
        .or_default()
        .insert(
            2,
            FuncTemplate::ConsumeChanger(DoodadFuncConsumeChanger {
                id: 2,
                slot_id: 26,
                count: 1,
            }),
        );
    doodads
        .func_templates
        .entry("DoodadFuncRecoverItem")
        .or_default()
        .insert(
            393,
            FuncTemplate::RecoverItem(DoodadFuncRecoverItem { id: 393 }),
        );
    doodads
        .consume_changer_items
        .push(DoodadFuncConsumeChangerItem {
            id: 10,
            doodad_func_consume_changer_id: 2,
            item_id: PACK,
        });
    data.systems.vehicles.slave_templates.insert(
        HULL,
        SlaveTemplate {
            id: HULL,
            name: "Merchant Schooner".into(),
            model_id: 1205,
            slave_kind: 2,
            doodad_bindings: (60..80)
                .map(|point| SlaveDoodadBinding {
                    id: u32::from(point),
                    owner_id: HULL,
                    owner_type: "Slave".into(),
                    attach_point_id: point,
                    doodad_id: RACK,
                    persist: true,
                    scale: 1.0,
                })
                .collect(),
            ..Default::default()
        },
    );
    data
}

struct CargoWorld {
    w: World,
    hull: SlaveRef,
    racks: Vec<DoodadRef>,
}

fn world(data: GameData) -> CargoWorld {
    let bindings = data
        .systems
        .vehicles
        .slave_template(HULL)
        .unwrap()
        .doodad_bindings
        .iter()
        .filter(|b| b.doodad_id == RACK)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(bindings.len(), 20);
    let server = Arc::new(test_server_with(data));
    super::install(&server);
    let mut a = TestClient::new(&server, 1, 1001);
    let mut b = TestClient::new(&server, 2, 1002);
    let mut alice = test_character(1, "Alice", 100.0, 100.0);
    alice.inventory = Inventory::new(1, 50, 50, &server.items, &server.ids.container);
    let mut bob = test_character(2, "Bob", 101.0, 100.0);
    bob.inventory = Inventory::new(2, 50, 50, &server.items, &server.ids.container);
    let pa = spawn_player(&server, &a, alice, 0x100);
    let pb = spawn_player(&server, &b, bob, 0x101);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    let instance = server.world.world(0).unwrap();
    let mut hull = Slave::new(700, HULL);
    hull.unit.model_id = 1205;
    hull.unit.transform.set_xyz(100.0, 100.0, 100.0);
    hull.summoner_id = 1;
    hull.summoner_obj_id = pa.obj_id;
    let hull = Entity::new(0x300, hull);
    instance.spawn(&hull, &server.view_ctx());
    let racks = bindings
        .into_iter()
        .map(|binding| {
            let rack = manager::create(&server, &instance, 0, RACK, None, true).unwrap();
            {
                let mut d = rack.data();
                d.owner_type = doodad_owner_type::SLAVE;
                d.owner_id = 1;
                d.owner_db_id = 700;
                d.parent_obj_id = hull.obj_id;
                d.attach_point = binding.attach_point_id;
                d.is_persistent = true;
            }
            instance.attach_child(&WorldObject::from(&hull), &WorldObject::from(&rack));
            hull.data().attached_doodads.push(rack.obj_id);
            instance.spawn(&rack, &server.view_ctx());
            rack
        })
        .collect();
    a.drain();
    b.drain();
    CargoWorld {
        w: World {
            server,
            a,
            pa,
            b,
            pb,
        },
        hull,
        racks,
    }
}

fn use_rack(c: &CargoWorld, index: usize, skill: u32) {
    assert!(interactions::execute_interaction(
        &c.w.server,
        interactions::world_interaction_type::USE,
        &WorldObject::from(&c.w.pa),
        Some(&WorldObject::from(&c.racks[index])),
        skill,
        0,
        None
    ));
}

fn transfer(c: &CargoWorld, item_id: u64, target_type: u8, slot: i32) {
    let mut txn = ItemTxn::begin(&c.w.server);
    let target = Inv::of(&c.w.pa).get(target_type).unwrap();
    assert!(crate::systems::economy::mail::add_or_move_existing_item(
        &mut txn,
        &target,
        task_type::INVALID,
        item_id,
        slot
    ));
}

fn equipped(c: &CargoWorld) -> Item {
    Inv::of(&c.w.pa).equipped(equip_slot::BACKPACK).unwrap()
}

fn cargo_state(rack: &DoodadRef) -> (u32, u64, u32) {
    let d = rack.data();
    (d.func_group_id, d.item_id, d.item_template_id)
}

fn repeated_cycles(c: &mut CargoWorld) {
    let ids: Vec<_> = (0..20)
        .map(|_| {
            let id = give(
                &c.w,
                &c.w.pa,
                slot_type::EQUIPMENT,
                PACK,
                1,
                equip_slot::BACKPACK,
            );
            transfer(c, id, slot_type::SYSTEM, -1);
            id
        })
        .collect();
    for cycle in 0..3 {
        for step in 0..20 {
            let index = (step * 7 + cycle * 3) % 20;
            transfer(c, ids[index], slot_type::EQUIPMENT, equip_slot::BACKPACK);
            use_rack(c, index, STORE_SKILL);
            assert_eq!(cargo_state(&c.racks[index]), (LOADED, ids[index], PACK));
            assert!(Inv::of(&c.w.pa).equipped(equip_slot::BACKPACK).is_none());
        }
        for step in 0..20 {
            let index = (step * 9 + cycle * 5) % 20;
            use_rack(c, index, RECOVER_SKILL);
            assert_eq!(equipped(c).id, ids[index]);
            assert_eq!(cargo_state(&c.racks[index]), (EMPTY, 0, 0));
            transfer(c, ids[index], slot_type::SYSTEM, -1);
        }
        for id in &ids {
            let copies: usize =
                c.w.server
                    .items
                    .all_containers()
                    .iter()
                    .map(|container| {
                        container
                            .lock()
                            .unwrap()
                            .items
                            .iter()
                            .filter(|i| i.id == *id)
                            .count()
                    })
                    .sum();
            assert_eq!(copies, 1);
        }
    }
    assert_eq!(c.hull.data().attached_doodads.len(), 20);
    assert!(!c.w.a.drain_ops().contains(&sc::SCErrorMsgPacket));
}

#[tokio::test]
async fn merchant_all_twenty_racks_survive_shuffled_repeated_load_unload() {
    repeated_cycles(&mut world(authored_data()));
}

#[tokio::test]
async fn cargo_checks_are_shared_with_other_hulls_and_nonpersistent_racks() {
    let mut data = authored_data();
    let mut other = data.systems.vehicles.slave_template(HULL).unwrap().clone();
    other.id = 21;
    for binding in &mut other.doodad_bindings {
        binding.owner_id = other.id;
        binding.persist = false;
    }
    data.systems
        .vehicles
        .slave_templates
        .insert(other.id, other);
    let c = world(data);
    c.hull.data().unit.template_id = 21;
    c.racks[11].data().is_persistent = false;
    let id = give(
        &c.w,
        &c.w.pa,
        slot_type::EQUIPMENT,
        PACK,
        1,
        equip_slot::BACKPACK,
    );
    use_rack(&c, 11, STORE_SKILL);
    use_rack(&c, 11, RECOVER_SKILL);
    assert_eq!(equipped(&c).id, id);
    assert_eq!(cargo_state(&c.racks[11]), (EMPTY, 0, 0));
}

#[tokio::test]
async fn missing_deposit_source_and_zero_guid_cargo_cannot_reuse_old_success() {
    let c = world(authored_data());
    c.racks[0].data().phase.to_next_phase = true;
    let ctx = DoodadCtx::of(&c.w.server, &c.racks[0]).unwrap();
    items::consume_changer(
        &DoodadFuncConsumeChanger {
            id: 2,
            slot_id: 26,
            count: 1,
        },
        &ctx,
        Some(&WorldObject::from(&c.w.pa)),
    );
    assert!(!c.racks[0].data().phase.to_next_phase);
    assert_eq!(cargo_state(&c.racks[0]), (EMPTY, 0, 0));
    c.racks[0].data().item_template_id = PACK;
    phase::set_func_group_id(&c.w.server, &c.racks[0], LOADED);
    use_rack(&c, 0, RECOVER_SKILL);
    assert_eq!(cargo_state(&c.racks[0]), (LOADED, 0, PACK));
    assert!(Inv::of(&c.w.pa).equipped(equip_slot::BACKPACK).is_none());
}

#[tokio::test]
async fn stale_captured_source_cannot_be_deposited_in_a_second_rack() {
    let c = world(authored_data());
    let id = give(
        &c.w,
        &c.w.pa,
        slot_type::EQUIPMENT,
        PACK,
        1,
        equip_slot::BACKPACK,
    );
    let captured = equipped(&c);
    use_rack(&c, 7, STORE_SKILL);
    c.racks[18].data().phase.to_next_phase = true;
    let _housing = c.w.server.systems.housing.operation_lock.lock().unwrap();
    assert!(!EconomyForNpcs.try_store_doodad_item(
        &c.w.server,
        &c.w.pa,
        &c.racks[18],
        &captured,
        1
    ));
    assert_eq!(cargo_state(&c.racks[7]), (LOADED, id, PACK));
    assert_eq!(cargo_state(&c.racks[18]), (EMPTY, 0, 0));
    assert!(!c.racks[18].data().phase.to_next_phase);
    assert_eq!(
        c.w.server.items.item_by_id(id).unwrap().slot_type,
        slot_type::SYSTEM
    );
}

#[tokio::test]
async fn occupied_rack_and_changed_source_reject_without_overwriting_escrow() {
    let mut c = world(authored_data());
    let first = give(
        &c.w,
        &c.w.pa,
        slot_type::EQUIPMENT,
        PACK,
        1,
        equip_slot::BACKPACK,
    );
    use_rack(&c, 3, STORE_SKILL);
    let second = give(
        &c.w,
        &c.w.pa,
        slot_type::EQUIPMENT,
        PACK,
        1,
        equip_slot::BACKPACK,
    );
    c.w.a.drain();
    c.racks[3].data().phase.to_next_phase = true;
    let ctx = DoodadCtx::of(&c.w.server, &c.racks[3]).unwrap();
    items::consume_changer(
        &DoodadFuncConsumeChanger {
            id: 2,
            slot_id: 26,
            count: 1,
        },
        &ctx,
        Some(&WorldObject::from(&c.w.pa)),
    );
    assert_eq!(cargo_state(&c.racks[3]), (LOADED, first, PACK));
    assert_eq!(equipped(&c).id, second);
    assert!(!c.racks[3].data().phase.to_next_phase);
    assert!(!c.w.a.drain_ops().contains(&sc::SCItemTaskSuccessPacket));
    for change in 0..3 {
        let mut captured = equipped(&c);
        match change {
            0 => captured.template_id += 1,
            1 => captured.count = 2,
            _ => captured.slot -= 1,
        }
        let _housing = c.w.server.systems.housing.operation_lock.lock().unwrap();
        assert!(!EconomyForNpcs.try_store_doodad_item(
            &c.w.server,
            &c.w.pa,
            &c.racks[4],
            &captured,
            1
        ));
        assert_eq!(cargo_state(&c.racks[4]), (EMPTY, 0, 0));
        assert_eq!(equipped(&c).id, second);
    }
}

#[tokio::test]
async fn failed_recovery_keeps_glider_and_cargo_then_retries_with_the_same_ids() {
    let mut c = world(authored_data());
    let pack = give(
        &c.w,
        &c.w.pa,
        slot_type::EQUIPMENT,
        PACK,
        1,
        equip_slot::BACKPACK,
    );
    use_rack(&c, 19, STORE_SKILL);
    let glider = give(
        &c.w,
        &c.w.pa,
        slot_type::EQUIPMENT,
        GLIDER,
        1,
        equip_slot::BACKPACK,
    );
    Inv::of(&c.w.pa).bag().lock().unwrap().set_container_size(1);
    let filler = give(&c.w, &c.w.pa, slot_type::INVENTORY, SINGLE, 1, 0);
    c.w.a.drain();
    c.racks[19].data().phase.to_next_phase = true;
    use_rack(&c, 19, RECOVER_SKILL);
    assert_eq!(equipped(&c).id, glider);
    assert_eq!(cargo_state(&c.racks[19]), (LOADED, pack, PACK));
    assert!(!c.racks[19].data().phase.to_next_phase);
    assert_eq!(
        c.w.server.items.item_by_id(pack).unwrap().slot_type,
        slot_type::SYSTEM
    );
    assert!(!c.w.a.drain_ops().contains(&sc::SCItemTaskSuccessPacket));
    {
        let mut txn = ItemTxn::begin(&c.w.server);
        Inv::of(&c.w.pa).bag().lock().unwrap().remove_item_task(
            task_type::INVALID,
            filler,
            true,
            &mut txn.env,
        );
    }
    use_rack(&c, 19, RECOVER_SKILL);
    assert_eq!(equipped(&c).id, pack);
    assert_eq!(cargo_state(&c.racks[19]), (EMPTY, 0, 0));
    assert_eq!(
        c.w.server.items.item_by_id(glider).unwrap().slot_type,
        slot_type::INVENTORY
    );
    use_rack(&c, 19, STORE_SKILL);
    let other_pack = give(
        &c.w,
        &c.w.pa,
        slot_type::EQUIPMENT,
        PACK,
        1,
        equip_slot::BACKPACK,
    );
    use_rack(&c, 19, RECOVER_SKILL);
    assert_eq!(equipped(&c).id, other_pack);
    assert_eq!(cargo_state(&c.racks[19]), (LOADED, pack, PACK));
    transfer(&c, other_pack, slot_type::SYSTEM, -1);
    use_rack(&c, 19, RECOVER_SKILL);
    assert_eq!(equipped(&c).id, pack);
    assert_eq!(cargo_state(&c.racks[19]), (EMPTY, 0, 0));
}

#[tokio::test]
async fn duplicate_cleanup_preserves_the_sole_pack_for_the_surviving_rack() {
    let c = world(authored_data());
    let id = give(
        &c.w,
        &c.w.pa,
        slot_type::EQUIPMENT,
        PACK,
        1,
        equip_slot::BACKPACK,
    );
    use_rack(&c, 0, STORE_SKILL);
    {
        let mut duplicate = c.racks[1].data();
        duplicate.item_id = id;
        duplicate.item_template_id = PACK;
    }
    phase::set_func_group_id(&c.w.server, &c.racks[1], LOADED);
    let instance = c.w.server.world.world(0).unwrap();
    manager::delete(&c.w.server, &instance, &c.racks[1]);
    assert_eq!(
        c.w.server.items.item_by_id(id).unwrap().slot_type,
        slot_type::SYSTEM
    );
    use_rack(&c, 0, RECOVER_SKILL);
    assert_eq!(equipped(&c).id, id);
}

#[tokio::test]
async fn dangling_rack_repair_does_not_move_another_players_pack_or_create_one() {
    let mut c = world(authored_data());
    let id = give(
        &c.w,
        &c.w.pa,
        slot_type::EQUIPMENT,
        PACK,
        1,
        equip_slot::BACKPACK,
    );
    use_rack(&c, 0, STORE_SKILL);
    {
        let mut txn = ItemTxn::begin(&c.w.server);
        assert!(crate::systems::economy::mail::add_or_move_existing_item(
            &mut txn,
            &Inv::of(&c.w.pb).equipment(),
            task_type::INVALID,
            id,
            equip_slot::BACKPACK
        ));
    }
    let before = Inv::of(&c.w.pb).equipped(equip_slot::BACKPACK).unwrap();
    c.w.a.drain();
    c.w.b.drain();
    use_rack(&c, 0, RECOVER_SKILL);
    assert_eq!(cargo_state(&c.racks[0]), (EMPTY, 0, 0));
    assert_eq!(
        Inv::of(&c.w.pb).equipped(equip_slot::BACKPACK).unwrap(),
        before
    );
    assert!(Inv::of(&c.w.pa).equipped(equip_slot::BACKPACK).is_none());
    assert!(!c.w.a.drain_ops().contains(&sc::SCItemTaskSuccessPacket));
    assert!(!c.w.b.drain_ops().contains(&sc::SCItemTaskSuccessPacket));

    // A legacy pack in another actor's bag is outside escrow too: repair the rack only.
    let foreign_equipment = Inv::of(&c.w.pb).equipment();
    let mut in_bag = foreign_equipment.lock().unwrap().remove_item(id).unwrap();
    in_bag.slot_type = slot_type::INVENTORY;
    in_bag.slot = 0;
    let foreign_bag = Inv::of(&c.w.pb).bag();
    foreign_bag.lock().unwrap().items.push(in_bag.clone());
    {
        let mut d = c.racks[3].data();
        d.item_id = id;
        d.item_template_id = PACK;
    }
    phase::set_func_group_id(&c.w.server, &c.racks[3], LOADED);
    use_rack(&c, 3, RECOVER_SKILL);
    assert_eq!(cargo_state(&c.racks[3]), (EMPTY, 0, 0));
    assert_eq!(c.w.server.items.item_by_id(id).unwrap(), in_bag);
    assert!(Inv::of(&c.w.pa).equipped(equip_slot::BACKPACK).is_none());

    // A missing item is also dangling, but only the rack reference/phase is repaired.
    {
        let mut d = c.racks[2].data();
        d.item_id = id + 99999;
        d.item_template_id = PACK;
    }
    phase::set_func_group_id(&c.w.server, &c.racks[2], LOADED);
    use_rack(&c, 2, RECOVER_SKILL);
    assert_eq!(cargo_state(&c.racks[2]), (EMPTY, 0, 0));
    assert!(c.w.server.items.item_by_id(id + 99999).is_none());
}

#[tokio::test]
async fn ambiguous_escrow_and_wrong_hull_graph_are_not_repaired() {
    let c = world(authored_data());
    let id = give(
        &c.w,
        &c.w.pa,
        slot_type::EQUIPMENT,
        PACK,
        1,
        equip_slot::BACKPACK,
    );
    use_rack(&c, 5, STORE_SKILL);
    let source = c.w.server.items.item_by_id(id).unwrap();
    let mut second_copy = source.clone();
    second_copy.slot_type = slot_type::INVENTORY;
    second_copy.owner_id = u64::from(c.w.pb.character_id);
    second_copy.slot = 0;
    let foreign_bag = Inv::of(&c.w.pb).bag();
    foreign_bag.lock().unwrap().items.push(second_copy);
    use_rack(&c, 5, RECOVER_SKILL);
    assert_eq!(cargo_state(&c.racks[5]), (LOADED, id, PACK));
    assert!(Inv::of(&c.w.pa).equipped(equip_slot::BACKPACK).is_none());
    foreign_bag.lock().unwrap().remove_item(id);
    c.w.server.items.add_loose(source.clone());
    // Loose items are not inferred as missing if the persistent holder disappears.
    let system = Inv::of(&c.w.pa).get(slot_type::SYSTEM).unwrap();
    system.lock().unwrap().remove_item(id);
    use_rack(&c, 5, RECOVER_SKILL);
    assert_eq!(cargo_state(&c.racks[5]), (LOADED, id, PACK));
    c.w.server.items.take_loose(id);
    c.racks[5].data().owner_db_id += 1;
    use_rack(&c, 5, RECOVER_SKILL);
    assert_eq!(cargo_state(&c.racks[5]), (LOADED, id, PACK));
}

#[tokio::test]
async fn removal_ignores_stale_children_but_blocks_a_live_persistent_child() {
    let c = world(authored_data());
    let instance = c.w.server.world.world(0).unwrap();
    let mut data = Doodad::new(RACK);
    data.is_persistent = true;
    let child = Entity::new(0x900, data);
    instance.spawn(&child, &c.w.server.view_ctx());
    instance.attach_child(&WorldObject::from(&c.racks[0]), &WorldObject::from(&child));
    assert!(!interactions::doodad_allow_removal(
        &c.w.server,
        &c.racks[0]
    ));
    // Existing attach_child leaves the old parent's entry behind when reparenting.
    instance.attach_child(&WorldObject::from(&c.racks[1]), &WorldObject::from(&child));
    assert!(interactions::doodad_allow_removal(&c.w.server, &c.racks[0]));
    assert!(!interactions::doodad_allow_removal(
        &c.w.server,
        &c.racks[1]
    ));
    instance.remove_object(child.obj_id);
    assert!(interactions::doodad_allow_removal(&c.w.server, &c.racks[1]));
    let replacement = Entity::new(child.obj_id, child.data().clone());
    instance.spawn(&replacement, &c.w.server.view_ctx());
    assert!(interactions::doodad_allow_removal(&c.w.server, &c.racks[1]));
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_compact_merchant_all_twenty_racks_survive_shuffled_cycles() {
    use crate::data::client_files::ClientFileManager;
    use crate::systems::DataLoadCtx;

    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let app = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let files = ClientFileManager::from_sources(&[], &app);
    let ctx = DataLoadCtx {
        conn: &conn,
        app_path: &app,
        files: &files,
        language: "en_us",
    };
    let mut data = GameData::empty();
    data.items = crate::data::ItemData::load(&conn).unwrap();
    data.systems.npcs.doodad = crate::systems::npcs::doodad::data::DoodadData::load(&ctx).unwrap();
    data.systems.vehicles = crate::systems::vehicles::Data::load(&ctx).unwrap();
    assert!(matches!(
        data.items.template(23398).unwrap().kind,
        TemplateKind::SummonSlave { slave_id: HULL }
    ));
    repeated_cycles(&mut world(data));
}
