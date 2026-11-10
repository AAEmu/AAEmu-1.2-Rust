//! Tests for the 10.x-derived `quest_sign_sphere_fill.json` loader and its runtime guard:
//! entries parse, duplicates are rejected, a 1.2 client sphere is never overridden, a closed
//! or unknown zone and an unknown component are skipped, and entering an imported sphere
//! advances the quest's `QuestActObjSphere` objective.

use std::collections::HashMap;

use super::data::{DbSphere, DbSphereQuest, QuestData, RawSignSphere};
use super::events;
use super::spheres::{load_sign_sphere_fill, load_sphere_areas};
use super::templates::ActKind;
use crate::data::client_files::ClientFileManager;
use crate::data::world_data::{Zone, ZoneData};
use crate::data::world_templates::WorldTemplates;
use crate::data::GameData;
use crate::models::character::Character;
use crate::systems::quests::model::kind;
use crate::systems::quests::templates::{QuestComponentTemplate, QuestTemplate};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};

fn quest_data() -> QuestData {
    let mut d = QuestData::default();
    d.add_template(QuestTemplate {
        id: 20,
        ..QuestTemplate::default()
    });
    d.add_template(QuestTemplate {
        id: 21,
        ..QuestTemplate::default()
    });
    d.add_component(QuestComponentTemplate {
        id: 201,
        quest_id: 20,
        kind_id: kind::START,
        npc_ai_id: 1,
        ..QuestComponentTemplate::default()
    });
    d.add_act(201, 201, 3, 0, ActKind::ConAutoComplete);
    d.add_component(QuestComponentTemplate {
        id: 202,
        quest_id: 20,
        kind_id: kind::PROGRESS,
        npc_ai_id: 1,
        ..QuestComponentTemplate::default()
    });
    d.add_act(
        202,
        202,
        1,
        0,
        ActKind::ObjSphere {
            sphere_id: 5,
            npc_id: 0,
            highlight_doodad_id: 0,
            highlight_doodad_phase: -1,
            use_alias: false,
            alias_id: 0,
        },
    );
    d.spheres.insert(
        5,
        DbSphere {
            id: 5,
            sphere_detail_id: 1,
            sphere_detail_type: "SphereQuest".into(),
            ..DbSphere::default()
        },
    );
    d.sphere_quests.insert(
        1,
        DbSphereQuest {
            id: 1,
            quest_id: 20,
            quest_trigger_id: 0,
        },
    );
    d.add_component(QuestComponentTemplate {
        id: 204,
        quest_id: 21,
        kind_id: kind::START,
        npc_ai_id: 1,
        ..QuestComponentTemplate::default()
    });
    d.add_act(204, 204, 4, 0, ActKind::ConAutoComplete);
    // The component whose sphere the 1.2 client pack already has.
    d.add_component(QuestComponentTemplate {
        id: 203,
        quest_id: 21,
        kind_id: kind::PROGRESS,
        npc_ai_id: 1,
        ..QuestComponentTemplate::default()
    });
    d.add_act(
        203,
        203,
        2,
        0,
        ActKind::ObjSphere {
            sphere_id: 6,
            npc_id: 0,
            highlight_doodad_id: 0,
            highlight_doodad_phase: -1,
            use_alias: false,
            alias_id: 0,
        },
    );
    d.spheres.insert(
        6,
        DbSphere {
            id: 6,
            sphere_detail_id: 2,
            sphere_detail_type: "SphereQuest".into(),
            ..DbSphere::default()
        },
    );
    d.sphere_quests.insert(
        2,
        DbSphereQuest {
            id: 2,
            quest_id: 21,
            quest_trigger_id: 0,
        },
    );
    d.sign_spheres.push(RawSignSphere {
        zone_id: 100,
        quest_id: 21,
        component_id: 203,
        local: [1.0, 2.0, 3.0],
        radius: 4.0,
    });
    d.finish();
    d
}

fn zone(key: u32, closed: bool) -> (u32, Zone) {
    (
        key,
        Zone {
            id: key,
            name: format!("zone {key}"),
            zone_key: key,
            closed,
            ..Zone::default()
        },
    )
}

fn write_fill(app: &std::path::Path, rows: &str) {
    let dir = app.join("Data").join("Worlds").join("main_world");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("quest_sign_sphere_fill.json"), rows).unwrap();
}

#[tokio::test]
async fn fill_rows_are_guarded_against_the_loaded_1_2_data() {
    let app = crate::data::pak::test_util::temp_path("quest_sphere_fill");
    write_fill(
        &app,
        r#"[
            {"quest_id": 20, "component_id": 202, "sphere_id": 5, "zone": 100,
             "x": 10.0, "y": 20.0, "z": 30.0, "radius": 5.0},
            {"quest_id": 20, "component_id": 202, "sphere_id": 5, "zone": 100,
             "x": 10.0, "y": 20.0, "z": 30.0, "radius": 5.0},
            {"quest_id": 21, "component_id": 203, "sphere_id": 6, "zone": 100,
             "x": 1.0, "y": 2.0, "z": 3.0, "radius": 4.0},
            {"quest_id": 20, "component_id": 999, "sphere_id": 5, "zone": 100,
             "x": 1.0, "y": 2.0, "z": 3.0, "radius": 4.0},
            {"quest_id": 20, "component_id": 202, "sphere_id": 5, "zone": 101,
             "x": 1.0, "y": 2.0, "z": 3.0, "radius": 4.0},
            {"quest_id": 20, "component_id": 202, "sphere_id": 5, "zone": 999,
             "x": 1.0, "y": 2.0, "z": 3.0, "radius": 4.0},
            {"quest_id": 20, "component_id": 202, "sphere_id": 5, "zone": 100,
             "x": 1.0, "y": 2.0},
            {"quest_id": 20, "component_id": 202, "sphere_id": 5, "zone": 100,
             "x": 1.0, "y": 2.0, "z": 3.0, "radius": 0.0}
        ]"#,
    );
    let zones = ZoneData {
        zones: HashMap::from([zone(100, false), zone(101, true)]),
        ..ZoneData::default()
    };
    let mut data = GameData::empty();
    data.systems.quests = quest_data();
    data.zones = zones;
    let server = test_server_with(data);
    let fill = load_sign_sphere_fill(
        &app,
        &server.game_data.systems.quests,
        &server.game_data.zones,
        &server.game_data.worlds,
    );
    assert_eq!(fill.len(), 1, "{fill:?}");
    assert_eq!(fill[0].0, "main_world");
    assert_eq!(fill[0].1.quest_id, 20);
    assert_eq!(fill[0].1.component_id, 202);
    assert_eq!(fill[0].1.zone_id, 100);
    assert_eq!(fill[0].1.radius, 5.0);
    let _ = std::fs::remove_dir_all(&app);
}

#[tokio::test]
async fn entering_an_imported_sphere_advances_the_objective() {
    let mut d = quest_data();
    // Only the imported fill row, no client sign sphere.
    d.sign_spheres.clear();
    let mut data = GameData::empty();
    data.systems.quests = d;
    let server = test_server_with(data);
    server.systems.quests.spheres.load(
        &server.game_data.systems.quests,
        &[],
        &[(
            "main_world".to_string(),
            RawSignSphere {
                zone_id: 100,
                quest_id: 20,
                component_id: 202,
                // Imported fill rows already hold world coordinates.
                local: [1124.0, 1124.0, 100.0],
                radius: 5.0,
            },
        )],
        "main_world",
        |_| (1, 1),
    );
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    let _ = client.drain();
    {
        let mut p = player.data();
        p.unit.transform.set_xyz(1124.0, 1124.0, 100.0);
    }
    events::add_quest(&server, &player, 20, false);
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(player.data().quests.active[0].objectives[0], 0);
    server.systems.quests.spheres.tick(&server);
    assert_eq!(player.data().quests.active[0].objectives[0], 1);
}

fn character() -> Character {
    test_character(1, "Tester", 100.0, 100.0)
}

#[test]
#[ignore = "needs AAEMU_COMPACT_DB (and AAEMU_GAME_PAK for the client world templates)"]
fn real_data_imported_quest_sign_spheres_load() {
    let Some(conn) = super::tests::real_db() else {
        return;
    };
    let app = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let mut rows = 0usize;
    let worlds_dir = app.join("Data").join("Worlds");
    for entry in std::fs::read_dir(&worlds_dir).unwrap().flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let path = entry.path().join("quest_sign_sphere_fill.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let list: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        rows += list.len();
        for row in list {
            let quest = row["quest_id"].as_u64().unwrap() as u32;
            let component = row["component_id"].as_u64().unwrap() as u32;
            let sphere = row["sphere_id"].as_u64().unwrap() as u32;
            let has_act: i64 = conn
                .query_row(
                    "SELECT count(*) FROM quest_acts qa
                     JOIN quest_components qc ON qc.id = qa.quest_component_id
                     JOIN quest_act_obj_spheres d ON d.id = qa.act_detail_id
                     WHERE qa.act_detail_type = 'QuestActObjSphere'
                       AND qc.quest_context_id = ?1 AND qc.id = ?2 AND d.sphere_id = ?3",
                    rusqlite::params![quest, component, sphere],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                has_act, 1,
                "quest {quest} component {component} sphere {sphere}"
            );
        }
    }
    assert_eq!(rows, 8, "the committed fill files hold the 8 kept spheres");

    if let Ok(pak) = std::env::var("AAEMU_GAME_PAK") {
        let mut files = crate::data::client_files::ClientFileManager::new();
        assert!(files.add_source(&pak, std::path::Path::new(".")));
        let data = QuestData::load_from(&conn, &files).expect("quest data");
        let zones = ZoneData::load(&conn).expect("zones");
        let worlds = crate::data::world_templates::WorldTemplates::load(&files, &app)
            .expect("world templates");
        let fill = load_sign_sphere_fill(&app, &data, &zones, &worlds);
        assert_eq!(fill.len(), 8, "{fill:?}");
    }
}

/// The zone's world rectangle from its 1.2 `world.xml` cells: every loaded sphere must lie
/// inside it. A converted zone-local position is `origin * 1024 + local`; without the
/// conversion a `sphere_area_fill.json` row sits in the top-left corner of the world and fails.
fn assert_in_zone(worlds: &WorldTemplates, world: &str, zone: u32, x: f32, y: f32) {
    let template = worlds
        .get_world_template_by_name(world)
        .unwrap_or_else(|| panic!("world {world} is not loaded"));
    let xml_zone = template
        .xml_world_zones()
        .get(&zone)
        .unwrap_or_else(|| panic!("zone {zone} is not in {world}"));
    if xml_zone.cells.is_empty() {
        return;
    }
    let min_x = xml_zone.cells.keys().map(|c| c.0).min().unwrap();
    let max_x = xml_zone.cells.keys().map(|c| c.0).max().unwrap();
    let min_y = xml_zone.cells.keys().map(|c| c.1).min().unwrap();
    let max_y = xml_zone.cells.keys().map(|c| c.1).max().unwrap();
    let inside = min_x as f32 * 1024.0 <= x
        && x <= (max_x + 1) as f32 * 1024.0
        && min_y as f32 * 1024.0 <= y
        && y <= (max_y + 1) as f32 * 1024.0;
    assert!(
        inside,
        "{world} zone {zone}: ({x}, {y}) is outside cells ({min_x}, {min_y})..({max_x}, {max_y})"
    );
}

#[test]
#[ignore = "needs AAEMU_COMPACT_DB and AAEMU_GAME_PAK"]
fn real_data_fill_spheres_lie_inside_their_zone_rectangle() {
    let Some(conn) = super::tests::real_db() else {
        return;
    };
    let Ok(pak) = std::env::var("AAEMU_GAME_PAK") else {
        return;
    };
    let app = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let mut files = ClientFileManager::new();
    assert!(files.add_source(&pak, std::path::Path::new(".")));
    let data = QuestData::load_from(&conn, &files).expect("quest data");
    let worlds = WorldTemplates::load(&files, &app).expect("world templates");

    let areas = load_sphere_areas(&app, |id| data.spheres.contains_key(&id));
    assert_eq!(areas.len(), 2112, "committed area spheres");
    for area in &areas {
        assert_in_zone(&worlds, &area.world, area.zone, area.xyz.0, area.xyz.1);
    }

    let zones = ZoneData::load(&conn).expect("zones");
    let fill = load_sign_sphere_fill(&app, &data, &zones, &worlds);
    assert_eq!(fill.len(), 8, "committed imported sign spheres");
    for (world, row) in &fill {
        assert_in_zone(&worlds, world, row.zone_id, row.local[0], row.local[1]);
    }
}
