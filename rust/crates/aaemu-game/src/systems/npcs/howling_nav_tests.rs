//! Greater Howling Abyss (`instance_howling_abyss_2`, zone 280) movement, chase and aggro, with
//! the 1.2 compact database and, for the ground, the client's collision and navigation graph.
//!
//! `$env:AAEMU_COMPACT_DB = '...\compact.sqlite3'; $env:AAEMU_GAME_PAK = 'C:\AA1.2\game_pak';
//! cargo test -p aaemu-game --lib howling_nav -- --ignored`
//!
//! The tests that need the ground return early without `AAEMU_GAME_PAK`.

use std::{path::Path, sync::Arc, time::Duration};

use crate::data::client_files::ClientFileManager;
use crate::data::world_templates::WorldTemplates;
use crate::data::GameData;
use crate::server::GameServer;
use crate::systems::npcs::heightmap::{HeightmapConfig, HeightmapWorlds};
use crate::systems::skills::{skill, types::skill_result};
use crate::test_support::{spawn_player, test_character, TestClient};
use crate::world::{NpcRef, PlayerRef, WorldInstance, WorldObject};

const GREATER: &str = "instance_howling_abyss_2";
const ZONE: u32 = 280;

fn app_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("app")
}

/// The Greater instance with the compact database (and, with `pak`, the client's collision,
/// navigation graph and `GeoDataMode`, like the live server).
fn server(pak: Option<String>) -> Option<(Arc<GameServer>, Arc<WorldInstance>)> {
    let db = std::env::var("AAEMU_COMPACT_DB").ok()?;
    let Some(pak) = pak else {
        let (server, world) =
            super::burntcastle_dungeon_tests::real_encounter_server(&db, GREATER, ZONE);
        return Some((server, world));
    };
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let app = app_path();
    let files = Arc::new(ClientFileManager::from_sources(&[pak], &app));
    let ctx = crate::systems::DataLoadCtx {
        conn: &conn,
        app_path: &app,
        files: &files,
        language: "en_us",
    };
    let mut data = GameData::empty();
    data.skills = crate::data::SkillData::load(&conn).expect("buff and skill metadata");
    data.formulas = crate::data::formulas::FormulaManager::load(&conn).expect("formulas");
    let localization = crate::data::world_data::Localization::load(&conn, "en_us").unwrap();
    data.factions = crate::data::world_data::FactionData::load(&conn, &localization).unwrap();
    data.items = crate::data::items::ItemData::load(&conn).expect("items");
    data.systems.economy.loot =
        crate::systems::economy::loot_data::LootData::load(&conn).expect("loot packs");
    data.systems.skills = crate::systems::skills::Data::load(&ctx).expect("skills data");
    data.systems.npcs = crate::systems::npcs::Data::load(&ctx).expect("npcs data");
    data.systems.ai = crate::systems::ai::Data::load(&ctx).expect("ai data");
    let worlds = WorldTemplates::load(&files, &app).expect("world templates");
    let template = worlds
        .get_world_template_by_name(GREATER)
        .expect("Greater Howling template")
        .clone();
    let mut config = crate::config::AppConfig {
        id: 1,
        ..Default::default()
    };
    config.world.geo_data_mode = true;
    let server = Arc::new(crate::test_support::test_server_with_world_config(
        data,
        template.clone(),
        config,
    ));
    server.set_self();
    let heights = Arc::new(HeightmapWorlds::new(
        files.clone(),
        &server.game_data.worlds,
        HeightmapConfig {
            heightmaps_enable: true,
            geo_data_mode: true,
            pre_load_terrain: false,
            placement_strict_surface: false,
        },
    ));
    server.systems.npcs.heightmaps.install(heights);
    crate::systems::ai::geodata::start(&server);
    let world = server.world.create_world_instance(Arc::new(template), 0);
    crate::systems::skills::bind(&server);
    crate::systems::npcs::install_hooks(&server);
    crate::systems::ai::hooks::install(&server);
    Some((server, world))
}

/// [`server`] with the ground (`None` without `AAEMU_GAME_PAK`).
fn ground_server() -> Option<(Arc<GameServer>, Arc<WorldInstance>)> {
    server(Some(std::env::var("AAEMU_GAME_PAK").ok()?))
}

fn npc_at(server: &GameServer, world: &WorldInstance, template: u32, xyz: [f32; 3]) -> NpcRef {
    let npc =
        super::npc::manager::create(server, server.ids.object.get_next_id().unwrap(), template)
            .expect("Howling NPC template");
    {
        let mut data = npc.data();
        data.unit.transform.set_xyz(xyz[0], xyz[1], xyz[2]);
        data.unit.transform.zone_id = ZONE;
        data.unit.transform.instance_id = world.id;
        data.unit.transform.world_id = world.id;
    }
    world.spawn(&npc, &server.view_ctx());
    crate::systems::ai::hooks::spawn_positioned(server, &npc);
    npc.data().spawner_z = Some(xyz[2]);
    npc
}

fn player_at(server: &Arc<GameServer>, id: u32, xyz: [f32; 3]) -> (TestClient, PlayerRef) {
    let client = TestClient::new(server, id, id);
    let mut ch = test_character(id, "Tester", xyz[0], xyz[1]);
    ch.unit.level = 50;
    ch.unit.hp = 1_000_000;
    ch.unit.max_hp = 1_000_000;
    ch.unit.transform.set_xyz(xyz[0], xyz[1], xyz[2]);
    ch.unit.transform.zone_id = ZONE;
    let player = spawn_player(server, &client, ch, 0x10_0000 + id);
    (client, player)
}

fn move_player(server: &GameServer, world: &WorldInstance, player: &PlayerRef, xyz: [f32; 3]) {
    player.data().unit.transform.set_xyz(xyz[0], xyz[1], xyz[2]);
    world.add_visible_object(player, &server.view_ctx());
}

/// One 100 ms AI tick; the victim is kept alive.
async fn tick(server: &GameServer, npc: &NpcRef, victim: &PlayerRef) {
    crate::systems::ai::manager::run_now(server, npc.obj_id, |ai| ai.tick(100, true));
    victim.data().unit.hp = 1_000_000;
    tokio::time::sleep(Duration::from_millis(100)).await;
}

fn pos(npc: &NpcRef) -> [f32; 3] {
    let t = npc.data().unit.transform;
    [t.x, t.y, t.z]
}

fn aggro_on(npc: &NpcRef, player: &PlayerRef) -> bool {
    npc.data()
        .unit
        .skill_state
        .aggro_table
        .contains_key(&player.obj_id)
}

/// The collision floor within a metre above or six below `z`.
fn floor(server: &GameServer, x: f32, y: f32, z: f32) -> Option<f32> {
    crate::systems::ai::collision::ground_height_probe(server, 0, x, y, z, 1.0, 6.0).flatten()
}

fn dist_2d(a: [f32; 3], b: [f32; 3]) -> f32 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

/// #2: the bridge model of the first corridor (`sal_temple_brige02.cgf`) keeps its physics
/// proxy under a root node turned by 90 degrees. Without that turn the stairs from the
/// corridor up to the entrance platform and the platform itself had no collision, so the
/// corridor's mobs took the floor underneath and walked inside the platform.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB and AAEMU_GAME_PAK"]
async fn real_howling_nav_first_corridor_mobs_climb_the_stairs_onto_the_platform() {
    let Some((server, world)) = ground_server() else {
        return;
    };
    // The stairs rise along y = 732 from the corridor (181.8) to the platform (184.8).
    let mut last = 0.0f32;
    for x in [650.0f32, 652.0, 654.0, 656.0, 658.0] {
        let z = floor(&server, x, 732.0, 186.0).expect("a stair tread");
        assert!(
            z + 0.3 >= last,
            "the stairs rise at x {x}: {z} after {last}"
        );
        last = z;
    }
    assert!(last > 184.4, "the stairs reach the platform ({last})");
    let platform = floor(&server, 664.0, 736.0, 186.0).expect("the platform floor");
    assert!((platform - 184.75).abs() < 0.2, "platform at {platform}");

    let combatant = npc_at(&server, &world, 12773, [645.0, 733.0, 181.85]);
    let (_client, player) = player_at(&server, 1, [666.0, 736.0, 184.75]);
    crate::systems::ai::hooks::engage(&server, &combatant, &WorldObject::from(&player));
    let target = [666.0, 736.0, 184.75];
    for _ in 0..100 {
        tick(&server, &combatant, &player).await;
        let p = pos(&combatant);
        if p[0] > 655.0 {
            let under = floor(&server, p[0], p[1], p[2]).unwrap_or(f32::MIN);
            assert!(
                (p[2] - under).abs() < 0.6,
                "the combatant walks on the floor at {p:?} (floor {under})"
            );
        }
        if dist_2d(p, target) < 3.0 {
            assert!(p[2] > 184.3, "the combatant is on the platform: {p:?}");
            return;
        }
    }
    panic!(
        "the combatant should reach the platform: {:?}",
        pos(&combatant)
    );
}

/// The walkable points of the dungeon's navigation graph stand on the collision floor: 2 180
/// of 2 198 with the CGF node transforms, 2 114 (51 over no floor at all) without them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB and AAEMU_GAME_PAK"]
async fn real_howling_nav_waypoints_stand_on_the_collision_floor() {
    let Some((server, world)) = ground_server() else {
        return;
    };
    let geo = server.systems.ai.geodata.get(world.template.id).unwrap();
    for x in (300..=720).step_by(20) {
        for y in (420..=760).step_by(20) {
            let _ = geo.get_height(crate::systems::skills::controllers::Vec3::new(
                x as f32, y as f32, 180.0,
            ));
        }
    }
    let nodes: Vec<_> = geo
        .graph()
        .nodes
        .iter()
        .filter(|n| n.pos.z > 140.0 && (280.0..=740.0).contains(&n.pos.x))
        .map(|n| n.pos)
        .collect();
    let on_floor = nodes
        .iter()
        .filter(|p| floor(&server, p.x, p.y, p.z).is_some_and(|h| (h - p.z).abs() <= 0.6))
        .count();
    let without = nodes
        .iter()
        .filter(|p| floor(&server, p.x, p.y, p.z).is_none())
        .count();
    assert!(nodes.len() > 2_000, "{} waypoints", nodes.len());
    assert!(
        on_floor * 100 >= nodes.len() * 98 && without == 0,
        "{on_floor} of {} waypoints on a floor, {without} over none",
        nodes.len()
    );
}

/// #20: the Knights on the walkway above the Medusa room (15 m down) chased a player who
/// jumped into the room through the air. A chaser now stops at the ledge and gives the victim
/// up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB and AAEMU_GAME_PAK"]
async fn real_howling_nav_walkway_knight_does_not_follow_a_player_down_into_the_room() {
    let Some((server, world)) = ground_server() else {
        return;
    };
    let knight = npc_at(&server, &world, 12765, [433.1, 593.8, 170.1]);
    let (_client, player) = player_at(&server, 1, [463.0, 598.0, 154.9]);
    crate::systems::ai::hooks::engage(&server, &knight, &WorldObject::from(&player));
    assert!(aggro_on(&knight, &player));
    let start = pos(&knight);
    for _ in 0..150 {
        tick(&server, &knight, &player).await;
        let p = pos(&knight);
        let under = floor(&server, p[0], p[1], p[2]).unwrap_or(f32::MIN);
        assert!(
            (p[2] - under).abs() < 0.6,
            "the knight stays on a floor: {p:?} over {under}"
        );
        if !aggro_on(&knight, &player) {
            assert!(dist_2d(p, start) > 5.0, "the knight chased first: {p:?}");
            return;
        }
    }
    panic!(
        "the knight should give up the player below: {:?}",
        pos(&knight)
    );
}

/// #13: the Medusa pack's help distance (30 m) is horizontal, so the Guardians answered the
/// Knights fighting on the walkway 15 m above their room. A Guardian on the player's floor
/// still answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_nav_medusa_pack_ignores_the_fight_on_the_walkway() {
    let Some((server, world)) = server(None) else {
        return;
    };
    let knight = npc_at(&server, &world, 12765, [435.7, 592.5, 170.2]);
    let guardian = npc_at(&server, &world, 12785, [461.7, 587.3, 154.9]);
    let medusa = npc_at(&server, &world, 12784, [466.1, 585.6, 154.9]);
    // Control: the same template on the walkway's floor, as far from the Knight.
    let walkway_guardian = npc_at(&server, &world, 12785, [455.0, 600.0, 170.2]);
    let (_client, player) = player_at(&server, 1, [438.0, 593.0, 170.2]);
    crate::systems::ai::hooks::engage(&server, &knight, &WorldObject::from(&player));
    for _ in 0..10 {
        tick(&server, &knight, &player).await;
    }
    assert!(
        aggro_on(&walkway_guardian, &player),
        "a Guardian on the player's floor answers the Knight"
    );
    assert!(!aggro_on(&guardian, &player), "the room's Guardian stays");
    assert!(!aggro_on(&medusa, &player), "Medusa stays");
}

/// #6 / #15: a boss follows a victim around its room for the whole fight. Viboros kept the
/// 30 m weapon range of a fallback pick and stood 15 m away; Musperosa's Hellfire Breath
/// (21049) repeated its single-ticket last pulse forever and kept her casting in the centre.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_nav_bosses_follow_their_victim_around_the_room() {
    let Some((server, world)) = server(None) else {
        return;
    };
    // Some boss strikes take most of a victim's health at once (GodMode: no damage to players).
    let mut skills = server.systems.skills.config();
    skills.god_mode = true;
    server.systems.skills.set_config(skills);
    for (i, (template, centre)) in [
        (12762, [664.9f32, 585.0, 196.3]),
        (12758, [463.4f32, 679.2, 167.0]),
    ]
    .into_iter()
    .enumerate()
    {
        let boss = npc_at(&server, &world, template, centre);
        let spot = |a: f32| {
            [
                centre[0] + 12.0 * a.cos(),
                centre[1] + 12.0 * a.sin(),
                centre[2],
            ]
        };
        let (_client, player) = player_at(&server, i as u32 + 1, spot(0.0));
        let victim = WorldObject::from(&player);
        let engaged_tick = || async {
            tick(&server, &boss, &player).await;
            assert!(aggro_on(&boss, &player), "boss {template} keeps its victim");
        };
        crate::systems::ai::hooks::engage(&server, &boss, &victim);
        for round in 1..=4 {
            // Let the boss fight for a while, then step across the room.
            for _ in 0..60 {
                engaged_tick().await;
            }
            let target = spot(round as f32 * 2.4);
            move_player(&server, &world, &player, target);
            let mut reached = false;
            for _ in 0..200 {
                engaged_tick().await;
                if dist_2d(pos(&boss), target) < 6.0 {
                    reached = true;
                    break;
                }
            }
            assert!(
                reached,
                "boss {template} follows the victim (round {round}): boss at {:?}, victim at \
                 {target:?}",
                pos(&boss)
            );
        }
        world.delete(&boss);
    }
}

/// #15: Hellfire Breath (plot 710) ends. Its last pulse (event 5793) loops onto itself with one
/// ticket, which the C# ticket rule never stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_nav_musperosa_hellfire_breath_ends() {
    let Some((server, world)) = server(None) else {
        return;
    };
    let boss = npc_at(&server, &world, 12758, [463.4, 679.2, 167.0]);
    let (_client, player) = player_at(&server, 1, [463.4, 672.0, 167.0]);
    let caster = WorldObject::from(&boss);
    crate::systems::ai::npc::set_current_aggro_target(&boss, Some(&WorldObject::from(&player)));
    assert_eq!(
        skill::unit_use_skill(&server, &caster, 21049, &caster),
        skill_result::SUCCESS
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(boss.data().unit.skill_state.active_plot.is_some());
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if boss.data().unit.skill_state.active_plot.is_none() {
            return;
        }
    }
    panic!("Hellfire Breath should end");
}

/// Floor dump for maps (diagnostic): `AAEMU_HA_DUMP="x0,y0,x1,y1,step,ztop,zbottom"` writes
/// `x,y,navmesh,floor1,floor2,...` rows (collision floors from the top down) to `AAEMU_HA_OUT`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB, AAEMU_GAME_PAK, AAEMU_HA_DUMP and AAEMU_HA_OUT"]
async fn real_howling_nav_dump_floors() {
    let (Ok(spec), Ok(out)) = (
        std::env::var("AAEMU_HA_DUMP"),
        std::env::var("AAEMU_HA_OUT"),
    ) else {
        return;
    };
    let Some((server, world)) = ground_server() else {
        return;
    };
    let v: Vec<f32> = spec.split(',').map(|s| s.trim().parse().unwrap()).collect();
    let (x0, y0, x1, y1, step, ztop, zbot) = (v[0], v[1], v[2], v[3], v[4], v[5], v[6]);
    let geo = server.systems.ai.geodata.get(world.template.id).unwrap();
    let mut text = String::new();
    let mut y = y0;
    while y <= y1 {
        let mut x = x0;
        while x <= x1 {
            let mut floors = Vec::new();
            let mut z = ztop;
            while z > zbot && floors.len() < 6 {
                match crate::systems::ai::collision::ground_height_probe(
                    &server,
                    0,
                    x,
                    y,
                    z,
                    0.0,
                    z - zbot,
                ) {
                    Some(Some(h)) => {
                        floors.push(h);
                        z = h - 1.0;
                    }
                    _ => break,
                }
            }
            let hint = floors.first().copied().unwrap_or(ztop);
            let p = crate::systems::skills::controllers::Vec3::new(x, y, hint);
            let nav = geo.get_height(p);
            text.push_str(&format!(
                "{x},{y},{nav:.2},{}\n",
                floors
                    .iter()
                    .map(|f| format!("{f:.2}"))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            x += step;
        }
        y += step;
    }
    std::fs::write(out, text).unwrap();
}

/// #2 live follow-up: the route up from the spot where the corridor mobs stand to a player
/// on the platform beside the entrance gate goes over the stairs (y 730-733), not along the
/// trench beside them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB and AAEMU_GAME_PAK"]
async fn real_howling_nav_corridor_mob_route_up_takes_the_stairs() {
    let Some((server, world)) = ground_server() else {
        return;
    };
    // The player's spot of the live check; the corridor mobs' spawns and where they stood.
    let target = [668.0, 738.0, 185.5];
    let goal = crate::systems::skills::controllers::Vec3::new(target[0], target[1], target[2]);
    for from in [
        [638.9, 732.5, 181.8],
        [641.5, 732.7, 181.9],
        [639.8, 735.0, 181.9],
        [641.08, 733.92, 181.85],
        [646.0, 735.5, 181.85],
    ] {
        let mob = npc_at(&server, &world, 12773, from);
        let route = crate::systems::ai::manager::run_now(&server, mob.obj_id, |c| {
            crate::systems::ai::geodata::plan_route(c, goal);
            c.ai.path_node
                .found_path
                .iter()
                .copied()
                .collect::<Vec<_>>()
        })
        .unwrap();
        assert!(
            route
                .iter()
                .any(|p| (650.0..=659.0).contains(&p.x) && p.y < 733.6),
            "the route from {from:?} climbs the stairs: {route:?}"
        );
        world.delete(&mob);
    }
    let start = [639.5, 733.7, 181.85];
    let combatant = npc_at(&server, &world, 12773, start);
    let (_client, player) = player_at(&server, 1, target);
    crate::systems::ai::hooks::engage(&server, &combatant, &WorldObject::from(&player));
    for _ in 0..120 {
        tick(&server, &combatant, &player).await;
        let p = pos(&combatant);
        assert!(p[2] > 181.0, "the combatant stays out of the trench: {p:?}");
        if (655.0..=659.0).contains(&p[0]) {
            assert!(p[1] < 734.0, "the combatant is on the stairs: {p:?}");
        }
        if dist_2d(p, target) < 3.0 {
            assert!(p[2] > 184.3, "the combatant is on the platform: {p:?}");
            return;
        }
    }
    panic!(
        "the combatant should reach the platform: {:?}",
        pos(&combatant)
    );
}

/// The chase's ledge wait (`ai::framework::LedgeBlock`) of the NPC.
fn ledge_wait(server: &GameServer, npc: &NpcRef) -> bool {
    crate::systems::ai::manager::run_now(server, npc.obj_id, |c| c.ai.ledge_block.is_some())
        .unwrap_or(false)
}

/// #20 live follow-up: a walkway Knight that stopped at the ledge turned back after about a
/// second, not three. When the victim landed in the room below, the chase planned a new route.
/// The room is not reachable from the walkway, and the search, cut after a number of
/// iterations, ended on a node 106 m from the victim. The Knight followed that route back
/// along the walkway, and its first possible step ended the wait. It then walked until the
/// 50 m leash. Now a cut route that leads away is no route, so the Knight waits at the edge
/// and gives the victim up 3 s after it stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB and AAEMU_GAME_PAK"]
async fn real_howling_nav_walkway_knight_waits_three_seconds_at_the_ledge() {
    let Some((server, world)) = ground_server() else {
        return;
    };
    let knight = npc_at(&server, &world, 12765, [433.1, 593.8, 170.1]);
    let (_client, player) = player_at(&server, 1, [437.8, 592.5, 170.2]);
    crate::systems::ai::hooks::engage(&server, &knight, &WorldObject::from(&player));
    // The live walk off the east end of the walkway (ha2-walkway-track.txt), 0.3 s apart.
    let landing = floor(&server, 456.92, 594.06, 158.0).expect("the room floor");
    let track: Vec<[f32; 3]> = [
        [438.79, 592.77, 170.2],
        [439.42, 593.15, 170.2],
        [440.51, 593.43, 170.2],
        [441.88, 593.56, 170.2],
        [443.05, 593.92, 170.2],
        [444.23, 594.28, 170.2],
        [445.40, 594.64, 170.2],
        [447.16, 595.17, 170.2],
        [448.50, 595.57, 170.2],
        [449.78, 595.69, 170.2],
        [451.34, 595.38, 170.2],
        [452.90, 595.01, 169.0],
        [454.47, 594.64, 165.5],
        [456.04, 594.27, 160.2],
        [456.92, 594.06, landing],
    ]
    .to_vec();
    for _ in 0..20 {
        tick(&server, &knight, &player).await;
    }
    let mut stop: Option<(std::time::Instant, [f32; 3])> = None;
    for i in 0..150 {
        if i % 3 == 0 {
            if let Some(p) = track.get(i / 3) {
                move_player(&server, &world, &player, *p);
            }
        }
        tick(&server, &knight, &player).await;
        let p = pos(&knight);
        if stop.is_none() && ledge_wait(&server, &knight) {
            stop = Some((std::time::Instant::now(), p));
        }
        let Some((since, at)) = stop else {
            continue;
        };
        if !aggro_on(&knight, &player) {
            let waited = since.elapsed().as_secs_f32();
            assert!(
                (2.8..=3.8).contains(&waited),
                "the knight gave up {waited} s after it stopped"
            );
            return;
        }
        assert!(
            dist_2d(p, at) < 1.0,
            "the knight waits at the edge {at:?}, not at {p:?}"
        );
    }
    panic!(
        "the knight should stop at the edge and give up: {:?}, stop {stop:?}",
        pos(&knight)
    );
}

/// Calleil the Cursed (12759) followed a victim who left his room to the north only to the
/// gate (6360 at (329.5, 523.9)). Along the gate line the collision has a strip about a metre
/// wide without floor, so his chase took it for a ledge, though the players and the waypoint
/// graph cross it. He now follows the victim into the corridor behind the gate, and once the
/// victim is out of his 40 m leash he returns home and drops the victim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB and AAEMU_GAME_PAK"]
async fn real_howling_nav_calleil_follows_a_victim_out_through_the_north_gate() {
    let Some((server, world)) = ground_server() else {
        return;
    };
    let mut skills = server.systems.skills.config();
    skills.god_mode = true;
    server.systems.skills.set_config(skills);
    let home = [322.4f32, 500.4, 170.8];
    let boss = npc_at(&server, &world, 12759, home);
    let (_client, player) = player_at(&server, 1, [322.0, 508.0, 170.4]);
    crate::systems::ai::hooks::engage(&server, &boss, &WorldObject::from(&player));
    for _ in 0..20 {
        tick(&server, &boss, &player).await;
    }
    let corridor = [327.0f32, 531.0];
    let z = floor(&server, corridor[0], corridor[1], 171.4).expect("the corridor floor");
    let corridor = [corridor[0], corridor[1], z];
    move_player(&server, &world, &player, corridor);
    let mut followed = false;
    for _ in 0..250 {
        tick(&server, &boss, &player).await;
        assert!(
            aggro_on(&boss, &player),
            "Calleil keeps the victim behind the gate: at {:?}",
            pos(&boss)
        );
        let p = pos(&boss);
        if p[1] > 526.0 && dist_2d(p, corridor) < 4.0 {
            followed = true;
            break;
        }
    }
    assert!(
        followed,
        "Calleil follows through the gate: at {:?}",
        pos(&boss)
    );
    // The hall north of the corridor lies beyond his leash.
    let hall = [310.0f32, 556.0];
    let z = floor(&server, hall[0], hall[1], 171.4).expect("the hall floor");
    move_player(&server, &world, &player, [hall[0], hall[1], z]);
    for _ in 0..400 {
        tick(&server, &boss, &player).await;
        if !aggro_on(&boss, &player) && dist_2d(pos(&boss), home) < 3.0 {
            return;
        }
    }
    panic!(
        "Calleil should return home: at {:?}, aggro {}",
        pos(&boss),
        aggro_on(&boss, &player)
    );
}

// Normal Howling Abyss (`instance_howling_abyss`, zone 265): the same rooms with the Normal
// templates (Knight 11258, Guardian 11757 and Medusa 11759 for Greater's 12765 / 12785 /
// 12784).

const NORMAL: &str = "instance_howling_abyss";
const NORMAL_ZONE: u32 = 265;

fn normal_server() -> Option<(Arc<GameServer>, Arc<WorldInstance>)> {
    let db = std::env::var("AAEMU_COMPACT_DB").ok()?;
    Some(super::burntcastle_dungeon_tests::real_encounter_server(
        &db,
        NORMAL,
        NORMAL_ZONE,
    ))
}

fn normal_npc_at(
    server: &GameServer,
    world: &WorldInstance,
    template: u32,
    xyz: [f32; 3],
) -> NpcRef {
    let npc =
        super::npc::manager::create(server, server.ids.object.get_next_id().unwrap(), template)
            .expect("Howling NPC template");
    {
        let mut data = npc.data();
        data.unit.transform.set_xyz(xyz[0], xyz[1], xyz[2]);
        data.unit.transform.zone_id = NORMAL_ZONE;
        data.unit.transform.instance_id = world.id;
        data.unit.transform.world_id = world.id;
    }
    world.spawn(&npc, &server.view_ctx());
    crate::systems::ai::hooks::spawn_positioned(server, &npc);
    npc.data().spawner_z = Some(xyz[2]);
    npc
}

fn normal_player_at(server: &Arc<GameServer>, id: u32, xyz: [f32; 3]) -> (TestClient, PlayerRef) {
    let client = TestClient::new(server, id, id);
    let mut ch = test_character(id, "Tester", xyz[0], xyz[1]);
    ch.unit.level = 50;
    ch.unit.hp = 1_000_000;
    ch.unit.max_hp = 1_000_000;
    ch.unit.transform.set_xyz(xyz[0], xyz[1], xyz[2]);
    ch.unit.transform.zone_id = NORMAL_ZONE;
    let player = spawn_player(server, &client, ch, 0x10_0000 + id);
    (client, player)
}

/// A3 on Normal: the Medusa pack under the walkway does not answer the Knight fighting 15 m
/// above its room. A Guardian on the walkway's floor does.
#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_nav_normal_medusa_pack_ignores_the_fight_on_the_walkway() {
    let Some((server, world)) = normal_server() else {
        return;
    };
    let knight = normal_npc_at(&server, &world, 11258, [435.7, 592.5, 170.2]);
    let guardian = normal_npc_at(&server, &world, 11757, [461.7, 587.3, 154.9]);
    let medusa = normal_npc_at(&server, &world, 11759, [466.1, 585.6, 154.9]);
    let walkway_guardian = normal_npc_at(&server, &world, 11757, [455.0, 600.0, 170.2]);
    let (_client, player) = normal_player_at(&server, 1, [438.0, 593.0, 170.2]);
    crate::systems::ai::hooks::engage(&server, &knight, &WorldObject::from(&player));
    for _ in 0..10 {
        tick(&server, &knight, &player).await;
    }
    assert!(
        aggro_on(&walkway_guardian, &player),
        "a Guardian on the player's floor answers the Knight"
    );
    assert!(!aggro_on(&guardian, &player), "the room's Guardian stays");
    assert!(!aggro_on(&medusa, &player), "Medusa stays");
}
