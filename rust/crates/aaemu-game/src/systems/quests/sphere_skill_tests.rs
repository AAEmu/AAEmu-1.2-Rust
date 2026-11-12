//! `SphereSkill`: the Sharpwind Mines proximity triggers (spheres 1965-1968) cast their
//! `sphere_skills` row with the character as caster on the enter edge; the skill's
//! `NpcControlEffect` `RunCommandSet` reaches the invisible "Spawn Slave" NPC of its
//! `skill_effects.target_npc_tag_id` and runs the authored spawn command.
//!
//! The unit fixtures prove the dispatch (enter edge, unit requirements, tag filter); the
//! `AAEMU_COMPACT_DB` flow test drives the whole shipped chain: sphere 1965 -> skill 19522 ->
//! NPC 12116 -> command set 202 -> skill 19525 -> slime spawners 94761-94765, and sphere 1968 ->
//! skill 19533 -> NPC 12119 -> command set 206 -> skill 19534 -> Okape's spawner 90004.

use std::path::Path;
use std::sync::Arc;

use super::data::DbSphere;
use super::spheres::{load_sphere_areas, SphereArea};
use super::unit_reqs::{req_kind, UnitReq};
use crate::data::client_files::ClientFileManager;
use crate::data::world_templates::WorldTemplate;
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::systems::npcs::doodad::spawner::{NpcSpawnerDef, SpawnerBase};
use crate::systems::npcs::sphere_quest::SphereSkills;
use crate::systems::skills::data::effects::BuffEffect;
use crate::systems::skills::data::{
    BuffTemplate, EffectKind, EffectTemplate, SkillEffect, SkillTemplate,
};
use crate::systems::skills::state::{CancelToken, SkillRun, SkillTaskKind, SkillTaskSlot};
use crate::systems::DataLoadCtx;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{Entity, NpcRef};

const SPHERE: u32 = 1965;
const DETAIL: u32 = 175;
const SKILL: u32 = 19522;
const EFFECT: u32 = 24156;
const TAG: u32 = 798;
const TRIGGER_TEMPLATE: u32 = 7000;
const OTHER_TEMPLATE: u32 = 7001;
const BUFF: u32 = 9001;

fn data() -> GameData {
    let mut data = GameData::empty();
    data.systems.quests.spheres.insert(
        SPHERE,
        DbSphere {
            id: SPHERE,
            enter_or_leave: true,
            sphere_detail_id: DETAIL,
            sphere_detail_type: "SphereSkill".into(),
            trigger_condition_id: 3,
            ..DbSphere::default()
        },
    );
    data.systems.npcs.sphere.sphere_skills.add(
        DETAIL,
        SphereSkills {
            id: DETAIL,
            skill_id: SKILL,
            max_rate: 100,
            min_rate: 100,
        },
    );
    // The shipped `tagged_npcs` row (tag 798 -> the "Spawn Slave" 12116); the fixture NPC
    // templates keep the flow test independent of the NPC catalog.
    data.systems
        .skills
        .npc_tags
        .entry(TAG)
        .or_default()
        .push(TRIGGER_TEMPLATE);
    data.systems.skills.add_buff(BuffTemplate {
        id: BUFF,
        duration: 60_000,
        ..BuffTemplate::default()
    });
    let mut skill = SkillTemplate {
        id: SKILL,
        // The shipped row: self-centred area skill (target_type 0, selection 1, radius 50).
        target_type: 0,
        target_selection: 1,
        target_relation: 0,
        target_area_radius: 50,
        target_area_count: 100,
        target_area_angle: 360,
        target_alive: true,
        ..SkillTemplate::default()
    };
    skill.effects.push(SkillEffect {
        effect_id: EFFECT,
        template: Some((EffectKind::BuffEffect, EFFECT)),
        weight: 0,
        start_level: 0,
        end_level: 255,
        friendly: false,
        non_friendly: false,
        target_buff_tag_id: 0,
        target_no_buff_tag_id: 0,
        source_buff_tag_id: 0,
        source_no_buff_tag_id: 0,
        chance: 100,
        front: false,
        back: false,
        target_npc_tag_id: TAG,
        application_method: 1,
        consume_source_item: false,
        consume_item_id: 0,
        consume_item_count: 0,
        always_hit: false,
        item_set_id: 0,
        interaction_success_hit: false,
    });
    data.systems.skills.add_effect(
        EffectKind::BuffEffect,
        EffectTemplate::Buff(BuffEffect {
            id: EFFECT,
            buff_id: Some(BUFF),
            chance: 100,
            stack: 1,
            ab_level: 0,
        }),
        EFFECT,
    );
    data.systems.skills.add_skill(skill);
    data
}

fn area() -> SphereArea {
    SphereArea {
        world: "main_world".into(),
        zone: TEST_ZONE,
        sphere_id: SPHERE,
        xyz: (100.0, 100.0, 100.0),
        radius: 8.0,
    }
}

/// An NPC standing at `(x, y, 100)`; no AI or catalog template is needed for the skill effects.
fn spawn_npc(server: &crate::GameServer, obj_id: u32, template: u32, x: f32) -> NpcRef {
    let mut n = Npc::new(template);
    n.unit.name = "Slave".into();
    n.unit.level = 10;
    n.unit.hp = 1000;
    n.unit.max_hp = 1000;
    n.unit.transform.set_xyz(x, 100.0, 100.0);
    n.unit.transform.zone_id = TEST_ZONE;
    let npc = Entity::new(obj_id, n);
    let world = server.world.world(0).expect("world");
    world.spawn(&npc, &server.view_ctx());
    npc
}

fn has_buff(npc: &NpcRef) -> bool {
    npc.data().unit.buffs.check_buff(BUFF)
}

fn clear_buff(npc: &NpcRef) {
    npc.data().unit.buffs.remove_buff(BUFF);
}

fn player_at(server: &crate::GameServer, client: &TestClient, x: f32) -> crate::world::PlayerRef {
    let mut character = test_character(1, "P", x, 100.0);
    character.unit.hp = 1000;
    character.unit.max_hp = 1000;
    spawn_player(server, client, character, 0x100)
}

/// The enter edge casts the `sphere_skills` row once; the scene includes a tagged and an
/// untagged NPC in the 50 m area, so the `target_npc_tag_id` filter is proven too.
#[tokio::test]
async fn entering_the_sphere_casts_the_skill_on_the_tagged_npc_once() {
    let server = test_server_with(data());
    server.systems.quests.spheres.set_areas(vec![area()]);
    let client = TestClient::new(&server, 1, 1);
    let _player = player_at(&server, &client, 100.0);
    let tagged = spawn_npc(&server, 0x200, TRIGGER_TEMPLATE, 104.0);
    let other = spawn_npc(&server, 0x201, OTHER_TEMPLATE, 101.0);

    server.systems.quests.spheres.tick_at(&server, 0);
    assert!(has_buff(&tagged), "the tagged slave takes the skill effect");
    assert!(
        !has_buff(&other),
        "an NPC outside the effect's tag is not a target"
    );

    // Still inside: the enter edge does not fire again.
    clear_buff(&tagged);
    server.systems.quests.spheres.tick_at(&server, 500);
    assert!(
        !has_buff(&tagged),
        "no second cast while the stay continues"
    );

    // Leave and re-enter: a new stay casts again.
    _player.data().unit.transform.set_xyz(200.0, 100.0, 100.0);
    server.systems.quests.spheres.tick_at(&server, 1_000);
    _player.data().unit.transform.set_xyz(100.0, 100.0, 100.0);
    server.systems.quests.spheres.tick_at(&server, 1_500);
    assert!(has_buff(&tagged), "a new stay casts the sphere skill again");
}

/// A character that enters while channeling is refused by the normal cast path
/// (`skill_cannot_cast_in_channeling`), so the enter edge is not consumed: the sphere retries
/// on the next area tick and casts once the channel ends, while the character still stands
/// inside. After the successful cast the stay does not cast again.
#[tokio::test]
async fn entering_while_channeling_casts_the_skill_after_the_channel_ends() {
    let server = test_server_with(data());
    server.systems.quests.spheres.set_areas(vec![area()]);
    let client = TestClient::new(&server, 1, 1);
    let player = player_at(&server, &client, 100.0);
    let tagged = spawn_npc(&server, 0x200, TRIGGER_TEMPLATE, 104.0);

    // The character is channeling when it crosses into the sphere. The task slot is the state
    // `use_skill` checks; starting a real channel needs the skills task scheduler, which the
    // unit fixture does not bind.
    let channel = SkillRun::new(
        Arc::new(SkillTemplate {
            id: 19_999,
            channeling_time: 10_000,
            ..SkillTemplate::default()
        }),
        1,
    );
    player.data().unit.skill_state.skill_task = Some(SkillTaskSlot {
        kind: SkillTaskKind::EndChanneling { channel_doodad: 0 },
        skill: channel,
        cancel: CancelToken::new(),
        extend_ms: 0,
    });

    server.systems.quests.spheres.tick_at(&server, 0);
    assert!(
        !has_buff(&tagged),
        "the channel refuses the sphere skill on the enter edge"
    );

    player.data().unit.skill_state.skill_task = None;
    server.systems.quests.spheres.tick_at(&server, 500);
    assert!(
        has_buff(&tagged),
        "the refused cast is retried once the channel ends"
    );

    clear_buff(&tagged);
    server.systems.quests.spheres.tick_at(&server, 1_000);
    assert!(
        !has_buff(&tagged),
        "a successful cast settles the enter edge"
    );
}

/// A character that misses the sphere's `unit_reqs` at the entry is not served later while it
/// stays inside (the requirement is evaluated on the enter edge, unlike the doodad retry); its
/// next entry passes once the requirements are met.
#[tokio::test]
async fn a_character_that_misses_the_requirements_is_not_cast_for_while_inside() {
    let mut data = data();
    data.systems
        .quests
        .unit_reqs
        .by_owner
        .entry("Sphere".into())
        .or_default()
        .insert(
            SPHERE,
            vec![UnitReq {
                id: 1,
                owner_id: SPHERE,
                owner_type: "Sphere".into(),
                kind: req_kind::LEVEL,
                value1: 50,
                value2: 0,
            }],
        );
    let server = test_server_with(data);
    server.systems.quests.spheres.set_areas(vec![area()]);
    let client = TestClient::new(&server, 1, 1);
    let player = player_at(&server, &client, 100.0);
    let tagged = spawn_npc(&server, 0x200, TRIGGER_TEMPLATE, 104.0);

    server.systems.quests.spheres.tick_at(&server, 0);
    assert!(!has_buff(&tagged), "level 10 < 50: the entry is refused");

    // The level-up while inside does not re-run the enter edge.
    player.data().unit.level = 50;
    server.systems.quests.spheres.tick_at(&server, 500);
    assert!(!has_buff(&tagged));

    // The next entry passes.
    player.data().unit.transform.set_xyz(200.0, 100.0, 100.0);
    server.systems.quests.spheres.tick_at(&server, 1_000);
    player.data().unit.transform.set_xyz(100.0, 100.0, 100.0);
    server.systems.quests.spheres.tick_at(&server, 1_500);
    assert!(has_buff(&tagged));
}

/// Only characters produce the enter edge: an NPC standing in the sphere does not cast it (the
/// later player entry proves the scene is able to fire).
#[tokio::test]
async fn a_non_character_inside_the_sphere_does_not_cast_the_skill() {
    let server = test_server_with(data());
    server.systems.quests.spheres.set_areas(vec![area()]);
    let client = TestClient::new(&server, 1, 1);
    let player = player_at(&server, &client, 500.0);
    let tagged = spawn_npc(&server, 0x200, TRIGGER_TEMPLATE, 104.0);

    server.systems.quests.spheres.tick_at(&server, 0);
    assert!(
        !has_buff(&tagged),
        "the NPC inside the area is not a character entering it"
    );

    // The same scene fires for a character: the negative assertion above is meaningful.
    player.data().unit.transform.set_xyz(100.0, 100.0, 100.0);
    server.systems.quests.spheres.tick_at(&server, 500);
    assert!(has_buff(&tagged));
}

// ---------------------------------------------------------------------------------------------
// Real data: the shipped Sharpwind proximity chains
// ---------------------------------------------------------------------------------------------

const SHARPWIND: &str = "instance_cuttingwind_deadmine";

/// A server with the real skills / npcs / quests / AI tables and a synthetic Sharpwind world
/// (the shipped `sphere_area_fill.json` areas are used as they are).
fn real_server(db: &str) -> Arc<crate::GameServer> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let app_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let files = ClientFileManager::from_sources(&[], &app_path);
    let ctx = DataLoadCtx {
        conn: &conn,
        app_path: &app_path,
        files: &files,
        language: "en_us",
    };
    let mut data = GameData::empty();
    data.formulas = crate::data::formulas::FormulaManager::load(&conn).expect("formulas");
    data.systems.skills = crate::systems::skills::Data::load(&ctx).expect("skills data");
    data.systems.npcs = crate::systems::npcs::Data::load(&ctx).expect("npcs data");
    data.systems.quests = crate::systems::quests::Data::load(&ctx).expect("quests data");
    data.systems.ai = crate::systems::ai::Data::load(&ctx).expect("ai data");
    let world = WorldTemplate::synthetic(0, SHARPWIND, 32, 32, TEST_ZONE);
    let server = Arc::new(crate::test_support::test_server_with_world(data, world));
    server.set_self();
    // `GameServer::new` creates the static instance only for a world named "main_world".
    let template = server
        .game_data
        .worlds
        .get_world_template_by_name(SHARPWIND)
        .expect("Sharpwind world template")
        .clone();
    server.world.create_world_instance(Arc::new(template), 0);
    crate::systems::skills::bind(&server);
    crate::systems::npcs::install_hooks(&server);
    crate::systems::ai::hooks::install(&server);
    server
}

fn event_spawner(spawner_id: u32, unit_id: u32, x: f32, y: f32, z: f32) -> NpcSpawnerDef {
    let mut base = SpawnerBase {
        unit_id,
        ..SpawnerBase::default()
    };
    base.position.world_id = 0;
    base.position.zone_id = TEST_ZONE;
    base.position.x = x;
    base.position.y = y;
    base.position.z = z;
    NpcSpawnerDef {
        base,
        count: 1,
        npc_spawner_ids: vec![spawner_id],
    }
}

/// Places the invisible trigger NPC (`event row` with an `NpcSpawnerIds` entry, like the level
/// spawner rows of `npc_rows_from_level_spawners.json`) and returns the spawned NPC.
fn place_trigger(
    server: &Arc<crate::GameServer>,
    spawner_id: u32,
    npc_id: u32,
    x: f32,
    y: f32,
    z: f32,
) -> NpcRef {
    let world = server.world.world(0).expect("world");
    let m = server.systems.npcs.spawns.get_or_create(&world);
    m.add_npc_spawner(server, event_spawner(spawner_id, npc_id, x, y, z))
        .expect("trigger spawner");
    let spawner = m.get_npc_spawner(spawner_id).remove(0);
    spawner.force_spawn(server, 0).expect("trigger NPC")
}

/// The Sharpwind trigger NPC rows (positions from the 10.x level, template ids and tags 1.2).
fn register_event_spawners(server: &Arc<crate::GameServer>) {
    let world = server.world.world(0).expect("world");
    let m = server.systems.npcs.spawns.get_or_create(&world);
    // The spawners skills 19525/19534 spawn (the level's inactive slime and Okape spawners).
    for (spawner, x, y, z) in [
        (94761, 524.021, 382.429, 162.625),
        (94762, 529.349, 380.388, 161.795),
        (94763, 530.396, 383.382, 161.398),
        (94764, 528.716, 385.78, 161.026),
        (94765, 525.695, 385.439, 161.996),
    ] {
        m.add_npc_spawner(server, event_spawner(spawner, 11361, x, y, z))
            .expect("slime spawner");
    }
    m.add_npc_spawner(
        server,
        event_spawner(90004, 12188, 613.275, 641.428, 139.873),
    )
    .expect("Okape spawner");
}

fn spawned_count(server: &Arc<crate::GameServer>, spawner_id: u32) -> i32 {
    let world = server.world.world(0).expect("world");
    let m = server.systems.npcs.spawns.get_or_create(&world);
    m.get_npc_spawner(spawner_id)
        .first()
        .map_or(0, |s| s.current_spawn_count())
}

fn ai_queued(server: &Arc<crate::GameServer>, obj_id: u32) -> usize {
    server
        .systems
        .ai
        .ais
        .inspect(obj_id, |a| a.commands_queue.len())
        .unwrap_or(0)
}

fn tick_ai(server: &Arc<crate::GameServer>, ticks: usize) {
    for _ in 0..ticks {
        crate::systems::ai::manager::tick_all(server, 100);
    }
}

/// The shipped Sharpwind chain: entering sphere 1965 casts 19522, whose `NpcControlEffect`
/// queues command set 202 on the invisible slave 12116; the slave's `UseSkill` 19525 spawns the
/// five slimes 94761-94765 and removes itself. Entering sphere 1968 does the same for Okape:
/// skill 19533 -> set 206 on slave 12119 -> skill 19534 -> spawner 90004, slave removed.
///
/// `AAEMU_COMPACT_DB` must name a COPY of the r208022 `compact.sqlite3`:
/// `cargo test -p aaemu-game --lib real_data_sharpwind_spheres -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB (a copy of the r208022 compact.sqlite3)"]
async fn real_data_sharpwind_spheres_run_the_slave_command_sets() {
    let Some(db) = std::env::var("AAEMU_COMPACT_DB").ok() else {
        return;
    };
    let server = real_server(&db);
    register_event_spawners(&server);
    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let areas: Vec<SphereArea> = load_sphere_areas(&app, |id| id == 1965 || id == 1968)
        .into_iter()
        .filter(|a| a.world == SHARPWIND)
        .collect();
    assert!(!areas.is_empty(), "the shipped Sharpwind sphere areas");
    server.systems.quests.spheres.set_areas(areas);

    let client = TestClient::new(&server, 1, 1);
    let mut character = test_character(1, "P", 528.836, 384.945);
    character.unit.transform.set_xyz(528.836, 384.945, 161.144);
    character.unit.level = 20;
    character.unit.hp = 1000;
    character.unit.max_hp = 1000;
    let player = spawn_player(&server, &client, character, 0x10_0001);

    // The slave 3 of the ogre hall (tag 798), placed at the level's position.
    let slave3 = place_trigger(&server, 94776, 12116, 533.237, 386.698, 160.257);

    server.systems.quests.spheres.tick_at(&server, 0);
    assert!(
        ai_queued(&server, slave3.obj_id) > 0,
        "sphere 1965 must cast 19522 and queue command set 202 on slave 3"
    );
    tick_ai(&server, 20);
    for spawner in 94761..=94765 {
        assert_eq!(
            spawned_count(&server, spawner),
            1,
            "skill 19525 must spawn slime spawner {spawner}"
        );
    }
    assert!(
        server.world.object(slave3.obj_id).is_none(),
        "skill 19525 removes the trigger NPC"
    );

    // Sphere 1968 (the lake corridor) -> slave 6 -> Okape.
    let slave6 = place_trigger(&server, 94821, 12119, 606.637, 588.487, 135.336);
    player
        .data()
        .unit
        .transform
        .set_xyz(609.952, 587.882, 135.327);
    server.systems.quests.spheres.tick_at(&server, 500);
    assert!(
        ai_queued(&server, slave6.obj_id) > 0,
        "sphere 1968 must cast 19533 and queue command set 206 on slave 6"
    );
    tick_ai(&server, 20);
    assert_eq!(
        spawned_count(&server, 90004),
        1,
        "skill 19534 must spawn Okape's spawner 90004"
    );
    assert!(
        server.world.object(slave6.obj_id).is_none(),
        "skill 19534 removes the trigger NPC"
    );
}
