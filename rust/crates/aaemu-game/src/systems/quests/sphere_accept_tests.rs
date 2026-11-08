//! `SCQuestAcceptConditional` 0x14a on entry to a `SphereAcceptQuest` sphere.
//!
//! The field roles come from the 1.2 client (reader `FUN_397b5d80`, handler ->
//! `START_QUEST_CONTEXT_SPHERE`): `a` = sphere id, `b` = quest context id. The entry trigger is
//! INFERRED (CAP-QU-2): the area comes from the shipped `sphere_area_fill.json` (the 1.2
//! `quest_sign_sphere.g` pack has no area for quest 5730) and the sphere/quest link from
//! `sphere_accept_quest_quests`, but no capture shows the retail send. The fixture uses the
//! real ids (sphere 2236, quest 5730, start component 24637, accept-sphere act detail 675).
//! `AAEMU_COMPACT_DB` real-data test:
//! `cargo test -p aaemu-game sphere_accept -- --ignored`.

use std::path::Path;

use super::data::{DbSphere, DbSphereAcceptQuestQuest, QuestData};
use super::events;
use super::model::kind;
use super::packets::CSStartQuestContextPacket;
use super::spheres::{load_sphere_areas, SphereArea};
use super::templates::{ActKind, QuestComponentTemplate, QuestTemplate};
use super::tests::{character, server_with};
use super::unit_reqs::{req_kind, UnitReq};
use crate::data::client_files::ClientFileManager;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::test_support::{spawn_player, test_character, TestClient};

const SPHERE: u32 = 2236;
const QUEST: u32 = 5730;
const START_COMPONENT: u32 = 24637;
const PROGRESS_COMPONENT: u32 = 24639;

/// Quest 5730's template shape from compact.sqlite3: a Start component with the accept-sphere
/// act (detail 675 -> sphere 2236) and a level-50 requirement, plus a Progress step so the
/// started quest stays active.
fn accept_quest(d: &mut QuestData) {
    d.add_template(QuestTemplate {
        id: QUEST,
        level: 50,
        ..QuestTemplate::default()
    });
    d.add_component(QuestComponentTemplate {
        id: START_COMPONENT,
        quest_id: QUEST,
        kind_id: kind::START,
        npc_ai_id: 1,
        ..QuestComponentTemplate::default()
    });
    d.add_act(
        START_COMPONENT,
        34157,
        675,
        0,
        ActKind::ConAcceptSphere { sphere_id: SPHERE },
    );
    d.add_component(QuestComponentTemplate {
        id: PROGRESS_COMPONENT,
        quest_id: QUEST,
        kind_id: kind::PROGRESS,
        npc_ai_id: 1,
        ..QuestComponentTemplate::default()
    });
    d.add_act(
        PROGRESS_COMPONENT,
        34158,
        1467,
        1,
        ActKind::ObjMonsterHunt {
            npc_id: 600,
            use_alias: false,
            alias_id: 0,
            highlight_doodad_id: 0,
            highlight_doodad_phase: -1,
        },
    );
    // unit_reqs 43986: level 50 on the start component.
    d.unit_reqs
        .by_owner
        .entry("QuestComponent".into())
        .or_default()
        .insert(
            START_COMPONENT,
            vec![UnitReq {
                id: 43986,
                owner_id: START_COMPONENT,
                owner_type: "QuestComponent".into(),
                kind: req_kind::LEVEL,
                value1: 50,
                value2: 0,
            }],
        );
    // sphere_accept_quest_quests row 14 (sphere_accept_quest_id 11 -> quest 5730) and the
    // sphere row it names (spheres 2236, sphere_detail_id 11).
    d.sphere_accept_quest_quests.push(DbSphereAcceptQuestQuest {
        id: 14,
        sphere_accept_quest_id: 11,
        quest_id: QUEST,
    });
    d.spheres.insert(
        SPHERE,
        DbSphere {
            id: SPHERE,
            name: "Q5730_운명의 이끌림 시작".into(),
            enter_or_leave: true,
            sphere_detail_id: 11,
            sphere_detail_type: "SphereAcceptQuest".into(),
            ..DbSphere::default()
        },
    );
    d.sphere_order.push(SPHERE);
}

/// The `sphere_area_fill.json` area of sphere 2236 around the test spawn point (the shipped
/// row is zone 205 at (20751.978, 23854.05, 137.417) radius 30).
fn set_offer_area(server: &GameServer) {
    server.systems.quests.spheres.set_areas(vec![SphereArea {
        world: "main_world".into(),
        zone: crate::test_support::TEST_ZONE,
        sphere_id: SPHERE,
        xyz: (100.0, 100.0, 100.0),
        radius: 50.0,
    }]);
}

fn eligible_character() -> crate::models::character::Character {
    let mut c = character();
    c.unit.level = 50;
    c
}

#[tokio::test]
async fn entering_the_area_offers_the_quest_with_0x14a() {
    let (server, _rec) = server_with(accept_quest);
    set_offer_area(&server);
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, eligible_character(), 0x100);
    client.conn.active_char = Some(player.clone());
    let _ = client.drain();

    server.systems.quests.spheres.tick(&server);
    let offer = vec![(
        sc::CSQuestAcceptConditionalPacket,
        // u32 sphere id (2236 = 0x8bc), u32 quest context id (5730 = 0x1662).
        vec![0xbc, 0x08, 0x00, 0x00, 0x62, 0x16, 0x00, 0x00],
    )];
    assert_eq!(client.drain(), offer);
    assert!(
        !events::has_quest(&player, QUEST),
        "the offer must not start the quest by itself"
    );

    // Staying inside sends nothing more; leaving and re-entering offers again (once per
    // entry, as long as the quest was not accepted).
    server.systems.quests.spheres.tick(&server);
    assert!(client.drain().is_empty());
    player.data().unit.transform.set_xyz(500.0, 500.0, 100.0);
    server.systems.quests.spheres.tick(&server);
    player.data().unit.transform.set_xyz(100.0, 100.0, 100.0);
    server.systems.quests.spheres.tick(&server);
    assert_eq!(client.drain(), offer);

    // The client answers with CSStartQuestContext carrying the sphere id; the existing start
    // path installs the quest with the sphere acceptor.
    CSStartQuestContextPacket {
        quest_context_id: QUEST,
        npc_obj_id: 0,
        doodad_obj_id: 0,
        sphere_id: SPHERE,
    }
    .handle(&server, &mut client.conn)
    .await
    .unwrap();
    assert!(events::has_quest(&player, QUEST));

    // Staying inside offers nothing more.
    client.drain();
    server.systems.quests.spheres.tick(&server);
    assert!(client.drain().is_empty());
}

#[tokio::test]
async fn the_offer_follows_the_normal_start_rules() {
    let (server, _rec) = server_with(accept_quest);
    set_offer_area(&server);

    // Below the start component's level 50: no dialog, no quest.
    let mut low_client = TestClient::new(&server, 1, 1001);
    let mut low = character();
    low.unit.level = 49;
    let low_player = spawn_player(&server, &low_client, low, 0x100);
    let _ = low_client.drain();
    server.systems.quests.spheres.tick(&server);
    assert!(low_client.drain().is_empty());
    assert!(!events::has_quest(&low_player, QUEST));

    // Already completed and not repeatable: no dialog.
    let mut done_client = TestClient::new(&server, 2, 1002);
    let mut done = test_character(2, "Done", 100.0, 100.0);
    done.unit.level = 50;
    let done_player = spawn_player(&server, &done_client, done, 0x101);
    super::engine::set_completed_quest_flag(&mut done_player.data().quests.completed, QUEST, true);
    let _ = done_client.drain();
    server.systems.quests.spheres.tick(&server);
    assert!(done_client.drain().is_empty());

    // Already active: no dialog.
    let mut active_client = TestClient::new(&server, 3, 1003);
    let mut active = test_character(3, "Active", 100.0, 100.0);
    active.unit.level = 50;
    let active_player = spawn_player(&server, &active_client, active, 0x102);
    events::add_quest(&server, &active_player, QUEST, true);
    assert!(events::has_quest(&active_player, QUEST));
    let _ = active_client.drain();
    server.systems.quests.spheres.tick(&server);
    assert!(active_client.drain().is_empty());
}

#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_compact_db_accept_quest_spheres_map_to_their_quests() {
    let Some(path) = std::env::var("AAEMU_COMPACT_DB").ok() else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let d = QuestData::load_from(&conn, &ClientFileManager::new()).expect("quest tables");
    assert_eq!(d.sphere_accept_quest_quests.len(), 3);
    // The three link rows and the four dedicated spheres (1520 has no link row).
    assert_eq!(d.accept_quest_sphere_id(1956), 1040);
    assert_eq!(d.accept_quest_sphere_id(4281), 1453);
    assert_eq!(d.accept_quest_sphere_id(5730), SPHERE);
    assert_eq!(d.accept_quest_sphere_id(4321), 0);
    // Only 5730 is a real quest in 1.2; the offer is gated on an existing template.
    assert!(d.template(QUEST).is_some());
    assert!(d.template(1956).is_none());
    assert!(d.template(4281).is_none());
    // Its Start component accepts the very sphere the offer names (the accept reply check).
    let start = d.component(START_COMPONENT).expect("start component");
    assert!(start.acts.iter().any(|&a| matches!(
        d.acts[a].kind,
        ActKind::ConAcceptSphere { sphere_id } if sphere_id == SPHERE
    )));

    // The trigger geometry: the shipped area fill carries sphere 2236 (and the other two
    // linked spheres); the 1.2 client's quest_sign_sphere.g has no area for quest 5730.
    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let areas = load_sphere_areas(&app, |id| d.spheres.contains_key(&id));
    for (sphere_id, quest_id) in [(1040, 1956), (1453, 4281), (SPHERE, QUEST)] {
        let areas: Vec<_> = areas.iter().filter(|a| a.sphere_id == sphere_id).collect();
        assert!(!areas.is_empty(), "sphere {sphere_id} has an area");
        assert!(
            areas.iter().all(|a| a.world == "main_world"),
            "sphere {sphere_id} is a main_world area"
        );
        assert_eq!(d.accept_quest_sphere_id(quest_id), sphere_id);
        assert_eq!(
            d.spheres[&sphere_id].sphere_detail_type,
            "SphereAcceptQuest"
        );
    }
}
