//! `SphereDoodadInteract`: Sharpwind Mines' bridge-collapse sphere (1552) uses skill 11284 on
//! the Derelict Bridge doodad (5058) when a player crosses the sphere area, which advances the
//! bridge to its broken phase (13147 -> 13148). The area comes from `sphere_area_fill.json`;
//! the sphere id, skill and the `DoodadFuncAreaTrigger` marker are the shipped data rows.

use std::sync::Arc;

use super::data::DbSphere;
use super::spheres::SphereArea;
use crate::data::GameData;
use crate::models::doodad::Doodad;
use crate::systems::npcs::doodad::templates::{
    DoodadFunc, DoodadFuncAreaTrigger, DoodadFuncGroups, DoodadTemplate, FuncTemplate,
};
use crate::systems::npcs::sphere_quest::SphereDoodadInteracts;
use crate::systems::skills::data::SkillTemplate;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{DoodadRef, Entity};

const SPHERE: u32 = 1552;
const DETAIL: u32 = 55;
const SKILL: u32 = 11284;
const BRIDGE_TEMPLATE: u32 = 5058;
const BRIDGE_PHASE: u32 = 13147;
const BROKEN_PHASE: u32 = 13148;

fn data() -> GameData {
    let mut data = GameData::empty();
    data.systems.quests.spheres.insert(
        SPHERE,
        DbSphere {
            id: SPHERE,
            enter_or_leave: true,
            sphere_detail_id: DETAIL,
            sphere_detail_type: "SphereDoodadInteract".into(),
            ..DbSphere::default()
        },
    );
    data.systems.npcs.sphere.sphere_doodad_interacts.add(
        DETAIL,
        SphereDoodadInteracts {
            id: DETAIL,
            skill_id: SKILL,
            doodad_family_id: 50000,
        },
    );
    data.systems.skills.skills.insert(
        SKILL,
        Arc::new(SkillTemplate {
            id: SKILL,
            max_range: 4,
            ..SkillTemplate::default()
        }),
    );

    // Doodad 5058 phase 13147: one `DoodadFuncAreaTrigger` row leading to 13148 (the broken
    // rail model). This is the shipped `doodad_funcs` row 11162.
    let doodads = &mut data.systems.npcs.doodad;
    let start = DoodadFuncGroups {
        id: BRIDGE_PHASE,
        almighty: BRIDGE_TEMPLATE,
        group_kind_id: crate::systems::npcs::doodad::templates::doodad_func_group_kind::START,
        ..DoodadFuncGroups::default()
    };
    let broken = DoodadFuncGroups {
        id: BROKEN_PHASE,
        almighty: BRIDGE_TEMPLATE,
        group_kind_id: crate::systems::npcs::doodad::templates::doodad_func_group_kind::NORMAL,
        ..DoodadFuncGroups::default()
    };
    for group in [start.clone(), broken.clone()] {
        doodads
            .all_func_groups_index
            .insert(group.id, doodads.all_func_groups.len());
        doodads.all_func_groups.push(group.clone());
    }
    let mut template = DoodadTemplate {
        id: BRIDGE_TEMPLATE,
        ..DoodadTemplate::default()
    };
    template.func_groups.push(start);
    template.func_groups.push(broken);
    doodads.template_order.push(template.id);
    doodads.templates.insert(template.id, template);
    let trigger = FuncTemplate::AreaTrigger(DoodadFuncAreaTrigger {
        id: 73,
        npc_id: 0,
        is_enter: true,
    });
    doodads
        .func_templates
        .entry("DoodadFuncAreaTrigger")
        .or_default()
        .insert(73, trigger);
    let func = DoodadFunc {
        group_id: BRIDGE_PHASE,
        func_id: 73,
        func_key: 11162,
        func_type: "DoodadFuncAreaTrigger".into(),
        next_phase: BROKEN_PHASE as i32,
        ..Default::default()
    };
    doodads.funcs_by_id.insert(func.func_key, func.clone());
    doodads
        .funcs_by_groups
        .entry(BRIDGE_PHASE)
        .or_default()
        .push(func);
    data
}

fn bridge(server: &crate::GameServer) -> DoodadRef {
    let doodad = Entity::new(0x200, Doodad::new(BRIDGE_TEMPLATE));
    doodad.data().func_group_id = BRIDGE_PHASE;
    doodad.data().transform.set_xyz(100.0, 100.0, 100.0);
    let world = server.world.world(0).unwrap();
    world.spawn(&doodad, &server.view_ctx());
    doodad
}

#[tokio::test]
async fn crossing_the_bridge_sphere_advances_the_bridge_to_its_broken_phase() {
    let server = test_server_with(data());
    server.systems.quests.spheres.set_areas(vec![SphereArea {
        world: "main_world".into(),
        zone: TEST_ZONE,
        sphere_id: SPHERE,
        xyz: (100.0, 100.0, 100.0),
        radius: 5.0,
    }]);
    let client = TestClient::new(&server, 1, 1);
    let _player = spawn_player(
        &server,
        &client,
        test_character(1, "P", 100.0, 100.0),
        0x100,
    );
    let bridge = bridge(&server);
    assert_eq!(bridge.data().func_group_id, BRIDGE_PHASE);

    server.systems.quests.spheres.tick_at(&server, 0);
    assert_eq!(
        bridge.data().func_group_id,
        BROKEN_PHASE,
        "the party crossed the bridge area: the bridge must give way"
    );
}

#[tokio::test]
async fn a_player_outside_the_sphere_does_not_collapse_the_bridge() {
    let server = test_server_with(data());
    server.systems.quests.spheres.set_areas(vec![SphereArea {
        world: "main_world".into(),
        zone: TEST_ZONE,
        sphere_id: SPHERE,
        xyz: (100.0, 100.0, 100.0),
        radius: 5.0,
    }]);
    let client = TestClient::new(&server, 1, 1);
    let _player = spawn_player(
        &server,
        &client,
        test_character(1, "P", 200.0, 200.0),
        0x100,
    );
    let bridge = bridge(&server);
    server.systems.quests.spheres.tick_at(&server, 0);
    assert_eq!(
        bridge.data().func_group_id,
        BRIDGE_PHASE,
        "nobody crossed the trigger area"
    );
}
