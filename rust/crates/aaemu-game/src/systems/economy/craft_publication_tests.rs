//! The real craft -> quest component -> force_spawn callback contends with an expired
//! source-item doodad's real cleanup. Channel/gate waits are bounded on both threads.
use super::*;
use crate::data::items::ItemTemplate;
use crate::data::GameData;
use crate::models::character::Inventory;
use crate::models::doodad::{doodad_owner_type, Doodad};
use crate::systems::economy::bridges::EconomyForNpcs;
use crate::systems::economy::craft_data::{Craft, CraftProduct};
use crate::systems::npcs::doodad::spawner::{NpcSpawnerDef, SpawnerBase};
use crate::systems::npcs::doodad::templates::DoodadTemplate;
use crate::systems::npcs::npc::data::NpcTemplate;
use crate::systems::npcs::npc::spawner_data::{NpcSpawnerData, NpcSpawnerNpc, NpcSpawnerTemplate};
use crate::systems::npcs::spawn_manager;
use crate::systems::quests::{
    self, events,
    model::kind,
    templates::{ActKind, QuestComponentTemplate, QuestTemplate},
};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::Entity;
use std::sync::mpsc;
use std::time::Duration;

struct CraftQuest(mpsc::SyncSender<()>);
impl CraftHooks for CraftQuest {
    fn on_craft(&self, server: &GameServer, player: &PlayerRef, craft_id: u32) {
        assert!(!crate::systems::economy::System::ops_held_on_this_thread());
        self.0.send(()).unwrap();
        // The timer has reached the real outer gate. With reversed ordering it would
        // already own spawn_lock here and the following quest callback would deadlock.
        server.systems.economy.wait_until_operation_blocked();
        events::on_craft(server, player, craft_id);
        // Run the queued component while this callback still owns the outer item
        // scope, so the actual quest-driven spawn contends with expired cleanup.
        server.systems.quests.run_queued_evaluations(server);
    }
}

#[tokio::test]
async fn expired_source_item_doodad_contends_with_craft_quest_npc_spawn() {
    const SOURCE: u32 = 4001;
    const PRODUCT: u32 = 4002;
    let mut data = GameData::empty();
    for id in [SOURCE, PRODUCT] {
        data.items.templates.insert(id, ItemTemplate::plain(id, 1));
    }
    data.systems.economy.craft.crafts.insert(
        1,
        Craft {
            id: 1,
            craft_products: vec![CraftProduct {
                craft_id: 1,
                item_id: PRODUCT,
                amount: 1,
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    let q = &mut data.systems.quests;
    q.add_template(QuestTemplate {
        id: 1,
        ..Default::default()
    });
    for (id, step, spawner) in [
        (10, kind::START, 0),
        (11, kind::PROGRESS, 10),
        (12, kind::READY, 0),
    ] {
        q.add_component(QuestComponentTemplate {
            id,
            quest_id: 1,
            kind_id: step,
            npc_spawner_id: spawner,
            ..Default::default()
        });
    }
    q.add_act(
        11,
        11,
        11,
        1,
        ActKind::ObjCraft {
            craft_id: 1,
            use_alias: false,
            alias_id: 0,
            highlight_doodad_id: 0,
            highlight_doodad_phase: -1,
        },
    );
    q.add_act(
        12,
        12,
        12,
        0,
        ActKind::ConReportNpc {
            npc_id: 500,
            use_alias: false,
            alias_id: 0,
        },
    );
    q.finish();
    data.systems.npcs.npc.templates.insert(
        500,
        NpcTemplate {
            id: 500,
            ..Default::default()
        },
    );
    data.systems.npcs.npc_spawners = NpcSpawnerData::from_parts(
        vec![NpcSpawnerTemplate {
            id: 10,
            max_population: 1,
            ..Default::default()
        }],
        vec![NpcSpawnerNpc {
            id: 100,
            npc_spawner_template_id: 10,
            member_id: 500,
            member_type: "Npc".into(),
            weight: 1.0,
            unit_id: 0,
        }],
    )
    .unwrap();
    data.systems.npcs.doodad.templates.insert(
        7000,
        DoodadTemplate {
            id: 7000,
            ..Default::default()
        },
    );
    let server = Arc::new(test_server_with(data));
    server.set_self();
    quests::install_hooks(&server);
    server
        .systems
        .npcs
        .hooks
        .economy
        .install(Arc::new(EconomyForNpcs));
    let client = TestClient::new(&server, 1, 1001);
    let mut character = test_character(1, "Crafter", 100.0, 100.0);
    character.inventory = Inventory::new(1, 50, 50, &server.items, &server.ids.container);
    let player_obj_id = server.ids.object.get_next_id().unwrap();
    let player = spawn_player(&server, &client, character, player_obj_id);
    let world = server.world.world(0).unwrap();
    let manager = server.systems.npcs.spawns.get_or_create(&world);
    let mut base = SpawnerBase {
        unit_id: 500,
        ..Default::default()
    };
    base.position.x = 101.0;
    base.position.y = 100.0;
    base.position.z = 100.0;
    manager
        .add_npc_spawner(
            &server,
            NpcSpawnerDef {
                base,
                count: 1,
                npc_spawner_ids: vec![10],
            },
        )
        .unwrap();
    assert_eq!(manager.get_npc_spawner(10).len(), 1);
    events::add_quest(&server, &player, 1, false);
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(player.data().quests.active[0].step, kind::PROGRESS);
    assert!(world.all_npcs().is_empty());
    let source = server
        .items
        .create(
            SOURCE,
            1,
            0,
            1,
            &server.game_data.items,
            &server.ids.item,
            0,
        )
        .unwrap();
    let source_id = source.id;
    Inv::of(&player)
        .get(slot_type::SYSTEM)
        .unwrap()
        .lock()
        .unwrap()
        .add_new_item(source, -1, &server.game_data.items)
        .unwrap();
    let mut doodad = Doodad::new(7000);
    doodad.owner_type = doodad_owner_type::CHARACTER;
    doodad.owner_id = 1;
    doodad.item_id = source_id;
    doodad.item_template_id = SOURCE;
    let doodad = Entity::new(server.ids.object.get_next_id().unwrap(), doodad);
    world.spawn(&doodad, &server.view_ctx());
    assert_ne!(doodad.obj_id, player.obj_id);
    assert!(world
        .doodad(doodad.obj_id)
        .is_some_and(|live| Arc::ptr_eq(&live, &doodad)));
    manager.add_player_doodad(doodad.clone());
    manager.add_despawn(
        &server,
        WorldObject::from(&doodad),
        Some(spawn_manager::now()),
    );
    server.systems.economy.craft.update(1, |state| {
        state.current_craft = Some(1);
        state.count = 1;
    });
    let (start, ready) = mpsc::sync_channel(1);
    server
        .systems
        .economy
        .craft
        .hooks
        .install(Arc::new(CraftQuest(start)));
    let (tick_done, tick_result) = mpsc::channel();
    let (craft_done, craft_result) = mpsc::channel();
    let runtime = tokio::runtime::Handle::current();
    let (tick_server, tick_runtime) = (server.clone(), runtime.clone());
    let tick_manager = manager.clone();
    let tick = std::thread::spawn(move || {
        let _runtime = tick_runtime.enter();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        tick_manager.check_respawns_tick(&tick_server);
        tick_done.send(()).unwrap();
    });
    let (craft_server, crafter) = (server.clone(), player.clone());
    let craft = std::thread::spawn(move || {
        let _runtime = runtime.enter();
        end_craft(&craft_server, &crafter);
        craft_done.send(()).unwrap();
    });
    craft_result
        .recv_timeout(Duration::from_secs(5))
        .expect("craft/quest/spawn cycle must finish");
    tick_result
        .recv_timeout(Duration::from_secs(5))
        .expect("expired source cleanup must finish");
    craft.join().unwrap();
    tick.join().unwrap();
    assert_eq!(
        Inv::of(&player).items_count_in(slot_type::INVENTORY, PRODUCT, -1),
        1
    );
    assert_eq!(player.data().quests.active[0].step, kind::READY);
    assert_eq!(player.data().quests.active[0].objectives[0], 1);
    let npcs = world.all_npcs();
    assert_eq!(npcs.len(), 1);
    assert_eq!(npcs[0].data().unit.template_id, 500);
    assert!(world.doodad(doodad.obj_id).is_none());
    assert!(world.object(doodad.obj_id).is_none());
    assert!(doodad.data().phase.deleted);
    assert!(!manager
        .get_all_player_doodads()
        .iter()
        .any(|live| Arc::ptr_eq(live, &doodad)));
    assert!(!manager.has_pending_timers(doodad.obj_id));
    assert!(world
        .object(player.obj_id)
        .is_some_and(|live| live.same(&WorldObject::from(&player))));
    assert!(server.items.item_by_id(source_id).is_none());
    assert!(server.items.removed_items().contains(&source_id));
}
