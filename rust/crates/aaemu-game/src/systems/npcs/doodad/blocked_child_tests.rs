//! `blocked_child_doodads` (WP-2c): a player cannot use a child doodad while its parent unit
//! carries a buff that lists the child's template. Data: buff 6064 -> doodad 6560 (fishing boat
//! crane) and buff 6245 -> doodad 3446 (backpack storage box).

use std::sync::{Arc, Mutex};

use super::*;
use crate::data::skills::{BuffKind, BuffTemplate};
use crate::data::GameData;
use crate::models::buff::{Buff, SkillCaster};
use crate::models::slave::Slave;
use crate::opcodes::sc;
use crate::systems::npcs::doodad::manager;
use crate::systems::npcs::doodad::templates::{
    doodad_func_group_kind, DoodadFunc, DoodadFuncFakeUse, DoodadFuncGroups, DoodadTemplate,
    FuncTemplate,
};
use crate::systems::npcs::hooks::SkillsBridge;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{Entity, PlayerRef, SlaveRef, WorldObject};

const TEMPLATE: u32 = 6560;
const OTHER_TEMPLATE: u32 = 3446;
const BLOCKING_BUFF: u32 = 6064;
const OTHER_BLOCKING_BUFF: u32 = 6245;
/// The `DoodadFuncFakeUse.FakeSkillId` the use is started with.
const USE_SKILL: u32 = 777;
const GROUP_START: u32 = 17348;
const GROUP_NEXT: u32 = 17412;
const FUNC_KEY: u32 = 1;

/// A template with a START group and one `DoodadFuncFakeUse` leading to the next group.
fn template(id: u32) -> DoodadTemplate {
    DoodadTemplate {
        id,
        func_groups: vec![
            DoodadFuncGroups {
                id: GROUP_START,
                almighty: id,
                group_kind_id: doodad_func_group_kind::START,
                ..DoodadFuncGroups::default()
            },
            DoodadFuncGroups {
                id: GROUP_NEXT,
                almighty: id,
                group_kind_id: doodad_func_group_kind::NORMAL,
                ..DoodadFuncGroups::default()
            },
        ],
        ..DoodadTemplate::default()
    }
}

fn data() -> GameData {
    let mut gd = GameData::empty();
    for id in [TEMPLATE, OTHER_TEMPLATE] {
        gd.systems.npcs.doodad.templates.insert(id, template(id));
        gd.systems.npcs.doodad.template_order.push(id);
    }
    let func = DoodadFunc {
        group_id: GROUP_START,
        func_id: FUNC_KEY,
        func_key: FUNC_KEY,
        func_type: "DoodadFuncFakeUse".into(),
        next_phase: GROUP_NEXT as i32,
        ..DoodadFunc::default()
    };
    gd.systems
        .npcs
        .doodad
        .funcs_by_id
        .insert(FUNC_KEY, func.clone());
    gd.systems
        .npcs
        .doodad
        .funcs_by_groups
        .insert(GROUP_START, vec![func]);
    gd.systems
        .npcs
        .doodad
        .func_templates
        .entry("DoodadFuncFakeUse")
        .or_default()
        .insert(
            FUNC_KEY,
            FuncTemplate::FakeUse(DoodadFuncFakeUse {
                id: FUNC_KEY,
                fake_skill_id: USE_SKILL,
                ..DoodadFuncFakeUse::default()
            }),
        );
    gd.systems
        .skills
        .blocked_child_doodads
        .add(BLOCKING_BUFF, TEMPLATE);
    gd.systems
        .skills
        .blocked_child_doodads
        .add(OTHER_BLOCKING_BUFF, OTHER_TEMPLATE);
    gd
}

#[derive(Default)]
struct RecordingSkills {
    cancelled: Mutex<Vec<u32>>,
}

impl SkillsBridge for RecordingSkills {
    fn cancel_skill(&self, _server: &GameServer, player: &PlayerRef) {
        self.cancelled.lock().unwrap().push(player.data().id);
    }
}

struct Fx {
    server: Arc<GameServer>,
    client: TestClient,
    player: PlayerRef,
    world: Arc<WorldInstance>,
    parent: SlaveRef,
    skills: Arc<RecordingSkills>,
}

fn fx() -> Fx {
    let server = Arc::new(test_server_with(data()));
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "User", 100.0, 100.0),
        server.ids.object.get_next_id().unwrap(),
    );
    let world = server.world.world(0).unwrap();
    let mut hull = Slave::new(700, 75);
    hull.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let parent = Entity::new(server.ids.object.get_next_id().unwrap(), hull);
    world.spawn(&parent, &server.view_ctx());
    let skills = Arc::new(RecordingSkills::default());
    server
        .systems
        .npcs
        .hooks
        .skills
        .install(skills.clone() as Arc<dyn SkillsBridge>);
    client.drain();
    Fx {
        server,
        client,
        player,
        world,
        parent,
        skills,
    }
}

fn add_buff(unit: &WorldObject, buff_id: u32) {
    unit.with_base_unit(|b| {
        b.buffs_mut().effects.push(Buff::new(
            BuffTemplate::simple(buff_id, BuffKind::Good),
            SkillCaster::default(),
            None,
            crate::world::now_ms(),
        ));
    });
}

/// A child doodad of the fixture's slave (`Doodad.Use` sees it as a child of the hull).
fn child(f: &Fx) -> DoodadRef {
    let doodad = manager::create(&f.server, &f.world, 0, TEMPLATE, None, true).expect("doodad");
    doodad.data().parent_obj_id = f.parent.obj_id;
    f.world
        .attach_child(&WorldObject::from(&f.parent), &WorldObject::from(&doodad));
    f.world.spawn(&doodad, &f.server.view_ctx());
    doodad
}

fn use_skill(f: &mut Fx, doodad: &DoodadRef) {
    manager::use_doodad(
        &f.server,
        &f.world,
        doodad,
        Some(&WorldObject::from(&f.player)),
        USE_SKILL,
        0,
    );
}

#[tokio::test]
async fn use_of_child_doodad_is_refused_while_parent_has_the_blocking_buff() {
    let mut f = fx();
    add_buff(&WorldObject::from(&f.parent), BLOCKING_BUFF);
    let doodad = child(&f);

    use_skill(&mut f, &doodad);

    assert_eq!(doodad.data().func_group_id, GROUP_START, "no func ran");
    assert_eq!(
        f.skills.cancelled.lock().unwrap().clone(),
        vec![1],
        "the cast is cancelled so it refunds its cost"
    );
    assert!(
        f.client
            .drain()
            .contains(&(sc::SCErrorMsgPacket, vec![3, 0, 3, 0, 0, 0, 0, 0, 1])),
        "NoInteractionAvailable to the player"
    );
}

#[tokio::test]
async fn use_of_child_doodad_without_the_buff_is_allowed() {
    let mut f = fx();
    // An unrelated blocking row on the parent must not block this template.
    add_buff(&WorldObject::from(&f.parent), OTHER_BLOCKING_BUFF);
    let doodad = child(&f);

    use_skill(&mut f, &doodad);

    assert_eq!(
        doodad.data().func_group_id,
        GROUP_NEXT,
        "the func ran and moved the phase"
    );
    assert!(f.skills.cancelled.lock().unwrap().is_empty());
    assert!(!f.client.drain_ops().contains(&sc::SCErrorMsgPacket));
}

#[tokio::test]
async fn doodad_without_a_parent_is_not_blocked_even_when_the_caster_has_the_buff() {
    let mut f = fx();
    add_buff(&WorldObject::from(&f.player), BLOCKING_BUFF);
    let doodad = manager::create(&f.server, &f.world, 0, TEMPLATE, None, true).expect("doodad");
    f.world.spawn(&doodad, &f.server.view_ctx());

    use_skill(&mut f, &doodad);

    assert_eq!(doodad.data().func_group_id, GROUP_NEXT);
    assert!(f.skills.cancelled.lock().unwrap().is_empty());
}
