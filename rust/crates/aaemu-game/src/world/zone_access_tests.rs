//! Regressions for the actual movement, session and portal/skill entry paths. Timer tests
//! use Tokio's paused clock; no MySQL, client files or servers are involved.

use std::sync::Arc;
use std::time::Duration;

use crate::data::world_data::{DistrictReturnPoint, Portal, Zone};
use crate::data::world_templates::{WorldTemplate, WorldTemplates};
use crate::data::GameData;
use crate::models::movement::{MoveKind, MoveType, UnitMove};
use crate::models::npc::{Npc, PortalInfo};
use crate::models::transform::Transform;
use crate::net::packet::encode_raw;
use crate::opcodes::{cs, sc};
use crate::server::GameServer;
use crate::systems::npcs::portal::{self, PortalEffectObject, PortalTables};
use crate::systems::skills::data::{effects::SpecialEffect, EffectKind, EffectTemplate};
use crate::systems::skills::types::{skill_result, SkillCastTarget, SkillCaster, SkillObject};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{Entity, PlayerRef, WorldObject};

use super::{on_zone_change, zone_access};

const CLOSED: u32 = 200;
const GATE_SKILL: u32 = 90001;
const OPEN_SKILL: u32 = 90002;
const GATE: u32 = 10;

fn data(respawn: bool) -> GameData {
    let mut data = GameData::empty();
    for (key, closed) in [(TEST_ZONE, false), (CLOSED, true), (201, true)] {
        data.zones.zones.insert(
            key,
            Zone {
                zone_key: key,
                closed,
                ..Default::default()
            },
        );
    }
    let point = |zone_id, x| Portal {
        id: GATE,
        zone_id,
        x,
        y: 100.0,
        z: 100.0,
        ..Default::default()
    };
    data.systems.npcs.portal = PortalTables::from_parts(
        if respawn {
            vec![point(TEST_ZONE, 128.0)]
        } else {
            vec![]
        },
        vec![point(CLOSED, 1400.0)],
        vec![],
        vec![],
    )
    .unwrap();
    data.portals.recalls.insert(1, vec![point(CLOSED, 1400.0)]);
    data.systems.npcs.portal.recalls_key.insert(GATE, 1);
    data.portals
        .district_return_points
        .push(DistrictReturnPoint {
            id: 1,
            district_id: 1,
            faction_id: 101,
            return_point_id: GATE,
        });
    data.systems.npcs.npc.templates.insert(
        900,
        crate::systems::npcs::npc::data::NpcTemplate {
            id: 900,
            teleporter: true,
            ..Default::default()
        },
    );
    data.systems.skills.add_effect(
        EffectKind::SpecialEffect,
        EffectTemplate::Special(SpecialEffect {
            id: 1,
            special_effect_type_id: 25,
            value1: GATE as i32,
            value2: 0,
            value3: 0,
            value4: 0,
        }),
        1,
    );
    data.systems
        .skills
        .add_skill(crate::systems::skills::data::SkillTemplate {
            target_type: 0,
            mana_cost: 20,
            cooldown_time: 10000,
            effects: vec![crate::systems::skills::tests::skill_effect(
                1,
                EffectKind::SpecialEffect,
                1,
            )],
            ..crate::systems::skills::tests::skill_template(GATE_SKILL)
        });
    data.systems.skills.add_effect(
        EffectKind::OpenPortalEffect,
        EffectTemplate::OpenPortal {
            id: 2,
            distance: 5.0,
        },
        2,
    );
    data.systems
        .skills
        .add_skill(crate::systems::skills::data::SkillTemplate {
            target_type: 0,
            mana_cost: 20,
            cooldown_time: 10000,
            effects: vec![crate::systems::skills::tests::skill_effect(
                2,
                EffectKind::OpenPortalEffect,
                2,
            )],
            ..crate::systems::skills::tests::skill_template(OPEN_SKILL)
        });
    data
}

fn server(respawn: bool) -> Arc<GameServer> {
    let mut server = test_server_with(data(respawn));
    let mut template = WorldTemplate::synthetic(0, "main_world", 2, 2, TEST_ZONE);
    for x in 16..32 {
        for y in 0..32 {
            template.set_zone_key_at(x, y, if x < 24 { CLOSED } else { 201 });
        }
    }
    Arc::get_mut(&mut server.game_data).unwrap().worlds =
        WorldTemplates::from_templates(vec![template], vec![]).unwrap();
    server.world = Arc::new(crate::world::WorldManager::default());
    server.world.create_world_instance(
        Arc::new(
            server
                .game_data
                .worlds
                .get_world_template_by_name("main_world")
                .unwrap()
                .clone(),
        ),
        0,
    );
    let server = Arc::new(server);
    server.set_self();
    crate::systems::npcs::install_hooks(&server);
    server
}

fn player(server: &GameServer, id: u32, x: f32) -> (TestClient, PlayerRef) {
    let mut client = TestClient::new(server, id, 1000 + id);
    let mut character = test_character(id, "ZoneTester", x, 100.0);
    character.is_online = true;
    character.unit.hp = 100;
    character.unit.max_hp = 100;
    character.unit.mp = 100;
    character.unit.max_mp = 100;
    let p = spawn_player(server, &client, character, 0x100 + id);
    client.conn.active_char = Some(p.clone());
    client.drain();
    (client, p)
}

async fn walk(server: &GameServer, client: &mut TestClient, p: &PlayerRef, x: f32) {
    crate::world::session::move_unit(
        server,
        &mut client.conn,
        p.obj_id,
        MoveType::new(MoveKind::Unit(UnitMove {
            x,
            y: 100.0,
            z: 100.0,
            vel_x: 10,
            ..Default::default()
        })),
    )
    .await;
    tokio::task::yield_now().await;
}

async fn advance(ms: u64) {
    tokio::time::advance(Duration::from_millis(ms)).await;
    tokio::task::yield_now().await;
}

fn warning_count(client: &mut TestClient) -> usize {
    client
        .drain()
        .into_iter()
        .filter(|(op, body)| {
            // Native 1.2 397C3030: ErrorMessage:i16 twice, type:u32, isNotify:bool.
            *op == sc::SCErrorMsgPacket && body.as_slice() == [0x1f, 1, 0x1f, 1, 0, 0, 0, 0, 0]
        })
        .count()
}

#[tokio::test(start_paused = true)]
async fn closed_zone_walk_warns_five_times_then_returns_and_refiles_visibility() {
    let server = server(true);
    let (mut client, p) = player(&server, 1, 100.0);
    let (mut arrival, a) = player(&server, 2, 128.0);
    let (mut departure, d) = player(&server, 3, 1400.0);
    walk(&server, &mut client, &p, 1400.0).await;
    assert_eq!(p.data().unit.transform.zone_id, CLOSED);
    assert_eq!(warning_count(&mut client), 1);
    for _ in 0..4 {
        // Duplicate callbacks must not allocate another timer or extend its deadline.
        on_zone_change(&server, &p, CLOSED, CLOSED);
        advance(2000).await;
        assert_eq!(warning_count(&mut client), 1);
    }
    arrival.drain();
    departure.drain();
    advance(2000).await;
    assert_eq!(p.data().unit.transform.x, 1400.0);
    advance(200).await;
    let t = p.data().unit.transform;
    assert_eq!(
        (t.instance_id, t.zone_id, t.x, t.y),
        (0, TEST_ZONE, 128.0, 100.0)
    );
    assert!(!p.data().disabled_set_position);
    let frames = client.drain();
    assert_eq!(
        frames
            .iter()
            .filter(|f| f.0 == sc::SCCharacterResurrectedPacket)
            .count(),
        1
    );
    assert!(arrival.drain_ops().contains(&sc::SCUnitStatePacket));
    assert!(departure.drain_ops().contains(&sc::SCUnitsRemovedPacket));
    let world = server.world.world(0).unwrap();
    let around = world.players_around(WorldObject::from(&p));
    assert!(around.iter().any(|x| Arc::ptr_eq(x, &a)));
    assert!(!around.iter().any(|x| Arc::ptr_eq(x, &d)));
    // Native resurrection has no TeleportEnded acknowledgement: movement remains usable.
    walk(&server, &mut client, &p, 130.0).await;
    assert_eq!(p.data().unit.transform.x, 130.0);
    advance(20000).await;
    assert_eq!(warning_count(&mut client), 0);
}

#[tokio::test(start_paused = true)]
async fn closed_zone_departure_cancels_and_reentry_gets_a_fresh_deadline() {
    let server = server(false);
    let (mut client, p) = player(&server, 1, 100.0);
    walk(&server, &mut client, &p, 1400.0).await;
    assert_eq!(warning_count(&mut client), 1);
    advance(2000).await;
    client.drain();
    walk(&server, &mut client, &p, 200.0).await;
    advance(4000).await;
    assert_eq!(warning_count(&mut client), 0);
    walk(&server, &mut client, &p, 1400.0).await;
    assert_eq!(warning_count(&mut client), 1);
    advance(4000).await;
    assert!(!client
        .drain_ops()
        .contains(&sc::SCCharacterResurrectedPacket));
    assert_eq!(p.data().unit.transform.x, 1400.0);
    advance(6000).await;
    advance(200).await;
    // With no respawn data, the last recorded open position is a real fallback.
    let t = p.data().unit.transform;
    assert_eq!((t.zone_id, t.x), (TEST_ZONE, 200.0));
}

#[tokio::test(start_paused = true)]
async fn closed_zone_no_safe_route_does_not_teleport_or_repeat() {
    let server = server(false);
    let (mut client, p) = player(&server, 1, 1400.0);
    on_zone_change(&server, &p, 0, CLOSED);
    assert_eq!(warning_count(&mut client), 1);
    advance(10000).await;
    advance(200).await;
    assert!(!client
        .drain_ops()
        .contains(&sc::SCCharacterResurrectedPacket));
    assert_eq!(p.data().unit.transform.x, 1400.0);
    walk(&server, &mut client, &p, 1600.0).await;
    advance(20000).await;
    assert_eq!(warning_count(&mut client), 0);
    assert_eq!(p.data().unit.transform.x, 1600.0);
}

#[tokio::test(start_paused = true)]
async fn closed_zone_gm_entry_and_escalation_are_exempt() {
    let server = server(true);
    let (mut client, p) = player(&server, 1, 100.0);
    p.data().access_level = 100;
    walk(&server, &mut client, &p, 1400.0).await;
    advance(12000).await;
    assert_eq!(warning_count(&mut client), 0);
    assert_eq!(p.data().unit.transform.x, 1400.0);
    p.data().access_level = 99;
    walk(&server, &mut client, &p, 1401.0).await;
    assert_eq!(warning_count(&mut client), 1);
    p.data().access_level = 100;
    advance(12000).await;
    assert_eq!(warning_count(&mut client), 0);
    assert_eq!(p.data().unit.transform.x, 1401.0);
}

#[tokio::test(start_paused = true)]
async fn closed_zone_offline_and_stale_player_cancel_pending_transport() {
    for offline in [true, false] {
        let server = server(true);
        let (mut client, p) = player(&server, 1, 100.0);
        walk(&server, &mut client, &p, 1400.0).await;
        assert_eq!(warning_count(&mut client), 1);
        if offline {
            crate::world::session::set_online(&server, &p, false);
        } else {
            server.world.try_remove_character(p.obj_id);
            // Same obj id in a different session must not make the old timer valid.
            let replacement = crate::world::Player::new(p.data().clone(), p.conn.clone());
            server.world.try_add_character(&replacement);
        }
        advance(20000).await;
        assert!(client.drain_ops().iter().all(|op| ![
            sc::SCErrorMsgPacket,
            sc::SCCharacterResurrectedPacket
        ]
        .contains(op)));
        assert_eq!(p.data().unit.transform.x, 1400.0);
    }
}

#[tokio::test]
async fn closed_zone_portal_packet_and_open_portal_reject_before_side_effects() {
    let server = server(true);
    let (mut client, p) = player(&server, 1, 100.0);
    let destination = Transform {
        zone_id: TEST_ZONE,
        x: 1400.0,
        y: 100.0,
        z: 100.0,
        ..Default::default()
    };
    let mut npc = Npc::new(portal::PORTAL_ENTRANCE_TEMPLATE_ID);
    npc.unit.transform.set_xyz(101.0, 100.0, 100.0);
    npc.portal = Some(PortalInfo {
        teleport_position: destination,
        linked_portal_obj_id: 0,
    });
    let npc = Entity::new(0x123456, npc);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&npc, &server.view_ctx());
    client.drain();
    let before = p.data().unit.transform;
    // Native x2game12.c:1756168..1756179 = bc(objId), onlyMyPortal:bool;
    // deliberately misleading saved zone.
    server
        .handle_frame(
            &mut client.conn,
            &encode_raw(1, cs::CSUsePortalPacket, &[0x56, 0x34, 0x12, 0]),
        )
        .await
        .unwrap();
    let frames = client.drain();
    assert_eq!(
        frames,
        vec![(sc::SCErrorMsgPacket, vec![0x1f, 1, 0x1f, 1, 0, 0, 0, 0, 1])]
    );
    assert_eq!(p.data().unit.transform, before);
    p.data().portals.private_portals.push(Portal {
        id: 10,
        zone_id: CLOSED,
        x: 1400.0,
        y: 100.0,
        ..Default::default()
    });
    assert!(portal::open_portal(
        &server,
        &p,
        &PortalEffectObject {
            id: 10,
            ..Default::default()
        }
    )
    .is_none());
    assert_eq!(client.drain_ops(), vec![sc::SCErrorMsgPacket]);
    p.data().access_level = 100;
    server
        .handle_frame(
            &mut client.conn,
            &encode_raw(1, cs::CSUsePortalPacket, &[0x56, 0x34, 0x12, 0]),
        )
        .await
        .unwrap();
    assert!(client.drain_ops().contains(&sc::SCTeleportUnitPacket));
}

#[tokio::test(start_paused = true)]
async fn closed_zone_logout_and_disconnect_cancel_during_dismount_grace() {
    for disconnect in [false, true] {
        let server = server(true);
        server.db.close().await;
        let (mut client, p) = player(&server, 1, 100.0);
        walk(&server, &mut client, &p, 1400.0).await;
        client.drain();
        advance(10000).await;
        client.drain();
        if disconnect {
            crate::world::session::on_disconnect(&server, &mut client.conn).await;
        } else {
            crate::world::session::leave_world_task(
                &server,
                Some(&mut client.conn),
                super::leave_target::CHARACTER_SELECT,
                Some(p.clone()),
            )
            .await;
        }
        client.drain();
        advance(20000).await;
        assert!(!client
            .drain_ops()
            .contains(&sc::SCCharacterResurrectedPacket));
        assert!(server.world.character_by_obj_id(p.obj_id).is_none());
        assert_eq!(p.data().unit.transform.x, 1400.0);
    }
}

#[tokio::test(start_paused = true)]
async fn closed_zone_instance_return_moves_the_world_registration_and_visibility() {
    let server = server(true);
    let template = WorldTemplate::synthetic(2, "closed_instance", 2, 2, CLOSED);
    let instance = server.world.create_world_instance(Arc::new(template), 77);
    let (mut arrival, _) = player(&server, 2, 128.0);
    let mut client = TestClient::new(&server, 1, 1001);
    let mut character = test_character(1, "InstanceTester", 100.0, 100.0);
    character.obj_id = 0x101;
    character.is_online = true;
    character.unit.transform.instance_id = 77;
    character.unit.transform.world_id = 2;
    character.unit.transform.zone_id = CLOSED;
    let p = crate::world::Player::new(character, client.conn.handle());
    server.world.try_add_character(&p);
    instance.spawn(&p, &server.view_ctx());
    client.conn.active_char = Some(p.clone());
    on_zone_change(&server, &p, 0, CLOSED);
    client.drain();
    arrival.drain();
    advance(10000).await;
    advance(200).await;
    let t = p.data().unit.transform;
    assert_eq!((t.instance_id, t.world_id, t.zone_id), (0, 0, TEST_ZONE));
    assert!(instance.character_by_obj_id(p.obj_id).is_none());
    assert!(server
        .world
        .world(0)
        .unwrap()
        .character_by_obj_id(p.obj_id)
        .is_some_and(|live| Arc::ptr_eq(&p, &live)));
    assert_eq!(p.region().map(|r| r.0), Some(0));
    assert!(client.drain_ops().contains(&sc::SCLoadInstancePacket));
    assert!(arrival.drain_ops().contains(&sc::SCUnitStatePacket));
    crate::systems::instances::worlds::instance_loaded(&server, &p);
    assert!(!p.data().disabled_set_position);
    advance(20000).await;
    assert_eq!(warning_count(&mut client), 0);
}

#[tokio::test]
async fn closed_zone_npc_teleport_does_not_charge_the_fare() {
    struct Wallet;
    impl crate::systems::npcs::hooks::EconomyBridge for Wallet {
        fn change_money(
            &self,
            _server: &GameServer,
            p: &PlayerRef,
            amount: i64,
            _task: u8,
        ) -> bool {
            p.data().money += amount;
            true
        }
    }
    let server = server(true);
    server.systems.npcs.hooks.economy.install(Arc::new(Wallet));
    let (mut client, p) = player(&server, 1, 100.0);
    p.data().money = 500;
    let mut npc = Npc::new(900);
    npc.unit.transform.set_xyz(101.0, 100.0, 100.0);
    let npc = Entity::new(0x123456, npc);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&npc, &server.view_ctx());
    client.drain();
    // Native 1.2 397CB0E0 (x2game12.c:1766873): return point:u32, money:i32, npc:bc.
    let request = [10, 0, 0, 0, 120, 0, 0, 0, 0x56, 0x34, 0x12];
    server
        .handle_frame(
            &mut client.conn,
            &encode_raw(1, cs::CSUseTeleportPacket, &request),
        )
        .await
        .unwrap();
    assert_eq!(p.data().money, 500);
    assert_eq!(client.drain_ops(), vec![sc::SCErrorMsgPacket]);
    assert!(!p.data().disabled_set_position);
    p.data().access_level = 100;
    server
        .handle_frame(
            &mut client.conn,
            &encode_raw(1, cs::CSUseTeleportPacket, &request),
        )
        .await
        .unwrap();
    assert_eq!(p.data().money, 380);
    assert_eq!(client.drain_ops(), vec![sc::SCTeleportUnitPacket]);
}

#[tokio::test]
async fn closed_zone_worldgate_skill_is_rejected_before_mana_and_cooldown() {
    let server = server(true);
    let (mut client, p) = player(&server, 1, 100.0);
    let caster = WorldObject::from(&p);
    let skill = crate::systems::skills::skill::new_skill_for(
        &server,
        server
            .game_data
            .systems
            .skills
            .skill(GATE_SKILL)
            .unwrap()
            .clone(),
        Some(&caster),
    );
    let before = p.data().unit.transform;
    let result = crate::systems::skills::skill::use_skill(
        &server,
        &skill,
        &caster,
        SkillCaster::Unit(p.obj_id),
        &mut SkillCastTarget::Unit(p.obj_id),
        None,
        false,
    );
    assert_eq!(result, (skill_result::NO_PERM, 0));
    assert_eq!(p.data().unit.mp, 100);
    assert!(!p
        .data()
        .unit
        .cooldowns
        .check_cooldown(GATE_SKILL, crate::world::now_ms()));
    assert_eq!(p.data().unit.transform, before);
    assert!(client.drain().is_empty());
}

#[tokio::test]
async fn closed_zone_open_portal_skill_rejects_the_saved_book_destination_before_charging() {
    let server = server(true);
    let (mut client, p) = player(&server, 1, 100.0);
    p.data().portals.private_portals.push(Portal {
        id: GATE,
        zone_id: CLOSED,
        x: 1400.0,
        y: 100.0,
        ..Default::default()
    });
    let caster = WorldObject::from(&p);
    let skill = crate::systems::skills::skill::new_skill_for(
        &server,
        server
            .game_data
            .systems
            .skills
            .skill(OPEN_SKILL)
            .unwrap()
            .clone(),
        Some(&caster),
    );
    let result = crate::systems::skills::skill::use_skill(
        &server,
        &skill,
        &caster,
        SkillCaster::Unit(p.obj_id),
        &mut SkillCastTarget::Unit(p.obj_id),
        Some(SkillObject::PortalInfo {
            kind: 0,
            id: GATE as i32,
            x: 101.0,
            y: 100.0,
            z: 100.0,
        }),
        false,
    );
    assert_eq!(result, (skill_result::NO_PERM, 0));
    assert_eq!(p.data().unit.mp, 100);
    assert!(!p
        .data()
        .unit
        .cooldowns
        .check_cooldown(OPEN_SKILL, crate::world::now_ms()));
    assert!(client.drain().is_empty());
}

#[tokio::test]
async fn closed_zone_saved_return_and_unknown_mapping_are_checked_without_fabrication() {
    use crate::systems::skills::effects::{EffectArgs, EffectSource};
    use crate::systems::skills::types::CastAction;
    let server = server(true);
    let (mut client, p) = player(&server, 1, 100.0);
    p.data().return_district_id = 1;
    let before = p.data().unit.transform;
    let caster = WorldObject::from(&p);
    let args = EffectArgs {
        caster: Some(caster.clone()),
        caster_caster: SkillCaster::Unit(p.obj_id),
        target: crate::systems::skills::geometry::Target::Obj(caster),
        target_caster: SkillCastTarget::Unit(p.obj_id),
        cast: CastAction::Skill {
            skill_id: GATE_SKILL,
            tl_id: 1,
        },
        source: EffectSource::default(),
        skill_object: &SkillObject::None,
    };
    crate::systems::skills::effects::special::execute(&server, 25, &args, [0; 4]);
    assert_eq!(p.data().unit.transform, before);
    assert_eq!(client.drain_ops(), vec![sc::SCErrorMsgPacket]);
    p.data().main_world_position = Some(Transform {
        zone_id: CLOSED,
        x: 1400.0,
        ..before
    });
    crate::systems::skills::effects::special::execute(&server, 25, &args, [9999, 0, 0, 0]);
    assert_eq!(p.data().unit.transform, before);
    assert_eq!(client.drain_ops(), vec![sc::SCErrorMsgPacket]);
    // Unknown zone keys alone do not make an otherwise open coordinate closed.
    assert!(zone_access::zone_access_allowed(
        &server, &p, 0, 202, 100.0, 100.0
    ));
    assert!(!zone_access::zone_access_allowed(
        &server, &p, 0, 202, 1400.0, 100.0
    ));
}
