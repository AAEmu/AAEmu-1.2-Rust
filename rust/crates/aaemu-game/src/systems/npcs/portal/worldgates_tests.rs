//! `Data/Portal/worldgates.json` itself: the file parses, every gate has a unique id and sub zone
//! (`PortalIndex::add` rejects duplicates and aborts the portal load), and the section generated
//! by `aaemu-aipaths return-points` lies on the 1.2 terrain
//! (`AAEMU_GAME_PAK=<1.2 game_pak> cargo test -p aaemu-game real_worldgates -- --ignored`).

use std::collections::HashSet;
use std::path::PathBuf;

use super::data::PortalTables;
use crate::data::world_data::{parse_lenient_json, Portal};
use crate::systems::ai::geodata::manager::AiGeoData;
use crate::systems::skills::controllers::Vec3;

/// First text of the marker comment that opens the generated section.
const GENERATED_MARKER: &str = "Generated from the 10.x return_point.xml";

fn worldgates_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("app")
        .join("Data")
        .join("Portal")
        .join("worldgates.json")
}

fn load_gates() -> (String, Vec<Portal>) {
    let text = std::fs::read_to_string(worldgates_path()).expect("worldgates.json");
    let gates = parse_lenient_json::<Vec<Portal>>(&text).expect("worldgates.json parses");
    (text, gates)
}

/// Ids of the gates of the generated section (the text after the marker comment).
fn generated_ids(text: &str) -> HashSet<u32> {
    let Some(at) = text.find(GENERATED_MARKER) else {
        return HashSet::new();
    };
    let mut ids = HashSet::new();
    for line in text[at..].lines() {
        if let Some(rest) = line.trim().strip_prefix("\"Id\":") {
            let digits: String = rest
                .trim()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            ids.extend(digits.parse::<u32>());
        }
    }
    ids
}

#[test]
fn worldgates_json_parses_with_unique_ids_and_sub_zones() {
    let (_, gates) = load_gates();
    assert!(gates.len() >= 26, "only {} gates", gates.len());
    let mut ids = HashSet::new();
    let mut sub_zones = HashSet::new();
    for g in &gates {
        assert!(ids.insert(g.id), "duplicate gate id {}", g.id);
        assert!(
            sub_zones.insert(g.sub_zone_id),
            "duplicate gate sub zone {} (id {})",
            g.sub_zone_id,
            g.id
        );
        assert!(g.zone_id != 0, "gate {} has no zone", g.id);
        assert!(g.x > 0.0 && g.y > 0.0, "gate {} has no position", g.id);
    }
    // The portal loader accepts the file the same way.
    let tables = PortalTables::from_parts(vec![], gates.clone(), vec![], vec![])
        .expect("no duplicate gate id or sub zone");
    assert_eq!(tables.world_gates.len(), gates.len());
}

#[test]
fn generated_gates_include_the_liania_quest_gate() {
    let (text, gates) = load_gates();
    let generated = generated_ids(&text);
    assert!(generated.contains(&30), "gate 30 is not generated");
    let g = gates.iter().find(|g| g.id == 30).expect("gate 30");
    assert_eq!(g.zone_id, 148);
    assert!((g.x - 14538.5).abs() < 0.1 && (g.y - 11113.2).abs() < 0.1);
    for id in &generated {
        assert!(gates.iter().any(|g| g.id == *id));
    }
}

/// Instance world gates generated with `--world` (`Return` effects of the Nachashgar portals,
/// the library doors, Howling Abyss and the Cradle): id -> (zone key, name). Gates 578 and 632
/// share their id with a hand written sub zone, so their `SubZoneId` is `100000 + Id`.
const INSTANCE_GATES: &[(u32, u32, &str)] = &[
    (484, 265, "portal_instance_howling_abyss"),
    (488, 264, "sprigun_room"),
    (524, 278, "portal_nachashgar_02"),
    (525, 280, "portal_howling_abyss_2"),
    (532, 278, "portal_nachashgar_09"),
    (546, 278, "portal_nachashgar_back_01"),
    (561, 278, "portal_nachashgar_010"),
    (572, 292, "portal_nacha_easy_02"),
    (581, 292, "portal_nacha_easy_back_01"),
    (625, 278, "portal_nachashgar_09_c"),
    (626, 278, "portal_nachashgar_09_a"),
    (630, 296, "instance_library_1_up_1"),
    (637, 297, "instance_library_2_dn_1"),
    (647, 298, "instance_library_3_dn_3"),
    (654, 296, "instance_library_1_Yn_in"),
    (666, 298, "instance_library_3_Tirenep_in"),
    (679, 296, "instance_library_1_Sealed_in2"),
];

#[test]
fn instance_worldgates_are_loaded_with_their_worlds_zone() {
    let (text, gates) = load_gates();
    let generated = generated_ids(&text);
    for (id, zone, name) in INSTANCE_GATES {
        let g = gates
            .iter()
            .find(|g| g.id == *id)
            .unwrap_or_else(|| panic!("gate {id} is missing"));
        assert_eq!((g.zone_id, g.name.as_str()), (*zone, *name), "gate {id}");
        assert!(generated.contains(id), "gate {id} is not generated");
        assert!(g.x > 0.0 && g.y > 0.0 && g.z > 0.0, "gate {id}");
    }
    // 67 instance gates were added to the 40 main_world ones.
    assert!(generated.len() >= 107, "only {} generated", generated.len());
    let sub = |id: u32| gates.iter().find(|g| g.id == id).unwrap().sub_zone_id;
    assert_eq!((sub(578), sub(632), sub(630)), (100_578, 100_632, 630));
    // Library zone 297 starts at the origin cell (0, 2): Y + 2048.
    let dn = gates.iter().find(|g| g.id == 637).unwrap();
    assert!((dn.y - 2304.5).abs() < 0.1, "{}", dn.y);
    // Ids that must stay out: rejected or low confidence points, reassigned ids.
    for id in [399, 400, 533, 555, 567, 627, 681, 68, 69] {
        assert!(!generated.contains(&id), "gate {id} must not be generated");
    }
}

/// How far a generated gate may float above or sink below the terrain. The 10.x editor spheres
/// sit on the ground; a few metres cover the heightmap sampling and small terrain edits.
const GROUND_TOLERANCE: f32 = 5.0;

/// A gate off the terrain (upper floor, platform) still counts when the navmesh has a walkable
/// node this close horizontally and vertically.
const NAVMESH_RADIUS: f32 = 20.0;
const NAVMESH_TOLERANCE: f32 = 3.0;

/// The vertical distance to the closest navmesh node within `NAVMESH_RADIUS` of (x, y).
fn navmesh_dz(g: &AiGeoData, x: f32, y: f32, z: f32) -> Option<f32> {
    let bai = g.bai_by_pos(Vec3::new(x, y, z)).ok().flatten()?;
    bai.net_mission_readers
        .iter()
        .flat_map(|n| n.nodes.iter())
        .filter(|n| (n.pos.x - x).hypot(n.pos.y - y) <= NAVMESH_RADIUS)
        .map(|n| (n.pos.z - z).abs())
        .min_by(f32::total_cmp)
}

#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_worldgates_follow_the_terrain() {
    use std::sync::Arc;

    use crate::data::client_files::ClientFileManager;
    use crate::data::world_templates::WorldTemplates;
    use crate::systems::ai::geodata::bai::{BaiFiles, UsedAreaNames};
    use crate::systems::ai::geodata::ClientBaiFiles;
    use crate::systems::npcs::heightmap::{HeightmapConfig, HeightmapWorlds};

    let Ok(pak) = std::env::var("AAEMU_GAME_PAK") else {
        return;
    };
    let app = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("app");
    let files = Arc::new(ClientFileManager::from_sources(&[pak], &app));
    let worlds = WorldTemplates::load(&files, &app).expect("world templates");
    let main = worlds
        .get_world_template_by_name("main_world")
        .expect("main_world");
    let maps = HeightmapWorlds::new(
        files.clone(),
        &worlds,
        HeightmapConfig {
            heightmaps_enable: true,
            geo_data_mode: false,
            pre_load_terrain: false,
        },
    );
    let terrain = maps.template(main.id).expect("main_world heightmap");
    let bai: Arc<dyn BaiFiles + Send + Sync> = Arc::new(ClientBaiFiles(files.clone()));
    let geo = AiGeoData::new(main, true, bai, Arc::new(UsedAreaNames::default()), None);

    let (text, gates) = load_gates();
    let generated = generated_ids(&text);
    // The instance world gates have their own origins (no main_world terrain).
    let main_zones: HashSet<u32> = main
        .xml_world_zones()
        .values()
        .map(|z| z.zone_key)
        .collect();
    assert!(generated.len() >= 40, "only {} generated", generated.len());
    let mut failures = Vec::new();
    let mut worst = 0.0f32;
    for g in gates
        .iter()
        .filter(|g| generated.contains(&g.id) && main_zones.contains(&g.zone_id))
    {
        let zone = worlds.get_zone_id(Some(main), g.x, g.y);
        let ground = terrain.get_height(&files, g.x, g.y).unwrap_or(0.0);
        let off = (g.z - ground).abs();
        println!(
            "gate {:>3} {:<28} zone {} (point in zone {zone}) z {:.1} ground {ground:.1} off {off:.1}",
            g.id, g.name, g.zone_id, g.z
        );
        worst = worst.max(off);
        let floor = if off > GROUND_TOLERANCE {
            navmesh_dz(&geo, g.x, g.y, g.z)
        } else {
            None
        };
        if off > GROUND_TOLERANCE {
            println!("    off the terrain, navmesh node dz {floor:?}");
        }
        let on_navmesh = floor.is_some_and(|dz| dz <= NAVMESH_TOLERANCE);
        if zone != g.zone_id {
            failures.push(format!(
                "gate {} lies in zone {zone}, not {}",
                g.id, g.zone_id
            ));
        }
        if off > GROUND_TOLERANCE && !on_navmesh {
            failures.push(format!(
                "gate {} ({}): z {:.1} is {off:.1} m from the terrain ({ground:.1}) and has no navmesh floor",
                g.id, g.name, g.z
            ));
        }
    }
    println!(
        "{} generated gates checked, worst vertical distance {worst:.1} m",
        generated.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
