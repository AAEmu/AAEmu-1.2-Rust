use super::*;

#[tokio::test]
async fn npc_faces_its_attack_target_before_angle_validation_and_keeps_self_skill_heading() {
    use super::super::framework::{AiKind, NpcAi};
    use crate::data::GameData;
    use crate::models::npc::Npc;
    use crate::systems::skills::data::SkillTemplate;
    use crate::systems::skills::state::SkillRun;
    use crate::test_support::test_server_with;
    use crate::world::Entity;

    for target_type in [5, 0] {
        let server = Arc::new(test_server_with(GameData::empty()));
        server.set_self();
        crate::systems::skills::bind(&server);
        let mut caster = Npc::new(1);
        caster.unit.hp = 1000;
        caster.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let caster = Entity::new(0x300, caster);
        let mut target = Npc::new(2);
        target.unit.hp = 1000;
        target.unit.transform.set_xyz(105.0, 100.0, 100.0);
        let target = WorldObject::from(Entity::new(0x301, target));
        let world = server.world.world(0).unwrap();
        world.spawn(&caster, &server.view_ctx());
        world.spawn(&target, &server.view_ctx());
        let mut ai = NpcAi::new(AiKind::AlmightyNpc, &caster);
        let mut context = AiCtx {
            server: &server,
            npc: caster.clone(),
            obj: WorldObject::from(&caster),
            ai: &mut ai,
        };
        let skill = SkillRun::new(
            Arc::new(SkillTemplate {
                id: 100,
                target_type,
                target_angle: 90,
                max_range: 30,
                ..Default::default()
            }),
            1,
        );
        assert_eq!(
            use_skill(
                &mut context,
                BehaviorKind::AlmightyAttack,
                skill,
                Some(&target),
                0.0
            ),
            skill_result::SUCCESS
        );
        let yaw = caster.data().unit.transform.rot_z;
        if target_type == 5 {
            assert!((yaw + std::f32::consts::FRAC_PI_2).abs() < 0.001);
        } else {
            assert_eq!(yaw, 0.0);
        }
    }
}

#[test]
fn big_monster_chase_range_follows_only_a_ranged_base_attack() {
    use crate::systems::skills::data::SkillTemplate;
    let skill = |id, max_range, weapon_slot_for_range_id| SkillTemplate {
        id,
        max_range,
        weapon_slot_for_range_id,
        ..Default::default()
    };
    // Blangar's bow and the Maddened Shaman's Flame Cluster.
    assert!(ranged_base_attack(&skill(10431, 15, -1)));
    assert!(ranged_base_attack(&skill(19981, 20, -1)));
    // Melee skill 2 (25 m in the data) takes its range from the weapon.
    assert!(!ranged_base_attack(&skill(2, 25, 15)));
    assert!(!ranged_base_attack(&skill(100, 20, 1)));
    assert!(!ranged_base_attack(&skill(101, 4, -1)));
}

#[test]
fn skill_list_health_and_time_windows() {
    // no ranges: always
    assert!(health_and_time_ok(50, 0.0, 0, 0, 0.0, 0.0));
    // health window (min exclusive, max inclusive)
    assert!(!health_and_time_ok(40, 0.0, 40, 90, 0.0, 0.0));
    assert!(health_and_time_ok(90, 0.0, 40, 90, 0.0, 0.0));
    // time window [5, 0] = from 5 s on
    assert!(!health_and_time_ok(50, 4.0, 0, 100, 5.0, 0.0));
    assert!(health_and_time_ok(50, 6.0, 0, 100, 5.0, 0.0));
    // time window [5, 140]
    assert!(health_and_time_ok(50, 140.0, 0, 100, 5.0, 140.0));
    assert!(!health_and_time_ok(50, 141.0, 0, 100, 5.0, 140.0));
    // negative start: never (C# falls through every branch)
    assert!(!health_and_time_ok(50, 1.0, 0, 0, -1.0, 0.0));
}

#[test]
fn delta_uses_the_millisecond_component() {
    assert_eq!(delta_seconds(120), 0.12);
    assert_eq!(delta_seconds(1120), 0.12);
}

/// `update_aggro_help` walks the linked NPCs in region order. An AI-less NPC used to end the
/// loop (the C# NullReferenceException), so the NPCs behind it never helped and the error
/// repeated on every tick. It must be skipped with one report, and the next NPC must help.
#[tokio::test]
async fn aggro_help_skips_an_ai_less_npc_and_still_helps_the_next() {
    use super::super::framework::{AiKind, NpcAi};
    use crate::data::GameData;
    use crate::models::npc::Npc;
    use crate::systems::npcs::npc::data::NpcTemplate;
    use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
    use crate::world::Entity;

    const CASTER: u32 = 0x2E1;
    const NO_AI: u32 = 0x2E2;
    const HELPER: u32 = 0x2E3;
    const ABUSER: u32 = 0x2E4;

    let mut data = GameData::empty();
    for id in [7000u32, 7001, 7002] {
        data.systems.npcs.npc.templates.insert(
            id,
            NpcTemplate {
                id,
                aggression: true,
                attack_start_range_scale: 1.0,
                accept_aggro_link: true,
                aggro_link_help_dist: 30.0,
                ..NpcTemplate::default()
            },
        );
    }
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Tank", 98.0, 100.0),
        ABUSER,
    );
    let world = server.world.world(0).unwrap();

    let mut caster = Npc::new(7000);
    caster.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let caster = Entity::new(CASTER, caster);
    world.spawn(&caster, &server.view_ctx());
    // First in the region list on purpose.
    let mut ai_less = Npc::new(7002);
    ai_less.unit.transform.set_xyz(101.0, 100.0, 100.0);
    let ai_less = Entity::new(NO_AI, ai_less);
    world.spawn(&ai_less, &server.view_ctx());
    assert!(!super::super::manager::has_ai(&server, NO_AI));
    let mut helper = Npc::new(7001);
    helper.unit.transform.set_xyz(102.0, 100.0, 100.0);
    let helper = Entity::new(HELPER, helper);
    server.systems.ai.ais.add_ai(&helper, AiKind::HoldPosition);
    world.spawn(&helper, &server.view_ctx());

    let abuser = WorldObject::from(&player);
    let mut ai = NpcAi::new(AiKind::AlmightyNpc, &caster);
    let mut context = AiCtx {
        server: &server,
        npc: caster.clone(),
        obj: WorldObject::from(&caster),
        ai: &mut ai,
    };
    let lines = warnings(|| {
        update_aggro_help(&mut context, &abuser, 200.0);
        // A second tick: the AI-less NPC is reported once, and the helper keeps its aggro.
        update_aggro_help(&mut context, &abuser, 200.0);
    });

    assert!(
        helper
            .data()
            .unit
            .skill_state
            .aggro_table
            .contains_key(&player.obj_id),
        "the NPC after the AI-less one must still receive aggro help"
    );
    assert!(
        !caster
            .data()
            .unit
            .skill_state
            .aggro_table
            .contains_key(&player.obj_id),
        "the caster does not add itself"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("has no AI") && l.contains(&NO_AI.to_string()))
            .count(),
        1,
        "the AI-less NPC must be reported once, got {lines:?}"
    );
}

/// An NPC template that refuses template-driven aggro help (the `update_aggro_help` rules).
fn link_only_template(
    id: u32,
    accept_aggro_link: bool,
    aggression: bool,
    aggro_link_help_dist: f32,
) -> crate::systems::npcs::npc::data::NpcTemplate {
    crate::systems::npcs::npc::data::NpcTemplate {
        id,
        aggression,
        attack_start_range_scale: 1.0,
        accept_aggro_link,
        aggro_link_help_dist,
        ..Default::default()
    }
}

/// Spawns an NPC with a `HoldPosition` AI at `(x, 100, 100)` so it can receive aggro help.
fn spawn_link_npc(
    server: &Arc<crate::server::GameServer>,
    obj_id: u32,
    template_id: u32,
    x: f32,
) -> crate::world::NpcRef {
    use super::super::framework::AiKind;
    use crate::models::npc::Npc;
    use crate::world::Entity;

    let mut npc = Npc::new(template_id);
    npc.unit.transform.set_xyz(x, 100.0, 100.0);
    let npc = Entity::new(obj_id, npc);
    server.systems.ai.ais.add_ai(&npc, AiKind::HoldPosition);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&npc, &server.view_ctx());
    npc
}

/// `aggro_links` group membership makes a member help another member of the same group even
/// when its own template rules (`accept_aggro_link` false, non-aggressive with rule 0)
/// would not; a non-member with the same template rules keeps the old behaviour.
#[tokio::test]
async fn aggro_link_members_help_each_other_despite_template_rules() {
    use super::super::framework::{AiKind, NpcAi};
    use crate::data::GameData;
    use crate::models::npc::Npc;
    use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
    use crate::world::Entity;

    const LEADER: u32 = 0x320;
    const NO_ACCEPT: u32 = 0x321;
    const NO_AGGRESSION: u32 = 0x322;
    const NON_MEMBER: u32 = 0x323;
    const ABUSER: u32 = 0x324;

    let mut data = GameData::empty();
    data.systems
        .npcs
        .npc
        .templates
        .insert(7000, link_only_template(7000, true, true, 30.0));
    data.systems
        .npcs
        .npc
        .templates
        .insert(7001, link_only_template(7001, false, true, 30.0));
    data.systems
        .npcs
        .npc
        .templates
        .insert(7002, link_only_template(7002, true, false, 30.0));
    data.systems
        .npcs
        .npc
        .templates
        .insert(7003, link_only_template(7003, false, true, 30.0));
    data.systems
        .npcs
        .npc
        .aggro_links
        .groups
        .insert(500, "herd".into());
    for id in [7000u32, 7001, 7002] {
        data.systems
            .npcs
            .npc
            .aggro_links
            .groups_by_npc
            .insert(id, vec![500]);
    }
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Tank", 98.0, 100.0),
        ABUSER,
    );

    // The leader is the NPC that just got the abuser as its target.
    let mut leader = Npc::new(7000);
    leader.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let leader = Entity::new(LEADER, leader);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&leader, &server.view_ctx());
    let no_accept = spawn_link_npc(&server, NO_ACCEPT, 7001, 105.0);
    let no_aggression = spawn_link_npc(&server, NO_AGGRESSION, 7002, 110.0);
    let non_member = spawn_link_npc(&server, NON_MEMBER, 7003, 115.0);

    let abuser = WorldObject::from(&player);
    let mut ai = NpcAi::new(AiKind::AlmightyNpc, &leader);
    let mut context = AiCtx {
        server: &server,
        npc: leader.clone(),
        obj: WorldObject::from(&leader),
        ai: &mut ai,
    };
    update_aggro_help(&mut context, &abuser, 200.0);

    let has_aggro = |npc: &crate::world::NpcRef| {
        npc.data()
            .unit
            .skill_state
            .aggro_table
            .contains_key(&player.obj_id)
    };
    assert!(
        has_aggro(&no_accept),
        "a linked member must help even with accept_aggro_link=false"
    );
    assert!(
        has_aggro(&no_aggression),
        "a linked member must help even when its template rule 0 needs aggression"
    );
    assert!(
        !has_aggro(&non_member),
        "a non-member with accept_aggro_link=false must keep refusing"
    );
    assert!(
        !has_aggro(&leader),
        "the linked NPC must not add aggro to itself"
    );
}

/// The link only bypasses the template rules; the helper's own `aggro_link_help_dist` still
/// bounds the distance.
#[tokio::test]
async fn aggro_link_help_respects_the_helpers_distance() {
    use super::super::framework::{AiKind, NpcAi};
    use crate::data::GameData;
    use crate::models::npc::Npc;
    use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
    use crate::world::Entity;

    const LEADER: u32 = 0x330;
    const NEAR: u32 = 0x331;
    const FAR: u32 = 0x332;
    const ABUSER: u32 = 0x333;

    let mut data = GameData::empty();
    for id in [7100u32, 7101, 7102] {
        data.systems
            .npcs
            .npc
            .templates
            .insert(id, link_only_template(id, false, false, 30.0));
    }
    data.systems
        .npcs
        .npc
        .aggro_links
        .groups
        .insert(501, "guard".into());
    for id in [7100u32, 7101, 7102] {
        data.systems
            .npcs
            .npc
            .aggro_links
            .groups_by_npc
            .insert(id, vec![501]);
    }
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Tank", 98.0, 100.0),
        ABUSER,
    );

    let mut leader = Npc::new(7100);
    leader.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let leader = Entity::new(LEADER, leader);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&leader, &server.view_ctx());
    let near = spawn_link_npc(&server, NEAR, 7101, 120.0);
    let far = spawn_link_npc(&server, FAR, 7102, 160.0);

    let abuser = WorldObject::from(&player);
    let mut ai = NpcAi::new(AiKind::AlmightyNpc, &leader);
    let mut context = AiCtx {
        server: &server,
        npc: leader.clone(),
        obj: WorldObject::from(&leader),
        ai: &mut ai,
    };
    update_aggro_help(&mut context, &abuser, 200.0);

    let has_aggro = |npc: &crate::world::NpcRef| {
        npc.data()
            .unit
            .skill_state
            .aggro_table
            .contains_key(&player.obj_id)
    };
    assert!(
        has_aggro(&near),
        "a member within the helper distance helps"
    );
    assert!(
        !has_aggro(&far),
        "a member beyond aggro_link_help_dist must not help"
    );
}

/// An NPC in two `aggro_links` groups helps a member of either group.
#[tokio::test]
async fn aggro_link_member_of_two_groups_helps_members_of_both() {
    use super::super::framework::{AiKind, NpcAi};
    use crate::data::GameData;
    use crate::models::npc::Npc;
    use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
    use crate::world::Entity;

    const LEADER_A: u32 = 0x340;
    const LEADER_B: u32 = 0x341;
    const HELPER: u32 = 0x342;
    const ABUSER_A: u32 = 0x343;
    const ABUSER_B: u32 = 0x344;

    let mut data = GameData::empty();
    for id in [7200u32, 7201, 7202] {
        data.systems
            .npcs
            .npc
            .templates
            .insert(id, link_only_template(id, false, false, 50.0));
    }
    data.systems
        .npcs
        .npc
        .aggro_links
        .groups
        .insert(500, "herd".into());
    data.systems
        .npcs
        .npc
        .aggro_links
        .groups
        .insert(501, "guard".into());
    data.systems
        .npcs
        .npc
        .aggro_links
        .groups_by_npc
        .insert(7200, vec![500]);
    data.systems
        .npcs
        .npc
        .aggro_links
        .groups_by_npc
        .insert(7201, vec![501]);
    data.systems
        .npcs
        .npc
        .aggro_links
        .groups_by_npc
        .insert(7202, vec![500, 501]);

    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    let client_a = TestClient::new(&server, 1, 1001);
    let client_b = TestClient::new(&server, 2, 1002);
    let player_a = spawn_player(
        &server,
        &client_a,
        test_character(1, "TankA", 98.0, 100.0),
        ABUSER_A,
    );
    let player_b = spawn_player(
        &server,
        &client_b,
        test_character(2, "TankB", 98.0, 100.0),
        ABUSER_B,
    );

    let mut leader_a = Npc::new(7200);
    leader_a.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let leader_a = Entity::new(LEADER_A, leader_a);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&leader_a, &server.view_ctx());
    let mut leader_b = Npc::new(7201);
    leader_b.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let leader_b = Entity::new(LEADER_B, leader_b);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&leader_b, &server.view_ctx());
    let helper = spawn_link_npc(&server, HELPER, 7202, 120.0);

    let mut ai_a = NpcAi::new(AiKind::AlmightyNpc, &leader_a);
    let mut context_a = AiCtx {
        server: &server,
        npc: leader_a.clone(),
        obj: WorldObject::from(&leader_a),
        ai: &mut ai_a,
    };
    update_aggro_help(&mut context_a, &WorldObject::from(&player_a), 200.0);
    let mut ai_b = NpcAi::new(AiKind::AlmightyNpc, &leader_b);
    let mut context_b = AiCtx {
        server: &server,
        npc: leader_b.clone(),
        obj: WorldObject::from(&leader_b),
        ai: &mut ai_b,
    };
    update_aggro_help(&mut context_b, &WorldObject::from(&player_b), 200.0);

    let table = helper.data().unit.skill_state.aggro_table.clone();
    assert!(
        table.contains_key(&player_a.obj_id),
        "the helper must help its group 500 member"
    );
    assert!(
        table.contains_key(&player_b.obj_id),
        "the helper must help its group 501 member"
    );
}

/// Rule 0 (aggressive, no special rule) help needs a reason to fight: the helper shares the
/// attacked NPC's faction or can attack the abuser itself. A system-neutral (faction 2)
/// Gossiper Banshee next to a hostile faction-115 mob must not take the mob's attacker into
/// its aggro table (it cannot attack a player, so it stayed in combat); a faction-115 helper
/// still joins, and the banshee still helps a neutral NPC of its own faction.
#[tokio::test]
async fn rule_zero_help_needs_the_same_faction_or_a_hostile_abuser() {
    use super::super::framework::{AiKind, NpcAi};
    use crate::data::world_data::{FactionRelation, SystemFaction};
    use crate::data::GameData;
    use crate::models::npc::Npc;
    use crate::systems::skills::types::RelationState;
    use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
    use crate::world::Entity;

    const LEADER: u32 = 0x360;
    const BANSHEE: u32 = 0x361;
    const KOBOLD: u32 = 0x362;
    const NEUTRAL_LEADER: u32 = 0x363;
    const ABUSER: u32 = 0x364;

    let mut data = GameData::empty();
    for (id, mother_id) in [(2, 0), (109, 149), (115, 0), (149, 0)] {
        data.factions.add(SystemFaction {
            id,
            mother_id,
            ..SystemFaction::default()
        });
    }
    data.factions.relations.push(FactionRelation {
        id: 149,
        id2: 115,
        state: RelationState::Hostile as u8,
        exp_time: 0,
    });
    for id in [7400, 7401, 7402] {
        data.systems
            .npcs
            .npc
            .templates
            .insert(id, link_only_template(id, true, true, 30.0));
    }
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Tank", 98.0, 100.0),
        ABUSER,
    );
    player.data().unit.faction_id = 109;

    let spawn_leader = |obj_id: u32, faction: u32| {
        let mut leader = Npc::new(7400);
        leader.unit.transform.set_xyz(100.0, 100.0, 100.0);
        leader.unit.faction_id = faction;
        let leader = Entity::new(obj_id, leader);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&leader, &server.view_ctx());
        leader
    };
    let leader = spawn_leader(LEADER, 115);
    let banshee = spawn_link_npc(&server, BANSHEE, 7401, 105.0);
    banshee.data().unit.faction_id = 2;
    let kobold = spawn_link_npc(&server, KOBOLD, 7402, 110.0);
    kobold.data().unit.faction_id = 115;

    let abuser = WorldObject::from(&player);
    let help = |leader: &crate::world::NpcRef| {
        let mut ai = NpcAi::new(AiKind::AlmightyNpc, leader);
        let mut context = AiCtx {
            server: &server,
            npc: leader.clone(),
            obj: WorldObject::from(leader),
            ai: &mut ai,
        };
        update_aggro_help(&mut context, &abuser, 200.0);
    };
    let has_aggro = |npc: &crate::world::NpcRef| {
        npc.data()
            .unit
            .skill_state
            .aggro_table
            .contains_key(&player.obj_id)
    };

    help(&leader);
    assert!(
        !has_aggro(&banshee),
        "a neutral helper of another faction must not join a fight it cannot start"
    );
    assert!(has_aggro(&kobold), "a same-faction helper still joins");

    let neutral_leader = spawn_leader(NEUTRAL_LEADER, 2);
    help(&neutral_leader);
    assert!(
        has_aggro(&banshee),
        "a neutral helper still helps a neutral NPC of its own faction"
    );
}

/// The link-group path must not relax the template sight gate for non-members. The C#
/// `UpdateAggroHelp` special-rule branch needs `aggro_link_sight_check` enabled (Behavior.cs,
/// `a2ab80abd^`), so a rule-4 non-member with the flag off does not help; the same template
/// with the flag on still does (control). Only the link path may ignore the flag.
#[tokio::test]
async fn aggro_link_does_not_relax_the_template_sight_gate_for_non_members() {
    use super::super::framework::{AiKind, NpcAi};
    use crate::data::GameData;
    use crate::models::npc::Npc;
    use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
    use crate::world::Entity;

    const LEADER: u32 = 0x350;
    const NO_SIGHT: u32 = 0x351;
    const WITH_SIGHT: u32 = 0x352;
    const ABUSER: u32 = 0x353;

    let mut data = GameData::empty();
    data.systems
        .npcs
        .npc
        .templates
        .insert(7300, link_only_template(7300, true, true, 30.0));
    data.systems.npcs.npc.templates.insert(
        7301,
        crate::systems::npcs::npc::data::NpcTemplate {
            aggro_link_special_rule_id: 4,
            aggro_link_sight_check: false,
            ..link_only_template(7301, true, true, 30.0)
        },
    );
    data.systems.npcs.npc.templates.insert(
        7302,
        crate::systems::npcs::npc::data::NpcTemplate {
            aggro_link_special_rule_id: 4,
            aggro_link_sight_check: true,
            ..link_only_template(7302, true, true, 30.0)
        },
    );
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Tank", 98.0, 100.0),
        ABUSER,
    );

    let mut leader = Npc::new(7300);
    leader.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let leader = Entity::new(LEADER, leader);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&leader, &server.view_ctx());
    let no_sight = spawn_link_npc(&server, NO_SIGHT, 7301, 105.0);
    let with_sight = spawn_link_npc(&server, WITH_SIGHT, 7302, 110.0);

    let abuser = WorldObject::from(&player);
    let mut ai = NpcAi::new(AiKind::AlmightyNpc, &leader);
    let mut context = AiCtx {
        server: &server,
        npc: leader.clone(),
        obj: WorldObject::from(&leader),
        ai: &mut ai,
    };
    update_aggro_help(&mut context, &abuser, 200.0);

    let has_aggro = |npc: &crate::world::NpcRef| {
        npc.data()
            .unit
            .skill_state
            .aggro_table
            .contains_key(&player.obj_id)
    };
    assert!(
        !has_aggro(&no_sight),
        "a non-member with aggro_link_sight_check off must keep refusing special-rule help"
    );
    assert!(
        has_aggro(&with_sight),
        "a non-member with the sight check on still helps through rule 4"
    );
}

// ---- `aggro_link_special_guard` city-guard help -------------------------------------------

const GUARD_FACTION: u32 = 162;
const NATION_FACTION: u32 = 101;
const PIRATE_FACTION: u32 = 161;
const MONSTER_FACTION: u32 = 115;
const GUARD_TEMPLATE: u32 = 7400;
const PLAIN_TEMPLATE: u32 = 7401;
const MONSTER_TEMPLATE: u32 = 7402;

/// Guard fixture: `nation_guard_help` is the `system_factions.guard_help` switch of the
/// victim's faction; the guard faction is hostile to the pirate and monster factions (as in
/// the shipped `system_faction_relations`, 162 vs 161 / 115 = hostile). The guard template
/// mirrors the city guards (faction 162, rule 4 everyone, sight check, 6 m help distance,
/// `ignore_npc_attacker`).
fn guard_data(nation_guard_help: bool) -> crate::data::GameData {
    use crate::data::world_data::{FactionRelation, SystemFaction};
    let mut data = crate::data::GameData::empty();
    use crate::systems::social::duel::{FACTION_BLUE_TEAM, FACTION_RED_TEAM};
    for (id, guard_help) in [
        (NATION_FACTION, nation_guard_help),
        (GUARD_FACTION, true),
        (PIRATE_FACTION, true),
        (MONSTER_FACTION, false),
        // The temporary team factions of an accepted duel (the shipped rows have guard_help
        // on for both).
        (FACTION_RED_TEAM, true),
        (FACTION_BLUE_TEAM, true),
    ] {
        data.factions.add(SystemFaction {
            id,
            guard_help,
            ..SystemFaction::default()
        });
    }
    for (a, b) in [
        (GUARD_FACTION, PIRATE_FACTION),
        (GUARD_FACTION, MONSTER_FACTION),
        (PIRATE_FACTION, NATION_FACTION),
        (MONSTER_FACTION, NATION_FACTION),
        // The guard is hostile to both duel teams here (the shipped data has no row for them),
        // so only the duel check can keep it out of a duel, not the faction relation.
        (GUARD_FACTION, FACTION_RED_TEAM),
        (GUARD_FACTION, FACTION_BLUE_TEAM),
        (FACTION_RED_TEAM, FACTION_BLUE_TEAM),
    ] {
        data.factions.relations.push(FactionRelation {
            id: a,
            id2: b,
            state: crate::systems::skills::types::RelationState::Hostile as u8,
            exp_time: 0,
        });
    }
    data.systems.npcs.npc.templates.insert(
        GUARD_TEMPLATE,
        crate::systems::npcs::npc::data::NpcTemplate {
            id: GUARD_TEMPLATE,
            faction_id: GUARD_FACTION,
            aggression: true,
            accept_aggro_link: true,
            aggro_link_special_guard: true,
            aggro_link_special_ignore_npc_attacker: true,
            aggro_link_special_rule_id: 4,
            aggro_link_sight_check: true,
            aggro_link_help_dist: 6.0,
            attack_start_range_scale: 1.0,
            ..Default::default()
        },
    );
    // The same shape without `aggro_link_special_guard` (the control NPC).
    data.systems.npcs.npc.templates.insert(
        PLAIN_TEMPLATE,
        crate::systems::npcs::npc::data::NpcTemplate {
            id: PLAIN_TEMPLATE,
            faction_id: GUARD_FACTION,
            aggression: true,
            accept_aggro_link: true,
            aggro_link_special_rule_id: 4,
            aggro_link_sight_check: true,
            aggro_link_help_dist: 6.0,
            attack_start_range_scale: 1.0,
            ..Default::default()
        },
    );
    // The shipped data has special guards without the 0x40 flag (45 of 107), so an NPC
    // attacker can pull some of them; keep the cheap data-level skip out of these tests.
    data.systems.npcs.npc.special_guards_allow_npc_attacker = true;
    data.systems.npcs.npc.templates.insert(
        MONSTER_TEMPLATE,
        crate::systems::npcs::npc::data::NpcTemplate {
            id: MONSTER_TEMPLATE,
            faction_id: MONSTER_FACTION,
            aggression: true,
            attack_start_range_scale: 1.0,
            ..Default::default()
        },
    );
    data
}

/// Applies one fixed 10-damage `DamageEffect` from `caster` to `target` (the real damage
/// pipeline, which is what triggers the guard help).
fn apply_hit(server: &GameServer, caster: &WorldObject, target: &WorldObject) {
    use crate::systems::skills::effects::{EffectArgs, EffectSource};
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::types::{CastAction, SkillCastTarget, SkillCaster, SkillObject};
    let args = EffectArgs {
        caster: Some(caster.clone()),
        caster_caster: SkillCaster::Unit(caster.obj_id()),
        target: Target::Obj(target.clone()),
        target_caster: SkillCastTarget::Unit(target.obj_id()),
        cast: CastAction::Skill {
            skill_id: 1,
            tl_id: 0,
        },
        source: EffectSource::default(),
        skill_object: &SkillObject::None,
    };
    let effect = crate::systems::skills::tests::damage_effect(1, 10);
    crate::systems::skills::effects::combat::damage(server, &effect, &args, None);
}

fn guard_fixture(
    nation_guard_help: bool,
) -> (
    Arc<GameServer>,
    crate::world::PlayerRef,
    crate::world::PlayerRef,
    crate::world::NpcRef,
) {
    use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};

    let server = Arc::new(test_server_with(guard_data(nation_guard_help)));
    server.set_self();
    crate::systems::skills::bind(&server);
    let victim_client = TestClient::new(&server, 1, 1001);
    let attacker_client = TestClient::new(&server, 2, 1002);
    let mut victim_char = test_character(1, "Victim", 100.0, 100.0);
    victim_char.unit.hp = 1000;
    victim_char.unit.max_hp = 1000;
    victim_char.unit.faction_id = NATION_FACTION;
    let victim = spawn_player(&server, &victim_client, victim_char, 0x401);
    let mut attacker_char = test_character(2, "Attacker", 102.0, 100.0);
    attacker_char.unit.hp = 1000;
    attacker_char.unit.max_hp = 1000;
    attacker_char.unit.faction_id = PIRATE_FACTION;
    let attacker = spawn_player(&server, &attacker_client, attacker_char, 0x402);
    let guard = spawn_link_npc(&server, 0x403, GUARD_TEMPLATE, 103.0);
    guard.data().unit.faction_id = GUARD_FACTION;
    // A dead guard is skipped by the scan, so the fixture guards need HP.
    guard.data().unit.hp = 1000;
    guard.data().unit.max_hp = 1000;
    (server, victim, attacker, guard)
}

fn has_aggro_on(npc: &crate::world::NpcRef, obj_id: u32) -> bool {
    npc.data()
        .unit
        .skill_state
        .aggro_table
        .contains_key(&obj_id)
}

/// A piracy attack on a protected-faction player near a city guard pulls the guard onto the
/// attacker (10.x `sub_39321C80`, Z:823528-823950). Driven through the damage effect.
#[tokio::test]
async fn a_special_guard_helps_a_player_attacked_by_a_pirate() {
    let (server, victim, attacker, guard) = guard_fixture(true);
    apply_hit(
        &server,
        &WorldObject::from(&attacker),
        &WorldObject::from(&victim),
    );
    assert!(
        has_aggro_on(&guard, attacker.obj_id),
        "the guard must join the fight against the pirate attacker"
    );
    assert!(
        !has_aggro_on(&guard, victim.obj_id),
        "the guard must not aggro the victim it protects"
    );
}

/// `aggro_link_special_ignore_npc_attacker` (0x40): a monster attacking a player does not
/// pull the city guard, which keeps attacking monsters it sees on its own (unchanged).
#[tokio::test]
async fn an_ignoring_guard_is_not_pulled_by_a_monster_attacking_a_player() {
    use crate::models::npc::Npc;
    use crate::world::Entity;
    let (server, victim, _attacker, guard) = guard_fixture(true);
    let mut mob = Npc::new(MONSTER_TEMPLATE);
    mob.unit.transform.set_xyz(102.0, 100.0, 100.0);
    mob.unit.faction_id = MONSTER_FACTION;
    let mob = Entity::new(0x404, mob);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&mob, &server.view_ctx());
    apply_hit(
        &server,
        &WorldObject::from(&mob),
        &WorldObject::from(&victim),
    );
    assert!(
        !has_aggro_on(&guard, mob.obj_id),
        "a guard with ignore_npc_attacker must ignore a monster attacker"
    );
}

/// `challenger` asks `challenged` for a duel and the challenged accepts: the pair is then a
/// started duel and both carry the temporary Red / Blue team factions.
async fn start_duel(
    server: &GameServer,
    challenger: &crate::world::PlayerRef,
    challenged: &crate::world::PlayerRef,
) {
    let duels = &server.systems.social.duels;
    duels.duel_request(server, challenger, challenged.character_id);
    duels
        .duel_accepted(server, challenged, challenger.character_id)
        .await;
}

/// A duel between the victim and the attacker is not helped (10.x `sub_39322ED0`,
/// Z:823876).
#[tokio::test]
async fn an_accepted_duel_does_not_pull_the_guard() {
    let (server, victim, attacker, guard) = guard_fixture(true);
    start_duel(&server, &attacker, &victim).await;
    assert!(
        server
            .systems
            .social
            .duels
            .is_dueling_with(attacker.character_id, victim.character_id),
        "fixture: the duel must be accepted"
    );
    apply_hit(
        &server,
        &WorldObject::from(&attacker),
        &WorldObject::from(&victim),
    );
    assert!(
        !has_aggro_on(&guard, attacker.obj_id),
        "a duel must not pull the city guard"
    );
}

/// A duel request nobody answered is no duel (it lives up to 30 s, so it would otherwise
/// let a pirate silence the guards by challenging the victim first).
#[tokio::test]
async fn a_pending_duel_request_does_not_keep_the_guard_out() {
    let (server, victim, attacker, guard) = guard_fixture(true);
    server
        .systems
        .social
        .duels
        .duel_request(&server, &attacker, victim.character_id);
    let duels = &server.systems.social.duels;
    assert!(
        duels.is_in_duel_with(attacker.character_id, victim.character_id)
            && !duels.is_dueling_with(attacker.character_id, victim.character_id),
        "fixture: the request is registered but unanswered"
    );
    apply_hit(
        &server,
        &WorldObject::from(&attacker),
        &WorldObject::from(&victim),
    );
    assert!(
        has_aggro_on(&guard, attacker.obj_id),
        "an unanswered duel request must not stop the guard from helping"
    );
}

/// The victim's `system_factions.guard_help` is the switch (10.x Z:823905-823909).
#[tokio::test]
async fn a_faction_without_guard_help_does_not_pull_the_guard() {
    let (server, victim, attacker, guard) = guard_fixture(false);
    apply_hit(
        &server,
        &WorldObject::from(&attacker),
        &WorldObject::from(&victim),
    );
    assert!(
        !has_aggro_on(&guard, attacker.obj_id),
        "a victim faction with guard_help=false must not pull the guard"
    );
}

/// Only `aggro_link_special_guard` templates answer the player-victim scan; a plain NPC in
/// the same spot stays out of the fight (the control for the four tests above).
#[tokio::test]
async fn a_non_special_npc_is_not_pulled_by_the_guard_scan() {
    let (server, victim, attacker, _guard) = guard_fixture(true);
    let plain = spawn_link_npc(&server, 0x405, PLAIN_TEMPLATE, 103.0);
    plain.data().unit.faction_id = GUARD_FACTION;
    apply_hit(
        &server,
        &WorldObject::from(&attacker),
        &WorldObject::from(&victim),
    );
    assert!(
        !has_aggro_on(&plain, attacker.obj_id),
        "a template without aggro_link_special_guard must not join"
    );
}

/// NPC victims keep the existing C# help path: a guard attacked by a monster still pulls a
/// nearby fellow guard (the player-victim guard gate must not override it).
#[tokio::test]
async fn a_guard_attacked_by_a_monster_pulls_a_nearby_fellow_guard() {
    use super::super::framework::{AiKind, NpcAi};
    use crate::models::npc::Npc;
    use crate::test_support::{test_server_with, TestClient};
    use crate::world::Entity;

    let server = Arc::new(test_server_with(guard_data(true)));
    server.set_self();
    crate::systems::skills::bind(&server);
    let _client = TestClient::new(&server, 1, 1001);

    let mut victim = Npc::new(GUARD_TEMPLATE);
    victim.unit.transform.set_xyz(100.0, 100.0, 100.0);
    victim.unit.faction_id = GUARD_FACTION;
    let victim = Entity::new(0x406, victim);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&victim, &server.view_ctx());
    let fellow = spawn_link_npc(&server, 0x407, GUARD_TEMPLATE, 103.0);
    fellow.data().unit.faction_id = GUARD_FACTION;
    let mut mob = Npc::new(MONSTER_TEMPLATE);
    mob.unit.transform.set_xyz(101.0, 100.0, 100.0);
    mob.unit.faction_id = MONSTER_FACTION;
    let mob = Entity::new(0x408, mob);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&mob, &server.view_ctx());

    let abuser = WorldObject::from(&mob);
    let mut ai = NpcAi::new(AiKind::AlmightyNpc, &victim);
    let mut context = AiCtx {
        server: &server,
        npc: victim.clone(),
        obj: WorldObject::from(&victim),
        ai: &mut ai,
    };
    update_aggro_help(&mut context, &abuser, 200.0);

    assert!(
        has_aggro_on(&fellow, mob.obj_id),
        "a fellow guard must still answer a guard attacked by a monster"
    );
}

/// The player-victim scan is rate-limited per `(victim, abuser)` pair: a second hit within
/// the interval does not rescan, an expired stamp lets the next hit pick up a newly arrived
/// guard.
#[tokio::test]
async fn guard_help_scans_are_rate_limited_per_pair() {
    let (server, victim, attacker, guard) = guard_fixture(true);
    let abuser = WorldObject::from(&attacker);
    let target = WorldObject::from(&victim);
    apply_hit(&server, &abuser, &target);
    assert!(has_aggro_on(&guard, attacker.obj_id), "first scan helps");

    // A guard spawned after the first scan is not reached by an immediate second hit. The
    // pair's stamp is moved into the future so "within the interval" does not depend on how
    // long the test takes on a loaded CI runner.
    let late = spawn_link_npc(&server, 0x409, GUARD_TEMPLATE, 104.0);
    late.data().unit.faction_id = GUARD_FACTION;
    late.data().unit.hp = 1000;
    late.data().unit.max_hp = 1000;
    server.systems.ai.guard_help_scans.lock().unwrap().insert(
        (victim.obj_id, attacker.obj_id),
        crate::world::now_ms() + 60_000,
    );
    apply_hit(&server, &abuser, &target);
    assert!(
        !has_aggro_on(&late, attacker.obj_id),
        "a second hit within one second must not rescan"
    );

    // Expire the pair's stamp (what one second of fighting does) and hit again.
    server.systems.ai.guard_help_scans.lock().unwrap().insert(
        (victim.obj_id, attacker.obj_id),
        crate::world::now_ms() - GUARD_HELP_SCAN_INTERVAL_MS,
    );
    apply_hit(&server, &abuser, &target);
    assert!(
        has_aggro_on(&late, attacker.obj_id),
        "an expired stamp must allow the next scan to help the late guard"
    );
}

/// The duel exception resolves pets / slaves to their owner: a duellist's mate attacking
/// the other duellist must not pull the guard.
#[tokio::test]
async fn a_duellists_pet_attacker_does_not_pull_the_guard() {
    use crate::models::mate::Mate;
    use crate::world::Entity;

    let (server, victim, attacker, guard) = guard_fixture(true);
    start_duel(&server, &attacker, &victim).await;
    let mut mate = Mate::new(0x409, 1);
    mate.owner_obj_id = attacker.obj_id;
    // A duellist's pet takes the owner's temporary team faction.
    mate.unit.faction_id = crate::systems::social::duel::FACTION_RED_TEAM;
    mate.unit.hp = 1000;
    mate.unit.max_hp = 1000;
    mate.unit.transform.set_xyz(102.0, 100.0, 100.0);
    let mate = Entity::new(0x40a, mate);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&mate, &server.view_ctx());

    apply_hit(
        &server,
        &WorldObject::from(&mate),
        &WorldObject::from(&victim),
    );
    assert!(
        !has_aggro_on(&guard, mate.obj_id),
        "the duel pair's pet attacker must not pull the guard"
    );
}

/// Captures the formatted WARN lines of the current thread.
#[derive(Clone, Default)]
struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn warnings(f: impl FnOnce()) -> Vec<String> {
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(capture.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let bytes = capture.0.lock().unwrap();
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}
