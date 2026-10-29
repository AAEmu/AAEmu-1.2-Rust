//! Quest 2423 "The Windwhips' Chest" on the real 1.2 data. The quest hands out the Anima Injector
//! (item 16425, skill 14054). The player uses it on the Run-down Machine (NPC 7894): a 1 s cast,
//! then a 5 s channel whose target buff 1728 "Moving Machine" times out into `NpcControlEffect`
//! 87 (`FollowPath`, `q2423_fm_tige`). The machine then has to walk its route, uncovering doodad
//! 2906 "Suspicious Chest" that holds the quest item (`QuestActObjItemGather` 1050, item 16424).
//!
//! The tests run the client path (`CSStartSkillPacket` with an item caster) against the real
//! tables and the real `Data/Path` file, with the NPC spawned like `NpcSpawnerNpc.Spawn`
//! (AI + simulation). Gated by `AAEMU_COMPACT_DB` (path to `compact.sqlite3`):
//! `AAEMU_COMPACT_DB=... cargo test -p aaemu-game quest_machine -- --ignored`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sqlx::mysql::MySqlPoolOptions;

use crate::config::AppConfig;
use crate::data::client_files::ClientFileManager;
use crate::data::skills::{BuffKind, BuffTemplate};
use crate::data::world_templates::{WorldTemplate, WorldTemplates};
use crate::data::GameData;
use crate::opcodes::sc;
use crate::systems::economy::inventory::Inv;
use crate::test_support::{spawn_player, test_character, TestClient};
use crate::world::unit_state::UNIT_STATE_FORCED_BUFFS;
use crate::world::{NpcRef, PlayerRef, WorldObject};
use crate::GameServer;

use super::handlers::{start_skill, CSStartSkillPacket};
use super::types::{SkillCastTarget, SkillCaster, SkillObject};

const QUEST: u32 = 2423;
const MACHINE_NPC: u32 = 7894;
const INJECTOR: u32 = 16425;
const INJECTOR_SKILL: u32 = 14054;
const MOVING_MACHINE_BUFF: u32 = 1728;
const MACHINE_OBJ: u32 = 0x200;
const QUEST_GIVER_NPC: u32 = 9802;
const GIVER_OBJ: u32 = 0x201;
/// `main_world` is 31 x 31 cells of 1024 m; the machine stands near (21115, 8333).
const CELLS: i32 = 32;
/// Zone key of the Junkyard (the synthetic world stamps it on every region).
const JUNKYARD_ZONE: u32 = 179;
/// The machine's spawn point in `Data/Worlds/main_world/npc_spawns.json`.
const MACHINE_POS: (f32, f32, f32) = (21115.479, 8333.54, 378.94327);

/// The tables the quest needs, loaded from the real database.
fn real_data() -> Option<GameData> {
    let path = std::env::var("AAEMU_COMPACT_DB").ok()?;
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let files = ClientFileManager::from_sources(&[], &app);
    let localization = crate::data::Localization::load(&conn, "en_us").unwrap();
    let mut data = GameData::empty();
    data.experience = crate::test_support::test_experience(55);
    data.items = crate::data::items::ItemData::load(&conn).unwrap();
    data.factions = crate::data::FactionData::load(&conn, &localization).unwrap();
    data.skills = crate::data::skills::SkillData::load(&conn).unwrap();
    data.formulas = crate::data::formulas::FormulaManager::load(&conn).unwrap();
    data.systems = crate::systems::SystemsData::load(&crate::systems::DataLoadCtx {
        conn: &conn,
        app_path: &app,
        files: &files,
        language: "en_us",
    })
    .unwrap();
    Some(data)
}

/// A server on the real tables with a `main_world` big enough for the Junkyard coordinates.
fn real_server(mut data: GameData) -> Arc<GameServer> {
    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    data.worlds = WorldTemplates::from_templates(
        vec![WorldTemplate::synthetic(
            0,
            "main_world",
            CELLS,
            CELLS,
            JUNKYARD_ZONE,
        )],
        Vec::new(),
    )
    .expect("world");
    for id in UNIT_STATE_FORCED_BUFFS {
        let mut t = BuffTemplate::simple(id, BuffKind::Good);
        t.duration = 0;
        data.skills.buffs.insert(id, t);
    }
    let mut config = AppConfig {
        id: 1,
        ..AppConfig::default()
    };
    config.app_path = app;
    config.connections.mysql_provider.host = "127.0.0.1".into();
    config.connections.mysql_provider.port = 1;
    let pool = MySqlPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy_with(config.connections.mysql_provider.connect_options());
    let server = Arc::new(GameServer::new(config, pool, data));
    server.set_self();
    crate::systems::skills::bind(&server);
    crate::systems::npcs::install_hooks(&server);
    crate::systems::social::install_hooks(&server);
    crate::systems::quests::install_hooks(&server);
    crate::systems::economy::wiring::install(&server);
    crate::systems::economy::bridges::install(&server);
    crate::systems::ai::hooks::install(&server);
    server
        .systems
        .ai
        .set_paths_folder(server.config.app_path.join("Data").join("Path"));
    server
}

struct Fx {
    server: Arc<GameServer>,
    client: TestClient,
    player: PlayerRef,
    machine: NpcRef,
}

/// A level 30 player with quest 2423 started (the quest supplies the Anima Injector once its
/// evaluation ran), three metres from the Run-down Machine, which is spawned like `NpcSpawnerNpc.Spawn` (AI, simulation).
fn fixture(data: GameData) -> Fx {
    let server = real_server(data);
    let mut client = TestClient::new(&server, 1, 1001);
    let mut c = test_character(1, "Tinkerer", MACHINE_POS.0, MACHINE_POS.1 - 3.0);
    c.unit
        .transform
        .set_xyz(MACHINE_POS.0, MACHINE_POS.1 - 3.0, MACHINE_POS.2);
    c.unit.transform.zone_id = JUNKYARD_ZONE;
    c.unit.level = 30;
    c.unit.hp = 1000;
    c.unit.max_hp = 1000;
    c.unit.mp = 1000;
    c.unit.max_mp = 1000;
    c.skill_char.ignore_skill_cooldowns = true;
    // The start component requires a Ferre (race 6) who finished quest 3449.
    c.race = 6;
    crate::systems::quests::engine::set_completed_quest_flag(&mut c.quests.completed, 3449, true);
    let player = spawn_player(&server, &client, c, 0x100);
    client.conn.active_char = Some(player.clone());
    server.items.add_container(Inv::of(&player).bag());
    // The player accepts the quest from Elter (NPC 9802), standing next to them.
    let giver = crate::systems::npcs::npc::manager::create(&server, GIVER_OBJ, QUEST_GIVER_NPC)
        .expect("npc 9802 template");
    giver
        .data()
        .unit
        .transform
        .set_xyz(MACHINE_POS.0 + 1.0, MACHINE_POS.1 - 3.0, MACHINE_POS.2);
    giver.data().unit.transform.zone_id = JUNKYARD_ZONE;
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&giver, &server.view_ctx());
    let started =
        crate::systems::quests::events::add_quest_from_npc(&server, &player, QUEST, GIVER_OBJ);
    assert_eq!(started, Some(true), "quest 2423 must start");
    // The quest engine evaluates a started quest on a queued task (Start -> Supply -> Progress):
    // the supply step hands out the Anima Injector.
    server.systems.quests.run_queued_evaluations(&server);
    let machine = crate::systems::npcs::npc::manager::create(&server, MACHINE_OBJ, MACHINE_NPC)
        .expect("npc 7894 template");
    {
        let mut n = machine.data();
        n.unit
            .transform
            .set_xyz(MACHINE_POS.0, MACHINE_POS.1, MACHINE_POS.2);
        n.unit.transform.zone_id = JUNKYARD_ZONE;
        n.unit.hp = 1000;
        n.unit.max_hp = 1000;
    }
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&machine, &server.view_ctx());
    // NpcSpawnerNpc.Spawn: home position, GoToSpawn, then Simulation + FollowPath
    let hooks = server.systems.hooks.ai.get().expect("ai hooks");
    hooks.npc_spawn_positioned(&server, &machine);
    hooks.npc_spawned(&server, &machine, "");
    client.drain();
    Fx {
        server,
        client,
        player,
        machine,
    }
}

impl Fx {
    fn injector(&self) -> u64 {
        let bag = Inv::of(&self.player).bag();
        let bag = bag.lock().unwrap();
        let (ids, count) = bag.items_by_template(INJECTOR, -1);
        assert!(count > 0, "the Anima Injector must be in the bag");
        ids[0]
    }

    /// The client's cast of the injector on the machine (`CSStartSkillPacket`, item caster).
    fn use_injector(&mut self) {
        let item_id = self.injector();
        self.player.data().unit.skill_last_used_ms = 0;
        self.player.data().unit.global_cooldown_ms = 0;
        start_skill(
            &self.server,
            &mut self.client.conn,
            CSStartSkillPacket {
                skill_id: INJECTOR_SKILL,
                caster: SkillCaster::Item {
                    obj_id: self.player.obj_id,
                    item_id,
                    item_template_id: INJECTOR,
                    type1: 0,
                    type2: 0,
                },
                target: SkillCastTarget::Unit(MACHINE_OBJ),
                flag: 0,
                skill_object: SkillObject::None,
            },
        );
    }

    fn machine_has_buff(&self, buff: u32) -> bool {
        WorldObject::from(&self.machine).with_base_unit(|b| b.buffs().check_buff(buff))
    }

    fn machine_pos(&self) -> (f32, f32, f32) {
        let t = self.machine.data().unit.transform;
        (t.x, t.y, t.z)
    }
}

fn dist(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt()
}

/// The whole chain on the real tables: the cast (1 s) and the channel (5 s) of the Anima
/// Injector put buff 1728 on the machine, its Timeout runs `NpcControlEffect` 87 and the
/// machine walks `Data/Path/q2423_fm_tige.path` (6 points) to its end. The AI is ticked beside
/// the walk like on the live server.
#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn quest_machine_walks_its_route_after_the_injector_channel() {
    let Some(data) = real_data() else {
        return;
    };
    let mut fx = fixture(data);
    let start = fx.machine_pos();
    fx.use_injector();

    // 1 s cast, then the channel applies the Moving Machine buff to the machine.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        fx.machine_has_buff(MOVING_MACHINE_BUFF),
        "the channel must put buff 1728 on the machine"
    );
    assert!(!fx.machine.data().unit.is_in_patrol, "not walking yet");

    // The channel ends 5 s after it started: buff 1728 times out into the FollowPath control
    // effect and the machine walks at 1.8 m/s; the route is about 16 m long.
    fx.client.drain();
    let mut moves = 0;
    for _ in 0..200 {
        crate::systems::ai::manager::tick_all(&fx.server, 100);
        tokio::time::sleep(Duration::from_millis(100)).await;
        moves += fx
            .client
            .drain_ops()
            .into_iter()
            .filter(|op| *op == sc::SCOneUnitMovementPacket)
            .count();
    }
    assert!(!fx.machine_has_buff(MOVING_MACHINE_BUFF), "the buff ended");
    assert!(moves > 50, "movement packets for the machine: {moves}");
    // It stands on the last point of the route and is free again (IsInPatrol cleared).
    let end = fx.machine_pos();
    let last = (21114.224, 8349.611, 378.740);
    assert!(
        dist(end, last) < 3.5,
        "the machine must end its walk at {last:?}, it is at {end:?} (started at {start:?})"
    );
    assert!(!fx.machine.data().unit.is_in_patrol);
    let sim = fx
        .server
        .systems
        .ai
        .simulations
        .get(&MACHINE_OBJ)
        .map(|s| s.clone())
        .expect("the machine has a simulation");
    let sim = crate::systems::ai::simulation::lock_sim(&sim);
    assert_eq!(sim.move_file_name, "q2423_fm_tige");
    assert_eq!(sim.path.len(), 6, "the route file must load");
}

const CHEST_DOODAD: u32 = 2906;
const CHEST_OBJ: u32 = 0x300;
/// "Investigate the Suspicious Chest" (the chest's `DoodadFuncFakeUse` skill).
const CHEST_SKILL: u32 = 14053;
/// The Metal Cylinder the chest holds (`QuestActObjItemGather` 1050).
const METAL_CYLINDER: u32 = 16424;
/// The chest's spawn point in `Data/Worlds/main_world/doodad_spawns.json`.
const CHEST_POS: (f32, f32, f32) = (21120.49, 8332.734, 387.6176);

/// The follow-through: with the quest in progress the player investigates the Suspicious Chest
/// (doodad 2906: `DoodadFuncRequireQuest` 632 -> `DoodadFuncFakeUse` -> `DoodadFuncLootItem`
/// 1801, item 16424 at 100 %) and receives the Metal Cylinder the quest objective wants.
#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn quest_machine_chest_hands_out_the_metal_cylinder() {
    let Some(mut data) = real_data() else {
        return;
    };
    crate::test_support::open_client_skills(&mut data);
    let mut fx = fixture(data);
    let world = fx.server.world.world(0).expect("world");
    let chest = crate::systems::npcs::doodad::manager::create(
        &fx.server,
        &world,
        CHEST_OBJ,
        CHEST_DOODAD,
        None,
        true,
    )
    .expect("doodad 2906 template");
    chest
        .data()
        .transform
        .set_xyz(CHEST_POS.0, CHEST_POS.1, CHEST_POS.2);
    crate::systems::npcs::doodad::manager::spawn(&fx.server, &world, &chest);
    crate::systems::npcs::doodad::manager::init_doodad(&fx.server, &world, &chest);
    // The chest sits on a ledge nine metres above the machine: the player climbed up to it.
    fx.player
        .data()
        .unit
        .transform
        .set_xyz(CHEST_POS.0 - 1.0, CHEST_POS.1, CHEST_POS.2);
    fx.server
        .world
        .world(0)
        .unwrap()
        .add_visible_object(&fx.player, &fx.server.view_ctx());
    fx.client.drain();
    let have = |fx: &Fx| Inv::of(&fx.player).items_count(METAL_CYLINDER, -1);
    // (step, first objective) of the quest: Progress (4) with nothing gathered.
    let row = |fx: &Fx| {
        fx.server.systems.quests.run_queued_evaluations(&fx.server);
        fx.player
            .data()
            .quests
            .active
            .iter()
            .find(|q| q.template_id == QUEST)
            .map(|q| (q.step, q.objectives[0]))
    };
    assert_eq!(have(&fx), 0);
    assert_eq!(row(&fx), Some((4, 0)));

    fx.player.data().unit.skill_last_used_ms = 0;
    fx.player.data().unit.global_cooldown_ms = 0;
    start_skill(
        &fx.server,
        &mut fx.client.conn,
        CSStartSkillPacket {
            skill_id: CHEST_SKILL,
            caster: SkillCaster::Unit(fx.player.obj_id),
            target: SkillCastTarget::Doodad(CHEST_OBJ),
            flag: 0,
            skill_object: SkillObject::None,
        },
    );
    // 3 s cast, then the chest's loot function runs.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let ops = fx.client.drain_ops();
    eprintln!("chest cast frames: {ops:x?}");
    assert_eq!(have(&fx), 1, "the chest must hand out the Metal Cylinder");
    // "Investigate the Suspicious Chest 1/1": the objective is done and the quest is Ready (6).
    assert_eq!(row(&fx), Some((6, 1)));
}
