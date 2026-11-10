//! Flow tests of the vehicles subsystem: mount / dismount, pet exp, slave summon / drive /
//! despawn / death, a transfer following its route, shipyard build steps, and the gated
//! real-data tests (`AAEMU_COMPACT_DB`, `AAEMU_GAME_PAK`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aaemu_commons::PacketStream;
use sqlx::mysql::MySqlPoolOptions;

use crate::config::AppConfig;
use crate::data::client_files::ClientFileManager;
use crate::data::experience::{ExperienceTable, LevelTemplate};
use crate::data::items::{ItemTemplate, TemplateKind};
use crate::data::world_templates::{WorldSpawnPosition, WorldTemplate, WorldTemplates};
use crate::data::GameData;
use crate::models::container::slot_type;
use crate::models::item::Item;
use crate::models::movement::{MoveKind, MoveType, VehicleMove};
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::economy::error_message;
use crate::systems::economy::hooks::{NpcEconomyHooks, NpcShopInfo};
use crate::systems::npcs::doodad::spawner::{SimpleSpawnerDef, SpawnerBase};
use crate::systems::npcs::npc::data::NpcTemplate;
use crate::systems::skills::ext::ItemSkillHooks;
use crate::systems::DataLoadCtx;
use crate::test_support::*;
use crate::world::{MateRef, NpcRef, PlayerRef, SlaveRef, WorldObject};

use super::data::{
    attach_point, ShipyardStep, ShipyardTemplate, SlaveTemplate, TransferPath, TransferRoad,
    TransferTemplate, VehicleModel,
};
use super::{mates, shipyards, slaves, transfers};

const MATE_NPC: u32 = 900;
const MATE_ITEM: u32 = 5000;
/// A battle pet: its model declares no attach points.
const PET_NPC: u32 = 902;
const PET_ITEM: u32 = 5002;
/// A five-seat mount (the elephant's model).
const ELEPHANT_NPC: u32 = 903;
const ELEPHANT_ITEM: u32 = 5003;
const SLAVE_TEMPLATE: u32 = 68;
const SLAVE_ITEM: u32 = 5001;
/// Player obj ids outside the range the object id manager hands out in the tests.
const PA: u32 = 0x00F0_0001;
/// The design item object id the shipyard create tests name (`item_shipyards` 7000 -> 3).
const DESIGN_ITEM: u64 = 0x0000_0000_0000_9100;
const PB: u32 = 0x00F0_0002;

#[path = "battle_pet_tests.rs"]
mod battle_pet_tests;

#[path = "cinema_mate_tests.rs"]
mod cinema_mate_tests;

#[path = "deck_standing_tests.rs"]
mod deck_standing_tests;

#[path = "mate_dash_skill_tests.rs"]
mod mate_dash_skill_tests;

#[path = "move_guard_flow_tests.rs"]
mod move_guard_flow_tests;

#[path = "pr38_regression_tests.rs"]
mod pr38_regression_tests;

#[path = "rider_heading_tests.rs"]
mod rider_heading_tests;

#[path = "ship_identity_tests.rs"]
mod ship_identity_tests;

fn exp_table() -> ExperienceTable {
    ExperienceTable::from_templates(
        (1..=3u8)
            .map(|l| LevelTemplate {
                level: l,
                total_exp: i32::from(l) * 1000,
                total_mate_exp: (i32::from(l) - 1) * 500,
                skill_points: 0,
            })
            .collect(),
        55,
    )
    .expect("levels")
}

/// MaxHp / MaxMp formulas of the vehicle kinds (`unit_formulas`: kind 14 / 15, owners 2, 4,
/// 5, 6).
fn pool_formulas() -> crate::data::formulas::FormulaManager {
    super::stats::tests::formulas(
        &[
            (1, "1000", 14, 2),
            (2, "100", 15, 2),
            (3, "500", 14, 4),
            (4, "800 + level", 14, 5),
            (5, "300", 15, 5),
            (6, "2000", 14, 6),
        ],
        &[],
    )
}

fn data() -> GameData {
    let mut d = GameData::empty();
    d.formulas = pool_formulas();
    d.experience = exp_table();
    d.systems.npcs.npc.templates.insert(
        MATE_NPC,
        NpcTemplate {
            id: MATE_NPC,
            name: "Horse".into(),
            level: 1,
            model_id: 77,
            scale: 1.0,
            ..NpcTemplate::default()
        },
    );
    d.items.templates.insert(
        MATE_ITEM,
        ItemTemplate {
            kind: TemplateKind::SummonMate { npc_id: MATE_NPC },
            ..ItemTemplate::plain(MATE_ITEM, 1)
        },
    );
    // The horse model's real seats: driver + passenger0.
    d.systems
        .vehicles
        .model_bindings
        .insert(77, vec![attach_point::DRIVER, attach_point::PASSENGER0]);
    d.systems.npcs.npc.templates.insert(
        PET_NPC,
        NpcTemplate {
            id: PET_NPC,
            name: "Cat".into(),
            level: 1,
            model_id: 78,
            scale: 1.0,
            ..NpcTemplate::default()
        },
    );
    d.items.templates.insert(
        PET_ITEM,
        ItemTemplate {
            kind: TemplateKind::SummonMate { npc_id: PET_NPC },
            ..ItemTemplate::plain(PET_ITEM, 1)
        },
    );
    d.systems.npcs.npc.templates.insert(
        ELEPHANT_NPC,
        NpcTemplate {
            id: ELEPHANT_NPC,
            name: "Elephant".into(),
            level: 1,
            model_id: 79,
            scale: 1.0,
            ..NpcTemplate::default()
        },
    );
    d.items.templates.insert(
        ELEPHANT_ITEM,
        ItemTemplate {
            kind: TemplateKind::SummonMate {
                npc_id: ELEPHANT_NPC,
            },
            ..ItemTemplate::plain(ELEPHANT_ITEM, 1)
        },
    );
    d.systems.vehicles.model_bindings.insert(
        79,
        vec![
            attach_point::DRIVER,
            attach_point::PASSENGER0,
            attach_point::PASSENGER1,
            attach_point::PASSENGER2,
            attach_point::PASSENGER3,
        ],
    );
    d.items.templates.insert(
        SLAVE_ITEM,
        ItemTemplate {
            kind: TemplateKind::SummonSlave {
                slave_id: SLAVE_TEMPLATE,
            },
            ..ItemTemplate::plain(SLAVE_ITEM, 1)
        },
    );
    d.systems.vehicles.npc_mount_skills = vec![super::data::NpcMountSkills {
        id: 1,
        npc_id: MATE_NPC,
        mount_skill_id: 11,
    }];
    d.systems.vehicles.slave_templates.insert(
        SLAVE_TEMPLATE,
        SlaveTemplate {
            id: SLAVE_TEMPLATE,
            name: "Cart".into(),
            model_id: 300,
            level: 1,
            slave_kind: super::data::slave_kind::MACHINE,
            spawn_y_offset: 3.0,
            portal_time: 2.0,
            ..SlaveTemplate::default()
        },
    );
    // The test cart has a driver and a passenger seat, as the 1.2 client reads them from
    // `model_bindings`.
    d.systems
        .vehicles
        .model_bindings
        .insert(300, vec![attach_point::DRIVER, attach_point::PASSENGER0]);
    d
}

fn server_with(d: GameData) -> Arc<GameServer> {
    server_with_world(
        d,
        WorldTemplate::synthetic(0, "main_world", 2, 2, TEST_ZONE),
    )
}

fn server_with_world(d: GameData, world: WorldTemplate) -> Arc<GameServer> {
    let s = Arc::new(crate::test_support::test_server_with_world(d, world));
    s.set_self();
    // This synthetic server has no MySQL. Explicitly supply an empty saved-hull store;
    // persistence/error regressions use their own loader or private temporary tables.
    *s.systems.vehicles.slave_summons.test_loader.lock().unwrap() =
        Some(slaves::TestSlaveLoader(Arc::new(|_, _| {
            Box::pin(async { Ok(slaves::LoadedHull::default()) })
        })));
    *s.systems.npcs.spawns.test_persistent_loader.lock().unwrap() =
        Some(Arc::new(|_, _| Box::pin(async { Ok(Default::default()) })));
    s
}

/// A player with a summon item in the bag; returns (player, item id).
fn player_with_item(
    server: &GameServer,
    client: &TestClient,
    id: u32,
    obj_id: u32,
    at: (f32, f32),
    item_template: u32,
) -> (PlayerRef, u64) {
    let p = spawn_player(
        server,
        client,
        test_character(id, &format!("P{id}"), at.0, at.1),
        obj_id,
    );
    let item_id = 0x7000 + u64::from(id);
    give_item(server, &p, item_id, item_template);
    (p, item_id)
}

/// Adds another item to `p`'s bag (e.g. a second summon item for the same owner).
fn give_item(server: &GameServer, p: &PlayerRef, item_id: u64, item_template: u32) -> u64 {
    let t = server
        .game_data
        .items
        .template(item_template)
        .unwrap()
        .clone();
    let mut item = Item::create(item_id, &t, 1, 0, 1, &server.game_data.items, 0);
    item.owner_id = u64::from(p.data().id);
    item.slot_type = slot_type::INVENTORY;
    item.slot = 0;
    let bag = p
        .data()
        .inventory
        .get(slot_type::INVENTORY)
        .cloned()
        .unwrap();
    bag.lock().unwrap().items.push(item);
    item_id
}

fn ops(frames: &[(u16, Vec<u8>)]) -> Vec<u16> {
    frames.iter().map(|f| f.0).collect()
}

fn body(frames: &[(u16, Vec<u8>)], op: u16) -> Vec<u8> {
    frames
        .iter()
        .find(|f| f.0 == op)
        .unwrap_or_else(|| panic!("no 0x{op:03x} in {:x?}", ops(frames)))
        .1
        .clone()
}

/// `ItemTaskType.MateDeath` / `RepairPets` / `DepositMoney`.
const MATE_DEATH_TASK: u8 = 59;
const REPAIR_PETS_TASK: u8 = 58;

/// The error codes of the `SCErrorMsgPacket`s in `frames`.
fn error_codes(frames: &[(u16, Vec<u8>)]) -> Vec<i16> {
    frames
        .iter()
        .filter(|f| f.0 == sc::SCErrorMsgPacket)
        .map(|f| i16::from_le_bytes([f.1[0], f.1[1]]))
        .collect()
}

/// The `SCSkillStarted` result of `skill` (Mount caster, Position or Unit target, no skill
/// object: the shapes the cannon flow sends).
fn started_result(frames: &[(u16, Vec<u8>)], skill: u32) -> Option<u8> {
    frames
        .iter()
        .filter(|f| f.0 == sc::SCSkillStartedPacket)
        .find_map(|(_, body)| {
            let mut s = PacketStream::from_bytes(body.clone());
            if s.read_u32().ok()? != skill {
                return None;
            }
            let _ = s.read_u16();
            let caster = s.read_u8().ok()?;
            let _ = s.read_bc();
            if caster == 3 {
                let _ = s.read_u32();
            }
            match s.read_u8().ok()? {
                1 => {
                    let _ = s.read_i64();
                    let _ = s.read_i64();
                    let _ = s.read_f32();
                    let _ = s.read_f32();
                    let _ = s.read_bc();
                    let _ = s.read_bc();
                }
                0 | 4 => {
                    let _ = s.read_bc();
                }
                _ => return None,
            }
            let _ = s.read_u8();
            let _ = s.read_u16();
            let _ = s.read_u16();
            let _ = s.read_u8();
            let flags = s.read_u8().ok()?;
            (flags & 1 != 0).then(|| s.read_u8().ok()).flatten()
        })
}

/// The actions of the `SCItemTaskSuccessPacket`s in `frames`.
fn task_actions(frames: &[(u16, Vec<u8>)]) -> Vec<u8> {
    frames
        .iter()
        .filter(|f| f.0 == sc::SCItemTaskSuccessPacket)
        .map(|f| f.1[0])
        .collect()
}

fn bc(v: u32) -> Vec<u8> {
    v.to_le_bytes()[..3].to_vec()
}

/// Put a not-yet-seated test player next to `target` so the client-reach bind check passes.
fn stand_next_to(
    world: &Arc<crate::world::WorldInstance>,
    player: &PlayerRef,
    target: &WorldObject,
) {
    let t = world.world_pos_rot(target);
    player.data().unit.transform.set_xyz(t.x + 1.0, t.y, t.z);
}

/// Captures the tracing output of the current test thread (error-line assertions).
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).expect("utf-8 log")
    }
}

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_logs() -> (tracing::subscriber::DefaultGuard, LogBuffer) {
    let buffer = LogBuffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    (tracing::subscriber::set_default(subscriber), buffer)
}

#[tokio::test]
async fn spawning_a_mate_syncs_the_summon_items_level_from_the_mate_row() {
    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    a.drain();
    // A mate row (the spawn source of truth) above the item's fresh `DetailLevel` of 0.
    let now = chrono::Utc::now().naive_utc();
    server.systems.vehicles.mates.character(1, |cm| {
        cm.mates.push(mates::MateDb {
            id: 77,
            item_id,
            name: "Horse".into(),
            xp: 1234,
            level: 50,
            mileage: 0,
            hp: 100,
            mp: 100,
            owner: 1,
            updated_at: now,
            created_at: now,
        });
    });

    mates::spawn_mount(&server, &pa, item_id);

    let bag = pa
        .data()
        .inventory
        .get(slot_type::INVENTORY)
        .cloned()
        .unwrap();
    let item = bag
        .lock()
        .unwrap()
        .get_item_by_id(item_id)
        .cloned()
        .expect("summon item");
    assert_eq!(item.details.mate_level, 50);
    assert_eq!(item.details.mate_exp, 1234);
    let mate = server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(1)
        .pop()
        .expect("active mate");
    assert_eq!(mate.data().unit.level, 50);
}

#[tokio::test]
async fn mount_and_dismount_flow() {
    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    let pb = spawn_player(&server, &b, test_character(2, "B", 110.0, 110.0), PB);
    a.drain();
    b.drain();

    mates::spawn_mount(&server, &pa, item_id);
    let fa = a.drain();
    assert_eq!(
        &ops(&fa)[..2],
        &[sc::SCItemTaskSuccessPacket, sc::SCMateSpawnedPacket]
    );
    let manager = server.systems.vehicles.mates.get(0);
    let mate = manager.get_active_mates(1).pop().expect("active mate");
    let tl = mate.data().unit.tl_id;
    // SCMateSpawnedPacket: tl, db id, item id, user state 1, exp of level 1 (0), mileage, delay,
    // the template's mount skill + 9 zeros.
    let mut s = PacketStream::new();
    s.write_u16(tl)
        .write_u32(mate.data().id)
        .write_u64(item_id)
        .write_u8(1)
        .write_i32(0)
        .write_i32(0)
        .write_u32(0)
        .write_u32(11);
    for _ in 0..9 {
        s.write_i32(0);
    }
    assert_eq!(body(&fa, sc::SCMateSpawnedPacket), s.as_bytes());
    // The mate appears to both players (SCUnitState + SCMateState + SCUnitPoints).
    for f in [&fa, &b.drain()] {
        let o = ops(f);
        let i = o
            .iter()
            .position(|op| *op == sc::SCUnitStatePacket)
            .unwrap();
        assert_eq!(
            &o[i..i + 3],
            &[
                sc::SCUnitStatePacket,
                sc::SCMateStatePacket,
                sc::SCUnitPointsPacket
            ]
        );
    }
    // The mount stands 3 m in front of the owner (yaw 0 -> +y).
    let t = mate.data().unit.transform;
    assert_eq!((t.x, t.y), (100.0, 103.0));

    // A non-owner cannot take the driver seat.
    mates::mount_mate(&server, &pb, u32::from(tl), attach_point::DRIVER, 1);
    assert!(a.drain().is_empty());
    assert!(!pb.data().is_riding);

    // The owner mounts: SCUnitAttachedPacket to everybody around, the rider becomes a child.
    mates::mount_mate(&server, &pa, u32::from(tl), attach_point::DRIVER, 1);
    let mut attached = bc(PA);
    attached.push(attach_point::DRIVER);
    attached.extend(bc(mate.obj_id));
    attached.push(1);
    for c in [&mut a, &mut b] {
        let f = c.drain();
        assert_eq!(body(&f, sc::SCUnitAttachedPacket), attached);
    }
    assert!(pa.data().is_riding);
    assert_eq!(pa.data().attached_point, attach_point::DRIVER);
    assert_eq!(pa.data().unit.transform.parent_obj_id, mate.obj_id);
    assert_eq!(
        mate.data().passenger(attach_point::DRIVER).unwrap().obj_id,
        PA
    );
    assert_eq!(
        server
            .systems
            .vehicles
            .mates
            .get(0)
            .get_is_mounted(PA)
            .map(|(m, p)| (m.obj_id, p)),
        Some((mate.obj_id, attach_point::DRIVER))
    );

    // Dismount: SCUnitDetachedPacket, the rider stands where the mount is.
    mates::unmount_mate(&server, &pa, u32::from(tl), attach_point::DRIVER, 4);
    let mut detached = bc(PA);
    detached.push(4);
    for c in [&mut a, &mut b] {
        let f = c.drain();
        assert_eq!(body(&f, sc::SCUnitDetachedPacket), detached);
    }
    assert!(!pa.data().is_riding);
    let t = pa.data().unit.transform;
    assert_eq!((t.parent_obj_id, t.x, t.y), (0, 100.0, 103.0));

    // Despawn (CSRemoveMatePacket): the mate leaves the world, its row is updated.
    mate.data().unit.hp = 42;
    mates::despawn_mate(&server, &pa, u32::from(tl));
    assert!(ops(&b.drain()).contains(&sc::SCUnitsRemovedPacket));
    assert!(server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(1)
        .is_empty());
    let db = server.systems.vehicles.mates.character_mates(1).unwrap();
    assert_eq!(db.mates[0].hp, 42);
    assert_eq!(db.mates[0].item_id, item_id);
    assert!(server.world.world(0).unwrap().mate(mate.obj_id).is_none());
}

/// The movement speed baseline must not survive a ride, a teleport or a mate respawn (they were
/// reported as 40 to 3000 m/s runs / pets).
#[tokio::test]
async fn a_server_teleport_dismounts_the_rider_instead_of_leaving_it_half_attached() {
    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    a.conn.active_char = Some(pa.clone());
    mates::spawn_mount(&server, &pa, item_id);
    let mate = server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(1)
        .pop()
        .expect("active mate");
    let tl = u32::from(mate.data().unit.tl_id);
    mates::mount_mate(&server, &pa, tl, attach_point::DRIVER, 1);
    assert!(pa.data().is_riding);
    a.drain();

    let destination = crate::models::transform::Transform {
        instance_id: 0,
        x: 300.0,
        y: 300.0,
        z: 100.0,
        ..crate::models::transform::Transform::default()
    };
    assert!(crate::world::session::teleport_character(
        &server,
        &pa,
        destination
    ));
    assert!(!pa.data().is_riding);
    assert_eq!(pa.data().attached_point, 0);
    let t = pa.data().unit.transform;
    assert_eq!((t.parent_obj_id, t.x, t.y), (0, 300.0, 300.0));
    assert!(mate
        .data()
        .passenger(attach_point::DRIVER)
        .is_none_or(|seat| seat.obj_id == 0));
    assert!(server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_is_mounted(PA)
        .is_none());
    assert!(
        ops(&a.drain()).contains(&sc::SCUnitDetachedPacket),
        "the client is told the rider left the mount"
    );
}

#[tokio::test]
async fn riding_teleports_and_mate_respawns_reseed_the_speed_baseline() {
    use crate::models::movement::UnitMove;
    use crate::systems::services::sus::SampleCtx;

    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    a.conn.active_char = Some(pa.clone());
    a.drain();
    let sus = &server.systems.services.sus;
    let far = [5000.0, 100.0, 100.0];
    let farther = [10000.0, 100.0, 100.0];
    let walking = MoveType::new(MoveKind::Unit(UnitMove {
        x: 150.0,
        y: 100.0,
        z: 100.0,
        vel_x: 10,
        ..Default::default()
    }));

    // A mate spawn / despawn drops the mount baseline (its database id outlives its spawns).
    mates::spawn_mount(&server, &pa, item_id);
    let mate = server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(1)
        .pop()
        .expect("active mate");
    let (mate_id, tl) = {
        let d = mate.data();
        (d.id, u32::from(d.unit.tl_id))
    };
    sus.analyze_mount_delta_movement(mate_id, [100.0, 100.0, 100.0], false, 5.0);
    mates::despawn_mate(&server, &pa, tl);
    assert_eq!(
        sus.analyze_mount_delta_movement(mate_id, far, false, 5.0),
        None
    );
    sus.reset_analyze_mount_delta_movement(mate_id);
    sus.analyze_mount_delta_movement(mate_id, [100.0, 100.0, 100.0], false, 5.0);
    mates::spawn_mount(&server, &pa, item_id);
    assert_eq!(
        sus.analyze_mount_delta_movement(mate_id, far, false, 5.0),
        None
    );
    let mate = server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(1)
        .pop()
        .expect("respawned mate");
    let tl = u32::from(mate.data().unit.tl_id);

    // Riding: the player's own moves are ignored, which reseeds its baseline.
    sus.analyze_player_delta_movement(1, [100.0, 100.0, 100.0], false, false, 5.0);
    mates::mount_mate(&server, &pa, tl, attach_point::DRIVER, 1);
    assert!(pa.data().is_riding);
    crate::world::session::move_unit(&server, &mut a.conn, PA, walking.clone()).await;
    assert_eq!(
        sus.analyze_player_delta_movement(1, far, false, false, 5.0),
        None,
        "the dismounted player is judged from the first sample, not the pre-ride position"
    );
    // ... and the check is live afterwards.
    assert!(sus
        .analyze_player_delta_movement(1, farther, false, false, 5.0)
        .is_some());
    mates::unmount_mate(&server, &pa, tl, attach_point::DRIVER, 4);

    // Teleport (`set_disabled_set_position(true)`): moves are dropped while it is set and the
    // baseline is invalid once it is cleared.
    let ctx = |ms: i64| {
        let c = pa.data();
        SampleCtx {
            now_ms: ms,
            instance_id: c.unit.transform.instance_id,
            teleport_seq: c.teleport_seq,
        }
    };
    sus.reset_analyze_player_delta_movement(1);
    sus.analyze_player_delta_movement_at(1, [100.0, 100.0, 100.0], false, false, 5.0, ctx(0));
    pa.data().set_disabled_set_position(true);
    crate::world::session::move_unit(&server, &mut a.conn, PA, walking.clone()).await;
    assert_eq!(
        pa.data().unit.transform.x,
        100.0,
        "moves are dropped meanwhile"
    );
    pa.data().set_disabled_set_position(false);
    assert_eq!(
        sus.analyze_player_delta_movement_at(1, far, false, false, 5.0, ctx(5_000)),
        None
    );
    assert!(sus
        .analyze_player_delta_movement_at(1, farther, false, false, 5.0, ctx(10_000))
        .is_some());
}

/// Sample the actual rider state within the mount baseline's 10 s window.
#[tokio::test]
async fn mount_sus_uses_rider_teleports_and_resets_on_seat_changes() {
    let server = server_with(data());
    let a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    mates::spawn_mount(&server, &pa, item_id);
    let mate = server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(1)
        .pop()
        .expect("active mate");
    let tl = u32::from(mate.data().unit.tl_id);
    let sus = &server.systems.services.sus;
    let sample = |ms| sus.analyze_mount_sample(&server, &mate, 5.0, ms);
    let move_to = |x| mate.data().unit.transform.set_xyz(x, 100.0, 100.0);
    assert!(sample(0).is_none());
    mates::mount_mate(&server, &pa, tl, attach_point::DRIVER, 1);
    move_to(5_000.0);
    assert!(sample(1_000).is_none(), "ride drops the baseline");
    pa.data().set_disabled_set_position(true);
    pa.data().set_disabled_set_position(false);
    move_to(15_000.0);
    assert!(
        sample(2_000).is_none(),
        "completed rider teleport reseeds the mount"
    );
    pa.data().set_disabled_set_position(true);
    move_to(20_000.0);
    assert!(
        sample(3_000).is_none(),
        "disabled position suppresses checks"
    );
    pa.data().set_disabled_set_position(false);
    move_to(20_050.0);
    assert!(sample(4_000).is_none(), "ordinary movement at destination");
    move_to(21_050.0);
    assert_eq!(
        sample(9_000)
            .expect("excessive movement still reported")
            .speed,
        200.0
    );
    mates::unmount_mate(&server, &pa, tl, attach_point::DRIVER, 4);
    move_to(30_000.0);
    assert!(sample(10_000).is_none(), "unride drops the baseline");
    pa.data().set_disabled_set_position(true);
    pa.data().set_disabled_set_position(false);
    move_to(35_000.0);
    assert!(
        sample(11_000).is_none(),
        "owner fallback reseeds the baseline"
    );
    // The rider walks up to the mate again (a mount needs the rider in reach).
    pa.data().unit.transform.set_xyz(35_000.0, 100.0, 100.0);
    mates::mount_mate(&server, &pa, tl, attach_point::DRIVER, 1);
    move_to(40_000.0);
    assert!(sample(12_000).is_none(), "remount drops the baseline");
    move_to(41_000.0);
    assert!(
        sample(17_000).is_some(),
        "remount does not exempt later speeding"
    );
}

/// A client keeps moving a mount the server just despawned for a moment: the move is dropped
/// without a warning and without touching anything.
#[tokio::test]
async fn a_move_for_a_despawned_mate_is_dropped_quietly() {
    use crate::models::movement::UnitMove;

    let (_guard, logs) = capture_logs();
    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    a.conn.active_char = Some(pa.clone());
    mates::spawn_mount(&server, &pa, item_id);
    let mate = server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(1)
        .pop()
        .expect("active mate");
    let (mate_obj, tl) = (mate.obj_id, u32::from(mate.data().unit.tl_id));
    mates::despawn_mate(&server, &pa, tl);
    a.drain();
    let walking = MoveType::new(MoveKind::Unit(UnitMove {
        x: 150.0,
        y: 100.0,
        z: 100.0,
        vel_x: 10,
        ..Default::default()
    }));
    crate::world::session::move_unit(&server, &mut a.conn, mate_obj, walking).await;
    assert!(a.drain().is_empty());
    assert_eq!(pa.data().unit.transform.x, 100.0);
    assert!(!logs.text().contains("Invalid target"), "{}", logs.text());
}

#[tokio::test]
async fn mate_summon_sends_attribute_pools_preserves_injured_saves_and_refuses_dead_ones() {
    use crate::net::registry::ClientPacket;
    let mut d = data();
    d.formulas = super::stats::tests::formulas(
        &[
            (99, "(level * 140 + sta * 12) * mate_kind", 14, 5),
            (100, "level * 15 + int * 10 * mate_kind", 15, 5),
            (144, "10 + (level - 1) * 4", 24, 5),
            (150, "10 + (level - 1) * 4", 25, 5),
            (156, "10 + (level - 1) * 4", 26, 5),
        ],
        &[(99, 5, 1, 1.0), (100, 5, 1, 1.0)],
    );
    let template = d.systems.npcs.npc.templates.get_mut(&MATE_NPC).unwrap();
    template.level = 5;
    template.mate_kind_id = 1;
    let server = server_with(d);
    let mut client = TestClient::new(&server, 1, 1);
    let (player, item_id) = player_with_item(&server, &client, 1, PA, (100.0, 100.0), MATE_ITEM);
    // The synthetic Character defaults to zero HP. A dead owner's mates are
    // correctly despawned on mate death, so this restore flow needs a living owner.
    {
        let mut data = player.data();
        data.unit.hp = 1000;
        data.unit.max_hp = 1000;
    }
    client.conn.active_char = Some(player.clone());
    client.drain();
    mates::spawn_mount(&server, &player, item_id);
    let manager = server.systems.vehicles.mates.get(0);
    let mate = manager.get_active_mates(1).pop().unwrap();
    {
        let mate = mate.data();
        assert_eq!(
            (
                mate.unit.hp,
                mate.unit.max_hp,
                mate.unit.mp,
                mate.unit.max_mp
            ),
            (1012, 1012, 335, 335)
        );
    }
    let frames = client.drain();
    let mut points = PacketStream::new();
    points
        .write_bc(mate.obj_id)
        .write_i32(1012 * 100)
        .write_i32(335 * 100);
    assert_eq!(body(&frames, sc::SCUnitPointsPacket), points.as_bytes());

    // An injured, exhausted mate comes back as saved.
    let (hp, mp) = (117, 0);
    {
        let mut unit = mate.data();
        unit.unit.hp = hp;
        unit.unit.mp = mp;
    }
    let tl_id = mate.data().unit.tl_id;
    super::packets::CSRemoveMatePacket { tl_id }
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    assert!(manager.get_active_mates(1).is_empty());
    let stored = server.systems.vehicles.mates.character_mates(1).unwrap();
    assert_eq!(
        (
            stored.get(item_id).unwrap().hp,
            stored.get(item_id).unwrap().mp
        ),
        (hp, mp)
    );
    assert_eq!(
        mates::owned_item(&server, &player, item_id)
            .unwrap()
            .details
            .mate_dead,
        0
    );
    client.drain();
    mates::spawn_mount(&server, &player, item_id);
    let restored = manager.get_active_mates(1).pop().unwrap();
    {
        let data = restored.data();
        assert_eq!((data.unit.hp, data.unit.mp), (hp, mp));
    }
    let mut points = PacketStream::new();
    points
        .write_bc(restored.obj_id)
        .write_i32(hp * 100)
        .write_i32(mp * 100);
    assert_eq!(
        body(&client.drain(), sc::SCUnitPointsPacket),
        points.as_bytes()
    );

    // A mate dismissed with zero HP is a dead one: the summon item is flagged (and the client
    // told), the mate row keeps no HP and the next summon is refused.
    {
        let mut unit = restored.data();
        unit.unit.hp = 0;
        unit.unit.mp = 42;
    }
    let tl_id = restored.data().unit.tl_id;
    super::packets::CSRemoveMatePacket { tl_id }
        .handle(&server, &mut client.conn)
        .await
        .unwrap();
    assert!(manager.get_active_mates(1).is_empty());
    let stored = server.systems.vehicles.mates.character_mates(1).unwrap();
    assert_eq!(
        (
            stored.get(item_id).unwrap().hp,
            stored.get(item_id).unwrap().mp
        ),
        (0, 42)
    );
    assert_eq!(task_actions(&client.drain()), vec![MATE_DEATH_TASK]);
    assert_eq!(
        mates::owned_item(&server, &player, item_id)
            .unwrap()
            .details
            .mate_dead,
        1
    );
    mates::spawn_mount(&server, &player, item_id);
    assert!(manager.get_active_mates(1).is_empty());
    let frames = client.drain();
    assert_eq!(
        error_codes(&frames),
        vec![error_message::MATE_CANNOT_SPAWN_WHILE_CORPSE]
    );
    assert!(!ops(&frames).contains(&sc::SCMateSpawnedPacket));
}

const STABLER_NPC: u32 = 901;
const STABLER_OBJ: u32 = 0x300;

/// Shop flags by template: only `STABLER_NPC` is a stabler.
struct StablerHook;
impl NpcEconomyHooks for StablerHook {
    fn shop_info(&self, _: &GameServer, npc: &NpcRef) -> Option<NpcShopInfo> {
        Some(NpcShopInfo {
            stabler: npc.data().unit.template_id == STABLER_NPC,
            ..Default::default()
        })
    }
}

fn spawn_npc_at(server: &GameServer, obj_id: u32, template: u32, x: f32, y: f32) -> NpcRef {
    let npc = crate::systems::npcs::npc::manager::create(server, obj_id, template).expect("npc");
    npc.data().unit.transform.set_xyz(x, y, 100.0);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&npc, &server.view_ctx());
    npc
}

/// A player (12 000 copper, living) with a summoned mate that was just killed by an NPC, and a
/// stabler 2 m away that is not yet the interaction object.
struct DeadMate {
    server: Arc<GameServer>,
    client: TestClient,
    player: PlayerRef,
    item_id: u64,
    mate: crate::world::MateRef,
    stabler: NpcRef,
}

fn dead_mate() -> DeadMate {
    let mut d = data();
    d.systems.npcs.npc.templates.insert(
        STABLER_NPC,
        NpcTemplate {
            id: STABLER_NPC,
            name: "Stablehand".into(),
            level: 1,
            ..NpcTemplate::default()
        },
    );
    let server = server_with(d);
    server
        .systems
        .economy
        .hooks
        .npcs
        .install(Arc::new(StablerHook));
    let mut client = TestClient::new(&server, 1, 1);
    let (player, item_id) = player_with_item(&server, &client, 1, PA, (100.0, 100.0), MATE_ITEM);
    {
        let mut c = player.data();
        c.unit.hp = 1000;
        c.unit.max_hp = 1000;
        c.money = 12_000;
    }
    client.conn.active_char = Some(player.clone());
    let stabler = spawn_npc_at(&server, STABLER_OBJ, STABLER_NPC, 102.0, 100.0);
    let killer = spawn_npc_at(&server, 0x200, MATE_NPC, 110.0, 110.0);
    mates::spawn_mount(&server, &player, item_id);
    let mate = mates::first_active_mate(&server, &player).expect("active mate");
    client.drain();
    crate::systems::skills::combat::reduce_current_hp(
        &server,
        &WorldObject::from(&mate),
        Some(&WorldObject::from(&killer)),
        1_000_000,
        crate::systems::skills::types::KillReason::Damage,
    );
    assert_eq!(mate.data().unit.hp, 0);
    DeadMate {
        server,
        client,
        player,
        item_id,
        mate,
        stabler,
    }
}

impl DeadMate {
    fn dismiss(&mut self) {
        let tl = u32::from(self.mate.data().unit.tl_id);
        mates::despawn_mate(&self.server, &self.player, tl);
        self.client.drain();
    }

    async fn revive_at(&mut self, npc_obj_id: u32) -> Vec<(u16, Vec<u8>)> {
        use crate::net::registry::ClientPacket;
        crate::systems::economy::mate_equipment::CSRepairPetItemsPacket { npc_id: npc_obj_id }
            .handle(&self.server, &mut self.client.conn)
            .await
            .unwrap();
        self.client.drain()
    }

    fn item(&self) -> Item {
        mates::owned_item(&self.server, &self.player, self.item_id).unwrap()
    }

    fn row(&self) -> mates::MateDb {
        let cm = self
            .server
            .systems
            .vehicles
            .mates
            .character_mates(1)
            .unwrap();
        cm.get(self.item_id).cloned().unwrap()
    }
}

#[test]
fn mate_revive_cost_follows_the_client_formula() {
    assert_eq!([0, 1, 5, 55].map(mates::revive_cost), [0, 2, 43, 10_093]);
}

#[tokio::test]
async fn a_killed_mate_flags_its_item_once_and_cannot_be_summoned_until_revived() {
    let mut t = dead_mate();
    let f = t.client.drain();
    // The death hook: MateDeath item task (byte-exact ItemUpdate of the flagged item) + MATE_DEAD.
    let item = t.item();
    assert_eq!(item.details.mate_dead, 1);
    assert_eq!(item.details.mate_level, 1);
    assert!(item.dirty);
    let mut expect = PacketStream::new();
    aaemu_commons::Marshal::write(
        &crate::systems::economy::item_packets::SCItemTaskSuccessPacket {
            action: MATE_DEATH_TASK,
            tasks: vec![crate::models::item_task::ItemTask::update(&item)],
            force_remove: Vec::new(),
        },
        &mut expect,
    );
    let frames: Vec<_> = f
        .iter()
        .filter(|x| x.0 == sc::SCItemTaskSuccessPacket)
        .collect();
    assert_eq!(frames.len(), 1, "{:x?}", ops(&f));
    assert_eq!(frames[0].1, expect.as_bytes());
    assert_eq!(error_codes(&f), vec![error_message::MATE_DEAD]);
    assert_eq!(t.row().hp, 0);

    // Idempotent: another death notice, and the dismissal that snapshots hp 0, send nothing new.
    mates::on_death(&t.server, &t.mate);
    assert!(t.client.drain().is_empty());
    t.dismiss();
    assert!(t
        .server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(1)
        .is_empty());
    mates::spawn_mount(&t.server, &t.player, t.item_id);
    let f = t.client.drain();
    assert_eq!(
        error_codes(&f),
        vec![error_message::MATE_CANNOT_SPAWN_WHILE_CORPSE]
    );
    assert!(task_actions(&f).is_empty());
    assert!(t
        .server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(1)
        .is_empty());
}

#[tokio::test]
async fn mate_make_get_up_revives_the_summoned_corpse() {
    let mut t = dead_mate();
    t.client.drain();
    assert_eq!(t.mate.data().unit.hp, 0);

    assert!(mates::revive_mate(&t.server, &t.mate));
    let frames = t.client.drain();
    assert_eq!(task_actions(&frames), vec![REPAIR_PETS_TASK]);
    assert!(ops(&frames).contains(&sc::SCUnitPointsPacket));
    assert_eq!(t.item().details.mate_dead, 0);
    let row = t.row();
    assert_eq!(
        (row.hp, row.mp),
        (mates::MATE_FULL_POOL, mates::MATE_FULL_POOL)
    );
    {
        let d = t.mate.data();
        assert_eq!((d.unit.hp, d.unit.mp), (d.unit.max_hp, d.unit.max_mp));
    }

    // A living mate is left alone (no packets, no battle-pet task).
    assert!(!mates::revive_mate(&t.server, &t.mate));
    assert!(t.client.drain().is_empty());
}

#[tokio::test]
async fn a_stabler_revives_dead_mates_for_the_client_price() {
    let mut t = dead_mate();
    t.dismiss();
    t.player.data().unit.current_interaction_object = t.stabler.obj_id;
    let frames = t.revive_at(STABLER_OBJ).await;
    // Level 1: floor(0.5 + 1 + 0.5) = 2 copper, as a money task, then the item update, then the
    // MATE_RESURRECTED message.
    assert_eq!(t.player.data().money, 12_000 - 2);
    let item = t.item();
    assert_eq!(item.details.mate_dead, 0);
    assert!(item.dirty);
    let mut expect = PacketStream::new();
    aaemu_commons::Marshal::write(
        &crate::systems::economy::item_packets::SCItemTaskSuccessPacket {
            action: REPAIR_PETS_TASK,
            tasks: vec![crate::models::item_task::ItemTask::update(&item)],
            force_remove: Vec::new(),
        },
        &mut expect,
    );
    let o = ops(&frames);
    assert_eq!(
        o,
        vec![
            sc::SCItemTaskSuccessPacket,
            sc::SCItemTaskSuccessPacket,
            sc::SCErrorMsgPacket
        ]
    );
    assert_eq!(frames[1].1, expect.as_bytes());
    assert_eq!(error_codes(&frames), vec![error_message::MATE_RESURRECTED]);
    let row = t.row();
    assert_eq!(
        (row.hp, row.mp),
        (mates::MATE_FULL_POOL, mates::MATE_FULL_POOL)
    );

    // The revived mate can be summoned again, alive.
    mates::spawn_mount(&t.server, &t.player, t.item_id);
    let mate = mates::first_active_mate(&t.server, &t.player).expect("summoned");
    let d = mate.data();
    assert!(d.unit.hp > 0 && d.unit.hp == d.unit.max_hp);
    assert!(d.unit.mp > 0);
}

#[tokio::test]
async fn a_legacy_zero_hp_row_is_flagged_on_the_summon_attempt() {
    let mut t = dead_mate();
    t.dismiss();
    // Pre-flag state: zero HP row, flag not set.
    mates::update_owned_item(&t.server, Some(&t.player), t.item_id, |i| {
        i.details.mate_dead = 0;
    });
    mates::spawn_mount(&t.server, &t.player, t.item_id);
    let f = t.client.drain();
    assert_eq!(task_actions(&f), vec![MATE_DEATH_TASK]);
    assert_eq!(
        error_codes(&f),
        vec![error_message::MATE_CANNOT_SPAWN_WHILE_CORPSE]
    );
    assert_eq!(t.item().details.mate_dead, 1);
    assert!(t
        .server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(1)
        .is_empty());
}

#[tokio::test]
async fn mate_revive_is_refused_without_a_stabler_interaction_or_while_the_corpse_is_summoned() {
    let mut t = dead_mate();
    t.client.drain();
    // The corpse is still summoned.
    t.player.data().unit.current_interaction_object = t.stabler.obj_id;
    let f = t.revive_at(STABLER_OBJ).await;
    assert_eq!(
        error_codes(&f),
        vec![error_message::MATE_CANNOT_REPAIR_WHILE_CORPSE]
    );
    assert_eq!(t.item().details.mate_dead, 1);
    assert_eq!(t.player.data().money, 12_000);
    t.dismiss();

    // Not the current interaction object.
    t.player.data().unit.current_interaction_object = 0;
    assert!(t.revive_at(STABLER_OBJ).await.is_empty());
    // An unknown object.
    t.player.data().unit.current_interaction_object = 0x999;
    assert!(t.revive_at(0x999).await.is_empty());
    // An NPC that is not a stabler.
    let other = spawn_npc_at(&t.server, 0x301, MATE_NPC, 101.0, 100.0);
    t.client.drain();
    t.player.data().unit.current_interaction_object = other.obj_id;
    assert!(t.revive_at(other.obj_id).await.is_empty());
    // A stabler out of reach.
    t.player.data().unit.current_interaction_object = t.stabler.obj_id;
    t.stabler.data().unit.transform.set_xyz(160.0, 100.0, 100.0);
    let f = t.revive_at(STABLER_OBJ).await;
    assert_eq!(error_codes(&f), vec![error_message::TOO_FAR_AWAY]);
    t.stabler.data().unit.transform.set_xyz(102.0, 100.0, 100.0);

    // Not enough money.
    t.player.data().money = 1;
    let f = t.revive_at(STABLER_OBJ).await;
    assert_eq!(error_codes(&f), vec![error_message::NOT_ENOUGH_MONEY]);
    assert!(task_actions(&f).is_empty());

    assert_eq!(t.player.data().money, 1);
    assert_eq!(t.item().details.mate_dead, 1);
    assert_eq!(t.row().hp, 0);

    // Nothing dead (a second request after the revive) does nothing.
    t.player.data().money = 100;
    assert!(!t.revive_at(STABLER_OBJ).await.is_empty());
    assert_eq!(t.player.data().money, 98);
    assert!(t.revive_at(STABLER_OBJ).await.is_empty());
    assert_eq!(t.player.data().money, 98);
}

#[tokio::test]
async fn second_use_of_the_same_summon_item_dismisses_the_mate() {
    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    mates::spawn_mount(&server, &pa, item_id);
    let m = server.systems.vehicles.mates.get(0);
    let first = m.get_active_mates(1).pop().expect("first mate");
    let first_obj = first.obj_id;
    assert!(server.world.world(0).unwrap().mate(first_obj).is_some());
    // Live state the despawn must save.
    {
        let mut d = first.data();
        d.experience = 4321;
        d.unit.hp = 42;
    }
    a.drain();

    // Using the same summon item again dismisses the mate (the 1.2 toggle): no new
    // SCMateSpawned, and the row keeps the live state the despawn saved.
    mates::spawn_mount(&server, &pa, item_id);
    let frames = a.drain();
    assert!(m.get_active_mates(1).is_empty(), "the mate is dismissed");
    assert!(first.data().deleted);
    assert!(server.world.world(0).unwrap().mate(first_obj).is_none());
    assert!(ops(&frames).contains(&sc::SCUnitsRemovedPacket));
    assert!(!ops(&frames).contains(&sc::SCMateSpawnedPacket));
    let row = server
        .systems
        .vehicles
        .mates
        .character_mates(1)
        .unwrap()
        .get(item_id)
        .cloned()
        .expect("mate row");
    assert_eq!((row.xp, row.hp), (4321, 42));

    // A third use summons it again (the toggle back on), and logout still clears the entry.
    mates::spawn_mount(&server, &pa, item_id);
    let second = m.get_active_mates(1).pop().expect("re-summoned mate");
    let (exp, hp) = {
        let d = second.data();
        (d.experience, d.unit.hp)
    };
    assert_eq!((exp, hp), (4321, 42));
    assert!(server.world.world(0).unwrap().mate(second.obj_id).is_some());
    mates::remove_and_despawn_all_active_owned_mates(&server, 0, 1);
    assert!(server.world.world(0).unwrap().mate(second.obj_id).is_none());
    assert!(m.get_active_mates(1).is_empty());
}

/// The toggle is per summon item, not per template: a second whistle (a different item id
/// bound to the same mate template) is a different mate and replaces the first.
#[tokio::test]
async fn a_second_whistle_of_the_same_template_replaces_the_mate() {
    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, first_item) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    let second_item = give_item(&server, &pa, 0x9003, MATE_ITEM);
    mates::spawn_mount(&server, &pa, first_item);
    let m = server.systems.vehicles.mates.get(0);
    let first = m.get_active_mates(1).pop().expect("first mate");
    a.drain();

    mates::spawn_mount(&server, &pa, second_item);
    let frames = a.drain();
    let active = m.get_active_mates(1);
    assert_eq!(active.len(), 1);
    let second = active[0].clone();
    assert_eq!(second.data().item_id, second_item);
    assert!(first.data().deleted);
    assert!(ops(&frames).contains(&sc::SCMateSpawnedPacket));
    assert!(ops(&frames).contains(&sc::SCUnitsRemovedPacket));
    let in_world = server
        .world
        .world(0)
        .unwrap()
        .mate(second.obj_id)
        .expect("the new whistle's mate is spawned");
    assert!(Arc::ptr_eq(&in_world, &second));
}

/// A summon that is refused (a dead mate) keeps the current mate out: the despawn runs only
/// after the checks.
#[tokio::test]
async fn a_refused_summon_keeps_the_current_mate() {
    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, mount_item) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    let pet_item = give_item(&server, &pa, 0x9004, PET_ITEM);
    {
        let bag = pa
            .data()
            .inventory
            .get(slot_type::INVENTORY)
            .cloned()
            .unwrap();
        let mut g = bag.lock().unwrap();
        g.get_item_by_id_mut(pet_item).unwrap().details.mate_dead = 1;
    }
    mates::spawn_mount(&server, &pa, mount_item);
    let m = server.systems.vehicles.mates.get(0);
    let mount = m.get_active_mates(1).pop().expect("mount");
    a.drain();

    mates::spawn_mount(&server, &pa, pet_item);
    let frames = a.drain();
    assert_eq!(
        error_codes(&frames),
        vec![error_message::MATE_CANNOT_SPAWN_WHILE_CORPSE]
    );
    assert_eq!(m.get_active_mates(1).len(), 1);
    assert!(!mount.data().deleted);
    assert!(server.world.world(0).unwrap().mate(mount.obj_id).is_some());
}

/// The 1.2 client has one player pet (`X2Mate:IsPlayerPetExists` / `DismissPet`, one pet
/// frame; `SCMateSpawnedPacket` carries no slot or kind), so a mount and a battle pet cannot be
/// out together: the second summon replaces the first and the new mate spawns.
#[tokio::test]
async fn a_second_mate_of_another_kind_replaces_the_first_and_spawns() {
    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, mount_item) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    let pet_item = give_item(&server, &pa, 0x9000, PET_ITEM);
    mates::spawn_mount(&server, &pa, mount_item);
    let m = server.systems.vehicles.mates.get(0);
    let mount = m.get_active_mates(1).pop().expect("mount");
    let mount_obj = mount.obj_id;
    assert!(server.world.world(0).unwrap().mate(mount_obj).is_some());
    a.drain();

    mates::spawn_mount(&server, &pa, pet_item);
    let frames = a.drain();
    let active = m.get_active_mates(1);
    assert_eq!(active.len(), 1);
    let pet = active[0].clone();
    assert_eq!(pet.data().item_id, pet_item);
    assert!(!Arc::ptr_eq(&mount, &pet));
    assert!(mount.data().deleted);
    let in_world = server
        .world
        .world(0)
        .unwrap()
        .mate(pet.obj_id)
        .expect("the pet is spawned");
    assert!(Arc::ptr_eq(&in_world, &pet));
    // The client creates the new mate from SCMateSpawned (tl u16, id u32, item u64).
    let spawned = body(&frames, sc::SCMateSpawnedPacket);
    assert_eq!(
        u64::from_le_bytes(spawned[6..14].try_into().unwrap()),
        pet_item
    );
    // The old mount's row was saved before its despawn.
    let rows = server.systems.vehicles.mates.character_mates(1).unwrap();
    assert!(rows.get(mount_item).is_some());
}

/// Seats come from the model's attach points (`model_bindings`), the list the 1.2 client's seat
/// test scans (`FUN_390c9f70`), not the C#'s hard-coded driver + passenger0.
#[tokio::test]
async fn mate_passenger_seats_come_from_the_models_attach_points() {
    let server = server_with(data());
    let a = TestClient::new(&server, 1, 1);
    let (pa, horse_item) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    let pet_item = give_item(&server, &pa, 0x9001, PET_ITEM);
    let elephant_item = give_item(&server, &pa, 0x9002, ELEPHANT_ITEM);
    let m = server.systems.vehicles.mates.get(0);
    let seats =
        |mate: &MateRef| -> Vec<u8> { mate.data().passengers.iter().map(|(p, _)| *p).collect() };

    mates::spawn_mount(&server, &pa, horse_item);
    let horse = m.get_active_mates(1).pop().expect("horse");
    assert_eq!(
        seats(&horse),
        vec![attach_point::DRIVER, attach_point::PASSENGER0]
    );

    // A battle pet's model declares no attach points: it has no seats.
    mates::spawn_mount(&server, &pa, pet_item);
    let pet = m.get_active_mates(1).pop().expect("pet");
    assert!(pet.data().passengers.is_empty());
    let pet_tl = u32::from(pet.data().unit.tl_id);
    mates::mount_mate(&server, &pa, pet_tl, attach_point::DRIVER, 1);
    assert!(!pa.data().is_riding);

    // The elephant model declares driver + four passenger seats.
    mates::spawn_mount(&server, &pa, elephant_item);
    let elephant = m.get_active_mates(1).pop().expect("elephant");
    assert_eq!(
        seats(&elephant),
        vec![
            attach_point::DRIVER,
            attach_point::PASSENGER0,
            attach_point::PASSENGER1,
            attach_point::PASSENGER2,
            attach_point::PASSENGER3
        ]
    );
    let elephant_tl = u32::from(elephant.data().unit.tl_id);
    mates::mount_mate(&server, &pa, elephant_tl, attach_point::PASSENGER1, 1);
    assert!(pa.data().is_riding);
    assert_eq!(pa.data().attached_point, attach_point::PASSENGER1);
}

#[tokio::test]
async fn logout_cleanup_only_removes_the_character_active_list() {
    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let (pa, a_item) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    // B's character id equals A's obj id: the old cleanup removed the active list by that key and
    // would have hit B's entry.
    let (pb, b_item) = player_with_item(&server, &b, PA, PB, (110.0, 110.0), MATE_ITEM);
    mates::spawn_mount(&server, &pa, a_item);
    mates::spawn_mount(&server, &pb, b_item);
    let m = server.systems.vehicles.mates.get(0);
    let mate_b = m.get_active_mates(PA).pop().expect("B's active mate");
    assert_eq!(m.get_active_mates(1).len(), 1);
    a.drain();
    b.drain();
    // Cleaning up A (char id 1) must not touch B's entry.
    mates::remove_and_despawn_all_active_owned_mates(&server, 0, 1);
    assert!(m.get_active_mates(1).is_empty());
    assert_eq!(m.get_active_mates(PA).len(), 1);
    assert!(server.world.world(0).unwrap().mate(mate_b.obj_id).is_some());
    // Repeating the cleanup is harmless and still leaves B alone.
    mates::remove_and_despawn_all_active_owned_mates(&server, 0, 1);
    assert_eq!(m.get_active_mates(PA).len(), 1);
    // B's own cleanup removes exactly its entry and despawns its mate.
    mates::remove_and_despawn_all_active_owned_mates(&server, 0, PA);
    assert!(m.get_active_mates(PA).is_empty());
    assert!(server.world.world(0).unwrap().mate(mate_b.obj_id).is_none());
}

#[tokio::test]
async fn pet_exp_and_level_up() {
    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    spawn_player(&server, &b, test_character(2, "B", 110.0, 110.0), PB);
    mates::spawn_mount(&server, &pa, item_id);
    let mate = server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(1)
        .pop()
        .unwrap();
    a.drain();
    b.drain();

    // 300 exp: no level up (level 2 needs 500 mate exp).
    mates::mate_xp_update_task(&server, &pa, &mate);
    let f = a.drain();
    let mut exp = bc(mate.obj_id);
    exp.extend(300i32.to_le_bytes());
    exp.push(0);
    assert_eq!(body(&f, sc::SCExpChangedPacket), exp);
    assert!(ops(&f).contains(&sc::SCChatMessagePacket));
    assert!(!ops(&f).contains(&sc::SCLevelChangedPacket));
    assert_eq!(mate.data().experience, 300);

    // +300 more: level 2, broadcast to everybody around.
    mates::add_exp(&server, &mate, 300);
    let f = a.drain();
    let mut lvl = bc(mate.obj_id);
    lvl.push(2);
    assert_eq!(body(&f, sc::SCLevelChangedPacket), lvl);
    assert_eq!(body(&b.drain(), sc::SCLevelChangedPacket), lvl);
    // The summon item and the mate row follow.
    let item = mates::owned_item(&server, &pa, item_id).unwrap();
    assert_eq!((item.details.mate_exp, item.details.mate_level), (600, 2));
    assert!(item.dirty);
    let db = server.systems.vehicles.mates.character_mates(1).unwrap();
    assert_eq!((db.mates[0].xp, db.mates[0].level), (600, 2));

    // Over the cap: the overflow is cut at the max level (3 = 1000 mate exp).
    mates::add_exp(&server, &mate, 5000);
    assert_eq!(mate.data().unit.level, 3);
    assert_eq!(mate.data().experience, 1000);
    a.drain();
    // Max level: nothing happens.
    mates::add_exp(&server, &mate, 10);
    assert!(a.drain().is_empty());
    assert!(mates::is_max_level(&server, &mate.data()));
}

fn slave_data() -> GameData {
    data()
}

#[tokio::test]
async fn summon_item_cast_rejects_unowned_items_then_creates_and_binds_vehicle() {
    use crate::systems::skills::data::effects::{EffectTemplate, SpecialEffect};
    use crate::systems::skills::data::{EffectKind, SkillTemplate};
    use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
    let mut d = slave_data();
    let skill_id = 58_002;
    d.items.templates.get_mut(&SLAVE_ITEM).unwrap().use_skill_id = skill_id;
    d.systems.skills.add_effect(
        EffectKind::SpecialEffect,
        EffectTemplate::Special(SpecialEffect {
            id: 1,
            special_effect_type_id: 60,
            value1: 0,
            value2: 0,
            value3: 0,
            value4: 0,
        }),
        1,
    );
    d.systems.skills.add_skill(SkillTemplate {
        id: skill_id,
        target_type: 0, // Summon items target their owner.
        level_rule_no_consideration: true,
        effects: vec![crate::systems::skills::tests::skill_effect(
            1,
            EffectKind::SpecialEffect,
            1,
        )],
        ..SkillTemplate::default()
    });
    let server = server_with(d);
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), SLAVE_ITEM);
    // Runtime inventory loading also registers containers with ItemManager, which owns
    // item-cast authorization. The generic vehicle fixture only populates the bag.
    let bag = pa
        .data()
        .inventory
        .get(slot_type::INVENTORY)
        .unwrap()
        .clone();
    bag.lock().unwrap().items[0].owner_id = u64::from(pa.character_id);
    server.items.add_container(bag);
    a.conn.active_char = Some(pa.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    a.drain();
    let packet = |item_id| CSStartSkillPacket {
        skill_id,
        caster: SkillCaster::Item {
            obj_id: PA,
            item_id,
            item_template_id: SLAVE_ITEM,
            type1: 0,
            type2: 0,
        },
        target: SkillCastTarget::Unit(PA),
        flag: 0,
        skill_object: SkillObject::None,
    };
    start_skill(&server, &mut a.conn, packet(item_id + 100));
    assert!(slaves::get_active_slave_by_owner_obj_id(&world, PA).is_none());
    start_skill(&server, &mut a.conn, packet(item_id));
    // The special effect queues the asynchronous saved-vehicle lookup before creation.
    let slave = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(s) = slaves::get_active_slave_by_owner_obj_id(&world, PA) {
                break s;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("summon skill must create the vehicle");
    let frames = a.drain();
    assert!(ops(&frames).contains(&sc::SCSlaveCreatedPacket));
    assert!(ops(&frames).contains(&sc::SCSlaveStatePacket));
    assert_eq!(slave.data().unit.template_id, SLAVE_TEMPLATE);
    let tl = slave.data().unit.tl_id;
    stand_next_to(&world, &pa, &WorldObject::from(&slave));
    slaves::bind_slave_by_tl(&server, &pa, u32::from(tl));
    assert_eq!(pa.data().unit.transform.parent_obj_id, slave.obj_id);
    assert!(ops(&a.drain()).contains(&sc::SCSlaveBoundPacket));
    // Go through the real despawn packet while still bound. A temporary player guard in
    // a call argument used to survive into delete/unbind and deadlock the whole world.
    use crate::net::registry::ClientPacket;
    let (done, result) = std::sync::mpsc::channel();
    let task_server = server.clone();
    let task_player = pa.clone();
    let obj_id = slave.obj_id;
    std::thread::spawn(move || {
        let mut client = TestClient::new(&task_server, 1, 1);
        client.conn.active_char = Some(task_player);
        let packet = super::packets::CSDespawnSlavePacket {
            slave_obj_id: obj_id,
        };
        let status = futures::executor::block_on(packet.handle(&task_server, &mut client.conn));
        done.send(status).unwrap();
    });
    result
        .recv_timeout(Duration::from_secs(2))
        .expect("despawning a mounted vehicle must not retain the player lock")
        .unwrap();
    assert_eq!(pa.data().unit.transform.parent_obj_id, 0);
    assert!(world.slave(slave.obj_id).is_none());
}

#[tokio::test]
async fn slave_summon_bind_drive_and_despawn() {
    let server = server_with(slave_data());
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), SLAVE_ITEM);
    spawn_player(&server, &b, test_character(2, "B", 110.0, 110.0), PB);
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    a.drain();
    b.drain();

    let item = mates::owned_item(&server, &pa, item_id).unwrap();
    let slave = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SLAVE_TEMPLATE,
        Some(item),
        false,
        None,
        None,
    )
    .expect("slave");
    let (tl, db_id) = {
        let d = slave.data();
        (d.unit.tl_id, d.id)
    };
    assert!(db_id > 0, "a player slave gets a db id");
    let fa = a.drain();
    let o = ops(&fa);
    assert_eq!(o[0], sc::SCItemTaskSuccessPacket);
    assert!(o.contains(&sc::SCSlaveCreatedPacket));
    assert!(o.contains(&sc::SCSlaveStatePacket));
    assert_eq!(*o.last().unwrap(), sc::SCMySlavePacket);
    let mut created = bc(PA);
    created.extend(tl.to_le_bytes());
    created.extend(bc(slave.obj_id));
    created.push(0);
    created.extend(0i64.to_le_bytes());
    created.extend([2, 0, b'P', b'1']);
    assert_eq!(body(&fa, sc::SCSlaveCreatedPacket), created);
    let fb = b.drain();
    assert_eq!(body(&fb, sc::SCSlaveCreatedPacket), created);
    assert!(ops(&fb).contains(&sc::SCSlaveStatePacket));
    // Summon item details: type 2, the slave's db id.
    let item = mates::owned_item(&server, &pa, item_id).unwrap();
    assert_eq!(
        (item.details.slave_type, item.details.slave_db_id),
        (2, db_id)
    );
    // Spawned 5 m (SpawnYOffset clamped to 5..50) in front, 90 degrees CCW of the owner.
    let t = slave.data().unit.transform;
    assert_eq!((t.x, t.y), (100.0, 105.0));
    assert!((t.rot_z - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
    assert_eq!(
        slaves::get_active_slave_by_owner_obj_id(&world, PA).map(|s| s.obj_id),
        Some(slave.obj_id)
    );

    // Bind as driver: SCUnitAttached + SCSlaveBound to everybody around.
    stand_next_to(&world, &pa, &WorldObject::from(&slave));
    slaves::bind_slave_by_tl(&server, &pa, u32::from(tl));
    for c in [&mut a, &mut b] {
        let f = c.drain();
        assert_eq!(
            ops(&f),
            vec![sc::SCUnitAttachedPacket, sc::SCSlaveBoundPacket]
        );
        let mut bound = 1u32.to_le_bytes().to_vec();
        bound.extend(bc(slave.obj_id));
        assert_eq!(body(&f, sc::SCSlaveBoundPacket), bound);
    }
    assert_eq!(
        slave.data().attached_characters,
        vec![(attach_point::DRIVER, PA)]
    );
    assert_eq!(pa.data().unit.transform.parent_obj_id, slave.obj_id);

    // Drive (VehicleMoveType): the move is applied and broadcast.
    let mv = MoveType::new(MoveKind::Vehicle(VehicleMove {
        x: 120.0,
        y: 125.0,
        z: 100.0,
        ..VehicleMove::default()
    }));
    a.conn.active_char = Some(pa.clone());
    crate::world::session::move_unit(&server, &mut a.conn, slave.obj_id, mv.clone()).await;
    let fb = b.drain();
    let mut expected = PacketStream::new();
    crate::models::movement::write_sc_one_unit_movement(&mut expected, slave.obj_id, &mv);
    assert_eq!(body(&fb, sc::SCOneUnitMovementPacket), expected.as_bytes());
    let t = slave.data().unit.transform;
    assert_eq!((t.x, t.y, t.z), (120.0, 125.0, 100.0));
    // The driver follows (child of the slave).
    let w = world.world_pos_rot(&WorldObject::from(&pa));
    assert_eq!((w.x, w.y), (120.0, 125.0));
    a.drain();

    // Discard (CSDiscardSlavePacket): SCUnitDetached.
    slaves::unbind_slave(
        &server,
        &pa,
        u32::from(tl),
        slaves::attach_reason::SLAVE_BINDING,
    );
    let mut det = bc(PA);
    det.push(slaves::attach_reason::SLAVE_BINDING);
    assert_eq!(body(&b.drain(), sc::SCUnitDetachedPacket), det);
    assert!(slave.data().attached_characters.is_empty());
    assert_eq!(pa.data().unit.transform.parent_obj_id, 0);
    a.drain();

    // Un-summon (CSDestroySlavePacket): despawn effect packets, removal scheduled.
    slaves::remove_active_slave(&server, &pa, tl, false);
    let fb = b.drain();
    assert_eq!(
        ops(&fb),
        vec![sc::SCSlaveDespawnPacket, sc::SCSlaveRemovedPacket]
    );
    let mut removed = bc(PA);
    removed.extend(tl.to_le_bytes());
    assert_eq!(body(&fb, sc::SCSlaveRemovedPacket), removed);
    assert!(
        world.slave(slave.obj_id).is_none(),
        "removed from the object table"
    );
    let sm = server.systems.npcs.spawns.get(0).unwrap();
    assert!(sm
        .pending_despawns()
        .iter()
        .any(|(id, at)| *id == slave.obj_id && at.is_some()));
    // Still visible until the despawn tick (portal time + 0.5 s).
    assert!(slave.core().is_visible());
    crate::systems::npcs::spawn_manager::advance_test_clock(Duration::from_secs(3));
    sm.check_respawns_tick(&server);
    assert!(!slave.core().is_visible());
    assert!(ops(&b.drain()).contains(&sc::SCUnitsRemovedPacket));
}

/// A seat another character already holds answers `slave_already_has_master` 0x61 with
/// `isNotify = false` (the form that clears the client's pending bind request; the C# returned
/// silently, but that leaves the client stuck on "already requested"): no attach, the occupant
/// stays, and the seat frees when the occupant leaves. The check and the reservation happen
/// under the slave's one lock, so the seat can never be handed to two characters.
#[tokio::test]
async fn a_taken_seat_answers_a_clearing_error_and_keeps_the_occupant() {
    use crate::models::slave::Slave;
    use crate::world::Entity;

    let server = server_with(slave_data());
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let mut c = TestClient::new(&server, 3, 3);
    let pa = spawn_player(&server, &a, test_character(1, "Driver", 100.0, 100.0), PA);
    let pb = spawn_player(
        &server,
        &b,
        test_character(2, "Passenger", 101.0, 100.0),
        PB,
    );
    let pc = spawn_player(
        &server,
        &c,
        test_character(3, "Third", 102.0, 100.0),
        0x00F0_0003,
    );
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    c.conn.active_char = Some(pc.clone());
    let world = server.world.world(0).unwrap();
    let mut cart = Slave::new(0, SLAVE_TEMPLATE);
    cart.unit.hp = 1000;
    cart.unit.transform.set_xyz(103.0, 100.0, 100.0);
    cart.unit.tl_id = 3;
    cart.summoner_obj_id = PA;
    cart.summoner_id = pa.character_id;
    let cart = Entity::new(0x300, cart);
    world.spawn(&cart, &server.view_ctx());
    a.drain();
    b.drain();
    c.drain();

    slaves::bind_slave(
        &server,
        &pa,
        cart.obj_id,
        attach_point::DRIVER,
        7,
        Some(4.0),
    );
    assert_eq!(
        cart.data().attached_characters,
        vec![(attach_point::DRIVER, PA)]
    );
    a.drain();
    b.drain();
    c.drain();

    // The driver seat is taken: B is not attached and gets the clearing 0x61.
    slaves::bind_slave(
        &server,
        &pb,
        cart.obj_id,
        attach_point::DRIVER,
        7,
        Some(4.0),
    );
    assert_eq!(
        body(&b.drain(), sc::SCErrorMsgPacket),
        vec![0x61, 0, 0x61, 0, 0, 0, 0, 0, 0],
        "a taken seat must answer the clearing error, not attach"
    );
    assert_eq!(
        cart.data().attached_characters,
        vec![(attach_point::DRIVER, PA)]
    );
    assert_eq!(pb.data().attached_point, attach_point::NONE);
    assert_eq!(pb.data().unit.transform.parent_obj_id, 0);
    assert!(a.drain().is_empty(), "a refused bind attaches nobody");

    // The free passenger seat still takes B at point 2, with the exact attach packet.
    slaves::bind_slave(
        &server,
        &pb,
        cart.obj_id,
        attach_point::PASSENGER0,
        7,
        Some(3.0),
    );
    assert_eq!(
        cart.data().attached_characters,
        vec![(attach_point::DRIVER, PA), (attach_point::PASSENGER0, PB)]
    );
    let mut attached = bc(PB);
    attached.push(attach_point::PASSENGER0);
    attached.extend(bc(cart.obj_id));
    attached.push(7);
    assert_eq!(body(&b.drain(), sc::SCUnitAttachedPacket), attached);
    assert!(ops(&a.drain()).contains(&sc::SCUnitAttachedPacket));
    c.drain();

    // A third character on the taken passenger seat gets the clearing 0x61, not silence.
    slaves::bind_slave(
        &server,
        &pc,
        cart.obj_id,
        attach_point::PASSENGER0,
        7,
        Some(3.0),
    );
    assert_eq!(
        body(&c.drain(), sc::SCErrorMsgPacket),
        vec![0x61, 0, 0x61, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(cart.data().attached_characters.len(), 2);

    // Leaving frees the seat for the next character.
    slaves::unbind_slave(&server, &pa, 3, slaves::attach_reason::SLAVE_BINDING);
    slaves::bind_slave(
        &server,
        &pc,
        cart.obj_id,
        attach_point::DRIVER,
        7,
        Some(4.0),
    );
    assert_eq!(
        cart.data().attached_characters,
        vec![
            (attach_point::PASSENGER0, PB),
            (attach_point::DRIVER, 0x00F0_0003)
        ]
    );

    // The C# owners-mark refusal is the only taken-driver error: a non-summoner driver bind
    // on a marked slave answers `slave_already_has_master` 0x61.
    slaves::unbind_slave(&server, &pc, 3, slaves::attach_reason::SLAVE_BINDING);
    slaves::unbind_slave(&server, &pb, 3, slaves::attach_reason::SLAVE_BINDING);
    let mark = crate::models::buff::Buff::new(
        crate::data::skills::BuffTemplate {
            id: slaves::buff::OWNERS_MARK,
            kind: crate::data::skills::BuffKind::Good,
            duration: 0,
            level_duration: 0,
            tick: 0.0,
            stack_rule: 0,
            max_stack: 1,
            init_min_charge: 0,
            save_rule_id: 0,
            real_time: false,
        },
        crate::models::buff::SkillCaster { kind: 0, obj_id: 0 },
        None,
        0,
    );
    cart.data().unit.buffs.effects.push(mark);
    slaves::bind_slave(
        &server,
        &pb,
        cart.obj_id,
        attach_point::DRIVER,
        7,
        Some(4.0),
    );
    assert_eq!(
        body(&b.drain(), sc::SCErrorMsgPacket),
        vec![0x61, 0, 0x61, 0, 0, 0, 0, 0, 0]
    );
    assert!(cart.data().attached_characters.is_empty());
}

const PASSENGER_SKILL: u32 = 13_863;
const PASSENGER_EFFECT: u32 = 2_378;

/// The real 1.2 passenger-seat data: skill 13863 carries effect 8869 -> special effect 2378,
/// type 53 `AttachTo`, with value1 2 (passenger point) and value2..4 0. This fixture keeps
/// value1 2 and sets value2 to the decoy 9, so a regression that read value2 would seat point 9.
fn passenger_data() -> GameData {
    use crate::systems::skills::data::effects::SpecialEffect;
    use crate::systems::skills::data::{EffectKind, EffectTemplate, SkillTemplate};
    let mut d = slave_data();
    d.systems.skills.add_effect(
        EffectKind::SpecialEffect,
        EffectTemplate::Special(SpecialEffect {
            id: PASSENGER_EFFECT,
            special_effect_type_id: 53,
            value1: attach_point::PASSENGER0.into(),
            value2: 9,
            value3: 0,
            value4: 0,
        }),
        PASSENGER_EFFECT,
    );
    d.systems.skills.add_skill(SkillTemplate {
        id: PASSENGER_SKILL,
        target_type: 5, // AnyUnit
        target_selection: 2,
        target_relation: 0, // Any
        min_range: 0,
        max_range: 4,
        default_gcd: false,
        level_rule_no_consideration: true,
        effects: vec![crate::systems::skills::tests::skill_effect(
            PASSENGER_EFFECT,
            EffectKind::SpecialEffect,
            PASSENGER_EFFECT,
        )],
        ..crate::systems::skills::tests::skill_template(PASSENGER_SKILL)
    });
    // `SkillData::load` marks the 1.2 AttachTo skill (special type 53) as client-sourced, so a
    // character that never learned it may cast it from the slave action bar.
    d.skills.add_client_sourced_skill(PASSENGER_SKILL);
    d
}

/// The Red Farm Hauler (slave 146, model 1553) of the 2026-11-01 live log: `model_bindings`
/// declares the passenger seat at point 2, `slave_attach_points.json` locates only its cannon
/// points 9-14 on the rear flatbed, and the model has no ship model. The positions are the real
/// rows.
fn hauler_data() -> GameData {
    let mut d = passenger_data();
    d.systems.vehicles.slave_templates.insert(
        146,
        SlaveTemplate {
            id: 146,
            name: "Red Farm Hauler".into(),
            model_id: 1553,
            slave_kind: super::data::slave_kind::MACHINE,
            ..SlaveTemplate::default()
        },
    );
    d.systems
        .vehicles
        .model_bindings
        .insert(1553, vec![1, 2, 9, 10, 11, 12, 13, 14, 35]);
    let at = |x: f32, y: f32, z: f32| WorldSpawnPosition {
        x,
        y,
        z,
        ..WorldSpawnPosition::default()
    };
    d.systems.vehicles.attach_points.insert(
        1553,
        vec![
            (9, at(-0.55, -2.0, 1.15)),
            (10, at(0.55, -2.0, 1.15)),
            (11, at(0.55, -3.2, 1.15)),
            (12, at(-0.55, -3.2, 1.15)),
            (13, at(0.55, -2.6, 2.15)),
            (14, at(-0.55, -2.6, 2.15)),
        ],
    );
    d
}

/// Skill 13863 (`Take the Passenger Seat`) seats the caster at point 2 through the real
/// `CSStartSkill` handler and `AttachTo` special effect; a second character on the same seat
/// gets the clearing `slave_already_has_master` 0x61 (the C# was silent, which left the client's
/// pending bind request set).
#[tokio::test]
async fn passenger_skill_13863_seats_point_two_and_a_taken_seat_answers_a_clearing_error() {
    use crate::models::slave::Slave;
    use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
    use crate::world::Entity;

    let server = server_with(passenger_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let mut c = TestClient::new(&server, 3, 3);
    let pa = spawn_player(&server, &a, test_character(1, "Driver", 100.0, 100.0), PA);
    let pb = spawn_player(
        &server,
        &b,
        test_character(2, "Passenger", 101.0, 100.0),
        PB,
    );
    let pc = spawn_player(
        &server,
        &c,
        test_character(3, "Third", 102.0, 100.0),
        0x00F0_0003,
    );
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    c.conn.active_char = Some(pc.clone());
    for p in [&pb, &pc] {
        let mut d = p.data();
        d.unit.hp = 1000;
        d.unit.max_hp = 1000;
    }
    let world = server.world.world(0).unwrap();
    let mut cart = Slave::new(0, SLAVE_TEMPLATE);
    cart.unit.hp = 1000;
    cart.unit.transform.set_xyz(103.0, 100.0, 100.0);
    cart.unit.tl_id = 3;
    cart.summoner_obj_id = PA;
    cart.summoner_id = pa.character_id;
    let cart = Entity::new(0x300, cart);
    world.spawn(&cart, &server.view_ctx());
    a.drain();
    b.drain();
    c.drain();

    let cast = |client: &mut TestClient, caster: u32, target: u32| {
        start_skill(
            &server,
            &mut client.conn,
            CSStartSkillPacket {
                skill_id: PASSENGER_SKILL,
                caster: SkillCaster::Unit(caster),
                target: SkillCastTarget::Unit(target),
                flag: 0,
                skill_object: SkillObject::None,
            },
        );
    };

    cast(&mut b, PB, cart.obj_id);
    assert_eq!(
        cart.data().attached_characters,
        vec![(attach_point::PASSENGER0, PB)],
        "AttachTo must use value1 (point 2), not the decoy value2 (point 9)"
    );
    assert_eq!(pb.data().attached_point, attach_point::PASSENGER0);
    assert_eq!(pb.data().unit.transform.parent_obj_id, cart.obj_id);
    let mut attached = bc(PB);
    attached.push(attach_point::PASSENGER0);
    attached.extend(bc(cart.obj_id));
    attached.push(slaves::attach_reason::NEW_MASTER);
    assert_eq!(body(&b.drain(), sc::SCUnitAttachedPacket), attached);
    a.drain();
    c.drain();

    // The passenger seat is now taken: the second cast attaches nobody and answers the clearing
    // 0x61 (`isNotify = false`).
    cast(&mut c, 0x00F0_0003, cart.obj_id);
    let fc = c.drain();
    assert!(!ops(&fc).contains(&sc::SCUnitAttachedPacket));
    assert_eq!(
        body(&fc, sc::SCErrorMsgPacket),
        vec![0x61, 0, 0x61, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(
        cart.data().attached_characters,
        vec![(attach_point::PASSENGER0, PB)]
    );
    assert_eq!(pc.data().unit.transform.parent_obj_id, 0);
    b.drain();
}

/// A dead slave (a wreck or a cart at 0 hp before it despawns) takes no passenger: 13863's
/// `AttachTo` reaches `bind_slave` without the 12076 path's liveness check.
#[tokio::test]
async fn the_passenger_skill_does_not_seat_on_a_dead_slave() {
    use crate::models::slave::Slave;
    use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
    use crate::world::Entity;

    let server = server_with(passenger_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut b = TestClient::new(&server, 2, 2);
    let pb = spawn_player(
        &server,
        &b,
        test_character(2, "Passenger", 101.0, 100.0),
        PB,
    );
    b.conn.active_char = Some(pb.clone());
    {
        let mut d = pb.data();
        d.unit.hp = 1000;
        d.unit.max_hp = 1000;
    }
    let world = server.world.world(0).unwrap();
    let mut cart = Slave::new(0, SLAVE_TEMPLATE);
    cart.unit.transform.set_xyz(103.0, 100.0, 100.0);
    cart.unit.tl_id = 3;
    cart.unit.hp = 0;
    let cart = Entity::new(0x300, cart);
    world.spawn(&cart, &server.view_ctx());
    b.drain();

    start_skill(
        &server,
        &mut b.conn,
        CSStartSkillPacket {
            skill_id: PASSENGER_SKILL,
            caster: SkillCaster::Unit(PB),
            target: SkillCastTarget::Unit(cart.obj_id),
            flag: 0,
            skill_object: SkillObject::None,
        },
    );
    assert!(cart.data().attached_characters.is_empty());
    assert_eq!(pb.data().unit.transform.parent_obj_id, 0);
    assert!(!ops(&b.drain()).contains(&sc::SCUnitAttachedPacket));
}

/// The 1.2 client only offers / accepts a seat its model declares (`model_bindings`): the Farm
/// Wagon (slave 60, model 1008) has no point 2, so 13863 is not offered and a forged bind there
/// answers the clearing 0x60; the Red Farm Hauler (slave 146, model 1553) has one and takes the
/// passenger. A point the model does declare (a ship cannon's point 9) still binds.
#[tokio::test]
async fn model_bindings_decide_which_seats_a_slave_offers_and_accepts() {
    use crate::models::slave::Slave;
    use crate::net::registry::ClientPacket;
    use crate::systems::npcs::npc::packets::{
        CSStartInteractionPacket, SCNpcInteractionSkillListPacket,
    };
    use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
    use crate::systems::vehicles::data::SlaveTemplate;
    use crate::world::Entity;
    use aaemu_commons::{Marshal, PacketStream};

    let mut d = passenger_data();
    d.systems.vehicles.slave_templates.insert(
        60,
        SlaveTemplate {
            id: 60,
            name: "Farm Wagon".into(),
            model_id: 1008,
            slave_kind: super::data::slave_kind::MACHINE,
            ..SlaveTemplate::default()
        },
    );
    d.systems.vehicles.slave_templates.insert(
        146,
        SlaveTemplate {
            id: 146,
            name: "Red Farm Hauler".into(),
            model_id: 1553,
            slave_kind: super::data::slave_kind::MACHINE,
            ..SlaveTemplate::default()
        },
    );
    d.systems.vehicles.slave_templates.insert(
        23,
        SlaveTemplate {
            id: 23,
            name: "Ship".into(),
            model_id: 512,
            slave_kind: super::data::slave_kind::BIG_SAILING_SHIP,
            ..SlaveTemplate::default()
        },
    );
    d.systems
        .vehicles
        .model_bindings
        .insert(1008, vec![attach_point::DRIVER, 35, 9, 10, 11, 12]);
    d.systems.vehicles.model_bindings.insert(
        1553,
        vec![
            attach_point::DRIVER,
            attach_point::PASSENGER0,
            35,
            9,
            10,
            11,
            12,
            13,
            14,
            15,
        ],
    );
    d.systems
        .vehicles
        .model_bindings
        .insert(512, vec![attach_point::DRIVER, attach_point::PASSENGER0, 9]);
    let server = server_with(d);
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let pa = spawn_player(&server, &a, test_character(1, "Driver", 100.0, 100.0), PA);
    let pb = spawn_player(
        &server,
        &b,
        test_character(2, "Passenger", 101.0, 100.0),
        PB,
    );
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    for p in [&pa, &pb] {
        let mut c = p.data();
        c.unit.hp = 1000;
        c.unit.max_hp = 1000;
    }
    let world = server.world.world(0).unwrap();
    let spawn = |template: u32, model: u32, obj_id: u32, tl: u16, x: f32| {
        let mut s = Slave::new(0, template);
        s.unit.hp = 1000;
        s.unit.max_hp = 1000;
        s.unit.model_id = model;
        s.unit.tl_id = tl;
        s.unit.transform.set_xyz(x, 100.0, 100.0);
        s.summoner_obj_id = PA;
        s.summoner_id = pa.character_id;
        let s = Entity::new(obj_id, s);
        world.spawn(&s, &server.view_ctx());
        s
    };
    let interaction = |slave_obj: u32, skills: Vec<u32>| {
        let p = SCNpcInteractionSkillListPacket {
            npc_obj_id: slave_obj,
            obj_id: PB,
            extra_info: 1,
            pick_id: 0,
            mouse_button: 3,
            modifier_keys: 0,
            skill_list: skills,
        };
        let mut s = PacketStream::new();
        p.write(&mut s);
        vec![(sc::SCNpcInteractionSkillListPacket, s.into_bytes())]
    };
    let req = |slave_obj: u32| CSStartInteractionPacket {
        npc_obj_id: slave_obj,
        obj_id: PB,
        extra_info: 1,
        pick_id: 0,
        mouse_button: 3,
        modifier_keys: 0,
    };
    let cast = |client: &mut TestClient, caster: u32, target: u32| {
        start_skill(
            &server,
            &mut client.conn,
            CSStartSkillPacket {
                skill_id: PASSENGER_SKILL,
                caster: SkillCaster::Unit(caster),
                target: SkillCastTarget::Unit(target),
                flag: 0,
                skill_object: SkillObject::None,
            },
        );
    };

    // Farm Wagon: the model has no point 2. With the driver seat taken the interaction list keeps
    // the occupy skill, and a forged passenger cast attaches nobody and answers 0x60.
    let wagon = spawn(60, 1008, 0x300, 3, 103.0);
    a.drain();
    b.drain();
    slaves::bind_slave(
        &server,
        &pa,
        wagon.obj_id,
        attach_point::DRIVER,
        7,
        Some(4.0),
    );
    a.drain();
    b.drain();
    req(wagon.obj_id)
        .handle(&server, &mut b.conn)
        .await
        .unwrap();
    assert_eq!(b.drain(), interaction(wagon.obj_id, vec![12_076]));
    cast(&mut b, PB, wagon.obj_id);
    let fb = b.drain();
    assert!(!ops(&fb).contains(&sc::SCUnitAttachedPacket));
    assert_eq!(
        body(&fb, sc::SCErrorMsgPacket),
        vec![0x60, 0, 0x60, 0, 0, 0, 0, 0, 0],
        "a missing seat must clear the pending bind request"
    );
    assert_eq!(
        wagon.data().attached_characters,
        vec![(attach_point::DRIVER, PA)]
    );
    assert_eq!(pb.data().unit.transform.parent_obj_id, 0);
    slaves::unbind_slave(&server, &pa, 3, slaves::attach_reason::SLAVE_BINDING);
    a.drain();

    // Red Farm Hauler: model 1553 has point 2, so it is offered once the driver seat is taken and
    // the cast seats the passenger.
    let hauler = spawn(146, 1553, 0x301, 4, 103.0);
    a.drain();
    b.drain();
    slaves::bind_slave(
        &server,
        &pa,
        hauler.obj_id,
        attach_point::DRIVER,
        7,
        Some(4.0),
    );
    a.drain();
    b.drain();
    req(hauler.obj_id)
        .handle(&server, &mut b.conn)
        .await
        .unwrap();
    assert_eq!(b.drain(), interaction(hauler.obj_id, vec![13_863]));
    cast(&mut b, PB, hauler.obj_id);
    assert_eq!(
        hauler.data().attached_characters,
        vec![(attach_point::DRIVER, PA), (attach_point::PASSENGER0, PB)]
    );
    assert_eq!(pb.data().attached_point, attach_point::PASSENGER0);
    slaves::unbind_slave(&server, &pb, 4, slaves::attach_reason::SLAVE_BINDING);
    slaves::unbind_slave(&server, &pa, 4, slaves::attach_reason::SLAVE_BINDING);
    a.drain();
    b.drain();

    // A point the model does declare still binds: a ship cannon's gunner seat at point 9.
    let ship = spawn(23, 512, 0x302, 5, 103.0);
    a.drain();
    b.drain();
    slaves::bind_slave(
        &server,
        &pa,
        ship.obj_id,
        9,
        slaves::attach_reason::BOARD_TRANSFER,
        None,
    );
    assert_eq!(
        ship.data().attached_characters,
        vec![(9, PA)],
        "a declared cannon seat must still bind"
    );
    a.drain();
}

/// The client's own bind reach: 12076 uses a 4.0 m surface gap (`FUN_39346290` ->
/// `FUN_39093860(..., 16.0)`), 13863 3.0 m (`FUN_393bf270` -> 9.0), widened to 3.5 m while
/// riding a mate (the S11 reading of `+0x3c26`; capture-pending S11-C5). A forged bind / cast
/// from outside is refused with `TOO_FAR_AWAY` 0x9a; inside it attaches.
#[tokio::test]
async fn a_slave_outside_the_client_reach_is_refused_and_an_in_reach_one_is_seated() {
    use crate::models::slave::Slave;
    use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
    use crate::world::Entity;

    let server = server_with(passenger_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let mut c = TestClient::new(&server, 3, 3);
    let pa = spawn_player(&server, &a, test_character(1, "Near", 100.0, 100.0), PA);
    let pb = spawn_player(&server, &b, test_character(2, "Far", 110.0, 100.0), PB);
    let pc = spawn_player(
        &server,
        &c,
        test_character(3, "Mounted", 106.1, 100.0),
        0x00F0_0003,
    );
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    c.conn.active_char = Some(pc.clone());
    for p in [&pb, &pc] {
        let mut d = p.data();
        d.unit.hp = 1000;
        d.unit.max_hp = 1000;
    }
    let world = server.world.world(0).unwrap();
    let mut cart = Slave::new(0, SLAVE_TEMPLATE);
    cart.unit.hp = 1000;
    cart.unit.transform.set_xyz(102.9, 100.0, 100.0);
    cart.unit.tl_id = 3;
    cart.unit.hp = 1000;
    cart.unit.max_hp = 1000;
    cart.summoner_obj_id = PA;
    cart.summoner_id = pa.character_id;
    let cart = Entity::new(0x300, cart);
    world.spawn(&cart, &server.view_ctx());
    a.drain();
    b.drain();
    c.drain();

    let cast = |client: &mut TestClient, caster: u32, target: u32| {
        start_skill(
            &server,
            &mut client.conn,
            CSStartSkillPacket {
                skill_id: PASSENGER_SKILL,
                caster: SkillCaster::Unit(caster),
                target: SkillCastTarget::Unit(target),
                flag: 0,
                skill_object: SkillObject::None,
            },
        );
    };

    // Far driver bind (7.1 m > 4.0 m) through the 0x031 handler path: the clearing form
    // (`isNotify = false`) or the client keeps its pending bind request.
    slaves::bind_slave_by_tl(&server, &pb, 3);
    assert_eq!(
        body(&b.drain(), sc::SCErrorMsgPacket),
        vec![0x9a, 0, 0x9a, 0, 0, 0, 0, 0, 0]
    );
    assert!(cart.data().attached_characters.is_empty());

    // Far passenger cast (7.1 m > 3.0 m) through the real skill runner.
    cast(&mut b, PB, cart.obj_id);
    let fb = b.drain();
    assert_eq!(
        body(&fb, sc::SCErrorMsgPacket),
        vec![0x9a, 0, 0x9a, 0, 0, 0, 0, 0, 0]
    );
    assert!(!ops(&fb).contains(&sc::SCUnitAttachedPacket));
    assert!(cart.data().attached_characters.is_empty());

    // In-reach driver bind (2.9 m < 4.0 m).
    slaves::bind_slave_by_tl(&server, &pa, 3);
    assert_eq!(
        cart.data().attached_characters,
        vec![(attach_point::DRIVER, PA)]
    );
    a.drain();
    slaves::unbind_slave(&server, &pa, 3, slaves::attach_reason::SLAVE_BINDING);
    a.drain();

    // 3.2 m is out of the 3.0 m passenger reach...
    cast(&mut c, 0x00F0_0003, cart.obj_id);
    assert!(error_codes(&c.drain()).contains(&super::error::TOO_FAR_AWAY));
    assert!(cart.data().attached_characters.is_empty());
    // ...but inside the 3.5 m mounted reach.
    pc.data().is_riding = true;
    cast(&mut c, 0x00F0_0003, cart.obj_id);
    assert_eq!(
        cart.data().attached_characters,
        vec![(attach_point::PASSENGER0, 0x00F0_0003)]
    );
}

/// The 1.2 client offers the Red Farm Hauler's passenger seat from beside the hull: it measures
/// the distance to the vehicle's collision parts, not to the model origin (`FUN_39093860` ->
/// `FUN_3989bf90`). Model 1553 has no ship model and no located point 2, so the origin fallback
/// refused a player beside the rear seat more than 3 m from the origin (live log 2026-11-01
/// 14:22:40-14:22:42, three `TOO_FAR_AWAY` answers). The model's nearest located attach point
/// stands in for the hull; a player beyond both the hull and the origin is still refused, and the
/// driver bind still uses the 4.0 m reach.
#[tokio::test]
async fn passenger_beside_the_hull_is_in_reach_without_a_ship_model_or_a_located_seat() {
    use crate::models::slave::Slave;
    use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
    use crate::world::Entity;

    let server = server_with(hauler_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let mut c = TestClient::new(&server, 3, 3);
    let pa = spawn_player(&server, &a, test_character(1, "Driver", 100.0, 100.0), PA);
    let pb = spawn_player(&server, &b, test_character(2, "Beside", 105.0, 97.5), PB);
    let pc = spawn_player(
        &server,
        &c,
        test_character(3, "Far", 110.0, 100.0),
        0x00F0_0003,
    );
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    c.conn.active_char = Some(pc.clone());
    for p in [&pb, &pc] {
        let mut d = p.data();
        d.unit.hp = 1000;
        d.unit.max_hp = 1000;
    }
    let world = server.world.world(0).unwrap();
    let mut hauler = Slave::new(0, 146);
    hauler.unit.hp = 1000;
    hauler.unit.max_hp = 1000;
    hauler.unit.model_id = 1553;
    hauler.unit.tl_id = 3;
    hauler.unit.transform.set_xyz(103.0, 100.0, 100.0);
    hauler.summoner_obj_id = PA;
    hauler.summoner_id = pa.character_id;
    let hauler = Entity::new(0x300, hauler);
    world.spawn(&hauler, &server.view_ctx());
    a.drain();
    b.drain();
    c.drain();

    let cast = |client: &mut TestClient, caster: u32, target: u32| {
        start_skill(
            &server,
            &mut client.conn,
            CSStartSkillPacket {
                skill_id: PASSENGER_SKILL,
                caster: SkillCaster::Unit(caster),
                target: SkillCastTarget::Unit(target),
                flag: 0,
                skill_object: SkillObject::None,
            },
        );
    };

    // The driver stands 3.0 m from the origin, farther (4.2 m) from the rear cannon points: the
    // smaller origin distance keeps the unchanged 4.0 m driver reach.
    slaves::bind_slave_by_tl(&server, &pa, 3);
    assert_eq!(
        hauler.data().attached_characters,
        vec![(attach_point::DRIVER, PA)]
    );
    a.drain();

    // The passenger stands beside the rear seat: 3.2 m from the origin but 1.9 m from the nearest
    // located attach point.
    cast(&mut b, PB, hauler.obj_id);
    let fb = b.drain();
    assert!(
        !error_codes(&fb).contains(&super::error::TOO_FAR_AWAY),
        "the client offers this seat: {:x?}",
        error_codes(&fb)
    );
    assert!(ops(&fb).contains(&sc::SCUnitAttachedPacket));
    assert_eq!(
        hauler.data().attached_characters,
        vec![(attach_point::DRIVER, PA), (attach_point::PASSENGER0, PB)]
    );
    assert_eq!(pb.data().attached_point, attach_point::PASSENGER0);

    // 7 m from the origin, beyond the hull and the origin bound.
    cast(&mut c, 0x00F0_0003, hauler.obj_id);
    assert_eq!(
        error_codes(&c.drain()),
        vec![super::error::TOO_FAR_AWAY],
        "a player away from the hull is still refused"
    );
    assert_eq!(
        hauler.data().attached_characters,
        vec![(attach_point::DRIVER, PA), (attach_point::PASSENGER0, PB)]
    );
}

/// The live log's second F bind (2026-11-01 14:22:50): after `CSDiscardSlave` the driver had
/// driven the hauler 3.5 m and stood 4.9 m from its origin beside the rear hull, and the re-bind
/// was refused with `TOO_FAR_AWAY`. The discard does clear the seat; the refusal was the same
/// origin-only gap. The 4.0 m driver reach is unchanged and still refuses a player away from the
/// hull.
#[tokio::test]
async fn a_discarded_driver_rebinds_beside_the_hull_and_a_far_one_is_still_refused() {
    use crate::models::slave::Slave;
    use crate::net::registry::ClientPacket;
    use crate::systems::vehicles::packets::{CSBindSlavePacket, CSDiscardSlavePacket};
    use crate::world::Entity;

    let server = server_with(hauler_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "Driver", 103.0, 98.0), PA);
    a.conn.active_char = Some(pa.clone());
    let world = server.world.world(0).unwrap();
    let mut hauler = Slave::new(0, 146);
    hauler.unit.hp = 1000;
    hauler.unit.max_hp = 1000;
    hauler.unit.model_id = 1553;
    hauler.unit.tl_id = 3;
    hauler.unit.transform.set_xyz(103.0, 100.0, 100.0);
    hauler.summoner_obj_id = PA;
    hauler.summoner_id = pa.character_id;
    let hauler = Entity::new(0x300, hauler);
    world.spawn(&hauler, &server.view_ctx());
    a.drain();

    CSBindSlavePacket { tl_id: 3 }
        .handle(&server, &mut a.conn)
        .await
        .unwrap();
    assert_eq!(
        hauler.data().attached_characters,
        vec![(attach_point::DRIVER, PA)]
    );
    a.drain();

    // Discard, then stand where the live driver stood: 4.9 m behind the origin, beside the hull.
    CSDiscardSlavePacket { tl_id: 3 }
        .handle(&server, &mut a.conn)
        .await
        .unwrap();
    assert!(hauler.data().attached_characters.is_empty());
    assert!(slaves::get_is_mounted(&world, PA).is_none());
    a.drain();
    pa.data().unit.transform.set_xyz(103.0, 95.1, 100.0);

    CSBindSlavePacket { tl_id: 3 }
        .handle(&server, &mut a.conn)
        .await
        .unwrap();
    assert_eq!(
        hauler.data().attached_characters,
        vec![(attach_point::DRIVER, PA)],
        "the client offers the driver seat from beside the hull"
    );
    assert!(error_codes(&a.drain()).is_empty());

    // 9 m behind the origin is beyond the hull and the origin bound: the 4.0 m reach still holds.
    CSDiscardSlavePacket { tl_id: 3 }
        .handle(&server, &mut a.conn)
        .await
        .unwrap();
    a.drain();
    pa.data().unit.transform.set_xyz(103.0, 91.0, 100.0);
    CSBindSlavePacket { tl_id: 3 }
        .handle(&server, &mut a.conn)
        .await
        .unwrap();
    let refused = a.drain();
    assert_eq!(
        body(&refused, sc::SCErrorMsgPacket),
        vec![0x9a, 0, 0x9a, 0, 0, 0, 0, 0, 0],
        "the refusal must be the clearing (isNotify = false) form"
    );
    assert!(hauler.data().attached_characters.is_empty());

    // The refusal latches nothing: the next in-reach bind succeeds.
    pa.data().unit.transform.set_xyz(103.0, 95.1, 100.0);
    CSBindSlavePacket { tl_id: 3 }
        .handle(&server, &mut a.conn)
        .await
        .unwrap();
    assert_eq!(
        hauler.data().attached_characters,
        vec![(attach_point::DRIVER, PA)]
    );
    assert!(error_codes(&a.drain()).is_empty());
}

/// A synthetic `mesh_physics_data` CGF with one default box of the given half extents.
fn box_cgf(half: [f32; 3]) -> Vec<u8> {
    use aaemu_physics::geometry::primitives::BoxPrim;
    use aaemu_physics::geometry::shapes::stream::{save_phys_geometry, PhysGeomHeader};
    use aaemu_physics::geometry::shapes::BoxGeom;
    use aaemu_physics::math::{Quat, Vec3};

    let geometry = BoxGeom::new(&BoxPrim::aligned(
        Vec3::ZERO,
        Vec3::new(half[0], half[1], half[2]),
    ));
    let mut blob = Vec::new();
    save_phys_geometry(
        &PhysGeomHeader {
            inertia: Vec3::splat(1.0),
            q: Quat::IDENTITY,
            origin: Vec3::ZERO,
            volume: 8.0,
            unk52: 1,
            surface_idx: 0,
        },
        &geometry,
        &mut blob,
    );
    crate::systems::ai::collision::cgf::tests::synthetic_cgf(&blob)
}

/// The Red Farm Hauler's model reference plus a synthetic body for it: the 1.2 prefab names
/// the body as a physicalized `AnimObject` whose `.chr` has no `MeshPhysicsData`; the
/// same-named `.cgf` holds the collision proxies. Half extents (1.7, 2.8, 1.0) put the 4.2 m
/// driver spot 2.5 m from the body but 4.2 m from the model origin and 4.3 m from the rear
/// cannon points, i.e. outside every fallback measure.
fn hauler_geometry_data() -> GameData {
    let mut d = hauler_data();
    d.systems.vehicles.vehicle_model_refs.insert(
        1553,
        "prefab://prefabs/vehicle_tractor.xml/vehicle_tractor_c.tractor_body".into(),
    );
    d
}

fn hauler_geometry_files() -> HashMap<String, Vec<u8>> {
    let prefab = r#"<PrefabsLibrary><Prefab Name="vehicle_tractor_c.tractor_body"><Objects>
      <Object Type="Entity" Name="body" EntityClass="AnimObject">
        <Properties object_Model="game/objects/env/06_unit/05_carriage/transfers_trailer_c.chr">
          <Physics bPhysicalize="1" bArticulated="0" Mass="-1" Density="-1" bRigidBody="1"/>
        </Properties>
      </Object>
    </Objects></Prefab></PrefabsLibrary>"#;
    let mut files = HashMap::new();
    files.insert(
        "game/prefabs/vehicle_tractor.xml".to_string(),
        prefab.as_bytes().to_vec(),
    );
    files.insert(
        "game/objects/env/06_unit/05_carriage/transfers_trailer_c.cgf".to_string(),
        box_cgf([1.7, 2.8, 1.0]),
    );
    files
}

/// Bounds are cached per model id, including a negative result: a second bind of the same
/// model must not re-read and re-parse the prefab library, and a model without geometry must
/// not repeat its failed model lookups.
#[test]
fn vehicle_bounds_cache_keeps_both_hits_and_misses() {
    use aaemu_physics::geometry::primitives::BoxPrim;
    use aaemu_physics::math::Vec3;

    let cache = dashmap::DashMap::new();
    let mut loads = 0;
    assert!(slaves::cached_vehicle_bounds(&cache, 1553, || {
        loads += 1;
        None
    })
    .is_none());
    assert!(
        slaves::cached_vehicle_bounds(&cache, 1553, || {
            loads += 1;
            Some(BoxPrim::aligned(Vec3::ZERO, Vec3::splat(1.0)))
        })
        .is_none(),
        "a cached miss is not reloaded"
    );
    assert_eq!(loads, 1);
    let bounds = BoxPrim::aligned(Vec3::new(1.0, 2.0, 3.0), Vec3::new(1.0, 2.0, 3.0));
    assert_eq!(
        slaves::cached_vehicle_bounds(&cache, 511, || {
            loads += 1;
            Some(bounds)
        }),
        Some(bounds)
    );
    assert_eq!(
        slaves::cached_vehicle_bounds(&cache, 511, || {
            loads += 1;
            None
        }),
        Some(bounds),
        "a cached hit is not reloaded"
    );
    assert_eq!(loads, 2);
}

/// A bind before the client files are available (heightmaps not loaded yet, or physics off)
/// falls back to the attach-point measure but does not cache the miss: once the files are
/// there, the next bind measures the body.
#[tokio::test]
async fn land_vehicle_bounds_are_not_cached_before_the_client_files_load() {
    use crate::models::slave::Slave;
    use crate::net::registry::ClientPacket;
    use crate::systems::vehicles::packets::CSBindSlavePacket;
    use crate::world::Entity;

    let server = server_with(hauler_geometry_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "Near", 107.2, 100.0), PA);
    a.conn.active_char = Some(pa.clone());
    {
        let mut data = pa.data();
        data.unit.hp = 1000;
        data.unit.max_hp = 1000;
    }
    let world = server.world.world(0).unwrap();
    let mut hauler = Slave::new(0, 146);
    hauler.unit.hp = 1000;
    hauler.unit.max_hp = 1000;
    hauler.unit.model_id = 1553;
    hauler.unit.tl_id = 3;
    hauler.unit.transform.set_xyz(103.0, 100.0, 100.0);
    hauler.summoner_obj_id = PA;
    hauler.summoner_id = pa.character_id;
    let hauler = Entity::new(0x300, hauler);
    world.spawn(&hauler, &server.view_ctx());
    a.drain();

    // No client files yet: 4.2 m from the origin is outside the fallback reach.
    CSBindSlavePacket { tl_id: 3 }
        .handle(&server, &mut a.conn)
        .await
        .unwrap();
    assert_eq!(error_codes(&a.drain()), vec![super::error::TOO_FAR_AWAY]);
    assert!(
        server.systems.vehicles.vehicle_bounds.get(&1553).is_none(),
        "a bind without client files must not cache the missing bounds"
    );

    // The files arrive: the same position now binds against the body.
    server
        .systems
        .ai
        .collision
        .test_files
        .set(Arc::new(hauler_geometry_files()))
        .unwrap();
    CSBindSlavePacket { tl_id: 3 }
        .handle(&server, &mut a.conn)
        .await
        .unwrap();
    assert_eq!(
        hauler.data().attached_characters,
        vec![(attach_point::DRIVER, PA)]
    );
}

/// The 2026-11-02 01:43-01:44 capture: the client offers the driver bind beside the Red Farm
/// Hauler's cab and the server refused with `TOO_FAR_AWAY` because the only located attach
/// points are the rear cannons. The client measures to the body's collision geometry
/// (`FUN_39346290` -> `FUN_39093860` -> `FUN_3989bf90`), so the server now measures the land
/// vehicle's model collision bounds. 2.5 m from the body (4.2 m from the origin, outside the
/// 4.0 m reach by every old measure) binds; 5 m from the body is refused. The passenger reach
/// keeps 3.0 m but also uses the body: 2.2 m from it binds, 3.2 m is refused.
#[tokio::test]
async fn land_vehicle_bind_measures_the_body_bounds_not_the_rear_attach_points() {
    use crate::models::slave::Slave;
    use crate::net::registry::ClientPacket;
    use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
    use crate::systems::vehicles::packets::CSBindSlavePacket;
    use crate::world::Entity;

    let server = server_with(hauler_geometry_data());
    server
        .systems
        .ai
        .collision
        .test_files
        .set(Arc::new(hauler_geometry_files()))
        .unwrap();
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let mut c = TestClient::new(&server, 3, 3);
    let mut d = TestClient::new(&server, 4, 4);
    // Hauler at (103, 100); model +X is world +X. Body half width 1.7, so the driver at
    // 4.2 m is 2.5 m from the body, the far one at 6.7 m is 5 m from it.
    let pa = spawn_player(&server, &a, test_character(1, "Near", 107.2, 100.0), PA);
    let pb = spawn_player(&server, &b, test_character(2, "Far", 109.7, 100.0), PB);
    let pc = spawn_player(
        &server,
        &c,
        test_character(3, "Passenger", 106.9, 100.0),
        0x00F0_0003,
    );
    let pd = spawn_player(
        &server,
        &d,
        test_character(4, "FarPassenger", 107.9, 100.0),
        0x00F0_0004,
    );
    for (client, p) in [(&mut a, &pa), (&mut b, &pb)] {
        client.conn.active_char = Some(p.clone());
    }
    c.conn.active_char = Some(pc.clone());
    d.conn.active_char = Some(pd.clone());
    for p in [&pa, &pb, &pc, &pd] {
        let mut data = p.data();
        data.unit.hp = 1000;
        data.unit.max_hp = 1000;
    }
    let world = server.world.world(0).unwrap();
    let mut hauler = Slave::new(0, 146);
    hauler.unit.hp = 1000;
    hauler.unit.max_hp = 1000;
    hauler.unit.model_id = 1553;
    hauler.unit.tl_id = 3;
    hauler.unit.transform.set_xyz(103.0, 100.0, 100.0);
    hauler.summoner_obj_id = PA;
    hauler.summoner_id = pa.character_id;
    let hauler = Entity::new(0x300, hauler);
    world.spawn(&hauler, &server.view_ctx());
    for client in [&mut a, &mut b, &mut c, &mut d] {
        client.drain();
    }

    // 5 m from the body: refused, and the 4.0 m reach is unchanged.
    CSBindSlavePacket { tl_id: 3 }
        .handle(&server, &mut b.conn)
        .await
        .unwrap();
    assert_eq!(
        error_codes(&b.drain()),
        vec![super::error::TOO_FAR_AWAY],
        "5 m from the body is outside the 4.0 m driver reach"
    );
    assert!(hauler.data().attached_characters.is_empty());

    // 2.5 m from the body, 4.2 m from the origin and 4.3 m from the rear cannon points:
    // only the body measure is inside the reach.
    CSBindSlavePacket { tl_id: 3 }
        .handle(&server, &mut a.conn)
        .await
        .unwrap();
    assert_eq!(
        hauler.data().attached_characters,
        vec![(attach_point::DRIVER, PA)]
    );
    assert!(error_codes(&a.drain()).is_empty());
    assert!(
        server
            .systems
            .vehicles
            .vehicle_bounds
            .get(&1553)
            .is_some_and(|bounds| bounds.is_some()),
        "the bind loaded and cached the model bounds"
    );

    let cast = |client: &mut TestClient, caster: u32, target: u32| {
        start_skill(
            &server,
            &mut client.conn,
            CSStartSkillPacket {
                skill_id: PASSENGER_SKILL,
                caster: SkillCaster::Unit(caster),
                target: SkillCastTarget::Unit(target),
                flag: 0,
                skill_object: SkillObject::None,
            },
        );
    };

    // The passenger reach is unchanged at 3.0 m: 2.2 m from the body binds, even though the
    // origin is 3.9 m away (outside the old origin measure).
    cast(&mut c, 0x00F0_0003, hauler.obj_id);
    assert!(error_codes(&c.drain()).is_empty());
    assert_eq!(
        hauler.data().attached_characters,
        vec![
            (attach_point::DRIVER, PA),
            (attach_point::PASSENGER0, 0x00F0_0003)
        ]
    );

    // 3.2 m from the body is outside the unchanged 3.0 m passenger reach.
    cast(&mut d, 0x00F0_0004, hauler.obj_id);
    assert_eq!(
        error_codes(&d.drain()),
        vec![super::error::TOO_FAR_AWAY],
        "3.2 m from the body is outside the 3.0 m passenger reach"
    );
    assert_eq!(
        hauler.data().attached_characters,
        vec![
            (attach_point::DRIVER, PA),
            (attach_point::PASSENGER0, 0x00F0_0003)
        ]
    );
}

/// One slave at a time: the client refuses a second bind with `slave_unbind_first` 0x66
/// (`FUN_39346290`), and a forged second bind (driver or passenger) gets the same error.
#[tokio::test]
async fn a_character_on_one_slave_cannot_bind_a_second_one() {
    use crate::models::slave::Slave;
    use crate::world::Entity;

    let server = server_with(slave_data());
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "Rider", 100.0, 100.0), PA);
    a.conn.active_char = Some(pa.clone());
    let world = server.world.world(0).unwrap();
    let mut first = Slave::new(0, SLAVE_TEMPLATE);
    first.unit.hp = 1000;
    first.unit.transform.set_xyz(101.0, 100.0, 100.0);
    first.unit.tl_id = 3;
    first.unit.hp = 1000;
    first.unit.max_hp = 1000;
    first.summoner_obj_id = PA;
    let first = Entity::new(0x300, first);
    world.spawn(&first, &server.view_ctx());
    let mut second = Slave::new(0, SLAVE_TEMPLATE);
    second.unit.hp = 1000;
    second.unit.transform.set_xyz(102.0, 100.0, 100.0);
    second.unit.tl_id = 4;
    second.unit.hp = 1000;
    second.unit.max_hp = 1000;
    second.summoner_obj_id = PA;
    let second = Entity::new(0x301, second);
    world.spawn(&second, &server.view_ctx());
    a.drain();

    slaves::bind_slave(
        &server,
        &pa,
        first.obj_id,
        attach_point::DRIVER,
        7,
        Some(4.0),
    );
    assert_eq!(
        first.data().attached_characters,
        vec![(attach_point::DRIVER, PA)]
    );
    a.drain();

    // The 0x031 handler path to a second slave.
    slaves::bind_slave_by_tl(&server, &pa, 4);
    assert_eq!(
        body(&a.drain(), sc::SCErrorMsgPacket),
        vec![0x66, 0, 0x66, 0, 0, 0, 0, 0, 0]
    );
    assert!(second.data().attached_characters.is_empty());

    // The 13863 passenger path to a second slave.
    slaves::bind_slave(
        &server,
        &pa,
        second.obj_id,
        attach_point::PASSENGER0,
        7,
        Some(3.0),
    );
    assert_eq!(
        error_codes(&a.drain()),
        vec![super::error::SLAVE_UNBIND_FIRST]
    );
    assert!(second.data().attached_characters.is_empty());

    // A second seat on the same slave is refused too (a forged bind must not duplicate the
    // character; the same 0x66 clears the client's pending request).
    slaves::bind_slave(
        &server,
        &pa,
        first.obj_id,
        attach_point::PASSENGER0,
        7,
        Some(3.0),
    );
    assert_eq!(
        error_codes(&a.drain()),
        vec![super::error::SLAVE_UNBIND_FIRST]
    );
    assert_eq!(first.data().attached_characters.len(), 1);
}

/// A cannon (slave kind 7 / 8) only has its gunner seat at point 1: the passenger skill 13863
/// is refused even though only the interaction list used to filter it.
#[tokio::test]
async fn a_cannon_refuses_the_passenger_seat_skill() {
    use crate::models::slave::Slave;
    use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
    use crate::systems::vehicles::data::SlaveTemplate;
    use crate::world::Entity;

    let mut d = passenger_data();
    d.systems.vehicles.slave_templates.insert(
        8,
        SlaveTemplate {
            id: 8,
            slave_kind: super::data::slave_kind::SLAVE_EQUIPMENT,
            ..SlaveTemplate::default()
        },
    );
    let server = server_with(d);
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "Gunner", 100.0, 100.0), PA);
    a.conn.active_char = Some(pa.clone());
    {
        let mut c = pa.data();
        c.unit.hp = 1000;
        c.unit.max_hp = 1000;
    }
    let world = server.world.world(0).unwrap();
    let mut cannon = Slave::new(0, 8);
    cannon.unit.hp = 1000;
    cannon.unit.transform.set_xyz(101.0, 100.0, 100.0);
    cannon.unit.tl_id = 3;
    cannon.summoner_obj_id = PA;
    let cannon = Entity::new(0x300, cannon);
    world.spawn(&cannon, &server.view_ctx());
    a.drain();

    start_skill(
        &server,
        &mut a.conn,
        CSStartSkillPacket {
            skill_id: PASSENGER_SKILL,
            caster: SkillCaster::Unit(PA),
            target: SkillCastTarget::Unit(cannon.obj_id),
            flag: 0,
            skill_object: SkillObject::None,
        },
    );
    let fa = a.drain();
    assert_eq!(
        body(&fa, sc::SCErrorMsgPacket),
        vec![0x60, 0, 0x60, 0, 0, 0, 0, 0, 0]
    );
    assert!(!ops(&fa).contains(&sc::SCUnitAttachedPacket));
    assert!(cannon.data().attached_characters.is_empty());
}

/// A ship's seat doodad can be far from the hull origin: the BoardTransfer caller skips the
/// bind reach (its doodad func already checked the range), so a seat 16 m from the origin is
/// accepted. The 13863 passenger cast still uses the ship mass box: 16 m from the origin but
/// 1 m outside the box is accepted, 30 m from the origin is refused with 0x9a.
#[tokio::test]
async fn ship_seats_measure_the_mass_box_not_the_hull_origin() {
    use crate::models::slave::Slave;
    use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
    use crate::systems::vehicles::data::{ShipModel, SlaveTemplate};
    use crate::world::Entity;

    let mut d = passenger_data();
    d.systems
        .vehicles
        .model_types
        .insert(511, (511, "ShipModel".into()));
    d.systems.vehicles.ship_models.insert(
        511,
        ShipModel {
            id: 511,
            mass_box_size_x: 6.0,
            mass_box_size_y: 30.0,
            mass_box_size_z: 10.0,
            ..ShipModel::default()
        },
    );
    d.systems.vehicles.slave_templates.insert(
        22,
        SlaveTemplate {
            id: 22,
            name: "Long ship".into(),
            model_id: 511,
            slave_kind: super::data::slave_kind::MACHINE,
            ..SlaveTemplate::default()
        },
    );
    d.systems
        .vehicles
        .model_bindings
        .insert(511, vec![attach_point::DRIVER, attach_point::PASSENGER0]);
    let server = server_with(d);
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let pa = spawn_player(&server, &a, test_character(1, "Seat", 100.0, 116.0), PA);
    let pb = spawn_player(&server, &b, test_character(2, "Far", 100.0, 130.0), PB);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    for p in [&pa, &pb] {
        let mut c = p.data();
        c.unit.hp = 1000;
        c.unit.max_hp = 1000;
    }
    let world = server.world.world(0).unwrap();
    let mut hull = Slave::new(0, 22);
    hull.unit.hp = 1000;
    hull.unit.model_id = 511;
    hull.unit.transform.set_xyz(100.0, 100.0, 100.0);
    hull.unit.tl_id = 3;
    hull.unit.hp = 1000;
    hull.unit.max_hp = 1000;
    hull.summoner_obj_id = PA;
    let hull = Entity::new(0x300, hull);
    world.spawn(&hull, &server.view_ctx());
    a.drain();
    b.drain();

    // BoardTransfer (the seat doodad func) skips the reach.
    slaves::bind_slave(
        &server,
        &pa,
        hull.obj_id,
        attach_point::PASSENGER0,
        slaves::attach_reason::BOARD_TRANSFER,
        None,
    );
    assert_eq!(
        hull.data().attached_characters,
        vec![(attach_point::PASSENGER0, PA)]
    );
    a.drain();
    slaves::unbind_slave(&server, &pa, 3, slaves::attach_reason::SLAVE_BINDING);
    a.drain();
    pa.data().unit.transform.set_xyz(100.0, 116.0, 100.0);

    // 16 m from the origin is 1 m outside the 30 m mass box: inside the 3.0 m passenger reach.
    start_skill(
        &server,
        &mut a.conn,
        CSStartSkillPacket {
            skill_id: PASSENGER_SKILL,
            caster: SkillCaster::Unit(PA),
            target: SkillCastTarget::Unit(hull.obj_id),
            flag: 0,
            skill_object: SkillObject::None,
        },
    );
    assert_eq!(
        hull.data().attached_characters,
        vec![(attach_point::PASSENGER0, PA)]
    );
    a.drain();
    slaves::unbind_slave(&server, &pa, 3, slaves::attach_reason::SLAVE_BINDING);
    a.drain();

    // 30 m from the origin is 15 m outside the box: the far 13863 cast is refused.
    start_skill(
        &server,
        &mut b.conn,
        CSStartSkillPacket {
            skill_id: PASSENGER_SKILL,
            caster: SkillCaster::Unit(PB),
            target: SkillCastTarget::Unit(hull.obj_id),
            flag: 0,
            skill_object: SkillObject::None,
        },
    );
    let fb = b.drain();
    assert!(error_codes(&fb).contains(&super::error::TOO_FAR_AWAY));
    assert!(hull.data().attached_characters.is_empty());
}

/// A passenger who dies is unbound from the slave seat through the logout unbind path, so the
/// one-slave guard cannot strand them after a resurrect.
#[tokio::test]
async fn a_dead_passenger_is_unbound_and_can_board_another_slave_after_resurrect() {
    use crate::models::slave::Slave;
    use crate::systems::skills::types::KillReason;
    use crate::world::Entity;

    let server = server_with(slave_data());
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let pa = spawn_player(&server, &a, test_character(1, "Killer", 100.0, 100.0), PA);
    let pb = spawn_player(
        &server,
        &b,
        test_character(2, "Passenger", 100.5, 100.0),
        PB,
    );
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    {
        let mut c = pb.data();
        c.unit.hp = 1000;
        c.unit.max_hp = 1000;
    }
    let world = server.world.world(0).unwrap();
    let mut first = Slave::new(0, SLAVE_TEMPLATE);
    first.unit.hp = 1000;
    first.unit.transform.set_xyz(100.0, 100.0, 100.0);
    first.unit.tl_id = 3;
    first.unit.hp = 1000;
    first.unit.max_hp = 1000;
    first.summoner_obj_id = PA;
    let first = Entity::new(0x300, first);
    world.spawn(&first, &server.view_ctx());
    let mut second = Slave::new(0, SLAVE_TEMPLATE);
    second.unit.hp = 1000;
    second.unit.transform.set_xyz(100.0, 100.0, 100.0);
    second.unit.tl_id = 4;
    second.unit.hp = 1000;
    second.unit.max_hp = 1000;
    second.summoner_obj_id = PA;
    let second = Entity::new(0x301, second);
    world.spawn(&second, &server.view_ctx());
    a.drain();
    b.drain();

    slaves::bind_slave(
        &server,
        &pb,
        first.obj_id,
        attach_point::PASSENGER0,
        7,
        Some(3.0),
    );
    assert_eq!(
        first.data().attached_characters,
        vec![(attach_point::PASSENGER0, PB)]
    );
    b.drain();

    // Death releases the seat through the same unbind path as logout.
    crate::systems::skills::combat::do_die(
        &server,
        &WorldObject::from(&pb),
        Some(&WorldObject::from(&pa)),
        KillReason::Damage,
    );
    assert!(first.data().attached_characters.is_empty());
    assert_eq!(pb.data().attached_point, attach_point::NONE);
    b.drain();

    // Resurrect and board another slave: the one-slave guard must not see a stale seat.
    crate::systems::skills::resurrect::resurrect_character(&server, &pb, true);
    {
        let mut c = pb.data();
        c.unit.hp = 1000;
        c.unit.max_hp = 1000;
    }
    pb.data().unit.transform.set_xyz(100.0, 100.0, 100.0);
    slaves::bind_slave_by_tl(&server, &pb, 4);
    assert_eq!(
        second.data().attached_characters,
        vec![(attach_point::DRIVER, PB)]
    );
}

/// A deck stander's transform is hull-local (`parent_obj_id` set) but it is not in the hull's
/// `AttachedCharacters` (it was not bound as a driver). Replacing the hull must give the owner
/// its world pose back before `create_spawn_committed` reads it: otherwise the summon places
/// the new hull at the old ship-local coordinates (live capture: (0.96, -20.27) instead of
/// 4293, 4122).
#[tokio::test]
async fn summoning_while_standing_on_the_prior_hull_uses_the_owners_world_position() {
    let server = server_with(slave_data());
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), SLAVE_ITEM);
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    a.drain();

    let item = mates::owned_item(&server, &pa, item_id).unwrap();
    let prior = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SLAVE_TEMPLATE,
        Some(item),
        false,
        None,
        None,
    )
    .expect("prior hull");
    prior
        .data()
        .unit
        .transform
        .set_position(200.0, 300.0, 50.0, 0.0, 0.0, 0.0);
    // Standing on the deck: parented with a ship-local position, not bound as a driver.
    let me = WorldObject::from(&pa);
    world.attach_child(&WorldObject::from(&prior), &me);
    pa.data()
        .unit
        .transform
        .set_position(1.0, -2.0, 3.0, 0.0, 0.0, 0.0);
    a.drain();

    let item = mates::owned_item(&server, &pa, item_id).unwrap();
    let second = slaves::create(
        &server,
        &world,
        Some(&pa),
        SLAVE_TEMPLATE,
        Some(item),
        false,
        None,
    )
    .await
    .expect("second hull");
    assert!(world.slave(prior.obj_id).is_none(), "prior hull replaced");
    assert_eq!(pa.data().unit.transform.parent_obj_id, 0);
    let owner = world.world_pos_rot(&me);
    assert_eq!((owner.x, owner.y, owner.z), (201.0, 298.0, 53.0));
    // 5 m in front (SpawnYOffset clamped to 5..50), level, 90 degrees CCW of the owner.
    let t = second.data().unit.transform;
    assert_eq!((t.x, t.y, t.z), (201.0, 303.0, 53.0));
    assert!((t.rot_z - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
}

#[tokio::test]
async fn rename_slave_packet_checks_ownership_and_updates_the_saved_name() {
    use crate::net::registry::ClientPacket;
    let server = server_with(slave_data());
    let mut owner = TestClient::new(&server, 1, 1);
    let (player, item_id) = player_with_item(&server, &owner, 1, PA, (100.0, 100.0), SLAVE_ITEM);
    owner.conn.active_char = Some(player.clone());
    let mut stranger = TestClient::new(&server, 2, 2);
    let other = spawn_player(
        &server,
        &stranger,
        test_character(2, "Other", 101.0, 100.0),
        PB,
    );
    stranger.conn.active_char = Some(other);
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let item = mates::owned_item(&server, &player, item_id).unwrap();
    let slave = slaves::create_core(
        &server,
        &world,
        Some(&player),
        None,
        SLAVE_TEMPLATE,
        Some(item),
        false,
        None,
        None,
    )
    .unwrap();
    let tl_id = slave.data().unit.tl_id;
    let original = slave.data().unit.name.clone();
    owner.drain();
    stranger.drain();
    super::packets::CSChangeSlaveNamePacket {
        tl_id,
        name: "Hijacked".into(),
    }
    .handle(&server, &mut stranger.conn)
    .await
    .unwrap();
    assert_eq!(slave.data().unit.name, original);
    for name in ["", "a very long invalid vehicle name", "Bad<Name>"] {
        super::packets::CSChangeSlaveNamePacket {
            tl_id,
            name: name.into(),
        }
        .handle(&server, &mut owner.conn)
        .await
        .unwrap();
        assert_eq!(slave.data().unit.name, original);
    }
    super::packets::CSChangeSlaveNamePacket {
        tl_id,
        name: "sEaBrEeZe".into(),
    }
    .handle(&server, &mut owner.conn)
    .await
    .unwrap();
    assert_eq!(slave.data().unit.name, "Seabreeze");
    assert_eq!(slaves::snapshot(&server, &slave)[0].name, "Seabreeze");
    let mut expected = PacketStream::new();
    expected.write_bc(slave.obj_id).write_str("Seabreeze");
    assert_eq!(
        body(&owner.drain(), sc::SCUnitNameChangedPacket),
        expected.as_bytes()
    );
    assert_eq!(
        body(&stranger.drain(), sc::SCUnitNameChangedPacket),
        expected.as_bytes()
    );
}

#[tokio::test]
async fn client_motion_and_despawn_cannot_control_other_players_or_slaves() {
    use crate::models::mate::Mate;
    use crate::models::movement::{ShipRequestMove, UnitMove, VehicleMove};
    use crate::models::slave::Slave;
    use crate::net::registry::ClientPacket;
    use crate::world::Entity;

    let server = server_with(GameData::empty());
    // Walks across the map in one packet: the movement guard has its own tests.
    server.config.update(|c| c.world.move_guard_enabled = false);
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_live_player(&server, &a, 1, PA, (100.0, 100.0));
    a.conn.active_char = Some(pa.clone());
    let b = TestClient::new(&server, 2, 2);
    let pb = spawn_live_player(&server, &b, 2, PB, (101.0, 100.0));
    let world = server.world.world(0).unwrap();
    let mut pet = Mate::new(1, 1);
    pet.owner_obj_id = PB;
    pet.unit.transform.set_xyz(102.0, 100.0, 100.0);
    let pet = Entity::new(0x300, pet);
    world.spawn(&pet, &server.view_ctx());
    let walking = MoveType::new(MoveKind::Unit(UnitMove {
        x: 500.0,
        y: 500.0,
        z: 100.0,
        vel_x: 10,
        ..Default::default()
    }));
    crate::world::session::move_unit(&server, &mut a.conn, PB, walking.clone()).await;
    crate::world::session::move_unit(&server, &mut a.conn, pet.obj_id, walking.clone()).await;
    assert_eq!(pb.data().unit.transform.x, 101.0);
    assert!(!pb.data().finished_loading);
    assert_eq!(pet.data().unit.transform.x, 102.0);
    assert_eq!(pet.data().xp_task, 0);
    // Ordinary motion still moves the authenticated character.
    crate::world::session::move_unit(&server, &mut a.conn, PA, walking).await;
    assert_eq!(pa.data().unit.transform.x, 500.0);

    let mut hull = Slave::new(2, 1);
    hull.unit.hp = 1000;
    hull.unit.transform.set_xyz(510.0, 500.0, 99.0);
    hull.unit.tl_id = 3;
    hull.summoner_obj_id = PB;
    let slave = Entity::new(0x301, hull);
    world.spawn(&slave, &server.view_ctx());
    let ship = MoveType::new(MoveKind::ShipRequest(ShipRequestMove {
        throttle: 1,
        steering: 1,
    }));
    crate::world::session::move_unit(&server, &mut a.conn, slave.obj_id, ship.clone()).await;
    assert_eq!(slave.data().throttle_request, 0);
    assert_eq!(pa.data().unit.transform.parent_obj_id, 0);
    let car = MoveType::new(MoveKind::Vehicle(VehicleMove {
        x: 700.0,
        y: 700.0,
        z: 100.0,
        ..Default::default()
    }));
    crate::world::session::move_unit(&server, &mut a.conn, slave.obj_id, car.clone()).await;
    assert_eq!(slave.data().unit.transform.x, 510.0);
    super::packets::CSDespawnSlavePacket {
        slave_obj_id: slave.obj_id,
    }
    .handle(&server, &mut a.conn)
    .await
    .unwrap();
    assert!(world.slave(slave.obj_id).is_some());

    // A passenger is not the driver, even when attached to the same vehicle.
    stand_next_to(&world, &pa, &WorldObject::from(&slave));
    slaves::bind_slave(
        &server,
        &pa,
        slave.obj_id,
        attach_point::PASSENGER0,
        7,
        Some(3.0),
    );
    crate::world::session::move_unit(&server, &mut a.conn, slave.obj_id, ship.clone()).await;
    assert_eq!(slave.data().throttle_request, 0);
    slaves::unbind_slave(&server, &pa, 3, slaves::attach_reason::SLAVE_BINDING);
    slaves::bind_slave(
        &server,
        &pa,
        slave.obj_id,
        attach_point::DRIVER,
        7,
        Some(4.0),
    );
    crate::world::session::move_unit(&server, &mut a.conn, slave.obj_id, ship).await;
    assert_eq!(slave.data().throttle_request, 1);
    crate::world::session::move_unit(&server, &mut a.conn, slave.obj_id, car).await;
    assert_eq!(slave.data().unit.transform.x, 700.0);
}

#[tokio::test]
async fn slave_driver_ignores_late_walking_and_reattaches_in_the_correct_frame() {
    use crate::models::movement::{ShipRequestMove, UnitMove};
    use crate::models::slave::Slave;
    use crate::world::Entity;

    let server = server_with(GameData::empty());
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Sailor", 1002.0, 1001.0),
        PA,
    );
    client.conn.active_char = Some(player.clone());
    let world = server.world.world(0).unwrap();
    let mut hull = Slave::new(1, 1);
    hull.unit.hp = 1000;
    hull.unit
        .transform
        .set_position(1000.0, 1000.0, 99.0, 0.0, 0.0, std::f32::consts::FRAC_PI_2);
    let slave = Entity::new(0x300, hull);
    world.spawn(&slave, &server.view_ctx());
    slave.data().unit.tl_id = 3;
    slaves::bind_slave(
        &server,
        &player,
        slave.obj_id,
        attach_point::DRIVER,
        7,
        Some(4.0),
    );

    // The real client can send its last swimming/walking position just after boarding.
    // It must not replace the seat's local transform with that world position.
    let walking = MoveType::new(MoveKind::Unit(UnitMove {
        x: 1002.0,
        y: 1001.0,
        z: 100.0,
        ..Default::default()
    }));
    crate::world::session::move_unit(&server, &mut client.conn, PA, walking.clone()).await;
    let t = player.data().unit.transform;
    assert_eq!(
        (t.parent_obj_id, t.x, t.y, t.z),
        (slave.obj_id, 0.0, 0.0, 0.0)
    );

    // Recovery from a missing transform parent preserves world position. Keeping the
    // world coordinates as local coordinates previously moved drivers outside the map.
    let me = WorldObject::from(&player);
    world.detach_child(&me);
    me.with_transform(|t| t.set_position(1002.0, 1001.0, 100.0, 0.0, 0.0, 0.0));
    let request = MoveType::new(MoveKind::ShipRequest(ShipRequestMove::default()));
    crate::world::session::move_unit(&server, &mut client.conn, slave.obj_id, request).await;
    let t = player.data().unit.transform;
    assert_eq!(t.parent_obj_id, slave.obj_id);
    assert!((t.x - 1.0).abs() < 1e-4 && (t.y + 2.0).abs() < 1e-4);
    let p = world.world_pos_rot(&me);
    assert!((p.x - 1002.0).abs() < 1e-4 && (p.y - 1001.0).abs() < 1e-4);
    assert_eq!(p.z, 100.0);

    // After the ordinary discard, walking packets resume applying world coordinates.
    slaves::unbind_slave(&server, &player, 3, slaves::attach_reason::SLAVE_BINDING);
    crate::world::session::move_unit(&server, &mut client.conn, PA, walking).await;
    let t = player.data().unit.transform;
    assert_eq!((t.parent_obj_id, t.x, t.y, t.z), (0, 1002.0, 1001.0, 100.0));
}

#[tokio::test]
async fn deck_movement_uses_relative_coordinates_and_keeps_the_current_parent() {
    use crate::models::movement::{MoveTypeActorFlags, MoveTypeFlags, UnitMove};
    use crate::models::slave::Slave;
    use crate::world::Entity;

    let server = server_with(GameData::empty());
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Sailor", 1002.0, 1001.0),
        PA,
    );
    client.conn.active_char = Some(player.clone());
    let world = server.world.world(0).unwrap();
    let mut hull = Slave::new(1, 1);
    hull.unit.hp = 1000;
    hull.unit
        .transform
        .set_position(1000.0, 1000.0, 99.0, 0.0, 0.0, std::f32::consts::FRAC_PI_2);
    let slave = Entity::new(0x300, hull);
    world.spawn(&slave, &server.view_ctx());
    let mut move_type = MoveType::new(MoveKind::Unit(UnitMove {
        // Captured 1.2 gcFlags=1 layout: primary local, auxiliary world.
        x: 1.0,
        y: -2.0,
        z: 1.0,
        rotation_z: 32,
        actor_flags: MoveTypeActorFlags::STANDING_ON_OBJECT,
        gc_flags: 1,
        gc_id: slave.obj_id,
        x2: 1002.0,
        y2: 1001.0,
        z2: 100.0,
        rotation_z2: 64,
        ..Default::default()
    }));
    move_type.flags = MoveTypeFlags::STANDING_ON_OBJECT;
    crate::world::session::move_unit(&server, &mut client.conn, PA, move_type.clone()).await;
    let t = player.data().unit.transform;
    assert_eq!(
        (t.parent_obj_id, t.x, t.y, t.z),
        (slave.obj_id, 1.0, -2.0, 1.0)
    );
    assert!((t.rot_z - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
    let p = world.world_pos_rot(&WorldObject::from(&player));
    assert!((p.x - 1002.0).abs() < 1e-4 && (p.y - 1001.0).abs() < 1e-4);
    assert_eq!(p.z, 100.0);
    // The next update references its current supporting object with the gc sentinel.
    let MoveKind::Unit(u) = &mut move_type.kind else {
        unreachable!()
    };
    u.gc_id = 1;
    u.x = 2.0;
    u.y = -3.0;
    u.x2 = 1003.0;
    u.y2 = 1002.0;
    crate::world::session::move_unit(&server, &mut client.conn, PA, move_type).await;
    let t = player.data().unit.transform;
    assert_eq!((t.parent_obj_id, t.x, t.y), (slave.obj_id, 2.0, -3.0));
    // Walking off the deck restores ordinary world coordinates.
    let mut off = MoveType::new(MoveKind::Unit(UnitMove {
        x: 1004.0,
        y: 1003.0,
        z: 100.0,
        ..Default::default()
    }));
    // The real client can retain the header flag for the first off-deck update,
    // while the actor no longer carries a gc block.
    off.flags = MoveTypeFlags::STANDING_ON_OBJECT;
    crate::world::session::move_unit(&server, &mut client.conn, PA, off).await;
    let t = player.data().unit.transform;
    assert_eq!((t.parent_obj_id, t.x, t.y, t.z), (0, 1004.0, 1003.0, 100.0));
    // A late deck update after the supporting object was removed uses the world
    // backup, not its small local offset.
    let mut late = MoveType::new(MoveKind::Unit(UnitMove {
        x: 1.0,
        y: -2.0,
        z: 1.0,
        actor_flags: MoveTypeActorFlags::STANDING_ON_OBJECT,
        gc_flags: 1,
        gc_id: 0x999,
        x2: 1005.0,
        y2: 1004.0,
        z2: 100.0,
        rotation_z2: 32,
        ..Default::default()
    }));
    late.flags = MoveTypeFlags::STANDING_ON_OBJECT;
    crate::world::session::move_unit(&server, &mut client.conn, PA, late).await;
    let t = player.data().unit.transform;
    assert_eq!((t.parent_obj_id, t.x, t.y, t.z), (0, 1005.0, 1004.0, 100.0));
    assert!((t.rot_z - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
}

#[tokio::test]
async fn slave_death_marks_the_summon_item_destroyed() {
    let server = server_with(slave_data());
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), SLAVE_ITEM);
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let item = mates::owned_item(&server, &pa, item_id).unwrap();
    let slave = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SLAVE_TEMPLATE,
        Some(item),
        false,
        None,
        None,
    )
    .unwrap();
    slave.data().unit.max_hp = 100;
    slave.data().unit.hp = 100;
    a.drain();
    let obj = WorldObject::from(&slave);
    let killer = WorldObject::from(&pa);
    crate::systems::skills::combat::reduce_current_hp(
        &server,
        &obj,
        Some(&killer),
        500,
        crate::systems::skills::types::KillReason::Damage,
    );
    let f = a.drain();
    let o = ops(&f);
    assert!(o.contains(&sc::SCUnitDeathPacket));
    assert!(o.contains(&sc::SCMySlavePacket));
    assert!(o.contains(&sc::SCSlaveRemovedPacket));
    // MarkSummoningItemAsDestroyed -> ItemTaskType.MateDeath (59)
    let tasks: Vec<&Vec<u8>> = f
        .iter()
        .filter(|x| x.0 == sc::SCItemTaskSuccessPacket)
        .map(|x| &x.1)
        .collect();
    assert_eq!(tasks.last().unwrap()[0], 59);
    let item = mates::owned_item(&server, &pa, item_id).unwrap();
    assert_eq!(item.details.slave_is_destroyed, 1);
    let now = chrono::Utc::now().timestamp();
    // The death starts the 10 minute repair time (the C# left it at DateTime.MinValue, so a
    // destroyed vehicle could be summoned again immediately).
    assert!(item.details.slave_repair_start_time > 0);
    assert!(item.details.slave_repair_start_time <= now);
    // A destroyed vehicle cannot be summoned again before it is repaired.
    assert!(slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SLAVE_TEMPLATE,
        Some(item.clone()),
        false,
        None,
        None,
    )
    .is_none());
    let f = a.drain();
    let e = body(&f, sc::SCErrorMsgPacket);
    assert_eq!(&e[..2], &540i16.to_le_bytes());
    let left = u32::from_le_bytes(e[4..8].try_into().unwrap());
    assert!((597..=600).contains(&left), "{left}");
    let sm = server.systems.npcs.spawns.get(0).unwrap();
    assert!(sm
        .pending_despawns()
        .iter()
        .any(|(id, _)| *id == slave.obj_id));
    crate::systems::npcs::spawn_manager::advance_test_clock(Duration::from_secs(21));
    sm.check_respawns_tick(&server);
    a.drain();
    // The despawn does not shorten the repair time.
    let item = mates::owned_item(&server, &pa, item_id).unwrap();
    assert!(slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SLAVE_TEMPLATE,
        Some(item),
        false,
        None,
        None,
    )
    .is_none());
    a.drain();
    // After the repair time has passed, the same vehicle can be summoned again; the successful
    // summon clears the repair time (the destroyed flag stays until a repair effect clears it).
    mates::update_owned_item(&server, Some(&pa), item_id, |i| {
        i.details.slave_repair_start_time = chrono::Utc::now().timestamp() - 600;
    });
    let item = mates::owned_item(&server, &pa, item_id).unwrap();
    assert!(slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SLAVE_TEMPLATE,
        Some(item),
        false,
        None,
        None,
    )
    .is_some());
    let item = mates::owned_item(&server, &pa, item_id).unwrap();
    assert_eq!(item.details.slave_repair_start_time, 0);
    assert_eq!(item.details.slave_is_destroyed, 1);
}

#[tokio::test]
async fn a_zero_hp_slave_load_needs_the_repair_time_before_it_returns_at_full_hp() {
    let server = server_with(slave_data());
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), SLAVE_ITEM);
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    a.drain();
    // A destroyed vehicle saved at 0 HP (the row a save tick writes after the death).
    mates::update_owned_item(&server, Some(&pa), item_id, |i| {
        i.details.slave_is_destroyed = 1;
        i.details.slave_repair_start_time = chrono::Utc::now().timestamp();
    });
    let saved = slaves::SavedSlave {
        id: 77,
        name: "Ship".into(),
        hp: 0,
        mp: 0,
    };
    let spawn = |saved: slaves::SavedSlave| {
        let item = mates::owned_item(&server, &pa, item_id).unwrap();
        slaves::create_core(
            &server,
            &world,
            Some(&pa),
            None,
            SLAVE_TEMPLATE,
            Some(item),
            false,
            None,
            Some(saved),
        )
    };
    // Before the repair time: no summon and no free full-HP ship.
    assert!(spawn(saved.clone()).is_none());
    let f = a.drain();
    let e = body(&f, sc::SCErrorMsgPacket);
    assert_eq!(&e[..2], &540i16.to_le_bytes());
    assert!(server.world.world(0).unwrap().all_slaves().is_empty());
    // After it: the C# 0-HP rule brings the repaired vehicle back at full HP.
    mates::update_owned_item(&server, Some(&pa), item_id, |i| {
        i.details.slave_repair_start_time = chrono::Utc::now().timestamp() - 600;
    });
    let slave = spawn(saved).expect("summon after the repair time");
    let d = slave.data();
    assert!(d.unit.hp > 0 && d.unit.hp == d.unit.max_hp);
    let item = mates::owned_item(&server, &pa, item_id).unwrap();
    assert_eq!(item.details.slave_repair_start_time, 0);
}

fn transfer_data() -> GameData {
    let mut d = GameData::empty();
    d.formulas = pool_formulas();
    let pos = |x: f32, y: f32| WorldSpawnPosition {
        world_id: 0,
        zone_id: TEST_ZONE,
        x,
        y,
        z: 100.0,
        ..WorldSpawnPosition::default()
    };
    d.systems.vehicles.transfer_templates.insert(
        6,
        TransferTemplate {
            id: 6,
            name: "Loop Carriage".into(),
            model_id: 654,
            wait_time: 0.0,
            cyclic: true,
            path_smoothing: 2.0,
            transfer_all_paths: vec![TransferPath {
                id: 1,
                owner_id: 6,
                owner_type: "Transfer".into(),
                path_name: "road".into(),
                wait_time_start: 0.0,
                wait_time_end: 0.0,
            }],
            transfer_roads: vec![TransferRoad {
                name: "road".into(),
                pos: vec![pos(100.0, 100.0), pos(110.0, 100.0), pos(110.0, 110.0)],
                ..TransferRoad::default()
            }],
            ..TransferTemplate::default()
        },
    );
    d.systems
        .vehicles
        .model_types
        .insert(654, (54, "VehicleModel".into()));
    d.systems.vehicles.vehicle_models.insert(
        54,
        VehicleModel {
            id: 54,
            velocity: 2.0,
            ang_vel: 10.0,
            wheeled_vehicle_mass: 1.0,
            wheeled_vehicle_max_gear: 1,
            wheeled_vehicle_gear_speed_ratio_1: 1.0,
            ..VehicleModel::default()
        },
    );
    d
}

#[tokio::test]
async fn transfer_follows_its_route() {
    let server = server_with(transfer_data());
    let mut b = TestClient::new(&server, 2, 2);
    spawn_player(&server, &b, test_character(2, "B", 105.0, 105.0), PB);
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    b.drain();
    let def = SimpleSpawnerDef {
        base: SpawnerBase {
            id: 1,
            unit_id: 6,
            position: WorldSpawnPosition {
                world_id: 0,
                zone_id: TEST_ZONE,
                x: 101.0,
                y: 101.0,
                z: 100.0,
                ..WorldSpawnPosition::default()
            },
            ..SpawnerBase::default()
        },
        count: 1,
    };
    assert_eq!(transfers::spawn_all(&server, &world, vec![def]), 1);
    let motor = server
        .systems
        .vehicles
        .transfers
        .get(0)
        .get_transfers()
        .remove(0);
    {
        let t = motor.data();
        assert!(t.unit.is_in_patrol);
        assert_eq!((t.unit.transform.x, t.unit.transform.y), (100.0, 100.0));
        // nearest checkpoint: the first point (distance 0)
        assert_eq!(t.move_step_index, 0);
        assert_eq!(t.v_target, [100.0, 100.0, 100.0]);
        assert_eq!(t.unit.owner_id, 255);
        assert!(
            t.unit.buffs.check_buff(slaves::buff::UNTOUCHABLE)
                || server
                    .game_data
                    .systems
                    .skills
                    .buff(slaves::buff::UNTOUCHABLE)
                    .is_none()
        );
    }
    assert!(ops(&b.drain()).contains(&sc::SCUnitStatePacket));

    // Tick 1: at the checkpoint -> next point (110, 100).
    transfers::move_to(&server, &motor);
    assert_eq!(motor.data().move_step_index, 1);
    assert_eq!(motor.data().v_target, [110.0, 100.0, 100.0]);
    // Following ticks move toward it and broadcast SCOneUnitMovementPacket (transfer type 5).
    let mut moved = 0;
    for _ in 0..50 {
        transfers::move_to(&server, &motor);
        let f = b.drain();
        for (op, body) in &f {
            if *op == sc::SCOneUnitMovementPacket {
                assert_eq!(&body[..3], &bc(motor.obj_id)[..]);
                assert_eq!(body[3], 5);
                moved += 1;
            }
        }
        if motor.data().move_step_index == 2 {
            break;
        }
    }
    assert!(moved > 0);
    let t = motor.data();
    assert_eq!(
        t.move_step_index, 2,
        "reached (110, 100), heading to (110, 110)"
    );
    assert!((t.unit.transform.x - 110.0).abs() < 1.6);
    assert_eq!(t.v_target, [110.0, 110.0, 100.0]);
    // Moving north now: angle pi/2, Rot = (0, 0, sin(yaw'/2), cos(yaw'/2)) of the client yaw.
    drop(t);
    transfers::move_to(&server, &motor);
    let t = motor.data();
    assert!((t.angle - std::f64::consts::FRAC_PI_2).abs() < 0.2);
    assert_eq!(t.speed, 2.0);
}

#[test]
fn transfer_rotation_helpers() {
    // CalculateDirection: atan2(dy, dx)
    assert!(
        (transfers::calculate_direction([0.0; 3], [0.0, 1.0, 0.0]) - std::f64::consts::FRAC_PI_2)
            .abs()
            < 1e-12
    );
    // ConvertRadianToDirectionShort(pi/2): yaw = pi/2 - 3.14159/2 ~ 0 -> identity
    let q = transfers::convert_radian_to_direction_short(std::f64::consts::FRAC_PI_2);
    assert!(q[1].abs() < 1e-5 && (q[3] - 1.0).abs() < 1e-5);
    // negative angles wrap by 2 * 3.14159 first
    let q = transfers::convert_radian_to_direction_short(-std::f64::consts::FRAC_PI_2);
    #[allow(clippy::approx_constant)]
    let yaw = (-std::f64::consts::FRAC_PI_2 + 3.14159 * 2.0 - 3.14159 * 0.5) as f32;
    assert!((q[1] - (yaw * 0.5).sin()).abs() < 1e-6);
}

#[derive(Default)]
struct ShipItems {
    consumed: Mutex<Vec<(u8, u32, i32)>>,
    acquired: Mutex<Vec<(u8, u32, i32)>>,
}

impl ItemSkillHooks for ShipItems {
    fn item_count(&self, _server: &GameServer, _player: &PlayerRef, _template_id: u32) -> i32 {
        5
    }
    fn item_use_skill(&self, _server: &GameServer, template_id: u32) -> Option<(u32, bool)> {
        (template_id == 7000).then_some((8000, false))
    }
    fn consume_bag_item(
        &self,
        _server: &GameServer,
        _player: &PlayerRef,
        task_type: u8,
        template_id: u32,
        amount: i32,
    ) -> i32 {
        self.consumed
            .lock()
            .unwrap()
            .push((task_type, template_id, amount));
        amount
    }
    fn consume_bag_item_by_id(
        &self,
        _server: &GameServer,
        _player: &PlayerRef,
        task_type: u8,
        item_id: u64,
        expected_template_id: u32,
        amount: i32,
    ) -> Option<bool> {
        // The one named bag item, not just any item of the template.
        if item_id != DESIGN_ITEM || expected_template_id != 7000 {
            return Some(false);
        }
        self.consumed
            .lock()
            .unwrap()
            .push((task_type, expected_template_id, amount));
        Some(true)
    }
    fn acquire_default_item(
        &self,
        _server: &GameServer,
        _player: &PlayerRef,
        task_type: u8,
        template_id: u32,
        amount: i32,
    ) -> bool {
        self.acquired
            .lock()
            .unwrap()
            .push((task_type, template_id, amount));
        true
    }
}

#[tokio::test]
async fn shipyard_build_steps_progress() {
    let mut d = GameData::empty();
    d.formulas = pool_formulas();
    let step = |s: i32, model: u32, n: i32| {
        (
            s,
            ShipyardStep {
                id: s as u32 + 1,
                shipyard_id: 3,
                step: s,
                model_id: model,
                skill_id: 9000 + s as u32,
                num_actions: n,
                max_hp: 10,
            },
        )
    };
    d.systems.vehicles.shipyard_templates.insert(
        3,
        ShipyardTemplate {
            id: 3,
            name: "Clipper Shipyard".into(),
            main_model_id: 500,
            item_id: 7001,
            ceremony_anim_time: 0,
            origin_item_id: 7000,
            taxation_id: 2,
            steps: vec![step(0, 501, 2), step(1, 502, 1)],
            ..ShipyardTemplate::default()
        },
    );
    d.systems.vehicles.taxations.insert(2, 1000);
    d.systems.vehicles.item_shipyards.insert(7000, 3);
    d.items.templates.insert(7000, ItemTemplate::plain(7000, 1));
    let server = server_with(d);
    let hooks = Arc::new(ShipItems::default());
    server.systems.skills.ext.items.install(hooks.clone());
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let pa = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    spawn_player(&server, &b, test_character(2, "B", 110.0, 110.0), PB);
    pa.data().money = 5000;
    give_item(&server, &pa, DESIGN_ITEM, 7000);
    a.drain();
    b.drain();

    let sy = shipyards::create(&server, &pa, 3, 120.0, 120.0, 100.0, 0.5, DESIGN_ITEM, 0)
        .expect("shipyard");
    assert_eq!(pa.data().money, 4000);
    assert_eq!(
        *hooks.consumed.lock().unwrap(),
        vec![(60, 7000, 1)],
        "the design is consumed (no reagents)"
    );
    let fa = a.drain();
    assert!(ops(&fa).contains(&sc::SCItemTaskSuccessPacket));
    assert!(ops(&fa).contains(&sc::SCShipyardStatePacket));
    assert!(ops(&b.drain()).contains(&sc::SCShipyardStatePacket));
    {
        let s = sy.data();
        assert_eq!(s.unit.model_id, 501);
        assert_eq!(s.all_action, 3);
        assert_eq!(s.data.hp, 1000);
        assert_eq!(s.data.owner_name, "Owner");
        assert_eq!(s.data.kind, 7000);
        assert_eq!(s.data.kind2, 1);
    }
    assert_eq!(shipyards::current_step_template(&server, &sy), Some(9000));

    // Step 0 needs 2 actions.
    shipyards::add_build_action(&server, &sy);
    shipyards::sync_build_state(&server, &sy);
    {
        let s = sy.data();
        assert_eq!((s.current_step, s.num_action), (0, 1));
        assert_eq!((s.data.actions, s.data.step), (1, 0));
    }
    shipyards::add_build_action(&server, &sy);
    shipyards::sync_build_state(&server, &sy);
    {
        let s = sy.data();
        assert_eq!((s.current_step, s.num_action, s.base_action), (1, 0, 2));
        assert_eq!(s.unit.model_id, 502);
        assert_eq!((s.data.actions, s.data.step), (2, 1));
    }
    assert_eq!(shipyards::current_step_template(&server, &sy), Some(9001));
    // Last action: done (-1, main model), ShipyardData reports all actions / step count.
    shipyards::add_build_action(&server, &sy);
    shipyards::sync_build_state(&server, &sy);
    {
        let s = sy.data();
        assert_eq!(s.current_step, -1);
        assert_eq!(s.unit.model_id, 500);
        assert_eq!((s.data.actions, s.data.step), (3, 2));
    }
    // Further actions are ignored.
    shipyards::add_build_action(&server, &sy);
    assert_eq!(sy.data().current_step, -1);

    // Launch ceremony: the ship item, step 1000 broadcast; the shipyard is removed afterwards.
    a.drain();
    shipyards::shipyard_completed_task(&server, &sy);
    assert_eq!(*hooks.acquired.lock().unwrap(), vec![(60, 7001, 1)]);
    let f = a.drain();
    let st = body(&f, sc::SCShipyardStatePacket);
    assert_eq!(&st[st.len() - 4..], &1000i32.to_le_bytes());
    shipyards::shipyard_completed(&server, &sy).await;
    assert!(server.systems.vehicles.shipyards.all().is_empty());
    assert!(server.world.world(0).unwrap().shipyard(sy.obj_id).is_none());
}

#[tokio::test]
async fn shipyard_row_round_trips_and_removal_queues_delete() {
    use crate::systems::vehicles::shipyards_db;
    let mut d = GameData::empty();
    d.formulas = pool_formulas();
    let step = |s: i32, model: u32, n: i32| {
        (
            s,
            ShipyardStep {
                id: s as u32 + 1,
                shipyard_id: 3,
                step: s,
                model_id: model,
                skill_id: 9000 + s as u32,
                num_actions: n,
                max_hp: 10,
            },
        )
    };
    d.systems.vehicles.shipyard_templates.insert(
        3,
        ShipyardTemplate {
            id: 3,
            main_model_id: 500,
            item_id: 7001,
            origin_item_id: 7000,
            taxation_id: 2,
            steps: vec![step(0, 501, 2), step(1, 502, 3)],
            ..ShipyardTemplate::default()
        },
    );
    d.systems.vehicles.taxations.insert(2, 1000);
    d.systems.vehicles.item_shipyards.insert(7000, 3);
    d.items.templates.insert(7000, ItemTemplate::plain(7000, 1));
    let server = server_with(d);
    let hooks = Arc::new(ShipItems::default());
    server.systems.skills.ext.items.install(hooks.clone());
    let a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    pa.data().money = 5000;
    give_item(&server, &pa, DESIGN_ITEM, 7000);

    let sy = shipyards::create(&server, &pa, 3, 120.0, 130.0, 100.0, 0.5, DESIGN_ITEM, 0)
        .expect("shipyard");
    // Two actions finish step 0, one more is the first action of step 1.
    for _ in 0..3 {
        shipyards::add_build_action(&server, &sy);
        shipyards::sync_build_state(&server, &sy);
    }
    sy.data().unit.hp -= 5;
    assert!(sy.data().is_dirty);
    let row = shipyards_db::snapshot(&sy);
    assert_eq!((row.current_step, row.num_action), (1, 1));
    assert_eq!((row.owner_id, row.owner_name.as_str()), (1, "Owner"));

    // The shipyard is gone (restart) and its row is queued for deletion.
    let ship_id = row.id;
    shipyards::remove_shipyard(&server, &sy);
    assert_eq!(
        server.systems.vehicles.shipyards.pending_delete(),
        vec![ship_id]
    );
    assert!(server.systems.vehicles.shipyards.all().is_empty());
    let (money, consumed) = (pa.data().money, hooks.consumed.lock().unwrap().clone());

    assert_eq!(
        shipyards_db::restore_all(&server, std::slice::from_ref(&row)),
        1
    );
    let all = server.systems.vehicles.shipyards.all();
    assert_eq!(all.len(), 1);
    let restored = &all[0];
    assert_ne!(restored.obj_id, 0);
    assert!(server
        .world
        .world(0)
        .unwrap()
        .shipyard(restored.obj_id)
        .is_some());
    {
        let s = restored.data();
        assert_eq!(s.data.id, u64::from(ship_id));
        assert_eq!(s.data.obj_id, restored.obj_id);
        assert_eq!(
            (s.data.x, s.data.y, s.data.z, s.data.z_rot),
            (120.0, 130.0, 100.0, 0.5)
        );
        assert_eq!((s.current_step, s.num_action, s.base_action), (1, 1, 2));
        assert_eq!(s.all_action, 5);
        assert_eq!(s.unit.model_id, 502, "the model of the restored step");
        assert_eq!((s.data.kind2, s.data.owner_name.as_str()), (1, "Owner"));
        assert_eq!(s.data.spawned, row.spawned);
        assert!(!s.is_dirty);
    }
    assert_eq!(
        shipyards_db::snapshot(restored),
        row,
        "row -> shipyard -> row"
    );
    // Loading consumes no money or items.
    assert_eq!(pa.data().money, money);
    assert_eq!(*hooks.consumed.lock().unwrap(), consumed);
    // The restored shipyard continues to build.
    shipyards::add_build_action(&server, restored);
    assert_eq!(restored.data().num_action, 2);

    // The id is live again: the queued delete of the old row must not hit the new shipyard.
    assert!(server
        .systems
        .vehicles
        .shipyards
        .pending_delete()
        .contains(&ship_id));
    assert!(server
        .systems
        .vehicles
        .shipyards
        .take_pending_delete()
        .is_empty());

    // A finished shipyard (`current_step == -1`) is not restored; its row is deleted.
    let done = shipyards_db::ShipyardRow {
        id: 99,
        current_step: -1,
        ..row
    };
    assert_eq!(shipyards_db::restore_all(&server, &[done]), 0);
    assert_eq!(server.systems.vehicles.shipyards.all().len(), 1);
    assert_eq!(
        server.systems.vehicles.shipyards.take_pending_delete(),
        vec![99]
    );
}

fn effect_args<'a>(
    caster: &WorldObject,
    target: crate::systems::skills::geometry::Target,
    target_caster: crate::systems::skills::types::SkillCastTarget,
    so: &'a crate::systems::skills::types::SkillObject,
) -> crate::systems::skills::effects::EffectArgs<'a> {
    crate::systems::skills::effects::EffectArgs {
        caster: Some(caster.clone()),
        caster_caster: crate::systems::skills::types::SkillCaster::Unit(caster.obj_id()),
        target,
        target_caster,
        cast: crate::systems::skills::types::CastAction::Skill {
            skill_id: 1,
            tl_id: 0,
        },
        source: crate::systems::skills::effects::EffectSource::default(),
        skill_object: so,
    }
}

#[tokio::test]
async fn repair_slave_effect_and_repair_cooldown() {
    use crate::systems::skills::data::{EffectKind, EffectTemplate};
    use crate::systems::skills::types::SkillCastTarget;
    let mut d = slave_data();
    d.systems
        .vehicles
        .repairable_slaves
        .insert(SLAVE_TEMPLATE, 77);
    let server = server_with(d);
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), SLAVE_ITEM);
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    a.drain();
    let me = WorldObject::from(&pa);
    let so = Default::default();
    let target = SkillCastTarget::Item {
        obj_id: PA,
        id: item_id,
        type1: 0,
        type2: 0,
    };
    // Wrong repair effect: ItemFailedRepair (23).
    let args = effect_args(
        &me,
        crate::systems::skills::geometry::Target::Obj(me.clone()),
        target,
        &so,
    );
    let wrong = EffectTemplate::RepairSlave {
        id: 78,
        health: 0,
        mana: 0,
    };
    assert!(super::effects::apply_effect(
        &server,
        EffectKind::RepairSlaveEffect,
        &wrong,
        &args
    ));
    let f = a.drain();
    assert_eq!(&body(&f, sc::SCErrorMsgPacket)[..2], &23i16.to_le_bytes());
    // The right one: IsDestroyed = 0, RepairStartTime = now, ItemTaskType.RepairSlaves (74).
    let right = EffectTemplate::RepairSlave {
        id: 77,
        health: 0,
        mana: 0,
    };
    assert!(super::effects::apply_effect(
        &server,
        EffectKind::RepairSlaveEffect,
        &right,
        &args
    ));
    let f = a.drain();
    let task = body(&f, sc::SCItemTaskSuccessPacket);
    assert_eq!(task[0], 74);
    let item = mates::owned_item(&server, &pa, item_id).unwrap();
    assert!(item.details.slave_repair_start_time > 0);
    // The slave detail block is always 29 bytes (the 1.2 client reader takes a fixed 29 for
    // detail tag 2), with the repair time inside it.
    assert_eq!(item.detail_type_and_details().len(), 1 + 29);
    // Summoning during the 10 minute repair: SlaveSpawnErrorNeedRepairTime (540) with the
    // seconds left in the error's type field.
    assert!(slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SLAVE_TEMPLATE,
        Some(item),
        false,
        None,
        None,
    )
    .is_none());
    let f = a.drain();
    let e = body(&f, sc::SCErrorMsgPacket);
    assert_eq!(&e[..2], &540i16.to_le_bytes());
    let left = u32::from_le_bytes(e[4..8].try_into().unwrap());
    assert!((598..=600).contains(&left), "{left}");
}

#[tokio::test]
async fn spawn_effect_creates_an_ownerless_test_slave() {
    use crate::systems::skills::data::effects::SpawnEffect;
    use crate::systems::skills::data::{EffectKind, EffectTemplate};
    let server = server_with(slave_data());
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "P1", 100.0, 100.0), PA);
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    a.drain();
    let me = WorldObject::from(&pa);
    let so = Default::default();
    let args = effect_args(
        &me,
        crate::systems::skills::geometry::Target::Obj(me.clone()),
        crate::systems::skills::types::SkillCastTarget::Unit(PA),
        &so,
    );
    let spawn = SpawnEffect {
        id: 1,
        owner_type_id: 2,
        sub_type: SLAVE_TEMPLATE,
        pos_dir_id: 0,
        pos_angle: 0.0,
        pos_distance: 0.0,
        ori_dir_id: 0,
        ori_angle: 90.0,
        use_summoner_faction: false,
        life_time: 0.0,
        despawn_on_creator_death: false,
        use_summoner_aggro_target: false,
        mate_state_id: 0,
    };
    let t = EffectTemplate::Spawn(spawn.clone());
    assert!(super::effects::apply_effect(
        &server,
        EffectKind::SpawnEffect,
        &t,
        &args
    ));
    let slave = slaves::all_slaves(&world).pop().expect("test slave");
    {
        let d = slave.data();
        assert_eq!(d.summoner_obj_id, 0, "owner-less");
        assert_eq!(d.id, 0, "not persistent");
        assert_eq!((d.unit.transform.x, d.unit.transform.y), (100.0, 102.0));
        assert!((d.unit.transform.rot_z - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        assert_ne!(d.leave_task, 0, "despawn task scheduled");
    }
    let o = ops(&a.drain());
    assert!(o.contains(&sc::SCUnitStatePacket));
    assert!(!o.contains(&sc::SCSlaveCreatedPacket));
    // An NPC spawn effect is not ours.
    let npc = EffectTemplate::Spawn(SpawnEffect {
        owner_type_id: 1,
        ..spawn
    });
    assert!(!super::effects::apply_effect(
        &server,
        EffectKind::SpawnEffect,
        &npc,
        &args
    ));
}

/// `SpawnEffect` (slave) dir ids: pos 1 = relative to the effect target, 2 = relative to the
/// caster, with `pos_angle`; ori 1 = the target's yaw, 2 = the caster's, plus `ori_angle`.
#[tokio::test]
async fn spawn_effect_slave_honours_pos_and_ori_dirs() {
    use crate::systems::skills::data::effects::SpawnEffect;
    use crate::systems::skills::data::{EffectKind, EffectTemplate};
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::types::SkillCastTarget;

    let server = server_with(slave_data());
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let pa = spawn_player(&server, &a, test_character(1, "P1", 100.0, 100.0), PA);
    let pb = spawn_player(&server, &b, test_character(2, "P2", 140.0, 100.0), PB);
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    pa.data().unit.transform.rot_z = 0.0;
    pb.data().unit.transform.rot_z = std::f32::consts::FRAC_PI_2;
    a.drain();
    b.drain();
    let me = WorldObject::from(&pa);
    let so = Default::default();
    let args = effect_args(
        &me,
        Target::Obj(WorldObject::from(&pb)),
        SkillCastTarget::Unit(PB),
        &so,
    );
    let effect = |pos_dir_id, ori_dir_id| {
        EffectTemplate::Spawn(SpawnEffect {
            id: 1,
            owner_type_id: 2,
            sub_type: SLAVE_TEMPLATE,
            pos_dir_id,
            pos_angle: 0.0,
            pos_distance: 10.0,
            ori_dir_id,
            ori_angle: 0.0,
            use_summoner_faction: false,
            life_time: 0.0,
            despawn_on_creator_death: false,
            use_summoner_aggro_target: false,
            mate_state_id: 3,
        })
    };
    // Position relative to the target, yaw from the target.
    assert!(super::effects::apply_effect(
        &server,
        EffectKind::SpawnEffect,
        &effect(1, 1),
        &args
    ));
    // Position relative to the caster, yaw from the caster.
    assert!(super::effects::apply_effect(
        &server,
        EffectKind::SpawnEffect,
        &effect(2, 2),
        &args
    ));
    let slaves = slaves::all_slaves(&world);
    let at = |x: f32, y: f32| {
        slaves
            .iter()
            .find(|s| {
                let t = s.data().unit.transform;
                (t.x - x).abs() < 1e-4 && (t.y - y).abs() < 1e-4
            })
            .unwrap_or_else(|| {
                panic!(
                    "no slave at ({x},{y}): {:?}",
                    slaves
                        .iter()
                        .map(|s| s.data().unit.transform)
                        .collect::<Vec<_>>()
                )
            })
    };
    let from_target = at(130.0, 100.0);
    assert!(
        (from_target.data().unit.transform.rot_z - std::f32::consts::FRAC_PI_2).abs() < 1e-5,
        "ori dir 1 uses the target's yaw"
    );
    let from_caster = at(100.0, 110.0);
    assert!(
        from_caster.data().unit.transform.rot_z.abs() < 1e-5,
        "ori dir 2 uses the caster's yaw"
    );
}

#[tokio::test]
async fn destroy_and_spawn_slave_replaces_a_spawner_slave_and_destroys_an_orphan() {
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::types::SkillCastTarget;

    let server = server_with(slave_data());
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "P1", 100.0, 100.0), PA);
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    a.drain();

    // A spawner-owned slave at its authored transform.
    let spawner = Arc::new(slaves::SlaveSpawner::new(
        0,
        SimpleSpawnerDef {
            base: SpawnerBase {
                id: 55,
                unit_id: SLAVE_TEMPLATE,
                position: WorldSpawnPosition {
                    world_id: 0,
                    zone_id: TEST_ZONE,
                    x: 120.0,
                    y: 130.0,
                    z: 100.0,
                    ..WorldSpawnPosition::default()
                },
                ..SpawnerBase::default()
            },
            count: 1,
        },
    ));
    server
        .systems
        .vehicles
        .slave_spawners
        .insert(spawner.clone());
    let old_obj_id = spawner.spawn(&server).expect("spawner slave");
    let old = world.slave(old_obj_id).expect("old slave");
    let old_tl = old.data().unit.tl_id;
    old.data().unit.hp = 7;
    let me = WorldObject::from(&pa);
    let so = Default::default();
    let args = effect_args(
        &me,
        Target::Obj(WorldObject::from(&old)),
        SkillCastTarget::Unit(old_obj_id),
        &so,
    );
    slaves::destroy_and_spawn(&server, &args);

    let slaves_after = slaves::all_slaves(&world);
    assert_eq!(slaves_after.len(), 1);
    let new = &slaves_after[0];
    {
        let d = new.data();
        assert_eq!(d.spawner_id, 55);
        assert_ne!(d.unit.tl_id, old_tl, "a new spawn, not the destroyed one");
        assert_ne!(d.unit.hp, 7);
        assert_eq!(
            (d.unit.transform.x, d.unit.transform.y, d.unit.transform.z),
            (120.0, 130.0, 100.0)
        );
    }

    // No spawner: the slave is destroyed and nothing replaces it.
    let mut transform = crate::models::transform::Transform::default();
    transform.set_position(150.0, 150.0, 100.0, 0.0, 0.0, 0.0);
    let orphan = slaves::create_core(
        &server,
        &world,
        None,
        None,
        SLAVE_TEMPLATE,
        None,
        true,
        Some(transform),
        None,
    )
    .expect("ownerless slave");
    let orphan_id = orphan.obj_id;
    let args = effect_args(
        &me,
        Target::Obj(WorldObject::from(&orphan)),
        SkillCastTarget::Unit(orphan_id),
        &so,
    );
    slaves::destroy_and_spawn(&server, &args);
    assert!(world.slave(orphan_id).is_none());
    assert_eq!(slaves::all_slaves(&world).len(), 1);
}

#[tokio::test]
async fn kill_exp_death_despawn_and_force_dismount() {
    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    mates::spawn_mount(&server, &pa, item_id);
    let mate = mates::first_active_mate(&server, &pa).unwrap();
    let tl = u32::from(mate.data().unit.tl_id);
    a.drain();
    // Npc kill exp: every active mate gets its own SCExpChanged; the client formats the localized
    // pet-exp chat line from it, so no server "Pet gained" chat is sent (WP7).
    mates::reward_kill_exp(&server, &pa, 100);
    let f = a.drain();
    assert!(ops(&f).contains(&sc::SCExpChangedPacket));
    assert!(!ops(&f).contains(&sc::SCChatMessagePacket));
    assert_eq!(mate.data().experience, 100);
    // Team.IsObjMember helper.
    assert_eq!(
        mates::active_mate_owner_obj_id(&server, mate.obj_id),
        Some(PA)
    );

    // Stun / root while riding: thrown off; Template.Unmount only where we drive.
    mates::mount_mate(&server, &pa, tl, attach_point::DRIVER, 0);
    mates::unmount_riding_character(&server, &pa, true);
    assert!(!pa.data().is_riding);
    mates::mount_mate(&server, &pa, tl, attach_point::PASSENGER0, 0);
    mates::unmount_riding_character(&server, &pa, true);
    assert!(pa.data().is_riding, "not the driver");
    // ForceDismount: off any seat.
    assert!(super::force_dismount(
        &server,
        &pa,
        slaves::attach_reason::PREFAB_CHANGED
    ));
    assert!(!pa.data().is_riding);
    let f = a.drain();
    let mut det = bc(PA);
    det.push(slaves::attach_reason::PREFAB_CHANGED);
    assert!(f
        .iter()
        .any(|x| x.0 == sc::SCUnitDetachedPacket && x.1 == det));

    // DespawnMate on death: nothing while the character lives, the mate goes when it dies.
    pa.data().unit.hp = 100;
    mates::despawn_mate_on_death(&server, &WorldObject::from(&pa));
    assert!(mates::first_active_mate(&server, &pa).is_some());
    pa.data().unit.hp = 0;
    mates::despawn_mate_on_death(&server, &WorldObject::from(&pa));
    assert!(mates::first_active_mate(&server, &pa).is_none());
}

#[tokio::test]
async fn self_attributed_npc_death_does_not_touch_unrelated_mates() {
    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    mates::spawn_mount(&server, &pa, item_id);
    let mate = mates::first_active_mate(&server, &pa).expect("active mate");
    a.drain();

    let (_guard, logs) = capture_logs();
    let npc = crate::systems::npcs::npc::manager::create(&server, 0x200, MATE_NPC).expect("npc");
    let world = server.world.world(0).unwrap();
    world.spawn(&npc, &server.view_ctx());
    npc.data().unit.hp = 0;
    let npc_obj = WorldObject::from(&npc);
    // The actual `Unit.DoDie` path with the NPC as its own killer.
    crate::systems::skills::combat::do_die(
        &server,
        &npc_obj,
        Some(&npc_obj),
        crate::systems::skills::types::KillReason::Damage,
    );

    assert!(!logs.text().contains("DespawnMate"), "{}", logs.text());
    assert!(mates::first_active_mate(&server, &pa).is_some());
    assert!(world.mate(mate.obj_id).is_some());
}

#[tokio::test]
async fn orphan_mate_death_logs_mate_and_owner_ids_without_deleting_mates() {
    let server = server_with(data());
    let a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    mates::spawn_mount(&server, &pa, item_id);
    let mate = mates::first_active_mate(&server, &pa).expect("active mate");
    let owner_obj = mate.data().owner_obj_id;
    // A real orphan: the owner leaves the server-wide character map.
    assert!(server.world.try_remove_character(PA));

    let (_guard, logs) = capture_logs();
    mates::despawn_mate_on_death(&server, &WorldObject::from(&mate));

    let text = logs.text();
    assert!(
        text.contains("DespawnMate: no character (NullReferenceException)"),
        "{text}"
    );
    assert!(
        text.contains(&format!("mate obj_id {}", mate.obj_id)),
        "{text}"
    );
    assert!(
        text.contains(&format!("owner_obj_id {owner_obj}")),
        "{text}"
    );
    assert!(
        mates::first_active_mate(&server, &pa).is_some(),
        "a missing owner must not delete mates"
    );
}

#[tokio::test]
async fn logout_despawns_mates_and_vehicles() {
    let server = server_with(slave_data());
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    spawn_player(&server, &b, test_character(2, "B", 110.0, 110.0), PB);
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    mates::spawn_mount(&server, &pa, item_id);
    let slave = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SLAVE_TEMPLATE,
        None,
        false,
        None,
        None,
    )
    .unwrap();
    a.drain();
    b.drain();
    // +10 minutes of logout delay with an active vehicle.
    assert_eq!(super::leave_extra_logout_ms(&server, &pa), 600_000);
    super::on_leave_world(&server, &pa);
    // The active list entry keyed by the character id is removed together with the mate.
    assert!(mates::first_active_mate(&server, &pa).is_none());
    assert!(world.slave(slave.obj_id).is_none());
    let o = ops(&b.drain());
    assert!(o.contains(&sc::SCUnitsRemovedPacket));
    assert!(o.contains(&sc::SCSlaveDespawnPacket));
    assert_eq!(super::leave_extra_logout_ms(&server, &pa), 0);
}

#[tokio::test]
async fn npc_and_social_hooks_reach_the_vehicles() {
    let server = server_with(data());
    // The npcs / social hook implementations call systems::vehicles directly.
    crate::systems::npcs::install_hooks(&server);
    crate::systems::social::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    a.drain();
    let caster = crate::systems::skills::types::SkillCaster::Item {
        obj_id: PA,
        item_id,
        item_template_id: MATE_ITEM,
        type1: 0,
        type2: 0,
    };
    server
        .systems
        .skills
        .ext
        .npcs()
        .spawn_mount(&server, &pa, &caster);
    let mate = mates::first_active_mate(&server, &pa).expect("summoned through the hook");
    assert!(!server
        .systems
        .skills
        .ext
        .npcs()
        .mate_is_max_level(&server, &mate));
    // social ForceDismount through the social hook slot
    let tl = u32::from(mate.data().unit.tl_id);
    mates::mount_mate(&server, &pa, tl, attach_point::DRIVER, 0);
    server
        .systems
        .social
        .hooks
        .get()
        .force_dismount(&server, &pa);
    assert!(!pa.data().is_riding);
    // Forwarded defaults stay the defaults.
    assert_eq!(
        server
            .systems
            .skills
            .ext
            .npcs()
            .npc_engage_combat_quest(&server, &crate::models::npc::Npc::new(1)),
        0
    );
}

// ---------------------------------------------------------------------------------------------
// Real data (gated)
// ---------------------------------------------------------------------------------------------

fn app_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("app")
}

fn load_real(db: &str, sources: &[String]) -> super::Data {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let app = app_path();
    let files = ClientFileManager::from_sources(sources, &app);
    super::Data::load(&DataLoadCtx {
        conn: &conn,
        app_path: &app,
        files: &files,
        language: "en_us",
    })
    .expect("vehicles data")
}

#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_compact_db_vehicle_tables() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let d = load_real(&db, &[]);
    assert_eq!(d.slave_templates.len(), 140);
    assert_eq!(d.transfer_templates.len(), 117);
    assert_eq!(d.npc_mount_skills.len(), 986);
    assert_eq!(d.mount_skills.len(), 351);
    assert_eq!(d.mount_attached_skills.len(), 212);
    assert_eq!(d.slave_mount_skills.len(), 324);
    assert_eq!(d.repairable_slaves.len(), 29);
    assert_eq!(d.shipyard_templates.len(), 15);
    assert_eq!(
        d.shipyard_templates
            .values()
            .map(|t| t.steps.len())
            .sum::<usize>(),
        85
    );
    // `item_shipyards` maps the design items to their shipyards; 2-4 and 10 / 11 have no
    // `origin_item_id` and exist only through these rows.
    assert_eq!(d.item_shipyards.len(), 13);
    assert_eq!(d.item_shipyards.get(&16163), Some(&2));
    assert_eq!(d.item_shipyards.get(&15687), Some(&3));
    assert_eq!(d.item_shipyards.get(&15564), Some(&4));
    assert_eq!(d.item_shipyards.get(&18124), Some(&10));
    assert_eq!(d.item_shipyards.get(&18149), Some(&11));
    assert_eq!(d.shipyard_rewards.values().map(Vec::len).sum::<usize>(), 9);
    assert!(d
        .shipyard_rewards
        .values()
        .flatten()
        .all(|r| r.doodad_id == 1330 && r.on_water && r.count > 0));
    assert_eq!(d.taxations.len(), 11);
    // Bindings land on their templates.
    let bindings: usize = d
        .transfer_templates
        .values()
        .map(|t| t.transfer_bindings.len())
        .sum();
    assert!(bindings > 0 && bindings <= 56);
    let paths: usize = d
        .transfer_templates
        .values()
        .map(|t| t.transfer_all_paths.len())
        .sum();
    assert!(paths > 0 && paths <= 444);
    let drops: usize = d
        .slave_templates
        .values()
        .map(|t| t.slave_drop_doodads.len())
        .sum();
    assert!(drops > 0 && drops <= 45);
    // Text booleans: `mountable` is read with GetBoolean (always false), `persist` / `on_water`
    // / `cyclic` with the string form.
    assert!(d.slave_templates.values().all(|t| !t.mountable));
    assert!(d.transfer_templates.values().any(|t| t.cyclic));
    // Boats have a ship model, carriages a vehicle model. In the 1.2 data 9 of the 49
    // boat-kind slaves have none (vehicle / prefab / missing models, e.g. 7, 19, 105), so
    // count them instead of sampling one (HashMap order made that flaky).
    let boats: Vec<_> = d
        .slave_templates
        .values()
        .filter(|t| t.is_a_boat())
        .collect();
    let with_ship_model = boats
        .iter()
        .filter(|t| d.ship_model(t.model_id).is_some())
        .count();
    assert_eq!((boats.len(), with_ship_model), (49, 40));
    assert!(d.ship_model(d.slave_templates[&9].model_id).is_some());
    let carriage = d.transfer_template(6).unwrap();
    assert!(d.vehicle_model(carriage.model_id).is_some());
    assert_eq!(
        d.vehicle_model_refs.get(&1553).map(String::as_str),
        Some("prefab://prefabs/vehicle_tractor.xml/vehicle_tractor_c.tractor_body"),
        "the Red Farm Hauler's model reference"
    );
    // slave_attach_points.json of the server data
    assert!(!d.attach_points.is_empty());
    let clipper = d
        .attach_points_for_slave(128)
        .expect("clipper attach points");
    assert!(clipper.iter().any(|(p, _)| *p == attach_point::DRIVER));
    // Timber Coupe (slaves 116 / 119): hood, repair point and both trade pack storages
    let coupe = d
        .attach_points_for_slave(d.slave_templates[&116].model_id)
        .expect("timber coupe attach points");
    for point in [21, 36, 60, 61] {
        assert!(coupe.iter().any(|(p, _)| *p == point), "point {point}");
    }
    // Equipment tables: 92 slots over the seven customizable hulls, 43 item visuals, and the
    // hull / slot packs.
    let slots: usize = d.slave_equip_slots.values().map(Vec::len).sum();
    assert_eq!((slots, d.item_slave_equipments.len()), (92, 43));
    assert_eq!(d.slave_equip_slots[&22].len(), 27);
    assert_eq!(
        d.slave_equip_attach_points(22, 13),
        vec![28, 29],
        "the lamp slot drives the front and rear lamps"
    );
    assert!(d.can_equip_item_on_slave(28852, 22, 3));
    assert!(!d.can_equip_item_on_slave(28852, 22, 0));
    assert!(!d.can_equip_item_on_slave(28852, 21, 3));
    assert!(!d.can_equip_item_on_slave(29723, 22, 3));
    assert!(!d.has_slave_equip_slot(21, 3));
    // Attach points recovered from the 1.2 client meshes.
    for (slave, point) in [(22, 9), (22, 35), (132, 9), (139, 10), (139, 35)] {
        let model = d.slave_templates[&slave].model_id;
        let points = d
            .attach_points_for_slave(model)
            .unwrap_or_else(|| panic!("model {model} attach points"));
        assert!(
            points.iter().any(|(p, _)| *p == point),
            "slave {slave} model {model} point {point}"
        );
    }
}

/// The Tower Cannon (slave 100, 성탑 대포) reaches the Nov 2014 video's target-frame value:
/// formula 48 `(level * 70 + sta * 12)` at level 50 is 4,796, and the 1.2
/// `unit_modifiers` row `(Slave, 100, MaxHealth, +100000)` adds exactly 100,000, so the server
/// hands out **104,796** (`video-evidence/siege-2014-11-30-ollo.md` §1/§2.6, grade C). This
/// pins the real rows and the loader/stats wiring that applies them.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_compact_tower_cannon_max_hp_is_the_formula_plus_the_authored_bonus() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let mut data = GameData::empty();
    data.formulas = crate::data::formulas::FormulaManager::load(&conn).expect("formulas");
    data.characters.unit_attribute_limits = Arc::new(
        crate::data::attribute_limits::UnitAttributeLimits::load(&conn).expect("attribute limits"),
    );
    data.systems.vehicles = load_real(&db, &[]);

    // The authored rows, independent of the loader: level 50 + one MaxHealth bonus.
    let template = &data.systems.vehicles.slave_templates[&100];
    assert_eq!(template.level, 50);
    let max_health: Vec<_> = template
        .bonuses
        .iter()
        .filter(|b| b.attribute == crate::models::unit::UnitAttribute::MaxHealth)
        .collect();
    assert_eq!(max_health.len(), 1);
    assert_eq!(max_health[0].value, 100_000);

    let server = server_with(data);
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "Defender", 100.0, 100.0), PA);
    a.conn.active_char = Some(pa.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    a.drain();
    let slave = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        100,
        None,
        false,
        None,
        None,
    )
    .expect("Tower Cannon spawn");
    let d = slave.data();
    assert_eq!(d.unit.level, 50);
    assert_eq!(
        d.unit.max_hp, 104_796,
        "video target frame: 104,796 (formula 4,796 + unit_modifiers 100,000)"
    );
}

/// The 1.2 `spawn_effects` slave rows: only the implemented dir ids 1/2 (position) and 1/2/3
/// (orientation) occur, and every `sub_type` is a real slave template.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_spawn_effect_slave_dirs_and_templates() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let mut stmt = conn
        .prepare("SELECT id, sub_type, pos_dir_id, ori_dir_id FROM spawn_effects WHERE owner_type_id = 2")
        .unwrap();
    let rows: Vec<(u32, u32, u32, u32)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len(), 36);
    for (id, sub_type, pos_dir, ori_dir) in rows {
        assert!(matches!(pos_dir, 1 | 2), "effect {id}: PosDirId {pos_dir}");
        assert!(matches!(ori_dir, 1..=3), "effect {id}: OriDirId {ori_dir}");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM slaves WHERE id = ?1",
                [sub_type],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "effect {id}: slave {sub_type}");
    }
}

/// The real `model_bindings` of the Farm Wagon (slave 60, model 1008) and the two-seat Red Farm
/// Hauler (slave 146, model 1553). The wagon has no `$passenger0` point, so the seat rule must
/// never offer or accept point 2 on it.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_model_bindings_pin_the_farm_wagon_seat() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let d = load_real(&db, &[]);
    let mut wagon = d.model_bindings.get(&1008).cloned().unwrap_or_default();
    wagon.sort_unstable();
    assert_eq!(wagon, vec![1, 9, 10, 11, 12, 35], "model 1008 bindings");
    assert!(!d.model_has_attach_point(1008, attach_point::PASSENGER0));
    assert!(!d.slave_has_attach_point(60, attach_point::PASSENGER0));
    let mut hauler = d.model_bindings.get(&1553).cloned().unwrap_or_default();
    hauler.sort_unstable();
    assert_eq!(
        hauler,
        vec![1, 2, 9, 10, 11, 12, 13, 14, 15, 35],
        "model 1553 bindings"
    );
    assert!(d.slave_has_attach_point(146, attach_point::PASSENGER0));
    // Rowboats, tanks and cars read the same table.
    for (slave, model) in [(55u32, 129u32), (36, 857), (106, 1323)] {
        assert_eq!(d.slave_templates[&slave].model_id, model);
        assert!(d.slave_has_attach_point(slave, attach_point::PASSENGER0));
    }
}

/// The real Red Farm Hauler (model 1553) against the real client assets: the body is a
/// physicalized `AnimObject` whose `.chr` has no `MeshPhysicsData`; the same-named `.cgf`
/// carries the collision proxies. The bind gap must measure to those proxies, not to the model
/// origin or the rear cannon attach points. The spot is 2.5 m from the front-side body corner,
/// more than 4.0 m from the origin, so only the body measure is inside the 4.0 m reach.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB and AAEMU_GAME_PAK"]
async fn real_hauler_bind_measures_the_client_body_collision_bounds() {
    use crate::models::slave::Slave;
    use crate::net::registry::ClientPacket;
    use crate::systems::vehicles::packets::{CSBindSlavePacket, CSDiscardSlavePacket};
    use crate::world::Entity;

    let (Ok(db), Ok(pak)) = (
        std::env::var("AAEMU_COMPACT_DB"),
        std::env::var("AAEMU_GAME_PAK"),
    ) else {
        return;
    };
    let vehicles = load_real(&db, std::slice::from_ref(&pak));
    let reference = vehicles
        .vehicle_model_refs
        .get(&1553)
        .cloned()
        .expect("model 1553 reference");
    let app = app_path();
    let client = ClientFileManager::from_sources(std::slice::from_ref(&pak), &app);
    let crate::systems::ai::collision::objects::ModelRef::Prefab { library, name } =
        crate::systems::ai::collision::objects::parse_model_ref(&reference)
    else {
        panic!("model 1553 is a prefab");
    };
    let xml = client.read_to_string(&library);
    assert!(!xml.is_empty(), "prefab {library}");
    let body = crate::systems::ai::collision::objects::prefab_house_geometry(&xml, &name)
        .into_iter()
        .next()
        .expect("hauler body entity");
    assert!(body.0.ends_with(".chr"), "body model {}", body.0);
    let cgf = format!("{}.cgf", &body.0[..body.0.len() - 4]);
    let mut files = HashMap::new();
    files.insert(library, xml.into_bytes());
    files.insert(
        cgf.clone(),
        client
            .read_file(&cgf)
            .unwrap_or_else(|| panic!("missing {cgf}")),
    );

    let mut data = GameData::empty();
    data.systems.vehicles = vehicles;
    let server = server_with(data);
    server
        .systems
        .ai
        .collision
        .test_files
        .set(Arc::new(files))
        .unwrap();
    let files = crate::systems::ai::collision::files(&server).unwrap();
    let models = crate::systems::ai::collision::model_cache(&server, &files);
    let bounds = crate::systems::ai::physics::cry::model_bounds_including_bodies(
        &files, &models, &reference,
    )
    .expect("the real hauler's collision body");
    println!(
        "hauler bounds center {:?} half {:?}",
        bounds.center, bounds.size
    );
    assert!(bounds.size.x > 0.3, "{bounds:?}");
    assert!(
        bounds.size.y > 1.5,
        "the body is longer than it is wide: {bounds:?}"
    );
    assert!(bounds.size.z > 0.3, "{bounds:?}");

    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "Near", 100.0, 100.0), PA);
    a.conn.active_char = Some(pa.clone());
    let world = server.world.world(0).unwrap();
    let mut hauler = Slave::new(0, 146);
    hauler.unit.hp = 1000;
    hauler.unit.max_hp = 1000;
    hauler.unit.model_id = 1553;
    hauler.unit.tl_id = 3;
    hauler.unit.transform.set_xyz(103.0, 100.0, 100.0);
    hauler.summoner_obj_id = PA;
    hauler.summoner_id = pa.character_id;
    let hauler = Entity::new(0x300, hauler);
    world.spawn(&hauler, &server.view_ctx());
    a.drain();

    // 2.5 m out from the front-side body corner (the +Y side; the located cannon points are
    // at -Y), so the origin and every located attach point are farther than 4.0 m.
    let out = 2.5 / std::f32::consts::SQRT_2;
    let x = 103.0 + bounds.center.x + bounds.size.x + out;
    let y = 100.0 + bounds.center.y + bounds.size.y + out;
    assert!(
        (x - 103.0).powi(2) + (y - 100.0).powi(2) > 16.0,
        "the spot must be outside the old origin measure"
    );
    pa.data().unit.transform.set_xyz(x, y, 100.0);

    CSBindSlavePacket { tl_id: 3 }
        .handle(&server, &mut a.conn)
        .await
        .unwrap();
    assert_eq!(
        hauler.data().attached_characters,
        vec![(attach_point::DRIVER, PA)],
        "the client offers the driver seat from beside the cab"
    );
    assert!(error_codes(&a.drain()).is_empty());

    // 5 m from the same body corner is outside the unchanged 4.0 m reach.
    CSDiscardSlavePacket { tl_id: 3 }
        .handle(&server, &mut a.conn)
        .await
        .unwrap();
    a.drain();
    let far = 5.0 / std::f32::consts::SQRT_2;
    pa.data().unit.transform.set_xyz(
        103.0 + bounds.center.x + bounds.size.x + far,
        100.0 + bounds.center.y + bounds.size.y + far,
        100.0,
    );
    CSBindSlavePacket { tl_id: 3 }
        .handle(&server, &mut a.conn)
        .await
        .unwrap();
    assert_eq!(
        error_codes(&a.drain()),
        vec![super::error::TOO_FAR_AWAY],
        "5 m from the body is outside the 4.0 m driver reach"
    );
    assert!(hauler.data().attached_characters.is_empty());
}

/// Real 1.2 cart / ship / cannon slaves. The 1.2 client offers 13863 only when the model declares
/// point 2 in `model_bindings`, so the interaction list must offer it for a real rowboat / ship
/// whose driver seat is taken, never for the Farm Wagon (model 1008: points 1, 35, 9-12 only) or a
/// cannon, and the real `AttachTo` row must still be special type 53 value1 2.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_cart_and_ship_slaves_offer_the_passenger_seat() {
    use crate::models::slave::Slave;
    use crate::world::Entity;

    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let d = load_real(&db, &[]);
    // Seat points (driver 1 + passenger0..6) of the real models in the shipped JSON.
    for (model, seats) in [
        (129u32, &[1u8, 2][..]), // Rowboat: driver + passenger0
        (128, &[1]),             // Clipper
        (393, &[1]),             // Eznan Cutter
        (511, &[1]),             // Small Sailing Ship
        (1008, &[]),             // Farm Wagon (crate points only)
        (1323, &[]),             // car
        (1553, &[]),             // Red Farm Hauler
    ] {
        let got: Vec<u8> = d
            .attach_points_for_slave(model)
            .map(|points| {
                points
                    .iter()
                    .map(|(p, _)| *p)
                    .filter(|p| (1..=8).contains(p))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(got, seats, "model {model} seat points");
    }
    // The passenger skill's effect chain in the real data.
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let (kind, value1): (i32, i32) = conn
        .query_row(
            "SELECT special_effect_type_id, value1 FROM special_effects WHERE id = 2378",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((kind, value1), (53, 2), "the 1.2 passenger AttachTo row");
    let effect_id: u32 = conn
        .query_row(
            "SELECT se.effect_id FROM skill_effects se \
             JOIN effects e ON e.id = se.effect_id \
             JOIN special_effects s ON e.actual_type = 'SpecialEffect' AND e.actual_id = s.id \
             WHERE se.skill_id = 13863",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(effect_id, 8869);

    let server = real_server(d);
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "Driver", 100.0, 100.0), PA);
    a.conn.active_char = Some(pa.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    a.drain();
    for (template, taken_skill) in [
        (60u32, 12_076u32), // Farm Wagon: no passenger point in model 1008
        (21, 12_076),       // Eznan Cutter: no point 2 either
        (55, 13_863),       // Rowboat: model 129 has point 2
        (23, 13_863),       // ship: model 512 has point 2
        (10, 12_076),       // cannon
    ] {
        let t = server
            .game_data
            .systems
            .vehicles
            .slave_template(template)
            .unwrap_or_else(|| panic!("slave {template}"))
            .clone();
        let mut s = Slave::new(0, t.id);
        s.unit.hp = 1000;
        s.unit.model_id = t.model_id;
        s.unit.tl_id = server.ids.tl.get_next_id().unwrap() as u16;
        s.unit.transform.set_xyz(100.0, 100.0, 100.0);
        s.summoner_obj_id = PA;
        s.summoner_id = pa.character_id;
        let slave = Entity::new(server.ids.object.get_next_id().unwrap(), s);
        world.spawn(&slave, &server.view_ctx());
        assert_eq!(
            crate::systems::npcs::npc::packets::slave_interaction_skill(&server, &slave),
            12_076,
            "slave {template} with a free driver seat"
        );
        slaves::bind_slave(
            &server,
            &pa,
            slave.obj_id,
            attach_point::DRIVER,
            slaves::attach_reason::NEW_MASTER,
            Some(4.0),
        );
        assert_eq!(
            crate::systems::npcs::npc::packets::slave_interaction_skill(&server, &slave),
            taken_skill,
            "slave {template} with the driver seat taken"
        );
        let tl = u32::from(slave.data().unit.tl_id);
        slaves::unbind_slave(&server, &pa, tl, slaves::attach_reason::SLAVE_BINDING);
        a.drain();
    }
}

/// Attach point 10 (Cannon1) shipped with point 47's (Cannon10, the stern cannon) transform in
/// the 2023 upstream data. The 1.2 `galleon_body.cgf` helper `$cannon1` is the port broadside
/// cannon, the mirror of point 14 (Cannon5); the Growling Yawl (model 523) carries a copy of the
/// same rows. Pin both in the shipped data before the bind test.
#[test]
fn eznan_cutter_and_growling_yawl_cannon_points_mirror_their_partner() {
    let shipped = super::data::parse_attach_points(include_str!(
        "../../../app/Data/slave_attach_points.json"
    ))
    .expect("shipped attach points");
    for model in [393u32, 523] {
        let points = shipped
            .iter()
            .find(|(id, _)| *id == model)
            .map(|(_, points)| points)
            .unwrap_or_else(|| panic!("model {model} attach points"));
        let cannon = |point: u8| {
            points
                .iter()
                .find(|(p, _)| *p == point)
                .map(|(_, location)| *location)
                .unwrap_or_else(|| panic!("model {model} point {point}"))
        };
        // Points 9-16 are four mirrored broadside pairs: opposite X, same Y / Z.
        for point in 9u8..=16 {
            let a = cannon(point);
            assert!(
                (9u8..=16).any(|other| other != point && {
                    let b = cannon(other);
                    (a.x + b.x).abs() <= 1.0 && (a.y - b.y).abs() <= 1.0 && (a.z - b.z).abs() <= 1.0
                }),
                "model {model} cannon point {point} has no mirror partner"
            );
        }
        // The port cannon is the exact mirror of point 14, not the stern offset it shipped with.
        let ten = cannon(10);
        let fourteen = cannon(14);
        let mirrored = |a: f32, b: f32| (a + b).abs(); // X flips sign
        assert!(
            mirrored(ten.x, fourteen.x) <= 0.05
                && (ten.y - fourteen.y).abs() <= 0.05
                && (ten.z - fourteen.z).abs() <= 0.05,
            "model {model} point 10 ({}, {}, {}) must mirror point 14 ({}, {}, {})",
            ten.x,
            ten.y,
            ten.z,
            fourteen.x,
            fourteen.y,
            fourteen.z
        );
        // The true helpers are authored at 84 / -86 degrees, not exactly +-90.
        let yaw_diff = (ten.yaw - -fourteen.yaw).to_degrees().rem_euclid(360.0);
        assert!(
            yaw_diff.min(360.0 - yaw_diff) <= 10.0,
            "model {model} point 10 yaw {} does not mirror point 14 yaw {}",
            ten.yaw.to_degrees(),
            fourteen.yaw.to_degrees()
        );
    }
}

/// The owner's 2026-11-01 Eznan Cutter session: the cannon at tl 0x52 (attach point 10) was
/// refused with `TOO_FAR_AWAY` (154) four times while the seven others bound at 1.1-2.9 m,
/// because the server placed it on the bad stern offset 12-13 m away. With the real
/// `slave_bindings`, every cannon point must mirror its partner and the player standing beside
/// the port cannon at point 10 must man it.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_eznan_cutter_port_cannon_at_point_10_binds_beside_the_deck() {
    use crate::models::transform::Transform;

    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let d = load_real(&db, &[]);
    let hull_template = d.slave_template(21).expect("Eznan Cutter").clone();
    assert_eq!(hull_template.model_id, 393);
    let mut cannons: Vec<u8> = hull_template
        .slave_bindings
        .iter()
        .filter(|b| b.owner_type == "Slave")
        .map(|b| b.attach_point_id)
        .collect();
    cannons.sort_unstable();
    assert_eq!(
        cannons,
        vec![9, 10, 11, 12, 13, 14, 15, 16],
        "the real Eznan Cutter broadside cannons"
    );

    // Every real cannon point has a mirror partner in the shipped data (catches other bad rows).
    let shipped = super::data::parse_attach_points(include_str!(
        "../../../app/Data/slave_attach_points.json"
    ))
    .expect("shipped attach points");
    let points = shipped
        .iter()
        .find(|(id, _)| *id == 393)
        .map(|(_, points)| points)
        .expect("model 393 attach points");
    let at = |point: u8| {
        points
            .iter()
            .find(|(p, _)| *p == point)
            .map(|(_, location)| *location)
            .unwrap_or_else(|| panic!("model 393 point {point}"))
    };
    for &point in &cannons {
        let a = at(point);
        assert!(
            cannons.iter().any(|&other| {
                if other == point {
                    return false;
                }
                let b = at(other);
                (a.x + b.x).abs() <= 1.0 && (a.y - b.y).abs() <= 1.0 && (a.z - b.z).abs() <= 1.0
            }),
            "model 393 cannon point {point} has no mirror partner"
        );
    }

    let server = real_server(d);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    a.conn.active_char = Some(pa.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    a.drain();

    let mut spawn_at = Transform::default();
    spawn_at.set_xyz(100.0, 100.0, 100.0);
    let hull = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        21,
        None,
        true,
        Some(spawn_at),
        None,
    )
    .expect("summoned Eznan Cutter");
    let cannon = hull
        .data()
        .attached_slaves
        .iter()
        .filter_map(|id| world.slave(*id))
        .find(|child| child.data().attach_point_id == 10)
        .expect("cannon child at attach point 10");
    // The child's local offset is the hull's attach point. The old data put it at the stern.
    let local = cannon.data().unit.transform;
    assert!(
        local.x < -4.4 && local.y.abs() < 0.3 && (local.z - 4.487).abs() < 0.01,
        "cannon point 10 local offset ({}, {}, {}) is not the port broadside",
        local.x,
        local.y,
        local.z
    );

    // The client draws the cannon at the model's real `$cannon1` helper, not at the packet
    // position, so the player stands there: 4.4767865 m to port and 4.48708954 m up (the
    // native 1.2 galleon_body.cgf node; the corrected JSON row). With the old stern row the
    // server measures ~13 m to its cannon and refuses.
    let hull_pos = world.world_pos_rot(&WorldObject::from(&hull));
    let deck = (-4.476_786_5_f32, 0.202_046_7_f32, 4.487_089_6_f32);
    let player_at = (
        hull_pos.x + deck.0 + 1.0,
        hull_pos.y + deck.1,
        hull_pos.z + deck.2,
    );
    pa.data()
        .unit
        .transform
        .set_xyz(player_at.0, player_at.1, player_at.2);
    let cannon_tl = u32::from(cannon.data().unit.tl_id);
    slaves::bind_slave_by_tl(&server, &pa, cannon_tl);
    assert_eq!(
        cannon.data().attached_characters,
        vec![(attach_point::DRIVER, PA)],
        "the player beside the port cannon must man it"
    );
    let frames = a.drain();
    assert!(
        !error_codes(&frames).contains(&error_message::TOO_FAR_AWAY),
        "bind must not be refused as too far"
    );
}

/// A server whose main_world covers the real map (31 x 31 cells), with the real vehicle data.
fn real_server(d: super::Data) -> Arc<GameServer> {
    let mut data = GameData::empty();
    data.systems.vehicles = d;
    real_game_server(data)
}

/// A server with the real 1.2 skill and vehicle rows, on the synthetic test world.
fn real_skill_server(db: &str) -> Arc<GameServer> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let app = app_path();
    let files = ClientFileManager::from_sources(&[], &app);
    let ctx = DataLoadCtx {
        conn: &conn,
        app_path: &app,
        files: &files,
        language: "en_us",
    };
    let mut data = GameData::empty();
    data.systems.vehicles = super::Data::load(&ctx).expect("vehicles data");
    data.systems.skills = crate::systems::skills::Data::load(&ctx).expect("skills data");
    // The tower cannon's rider skill 14372 needs one steel cannonball (23671).
    data.items
        .templates
        .insert(23671, ItemTemplate::plain(23671, 1));
    real_game_server(data)
}

/// A `GameServer` on the synthetic map whose instance 0 covers 32 x 32 cells.
fn real_game_server(mut data: GameData) -> Arc<GameServer> {
    data.worlds = WorldTemplates::from_templates(
        vec![WorldTemplate::synthetic(0, "main_world", 32, 32, TEST_ZONE)],
        Vec::new(),
    )
    .expect("world");
    let mut config = AppConfig {
        id: 1,
        ..AppConfig::default()
    };
    config.app_path = app_path();
    config.connections.mysql_provider.host = "127.0.0.1".into();
    config.connections.mysql_provider.port = 1;
    let pool = MySqlPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy_with(config.connections.mysql_provider.connect_options());
    let server = Arc::new(GameServer::new(config, pool, data));
    server.set_self();
    server
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB and AAEMU_GAME_PAK"]
async fn real_transfer_follows_its_client_route() {
    let (Ok(db), Ok(pak)) = (
        std::env::var("AAEMU_COMPACT_DB"),
        std::env::var("AAEMU_GAME_PAK"),
    ) else {
        return;
    };
    let d = load_real(&db, &[pak]);
    let with_roads = d
        .transfer_templates
        .values()
        .filter(|t| !t.transfer_roads.is_empty())
        .count();
    assert!(with_roads > 10, "templates with client roads: {with_roads}");
    // 6: Salislead Peninsula ~ Liriot Hillside Loop Carriage (transfer_spawns.json entry).
    let t6 = d.transfer_template(6).expect("template 6").clone();
    assert!(!t6.transfer_roads.is_empty());
    assert!(t6.transfer_roads.iter().all(|r| r.pos.len() >= 2));
    let server = real_server(d);
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let first = t6.transfer_roads[0].pos[0];
    let def = SimpleSpawnerDef {
        base: SpawnerBase {
            id: 1,
            unit_id: 6,
            position: WorldSpawnPosition {
                world_id: 0,
                zone_id: TEST_ZONE,
                x: 12609.46,
                y: 15359.16,
                z: 158.5336,
                ..WorldSpawnPosition::default()
            },
            ..SpawnerBase::default()
        },
        count: 1,
    };
    transfers::spawn_all(&server, &world, vec![def]);
    let active = server.systems.vehicles.transfers.get(0).get_transfers();
    // The carriage and its boarding part (transfer_bindings).
    assert_eq!(active.len(), 1 + t6.transfer_bindings.len().min(1));
    let motor = active[0].clone();
    {
        let t = motor.data();
        assert!(t.unit.is_in_patrol, "on its route");
        assert_eq!((t.unit.transform.x, t.unit.transform.y), (first.x, first.y));
        assert_eq!(t.routes.len(), t6.transfer_roads.len());
    }
    // Run the 100 ms tick: the carriage must walk its checkpoints in order.
    let mut last_index = motor.data().move_step_index;
    let mut advanced = 0;
    for _ in 0..3000 {
        motor.data().wait_time_ms = 0;
        transfers::move_to(&server, &motor);
        let i = motor.data().move_step_index;
        if i != last_index {
            advanced += 1;
            last_index = i;
        }
    }
    assert!(advanced >= 3, "checkpoints passed: {advanced}");
    if let Some(part) = active.get(1) {
        // The boarding part trails 9.24 m behind the motor.
        let m = motor.data().unit.transform;
        let p = part.data().unit.transform;
        let d2 = ((m.x - p.x).powi(2) + (m.y - p.y).powi(2)).sqrt();
        assert!((d2 - 9.24417).abs() < 0.01, "trail distance {d2}");
    }
}

#[tokio::test]
async fn mate_client_packets_require_owner_for_target_and_actual_rider_for_unmount() {
    use super::packets::{CSChangeMateTargetPacket, CSUnMountMatePacket};
    use crate::net::registry::ClientPacket;

    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    let pb = spawn_player(&server, &b, test_character(2, "B", 103.0, 102.0), PB);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    mates::spawn_mount(&server, &pa, item_id);
    let mate = server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(1)
        .pop()
        .unwrap();
    let tl = mate.data().unit.tl_id;
    CSChangeMateTargetPacket {
        tl_id: tl,
        obj_id: PB,
    }
    .handle(&server, &mut b.conn)
    .await
    .unwrap();
    assert_eq!(mate.data().unit.current_target, 0);
    CSChangeMateTargetPacket {
        tl_id: tl,
        obj_id: PB,
    }
    .handle(&server, &mut a.conn)
    .await
    .unwrap();
    assert_eq!(mate.data().unit.current_target, PB);
    mates::mount_mate(&server, &pa, u32::from(tl), attach_point::DRIVER, 1);
    mates::mount_mate(&server, &pb, u32::from(tl), attach_point::PASSENGER0, 1);
    a.drain();
    b.drain();
    CSUnMountMatePacket {
        tl_id: tl,
        ap: attach_point::DRIVER,
        reason: 4,
    }
    .handle(&server, &mut b.conn)
    .await
    .unwrap();
    CSUnMountMatePacket {
        tl_id: tl,
        ap: attach_point::PASSENGER0,
        reason: 4,
    }
    .handle(&server, &mut a.conn)
    .await
    .unwrap();
    assert!(a.drain().is_empty());
    assert!(b.drain().is_empty());
    assert!(pa.data().is_riding);
    assert!(pb.data().is_riding);
    assert_eq!(
        mate.data().passenger(attach_point::DRIVER).unwrap().obj_id,
        PA
    );
    assert_eq!(
        mate.data()
            .passenger(attach_point::PASSENGER0)
            .unwrap()
            .obj_id,
        PB
    );
    CSUnMountMatePacket {
        tl_id: tl,
        ap: attach_point::PASSENGER0,
        reason: 4,
    }
    .handle(&server, &mut b.conn)
    .await
    .unwrap();
    assert!(!pb.data().is_riding);
    assert!(pa.data().is_riding);
    CSUnMountMatePacket {
        tl_id: tl,
        ap: attach_point::DRIVER,
        reason: 4,
    }
    .handle(&server, &mut a.conn)
    .await
    .unwrap();
    assert!(!pa.data().is_riding);
}

#[tokio::test]
async fn remove_mate_packet_rejects_foreign_owner_and_preserves_actual_owner_despawn() {
    use super::packets::CSRemoveMatePacket;
    use crate::net::registry::ClientPacket;

    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    let pb = spawn_player(&server, &b, test_character(2, "B", 110.0, 110.0), PB);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb);
    mates::spawn_mount(&server, &pa, item_id);
    let manager = server.systems.vehicles.mates.get(0);
    let mate = manager.get_active_mates(1).pop().unwrap();
    let tl = mate.data().unit.tl_id;
    let world = server.world.world(0).unwrap();
    CSRemoveMatePacket { tl_id: tl }
        .handle(&server, &mut b.conn)
        .await
        .unwrap();
    assert!(world.mate(mate.obj_id).is_some());
    assert_eq!(manager.get_active_mates(1).len(), 1);
    assert!(!mate.data().deleted);
    CSRemoveMatePacket { tl_id: tl }
        .handle(&server, &mut a.conn)
        .await
        .unwrap();
    assert!(world.mate(mate.obj_id).is_none());
    assert!(manager.get_active_mates(1).is_empty());
    assert!(mate.data().deleted);
}

#[tokio::test]
async fn hang_and_unhang_packets_only_mutate_the_authenticated_character() {
    use super::packets::{CSHangPacket, CSUnhangPacket};
    use crate::net::registry::ClientPacket;

    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let pa = spawn_live_player(&server, &a, 1, PA, (100.0, 100.0));
    let pb = spawn_player(&server, &b, test_character(2, "B", 103.0, 100.0), PB);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    let world = server.world.world(0).unwrap();
    let mut data = crate::models::npc::Npc::new(MATE_NPC);
    data.unit.transform.set_xyz(105.0, 100.0, 100.0);
    let anchor = crate::world::Entity::new(0x200, data);
    world.spawn(&anchor, &server.view_ctx());
    a.drain();
    b.drain();
    CSHangPacket {
        unit_obj_id: PB,
        target_obj_id: anchor.obj_id,
    }
    .handle(&server, &mut a.conn)
    .await
    .unwrap();
    assert_eq!(pb.core().sticky_parent(), 0);
    assert!(a.drain().is_empty());
    assert!(b.drain().is_empty());
    // A server-forced sticky attachment remains available and cannot be cleared by another client.
    world.set_sticky_parent(&WorldObject::from(&pb), Some(&WorldObject::from(&anchor)));
    CSUnhangPacket {
        unit_obj_id: PB,
        reason: 2,
    }
    .handle(&server, &mut a.conn)
    .await
    .unwrap();
    assert_eq!(pb.core().sticky_parent(), anchor.obj_id);
    CSHangPacket {
        unit_obj_id: PA,
        target_obj_id: anchor.obj_id,
    }
    .handle(&server, &mut a.conn)
    .await
    .unwrap();
    assert_eq!(pa.core().sticky_parent(), anchor.obj_id);
    assert!(b.drain_ops().contains(&sc::SCHungPacket));
    CSUnhangPacket {
        unit_obj_id: PA,
        reason: 2,
    }
    .handle(&server, &mut a.conn)
    .await
    .unwrap();
    assert_eq!(pa.core().sticky_parent(), 0);
    assert_eq!(pb.core().sticky_parent(), anchor.obj_id);
    assert!(b.drain_ops().contains(&sc::SCUnhungPacket));
    CSUnhangPacket {
        unit_obj_id: PB,
        reason: 2,
    }
    .handle(&server, &mut b.conn)
    .await
    .unwrap();
    assert_eq!(pb.core().sticky_parent(), 0);
}

// ---------------------------------------------------------------------------------------------
// SummonPos summon placement (2026-10-22)
// ---------------------------------------------------------------------------------------------

const SUMMON_POS_SKILL: u32 = 58_003;
const SLAVE_PART: u32 = 5002;

/// A world whose ocean is far below the synthetic terrain fallback, so land placements are not
/// mistaken for sea-floor spots (the default test world's ocean level is 100).
fn low_ocean_world() -> WorldTemplate {
    let mut world = WorldTemplate::synthetic(0, "main_world", 2, 2, TEST_ZONE);
    world.ocean_level = -1000.0;
    world
}

fn summon_pos_data(spawn_range: u32) -> GameData {
    use crate::systems::skills::data::effects::{EffectTemplate, SpecialEffect};
    use crate::systems::skills::data::{EffectKind, SkillTemplate};
    let mut d = slave_data();
    d.items.templates.get_mut(&SLAVE_ITEM).unwrap().use_skill_id = SUMMON_POS_SKILL;
    d.systems
        .vehicles
        .slave_templates
        .get_mut(&SLAVE_TEMPLATE)
        .unwrap()
        .spawn_valid_area_range = spawn_range;
    d.systems.skills.add_effect(
        EffectKind::SpecialEffect,
        EffectTemplate::Special(SpecialEffect {
            id: 1,
            special_effect_type_id: 60,
            value1: 0,
            value2: 0,
            value3: 0,
            value4: 0,
        }),
        1,
    );
    d.systems.skills.add_skill(SkillTemplate {
        id: SUMMON_POS_SKILL,
        target_type: 12, // SummonPos
        level_rule_no_consideration: true,
        effects: vec![crate::systems::skills::tests::skill_effect(
            1,
            EffectKind::SpecialEffect,
            1,
        )],
        ..SkillTemplate::default()
    });
    d
}

/// A client that owns `SLAVE_ITEM` in its bag and can cast the summon skill.
fn summon_pos_client(
    server: &Arc<GameServer>,
) -> (TestClient, PlayerRef, u64, Arc<crate::world::WorldInstance>) {
    let mut a = TestClient::new(server, 1, 1);
    let (pa, item_id) = player_with_item(server, &a, 1, PA, (100.0, 100.0), SLAVE_ITEM);
    let bag = pa
        .data()
        .inventory
        .get(slot_type::INVENTORY)
        .unwrap()
        .clone();
    bag.lock().unwrap().items[0].owner_id = u64::from(pa.character_id);
    server.items.add_container(bag);
    a.conn.active_char = Some(pa.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    a.drain();
    (a, pa, item_id, world)
}

fn cast_summon_pos(
    server: &GameServer,
    client: &mut TestClient,
    item_id: u64,
    x: f32,
    y: f32,
    z: f32,
    yaw: f32,
) {
    use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
    start_skill(
        server,
        &mut client.conn,
        CSStartSkillPacket {
            skill_id: SUMMON_POS_SKILL,
            caster: SkillCaster::Item {
                obj_id: PA,
                item_id,
                item_template_id: SLAVE_ITEM,
                type1: 0,
                type2: 0,
            },
            target: SkillCastTarget::Position {
                obj_id: 0,
                x,
                y,
                z,
                rot: yaw,
                obj_id1: 0,
                obj_id2: 0,
            },
            flag: 0,
            skill_object: SkillObject::None,
        },
    );
}

async fn wait_for_slave(world: &Arc<crate::world::WorldInstance>) -> crate::world::SlaveRef {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(s) = slaves::get_active_slave_by_owner_obj_id(world, PA) {
                break s;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("summon skill must create the vehicle")
}

#[tokio::test]
async fn summon_pos_places_the_vehicle_at_the_chosen_spot() {
    let server = server_with_world(summon_pos_data(50), low_ocean_world());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let (mut a, _pa, item_id, world) = summon_pos_client(&server);
    cast_summon_pos(&server, &mut a, item_id, 103.0, 104.0, 200.0, 1.25);
    let slave = wait_for_slave(&world).await;
    let t = slave.data().unit.transform;
    assert_eq!((t.x, t.y), (103.0, 104.0));
    // Ships and land vehicles spawn 90 degrees CCW of the player, the pose the 1.2 check used;
    // the client's SummonPos yaw is not the spawn yaw.
    assert!(
        (t.rot_z - std::f32::consts::FRAC_PI_2).abs() < 1e-6,
        "yaw {}, wanted 90 degrees CCW of the player",
        t.rot_z
    );
    assert!(
        !error_codes(&a.drain()).contains(&super::error::SLAVE_SPAWN_ERROR_INVALID_AREA),
        "a valid placement must not send the invalid-area error"
    );
}

#[tokio::test]
async fn summon_pos_outside_the_valid_area_errors_without_spawning() {
    let server = server_with_world(summon_pos_data(10), low_ocean_world());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let (mut a, _pa, item_id, world) = summon_pos_client(&server);
    cast_summon_pos(&server, &mut a, item_id, 130.0, 100.0, 0.0, 1.25);
    let frames = tokio::time::timeout(Duration::from_secs(5), async {
        let mut frames = Vec::new();
        loop {
            frames.extend(a.drain());
            if error_codes(&frames).contains(&super::error::SLAVE_SPAWN_ERROR_INVALID_AREA) {
                break frames;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("invalid summon placement must be handled");
    assert!(
        error_codes(&frames).contains(&super::error::SLAVE_SPAWN_ERROR_INVALID_AREA),
        "invalid placement must send SlaveSpawnErrorInvalidArea"
    );
    assert!(slaves::get_active_slave_by_owner_obj_id(&world, PA).is_none());
}

/// The client picks boat summon points on its wave-displaced sea, a little above the nominal
/// surface: `z = 100.3` over ocean level 100.0 was refused as "not on water".
#[tokio::test]
async fn boat_summon_point_slightly_above_the_sea_surface_is_accepted() {
    use crate::systems::skills::ext::SummonPlacement;
    let mut data = summon_pos_data(50);
    data.systems
        .vehicles
        .slave_templates
        .get_mut(&SLAVE_TEMPLATE)
        .unwrap()
        .slave_kind = super::data::slave_kind::BOAT;
    let server = server_with(data);
    let (mut a, pa, _item_id, world) = summon_pos_client(&server);
    let template = server
        .game_data
        .systems
        .vehicles
        .slave_template(SLAVE_TEMPLATE)
        .unwrap()
        .clone();
    assert!(template.is_a_boat());
    assert_eq!(world.template.ocean_level, 100.0);
    let place = |z: f32| {
        slaves::validated_summon_placement(
            &server,
            &world,
            &pa,
            &template,
            Some(SummonPlacement {
                x: 103.0,
                y: 104.0,
                z,
                yaw: 1.25,
            }),
            0.0,
        )
    };
    for z in [99.0, 100.0, 100.058, 100.3, 100.6] {
        let t = place(z).unwrap_or_else(|| panic!("z {z} over the sea must be accepted"));
        assert_eq!((t.x, t.y, t.z), (103.0, 104.0, 100.0), "z {z}");
    }
    assert!(
        !error_codes(&a.drain()).contains(&super::error::SLAVE_SPAWN_ERROR_INVALID_AREA),
        "accepted placements must not send the invalid-area error"
    );
    // Beyond the tolerance the point is in the air, not on the water.
    assert!(place(101.5).is_none());
    assert!(error_codes(&a.drain()).contains(&super::error::SLAVE_SPAWN_ERROR_INVALID_AREA));
}

#[tokio::test]
async fn slave_equipment_equips_unequips_and_survives_a_resummon() {
    use crate::models::item::ItemClass;
    use crate::net::registry::ClientPacket;
    use crate::systems::vehicles::packets::CSChangeSlaveEquipmentPacket;

    let mut d = slave_data();
    d.items.templates.insert(
        SLAVE_PART,
        ItemTemplate {
            impl_id: crate::data::items::item_impl::SLAVE_EQUIPMENT,
            ..ItemTemplate::plain(SLAVE_PART, 1)
        },
    );
    d.systems
        .vehicles
        .slave_equip_slots
        .entry(SLAVE_TEMPLATE)
        .or_default()
        .push(super::data::SlaveEquipSlot {
            id: 1,
            slave_id: SLAVE_TEMPLATE,
            attach_point_id: 9,
            equip_slot_id: 3,
            require_slot_id: -1,
        });
    d.systems.vehicles.item_slave_equipments.insert(
        SLAVE_PART,
        super::data::ItemSlaveEquipment {
            id: 1,
            item_id: SLAVE_PART,
            slave_id: 0,
            doodad_id: 0,
            slave_equip_pack_id: 0,
            slot_pack_id: 0,
            require_item_id: 0,
        },
    );
    let server = server_with(d);
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), SLAVE_ITEM);
    a.conn.active_char = Some(pa.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let bag = pa
        .data()
        .inventory
        .get(slot_type::INVENTORY)
        .unwrap()
        .clone();
    bag.lock().unwrap().items[0].owner_id = u64::from(pa.character_id);
    server.items.add_container(bag.clone());

    let item = mates::owned_item(&server, &pa, item_id).unwrap();
    let slave = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SLAVE_TEMPLATE,
        Some(item),
        false,
        None,
        None,
    )
    .expect("slave");
    let (tl, db_id) = {
        let d = slave.data();
        (d.unit.tl_id, d.id)
    };
    assert!(db_id > 0);

    let mut part = server
        .items
        .create(
            SLAVE_PART,
            1,
            0,
            1,
            &server.game_data.items,
            &server.ids.item,
            0,
        )
        .expect("part");
    assert_eq!(part.class, ItemClass::SlaveEquipment);
    part.slot_type = slot_type::INVENTORY;
    part.slot = 7;
    part.owner_id = u64::from(pa.character_id);
    part.dirty = true;
    let part_id = part.id;
    bag.lock().unwrap().items.push(part.clone());
    a.drain();

    let character_id = pa.character_id;
    let packet_body = |item1: Option<&crate::models::item::Item>,
                       slot1: u8,
                       item2: Option<&crate::models::item::Item>,
                       slot2: u8,
                       bts: bool| {
        let mut s = PacketStream::new();
        s.write_u32(character_id)
            .write_u16(tl)
            .write_u32(db_id)
            .write_bool(bts);
        match item1 {
            Some(i) => {
                s.write(i);
            }
            None => {
                s.write_u32(0);
            }
        }
        match item2 {
            Some(i) => {
                s.write(i);
            }
            None => {
                s.write_u32(0);
            }
        }
        s.write_u8(slot_type::INVENTORY)
            .write_u8(slot1)
            .write_u8(slot_type::EQUIPMENT_SLAVE)
            .write_u8(slot2);
        s.into_bytes()
    };
    let run = |body: Vec<u8>, a: &mut TestClient| {
        let p = CSChangeSlaveEquipmentPacket::read(&mut PacketStream::from_bytes(body)).unwrap();
        futures::executor::block_on(p.handle(&server, &mut a.conn)).unwrap();
        a.drain()
    };

    // Equip bag slot 7 -> slave slot 3.
    let frames = run(packet_body(Some(&part), 7, None, 3, false), &mut a);
    assert!(ops(&frames).contains(&sc::SCSlaveEquipmentChangedPacket));
    assert_eq!(
        *body(&frames, sc::SCSlaveEquipmentChangedPacket)
            .last()
            .unwrap(),
        1,
        "success flag"
    );
    assert!(ops(&frames).contains(&sc::SCUnitEquipmentsChangedPacket));
    assert!(bag.lock().unwrap().get_item_by_slot(7).is_none());
    assert_eq!(
        slave.data().equipment[3].as_ref().map(|i| i.id),
        Some(part_id)
    );
    let equipment = server
        .items
        .find_container_for_character(pa.character_id, slot_type::EQUIPMENT_SLAVE, db_id)
        .expect("persistent slave equipment container");
    let stored = equipment
        .lock()
        .unwrap()
        .get_item_by_slot(3)
        .cloned()
        .expect("part in the slave container");
    assert_eq!(
        (stored.id, stored.slot_type),
        (part_id, slot_type::EQUIPMENT_SLAVE)
    );

    // Unequip slave slot 3 -> empty bag slot 9.
    let frames = run(packet_body(None, 9, Some(&stored), 3, true), &mut a);
    assert_eq!(
        *body(&frames, sc::SCSlaveEquipmentChangedPacket)
            .last()
            .unwrap(),
        1
    );
    assert_eq!(
        bag.lock().unwrap().get_item_by_slot(9).map(|i| i.id),
        Some(part_id)
    );
    assert!(slave.data().equipment[3].is_none());
    assert!(equipment.lock().unwrap().get_item_by_slot(3).is_none());

    // Equip again, then destroy the hull and summon the saved slave: the parts must come back
    // from the persistent container (item_containers.mate_id == slave DB id).
    let frames = run(packet_body(Some(&stored), 9, None, 3, false), &mut a);
    assert_eq!(
        *body(&frames, sc::SCSlaveEquipmentChangedPacket)
            .last()
            .unwrap(),
        1
    );
    slaves::delete(&server, Some(&pa), world.id, slave.obj_id, false);
    let item = mates::owned_item(&server, &pa, item_id).unwrap();
    let again = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SLAVE_TEMPLATE,
        Some(item),
        false,
        None,
        Some(slaves::SavedSlave {
            id: db_id,
            name: "Cart".into(),
            hp: 1,
            mp: 1,
        }),
    )
    .expect("re-summoned slave");
    assert_eq!(
        again.data().equipment[3].as_ref().map(|i| i.id),
        Some(part_id),
        "the saved hull keeps its equipped parts"
    );
}

const SHIP_HULL: u32 = 22;
const OTHER_HULL: u32 = 23;
const CANNON_ITEM: u32 = 28852;
const CANNON_CHILD: u32 = 10;
const SLOT20_ITEM: u32 = 5003;

fn ship_equip_data() -> GameData {
    use super::data::{ItemSlaveEquipment, SlaveEquipSlot, SlaveInitialItem};
    let mut d = slave_data();
    d.items.templates.get_mut(&SLAVE_ITEM).unwrap().kind = TemplateKind::SummonSlave {
        slave_id: SHIP_HULL,
    };
    d.systems.vehicles.slave_templates.insert(
        SHIP_HULL,
        SlaveTemplate {
            id: SHIP_HULL,
            name: "Ship".into(),
            model_id: 511,
            level: 1,
            slave_kind: super::data::slave_kind::MACHINE,
            slave_initial_item_pack_id: 1,
            // A static binding on an equipment attach point must not add a second cannon.
            slave_bindings: vec![super::data::SlaveBinding {
                id: 1,
                owner_id: SHIP_HULL,
                owner_type: "Slave".into(),
                slave_id: CANNON_CHILD,
                attach_point_id: 9,
            }],
            ..SlaveTemplate::default()
        },
    );
    d.systems.vehicles.slave_templates.insert(
        OTHER_HULL,
        SlaveTemplate {
            id: OTHER_HULL,
            name: "Other".into(),
            model_id: 512,
            level: 1,
            slave_kind: super::data::slave_kind::MACHINE,
            ..SlaveTemplate::default()
        },
    );
    d.systems.vehicles.slave_templates.insert(
        CANNON_CHILD,
        SlaveTemplate {
            id: CANNON_CHILD,
            name: "Steel Cannon".into(),
            model_id: 117,
            level: 1,
            slave_kind: super::data::slave_kind::SLAVE_EQUIPMENT,
            ..SlaveTemplate::default()
        },
    );
    for item in [CANNON_ITEM, SLOT20_ITEM] {
        d.items.templates.insert(
            item,
            ItemTemplate {
                impl_id: crate::data::items::item_impl::SLAVE_EQUIPMENT,
                ..ItemTemplate::plain(item, 1)
            },
        );
    }
    d.systems.vehicles.slave_initial_items.insert(
        1,
        vec![
            SlaveInitialItem {
                pack_id: 1,
                equip_slot_id: 3,
                item_id: CANNON_ITEM,
            },
            // No slave_equip_slots entry: must not be seeded.
            SlaveInitialItem {
                pack_id: 1,
                equip_slot_id: 20,
                item_id: SLOT20_ITEM,
            },
        ],
    );
    d.systems.vehicles.slave_equip_slots.insert(
        SHIP_HULL,
        vec![SlaveEquipSlot {
            id: 1,
            slave_id: SHIP_HULL,
            attach_point_id: 9,
            equip_slot_id: 3,
            require_slot_id: -1,
        }],
    );
    d.systems.vehicles.item_slave_equipments.insert(
        CANNON_ITEM,
        ItemSlaveEquipment {
            id: 1,
            item_id: CANNON_ITEM,
            slave_id: CANNON_CHILD,
            doodad_id: 0,
            slave_equip_pack_id: 4,
            slot_pack_id: 11,
            require_item_id: 0,
        },
    );
    d.systems
        .vehicles
        .slave_equip_packs
        .insert(4, vec![SHIP_HULL]);
    d.systems.vehicles.slave_slot_packs.insert(11, vec![3]);
    d.systems.vehicles.attach_points.insert(
        511,
        vec![(
            9,
            WorldSpawnPosition {
                x: 1.5,
                y: 2.5,
                z: 3.5,
                ..WorldSpawnPosition::default()
            },
        )],
    );
    d
}

#[tokio::test]
async fn slave_equipment_cannon_visuals_and_pack_validation() {
    use crate::models::item::ItemClass;
    use crate::net::registry::ClientPacket;
    use crate::systems::vehicles::packets::CSChangeSlaveEquipmentPacket;

    let server = server_with(ship_equip_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "P1", 100.0, 100.0), PA);
    a.conn.active_char = Some(pa.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let bag = pa
        .data()
        .inventory
        .get(slot_type::INVENTORY)
        .unwrap()
        .clone();
    server.items.add_container(bag.clone());

    let hull = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SHIP_HULL,
        None,
        false,
        None,
        None,
    )
    .expect("hull");
    let (tl, db_id) = {
        let d = hull.data();
        (d.unit.tl_id, d.id)
    };
    assert!(db_id > 0);

    // The starter pack seeded the declared slot only and its cannon visual spawned.
    assert_eq!(
        hull.data().equipment[3].as_ref().map(|i| i.template_id),
        Some(CANNON_ITEM)
    );
    assert!(
        hull.data().equipment[20].is_none(),
        "seeding must skip equipment slots the hull does not declare"
    );
    let equipment = server
        .items
        .find_container_for_character(pa.character_id, slot_type::EQUIPMENT_SLAVE, db_id)
        .expect("persistent container");
    assert!(equipment.lock().unwrap().get_item_by_slot(20).is_none());
    let first_child = {
        let ids = hull.data().attached_slaves.clone();
        assert_eq!(ids.len(), 1, "the seeded cannon must spawn its child slave");
        let child = world.slave(ids[0]).expect("child slave");
        assert_eq!(child.data().unit.template_id, CANNON_CHILD);
        assert_eq!(child.data().attach_point_id, 9);
        let t = child.data().unit.transform;
        assert_eq!((t.x, t.y, t.z), (1.5, 2.5, 3.5));
        ids[0]
    };

    let character_id = pa.character_id;
    let packet_body = |item1: Option<&crate::models::item::Item>,
                       slot1: u8,
                       item2: Option<&crate::models::item::Item>,
                       slot2: u8|
     -> Vec<u8> {
        let mut s = PacketStream::new();
        s.write_u32(character_id)
            .write_u16(tl)
            .write_u32(db_id)
            .write_bool(false);
        match item1 {
            Some(i) => {
                s.write(i);
            }
            None => {
                s.write_u32(0);
            }
        }
        match item2 {
            Some(i) => {
                s.write(i);
            }
            None => {
                s.write_u32(0);
            }
        }
        s.write_u8(slot_type::INVENTORY)
            .write_u8(slot1)
            .write_u8(slot_type::EQUIPMENT_SLAVE)
            .write_u8(slot2);
        s.into_bytes()
    };
    let run = |body: Vec<u8>, a: &mut TestClient| {
        let p = CSChangeSlaveEquipmentPacket::read(&mut PacketStream::from_bytes(body)).unwrap();
        futures::executor::block_on(p.handle(&server, &mut a.conn)).unwrap();
        a.drain()
    };
    let stored = equipment
        .lock()
        .unwrap()
        .get_item_by_slot(3)
        .cloned()
        .expect("cannon in the container");

    // Unequip: the child slave goes away with the item.
    let frames = run(packet_body(None, 9, Some(&stored), 3), &mut a);
    assert_eq!(
        *body(&frames, sc::SCSlaveEquipmentChangedPacket)
            .last()
            .unwrap(),
        1
    );
    assert!(world.slave(first_child).is_none());
    assert!(hull.data().attached_slaves.is_empty());
    assert!(hull.data().equipment[3].is_none());

    // Equip it again, then re-summon the saved hull: the persistent container respawns the visual.
    let frames = run(packet_body(Some(&stored), 9, None, 3), &mut a);
    assert_eq!(
        *body(&frames, sc::SCSlaveEquipmentChangedPacket)
            .last()
            .unwrap(),
        1
    );
    let respawned = hull.data().attached_slaves.clone();
    assert_eq!(respawned.len(), 1);
    slaves::delete(&server, Some(&pa), world.id, hull.obj_id, false);
    assert!(world.slave(respawned[0]).is_none());
    let again = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SHIP_HULL,
        None,
        false,
        None,
        Some(slaves::SavedSlave {
            id: db_id,
            name: "Ship".into(),
            hp: 1,
            mp: 1,
        }),
    )
    .expect("re-summoned hull");
    let anew = again.data().attached_slaves.clone();
    assert_eq!(
        anew.len(),
        1,
        "re-summon must spawn the visual from the equipment container"
    );
    assert_eq!(
        again.data().equipment[3].as_ref().map(|i| i.id),
        Some(stored.id)
    );

    // Bag cannon into a slot the hull does not declare: refused, no move, proper error.
    let mut extra = server
        .items
        .create(
            CANNON_ITEM,
            1,
            0,
            1,
            &server.game_data.items,
            &server.ids.item,
            0,
        )
        .expect("extra cannon");
    assert_eq!(extra.class, ItemClass::SlaveEquipment);
    extra.slot_type = slot_type::INVENTORY;
    extra.slot = 7;
    extra.owner_id = u64::from(pa.character_id);
    extra.dirty = true;
    bag.lock().unwrap().items.push(extra.clone());
    let frames = run(packet_body(Some(&extra), 7, None, 4), &mut a);
    assert_eq!(
        *body(&frames, sc::SCSlaveEquipmentChangedPacket)
            .last()
            .unwrap(),
        0
    );
    assert!(error_codes(&frames).contains(&super::error::ITEM_NOT_FOR_PET));
    assert!(bag.lock().unwrap().get_item_by_slot(7).is_some());
    assert!(again.data().equipment[4].is_none());

    // Same item on a hull whose pack does not allow it: refused as well.
    let other = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        OTHER_HULL,
        None,
        false,
        None,
        None,
    )
    .expect("other hull");
    let (other_tl, other_db) = {
        let d = other.data();
        (d.unit.tl_id, d.id)
    };
    let mut s = PacketStream::new();
    s.write_u32(character_id)
        .write_u16(other_tl)
        .write_u32(other_db)
        .write_bool(false);
    s.write(&extra);
    s.write_u32(0);
    s.write_u8(slot_type::INVENTORY)
        .write_u8(7)
        .write_u8(slot_type::EQUIPMENT_SLAVE)
        .write_u8(3);
    let frames = run(s.into_bytes(), &mut a);
    assert_eq!(
        *body(&frames, sc::SCSlaveEquipmentChangedPacket)
            .last()
            .unwrap(),
        0
    );
    assert!(error_codes(&frames).contains(&super::error::ITEM_NOT_FOR_PET));
    assert!(bag.lock().unwrap().get_item_by_slot(7).is_some());
    assert!(other.data().equipment[3].is_none());
}

const FARM_WAGON: u32 = 69;
const WAGON_CRATE: u32 = 9001;

/// The shipped Farm Wagon (model 1008) attach points and four trade-pack crate bindings.
fn farm_wagon_data() -> GameData {
    use super::data::SlaveDoodadBinding;
    use crate::systems::npcs::doodad::templates::DoodadTemplate;
    let mut d = slave_data();
    d.systems.npcs.doodad.templates.insert(
        WAGON_CRATE,
        DoodadTemplate {
            id: WAGON_CRATE,
            ..DoodadTemplate::default()
        },
    );
    d.systems.vehicles.slave_templates.insert(
        FARM_WAGON,
        SlaveTemplate {
            id: FARM_WAGON,
            name: "Farm Wagon".into(),
            model_id: 1008,
            level: 1,
            slave_kind: super::data::slave_kind::MACHINE,
            doodad_bindings: [9u8, 10, 11, 12]
                .into_iter()
                .enumerate()
                .map(|(i, attach_point_id)| SlaveDoodadBinding {
                    id: i as u32 + 1,
                    owner_id: FARM_WAGON,
                    owner_type: "Slave".into(),
                    attach_point_id,
                    doodad_id: WAGON_CRATE,
                    persist: false,
                    scale: 1.0,
                })
                .collect(),
            ..SlaveTemplate::default()
        },
    );
    let wagon = super::data::parse_attach_points(include_str!(
        "../../../app/Data/slave_attach_points.json"
    ))
    .expect("shipped attach points")
    .into_iter()
    .find(|(model, _)| *model == 1008)
    .expect("Farm Wagon (model 1008)");
    d.systems.vehicles.attach_points.insert(wagon.0, wagon.1);
    d
}

#[tokio::test]
async fn farm_wagon_crate_doodads_bind_at_the_prefab_cannon_offsets() {
    let server = server_with(farm_wagon_data());
    crate::systems::npcs::install_hooks(&server);
    let a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "Wagon", 100.0, 100.0), PA);
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let wagon = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        FARM_WAGON,
        None,
        false,
        None,
        None,
    )
    .expect("wagon");
    let crates = wagon.data().attached_doodads.clone();
    assert_eq!(crates.len(), 4, "one crate per cannon marker");
    // game/prefabs/vehicle_tractor.xml, vehicle_tractor.tractor_body, metres.
    for (point, expected) in [
        (9u8, [-0.629_516_6, -1.942_382_8, 1.141_372_7]),
        (10, [-0.627_685_55, -3.170_410_2, 1.141_372_7]),
        (11, [0.632_934_6, -3.174_682_6, 1.141_372_7]),
        (12, [0.633_850_1, -1.942_016_6, 1.141_418_5]),
    ] {
        let crate_ = crates
            .iter()
            .filter_map(|id| world.doodad(*id))
            .find(|d| d.data().attach_point == point)
            .unwrap_or_else(|| panic!("no crate at point {point}"));
        assert_eq!(
            crate_.data().transform.parent_obj_id,
            wagon.obj_id,
            "crate {point} follows the wagon"
        );
        let t = crate_.data().transform;
        for (got, want) in [(t.x, expected[0]), (t.y, expected[1]), (t.z, expected[2])] {
            assert!(
                (got - want).abs() < 0.000_01,
                "crate at point {point}: {got} != {want}"
            );
        }
    }
}

const FIRE_CANNON: u32 = 10473;
const GUNNER_STATE_SKILL: u32 = 14372;
const CANNON_MOUNT_SKILL: u32 = 16;
const CANNON_TARGET_NPC: u32 = 901;

/// `ship_equip_data` plus the 1.2 cannon fire skill: mount skill 16 -> skill 10473 (ballistic
/// position target, 4 m area, 4-120 m, fixed damage), attached skill 14372 at the gunner seat.
fn cannon_data() -> GameData {
    use crate::systems::skills::data::effects::EffectTemplate;
    use crate::systems::skills::data::{EffectKind, SkillTemplate};
    let mut d = ship_equip_data();
    d.systems.skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(crate::systems::skills::tests::damage_effect(
            183, 150,
        ))),
        183,
    );
    d.systems.skills.add_skill(SkillTemplate {
        id: FIRE_CANNON,
        target_type: 11, // BallisticPos
        target_selection: 2,
        // The real row is Hostile (4); this fixture has no factions.
        target_relation: 5,
        target_area_radius: 4,
        min_range: 4,
        max_range: 120,
        source_mount: true,
        level_rule_no_consideration: true,
        effects: vec![crate::systems::skills::tests::skill_effect(
            183,
            EffectKind::DamageEffect,
            183,
        )],
        ..SkillTemplate::default()
    });
    d.systems.skills.add_skill(SkillTemplate {
        id: GUNNER_STATE_SKILL,
        target_type: 0,
        level_rule_no_consideration: true,
        ..SkillTemplate::default()
    });
    d.systems
        .vehicles
        .mount_skills
        .push(super::data::MountSkills {
            id: CANNON_MOUNT_SKILL,
            name: "Fire".into(),
            skill_id: FIRE_CANNON,
        });
    d.systems
        .vehicles
        .mount_attached_skills
        .push(super::data::MountAttachedSkills {
            id: 131,
            mount_skill_id: CANNON_MOUNT_SKILL,
            attach_point_id: attach_point::DRIVER,
            skill_id: GUNNER_STATE_SKILL,
        });
    // The cannon's own driver bar entry (real data: `slave_mount_skills`).
    d.systems
        .vehicles
        .slave_mount_skills
        .push((1, CANNON_CHILD, CANNON_MOUNT_SKILL));
    // The real data shape: models 117 -> vehicle_models 10 (slave 77 and every ship cannon),
    // pitch -20..35 deg, yaw -22..22 deg (`compact.sqlite3`).
    d.systems
        .vehicles
        .model_types
        .insert(117, (10, "VehicleModel".into()));
    d.systems.vehicles.vehicle_models.insert(
        10,
        VehicleModel {
            id: 10,
            turret_pitch_angle_min: Some(-20.0),
            turret_pitch_angle_max: Some(35.0),
            turret_yaw_angle_min: Some(-22.0),
            turret_yaw_angle_max: Some(22.0),
            ..VehicleModel::default()
        },
    );
    d.systems.npcs.npc.templates.insert(
        CANNON_TARGET_NPC,
        NpcTemplate {
            id: CANNON_TARGET_NPC,
            name: "Target".into(),
            level: 1,
            ..NpcTemplate::default()
        },
    );
    d
}

#[tokio::test]
async fn ship_cannon_gunner_seat_skill_and_leaving() {
    use crate::net::registry::ClientPacket;
    use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};

    let server = server_with(cannon_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let mut c = TestClient::new(&server, 3, 3);
    let pa = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    let pb = spawn_player(&server, &b, test_character(2, "Crew", 101.0, 100.0), PB);
    let pc = spawn_player(
        &server,
        &c,
        test_character(3, "Stranger", 102.0, 100.0),
        0x00F0_0003,
    );
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    c.conn.active_char = Some(pc.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);

    let hull = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SHIP_HULL,
        None,
        false,
        None,
        None,
    )
    .expect("hull");
    let cannon = world
        .slave(hull.data().attached_slaves[0])
        .expect("cannon child");
    let cannon_tl = u32::from(cannon.data().unit.tl_id);
    assert!(slaves::is_manned_cannon(&server, &cannon));
    assert!(!slaves::is_manned_cannon(&server, &hull));
    // `create_core` puts the hull 5 m in front of the owner and the cannon on its equipment
    // point; the 12076 bind reach is 4.0 m.
    let cannon_pos = world.world_pos_rot(&WorldObject::from(&cannon));
    pa.data()
        .unit
        .transform
        .set_xyz(cannon_pos.x, cannon_pos.y + 1.0, cannon_pos.z);

    let target = spawn_npc_at(&server, 0x5000, CANNON_TARGET_NPC, 130.0, 130.0);
    target.data().unit.hp = 1000;
    target.data().unit.max_hp = 1000;
    let fire = |client: &mut TestClient, x: f32, y: f32| {
        start_skill(
            &server,
            &mut client.conn,
            CSStartSkillPacket {
                skill_id: FIRE_CANNON,
                caster: SkillCaster::Mount {
                    obj_id: cannon.obj_id,
                    mount_skill_template_id: CANNON_MOUNT_SKILL,
                },
                target: SkillCastTarget::Position {
                    obj_id: 0,
                    x,
                    y,
                    z: 100.0,
                    rot: 0.0,
                    obj_id1: 0,
                    obj_id2: 0,
                },
                flag: 0,
                skill_object: SkillObject::None,
            },
        );
    };
    a.drain();
    b.drain();
    c.drain();

    // Not manned yet: not even the ship owner may fire the cannon.
    fire(&mut a, 130.0, 130.0);
    assert_eq!(target.data().unit.hp, 1000);
    a.drain();

    // A stranger off the ship is refused with the bind error and is not seated.
    slaves::bind_slave_by_tl(&server, &pc, cannon_tl);
    assert!(error_codes(&c.drain()).contains(&super::error::SLAVE_CANNOT_BIND));
    assert!(cannon.data().attached_characters.is_empty());

    // The ship owner mans it at the gunner seat, behind the barrel.
    stand_next_to(&world, &pa, &WorldObject::from(&cannon));
    slaves::bind_slave_by_tl(&server, &pa, cannon_tl);
    assert!(ops(&a.drain()).contains(&sc::SCUnitAttachedPacket));
    assert_eq!(
        cannon.data().attached_characters,
        vec![(attach_point::DRIVER, PA)]
    );
    assert_eq!(pa.data().unit.transform.parent_obj_id, cannon.obj_id);
    let t = pa.data().unit.transform;
    assert_eq!((t.x, t.y, t.z), slaves::GUNNER_SEAT_OFFSET);
    b.drain();
    c.drain();

    // The gunner follows the cannon on the moving ship.
    let before = world.world_pos_rot(&WorldObject::from(&pa));
    hull.data().unit.transform.x += 25.0;
    hull.data().unit.transform.y -= 10.0;
    let after = world.world_pos_rot(&WorldObject::from(&pa));
    assert!(
        (after.x - before.x - 25.0).abs() < 1e-3,
        "{before:?} {after:?}"
    );
    assert!((after.y - before.y + 10.0).abs() < 1e-3);

    // Second gunner: a crew member standing on the ship gets the clearing taken-seat 0x61,
    // one gunner per cannon. Put the crew member at the cannon's own local offset so it is
    // inside the 4.0 m bind reach after the hull moved.
    world.attach_child(&WorldObject::from(&hull), &WorldObject::from(&pb));
    let cannon_local = cannon.data().unit.transform;
    pb.data().unit.transform.set_position(
        cannon_local.x,
        cannon_local.y,
        cannon_local.z,
        0.0,
        0.0,
        0.0,
    );
    slaves::bind_slave_by_tl(&server, &pb, cannon_tl);
    assert_eq!(
        body(&b.drain(), sc::SCErrorMsgPacket),
        vec![0x61, 0, 0x61, 0, 0, 0, 0, 0, 0],
        "a taken cannon seat must answer the clearing error"
    );
    assert_eq!(cannon.data().attached_characters.len(), 1);
    // ... and cannot cast its skills either.
    fire(&mut b, 130.0, 130.0);
    assert_eq!(target.data().unit.hp, 1000);
    b.drain();

    // The gunner fires at a ground position: the unit in the area takes the shot.
    fire(&mut a, 130.0, 130.0);
    assert_eq!(target.data().unit.hp, 850, "fixed 150 damage");
    let fa = a.drain();
    assert!(ops(&fa).contains(&sc::SCSkillFiredPacket));
    assert!(ops(&fa).contains(&sc::SCUnitPointsPacket));
    assert!(ops(&b.drain()).contains(&sc::SCSkillFiredPacket));
    // Out of the 4 m area: no damage. Wait out the 150 ms global cooldown so the shot is
    // really cast instead of refused as COOLDOWN_TIME.
    tokio::time::sleep(Duration::from_millis(160)).await;
    fire(&mut a, 60.0, 60.0);
    assert_eq!(target.data().unit.hp, 850);
    assert!(ops(&a.drain()).contains(&sc::SCSkillFiredPacket));

    // A Unit target on a BallisticPos skill is NO_TARGET (0x9), not a shot at the caster.
    start_skill(
        &server,
        &mut a.conn,
        CSStartSkillPacket {
            skill_id: FIRE_CANNON,
            caster: SkillCaster::Mount {
                obj_id: cannon.obj_id,
                mount_skill_template_id: CANNON_MOUNT_SKILL,
            },
            target: SkillCastTarget::Unit(target.obj_id),
            flag: 0,
            skill_object: SkillObject::None,
        },
    );
    let frames = a.drain();
    assert_eq!(
        started_result(&frames, FIRE_CANNON),
        Some(crate::systems::skills::types::skill_result::NO_TARGET),
        "{:x?}",
        frames
    );
    assert_eq!(target.data().unit.hp, 850);

    // Leaving the cannon (CSDiscardSlave) frees the seat for the crew member.
    let discard = super::packets::CSDiscardSlavePacket {
        tl_id: cannon_tl as u16,
    };
    futures::executor::block_on(discard.handle(&server, &mut a.conn)).unwrap();
    assert!(cannon.data().attached_characters.is_empty());
    assert_eq!(pa.data().unit.transform.parent_obj_id, 0);
    assert!(ops(&a.drain()).contains(&sc::SCUnitDetachedPacket));
    b.drain();
    slaves::bind_slave_by_tl(&server, &pb, cannon_tl);
    assert_eq!(
        cannon.data().attached_characters,
        vec![(attach_point::DRIVER, PB)]
    );
    assert!(error_codes(&b.drain()).is_empty());

    // The gunner dying leaves the cannon.
    crate::systems::skills::combat::do_die(
        &server,
        &WorldObject::from(&pb),
        Some(&WorldObject::from(&target)),
        crate::systems::skills::types::KillReason::Damage,
    );
    assert!(cannon.data().attached_characters.is_empty());
    assert_eq!(pb.data().unit.transform.parent_obj_id, 0);

    // Recalling the ship unbinds a gunner too.
    stand_next_to(&world, &pa, &WorldObject::from(&cannon));
    slaves::bind_slave_by_tl(&server, &pa, cannon_tl);
    assert_eq!(cannon.data().attached_characters.len(), 1);
    slaves::delete(&server, Some(&pa), world.id, hull.obj_id, false);
    assert_eq!(pa.data().attached_point, attach_point::NONE);
    assert_eq!(pa.data().unit.transform.parent_obj_id, 0);
    assert!(!pa.data().is_riding);
    assert!(cannon.data().attached_characters.is_empty());
}

/// The skill's `target_angle` cone is measured from the cannon's composed world facing (hull
/// yaw + the attach point's yaw), not from the hull alone. The hull is turned 45 deg and the
/// cannon carries a 45 deg attach yaw, so the cannon faces west; the shot is aimed at a fixed
/// world direction (west of the cannon), which a hull-only or attach-only composition would
/// place 45 deg outside the 50 deg cone.
#[tokio::test]
async fn cannon_fires_at_a_fixed_world_direction_with_the_hull_turned() {
    use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
    let mut d = cannon_data();
    let cannon_skill = d
        .systems
        .skills
        .skills
        .get_mut(&FIRE_CANNON)
        .expect("cannon skill");
    Arc::get_mut(cannon_skill)
        .expect("sole cannon skill reference")
        .target_angle = 50;
    let server = server_with(d);
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    a.conn.active_char = Some(pa.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let hull = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SHIP_HULL,
        None,
        false,
        None,
        None,
    )
    .expect("hull");
    let cannon = world
        .slave(hull.data().attached_slaves[0])
        .expect("cannon child");
    let cannon_tl = u32::from(cannon.data().unit.tl_id);
    // Hull yaw 45 deg + attach yaw 45 deg = cannon world yaw 90 deg (facing -x, west).
    let hull_t = hull.data().unit.transform;
    hull.data().unit.transform.set_position(
        hull_t.x,
        hull_t.y,
        hull_t.z,
        0.0,
        0.0,
        std::f32::consts::FRAC_PI_4,
    );
    let cannon_t = cannon.data().unit.transform;
    cannon.data().unit.transform.set_position(
        cannon_t.x,
        cannon_t.y,
        cannon_t.z,
        0.0,
        0.0,
        std::f32::consts::FRAC_PI_4,
    );
    a.drain();
    stand_next_to(&world, &pa, &WorldObject::from(&cannon));
    slaves::bind_slave_by_tl(&server, &pa, cannon_tl);
    a.drain();

    let origin = world.world_pos_rot(&WorldObject::from(&cannon));
    assert!(
        (origin.rot_z - std::f32::consts::FRAC_PI_2).abs() < 1e-4,
        "{origin:?}"
    );
    let (shot_x, shot_y) = (origin.x - 40.0, origin.y);
    start_skill(
        &server,
        &mut a.conn,
        CSStartSkillPacket {
            skill_id: FIRE_CANNON,
            caster: SkillCaster::Mount {
                obj_id: cannon.obj_id,
                mount_skill_template_id: CANNON_MOUNT_SKILL,
            },
            target: SkillCastTarget::Position {
                obj_id: 0,
                x: shot_x,
                y: shot_y,
                z: 100.0,
                rot: 0.0,
                obj_id1: 0,
                obj_id2: 0,
            },
            flag: 0,
            skill_object: SkillObject::None,
        },
    );
    let frames = a.drain();
    assert!(
        ops(&frames).contains(&sc::SCSkillFiredPacket),
        "{:x?}",
        ops(&frames)
    );
}

/// A reversed `vehicle_models` window (min above max) is bad data, not a panic: the clamp uses
/// the sorted window and the aim is still stored and relayed.
#[tokio::test]
async fn degenerate_turret_window_does_not_panic() {
    let mut d = data();
    d.systems
        .vehicles
        .model_types
        .insert(300, (54, "VehicleModel".into()));
    d.systems.vehicles.vehicle_models.insert(
        54,
        VehicleModel {
            id: 54,
            turret_pitch_angle_min: Some(30.0),
            turret_pitch_angle_max: Some(-30.0),
            turret_yaw_angle_min: Some(45.0),
            turret_yaw_angle_max: Some(-45.0),
            ..VehicleModel::default()
        },
    );
    let server = server_with(d);
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let pa = spawn_player(&server, &a, test_character(1, "Gunner", 100.0, 100.0), PA);
    let pb = spawn_player(&server, &b, test_character(2, "Crew", 101.0, 100.0), PB);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let slave = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SLAVE_TEMPLATE,
        None,
        false,
        None,
        None,
    )
    .expect("vehicle");
    a.drain();
    b.drain();
    stand_next_to(&world, &pa, &WorldObject::from(&slave));
    slaves::bind_slave(
        &server,
        &pa,
        slave.obj_id,
        attach_point::PASSENGER0,
        slaves::attach_reason::SLAVE_BINDING,
        Some(3.0),
    );
    a.drain();
    b.drain();

    send_turret_state(&mut a, &server, slave.obj_id, 1.0, -1.0).await;

    let aim = {
        let d = slave.data();
        (d.turret_pitch, d.turret_yaw)
    };
    assert_eq!(
        aim,
        (30f32.to_radians(), -45f32.to_radians()),
        "the reversed window is sorted, not a panic"
    );
    let fb = b.drain();
    assert_eq!(body(&fb, sc::SCUnitModelPostureChangedPacket), {
        let mut v = bc(slave.obj_id);
        v.extend(turret_posture(aim.0, aim.1));
        v
    });
}

/// The type-8 posture body of `SCUnitModelPostureChangedPacket` / `SCUnitStatePacket`:
/// `u8 posture, bool isLooted, f32 pitch, f32 yaw` (1.2 client `397bda20` -> `397b1df0`).
fn turret_posture(pitch: f32, yaw: f32) -> Vec<u8> {
    let mut v = vec![8, 0];
    v.extend_from_slice(&pitch.to_le_bytes());
    v.extend_from_slice(&yaw.to_le_bytes());
    v
}

fn unit_state_of(frames: &[(u16, Vec<u8>)], obj_id: u32) -> Option<Vec<u8>> {
    frames
        .iter()
        .find(|(op, b)| *op == sc::SCUnitStatePacket && b.starts_with(&bc(obj_id)))
        .map(|f| f.1.clone())
}

async fn send_turret_state(
    client: &mut TestClient,
    server: &GameServer,
    unit_id: u32,
    pitch: f32,
    yaw: f32,
) {
    use crate::net::registry::ClientPacket;
    crate::packets::c2g_misc::CSTurretStatePacket {
        unit_id,
        pitch,
        yaw,
    }
    .handle(server, &mut client.conn)
    .await
    .unwrap();
}

/// A ship cannon gunner (the cannon child slave's driver seat) aims the cannon: the posture is
/// stored and relayed to the other players in range.
#[tokio::test]
async fn seated_gunner_turret_state_is_stored_and_broadcast() {
    let server = server_with(cannon_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let pa = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    let pb = spawn_player(&server, &b, test_character(2, "Crew", 101.0, 100.0), PB);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let hull = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SHIP_HULL,
        None,
        false,
        None,
        None,
    )
    .expect("hull");
    let cannon = world
        .slave(hull.data().attached_slaves[0])
        .expect("cannon child");
    let cannon_tl = u32::from(cannon.data().unit.tl_id);
    a.drain();
    b.drain();
    stand_next_to(&world, &pa, &WorldObject::from(&cannon));
    slaves::bind_slave_by_tl(&server, &pa, cannon_tl);
    a.drain();
    b.drain();

    send_turret_state(&mut a, &server, cannon.obj_id, 0.25, -0.3).await;

    let aim = {
        let d = cannon.data();
        (d.turret_pitch, d.turret_yaw)
    };
    assert_eq!(aim, (0.25, -0.3));
    let fb = b.drain();
    assert_eq!(body(&fb, sc::SCUnitModelPostureChangedPacket), {
        let mut v = bc(cannon.obj_id);
        v.extend(turret_posture(0.25, -0.3));
        v
    });
    assert!(
        !ops(&a.drain()).contains(&sc::SCUnitModelPostureChangedPacket),
        "the aiming client keeps its local aim"
    );
}

/// A player who is not seated on the slave cannot aim it, even while someone else is seated.
#[tokio::test]
async fn unseated_player_cannot_set_turret_state() {
    let server = server_with(cannon_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let pa = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    let pb = spawn_player(&server, &b, test_character(2, "Stranger", 101.0, 100.0), PB);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let hull = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SHIP_HULL,
        None,
        false,
        None,
        None,
    )
    .expect("hull");
    let cannon = world
        .slave(hull.data().attached_slaves[0])
        .expect("cannon child");
    let cannon_tl = u32::from(cannon.data().unit.tl_id);
    a.drain();
    b.drain();
    stand_next_to(&world, &pa, &WorldObject::from(&cannon));
    slaves::bind_slave_by_tl(&server, &pa, cannon_tl);
    a.drain();
    b.drain();

    send_turret_state(&mut b, &server, cannon.obj_id, 0.75, 1.25).await;

    let aim = {
        let d = cannon.data();
        (d.turret_pitch, d.turret_yaw)
    };
    assert_eq!(aim, (0.0, 0.0));
    assert!(!ops(&a.drain()).contains(&sc::SCUnitModelPostureChangedPacket));
    assert!(!ops(&b.drain()).contains(&sc::SCUnitModelPostureChangedPacket));
}

/// A passenger on the hull is not seated on the cannon child, so the cannon aim stays put.
#[tokio::test]
async fn hull_passenger_cannot_aim_the_cannon_child() {
    let server = server_with(cannon_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let pa = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    let pb = spawn_player(&server, &b, test_character(2, "Crew", 101.0, 100.0), PB);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let hull = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SHIP_HULL,
        None,
        false,
        None,
        None,
    )
    .expect("hull");
    let cannon = world
        .slave(hull.data().attached_slaves[0])
        .expect("cannon child");
    a.drain();
    b.drain();
    stand_next_to(&world, &pa, &WorldObject::from(&hull));
    slaves::bind_slave(
        &server,
        &pa,
        hull.obj_id,
        attach_point::PASSENGER0,
        slaves::attach_reason::SLAVE_BINDING,
        Some(3.0),
    );
    a.drain();
    b.drain();

    send_turret_state(&mut a, &server, cannon.obj_id, 0.5, 0.5).await;

    let aim = {
        let d = cannon.data();
        (d.turret_pitch, d.turret_yaw)
    };
    assert_eq!(aim, (0.0, 0.0));
    assert!(!ops(&a.drain()).contains(&sc::SCUnitModelPostureChangedPacket));
    assert!(!ops(&b.drain()).contains(&sc::SCUnitModelPostureChangedPacket));
}

/// The client's `39356990` sends at most every 200 ms and only on change; the server never
/// relays an unchanged aim and relays a changed aim inside its 150 ms window only from the
/// slave tick.
#[tokio::test]
async fn turret_state_is_throttled_and_deduplicated() {
    let server = server_with(cannon_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let pa = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    let pb = spawn_player(&server, &b, test_character(2, "Crew", 101.0, 100.0), PB);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let hull = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SHIP_HULL,
        None,
        false,
        None,
        None,
    )
    .expect("hull");
    let cannon = world
        .slave(hull.data().attached_slaves[0])
        .expect("cannon child");
    let cannon_tl = u32::from(cannon.data().unit.tl_id);
    a.drain();
    b.drain();
    stand_next_to(&world, &pa, &WorldObject::from(&cannon));
    slaves::bind_slave_by_tl(&server, &pa, cannon_tl);
    a.drain();
    b.drain();

    let posture = |pitch: f32, yaw: f32| {
        let mut v = bc(cannon.obj_id);
        v.extend(turret_posture(pitch, yaw));
        v
    };

    send_turret_state(&mut a, &server, cannon.obj_id, 0.25, -0.3).await;
    assert_eq!(
        body(&b.drain(), sc::SCUnitModelPostureChangedPacket),
        posture(0.25, -0.3)
    );

    // Changed, but inside the window: stored at once, relayed by the tick after the window.
    send_turret_state(&mut a, &server, cannon.obj_id, 0.35, -0.32).await;
    assert!(!ops(&b.drain()).contains(&sc::SCUnitModelPostureChangedPacket));
    let aim = {
        let d = cannon.data();
        (d.turret_pitch, d.turret_yaw)
    };
    assert_eq!(aim, (0.35, -0.32));

    tokio::time::sleep(Duration::from_millis(160)).await;
    super::active_region_tick(&server);
    assert_eq!(
        body(&b.drain(), sc::SCUnitModelPostureChangedPacket),
        posture(0.35, -0.32)
    );

    // Outside the window but unchanged: dropped, and the tick does not relay it again.
    tokio::time::sleep(Duration::from_millis(160)).await;
    send_turret_state(&mut a, &server, cannon.obj_id, 0.35, -0.32).await;
    assert!(!ops(&b.drain()).contains(&sc::SCUnitModelPostureChangedPacket));
    super::active_region_tick(&server);
    assert!(!ops(&b.drain()).contains(&sc::SCUnitModelPostureChangedPacket));

    // Outside the window and changed: relayed at once.
    send_turret_state(&mut a, &server, cannon.obj_id, 0.45, -0.34).await;
    assert_eq!(
        body(&b.drain(), sc::SCUnitModelPostureChangedPacket),
        posture(0.45, -0.34)
    );
}

/// Two changed aims that reach the server 50 ms apart (network jitter on two client sends
/// 200 ms apart): the second one is stored at once and the tick relays it after the window,
/// so a gunner who stops there does not leave the viewers on the first aim.
#[tokio::test]
async fn turret_aim_inside_the_window_is_stored_and_relayed_by_the_tick() {
    let server = server_with(cannon_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let pa = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    let pb = spawn_player(&server, &b, test_character(2, "Crew", 101.0, 100.0), PB);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let hull = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SHIP_HULL,
        None,
        false,
        None,
        None,
    )
    .expect("hull");
    let cannon = world
        .slave(hull.data().attached_slaves[0])
        .expect("cannon child");
    let cannon_tl = u32::from(cannon.data().unit.tl_id);
    a.drain();
    b.drain();
    stand_next_to(&world, &pa, &WorldObject::from(&cannon));
    slaves::bind_slave_by_tl(&server, &pa, cannon_tl);
    a.drain();
    b.drain();

    send_turret_state(&mut a, &server, cannon.obj_id, 0.25, -0.3).await;
    b.drain();
    tokio::time::sleep(Duration::from_millis(50)).await;
    send_turret_state(&mut a, &server, cannon.obj_id, 0.35, -0.32).await;

    let aim = {
        let d = cannon.data();
        (d.turret_pitch, d.turret_yaw)
    };
    assert_eq!(aim, (0.35, -0.32), "the newest aim is stored at once");
    assert!(!ops(&b.drain()).contains(&sc::SCUnitModelPostureChangedPacket));

    tokio::time::sleep(Duration::from_millis(160)).await;
    super::active_region_tick(&server);
    let mut expected = bc(cannon.obj_id);
    expected.extend(turret_posture(0.35, -0.32));
    assert_eq!(
        body(&b.drain(), sc::SCUnitModelPostureChangedPacket),
        expected
    );
}

/// A viewer who arrives while the newest aim still waits for the throttle window gets that
/// newest aim in its `SCUnitState`, not the one the players around last received.
#[tokio::test]
async fn late_viewer_sees_the_newest_turret_aim() {
    let server = server_with(cannon_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    a.conn.active_char = Some(pa.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let hull = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SHIP_HULL,
        None,
        false,
        None,
        None,
    )
    .expect("hull");
    let cannon = world
        .slave(hull.data().attached_slaves[0])
        .expect("cannon child");
    let cannon_tl = u32::from(cannon.data().unit.tl_id);
    a.drain();
    stand_next_to(&world, &pa, &WorldObject::from(&cannon));
    slaves::bind_slave_by_tl(&server, &pa, cannon_tl);
    a.drain();
    send_turret_state(&mut a, &server, cannon.obj_id, 0.25, -0.3).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    send_turret_state(&mut a, &server, cannon.obj_id, 0.35, -0.32).await;
    a.drain();

    let mut viewer = TestClient::new(&server, 3, 3);
    spawn_player(
        &server,
        &viewer,
        test_character(3, "Viewer", 102.0, 100.0),
        0x00F0_0003,
    );
    let fv = viewer.drain();
    let state = unit_state_of(&fv, cannon.obj_id).expect("cannon SCUnitState for the late viewer");
    let posture = turret_posture(0.35, -0.32);
    assert!(
        state.windows(posture.len()).any(|w| w == posture),
        "newest turret posture missing from {:02x?}",
        state
    );
}

/// A passenger seat (point 2) is a legitimate aim path too (the 1.2 tank gunner models sit
/// there); the packet names the slave whose model carries the turret.
#[tokio::test]
async fn passenger_seat_can_set_the_slave_turret_state() {
    // The cart's model 300 is a `VehicleModel` here: the client only sends turret aim for a
    // `VehicleUnitModel` (`FUN_390b3750`), so a slave without one has no turret (see
    // `non_vehicle_slave_gets_no_posture_relay`).
    let mut d = data();
    d.systems
        .vehicles
        .model_types
        .insert(300, (54, "VehicleModel".into()));
    d.systems.vehicles.vehicle_models.insert(
        54,
        VehicleModel {
            id: 54,
            turret_pitch_angle_min: Some(-45.0),
            turret_pitch_angle_max: Some(45.0),
            turret_yaw_angle_min: Some(-180.0),
            turret_yaw_angle_max: Some(180.0),
            ..VehicleModel::default()
        },
    );
    let server = server_with(d);
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let pa = spawn_player(&server, &a, test_character(1, "Gunner", 100.0, 100.0), PA);
    let pb = spawn_player(&server, &b, test_character(2, "Crew", 101.0, 100.0), PB);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let slave = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SLAVE_TEMPLATE,
        None,
        false,
        None,
        None,
    )
    .expect("vehicle");
    a.drain();
    b.drain();
    stand_next_to(&world, &pa, &WorldObject::from(&slave));
    slaves::bind_slave(
        &server,
        &pa,
        slave.obj_id,
        attach_point::PASSENGER0,
        slaves::attach_reason::SLAVE_BINDING,
        Some(3.0),
    );
    a.drain();
    b.drain();

    send_turret_state(&mut a, &server, slave.obj_id, -0.1, 1.5).await;

    let aim = {
        let d = slave.data();
        (d.turret_pitch, d.turret_yaw)
    };
    assert_eq!(aim, (-0.1, 1.5));
    let fb = b.drain();
    assert_eq!(body(&fb, sc::SCUnitModelPostureChangedPacket), {
        let mut v = bc(slave.obj_id);
        v.extend(turret_posture(-0.1, 1.5));
        v
    });
}

/// Turret aim outside the model's limits is clamped before it is stored and relayed
/// (`vehicle_models` 10: pitch -20..35 deg, yaw -22..22 deg), like the client's `39425490`.
#[tokio::test]
async fn turret_state_outside_the_model_limits_is_clamped() {
    let server = server_with(cannon_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let pa = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    let pb = spawn_player(&server, &b, test_character(2, "Crew", 101.0, 100.0), PB);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let hull = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SHIP_HULL,
        None,
        false,
        None,
        None,
    )
    .expect("hull");
    let cannon = world
        .slave(hull.data().attached_slaves[0])
        .expect("cannon child");
    let cannon_tl = u32::from(cannon.data().unit.tl_id);
    a.drain();
    b.drain();
    stand_next_to(&world, &pa, &WorldObject::from(&cannon));
    slaves::bind_slave_by_tl(&server, &pa, cannon_tl);
    a.drain();
    b.drain();

    send_turret_state(&mut a, &server, cannon.obj_id, 1.0, -1.0).await;

    let (pitch, yaw) = (35f32.to_radians(), -22f32.to_radians());
    let aim = {
        let d = cannon.data();
        (d.turret_pitch, d.turret_yaw)
    };
    assert_eq!(aim, (pitch, yaw));
    let fb = b.drain();
    assert_eq!(body(&fb, sc::SCUnitModelPostureChangedPacket), {
        let mut v = bc(cannon.obj_id);
        v.extend(turret_posture(pitch, yaw));
        v
    });
}

/// A slave whose model is not a `VehicleModel` has no turret (the client needs a
/// `VehicleUnitModel` to aim), so an aim packet is ignored: nothing is stored or relayed.
#[tokio::test]
async fn non_vehicle_slave_gets_no_posture_relay() {
    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let mut b = TestClient::new(&server, 2, 2);
    let pa = spawn_player(&server, &a, test_character(1, "Gunner", 100.0, 100.0), PA);
    let pb = spawn_player(&server, &b, test_character(2, "Crew", 101.0, 100.0), PB);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let slave = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SLAVE_TEMPLATE,
        None,
        false,
        None,
        None,
    )
    .expect("vehicle");
    a.drain();
    b.drain();
    stand_next_to(&world, &pa, &WorldObject::from(&slave));
    slaves::bind_slave(
        &server,
        &pa,
        slave.obj_id,
        attach_point::PASSENGER0,
        slaves::attach_reason::SLAVE_BINDING,
        Some(3.0),
    );
    a.drain();
    b.drain();

    send_turret_state(&mut a, &server, slave.obj_id, 0.1, 0.2).await;

    let aim = {
        let d = slave.data();
        (d.turret_pitch, d.turret_yaw)
    };
    assert_eq!(aim, (0.0, 0.0));
    assert!(!ops(&b.drain()).contains(&sc::SCUnitModelPostureChangedPacket));
}

/// A viewer who arrives after the aim was set gets the stored posture in its `SCUnitState`.
#[tokio::test]
async fn late_viewer_receives_the_stored_turret_state() {
    let server = server_with(cannon_data());
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "Owner", 100.0, 100.0), PA);
    a.conn.active_char = Some(pa.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let hull = slaves::create_core(
        &server,
        &world,
        Some(&pa),
        None,
        SHIP_HULL,
        None,
        false,
        None,
        None,
    )
    .expect("hull");
    let cannon = world
        .slave(hull.data().attached_slaves[0])
        .expect("cannon child");
    let cannon_tl = u32::from(cannon.data().unit.tl_id);
    a.drain();
    stand_next_to(&world, &pa, &WorldObject::from(&cannon));
    slaves::bind_slave_by_tl(&server, &pa, cannon_tl);
    a.drain();
    send_turret_state(&mut a, &server, cannon.obj_id, 0.25, -0.3).await;
    a.drain();

    let mut viewer = TestClient::new(&server, 3, 3);
    spawn_player(
        &server,
        &viewer,
        test_character(3, "Viewer", 102.0, 100.0),
        0x00F0_0003,
    );
    let fv = viewer.drain();
    let state = unit_state_of(&fv, cannon.obj_id).expect("cannon SCUnitState for the late viewer");
    let posture = turret_posture(0.25, -0.3);
    assert!(
        state.windows(posture.len()).any(|w| w == posture),
        "stored turret posture missing from {:02x?}",
        state
    );
}

/// The real 1.2 cannon slaves accept and store turret aim: 10 / 97 / 93 / 100 are the
/// `SLAVE_EQUIPMENT` ship-cannon children (models 117 / 1255 / 1277) and 77 the standalone
/// equipment cannon (`slave_kind_id` 7), each with a `slave_mount_skills` firing mount.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_cannon_slaves_store_the_turret_state() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let d = load_real(&db, &[]);
    // The turret clamp reads this real row: model 117 -> vm 10, the cannon's limits.
    let vm = d.vehicle_model(117).expect("model 117 vehicle model");
    assert_eq!(
        (
            vm.turret_pitch_angle_min,
            vm.turret_pitch_angle_max,
            vm.turret_yaw_angle_min,
            vm.turret_yaw_angle_max,
        ),
        (Some(-20.0), Some(35.0), Some(-22.0), Some(22.0))
    );
    // The training ballista (slave 43, model 6 -> vm 6): `turret_yaw_angle_max` is the text
    // 'f' in the real data and is kept as "no limit" instead of the C# coerce-to-0.0.
    let ballista = d.vehicle_model(6).expect("model 6 vehicle model");
    assert_eq!(
        (
            ballista.turret_pitch_angle_min,
            ballista.turret_pitch_angle_max,
            ballista.turret_yaw_angle_min,
            ballista.turret_yaw_angle_max,
        ),
        (Some(-0.1), Some(0.1), Some(0.0), None)
    );
    for id in [10u32, 97, 93, 100, 77] {
        let t = d.slave_template(id).unwrap_or_else(|| panic!("slave {id}"));
        assert!(
            matches!(t.slave_kind, 7 | 8),
            "slave {id} kind {}",
            t.slave_kind
        );
        assert!(
            d.slave_mount_skills.iter().any(|(_, s, _)| *s == id),
            "slave {id} mount skill"
        );
    }
    let server = real_server(d);
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_player(&server, &a, test_character(1, "Gunner", 100.0, 100.0), PA);
    a.conn.active_char = Some(pa.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    a.drain();

    for (i, id) in [10u32, 97, 93, 100, 77].into_iter().enumerate() {
        let t = server
            .game_data
            .systems
            .vehicles
            .slave_template(id)
            .expect("slave template")
            .clone();
        let mut s = crate::models::slave::Slave::new(0, t.id);
        s.unit.hp = 1000;
        s.unit.tl_id = server.ids.tl.get_next_id().expect("tl") as u16;
        s.unit.name = t.name.clone();
        s.unit.model_id = t.model_id;
        s.unit.transform.set_xyz(100.0, 100.0, 100.0);
        s.unit.level = t.level as u8;
        s.summoner_id = pa.character_id;
        s.summoner_obj_id = PA;
        s.summoner_name = pa.data().unit.name.clone();
        let obj_id = server.ids.object.get_next_id().expect("obj");
        let slave = crate::world::Entity::new(obj_id, s);
        world.spawn(&slave, &server.view_ctx());
        slaves::bind_slave(
            &server,
            &pa,
            slave.obj_id,
            attach_point::DRIVER,
            slaves::attach_reason::NEW_MASTER,
            Some(4.0),
        );
        assert_eq!(
            slave.data().attached_characters,
            vec![(attach_point::DRIVER, PA)]
        );
        a.drain();
        // Inside every cannon model's limits (the narrowest here is vm 10's -22..22 deg).
        let pitch = 0.05 * (i as f32 + 1.0);
        let yaw = -0.05 * (i as f32 + 1.0);
        send_turret_state(&mut a, &server, slave.obj_id, pitch, yaw).await;
        let (stored, tl) = {
            let d = slave.data();
            ((d.turret_pitch, d.turret_yaw), u32::from(d.unit.tl_id))
        };
        assert_eq!(stored, (pitch, yaw), "slave {id}");
        slaves::unbind_slave(&server, &pa, tl, 0);
        a.drain();
    }
}

/// A slave spawned from a real `slaves` template (no hull / equipment path).
fn spawn_real_slave(
    server: &GameServer,
    world: &Arc<crate::world::WorldInstance>,
    summoner: &PlayerRef,
    template_id: u32,
    at: (f32, f32, f32),
) -> SlaveRef {
    let t = server
        .game_data
        .systems
        .vehicles
        .slave_template(template_id)
        .unwrap_or_else(|| panic!("slave {template_id}"))
        .clone();
    let mut s = crate::models::slave::Slave::new(0, t.id);
    s.unit.hp = 1000;
    s.unit.tl_id = server.ids.tl.get_next_id().expect("tl") as u16;
    s.unit.name = t.name.clone();
    s.unit.model_id = t.model_id;
    s.unit.transform.set_xyz(at.0, at.1, at.2);
    s.unit.level = t.level as u8;
    s.summoner_id = summoner.character_id;
    s.summoner_obj_id = summoner.obj_id;
    s.summoner_name = summoner.data().unit.name.clone();
    let obj_id = server.ids.object.get_next_id().expect("obj");
    let slave = crate::world::Entity::new(obj_id, s);
    world.spawn(&slave, &server.view_ctx());
    slave
}

/// The real 1.2 rows that a model-limit refusal broke. The client evaluates `CanUse` on the
/// aimed Position before it clamps the turret to the model (`FUN_394322f0` step 7, the clamp
/// in step 8), so these are legal shots even though the turret cannot reach them:
/// - slave 43 training ballista, vm 6 (pitch -0.1..0.1 deg, `turret_yaw_angle_max` = 'f'),
///   10 deg off-axis inside the skill's 40 deg `target_angle`;
/// - slave 93 tower cannon, vm 42 (pitch min -20 deg), 15 m above a target 30 m out;
/// - slave 77 ship cannon, vm 10 (pitch min -20 deg), 20 m above a target 40 m out.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_cannons_fire_where_the_model_limits_would_have_refused() {
    use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};

    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let server = real_skill_server(&db);
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::install_hooks(&server);
    super::install_hooks(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);

    // (slave template, skill, caster position, fixed world target position)
    type Point = (f32, f32, f32);
    type Case = (u32, u32, Point, Point);
    let cases: [Case; 3] = [
        (43, 13562, (100.0, 100.0, 100.0), (106.946, 139.392, 100.0)),
        (93, 19439, (100.0, 100.0, 115.0), (100.0, 130.0, 100.0)),
        (77, 17742, (100.0, 100.0, 120.0), (100.0, 140.0, 100.0)),
    ];
    for (i, (slave_id, skill_id, at, target)) in cases.into_iter().enumerate() {
        // A fresh player per case: a successful cast arms the global cooldown.
        let obj_id = 0x00F0_0100 + i as u32;
        let pa = spawn_player(
            &server,
            &client,
            test_character(i as u32 + 1, "Gunner", at.0, at.1),
            obj_id,
        );
        pa.data().unit.transform.set_xyz(at.0, at.1, at.2);
        pa.data().unit.hp = 1000;
        pa.data().unit.max_hp = 1000;
        client.conn.active_char = Some(pa.clone());
        // The tower cannon's rider skill 14372 requires one steel cannonball (23671).
        let item_template = server
            .game_data
            .items
            .template(23671)
            .expect("steel cannonball")
            .clone();
        let mut item = Item::create(
            0x9000 + i as u64,
            &item_template,
            1,
            0,
            1,
            &server.game_data.items,
            0,
        );
        item.owner_id = u64::from(pa.character_id);
        item.slot_type = slot_type::INVENTORY;
        item.slot = i as i32;
        let bag = pa
            .data()
            .inventory
            .get(slot_type::INVENTORY)
            .expect("bag")
            .clone();
        bag.lock().unwrap().items.push(item);
        let mount_id = server
            .game_data
            .systems
            .vehicles
            .slave_mount_skills
            .iter()
            .find(|(_, slave, _)| *slave == slave_id)
            .map(|(_, _, mount)| *mount)
            .unwrap_or_else(|| panic!("slave {slave_id} mount skill"));
        assert_eq!(
            server
                .game_data
                .systems
                .vehicles
                .mount_skill_skill_id(mount_id),
            skill_id,
            "slave {slave_id}"
        );
        let slave = spawn_real_slave(&server, &world, &pa, slave_id, at);
        slaves::bind_slave(
            &server,
            &pa,
            slave.obj_id,
            attach_point::DRIVER,
            slaves::attach_reason::NEW_MASTER,
            Some(4.0),
        );
        assert_eq!(
            slave.data().attached_characters,
            vec![(attach_point::DRIVER, obj_id)]
        );
        client.drain();
        start_skill(
            &server,
            &mut client.conn,
            CSStartSkillPacket {
                skill_id,
                caster: SkillCaster::Mount {
                    obj_id: slave.obj_id,
                    mount_skill_template_id: mount_id,
                },
                target: SkillCastTarget::Position {
                    obj_id: 0,
                    x: target.0,
                    y: target.1,
                    z: target.2,
                    rot: 0.0,
                    obj_id1: 0,
                    obj_id2: 0,
                },
                flag: 0,
                skill_object: SkillObject::None,
            },
        );
        let frames = client.drain();
        assert!(
            ops(&frames).contains(&sc::SCSkillFiredPacket),
            "slave {slave_id} skill {skill_id}: {:x?}",
            ops(&frames)
        );
        let tl = u32::from(slave.data().unit.tl_id);
        slaves::unbind_slave(&server, &pa, tl, 0);
        client.drain();
    }
}

// ---------------------------------------------------------------------------------------------
// Mount rider skills (`mount_attached_skills`): the client names the `mount_skills` row
// ---------------------------------------------------------------------------------------------

use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
use crate::world::Entity;

const WATER_MOUNT_SKILL: u32 = 15_546; // slave 146 bar row 59 -> skill 15546
const WATER_MOUNT_ROW: u32 = 59; // '물 퍼올리기' (real)
const WATER_SAMPLE_MOUNT_ROW: u32 = 226; // '물 퍼올리기(견본)' (sample, same skill id)
const WATER_RIDER_SKILL: u32 = 21_505; // row 59, attach point 1 (driver)
const WATER_SAMPLE_RIDER_SKILL: u32 = 15_548; // row 226, attach point 1 (display only)
const WATERING_MOUNT_SKILL: u32 = 15_450; // slave 146 bar row 56 -> skill 15450
const WATERING_MOUNT_ROW: u32 = 56;
const WATERING_SAMPLE_MOUNT_ROW: u32 = 225;
const WATERING_RIDER_SKILL: u32 = 21_504; // row 56, attach point 1 (consumes 10 water)
const RED_FARM_HAULER: u32 = 146;
const WATER_ITEM: u32 = 15_694;
const WATER_LOOT_PACK: u32 = 7_732;
const WATER_LOOT: u32 = 71_267;
const WATER_COUNT: i32 = 10;

/// The mount skill ids carried by a skill id's `mount_skills` rows, in 1.2 rowid order.
fn mount_rows_for_skill(server: &GameServer, skill_id: u32) -> Vec<u32> {
    server
        .game_data
        .systems
        .vehicles
        .mount_skills
        .iter()
        .filter(|m| m.skill_id == skill_id)
        .map(|m| m.id)
        .collect()
}

fn skill_ids_of(frames: &[(u16, Vec<u8>)], op: u16) -> Vec<u32> {
    frames
        .iter()
        .filter(|(o, _)| *o == op)
        .map(|(_, body)| u32::from_le_bytes([body[0], body[1], body[2], body[3]]))
        .collect()
}

fn water_count(player: &PlayerRef) -> i32 {
    crate::systems::economy::inventory::Inv::of(player).items_count(WATER_ITEM, -1)
}

fn give_water(server: &GameServer, player: &PlayerRef, item_id: u64, count: i32) {
    use crate::models::item::Item;
    use crate::systems::economy::inventory::Inv;
    let template = server
        .game_data
        .items
        .template(WATER_ITEM)
        .expect("water template");
    let mut item = Item::create(item_id, template, count, 0, 1, &server.game_data.items, 0);
    item.owner_id = u64::from(player.character_id);
    item.slot = -1;
    let bag = Inv::of(player).bag();
    bag.lock()
        .expect("bag")
        .add_new_item(item, 0, &server.game_data.items)
        .expect("water");
    server.items.add_container(bag);
}

/// The real 1.2 rows of the Red Farm Hauler and of the tractor pair, with the duplicate
/// `mount_skills` rows in their real order: 226 '물 퍼올리기(견본)' (rowid 132) before 59
/// '물 퍼올리기' (rowid 316), and 225 '물 뿌리기(견본)' before 56 '물 뿌리기'. Resolving the
/// rider by skill id picked the sample row's display-only 15548.
fn mount_rider_data() -> GameData {
    use super::data::{MountAttachedSkills, MountSkills};
    use crate::systems::economy::loot_data::{Loot, LootPack};
    use crate::systems::skills::data::effects::{EffectTemplate, GainLootPackItemEffect};
    use crate::systems::skills::data::{EffectKind, SkillItemAmount};

    let mut data = crate::systems::economy::tests::test_data();
    let skills = &mut data.systems.skills;
    skills.add_effect(
        EffectKind::GainLootPackItemEffect,
        EffectTemplate::GainLootPackItem(GainLootPackItemEffect {
            id: 251,
            loot_pack_id: WATER_LOOT_PACK,
            consume_source_item: false,
            consume_item_id: 0,
            consume_count: 0,
            inherit_grade: false,
        }),
        251,
    );
    let mut mount = crate::systems::skills::tests::skill_template(WATER_MOUNT_SKILL);
    mount.target_type = 5; // AnyUnit
    mount.casting_time = 5_000;
    mount.damage_type_id = 0;
    skills.add_skill(mount);
    let mut sample = crate::systems::skills::tests::skill_template(WATER_SAMPLE_RIDER_SKILL);
    sample.target_type = 0;
    sample.damage_type_id = 0;
    skills.add_skill(sample);
    let mut rider = crate::systems::skills::tests::skill_template(WATER_RIDER_SKILL);
    rider.target_type = 0;
    rider.casting_time = 5_000;
    rider.damage_type_id = 0;
    let mut effect = crate::systems::skills::tests::skill_effect(
        29_504,
        EffectKind::GainLootPackItemEffect,
        251,
    );
    effect.application_method = 3; // SourceOnce
    effect.friendly = true;
    effect.non_friendly = true;
    rider.effects.push(effect);
    skills.add_skill(rider);
    let mut watering = crate::systems::skills::tests::skill_template(WATERING_MOUNT_SKILL);
    watering.target_type = 5;
    watering.damage_type_id = 0;
    skills.add_skill(watering);
    let mut watering_rider = crate::systems::skills::tests::skill_template(WATERING_RIDER_SKILL);
    watering_rider.target_type = 0;
    watering_rider.damage_type_id = 0;
    skills.add_skill(watering_rider);
    skills.skill_reagents.push(SkillItemAmount {
        id: 2_373,
        skill_id: WATERING_RIDER_SKILL,
        item_id: WATER_ITEM,
        amount: WATER_COUNT as i16,
    });

    let vehicles = &mut data.systems.vehicles;
    for (id, skill_id) in [
        (WATER_SAMPLE_MOUNT_ROW, WATER_MOUNT_SKILL),
        (WATER_MOUNT_ROW, WATER_MOUNT_SKILL),
        (WATERING_SAMPLE_MOUNT_ROW, WATERING_MOUNT_SKILL),
        (WATERING_MOUNT_ROW, WATERING_MOUNT_SKILL),
    ] {
        vehicles.mount_skills.push(MountSkills {
            id,
            name: format!("mount {id}"),
            skill_id,
        });
    }
    for (id, mount_skill_id, skill_id) in [
        (320, WATER_SAMPLE_MOUNT_ROW, WATER_SAMPLE_RIDER_SKILL),
        (192, WATER_MOUNT_ROW, WATER_RIDER_SKILL),
        (321, WATERING_SAMPLE_MOUNT_ROW, WATER_SAMPLE_RIDER_SKILL),
        (231, WATERING_MOUNT_ROW, WATERING_RIDER_SKILL),
    ] {
        vehicles.mount_attached_skills.push(MountAttachedSkills {
            id,
            mount_skill_id,
            attach_point_id: attach_point::DRIVER,
            skill_id,
        });
    }
    vehicles
        .slave_mount_skills
        .push((443, RED_FARM_HAULER, WATER_MOUNT_ROW));
    vehicles
        .slave_mount_skills
        .push((442, RED_FARM_HAULER, WATERING_MOUNT_ROW));

    let loot = Loot {
        id: WATER_LOOT,
        group: 1,
        item_id: WATER_ITEM,
        drop_rate: 10_000_000,
        min_amount: WATER_COUNT,
        max_amount: WATER_COUNT,
        loot_pack_id: WATER_LOOT_PACK,
        grade_id: 0,
        always_drop: false,
    };
    data.systems.economy.loot.packs.insert(
        WATER_LOOT_PACK,
        LootPack {
            id: WATER_LOOT_PACK,
            group_count: 1,
            loots: vec![loot],
            groups: Vec::new(),
            actability_groups: Vec::new(),
            loots_by_group_no: vec![(1, vec![loot])],
        },
    );
    data.items
        .templates
        .insert(WATER_ITEM, ItemTemplate::plain(WATER_ITEM, 1_000));
    data
}

fn mount_rider_server() -> Arc<GameServer> {
    let server = server_with(mount_rider_data());
    crate::systems::skills::bind(&server);
    crate::systems::economy::bridges::install(&server);
    server
}

/// A player bound to the driver seat of a Red Farm Hauler (template 146).
fn bind_water_driver(server: &Arc<GameServer>, client: &mut TestClient) -> (PlayerRef, SlaveRef) {
    let player = spawn_player(
        server,
        client,
        test_character(1, "Driver", 100.0, 100.0),
        PA,
    );
    {
        let mut d = player.data();
        d.unit.hp = 1_000;
        d.unit.max_hp = 1_000;
        d.unit.mp = 1_000;
        d.unit.max_mp = 1_000;
    }
    client.conn.active_char = Some(player.clone());
    let world = server.world.world(0).unwrap();
    let mut s = crate::models::slave::Slave::new(0x300, RED_FARM_HAULER);
    s.unit.hp = 1_000;
    s.unit.max_hp = 1_000;
    s.unit.tl_id = server.ids.tl.get_next_id().expect("tl") as u16;
    s.summoner_id = player.character_id;
    s.summoner_obj_id = player.obj_id;
    s.summoner_name = player.data().unit.name.clone();
    s.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let slave = Entity::new(0x300, s);
    world.spawn(&slave, &server.view_ctx());
    let tl = u32::from(slave.data().unit.tl_id);
    slaves::bind_slave_by_tl(server, &player, tl);
    client.drain();
    (player, slave)
}

fn water_cast(slave_obj_id: u32, skill_id: u32, mount_skill_id: u32) -> CSStartSkillPacket {
    CSStartSkillPacket {
        skill_id,
        caster: SkillCaster::Mount {
            obj_id: slave_obj_id,
            mount_skill_template_id: mount_skill_id,
        },
        target: SkillCastTarget::Unit(slave_obj_id),
        flag: 0,
        skill_object: SkillObject::None,
    }
}

#[tokio::test(start_paused = true)]
async fn water_rider_follows_the_mount_skill_row_the_client_pressed() {
    let server = mount_rider_server();
    assert_eq!(
        mount_rows_for_skill(&server, WATER_MOUNT_SKILL),
        vec![WATER_SAMPLE_MOUNT_ROW, WATER_MOUNT_ROW],
        "the 1.2 data lists the sample row first; resolving by skill id picks it"
    );
    let mut client = TestClient::new(&server, 1, 1);
    let (player, slave) = bind_water_driver(&server, &mut client);
    assert_eq!(player.data().attached_point, attach_point::DRIVER);
    start_skill(
        &server,
        &mut client.conn,
        water_cast(slave.obj_id, WATER_MOUNT_SKILL, WATER_MOUNT_ROW),
    );
    let frames = client.drain();
    assert!(
        skill_ids_of(&frames, sc::SCSkillStartedPacket).contains(&WATER_RIDER_SKILL),
        "the driver rider 21505 must start: {:x?}",
        ops(&frames)
    );
    assert!(
        !skill_ids_of(&frames, sc::SCSkillFiredPacket).contains(&WATER_SAMPLE_RIDER_SKILL),
        "the sample rider 15548 must not run: {:x?}",
        ops(&frames)
    );
    assert_eq!(
        water_count(&player),
        0,
        "the rider grants at the end of its cast"
    );
    // Let the spawned cast tasks register their timers before visiting the deadline.
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_millis(5_100)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    let frames = client.drain();
    assert!(
        skill_ids_of(&frames, sc::SCSkillFiredPacket).contains(&WATER_RIDER_SKILL),
        "the 5 s rider cast must fire: {:x?}",
        ops(&frames)
    );
    assert!(
        ops(&frames).contains(&sc::SCItemTaskSuccessPacket),
        "the client must see the item task: {:x?}",
        ops(&frames)
    );
    assert_eq!(water_count(&player), WATER_COUNT, "10 x item 15694");
}

#[tokio::test(start_paused = true)]
async fn water_rider_needs_the_driver_attach_point() {
    let server = mount_rider_server();
    let mut client = TestClient::new(&server, 1, 1);
    let (player, slave) = bind_water_driver(&server, &mut client);
    // A passenger is not the attach point the rider row is bound to.
    player.data().attached_point = attach_point::PASSENGER0;
    start_skill(
        &server,
        &mut client.conn,
        water_cast(slave.obj_id, WATER_MOUNT_SKILL, WATER_MOUNT_ROW),
    );
    let frames = client.drain();
    assert!(
        !skill_ids_of(&frames, sc::SCSkillStartedPacket).contains(&WATER_RIDER_SKILL),
        "a passenger must not start the driver rider: {:x?}",
        ops(&frames)
    );
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_millis(5_100)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    let frames = client.drain();
    assert!(
        skill_ids_of(&frames, sc::SCSkillFiredPacket).contains(&WATER_MOUNT_SKILL),
        "the mount skill itself still casts: {:x?}",
        ops(&frames)
    );
    assert_eq!(water_count(&player), 0, "a passenger gets no water");
}

#[tokio::test(start_paused = true)]
async fn watering_rider_follows_the_mount_skill_row_and_consumes_ten_water() {
    let server = mount_rider_server();
    assert_eq!(
        mount_rows_for_skill(&server, WATERING_MOUNT_SKILL),
        vec![WATERING_SAMPLE_MOUNT_ROW, WATERING_MOUNT_ROW],
        "the 1.2 data lists the watering sample row first"
    );
    let mut client = TestClient::new(&server, 1, 1);
    let (player, slave) = bind_water_driver(&server, &mut client);
    give_water(&server, &player, 0x7001, WATER_COUNT);
    assert_eq!(water_count(&player), WATER_COUNT);
    start_skill(
        &server,
        &mut client.conn,
        water_cast(slave.obj_id, WATERING_MOUNT_SKILL, WATERING_MOUNT_ROW),
    );
    let frames = client.drain();
    let fired = skill_ids_of(&frames, sc::SCSkillFiredPacket);
    assert!(fired.contains(&WATERING_RIDER_SKILL), "{:x?}", ops(&frames));
    assert!(
        !fired.contains(&WATER_SAMPLE_RIDER_SKILL),
        "the sample rider must not run: {:x?}",
        ops(&frames)
    );
    assert_eq!(water_count(&player), 0, "the rider consumes 10 water");
}

// ---------------------------------------------------------------------------------------------
// Real data (gated): the Red Farm Hauler driver's Create Water
// ---------------------------------------------------------------------------------------------

/// The real 1.2 skill, vehicle, item and loot rows the water cast needs.
fn real_mount_rider_server(db: &str) -> Arc<GameServer> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let app = app_path();
    let files = ClientFileManager::from_sources(&[], &app);
    let ctx = DataLoadCtx {
        conn: &conn,
        app_path: &app,
        files: &files,
        language: "en_us",
    };
    let mut data = GameData::empty();
    data.items = crate::data::ItemData::load(&conn).expect("item data");
    data.systems.vehicles = super::Data::load(&ctx).expect("vehicles data");
    data.systems.skills = crate::systems::skills::Data::load(&ctx).expect("skills data");
    data.systems.economy = crate::systems::economy::Data::load(&ctx).expect("economy data");
    let server = real_game_server(data);
    crate::systems::skills::bind(&server);
    crate::systems::economy::bridges::install(&server);
    server
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_farm_hauler_driver_gets_ten_water_from_skill_15546() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let server = real_mount_rider_server(&db);
    let d = &server.game_data.systems.vehicles;
    assert_eq!(
        d.slave_mount_skill_list(RED_FARM_HAULER),
        vec![201, 369, 65, 56, 59],
        "slave 146 bar (slave_mount_skills rows 439-443)"
    );
    assert_eq!(
        d.mount_attached_skill(WATER_MOUNT_ROW, 1),
        WATER_RIDER_SKILL
    );
    assert_eq!(
        d.mount_attached_skill(WATER_SAMPLE_MOUNT_ROW, 1),
        WATER_SAMPLE_RIDER_SKILL
    );
    assert_eq!(
        d.mount_attached_skill(WATERING_MOUNT_ROW, 1),
        WATERING_RIDER_SKILL
    );
    // Every row of the real table resolves by its own `(mount_skill_id, attach_point_id)`.
    for row in &d.mount_attached_skills {
        assert_eq!(
            d.mount_attached_skill(row.mount_skill_id, row.attach_point_id),
            row.skill_id,
            "mount_attached_skills row {}",
            row.id
        );
    }

    let mut client = TestClient::new(&server, 1, 1);
    let pa = spawn_player(
        &server,
        &client,
        test_character(1, "Driver", 100.0, 100.0),
        PA,
    );
    {
        let mut c = pa.data();
        c.unit.hp = 1_000;
        c.unit.max_hp = 1_000;
        c.unit.mp = 1_000;
        c.unit.max_mp = 1_000;
    }
    client.conn.active_char = Some(pa.clone());
    let world = server.world.world(0).unwrap();
    server.systems.npcs.spawns.get_or_create(&world);
    let slave = spawn_real_slave(&server, &world, &pa, RED_FARM_HAULER, (100.0, 100.0, 100.0));
    slaves::bind_slave(
        &server,
        &pa,
        slave.obj_id,
        attach_point::DRIVER,
        slaves::attach_reason::NEW_MASTER,
        Some(4.0),
    );
    assert_eq!(pa.data().attached_point, attach_point::DRIVER);
    client.drain();
    assert_eq!(water_count(&pa), 0);

    start_skill(
        &server,
        &mut client.conn,
        water_cast(slave.obj_id, WATER_MOUNT_SKILL, WATER_MOUNT_ROW),
    );
    let frames = client.drain();
    assert!(
        skill_ids_of(&frames, sc::SCSkillStartedPacket).contains(&WATER_RIDER_SKILL),
        "{:x?}",
        ops(&frames)
    );
    assert_eq!(water_count(&pa), 0);
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_millis(5_100)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    client.drain();
    assert_eq!(water_count(&pa), WATER_COUNT, "10 x item 15694 (Water)");
}

// ---------------------------------------------------------------------------------------------
// Transfer boarding (`CSBoardingTransferPacket`) and the repairman's slave item repair
// (`CSRepairSlaveItemsPacket`)
// ---------------------------------------------------------------------------------------------

const TRANSFER_TL: u16 = 4242;

/// The synthetic carriage pair: the boarding part model 653 declares the two passenger seats
/// the real `model_bindings` row has; the motor model 654 only the system point 30.
fn transfer_server() -> Arc<GameServer> {
    let mut d = data();
    d.systems.vehicles.model_bindings.insert(653, vec![2, 3]);
    d.systems.vehicles.model_bindings.insert(654, vec![30]);
    server_with(d)
}

fn spawn_test_transfer(
    server: &GameServer,
    obj_id: u32,
    model_id: u32,
) -> crate::world::TransferRef {
    let world = server.world.world(0).unwrap();
    let mut t = crate::models::transfer::Transfer::new(model_id);
    t.unit.model_id = model_id;
    t.unit.tl_id = TRANSFER_TL;
    t.unit.level = 1;
    t.unit.max_hp = 1000;
    t.unit.hp = 1000;
    t.unit.transform.instance_id = 0;
    t.unit.transform.set_xyz(100.0, 101.0, 100.0);
    let t = crate::world::Entity::new(obj_id, t);
    world.spawn(&t, &server.view_ctx());
    t
}

fn spawn_live_player(
    server: &GameServer,
    client: &TestClient,
    id: u32,
    obj_id: u32,
    at: (f32, f32),
) -> PlayerRef {
    let p = spawn_player(
        server,
        client,
        test_character(id, &format!("P{id}"), at.0, at.1),
        obj_id,
    );
    {
        let mut d = p.data();
        d.unit.hp = 1000;
        d.unit.max_hp = 1000;
    }
    p
}

async fn board_transfer(
    server: &GameServer,
    conn: &mut crate::net::connection::GameConnection,
    tl: u16,
    ap: u8,
) {
    use crate::net::registry::ClientPacket;
    super::packets::CSBoardingTransferPacket { tl, ap }
        .handle(server, conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn boarding_a_transfer_seats_the_player_on_the_boarding_part() {
    let server = transfer_server();
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_live_player(&server, &a, 1, PA, (100.0, 105.0));
    a.conn.active_char = Some(pa.clone());
    // Same tl id on both parts, as `TransferManager.Create` sets them: only the boarding part
    // (653) declares the passenger seats.
    let part = spawn_test_transfer(&server, 0x500, 653);
    let motor = spawn_test_transfer(&server, 0x501, 654);
    a.drain();
    assert_eq!(
        server
            .world
            .world(0)
            .unwrap()
            .all_of_kind(crate::world::ObjectKind::Transfer)
            .len(),
        2
    );
    assert!(server
        .game_data
        .systems
        .vehicles
        .model_has_attach_point(653, 2));
    board_transfer(&server, &mut a.conn, TRANSFER_TL, 2).await;
    let frames = a.drain();
    let mut expect = PacketStream::new();
    expect
        .write_bc(PA)
        .write_u8(2)
        .write_bc(part.obj_id)
        .write_u8(slaves::attach_reason::BOARD_TRANSFER);
    assert_eq!(
        body(&frames, sc::SCUnitAttachedPacket),
        expect.into_bytes(),
        "{:x?}",
        ops(&frames)
    );
    {
        let p = pa.data();
        assert_eq!(p.attached_point, 2);
        assert_eq!(p.unit.transform.parent_obj_id, part.obj_id);
    }
    assert_eq!(part.data().attached_characters, vec![(2, PA)]);
    assert!(motor.data().attached_characters.is_empty());

    // A second player: the taken seat is refused, the free one works.
    let mut b = TestClient::new(&server, 2, 2);
    let pb = spawn_live_player(&server, &b, 2, PB, (101.0, 105.0));
    b.conn.active_char = Some(pb.clone());
    b.drain();
    board_transfer(&server, &mut b.conn, TRANSFER_TL, 2).await;
    let f = b.drain();
    assert_eq!(error_codes(&f), vec![97]); // SLAVE_ALREADY_HAS_MASTER clears the request
    assert!(!ops(&f).contains(&sc::SCUnitAttachedPacket));
    assert_eq!(pb.data().attached_point, 0);

    board_transfer(&server, &mut b.conn, TRANSFER_TL, 3).await;
    let f = b.drain();
    assert!(ops(&f).contains(&sc::SCUnitAttachedPacket));
    assert_eq!(pb.data().attached_point, 3);
    assert_eq!(part.data().attached_characters, vec![(2, PA), (3, PB)]);
}

#[tokio::test]
async fn boarding_a_transfer_refuses_an_undeclared_seat_and_an_out_of_range_request() {
    let server = transfer_server();
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_live_player(&server, &a, 1, PA, (100.0, 105.0));
    a.conn.active_char = Some(pa.clone());
    spawn_test_transfer(&server, 0x500, 653);
    a.drain();

    // 653 declares only 2 and 3; the client's scan starts at 2, so 1 is never offered either.
    for ap in [1, 4] {
        board_transfer(&server, &mut a.conn, TRANSFER_TL, ap).await;
        let f = a.drain();
        assert_eq!(error_codes(&f), vec![96], "seat {ap}"); // SLAVE_CANNOT_BIND
        assert!(!ops(&f).contains(&sc::SCUnitAttachedPacket));
    }
    assert_eq!(pa.data().attached_point, 0);

    // Past the client's 3 m edge range (`FUN_393bf310` -> `FUN_39093860(..., 9.0, 0)`,
    // where 9.0 is the squared range the function compares with `SQRT(param)`).
    pa.data().unit.transform.set_xyz(104.5, 101.0, 100.0);
    board_transfer(&server, &mut a.conn, TRANSFER_TL, 2).await;
    let f = a.drain();
    assert_eq!(error_codes(&f), vec![error_message::TOO_FAR_AWAY]);
    assert!(!ops(&f).contains(&sc::SCUnitAttachedPacket));
    assert_eq!(pa.data().attached_point, 0);

    // Inside 3 m of the seat itself the same seat is accepted.
    pa.data().unit.transform.set_xyz(100.0, 104.5, 100.0);
    board_transfer(&server, &mut a.conn, TRANSFER_TL, 2).await;
    let f = a.drain();
    assert!(ops(&f).contains(&sc::SCUnitAttachedPacket));
    assert_eq!(pa.data().attached_point, 2);
}

#[tokio::test]
async fn boarding_measures_to_the_seat_not_the_transfer_origin() {
    let server = transfer_server();
    let mut a = TestClient::new(&server, 1, 1);
    // 2.29 m from Passenger0 at (100.005, 106.785, 101.366) but 3.5 m from the part origin.
    let pa = spawn_live_player(&server, &a, 1, PA, (100.0, 104.5));
    a.conn.active_char = Some(pa.clone());
    let part = spawn_test_transfer(&server, 0x500, 653);
    a.drain();
    board_transfer(&server, &mut a.conn, TRANSFER_TL, 2).await;
    assert_eq!(pa.data().attached_point, 2);
    assert_eq!(part.data().attached_characters, vec![(2, PA)]);

    // 4.0 m from the seat: refused although the origin is only 7.8 m away (the old 9 m origin
    // limit would have accepted it).
    let mut b = TestClient::new(&server, 2, 2);
    let pb = spawn_live_player(&server, &b, 2, PB, (100.0, 108.8));
    b.conn.active_char = Some(pb.clone());
    b.drain();
    board_transfer(&server, &mut b.conn, TRANSFER_TL, 3).await;
    let f = b.drain();
    assert_eq!(error_codes(&f), vec![error_message::TOO_FAR_AWAY]);
    assert_eq!(pb.data().attached_point, 0);
}

#[tokio::test]
async fn a_same_world_teleport_ended_frees_the_seat() {
    // Every same-world SCTeleportUnit sender (the same-instance portal branch, the NPC
    // teleporter, dominion / instant-game / GM move-all / nudge / sphere teleports) ends in
    // `CSTeleportEndedPacket`; the sink must release the transfer seat.
    let (server, mut a, pa, part) = boarded_transfer().await;
    pa.data().set_disabled_set_position(true);
    use crate::net::registry::ClientPacket;
    crate::systems::npcs::portal::packets::CSTeleportEndedPacket {
        x: 110.0,
        y: 100.0,
        z: 100.0,
        ori: [0; 16],
    }
    .handle(&server, &mut a.conn)
    .await
    .unwrap();
    let f = a.drain();
    assert_unboarded(&pa, &part, &f);
}

#[tokio::test]
async fn unboarding_clears_a_stale_sticky_parent_without_detaching_another_parent() {
    let (server, mut a, pa, part) = boarded_transfer().await;
    let world = server.world.world(0).unwrap();
    let other = crate::world::Entity::new(0x702, crate::models::doodad::Doodad::new(1));
    world.spawn(&other, &server.view_ctx());
    let me = WorldObject::from(&pa);
    world.attach_child(&WorldObject::from(&other), &me);
    transfers::unboard(
        &server,
        &pa,
        TRANSFER_TL,
        slaves::attach_reason::BOARD_TRANSFER,
    );
    a.drain();
    assert!(part.data().attached_characters.is_empty());
    assert_eq!(me.transform().parent_obj_id, other.obj_id);
    assert!(world.sticky_parent(&me).is_none());
}

#[tokio::test]
async fn a_boarded_passenger_cannot_take_a_doodad_bench() {
    let (server, _a, pa, _part) = boarded_transfer().await;
    let world = server.world.world(0).unwrap();
    let doodad = crate::world::Entity::new(0x701, crate::models::doodad::Doodad::new(5890));
    world.spawn(&doodad, &server.view_ctx());
    let ctx = crate::systems::npcs::doodad::runtime::DoodadCtx::new(
        &server,
        world.clone(),
        doodad.clone(),
    );
    let f = crate::systems::npcs::doodad::templates::DoodadFuncAttachment {
        id: 1,
        attach_point_id: 2,
        space: 1,
        bond_kind_id: 2,
    };
    crate::systems::npcs::doodad::funcs::world::attachment(&f, &ctx, Some(&WorldObject::from(&pa)));
    assert!(pa.data().bonding.is_none());
}

#[tokio::test]
async fn unboarding_a_transfer_detaches_the_passenger_and_is_idempotent() {
    let server = transfer_server();
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_live_player(&server, &a, 1, PA, (100.0, 105.0));
    a.conn.active_char = Some(pa.clone());
    let part = spawn_test_transfer(&server, 0x500, 653);
    a.drain();
    board_transfer(&server, &mut a.conn, TRANSFER_TL, 2).await;
    a.drain();

    board_transfer(&server, &mut a.conn, TRANSFER_TL, 0).await;
    let frames = a.drain();
    let mut expect = PacketStream::new();
    expect
        .write_bc(PA)
        .write_u8(slaves::attach_reason::BOARD_TRANSFER);
    assert_eq!(
        body(&frames, sc::SCUnitDetachedPacket),
        expect.into_bytes(),
        "{:x?}",
        ops(&frames)
    );
    {
        let p = pa.data();
        assert_eq!(p.attached_point, 0);
        assert_eq!(p.unit.transform.parent_obj_id, 0);
    }
    assert!(part.data().attached_characters.is_empty());

    // A repeated request (the client's `FUN_39362170` sender is periodic) sends nothing.
    board_transfer(&server, &mut a.conn, TRANSFER_TL, 0).await;
    assert!(a.drain().is_empty());
}

#[tokio::test]
async fn a_late_viewer_receives_the_transfer_passenger_seat() {
    let server = transfer_server();
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_live_player(&server, &a, 1, PA, (100.0, 105.0));
    a.conn.active_char = Some(pa.clone());
    spawn_test_transfer(&server, 0x500, 653);
    a.drain();
    board_transfer(&server, &mut a.conn, TRANSFER_TL, 2).await;
    a.drain();

    let mut b = TestClient::new(&server, 2, 2);
    spawn_live_player(&server, &b, 2, PB, (101.0, 105.0));
    let f = b.drain();
    let mut expect = PacketStream::new();
    expect.write_bc(PA).write_u8(2).write_bc(0x500).write_u8(0);
    assert_eq!(
        body(&f, sc::SCUnitAttachedPacket),
        expect.into_bytes(),
        "{:x?}",
        ops(&f)
    );
}

const REPAIRMAN_NPC: u32 = 904;
const REPAIRMAN_OBJ: u32 = 0x301;
const REPAIR_SLAVES_TASK: u8 = 74;

struct RepairmanHook;

impl crate::systems::economy::hooks::NpcEconomyHooks for RepairmanHook {
    fn shop_info(
        &self,
        _: &GameServer,
        npc: &NpcRef,
    ) -> Option<crate::systems::economy::hooks::NpcShopInfo> {
        Some(crate::systems::economy::hooks::NpcShopInfo {
            repairman: npc.data().unit.template_id == REPAIRMAN_NPC,
            ..Default::default()
        })
    }
}

/// A destroyed slave summon item (`slave_is_destroyed`, from `MarkSummoningItemAsDestroyed`)
/// and a repairman 2 m away that is not yet the interaction object.
struct RepairSlave {
    server: Arc<GameServer>,
    client: TestClient,
    player: PlayerRef,
    item_id: u64,
    npc: NpcRef,
}

fn repair_slave(cost: i32) -> RepairSlave {
    let mut d = data();
    d.systems.npcs.npc.templates.insert(
        REPAIRMAN_NPC,
        NpcTemplate {
            id: REPAIRMAN_NPC,
            name: "Repairman".into(),
            level: 1,
            repairman: true,
            ..NpcTemplate::default()
        },
    );
    d.systems
        .vehicles
        .slave_templates
        .get_mut(&SLAVE_TEMPLATE)
        .unwrap()
        .cost = cost;
    let server = server_with(d);
    server
        .systems
        .economy
        .hooks
        .npcs
        .install(Arc::new(RepairmanHook));
    let mut client = TestClient::new(&server, 1, 1);
    let (player, item_id) = player_with_item(&server, &client, 1, PA, (100.0, 100.0), SLAVE_ITEM);
    {
        let mut c = player.data();
        c.money = 12_000;
        c.unit.hp = 1000;
        c.unit.max_hp = 1000;
    }
    client.conn.active_char = Some(player.clone());
    mates::update_owned_item(&server, Some(&player), item_id, |i| {
        i.details.slave_is_destroyed = 1;
        i.details.slave_repair_start_time = chrono::Utc::now().timestamp();
        i.dirty = true;
    });
    let npc = spawn_npc_at(&server, REPAIRMAN_OBJ, REPAIRMAN_NPC, 102.0, 100.0);
    client.drain();
    RepairSlave {
        server,
        client,
        player,
        item_id,
        npc,
    }
}

impl RepairSlave {
    fn item(&self) -> Item {
        mates::owned_item(&self.server, &self.player, self.item_id).unwrap()
    }

    async fn repair_at(&mut self, npc_obj_id: u32) -> Vec<(u16, Vec<u8>)> {
        use crate::net::registry::ClientPacket;
        crate::systems::economy::mate_equipment::CSRepairSlaveItemsPacket { npc_id: npc_obj_id }
            .handle(&self.server, &mut self.client.conn)
            .await
            .unwrap();
        self.client.drain()
    }
}

#[tokio::test]
async fn a_repairman_repairs_destroyed_slave_items_for_the_slave_cost() {
    let mut t = repair_slave(50);
    t.player.data().unit.current_interaction_object = t.npc.obj_id;
    let frames = t.repair_at(REPAIRMAN_OBJ).await;
    // Money task (-50), then the ItemUpdate as RepairSlaves (74), then SlaveRepaired (382).
    assert_eq!(t.player.data().money, 12_000 - 50);
    let item = t.item();
    assert_eq!(item.details.slave_is_destroyed, 0);
    assert_eq!(item.details.slave_repair_start_time, 0);
    assert!(item.dirty);
    let mut expect = PacketStream::new();
    aaemu_commons::Marshal::write(
        &crate::systems::economy::item_packets::SCItemTaskSuccessPacket {
            action: REPAIR_SLAVES_TASK,
            tasks: vec![crate::models::item_task::ItemTask::update(&item)],
            force_remove: Vec::new(),
        },
        &mut expect,
    );
    assert_eq!(
        ops(&frames),
        vec![
            sc::SCItemTaskSuccessPacket,
            sc::SCItemTaskSuccessPacket,
            sc::SCErrorMsgPacket
        ]
    );
    assert_eq!(frames[1].1, expect.as_bytes());
    assert_eq!(error_codes(&frames), vec![error_message::SLAVE_REPAIRED]);

    // The vehicle can be summoned again (the destroyed flag is gone).
    let world = t.server.world.world(0).unwrap();
    t.server.systems.npcs.spawns.get_or_create(&world);
    let item = t.item();
    assert!(slaves::create_core(
        &t.server,
        &world,
        Some(&t.player),
        None,
        SLAVE_TEMPLATE,
        Some(item),
        false,
        None,
        None,
    )
    .is_some());
}

#[tokio::test]
async fn slave_repair_is_refused_without_gold_and_leaves_the_item_destroyed() {
    let mut t = repair_slave(50);
    t.player.data().money = 10;
    t.player.data().unit.current_interaction_object = t.npc.obj_id;
    let frames = t.repair_at(REPAIRMAN_OBJ).await;
    assert_eq!(t.player.data().money, 10);
    let item = t.item();
    assert_eq!(item.details.slave_is_destroyed, 1);
    assert!(!task_actions(&frames).contains(&REPAIR_SLAVES_TASK));
    assert_eq!(error_codes(&frames), vec![error_message::NOT_ENOUGH_MONEY]);
}

#[tokio::test]
async fn slave_repair_is_bag_only_and_ignores_the_bank() {
    let mut t = repair_slave(50);
    // A second destroyed summon item in the bank: the client's preview keeps only slots whose
    // container is the bag, so it must stay destroyed and not be charged.
    let bank = t
        .player
        .data()
        .inventory
        .get(slot_type::BANK)
        .cloned()
        .expect("bank");
    let template = t
        .server
        .game_data
        .items
        .template(SLAVE_ITEM)
        .unwrap()
        .clone();
    let mut bank_item = Item::create(0x9100, &template, 1, 0, 1, &t.server.game_data.items, 0);
    bank_item.owner_id = u64::from(t.player.data().id);
    bank_item.slot_type = slot_type::BANK;
    bank_item.slot = 0;
    bank_item.details.slave_is_destroyed = 1;
    bank.lock().unwrap().items.push(bank_item);
    t.player.data().unit.current_interaction_object = t.npc.obj_id;

    let frames = t.repair_at(REPAIRMAN_OBJ).await;
    assert_eq!(t.player.data().money, 12_000 - 50);
    assert_eq!(t.item().details.slave_is_destroyed, 0);
    let bank = bank.lock().unwrap();
    assert_eq!(
        bank.get_item_by_id(0x9100)
            .unwrap()
            .details
            .slave_is_destroyed,
        1
    );
    let tasks: Vec<u8> = task_actions(&frames);
    assert!(tasks.contains(&REPAIR_SLAVES_TASK));
}

#[tokio::test]
async fn a_spawned_slave_cannot_be_repaired_and_nothing_is_charged() {
    let mut t = repair_slave(50);
    t.player.data().unit.current_interaction_object = t.npc.obj_id;
    mates::update_owned_item(&t.server, Some(&t.player), t.item_id, |i| {
        i.details.slave_db_id = 4321;
    });
    let world = t.server.world.world(0).unwrap();
    let mut slave = crate::models::slave::Slave::new(4321, SLAVE_TEMPLATE);
    slave.unit.tl_id = 7;
    slave.summoner_obj_id = PA;
    let slave = crate::world::Entity::new(0x600, slave);
    world.spawn(&slave, &t.server.view_ctx());
    t.client.drain();

    let frames = t.repair_at(REPAIRMAN_OBJ).await;
    assert_eq!(t.player.data().money, 12_000);
    assert_eq!(t.item().details.slave_is_destroyed, 1);
    assert!(!task_actions(&frames).contains(&REPAIR_SLAVES_TASK));
    assert_eq!(
        error_codes(&frames),
        vec![error_message::SLAVE_CANNOT_REPAIR_ALREADY_SPAWNED]
    );
}

#[tokio::test]
async fn a_repairman_service_requires_the_interaction_the_role_and_the_range() {
    // No interaction object: the 1.2 client only sends the packet at a repairman it talks to.
    let mut t = repair_slave(50);
    let before = t.player.data().money;
    assert!(t.repair_at(REPAIRMAN_OBJ).await.is_empty());
    assert_eq!(t.player.data().money, before);
    assert_eq!(t.item().details.slave_is_destroyed, 1);

    // A plain NPC with the same talk is not a repairman.
    let mut t = repair_slave(50);
    let plain = spawn_npc_at(&t.server, 0x302, MATE_NPC, 102.0, 100.0);
    t.client.drain();
    t.player.data().unit.current_interaction_object = plain.obj_id;
    assert!(t.repair_at(plain.obj_id).await.is_empty());
    assert_eq!(t.player.data().money, 12_000);
    assert_eq!(t.item().details.slave_is_destroyed, 1);

    // A repairman further than the 5 m the other repair services use.
    let mut t = repair_slave(50);
    t.player.data().unit.current_interaction_object = t.npc.obj_id;
    t.player.data().unit.transform.set_xyz(120.0, 100.0, 100.0);
    let frames = t.repair_at(REPAIRMAN_OBJ).await;
    assert_eq!(error_codes(&frames), vec![error_message::TOO_FAR_AWAY]);
    assert_eq!(t.player.data().money, 12_000);
    assert_eq!(t.item().details.slave_is_destroyed, 1);
}

// ---------------------------------------------------------------------------------------------
// Transfer seat ownership against the other attachment paths (review follow-up)
// ---------------------------------------------------------------------------------------------

async fn boarded_transfer() -> (
    Arc<GameServer>,
    TestClient,
    PlayerRef,
    crate::world::TransferRef,
) {
    let server = transfer_server();
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_live_player(&server, &a, 1, PA, (100.0, 105.0));
    a.conn.active_char = Some(pa.clone());
    let part = spawn_test_transfer(&server, 0x500, 653);
    a.drain();
    board_transfer(&server, &mut a.conn, TRANSFER_TL, 2).await;
    a.drain();
    assert_eq!(part.data().attached_characters, vec![(2, PA)]);
    (server, a, pa, part)
}

fn assert_unboarded(pa: &PlayerRef, part: &crate::world::TransferRef, frames: &[(u16, Vec<u8>)]) {
    assert!(part.data().attached_characters.is_empty());
    let c = pa.data();
    assert_eq!(c.attached_point, 0);
    assert_eq!(c.unit.transform.parent_obj_id, 0);
    assert!(
        frames.iter().any(|f| f.0 == sc::SCUnitDetachedPacket),
        "{:x?}",
        ops(frames)
    );
}

#[tokio::test]
async fn a_server_teleport_frees_the_transfer_seat() {
    let (server, mut a, pa, part) = boarded_transfer().await;
    let mut destination = pa.data().unit.transform;
    destination.set_xyz(110.0, 100.0, 100.0);
    assert!(crate::world::session::teleport_character(
        &server,
        &pa,
        destination
    ));
    let f = a.drain();
    assert_unboarded(&pa, &part, &f);
}

#[tokio::test]
async fn a_gm_move_frees_the_transfer_seat() {
    let (server, mut a, pa, part) = boarded_transfer().await;
    let commands = &server.systems.services.commands;
    crate::systems::services::commands::scripts::install(
        commands,
        crate::systems::services::commands::scripts::ScriptEnv::default(),
    );
    assert!(commands.reload());
    let (handled, _) =
        crate::systems::services::commands::scripts::harness::run(&server, &pa, "move 120 100 100")
            .await;
    assert!(handled);
    let f = a.drain();
    assert_unboarded(&pa, &part, &f);
}

#[tokio::test]
async fn logout_and_disconnect_free_the_transfer_seat() {
    let (server, mut a, pa, part) = boarded_transfer().await;
    super::on_leave_world(&server, &pa);
    let f = a.drain();
    assert_unboarded(&pa, &part, &f);

    let (server, mut a, pa, part) = boarded_transfer().await;
    super::on_disconnect(&server, &pa);
    let f = a.drain();
    assert_unboarded(&pa, &part, &f);
}

#[tokio::test]
async fn death_frees_the_transfer_seat() {
    let (server, mut a, pa, part) = boarded_transfer().await;
    super::on_death_event(&server, &WorldObject::from(&pa));
    let f = a.drain();
    assert_unboarded(&pa, &part, &f);
}

#[tokio::test]
async fn a_transfer_despawn_frees_the_passenger() {
    let (server, mut a, pa, part) = boarded_transfer().await;
    transfers::unboard_passengers(&server, &part);
    let f = a.drain();
    assert_unboarded(&pa, &part, &f);
    assert!(!transfers::is_boarded(&server, &pa));
}

#[tokio::test]
async fn a_boarded_passenger_cannot_mount_a_mate_or_bind_a_slave() {
    let (server, mut a, pa, part) = boarded_transfer().await;
    let mate_item = give_item(&server, &pa, 0x9001, MATE_ITEM);
    mates::spawn_mount(&server, &pa, mate_item);
    let mate = mates::first_active_mate(&server, &pa).expect("active mate");
    a.drain();
    let tl = u32::from(mate.data().unit.tl_id);
    mates::mount_mate(&server, &pa, tl, attach_point::DRIVER, 1);
    let f = a.drain();
    assert_eq!(error_codes(&f), vec![error_message::SLAVE_UNBIND_FIRST]);
    assert!(!pa.data().is_riding);
    assert!(mate
        .data()
        .passenger(attach_point::DRIVER)
        .is_none_or(|p| p.obj_id == 0));
    assert_eq!(part.data().attached_characters, vec![(2, PA)]);

    let world = server.world.world(0).unwrap();
    let mut model = crate::models::slave::Slave::new(0, SLAVE_TEMPLATE);
    model.unit.model_id = 300;
    model.unit.hp = 100;
    model.unit.max_hp = 100;
    let slave = crate::world::Entity::new(0x610, model);
    world.spawn(&slave, &server.view_ctx());
    a.drain();
    slaves::bind_slave(
        &server,
        &pa,
        slave.obj_id,
        attach_point::PASSENGER0,
        7,
        Some(100.0),
    );
    let f = a.drain();
    assert_eq!(error_codes(&f), vec![error_message::SLAVE_UNBIND_FIRST]);
    assert!(slave.data().attached_characters.is_empty());
    assert_eq!(pa.data().attached_point, 2);
    assert_eq!(part.data().attached_characters, vec![(2, PA)]);
}

#[tokio::test]
async fn a_bonded_bench_seat_refuses_transfer_boarding() {
    let server = transfer_server();
    let mut a = TestClient::new(&server, 1, 1);
    let pa = spawn_live_player(&server, &a, 1, PA, (100.0, 105.0));
    a.conn.active_char = Some(pa.clone());
    let part = spawn_test_transfer(&server, 0x500, 653);
    a.drain();
    pa.data().bonding = Some(crate::models::doodad::BondDoodad {
        owner_obj_id: 0x700,
        ..Default::default()
    });
    board_transfer(&server, &mut a.conn, TRANSFER_TL, 2).await;
    let f = a.drain();
    assert_eq!(error_codes(&f), vec![error_message::SLAVE_UNBIND_FIRST]);
    assert!(!ops(&f).contains(&sc::SCUnitAttachedPacket));
    assert!(part.data().attached_characters.is_empty());
    assert_eq!(pa.data().attached_point, 0);
}

#[tokio::test]
async fn unboarding_keeps_a_different_attachment_point() {
    let (server, mut a, pa, part) = boarded_transfer().await;
    // Another path moved the character to point 5 after the transfer reservation: the
    // unboard must not reset that state.
    pa.data().attached_point = 5;
    transfers::unboard(
        &server,
        &pa,
        TRANSFER_TL,
        slaves::attach_reason::BOARD_TRANSFER,
    );
    let f = a.drain();
    assert!(part.data().attached_characters.is_empty());
    assert_eq!(pa.data().attached_point, 5);
    assert!(f.iter().any(|x| x.0 == sc::SCUnitDetachedPacket));
}

/// `ApplySlaveBonuses` stores the flat `unit_modifiers.value`; the C# `LinearLevelBonus` term is
/// unused. All 452 Slave rows of the shipped data have `linear_level_bonus = 0`, so ignoring it
/// cannot change a bonus today. This pins that data assumption: a future row with a non-zero
/// term must be noticed (and the stat code revisited).
#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_slave_unit_modifiers_have_no_linear_level_bonus() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM unit_modifiers WHERE owner_type = 'Slave'",
            [],
            |r| r.get(0),
        )
        .expect("Slave rows");
    assert_eq!(total, 452);
    let with_bonus: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM unit_modifiers WHERE owner_type = 'Slave' \
             AND linear_level_bonus != 0",
            [],
            |r| r.get(0),
        )
        .expect("Slave linear level bonus rows");
    assert_eq!(with_bonus, 0);
}

#[tokio::test]
async fn hang_and_mount_need_the_target_in_reach() {
    use super::packets::CSHangPacket;
    use crate::net::registry::ClientPacket;

    let server = server_with(data());
    let mut a = TestClient::new(&server, 1, 1);
    let (pa, item_id) = player_with_item(&server, &a, 1, PA, (100.0, 100.0), MATE_ITEM);
    a.conn.active_char = Some(pa.clone());
    mates::spawn_mount(&server, &pa, item_id);
    let mate = server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(1)
        .pop()
        .unwrap();
    let tl = u32::from(mate.data().unit.tl_id);
    let world = server.world.world(0).unwrap();
    let mut far = crate::models::npc::Npc::new(MATE_NPC);
    far.unit.transform.set_xyz(900.0, 900.0, 100.0);
    let anchor = crate::world::Entity::new(0x201, far);
    world.spawn(&anchor, &server.view_ctx());
    a.drain();
    // A hang target across the map is ignored.
    CSHangPacket {
        unit_obj_id: PA,
        target_obj_id: anchor.obj_id,
    }
    .handle(&server, &mut a.conn)
    .await
    .unwrap();
    assert_eq!(pa.core().sticky_parent(), 0);
    // A mate out of reach cannot be mounted; in reach it can, and only one seat is taken.
    let near = mate.data().unit.transform;
    mate.data().unit.transform.set_xyz(500.0, 500.0, near.z);
    mates::mount_mate(&server, &pa, tl, attach_point::DRIVER, 1);
    assert!(!pa.data().is_riding);
    mate.data().unit.transform.set_xyz(near.x, near.y, near.z);
    mates::mount_mate(&server, &pa, tl, attach_point::DRIVER, 1);
    assert!(pa.data().is_riding);
    mates::mount_mate(&server, &pa, tl, attach_point::PASSENGER0, 1);
    assert_eq!(
        mate.data()
            .passenger(attach_point::PASSENGER0)
            .map(|s| s.obj_id),
        Some(0),
        "an already seated rider cannot take a second seat"
    );
}
