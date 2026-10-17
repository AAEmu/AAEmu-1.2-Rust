//! Quest follow-up fixes: item-gain starters, `with_quests` panic safety, sphere trigger
//! lifetime and report refusal logging.

use super::events;
use super::model::kind;
use super::templates::{ActKind, QuestComponentTemplate, QuestTemplate};
use super::tests::{character, server_with};
use crate::data::items::{ItemData, ItemTemplate};
use crate::models::character::Character;
use crate::models::item::Item;
use crate::test_support::{spawn_player, TestClient};

fn give_item(c: &mut Character, template_id: u32, count: i32) {
    let t = ItemTemplate::plain(template_id, 1000);
    let mut item = Item::create(
        900_000 + u64::from(template_id),
        &t,
        count,
        0,
        1,
        &ItemData::default(),
        0,
    );
    let bag = c.inventory.bag().clone();
    let mut g = bag.lock().unwrap();
    item.slot = g.get_unused_slot(-1);
    g.items.push(item);
    g.update_free_slot_count();
}

fn quest(id: u32) -> QuestTemplate {
    QuestTemplate {
        id,
        detail_id: super::model::detail::NORMAL,
        ..QuestTemplate::default()
    }
}

fn component(id: u32, quest_id: u32, kind_id: u8) -> QuestComponentTemplate {
    QuestComponentTemplate {
        id,
        quest_id,
        kind_id,
        npc_ai_id: 1,
        ..QuestComponentTemplate::default()
    }
}

/// Quest `qid`: Start = gain `count` x item `item`, Progress = report to NPC 1 (stays active).
fn item_gain_quest(d: &mut super::data::QuestData, qid: u32, item: u32, count: i32) {
    d.add_template(quest(qid));
    d.add_component(component(qid * 10 + 1, qid, kind::START));
    d.add_act(
        qid * 10 + 1,
        qid * 10 + 1,
        qid * 10 + 1,
        count,
        ActKind::ConAcceptItemGain { item_id: item },
    );
    d.add_component(component(qid * 10 + 2, qid, kind::PROGRESS));
    d.add_act(
        qid * 10 + 2,
        qid * 10 + 2,
        qid * 10 + 2,
        1,
        ActKind::ObjMonsterHunt {
            npc_id: 600,
            use_alias: false,
            alias_id: 0,
            highlight_doodad_id: 0,
            highlight_doodad_phase: -1,
        },
    );
}

#[tokio::test]
async fn item_gain_starts_quest_once_the_count_is_reached() {
    let (server, _rec) = server_with(|d| item_gain_quest(d, 7, 3711, 3));
    assert_eq!(
        server
            .game_data
            .systems
            .quests
            .item_gain_starters
            .get(&3711),
        Some(&vec![7])
    );
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);

    // 2 of 3 roses: not enough.
    give_item(&mut player.data(), 3711, 2);
    events::on_items_acquired(&server, &player, 3711, 2);
    assert!(!events::has_quest(&player, 7));

    // The third rose starts it (the acceptor is the item).
    let t = ItemTemplate::plain(3711, 1000);
    let mut item = Item::create(990_001, &t, 1, 0, 1, &ItemData::default(), 0);
    {
        let bag = player.data().inventory.bag().clone();
        let mut g = bag.lock().unwrap();
        item.slot = g.get_unused_slot(-1);
        g.items.push(item);
        g.update_free_slot_count();
    }
    events::on_items_acquired(&server, &player, 3711, 1);
    assert!(events::has_quest(&player, 7));

    // Gaining more while the quest is active does not restart it, and a consumption never
    // starts anything.
    events::on_items_acquired(&server, &player, 3711, 5);
    assert_eq!(
        player
            .data()
            .quests
            .active
            .iter()
            .filter(|q| q.template_id == 7)
            .count(),
        1
    );
}

#[tokio::test]
async fn item_gain_ignores_completed_quests_and_unrelated_items() {
    let (server, _rec) = server_with(|d| item_gain_quest(d, 7, 3711, 1));
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    give_item(&mut player.data(), 3711, 1);
    events::on_items_acquired(&server, &player, 9999, 1);
    assert!(!events::has_quest(&player, 7));
    events::on_items_acquired(&server, &player, 3711, -1);
    assert!(!events::has_quest(&player, 7));

    super::engine::set_completed_quest_flag(&mut player.data().quests.completed, 7, true);
    events::on_items_acquired(&server, &player, 3711, 1);
    assert!(!events::has_quest(&player, 7));
}

#[tokio::test]
async fn a_panicking_quest_operation_leaves_the_character_usable() {
    let (server, _rec) = server_with(|d| item_gain_quest(d, 7, 3711, 1));
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);

    // An already active quest, so the lost quest state would be visible.
    give_item(&mut player.data(), 3711, 1);
    events::on_items_acquired(&server, &player, 3711, 1);
    assert!(events::has_quest(&player, 7));

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        events::with_quests(&server, &player, None, |_| panic!("boom"));
    }));
    assert!(panicked.is_err());
    // A queued operation that panics is covered the same way.
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        events::with_quests(&server, &player, None, |_| {
            events::run_or_queue(&server, &player, |_| panic!("queued boom"));
        });
    }));
    assert!(panicked.is_err());

    // The quest state is back and the character is not stuck in the active list.
    assert!(events::has_quest(&player, 7));
    assert!(
        events::with_quests(&server, &player, None, |ctx| ctx.has_quest(7)).unwrap(),
        "with_quests runs again instead of queueing forever"
    );
    assert_eq!(
        player
            .data()
            .quests
            .active
            .iter()
            .filter(|q| q.template_id == 7)
            .count(),
        1
    );
}

/// Quest 30 starts by entering sphere 5 (a 50 m sign sphere around the test character).
fn sphere_starter_quest(d: &mut super::data::QuestData) {
    d.add_template(quest(30));
    d.add_component(component(301, 30, kind::START));
    d.add_act(301, 301, 301, 0, ActKind::ConAcceptSphere { sphere_id: 5 });
    d.add_component(component(302, 30, kind::PROGRESS));
    d.add_act(
        302,
        302,
        302,
        1,
        ActKind::ObjMonsterHunt {
            npc_id: 600,
            use_alias: false,
            alias_id: 0,
            highlight_doodad_id: 0,
            highlight_doodad_phase: -1,
        },
    );
    d.sphere_quests.insert(
        1,
        super::data::DbSphereQuest {
            id: 1,
            quest_id: 30,
            quest_trigger_id: 0,
        },
    );
    d.sphere_quest_order.push(1);
    d.spheres.insert(
        5,
        super::data::DbSphere {
            id: 5,
            sphere_detail_id: 1,
            sphere_detail_type: "SphereQuest".into(),
            ..super::data::DbSphere::default()
        },
    );
    d.sphere_order.push(5);
}

#[tokio::test]
async fn relogin_inside_a_sphere_fires_the_enter_event_again() {
    let (server, _rec) = server_with(sphere_starter_quest);
    server.systems.quests.spheres.load(
        &server.game_data.systems.quests,
        &[super::data::RawSignSphere {
            zone_id: 1,
            quest_id: 30,
            component_id: 301,
            local: [100.0, 100.0, 100.0],
            radius: 50.0,
        }],
        "main_world",
        |_| (0, 0),
    );
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);

    // Entering the sphere starts the quest.
    server.systems.quests.spheres.tick(&server);
    assert!(events::has_quest(&player, 30));

    // Still inside: dropping the quest does not restart it by itself ...
    events::drop_quest(&server, &player, 30, false);
    assert!(!events::has_quest(&player, 30));
    server.systems.quests.spheres.tick(&server);
    assert!(!events::has_quest(&player, 30));

    // ... but logging out forgets the last position, so the next login inside the sphere
    // is an enter again.
    super::on_leave_world(&server, player.character_id);
    server.systems.quests.spheres.tick(&server);
    assert!(events::has_quest(&player, 30));
}

#[tokio::test]
async fn leaving_the_world_drops_the_sphere_triggers_of_the_character() {
    let (server, _rec) = server_with(|d| {
        d.add_template(quest(20));
        d.add_component(component(201, 20, kind::START));
        d.add_act(201, 201, 201, 0, ActKind::ConAutoComplete);
        d.add_component(component(202, 20, kind::PROGRESS));
        d.add_act(
            202,
            202,
            202,
            1,
            ActKind::ObjSphere {
                sphere_id: 0,
                npc_id: 0,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
                use_alias: false,
                alias_id: 0,
            },
        );
    });
    server.systems.quests.spheres.load(
        &server.game_data.systems.quests,
        &[super::data::RawSignSphere {
            zone_id: 1,
            quest_id: 20,
            component_id: 202,
            local: [100.0, 100.0, 100.0],
            radius: 50.0,
        }],
        "main_world",
        |_| (0, 0),
    );
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    events::add_quest(&server, &player, 20, false);
    server.systems.quests.run_queued_evaluations(&server);
    server.systems.quests.spheres.tick(&server);
    assert_eq!(server.systems.quests.spheres.triggers().len(), 1);

    super::on_leave_world(&server, player.character_id);
    server.systems.quests.spheres.tick(&server);
    assert!(server.systems.quests.spheres.triggers().is_empty());
}

// ----- report refusals are logged -----

struct Messages(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

struct Visit(String);

impl tracing::field::Visit for Visit {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Messages {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut v = Visit(String::new());
        event.record(&mut v);
        self.0
            .lock()
            .unwrap()
            .push(format!("{} {}", event.metadata().level(), v.0));
    }
}

/// Runs `f` and returns the WARN lines it logged.
fn warnings(f: impl FnOnce()) -> Vec<String> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Layer;
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry()
        .with(Messages(lines.clone()).with_filter(tracing_subscriber::filter::LevelFilter::WARN));
    tracing::subscriber::with_default(subscriber, f);
    let out = lines.lock().unwrap().clone();
    out
}

fn spawn_npc(server: &crate::server::GameServer, template: u32, obj_id: u32) -> u32 {
    let mut npc = crate::models::npc::Npc::new(template);
    npc.unit.transform.set_xyz(101.0, 100.0, 100.0);
    let e = crate::world::Entity::new(obj_id, npc);
    server.world.world(0).unwrap().spawn(&e, &server.view_ctx());
    obj_id
}

#[tokio::test]
async fn refused_reports_leave_a_warning() {
    let (server, _rec) = server_with(|d| {
        d.add_template(quest(40));
        d.add_component(component(401, 40, kind::START));
        d.add_act(401, 401, 401, 0, ActKind::ConAutoComplete);
        d.add_component(component(402, 40, kind::PROGRESS));
        d.add_act(
            402,
            402,
            402,
            3,
            ActKind::ObjMonsterHunt {
                npc_id: 600,
                use_alias: false,
                alias_id: 0,
                highlight_doodad_id: 0,
                highlight_doodad_phase: -1,
            },
        );
        d.add_component(component(403, 40, kind::READY));
        d.add_act(
            403,
            403,
            403,
            0,
            ActKind::ConReportNpc {
                npc_id: 500,
                use_alias: false,
                alias_id: 0,
            },
        );
    });
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    events::add_quest(&server, &player, 40, false);
    server.systems.quests.run_queued_evaluations(&server);
    assert!(events::has_quest(&player, 40));
    let right = spawn_npc(&server, 500, 0x201);
    let wrong = spawn_npc(&server, 501, 0x202);

    // The NPC object does not exist.
    let w = warnings(|| events::on_report(&server, &player, 40, 0x999, 0, 0));
    assert!(
        w.iter()
            .any(|l| l.contains("Quest 40") && l.contains("2457")),
        "{w:?}"
    );
    // Another NPC than the quest's report NPC.
    let w = warnings(|| events::on_report(&server, &player, 40, wrong, 0, 0));
    assert!(
        w.iter()
            .any(|l| l.contains("Quest 40") && l.contains("501") && l.contains("500")),
        "{w:?}"
    );
    // The right NPC, objectives not complete.
    let w = warnings(|| events::on_report(&server, &player, 40, right, 0, 0));
    assert!(
        w.iter()
            .any(|l| l.contains("Quest 40") && l.contains("objectives are not complete")),
        "{w:?}"
    );
    // A quest the character does not have.
    let w = warnings(|| events::on_report(&server, &player, 41, right, 0, 0));
    assert!(
        w.iter()
            .any(|l| l.contains("Quest 41") && l.contains("not active")),
        "{w:?}"
    );
}
