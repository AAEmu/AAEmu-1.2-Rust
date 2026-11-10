use std::time::Duration;

use super::*;
use crate::data::world_data::{Zone, ZoneConflict};
use crate::data::world_templates::WorldSpawnPosition;
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::opcodes::sc;
use crate::systems::instances::data::{
    TowerDefGroupMember, TowerDefProgKillTarget, TowerDefProgSpawnTarget,
};
use crate::systems::npcs::doodad::spawner::{NpcSpawnerDef, SpawnerBase};
use crate::systems::npcs::npc::data::NpcTemplate;
use crate::systems::npcs::npc::spawner_data::{NpcSpawnerData, NpcSpawnerNpc, NpcSpawnerTemplate};
use crate::systems::npcs::spawn_manager::{backend::TestBackend, TemplateExists};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{DoodadRef, Entity, WorldInstance};

const KEY: EventKey = EventKey {
    world_id: 0,
    id: 12,
    zone_group_id: 5,
};

struct Backend;
impl TestBackend for Backend {
    fn npc_create(&self, server: &GameServer, _: u32, id: u32) -> Option<NpcRef> {
        let mut n = Npc::new(id);
        n.unit.hp = 100;
        n.unit.max_hp = 100;
        Some(Entity::new(server.ids.object.get_next_id().ok()?, n))
    }
    fn doodad_create(
        &self,
        server: &GameServer,
        _: &Arc<WorldInstance>,
        _: u32,
        template: u32,
    ) -> Option<DoodadRef> {
        Some(Entity::new(
            server.ids.object.get_next_id().ok()?,
            crate::models::doodad::Doodad::new(template),
        ))
    }
}

fn fixture() -> Arc<GameServer> {
    let mut data = GameData::empty();
    data.zones.zones.insert(
        TEST_ZONE,
        Zone {
            id: 77,
            zone_key: TEST_ZONE,
            group_id: 5,
            ..Default::default()
        },
    );
    let td = &mut data.systems.instances.tower_defs;
    td.tower_defs.insert(
        12,
        TowerDef {
            id: 12,
            target_npc_spawn_id: 10,
            first_wave_after: 3.0,
            force_end_time: 100.0,
            time_of_day: 18.0,
            time_of_day_day_interval: 1,
            progs: vec![69, 70],
            ..Default::default()
        },
    );
    td.progs.insert(
        69,
        TowerDefProg {
            id: 69,
            tower_def_id: 12,
            cond_to_next_time: 5.0,
            cond_comp_by_and: true,
            spawn_targets: vec![TowerDefProgSpawnTarget {
                id: 1,
                spawn_target_id: 20,
                spawn_target_type: "NpcSpawner".into(),
                despawn_on_next_step: true,
            }],
            kill_targets: vec![TowerDefProgKillTarget {
                id: 1,
                kill_target_id: 700,
                kill_target_type: "Npc".into(),
                kill_count: 2,
            }],
        },
    );
    td.progs.insert(
        70,
        TowerDefProg {
            id: 70,
            tower_def_id: 12,
            cond_to_next_time: 10.0,
            cond_comp_by_and: true,
            ..Default::default()
        },
    );
    for id in [500, 600, 700] {
        data.systems.npcs.npc.templates.insert(
            id,
            NpcTemplate {
                id,
                ..Default::default()
            },
        );
    }
    data.systems.npcs.npc_spawners = NpcSpawnerData::from_parts(
        vec![
            NpcSpawnerTemplate::with_member(10, 500),
            NpcSpawnerTemplate::with_member(20, 600),
        ],
        vec![
            NpcSpawnerNpc {
                id: 1,
                ..NpcSpawnerNpc::with_member(10, 500)
            },
            NpcSpawnerNpc {
                id: 2,
                ..NpcSpawnerNpc::with_member(20, 600)
            },
        ],
    )
    .unwrap();
    let server = Arc::new(test_server_with(data));
    *server.systems.npcs.spawns.test_backend.lock().unwrap() = Some(Arc::new(Backend));
    let world = server.world.world(0).unwrap();
    let spawns = server.systems.npcs.spawns.get_or_create(&world);
    for id in [500, 600] {
        spawns
            .add_npc_spawner(
                &server,
                NpcSpawnerDef {
                    base: SpawnerBase {
                        unit_id: id,
                        position: WorldSpawnPosition {
                            zone_id: TEST_ZONE,
                            x: 100.0,
                            y: 100.0,
                            z: 100.0,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    count: 1,
                    npc_spawner_ids: Vec::new(),
                },
            )
            .unwrap();
    }
    server
}

fn advance(seconds: u64) {
    spawn_manager::advance_test_clock(Duration::from_secs(seconds));
}

const HALCYONA_GROUP: u16 = 22;

/// Zone group 22 (Halcyona) with the Golden Plains War (18), its victory follow-ups (19, 20)
/// and the two Ynystere rift definitions (5, 15), all on the synthetic main world.
fn halcyona_fixture() -> Arc<GameServer> {
    let mut data = GameData::empty();
    data.zones.zones.insert(
        TEST_ZONE,
        Zone {
            id: 77,
            zone_key: TEST_ZONE,
            group_id: u32::from(HALCYONA_GROUP),
            ..Default::default()
        },
    );
    data.zones.conflicts.push(ZoneConflict {
        zone_group_id: HALCYONA_GROUP,
        conflict_min: 15,
        war_min: 80,
        peace_min: 240,
        war_tower_def_id: 18,
        ..Default::default()
    });
    for id in [500, 600, 700, 13647, 13661] {
        data.systems.npcs.npc.templates.insert(
            id,
            NpcTemplate {
                id,
                ..Default::default()
            },
        );
    }
    let td = &mut data.systems.instances.tower_defs;
    td.tower_defs.insert(
        18,
        TowerDef {
            id: 18,
            target_npc_spawn_id: 10,
            first_wave_after: 3.0,
            force_end_time: 100.0,
            time_of_day_day_interval: 1,
            progs: vec![1],
            ..Default::default()
        },
    );
    td.progs.insert(
        1,
        TowerDefProg {
            id: 1,
            tower_def_id: 18,
            spawn_targets: vec![TowerDefProgSpawnTarget {
                id: 1,
                spawn_target_id: 20,
                spawn_target_type: "NpcSpawner".into(),
                despawn_on_next_step: false,
            }],
            kill_targets: vec![
                TowerDefProgKillTarget {
                    id: 1,
                    kill_target_id: 13661,
                    kill_target_type: "Npc".into(),
                    kill_count: 1,
                },
                TowerDefProgKillTarget {
                    id: 2,
                    kill_target_id: 13647,
                    kill_target_type: "Npc".into(),
                    kill_count: 1,
                },
            ],
            ..Default::default()
        },
    );
    for (id, relic) in [(19, 13661), (20, 13647)] {
        td.tower_defs.insert(
            id,
            TowerDef {
                id,
                target_npc_spawn_id: 20,
                force_end_time: 100.0,
                time_of_day_day_interval: 1,
                kill_npc_id: relic,
                kill_npc_count: 1,
                ..Default::default()
            },
        );
    }
    for (id, time_of_day) in [(5, 12.0), (15, 0.1)] {
        td.tower_defs.insert(
            id,
            TowerDef {
                id,
                target_npc_spawn_id: 20,
                first_wave_after: 3.0,
                force_end_time: 100.0,
                time_of_day,
                time_of_day_day_interval: 1,
                ..Default::default()
            },
        );
    }
    data.systems.npcs.npc_spawners = NpcSpawnerData::from_parts(
        vec![
            NpcSpawnerTemplate::with_member(10, 500),
            NpcSpawnerTemplate::with_member(20, 600),
        ],
        vec![
            NpcSpawnerNpc {
                id: 1,
                ..NpcSpawnerNpc::with_member(10, 500)
            },
            NpcSpawnerNpc {
                id: 2,
                ..NpcSpawnerNpc::with_member(20, 600)
            },
        ],
    )
    .unwrap();
    let server = Arc::new(test_server_with(data));
    *server.systems.npcs.spawns.test_backend.lock().unwrap() = Some(Arc::new(Backend));
    let world = server.world.world(0).unwrap();
    let spawns = server.systems.npcs.spawns.get_or_create(&world);
    for id in [500, 600] {
        spawns
            .add_npc_spawner(
                &server,
                NpcSpawnerDef {
                    base: SpawnerBase {
                        unit_id: id,
                        position: WorldSpawnPosition {
                            zone_id: TEST_ZONE,
                            x: 100.0,
                            y: 100.0,
                            z: 100.0,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    count: 1,
                    npc_spawner_ids: Vec::new(),
                },
            )
            .unwrap();
    }
    server
}

fn scheduled_ids(server: &GameServer) -> Vec<u32> {
    manager(server)
        .state
        .lock()
        .unwrap()
        .events
        .iter()
        .map(|event| event.id)
        .collect()
}

#[tokio::test]
async fn tower_def_default_events_cover_the_1_2_open_world() {
    let server = halcyona_fixture();
    let events = default_events(&server);
    let ids: Vec<u32> = events.iter().map(|event| event.id).collect();
    assert_eq!(ids, vec![5, 15, 18, 19, 20]);
    let war = events.iter().find(|event| event.id == 18).unwrap();
    assert_eq!(war.world_id, 0);
    assert_eq!(war.zone_group_id, HALCYONA_GROUP);
    assert_eq!(war.zone_id, TEST_ZONE);
    for id in [19, 20] {
        let followup = events.iter().find(|event| event.id == id).unwrap();
        assert_eq!(followup.zone_group_id, HALCYONA_GROUP);
        assert_eq!(followup.zone_id, TEST_ZONE);
    }
    // The KR 1.2 profile gets the same data-derived schedule.
    server.config.update(|config| {
        config.world.profile = crate::config::ServerProfile::Kr12;
    });
    assert_eq!(default_events(&server), events);
}

#[tokio::test]
async fn tower_def_explicit_events_override_defaults_and_disable_empties_them() {
    let server = halcyona_fixture();
    initialize(&server).unwrap();
    assert_eq!(scheduled_ids(&server), vec![5, 15, 18, 19, 20]);
    server.config.update(|config| {
        config.dungeons.tower_defense.events = vec![TowerDefenseEventConfig {
            id: 5,
            world_id: 0,
            zone_group_id: HALCYONA_GROUP,
            zone_id: TEST_ZONE,
        }];
    });
    initialize(&server).unwrap();
    assert_eq!(scheduled_ids(&server), vec![5]);
    server
        .config
        .update(|config| config.dungeons.tower_defense.enabled = false);
    initialize(&server).unwrap();
    assert!(scheduled_ids(&server).is_empty());
}

#[tokio::test]
async fn tower_def_default_halcyona_war_starts_and_stops_with_zone_22() {
    let server = halcyona_fixture();
    let conflict = server.game_data.zones.conflicts[0].clone();
    server
        .systems
        .social
        .conflicts
        .init(std::slice::from_ref(&conflict), crate::world::now_ms());
    // 06:00 game time: neither rift's time of day is due, so only the war can start.
    server.time.set(6.0);
    initialize(&server).unwrap();
    assert_eq!(scheduled_ids(&server), vec![5, 15, 18, 19, 20]);
    tick(&server);
    assert!(infos(&server, 0).is_empty());

    server.systems.social.conflicts.set_state(
        &server,
        HALCYONA_GROUP,
        crate::data::world_data::conflict_type::WAR,
    );
    tick(&server);
    let running = infos(&server, 0);
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].key.tower_def_id, 18);
    assert_eq!(running[0].key.zone_group_id, HALCYONA_GROUP);
    assert!(
        server
            .world
            .world(0)
            .unwrap()
            .npc_by_template_id(500)
            .is_some(),
        "the war controller spawned in the open world"
    );

    server.systems.social.conflicts.set_state(
        &server,
        HALCYONA_GROUP,
        crate::data::world_data::conflict_type::PEACE,
    );
    tick(&server);
    assert!(infos(&server, 0).is_empty());
    assert!(server
        .world
        .world(0)
        .unwrap()
        .npc_by_template_id(500)
        .is_none());
}

#[tokio::test]
async fn tower_def_doodad_waves_clean_only_their_owned_objects() {
    let mut server = fixture();
    let data = Arc::get_mut(&mut Arc::get_mut(&mut server).unwrap().game_data).unwrap();
    for id in [8000, 8001] {
        data.systems.npcs.doodad.templates.insert(
            id,
            crate::systems::npcs::doodad::templates::DoodadTemplate {
                id,
                ..Default::default()
            },
        );
        data.systems
            .instances
            .tower_defs
            .progs
            .get_mut(&69)
            .unwrap()
            .spawn_targets
            .push(TowerDefProgSpawnTarget {
                id,
                spawn_target_id: id,
                spawn_target_type: "DoodadAlmighty".into(),
                despawn_on_next_step: id == 8000,
            });
    }
    let dir = std::env::temp_dir().join(format!("aaemu-tower-doodads-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("npc_spawns.json"),
        r#"[
        {"UnitId":500,"Position":{"X":100,"Y":100,"Z":100}},
        {"UnitId":600,"Position":{"X":100,"Y":100,"Z":100}}
    ]"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("doodad_spawns.json"),
        r#"[
        {"UnitId":8000,"Position":{"X":101,"Y":100,"Z":100}},
        {"UnitId":8001,"Position":{"X":102,"Y":100,"Z":100}}
    ]"#,
    )
    .unwrap();
    let world = server.world.world(0).unwrap();
    server
        .systems
        .npcs
        .spawns
        .get(0)
        .unwrap()
        .load_spawn_files(
            &server,
            &dir,
            &TemplateExists {
                npc: &|_| true,
                doodad: &|_| true,
                transfer: &|_| false,
                gimmick: &|_| false,
                slave: &|_| false,
            },
        )
        .unwrap();
    std::fs::remove_file(dir.join("npc_spawns.json")).unwrap();
    std::fs::remove_file(dir.join("doodad_spawns.json")).unwrap();
    std::fs::remove_dir(&dir).unwrap();
    let unrelated = Entity::new(9000, crate::models::doodad::Doodad::new(8000));
    world.spawn(&unrelated, &server.view_ctx());
    start(&server, KEY, TEST_ZONE).unwrap();
    next_wave(&server, KEY, 1).unwrap();
    let owned: Vec<_> = world
        .all_doodads()
        .into_iter()
        .filter(|d| !Arc::ptr_eq(d, &unrelated))
        .collect();
    assert_eq!(owned.len(), 2);
    next_wave(&server, KEY, 2).unwrap();
    // The new wave may reuse a released ID; only the original handle can survive.
    for doodad in &owned {
        assert_eq!(
            world
                .doodad(doodad.obj_id)
                .is_some_and(|current| Arc::ptr_eq(&current, doodad)),
            doodad.data().template_id == 8001
        );
    }
    end(&server, KEY).unwrap();
    assert!(owned.iter().all(|d| world.doodad(d.obj_id).is_none()));
    assert!(world.doodad(unrelated.obj_id).is_some());
}

fn kill(server: &GameServer, id: u32, template: u32) -> NpcRef {
    let mut n = Npc::new(template);
    n.unit.transform.instance_id = 0;
    n.unit.transform.zone_id = TEST_ZONE;
    let npc = Entity::new(id, n);
    super::super::events::on_do_die(server, &WorldObject::from(&npc), None);
    npc
}

#[tokio::test]
async fn tower_def_lifecycle_broadcasts_progresses_and_cleans_up() {
    let server = fixture();
    let mut c1 = TestClient::new(&server, 1, 1);
    let mut c2 = TestClient::new(&server, 2, 2);
    let p1 = spawn_player(&server, &c1, test_character(1, "One", 100.0, 100.0), 5000);
    spawn_player(&server, &c2, test_character(2, "Two", 100.0, 100.0), 5001);
    c1.drain();
    c2.drain();
    send_zone_list(&server, &p1);
    assert!(c1.drain().is_empty());
    assert!(
        server
            .systems
            .npcs
            .spawns
            .get(0)
            .unwrap()
            .get_all_spawners()
            .is_empty(),
        "controllers must not spawn at startup"
    );
    start(&server, KEY, TEST_ZONE).unwrap();
    for client in [&mut c1, &mut c2] {
        assert!(client.drain_ops().contains(&sc::SCTowerDefStartPacket));
    }
    assert_eq!(infos(&server, 0)[0].current_step, 0);
    assert!(start(&server, KEY, TEST_ZONE).is_err());
    assert!(next_wave(&server, KEY, 2).is_err());
    advance(3);
    tick(&server);
    assert_eq!(infos(&server, 0)[0].current_step, 1);
    let wave_npc = server
        .world
        .world(0)
        .unwrap()
        .npc_by_template_id(600)
        .unwrap();
    // Descendants inherit cleanup policy from the controller.
    let mut child = Npc::new(700);
    child.unit.hp = 100;
    child.unit.max_hp = 100;
    child.unit.owner_id = wave_npc.obj_id;
    child.unit.transform.zone_id = TEST_ZONE;
    let child = Entity::new(server.ids.object.get_next_id().unwrap(), child);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&child, &server.view_ctx());
    on_spawn(&server, &child);
    let mut summoned = Npc::new(700);
    summoned.unit.hp = 100;
    summoned.unit.max_hp = 100;
    let summoned = Entity::new(server.ids.object.get_next_id().unwrap(), summoned);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&summoned, &server.view_ctx());
    assert_eq!(summoned.data().unit.owner_id, 0);
    on_summoned(
        &server,
        &WorldObject::from(&wave_npc),
        std::slice::from_ref(&summoned),
    );
    let unrelated = Entity::new(9000, Npc::new(700));
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&unrelated, &server.view_ctx());
    // Death callbacks are deduplicated. The timer and both kills are required.
    let dead = kill(&server, 8000, 700);
    on_death(&server, &WorldObject::from(&dead));
    advance(5);
    tick(&server);
    assert_eq!(infos(&server, 0)[0].current_step, 1);
    // A new handle reusing the same id counts as a new death.
    kill(&server, 8000, 700);
    tick(&server);
    assert_eq!(infos(&server, 0)[0].current_step, 2);
    assert!(server.world.object(wave_npc.obj_id).is_none());
    assert!(server.world.object(child.obj_id).is_none());
    assert!(server.world.object(summoned.obj_id).is_none());
    assert!(server.world.object(unrelated.obj_id).is_some());
    send_zone_list(&server, &p1);
    assert!(c1.drain_ops().contains(&sc::SCTowerDefListPacket));
    advance(10);
    tick(&server);
    assert!(infos(&server, 0).is_empty());
    assert!(c2.drain_ops().contains(&sc::SCTowerDefEndPacket));
    assert!(server
        .world
        .world(0)
        .unwrap()
        .npc_by_template_id(500)
        .is_none());
    start(&server, KEY, TEST_ZONE).unwrap();
    cleanup_world(&server, 0);
    assert!(infos(&server, 0).is_empty());
}

#[test]
fn tower_def_or_conditions_use_any_kill_or_timer() {
    let p = TowerDefProg {
        cond_comp_by_and: false,
        cond_to_next_time: 10.0,
        kill_targets: vec![
            TowerDefProgKillTarget {
                id: 1,
                kill_target_id: 7,
                kill_target_type: "Npc".into(),
                kill_count: 2,
            },
            TowerDefProgKillTarget {
                id: 2,
                kill_target_id: 8,
                kill_target_type: "Npc".into(),
                kill_count: 1,
            },
        ],
        ..Default::default()
    };
    assert!(!wave_complete(&p, &HashMap::new(), 1.0));
    assert!(wave_complete(&p, &HashMap::from([(8, 1)]), 1.0));
    assert!(wave_complete(&p, &HashMap::new(), 10.0));
}

#[tokio::test]
async fn tower_def_cleanup_preserves_loot_corpses_until_their_normal_timer() {
    let server = fixture();
    start(&server, KEY, TEST_ZONE).unwrap();
    next_wave(&server, KEY, 1).unwrap();
    let corpse = server
        .world
        .world(0)
        .unwrap()
        .npc_by_template_id(600)
        .unwrap();
    {
        let mut npc = corpse.data();
        npc.unit.hp = 0;
        npc.unit.has_loot_left = true;
        npc.dead_time_ms = crate::world::now_ms();
    }
    let spawns = server.systems.npcs.spawns.get(0).unwrap();
    let deadline = spawn_manager::add_secs(spawn_manager::now(), 30.0);
    spawns.add_despawn(&server, WorldObject::from(&corpse), Some(deadline));
    spawns.add_respawn(&server, WorldObject::from(&corpse), Some(deadline));
    next_wave(&server, KEY, 2).unwrap();
    end(&server, KEY).unwrap();
    assert!(infos(&server, 0).is_empty());
    assert!(server.world.object(corpse.obj_id).is_some());
    assert!(corpse.data().unit.has_loot_left);
    assert!(spawns.pending_respawns().is_empty());
    assert_eq!(
        spawns.pending_despawns(),
        vec![(corpse.obj_id, Some(deadline))]
    );
    advance(29);
    spawns.check_respawns_tick(&server);
    assert!(server.world.object(corpse.obj_id).is_some());
    advance(1);
    spawns.check_respawns_tick(&server);
    assert!(server.world.object(corpse.obj_id).is_none());

    start(&server, KEY, TEST_ZONE).unwrap();
    let target = server
        .world
        .world(0)
        .unwrap()
        .npc_by_template_id(500)
        .unwrap();
    target.data().unit.hp = 0;
    target.data().unit.has_loot_left = true;
    target.data().dead_time_ms = crate::world::now_ms();
    spawns.add_despawn(&server, WorldObject::from(&target), Some(deadline));
    cleanup_world(&server, 0);
    assert!(server.world.object(target.obj_id).is_none());
    assert!(spawns.pending_despawns().is_empty());
}

#[tokio::test]
async fn tower_def_gm_controls_run_the_event_and_report_readiness() {
    use crate::systems::services::commands::scripts::{self, harness, ScriptEnv};
    let server = fixture();
    scripts::install(&server.systems.services.commands, ScriptEnv::default());
    assert!(server.systems.services.commands.reload());
    let mut gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
    let (_, out) = harness::run(&server, &gm.p, "towerdef list").await;
    assert!(out
        .iter()
        .any(|s| s.contains("World 0, zone 100, zone group 5")));
    assert!(out.iter().any(|s| s.contains("12: ready")));
    gm.frames();
    harness::run_at(&server, &gm.p, "towerdef start 12", 0).await;
    assert!(infos(&server, 0).is_empty());
    let (_, out) = harness::run(&server, &gm.p, "tower_def start 12").await;
    assert!(out[0].contains("Tower defense 12: start"));
    assert_eq!(infos(&server, 0)[0].key.zone_group_id, 5);
    harness::run(&server, &gm.p, "towerdef next 12 1").await;
    assert_eq!(infos(&server, 0)[0].current_step, 1);
    let (_, out) = harness::run(&server, &gm.p, "towerdef status").await;
    assert!(out.iter().any(|s| s.contains("12: running, wave 1")));
    harness::run(&server, &gm.p, "towerdef end 12").await;
    assert!(infos(&server, 0).is_empty());
}

#[tokio::test]
async fn tower_def_end_kills_are_scoped_and_target_death_ends_the_event() {
    let mut server = fixture();
    let data = Arc::get_mut(&mut Arc::get_mut(&mut server).unwrap().game_data).unwrap();
    let def = data
        .systems
        .instances
        .tower_defs
        .tower_defs
        .get_mut(&12)
        .unwrap();
    def.kill_npc_count = 1;
    start(&server, KEY, TEST_ZONE).unwrap();
    let mut outside = Npc::new(500);
    outside.unit.transform.instance_id = 1;
    outside.unit.transform.zone_id = TEST_ZONE;
    on_death(&server, &WorldObject::from(&Entity::new(8000, outside)));
    tick(&server);
    assert_eq!(infos(&server, 0).len(), 1);
    // Reusing the target's id does not make an unrelated NPC the defended target.
    let target = server
        .world
        .world(0)
        .unwrap()
        .npc_by_template_id(500)
        .unwrap();
    let mut replacement = Npc::new(700);
    replacement.unit.transform.zone_id = TEST_ZONE;
    on_death(
        &server,
        &WorldObject::from(&Entity::new(target.obj_id, replacement)),
    );
    tick(&server);
    assert_eq!(infos(&server, 0).len(), 1);
    // A summoned NPC at step zero is not the defended target.
    let mut child = Npc::new(700);
    child.unit.owner_id = target.obj_id;
    child.unit.transform.zone_id = TEST_ZONE;
    let child = Entity::new(8100, child);
    on_spawn(&server, &child);
    on_death(&server, &WorldObject::from(&child));
    tick(&server);
    assert_eq!(infos(&server, 0).len(), 1);
    on_death(&server, &WorldObject::from(&target));
    tick(&server);
    assert!(infos(&server, 0).is_empty());
}

#[tokio::test]
async fn tower_def_preflight_and_force_end() {
    let mut server = fixture();
    let data = Arc::get_mut(&mut Arc::get_mut(&mut server).unwrap().game_data).unwrap();
    data.systems
        .instances
        .tower_defs
        .progs
        .get_mut(&70)
        .unwrap()
        .spawn_targets
        .push(TowerDefProgSpawnTarget {
            id: 2,
            spawn_target_id: 7297,
            spawn_target_type: "DoodadAlmighty".into(),
            despawn_on_next_step: false,
        });
    assert!(start(&server, KEY, TEST_ZONE)
        .unwrap_err()
        .to_string()
        .contains("Missing doodad template"));
    assert!(infos(&server, 0).is_empty());
    assert!(server.world.world(0).unwrap().all_npcs().is_empty());
    let data = Arc::get_mut(&mut Arc::get_mut(&mut server).unwrap().game_data).unwrap();
    data.systems
        .instances
        .tower_defs
        .progs
        .get_mut(&70)
        .unwrap()
        .spawn_targets
        .clear();
    start(&server, KEY, TEST_ZONE).unwrap();
    let target = server
        .world
        .world(0)
        .unwrap()
        .npc_by_template_id(500)
        .unwrap();
    let s = server
        .systems
        .npcs
        .spawns
        .npc_spawner_of(target.obj_id)
        .unwrap();
    s.do_despawn(&server, &target);
    advance(100);
    tick(&server);
    assert!(infos(&server, 0).is_empty());
    let m = server.systems.npcs.spawns.get(0).unwrap();
    assert!(m.pending_despawns().is_empty());
    assert!(m.pending_respawns().is_empty());
    m.check_respawns_tick(&server);
}

#[tokio::test]
async fn tower_def_schedule_handles_midnight_intervals_and_the_disable_switch() {
    let mut server = fixture();
    server.config.update(|c| {
        // `Enabled: false` is the switch that turns every event off, defaults included.
        c.dungeons.tower_defense.enabled = false;
        c.dungeons
            .tower_defense
            .events
            .push(TowerDefenseEventConfig {
                id: 12,
                world_id: 0,
                zone_group_id: 5,
                zone_id: TEST_ZONE,
            })
    });
    server.time.set(17.0);
    initialize(&server).unwrap();
    server.time.set(18.0);
    tick(&server);
    assert!(infos(&server, 0).is_empty());
    server
        .config
        .update(|c| c.dungeons.tower_defense.enabled = true);
    server.time.set(17.0);
    initialize(&server).unwrap();
    server.time.set(18.1);
    tick(&server);
    assert_eq!(infos(&server, 0).len(), 1);
    end(&server, KEY).unwrap();
    tick(&server);
    assert!(
        infos(&server, 0).is_empty(),
        "must not restart in the same clock interval"
    );
    let data = Arc::get_mut(&mut Arc::get_mut(&mut server).unwrap().game_data).unwrap();
    let def = data
        .systems
        .instances
        .tower_defs
        .tower_defs
        .get_mut(&12)
        .unwrap();
    def.time_of_day = 0.0;
    def.time_of_day_day_interval = 2;
    server.time.set(23.9);
    tick(&server);
    server.time.set(0.0);
    tick(&server);
    assert!(infos(&server, 0).is_empty(), "skip day 1");
    server.time.set(23.9);
    tick(&server);
    server.time.set(0.0);
    tick(&server);
    assert_eq!(infos(&server, 0).len(), 1, "start on day 2 at midnight");
}

#[tokio::test]
async fn tower_def_war_event_runs_once_per_war_and_stops_on_peace() {
    let mut server = fixture();
    let s = Arc::get_mut(&mut server).unwrap();
    s.config.update(|c| {
        c.dungeons.tower_defense.enabled = true;
        c.dungeons
            .tower_defense
            .events
            .push(TowerDefenseEventConfig {
                id: 12,
                world_id: 0,
                zone_group_id: 5,
                zone_id: TEST_ZONE,
            });
    });
    let conflict = ZoneConflict {
        zone_group_id: 5,
        war_tower_def_id: 12,
        ..Default::default()
    };
    Arc::get_mut(&mut s.game_data)
        .unwrap()
        .zones
        .conflicts
        .push(conflict.clone());
    server
        .systems
        .social
        .conflicts
        .init(&[conflict], crate::world::now_ms());
    initialize(&server).unwrap();
    server.systems.social.conflicts.set_state(
        &server,
        5,
        crate::data::world_data::conflict_type::WAR,
    );
    tick(&server);
    assert_eq!(infos(&server, 0).len(), 1);
    end(&server, KEY).unwrap();
    tick(&server);
    assert!(infos(&server, 0).is_empty());
    server.systems.social.conflicts.set_state(
        &server,
        5,
        crate::data::world_data::conflict_type::PEACE,
    );
    tick(&server);
    server.systems.social.conflicts.set_state(
        &server,
        5,
        crate::data::world_data::conflict_type::WAR,
    );
    tick(&server);
    assert_eq!(infos(&server, 0).len(), 1);
    server.systems.social.conflicts.set_state(
        &server,
        5,
        crate::data::world_data::conflict_type::PEACE,
    );
    tick(&server);
    assert!(infos(&server, 0).is_empty());
}

/// `/towerdef list` names why a definition is unavailable in the GM's zone group: its
/// controllers stand in another zone group, or the spawn data has no position for them at all.
#[tokio::test]
async fn tower_def_war_event_started_by_a_gm_counts_as_this_wars_run() {
    let mut server = fixture();
    let s = Arc::get_mut(&mut server).unwrap();
    s.config.update(|c| {
        c.dungeons.tower_defense.enabled = true;
        c.dungeons
            .tower_defense
            .events
            .push(TowerDefenseEventConfig {
                id: 12,
                world_id: 0,
                zone_group_id: 5,
                zone_id: TEST_ZONE,
            });
    });
    let conflict = ZoneConflict {
        zone_group_id: 5,
        war_tower_def_id: 12,
        ..Default::default()
    };
    Arc::get_mut(&mut s.game_data)
        .unwrap()
        .zones
        .conflicts
        .push(conflict.clone());
    server
        .systems
        .social
        .conflicts
        .init(&[conflict], crate::world::now_ms());
    initialize(&server).unwrap();
    server.systems.social.conflicts.set_state(
        &server,
        5,
        crate::data::world_data::conflict_type::WAR,
    );
    // A GM already started the event: the war tick adopts it instead of failing to start it.
    start(&server, KEY, TEST_ZONE).unwrap();
    tick(&server);
    assert_eq!(infos(&server, 0).len(), 1);
    end(&server, KEY).unwrap();
    tick(&server);
    assert!(
        infos(&server, 0).is_empty(),
        "no second run in the same war"
    );
    server.systems.social.conflicts.set_state(
        &server,
        5,
        crate::data::world_data::conflict_type::PEACE,
    );
    tick(&server);
    server.systems.social.conflicts.set_state(
        &server,
        5,
        crate::data::world_data::conflict_type::WAR,
    );
    tick(&server);
    assert_eq!(infos(&server, 0).len(), 1, "the next war starts it again");
}

#[tokio::test]
async fn tower_def_preflight_names_where_missing_controllers_stand() {
    let mut server = fixture();
    let data = Arc::get_mut(&mut Arc::get_mut(&mut server).unwrap().game_data).unwrap();
    data.zones.zones.insert(
        999,
        Zone {
            id: 78,
            zone_key: 999,
            group_id: 6,
            ..Default::default()
        },
    );
    data.systems.npcs.npc.templates.insert(
        502,
        NpcTemplate {
            id: 502,
            ..Default::default()
        },
    );
    data.systems.npcs.npc_spawners = NpcSpawnerData::from_parts(
        vec![
            NpcSpawnerTemplate::with_member(10, 500),
            NpcSpawnerTemplate::with_member(20, 600),
            NpcSpawnerTemplate::with_member(25, 502),
            NpcSpawnerTemplate::with_member(26, 502),
        ],
        vec![
            NpcSpawnerNpc {
                id: 1,
                ..NpcSpawnerNpc::with_member(10, 500)
            },
            NpcSpawnerNpc {
                id: 2,
                ..NpcSpawnerNpc::with_member(20, 600)
            },
            NpcSpawnerNpc {
                id: 3,
                ..NpcSpawnerNpc::with_member(25, 502)
            },
            NpcSpawnerNpc {
                id: 4,
                ..NpcSpawnerNpc::with_member(26, 502)
            },
        ],
    )
    .unwrap();
    let wave = |id| TowerDefProgSpawnTarget {
        id: 9,
        spawn_target_id: id,
        spawn_target_type: "NpcSpawner".into(),
        despawn_on_next_step: false,
    };
    data.systems
        .instances
        .tower_defs
        .progs
        .get_mut(&70)
        .unwrap()
        .spawn_targets
        .push(wave(25));
    let error = start(&server, KEY, TEST_ZONE).unwrap_err().to_string();
    assert!(
        error.contains("Missing NPC spawner 25 positions in zone group 5")
            && error.contains("the spawn data has none"),
        "{error}"
    );

    let data = Arc::get_mut(&mut Arc::get_mut(&mut server).unwrap().game_data).unwrap();
    data.systems
        .instances
        .tower_defs
        .progs
        .get_mut(&70)
        .unwrap()
        .spawn_targets = vec![wave(26)];
    server
        .systems
        .npcs
        .spawns
        .get(0)
        .unwrap()
        .add_npc_spawner(
            &server,
            NpcSpawnerDef {
                base: SpawnerBase {
                    unit_id: 502,
                    position: WorldSpawnPosition {
                        zone_id: 999,
                        x: 100.0,
                        y: 100.0,
                        z: 100.0,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                count: 1,
                npc_spawner_ids: Vec::new(),
            },
        )
        .unwrap();
    let error = start(&server, KEY, TEST_ZONE).unwrap_err().to_string();
    assert!(
        error.contains("Missing NPC spawner 26 positions in zone group 5")
            && error.contains("authored in zone group 6"),
        "{error}"
    );
}

fn active_ids(server: &GameServer) -> Vec<u32> {
    manager(server)
        .state
        .lock()
        .unwrap()
        .active
        .keys()
        .map(|k| k.id)
        .collect()
}

/// Positions (rounded) of the NPCs of a template in the world.
fn positions_of(server: &GameServer, template: u32) -> Vec<(i32, i32)> {
    let world = server.world.world(0).unwrap();
    let mut out: Vec<_> = world
        .all_npcs()
        .into_iter()
        .filter(|n| n.data().unit.template_id == template)
        .map(|n| {
            let p = world.world_pos_rot(&WorldObject::from(&n));
            (p.x.round() as i32, p.y.round() as i32)
        })
        .collect();
    out.sort_unstable();
    out
}

/// The shipped JSON has no entry for the `NpcGroup` of a wave spawner: it lists every member of
/// the group, one entry each, at the group's spawn site. The loader finds the sites (positions
/// whose entries are exactly the group's members), keeps them away from the ordinary world
/// spawns and the wave spawns every member at the site plus its formation offset.
#[tokio::test]
async fn tower_def_group_wave_spawns_the_members_at_every_group_site() {
    let mut server = fixture();
    let data = Arc::get_mut(&mut Arc::get_mut(&mut server).unwrap().game_data).unwrap();
    for id in [800, 801] {
        data.systems.npcs.npc.templates.insert(
            id,
            NpcTemplate {
                id,
                ..Default::default()
            },
        );
    }
    let group = NpcSpawnerNpc {
        id: 3,
        npc_spawner_template_id: 30,
        member_id: 40,
        member_type: "NpcGroup".into(),
        ..Default::default()
    };
    data.systems.npcs.npc_spawners = NpcSpawnerData::from_parts(
        vec![
            NpcSpawnerTemplate {
                id: 1,
                ..Default::default()
            },
            NpcSpawnerTemplate::with_member(10, 500),
            NpcSpawnerTemplate::with_member(20, 600),
            NpcSpawnerTemplate {
                id: 30,
                ..Default::default()
            },
        ],
        vec![
            NpcSpawnerNpc {
                id: 1,
                ..NpcSpawnerNpc::with_member(10, 500)
            },
            NpcSpawnerNpc {
                id: 2,
                ..NpcSpawnerNpc::with_member(20, 600)
            },
            group,
        ],
    )
    .unwrap();
    let td = &mut data.systems.instances.tower_defs;
    let member = |npc_id, is_leader, offset| TowerDefGroupMember {
        npc_id,
        is_leader,
        offset,
    };
    td.groups.insert(
        40,
        vec![
            member(800, true, [0.0, 0.0, 0.0]),
            member(800, false, [2.0, 0.0, 0.0]),
            member(801, false, [0.0, 3.0, 0.0]),
        ],
    );
    td.spawner_groups.insert(30, vec![40]);
    td.group_npcs = td.group_member_npcs();
    td.progs
        .get_mut(&70)
        .unwrap()
        .spawn_targets
        .push(TowerDefProgSpawnTarget {
            id: 2,
            spawn_target_id: 30,
            spawn_target_type: "NpcSpawner".into(),
            despawn_on_next_step: false,
        });
    let entry = |unit: u32, x: u32, y: u32| {
        format!(r#"{{"UnitId":{unit},"Position":{{"X":{x},"Y":{y},"Z":100}}}}"#)
    };
    let entries = [
        entry(500, 100, 100),
        entry(600, 100, 100),
        // Site A: the complete group; the entries come in any order.
        entry(800, 30, 30),
        entry(801, 30, 30),
        entry(800, 30, 30),
        // Site B: a second complete group.
        entry(800, 50, 30),
        entry(800, 50, 30),
        entry(801, 50, 30),
        // One member standing alone completes no group: an ordinary world spawn.
        entry(800, 30, 50),
    ];
    let dir = std::env::temp_dir().join(format!("aaemu-tower-groups-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("npc_spawns.json"),
        format!("[{}]", entries.join(",")),
    )
    .unwrap();
    let spawns = server.systems.npcs.spawns.get(0).unwrap();
    let loaded = spawns.load_spawn_files(
        &server,
        &dir,
        &TemplateExists {
            npc: &|_| true,
            doodad: &|_| false,
            transfer: &|_| false,
            gimmick: &|_| false,
            slave: &|_| false,
        },
    );
    std::fs::remove_file(dir.join("npc_spawns.json")).unwrap();
    std::fs::remove_dir(&dir).unwrap();
    loaded.unwrap();
    let mut sources: Vec<_> = spawns
        .tower_def_spawners(30)
        .iter()
        .map(|s| {
            (
                s.unit_id,
                s.position().x.round() as i32,
                s.position().y.round() as i32,
            )
        })
        .collect();
    sources.sort_unstable();
    assert_eq!(
        sources,
        vec![
            (800, 30, 30),
            (800, 32, 30),
            (800, 50, 30),
            (800, 52, 30),
            (801, 30, 33),
            (801, 50, 33),
        ],
        "every member of every complete group site, at its formation offset"
    );
    let ordinary: Vec<_> = spawns
        .get_all_spawners()
        .values()
        .flatten()
        .map(|s| {
            (
                s.unit_id,
                s.position().x.round() as i32,
                s.position().y.round() as i32,
            )
        })
        .collect();
    assert_eq!(
        ordinary,
        vec![(800, 30, 50)],
        "the group sites must not become permanent world spawns"
    );

    start(&server, KEY, TEST_ZONE).unwrap();
    next_wave(&server, KEY, 1).unwrap();
    assert!(positions_of(&server, 800).is_empty());
    next_wave(&server, KEY, 2).unwrap();
    assert_eq!(
        positions_of(&server, 800),
        vec![(30, 30), (32, 30), (50, 30), (52, 30)]
    );
    assert_eq!(positions_of(&server, 801), vec![(30, 33), (50, 33)]);
    end(&server, KEY).unwrap();
    assert!(positions_of(&server, 800).is_empty());
    assert!(positions_of(&server, 801).is_empty());
}

/// The Golden Plains War names the two relics as kill targets of its last wave, and the victory
/// definitions name the relic of the side that lost as their `kill_npc_id`: the relic's death
/// starts the follow-up, no clock does, and the follow-up is not ended by the same NPC.
#[tokio::test]
async fn tower_def_wave_kill_target_starts_the_victory_followup() {
    let mut server = fixture();
    let data = Arc::get_mut(&mut Arc::get_mut(&mut server).unwrap().game_data).unwrap();
    data.systems.npcs.npc.templates.insert(
        501,
        NpcTemplate {
            id: 501,
            ..Default::default()
        },
    );
    data.systems.instances.tower_defs.tower_defs.insert(
        13,
        TowerDef {
            id: 13,
            target_npc_spawn_id: 11,
            first_wave_after: 3.0,
            force_end_time: 100.0,
            time_of_day: 18.0,
            time_of_day_day_interval: 1,
            kill_npc_id: 700,
            kill_npc_count: 1,
            ..Default::default()
        },
    );
    data.systems.npcs.npc_spawners = NpcSpawnerData::from_parts(
        vec![
            NpcSpawnerTemplate::with_member(10, 500),
            NpcSpawnerTemplate::with_member(20, 600),
            NpcSpawnerTemplate::with_member(11, 501),
        ],
        vec![
            NpcSpawnerNpc {
                id: 1,
                ..NpcSpawnerNpc::with_member(10, 500)
            },
            NpcSpawnerNpc {
                id: 2,
                ..NpcSpawnerNpc::with_member(20, 600)
            },
            NpcSpawnerNpc {
                id: 3,
                ..NpcSpawnerNpc::with_member(11, 501)
            },
        ],
    )
    .unwrap();
    server
        .systems
        .npcs
        .spawns
        .get(0)
        .unwrap()
        .add_npc_spawner(
            &server,
            NpcSpawnerDef {
                base: SpawnerBase {
                    unit_id: 501,
                    position: WorldSpawnPosition {
                        zone_id: TEST_ZONE,
                        x: 110.0,
                        y: 100.0,
                        z: 100.0,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                count: 1,
                npc_spawner_ids: Vec::new(),
            },
        )
        .unwrap();
    let defs = &server.game_data.systems.instances.tower_defs.tower_defs;
    assert!(!is_victory_followup(&server, &defs[&12]));
    assert!(is_victory_followup(&server, &defs[&13]));

    // The clock never starts a follow-up, even when it is configured.
    server.config.update(|c| {
        c.dungeons.tower_defense.enabled = true;
        c.dungeons
            .tower_defense
            .events
            .push(TowerDefenseEventConfig {
                id: 13,
                world_id: 0,
                zone_group_id: 5,
                zone_id: TEST_ZONE,
            })
    });
    server.time.set(17.0);
    initialize(&server).unwrap();
    server.time.set(18.1);
    tick(&server);
    assert!(active_ids(&server).is_empty());

    start(&server, KEY, TEST_ZONE).unwrap();
    // The NPC is not a kill target of the wave that is running (the target at step zero).
    kill(&server, 7999, 700);
    tick(&server);
    assert_eq!(active_ids(&server), vec![12]);
    advance(3);
    tick(&server);
    assert_eq!(infos(&server, 0)[0].current_step, 1);
    // The wave needs two kills; the first one does not meet the kill target.
    kill(&server, 8000, 700);
    tick(&server);
    assert_eq!(active_ids(&server), vec![12]);
    kill(&server, 8001, 700);
    tick(&server);
    assert_eq!(active_ids(&server), vec![12, 13]);
    assert!(server
        .world
        .world(0)
        .unwrap()
        .npc_by_template_id(501)
        .is_some());
    // The NPC that started the follow-up does not end it, and more deaths do not restart it.
    kill(&server, 8002, 700);
    tick(&server);
    assert_eq!(active_ids(&server), vec![12, 13]);
    end(&server, KEY).unwrap();
    assert_eq!(active_ids(&server), vec![13]);
    end(&server, EventKey { id: 13, ..KEY }).unwrap();
    assert!(active_ids(&server).is_empty());
}

/// Audit against the actual 1.2 content without MySQL or running the live server.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB and AAEMU_CLIENT_PAK"]
async fn real_tower_def_event_readiness() {
    let (Ok(path), Ok(pak)) = (
        std::env::var("AAEMU_COMPACT_DB"),
        std::env::var("AAEMU_CLIENT_PAK"),
    ) else {
        return;
    };
    let app = std::path::Path::new(&path)
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let files = crate::data::client_files::ClientFileManager::from_sources(&[pak], app);
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut data = GameData::empty();
    data.systems.instances.tower_defs = super::super::data::TowerDefData::load(&conn).unwrap();
    assert_eq!(data.systems.instances.tower_defs.progs.len(), 89);
    assert_eq!(
        data.systems.instances.tower_defs.tower_defs[&12]
            .progs
            .len(),
        13
    );
    data.systems.npcs.npc_spawners = NpcSpawnerData::load_from(&conn).unwrap();
    for row in conn
        .prepare("SELECT id FROM npcs")
        .unwrap()
        .query_map([], |r| r.get::<_, u32>(0))
        .unwrap()
    {
        let id = row.unwrap();
        data.systems.npcs.npc.templates.insert(
            id,
            NpcTemplate {
                id,
                ..Default::default()
            },
        );
    }
    data.zones = crate::data::world_data::ZoneData::load(&conn).unwrap();
    let mut server = test_server_with(data);
    let worlds = crate::data::world_templates::WorldTemplates::load(&files, app).unwrap();
    // Keep the real zone grid, replacing the test helper's synthetic world.
    let world = worlds
        .get_world_template_by_name("main_world")
        .unwrap()
        .clone();
    Arc::get_mut(&mut server.game_data).unwrap().worlds = worlds;
    server.world.remove_world(0);
    server.world.create_world_instance(Arc::new(world), 0);
    let spawns = server
        .systems
        .npcs
        .spawns
        .get_or_create(&server.world.world(0).unwrap());
    spawns
        .load_spawn_files(
            &server,
            &app.join("Data/Worlds/main_world"),
            &TemplateExists {
                npc: &|_| true,
                doodad: &|_| false,
                transfer: &|_| false,
                gimmick: &|_| false,
                slave: &|_| false,
            },
        )
        .unwrap();
    let mut ready = Vec::new();
    let mut ids: Vec<_> = server
        .game_data
        .systems
        .instances
        .tower_defs
        .tower_defs
        .keys()
        .copied()
        .collect();
    ids.sort_unstable();
    for id in ids {
        let target =
            server.game_data.systems.instances.tower_defs.tower_defs[&id].target_npc_spawn_id;
        let Some(source) = spawns.tower_def_spawners(target).into_iter().next() else {
            eprintln!("event {id}: unavailable, missing target spawner {target} positions");
            continue;
        };
        let zone_id = source.position().zone_id;
        let Some(zone) = server.game_data.zones.zone_by_key(zone_id) else {
            eprintln!("event {id}: unavailable, target position has no 1.2 zone");
            continue;
        };
        let group = zone.group_id as u16;
        let key = EventKey {
            world_id: 0,
            id,
            zone_group_id: group,
        };
        let result = validate(&server, key, zone_id);
        eprintln!("event {id}: group {group}, zone {zone_id}, {result:?}");
        if result.is_ok() {
            ready.push(id);
        }
    }
    let td = &server.game_data.systems.instances.tower_defs;
    for (spawner, groups) in &td.spawner_groups {
        eprintln!(
            "group spawner {spawner}: groups {groups:?}, {} member positions",
            spawns.tower_def_spawners(*spawner).len()
        );
    }
    let strays: Vec<_> = spawns
        .get_all_spawners()
        .values()
        .flatten()
        .filter(|s| td.group_npcs.contains(&s.unit_id))
        .map(|s| (s.unit_id, s.position().x, s.position().y))
        .collect();
    eprintln!("group members left as world spawns: {strays:?}");
    // Events whose positions the data authors, including those that spawn `NpcGroup` waves at
    // group sites; the two Golden Plains victory definitions follow the war (18).
    for id in [3, 5, 6, 8, 12, 13, 15, 18, 19, 20, 27, 28, 29, 31, 32] {
        assert!(ready.contains(&id), "event {id}: {ready:?}");
    }
    // The data has no positions for these (see PORTING.md, tower defense).
    for id in [4, 14, 16, 17, 21, 23, 24, 25, 26] {
        assert!(!ready.contains(&id), "event {id}: {ready:?}");
    }
    for id in [19, 20] {
        assert!(is_victory_followup(&server, &td.tower_defs[&id]));
    }
    assert!(!is_victory_followup(&server, &td.tower_defs[&18]));

    // The default schedule is the 1.2 open-world one: the war + victory follow-ups, the two
    // Crimson and two Grimghast rifts, the Hasla Rift and Oblivion Rift / Clockwork Rebellion.
    // Closed-zone definitions (6 Sungold Fields, 8 Reedwind, 27 Reedwind) and the definitions
    // without authored positions are not scheduled.
    let defaults = default_events(&server);
    let default_ids: Vec<u32> = defaults.iter().map(|event| event.id).collect();
    assert_eq!(
        default_ids,
        vec![3, 5, 12, 13, 15, 18, 19, 20, 28, 29, 31, 32]
    );
    for event in &defaults {
        validate(&server, EventKey::from(event), event.zone_id)
            .unwrap_or_else(|error| panic!("default event {}: {error}", event.id));
    }
}
