//! Flow tests of the instances subsystem without MySQL: a party dungeon (creation, loading,
//! entering, events / actions, room clear, leaving, party kick, cleanup), a system instance with
//! a disconnect, battlefield matchmaking / scoring, and (gated by `AAEMU_COMPACT_DB`) a dungeon
//! on the real 1.2 data with the real `Data/Worlds/instance_burntcastle_armory` spawn files.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aaemu_commons::PacketStream;
use sqlx::mysql::MySqlPoolOptions;

use crate::config::{AppConfig, ServerProfile};
use crate::data::skills::{BuffKind, BuffTemplate};
use crate::data::world_data::{Zone, ZoneData, ZoneGroup};
use crate::data::world_templates::{
    WorldSpawnLocation, WorldSpawnPosition, WorldTemplate, WorldTemplates,
};
use crate::data::GameData;
use crate::models::doodad::Doodad;
use crate::models::npc::Npc;
use crate::net::packet::{encode, ServerPacket};
use crate::opcodes::{cs, sc};
use crate::systems::npcs::doodad::runtime::DoodadCtx;
use crate::systems::npcs::doodad::templates::{
    DoodadFunc, DoodadFuncEnterInstance, DoodadFuncEnterSysInstance, DoodadFuncExitIndun,
    DoodadFuncGroups, DoodadTemplate, FuncTemplate,
};
use crate::systems::npcs::portal::packets::SCLoadInstancePacket;
use crate::test_support::{test_character, TestClient, TEST_ZONE};
use crate::world::unit_state::UNIT_STATE_FORCED_BUFFS;
use crate::world::{Entity, Player, PlayerRef, WorldObject};
use crate::GameServer;

use super::data::*;
use super::events::CharEvent;
use super::instant_game::{self, corps};
use super::packets::*;
use super::{dungeon, events, indun, worlds};

const DUNGEON_ZONE: u32 = 236;
const DUNGEON_GROUP: u32 = 45;
const MIRAGE_ZONE: u32 = 260;
const MIRAGE_GROUP: u32 = 49;
const CAMP_ZONE: u32 = 198;
const DUNGEON_SPAWN: WorldSpawnPosition = WorldSpawnPosition {
    world_id: 1,
    zone_id: DUNGEON_ZONE,
    x: 200.0,
    y: 200.0,
    z: 20.0,
    yaw: 90.0,
    pitch: 0.0,
    roll: 0.0,
};

fn body(p: &dyn ServerPacket) -> Vec<u8> {
    encode(p)[6..].to_vec()
}

/// The `qualified_id` of the last `SCInviteToInstantGame` in `frames` (body layout:
/// `zone_instance_id`, `ruleset_id`, `corps`, `qualified_id`).
fn invite_qualified_id(frames: &[(u16, Vec<u8>)]) -> u64 {
    let (_, b) = frames
        .iter()
        .rev()
        .find(|(op, _)| *op == sc::SCInviteToInstantGamePacket)
        .expect("an instant game invitation");
    u64::from_le_bytes(b[13..21].try_into().expect("qualified id"))
}

fn zone(id: u32, key: u32, group: u32) -> Zone {
    Zone {
        id,
        name: format!("zone_{key}"),
        zone_key: key,
        group_id: group,
        ..Zone::default()
    }
}

/// `template` with its single XML zone positioned by a `world_spawns.json` entry.
fn instance_template(
    id: u32,
    name: &str,
    zone_key: u32,
    spawn: WorldSpawnPosition,
) -> WorldTemplate {
    let mut t = WorldTemplate::synthetic(id, name, 2, 2, zone_key);
    t.xml_world.zones.get_mut(&zone_key).unwrap().spawn_position = spawn;
    t
}

fn lookup(name: &str, spawn: WorldSpawnPosition) -> WorldSpawnLocation {
    WorldSpawnLocation {
        name: Some(name.into()),
        spawn_position: Some(spawn),
    }
}

/// A server like `test_support::test_server_with`, with extra world templates / spawn lookups
/// and a zero party-kick delay.
fn build_server(
    data: GameData,
    templates: Vec<WorldTemplate>,
    lookups: Vec<WorldSpawnLocation>,
    app_path: Option<&Path>,
) -> Arc<GameServer> {
    build_server_with(data, templates, lookups, app_path, |_| {})
}

fn build_server_with(
    mut data: GameData,
    mut templates: Vec<WorldTemplate>,
    lookups: Vec<WorldSpawnLocation>,
    app_path: Option<&Path>,
    adjust: impl FnOnce(&mut AppConfig),
) -> Arc<GameServer> {
    let mut config = AppConfig {
        id: 1,
        ..AppConfig::default()
    };
    config.connections.mysql_provider.host = "127.0.0.1".into();
    config.connections.mysql_provider.port = 1;
    config.dungeons.auto_team_disband_kick_time = 0;
    if let Some(p) = app_path {
        config.app_path = p.to_path_buf();
    }
    adjust(&mut config);
    let db = MySqlPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy_with(config.connections.mysql_provider.connect_options());
    templates.insert(
        0,
        WorldTemplate::synthetic(0, "main_world", 2, 2, TEST_ZONE),
    );
    data.worlds = WorldTemplates::from_templates(templates, lookups).expect("worlds");
    for id in UNIT_STATE_FORCED_BUFFS {
        let mut t = BuffTemplate::simple(id, BuffKind::Good);
        t.duration = 0;
        data.skills.buffs.insert(id, t);
    }
    let server = Arc::new(GameServer::new(config, db, data));
    server.set_self();
    server
}

fn zones(list: &[Zone]) -> ZoneData {
    let mut z = ZoneData::default();
    for x in list {
        z.zone_id_to_key.insert(x.id, x.zone_key);
        z.zones.insert(x.zone_key, x.clone());
        z.groups.entry(x.group_id).or_insert(ZoneGroup {
            id: x.group_id,
            name: format!("group_{}", x.group_id),
            ..ZoneGroup::default()
        });
    }
    z
}

/// A player in the world with a test connection.
struct Pc {
    c: TestClient,
    p: PlayerRef,
}

impl Pc {
    fn new(server: &GameServer, id: u32, name: &str, x: f32, y: f32) -> Self {
        let mut c = TestClient::new(server, id, 1000 + id);
        let mut ch = test_character(id, name, x, y);
        ch.is_online = true;
        ch.unit.hp = 100;
        ch.unit.max_hp = 100;
        ch.obj_id = server.ids.object.get_next_id().unwrap();
        let p = Player::new(ch, c.conn.handle());
        server.world.try_add_character(&p);
        let world = server.world.world(0).expect("main_world");
        world.spawn(&p, &server.view_ctx());
        c.conn.active_char = Some(p.clone());
        Self { c, p }
    }

    fn frames(&mut self) -> Vec<(u16, Vec<u8>)> {
        self.c.drain()
    }

    fn ops(&mut self) -> Vec<u16> {
        self.c.drain_ops()
    }

    async fn frame(&mut self, server: &GameServer, opcode: u16, body: &[u8]) {
        let mut v = vec![0xDD, 0x01, 0, 0];
        v.extend_from_slice(&opcode.to_le_bytes());
        v.extend_from_slice(body);
        server.handle_frame(&mut self.c.conn, &v).await.unwrap();
    }

    /// `CSMoveUnitPacket` (a `UnitMoveType` to x, y, z).
    async fn move_to(&mut self, server: &GameServer, x: f32, y: f32, z: f32) {
        let mut s = PacketStream::new();
        s.write_bc(self.p.obj_id)
            .write_u8(1)
            .write_u32(1234)
            .write_u8(0x02)
            .write_position(x, y, z)
            .write_i16(0)
            .write_i16(0)
            .write_i16(0)
            .write_i8(0)
            .write_i8(0)
            .write_i8(0)
            .write_i8(0)
            .write_i8(0)
            .write_i8(0)
            .write_u8(1)
            .write_u8(0)
            .write_u8(0);
        self.frame(server, cs::CSMoveUnitPacket, s.as_bytes()).await;
    }

    fn instance(&self) -> u32 {
        self.p.data().unit.transform.instance_id
    }
}

fn removed_ids(frames: &[(u16, Vec<u8>)]) -> Vec<u32> {
    let mut out = Vec::new();
    for (op, b) in frames {
        if *op != sc::SCUnitsRemovedPacket {
            continue;
        }
        let n = u16::from_le_bytes([b[0], b[1]]) as usize;
        for i in 0..n {
            let o = 2 + i * 3;
            out.push(u32::from(b[o]) | u32::from(b[o + 1]) << 8 | u32::from(b[o + 2]) << 16);
        }
    }
    out
}

fn bc(b: &[u8]) -> u32 {
    u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16
}

fn load_instance(
    instance: u32,
    zone: u32,
    x: f32,
    y: f32,
    z: f32,
    rot: (f32, f32, f32),
) -> Vec<u8> {
    body(&SCLoadInstancePacket {
        instance_id: instance,
        zone_id: zone,
        x,
        y,
        z,
        ang_x: rot.0,
        ang_y: rot.1,
        ang_z: rot.2,
    })
}

fn dungeon_data() -> GameData {
    let mut data = GameData::empty();
    data.zones = zones(&[
        zone(TEST_ZONE, TEST_ZONE, 1),
        zone(DUNGEON_ZONE, DUNGEON_ZONE, DUNGEON_GROUP),
        zone(MIRAGE_ZONE, MIRAGE_ZONE, MIRAGE_GROUP),
    ]);
    let inst = &mut data.systems.instances;
    inst.zone_order = ZoneOrder {
        zones: vec![
            (TEST_ZONE, 1),
            (DUNGEON_ZONE, DUNGEON_GROUP),
            (MIRAGE_ZONE, MIRAGE_GROUP),
        ],
    };
    inst.indun.zones.insert(
        DUNGEON_GROUP,
        IndunZone {
            zone_group_id: DUNGEON_GROUP,
            name: "armory".into(),
            level_min: 1,
            level_max: 55,
            max_players: 5,
            enter_count: 3,
            ..IndunZone::default()
        },
    );
    inst.indun.zones.insert(
        MIRAGE_GROUP,
        IndunZone {
            zone_group_id: MIRAGE_GROUP,
            name: "mirage".into(),
            max_players: 190,
            pvp: false,
            ..IndunZone::default()
        },
    );
    // Scripting: NPC 900 killed -> room 5 cleared -> (spawner effect); room 6 (doodad 777,
    // radius 20) emptied -> NPCs tagged 3 removed.
    let actions = [
        (
            10,
            IndunActionKind::SetRoomCleareds { indun_room_id: 5 },
            11,
        ),
        (11, IndunActionKind::NpcSpawner, 0),
        (12, IndunActionKind::RemoveTaggedNpcs { tag_id: 3 }, 0),
    ];
    for (id, kind, next) in actions {
        inst.indun.actions.insert(
            id,
            IndunAction {
                id,
                detail_id: 0,
                zone_group_id: DUNGEON_GROUP,
                next_action_id: next,
                kind,
            },
        );
    }
    inst.indun.events.insert(
        DUNGEON_GROUP,
        vec![
            IndunEvent {
                id: 1,
                condition_id: 1,
                zone_group_id: DUNGEON_GROUP,
                start_action_id: 12,
                kind: IndunEventKind::NoAliveChInRooms { room_id: 6 },
            },
            IndunEvent {
                id: 2,
                condition_id: 2,
                zone_group_id: DUNGEON_GROUP,
                start_action_id: 10,
                kind: IndunEventKind::NpcKilleds { npc_id: 900 },
            },
        ],
    );
    inst.indun.rooms.insert(
        6,
        IndunRoom {
            id: 6,
            doodad_id: 777,
            radius: 20,
            zone_group_id: DUNGEON_GROUP,
        },
    );
    data.systems.skills.npc_tags.insert(3, vec![901]);
    data
}

fn dungeon_server() -> Arc<GameServer> {
    dungeon_server_with(dungeon_data())
}

fn dungeon_server_with(data: GameData) -> Arc<GameServer> {
    dungeon_server_with_config(data, |_| {})
}

fn dungeon_server_with_config(
    data: GameData,
    adjust: impl FnOnce(&mut AppConfig),
) -> Arc<GameServer> {
    let mirage_spawn = WorldSpawnPosition {
        world_id: 2,
        zone_id: MIRAGE_ZONE,
        x: 300.0,
        y: 300.0,
        z: 50.0,
        yaw: 200.0,
        ..WorldSpawnPosition::default()
    };
    build_server_with(
        data,
        vec![
            instance_template(1, "instance_test", DUNGEON_ZONE, DUNGEON_SPAWN),
            instance_template(2, "arche_mall_world", MIRAGE_ZONE, mirage_spawn),
        ],
        vec![
            lookup("instance_test", DUNGEON_SPAWN),
            lookup("arche_mall_world", mirage_spawn),
        ],
        None,
        adjust,
    )
}

async fn await_loader(d: &Arc<dungeon::Dungeon>) {
    let h = d.st().loader.take().expect("loader scheduled");
    h.await.expect("loader");
}

fn form_party(server: &GameServer, a: &Pc, b: &Pc) {
    let teams = &server.systems.social.teams;
    let name = b.p.data().unit.name.clone();
    teams.ask_to_join(server, &a.p, &name, 0, true, Some(&b.p));
    teams.reply_to_join_team(server, &b.p, 0, true, a.p.character_id, false, "", false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_party_entries_share_one_run_and_loading_reserves_capacity() {
    let mut data = dungeon_data();
    data.systems
        .instances
        .indun
        .zones
        .get_mut(&DUNGEON_GROUP)
        .unwrap()
        .max_players = 2;
    let server = dungeon_server_with(data);
    let a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    let b = Pc::new(&server, 2, "Beta", 100.0, 100.0);
    let c = Pc::new(&server, 3, "Gamma", 100.0, 100.0);
    form_party(&server, &a, &b);
    let sa = server.clone();
    let pa = a.p.clone();
    let sb = server.clone();
    let pb = b.p.clone();
    let first = tokio::task::spawn_blocking(move || {
        indun::request_dungeon_instance(&sa, &pa, DUNGEON_ZONE, 0)
    });
    let second = tokio::task::spawn_blocking(move || {
        indun::request_dungeon_instance(&sb, &pb, DUNGEON_ZONE, 0)
    });
    assert!(first.await.unwrap() && second.await.unwrap());
    assert_eq!(server.systems.instances.all_dungeons().len(), 1);
    let d = server.systems.instances.all_dungeons()[0].clone();
    assert!(d.is_full(&server));
    form_party(&server, &a, &c);
    assert!(!dungeon::queue_player(&server, &d, &c.p));
    assert_eq!(
        server.systems.instances.indun.entry_count(3, DUNGEON_GROUP),
        0
    );
    await_loader(&d).await;
    assert_eq!(a.instance(), b.instance());
    assert_eq!(worlds::character_count(&d.world(&server).unwrap()), 2);
    dungeon::destroy_dungeon(&server, &d);
    assert_eq!((a.instance(), b.instance()), (0, 0));
}

#[tokio::test]
async fn party_kick_during_loading_cancels_entry_and_solo_run_can_become_party_owned() {
    let server = dungeon_server();
    let a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    let b = Pc::new(&server, 2, "Beta", 100.0, 100.0);
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let d = server.systems.instances.all_dungeons()[0].clone();
    assert!(!d.is_team_owned());
    form_party(&server, &a, &b);
    use_enter_func(&server, &b.p, DUNGEON_ZONE);
    assert!(d.is_team_owned());
    let team = server
        .systems
        .social
        .teams
        .team_by_obj_id(a.p.obj_id)
        .unwrap();
    server.systems.social.teams.ask_risky_team(
        &server,
        &a.p,
        team.id,
        b.p.character_id,
        crate::systems::social::team::risky_action::KICK,
    );
    await_loader(&d).await;
    assert_eq!(b.instance(), 0);
    assert!(!d.has_access(b.p.character_id));
    assert!(d.st().enter_requests.is_empty());
    dungeon::leave_dungeon_tick(&server, &d);
    assert!(d.world_id().is_none());
    assert!(server.systems.instances.all_dungeons().is_empty());
}

#[tokio::test]
async fn separate_parties_are_isolated_and_leader_reset_creates_fresh_progress() {
    let server = dungeon_server();
    let a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    let b = Pc::new(&server, 2, "Beta", 100.0, 100.0);
    let c = Pc::new(&server, 3, "Gamma", 100.0, 100.0);
    let e = Pc::new(&server, 4, "Delta", 100.0, 100.0);
    form_party(&server, &a, &b);
    form_party(&server, &c, &e);
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    use_enter_func(&server, &c.p, DUNGEON_ZONE);
    let d1 = server.systems.instances.all_dungeons()[0].clone();
    let d2 = server.systems.instances.all_dungeons()[1].clone();
    assert!(indun::reset_dungeon_instance(&server, &a.p, DUNGEON_ZONE).is_err());
    await_loader(&d1).await;
    await_loader(&d2).await;
    use_enter_func(&server, &b.p, DUNGEON_ZONE);
    assert_eq!(a.instance(), b.instance());
    assert_ne!(a.instance(), c.instance());
    d1.set_room_cleared(5);
    assert!(!d2.is_room_cleared(5));
    assert!(indun::reset_dungeon_instance(&server, &a.p, DUNGEON_ZONE).is_err());
    use_exit_func(&server, &a.p);
    use_exit_func(&server, &b.p);
    assert!(indun::reset_dungeon_instance(&server, &b.p, DUNGEON_ZONE).is_err());
    indun::reset_dungeon_instance(&server, &a.p, DUNGEON_ZONE).unwrap();
    assert!(d1.world_id().is_none());
    assert!(d2.world_id().is_some());
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let fresh = server
        .systems
        .instances
        .all_dungeons()
        .into_iter()
        .find(|d| d.has_access(1))
        .unwrap();
    assert_ne!(fresh.uid, d1.uid); // world ids can be recycled, run identities cannot
    assert!(!fresh.is_room_cleared(5));
    await_loader(&fresh).await;
    use_enter_func(&server, &b.p, DUNGEON_ZONE);
    assert_eq!(b.instance(), a.instance());
    assert_eq!(
        server.systems.instances.indun.entry_count(1, DUNGEON_GROUP),
        2
    );
    dungeon::destroy_dungeon(&server, &fresh);
    dungeon::destroy_dungeon(&server, &d2);
}

#[tokio::test]
async fn same_run_reentry_does_not_charge_another_entry_and_destroy_cancels_loader() {
    let data = dungeon_data();
    let server = dungeon_server_with_config(data, |c| {
        c.dungeons.entry_limit.enabled = Some(true);
        c.dungeons.entry_limit.count = 1;
    });
    let a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    use_exit_func(&server, &a.p);
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    assert_eq!(a.instance(), d.world_id().unwrap());
    assert_eq!(
        server.systems.instances.indun.entry_count(1, DUNGEON_GROUP),
        1
    );
    use_exit_func(&server, &a.p);
    indun::reset_dungeon_instance(&server, &a.p, DUNGEON_ZONE).unwrap();
    assert!(!indun::request_dungeon_instance(
        &server,
        &a.p,
        DUNGEON_ZONE,
        0
    ));
    assert!(server.systems.instances.all_dungeons().is_empty());
    let b = Pc::new(&server, 2, "Beta", 100.0, 100.0);
    use_enter_func(&server, &b.p, DUNGEON_ZONE);
    let d = server.systems.instances.all_dungeons()[0].clone();
    dungeon::destroy_dungeon(&server, &d);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(b.instance(), 0);
    assert!(server.systems.instances.all_dungeons().is_empty());
    assert!(d.st().enter_requests.is_empty());
}

#[derive(Default)]
struct DungeonItems {
    items: std::sync::Mutex<Vec<(u32, u32)>>,
    fail: std::sync::atomic::AtomicBool,
    charges: std::sync::atomic::AtomicUsize,
}

impl crate::systems::npcs::hooks::EconomyBridge for DungeonItems {
    fn acquire_item(
        &self,
        _server: &GameServer,
        p: &PlayerRef,
        _task: u8,
        item: u32,
        _count: i32,
        _grade: i32,
    ) -> bool {
        if self.fail.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        self.items.lock().unwrap().push((p.character_id, item));
        true
    }
    fn bag_consume_item(
        &self,
        _server: &GameServer,
        _p: &PlayerRef,
        _task: u8,
        _item: u32,
        count: i32,
    ) -> i32 {
        self.charges
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        count
    }
}

#[tokio::test]
async fn dungeon_admission_consumes_one_ticket_and_failed_admission_leaves_no_run() {
    let mut data = dungeon_data();
    data.systems
        .instances
        .indun
        .zones
        .get_mut(&DUNGEON_GROUP)
        .unwrap()
        .item_id = 123;
    let server = dungeon_server_with(data);
    let a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    // Without an item transaction hook, entry must fail closed.
    assert!(!indun::request_dungeon_instance(
        &server,
        &a.p,
        DUNGEON_ZONE,
        0
    ));
    assert!(server.systems.instances.all_dungeons().is_empty());
    assert_eq!(
        server.systems.instances.indun.entry_count(1, DUNGEON_GROUP),
        0
    );
    let items = Arc::new(DungeonItems::default());
    server.systems.npcs.hooks.economy.install(items.clone());
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    use_exit_func(&server, &a.p);
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    assert_eq!(items.charges.load(std::sync::atomic::Ordering::Relaxed), 1);
    dungeon::destroy_dungeon(&server, &d);
}

fn active_mentor_quest(p: &PlayerRef, quest_id: u32) {
    use crate::systems::quests::{model::kind, state::Quest};
    let mut c = p.data();
    let mut quest = Quest::new_blank(quest_id, quest_id);
    quest.step = kind::PROGRESS;
    let h = c.quest_state.alloc(quest);
    c.quest_state.active.add(quest_id, h);
}

fn mentor_chest_fixture(data: &mut GameData) {
    use crate::systems::npcs::doodad::templates::*;
    let def = &super::mentoring::DUNGEONS[1]; // Burnt Castle: two pairs
    let dd = &mut data.systems.npcs.doodad;
    let groups: Vec<_> = def
        .phases
        .iter()
        .map(|&id| DoodadFuncGroups {
            id,
            almighty: def.chest_id,
            group_kind_id: doodad_func_group_kind::NORMAL,
            ..Default::default()
        })
        .collect();
    dd.templates.insert(
        def.chest_id,
        DoodadTemplate {
            id: def.chest_id,
            func_groups: groups,
            ..Default::default()
        },
    );
    for (stage, &phase) in def.phases.iter().enumerate() {
        let (name, func) = if matches!(stage, 2 | 4) {
            (
                "DoodadFuncLootItem",
                FuncTemplate::LootItem(DoodadFuncLootItem {
                    id: phase,
                    item_id: if stage == 2 { 31589 } else { 31590 },
                    count_min: 1,
                    count_max: 1,
                    percent: 10000,
                    ..Default::default()
                }),
            )
        } else {
            (
                "DoodadFuncFakeUse",
                FuncTemplate::FakeUse(DoodadFuncFakeUse {
                    id: phase,
                    fake_skill_id: if stage == 1 { 24656 } else { 24657 },
                    ..Default::default()
                }),
            )
        };
        dd.func_templates
            .entry(name)
            .or_default()
            .insert(phase, func);
        dd.funcs_by_groups.insert(
            phase,
            vec![DoodadFunc {
                group_id: phase,
                func_id: phase,
                func_key: phase,
                func_type: name.into(),
                next_phase: def.phases.get(stage + 1).copied().unwrap_or(20858) as i32,
                ..Default::default()
            }],
        );
    }
}

#[tokio::test]
async fn mentor_chests_require_boss_party_quests_levels_and_seal_order() {
    use crate::systems::npcs::doodad::manager;
    let mut data = dungeon_data();
    mentor_chest_fixture(&mut data);
    let server = dungeon_server_with(data);
    let mentor = Pc::new(&server, 1, "Mentor", 100.0, 100.0);
    let mentee = Pc::new(&server, 2, "Mentee", 100.0, 100.0);
    let late = Pc::new(&server, 3, "Late", 100.0, 100.0);
    mentor.p.data().unit.level = 40;
    mentee.p.data().unit.level = 39;
    late.p.data().unit.level = 39;
    active_mentor_quest(&mentor.p, 6088);
    active_mentor_quest(&mentee.p, 6087);
    active_mentor_quest(&late.p, 6087);
    form_party(&server, &mentor, &mentee);
    use_enter_func(&server, &mentor.p, DUNGEON_ZONE);
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    use_enter_func(&server, &mentee.p, DUNGEON_ZONE);
    let world = d.world(&server).unwrap();
    let items = Arc::new(DungeonItems::default());
    server.systems.npcs.hooks.economy.install(items.clone());
    let mut boss = Npc::new(10518);
    boss.unit.transform.set_xyz(200.0, 200.0, 20.0);
    let boss = Entity::new(0x8000, boss);
    world.spawn(&boss, &server.view_ctx());
    events::on_do_die(
        &server,
        &WorldObject::from(&boss),
        Some(&WorldObject::from(&mentor.p)),
    );
    let mut chests = world.all_doodads();
    chests.sort_by_key(|d| d.obj_id);
    assert_eq!(chests.len(), 2);
    // Duplicate death signals never generate extra rewards.
    events::on_do_die(
        &server,
        &WorldObject::from(&boss),
        Some(&WorldObject::from(&mentor.p)),
    );
    assert_eq!(world.all_doodads().len(), 2);
    let chest = &chests[0];
    let use_chest = |p: &PlayerRef, skill, phase| {
        manager::use_doodad(
            &server,
            &world,
            chest,
            Some(&WorldObject::from(p)),
            skill,
            phase,
        )
    };
    use_chest(&mentee.p, 24656, 0);
    assert_eq!(chest.data().func_group_id, 21051);
    use_chest(&mentor.p, 9999, 0);
    assert_eq!(chest.data().func_group_id, 21051);
    use_chest(&mentor.p, 24657, 20857);
    assert_eq!(chest.data().func_group_id, 21051);
    use_chest(&mentor.p, 24657, 0);
    assert_eq!(chest.data().func_group_id, 20854);
    // A qualified late arrival was absent at the kill, and cannot take the second seal.
    form_party(&server, &mentor, &late);
    use_enter_func(&server, &late.p, DUNGEON_ZONE);
    use_chest(&late.p, 24656, 0);
    assert_eq!(chest.data().func_group_id, 20854);
    mentee.p.data().unit.level = 40;
    use_chest(&mentee.p, 24656, 0);
    assert_eq!(chest.data().func_group_id, 20854);
    mentee.p.data().unit.level = 39;
    items.fail.store(true, std::sync::atomic::Ordering::Relaxed);
    use_chest(&mentee.p, 24656, 0);
    assert_eq!(chest.data().func_group_id, 20855);
    assert!(items.items.lock().unwrap().is_empty());
    items
        .fail
        .store(false, std::sync::atomic::Ordering::Relaxed);
    use_chest(&mentee.p, 0, 0);
    assert_eq!(chest.data().func_group_id, 20856);
    // The mentee quest may already have completed when the mentor collects their seal.
    mentee.p.data().quest_state.active.remove(6087);
    use_chest(&mentor.p, 24657, 0);
    assert_eq!(chest.data().func_group_id, 20858);
    use_chest(&mentor.p, 24657, 0);
    use_chest(&mentee.p, 24656, 0);
    assert_eq!(*items.items.lock().unwrap(), vec![(2, 31589), (1, 31590)]);
    dungeon::destroy_dungeon(&server, &d);
    // Recycled world ids cannot carry a delayed boss death into a fresh run.
    active_mentor_quest(&mentee.p, 6087);
    use_enter_func(&server, &mentor.p, DUNGEON_ZONE);
    let fresh = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&fresh).await;
    use_enter_func(&server, &mentee.p, DUNGEON_ZONE);
    let fresh_world = fresh.world(&server).unwrap();
    assert_eq!(fresh_world.id, world.id);
    events::on_do_die(
        &server,
        &WorldObject::from(&boss),
        Some(&WorldObject::from(&mentor.p)),
    );
    assert!(fresh_world.all_doodads().is_empty());
    let mut replacement = Npc::new(10518);
    replacement.unit.transform.set_xyz(200.0, 200.0, 20.0);
    let replacement = Entity::new(boss.obj_id, replacement);
    fresh_world.spawn(&replacement, &server.view_ctx());
    events::on_do_die(
        &server,
        &WorldObject::from(&boss),
        Some(&WorldObject::from(&mentor.p)),
    );
    assert!(fresh_world.all_doodads().is_empty());
    events::on_do_die(
        &server,
        &WorldObject::from(&replacement),
        Some(&WorldObject::from(&mentor.p)),
    );
    assert_eq!(fresh_world.all_doodads().len(), 2);
    dungeon::destroy_dungeon(&server, &fresh);
}

#[tokio::test]
async fn mentor_boss_kill_without_both_quests_cannot_be_qualified_afterwards() {
    let mut data = dungeon_data();
    mentor_chest_fixture(&mut data);
    let server = dungeon_server_with(data);
    let a = Pc::new(&server, 1, "Mentor", 100.0, 100.0);
    let b = Pc::new(&server, 2, "Mentee", 100.0, 100.0);
    a.p.data().unit.level = 40;
    b.p.data().unit.level = 31;
    active_mentor_quest(&a.p, 6088);
    form_party(&server, &a, &b);
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    use_enter_func(&server, &b.p, DUNGEON_ZONE);
    let world = d.world(&server).unwrap();
    let mut boss = Npc::new(10518);
    boss.unit.transform.set_xyz(200.0, 200.0, 20.0);
    let boss = Entity::new(0x8000, boss);
    world.spawn(&boss, &server.view_ctx());
    events::on_do_die(
        &server,
        &WorldObject::from(&boss),
        Some(&WorldObject::from(&a.p)),
    );
    assert!(world.all_doodads().is_empty());
    active_mentor_quest(&b.p, 6087);
    events::on_do_die(
        &server,
        &WorldObject::from(&boss),
        Some(&WorldObject::from(&a.p)),
    );
    assert!(world.all_doodads().is_empty());
    dungeon::destroy_dungeon(&server, &d);
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_mentor_chest_seals_complete_daily_quests_and_award_gilda() {
    use crate::systems::{npcs::doodad, quests};
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let files = crate::data::client_files::ClientFileManager::from_sources(&[], &app);
    let mut data = dungeon_data();
    data.items = crate::data::items::ItemData::load(&conn).unwrap();
    data.systems.npcs.doodad = doodad::data::DoodadData::load_from(&conn).unwrap();
    data.systems.quests = quests::Data::load_from(&conn, &files).unwrap();
    // Verify every version-specific binding against the real tables, including level ranges
    // and all five chest phases, rather than accepting constants without data evidence.
    for def in super::mentoring::DUNGEONS {
        let dd = &data.systems.npcs.doodad;
        assert!(dd.template(def.chest_id).is_some());
        for phase in def.phases {
            assert!(!dd.funcs_for_group(phase).is_empty());
        }
        for (quest, range) in [
            (def.mentor_quest, def.mentor_levels),
            (def.mentee_quest, def.mentee_levels),
        ] {
            let template = data.systems.quests.template(quest).unwrap();
            let components: Vec<_> = template
                .components
                .iter()
                .filter_map(|c| data.systems.quests.component(*c))
                .filter(|c| c.kind_id == quests::model::kind::START)
                .collect();
            assert!(!components.is_empty());
            let reqs = data
                .systems
                .quests
                .unit_reqs
                .requirements("QuestComponent", components[0].id);
            assert!(reqs
                .iter()
                .any(|r| r.kind == quests::unit_reqs::req_kind::LEVEL
                    && r.value1 == u32::from(range.0)));
            assert!(reqs
                .iter()
                .any(|r| r.kind == quests::unit_reqs::req_kind::MAX_LEVEL
                    && r.value1 == u32::from(range.1)));
        }
    }
    let server = dungeon_server_with(data);
    crate::systems::economy::bridges::install(&server);
    quests::bridges::install(&server);
    let mentor = Pc::new(&server, 1, "Mentor", 100.0, 100.0);
    let mentee = Pc::new(&server, 2, "Mentee", 100.0, 100.0);
    mentor.p.data().unit.level = 40;
    mentee.p.data().unit.level = 31;
    form_party(&server, &mentor, &mentee);
    let mut lucius = Npc::new(14422);
    lucius.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let lucius = Entity::new(0x8001, lucius);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&lucius, &server.view_ctx());
    assert_eq!(
        quests::events::add_quest_from_npc(&server, &mentor.p, 6088, lucius.obj_id),
        Some(true)
    );
    assert_eq!(
        quests::events::add_quest_from_npc(&server, &mentee.p, 6087, lucius.obj_id),
        Some(true)
    );
    server.systems.quests.run_queued_evaluations(&server);
    use_enter_func(&server, &mentor.p, DUNGEON_ZONE);
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    use_enter_func(&server, &mentee.p, DUNGEON_ZONE);
    let world = d.world(&server).unwrap();
    let mut boss = Npc::new(10518);
    boss.unit.transform.set_xyz(200.0, 200.0, 20.0);
    let boss = Entity::new(0x8000, boss);
    world.spawn(&boss, &server.view_ctx());
    events::on_do_die(
        &server,
        &WorldObject::from(&boss),
        Some(&WorldObject::from(&mentor.p)),
    );
    let chest = world
        .all_doodads()
        .into_iter()
        .find(|d| d.data().template_id == 7659)
        .expect("mentor chest");
    for (p, skill) in [(&mentor.p, 24657), (&mentee.p, 24656), (&mentor.p, 24657)] {
        doodad::manager::use_doodad(
            &server,
            &world,
            &chest,
            Some(&WorldObject::from(p)),
            skill,
            0,
        );
        server.systems.quests.run_queued_evaluations(&server);
    }
    let gilda = |p: &PlayerRef| {
        let bag = p.data().inventory.bag().clone();
        let count = bag.lock().unwrap().items_by_template(23633, -1).1;
        count
    };
    assert_eq!(gilda(&mentor.p), 3);
    assert_eq!(gilda(&mentee.p), 1);
    assert!(quests::events::has_quest_completed_in(
        &mentor.p.data(),
        6088
    ));
    assert!(quests::events::has_quest_completed_in(
        &mentee.p.data(),
        6087
    ));
    doodad::manager::use_doodad(
        &server,
        &world,
        &chest,
        Some(&WorldObject::from(&mentor.p)),
        24657,
        0,
    );
    assert_eq!(gilda(&mentor.p), 3);
    quests::daily_reset(&server);
    assert!(!quests::events::has_quest_completed_in(
        &mentor.p.data(),
        6088
    ));
    assert!(!quests::events::has_quest_completed_in(
        &mentee.p.data(),
        6087
    ));
    dungeon::destroy_dungeon(&server, &d);
}

/// Uses an `EnterInstance` doodad func (`DoodadFuncEnterInstance.Use`).
fn use_enter_func(server: &GameServer, p: &PlayerRef, zone_id: u32) {
    let world = server.world.world(0).unwrap();
    let doodad = Entity::new(0x9000, Doodad::new(1));
    let ctx = DoodadCtx::new(server, world, doodad);
    let f = DoodadFuncEnterInstance {
        id: 1,
        zone_id,
        item_id: 0,
    };
    crate::systems::npcs::doodad::funcs::world::enter_instance(
        &f,
        &ctx,
        Some(&WorldObject::from(p)),
    );
}

fn use_exit_func(server: &GameServer, p: &PlayerRef) {
    let world = server.world.world(0).unwrap();
    let doodad = Entity::new(0x9001, Doodad::new(1));
    let ctx = DoodadCtx::new(server, world, doodad);
    crate::systems::npcs::doodad::funcs::world::exit_indun(
        &DoodadFuncExitIndun::default(),
        &ctx,
        Some(&WorldObject::from(p)),
    );
}

// ---------------------------------------------------------------------------------------------
// System-dungeon channel select (SC 0x206 / CS 0x13d / SC 0x207, WP-X1)
// ---------------------------------------------------------------------------------------------

/// `zones.id` of the library dungeon (the entrance func's `zone_id`).
const LIB_ZONE: u32 = 209;
/// `zones.zone_key` (the value `SCLoadInstance` sends).
const LIB_ZONE_KEY: u32 = 296;
const LIB_GROUP: u32 = 70;
const LIB_SPAWN: WorldSpawnPosition = WorldSpawnPosition {
    world_id: 3,
    zone_id: LIB_ZONE_KEY,
    x: 400.0,
    y: 400.0,
    z: 10.0,
    yaw: 90.0,
    pitch: 0.0,
    roll: 0.0,
};

/// A selective (`7457`) and a plain (`7458`) entrance doodad, like the real rows 2-9.
fn add_sys_entrance(
    data: &mut GameData,
    template_id: u32,
    group_id: u32,
    func_id: u32,
    selective: bool,
) {
    use crate::systems::npcs::doodad::templates::doodad_func_group_kind;
    let dd = &mut data.systems.npcs.doodad;
    dd.templates.insert(
        template_id,
        DoodadTemplate {
            id: template_id,
            func_groups: vec![DoodadFuncGroups {
                id: group_id,
                almighty: template_id,
                group_kind_id: doodad_func_group_kind::START,
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    dd.func_templates
        .entry("DoodadFuncEnterSysInstance")
        .or_default()
        .insert(
            func_id,
            FuncTemplate::EnterSysInstance(DoodadFuncEnterSysInstance {
                id: func_id,
                zone_id: LIB_ZONE,
                faction_id: 0,
                selective,
            }),
        );
    let func = DoodadFunc {
        group_id,
        func_id,
        func_key: func_id,
        func_type: "DoodadFuncEnterSysInstance".into(),
        next_phase: -1,
        // The real rows 2-9 bind the entrance interaction skills 24354-24373; a skill-bound
        // func is how `Doodad.Use` dispatches a Phase func (`func_for_skill`).
        skill_id: 24354,
        ..Default::default()
    };
    dd.funcs_by_groups.insert(group_id, vec![func.clone()]);
    dd.funcs_by_id.insert(func_id, func);
}

fn library_data() -> GameData {
    let mut data = dungeon_data();
    let lib = zone(LIB_ZONE, LIB_ZONE_KEY, LIB_GROUP);
    data.zones.zone_id_to_key.insert(lib.id, lib.zone_key);
    data.zones.groups.entry(lib.group_id).or_insert(ZoneGroup {
        id: lib.group_id,
        name: "group_70".into(),
        ..ZoneGroup::default()
    });
    data.zones.zones.insert(lib.zone_key, lib);
    data.systems
        .instances
        .zone_order
        .zones
        .push((LIB_ZONE_KEY, LIB_GROUP));
    data.systems.instances.indun.zones.insert(
        LIB_GROUP,
        IndunZone {
            zone_group_id: LIB_GROUP,
            name: "library".into(),
            level_min: 1,
            level_max: 55,
            max_players: 190,
            select_channel: true,
            ..IndunZone::default()
        },
    );
    add_sys_entrance(&mut data, 7457, 9001, 9002, true);
    add_sys_entrance(&mut data, 7458, 9003, 9004, false);
    data
}

fn library_server(data: GameData) -> Arc<GameServer> {
    let mirage_spawn = WorldSpawnPosition {
        world_id: 2,
        zone_id: MIRAGE_ZONE,
        x: 300.0,
        y: 300.0,
        z: 50.0,
        yaw: 200.0,
        ..WorldSpawnPosition::default()
    };
    build_server(
        data,
        vec![
            instance_template(1, "instance_test", DUNGEON_ZONE, DUNGEON_SPAWN),
            instance_template(2, "arche_mall_world", MIRAGE_ZONE, mirage_spawn),
            instance_template(3, "instance_library_1", LIB_ZONE_KEY, LIB_SPAWN),
        ],
        vec![
            lookup("instance_test", DUNGEON_SPAWN),
            lookup("arche_mall_world", mirage_spawn),
            lookup("instance_library_1", LIB_SPAWN),
        ],
        None,
    )
}

async fn create_library(server: &GameServer, channel: u32) -> Arc<dungeon::Dungeon> {
    let d = indun::create_system_instance(server, None, LIB_ZONE_KEY, channel, false, 0)
        .expect("library instance");
    await_loader(&d).await;
    d
}

fn spawn_sys_entrance(
    server: &GameServer,
    template_id: u32,
    group_id: u32,
    obj_id: u32,
) -> crate::world::DoodadRef {
    let mut d = Doodad::new(template_id);
    d.func_group_id = group_id;
    d.transform.set_xyz(100.0, 100.0, 100.0);
    d.transform.zone_id = TEST_ZONE;
    let d = Entity::new(obj_id, d);
    let world = server.world.world(0).unwrap();
    world.spawn(&d, &server.view_ctx());
    d
}

fn use_sys_entrance(server: &GameServer, p: &PlayerRef, doodad: &crate::world::DoodadRef) {
    let world = server.world.world(0).unwrap();
    crate::systems::npcs::doodad::manager::use_doodad(
        server,
        &world,
        doodad,
        Some(&WorldObject::from(p)),
        24354,
        0,
    );
}

fn select_body(inst_id: u32, doodad_obj_id: u32) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_u32(inst_id).write_bc(doodad_obj_id);
    s.into_bytes()
}

#[tokio::test]
async fn selective_system_entrance_sends_the_live_channel_list() {
    let server = library_server(library_data());
    let first = create_library(&server, 0).await;
    let second = create_library(&server, 1).await;
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    let doodad = spawn_sys_entrance(&server, 7457, 9001, 0xD001);
    a.frames();

    use_sys_entrance(&server, &a.p, &doodad);
    let f = a.frames();
    let expected = body(&SCSystemDungeonStatePacket {
        zone_key: LIB_ZONE,
        channels: vec![
            SystemDungeonChannel {
                index: 0,
                instance_id: first.world_id().unwrap(),
                restrict: 190,
                current: 0,
            },
            SystemDungeonChannel {
                index: 1,
                instance_id: second.world_id().unwrap(),
                restrict: 190,
                current: 0,
            },
        ],
    });
    assert!(
        f.contains(&(sc::SCUnkPacket15, expected)),
        "0x206 list: {f:?}"
    );
    assert!(!f.iter().any(|(op, _)| *op == sc::SCLoadInstancePacket));
    assert_eq!(a.instance(), 0, "the list alone does not move the player");
}

#[tokio::test]
async fn selecting_the_second_channel_loads_the_player_there_and_sends_0x207() {
    let server = library_server(library_data());
    let _first = create_library(&server, 0).await;
    let second = create_library(&server, 1).await;
    let second_id = second.world_id().unwrap();
    let mut a = Pc::new(&server, 1, "Alpha", 137.5, 88.0);
    a.p.data().unit.transform.set_xyz(137.5, 88.0, 100.0);
    let doodad = spawn_sys_entrance(&server, 7457, 9001, 0xD002);
    doodad.data().transform.set_xyz(137.5, 88.0, 100.0);
    use_sys_entrance(&server, &a.p, &doodad);
    let list = a.frames();
    assert!(list.iter().any(|(op, _)| *op == sc::SCUnkPacket15));
    assert!(
        a.p.data().main_world_position.is_none(),
        "the dialog alone saves no return point"
    );

    // CS 0x13d with the second row's instId loads the player into that instance.
    a.frame(
        &server,
        cs::CSSelectSysInstanceChannelPacket,
        &select_body(second_id, doodad.obj_id),
    )
    .await;
    let f = a.frames();
    let expected_load = load_instance(
        second_id,
        LIB_ZONE_KEY,
        LIB_SPAWN.x,
        LIB_SPAWN.y,
        LIB_SPAWN.z,
        (0.0, 0.0, worlds::deg_to_rad(LIB_SPAWN.yaw)),
    );
    assert!(
        f.contains(&(sc::SCLoadInstancePacket, expected_load)),
        "load into the picked channel: {f:?}"
    );
    assert_eq!(a.instance(), second_id);
    let mwp = a.p.data().main_world_position.expect("return point saved");
    assert_eq!((mwp.instance_id, mwp.zone_id), (0, TEST_ZONE));
    assert_eq!((mwp.x, mwp.y, mwp.z), (137.5, 88.0, 100.0));

    // The channel was loaded already, so SC 0x207 follows SCLoadInstance directly: the zone
    // of the load, the instance and the 0-based index.
    let expected = body(&SCSystemDungeonChannelPacket {
        zone_id: LIB_ZONE_KEY as i32,
        instance_id: second_id,
        instance_index: 1,
    });
    assert!(f.contains(&(sc::SCUnkPacket16, expected)), "0x207: {f:?}");

    // It is answered once; the load confirmation does not repeat it.
    a.frame(&server, cs::CSInstanceLoadedPacket, &[]).await;
    let f = a.frames();
    assert!(
        !f.iter().any(|(op, _)| *op == sc::SCUnkPacket16),
        "0x207 once: {f:?}"
    );
}

#[tokio::test]
async fn selecting_a_channel_that_is_still_loading_sends_0x207_after_instance_loaded() {
    let server = library_server(library_data());
    let _first = create_library(&server, 0).await;
    let second = indun::create_system_instance(&server, None, LIB_ZONE_KEY, 1, false, 0)
        .expect("library instance");
    let second_id = second.world_id().unwrap();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    let doodad = spawn_sys_entrance(&server, 7457, 9001, 0xD008);
    use_sys_entrance(&server, &a.p, &doodad);
    a.frames();

    a.frame(
        &server,
        cs::CSSelectSysInstanceChannelPacket,
        &select_body(second_id, doodad.obj_id),
    )
    .await;
    let f = a.frames();
    assert!(f
        .iter()
        .any(|(op, _)| *op == sc::SCProcessingInstancePacket));
    assert!(!f.iter().any(|(op, _)| *op == sc::SCUnkPacket16));
    assert_eq!(a.instance(), 0);

    // The loader moves the player once the channel is ready.
    await_loader(&second).await;
    assert_eq!(a.instance(), second_id);
    a.frame(&server, cs::CSInstanceLoadedPacket, &[]).await;
    let f = a.frames();
    let expected = body(&SCSystemDungeonChannelPacket {
        zone_id: LIB_ZONE_KEY as i32,
        instance_id: second_id,
        instance_index: 1,
    });
    assert!(
        f.contains(&(sc::SCUnkPacket16, expected)),
        "0x207 after load: {f:?}"
    );
}

#[tokio::test]
async fn channel_select_saves_the_main_world_position_only_on_a_successful_pick() {
    let server = library_server(library_data());
    let _first = create_library(&server, 0).await;
    let second = create_library(&server, 1).await;
    let second_id = second.world_id().unwrap();
    let mut a = Pc::new(&server, 1, "Alpha", 137.5, 88.0);
    a.p.data().unit.transform.set_xyz(137.5, 88.0, 100.0);
    let doodad = spawn_sys_entrance(&server, 7457, 9001, 0xD009);
    doodad.data().transform.set_xyz(137.5, 88.0, 100.0);
    // A return point from an earlier visit must survive a refused pick.
    let sentinel = crate::models::transform::Transform {
        instance_id: 0,
        zone_id: TEST_ZONE,
        x: -1.0,
        y: -2.0,
        z: -3.0,
        ..Default::default()
    };
    a.p.data().main_world_position = Some(sentinel);
    use_sys_entrance(&server, &a.p, &doodad);
    a.frames();
    assert_eq!(
        a.p.data().main_world_position,
        Some(sentinel),
        "the dialog saves nothing"
    );

    // A refused pick (unknown doodad) leaves the saved position untouched.
    a.frame(
        &server,
        cs::CSSelectSysInstanceChannelPacket,
        &select_body(second_id, 0xDEAD),
    )
    .await;
    a.frames();
    assert_eq!(a.p.data().main_world_position, Some(sentinel));

    // A successful pick replaces it with the pre-entry position: the player is still in the
    // default instance, so the direct entrance's save rule applies.
    a.frame(
        &server,
        cs::CSSelectSysInstanceChannelPacket,
        &select_body(second_id, doodad.obj_id),
    )
    .await;
    a.frames();
    let mwp = a.p.data().main_world_position.expect("return point saved");
    assert_eq!((mwp.instance_id, mwp.zone_id), (0, TEST_ZONE));
    assert_eq!((mwp.x, mwp.y, mwp.z), (137.5, 88.0, 100.0));
    assert_eq!(a.instance(), second_id);
}

#[tokio::test]
async fn selected_channel_full_refuses_with_the_normal_instance_quota_error() {
    let mut data = library_data();
    data.systems
        .instances
        .indun
        .zones
        .get_mut(&LIB_GROUP)
        .unwrap()
        .max_players = 1;
    let server = library_server(data);
    let b = Pc::new(&server, 2, "Beta", 100.0, 100.0);
    let full = indun::create_system_instance(&server, Some(&b.p), LIB_ZONE_KEY, 0, false, 0)
        .expect("library instance");
    await_loader(&full).await;
    let full_id = full.world_id().unwrap();
    assert!(worlds::has_character(
        &full.world(&server).unwrap(),
        b.p.character_id
    ));
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    let doodad = spawn_sys_entrance(&server, 7457, 9001, 0xD003);
    use_sys_entrance(&server, &a.p, &doodad);
    a.frames();

    a.frame(
        &server,
        cs::CSSelectSysInstanceChannelPacket,
        &select_body(full_id, doodad.obj_id),
    )
    .await;
    let f = a.frames();
    let error = body(&crate::packets::world::SCErrorMsgPacket::new(
        worlds::error_message::INSTANCE_QUOTA,
        0,
        true,
    ));
    assert!(
        f.contains(&(sc::SCErrorMsgPacket, error)),
        "full error: {f:?}"
    );
    assert!(!f.iter().any(|(op, _)| *op == sc::SCLoadInstancePacket));
    assert!(!f.iter().any(|(op, _)| *op == sc::SCUnkPacket16));
    assert_eq!(a.instance(), 0);
}

#[tokio::test]
async fn selection_ignores_a_bad_doodad_out_of_reach_and_a_stale_instance() {
    let server = library_server(library_data());
    let _first = create_library(&server, 0).await;
    let second = create_library(&server, 1).await;
    let second_id = second.world_id().unwrap();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    let doodad = spawn_sys_entrance(&server, 7457, 9001, 0xD004);
    let plain = spawn_sys_entrance(&server, 7458, 9003, 0xD005);
    use_sys_entrance(&server, &a.p, &doodad);
    a.frames();

    // Unknown doodad object id.
    a.frame(
        &server,
        cs::CSSelectSysInstanceChannelPacket,
        &select_body(second_id, 0xDEAD),
    )
    .await;
    // A non-selective entrance of the same zone.
    a.frame(
        &server,
        cs::CSSelectSysInstanceChannelPacket,
        &select_body(second_id, plain.obj_id),
    )
    .await;
    // A stale / unknown world instance id.
    a.frame(
        &server,
        cs::CSSelectSysInstanceChannelPacket,
        &select_body(0x00C0_FFEE, doodad.obj_id),
    )
    .await;
    let f = a.frames();
    assert!(f.is_empty(), "ignored picks send nothing: {f:?}");
    assert_eq!(a.instance(), 0);

    // Out of reach (> 5 m + 15 m doodad allowance).
    doodad.data().transform.set_xyz(1000.0, 100.0, 100.0);
    a.frame(
        &server,
        cs::CSSelectSysInstanceChannelPacket,
        &select_body(second_id, doodad.obj_id),
    )
    .await;
    let f = a.frames();
    assert!(!f.iter().any(|(op, _)| *op == sc::SCLoadInstancePacket));
    assert!(!f.iter().any(|(op, _)| *op == sc::SCUnkPacket16));
    assert_eq!(a.instance(), 0);
}

#[tokio::test]
async fn non_selective_entrance_still_enters_channel_zero() {
    let server = library_server(library_data());
    let first = create_library(&server, 0).await;
    let first_id = first.world_id().unwrap();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    let doodad = spawn_sys_entrance(&server, 7458, 9003, 0xD006);
    a.frames();

    use_sys_entrance(&server, &a.p, &doodad);
    let f = a.frames();
    assert!(
        !f.iter().any(|(op, _)| *op == sc::SCUnkPacket15),
        "no channel list: {f:?}"
    );
    let expected = load_instance(
        first_id,
        LIB_ZONE_KEY,
        LIB_SPAWN.x,
        LIB_SPAWN.y,
        LIB_SPAWN.z,
        (0.0, 0.0, worlds::deg_to_rad(LIB_SPAWN.yaw)),
    );
    assert!(f.contains(&(sc::SCLoadInstancePacket, expected)));
    assert_eq!(a.instance(), first_id);
}

#[tokio::test]
async fn selective_entrance_without_a_live_instance_creates_channel_zero() {
    let server = library_server(library_data());
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    let doodad = spawn_sys_entrance(&server, 7457, 9001, 0xD007);
    a.frames();

    use_sys_entrance(&server, &a.p, &doodad);
    let f = a.frames();
    assert!(
        !f.iter().any(|(op, _)| *op == sc::SCUnkPacket15),
        "no list without a live instance: {f:?}"
    );
    assert!(f
        .iter()
        .any(|(op, _)| *op == sc::SCProcessingInstancePacket));
    let dungeons = server.systems.instances.all_dungeons();
    assert_eq!(dungeons.len(), 1);
    await_loader(&dungeons[0]).await;
    assert_eq!(a.instance(), dungeons[0].world_id().unwrap());
}

/// `CSInstanceLoaded` after `SCLoadInstance` re-files the character in the arrival instance's
/// region grid: the units / doodads in view are sent and the character is shown to the players
/// there without a movement packet, the old instance's objects are removed, and nothing is sent
/// twice on a repeated load or the next move.
#[tokio::test]
async fn instance_return_refiles_character_and_sends_main_world_objects_without_movement() {
    let server = dungeon_server();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    let mut observer = Pc::new(&server, 2, "Observer", 110.0, 100.0);
    a.frames();
    observer.frames();

    // main_world fixtures around A's return point (100, 100, 100).
    let main = server.world.world(0).unwrap();
    let ctx = server.view_ctx();
    let mut main_npc = Npc::new(500);
    main_npc.unit.transform.set_xyz(105.0, 100.0, 100.0);
    let main_npc = Entity::new(0x6001, main_npc);
    main.spawn(&main_npc, &ctx);
    let mut main_doodad = Doodad::new(600);
    main_doodad.transform.set_xyz(106.0, 100.0, 100.0);
    let main_doodad = Entity::new(0x6002, main_doodad);
    main.spawn(&main_doodad, &ctx);
    a.frames();
    observer.frames();

    // Enter the dungeon (instance 100), which has an NPC of its own.
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let d = server
        .systems
        .instances
        .dungeon_of_world(100)
        .expect("dungeon");
    let instance_world = server.world.world(100).unwrap();
    let mut instance_npc = Npc::new(501);
    instance_npc.unit.transform.set_xyz(200.0, 200.0, 20.0);
    let instance_npc = Entity::new(0x6003, instance_npc);
    instance_world.spawn(&instance_npc, &ctx);
    await_loader(&d).await;
    a.frames();

    // CSInstanceLoaded files A into the dungeon immediately (no move).
    a.frame(&server, cs::CSInstanceLoadedPacket, &[]).await;
    let entered = a.frames();
    assert!(entered
        .iter()
        .any(|(op, b)| *op == sc::SCUnitStatePacket && bc(b) == 0x6003));
    // The observer in main_world loses A when it leaves main_world's region grid.
    assert!(removed_ids(&observer.frames()).contains(&a.p.obj_id));

    // Leave through the exit: back to the main_world return point.
    use_exit_func(&server, &a.p);
    let load = a.frames();
    assert!(load.iter().any(|(op, _)| *op == sc::SCLoadInstancePacket));
    assert_eq!(a.instance(), 0);

    // The client cleared its world on SCLoadInstance: CSInstanceLoaded must repopulate it
    // without any movement.
    a.frame(&server, cs::CSInstanceLoadedPacket, &[]).await;
    let f = a.frames();
    let npc_states = f
        .iter()
        .filter(|(op, b)| *op == sc::SCUnitStatePacket && bc(b) == 0x6001)
        .count();
    assert_eq!(npc_states, 1, "main_world NPC sent once: {f:?}");
    let doodad_creates = f
        .iter()
        .filter(|(op, b)| {
            *op == sc::SCDoodadsCreatedPacket && b.first() == Some(&1) && bc(&b[1..]) == 0x6002
        })
        .count();
    assert_eq!(doodad_creates, 1, "main_world doodad sent once: {f:?}");
    // The stale instance objects are removed from the returning client.
    assert!(removed_ids(&f).contains(&0x6003));
    // The observer in main_world sees the player appear again.
    let seen = observer
        .frames()
        .iter()
        .filter(|(op, b)| *op == sc::SCUnitStatePacket && bc(b) == a.p.obj_id)
        .count();
    assert_eq!(seen, 1, "observer sees the returning player once");

    // A repeated load (and the next move) must not spawn anything twice.
    a.frame(&server, cs::CSInstanceLoadedPacket, &[]).await;
    let again = a.frames();
    assert!(!again
        .iter()
        .any(|(op, b)| *op == sc::SCUnitStatePacket && bc(b) == 0x6001));
    assert!(!again
        .iter()
        .any(|(op, _)| *op == sc::SCDoodadsCreatedPacket));
    assert!(!observer
        .frames()
        .iter()
        .any(|(op, b)| *op == sc::SCUnitStatePacket && bc(b) == a.p.obj_id));
    a.move_to(&server, 100.0, 100.0, 100.0).await;
    let moved = a.frames();
    assert!(!moved
        .iter()
        .any(|(op, b)| *op == sc::SCUnitStatePacket && bc(b) == 0x6001));
    assert!(!moved
        .iter()
        .any(|(op, _)| *op == sc::SCDoodadsCreatedPacket));
}

#[tokio::test]
async fn party_dungeon_create_enter_script_leave_kick_and_cleanup() {
    let server = dungeon_server();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    let mut b = Pc::new(&server, 2, "Beta", 110.0, 100.0);
    let mut c = Pc::new(&server, 3, "Gamma", 105.0, 100.0);

    // A and B form a party.
    let teams = &server.systems.social.teams;
    teams.ask_to_join(&server, &a.p, "Beta", 0, true, Some(&b.p));
    teams.reply_to_join_team(&server, &b.p, 0, true, 1, false, "", false);
    let team_id = teams.team_by_obj_id(a.p.obj_id).expect("team").id;
    a.frames();
    b.frames();
    c.frames();

    // ---- A uses the dungeon entrance: the dungeon (instance 100) is created and loads.
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let mwp = a.p.data().main_world_position.expect("return point saved");
    assert_eq!(
        (mwp.instance_id, mwp.zone_id, mwp.x, mwp.y, mwp.z),
        (0, TEST_ZONE, 100.0, 100.0, 100.0)
    );
    let d = server
        .systems
        .instances
        .dungeon_of_world(100)
        .expect("dungeon of instance 100");
    assert!(!d.is_system);
    assert!(d.is_team_owned());
    assert!(d.has_access(1) && d.has_access(2));
    assert_eq!(
        server.systems.instances.indun.entry_count(1, DUNGEON_GROUP),
        1
    );
    let processing = body(&SCProcessingInstancePacket {
        zone_id: DUNGEON_ZONE as i32,
    });
    // CreateWorldInstance(notifyPlayer) + QueuePlayer while loading.
    let f = a.frames();
    let p: Vec<_> = f
        .iter()
        .filter(|(op, _)| *op == sc::SCProcessingInstancePacket)
        .collect();
    assert_eq!(p.len(), 2);
    assert!(p.iter().all(|(_, b)| *b == processing));
    assert!(!f.iter().any(|(op, _)| *op == sc::SCLoadInstancePacket));

    // The room doodad and two NPCs exist in the instance before the loader registers again.
    let dworld = server.world.world(100).unwrap();
    let ctx = server.view_ctx();
    let mut room = Doodad::new(777);
    room.transform.set_xyz(205.0, 200.0, 20.0);
    let room = Entity::new(0x7000, room);
    dworld.spawn(&room, &ctx);
    let mut tagged = Npc::new(901);
    tagged.unit.transform.set_xyz(210.0, 210.0, 20.0);
    tagged.unit.hp = 10;
    tagged.unit.max_hp = 10;
    let tagged = Entity::new(0x7001, tagged);
    dworld.spawn(&tagged, &ctx);
    let mut boss = Npc::new(900);
    boss.unit.transform.set_xyz(220.0, 220.0, 20.0);
    let boss = Entity::new(0x7002, boss);
    dworld.spawn(&boss, &ctx);

    await_loader(&d).await;
    assert!(d.st().finished_loading);
    assert!(d.st().enter_requests.is_empty());
    // The room event found its doodad on the second registration.
    assert_eq!(
        server.systems.instances.events.regs(100).area_clear.len(),
        1
    );
    assert_eq!(
        server.systems.instances.events.regs(100).unit_killed.len(),
        2
    );
    // A was moved: SCLoadInstance(instance, zone, spawn, DegToRad(rotation)).
    let f = a.frames();
    let expected = load_instance(
        100,
        DUNGEON_ZONE,
        200.0,
        200.0,
        20.0,
        (0.0, 0.0, worlds::deg_to_rad(90.0)),
    );
    assert!(f.contains(&(sc::SCLoadInstancePacket, expected)));
    assert_eq!(a.instance(), 100);
    assert_eq!(a.p.data().unit.transform.zone_id, DUNGEON_ZONE);
    assert!(a.p.data().disabled_set_position);
    // The C# setter reset the speed analysis here; the counter invalidates its baseline.
    assert!(a.p.data().teleport_seq > 0);
    assert!(worlds::has_character(&dworld, 1));
    let ev = &server.systems.instances.events.characters;
    assert_eq!(ev.count(&a.p, CharEvent::DungeonLeave), 1);
    assert_eq!(ev.count(&a.p, CharEvent::TeamLeave), 1);

    // ---- CSInstanceLoaded re-files A into the instance; the first move finds it filed.
    a.frame(&server, cs::CSInstanceLoadedPacket, &[]).await;
    let f = a.frames();
    assert_eq!(f.first().map(|(op, _)| *op), Some(sc::SCUnitStatePacket));
    assert!(f.iter().any(|(op, _)| *op == sc::SCDetailedTimeOfDayPacket));
    assert!(!a.p.data().disabled_set_position);
    let gone = removed_ids(&f);
    assert!(gone.contains(&b.p.obj_id) && gone.contains(&c.p.obj_id));
    assert!(f.iter().any(|(op, _)| *op == sc::SCDoodadsCreatedPacket));
    assert!(f
        .iter()
        .any(|(op, b)| *op == sc::SCUnitStatePacket && bc(b) == 0x7001));
    assert!(removed_ids(&c.frames()).contains(&a.p.obj_id));
    assert!(removed_ids(&b.frames()).contains(&a.p.obj_id));
    assert_eq!(WorldObject::from(&a.p).region().map(|r| r.0), Some(100));
    a.move_to(&server, 200.0, 200.0, 20.0).await;
    let f = a.frames();
    assert!(!f
        .iter()
        .any(|(op, b)| *op == sc::SCUnitStatePacket && bc(b) == 0x7001));
    assert!(!f.iter().any(|(op, _)| *op == sc::SCDoodadsCreatedPacket));

    // AreaClearTick: A is in room 6.
    dungeon::area_clear_tick(&server, &d);
    assert_eq!(server.systems.instances.events.room_player_count(1, 100), 1);

    // ---- B (party member) enters the loaded dungeon right away.
    use_enter_func(&server, &b.p, DUNGEON_ZONE);
    assert!(b
        .frames()
        .iter()
        .any(|(op, _)| *op == sc::SCLoadInstancePacket));
    assert_eq!(b.instance(), 100);
    b.frame(&server, cs::CSInstanceLoadedPacket, &[]).await;
    b.move_to(&server, 260.0, 200.0, 20.0).await;
    assert!(a
        .frames()
        .iter()
        .any(|(op, body)| *op == sc::SCUnitStatePacket && bc(body) == b.p.obj_id));
    b.frames();

    // ---- The boss dies: room 5 cleared through the action chain.
    assert!(!d.is_room_cleared(5));
    events::on_do_die(
        &server,
        &WorldObject::from(&boss),
        Some(&WorldObject::from(&a.p)),
    );
    assert!(d.is_room_cleared(5));

    // ---- A leaves through the exit doodad: back to the main world return point.
    use_exit_func(&server, &a.p);
    let f = a.frames();
    let expected = load_instance(
        mwp.world_id,
        TEST_ZONE,
        100.0,
        100.0,
        100.0,
        (
            worlds::deg_to_rad(mwp.rot_x),
            worlds::deg_to_rad(mwp.rot_y),
            worlds::deg_to_rad(mwp.rot_z),
        ),
    );
    assert!(f.contains(&(sc::SCLoadInstancePacket, expected)));
    assert_eq!(a.instance(), 0);
    assert!(!worlds::has_character(&dworld, 1));
    assert_eq!(ev.count(&a.p, CharEvent::DungeonLeave), 0);
    a.frame(&server, cs::CSInstanceLoadedPacket, &[]).await;
    a.move_to(&server, 100.0, 100.0, 100.0).await;
    assert!(c
        .frames()
        .iter()
        .any(|(op, body)| *op == sc::SCUnitStatePacket && bc(body) == a.p.obj_id));
    assert!(removed_ids(&b.frames()).contains(&a.p.obj_id));

    // AreaClearTick: room 6 is empty now -> the tagged NPC is removed.
    assert!(dworld.npc(0x7001).is_some());
    dungeon::area_clear_tick(&server, &d);
    assert!(dworld.npc(0x7001).is_none());
    assert_eq!(server.systems.instances.events.room_player_count(1, 100), 0);

    // ---- B leaves the party: kicked out after the (zero) delay, the empty dungeon goes away.
    teams.ask_risky_team(
        &server,
        &b.p,
        team_id,
        2,
        crate::systems::social::team::risky_action::LEAVE,
    );
    let leave_party = body(&crate::packets::world::SCErrorMsgPacket::new(
        worlds::error_message::INSTANCE_LEAVE_PARTY,
        0,
        true,
    ));
    assert!(b
        .frames()
        .contains(&(sc::SCErrorMsgPacket, leave_party.clone())));
    // The party auto-disbands: A left the dungeon (unsubscribed), so no dungeon handler runs.
    assert!(!a.frames().contains(&(sc::SCErrorMsgPacket, leave_party)));
    assert_eq!(d.st().leave_requests.len(), 1);
    tokio::time::sleep(Duration::from_millis(5)).await;
    dungeon::leave_dungeon_tick(&server, &d);
    assert!(b
        .frames()
        .iter()
        .any(|(op, _)| *op == sc::SCLoadInstancePacket));
    assert_eq!(b.instance(), 0);
    assert!(server.systems.instances.dungeon_of_world(100).is_none());
    assert!(!server.world.has_world(100));
    assert!(server.systems.npcs.spawns.get(100).is_none());
    assert!(d.world_id().is_none());
    // The instance id was released.
    assert_eq!(server.ids.world.get_next_id().unwrap(), 100);
}

#[tokio::test]
async fn dungeon_requirements_and_deletion() {
    let server = dungeon_server();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    a.p.data().unit.level = 60;
    a.frames();
    // Level too high.
    assert!(!indun::request_dungeon_instance(
        &server,
        &a.p,
        DUNGEON_ZONE,
        0
    ));
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCErrorMsgPacket,
            body(&crate::packets::world::SCErrorMsgPacket::new(
                worlds::error_message::INSTANCE_LEVEL,
                0,
                true
            ))
        )]
    );
    // Unknown zone / not a dungeon.
    assert!(!indun::request_dungeon_instance(&server, &a.p, 999, 0));
    assert!(!indun::request_dungeon_instance(
        &server, &a.p, TEST_ZONE, 0
    ));
    a.p.data().unit.level = 10;
    // A solo dungeon: queued, then the entry limit (3 within 4 hours) applies.
    assert!(indun::request_dungeon_instance(
        &server,
        &a.p,
        DUNGEON_ZONE,
        0
    ));
    let d = server.systems.instances.dungeon_of_world(100).unwrap();
    assert!(!d.is_team_owned());
    assert!(d.is_queued(&a.p));
    // Asking again while queued: "try later".
    a.frames();
    assert!(indun::request_dungeon_instance(
        &server,
        &a.p,
        DUNGEON_ZONE,
        0
    ));
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCErrorMsgPacket,
            body(&crate::packets::world::SCErrorMsgPacket::new(
                worlds::error_message::TRY_LATER_INSTANCE,
                0,
                true
            ))
        )]
    );
    await_loader(&d).await;
    assert_eq!(a.instance(), 100);
    // RequestDeletion: A gives up the access, nobody is left -> destroyed.
    let zone = indun::zone_by_id(&server, DUNGEON_ZONE);
    assert!(indun::request_deletion(&server, &a.p, zone.as_ref()));
    assert!(server.systems.instances.dungeon_of_world(100).is_none());
    a.frames();
    // Nothing left to unbind.
    assert!(indun::request_deletion(&server, &a.p, zone.as_ref()));
    assert_eq!(
        a.frames(),
        vec![(
            sc::SCErrorMsgPacket,
            body(&crate::packets::world::SCErrorMsgPacket::new(
                worlds::error_message::ALREADY_UNBOUND_INSTANCE,
                0,
                true
            ))
        )]
    );
    // Info tick with no instances left is a no-op.
    indun::indun_info_tick(&server);
}

#[tokio::test]
async fn system_instance_with_fixed_id_and_disconnect() {
    let server = dungeon_server();
    // `Dungeons.AutoCreate`: arche_mall_world as instance 1, channel 0.
    let d = indun::create_system_instance(&server, None, MIRAGE_ZONE, 0, true, 1)
        .expect("system instance");
    assert!(d.is_system);
    assert_eq!(d.world_id(), Some(1));
    assert_eq!(server.systems.instances.worlds.channel_id(1), 0);
    await_loader(&d).await;
    // A second request for the same zone group / channel returns the same instance.
    let again = indun::create_system_instance(&server, None, MIRAGE_ZONE, 0, true, 1).unwrap();
    assert!(Arc::ptr_eq(&d, &again));

    let mut p = Pc::new(&server, 4, "Delta", 100.0, 100.0);
    p.frames();
    let (ok, joined) = indun::request_system_instance(&server, &p.p, MIRAGE_ZONE, 0);
    assert!(ok);
    assert!(Arc::ptr_eq(&joined.unwrap(), &d));
    let expected = load_instance(
        1,
        MIRAGE_ZONE,
        300.0,
        300.0,
        50.0,
        (0.0, 0.0, worlds::deg_to_rad(200.0)),
    );
    assert!(p.frames().contains(&(sc::SCLoadInstancePacket, expected)));
    let ev = &server.systems.instances.events.characters;
    assert_eq!(ev.count(&p.p, CharEvent::Disconnect), 1);
    // System instances do not subscribe to team events.
    assert_eq!(ev.count(&p.p, CharEvent::TeamJoin), 0);
    let world = server.world.world(1).unwrap();
    assert!(worlds::has_character(&world, 4));

    // Hard disconnect: removed from the instance, unsubscribed.
    events::fire_disconnect(&server, &p.p);
    assert!(!worlds::has_character(&world, 4));
    assert_eq!(ev.count(&p.p, CharEvent::Disconnect), 0);
    assert_eq!(ev.count(&p.p, CharEvent::DungeonLeave), 0);
    // System instances survive (not expired, not destroyed by deletion requests).
    assert!(!d.is_expired());
    let zone = indun::zone_by_id(&server, MIRAGE_ZONE);
    indun::request_deletion(&server, &p.p, zone.as_ref());
    assert!(server.systems.instances.dungeon_of_world(1).is_some());
    indun::indun_info_tick(&server);
}

#[tokio::test]
async fn court_case_blocks_queueing() {
    let server = dungeon_server();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    // Without a trial the player is not in court; the check itself is TrialManager's.
    assert!(!server.systems.social.trials.is_player_in_court(1));
    assert!(indun::request_dungeon_instance(
        &server,
        &a.p,
        DUNGEON_ZONE,
        0
    ));
    a.frames();
}

// ---------------------------------------------------------------------------------------------
// Destroyed instances
// ---------------------------------------------------------------------------------------------

/// One pass of every periodic task that walks a per-instance registry (the ticks the server
/// runs on its timers), driven by hand.
fn tick_every_instance_registry(server: &GameServer) {
    use crate::systems::npcs::gimmick::manager as gimmicks;
    gimmicks::tick_all(server, Duration::from_millis(50));
    server.systems.npcs.spheres.tick(server);
    server.systems.npcs.area_triggers.tick(server);
    crate::systems::ai::collision::residency::tick(server);
    crate::systems::ai::manager::tick_all(server, 100);
    crate::systems::vehicles::transfers::transfer_tick(server);
    super::tick(server, 30);
}

/// Registers the per-instance state the game creates while an instance runs (a gimmick registry
/// is created when an NPC asks for its gimmick or one is spawned, sphere quest triggers when a
/// quest is accepted in the instance, the vehicle managers when a mate or a transfer is used).
fn populate_instance_registries(server: &GameServer, world_id: u32) {
    let npcs = &server.systems.npcs;
    npcs.gimmicks.for_world(world_id);
    npcs.spheres.for_world(world_id);
    server.systems.vehicles.mates.get(world_id);
    server.systems.vehicles.transfers.get(world_id);
    server.systems.ai.physics.world_or_create(world_id);
}

/// Bug: after dungeon instance 100 was destroyed (its players had been disconnected) the log
/// filled with `GetWorld: No such World Instance 100`, about 17 a second for hours: the gimmick
/// and sphere quest registries of the instance were never dropped, and their ticks asked for the
/// gone instance on every pass.
#[tokio::test]
async fn a_destroyed_dungeon_leaves_nothing_that_asks_for_its_instance() {
    let server = dungeon_server();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    assert_eq!(a.instance(), 100);
    populate_instance_registries(&server, 100);
    a.frames();
    // While it runs, the ticks find the instance.
    tick_every_instance_registry(&server);
    assert!(server.world.all_missing_lookups().is_empty());

    // The player's connection dies inside the dungeon: the disconnect removes the player, and
    // since the player is in no party the run is given up and the dungeon destroyed.
    events::fire_disconnect(&server, &a.p);
    assert!(server.systems.instances.dungeon_of_world(100).is_none());
    assert!(!server.world.has_world(100));

    for _ in 0..3 {
        tick_every_instance_registry(&server);
    }
    assert_eq!(
        server.world.all_missing_lookups(),
        Vec::<(u32, u64)>::new(),
        "something still asks for the destroyed instance"
    );
    let npcs = &server.systems.npcs;
    assert!(npcs.gimmicks.existing(100).is_none());
    assert!(!npcs.spheres.remove_world(100));
    assert!(!npcs.fish_schools.remove_world(100));
    assert!(!server.systems.vehicles.mates.remove_world(100));
    assert!(!server.systems.vehicles.transfers.remove_world(100));
    assert!(server.systems.ai.physics.world(100).is_none());
    assert!(server.systems.ai.collision.existing(100).is_none());
}

/// The registries are dropped on every way an instance goes away (`RemoveWorld`), and an
/// instance id that is handed out again does not inherit the old instance's registries.
#[tokio::test]
async fn a_recycled_instance_id_starts_without_the_old_instances_registries() {
    let server = dungeon_server();
    let a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    populate_instance_registries(&server, 100);
    dungeon::destroy_dungeon(&server, &d);
    assert!(!server.world.has_world(100));

    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let fresh = server.systems.instances.all_dungeons()[0].clone();
    assert_eq!(fresh.world_id(), Some(100));
    await_loader(&fresh).await;
    assert!(server.systems.npcs.gimmicks.existing(100).is_none());
    assert!(!server.systems.vehicles.mates.remove_world(100));
    tick_every_instance_registry(&server);
    assert!(server.world.all_missing_lookups().is_empty());
    dungeon::destroy_dungeon(&server, &fresh);
}

/// A lookup of an instance that does not exist is counted, and logged once per instance id (the
/// miss counter starts again when the id is handed out again).
#[tokio::test]
async fn a_lookup_of_a_missing_instance_is_counted_and_logged_once() {
    let server = dungeon_server();
    assert!(server.world.world(77).is_none());
    assert!(server.world.world(77).is_none());
    assert!(server.world.world(77).is_none());
    assert_eq!(server.world.missing_lookups(77), 3);
    assert_eq!(server.world.all_missing_lookups(), vec![(77, 3)]);
    // A periodic walk does not count as a miss.
    assert!(server.world.world_if_exists(78).is_none());
    assert_eq!(server.world.missing_lookups(78), 0);
    // The id is created: the counter restarts, and a later miss is logged again.
    let template = server
        .game_data
        .worlds
        .get_world_template_by_name("main_world")
        .unwrap();
    server
        .world
        .create_world_instance(Arc::new(template.clone()), 77);
    assert_eq!(server.world.missing_lookups(77), 0);
}

// ---------------------------------------------------------------------------------------------
// Entry refusals answer the client
// ---------------------------------------------------------------------------------------------

/// One `SCErrorMsgPacket` frame (`isNotify`), as `send_error` sends it.
fn error_frame(code: i16) -> (u16, Vec<u8>) {
    (
        sc::SCErrorMsgPacket,
        body(&crate::packets::world::SCErrorMsgPacket::new(code, 0, true)),
    )
}

fn error_frames(frames: &[(u16, Vec<u8>)]) -> Vec<(u16, Vec<u8>)> {
    frames
        .iter()
        .filter(|(op, _)| *op == sc::SCErrorMsgPacket)
        .cloned()
        .collect()
}

fn has_load_instance(frames: &[(u16, Vec<u8>)]) -> bool {
    frames.iter().any(|(op, _)| *op == sc::SCLoadInstancePacket)
}

/// The entry limit (`Dungeons.EntryLimit`: C# 3 entries within 4 hours) answers with the 1.2
/// client's `INSTANCE_VISIT_LIMIT` (453, "you have entered this instance too often") and the exact
/// bytes of the notification; the refused entry leaves no run and no extra entry behind.
#[tokio::test]
async fn an_entry_over_the_visit_limit_is_answered_with_the_visit_limit_error() {
    let data = dungeon_data();
    let server = dungeon_server_with_config(data, |c| {
        c.dungeons.entry_limit.enabled = Some(true);
        c.dungeons.entry_limit.count = 1;
    });
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    use_exit_func(&server, &a.p);
    indun::reset_dungeon_instance(&server, &a.p, DUNGEON_ZONE).unwrap();
    a.frames();
    // A new run would be a second entry within the 4 hours.
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let errors = error_frames(&a.frames());
    assert_eq!(
        errors,
        vec![(
            sc::SCErrorMsgPacket,
            vec![0xC5, 0x01, 0xC5, 0x01, 0, 0, 0, 0, 1]
        )]
    );
    assert_eq!(
        errors,
        vec![error_frame(worlds::error_message::INSTANCE_VISIT_LIMIT)]
    );
    assert!(server.systems.instances.all_dungeons().is_empty());
    assert_eq!(a.instance(), 0);
    assert_eq!(
        server.systems.instances.indun.entry_count(1, DUNGEON_GROUP),
        1
    );
}

/// The owner's EU 1.2 recollection (2026-11-10): 1.2 had no dungeon entry limit (the "5 per
/// dungeon" cap came later). Under the default `EuAuroria` profile a fourth entry within 4 hours
/// is admitted; the history still records every entry.
#[tokio::test]
async fn eu_auroria_profile_admits_a_fourth_entry_within_four_hours() {
    let server = dungeon_server();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    a.frames();
    for entry in 1..=4 {
        use_enter_func(&server, &a.p, DUNGEON_ZONE);
        assert!(error_frames(&a.frames()).is_empty(), "entry {entry}");
        let d = server.systems.instances.all_dungeons()[0].clone();
        await_loader(&d).await;
        assert_ne!(a.instance(), 0, "entry {entry}");
        use_exit_func(&server, &a.p);
        dungeon::destroy_dungeon(&server, &d);
    }
    assert_eq!(
        server.systems.instances.indun.entry_count(1, DUNGEON_GROUP),
        4
    );
}

/// The other era profiles keep the C# cap (3 entries per 4 hours).
#[tokio::test]
async fn eu_launch_profile_keeps_the_csharp_entry_limit() {
    let server = dungeon_server_with_config(dungeon_data(), |c| {
        c.world.profile = ServerProfile::EuLaunch;
        c.dungeons.entry_limit.count = 1;
    });
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    a.frames();
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    assert!(error_frames(&a.frames()).is_empty());
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    use_exit_func(&server, &a.p);
    dungeon::destroy_dungeon(&server, &d);
    a.frames();
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    assert_eq!(
        error_frames(&a.frames()),
        vec![error_frame(worlds::error_message::INSTANCE_VISIT_LIMIT)]
    );
    assert!(server.systems.instances.all_dungeons().is_empty());
    assert_eq!(
        server.systems.instances.indun.entry_count(1, DUNGEON_GROUP),
        1
    );
}

/// `Dungeons.RequireEntryItems = false`: the `indun_zones.item_id` door ticket is never asked
/// for (no economy hook is installed here, so the default configuration fails closed).
#[tokio::test]
async fn require_entry_items_false_needs_no_ticket() {
    let mut data = dungeon_data();
    data.systems
        .instances
        .indun
        .zones
        .get_mut(&DUNGEON_GROUP)
        .unwrap()
        .item_id = 123;
    let server = dungeon_server_with_config(data, |c| {
        c.dungeons.require_entry_items = false;
    });
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    a.frames();
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    assert!(error_frames(&a.frames()).is_empty());
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    assert_ne!(a.instance(), 0);
    assert_eq!(
        server.systems.instances.indun.entry_count(1, DUNGEON_GROUP),
        1
    );
    dungeon::destroy_dungeon(&server, &d);
}

fn ticket_dungeon() -> (GameData, std::sync::Arc<DungeonItems>) {
    let mut data = dungeon_data();
    {
        let zone = data
            .systems
            .instances
            .indun
            .zones
            .get_mut(&DUNGEON_GROUP)
            .unwrap();
        zone.item_id = 123;
        zone.restore_item_time = 3600;
    }
    (data, std::sync::Arc::new(DungeonItems::default()))
}

/// `indun_zones.restore_item_time` (3600 s on the real Serpentis row): re-entering the same zone
/// group within the window after leaving a run that consumed the ticket consumes no second ticket.
#[tokio::test]
async fn reentry_within_restore_item_time_does_not_consume_another_ticket() {
    let (data, items) = ticket_dungeon();
    let server = dungeon_server_with(data);
    server.systems.npcs.hooks.economy.install(items.clone());
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    a.frames();
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    assert_eq!(items.charges.load(std::sync::atomic::Ordering::Relaxed), 1);
    use_exit_func(&server, &a.p);
    dungeon::destroy_dungeon(&server, &d);
    a.frames();
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    assert!(error_frames(&a.frames()).is_empty());
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    assert_ne!(a.instance(), 0);
    assert_eq!(items.charges.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(
        server.systems.instances.indun.entry_count(1, DUNGEON_GROUP),
        2
    );
    dungeon::destroy_dungeon(&server, &d);
}

/// After `restore_item_time` has passed, the next entry consumes a ticket again.
#[tokio::test]
async fn after_restore_item_time_a_new_ticket_is_consumed() {
    let (data, items) = ticket_dungeon();
    let server = dungeon_server_with(data);
    server.systems.npcs.hooks.economy.install(items.clone());
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    a.frames();
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    use_exit_func(&server, &a.p);
    dungeon::destroy_dungeon(&server, &d);
    server
        .systems
        .instances
        .indun
        .backdate_entry_item_leave(1, DUNGEON_GROUP, 3601);
    a.frames();
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    assert!(error_frames(&a.frames()).is_empty());
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    assert_eq!(items.charges.load(std::sync::atomic::Ordering::Relaxed), 2);
    dungeon::destroy_dungeon(&server, &d);
}

/// One ticket covers one paid run and exactly one free return: the third run within the window
/// charges again (a reused free return must not restart the window).
#[tokio::test]
async fn one_ticket_covers_at_most_one_free_return() {
    let (data, items) = ticket_dungeon();
    let server = dungeon_server_with(data);
    server.systems.npcs.hooks.economy.install(items.clone());
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    a.frames();
    for run in 1..=2 {
        use_enter_func(&server, &a.p, DUNGEON_ZONE);
        assert!(error_frames(&a.frames()).is_empty(), "run {run}");
        let d = server.systems.instances.all_dungeons()[0].clone();
        await_loader(&d).await;
        use_exit_func(&server, &a.p);
        dungeon::destroy_dungeon(&server, &d);
        assert_eq!(
            items.charges.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "run {run}"
        );
    }
    // Run 3 is still inside restore_item_time, but the ticket's free return was used.
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    assert!(error_frames(&a.frames()).is_empty());
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    assert_ne!(a.instance(), 0);
    assert_eq!(items.charges.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert_eq!(
        server.systems.instances.indun.entry_count(1, DUNGEON_GROUP),
        3
    );
    dungeon::destroy_dungeon(&server, &d);
}

/// A run destroyed while the player is still queued (the loader never delivers them) must not
/// swallow the ticket: the retry within the restore window is free.
#[tokio::test]
async fn a_run_destroyed_before_arrival_does_not_swallow_the_ticket() {
    let (data, items) = ticket_dungeon();
    let server = dungeon_server_with(data);
    server.systems.npcs.hooks.economy.install(items.clone());
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    a.frames();
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    assert_eq!(items.charges.load(std::sync::atomic::Ordering::Relaxed), 1);
    let d = server.systems.instances.all_dungeons()[0].clone();
    // The loader sleeps 100 ms before it runs; destroy the run before it arrives.
    dungeon::destroy_dungeon(&server, &d);
    assert_eq!(a.instance(), 0);
    a.frames();
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    assert!(error_frames(&a.frames()).is_empty());
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    assert_ne!(a.instance(), 0);
    assert_eq!(items.charges.load(std::sync::atomic::Ordering::Relaxed), 1);
    dungeon::destroy_dungeon(&server, &d);
}

/// Every refusal of a dungeon request carries an error for the client: unknown zone, a zone that
/// is no dungeon, a dungeon without zone keys (the server cannot build it), a dungeon that needs a
/// party, and a full party run.
#[tokio::test]
async fn every_refused_dungeon_request_is_answered() {
    let server = dungeon_server();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    a.frames();
    for zone_id in [999, TEST_ZONE] {
        assert!(!indun::request_dungeon_instance(&server, &a.p, zone_id, 0));
        assert_eq!(
            a.frames(),
            vec![error_frame(
                worlds::error_message::NO_SERVER_INSTANCE_RESOURCE
            )],
            "zone {zone_id}"
        );
    }

    // A dungeon whose zone group has no zone keys cannot be created.
    let mut data = dungeon_data();
    data.systems
        .instances
        .zone_order
        .zones
        .retain(|(_, group)| *group != DUNGEON_GROUP);
    let server = dungeon_server_with(data);
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    a.frames();
    assert!(!indun::request_dungeon_instance(
        &server,
        &a.p,
        DUNGEON_ZONE,
        0
    ));
    assert_eq!(
        a.frames(),
        vec![error_frame(
            worlds::error_message::NO_SERVER_INSTANCE_RESOURCE
        )]
    );
    assert!(server.systems.instances.all_dungeons().is_empty());

    // A party dungeon without a party, then a full party run (one seat).
    let mut data = dungeon_data();
    {
        let zone = data
            .systems
            .instances
            .indun
            .zones
            .get_mut(&DUNGEON_GROUP)
            .unwrap();
        zone.party_only = true;
        zone.max_players = 1;
    }
    let server = dungeon_server_with(data);
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    let mut b = Pc::new(&server, 2, "Beta", 100.0, 100.0);
    a.frames();
    b.frames();
    assert!(!indun::request_dungeon_instance(
        &server,
        &a.p,
        DUNGEON_ZONE,
        0
    ));
    assert_eq!(
        a.frames(),
        vec![error_frame(worlds::error_message::NEED_PARTY)]
    );
    form_party(&server, &a, &b);
    a.frames();
    b.frames();
    assert!(indun::request_dungeon_instance(
        &server,
        &a.p,
        DUNGEON_ZONE,
        0
    ));
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    assert!(!indun::request_dungeon_instance(
        &server,
        &b.p,
        DUNGEON_ZONE,
        0
    ));
    assert_eq!(
        error_frames(&b.frames()),
        vec![error_frame(worlds::error_message::INSTANCE_QUOTA)]
    );
    dungeon::destroy_dungeon(&server, &d);
}

// ---------------------------------------------------------------------------------------------
// A registration in the dungeon that the character no longer stands behind
// ---------------------------------------------------------------------------------------------

/// A character the dungeon's character table lists while its own transform stands in the main
/// world (the registration outlived the move out of the dungeon) pressed the door for ever: the
/// request found the member and returned `true` without sending a thing. The C# adds the member
/// to the dungeon again; so does the port now, without charging a second entry.
#[tokio::test]
async fn a_stale_dungeon_registration_does_not_swallow_the_door() {
    let server = dungeon_server();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    let world_id = d.world_id().unwrap();
    assert_eq!(a.instance(), world_id);
    assert_eq!(
        server.systems.instances.indun.entry_count(1, DUNGEON_GROUP),
        1
    );
    // The leftover: the character is back in the main world for the client and the server's
    // object, but the dungeon's table still lists it.
    a.p.data().unit.transform.instance_id = 0;
    let world = server.world.world(world_id).unwrap();
    assert!(worlds::has_character(&world, 1));
    a.frames();

    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    assert_eq!(a.instance(), world_id, "the door moved the character again");
    assert_eq!(world.all_characters().len(), 1);
    let frames = a.frames();
    assert!(has_load_instance(&frames));
    assert!(error_frames(&frames).is_empty());
    assert_eq!(
        server.systems.instances.indun.entry_count(1, DUNGEON_GROUP),
        1
    );
    dungeon::destroy_dungeon(&server, &d);
}

/// A character really standing in the dungeon whose client asks for the door (it believes it is
/// outside) is put back at the entrance with one event subscription per event, not two.
#[tokio::test]
async fn a_member_inside_who_asks_for_the_door_is_moved_in_again_once() {
    let server = dungeon_server();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    a.frames();
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let frames = a.frames();
    let expected = load_instance(
        d.world_id().unwrap(),
        DUNGEON_ZONE,
        DUNGEON_SPAWN.x,
        DUNGEON_SPAWN.y,
        DUNGEON_SPAWN.z,
        (
            worlds::deg_to_rad(DUNGEON_SPAWN.roll),
            worlds::deg_to_rad(DUNGEON_SPAWN.pitch),
            worlds::deg_to_rad(DUNGEON_SPAWN.yaw),
        ),
    );
    assert!(frames.contains(&(sc::SCLoadInstancePacket, expected)));
    let ev = &server.systems.instances.events.characters;
    assert_eq!(ev.count(&a.p, CharEvent::Disconnect), 1);
    assert_eq!(ev.count(&a.p, CharEvent::DungeonLeave), 1);
    assert_eq!(ev.count(&a.p, CharEvent::TeamLeave), 1);
    dungeon::destroy_dungeon(&server, &d);
}

/// The registration of an earlier session of the same character (a reconnect before the old
/// session was cleaned up) is dropped; the live character enters like a returning member.
#[tokio::test]
async fn the_registration_of_an_earlier_session_does_not_block_the_new_one() {
    let server = dungeon_server();
    let old = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    use_enter_func(&server, &old.p, DUNGEON_ZONE);
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    let world_id = d.world_id().unwrap();
    // The old session's connection is gone but its character is still listed in the dungeon.
    let mut new = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    assert_ne!(old.p.obj_id, new.p.obj_id);
    new.frames();
    use_enter_func(&server, &new.p, DUNGEON_ZONE);
    let world = server.world.world(world_id).unwrap();
    let listed = world.all_characters();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].obj_id, new.p.obj_id);
    assert_eq!(new.instance(), world_id);
    assert!(has_load_instance(&new.frames()));
    assert_eq!(
        server.systems.instances.indun.entry_count(1, DUNGEON_GROUP),
        1
    );
    dungeon::destroy_dungeon(&server, &d);
}

/// The exit door of a character no instance lists used to do nothing at all. A character whose
/// transform stands in an instance is returned to the main world; one that does not gets the
/// "invalid return position" error.
#[tokio::test]
async fn an_exit_door_answers_a_character_no_instance_lists() {
    let server = dungeon_server();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    use_enter_func(&server, &a.p, DUNGEON_ZONE);
    let d = server.systems.instances.all_dungeons()[0].clone();
    await_loader(&d).await;
    let world_id = d.world_id().unwrap();
    assert_eq!(a.instance(), world_id);
    // The registration is gone while the transform still says "in the dungeon".
    let world = server.world.world(world_id).unwrap();
    world.remove_object(a.p.obj_id);
    assert!(!worlds::has_character(&world, 1));
    a.frames();
    use_exit_func(&server, &a.p);
    assert_eq!(a.instance(), 0);
    assert!(has_load_instance(&a.frames()));
    // Listed nowhere and standing in the main world, the door has nothing to leave: it says so.
    server.world.world(0).unwrap().remove_object(a.p.obj_id);
    a.frames();
    use_exit_func(&server, &a.p);
    assert_eq!(
        a.frames(),
        vec![error_frame(
            worlds::error_message::INVALID_RETURN_POS_INSTANCE
        )]
    );
    dungeon::destroy_dungeon(&server, &d);
}

/// A use of an instance door that meets another thread's phase reservation was dropped without a
/// word (the C# waited for the lock). The player is told to try again; other doodads stay quiet.
#[tokio::test]
async fn a_busy_instance_door_tells_the_player_to_try_again() {
    use crate::systems::npcs::doodad::templates::DoodadFunc;
    let mut data = dungeon_data();
    for (group, kind) in [(77, "DoodadFuncEnterInstance"), (78, "DoodadFuncFakeUse")] {
        data.systems
            .npcs
            .doodad
            .funcs_by_groups
            .entry(group)
            .or_default()
            .push(DoodadFunc {
                group_id: group,
                func_key: group,
                func_type: kind.into(),
                skill_id: 18701,
                ..DoodadFunc::default()
            });
    }
    let server = dungeon_server_with(data);
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    a.frames();
    let other_thread = std::thread::spawn(|| std::thread::current().id())
        .join()
        .unwrap();
    for (group, answered) in [(77u32, true), (78u32, false)] {
        let world = server.world.world(0).unwrap();
        let doodad = Entity::new(0x9100 + group, Doodad::new(1));
        {
            let mut d = doodad.data();
            d.func_group_id = group;
            d.phase.operation_thread = Some(other_thread);
        }
        let ctx = DoodadCtx::new(&server, world, doodad);
        crate::systems::npcs::doodad::phase::use_doodad(
            &ctx,
            Some(&WorldObject::from(&a.p)),
            18701,
            0,
        );
        let frames = a.frames();
        if answered {
            assert_eq!(
                frames,
                vec![error_frame(worlds::error_message::TRY_LATER_INSTANCE)]
            );
        } else {
            assert!(frames.is_empty(), "{frames:?}");
        }
        // The reservation of the other thread is untouched.
        assert_eq!(ctx.doodad.data().phase.operation_thread, Some(other_thread));
    }
}

// ---------------------------------------------------------------------------------------------
// Battlefields
// ---------------------------------------------------------------------------------------------

fn camp_server(corps_size: u32, victory_score: i32) -> Arc<GameServer> {
    camp_server_kills(corps_size, victory_score, 0)
}

fn camp_server_kills(
    corps_size: u32,
    victory_score: i32,
    victory_kill_count: u32,
) -> Arc<GameServer> {
    camp_server_with(corps_size, victory_score, victory_kill_count, |_| {})
}

fn camp_server_with(
    corps_size: u32,
    victory_score: i32,
    victory_kill_count: u32,
    tweak: impl FnOnce(&mut GameRuleSet),
) -> Arc<GameServer> {
    camp_server_build(
        corps_size,
        victory_score,
        victory_kill_count,
        30,
        2,
        false,
        7,
        tweak,
    )
}

/// A Drill Camp server whose `game_score_rules` author `kill_score` / `assist_score` (the
/// shipped rule sets 8/17/22/26/27 author 3 / 2; rule set 28 authors 30 for kills).
fn camp_server_with_score(
    corps_size: u32,
    victory_score: i32,
    kill_score: i32,
    assist_score: i32,
    tweak: impl FnOnce(&mut GameRuleSet),
) -> Arc<GameServer> {
    camp_server_build(
        corps_size,
        victory_score,
        0,
        kill_score,
        assist_score,
        false,
        7,
        tweak,
    )
}

/// [`camp_server`] in the arena ranking board's zone group (77) with the gladiator duel's
/// authored points (win 4, lose 1).
fn camp_server_ranked(corps_size: u32, victory_score: i32) -> Arc<GameServer> {
    camp_server_build(corps_size, victory_score, 0, 30, 2, true, 7, |r| {
        r.rank_win_point = 4;
        r.rank_lose_point = 1;
    })
}

#[allow(clippy::too_many_arguments)]
fn camp_server_build(
    corps_size: u32,
    victory_score: i32,
    victory_kill_count: u32,
    kill_score: i32,
    assist_score: i32,
    ranked: bool,
    battlefield_id: u32,
    tweak: impl FnOnce(&mut GameRuleSet),
) -> Arc<GameServer> {
    let mut data = GameData::empty();
    let camp_group = if ranked {
        crate::systems::social::ranking_flow_tests::ARENA_ZONE_GROUP
    } else {
        31
    };
    data.zones = zones(&[
        zone(TEST_ZONE, TEST_ZONE, 1),
        zone(CAMP_ZONE, CAMP_ZONE, camp_group),
    ]);
    if ranked {
        data.systems.social.ranking = crate::systems::social::ranking_flow_tests::ranking_data();
    }
    let point = |x: f32, y: f32| Point {
        zone_id: CAMP_ZONE,
        x,
        y,
        z: 199.0,
        ..Point::default()
    };
    let mut rule_set = GameRuleSet {
        id: 22,
        corps_size,
        corps1_faction_id: 159,
        corps2_faction_id: 160,
        time_opening: 4,
        time_ending: 0,
        time_playing: 15,
        victory_score,
        battlefield_id,
        victory_kill_count,
        killstreak_skills: [0; 24],
        ..GameRuleSet::default()
    };
    tweak(&mut rule_set);
    // One battlefield buff for the grant / removal flow (the real 1.2 row is battlefield 11 /
    // buff 8246, value 1; the template is synthetic here).
    data.systems
        .skills
        .add_buff(crate::systems::skills::data::BuffTemplate {
            id: 8246,
            duration: 0,
            ..Default::default()
        });
    data.systems
        .instances
        .battlefields
        .battlefield_buffs
        .insert(
            battlefield_id,
            vec![BattleFieldBuff {
                id: 27,
                buff_id: 8246,
                value: 1,
            }],
        );
    // The synthetic rule set scores 30 per kill (the Gladiator row) so the flow tests can
    // reach the 60-point victory threshold; the shipped Drill Camp rows are checked by the
    // data tests.
    data.systems.instances.battlefields.score_rules.insert(
        rule_set.id,
        vec![
            GameScoreRule {
                rule_set_corps: SCORE_RULE_ANY_CORPS,
                event_id: game_score_event::KILL_ENEMY_UNIT_FINAL_HITTER,
                event_value: 0,
                event_score: kill_score,
                event_tag_id: 0,
            },
            GameScoreRule {
                rule_set_corps: SCORE_RULE_ANY_CORPS,
                event_id: game_score_event::KILL_ENEMY_UNIT_ASSIST_DEALER1,
                event_value: 0,
                event_score: assist_score,
                event_tag_id: 0,
            },
        ],
    );
    data.systems.instances.battlefields.battlefields = HashMap::from([(
        battlefield_id,
        Battlefield {
            id: battlefield_id,
            zone_key: CAMP_ZONE,
            spawns: Some(BattlefieldSpawns {
                battlefield_id,
                corps1_spawn: Some(point(1101.1, 401.5)),
                corps2_spawn: Some(point(1229.0, 530.0)),
            }),
            rule_set: Some(rule_set),
        },
    )]);
    build_server(
        data,
        vec![WorldTemplate::synthetic(
            3,
            "instance_training_camp",
            2,
            2,
            CAMP_ZONE,
        )],
        Vec::new(),
        None,
    )
}

#[tokio::test]
async fn battlefield_queue_match_join_score_and_end() {
    let server = camp_server(1, 60);
    let mut e = Pc::new(&server, 5, "Echo", 100.0, 100.0);
    let mut f = Pc::new(&server, 6, "Foxtrot", 120.0, 100.0);
    e.frames();
    f.frames();

    // ---- Both apply (CSApplyToInstantGame) and get SCAppliedToInstantGame.
    e.frame(&server, cs::CSApplyToInstantGamePacket, &[7, 0, 0, 0, 2])
        .await;
    f.frame(&server, cs::CSApplyToInstantGamePacket, &[7, 0, 0, 0, 2])
        .await;
    let applied = body(&SCAppliedToInstantGamePacket {
        battlefield_id: 7,
        corps: corps::ANY,
        error_message_id: 0,
    });
    // Per-corps size 1; both applied as "any corps", so only the any-count grows.
    let state = |any_count| {
        body(&SCInstantGameStatePacket {
            battlefield_id: 7,
            corps_size: 1,
            corps1_count: 0,
            corps2_count: 0,
            any_count,
        })
    };
    assert_eq!(
        e.frames(),
        vec![
            (sc::SCAppliedToInstantGamePacket, applied.clone()),
            (sc::SCInstantGameStatePacket, state(1)),
            (sc::SCInstantGameStatePacket, state(2)),
        ]
    );
    assert_eq!(
        f.frames(),
        vec![
            (sc::SCAppliedToInstantGamePacket, applied),
            (sc::SCInstantGameStatePacket, state(2)),
        ]
    );
    // Applying twice is ignored; unknown battlefields are refused.
    instant_game::apply_to_battlefield(&server, 7, corps::ANY, &e.p);
    instant_game::apply_to_battlefield(&server, 99, corps::ANY, &e.p);
    assert!(e.frames().is_empty());
    let mgr = &server.systems.instances.instant_games;
    assert_eq!(mgr.queued(7), Some(vec![5, 6]));

    // ---- Matchmaking: one game (instance 100), E -> corps 2, F -> corps 1.
    instant_game::battlefield_tick(&server);
    assert_eq!(mgr.games().len(), 1);
    assert_eq!(mgr.queued(7), Some(vec![]));
    let game = mgr.games()[0].clone();
    assert!(game.is_full());
    assert_eq!(game.world_id, 100);
    let zid = ZoneInstanceId {
        zone_id: CAMP_ZONE,
        instance_id: 100,
    };
    let invite = |c| {
        body(&SCInviteToInstantGamePacket {
            zone_instance_id: zid,
            ruleset_id: 22,
            corps: c,
            qualified_id: 0,
        })
    };
    let e_invite = e.frames();
    let f_invite = f.frames();
    assert_eq!(e_invite.len(), 1);
    assert_eq!(f_invite.len(), 1);
    assert_eq!(e_invite[0].0, sc::SCInviteToInstantGamePacket);
    assert_eq!(f_invite[0].0, sc::SCInviteToInstantGamePacket);
    // The invitation body is unchanged up to the qualified id, which is per invitation.
    assert_eq!(&e_invite[0].1[..13], &invite(corps::CORPS2)[..13]);
    assert_eq!(&f_invite[0].1[..13], &invite(corps::CORPS1)[..13]);
    let e_id = invite_qualified_id(&e_invite);
    let f_id = invite_qualified_id(&f_invite);
    assert_ne!(e_id, 0);
    assert_ne!(e_id, f_id, "each invitation carries its own id");

    // ---- Both accept (CSJoinInstantGame) and are moved to their corps spawn.
    let mut e_join = vec![1];
    e_join.extend_from_slice(&e_id.to_le_bytes());
    e.frame(&server, cs::CSJoinInstantGamePacket, &e_join).await;
    let mut f_join = vec![1];
    f_join.extend_from_slice(&f_id.to_le_bytes());
    f.frame(&server, cs::CSJoinInstantGamePacket, &f_join).await;
    assert!(e.frames().contains(&(
        sc::SCLoadInstancePacket,
        load_instance(100, CAMP_ZONE, 1229.0, 530.0, 199.0, (0.0, 0.0, 0.0))
    )));
    assert!(f.frames().contains(&(
        sc::SCLoadInstancePacket,
        load_instance(100, CAMP_ZONE, 1101.1, 401.5, 199.0, (0.0, 0.0, 0.0))
    )));
    assert_eq!(e.instance(), 100);
    assert!(e.p.data().main_world_position.is_some());

    // ---- Entered: joined packet + corps faction; the second entry starts the game.
    e.frame(
        &server,
        cs::CSEnteredInstantGameWorldPacket,
        &e_id.to_le_bytes(),
    )
    .await;
    let ef = e.frames();
    assert_eq!(ef[0].0, sc::SCInstantGameJoinedPacket);
    assert_eq!(
        ef[0].1,
        body(&SCInstantGameJoinedPacket {
            zone_instance_id: zid,
            corps: corps::CORPS2,
            rule_set: game.rule_set
        })
    );
    assert_eq!(e.p.data().unit.faction_id, 160);
    assert_eq!(e.p.data().unit.origin_faction_id, 101);
    f.frame(
        &server,
        cs::CSEnteredInstantGameWorldPacket,
        &f_id.to_le_bytes(),
    )
    .await;
    let ff = f.frames();
    assert!(ff.iter().any(|(op, _)| *op == sc::SCInstantGameStartPacket));
    assert!(e
        .frames()
        .iter()
        .any(|(op, _)| *op == sc::SCInstantGameStartPacket));
    assert_eq!(f.p.data().unit.faction_id, 159);

    // ---- E kills F: kill packet, killstreak, 30 points (victory at 60).
    events::on_do_die(
        &server,
        &WorldObject::from(&f.p),
        Some(&WorldObject::from(&e.p)),
    );
    let kill = body(&SCInstantGameKillPacket {
        zone_instance_id: zid,
        killer_id: 5,
        victim_id: 6,
        killer_corps: corps::CORPS2,
        victim_corps: corps::CORPS1,
        killer_killstreak: 1,
        killer_name: "Echo".into(),
        victim_name: "Foxtrot".into(),
        killer_corps_kills: 1,
        victim_corps_deaths: 0,
    });
    // Echo is corps 2: the packet names corps 2 and carries both totals.
    let point = |total| {
        body(&SCInstantGameAddPointPacket {
            zone_instance_id: zid,
            corps: corps::CORPS2,
            points: 30,
            score1: 0,
            score2: total,
            char_name: "Echo".into(),
        })
    };
    let streak = |n| {
        body(&SCInstantGameKillstreakPacket {
            zone_instance_id: zid,
            killstreak: n,
            skill_id: 0,
            enabled: true,
        })
    };
    assert_eq!(
        e.frames(),
        vec![
            (sc::SCInstantGameKillPacket, kill.clone()),
            (sc::SCInstantGameKillstreakPacket, streak(1)),
            (sc::SCInstantGameAddPointPacket, point(30)),
        ]
    );
    assert_eq!(
        f.frames(),
        vec![
            (sc::SCInstantGameKillPacket, kill),
            (sc::SCInstantGameAddPointPacket, point(30)),
        ]
    );

    // ---- Second kill: 60 points -> corps 2 wins, the result is sent, the game ends.
    events::on_do_die(
        &server,
        &WorldObject::from(&f.p),
        Some(&WorldObject::from(&e.p)),
    );
    let ef = e.frames();
    let end = ef
        .iter()
        .find(|(op, _)| *op == sc::SCInstantGameEndPacket)
        .expect("end packet");
    let mut s = PacketStream::new();
    s.write_u32(CAMP_ZONE).write_u32(100).write_u8(8);
    // corps 1 (F): lose, 0 points, 0 kills, 2 deaths
    s.write_u8(1)
        .write_u32(159)
        .write_i32(0)
        .write_i16(0)
        .write_i16(2)
        .write_u8(1);
    s.write_u32(6)
        .write_str("Foxtrot")
        .write_i32(0)
        .write_i32(0)
        // Bonus set: loser only (2 deaths, no kills).
        .write_u32(instant_game::bonus_set::LOSER)
        .write_u16(0)
        .write_u16(0)
        .write_u16(0)
        .write_u16(2);
    // corps 2 (E): win, 60 points, 2 kills
    s.write_u8(2)
        .write_u32(160)
        .write_i32(60)
        .write_i16(2)
        .write_i16(0)
        .write_u8(1);
    s.write_u32(5)
        .write_str("Echo")
        .write_i32(60)
        .write_i32(0)
        // Bonus set: victory + no death + top PC kill.
        .write_u32(
            instant_game::bonus_set::VICTORY
                | instant_game::bonus_set::NO_DEATH
                | instant_game::bonus_set::TOP_PC_KILL,
        )
        .write_u16(2)
        .write_u16(2)
        .write_u16(0)
        .write_u16(0);
    assert_eq!(end.1, s.into_bytes());
    // TimeEnding is 0: the game is destroyed right away, the players go back.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(mgr.games().is_empty());
    assert!(!server.world.has_world(100));
    assert_eq!(e.instance(), 0);
    assert_eq!(e.p.data().unit.faction_id, 101);
    assert!(mgr.current(&e.p).is_none());
    assert!(e
        .frames()
        .iter()
        .any(|(op, _)| *op == sc::SCLoadInstancePacket));
    assert_eq!(
        server
            .systems
            .instances
            .events
            .characters
            .count(&e.p, CharEvent::Kill),
        0
    );
}

#[tokio::test]
async fn battlefield_decline_requeues() {
    let server = camp_server(1, 60);
    let mut h = Pc::new(&server, 7, "Hotel", 100.0, 100.0);
    instant_game::apply_to_battlefield(&server, 7, corps::ANY, &h.p);
    instant_game::battlefield_tick(&server);
    let mgr = &server.systems.instances.instant_games;
    let game = mgr.games()[0].clone();
    // Not full (drill camp): kept in the queue list.
    assert!(!game.is_full());
    assert_eq!(mgr.queue_list().len(), 1);
    let invite_id = invite_qualified_id(&h.frames());
    // Decline: removed from the game, applied again (no Cancel: that would reset the 1.2
    // client to not-applied).
    let mut decline = vec![0];
    decline.extend_from_slice(&invite_id.to_le_bytes());
    h.frame(&server, cs::CSJoinInstantGamePacket, &decline)
        .await;
    assert!(!game.has_player(&h.p));
    assert_eq!(mgr.queued(7), Some(vec![7]));
    assert_eq!(
        h.ops(),
        vec![
            sc::SCAppliedToInstantGamePacket,
            sc::SCInstantGameStatePacket
        ]
    );
    // The next tick refills the waiting game.
    instant_game::battlefield_tick(&server);
    assert!(game.has_player(&h.p));
    assert_eq!(mgr.games().len(), 1);
    // Cancel: withdraw from the game.
    h.frame(&server, cs::CSCancelInstantGamePacket, &[]).await;
    assert!(!game.has_player(&h.p));
    assert!(!instant_game::player_can_enter(&server, &h.p) || h.instance() == 0);
}

/// `SCInviteToInstantGame.qualified_id` is per game + invitation: a join / entered-world
/// response that echoes another value is stale and ignored.
#[tokio::test]
async fn battlefield_stale_qualified_ids_are_ignored() {
    let server = camp_server(1, 60);
    let mut p = Pc::new(&server, 7, "Invited", 100.0, 100.0);
    instant_game::apply_to_battlefield(&server, 7, corps::ANY, &p.p);
    instant_game::battlefield_tick(&server);
    let game = server.systems.instances.instant_games.games()[0].clone();
    let invite_id = invite_qualified_id(&p.frames());
    let stale_id = invite_id.wrapping_add(1);
    let mut stale = vec![1];
    stale.extend_from_slice(&stale_id.to_le_bytes());

    p.frame(&server, cs::CSJoinInstantGamePacket, &stale).await;
    assert!(p.frames().is_empty(), "a stale join is ignored");
    assert_eq!(p.instance(), 0);

    let mut join = vec![1];
    join.extend_from_slice(&invite_id.to_le_bytes());
    p.frame(&server, cs::CSJoinInstantGamePacket, &join).await;
    assert_eq!(p.instance(), game.world_id);
    p.frames();

    p.frame(
        &server,
        cs::CSEnteredInstantGameWorldPacket,
        &stale_id.to_le_bytes(),
    )
    .await;
    assert!(p.frames().is_empty(), "a stale entered-world is ignored");
    assert!(game.st().members.is_empty());

    p.frame(
        &server,
        cs::CSEnteredInstantGameWorldPacket,
        &invite_id.to_le_bytes(),
    )
    .await;
    assert!(p.ops().contains(&sc::SCInstantGameJoinedPacket));
    assert_eq!(game.st().members.len(), 1);
}

/// Declining replaces the invitation: the previous id is stale and ignored.
#[tokio::test]
async fn battlefield_reinvite_replaces_the_qualified_id() {
    let server = camp_server(1, 60);
    let mut p = Pc::new(&server, 7, "Reinvited", 100.0, 100.0);
    instant_game::apply_to_battlefield(&server, 7, corps::ANY, &p.p);
    instant_game::battlefield_tick(&server);
    let game = server.systems.instances.instant_games.games()[0].clone();
    let first = invite_qualified_id(&p.frames());
    let mut decline = vec![0];
    decline.extend_from_slice(&first.to_le_bytes());
    p.frame(&server, cs::CSJoinInstantGamePacket, &decline)
        .await;
    p.frames();
    instant_game::battlefield_tick(&server);
    let second = invite_qualified_id(&p.frames());
    assert_ne!(first, second);

    let mut stale = vec![1];
    stale.extend_from_slice(&first.to_le_bytes());
    p.frame(&server, cs::CSJoinInstantGamePacket, &stale).await;
    assert!(p.frames().is_empty(), "the previous invitation is stale");
    assert_eq!(p.instance(), 0);

    let mut join = vec![1];
    join.extend_from_slice(&second.to_le_bytes());
    p.frame(&server, cs::CSJoinInstantGamePacket, &join).await;
    assert_eq!(p.instance(), game.world_id);
}

/// `PlayerCanEnter` now gates `ApplyToBattlefield`: a duelling, seated-jury or jailed
/// character cannot queue (the C# had the checks but never called them).
#[tokio::test]
async fn battlefield_apply_refuses_a_duel_jury_or_jail() {
    use crate::systems::social::crime::buff;

    let server = camp_server(1, 60);
    let mut duelist = Pc::new(&server, 7, "Duelist", 100.0, 100.0);
    let opponent = Pc::new(&server, 8, "Opponent", 110.0, 100.0);
    server
        .systems
        .social
        .duels
        .duel_request(&server, &duelist.p, opponent.p.character_id);
    assert!(!instant_game::player_can_enter(&server, &duelist.p));
    assert!(!instant_game::player_can_enter(&server, &opponent.p));
    duelist.frames();
    instant_game::apply_to_battlefield(&server, 7, corps::ANY, &duelist.p);
    assert_eq!(server.systems.instances.instant_games.queued(7), None);
    assert!(duelist.frames().is_empty());

    // A seated juror (`JURY` 3621) and the jail states of the crime system.
    for (id, buff_id) in [
        (9, buff::JURY),
        (10, buff::FORCIBLY_AWAITING_TRIAL),
        (11, buff::PRISONER_NUIAN),
        (12, buff::PRISONER_HARANYAN),
    ] {
        let mut pc = Pc::new(&server, id, &format!("Held{id}"), 100.0, 100.0);
        pc.frames();
        assert!(instant_game::player_can_enter(&server, &pc.p));
        {
            let mut c = pc.p.data();
            c.unit.buffs.add_buff(
                crate::models::buff::Buff::new(
                    BuffTemplate::simple(buff_id, BuffKind::Good),
                    crate::models::buff::SkillCaster::default(),
                    None,
                    0,
                ),
                0,
                0,
                0,
            );
        }
        assert!(
            !instant_game::player_can_enter(&server, &pc.p),
            "buff {buff_id}"
        );
        instant_game::apply_to_battlefield(&server, 7, corps::ANY, &pc.p);
        assert_eq!(
            server.systems.instances.instant_games.queued(7),
            None,
            "buff {buff_id}"
        );
        assert!(pc.frames().is_empty(), "buff {buff_id}");
    }
}

/// A kill teleport restarts the victim's (and the Gladiator killer's) ZWUnitFell tracker at
/// the spawn, so a fall from before the death cannot deal damage after the respawn.
#[tokio::test(start_paused = true)]
async fn battlefield_kill_teleport_resets_fall_tracking() {
    use crate::systems::ai::collision::fall::zone_fall_vel;

    let server = camp_server_build(1, 60, 0, 30, 2, false, 11, |r| {
        r.id = 28;
        r.time_resurrection_delay = 1;
    });
    let (a, mut b, game) = started_1v1_on(&server, 11).await;
    assert_eq!(game.battlefield.id, instant_game::game_type::GLADIATOR);
    let fall = &server.systems.ai.collision.fall;
    // Both units were falling from high above before the kill.
    for p in [&a.p, &b.p] {
        assert_eq!(fall.sample(p.obj_id, 1000.0, Some(0.0), 0), None);
        assert!(fall.state(p.obj_id).unwrap().airborne);
    }
    b.frames();
    events::on_do_die(
        &server,
        &WorldObject::from(&b.p),
        Some(&WorldObject::from(&a.p)),
    );
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert!(b
        .frames()
        .iter()
        .any(|(op, _)| *op == sc::SCCharacterResurrectedPacket));
    // The victim and the Gladiator killer respawn at their corps spawn (z 199 in this fixture).
    for p in [&a.p, &b.p] {
        let state = fall.state(p.obj_id).expect("fall state");
        assert!(!state.airborne, "the teleport restarts the fall grounded");
        assert_eq!(state.max_down_vel, 0.0);
        assert_eq!(state.start_z, 199.0, "restarted at the spawn z");
        assert_eq!(fall.sample(p.obj_id, 199.0, Some(199.0), 2000), None);
        // The zone gate cannot see the old fall any more: no ZWUnitFell, no fall damage.
        assert_eq!(zone_fall_vel(32000, state.start_z, 199.0), None);
    }
}

/// Ending a game unbinds the members' `OnKill` handlers even when a member has no origin
/// faction (the `LeaveInstantGame` faction check returns early): removal itself unbinds.
#[tokio::test]
async fn battlefield_end_unbinds_kill_events_without_an_origin_faction() {
    let server = camp_server(1, 60);
    let mut p = Pc::new(&server, 7, "Allied", 100.0, 100.0);
    // The drill camp's first player takes corps 2 (faction 160): give that faction up front,
    // so entering makes no temporary change and the character has no origin faction.
    p.p.data().unit.faction_id = 160;
    instant_game::apply_to_battlefield(&server, 7, corps::ANY, &p.p);
    instant_game::battlefield_tick(&server);
    let game = server.systems.instances.instant_games.games()[0].clone();
    let id = invite_qualified_id(&p.frames());
    let mut join = vec![1];
    join.extend_from_slice(&id.to_le_bytes());
    p.frame(&server, cs::CSJoinInstantGamePacket, &join).await;
    p.frame(
        &server,
        cs::CSEnteredInstantGameWorldPacket,
        &id.to_le_bytes(),
    )
    .await;
    assert_eq!(game.st().members.len(), 1);
    assert_eq!(p.p.data().unit.origin_faction_id, 0);
    assert_eq!(
        server
            .systems
            .instances
            .events
            .characters
            .count(&p.p, CharEvent::Kill),
        1
    );
    instant_game::destroy_game(&server, &game);
    assert_eq!(
        server
            .systems
            .instances
            .events
            .characters
            .count(&p.p, CharEvent::Kill),
        0
    );
    assert!(server.systems.instances.instant_games.games().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Real data (gated)
// ---------------------------------------------------------------------------------------------

/// The real 1.2 tables + the real `instance_burntcastle_armory` spawn files
/// (`AAEMU_COMPACT_DB=... cargo test -p aaemu-game real_dungeon -- --ignored`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_dungeon_spawns_load_and_leave_returns_to_main_world() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let app_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let files = crate::data::client_files::ClientFileManager::from_sources(&[], &app_path);
    let ctx = crate::systems::DataLoadCtx {
        conn: &conn,
        app_path: &app_path,
        files: &files,
        language: "en_us",
    };
    let mut data = GameData::empty();
    data.zones = ZoneData::load(&conn).expect("zones");
    data.systems.npcs = crate::systems::npcs::Data::load(&ctx).expect("npcs data");
    data.systems.instances = super::Data::load(&ctx).expect("instances data");
    let spawns_json =
        std::fs::read_to_string(app_path.join("Data/Worlds/world_spawns.json")).unwrap();
    let lookups: Vec<WorldSpawnLocation> = crate::data::world_templates::parse_world_spawns(
        spawns_json.trim_start_matches('\u{feff}'),
    )
    .unwrap()
    .into_iter()
    .flatten()
    .collect();
    let spawn = lookups
        .iter()
        .find(|l| l.name.as_deref() == Some("instance_burntcastle_armory"))
        .and_then(|l| l.spawn_position)
        .unwrap();
    let server = build_server(
        data,
        vec![instance_template(
            1,
            "instance_burntcastle_armory",
            236,
            spawn,
        )],
        lookups,
        Some(&app_path),
    );
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    a.p.data().unit.level = 40;
    a.frames();
    // Zone id 168 is zone key 236 (group 45, "instance_burntcastle_armory").
    use_enter_func(&server, &a.p, 168);
    let d = server
        .systems
        .instances
        .dungeon_of_world(100)
        .expect("dungeon");
    await_loader(&d).await;
    let stats = server.systems.npcs.spawns.get(100).unwrap().counts();
    eprintln!("instance spawners: {stats:?}");
    assert!(stats[0] > 30, "npc spawners {stats:?}");
    let world = server.world.world(100).unwrap();
    assert!(!world.all_doodads().is_empty(), "the doodad spawned");
    let expected = load_instance(
        100,
        236,
        spawn.x,
        spawn.y,
        spawn.z,
        (
            worlds::deg_to_rad(spawn.roll),
            worlds::deg_to_rad(spawn.pitch),
            worlds::deg_to_rad(spawn.yaw),
        ),
    );
    assert!(a.frames().contains(&(sc::SCLoadInstancePacket, expected)));
    a.frame(&server, cs::CSInstanceLoadedPacket, &[]).await;
    a.move_to(&server, 199.0, 180.0, spawn.z).await;
    a.frames();
    // The active-region tick spawns the NPCs around the player.
    crate::systems::npcs::spawn_manager::active_region_tick(&server);
    let npcs = world.around(&WorldObject::from(&a.p), |o| o.as_npc().cloned());
    assert!(!npcs.is_empty(), "no NPCs spawned around the player");
    assert!(a
        .frames()
        .iter()
        .any(|(op, _)| *op == sc::SCUnitStatePacket));
    // Leave: back to the saved main_world position.
    use_exit_func(&server, &a.p);
    assert_eq!(a.instance(), 0);
    let t = a.p.data().unit.transform;
    assert_eq!((t.zone_id, t.x, t.y, t.z), (TEST_ZONE, 100.0, 100.0, 100.0));
    assert!(a
        .frames()
        .iter()
        .any(|(op, _)| *op == sc::SCLoadInstancePacket));
    // Destroy the run: with its real spawners, doodads and NPCs gone nothing may keep asking
    // for the instance (`GetWorld: No such World Instance 100` once per tick for hours).
    populate_instance_registries(&server, 100);
    dungeon::destroy_dungeon(&server, &d);
    assert!(!server.world.has_world(100));
    for _ in 0..3 {
        tick_every_instance_registry(&server);
        crate::systems::npcs::spawn_manager::active_region_tick(&server);
    }
    assert_eq!(
        server.world.all_missing_lookups(),
        Vec::<(u32, u64)>::new(),
        "something still asks for the destroyed instance"
    );
}

#[tokio::test]
async fn battlefield_entered_requires_actual_instance_and_subscribes_kills_once() {
    let server = camp_server(1, 60);
    let mut p = Pc::new(&server, 7, "Invited", 100.0, 100.0);
    instant_game::apply_to_battlefield(&server, 7, corps::ANY, &p.p);
    instant_game::battlefield_tick(&server);
    let game = server
        .systems
        .instances
        .instant_games
        .current(&p.p)
        .unwrap();
    let invite_id = invite_qualified_id(&p.frames());
    let original_faction = p.p.data().unit.faction_id;
    let entered = invite_id.to_le_bytes();
    p.frame(&server, cs::CSEnteredInstantGameWorldPacket, &entered)
        .await;
    assert!(p.frames().is_empty());
    assert!(game.st().members.is_empty());
    assert_eq!(p.p.data().unit.faction_id, original_faction);
    assert_eq!(
        server
            .systems
            .instances
            .events
            .characters
            .count(&p.p, CharEvent::Kill),
        0
    );
    let mut join = vec![1];
    join.extend_from_slice(&entered);
    p.frame(&server, cs::CSJoinInstantGamePacket, &join).await;
    assert_eq!(p.instance(), game.world_id);
    p.frames();
    p.frame(&server, cs::CSEnteredInstantGameWorldPacket, &entered)
        .await;
    assert!(p.ops().contains(&sc::SCInstantGameJoinedPacket));
    assert_eq!(game.st().members.len(), 1);
    assert_eq!(
        server
            .systems
            .instances
            .events
            .characters
            .count(&p.p, CharEvent::Kill),
        1
    );
    p.frame(&server, cs::CSEnteredInstantGameWorldPacket, &entered)
        .await;
    assert!(p.frames().is_empty());
    assert_eq!(game.st().members.len(), 1);
    assert_eq!(
        server
            .systems
            .instances
            .events
            .characters
            .count(&p.p, CharEvent::Kill),
        1
    );
    // RPA-09: the corps faction is temporary and restored when the player leaves.
    assert_eq!(p.p.data().unit.faction_id, 160);
    assert!(p.p.data().unit.faction_temporary);
    assert_eq!(p.p.data().unit.origin_faction_id, original_faction);
    instant_game::leave_current_game(&server, &p.p);
    assert_eq!(p.p.data().unit.faction_id, original_faction);
    assert!(!p.p.data().unit.faction_temporary);
}

/// Two players matched, joined and entered on battlefield 7; the game has started.
async fn started_1v1(server: &Arc<GameServer>) -> (Pc, Pc, Arc<instant_game::InstantGame>) {
    started_1v1_on(server, 7).await
}

/// [`started_1v1`] on the given battlefield (7 = Drill Camp, 11 = Gladiator).
async fn started_1v1_on(
    server: &Arc<GameServer>,
    battlefield_id: u32,
) -> (Pc, Pc, Arc<instant_game::InstantGame>) {
    let mut a = Pc::new(server, 5, "Echo", 100.0, 100.0);
    let mut b = Pc::new(server, 6, "Foxtrot", 120.0, 100.0);
    for pc in [&a, &b] {
        instant_game::apply_to_battlefield(server, battlefield_id, corps::ANY, &pc.p);
    }
    instant_game::battlefield_tick(server);
    let game = server.systems.instances.instant_games.games()[0].clone();
    let a_id = invite_qualified_id(&a.frames());
    let b_id = invite_qualified_id(&b.frames());
    let mut join = vec![1];
    join.extend_from_slice(&a_id.to_le_bytes());
    a.frame(server, cs::CSJoinInstantGamePacket, &join).await;
    join.truncate(1);
    join.extend_from_slice(&b_id.to_le_bytes());
    b.frame(server, cs::CSJoinInstantGamePacket, &join).await;
    a.frame(
        server,
        cs::CSEnteredInstantGameWorldPacket,
        &a_id.to_le_bytes(),
    )
    .await;
    b.frame(
        server,
        cs::CSEnteredInstantGameWorldPacket,
        &b_id.to_le_bytes(),
    )
    .await;
    a.frames();
    b.frames();
    (a, b, game)
}

/// `battle_field_buffs` are granted at match start (after the reset that removes every buff)
/// with the row's `value` as the buff charge, and removed again when the player leaves.
#[tokio::test]
async fn battlefield_buffs_are_granted_at_start_and_removed_on_leave() {
    let server = camp_server(1, 60);
    let (mut a, _b, game) = started_1v1(&server).await;
    assert!(!a.p.data().unit.buffs.check_buff(8246));
    instant_game::reset_player(&server, &game, &a.p);
    let buff =
        a.p.data()
            .unit
            .buffs
            .effect_from_buff_id(8246)
            .cloned()
            .expect("battlefield buff");
    assert_eq!(buff.charge, 1);
    // Never saved with the character (autosave / logout to the character select).
    assert!(buff.session_only);
    a.frames();
    instant_game::leave_current_game(&server, &a.p);
    assert!(!a.p.data().unit.buffs.check_buff(8246));
}

/// The kill / assist points come from the rule set's `game_score_rules` rows (event 200 for
/// the final hitter, 204 for an assist; the 1.2 Drill Camp rows are 3 / 2), not from the old
/// hard-coded 30.
#[tokio::test]
async fn battlefield_kill_and_assist_scores_come_from_game_score_rules() {
    let server = camp_server_with_score(2, 100, 3, 2, |r| {
        r.id = 8;
    });
    let mut a = Pc::new(&server, 5, "Killer", 100.0, 100.0);
    let mut b = Pc::new(&server, 6, "Victim", 120.0, 100.0);
    let mut c = Pc::new(&server, 7, "Helper", 130.0, 100.0);
    for pc in [&a, &b, &c] {
        instant_game::apply_to_battlefield(&server, 7, corps::ANY, &pc.p);
    }
    instant_game::battlefield_tick(&server);
    let game = server.systems.instances.instant_games.games()[0].clone();
    assert_eq!(game.rule_set.id, 8);
    for pc in [&mut a, &mut b, &mut c] {
        let id = invite_qualified_id(&pc.frames());
        let mut join = vec![1];
        join.extend_from_slice(&id.to_le_bytes());
        pc.frame(&server, cs::CSJoinInstantGamePacket, &join).await;
        pc.frame(
            &server,
            cs::CSEnteredInstantGameWorldPacket,
            &id.to_le_bytes(),
        )
        .await;
        pc.frames();
    }
    assert_eq!(game.st().members.len(), 3);
    instant_game::start_game(&server, &game);
    for pc in [&mut a, &mut b, &mut c] {
        pc.frames();
    }

    // Helper damaged the victim recently: the kill pays the killer 3 and the helper 2.
    crate::systems::skills::combat::record_pvp_damage_from(&b.p, c.p.character_id);
    events::on_do_die(
        &server,
        &WorldObject::from(&b.p),
        Some(&WorldObject::from(&a.p)),
    );
    let st = game.st();
    let index = |p: &PlayerRef| {
        st.members
            .iter()
            .position(|m| Arc::ptr_eq(&m.player, p))
            .unwrap()
    };
    assert_eq!(st.members[index(&a.p)].score, 3);
    assert_eq!(st.members[index(&b.p)].score, 0);
    assert_eq!(st.members[index(&c.p)].score, 2);
    assert_eq!(st.members[index(&a.p)].kills, 1);
    assert_eq!(st.members[index(&c.p)].assists, 1);
}

fn end_reason(frames: &[(u16, Vec<u8>)]) -> Option<u8> {
    frames
        .iter()
        .find(|(op, _)| *op == sc::SCInstantGameEndPacket)
        .map(|(_, b)| b[8])
}

#[tokio::test]
async fn battlefield_zero_victory_score_waits_for_timeover() {
    // The real Drill Camp rule set (22) has victory_score = 0: the C# check
    // `Score >= VictoryScore` ended such a game on the first kill.
    let server = camp_server_kills(1, 0, 0);
    let (mut a, mut b, game) = started_1v1(&server).await;
    events::on_do_die(
        &server,
        &WorldObject::from(&b.p),
        Some(&WorldObject::from(&a.p)),
    );
    assert_eq!(end_reason(&a.frames()), None);
    assert_eq!(end_reason(&b.frames()), None);
    assert_eq!(server.systems.instances.instant_games.games().len(), 1);
    // Time over: the killer's corps wins by kill count, reason TimeoverCompareKillCount.
    instant_game::finish_timeover(&server, &game);
    assert_eq!(
        end_reason(&a.frames()),
        Some(instant_game::ENDING_TIMEOVER_COMPARE_KILL_COUNT)
    );
    let killer_corps = game.st().members[0].result;
    let st = game.st();
    let (winner, loser) = if killer_corps == corps::CORPS1 {
        (st.corps1.state, st.corps2.state)
    } else {
        (st.corps2.state, st.corps1.state)
    };
    assert_eq!(
        (winner, loser),
        (
            instant_game::victory_state::WIN,
            instant_game::victory_state::LOSE
        )
    );
}

#[tokio::test]
async fn battlefield_victory_kill_count_ends_game() {
    let server = camp_server_kills(1, 0, 1);
    let (mut a, b, _game) = started_1v1(&server).await;
    events::on_do_die(
        &server,
        &WorldObject::from(&b.p),
        Some(&WorldObject::from(&a.p)),
    );
    assert_eq!(
        end_reason(&a.frames()),
        Some(instant_game::ENDING_ACHIEVEMENT_KILL_COUNT)
    );
}

#[tokio::test]
async fn battlefield_level_min_refuses_apply() {
    // Test characters are level 10; a level-50 gate (the Gladiator rule set's) refuses the
    // apply with the client's level-requirements message and never queues.
    let server = camp_server_with(1, 60, 0, |r| r.level_min = 50);
    let mut p = Pc::new(&server, 7, "Lowbie", 100.0, 100.0);
    p.frames();
    p.frame(&server, cs::CSApplyToInstantGamePacket, &[7, 0, 0, 0, 2])
        .await;
    assert_eq!(
        p.frames(),
        vec![(
            sc::SCAppliedToInstantGamePacket,
            body(&SCAppliedToInstantGamePacket {
                battlefield_id: 7,
                corps: corps::ANY,
                error_message_id: instant_game::ERROR_LEVEL_REQUIREMENTS,
            })
        )]
    );
    assert_eq!(server.systems.instances.instant_games.queued(7), None);
}

#[tokio::test]
async fn battlefield_corps_head_kill_wins() {
    let server = camp_server_with(1, 0, 0, |r| {
        r.victory_kill_corps1_head = 13353;
        r.victory_kill_corps2_head = 13352;
    });
    let (mut a, _b, game) = started_1v1(&server).await;
    let killer_corps = game.st().members[0].result;
    let target = game.rule_set.corps_head_target(killer_corps);
    assert!(target == 13352 || target == 13353);
    // A non-target NPC only counts towards the non-PC-kill bonus.
    let mob = WorldObject::from(&Entity::new(0x301, Npc::new(9999)));
    events::on_do_die(&server, &mob, Some(&WorldObject::from(&a.p)));
    assert_eq!(end_reason(&a.frames()), None);
    assert_eq!(game.st().members[0].npc_kills, 1);
    // The target Polestar ends the game for the killer's corps.
    let head = WorldObject::from(&Entity::new(0x302, Npc::new(target)));
    events::on_do_die(&server, &head, Some(&WorldObject::from(&a.p)));
    assert_eq!(
        end_reason(&a.frames()),
        Some(instant_game::ENDING_ACHIEVEMENT_KILL_CORPS_HEAD)
    );
    let st = game.st();
    let winner = if killer_corps == corps::CORPS1 {
        st.corps1.state
    } else {
        st.corps2.state
    };
    assert_eq!(winner, instant_game::victory_state::WIN);
    assert_eq!(st.members[0].npc_kills, 2);
}

#[tokio::test]
async fn battlefield_end_awards_bonus_honor() {
    let server = camp_server_with(1, 30, 0, |r| {
        r.bonus_winner = 50;
        r.bonus_loser = 10;
        r.bonus_no_death = 7;
        r.bonus_top_enemy_pc_kill = 10;
        r.bonus_top_enemy_non_pc_kill = 3;
    });
    let (a, b, game) = started_1v1(&server).await;
    // One kill reaches the 30-point victory score and pays out at the result.
    events::on_do_die(
        &server,
        &WorldObject::from(&b.p),
        Some(&WorldObject::from(&a.p)),
    );
    let st = game.st();
    assert_eq!(st.members[0].bonus, 50 + 7 + 10);
    assert_eq!(
        st.members[0].bonus_set,
        instant_game::bonus_set::VICTORY
            | instant_game::bonus_set::TOP_PC_KILL
            | instant_game::bonus_set::NO_DEATH
    );
    assert_eq!(st.members[1].bonus, 10);
    assert_eq!(st.members[1].bonus_set, instant_game::bonus_set::LOSER);
    drop(st);
    assert_eq!(a.p.data().honor_point, 67);
    assert_eq!(b.p.data().honor_point, 10);
}

#[tokio::test]
async fn a_battlefield_result_scores_the_arena_ranking_board() {
    let server = camp_server_ranked(1, 60);
    let mut e = Pc::new(&server, 5, "Echo", 100.0, 100.0);
    let mut f = Pc::new(&server, 6, "Foxtrot", 120.0, 100.0);
    e.frames();
    f.frames();
    for p in [&e, &f] {
        instant_game::apply_to_battlefield(&server, 7, corps::ANY, &p.p);
    }
    instant_game::battlefield_tick(&server);
    for p in [&mut e, &mut f] {
        let id = invite_qualified_id(&p.frames());
        let mut join = vec![1];
        join.extend_from_slice(&id.to_le_bytes());
        p.frame(&server, cs::CSJoinInstantGamePacket, &join).await;
        p.frame(
            &server,
            cs::CSEnteredInstantGameWorldPacket,
            &id.to_le_bytes(),
        )
        .await;
    }
    // Nothing is scored while the game runs.
    let ranking = &server.systems.social.ranking;
    assert!(ranking.is_empty());

    // Echo kills Foxtrot twice: 60 points, corps 2 (Echo) wins, the result is final.
    for _ in 0..2 {
        events::on_do_die(
            &server,
            &WorldObject::from(&f.p),
            Some(&WorldObject::from(&e.p)),
        );
    }
    let board = server.game_data.systems.social.ranking.rank(1).unwrap();
    let period = board
        .period_at(chrono::Utc::now())
        .unwrap()
        .start
        .timestamp();
    let winner = ranking.score_of(5, 1, period).expect("winner scored");
    assert_eq!((winner.v1, winner.v2), (4, 1));
    assert_eq!(winner.account, "Echo");
    let loser = ranking.score_of(6, 1, period).expect("loser scored");
    assert_eq!((loser.v1, loser.v2), (1, 0));

    // The result is offered once: the destroy that follows does not score again.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(ranking.score_of(5, 1, period).unwrap().v1, 4);
}

/// The battlefield resurrects the victim after the running rule set's
/// `time_resurrection_delay` (the 1.2 rows author 15 s), not after the C# hard-coded 6 s.
#[tokio::test(start_paused = true)]
async fn battlefield_resurrection_waits_the_rule_set_delay() {
    let server = camp_server_with(1, 60, 0, |r| r.time_resurrection_delay = 15);
    let (a, mut b, game) = started_1v1(&server).await;
    assert_eq!(game.rule_set.time_resurrection_delay, 15);
    b.frames();
    events::on_do_die(
        &server,
        &WorldObject::from(&b.p),
        Some(&WorldObject::from(&a.p)),
    );
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(14)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert!(
        !b.frames()
            .iter()
            .any(|(op, _)| *op == sc::SCCharacterResurrectedPacket),
        "the victim must still be dead 14 s after the kill"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert!(
        b.frames()
            .iter()
            .any(|(op, _)| *op == sc::SCCharacterResurrectedPacket),
        "the victim must be resurrected 15 s after the kill"
    );
}

/// The Gladiator rule set (28) authors a 5 s delay.
#[tokio::test(start_paused = true)]
async fn gladiator_resurrection_waits_five_seconds() {
    let server = camp_server_build(1, 60, 0, 30, 2, false, 11, |r| {
        r.id = 28;
        r.time_resurrection_delay = 5;
    });
    let (a, mut b, game) = started_1v1_on(&server, 11).await;
    assert_eq!(game.battlefield.id, instant_game::game_type::GLADIATOR);
    assert_eq!(game.rule_set.time_resurrection_delay, 5);
    b.frames();
    events::on_do_die(
        &server,
        &WorldObject::from(&b.p),
        Some(&WorldObject::from(&a.p)),
    );
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(4)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert!(
        !b.frames()
            .iter()
            .any(|(op, _)| *op == sc::SCCharacterResurrectedPacket),
        "the victim must still be dead 4 s after the kill"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert!(
        b.frames()
            .iter()
            .any(|(op, _)| *op == sc::SCCharacterResurrectedPacket),
        "the victim must be resurrected 5 s after the kill"
    );
}

/// A rule set without an authored delay (0 / NULL) keeps the old 6 s.
#[tokio::test(start_paused = true)]
async fn battlefield_resurrection_without_delay_keeps_six_seconds() {
    let server = camp_server(1, 60);
    let (a, mut b, game) = started_1v1(&server).await;
    assert_eq!(game.rule_set.time_resurrection_delay, 0);
    b.frames();
    events::on_do_die(
        &server,
        &WorldObject::from(&b.p),
        Some(&WorldObject::from(&a.p)),
    );
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(5)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert!(
        !b.frames()
            .iter()
            .any(|(op, _)| *op == sc::SCCharacterResurrectedPacket),
        "without an authored delay the victim must not resurrect before 6 s"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert!(
        b.frames()
            .iter()
            .any(|(op, _)| *op == sc::SCCharacterResurrectedPacket),
        "without an authored delay the victim must be resurrected at 6 s"
    );
}

// ---- Return effect (world gates) inside instances ----

const RETURN_GATE_INSTANCE: u32 = 6001;
const RETURN_GATE_MAIN: u32 = 6002;
const RETURN_GATE_DISTANT: u32 = 6003;
const RETURN_SKILL: u32 = 24979;

fn return_gate_data() -> GameData {
    use crate::systems::skills::data::{effects::SpecialEffect, EffectKind, EffectTemplate};

    let mut data = dungeon_data();
    let gate = |id: u32, zone_id: u32, x: f32| crate::data::world_data::Portal {
        id,
        name: format!("gate_{id}"),
        zone_id,
        x,
        y: 210.0,
        z: 25.0,
        yaw: 90.0,
        sub_zone_id: id,
        ..Default::default()
    };
    data.systems.npcs.portal = crate::systems::npcs::portal::data::PortalTables::from_parts(
        vec![],
        vec![
            gate(RETURN_GATE_INSTANCE, DUNGEON_ZONE, 220.0),
            gate(RETURN_GATE_MAIN, TEST_ZONE, 120.0),
            gate(RETURN_GATE_DISTANT, DUNGEON_ZONE, 1300.0),
        ],
        vec![],
        vec![],
    )
    .expect("gates");
    data.systems.skills.add_effect(
        EffectKind::SpecialEffect,
        EffectTemplate::Special(SpecialEffect {
            id: 1,
            special_effect_type_id: 25,
            value1: RETURN_GATE_DISTANT as i32,
            value2: 0,
            value3: 0,
            value4: 0,
        }),
        1,
    );
    data.systems
        .skills
        .add_skill(crate::systems::skills::data::SkillTemplate {
            target_type: 0, // Self
            effects: vec![crate::systems::skills::tests::skill_effect(
                1,
                EffectKind::SpecialEffect,
                1,
            )],
            ..crate::systems::skills::tests::skill_template(RETURN_SKILL)
        });
    crate::test_support::open_client_skills(&mut data);
    data
}

fn cast_return(server: &GameServer, p: &PlayerRef, gate_id: i32) {
    use crate::systems::skills::effects::{EffectArgs, EffectSource};
    use crate::systems::skills::types::{CastAction, SkillCastTarget, SkillCaster, SkillObject};
    let caster = WorldObject::from(p);
    let args = EffectArgs {
        caster: Some(caster.clone()),
        caster_caster: SkillCaster::Unit(p.obj_id),
        target: crate::systems::skills::geometry::Target::Obj(caster),
        target_caster: SkillCastTarget::Unit(p.obj_id),
        cast: CastAction::Skill {
            skill_id: 24979,
            tl_id: 1,
        },
        source: EffectSource::default(),
        skill_object: &SkillObject::None,
    };
    crate::systems::skills::effects::special::execute(server, 25, &args, [gate_id, 0, 0, 0]);
}

async fn enter_dungeon(server: &Arc<GameServer>, a: &mut Pc) {
    use_enter_func(server, &a.p, DUNGEON_ZONE);
    let d = server
        .systems
        .instances
        .dungeon_of_world(100)
        .expect("dungeon");
    await_loader(&d).await;
    a.frame(server, cs::CSInstanceLoadedPacket, &[]).await;
    a.frames();
    assert_eq!(a.instance(), 100);
}

/// A gate id without data, or a gate of a main-world zone, never ejects a player from its
/// instance: error 439, no `SCLoadInstance`, transform unchanged.
#[tokio::test]
async fn return_without_usable_gate_keeps_the_player_in_the_instance() {
    let server = build_return_server();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    enter_dungeon(&server, &mut a).await;
    let before = a.p.data().unit.transform;
    assert!(a.p.data().main_world_position.is_some());

    for gate in [7777, RETURN_GATE_MAIN as i32] {
        cast_return(&server, &a.p, gate);
        let ops = a.ops();
        assert!(!ops.contains(&sc::SCLoadInstancePacket), "gate {gate}");
        assert!(!ops.contains(&sc::SCTeleportUnitPacket), "gate {gate}");
        assert!(ops.contains(&sc::SCErrorMsgPacket), "gate {gate}");
        assert_eq!(a.p.data().unit.transform, before, "gate {gate}");
    }
}

/// A gate whose zone is in the player's instance world moves the player inside it.
#[tokio::test]
async fn return_to_a_gate_of_the_instance_world_teleports_inside_it() {
    let server = build_return_server();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    enter_dungeon(&server, &mut a).await;

    cast_return(&server, &a.p, RETURN_GATE_INSTANCE as i32);
    let frames = a.frames();
    let ops: Vec<u16> = frames.iter().map(|f| f.0).collect();
    assert!(!ops.contains(&sc::SCLoadInstancePacket));
    assert!(ops.contains(&sc::SCTeleportUnitPacket));
    assert!(!ops.contains(&sc::SCErrorMsgPacket));
    let t = a.p.data().unit.transform;
    assert_eq!(t.instance_id, 100);
    assert_eq!(t.zone_id, DUNGEON_ZONE);
    assert!((t.x - 220.0).abs() < 0.01 && (t.y - 210.0).abs() < 0.01);
}

/// Return crosses disjoint visibility neighbourhoods, then the client completes the teleport
/// without sending movement. Both observers and the caster must see the arrival region.
#[tokio::test]
async fn return_to_a_distant_instance_gate_refiles_visibility_without_movement() {
    use crate::systems::skills::types::{convert_long, SkillCastTarget, SkillCaster};

    #[derive(Default)]
    struct ZoneChanges(std::sync::Mutex<Vec<(u32, u32, u32)>>);
    impl crate::systems::UnitHooks for ZoneChanges {
        fn on_unit_zone_change(
            &self,
            _server: &GameServer,
            unit: &WorldObject,
            old_zone: u32,
            new_zone: u32,
        ) {
            self.0
                .lock()
                .unwrap()
                .push((unit.obj_id(), old_zone, new_zone));
        }
    }

    let server = build_return_server();
    // Walks across the map in one packet: the movement guard has its own tests.
    server.config.update(|c| c.world.move_guard_enabled = false);
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    let mut old = Pc::new(&server, 2, "Departure", 110.0, 100.0);
    let mut new = Pc::new(&server, 3, "Arrival", 120.0, 100.0);
    form_party(&server, &a, &old);
    form_party(&server, &a, &new);
    enter_dungeon(&server, &mut a).await;
    // The party shares the dungeon that Alpha's loader has already finished.
    for observer in [&mut old, &mut new] {
        use_enter_func(&server, &observer.p, DUNGEON_ZONE);
        observer
            .frame(&server, cs::CSInstanceLoadedPacket, &[])
            .await;
        assert_eq!(observer.instance(), a.instance());
    }
    new.move_to(&server, 1305.0, 210.0, 25.0).await;
    let world = server.world.world(a.instance()).unwrap();
    let departure = a.p.region().unwrap();
    let arrival = new.p.region().unwrap();
    assert_eq!(departure, (100, 3, 3));
    assert_eq!(arrival, (100, 20, 3));
    assert!(world
        .objects_around(&WorldObject::from(&a.p))
        .iter()
        .any(|o| o.obj_id() == old.p.obj_id));
    assert!(!world
        .objects_around(&WorldObject::from(&a.p))
        .iter()
        .any(|o| o.obj_id() == new.p.obj_id));
    let saved_main_world_position = a.p.data().main_world_position.unwrap();
    let zone_changes = Arc::new(ZoneChanges::default());
    server.systems.hooks.unit.install(zone_changes.clone());
    a.frames();
    old.frames();
    new.frames();

    let mut cast = PacketStream::new();
    cast.write_u32(RETURN_SKILL);
    SkillCaster::Unit(a.p.obj_id).write(&mut cast);
    SkillCastTarget::Unit(a.p.obj_id).write(&mut cast);
    cast.write_u8(0);
    a.frame(&server, cs::CSStartSkillPacket, cast.as_bytes())
        .await;
    let sent = a.frames();
    assert!(sent.iter().any(|(op, _)| *op == sc::SCTeleportUnitPacket));
    assert!(!sent.iter().any(|(op, _)| *op == sc::SCLoadInstancePacket));
    assert!(a.p.data().disabled_set_position);

    let mut ended = PacketStream::new();
    ended
        .write_i64(convert_long(1300.0))
        .write_i64(convert_long(210.0))
        .write_f32(25.0);
    for _ in 0..16 {
        ended.write_u8(0);
    }
    a.frame(&server, cs::CSTeleportEndedPacket, ended.as_bytes())
        .await;
    let completed = a.frames();
    assert!(!a.p.data().disabled_set_position);
    assert_eq!(a.p.region(), Some(arrival));
    assert!(!world
        .region_object_ids((departure.1, departure.2))
        .contains(&a.p.obj_id));
    assert!(world
        .region_object_ids((arrival.1, arrival.2))
        .contains(&a.p.obj_id));
    let state_count = |frames: &[(u16, Vec<u8>)], id| {
        frames
            .iter()
            .filter(|(op, b)| *op == sc::SCUnitStatePacket && bc(b) == id)
            .count()
    };
    assert!(removed_ids(&sent).contains(&old.p.obj_id));
    assert_eq!(
        state_count(&sent, new.p.obj_id),
        1,
        "arrival visibility is filed when Return moves the transform"
    );
    assert_eq!(
        state_count(&completed, new.p.obj_id),
        1,
        "completion repaints the arrival"
    );
    assert_eq!(
        state_count(&completed, old.p.obj_id),
        0,
        "departure must not be repainted"
    );
    assert_eq!(
        removed_ids(&old.frames())
            .iter()
            .filter(|&&id| id == a.p.obj_id)
            .count(),
        1
    );
    assert_eq!(
        state_count(&new.frames(), a.p.obj_id),
        1,
        "arrival observer sees the caster once"
    );
    let t = a.p.data().unit.transform;
    assert_eq!(
        (t.instance_id, t.world_id, t.zone_id),
        (100, 1, DUNGEON_ZONE)
    );
    assert_eq!((t.x, t.y, t.z), (1300.0, 210.0, 25.0));
    assert!((t.rot_z - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
    assert_eq!(
        a.p.data().main_world_position,
        Some(saved_main_world_position)
    );
    assert!(
        zone_changes.0.lock().unwrap().is_empty(),
        "same-zone Return must not fire zone hooks"
    );

    // A repeated completion may repaint for the caster, but must not repeat observer deltas
    // or zone events, and must never re-file the player back at the departure.
    a.frame(&server, cs::CSTeleportEndedPacket, ended.as_bytes())
        .await;
    assert!(old.frames().is_empty());
    assert!(new.frames().is_empty());
    assert!(zone_changes.0.lock().unwrap().is_empty());
    assert_eq!(a.p.region(), Some(arrival));
}

/// Outside an instance a main-world gate still loads the main world (`SCLoadInstance` +
/// `SCTeleportUnit`), and a gate that exists only in an instance world is never used.
#[tokio::test]
async fn return_in_the_main_world_is_unchanged() {
    let server = build_return_server();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    a.frames();

    cast_return(&server, &a.p, RETURN_GATE_MAIN as i32);
    let ops = a.ops();
    assert_eq!(
        ops,
        vec![sc::SCLoadInstancePacket, sc::SCTeleportUnitPacket]
    );
    let t = a.p.data().unit.transform;
    assert_eq!((t.instance_id, t.zone_id), (0, TEST_ZONE));
    assert!((t.x - 120.0).abs() < 0.01);

    cast_return(&server, &a.p, RETURN_GATE_INSTANCE as i32);
    assert!(a.ops().is_empty());
    assert!((a.p.data().unit.transform.x - 120.0).abs() < 0.01);
}

/// A main-world gate next to the caster (Brimblaze Tower's up / down circles, Hadir Farm's
/// escape circle): `SCLoadInstance` empties the client's world although the character stays
/// filed in the main world. `CSInstanceLoaded` must resend the objects of the neighbourhood the
/// departure and the arrival share, not only the regions that are new; a repeated load must not.
#[tokio::test]
async fn return_to_a_nearby_main_world_gate_repaints_the_shared_neighbourhood() {
    let server = build_return_server();
    let mut a = Pc::new(&server, 1, "Alpha", 100.0, 100.0);
    let main = server.world.world(0).unwrap();
    let ctx = server.view_ctx();
    // Between the caster (100, 100) and the gate (120, 210): in both neighbourhoods.
    let mut npc = Npc::new(500);
    npc.unit.transform.set_xyz(110.0, 150.0, 25.0);
    let npc = Entity::new(0x6101, npc);
    main.spawn(&npc, &ctx);
    let mut doodad = Doodad::new(600);
    doodad.transform.set_xyz(112.0, 150.0, 25.0);
    let doodad = Entity::new(0x6102, doodad);
    main.spawn(&doodad, &ctx);
    let departure = a.p.region().unwrap();
    a.frames();

    cast_return(&server, &a.p, RETURN_GATE_MAIN as i32);
    assert!(a.ops().contains(&sc::SCLoadInstancePacket));
    a.frame(&server, cs::CSInstanceLoadedPacket, &[]).await;
    let f = a.frames();
    assert_ne!(a.p.region(), Some(departure));
    assert_eq!(a.p.region().map(|r| r.0), Some(0));
    assert!(
        f.iter()
            .any(|(op, b)| *op == sc::SCUnitStatePacket && bc(b) == 0x6101),
        "shared-neighbourhood NPC resent: {f:?}"
    );
    assert!(
        f.iter().any(|(op, b)| *op == sc::SCDoodadsCreatedPacket
            && b.first() == Some(&1)
            && bc(&b[1..]) == 0x6102),
        "shared-neighbourhood doodad resent: {f:?}"
    );

    a.frame(&server, cs::CSInstanceLoadedPacket, &[]).await;
    let again = a.frames();
    assert!(!again
        .iter()
        .any(|(op, b)| *op == sc::SCUnitStatePacket && bc(b) == 0x6101));
    assert!(!again
        .iter()
        .any(|(op, _)| *op == sc::SCDoodadsCreatedPacket));
}

fn build_return_server() -> Arc<GameServer> {
    let server = dungeon_server_with(return_gate_data());
    // The world gate lookup is the npcs subsystem's hook.
    server
        .systems
        .skills
        .ext
        .npcs
        .install(Arc::new(crate::systems::npcs::bridges::NpcSkills));
    server
}
