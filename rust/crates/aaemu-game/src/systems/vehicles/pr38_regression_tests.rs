//! Regressions for legacy hull defaults, persistent empty equipment, and gunner authority/lifetime.

use super::*;
use crate::world::SlaveRef;

fn hull(server: &GameServer, owner: &PlayerRef, saved: Option<slaves::SavedSlave>) -> SlaveRef {
    let world = server.world.world(0).unwrap();
    slaves::create_core(
        server,
        &world,
        Some(owner),
        None,
        SHIP_HULL,
        None,
        false,
        None,
        saved,
    )
    .expect("hull")
}

fn saved(id: u32) -> slaves::SavedSlave {
    slaves::SavedSlave {
        id,
        name: "Legacy hull".into(),
        hp: 100,
        mp: 10,
    }
}

#[tokio::test]
async fn no_starter_pack_migrates_equivalent_defaults_and_preserves_unmapped_bindings() {
    let mut d = ship_equip_data();
    d.systems.vehicles.slave_initial_items.clear();
    let template = d
        .systems
        .vehicles
        .slave_templates
        .get_mut(&SHIP_HULL)
        .unwrap();
    template
        .slave_bindings
        .push(super::super::data::SlaveBinding {
            id: 2,
            owner_id: SHIP_HULL,
            owner_type: "Slave".into(),
            slave_id: OTHER_HULL,
            attach_point_id: 10,
        });
    d.systems
        .vehicles
        .slave_equip_slots
        .get_mut(&SHIP_HULL)
        .unwrap()
        .push(super::super::data::SlaveEquipSlot {
            id: 2,
            slave_id: SHIP_HULL,
            attach_point_id: 10,
            equip_slot_id: 4,
            require_slot_id: -1,
        });
    let server = server_with(d);
    let a = TestClient::new(&server, 1, 1);
    let owner = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    let world = server.world.world(0).unwrap();
    let first = hull(&server, &owner, None);
    let db_id = first.data().id;
    assert_eq!(
        first.data().attached_slaves.len(),
        2,
        "both authored defaults survive"
    );
    assert_eq!(
        first.data().equipment[3].as_ref().unwrap().template_id,
        CANNON_ITEM
    );
    assert!(
        first.data().equipment[4].is_none(),
        "no equivalent item for the static child"
    );
    let container = server
        .items
        .find_container_for_character(1, slot_type::EQUIPMENT_SLAVE, db_id)
        .unwrap();
    // Deliberate unequipping leaves a persistent, empty container. It must not seed again.
    container.lock().unwrap().items.clear();
    slaves::delete(&server, Some(&owner), world.id, first.obj_id, false);
    let again = hull(&server, &owner, Some(saved(db_id)));
    assert!(again.data().equipment[3].is_none());
    let children = again.data().attached_slaves.clone();
    assert_eq!(
        children.len(),
        1,
        "only the unmapped static default returns"
    );
    assert_eq!(
        world.slave(children[0]).unwrap().data().unit.template_id,
        OTHER_HULL
    );
}

#[tokio::test]
async fn saved_hull_initializes_starter_equipment_only_when_container_is_created() {
    let server = server_with(ship_equip_data());
    let a = TestClient::new(&server, 1, 1);
    let owner = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    let first = hull(&server, &owner, Some(saved(700)));
    let item = first.data().equipment[3]
        .clone()
        .expect("legacy hull starter cannon");
    assert_eq!(item.template_id, CANNON_ITEM);
    let container = server
        .items
        .find_container_for_character(1, slot_type::EQUIPMENT_SLAVE, 700)
        .unwrap();
    assert_eq!(
        container.lock().unwrap().get_item_by_slot(3).unwrap().id,
        item.id
    );
    container.lock().unwrap().items.clear();
    let world = server.world.world(0).unwrap();
    slaves::delete(&server, Some(&owner), world.id, first.obj_id, false);
    let again = hull(&server, &owner, Some(saved(700)));
    assert!(again.data().equipment[3].is_none());
    assert!(
        again.data().attached_slaves.is_empty(),
        "unequipped defaults stay absent"
    );
}

#[tokio::test]
#[ignore = "needs AAEMU_TEST_MYSQL on private MySQL3317; uses connection-local temporary tables"]
async fn private_mysql_legacy_slave_equipment_survives_restart_and_stays_unequipped() {
    use sqlx::mysql::MySqlConnectOptions;
    use std::str::FromStr;

    let options = MySqlConnectOptions::from_str(
        &std::env::var("AAEMU_TEST_MYSQL").expect("AAEMU_TEST_MYSQL"),
    )
    .unwrap();
    assert_eq!(options.get_port(), 3317);
    assert!(matches!(options.get_host(), "127.0.0.1" | "localhost"));
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    // Exercise production serializers/loaders with connection-local tables; no persistent rows
    // or another session's tables are touched, even if an assertion fails.
    for table in ["items", "item_containers", "slaves"] {
        let (_, ddl): (String, String) = sqlx::query_as(&format!("SHOW CREATE TABLE `{table}`"))
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query(&ddl.replacen("CREATE TABLE", "CREATE TEMPORARY TABLE", 1))
            .execute(&pool)
            .await
            .unwrap();
    }
    let make_server = || {
        let mut server = test_server_with(ship_equip_data());
        server.db = pool.clone();
        let server = Arc::new(server);
        server.set_self();
        server
    };
    let server = make_server();
    let a = TestClient::new(&server, 1, 1);
    let (owner, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), SLAVE_ITEM);
    let bag = owner.data().inventory.bag().clone();
    bag.lock().unwrap().container_id = 900;
    server.items.add_container(bag.clone());
    let item = {
        let mut bag = bag.lock().unwrap();
        let item = bag.items.iter_mut().find(|i| i.id == item_id).unwrap();
        item.owner_id = 1;
        item.details.slave_db_id = 700;
        item.dirty = true;
        item.clone()
    };
    let row = slaves::SlaveRow {
        id: 700,
        item_id,
        template_id: SHIP_HULL,
        attach_point: -1,
        name: "Legacy hull".into(),
        owner_type: 0,
        owner_id: 1,
        summoner: 1,
        hp: 100,
        mp: 10,
        x: 100.0,
        y: 100.0,
        z: 100.0,
    };
    slaves::write_row(&mut pool.acquire().await.unwrap(), &row)
        .await
        .unwrap();
    let world = server.world.world(0).unwrap();
    let first = slaves::create(
        &server,
        &world,
        Some(&owner),
        SHIP_HULL,
        Some(item.clone()),
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(first.data().id, 700);
    let cannon_item = first.data().equipment[3]
        .clone()
        .expect("legacy starter equipment");
    assert_eq!(first.data().attached_slaves.len(), 1);
    let mut tx = pool.begin().await.unwrap();
    server.items.save(&mut tx).await.unwrap();
    for row in slaves::snapshot(&server, &first) {
        slaves::write_row(&mut tx, &row).await.unwrap();
    }
    tx.commit().await.unwrap();
    server.items.commit_save();

    let restart = make_server();
    restart
        .items
        .load_user_items(
            &pool,
            &restart.game_data.items,
            1,
            &restart.ids.container,
            &restart.ids.item,
        )
        .await
        .unwrap();
    let a = TestClient::new(&restart, 1, 1);
    let owner = spawn_player(&restart, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    let world = restart.world.world(0).unwrap();
    let again = slaves::create(
        &restart,
        &world,
        Some(&owner),
        SHIP_HULL,
        Some(item.clone()),
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        again.data().equipment[3].as_ref().unwrap().id,
        cannon_item.id,
        "restart preserves item identity"
    );
    assert_eq!(again.data().attached_slaves.len(), 1);
    let container = restart
        .items
        .find_container_for_character(1, slot_type::EQUIPMENT_SLAVE, 700)
        .unwrap();
    let removed = {
        let mut container = container.lock().unwrap();
        container.dirty = true;
        container.items.remove(0)
    };
    restart.items.mark_for_db_deletion(removed.id);
    let mut tx = pool.begin().await.unwrap();
    restart.items.save(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    restart.items.commit_save();

    let restart = make_server();
    restart
        .items
        .load_user_items(
            &pool,
            &restart.game_data.items,
            1,
            &restart.ids.container,
            &restart.ids.item,
        )
        .await
        .unwrap();
    let a = TestClient::new(&restart, 1, 1);
    let owner = spawn_player(&restart, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    let world = restart.world.world(0).unwrap();
    let empty = slaves::create(
        &restart,
        &world,
        Some(&owner),
        SHIP_HULL,
        Some(item),
        false,
        None,
    )
    .await
    .unwrap();
    assert!(empty.data().equipment[3].is_none());
    assert!(
        empty.data().attached_slaves.is_empty(),
        "empty persisted container is intentional"
    );
    pool.close().await;
}

async fn parent_only_cannon_attack(occupied: bool) {
    use crate::models::movement::{MoveTypeActorFlags, UnitMove};
    use crate::net::registry::ClientPacket;
    use crate::packets::c2g_world::CSMoveUnitPacket;
    use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};

    let server = server_with(cannon_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let owner = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    let attacker = spawn_player(&server, &b, test_character(2, "Attacker", 101.0, 100.0), PB);
    a.conn.active_char = Some(owner.clone());
    b.conn.active_char = Some(attacker.clone());
    let world = server.world.world(0).unwrap();
    let carrier = hull(&server, &owner, None);
    let cannon_id = carrier.data().attached_slaves[0];
    let cannon = world.slave(cannon_id).unwrap();
    if occupied {
        let tl = u32::from(cannon.data().unit.tl_id);
        slaves::bind_slave_by_tl(&server, &owner, tl);
    }
    // The authenticated movement path accepts a supporting object, but grants no gunner seat.
    CSMoveUnitPacket {
        obj_id: PB,
        move_type: MoveType::new(MoveKind::Unit(UnitMove {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            gc_id: cannon.obj_id,
            gc_flags: 1,
            actor_flags: MoveTypeActorFlags::STANDING_ON_OBJECT,
            ..UnitMove::default()
        })),
    }
    .handle(&server, &mut b.conn)
    .await
    .unwrap();
    assert_eq!(attacker.data().unit.transform.parent_obj_id, cannon.obj_id);
    assert_eq!(attacker.data().attached_point, attach_point::NONE);
    let target = spawn_npc_at(&server, 0x5000, CANNON_TARGET_NPC, 130.0, 130.0);
    target.data().unit.hp = 1000;
    target.data().unit.max_hp = 1000;
    let shot = CSStartSkillPacket {
        skill_id: FIRE_CANNON,
        caster: SkillCaster::Mount {
            obj_id: cannon.obj_id,
            mount_skill_template_id: CANNON_MOUNT_SKILL,
        },
        target: SkillCastTarget::Position {
            obj_id: 0,
            x: 130.0,
            y: 130.0,
            z: 100.0,
            rot: 0.0,
            obj_id1: 0,
            obj_id2: 0,
        },
        flag: 0,
        skill_object: SkillObject::None,
    };
    b.drain();
    start_skill(&server, &mut b.conn, shot.clone());
    assert_eq!(
        target.data().unit.hp,
        1000,
        "a parent pointer cannot grant a cannon shot"
    );
    assert!(!ops(&b.drain()).contains(&sc::SCSkillFiredPacket));
    if occupied {
        assert_eq!(
            cannon.data().attached_characters,
            vec![(attach_point::DRIVER, PA)]
        );
        // A stale or inconsistent attachment does not authorize the registered gunner either.
        world.detach_child(&WorldObject::from(&owner));
        start_skill(&server, &mut a.conn, shot.clone());
        assert_eq!(target.data().unit.hp, 1000);
        world.attach_child(&WorldObject::from(&cannon), &WorldObject::from(&owner));
        owner.data().attached_point = attach_point::NONE;
        start_skill(&server, &mut a.conn, shot.clone());
        assert_eq!(target.data().unit.hp, 1000);
        owner.data().attached_point = attach_point::DRIVER;
    } else {
        let tl = u32::from(cannon.data().unit.tl_id);
        slaves::bind_slave_by_tl(&server, &owner, tl);
    }
    start_skill(&server, &mut a.conn, shot);
    assert_eq!(
        target.data().unit.hp,
        850,
        "the registered, attached driver can fire"
    );
}

#[tokio::test]
async fn parent_only_attacker_cannot_fire_an_unoccupied_cannon() {
    parent_only_cannon_attack(false).await;
}

#[tokio::test]
async fn parent_only_attacker_cannot_fire_another_gunners_cannon() {
    parent_only_cannon_attack(true).await;
}

#[tokio::test]
async fn carrier_death_detaches_gunner_before_deleting_cannon() {
    let server = server_with(cannon_data());
    super::super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let owner = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    let world = server.world.world(0).unwrap();
    let carrier = hull(&server, &owner, None);
    let cannon_id = carrier.data().attached_slaves[0];
    let cannon = world.slave(cannon_id).unwrap();
    let tl = u32::from(cannon.data().unit.tl_id);
    slaves::bind_slave_by_tl(&server, &owner, tl);
    carrier
        .data()
        .unit
        .transform
        .set_position(250.0, 175.0, 90.0, 0.0, 0.0, 1.2);
    let before = world.world_pos_rot(&WorldObject::from(&owner));
    a.drain();
    slaves::do_die(
        &server,
        &carrier,
        Some(&WorldObject::from(&owner)),
        crate::systems::skills::types::KillReason::Damage,
    );
    assert!(world.slave(cannon.obj_id).is_none());
    assert!(
        cannon.data().attached_characters.is_empty(),
        "gunner seat released before removal"
    );
    let player = owner.data();
    assert_eq!(player.attached_point, attach_point::NONE);
    assert!(!player.is_riding);
    assert_eq!(player.unit.transform.parent_obj_id, 0);
    let after = player.unit.transform.local();
    drop(player);
    for (a, b) in [
        (before.x, after.x),
        (before.y, after.y),
        (before.z, after.z),
        (before.rot_z, after.rot_z),
    ] {
        assert!(
            (a - b).abs() < 1e-4,
            "world transform changed: {before:?} -> {after:?}"
        );
    }
    let mut detached = bc(PA);
    detached.push(slaves::attach_reason::NONE);
    assert_eq!(body(&a.drain(), sc::SCUnitDetachedPacket), detached);
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_compact_db_vehicle_defaults_spawn_on_legacy_hulls() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut d = slave_data();
    d.systems.vehicles = load_real(&db, &[]);
    d.items = crate::data::items::ItemData::load(&conn).unwrap();
    d.systems.npcs.doodad =
        crate::systems::npcs::doodad::data::DoodadData::load_from(&conn).unwrap();
    let server = server_with(d);
    let a = TestClient::new(&server, 1, 1);
    let owner = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    let world = server.world.world(0).unwrap();
    for id in [14, 21, 75, 76, 110] {
        let template = server
            .game_data
            .systems
            .vehicles
            .slave_template(id)
            .unwrap();
        assert!(server
            .game_data
            .systems
            .vehicles
            .slave_initial_item_pack(template.slave_initial_item_pack_id)
            .is_none());
        let carrier = slaves::create_core(
            &server,
            &world,
            Some(&owner),
            None,
            id,
            None,
            false,
            None,
            Some(saved(700 + id)),
        )
        .unwrap();
        let children = carrier.data().attached_slaves.clone();
        assert_eq!(
            children.len(),
            template.slave_bindings.len(),
            "hull {id}: all child defaults"
        );
        for binding in &template.slave_bindings {
            assert_eq!(
                children
                    .iter()
                    .filter(|child| {
                        let child = world.slave(**child).unwrap();
                        let child = child.data();
                        child.attach_point_id as u8 == binding.attach_point_id
                            && child.unit.template_id == binding.slave_id
                    })
                    .count(),
                1,
                "hull {id} point {}",
                binding.attach_point_id
            );
        }
        let doodads = carrier.data().attached_doodads.clone();
        for binding in &template.doodad_bindings {
            assert_eq!(
                doodads
                    .iter()
                    .filter(|child| {
                        let child = world.doodad(**child).unwrap();
                        let child = child.data();
                        child.attach_point == binding.attach_point_id
                            && child.template_id == binding.doodad_id
                    })
                    .count(),
                1,
                "hull {id} doodad point {}",
                binding.attach_point_id
            );
        }
        slaves::delete(&server, Some(&owner), world.id, carrier.obj_id, false);
    }
}
