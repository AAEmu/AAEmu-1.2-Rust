//! Deterministic source tests; no runtime DB, live stack, or asset dependency.

use super::*;
use crate::data::items::{ItemTemplate, TemplateKind};
use crate::data::GameData;
use crate::models::doodad::Doodad;
use crate::systems::vehicles::audit::{self, Request};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use futures::{poll, FutureExt};
use serde_json::{json, Value};

fn request() -> Request {
    Request::parse(&["7".into(), "4001".into(), "test_N-1".into()]).unwrap()
}

fn fixture() -> (Arc<GameServer>, TestClient, PlayerRef) {
    let mut data = GameData::empty();
    data.items.templates.insert(
        23398,
        ItemTemplate {
            kind: TemplateKind::SummonSlave { slave_id: 75 },
            ..ItemTemplate::plain(23398, 1)
        },
    );
    data.systems.vehicles.slave_templates.insert(
        75,
        SlaveTemplate {
            id: 75,
            ..Default::default()
        },
    );
    let server = Arc::new(test_server_with(data));
    server.set_self();
    let client = TestClient::new(&server, 7, 1007);
    let owner = spawn_player(
        &server,
        &client,
        test_character(7, "Owner", 100.0, 100.0),
        700,
    );
    let bag = owner
        .data()
        .inventory
        .get(slot_type::INVENTORY)
        .unwrap()
        .clone();
    let mut item = Item::new(0);
    item.id = 4001;
    item.class = ItemClass::SummonSlave;
    item.template_id = 23398;
    item.owner_id = 7;
    item.slot_type = slot_type::INVENTORY;
    item.count = 1;
    item.details.slave_db_id = 123;
    bag.lock().unwrap().items.push(item);
    server.items.add_container(bag);
    (server, client, owner)
}

#[tokio::test]
async fn audit_pending_before_poll_and_early_completion_release() {
    let (server, _client, owner) = fixture();
    let future = create_for_owner(&server, &owner, 4001, 23398, false, None, None, 0.0);
    let snap = audit::snapshot(&server, &request());
    assert_eq!(snap["pending_summons"], 1);
    assert_eq!(snap["jobs"][0]["phase"], "queued");
    assert!(snap["candidates"].as_array().unwrap().is_empty());
    drop(future); // Never polled, so no reservation/DB/gameplay work has run.
    assert_eq!(audit::snapshot(&server, &request())["pending_summons"], 0);
    let world = server.world.world(0).unwrap();
    let unpolled = create(
        &server,
        &world,
        Some(&owner),
        75,
        server.items.item_by_id(4001),
        false,
        None,
    );
    assert_eq!(
        audit::snapshot(&server, &request())["jobs"][0]["phase"],
        "queued"
    );
    drop(unpolled);
    assert_eq!(audit::snapshot(&server, &request())["pending_summons"], 0);
    assert!(
        create_for_owner(&server, &owner, 4001, 0, false, None, None, 0.0)
            .await
            .is_none()
    );
    assert_eq!(audit::snapshot(&server, &request())["pending_summons"], 0);
}

#[tokio::test]
async fn audit_real_owner_and_item_waiters_release_on_cancellation() {
    let (server, _client, owner) = fixture();
    let summons = &server.systems.vehicles.slave_summons;
    *summons.test_loader.lock().unwrap() = Some(TestSlaveLoader(Arc::new(|_, _| {
        Box::pin(std::future::pending())
    })));
    let mut active = Box::pin(create_for_owner(
        &server, &owner, 4001, 23398, false, None, None, 0.0,
    ));
    assert!(poll!(&mut active).is_pending());
    let mut waiter = Box::pin(create_for_owner(
        &server, &owner, 4001, 23398, false, None, None, 0.0,
    ));
    assert!(poll!(&mut waiter).is_pending());
    let snap = audit::snapshot(&server, &request());
    assert_eq!(snap["pending_summons"], 2);
    assert_eq!(snap["jobs"][0]["phase"], "active");
    assert_eq!(snap["jobs"][1]["phase"], "waiting_owner");
    drop(waiter);
    drop(active);
    assert_eq!(audit::snapshot(&server, &request())["pending_summons"], 0);
    let item_lock = summons
        .reservation(SummonKey::Item(4001))
        .lock_owned()
        .await;
    let mut waiter = Box::pin(create_for_owner(
        &server, &owner, 4001, 23398, false, None, None, 0.0,
    ));
    assert!(poll!(&mut waiter).is_pending());
    assert_eq!(
        audit::snapshot(&server, &request())["jobs"][0]["phase"],
        "waiting_item"
    );
    drop(waiter);
    drop(item_lock);
    assert_eq!(audit::snapshot(&server, &request())["pending_summons"], 0);
    assert!(summons.reservation(SummonKey::Owner(7)).try_lock().is_ok());
}

#[tokio::test]
async fn audit_real_loader_panic_releases_job_and_authority_reservations() {
    let (server, _client, owner) = fixture();
    let summons = &server.systems.vehicles.slave_summons;
    *summons.test_loader.lock().unwrap() = Some(TestSlaveLoader(Arc::new(|_, _| {
        Box::pin(async { panic!("audit loader unwind") })
    })));
    let future = create_for_owner(&server, &owner, 4001, 23398, false, None, None, 0.0);
    assert!(std::panic::AssertUnwindSafe(future)
        .catch_unwind()
        .await
        .is_err());
    assert_eq!(audit::snapshot(&server, &request())["pending_summons"], 0);
    assert!(summons.reservation(SummonKey::Owner(7)).try_lock().is_ok());
    assert!(summons
        .reservation(SummonKey::Item(4001))
        .try_lock()
        .is_ok());
}

#[tokio::test]
async fn audit_detached_skill_is_pending_before_runtime_polls_it() {
    let (server, _client, owner) = fixture();
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let signal = entered.clone();
    *server
        .systems
        .vehicles
        .slave_summons
        .test_loader
        .lock()
        .unwrap() = Some(TestSlaveLoader(Arc::new(move |_, _| {
        let signal = signal.clone();
        Box::pin(async move {
            signal.add_permits(1);
            Err(sqlx::Error::RowNotFound)
        })
    })));
    create_from_skill(
        &server,
        &owner,
        &SkillCaster::Item {
            obj_id: owner.obj_id,
            item_id: 4001,
            item_template_id: 23398,
            type1: 0,
            type2: 0,
        },
        None,
        0.0,
    );
    assert_eq!(
        audit::snapshot(&server, &request())["jobs"][0]["phase"],
        "queued"
    );
    entered.acquire().await.unwrap().forget();
    assert_eq!(audit::snapshot(&server, &request())["pending_summons"], 0);
}

fn hull(world: &WorldInstance, object: u32, owner: u32, item: u64, child: bool) -> SlaveRef {
    let mut slave = Slave::new(object + 1000, 75);
    slave.summoner_id = owner;
    slave.summoning_item_id = item;
    slave.owner_type = if child {
        BaseUnitType::Slave
    } else {
        BaseUnitType::Character
    };
    slave.unit.hp = 10;
    slave.unit.transform.instance_id = world.id;
    let slave = Entity::new(object, slave);
    world.add_object(&slave); // Hidden objects must be included without visibility filtering.
    slave
}

#[tokio::test]
async fn audit_retirement_task_leases_release_on_abort_and_panic() {
    let (server, _client, _owner) = fixture();
    let world = server.world.world(0).unwrap();
    let root = hull(&world, 850, 7, 4001, false);
    let diagnostics = &server.systems.vehicles.slave_summons.diagnostics;
    let lease = diagnostics.retirement(&root, 1);
    let task = tokio::spawn(async move {
        let _lease = lease;
        std::future::pending::<()>().await;
    });
    // No await since construction: the task has not been polled on this runtime.
    assert_eq!(
        audit::snapshot(&server, &request())["pending_retirement_tasks"],
        1
    );
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(
        audit::snapshot(&server, &request())["pending_retirement_tasks"],
        0
    );
    let lease = diagnostics.retirement(&root, 2);
    let task: tokio::task::JoinHandle<()> = tokio::spawn(async move {
        let _lease = lease;
        panic!("retirement task unwind");
    });
    assert!(task.await.unwrap_err().is_panic());
    assert_eq!(
        audit::snapshot(&server, &request())["pending_retirement_tasks"],
        0
    );
}

#[tokio::test]
async fn audit_observation_does_not_extend_retired_hull_lifetime() {
    let (server, _client, _owner) = fixture();
    let world = server.world.world(0).unwrap();
    let root = hull(&world, 851, 7, 4001, false);
    audit_retirement(&server, &world, &root);
    world.remove_object(root.obj_id);
    assert_eq!(Arc::strong_count(&root), 1);
    assert_eq!(
        audit::snapshot(&server, &request())["candidates"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(Arc::strong_count(&root), 1);
    drop(root);
    assert!(audit::snapshot(&server, &request())["candidates"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn audit_all_worlds_hidden_dead_children_retiring_and_owner_item_isolation() {
    let (server, _client, _owner) = fixture();
    let world = server.world.world(0).unwrap();
    let other = server
        .world
        .create_world_instance(world.template.clone(), 42);
    let root = hull(&world, 800, 7, 4001, false);
    let child = hull(&other, 801, 7, 0, true);
    child.data().unit.hp = 0;
    let foreign_item = hull(&other, 802, 8, 4001, false);
    let _unrelated = hull(&other, 803, 8, 4002, false);
    let retiring = hull(&world, 804, 7, 4001, false);
    let retirement = server
        .systems
        .vehicles
        .slave_summons
        .diagnostics
        .retirement(&retiring, 88);
    audit_retirement(&server, &world, &retiring);
    server
        .systems
        .npcs
        .spawns
        .get_or_create(&world)
        .add_despawn(
            &server,
            WorldObject::from(&retiring),
            Some(spawn_manager::now()),
        );
    world.remove_object(retiring.obj_id);
    let _unrelated_job = server
        .systems
        .vehicles
        .slave_summons
        .diagnostics
        .summon(8, 4002);
    let snap = audit::snapshot(&server, &request());
    assert_eq!(snap["world_instances"], json!([0, 42]));
    let candidates = snap["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 4);
    let get = |id: u32| candidates.iter().find(|c| c["object_id"] == id).unwrap();
    assert_eq!(get(root.obj_id)["visible"], false);
    assert_eq!(get(root.obj_id)["owner_root_lookup_member"], true);
    assert_eq!(get(child.obj_id)["dead"], true);
    assert_eq!(get(child.obj_id)["owner_root_lookup_member"], false);
    assert_eq!(
        get(foreign_item.obj_id)["item_live_root_lookup_member"],
        true
    );
    assert_eq!(get(retiring.obj_id)["registered"], false);
    assert_eq!(snap["pending_retirement_tasks"], 1);
    assert_eq!(snap["pending_summons"], 0);
    assert_eq!(
        snap["queues"][0]["matching_despawns_sample"][0]["object_id"],
        retiring.obj_id
    );
    assert_eq!(snap["consistent"], false);
    drop(retirement);
    assert_eq!(
        audit::snapshot(&server, &request())["pending_retirement_tasks"],
        0
    );
}

#[tokio::test]
async fn audit_twenty_real_rack_references_and_items_are_read_only() {
    let (server, _client, _owner) = fixture();
    let world = server.world.world(0).unwrap();
    let root = hull(&world, 900, 7, 4001, false);
    let system =
        server
            .items
            .container_for_character(7, slot_type::SYSTEM, 0, &server.ids.container);
    let mut racks = Vec::new();
    for attach in 60..=79u8 {
        let mut item = Item::new(0);
        item.id = 10_000 + u64::from(attach);
        item.template_id = 20090;
        item.count = 1;
        item.owner_id = 7;
        item.slot_type = slot_type::SYSTEM;
        let mut rack = Doodad::new(3446);
        rack.db_id = 100 + u32::from(attach);
        rack.attach_point = attach;
        rack.parent_obj_id = root.obj_id;
        rack.transform.parent_obj_id = root.obj_id;
        rack.func_group_id = 8265;
        rack.item_id = item.id;
        rack.item_template_id = item.template_id;
        system.lock().unwrap().items.push(item);
        let rack = Entity::new(1000 + u32::from(attach), rack);
        world.add_object(&rack);
        root.data().attached_doodads.push(rack.obj_id);
        racks.push(rack);
    }
    let root_before = root.data().clone();
    let racks_before: Vec<_> = racks.iter().map(|r| r.data().clone()).collect();
    let items_before = system.lock().unwrap().items.clone();
    let dirty_before = system.lock().unwrap().dirty;
    let removed_before = server.items.removed_items();
    let containers_before = server.items.all_containers().len();
    let snap = audit::snapshot(&server, &request());
    assert_eq!(snap["summon_item"]["lookup"]["saved_slave_db_id"], 123);
    let candidate = &snap["candidates"][0];
    assert_eq!(candidate["doodads"].as_array().unwrap().len(), 20);
    assert_eq!(candidate["missing_rack_attach"], json!([]));
    for rack in candidate["doodads"].as_array().unwrap() {
        let attach = rack["attach"].as_u64().unwrap();
        assert_eq!(rack["item"]["lookup"]["id"], 10_000 + attach);
        assert_eq!(
            rack["item"]["containers"][0]["slot_type"],
            slot_type::SYSTEM
        );
        assert_eq!(rack["item"]["containers"][0]["items"][0]["count"], 1);
    }
    assert_eq!(*root.data(), root_before);
    assert_eq!(
        racks.iter().map(|r| r.data().clone()).collect::<Vec<_>>(),
        racks_before
    );
    assert_eq!(system.lock().unwrap().items, items_before);
    assert_eq!(system.lock().unwrap().dirty, dirty_before);
    assert_eq!(server.items.removed_items(), removed_before);
    assert_eq!(server.items.all_containers().len(), containers_before);
}

#[test]
fn audit_request_rejects_malformed_ids_nonce_and_extra_arguments() {
    for args in [
        vec!["7", "1"],
        vec!["0", "1", "n"],
        vec!["7", "0", "n"],
        vec!["-1", "1", "n"],
        vec!["+7", "1", "n"],
        vec!["4294967296", "1", "n"],
        vec!["7", "18446744073709551616", "n"],
        vec!["7", "1", "bad|nonce"],
        vec!["7", "1", "n", "extra"],
        vec!["7", "1", ""],
        vec!["7", "1", "é"],
    ] {
        assert!(
            Request::parse(&args.iter().map(|s| (*s).into()).collect::<Vec<_>>()).is_err(),
            "{args:?}"
        );
    }
    assert!(Request::parse(&["7".into(), "1".into(), "x".repeat(33)]).is_err());
    assert!(Request::parse(&[u32::MAX.to_string(), u64::MAX.to_string(), "x".repeat(32)]).is_ok());
}

// A strict independent consumer: reject missing, stale, duplicate, corrupt or partial replies.
fn reassemble(lines: &[String]) -> Option<Value> {
    let end: Vec<_> = lines.last()?.split_ascii_whitespace().collect();
    if end.len() != 7 || end[0] != audit::PREFIX || end[3] != "END" {
        return None;
    }
    let count = end[4].parse::<usize>().ok()?;
    let length = end[5].parse::<usize>().ok()?;
    if lines.len() != count + 1 {
        return None;
    }
    let mut bytes = Vec::new();
    for (index, line) in lines[..count].iter().enumerate() {
        let fields: Vec<_> = line.split_ascii_whitespace().collect();
        if fields.len() != 8
            || fields[..3] != end[..3]
            || fields[3] != "DATA"
            || fields[4] != format!("{}/{count}", index + 1)
            || fields[5] != end[5]
            || fields[6] != end[6]
        {
            return None;
        }
        let payload = fields[7].strip_prefix("hex:")?;
        if !payload.len().is_multiple_of(2) {
            return None;
        }
        for i in (0..payload.len()).step_by(2) {
            bytes.push(u8::from_str_radix(payload.get(i..i + 2)?, 16).ok()?);
        }
    }
    let hash = bytes.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    });
    if bytes.len() != length || end[6] != format!("fnv1a64:{hash:016x}") {
        return None;
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    if value["nonce"] != end[1] || value["sample_sequence"].to_string() != end[2] {
        return None;
    }
    Some(value)
}

#[test]
fn audit_chunks_are_bounded_complete_and_reject_corrupt_or_stale_data() {
    let req = request();
    let value = json!({"nonce": req.nonce, "sample_sequence": 83, "payload": "船".repeat(300)});
    let lines = audit::chunks(&req, &value).unwrap();
    assert!(lines.iter().all(|l| l.len() < 450));
    assert_eq!(reassemble(&lines), Some(value));
    assert!(reassemble(&lines[..lines.len() - 1]).is_none());
    let mut missing = lines.clone();
    missing.remove(1);
    assert!(reassemble(&missing).is_none());
    let mut duplicate = lines.clone();
    duplicate[1] = duplicate[0].clone();
    assert!(reassemble(&duplicate).is_none());
    let mut stale = lines.clone();
    stale[0] = stale[0].replace("test_N-1", "old");
    assert!(reassemble(&stale).is_none());
    let mut corrupt = lines.clone();
    corrupt[0].push_str("00");
    assert!(reassemble(&corrupt).is_none());
    let huge = json!({"nonce": req.nonce, "sample_sequence": 83, "payload": "x".repeat(70_000)});
    let unsupported = reassemble(&audit::chunks(&req, &huge).unwrap()).unwrap();
    assert_eq!(unsupported["consistent"], false);
    assert!(unsupported.get("payload").is_none());
}

#[tokio::test]
async fn audit_invalid_command_and_gm_authorization_use_existing_root() {
    use crate::systems::services::commands::scripts::harness;
    let server = harness::server();
    let gm = harness::pc(&server, 7, "Observer", 100.0, 100.0);
    let (_, denied) = harness::run_at(&server, &gm.p, "slave audit 7 4001 valid", 0).await;
    assert!(denied.iter().all(|s| !s.starts_with(audit::PREFIX)));
    for command in [
        "slave audit 7 4001 bad|nonce",
        "slave audit 7 4001 n extra",
        "slave audit 0 4001 n",
    ] {
        let (_, lines) = harness::run(&server, &gm.p, command).await;
        assert!(lines.iter().all(|s| !s.starts_with(audit::PREFIX)));
    }
    let (_, lines) = harness::run(&server, &gm.p, "slave audit 7 4001 valid").await;
    assert_eq!(reassemble(&lines).unwrap()["consistent"], false);
    // This also verifies requesting an absent owner/item does not create any containers/items.
    assert!(server.items.all_containers().is_empty());
}
