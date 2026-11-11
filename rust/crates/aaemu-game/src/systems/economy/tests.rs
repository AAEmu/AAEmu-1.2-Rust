//! Flow tests of the core item layer (no MySQL): moves / splits / merges / equips through the
//! real packet handlers, destroy, money, acquire with timers, bag expansion.

use std::sync::Arc;

use aaemu_commons::{Marshal, PacketStream};

use crate::data::items::{bind_type, EquipTemplate, ItemTemplate, TemplateKind};
use crate::data::GameData;
use crate::models::char_data::Actability;
use crate::models::container::{equip_slot, slot_type, ContainerKind, ItemContainer};
use crate::models::item::{Item, ItemClass, ITEM_FLAG_SOUL_BOUND};
use crate::models::item_task::{task_type, ItemFx, ItemFxList, ItemTask};
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::economy::data::Expand;
use crate::systems::economy::error_message;
use crate::systems::economy::inventory::{self, Inv, ItemTxn};
use crate::systems::economy::item_handlers::*;
use crate::systems::economy::item_packets::SCItemTaskSuccessPacket;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::PlayerRef;

pub const STACK: u32 = 1000; // max 20
pub const ARMOR: u32 = 2000; // chest, bind on equip
pub const TIMED: u32 = 3000; // 60 minutes absolute lifetime
pub const SINGLE: u32 = 4000; // max 1
pub const ACTABILITY_ROD: u32 = 2730; // mainhand, actability group 7 needs 10000 points

#[tokio::test]
async fn unpack_rejects_another_characters_item() {
    let w = world();
    let id = give(&w, &w.pb, slot_type::INVENTORY, SINGLE, 1, 3);
    w.server.items.add_container(Inv::of(&w.pb).bag());
    let before = item_at(&w.pb, slot_type::INVENTORY, 3).unwrap();
    assert!(!unwrap_item(&w.server, &w.pa, slot_type::INVENTORY, 3, id));
    assert_eq!(item_at(&w.pb, slot_type::INVENTORY, 3).unwrap(), before);
    assert!(unwrap_item(&w.server, &w.pb, slot_type::INVENTORY, 3, id));
    let after = item_at(&w.pb, slot_type::INVENTORY, 3).unwrap();
    assert_ne!(after.flags, before.flags);
}

pub fn test_data() -> GameData {
    let mut d = GameData::empty();
    d.items.grades.insert(
        0,
        crate::data::items::GradeTemplate {
            grade: 0,
            grade_order: 0,
            durability: 1.0,
        },
    );
    d.items.config.durability_const = 1.0;
    let mut t = ItemTemplate::plain(STACK, 20);
    t.price = 10;
    t.refund = 5;
    t.sellable = true;
    d.items.templates.insert(STACK, t);
    d.items.templates.insert(
        ARMOR,
        ItemTemplate {
            bind_type: bind_type::BIND_ON_EQUIP,
            ..ItemTemplate {
                kind: TemplateKind::Armor(EquipTemplate {
                    slot_type_id: 3,
                    coverage: 10,
                    durability_ratio: 1.0,
                    durability_multiplier: 100,
                    ..EquipTemplate::default()
                }),
                ..ItemTemplate::plain(ARMOR, 1)
            }
        },
    );
    d.items.templates.insert(
        TIMED,
        ItemTemplate {
            exp_abs_lifetime: 60,
            ..ItemTemplate::plain(TIMED, 5)
        },
    );
    d.items
        .templates
        .insert(SINGLE, ItemTemplate::plain(SINGLE, 1));
    let mut rod = ItemTemplate::new(
        ACTABILITY_ROD,
        TemplateKind::Weapon(EquipTemplate {
            slot_type_id: 14,
            durability_ratio: 1.0,
            durability_multiplier: 100,
            ..EquipTemplate::default()
        }),
    );
    rod.max_count = 1;
    rod.actability_group_id = 7;
    rod.actability_requirement = 10000;
    d.items.templates.insert(ACTABILITY_ROD, rod);
    d.systems.economy.expands.insert(
        0,
        vec![
            Expand {
                is_bank: false,
                step: 0,
                price: 100,
                item_id: 0,
                item_count: 0,
                currency_id: 0,
            },
            Expand {
                is_bank: true,
                step: 0,
                price: 0,
                item_id: STACK,
                item_count: 2,
                currency_id: 0,
            },
        ],
    );
    d
}

#[allow(dead_code)]
pub struct World {
    pub server: Arc<GameServer>,
    pub a: TestClient,
    pub pa: PlayerRef,
    pub b: TestClient,
    pub pb: PlayerRef,
}

/// Two players next to each other (char ids 1 / 2).
pub fn world() -> World {
    world_with(test_data())
}

/// [`world`] on a caller-supplied `GameData` (e.g. with faction relations).
pub fn world_with(data: GameData) -> World {
    let server = Arc::new(test_server_with(data));
    server.set_self();
    let mut a = TestClient::new(&server, 1, 1001);
    let mut b = TestClient::new(&server, 2, 1002);
    // `UnitBase` defaults to `hp = 0`, which is also what a dead unit has (`IsDead`, used by the
    // trade state check): the fixtures' players are alive.
    let mut alice = test_character(1, "Alice", 100.0, 100.0);
    alice.unit.hp = 100;
    alice.unit.max_hp = 100;
    let mut bob = test_character(2, "Bob", 101.0, 100.0);
    bob.unit.hp = 100;
    bob.unit.max_hp = 100;
    let pa = spawn_player(&server, &a, alice, 0x100);
    let pb = spawn_player(&server, &b, bob, 0x101);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    a.drain();
    b.drain();
    World {
        server,
        a,
        pa,
        b,
        pb,
    }
}

/// A world where a finished house at the player's position carries a +1000 act-ability
/// decoration for fishing (group 7). The client adds this bonus in `FUN_394e1a90`.
fn world_with_house_bonus() -> World {
    use crate::models::doodad::{doodad_owner_type, Doodad};
    use crate::models::unit::attach_point;
    use crate::systems::housing::data::{HousingDecoration, HousingTemplate};
    use crate::systems::housing::manager;
    use crate::systems::npcs::doodad::templates::DoodadTemplate;
    use crate::world::Entity;
    let mut data = test_data();
    data.systems.housing.add_template(HousingTemplate {
        id: 300,
        garden_radius: 8.0,
        main_model_id: 700,
        ..Default::default()
    });
    data.systems.npcs.doodad.templates.insert(
        4001,
        DoodadTemplate {
            id: 4001,
            ..Default::default()
        },
    );
    data.systems
        .housing
        .add_decoration(HousingDecoration {
            id: 40,
            doodad_id: 4001,
            actability_group_id: 7,
            actability_up: 1000,
            ..Default::default()
        })
        .unwrap();
    let w = world_with(data);
    let world = w.server.world.world(0).expect("world 0");
    let house = manager::create(&w.server, 300, 0, &world, 0, 0).expect("house");
    {
        let mut h = house.data();
        h.id = 1;
        h.account_id = 1001;
        h.unit.owner_id = 1;
        h.unit.transform.set_xyz(100.0, 100.0, 100.0);
    }
    assert!(w.server.systems.housing.manager.insert(&house));
    manager::spawn_house(&w.server, &house);
    let mut deco = Doodad::new(4001);
    deco.owner_type = doodad_owner_type::HOUSING;
    deco.owner_db_id = 1;
    deco.attach_point = attach_point::NONE;
    deco.transform.instance_id = 0;
    deco.transform.set_xyz(100.0, 100.0, 100.0);
    let deco = Entity::new(0x400, deco);
    world.spawn(&deco, &w.server.view_ctx());
    w
}

/// NPC templates of the bank / auction service tests.
pub const BANKER_TEMPLATE: u32 = 7001;
pub const AUCTIONEER_TEMPLATE: u32 = 7002;
pub const MERCHANT_TEMPLATE: u32 = 7003;

struct ServiceNpcs;
impl crate::systems::economy::hooks::NpcEconomyHooks for ServiceNpcs {
    fn shop_info(
        &self,
        _server: &GameServer,
        npc: &crate::world::NpcRef,
    ) -> Option<crate::systems::economy::hooks::NpcShopInfo> {
        let t = npc.data().unit.template_id;
        Some(crate::systems::economy::hooks::NpcShopInfo {
            banker: t == BANKER_TEMPLATE,
            auctioneer: t == AUCTIONEER_TEMPLATE,
            merchant: t == MERCHANT_TEMPLATE,
            ..Default::default()
        })
    }
}

/// Installs the service-NPC hooks and spawns an NPC of `template` at `x` (players stand at 100).
pub fn spawn_service_npc(w: &mut World, obj_id: u32, template: u32, x: f32) {
    w.server
        .systems
        .economy
        .hooks
        .npcs
        .install(Arc::new(ServiceNpcs));
    let mut n = crate::models::npc::Npc::new(template);
    n.unit.transform.set_xyz(x, 100.0, 100.0);
    let n = crate::world::Entity::new(obj_id, n);
    w.server
        .world
        .world(0)
        .unwrap()
        .spawn(&n, &w.server.view_ctx());
    w.a.drain();
    w.b.drain();
}

/// Puts a new item of `template` x `count` into `st` at `slot` (no packets).
pub fn give(w: &World, p: &PlayerRef, st: u8, template: u32, count: i32, slot: i32) -> u64 {
    let inv = Inv::of(p);
    let item = w
        .server
        .items
        .create(
            template,
            count,
            0,
            1,
            &w.server.game_data.items,
            &w.server.ids.item,
            0,
        )
        .expect("item");
    let id = item.id;
    let c = inv.get(st).unwrap();
    let mut c = c.lock().unwrap();
    assert!(c
        .add_new_item(item, slot, &w.server.game_data.items)
        .is_ok());
    id
}

/// Records the quest item events (`owner character id, template, signed count`).
#[derive(Default)]
pub struct ItemEventLog(pub std::sync::Mutex<Vec<(u32, u32, i32)>>);

impl crate::systems::economy::hooks::QuestItemHooks for ItemEventLog {
    fn on_items_acquired(&self, _: &GameServer, p: &PlayerRef, template_id: u32, count: i32) {
        self.0
            .lock()
            .unwrap()
            .push((p.character_id, template_id, count));
    }

    fn on_items_consumed(&self, _: &GameServer, p: &PlayerRef, template_id: u32, count: i32) {
        self.0
            .lock()
            .unwrap()
            .push((p.character_id, template_id, -count));
    }
}

/// Installs an [`ItemEventLog`] as the economy's quest item hooks.
pub fn log_item_events(w: &World) -> Arc<ItemEventLog> {
    let log = Arc::new(ItemEventLog::default());
    w.server.systems.economy.hooks.quests.install(log.clone());
    log
}

pub fn item_at(p: &PlayerRef, st: u8, slot: u8) -> Option<Item> {
    Inv::of(p).item_at(st, slot)
}

/// Decodes the body of an `SCItemTaskSuccessPacket` written by [`SCItemTaskSuccessPacket`].
pub fn task_body(action: u8, tasks: Vec<ItemTask>) -> Vec<u8> {
    let mut s = PacketStream::new();
    SCItemTaskSuccessPacket {
        action,
        tasks,
        force_remove: Vec::new(),
    }
    .write(&mut s);
    s.into_bytes()
}

pub fn run<P: ClientPacket>(w: &mut World, which: u8, body: &[u8]) {
    let mut s = PacketStream::from_bytes(body.to_vec());
    let p = P::read(&mut s).expect("read");
    let conn = if which == 0 {
        &mut w.a.conn
    } else {
        &mut w.b.conn
    };
    futures::executor::block_on(p.handle(&w.server, conn)).expect("handle");
}

fn move_body(from: u64, to: u64, fst: u8, fs: u8, tst: u8, ts: u8) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_u64(from)
        .write_u64(to)
        .write_u8(fst)
        .write_u8(fs)
        .write_u8(tst)
        .write_u8(ts);
    s
}

fn error_body(error: i16, kind: u32) -> Vec<u8> {
    let mut s = PacketStream::new();
    crate::packets::world::SCErrorMsgPacket::new(error, kind, true).write(&mut s);
    s.into_bytes()
}

/// The 1.2 client refuses the use/equip below `items.actability_requirement` of the item's
/// `actability_group_id` (`FUN_394e1a90` through the equip validation `FUN_394e41f0`) with
/// `actability_not_enough_point` 508 and keeps the item. The client's own check is authoritative;
/// the server repeats it for a forged request.
#[tokio::test]
async fn equipping_an_item_requires_its_actability_points() {
    let mut w = world();
    let rod = give(
        &w,
        &w.pa.clone(),
        slot_type::INVENTORY,
        ACTABILITY_ROD,
        1,
        0,
    );
    w.a.drain();
    let body = move_body(
        rod,
        0,
        slot_type::INVENTORY,
        0,
        slot_type::EQUIPMENT,
        equip_slot::MAINHAND as u8,
    );

    // No actability rows: 0 < 10000, the rod stays in the bag.
    run::<CSSwapItemsPacket>(&mut w, 0, body.as_bytes());
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCErrorMsgPacket,
            error_body(error_message::ACTABILITY_NOT_ENOUGH_POINT, 7),
        )]
    );
    assert!(item_at(&w.pa, slot_type::INVENTORY, 0).is_some());
    assert!(item_at(&w.pa, slot_type::EQUIPMENT, equip_slot::MAINHAND as u8).is_none());

    // 9999 is still one point short.
    w.pa.data()
        .actability
        .actabilities
        .push(crate::models::char_data::Actability {
            id: 7,
            point: 9999,
            step: 1,
        });
    run::<CSSwapItemsPacket>(&mut w, 0, body.as_bytes());
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCErrorMsgPacket,
            error_body(error_message::ACTABILITY_NOT_ENOUGH_POINT, 7),
        )]
    );
    assert!(item_at(&w.pa, slot_type::INVENTORY, 0).is_some());
    assert!(item_at(&w.pa, slot_type::EQUIPMENT, equip_slot::MAINHAND as u8).is_none());

    // Exactly the requirement: the equip goes through.
    w.pa.data().actability.actabilities[0].point = 10000;
    run::<CSSwapItemsPacket>(&mut w, 0, body.as_bytes());
    assert!(item_at(&w.pa, slot_type::INVENTORY, 0).is_none());
    assert_eq!(
        item_at(&w.pa, slot_type::EQUIPMENT, equip_slot::MAINHAND as u8)
            .expect("rod equipped")
            .template_id,
        ACTABILITY_ROD
    );
    w.a.drain();
}

/// The client's `DoEquipInEmptySlot` form: it sends the empty equipment slot as the source and
/// the bag item as the target. The target item moves into the equipment slot, so it is gated.
#[tokio::test]
async fn equipping_from_the_empty_equipment_slot_checks_the_target_item() {
    let mut w = world();
    let rod = give(
        &w,
        &w.pa.clone(),
        slot_type::INVENTORY,
        ACTABILITY_ROD,
        1,
        0,
    );
    w.a.drain();
    let body = move_body(
        0,
        rod,
        slot_type::EQUIPMENT,
        equip_slot::MAINHAND as u8,
        slot_type::INVENTORY,
        0,
    );
    run::<CSSwapItemsPacket>(&mut w, 0, body.as_bytes());
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCErrorMsgPacket,
            error_body(error_message::ACTABILITY_NOT_ENOUGH_POINT, 7),
        )]
    );
    assert!(item_at(&w.pa, slot_type::INVENTORY, 0).is_some());
    assert!(item_at(&w.pa, slot_type::EQUIPMENT, equip_slot::MAINHAND as u8).is_none());
}

/// The swap bypass: an equipped item (id != 0) dragged onto an occupied bag / bank slot moves
/// the target item into the vacated equipment slot. The target item must pass the same gate.
#[tokio::test]
async fn swapping_an_equipped_item_onto_an_occupied_slot_checks_the_target_item() {
    let mut w = world();
    // Two rods (max_count 1) so the swap is a `DoSwap`, not a merge; the equipped copy is
    // placed directly and only the target copy that would enter the equipment slot is gated.
    let equipped = give(
        &w,
        &w.pa.clone(),
        slot_type::EQUIPMENT,
        ACTABILITY_ROD,
        1,
        equip_slot::MAINHAND,
    );
    let rod = give(
        &w,
        &w.pa.clone(),
        slot_type::INVENTORY,
        ACTABILITY_ROD,
        1,
        0,
    );
    w.a.drain();
    let body = move_body(
        equipped,
        rod,
        slot_type::EQUIPMENT,
        equip_slot::MAINHAND as u8,
        slot_type::INVENTORY,
        0,
    );
    run::<CSSwapItemsPacket>(&mut w, 0, body.as_bytes());
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCErrorMsgPacket,
            error_body(error_message::ACTABILITY_NOT_ENOUGH_POINT, 7),
        )]
    );
    assert_eq!(
        item_at(&w.pa, slot_type::EQUIPMENT, equip_slot::MAINHAND as u8)
            .expect("the old item stays equipped")
            .id,
        equipped
    );
    assert_eq!(
        item_at(&w.pa, slot_type::INVENTORY, 0)
            .expect("the rod stays in the bag")
            .id,
        rod
    );
}

/// The core move (without the packet pre-check) refuses the same swap direction, so an internal
/// caller cannot equip the target item around the gate.
#[tokio::test]
async fn core_move_refuses_the_equipment_swap_bypass() {
    let w = world();
    let equipped = give(
        &w,
        &w.pa.clone(),
        slot_type::EQUIPMENT,
        ACTABILITY_ROD,
        1,
        equip_slot::MAINHAND,
    );
    let rod = give(
        &w,
        &w.pa.clone(),
        slot_type::INVENTORY,
        ACTABILITY_ROD,
        1,
        0,
    );
    let inv = Inv::of(&w.pa);
    {
        let mut txn = ItemTxn::begin(&w.server);
        assert!(!inventory::split_or_move_item_ex(
            &mut txn,
            &inv,
            task_type::SWAP_ITEMS,
            Some(inv.equipment()),
            Some(inv.bag()),
            equipped,
            slot_type::EQUIPMENT,
            equip_slot::MAINHAND as u8,
            rod,
            slot_type::INVENTORY,
            0,
            0,
        ));
    }
    assert_eq!(
        item_at(&w.pa, slot_type::EQUIPMENT, equip_slot::MAINHAND as u8)
            .expect("the equipped item stays")
            .id,
        equipped
    );
    assert_eq!(
        item_at(&w.pa, slot_type::INVENTORY, 0)
            .expect("the rod stays in the bag")
            .id,
        rod
    );
}

/// Unequipping into an empty slot is not an equip: it is allowed even below the requirement.
#[tokio::test]
async fn unequipping_a_rod_into_an_empty_slot_is_allowed() {
    let mut w = world();
    let rod = give(
        &w,
        &w.pa.clone(),
        slot_type::EQUIPMENT,
        ACTABILITY_ROD,
        1,
        equip_slot::MAINHAND,
    );
    w.a.drain();
    let body = move_body(
        rod,
        0,
        slot_type::EQUIPMENT,
        equip_slot::MAINHAND as u8,
        slot_type::INVENTORY,
        1,
    );
    run::<CSSwapItemsPacket>(&mut w, 0, body.as_bytes());
    assert!(w
        .a
        .drain()
        .iter()
        .all(|(op, _)| *op != sc::SCErrorMsgPacket));
    assert!(item_at(&w.pa, slot_type::EQUIPMENT, equip_slot::MAINHAND as u8).is_none());
    assert_eq!(
        item_at(&w.pa, slot_type::INVENTORY, 1)
            .expect("the rod is back in the bag")
            .id,
        rod
    );
}

/// A house decoration adds its act-ability bonus (`FUN_390b3a80`) to the check: 8999 points
/// plus a 1000-point decoration is still short, 9000 + 1000 meets the rod's 10000.
#[tokio::test]
async fn house_actability_bonus_counts_towards_the_requirement() {
    let mut w = world_with_house_bonus();
    let rod = give(
        &w,
        &w.pa.clone(),
        slot_type::INVENTORY,
        ACTABILITY_ROD,
        1,
        0,
    );
    w.pa.data().actability.actabilities.push(Actability {
        id: 7,
        point: 8999,
        step: 1,
    });
    w.a.drain();
    let body = move_body(
        rod,
        0,
        slot_type::INVENTORY,
        0,
        slot_type::EQUIPMENT,
        equip_slot::MAINHAND as u8,
    );
    run::<CSSwapItemsPacket>(&mut w, 0, body.as_bytes());
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCErrorMsgPacket,
            error_body(error_message::ACTABILITY_NOT_ENOUGH_POINT, 7),
        )]
    );
    assert!(item_at(&w.pa, slot_type::EQUIPMENT, equip_slot::MAINHAND as u8).is_none());

    w.pa.data().actability.actabilities[0].point = 9000;
    run::<CSSwapItemsPacket>(&mut w, 0, body.as_bytes());
    assert!(
        item_at(&w.pa, slot_type::EQUIPMENT, equip_slot::MAINHAND as u8).is_some(),
        "the house bonus covers the 1000-point shortfall"
    );
    w.a.drain();
}

/// A coffer (TRADE) item is not in `Inv`; the handler must resolve it through the server item
/// manager so the 508 is sent instead of the core silently refusing the move.
#[tokio::test]
async fn coffer_item_to_equipment_is_refused_below_actability() {
    use crate::models::doodad::{Doodad, DoodadCofferState};
    use crate::models::house::housing_permission;
    use crate::world::Entity;
    let mut w = world();
    w.server.names.add_character(1, "Alice", 1001);
    let container = w
        .server
        .items
        .new_coffer_container(1, &w.server.ids.container);
    let container_id = container.lock().unwrap().container_id;
    let item = Item::create(
        901,
        w.server.game_data.items.template(ACTABILITY_ROD).unwrap(),
        1,
        0,
        1,
        &w.server.game_data.items,
        0,
    );
    container
        .lock()
        .unwrap()
        .add_new_item(item, 0, &w.server.game_data.items)
        .unwrap();
    let mut d = Doodad::new(100);
    d.owner_id = 1;
    d.data = i32::from(housing_permission::PRIVATE);
    d.transform.set_xyz(100.0, 100.0, 100.0);
    d.coffer = Some(DoodadCofferState {
        container_id,
        capacity: 10,
        opened_by: Some(1),
    });
    let doodad = Entity::new(0x300, d);
    w.server
        .world
        .world(0)
        .unwrap()
        .spawn(&doodad, &w.server.view_ctx());
    w.a.drain();
    let mut body = move_body(
        901,
        0,
        slot_type::TRADE,
        0,
        slot_type::EQUIPMENT,
        equip_slot::MAINHAND as u8,
    );
    body.write_u64(container_id);
    run::<CSSwapCofferItemsPacket>(&mut w, 0, body.as_bytes());
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCErrorMsgPacket,
            error_body(error_message::ACTABILITY_NOT_ENOUGH_POINT, 7),
        )]
    );
    assert!(container.lock().unwrap().get_item_by_id(901).is_some());
    assert!(item_at(&w.pa, slot_type::EQUIPMENT, equip_slot::MAINHAND as u8).is_none());
}

/// The equip gate against the real 1.2 row (27309, group 7, requirement 10000).
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_fishing_rod_row_requires_its_actability_points() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let real = crate::data::items::ItemData::load(&conn).unwrap();
    let rod = real.template(27309).expect("iron fishing rod").clone();
    assert_eq!(
        (rod.actability_group_id, rod.actability_requirement),
        (7, 10000)
    );
    let mut data = test_data();
    data.items.templates.insert(27309, rod);
    let mut w = world_with(data);
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, 27309, 1, 0);
    w.a.drain();
    let body = move_body(
        id,
        0,
        slot_type::INVENTORY,
        0,
        slot_type::EQUIPMENT,
        equip_slot::MAINHAND as u8,
    );
    run::<CSSwapItemsPacket>(&mut w, 0, body.as_bytes());
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCErrorMsgPacket,
            error_body(error_message::ACTABILITY_NOT_ENOUGH_POINT, 7),
        )]
    );
    assert!(item_at(&w.pa, slot_type::INVENTORY, 0).is_some());
    w.pa.data().actability.actabilities.push(Actability {
        id: 7,
        point: 10000,
        step: 1,
    });
    run::<CSSwapItemsPacket>(&mut w, 0, body.as_bytes());
    assert_eq!(
        item_at(&w.pa, slot_type::EQUIPMENT, equip_slot::MAINHAND as u8)
            .expect("equipped at the real requirement")
            .template_id,
        27309
    );
    w.a.drain();
}

#[tokio::test]
async fn expanding_expert_without_a_limit_row_reports_store_unknown_error() {
    let mut w = world();
    w.a.drain();
    run::<crate::systems::economy::store::CSExpandExpertPacket>(&mut w, 0, &[]);
    assert_eq!(
        w.a.drain(),
        vec![(
            sc::SCErrorMsgPacket,
            error_body(error_message::STORE_UNKNOWN_ERROR, 0),
        )]
    );
}

#[tokio::test]
async fn ordinary_item_moves_reject_foreign_global_items() {
    let mut w = world();
    let id = give(&w, &w.pb, slot_type::INVENTORY, SINGLE, 1, 3);
    w.server.items.add_container(Inv::of(&w.pb).bag());
    let before = item_at(&w.pb, slot_type::INVENTORY, 3).unwrap();
    let body = move_body(id, 0, slot_type::INVENTORY, 3, slot_type::INVENTORY, 0);
    run::<CSSwapItemsPacket>(&mut w, 0, body.as_bytes());
    assert_eq!(item_at(&w.pb, slot_type::INVENTORY, 3).unwrap(), before);
    assert!(item_at(&w.pa, slot_type::INVENTORY, 0).is_none());
    assert_eq!(w.a.drain_ops(), vec![sc::SCErrorMsgPacket]);
    assert!(w.b.drain().is_empty());
}

#[tokio::test]
async fn moves_naming_a_consumed_or_unknown_item_are_rejected() {
    let mut w = world();
    let kept = give(&w, &w.pa.clone(), slot_type::INVENTORY, STACK, 5, 1);
    let consumed = give(&w, &w.pa.clone(), slot_type::INVENTORY, STACK, 5, 2);
    let removed = Inv::of(&w.pa).bag().lock().unwrap().remove_item(consumed);
    assert!(removed.is_some());
    let before = (
        item_at(&w.pa, slot_type::INVENTORY, 1).unwrap(),
        item_at(&w.pa, slot_type::INVENTORY, 2),
    );
    w.a.drain();

    // Source id no longer held, source id never existed, and target id no longer held.
    let requests = [
        move_body(
            consumed,
            0,
            slot_type::INVENTORY,
            2,
            slot_type::INVENTORY,
            4,
        ),
        move_body(
            16_777_663,
            0,
            slot_type::INVENTORY,
            2,
            slot_type::INVENTORY,
            4,
        ),
        move_body(
            kept,
            consumed,
            slot_type::INVENTORY,
            1,
            slot_type::INVENTORY,
            2,
        ),
    ];
    for body in requests {
        run::<CSSwapItemsPacket>(&mut w, 0, body.as_bytes());
        assert_eq!(w.a.drain_ops(), vec![sc::SCErrorMsgPacket]);
        assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 1).unwrap(), before.0);
        assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 2), before.1);
        assert!(item_at(&w.pa, slot_type::INVENTORY, 4).is_none());
    }
    assert!(w.b.drain().is_empty());
}

#[tokio::test]
async fn coffer_moves_require_opening_permission_and_nearby_doodad() {
    use crate::models::doodad::{Doodad, DoodadCofferState};
    use crate::models::house::housing_permission;
    use crate::systems::npcs::doodad::coffer;
    use crate::world::Entity;
    let mut w = world();
    w.server.names.add_character(1, "Alice", 1001);
    w.server.names.add_character(2, "Bob", 1002);
    let container = w
        .server
        .items
        .new_coffer_container(2, &w.server.ids.container);
    let container_id = container.lock().unwrap().container_id;
    let item = Item::create(
        900,
        w.server.game_data.items.template(SINGLE).unwrap(),
        1,
        0,
        1,
        &w.server.game_data.items,
        0,
    );
    container
        .lock()
        .unwrap()
        .add_new_item(item, 0, &w.server.game_data.items)
        .unwrap();
    let mut d = Doodad::new(100);
    d.owner_id = 2;
    d.data = i32::from(housing_permission::PRIVATE);
    d.transform.set_xyz(102.0, 100.0, 100.0);
    d.coffer = Some(DoodadCofferState {
        container_id,
        capacity: 10,
        opened_by: None,
    });
    let doodad = Entity::new(0x300, d);
    w.server
        .world
        .world(0)
        .unwrap()
        .spawn(&doodad, &w.server.view_ctx());
    w.a.drain();
    w.b.drain();
    assert!(!coffer::open_coffer_doodad(&w.server, &w.pa, doodad.obj_id));
    assert!(w.a.drain().is_empty());
    let mut body = move_body(900, 0, slot_type::TRADE, 0, slot_type::INVENTORY, 0);
    body.write_u64(container_id);
    run::<CSSwapCofferItemsPacket>(&mut w, 0, body.as_bytes());
    assert_eq!(w.a.drain_ops(), vec![sc::SCErrorMsgPacket]);
    assert!(container.lock().unwrap().get_item_by_id(900).is_some());

    // The normal item packet must not bypass coffer permission using its global item id.
    let bypass = move_body(900, 0, slot_type::TRADE, 0, slot_type::INVENTORY, 0);
    run::<CSSwapItemsPacket>(&mut w, 0, bypass.as_bytes());
    assert_eq!(w.a.drain_ops(), vec![sc::SCErrorMsgPacket]);

    // A shared family chest remains usable through the coffer workflow.
    w.pa.data().family = 99;
    w.pb.data().family = 99;
    doodad.data().data = i32::from(housing_permission::FAMILY);
    assert!(coffer::open_coffer_doodad(&w.server, &w.pa, doodad.obj_id));
    w.a.drain();
    doodad.data().transform.set_xyz(200.0, 100.0, 100.0);
    run::<CSSwapCofferItemsPacket>(&mut w, 0, body.as_bytes());
    assert_eq!(w.a.drain_ops(), vec![sc::SCErrorMsgPacket]);
    assert!(container.lock().unwrap().get_item_by_id(900).is_some());
    doodad.data().transform.set_xyz(102.0, 100.0, 100.0);
    run::<CSSwapCofferItemsPacket>(&mut w, 0, body.as_bytes());
    assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 0).unwrap().id, 900);
    assert!(container.lock().unwrap().items.is_empty());
    assert_eq!(w.a.drain_ops(), vec![sc::SCItemTaskSuccessPacket]);
}

#[tokio::test]
async fn split_merge_and_move_in_bag() {
    let mut w = world();
    let id = give(&w, &w.pa.clone(), slot_type::INVENTORY, STACK, 10, 0);

    // Split 4 into empty slot 1
    let mut s = move_body(id, 0, 2, 0, 2, 1);
    s.write_i32(4);
    run::<CSSplitBagItemPacket>(&mut w, 0, s.as_bytes());
    let src = item_at(&w.pa, 2, 0).unwrap();
    let new = item_at(&w.pa, 2, 1).unwrap();
    assert_eq!((src.count, new.count), (6, 4));
    // The split item belongs to the bag it was created in (C# left it at owner 0, and the
    // item could be lost on relogin).
    assert_eq!(new.owner_id, 1);
    let frames = w.a.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, sc::SCItemTaskSuccessPacket);
    assert_eq!(
        frames[0].1,
        task_body(
            task_type::SWAP_ITEMS,
            vec![ItemTask::count_update(&src, -4), ItemTask::add(&new)]
        )
    );

    // Merge slot 1 back into slot 0 (all 4)
    let s = move_body(new.id, id, 2, 1, 2, 0);
    run::<CSSwapItemsPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(item_at(&w.pa, 2, 0).unwrap().count, 10);
    assert!(item_at(&w.pa, 2, 1).is_none());
    let frames = w.a.drain();
    assert_eq!(frames.len(), 1);
    let merged = item_at(&w.pa, 2, 0).unwrap();
    assert_eq!(
        frames[0].1,
        task_body(
            task_type::SWAP_ITEMS,
            vec![
                ItemTask::remove(&Item {
                    count: 0,
                    ..new.clone()
                }),
                ItemTask::count_update(&merged, 4)
            ]
        )
    );
    // The merged-away id is released.
    assert!(w.server.items.item_by_id(new.id).is_none());

    // Move everything to slot 5
    let s = move_body(id, 0, 2, 0, 2, 5);
    run::<CSSwapItemsPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(item_at(&w.pa, 2, 5).unwrap().id, id);
    let frames = w.a.drain();
    assert_eq!(
        frames[0].1,
        task_body(
            task_type::SWAP_ITEMS,
            vec![ItemTask::ItemMove {
                from_slot_type: 2,
                from_slot: 0,
                from_item_id: id,
                to_slot_type: 2,
                to_slot: 5,
                to_item_id: 0
            }]
        )
    );

    // Wrong source slot -> InvalidSlot error
    let s = move_body(id, 0, 2, 3, 2, 6);
    run::<CSSwapItemsPacket>(&mut w, 0, s.as_bytes());
    let frames = w.a.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, sc::SCErrorMsgPacket);
}

#[test]
fn merging_from_another_container_releases_the_stack() {
    let game = test_data();
    let data = &game.items;
    let mut source = ItemContainer::new(ContainerKind::Item, 1, slot_type::INVENTORY, 100);
    source.set_container_size(2);
    let item = Item::create(3000, data.template(STACK).unwrap(), 5, 0, 1, data, 0);
    assert!(source.add_new_item(item, 0, data).is_ok());

    let mut target = ItemContainer::new(ContainerKind::Item, 1, slot_type::INVENTORY, 101);
    target.set_container_size(2);
    let first = Item::create(3001, data.template(STACK).unwrap(), 10, 0, 1, data, 0);
    assert!(target.add_new_item(first, 0, data).is_ok());

    let mut fx = ItemFxList::default();
    assert!(target.move_item_from(&mut source, task_type::MAIL, 3000, 0, data, &mut fx));
    assert_eq!(target.get_item_by_slot(0).unwrap().count, 15);
    assert!(source.items.is_empty());
    // The merged stack left every container: release its id / delete its row instead of
    // keeping a loose item copy.
    assert!(fx
        .0
        .iter()
        .any(|e| matches!(e, ItemFx::ReleaseId(id) if *id == 3000)));
    assert!(!fx.0.iter().any(|e| matches!(e, ItemFx::Orphaned(_))));
}

#[test]
fn moving_an_item_into_mail_raises_the_consumed_event() {
    let game = test_data();
    let data = &game.items;
    let mut bag = ItemContainer::new(ContainerKind::Item, 1, slot_type::INVENTORY, 100);
    bag.set_container_size(2);
    let item = Item::create(3000, data.template(STACK).unwrap(), 5, 0, 1, data, 0);
    assert!(bag.add_new_item(item, 0, data).is_ok());

    let mut mail = ItemContainer::new(ContainerKind::Item, 1, slot_type::MAIL, 101);
    mail.set_container_size(10);
    let mut fx = ItemFxList::default();
    assert!(mail.move_item_from(&mut bag, task_type::MAIL, 3000, -1, data, &mut fx));
    // Moving a bag item into the mail container consumes it (the C# read the overwritten
    // slot type and raised no event).
    assert_eq!(
        fx.0.iter()
            .filter(|e| matches!(
                e,
                ItemFx::Consumed {
                    owner: 1,
                    template_id: STACK,
                    count: 5
                }
            ))
            .count(),
        1
    );
    assert!(!fx.0.iter().any(|e| matches!(e, ItemFx::Acquired { .. })));
}

#[test]
fn creating_a_new_item_in_mail_raises_no_consumed_event() {
    let game = test_data();
    let data = &game.items;
    let mut mail = ItemContainer::new(ContainerKind::Item, 1, slot_type::MAIL, 101);
    mail.set_container_size(10);
    let item = Item::create(3000, data.template(STACK).unwrap(), 5, 0, 1, data, 0);
    let mut fx = ItemFxList::default();
    // A new item (achievement reward, schedule item, housing mail) was never in a bag, so
    // nothing is consumed: an ItemGather objective must not go negative.
    assert!(mail
        .add_item(task_type::MAIL, item, -1, data, &mut fx)
        .is_ok());
    assert_eq!(mail.items.len(), 1);
    assert!(!fx
        .0
        .iter()
        .any(|e| matches!(e, ItemFx::Consumed { .. } | ItemFx::Acquired { .. })));
}

#[tokio::test]
async fn cross_owner_moves_assign_the_holding_container_owner() {
    let w = world();
    let alice = w.pa.clone();
    let bob = w.pb.clone();
    let alice_bag = Inv::of(&alice).bag();
    let bob_bag = Inv::of(&bob).bag();
    let id = give(&w, &alice, slot_type::INVENTORY, STACK, 10, 0);
    {
        let mut txn = ItemTxn::begin(&w.server);
        assert!(inventory::split_or_move_item_ex(
            &mut txn,
            &Inv::of(&alice),
            task_type::SWAP_ITEMS,
            Some(alice_bag.clone()),
            Some(bob_bag.clone()),
            id,
            slot_type::INVENTORY,
            0,
            0,
            slot_type::INVENTORY,
            0,
            0,
        ));
    }
    let moved = item_at(&bob, slot_type::INVENTORY, 0).unwrap();
    assert_eq!(moved.owner_id, 2);
    assert!(item_at(&alice, slot_type::INVENTORY, 0).is_none());

    // A split from Bob's bag into Alice's bag creates Alice's item.
    {
        let mut txn = ItemTxn::begin(&w.server);
        assert!(inventory::split_or_move_item_ex(
            &mut txn,
            &Inv::of(&bob),
            task_type::SPLIT,
            Some(bob_bag.clone()),
            Some(alice_bag.clone()),
            id,
            slot_type::INVENTORY,
            0,
            0,
            slot_type::INVENTORY,
            0,
            4,
        ));
    }
    assert_eq!(item_at(&bob, slot_type::INVENTORY, 0).unwrap().owner_id, 2);
    let split = item_at(&alice, slot_type::INVENTORY, 0).unwrap();
    assert_eq!((split.count, split.owner_id), (4, 1));

    // A swap between two owners' bags gives each item its new holder.
    let alice_item = give(&w, &alice, slot_type::INVENTORY, STACK, 3, 1);
    let bob_item = give(&w, &bob, slot_type::INVENTORY, SINGLE, 1, 1);
    {
        let mut txn = ItemTxn::begin(&w.server);
        assert!(inventory::split_or_move_item_ex(
            &mut txn,
            &Inv::of(&alice),
            task_type::SWAP_ITEMS,
            Some(alice_bag),
            Some(bob_bag),
            alice_item,
            slot_type::INVENTORY,
            1,
            bob_item,
            slot_type::INVENTORY,
            1,
            0,
        ));
    }
    let in_bob = item_at(&bob, slot_type::INVENTORY, 1).unwrap();
    let in_alice = item_at(&alice, slot_type::INVENTORY, 1).unwrap();
    assert_eq!((in_bob.id, in_bob.owner_id), (alice_item, 2));
    assert_eq!((in_alice.id, in_alice.owner_id), (bob_item, 1));
}

#[tokio::test]
async fn swapping_equipment_reports_the_equipment_slot_as_previous() {
    let mut w = world();
    let pa = w.pa.clone();
    let bag_armor = give(&w, &pa, slot_type::INVENTORY, ARMOR, 1, 3);
    let worn = give(&w, &pa, slot_type::EQUIPMENT, ARMOR, 1, equip_slot::CHEST);
    w.a.drain();
    w.b.drain();

    let s = move_body(bag_armor, worn, 2, 3, 1, equip_slot::CHEST as u8);
    run::<CSSwapItemsPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(
        item_at(&pa, 1, equip_slot::CHEST as u8).unwrap().id,
        bag_armor
    );
    assert_eq!(item_at(&pa, 2, 3).unwrap().id, worn);

    // Both equipment events name the chest slot: the worn item entered it and the old item
    // left it (the C# passed the source bag slot as its previous slot).
    let slots: Vec<u8> =
        w.b.drain()
            .into_iter()
            .filter(|(op, _)| *op == sc::SCUnitEquipmentsChangedPacket)
            .map(|(_, body)| {
                let mut r = PacketStream::from_bytes(body);
                r.read_bc().unwrap();
                assert_eq!(r.read_u8().unwrap(), 1);
                r.read_u8().unwrap()
            })
            .collect();
    assert_eq!(slots, vec![equip_slot::CHEST as u8; 2]);
}

#[tokio::test]
async fn equip_into_empty_slot_binds_and_broadcasts() {
    let mut w = world();
    let pa = w.pa.clone();
    let id = give(&w, &pa, slot_type::INVENTORY, ARMOR, 1, 3);
    let armor = item_at(&pa, 2, 3).unwrap();
    assert_eq!(armor.class, ItemClass::Armor);

    // The client sends FROM the empty equipment slot TO the bag item.
    let s = move_body(0, id, 1, equip_slot::CHEST as u8, 2, 3);
    run::<CSSwapItemsPacket>(&mut w, 0, s.as_bytes());
    let equipped = item_at(&pa, 1, equip_slot::CHEST as u8).expect("equipped");
    assert_eq!(equipped.id, id);
    assert_ne!(equipped.flags & ITEM_FLAG_SOUL_BOUND, 0);
    assert!(item_at(&pa, 2, 3).is_none());

    let frames = w.a.drain();
    let ops: Vec<u16> = frames.iter().map(|f| f.0).collect();
    assert_eq!(
        ops,
        vec![sc::SCItemTaskSuccessPacket, sc::SCItemTaskSuccessPacket]
    );
    assert_eq!(
        frames[0].1,
        task_body(
            task_type::SWAP_ITEMS,
            vec![ItemTask::ItemMove {
                from_slot_type: 1,
                from_slot: 2,
                from_item_id: 0,
                to_slot_type: 2,
                to_slot: 3,
                to_item_id: id
            }]
        )
    );
    // ApplyBindRules on the equipment
    assert_eq!(
        frames[1].1,
        task_body(
            task_type::SWAP_ITEMS,
            vec![ItemTask::update_bits(&equipped)]
        )
    );
    // Bob sees the equipment change (the event carries the item as it was moved, before the
    // bind rules ran).
    let fb = w.b.drain();
    assert_eq!(fb.len(), 1);
    assert_eq!(fb[0].0, sc::SCUnitEquipmentsChangedPacket);
    let mut want = PacketStream::new();
    want.write_bc(0x100).write_u8(1).write_u8(2);
    Item {
        flags: 0,
        ..equipped.clone()
    }
    .write(&mut want);
    assert_eq!(fb[0].1, want.into_bytes());

    // Un-equip back to the bag (equipment -> bag slot 7)
    let s = move_body(id, 0, 1, 2, 2, 7);
    run::<CSSwapItemsPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(item_at(&pa, 2, 7).unwrap().id, id);
    let fb = w.b.drain();
    assert_eq!(fb.len(), 1);
    let mut want = PacketStream::new();
    want.write_bc(0x100).write_u8(1).write_u8(2).write_i32(0);
    assert_eq!(fb[0].1, want.into_bytes());
}

#[tokio::test]
async fn equip_into_wrong_slot_is_rejected() {
    let mut w = world();
    let pa = w.pa.clone();
    // Chest armor (slot type 3 -> equipment slot CHEST only) on bag slot 3.
    let id = give(&w, &pa, slot_type::INVENTORY, ARMOR, 1, 3);
    let worn = give(&w, &pa, slot_type::EQUIPMENT, ARMOR, 1, equip_slot::CHEST);
    let bag_before = item_at(&pa, 2, 3).unwrap();
    let worn_before = item_at(&pa, 1, equip_slot::CHEST as u8).unwrap();
    w.a.drain();

    // Bag item -> equipment slot 20 (hair), as seen on the live server.
    let s = move_body(id, 0, 2, 3, 1, equip_slot::HAIR as u8);
    run::<CSSwapItemsPacket>(&mut w, 0, s.as_bytes());
    // Swap with the worn item, but aimed at the hair slot.
    let s = move_body(id, worn, 2, 3, 1, equip_slot::HAIR as u8);
    run::<CSSwapItemsPacket>(&mut w, 0, s.as_bytes());

    let frames = w.a.drain();
    assert_eq!(frames.len(), 2);
    assert!(frames.iter().all(|f| f.0 == sc::SCErrorMsgPacket));
    assert_eq!(item_at(&pa, 2, 3).unwrap(), bag_before);
    assert_eq!(
        item_at(&pa, 1, equip_slot::CHEST as u8).unwrap(),
        worn_before
    );
    assert!(item_at(&pa, 1, equip_slot::HAIR as u8).is_none());
    assert!(w.b.drain().is_empty());
}

#[tokio::test]
async fn destroy_partial_and_full() {
    let mut w = world();
    let pa = w.pa.clone();
    let id = give(&w, &pa, slot_type::INVENTORY, STACK, 10, 0);
    let mut s = PacketStream::new();
    s.write_u64(id).write_u8(2).write_u8(0).write_i32(3);
    run::<CSDestroyItemPacket>(&mut w, 0, s.as_bytes());
    let it = item_at(&pa, 2, 0).unwrap();
    assert_eq!(it.count, 7);
    let f = w.a.drain();
    assert_eq!(
        f[0].1,
        task_body(task_type::DESTROY, vec![ItemTask::count_update(&it, -3)])
    );

    let mut s = PacketStream::new();
    s.write_u64(id).write_u8(2).write_u8(0).write_i32(7);
    run::<CSDestroyItemPacket>(&mut w, 0, s.as_bytes());
    assert!(item_at(&pa, 2, 0).is_none());
    let f = w.a.drain();
    assert_eq!(
        f[0].1,
        task_body(task_type::DESTROY, vec![ItemTask::remove_slot(&it)])
    );
    assert!(w.server.items.item_by_id(id).is_none());

    // Too many -> nothing
    let id = give(&w, &pa, slot_type::INVENTORY, STACK, 1, 0);
    let mut s = PacketStream::new();
    s.write_u64(id).write_u8(2).write_u8(0).write_i32(2);
    run::<CSDestroyItemPacket>(&mut w, 0, s.as_bytes());
    assert!(w.a.drain().is_empty());
}

/// `porting-md#10`: a non-positive `CSDestroyItem` count is a Sus cheating report with the
/// exact C# text, and the whole stack is destroyed as in C#.
#[tokio::test]
async fn destroy_negative_count_logs_a_sus_entry() {
    let mut w = world();
    let pa = w.pa.clone();
    let id = give(&w, &pa, slot_type::INVENTORY, STACK, 10, 0);
    let mut s = PacketStream::new();
    s.write_u64(id).write_u8(2).write_u8(0).write_i32(-1);
    run::<CSDestroyItemPacket>(&mut w, 0, s.as_bytes());
    let expected = format!(
        "CSDestroyItemPacket, player Alice attempted to destroy a negative amount of items -1 for item: template {STACK}, id {id}"
    );
    let mut entries = Vec::new();
    for _ in 0..100 {
        entries = w.server.systems.services.sus.recent_activities();
        if !entries.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert_eq!(
        entries,
        vec![(
            crate::systems::services::sus::CATEGORY_CHEATING.to_string(),
            1,
            expected
        )]
    );
    assert!(item_at(&pa, slot_type::INVENTORY, 0).is_none());
}

/// A partial `CSDestroyItem` raises `OnConsumedItem` for the destroyed units (deviation from
/// C#), so `ItemGather` objectives drop with them; a full destroy raises it once for the stack.
#[tokio::test]
async fn destroy_raises_consumed_events() {
    let mut w = world();
    let pa = w.pa.clone();
    let log = log_item_events(&w);
    let id = give(&w, &pa, slot_type::INVENTORY, STACK, 10, 0);
    let mut s = PacketStream::new();
    s.write_u64(id).write_u8(2).write_u8(0).write_i32(3);
    run::<CSDestroyItemPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(*log.0.lock().unwrap(), vec![(1, STACK, -3)]);

    let mut s = PacketStream::new();
    s.write_u64(id).write_u8(2).write_u8(0).write_i32(7);
    run::<CSDestroyItemPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(*log.0.lock().unwrap(), vec![(1, STACK, -3), (1, STACK, -7)]);
}

#[tokio::test]
async fn deposit_and_withdraw_money() {
    let mut w = world();
    spawn_service_npc(&mut w, 0x300, BANKER_TEMPLATE, 101.0);
    w.pa.data().unit.current_interaction_object = 0x300;
    w.pa.data().money = 500;
    let mut s = PacketStream::new();
    s.write_i32(200).write_i32(0);
    run::<CSDepositMoneyPacket>(&mut w, 0, s.as_bytes());
    let m = w.pa.data().money;
    let m2 = w.pa.data().money2;
    assert_eq!((m, m2), (300, 200));
    let f = w.a.drain();
    assert_eq!(
        f[0].1,
        task_body(
            task_type::DEPOSIT_MONEY,
            vec![ItemTask::MoneyChange(-200), ItemTask::MoneyChangeBank(200)]
        )
    );
    let mut s = PacketStream::new();
    s.write_i32(500).write_i32(0);
    run::<CSWithdrawMoneyPacket>(&mut w, 0, s.as_bytes());
    let f = w.a.drain();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].0, sc::SCErrorMsgPacket);
    let mut s = PacketStream::new();
    s.write_i32(50).write_i32(0);
    run::<CSWithdrawMoneyPacket>(&mut w, 0, s.as_bytes());
    let m = w.pa.data().money;
    let m2 = w.pa.data().money2;
    assert_eq!((m, m2), (350, 150));
    let f = w.a.drain();
    // Withdraw reports its own task type (`ItemTaskType.WithdrawMoney`; the C# kept the
    // default DepositMoney).
    assert_eq!(
        f[0].1,
        task_body(
            task_type::WITHDRAW_MONEY,
            vec![ItemTask::MoneyChangeBank(-50), ItemTask::MoneyChange(50)]
        )
    );
}

#[tokio::test]
async fn bank_refuses_non_positive_amounts_and_missing_banker() {
    let mut w = world();
    w.pa.data().money = 500;
    w.pa.data().money2 = 100;
    let mut body = PacketStream::new();
    body.write_i32(-1000).write_i32(0);
    let neg = body.into_bytes();
    let mut body = PacketStream::new();
    body.write_i32(0).write_i32(0);
    let zero = body.into_bytes();
    let mut body = PacketStream::new();
    body.write_i32(50).write_i32(0);
    let fifty = body.into_bytes();

    // At a banker, non-positive amounts are refused (no packet, no change).
    spawn_service_npc(&mut w, 0x300, BANKER_TEMPLATE, 101.0);
    w.pa.data().unit.current_interaction_object = 0x300;
    for body in [&neg, &zero] {
        run::<CSDepositMoneyPacket>(&mut w, 0, body);
        {
            let c = w.pa.data();
            assert_eq!((c.money, c.money2), (500, 100));
        }
        run::<CSWithdrawMoneyPacket>(&mut w, 0, body);
        {
            let c = w.pa.data();
            assert_eq!((c.money, c.money2), (500, 100));
        }
    }
    assert!(w.a.drain().is_empty());

    // No interaction at all: even a sane amount is refused (error to the client).
    w.pa.data().unit.current_interaction_object = 0;
    run::<CSDepositMoneyPacket>(&mut w, 0, &fifty);
    run::<CSWithdrawMoneyPacket>(&mut w, 0, &fifty);
    {
        let c = w.pa.data();
        assert_eq!((c.money, c.money2), (500, 100));
    }
    let f = w.a.drain();
    assert_eq!(f.len(), 2);
    assert!(f.iter().all(|(op, _)| *op == sc::SCErrorMsgPacket));

    // Out of range of the banker: refused.
    spawn_service_npc(&mut w, 0x301, BANKER_TEMPLATE, 150.0);
    w.pa.data().unit.current_interaction_object = 0x301;
    run::<CSDepositMoneyPacket>(&mut w, 0, &fifty);
    {
        let c = w.pa.data();
        assert_eq!((c.money, c.money2), (500, 100));
    }
}

#[tokio::test]
async fn acquire_stacks_then_creates_and_sends_timers() {
    let mut w = world();
    let pa = w.pa.clone();
    give(&w, &pa, slot_type::INVENTORY, STACK, 15, 0);
    let inv = Inv::of(&pa);
    {
        let mut txn = ItemTxn::begin(&w.server);
        assert!(inventory::try_add_new_item(
            &mut txn,
            &inv,
            task_type::GM,
            STACK,
            30,
            -1,
            0
        ));
    }
    assert_eq!(item_at(&pa, 2, 0).unwrap().count, 20);
    assert_eq!(item_at(&pa, 2, 1).unwrap().count, 20);
    assert_eq!(item_at(&pa, 2, 2).unwrap().count, 5);
    let f = w.a.drain();
    assert_eq!(f.len(), 1);
    let first = item_at(&pa, 2, 0).unwrap();
    assert_eq!(
        f[0].1,
        task_body(
            task_type::GM,
            vec![
                ItemTask::count_update(&first, 5),
                ItemTask::add(&item_at(&pa, 2, 1).unwrap()),
                ItemTask::add(&item_at(&pa, 2, 2).unwrap()),
            ]
        )
    );

    {
        let mut txn = ItemTxn::begin(&w.server);
        assert!(inventory::try_add_new_item(
            &mut txn,
            &inv,
            task_type::GM,
            TIMED,
            1,
            -1,
            0
        ));
    }
    let timed = item_at(&pa, 2, 3).unwrap();
    assert!(timed.expiration_time > 0);
    let f = w.a.drain();
    assert_eq!(
        f.iter().map(|x| x.0).collect::<Vec<_>>(),
        vec![sc::SCItemTaskSuccessPacket, sc::SCSyncItemLifespanPacket]
    );

    // Not enough room: nothing happens.
    {
        let mut txn = ItemTxn::begin(&w.server);
        assert!(!inventory::try_add_new_item(
            &mut txn,
            &inv,
            task_type::GM,
            SINGLE,
            100,
            -1,
            0
        ));
    }
    assert!(w.a.drain().is_empty());
}

#[tokio::test]
async fn consume_prefers_lowest_slots() {
    let w = world();
    let pa = w.pa.clone();
    give(&w, &pa, slot_type::INVENTORY, STACK, 5, 4);
    give(&w, &pa, slot_type::INVENTORY, STACK, 5, 2);
    let inv = Inv::of(&pa);
    {
        let mut txn = ItemTxn::begin(&w.server);
        let used = inventory::consume_item(
            &mut txn,
            &inv,
            None,
            task_type::SKILL_REAGENTS,
            STACK,
            7,
            None,
        );
        assert_eq!(used, 7);
    }
    assert!(item_at(&pa, 2, 2).is_none());
    assert_eq!(item_at(&pa, 2, 4).unwrap().count, 3);
}

#[tokio::test]
async fn expand_bag_and_bank() {
    let mut w = world();
    w.pa.data().money = 150;
    let mut s = PacketStream::new();
    s.write_i32(2).write_bool(false);
    run::<CSExpandSlotsPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(w.pa.data().money, 50);
    assert_eq!(w.pa.data().num_inventory_slots, 60);
    assert_eq!(Inv::of(&w.pa).bag().lock().unwrap().container_size(), 60);
    let f = w.a.drain();
    // The price is reported with the expand (the C# deducted it without sending the task).
    assert_eq!(f.len(), 2);
    assert_eq!(
        f[0].1,
        task_body(task_type::EXPAND_BAG, vec![ItemTask::MoneyChange(-100)])
    );
    assert_eq!(f[1].0, sc::SCInvenExpandedPacket);
    assert_eq!(f[1].1, vec![2, 60]);

    // Bank needs 2 x STACK in the bag
    let mut s = PacketStream::new();
    s.write_i32(3).write_bool(false);
    run::<CSExpandSlotsPacket>(&mut w, 0, s.as_bytes());
    assert!(w.a.drain().is_empty());
    let pa = w.pa.clone();
    give(&w, &pa, slot_type::INVENTORY, STACK, 3, 0);
    run::<CSExpandSlotsPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(w.pa.data().num_bank_slots, 60);
    assert_eq!(item_at(&pa, 2, 0).unwrap().count, 1);
    let f = w.a.drain();
    assert_eq!(
        f.iter().map(|x| x.0).collect::<Vec<_>>(),
        vec![sc::SCItemTaskSuccessPacket, sc::SCInvenExpandedPacket]
    );
}

#[tokio::test]
async fn swap_between_bag_and_bank() {
    let mut w = world();
    spawn_service_npc(&mut w, 0x300, BANKER_TEMPLATE, 101.0);
    w.pa.data().unit.current_interaction_object = 0x300;
    let pa = w.pa.clone();
    let a = give(&w, &pa, slot_type::INVENTORY, STACK, 3, 0);
    let b = give(&w, &pa, slot_type::BANK, SINGLE, 1, 4);
    let s = move_body(a, b, 2, 0, 3, 4);
    run::<CSSwapItemsPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(item_at(&pa, 3, 4).unwrap().id, a);
    assert_eq!(item_at(&pa, 2, 0).unwrap().id, b);
    assert_eq!(item_at(&pa, 3, 4).unwrap().slot_type, slot_type::BANK);
    let f = w.a.drain();
    assert_eq!(f.len(), 1);
}

fn set_item(p: &PlayerRef, st: u8, id: u64, f: impl FnOnce(&mut Item)) {
    let c = Inv::of(p).get(st).unwrap();
    let mut c = c.lock().unwrap();
    f(c.get_item_by_id_mut(id).expect("item"));
}

#[tokio::test]
async fn merging_needs_the_same_grade_and_flags() {
    let mut w = world();
    let pa = w.pa.clone();
    let a = give(&w, &pa, slot_type::INVENTORY, STACK, 3, 0);
    let b = give(&w, &pa, slot_type::INVENTORY, STACK, 2, 1);
    // A higher-grade / unbound target must not absorb a soulbound or lower-grade stack.
    set_item(&pa, slot_type::INVENTORY, a, |i| i.grade = 3);
    run::<CSSwapItemsPacket>(&mut w, 0, move_body(b, a, 2, 1, 2, 0).as_bytes());
    assert_eq!(item_at(&pa, 2, 0).unwrap().id, b);
    assert_eq!(item_at(&pa, 2, 1).unwrap().id, a);
    assert_eq!(item_at(&pa, 2, 0).unwrap().count, 2);
    assert_eq!(item_at(&pa, 2, 1).unwrap().count, 3);
    set_item(&pa, slot_type::INVENTORY, a, |i| i.grade = 0);
    set_item(&pa, slot_type::INVENTORY, b, |i| i.flags = 1);
    w.a.drain();
    run::<CSSwapItemsPacket>(&mut w, 0, move_body(b, a, 2, 0, 2, 1).as_bytes());
    assert_eq!(item_at(&pa, 2, 0).unwrap().id, a);
    assert_eq!(item_at(&pa, 2, 1).unwrap().count, 2);
    assert_eq!(item_at(&pa, 2, 0).unwrap().count, 3);
    // Identical stacks still merge.
    set_item(&pa, slot_type::INVENTORY, b, |i| i.flags = 0);
    run::<CSSwapItemsPacket>(&mut w, 0, move_body(b, a, 2, 1, 2, 0).as_bytes());
    assert_eq!(item_at(&pa, 2, 0).unwrap().count, 5);
    assert!(item_at(&pa, 2, 1).is_none());
}

#[tokio::test]
async fn moves_outside_the_container_size_are_refused() {
    let mut w = world();
    let pa = w.pa.clone();
    let a = give(&w, &pa, slot_type::INVENTORY, STACK, 3, 0);
    let free = Inv::of(&pa).bag().lock().unwrap().free_slot_count();
    let size = Inv::of(&pa).bag().lock().unwrap().size();
    assert!(size > 0 && size < 200);
    run::<CSSwapItemsPacket>(&mut w, 0, move_body(a, 0, 2, 0, 2, 200).as_bytes());
    let mut s = move_body(a, 0, 2, 0, 2, 200);
    s.write_i32(1);
    run::<CSSplitBagItemPacket>(&mut w, 0, s.as_bytes());
    let bag = Inv::of(&pa).bag();
    let bag = bag.lock().unwrap();
    assert_eq!(bag.free_slot_count(), free);
    assert_eq!(bag.items.len(), 1);
    assert_eq!(bag.items[0].slot, 0);
    assert_eq!(bag.items[0].count, 3);
}

#[tokio::test]
async fn warehouse_item_moves_need_a_banker_in_range() {
    let mut w = world();
    let pa = w.pa.clone();
    let a = give(&w, &pa, slot_type::INVENTORY, STACK, 3, 0);
    let b = give(&w, &pa, slot_type::BANK, SINGLE, 1, 4);
    // Without a banker the swap and the split are refused and nothing moves.
    run::<CSSwapItemsPacket>(&mut w, 0, move_body(a, b, 2, 0, 3, 4).as_bytes());
    let mut s = move_body(a, 0, 2, 0, 3, 7);
    s.write_i32(1);
    run::<CSSplitBagItemPacket>(&mut w, 0, s.as_bytes());
    assert_eq!(item_at(&pa, 2, 0).unwrap().id, a);
    assert_eq!(item_at(&pa, 3, 4).unwrap().id, b);
    assert!(item_at(&pa, 3, 7).is_none());
    let f = w.a.drain();
    assert_eq!(f.len(), 2);
    assert!(f.iter().all(|x| x.0 == sc::SCErrorMsgPacket));
    // With the banker in range the same swap works.
    spawn_service_npc(&mut w, 0x300, BANKER_TEMPLATE, 101.0);
    pa.data().unit.current_interaction_object = 0x300;
    run::<CSSwapItemsPacket>(&mut w, 0, move_body(a, b, 2, 0, 3, 4).as_bytes());
    assert_eq!(item_at(&pa, 3, 4).unwrap().id, a);
}

/// The func group of the service doodad tests: both window funcs, like the Fellowship Plaza's
/// "Auctioneer/Warehouse" (doodad 7983, group 22065: `DoodadFuncAuctionUi` 2 on skill 12083,
/// `DoodadFuncBankUi` 2 on skill 13238).
pub const SERVICE_DOODAD_GROUP: u32 = 77;

/// `data` plus the [`SERVICE_DOODAD_GROUP`] funcs. Like the real data, no func templates: the
/// loader reads no rows for these two func types.
pub fn with_service_doodad_funcs(mut data: GameData) -> GameData {
    use crate::systems::npcs::doodad::templates::DoodadFunc;
    let group = SERVICE_DOODAD_GROUP;
    let func = |key: u32, func_type: &str, skill_id: u32| DoodadFunc {
        group_id: group,
        func_id: 2,
        func_key: key,
        func_type: func_type.into(),
        next_phase: -1,
        sound_id: 0,
        skill_id,
        perm_id: 0,
        count: 0,
    };
    data.systems.npcs.doodad.funcs_by_groups.insert(
        group,
        vec![
            func(18333, "DoodadFuncAuctionUi", 12083),
            func(18334, "DoodadFuncBankUi", 13238),
        ],
    );
    data
}

/// Spawns a doodad of func group `group` at `x` (players stand at 100).
pub fn spawn_service_doodad(w: &mut World, obj_id: u32, group: u32, x: f32) {
    let mut d = crate::models::doodad::Doodad::new(7983);
    d.func_group_id = group;
    d.transform.set_xyz(x, 100.0, 100.0);
    let d = crate::world::Entity::new(obj_id, d);
    w.server
        .world
        .world(0)
        .unwrap()
        .spawn(&d, &w.server.view_ctx());
    w.a.drain();
    w.b.drain();
}

/// The Fellowship Plaza's "Auctioneer/Warehouse" opens the warehouse without a banker NPC:
/// deposits and warehouse moves are accepted next to it (with or without the doodad as the
/// interaction object) and refused once the player walks away. A doodad without the bank func
/// does not count.
#[tokio::test]
async fn the_warehouse_works_at_a_warehouse_doodad() {
    let mut w = world_with(with_service_doodad_funcs(test_data()));
    let pa = w.pa.clone();
    pa.data().money = 500;
    let mut body = PacketStream::new();
    body.write_i32(50).write_i32(0);
    let fifty = body.into_bytes();

    spawn_service_doodad(&mut w, 0x500, 999, 101.0);
    run::<CSDepositMoneyPacket>(&mut w, 0, &fifty);
    assert_eq!(pa.data().money, 500, "a plain doodad is no warehouse");
    let f = w.a.drain();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].0, sc::SCErrorMsgPacket);

    spawn_service_doodad(&mut w, 0x501, SERVICE_DOODAD_GROUP, 104.0);
    run::<CSDepositMoneyPacket>(&mut w, 0, &fifty);
    assert_eq!(
        pa.data().money,
        450,
        "no interaction object: the doodad around counts"
    );
    pa.data().unit.current_interaction_object = 0x501;
    let a = give(&w, &pa, slot_type::INVENTORY, STACK, 3, 0);
    let b = give(&w, &pa, slot_type::BANK, SINGLE, 1, 4);
    run::<CSSwapItemsPacket>(&mut w, 0, move_body(a, b, 2, 0, 3, 4).as_bytes());
    assert_eq!(item_at(&pa, 3, 4).unwrap().id, a);
    w.a.drain();

    // 9 m away from the doodad: refused again.
    pa.data().unit.transform.set_xyz(95.0, 100.0, 100.0);
    run::<CSDepositMoneyPacket>(&mut w, 0, &fifty);
    assert_eq!(pa.data().money, 450);
    let f = w.a.drain();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].0, sc::SCErrorMsgPacket);
}
