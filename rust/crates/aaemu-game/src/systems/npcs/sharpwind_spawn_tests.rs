//! Sharpwind Mines (`instance_cuttingwind_deadmine`, zone 262) spawn conformance.
//!
//! A plain `npc_spawns.json` row expands into every game-data spawner that lists the NPC as a
//! member (`SpawnManager::add_npc_spawner`). That is only correct for spawners that are active
//! when a world or dungeon instance starts: `npc_spawners.activation_state = 'f'` rows are
//! event spawners (their activating skill / quest / indun event names the spawner id itself),
//! and category-0 spawners marked `연출` ("staging") are cutscene actors. The synthetic test
//! runs in CI; the real-data tests are gated by `AAEMU_COMPACT_DB` like the rest of the crate's
//! data tests.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use sqlx::mysql::MySqlPoolOptions;

use crate::config::AppConfig;
use crate::data::world_templates::{WorldSpawnPosition, WorldTemplate, WorldTemplates};
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::server::GameServer;
use crate::systems::npcs::doodad::spawner::{parse_list, NpcSpawnerDef, SpawnerBase};
use crate::systems::npcs::npc::spawner_data::{
    npc_spawner_category, NpcSpawnerData, NpcSpawnerNpc, NpcSpawnerTemplate,
};
use crate::systems::npcs::spawn_manager::{
    backend::TestBackend, spawner_skip, LoadStats, SpawnManager, SpawnerSkip, TemplateExists,
};
use crate::test_support::{test_character, TestClient, TEST_ZONE};
use crate::world::{DoodadRef, Entity, NpcRef, Player, PlayerRef, WorldInstance, WorldObject};

const WORLD: &str = "instance_cuttingwind_deadmine";
const ZONE: u32 = 262;
const INSTANCE_ID: u32 = 100;

struct Backend;

impl TestBackend for Backend {
    fn npc_create(&self, server: &GameServer, obj_id: u32, template_id: u32) -> Option<NpcRef> {
        let id = if obj_id == 0 {
            server.ids.object.get_next_id().ok()?
        } else {
            obj_id
        };
        let mut n = Npc::new(template_id);
        n.unit.hp = 100;
        n.unit.max_hp = 100;
        n.unit.level = 10;
        Some(Entity::new(id, n))
    }

    fn doodad_create(
        &self,
        _server: &GameServer,
        _world: &Arc<WorldInstance>,
        _obj_id: u32,
        _template_id: u32,
    ) -> Option<DoodadRef> {
        None
    }
}

fn tpl(
    id: u32,
    category: u32,
    name: &str,
    comment: &str,
    activation_state: bool,
) -> NpcSpawnerTemplate {
    NpcSpawnerTemplate {
        id,
        npc_spawner_category_id: category,
        name: name.into(),
        comment: comment.into(),
        activation_state,
        spawn_delay_min: 0.0,
        spawn_delay_max: 0.0,
        ..NpcSpawnerTemplate::default()
    }
}

fn member(id: u32, spawner: u32, npc: u32) -> NpcSpawnerNpc {
    NpcSpawnerNpc {
        id,
        npc_spawner_template_id: spawner,
        member_id: npc,
        member_type: "Npc".into(),
        weight: 1.0,
        unit_id: 0,
    }
}

/// A synthetic server whose only world is `name` (the start-active rule depends on the world).
fn data_server_in(name: &str, spawners: NpcSpawnerData) -> Arc<GameServer> {
    let mut data = GameData::empty();
    data.systems.npcs.npc_spawners = spawners;
    let template = WorldTemplate::synthetic(0, name, 2, 2, TEST_ZONE);
    let server = Arc::new(crate::test_support::test_server_with_world(
        data,
        template.clone(),
    ));
    server.set_self();
    *server.systems.npcs.spawns.test_backend.lock().unwrap() = Some(Arc::new(Backend));
    // `GameServer::new` only creates `main_world`; the other test worlds are created here.
    server.world.create_world_instance(Arc::new(template), 0);
    server
}

fn plain_def(unit_id: u32, x: f32, y: f32) -> NpcSpawnerDef {
    NpcSpawnerDef {
        base: SpawnerBase {
            unit_id,
            position: WorldSpawnPosition {
                world_id: 1,
                zone_id: TEST_ZONE,
                x,
                y,
                z: 100.0,
                ..WorldSpawnPosition::default()
            },
            ..SpawnerBase::default()
        },
        count: 1,
        npc_spawner_ids: Vec::new(),
    }
}

/// Start-active rule: a category-0 `연출` cutscene spawner is skipped in every world; an
/// `activation_state = 'f'` spawner is skipped only in dungeon instances (not in an open world
/// and not in `instance_library`, whose indun action never activates its spawners). A
/// category-1 "staging NPC" template and ordinary spawners always create their placement. The
/// skipped spawner stays reachable for its effect.
#[tokio::test]
async fn plain_rows_skip_cutscene_everywhere_and_inactive_at_start_in_dungeons() {
    fn spawner_data() -> NpcSpawnerData {
        NpcSpawnerData::from_parts(
            vec![
                tpl(
                    10,
                    npc_spawner_category::AUTOCREATED,
                    "500: villager",
                    "",
                    true,
                ),
                tpl(
                    11,
                    npc_spawner_category::AUTOCREATED,
                    "비활성_500_event",
                    "",
                    false,
                ),
                tpl(
                    12,
                    npc_spawner_category::NORMAL,
                    "instance_x_alistair0",
                    "연출용 알리스테어0",
                    true,
                ),
                tpl(
                    13,
                    npc_spawner_category::NORMAL,
                    "instance_x_melee",
                    "",
                    true,
                ),
                tpl(
                    14,
                    npc_spawner_category::AUTOCREATED,
                    "500 (연출용NPC)",
                    "연출용",
                    true,
                ),
            ],
            (10u32..=14).map(|s| member(s, s, 500)).collect(),
        )
        .unwrap()
    }

    fn placed_ids(m: &SpawnManager) -> Vec<u32> {
        let mut ids: Vec<u32> = m
            .get_all_spawners()
            .into_values()
            .flatten()
            .map(|s| s.spawner_id)
            .collect();
        ids.sort();
        ids
    }

    // Open world: 'f' spawner 11 is a plain placement (main_world keeps its quest givers and
    // arche_mall its vendors); the cutscene actor 12 is not.
    let server = data_server_in("main_world", spawner_data());
    let m = server
        .systems
        .npcs
        .spawns
        .get_or_create(&server.world.world(0).unwrap());
    assert_eq!(
        m.add_npc_spawner(&server, plain_def(500, 20.0, 20.0)),
        Ok(4)
    );
    assert_eq!(
        placed_ids(&m),
        vec![10, 11, 13, 14],
        "open world: cutscene 12 only"
    );

    // Dungeon instance: both the event spawner 11 and the cutscene actor 12 are skipped; the
    // category-1 staging NPC 14 stays.
    let server = data_server_in("instance_test", spawner_data());
    let m = server
        .systems
        .npcs
        .spawns
        .get_or_create(&server.world.world(0).unwrap());
    assert_eq!(
        m.add_npc_spawner(&server, plain_def(500, 20.0, 20.0)),
        Ok(3)
    );
    assert_eq!(
        placed_ids(&m),
        vec![10, 13, 14],
        "dungeon: inactive-at-start 11 and cutscene 12"
    );

    // The event spawner the dungeon plain row skipped is still registered by its effect row.
    let mut event = plain_def(500, 30.0, 30.0);
    event.npc_spawner_ids = vec![11];
    assert_eq!(m.add_npc_spawner(&server, event), Ok(0));
    let spawned = m.get_npc_spawner(11);
    assert_eq!(spawned.len(), 1, "the activating effect finds spawner 11");
    assert!(spawned[0].force_spawn(&server, 0).is_some());
    assert_eq!(spawned[0].current_spawn_count(), 1);

    // The instance_library exclusion keeps its 'f' spawners; cutscene rules still apply.
    let server = data_server_in("instance_library", spawner_data());
    let m = server
        .systems
        .npcs
        .spawns
        .get_or_create(&server.world.world(0).unwrap());
    assert_eq!(
        m.add_npc_spawner(&server, plain_def(500, 20.0, 20.0)),
        Ok(4)
    );
    assert_eq!(
        placed_ids(&m),
        vec![10, 11, 13, 14],
        "instance_library: no indun activation, so only cutscene 12 is skipped"
    );
}

/// The real tables of one world directory and the independent re-count of what
/// `LoadNpcSpawns` must create.
#[derive(Debug, Default, PartialEq, Eq)]
struct ExpectedNpcLoad {
    entries: usize,
    duplicates: usize,
    missing_templates: usize,
    event_rows: usize,
    /// Plain rows that keep at least one start-active spawner.
    rows_with_spawners: usize,
    /// Plain rows whose every game-data spawner is event-only / cutscene.
    rows_only_event_spawners: usize,
    /// Start-active spawner clones the plain rows create.
    spawners: usize,
    /// `activation_state = 'f'` clones the plain rows no longer create (dungeon instances).
    skipped_inactive: usize,
    /// Category-0 `연출` clones the plain rows no longer create (every world).
    skipped_cutscene: usize,
}

fn world_dirs(app_path: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(app_path.join("Data/Worlds"))
        .map(|it| {
            it.filter_map(Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| {
                    let path = e.path();
                    let has_npc_spawns = std::fs::read_dir(&path).ok().is_some_and(|it| {
                        it.filter_map(Result::ok).any(|f| {
                            f.file_name()
                                .to_string_lossy()
                                .to_ascii_lowercase()
                                .starts_with("npc_spawns")
                        })
                    });
                    has_npc_spawns.then(|| (e.file_name().to_string_lossy().into_owned(), path))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn npc_spawn_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .map(|it| {
            it.filter_map(Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                    (name.starts_with("npc_spawns") && name.ends_with(".json"))
                        .then(|| (name, e.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files.reverse();
    files.into_iter().map(|(_, p)| p).collect()
}

/// Independent re-count of `SpawnManager::load_npc_spawns` (dedup, missing templates, event
/// rows, and the start-active filter), driven only by the compact DB and the shipped JSON.
fn expected_npc_load(
    world: &str,
    dir: &Path,
    npcs: &HashSet<u32>,
    data: &NpcSpawnerData,
) -> ExpectedNpcLoad {
    let mut out = ExpectedNpcLoad::default();
    let mut stored: HashMap<u32, Vec<(f32, f32, f32)>> = HashMap::new();
    for path in npc_spawn_files(dir) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let defs = parse_list(&text, NpcSpawnerDef::from_json).expect("npc_spawns.json");
        for def in defs {
            let p = &def.base.position;
            let unit = def.base.unit_id;
            if stored.get(&unit).is_some_and(|v| {
                v.iter().any(|&(sx, sy, sz)| {
                    (sx - p.x).abs() < 2.0 && (sy - p.y).abs() < 2.0 && (sz - p.z).abs() < 2.0
                })
            }) {
                out.duplicates += 1;
                continue;
            }
            if !npcs.contains(&unit) {
                out.missing_templates += 1;
                continue;
            }
            out.entries += 1;
            let event = !def.npc_spawner_ids.is_empty() && def.npc_spawner_ids != [0];
            if event {
                out.event_rows += 1;
                continue;
            }
            let ids = data.get_spawner_ids(unit).unwrap_or_default();
            if ids.is_empty() {
                // Generated ("fake") spawner from template 1.
                out.rows_with_spawners += 1;
                out.spawners += 1;
                stored.entry(unit).or_default().push((p.x, p.y, p.z));
                continue;
            }
            let mut kept = 0;
            for id in ids {
                match data.get_npc_spawner_template(id) {
                    Some(t) => match spawner_skip(world, &t) {
                        Some(SpawnerSkip::Cutscene) => out.skipped_cutscene += 1,
                        Some(SpawnerSkip::InactiveAtStart) => out.skipped_inactive += 1,
                        None => kept += 1,
                    },
                    None => kept += 1,
                }
            }
            if kept > 0 {
                out.rows_with_spawners += 1;
                out.spawners += kept;
                stored.entry(unit).or_default().push((p.x, p.y, p.z));
            } else {
                out.rows_only_event_spawners += 1;
            }
        }
    }
    out
}

/// Builds a server with the given synthetic world templates `(id, name, zone)`.
fn server_with_worlds(
    npc_spawners: NpcSpawnerData,
    worlds: impl IntoIterator<Item = (u32, String, u32)>,
) -> Arc<GameServer> {
    let mut data = GameData::empty();
    data.systems.npcs.npc_spawners = npc_spawners;
    data.worlds = WorldTemplates::from_templates(
        worlds
            .into_iter()
            .map(|(id, name, zone)| WorldTemplate::synthetic(id, &name, 2, 2, zone))
            .collect(),
        Vec::new(),
    )
    .unwrap();
    let mut config = AppConfig {
        id: 1,
        ..AppConfig::default()
    };
    config.connections.mysql_provider.host = "127.0.0.1".into();
    config.connections.mysql_provider.port = 1;
    let db = MySqlPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy_with(config.connections.mysql_provider.connect_options());
    let server = Arc::new(GameServer::new(config, db, data));
    server.set_self();
    *server.systems.npcs.spawns.test_backend.lock().unwrap() = Some(Arc::new(Backend));
    server
}

/// Builds a server whose `main_world` and the Sharpwind instance template exist, plus the
/// instance `INSTANCE_ID` and its spawn manager (the `DungeonLoaderTask` load target).
fn sharpwind_server(npc_spawners: NpcSpawnerData) -> (Arc<GameServer>, Arc<WorldInstance>) {
    let server = server_with_worlds(
        npc_spawners,
        [
            (0, "main_world".to_string(), TEST_ZONE),
            (1, WORLD.to_string(), ZONE),
        ],
    );
    let template = server
        .game_data
        .worlds
        .templates()
        .iter()
        .find(|t| t.name == WORLD)
        .expect("instance template")
        .clone();
    let world = server
        .world
        .create_world_instance(Arc::new(template), INSTANCE_ID);
    server.systems.npcs.spawns.get_or_create(&world);
    (server, world)
}

fn place_player(
    server: &Arc<GameServer>,
    world: &Arc<WorldInstance>,
    pid: u32,
    x: f32,
    y: f32,
    z: f32,
) -> PlayerRef {
    let client = TestClient::new(server, pid, pid);
    let mut ch = test_character(pid, &format!("P{pid}"), x, y);
    ch.is_online = true;
    ch.unit.transform.instance_id = world.id;
    ch.unit.transform.zone_id = ZONE;
    ch.unit.transform.set_xyz(x, y, z);
    ch.obj_id = server.ids.object.get_next_id().unwrap();
    let p = Player::new(ch, client.conn.handle());
    server.world.try_add_character(&p);
    world.spawn(&p, &server.view_ctx());
    p
}

fn npcs_of(world: &Arc<WorldInstance>, template_id: u32) -> Vec<NpcRef> {
    world
        .all_npcs()
        .into_iter()
        .filter(|n| n.data().unit.template_id == template_id)
        .collect()
}

fn npc_ids(conn: &Connection) -> HashSet<u32> {
    let mut npcs = HashSet::new();
    let mut stmt = conn.prepare("SELECT id FROM npcs").unwrap();
    for id in stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap() {
        npcs.insert(id.unwrap() as u32);
    }
    npcs
}

fn load_sharpwind(server: &Arc<GameServer>, conn: &Connection) -> LoadStats {
    load_world(server, conn, INSTANCE_ID, WORLD)
}

/// `load_spawn_files` of `Data/Worlds/<name>` for the manager of world id `world_id`.
fn load_world(server: &Arc<GameServer>, conn: &Connection, world_id: u32, name: &str) -> LoadStats {
    let npcs = npc_ids(conn);
    let none = |_: u32| false;
    let npc = |id: u32| npcs.contains(&id);
    let exists = TemplateExists {
        npc: &npc,
        doodad: &none,
        transfer: &none,
        gimmick: &none,
        slave: &none,
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("app/Data/Worlds")
        .join(name);
    let world = server.world.world(world_id).expect("world instance");
    let m = server.systems.npcs.spawns.get_or_create(&world);
    m.load_spawn_files(server, &path, &exists).unwrap()
}

/// Instance creation must place only the authored start NPCs: one Alistair 12109 near the
/// arrival point, the seven invisible trigger NPCs, and none of the event-only Sharpwind NPCs
/// (Nerta 11362/12146, Okape 12188, slimes 11361, researchers 12153/12155).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_sharpwind_instance_creation_spawns_the_authored_npcs() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let data = NpcSpawnerData::load_from(&conn).unwrap();
    let (server, world) = sharpwind_server(data);
    let stats = load_sharpwind(&server, &conn);
    eprintln!("sharpwind load: {stats:?}");
    assert_eq!(
        stats.npc_event_only_spawners_skipped, 0,
        "Sharpwind has no plain-row 'f' spawner that stays inactive"
    );
    assert_eq!(stats.npc_cutscene_spawners_skipped, 9);

    let m = server.systems.npcs.spawns.get(INSTANCE_ID).unwrap();
    let spawner_ids: Vec<u32> = m
        .get_all_spawners()
        .into_values()
        .flatten()
        .map(|s| s.spawner_id)
        .collect();
    for absent in [
        13387, 13388, // Alistair cutscene actors
        13389, 13390, 13391, // Alistair cutscene actors 2-4 (inactive)
        13392, 13393, // slimes / slime masters (inactive)
        13436, 13437, // Nerta cutscene / boss (inactive)
        13449, 13450, 13451, // researchers (inactive)
        13452, // Okape (inactive)
        13421, 13422, 13435, 13440, 13441, 13442, 13453, // trigger cutscene actors
    ] {
        assert!(
            !spawner_ids.contains(&absent),
            "event-only / cutscene spawner {absent} must not be placed at start"
        );
    }
    for present in [
        13382, 13383, // the regular Alistair spawners the plain rows use
        13394, 13395, 13396, 13397, 13398, 13399, 13400, // trigger NPCs 12113-12119
    ] {
        assert!(
            spawner_ids.contains(&present),
            "start-active spawner {present} must be placed"
        );
    }
    // The activating skills find their spawners through the event rows.
    for event_id in [94694, 94689, 94696, 90004, 94817, 94818, 94819] {
        assert_eq!(
            m.get_npc_spawner(event_id).len(),
            1,
            "event spawner {event_id}"
        );
    }

    // The arrival point sees the entrance Alistair; the whole start area is covered by the
    // 2-region visible ring plus the prewarm ring.
    let _p = place_player(&server, &world, 1, 739.4, 326.7, 166.2);
    for _ in 0..4 {
        crate::systems::npcs::spawn_manager::active_region_tick(&server);
    }
    let alistair: Vec<_> = npcs_of(&world, 12109)
        .into_iter()
        .filter(|n| {
            let t = n.data().unit.transform;
            (t.x - 718.8).abs() < 1.5 && (t.y - 327.0).abs() < 1.5 && (t.z - 167.8).abs() < 3.0
        })
        .collect();
    assert_eq!(
        alistair.len(),
        1,
        "exactly one Alistair 12109 at the arrival point"
    );
    assert!(npcs_of(&world, 12108).len() <= 1, "one Alistair 12108");
    for (unit, label) in [
        (11362, "Nerta boss"),
        (12146, "cutscene Nerta"),
        (12188, "Okape"),
        (11361, "slimes"),
        (12153, "researcher 12153"),
        (12154, "researcher 12154"),
        (12155, "researcher 12155"),
    ] {
        assert!(
            npcs_of(&world, unit).is_empty(),
            "{label} ({unit}) must not exist at instance start"
        );
    }
    // The seven invisible trigger NPCs spawn at their level positions.
    let _p2 = place_player(&server, &world, 2, 682.4, 326.4, 143.7);
    let _p3 = place_player(&server, &world, 3, 533.2, 386.7, 160.3);
    let _p4 = place_player(&server, &world, 4, 538.1, 408.8, 157.1);
    let _p5 = place_player(&server, &world, 5, 592.2, 427.5, 151.2);
    let _p6 = place_player(&server, &world, 6, 606.6, 588.5, 135.3);
    for _ in 0..6 {
        crate::systems::npcs::spawn_manager::active_region_tick(&server);
    }
    for unit in 12113..=12119 {
        assert_eq!(
            npcs_of(&world, unit).len(),
            1,
            "trigger NPC {unit} must exist exactly once"
        );
    }
}

/// Activating spawner 94694 through its `NpcSpawnerSpawnEffect` (Vera's death skill) spawns
/// exactly one boss Nerta; a second activation does not add another.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_sharpwind_vera_effect_spawns_one_nerta() {
    use crate::systems::npcs::bridges::NpcSkills;
    use crate::systems::skills::data::effects::NpcSpawnerSpawnEffect;
    use crate::systems::skills::data::{EffectKind, EffectTemplate};
    use crate::systems::skills::effects::{EffectArgs, EffectSource};
    use crate::systems::skills::ext::NpcSkillHooks;
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::types::{CastAction, SkillCastTarget, SkillCaster, SkillObject};

    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let data = NpcSpawnerData::load_from(&conn).unwrap();
    let (server, world) = sharpwind_server(data);
    let _ = load_sharpwind(&server, &conn);
    let player = place_player(&server, &world, 1, 516.3, 329.7, 166.0);
    let caster = WorldObject::from(player.clone());
    crate::systems::npcs::install_hooks(&server);
    assert!(npcs_of(&world, 11362).is_empty());
    let effect = NpcSpawnerSpawnEffect {
        id: 764,
        spawner_id: 94694,
        life_time: 0.0,
        despawn_on_creator_death: false,
        use_summoner_aggro_target: false,
        activation_state: false,
    };
    let template = EffectTemplate::NpcSpawnerSpawn(effect);
    for _ in 0..2 {
        let args = EffectArgs {
            caster: Some(caster.clone()),
            caster_caster: SkillCaster::Unit(player.obj_id),
            target: Target::Obj(caster.clone()),
            target_caster: SkillCastTarget::Unit(player.obj_id),
            cast: CastAction::Skill {
                skill_id: 19485,
                tl_id: 0,
            },
            source: EffectSource::default(),
            skill_object: &SkillObject::None,
        };
        assert!(NpcSkills.apply_effect(
            &server,
            EffectKind::NpcSpawnerSpawnEffect,
            &template,
            &args,
        ));
    }
    let nertas = npcs_of(&world, 11362);
    assert_eq!(nertas.len(), 1, "one boss Nerta after the effect");
    let t = nertas[0].data().unit.transform;
    assert!(
        (t.x - 516.301).abs() < 1.5 && (t.y - 329.663).abs() < 1.5,
        "the boss spawns at the level placement (94694), got {t:?}"
    );
}

/// Counts the spawner expansions the start-active rule skips in every shipped world (gated:
/// `AAEMU_COMPACT_DB=... cargo test -p aaemu-game sharpwind -- --ignored`). The counts are a
/// conformance snapshot: a data change that alters them must update this table deliberately.
/// `main_world` 0 'f' skips and 0 skipped rows is the guard against the unsafe global rule;
/// the open worlds skip only `연출` cutscene spawners.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_start_active_rule_counts_per_world() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let app_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let conn =
        Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let data = NpcSpawnerData::load_from(&conn).unwrap();
    let npcs = npc_ids(&conn);
    let worlds = world_dirs(&app_path);
    // One manager per world: the rule keys on the world name (instance vs open world). Instance
    // ids start at 1 because `GameServer::new` already created `main_world` as instance 0.
    let server = server_with_worlds(
        NpcSpawnerData::load_from(&conn).unwrap(),
        worlds
            .iter()
            .enumerate()
            .map(|(id, (name, _))| (id as u32 + 1, name.clone(), 1000 + id as u32)),
    );
    let mut managers = Vec::new();
    for (id, (name, _)) in worlds.iter().enumerate() {
        let template = server
            .game_data
            .worlds
            .get_world_template_by_name(name)
            .expect("world template")
            .clone();
        let world = server
            .world
            .create_world_instance(Arc::new(template), id as u32 + 1);
        managers.push(server.systems.npcs.spawns.get_or_create(&world));
    }
    let none = |_: u32| false;
    let npc = |id: u32| npcs.contains(&id);
    let exists = TemplateExists {
        npc: &npc,
        doodad: &none,
        transfer: &none,
        gimmick: &none,
        slave: &none,
    };

    let mut counts: Vec<(String, usize, usize, usize)> = Vec::new();
    for ((name, dir), m) in worlds.iter().zip(&managers) {
        let expected = expected_npc_load(name, dir, &npcs, &data);
        let stats = m.load_spawn_files(&server, dir, &exists).unwrap();
        assert_eq!(
            stats.npc_entries - stats.npc_duplicates - stats.npc_missing_templates,
            expected.entries,
            "{name}: entries"
        );
        assert_eq!(
            stats.npc_event_only_spawners_skipped, expected.skipped_inactive,
            "{name}: skipped activation_state='f' spawner expansions (dungeon instances only)"
        );
        assert_eq!(
            stats.npc_cutscene_spawners_skipped, expected.skipped_cutscene,
            "{name}: skipped category-0 연출 cutscene spawner expansions"
        );
        assert_eq!(
            stats.npc_spawners, expected.spawners,
            "{name}: start-active spawner clones (nothing that should spawn stops)"
        );
        assert_eq!(
            stats.npc_event_spawners, expected.event_rows,
            "{name}: event rows"
        );
        counts.push((
            name.clone(),
            expected.skipped_inactive,
            expected.skipped_cutscene,
            expected.rows_only_event_spawners,
        ));
    }
    eprintln!("world, 'f' skips, cutscene skips, rows whose spawners are all skipped:");
    for (name, inactive, cutscene, only_event) in &counts {
        eprintln!(
            "{name}: {inactive} 'f', {cutscene} cutscene, {only_event} rows only event spawners"
        );
    }
    assert_eq!(
        counts,
        vec![
            ("arche_mall_world".into(), 0, 0, 0),
            ("instance_burntcastle_armory".into(), 2, 2, 1),
            ("instance_cradle_of_destruction".into(), 5, 0, 0),
            ("instance_cuttingwind_deadmine".into(), 0, 9, 0),
            ("instance_hadir_farm".into(), 102, 0, 1),
            ("instance_howling_abyss".into(), 0, 0, 0),
            ("instance_howling_abyss_2".into(), 0, 0, 0),
            ("instance_immortal_isle".into(), 1, 0, 1),
            ("instance_immortal_isle_easy".into(), 1, 0, 1),
            ("instance_library".into(), 0, 0, 0),
            ("instance_nachashgar".into(), 9, 0, 1),
            ("instance_nachashgar_easy".into(), 0, 0, 0),
            ("instance_prologue".into(), 0, 0, 0),
            ("instance_sal_temple".into(), 0, 0, 0),
            ("instance_training_camp".into(), 4, 0, 4),
            ("instance_training_camp_1on1".into(), 0, 0, 0),
            ("main_world".into(), 0, 4, 0),
            ("racing_test_world".into(), 0, 0, 0),
        ]
    );
}

fn placed_spawner_ids(m: &SpawnManager) -> Vec<u32> {
    m.get_all_spawners()
        .into_values()
        .flatten()
        .map(|s| s.spawner_id)
        .collect()
}

/// The narrowed rule must leave the open worlds alone: `activation_state = 'f'` does not mean
/// "absent at start" there. Guards against reintroducing the global 'f' skip: `arche_mall_world`
/// keeps its always-present vendors and `main_world` keeps its quest-accept NPCs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_data_open_worlds_keep_inactive_at_start_npcs() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    // Instance 0 is `main_world` (created by `GameServer::new`); the test worlds get 1 and 2.
    let server = server_with_worlds(
        NpcSpawnerData::load_from(&conn).unwrap(),
        [
            (1, "arche_mall_world".to_string(), 1000),
            (2, "main_world".to_string(), 1001),
        ],
    );
    for (id, name) in [(1u32, "arche_mall_world"), (2u32, "main_world")] {
        let template = server
            .game_data
            .worlds
            .get_world_template_by_name(name)
            .expect("world template")
            .clone();
        server.world.create_world_instance(Arc::new(template), id);
    }

    // arche_mall_world: 42 plain rows of always-present mall NPCs with 'f' spawners. They must
    // all stay (the review's vendors and models), or the world is empty.
    let stats = load_world(&server, &conn, 1, "arche_mall_world");
    assert_eq!(
        stats.npc_event_only_spawners_skipped, 0,
        "arche_mall_world must not skip its 'f' vendors"
    );
    assert_eq!(stats.npc_cutscene_spawners_skipped, 0);
    let mall = placed_spawner_ids(&server.systems.npcs.spawns.get(1).unwrap());
    for (spawner, npc, name) in [
        (15192, 13639, "Wardrobe Stylist"),
        (15195, 13642, "Fairmoon Clerk"),
        (15197, 13644, "Fairmoon Clerk"),
        (15198, 13645, "Veroe Weapons Dealer"),
        (15199, 13646, "Fairmoon Clerk"),
        (15240, 13685, "Fairmoon Model"),
        (15654, 14067, "Maintenance Foreman"),
        (15655, 14068, "Premium Potion Merchant"),
        (15656, 14069, "Receptionist"),
        (15952, 14320, "All Hallows Merchant"),
    ] {
        assert!(
            mall.contains(&spawner),
            "{name} (npc {npc}) spawner {spawner} must be placed"
        );
    }

    // main_world: 0 'f' skips. The only cutscene skips are the four 뱀의틈연출 actors 3709.
    let stats = load_world(&server, &conn, 2, "main_world");
    assert_eq!(
        stats.npc_event_only_spawners_skipped, 0,
        "main_world must not skip its 'f' quest givers"
    );
    assert_eq!(
        stats.npc_cutscene_spawners_skipped, 4,
        "main_world only skips the 뱀의틈연출 cutscene actors"
    );
    let world = placed_spawner_ids(&server.systems.npcs.spawns.get(2).unwrap());
    assert_eq!(
        world.iter().filter(|&&s| s == 15350).count(),
        5,
        "the five capital Arena Managers (13792)"
    );
    for (spawner, npc, name) in [
        (15202, 13649, "Nuia Alliance Quartermaster"),
        (15205, 13652, "Mercenary Quartermaster"),
        (15216, 13663, "Harani Alliance Quartermaster"),
        (15102, 13559, "Liteia"),
        (15103, 13560, "Baron Alfonso"),
        (15104, 13561, "Bard Pagos"),
        (11542, 10393, "Pavitra's Spirit"),
        (15280, 13725, "Arcane Soul"),
        (7948, 7152, "Century Root"),
    ] {
        assert!(
            world.contains(&spawner),
            "{name} (npc {npc}) spawner {spawner} must be placed"
        );
    }
}
