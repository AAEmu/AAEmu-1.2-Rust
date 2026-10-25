//! Ship identity and session regressions. Synthetic fixtures exercise lifecycle ordering;
//! the gated temporary-table fixture exercises the production MySQL lookup/save fence.

use super::*;
use crate::models::doodad::{doodad_owner_type, Doodad};
use crate::models::unit::BaseUnitType;
use crate::world::{Entity, SlaveRef};
use futures::poll;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Semaphore;

const MERCHANT: u32 = 75;
const MERCHANT_ITEM: u32 = 23398;
const SECOND_SHIP_ITEM: u32 = 23400;

fn identity_data() -> GameData {
    let mut data = ship_equip_data();
    data.systems
        .vehicles
        .slave_templates
        .get_mut(&SHIP_HULL)
        .unwrap()
        .slave_kind = super::super::data::slave_kind::SMALL_SAILING_SHIP;
    let mut merchant = data.systems.vehicles.slave_templates[&SHIP_HULL].clone();
    merchant.id = MERCHANT;
    merchant.name = "Merchant Ship".into();
    merchant.model_id = 1205;
    merchant.slave_kind = super::super::data::slave_kind::MERCHANT_SHIP;
    merchant.portal_time = 2.0;
    for binding in &mut merchant.slave_bindings {
        binding.owner_id = MERCHANT;
    }
    let mut slots = data.systems.vehicles.slave_equip_slots[&SHIP_HULL].clone();
    for slot in &mut slots {
        slot.slave_id = MERCHANT;
    }
    data.systems
        .vehicles
        .slave_templates
        .insert(MERCHANT, merchant);
    data.systems
        .vehicles
        .slave_equip_slots
        .insert(MERCHANT, slots);
    let points = data.systems.vehicles.attach_points[&511].clone();
    data.systems.vehicles.attach_points.insert(1205, points);
    data.systems
        .vehicles
        .slave_equip_packs
        .get_mut(&4)
        .unwrap()
        .push(MERCHANT);
    for (item_id, hull_id) in [(MERCHANT_ITEM, MERCHANT), (SECOND_SHIP_ITEM, SHIP_HULL)] {
        data.items.templates.insert(
            item_id,
            ItemTemplate {
                kind: TemplateKind::SummonSlave { slave_id: hull_id },
                ..ItemTemplate::plain(item_id, 1)
            },
        );
    }
    data
}

fn owner_with_ship(
    server: &GameServer,
    client: &TestClient,
    character_id: u32,
    obj_id: u32,
    template: u32,
) -> (PlayerRef, u64) {
    let (owner, item_id) = player_with_item(
        server,
        client,
        character_id,
        obj_id,
        (100.0, 100.0),
        template,
    );
    let bag = owner.data().inventory.bag().clone();
    bag.lock().unwrap().items[0].owner_id = u64::from(character_id);
    server.items.add_container(bag);
    (owner, item_id)
}

fn second_item(server: &GameServer, owner: &PlayerRef, item_id: u64, template: u32) {
    let mut item = Item::create(
        item_id,
        server.game_data.items.template(template).unwrap(),
        1,
        0,
        1,
        &server.game_data.items,
        0,
    );
    item.owner_id = u64::from(owner.character_id);
    item.slot_type = slot_type::INVENTORY;
    item.slot = 1;
    let bag = owner.data().inventory.bag().clone();
    bag.lock().unwrap().items.push(item);
}

fn root(server: &GameServer, owner: &PlayerRef, item_id: u64, hull_id: u32) -> SlaveRef {
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let item = mates::owned_item(server, owner, item_id).unwrap();
    slaves::create_core(
        server,
        &world,
        Some(owner),
        None,
        hull_id,
        Some(item),
        false,
        Some(crate::models::transform::Transform {
            x: 100.0,
            y: 100.0,
            z: 200.0,
            ..Default::default()
        }),
        None,
    )
    .unwrap()
}

fn roots(server: &GameServer, owner: u32) -> Vec<SlaveRef> {
    server
        .world
        .worlds()
        .into_iter()
        .flat_map(|world| slaves::all_slaves(&world))
        .filter(|slave| {
            let d = slave.data();
            d.owner_type == BaseUnitType::Character && d.summoner_id == owner && !d.unit.is_dead()
        })
        .collect()
}

fn persistent_id(slave: &SlaveRef) -> u32 {
    slave.data().id
}

fn loaded_rack(server: &GameServer, hull: &SlaveRef) -> crate::world::DoodadRef {
    let world = server.world.world(0).unwrap();
    let mut rack = Doodad::new(3446);
    rack.item_id = 0xdead;
    rack.item_template_id = 29203;
    rack.attach_point = 60;
    rack.func_group_id = 8265;
    rack.owner_type = doodad_owner_type::SLAVE;
    rack.owner_id = hull.data().summoner_id;
    rack.owner_db_id = hull.data().id;
    let rack = Entity::new(server.ids.object.get_next_id().unwrap(), rack);
    world.attach_child(&WorldObject::from(hull), &WorldObject::from(&rack));
    hull.data().attached_doodads.push(rack.obj_id);
    world.spawn(&rack, &server.view_ctx());
    rack
}

/// Model a saved hull's identity without a database. The production persistence tests below
/// bypass this loader; a nonzero detail id is explicitly a saved identity, never a live flag.
fn install_memory_loader(server: &Arc<GameServer>) {
    let weak = Arc::downgrade(server);
    *server
        .systems
        .vehicles
        .slave_summons
        .test_loader
        .lock()
        .unwrap() = Some(slaves::TestSlaveLoader(Arc::new(move |_, item_id| {
        let server = weak.upgrade().unwrap();
        let id = server
            .items
            .item_by_id(item_id)
            .unwrap()
            .details
            .slave_db_id;
        Box::pin(async move {
            Ok(slaves::LoadedHull {
                saved: (id != 0).then_some(slaves::SavedSlave {
                    id,
                    name: "Saved hull".into(),
                    hp: 100,
                    mp: 10,
                }),
                children: Vec::new(),
            })
        })
    })));
}

fn install_barrier_loader(server: &GameServer) -> (Arc<AtomicUsize>, Arc<Semaphore>) {
    let entered = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (calls, permits) = (entered.clone(), release.clone());
    *server
        .systems
        .vehicles
        .slave_summons
        .test_loader
        .lock()
        .unwrap() = Some(slaves::TestSlaveLoader(Arc::new(move |_, _| {
        calls.fetch_add(1, Ordering::SeqCst);
        let permits = permits.clone();
        Box::pin(async move {
            permits.acquire_owned().await.unwrap().forget();
            Ok(slaves::LoadedHull::default())
        })
    })));
    (entered, release)
}

async fn summon(
    server: &GameServer,
    owner: &PlayerRef,
    item_id: u64,
    template: u32,
) -> Option<SlaveRef> {
    slaves::create_for_owner(server, owner, item_id, template, false, None, None, 0.0).await
}

#[tokio::test]
async fn merchant_loaded_refusal_preserves_hull_rider_gunner_and_item_identity() {
    let server = server_with(identity_data());
    let mut owner_client = TestClient::new(&server, 1, 1);
    let mut gunner_client = TestClient::new(&server, 2, 2);
    let (owner, item_id) = owner_with_ship(&server, &owner_client, 1, PA, MERCHANT_ITEM);
    let gunner = spawn_player(
        &server,
        &gunner_client,
        test_character(2, "Gunner", 100.0, 100.0),
        PB,
    );
    let hull = root(&server, &owner, item_id, MERCHANT);
    let world = server.world.world(0).unwrap();
    let cannon_id = hull.data().attached_slaves[0];
    let cannon = world.slave(cannon_id).unwrap();
    let tl = u32::from(hull.data().unit.tl_id);
    slaves::bind_slave_by_tl(&server, &owner, tl);
    let pos = world.world_pos_rot(&WorldObject::from(&cannon));
    gunner.data().unit.transform.set_xyz(pos.x, pos.y, pos.z);
    world.attach_child(&WorldObject::from(&hull), &WorldObject::from(&gunner));
    let cannon_tl = u32::from(cannon.data().unit.tl_id);
    slaves::bind_slave_by_tl(&server, &gunner, cannon_tl);
    assert!(!hull.data().attached_characters.is_empty());
    assert!(!cannon.data().attached_characters.is_empty());
    let rack = loaded_rack(&server, &hull);
    let hull_before = hull.data().clone();
    let cannon_before = cannon.data().clone();
    let rack_before = rack.data().clone();
    let item_before = mates::owned_item(&server, &owner, item_id).unwrap();
    owner_client.drain();
    gunner_client.drain();

    slaves::delete(&server, Some(&owner), world.id, hull.obj_id, false);
    assert!(summon(&server, &owner, item_id, MERCHANT_ITEM)
        .await
        .is_none());
    let item = mates::owned_item(&server, &owner, item_id).unwrap();
    assert!(slaves::create(
        &server,
        &world,
        Some(&owner),
        MERCHANT,
        Some(item),
        false,
        None
    )
    .await
    .is_none());
    assert_eq!(
        roots(&server, 1)
            .iter()
            .map(|s| s.obj_id)
            .collect::<Vec<_>>(),
        vec![hull.obj_id]
    );
    assert_eq!(*hull.data(), hull_before);
    assert_eq!(*cannon.data(), cannon_before);
    assert_eq!(*rack.data(), rack_before);
    assert_eq!(
        mates::owned_item(&server, &owner, item_id).unwrap(),
        item_before
    );
    assert_eq!(owner.data().unit.transform.parent_obj_id, hull.obj_id);
    assert_eq!(gunner.data().unit.transform.parent_obj_id, cannon.obj_id);
    let frames = owner_client.drain();
    let gunner_frames = gunner_client.drain();
    assert!(error_codes(&frames).contains(&super::super::error::SLAVE_EQUIPMENT_LOADED_ITEM));
    for op in [
        sc::SCSlaveCreatedPacket,
        sc::SCSlaveDespawnPacket,
        sc::SCSlaveRemovedPacket,
        sc::SCUnitDetachedPacket,
        sc::SCItemTaskSuccessPacket,
    ] {
        assert!(!ops(&frames).contains(&op));
        assert!(!ops(&gunner_frames).contains(&op));
    }
    assert!(server
        .systems
        .npcs
        .spawns
        .get(0)
        .unwrap()
        .pending_despawns()
        .is_empty());
}

#[tokio::test]
async fn cannon_children_never_qualify_as_an_active_root() {
    let server = server_with(identity_data());
    let mut client = TestClient::new(&server, 1, 1);
    let (owner, item_id) = owner_with_ship(&server, &client, 1, PA, SECOND_SHIP_ITEM);
    let hull = root(&server, &owner, item_id, SHIP_HULL);
    let world = server.world.world(0).unwrap();
    assert!(!hull.data().attached_slaves.is_empty());
    assert_eq!(
        slaves::get_active_slave_by_owner_obj_id(&world, PA)
            .unwrap()
            .obj_id,
        hull.obj_id
    );
    client.drain();
    slaves::send_my_slave_packet_to_all_owners(&server);
    let notifications = client.drain();
    assert_eq!(ops(&notifications), vec![sc::SCMySlavePacket]);
    assert!(body(&notifications, sc::SCMySlavePacket).starts_with(&bc(hull.obj_id)));
    // Deterministic: even when it is the only slave in the table, the cannon is not a root.
    world.remove_object(hull.obj_id);
    assert!(slaves::get_active_slave_by_owner_obj_id(&world, PA).is_none());
    assert_eq!(super::super::leave_extra_logout_ms(&server, &owner), 0);
    slaves::send_my_slave_packet_to_all_owners(&server);
    assert!(client.drain().is_empty());
}

#[tokio::test]
async fn invalid_item_placement_and_repair_leave_the_prior_hull_intact() {
    let server = server_with(identity_data());
    let mut client = TestClient::new(&server, 1, 1);
    let (owner, item_id) = owner_with_ship(&server, &client, 1, PA, SECOND_SHIP_ITEM);
    let hull = root(&server, &owner, item_id, SHIP_HULL);
    let original = hull.data().clone();
    client.drain();
    assert!(summon(&server, &owner, item_id, MERCHANT_ITEM)
        .await
        .is_none());
    assert!(slaves::create_for_owner(
        &server,
        &owner,
        item_id,
        SECOND_SHIP_ITEM,
        false,
        None,
        Some(crate::systems::skills::ext::SummonPlacement {
            x: 500.0,
            y: 500.0,
            z: 200.0,
            yaw: 0.0,
        }),
        10.0
    )
    .await
    .is_none());
    mates::update_owned_item(&server, Some(&owner), item_id, |item| {
        item.details.slave_is_destroyed = 1;
        item.details.slave_repair_start_time = chrono::Utc::now().timestamp();
    });
    assert!(summon(&server, &owner, item_id, SECOND_SHIP_ITEM)
        .await
        .is_none());
    assert_eq!(*hull.data(), original);
    assert_eq!(roots(&server, 1).len(), 1);
    assert!(!ops(&client.drain()).contains(&sc::SCSlaveDespawnPacket));
}

#[tokio::test]
async fn owner_reservation_serializes_requests_across_the_load_await() {
    let server = server_with(identity_data());
    let client = TestClient::new(&server, 1, 1);
    let (owner, item_id) = owner_with_ship(&server, &client, 1, PA, SECOND_SHIP_ITEM);
    let (calls, release) = install_barrier_loader(&server);
    let mut first = Box::pin(summon(&server, &owner, item_id, SECOND_SHIP_ITEM));
    let mut second = Box::pin(summon(&server, &owner, item_id, SECOND_SHIP_ITEM));
    assert!(poll!(first.as_mut()).is_pending());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(poll!(second.as_mut()).is_pending());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "second request must wait before querying"
    );
    release.add_permits(1);
    let first_hull = first.await.unwrap();
    assert!(poll!(second.as_mut()).is_pending());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    release.add_permits(1);
    let second_hull = second.await.unwrap();
    assert_ne!(first_hull.obj_id, second_hull.obj_id);
    assert_eq!(
        roots(&server, 1)
            .iter()
            .map(|s| s.obj_id)
            .collect::<Vec<_>>(),
        vec![second_hull.obj_id]
    );
}

#[tokio::test]
async fn cancelled_load_releases_reservation_and_departed_session_cannot_publish() {
    let server = server_with(identity_data());
    let mut client = TestClient::new(&server, 1, 1);
    let (owner, item_id) = owner_with_ship(&server, &client, 1, PA, SECOND_SHIP_ITEM);
    let (calls, release) = install_barrier_loader(&server);
    let mut cancelled = Box::pin(summon(&server, &owner, item_id, SECOND_SHIP_ITEM));
    assert!(poll!(cancelled.as_mut()).is_pending());
    drop(cancelled);
    let mut pending = Box::pin(summon(&server, &owner, item_id, SECOND_SHIP_ITEM));
    assert!(poll!(pending.as_mut()).is_pending());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "cancel released owner and item guards"
    );
    super::super::on_disconnect(&server, &owner);
    // Even before world removal, the departure marker closes the publication window.
    release.add_permits(1);
    assert!(pending.await.is_none());
    assert!(roots(&server, 1).is_empty());
    assert!(!ops(&client.drain()).contains(&sc::SCSlaveCreatedPacket));

    server.world.world(0).unwrap().delete(&owner);
    server.world.try_remove_character(owner.obj_id);
    let reconnect = TestClient::new(&server, 2, 1);
    let (new_owner, new_item) = owner_with_ship(&server, &reconnect, 1, PA, SECOND_SHIP_ITEM);
    super::super::on_character_selected(&server, &new_owner);
    install_memory_loader(&server);
    assert!(summon(&server, &new_owner, new_item, SECOND_SHIP_ITEM)
        .await
        .is_some());
    assert!(summon(&server, &owner, item_id, SECOND_SHIP_ITEM)
        .await
        .is_none());
    assert_eq!(roots(&server, 1).len(), 1);
}

#[tokio::test]
async fn reconnect_during_an_old_sessions_load_cannot_publish_its_retained_player() {
    let server = server_with(identity_data());
    let mut old_client = TestClient::new(&server, 1, 1);
    let (old_owner, item_id) = owner_with_ship(&server, &old_client, 1, PA, SECOND_SHIP_ITEM);
    let (calls, release) = install_barrier_loader(&server);
    let mut old_request = Box::pin(summon(&server, &old_owner, item_id, SECOND_SHIP_ITEM));
    assert!(poll!(old_request.as_mut()).is_pending());
    super::super::on_disconnect(&server, &old_owner);
    server.world.world(0).unwrap().delete(&old_owner);
    server.world.try_remove_character(old_owner.obj_id);
    let reconnect = TestClient::new(&server, 2, 1);
    let character = old_owner.data().clone();
    let new_owner = spawn_player(&server, &reconnect, character, PA);
    super::super::on_character_selected(&server, &new_owner);
    let mut new_request = Box::pin(summon(&server, &new_owner, item_id, SECOND_SHIP_ITEM));
    assert!(poll!(new_request.as_mut()).is_pending());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.add_permits(1);
    assert!(old_request.await.is_none());
    assert!(!ops(&old_client.drain()).contains(&sc::SCSlaveCreatedPacket));
    assert!(poll!(new_request.as_mut()).is_pending());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    release.add_permits(1);
    new_request.await.unwrap();
    assert_eq!(roots(&server, 1).len(), 1);
}

#[tokio::test]
async fn another_owner_can_load_while_the_first_owner_is_waiting() {
    let server = server_with(identity_data());
    let a = TestClient::new(&server, 1, 1);
    let b = TestClient::new(&server, 2, 2);
    let (owner, item_id) = owner_with_ship(&server, &a, 1, PA, SECOND_SHIP_ITEM);
    let (other, other_item) = owner_with_ship(&server, &b, 2, PB, SECOND_SHIP_ITEM);
    let (calls, _) = install_barrier_loader(&server);
    let mut first = Box::pin(summon(&server, &owner, item_id, SECOND_SHIP_ITEM));
    let mut second = Box::pin(summon(&server, &other, other_item, SECOND_SHIP_ITEM));
    assert!(poll!(first.as_mut()).is_pending());
    assert!(poll!(second.as_mut()).is_pending());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn item_reservation_also_serializes_conflicting_owners() {
    let server = server_with(identity_data());
    let a = TestClient::new(&server, 1, 1);
    let b = TestClient::new(&server, 2, 2);
    let (owner, item_id) = owner_with_ship(&server, &a, 1, PA, SECOND_SHIP_ITEM);
    let (other, _) = owner_with_ship(&server, &b, 2, PB, SECOND_SHIP_ITEM);
    // Deliberately conflicting inventory snapshots must not publish the same persistent item
    // twice, even when each request initially sees its own character as the item owner.
    let bag = other.data().inventory.bag().clone();
    bag.lock().unwrap().items[0].id = item_id;
    let (calls, release) = install_barrier_loader(&server);
    let mut first = Box::pin(summon(&server, &owner, item_id, SECOND_SHIP_ITEM));
    let mut conflicting = Box::pin(summon(&server, &other, item_id, SECOND_SHIP_ITEM));
    assert!(poll!(first.as_mut()).is_pending());
    assert!(poll!(conflicting.as_mut()).is_pending());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.add_permits(1);
    first.await.unwrap();
    assert!(conflicting.await.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(roots(&server, 1).len(), 1);
    assert!(roots(&server, 2).is_empty());
}

#[tokio::test]
async fn switching_distinct_items_preserves_each_identity_and_other_owners() {
    let server = server_with(identity_data());
    install_memory_loader(&server);
    let a = TestClient::new(&server, 1, 1);
    let b = TestClient::new(&server, 2, 2);
    let (owner, first_item) = owner_with_ship(&server, &a, 1, PA, MERCHANT_ITEM);
    let (other, other_item) = owner_with_ship(&server, &b, 2, PB, SECOND_SHIP_ITEM);
    second_item(&server, &owner, 0x8000, SECOND_SHIP_ITEM);
    let first = summon(&server, &owner, first_item, MERCHANT_ITEM)
        .await
        .unwrap();
    let unaffected = summon(&server, &other, other_item, SECOND_SHIP_ITEM)
        .await
        .unwrap();
    assert!(summon(&server, &owner, other_item, SECOND_SHIP_ITEM)
        .await
        .is_none());
    let second = summon(&server, &owner, 0x8000, SECOND_SHIP_ITEM)
        .await
        .unwrap();
    assert_ne!(persistent_id(&first), persistent_id(&second));
    assert_eq!(roots(&server, 1).len(), 1);
    assert_eq!(roots(&server, 2)[0].obj_id, unaffected.obj_id);
    let again = summon(&server, &owner, first_item, MERCHANT_ITEM)
        .await
        .unwrap();
    assert_eq!(persistent_id(&again), persistent_id(&first));
    assert_ne!(again.obj_id, first.obj_id);
    assert_eq!(roots(&server, 1).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn retained_reconnect_cancels_the_hull_timer_and_cannot_duplicate_loaded_merchant() {
    let server = server_with(identity_data());
    let client = TestClient::new(&server, 1, 1);
    let (owner, item_id) = owner_with_ship(&server, &client, 1, PA, MERCHANT_ITEM);
    let hull = root(&server, &owner, item_id, MERCHANT);
    loaded_rack(&server, &hull);
    super::super::on_disconnect(&server, &owner);
    assert_ne!(hull.data().leave_task, 0);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(599)).await;
    assert_eq!(roots(&server, 1)[0].obj_id, hull.obj_id);
    server.world.world(0).unwrap().delete(&owner);
    server.world.try_remove_character(owner.obj_id);
    let reconnect = TestClient::new(&server, 2, 1);
    let mut character = owner.data().clone();
    character.unit.transform.parent_obj_id = 0;
    let new_owner = spawn_player(&server, &reconnect, character, PA);
    super::super::on_character_selected(&server, &new_owner);
    assert_eq!(hull.data().leave_task, 0);
    assert!(summon(&server, &new_owner, item_id, MERCHANT_ITEM)
        .await
        .is_none());
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert_eq!(roots(&server, 1)[0].obj_id, hull.obj_id);
    // A late disconnect callback from the old connection cannot schedule the new session's hull.
    super::super::on_disconnect(&server, &owner);
    assert_eq!(hull.data().leave_task, 0);
}

#[tokio::test]
async fn a_root_in_another_world_still_blocks_a_loaded_replacement() {
    let server = server_with(identity_data());
    let client = TestClient::new(&server, 1, 1);
    let (owner, item_id) = owner_with_ship(&server, &client, 1, PA, MERCHANT_ITEM);
    let hull = root(&server, &owner, item_id, MERCHANT);
    let rack = loaded_rack(&server, &hull);
    let old_world = server.world.world(0).unwrap();
    let template = Arc::new(WorldTemplate::synthetic(1, "other_world", 2, 2, TEST_ZONE));
    let other_world = server.world.create_world_instance(template, 1);
    old_world.delete(&owner);
    owner.data().unit.transform.instance_id = 1;
    other_world.spawn(&owner, &server.view_ctx());
    assert!(summon(&server, &owner, item_id, MERCHANT_ITEM)
        .await
        .is_none());
    assert!(other_world
        .all_of_kind(crate::world::ObjectKind::Slave)
        .is_empty());
    assert!(old_world.slave(hull.obj_id).is_some());
    {
        let mut rack = rack.data();
        rack.item_id = 0;
        rack.item_template_id = 0;
    }
    install_memory_loader(&server);
    let replacement = summon(&server, &owner, item_id, MERCHANT_ITEM)
        .await
        .unwrap();
    assert_eq!(persistent_id(&replacement), persistent_id(&hull));
    assert!(old_world.slave(hull.obj_id).is_none());
    assert!(other_world.slave(replacement.obj_id).is_some());
    assert_eq!(roots(&server, 1).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn offline_hull_remains_for_600_seconds_then_uses_delayed_portal_removal() {
    let server = server_with(identity_data());
    let client = TestClient::new(&server, 1, 1);
    let (owner, item_id) = owner_with_ship(&server, &client, 1, PA, MERCHANT_ITEM);
    let hull = root(&server, &owner, item_id, MERCHANT);
    super::super::on_disconnect(&server, &owner);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(599)).await;
    assert_eq!(roots(&server, 1)[0].obj_id, hull.obj_id);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(roots(&server, 1).is_empty());
    assert!(
        hull.core().is_visible(),
        "portal rendering survives authoritative removal"
    );
    let spawns = server.systems.npcs.spawns.get(0).unwrap();
    crate::systems::npcs::spawn_manager::advance_test_clock(Duration::from_secs(3));
    spawns.check_respawns_tick(&server);
    assert!(!hull.core().is_visible());
}

#[tokio::test(start_paused = true)]
async fn old_disconnect_timer_cannot_delete_another_hull_reusing_the_object_id() {
    let server = server_with(identity_data());
    let client = TestClient::new(&server, 1, 1);
    let (owner, item_id) = owner_with_ship(&server, &client, 1, PA, MERCHANT_ITEM);
    let old_hull = root(&server, &owner, item_id, MERCHANT);
    super::super::on_disconnect(&server, &owner);
    tokio::task::yield_now().await;
    let world = server.world.world(0).unwrap();
    world.delete(&old_hull);
    // Model a later allocation after the old object's portal/death timer released its id.
    let mut data = crate::models::slave::Slave::new(900, MERCHANT);
    data.summoner_id = 1;
    data.summoner_obj_id = PA;
    data.owner_obj_id = PA;
    data.summoning_item_id = 0xbeef;
    data.unit.hp = 1;
    data.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let replacement = Entity::new(old_hull.obj_id, data);
    world.spawn(&replacement, &server.view_ctx());
    tokio::time::advance(Duration::from_secs(600)).await;
    tokio::task::yield_now().await;
    assert!(Arc::ptr_eq(
        &world.slave(replacement.obj_id).unwrap(),
        &replacement
    ));
}

#[tokio::test]
async fn failed_lookup_releases_reservations_without_changing_the_prior_hull() {
    let server = server_with(identity_data());
    let client = TestClient::new(&server, 1, 1);
    let (owner, item_id) = owner_with_ship(&server, &client, 1, PA, SECOND_SHIP_ITEM);
    let hull = root(&server, &owner, item_id, SHIP_HULL);
    *server
        .systems
        .vehicles
        .slave_summons
        .test_loader
        .lock()
        .unwrap() = Some(slaves::TestSlaveLoader(Arc::new(|_, _| {
        Box::pin(async { Err(sqlx::Error::Protocol("deliberate failed SELECT".into())) })
    })));
    assert!(summon(&server, &owner, item_id, SECOND_SHIP_ITEM)
        .await
        .is_none());
    assert_eq!(roots(&server, 1)[0].obj_id, hull.obj_id);
    install_memory_loader(&server);
    let retry = summon(&server, &owner, item_id, SECOND_SHIP_ITEM)
        .await
        .unwrap();
    assert_eq!(persistent_id(&retry), persistent_id(&hull));
    assert_eq!(roots(&server, 1).len(), 1);
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; connection-local temporary tables only"]
async fn private_mysql_ship_save_fence_query_failure_retry_and_restart() {
    use sqlx::mysql::MySqlConnectOptions;
    use std::str::FromStr;

    let options =
        MySqlConnectOptions::from_str(&std::env::var("AAEMU_TEST_MYSQL").unwrap()).unwrap();
    assert_eq!(options.get_port(), 3317);
    assert!(matches!(options.get_host(), "127.0.0.1" | "localhost"));
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let mut slave_ddl = String::new();
    for table in ["slaves", "doodads"] {
        let (_, ddl): (String, String) = sqlx::query_as(&format!("SHOW CREATE TABLE `{table}`"))
            .fetch_one(&pool)
            .await
            .unwrap();
        let ddl = ddl.replacen("CREATE TABLE", "CREATE TEMPORARY TABLE", 1);
        if table == "slaves" {
            slave_ddl = ddl;
            sqlx::query("CREATE TEMPORARY TABLE slaves (id INT UNSIGNED PRIMARY KEY)")
                .execute(&pool)
                .await
                .unwrap();
        } else {
            sqlx::query(&ddl).execute(&pool).await.unwrap();
        }
    }
    let make_server = || {
        let mut server = test_server_with(identity_data());
        server.db = pool.clone();
        let server = Arc::new(server);
        server.set_self();
        server
    };
    let server = make_server();
    let client = TestClient::new(&server, 1, 1);
    let (owner, item_id) = owner_with_ship(&server, &client, 1, PA, SECOND_SHIP_ITEM);
    let before = mates::owned_item(&server, &owner, item_id).unwrap();
    // A genuine SQL error must not become "no row" and allocate/publish a new identity.
    assert!(summon(&server, &owner, item_id, SECOND_SHIP_ITEM)
        .await
        .is_none());
    assert_eq!(mates::owned_item(&server, &owner, item_id).unwrap(), before);
    assert!(roots(&server, 1).is_empty());
    // No writes/tasks exist yet, so restoring this temporary fixture cannot expose a real table
    // to an in-flight writer during the DROP/CREATE interval.
    sqlx::query("DROP TEMPORARY TABLE slaves")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&slave_ddl).execute(&pool).await.unwrap();
    let first = summon(&server, &owner, item_id, SECOND_SHIP_ITEM)
        .await
        .unwrap();
    let db_id = first.data().id;
    let second = summon(&server, &owner, item_id, SECOND_SHIP_ITEM)
        .await
        .unwrap();
    assert_eq!(
        second.data().id,
        db_id,
        "SELECT must observe queued save before replacement"
    );
    assert_ne!(second.obj_id, first.obj_id);
    assert_eq!(roots(&server, 1).len(), 1);
    slaves::delete(&server, Some(&owner), 0, second.obj_id, false);
    let third = summon(&server, &owner, item_id, SECOND_SHIP_ITEM)
        .await
        .unwrap();
    assert_eq!(
        third.data().id,
        db_id,
        "manual despawn's queued save is fenced too"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM slaves WHERE item_id = ?")
        .bind(item_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    {
        let _transaction = server.save.transaction_lock().await;
        let mut tx = pool.begin().await.unwrap();
        slaves::save_all(&server, &mut tx).await.unwrap();
        tx.commit().await.unwrap();
    }
    // Keep unknown legacy duplicates intact. The actual item's saved id wins the lookup,
    // while the fresh server still admits only one live root for that item.
    let mut legacy_duplicate = slaves::snapshot(&server, &third)[0].clone();
    legacy_duplicate.id = db_id + 10_000;
    slaves::write_row(&mut pool.acquire().await.unwrap(), &legacy_duplicate)
        .await
        .unwrap();
    let restart = make_server();
    let restarted_client = TestClient::new(&restart, 2, 1);
    let mut character = owner.data().clone();
    character.unit.transform.parent_obj_id = 0;
    let restarted_owner = spawn_player(&restart, &restarted_client, character, PA);
    let bag = restarted_owner.data().inventory.bag().clone();
    restart.items.add_container(bag);
    let restored = summon(&restart, &restarted_owner, item_id, SECOND_SHIP_ITEM)
        .await
        .unwrap();
    assert_eq!(restored.data().id, db_id);
    assert_eq!(roots(&restart, 1).len(), 1);
    pool.close().await;
}
