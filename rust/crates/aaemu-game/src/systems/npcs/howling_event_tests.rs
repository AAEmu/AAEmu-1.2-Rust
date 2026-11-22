//! Howling Abyss follow-ups: Godfrey's scripted event, Calleil's water and the dry passages.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::data::client_files::ClientFileManager;
use crate::data::world_templates::WorldTemplates;
use crate::server::GameServer;
use crate::systems::ai::physics::objects_file::tests::{object_dat, water_record};
use crate::systems::ai::physics::objects_file::ObjectsFile;
use crate::systems::ai::physics::water::WaterBodies;
use crate::systems::npcs::heightmap::{HeightmapConfig, HeightmapWorlds};
use crate::systems::skills::controllers::Vec3;
use crate::systems::skills::{skill, types::skill_result};
use crate::test_support::{spawn_player, test_character, TestClient};
use crate::world::{NpcRef, PlayerRef, WorldInstance, WorldObject};

use super::{burntcastle_dungeon_tests::real_encounter_server, spawn_manager::TemplateExists};

const GREATER: &str = "instance_howling_abyss_2";
const ZONE: u32 = 280;

fn app_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("app")
}

/// A 1.2 level river sector: a 30 x 10 m quad whose shape and physics contour are the same
/// four corners (every Howling Abyss water volume has this layout).
fn river_sector() -> ObjectsFile {
    let quad = [
        [100.0, 100.0, 0.0],
        [100.0, 110.0, 0.0],
        [130.0, 110.0, 0.0],
        [130.0, 100.0, 0.0],
    ];
    let block = water_record(2, [0.0, 0.0, 180.0], [0.0, 0.0, 184.0], 19.6, &quad, &quad);
    ObjectsFile::read(&object_dat(&block))
}

#[test]
fn howling_river_sectors_only_cover_their_contour() {
    // Beside the quad, 15 m off its edge, two metres under its surface.
    let passage = Vec3::new(115.0, 125.0, 182.0);
    let pool = Vec3::new(115.0, 105.0, 182.0);
    let open_world = WaterBodies::new(100.0);
    open_world.add_cell_objects(None, 100.0, (0, 0), &river_sector(), false);
    assert!(
        open_world.is_water(passage).is_some(),
        "the open-world corridor heuristic is unchanged"
    );
    let howling = WaterBodies::new(100.0);
    howling.add_cell_objects(None, 100.0, (0, 0), &river_sector(), true);
    assert!(howling.is_water(passage).is_none(), "the passage stays dry");
    assert!(howling.is_water(pool).is_some(), "the pool is still water");
    assert_eq!(howling.get_water_surface(pool).0, 184.0);
}

/// Every water volume the server ingests from a world's cells, as the live server reads them.
fn cell_water(pak: &str, world_name: &str) -> WaterBodies {
    let files = Arc::new(ClientFileManager::from_sources(&[pak.into()], &app_path()));
    let worlds = WorldTemplates::load(&files, &app_path()).expect("world templates");
    let t = worlds
        .get_world_template_by_name(world_name)
        .expect("world")
        .clone();
    let hm = HeightmapWorlds::new(
        files.clone(),
        &worlds,
        HeightmapConfig {
            heightmaps_enable: true,
            geo_data_mode: false,
            pre_load_terrain: false,
            placement_strict_surface: false,
        },
    );
    let world_hm = hm.template(t.id).expect("heightmap");
    for cx in 0..t.cell_x {
        for cy in 0..t.cell_y {
            let (x, y) = (cx as f32 * 1024.0 + 512.0, cy as f32 * 1024.0 + 512.0);
            let _ = world_hm.get_height(&files, x, y);
        }
    }
    let water = WaterBodies::new(t.ocean_level);
    water.sync_loaded_cells(world_hm, &files, &t.name, t.ocean_level);
    water
}

/// Both Howling Abyss worlds read their nine river sectors as the sector quads. The 40 m
/// river corridor around them put the first corridor (the Blackscale guards past the entrance
/// at 181.9 m stand under a 184.6 m surface), Godfrey's room and the Wailing Ranch approach
/// under water, so players got a breath meter and drowned on dry floors.
/// `AAEMU_GAME_PAK=C:\AA1.2\game_pak cargo test -p aaemu-game real_howling_floors -- --ignored`
#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_howling_floors_outside_the_pools_are_dry() {
    let Ok(pak) = std::env::var("AAEMU_GAME_PAK") else {
        return;
    };
    for name in ["instance_howling_abyss", GREATER] {
        let water = cell_water(&pak, name);
        assert_eq!(water.area_count(), 9, "{name}");
        for [x, y, z] in [
            [641.496, 732.742, 181.9026],
            [638.856, 732.548, 181.8246],
            [552.228, 487.354, 187.977],
            [545.56, 544.614, 189.7187],
            [522.544, 542.466, 189.6486],
            [693.7, 727.3, 185.4],
        ] {
            assert!(
                water.is_water(Vec3::new(x, y, z)).is_none(),
                "{name}: ({x}, {y}, {z}) is dry floor"
            );
        }
        // Inside the sector quads below their surfaces.
        assert!(water.is_water(Vec3::new(587.0, 716.0, 192.0)).is_some());
        assert!(water.is_water(Vec3::new(700.0, 725.0, 184.0)).is_some());
    }
}

fn load_greater(db: &str) -> (Arc<GameServer>, Arc<WorldInstance>) {
    let (server, world) = real_encounter_server(db, GREATER, ZONE);
    let data = &server.game_data.systems.npcs;
    let no = |_: u32| false;
    let exists = TemplateExists {
        npc: &|id| data.npc.exist(id),
        doodad: &|id| data.doodad.exist(id),
        transfer: &no,
        gimmick: &no,
        slave: &no,
    };
    let folder = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("app/Data/Worlds")
        .join(GREATER);
    server
        .systems
        .npcs
        .spawns
        .get_or_create(&world)
        .load_spawn_files(&server, &folder, &exists)
        .unwrap();
    (server, world)
}

fn npc_at(server: &GameServer, world: &WorldInstance, template: u32, xyz: [f32; 3]) -> NpcRef {
    let npc =
        super::npc::manager::create(server, server.ids.object.get_next_id().unwrap(), template)
            .expect("Howling NPC template");
    {
        let mut data = npc.data();
        data.unit.transform.set_xyz(xyz[0], xyz[1], xyz[2]);
        data.unit.transform.zone_id = ZONE;
    }
    world.spawn(&npc, &server.view_ctx());
    npc
}

fn player_at(server: &Arc<GameServer>, id: u32, xyz: [f32; 3]) -> (TestClient, PlayerRef) {
    let client = TestClient::new(server, id, id);
    let mut ch = test_character(id, "Tester", xyz[0], xyz[1]);
    ch.unit.level = 50;
    ch.unit.hp = 10_000;
    ch.unit.max_hp = 10_000;
    ch.unit.transform.set_xyz(xyz[0], xyz[1], xyz[2]);
    ch.unit.transform.zone_id = ZONE;
    let player = spawn_player(server, &client, ch, 0x10_0000 + id);
    (client, player)
}

/// `SCDoodadPhaseChangedPacket` bodies start with the doodad's 3-byte id and the new group.
fn phase_changes(frames: &[(u16, Vec<u8>)], obj_id: u32) -> Vec<u32> {
    frames
        .iter()
        .filter(|(opcode, body)| {
            *opcode == crate::opcodes::sc::SCDoodadPhaseChangedPacket
                && body.len() >= 7
                && body[..3] == obj_id.to_le_bytes()[..3]
        })
        .map(|(_, body)| u32::from_le_bytes([body[3], body[4], body[5], body[6]]))
        .collect()
}

/// Raise Calleil's Water reaches up to ten area targets, and sixteen props answer it: eleven
/// waterfalls, four lifting spouts and the water volume. The volume's anchor is a corner of the
/// pool, so a boss fighting on the east side ranked it last and the client never saw the
/// rising phase. Every responding prop now receives the signal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_calleil_raises_the_water_from_anywhere_in_the_room() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    server
        .systems
        .npcs
        .spawns
        .get_or_create(&world)
        .spawn_doodads(&server);
    let of = |template: u32| {
        world
            .all_doodads()
            .into_iter()
            .filter(|d| d.data().template_id == template)
            .collect::<Vec<_>>()
    };
    let water = of(6288).pop().expect("Calleil's rising water control");
    let (spouts, waterfalls) = (of(6277), of(6276));
    assert_eq!((spouts.len(), waterfalls.len()), (4, 11));
    let boss = WorldObject::from(npc_at(&server, &world, 12759, [350.0, 505.0, 170.0]));
    let (mut client, _player) = player_at(&server, 1, [345.0, 505.0, 170.0]);
    client.drain();
    assert_eq!(
        skill::unit_use_skill(&server, &boss, 21092, &boss),
        skill_result::SUCCESS
    );
    assert_eq!(water.data().func_group_id, 16637, "the water starts rising");
    assert!(spouts.iter().all(|d| d.data().func_group_id == 16613));
    assert!(waterfalls.iter().all(|d| d.data().func_group_id == 16609));
    let frames = client.drain();
    assert_eq!(
        phase_changes(&frames, water.obj_id),
        [16637],
        "the client receives the rising phase"
    );
}

fn alive(world: &WorldInstance, template: u32) -> usize {
    world
        .all_npcs()
        .into_iter()
        .filter(|n| {
            let d = n.data();
            d.unit.template_id == template && d.unit.hp > 0
        })
        .count()
}

struct GodfreyRun {
    server: Arc<GameServer>,
    world: Arc<WorldInstance>,
    players: Vec<(TestClient, PlayerRef)>,
    now: i64,
}

impl GodfreyRun {
    /// Walks the party through five overlapping circles of the approach sphere and back out,
    /// then waits. Returns the skills fired around the first player.
    async fn walk(&mut self, steps: usize) -> BTreeMap<u32, usize> {
        let path = [
            [514.93, 476.863, 187.629],
            [519.631, 486.285, 187.628],
            [526.821, 488.647, 187.634],
            [533.717, 491.259, 187.637],
            [540.718, 493.551, 187.651],
            [505.0, 478.0, 187.6],
        ];
        let mut fired = BTreeMap::new();
        for step in 0..steps {
            if let Some(p) = path.get(step / 10) {
                for (i, (_, player)) in self.players.iter().enumerate() {
                    let mut data = player.data();
                    data.unit.transform.set_xyz(p[0], p[1] + i as f32, p[2]);
                }
            }
            if step % 5 == 0 {
                let spheres = &self.server.systems.quests.spheres;
                spheres.tick_at(&self.server, self.now);
            }
            self.now += 100;
            for npc in self.world.all_npcs() {
                crate::systems::ai::manager::run_now(&self.server, npc.obj_id, |ai| {
                    ai.tick(100, true)
                });
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            for (opcode, body) in self.players[0].0.drain() {
                if opcode == crate::opcodes::sc::SCSkillFiredPacket && body.len() >= 4 {
                    let id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                    *fired.entry(id).or_default() += 1;
                }
            }
            for (client, _) in self.players.iter_mut().skip(1) {
                client.drain();
            }
        }
        fired
    }
}

/// Godfrey's Greater event runs once per instance. Walking through the fifteen circles of the
/// approach sphere casts its trigger once per player, Godfrey's lines execute the captives,
/// and the finale kills the friendly trigger NPC, so a killed Godfrey does not return when the
/// party walks back through the room.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_howling_godfrey_event_runs_once_and_the_boss_stays_dead() {
    use crate::systems::skills::{combat, types::KillReason};

    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let (server, world) = load_greater(&db);
    let trigger = npc_at(&server, &world, 12133, [566.856, 450.396, 201.5743]);
    crate::systems::ai::hooks::spawn_positioned(&server, &trigger);
    // The room's placements: captives (12789-12791) and their Blackscale guards (12792).
    let mut captives = Vec::new();
    for (template, xyz) in [
        (12791, [538.602, 484.894, 187.6719]),
        (12792, [537.248, 484.438, 187.7109]),
        (12792, [538.338, 481.228, 187.7109]),
        (12791, [534.63, 483.506, 187.6859]),
        (12791, [539.654, 481.654, 187.6629]),
        (12792, [533.138, 483.05, 187.6859]),
        (12791, [535.672, 480.18, 187.6759]),
        (12790, [540.748, 478.102, 187.6539]),
        (12792, [539.652, 477.682, 187.6619]),
        (12792, [534.142, 479.66, 187.7209]),
        (12789, [536.774, 476.654, 187.6659]),
        (12792, [535.46, 476.216, 187.7039]),
    ] {
        let npc = npc_at(&server, &world, template, xyz);
        crate::systems::ai::hooks::spawn_positioned(&server, &npc);
        if template != 12792 {
            captives.push(npc);
        }
    }
    let players = (0..3)
        .map(|i| player_at(&server, 1 + i, [505.0, 478.0 + i as f32, 187.6]))
        .collect();
    let areas = crate::systems::quests::spheres::load_sphere_areas(&app_path(), |id| id == 2184);
    server.systems.quests.spheres.set_areas(areas);
    let mut run = GodfreyRun {
        server: server.clone(),
        world: world.clone(),
        players,
        now: 0,
    };
    let fired = run.walk(700).await;
    assert_eq!(fired.get(&21413), Some(&3), "one sphere cast per player");
    assert_eq!(fired.get(&21414), Some(&3), "the trigger answers each cast");
    assert_eq!(fired.get(&21431), Some(&1), "Godfrey's last line runs once");
    assert_eq!(fired.get(&21438), Some(&1), "the finale runs once");
    assert_eq!(alive(&world, 12788), 0, "the staging actor leaves");
    assert_eq!(alive(&world, 12761), 1, "the combat Godfrey appears once");
    for captive in &captives {
        let hp = captive.data().unit.hp;
        assert!(hp <= 0, "Godfrey executes the captives ({hp} HP left)");
    }
    let trigger_hp = trigger.data().unit.hp;
    assert!(trigger_hp <= 0, "the finale removes the event trigger");

    let boss = world
        .all_npcs()
        .into_iter()
        .find(|n| n.data().unit.template_id == 12761)
        .unwrap();
    let boss_hp = boss.data().unit.hp;
    let killer = WorldObject::from(&run.players[0].1);
    combat::reduce_current_hp(
        &server,
        &WorldObject::from(&boss),
        Some(&killer),
        boss_hp,
        KillReason::Damage,
    );
    assert_eq!(alive(&world, 12761), 0);
    let fired = run.walk(200).await;
    assert_eq!(
        fired.get(&21414),
        None,
        "re-entering does not restart the event"
    );
    assert_eq!(alive(&world, 12788), 0);
    assert_eq!(alive(&world, 12761), 0, "the killed boss stays dead");
}
