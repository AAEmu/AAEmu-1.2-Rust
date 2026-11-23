use super::*;
use crate::test_support::test_server;

fn elevator_spawner() -> GimmickSpawner {
    GimmickSpawner {
        position: Some(crate::data::world_templates::WorldSpawnPosition {
            world_id: 1,
            zone_id: 0,
            x: 100.0,
            y: 200.0,
            z: 10.0,
            ..Default::default()
        }),
        top_z: 11.0,
        bottom_z: 10.0,
        wait_time: 1.0,
        scale: 1.0,
        rotation_w: 1.0,
        entity_guid: 0x4a0d_a1f4_d842_0a38,
        ..GimmickSpawner::default()
    }
}

#[tokio::test]
async fn doodad_gimmick_uses_authored_offset_faction_and_motion() {
    let mut data = crate::data::GameData::empty();
    data.systems.npcs.gimmick.templates.insert(
        48,
        GimmickTemplate {
            id: 48,
            life_time: 1000,
            ..Default::default()
        },
    );
    let server = crate::test_support::test_server_with(data);
    let world = test_world(&server);
    let mut source = crate::models::doodad::Doodad::new(100);
    source.transform.set_xyz(100.0, 200.0, 10.0);
    let source = Entity::new(0x500, source);
    let g = spawn_from_doodad(
        &server,
        &world,
        &WorldObject::from(&source),
        &crate::systems::npcs::doodad::templates::DoodadFuncSpawnGimmick {
            gimmick_id: 48,
            faction_id: 115,
            scale: 0.4,
            offset_z: -2.0,
            velocity_z: 5.0,
            angle_x: 3.0,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(g.data().unit.transform.z, 8.0);
    assert_eq!(g.data().unit.faction_id, 115);
    assert_eq!(g.data().unit.scale, 0.4);
    let active = server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
    gimmick_tick(&server, &world, &active, Duration::from_millis(100), 1000);
    assert_eq!(g.data().unit.transform.z, 8.5);
    assert!((g.data().unit.transform.rot_x - 0.3).abs() < 1e-6);
    assert_eq!(g.data().vel, [0.0, 0.0, 5.0]);
    gimmick_tick(&server, &world, &active, Duration::from_millis(900), 1900);
    assert!(world.gimmick(g.obj_id).is_none());
}

fn test_world(server: &GameServer) -> Arc<WorldInstance> {
    match server.world.worlds().into_iter().next() {
        Some(w) => w,
        None => {
            let t = Arc::new(crate::data::world_templates::WorldTemplate::synthetic(
                0,
                "main_world",
                4,
                4,
                100,
            ));
            server.world.create_world_instance(t, 0)
        }
    }
}

fn cannon_fixture() -> (GameServer, Arc<WorldInstance>, WorldObject) {
    use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};

    let mut data = crate::data::GameData::empty();
    let mut impact = crate::systems::skills::tests::skill_template(17741);
    impact.target_type = 6; // Pos
    data.systems.skills.add_skill(impact);
    data.systems.npcs.gimmick.templates.insert(
        45,
        GimmickTemplate {
            id: 45,
            life_time: 5000,
            gravity: 9.8,
            collision_skill_id: 17741,
            disappear_by_collision: true,
            ..Default::default()
        },
    );
    let server = test_server_with(data);
    crate::systems::skills::bridges::install(&server);
    crate::systems::ai::collision::flow_tests::enable_collision(&server, 90.0);
    let world = test_world(&server);
    let client = TestClient::new(&server, 1, 1);
    let mut player = test_character(1, "Gunner", 100.0, 100.0);
    player.unit.transform.set_height(102.0);
    let shooter = WorldObject::from(spawn_player(&server, &client, player, 0x500));
    (server, world, shooter)
}

fn fire_cannon(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    shooter: &WorldObject,
    vertical_speed: f32,
) -> Arc<ActiveGimmick> {
    let shot = spawn_from_effect(
        server,
        world,
        &SpawnGimmickParams {
            gimmick_id: 45,
            offset_coordinate_id: 2,
            offset_y: 3.0,
            scale: 1.0,
            velocity_coordinate_id: 2,
            velocity_y: 20.0,
            velocity_z: vertical_speed,
            ..Default::default()
        },
        shooter,
    )
    .unwrap();
    server.systems.npcs.gimmicks.find(shot.obj_id).unwrap()
}

#[tokio::test]
async fn portable_cannon_hits_a_wall_inside_the_muzzle_offset() {
    use aaemu_physics::math::Vec3;

    let (server, world, shooter) = cannon_fixture();
    crate::systems::ai::collision::flow_tests::add_wall_boxes(
        &server,
        &[(0x600, Vec3::new(100.0, 101.5, 102.0), 0.25)],
    );
    let shot = fire_cannon(&server, &world, &shooter, 0.0);
    assert_eq!(shot.gimmick.data().unit.transform.y, 103.0);

    gimmick_tick(&server, &world, &shot, Duration::from_millis(50), 1000);

    assert_eq!(shot.state().skill_started, Some(17741));
    assert!(world.gimmick(shot.gimmick.obj_id).is_none());
    let y = shot.gimmick.data().unit.transform.y;
    assert!((y - 101.22).abs() < 0.05, "impact y={y}");
}

#[tokio::test]
async fn portable_cannon_detonates_at_the_water_surface() {
    let (server, world, shooter) = cannon_fixture();
    let shot = fire_cannon(&server, &world, &shooter, -20.0);
    gimmick_tick(&server, &world, &shot, Duration::from_millis(150), 1000);

    assert_eq!(shot.state().skill_started, Some(17741));
    assert!(world.gimmick(shot.gimmick.obj_id).is_none());
    let z = shot.gimmick.data().unit.transform.z;
    assert!((z - 100.03).abs() < 0.05, "impact z={z}");
}

#[tokio::test]
async fn portable_cannon_passes_its_own_hull_and_hits_another_ship() {
    use crate::systems::ai::collision::{
        phys_world,
        world::{ObjKind, Owner},
    };
    use aaemu_physics::entities::base::add_geometry;
    use aaemu_physics::geometry::primitives::{BoxPrim, Primitive};
    use aaemu_physics::math::Vec3;
    use aaemu_physics::params::{GeomParams, ParamsPos};

    let (server, world, shooter) = cannon_fixture();
    let mut own_ship = crate::models::slave::Slave::new(1, 24);
    own_ship.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let own_ship = WorldObject::from(Entity::new(0x600, own_ship));
    world.spawn(&own_ship, &server.view_ctx());
    world.set_sticky_parent(&shooter, Some(&own_ship));
    {
        let hulls = phys_world(&server, world.id).unwrap();
        let mut state = hulls.lock();
        let geom = state
            .world
            .geometry(&Primitive::Box(BoxPrim::aligned(
                Vec3::ZERO,
                Vec3::splat(0.5),
            )))
            .unwrap();
        for (obj_id, y) in [(own_ship.obj_id(), 101.5), (0x601, 104.0)] {
            let pose = ParamsPos {
                pos: Vec3::new(100.0, y, 102.0),
                ..Default::default()
            };
            let hull = state
                .world
                .create_physical_entity(2, 0.0, Some(&pose), 0, 0, -1)
                .unwrap();
            let params = GeomParams {
                mass: 10.0,
                flags: 0x80,
                flags_collider: 0x80,
                ..Default::default()
            };
            add_geometry(&mut state.world, hull, &geom, &params, -1);
            // Ship physics registers hulls by owner outside the normal object cache.
            state.owners.insert(
                hull,
                Owner::Object {
                    kind: ObjKind::Ship,
                    obj_id,
                },
            );
        }
    }
    let shot = fire_cannon(&server, &world, &shooter, 0.0);
    gimmick_tick(&server, &world, &shot, Duration::from_millis(100), 1000);

    assert_eq!(shot.state().skill_started, Some(17741));
    assert!(world.gimmick(shot.gimmick.obj_id).is_none());
    let y = shot.gimmick.data().unit.transform.y;
    assert!((y - 103.47).abs() < 0.05, "impact y={y}");
}

#[tokio::test]
async fn elevator_spawns_ticks_and_despawns() {
    let server = test_server();
    let world = test_world(&server);
    let g = spawn_from_spawner(&server, &world, &elevator_spawner()).expect("spawned");
    assert!(world.gimmick(g.obj_id).is_some());
    {
        let d = g.data();
        assert_eq!(d.unit.template_id, 0);
        assert_eq!(d.entity_guid, 0x4a0d_a1f4_d842_0a38);
        assert_eq!(d.unit.scale, 1.0);
        assert_eq!(d.unit.transform.instance_id, world.id);
    }
    let active = server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
    assert_eq!(active.handler, Some(MovementHandler::Elevator));
    // 1 m up at 4.5 m/s, 0.05 s ticks: 4.444 -> 5 ticks (4 steps + snap), then waits 1 s.
    let dt = Duration::from_millis(50);
    for i in 0..5 {
        gimmick_tick(&server, &world, &active, dt, 1_000 + i * 50);
    }
    assert_eq!(g.data().unit.transform.z, 11.0);
    let wait = active.state().wait_time_ms;
    assert_eq!(wait, 1_200 + 1_000);
    assert!(active.state().move_down);
    // waiting: no movement
    gimmick_tick(&server, &world, &active, dt, 1_300);
    assert_eq!(g.data().unit.transform.z, 11.0);
    // after the wait it goes down
    gimmick_tick(&server, &world, &active, dt, 2_300);
    assert!(g.data().unit.transform.z < 11.0);
    despawn(&server, &active);
    assert!(world.gimmick(g.obj_id).is_none());
    assert!(server.systems.npcs.gimmicks.find(g.obj_id).is_none());
}

// ----- contact hits (`GimmickTemplate.CollisionSkillId`) -----

const CONTACT_GIMMICK: u32 = 900;
const CONTACT_SKILL: u32 = 900_100;
const TIMER_SKILL: u32 = 900_102;

/// Game data with one contact gimmick template and the fixed 100-damage skill it fires
/// (`AnyUnit` target, no area, `max_range` 0 like the shipped collision skills).
fn contact_data(template: GimmickTemplate, area_radius: i32) -> crate::data::GameData {
    contact_data_with(
        template,
        area_radius,
        crate::systems::skills::tests::damage_effect(1, 100),
        |_| {},
    )
}

/// [`contact_data`] with another damage row and a change to the collision skill (a cooldown,
/// a target rule, ...) applied last.
fn contact_data_with(
    template: GimmickTemplate,
    area_radius: i32,
    damage: crate::systems::skills::data::effects::DamageEffect,
    tweak: impl FnOnce(&mut crate::systems::skills::data::SkillTemplate),
) -> crate::data::GameData {
    use crate::systems::skills::data::{EffectKind, EffectTemplate};
    use crate::systems::skills::tests::{skill_effect, skill_template};
    let mut data = crate::data::GameData::empty();
    let s = &mut data.systems.skills;
    s.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(damage)),
        900_001,
    );
    let mut skill = skill_template(CONTACT_SKILL);
    skill.target_type = 5; // AnyUnit
    skill.target_relation = 0; // Any
    skill.target_selection = 1; // Source: the area is centered on the first touched unit
    skill.target_area_radius = area_radius;
    skill.max_range = 0;
    // The shipped collision skills' effects start at level 1; a level-0 gimmick caster
    // would skip them all.
    let mut effect = skill_effect(900_001, EffectKind::DamageEffect, 1);
    effect.start_level = 1;
    effect.end_level = 99;
    effect.non_friendly = true; // the shipped collision effects are hostile-only
    skill.effects = vec![effect];
    tweak(&mut skill);
    s.add_skill(skill);
    data.systems
        .npcs
        .gimmick
        .templates
        .insert(template.id, template);
    data
}

/// [`contact_data`] plus a position-targeted timer skill (900_102) for the delayed-skill
/// path (`GimmickTemplate.skill_id` / `skill_delay`).
fn timer_data(template: GimmickTemplate) -> crate::data::GameData {
    use crate::systems::skills::data::{EffectKind, EffectTemplate};
    use crate::systems::skills::tests::{damage_effect, skill_effect, skill_template};
    let mut data = contact_data(template, 0);
    let s = &mut data.systems.skills;
    s.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(damage_effect(2, 100))),
        900_002,
    );
    let mut skill = skill_template(TIMER_SKILL);
    skill.target_type = 6; // Pos
    skill.target_selection = 1; // Source
    skill.target_relation = 0; // Any
    skill.target_area_radius = 3;
    skill.max_range = 5;
    let mut effect = skill_effect(900_002, EffectKind::DamageEffect, 2);
    effect.start_level = 1;
    effect.end_level = 99;
    skill.effects = vec![effect];
    s.add_skill(skill);
    data
}

struct ContactFixture {
    server: GameServer,
    world: Arc<WorldInstance>,
    client: crate::test_support::TestClient,
    victim: crate::world::PlayerRef,
}

fn contact_fixture(template: GimmickTemplate, victim_at: (f32, f32)) -> ContactFixture {
    contact_fixture_with_area(template, victim_at, 0)
}

fn contact_fixture_with_area(
    template: GimmickTemplate,
    victim_at: (f32, f32),
    area_radius: i32,
) -> ContactFixture {
    contact_fixture_with_data(contact_data(template, area_radius), victim_at)
}

fn contact_fixture_with_data(data: crate::data::GameData, victim_at: (f32, f32)) -> ContactFixture {
    let server = crate::test_support::test_server_with(data);
    // The fixture ids (player 0x100, doodad 0x300) are assigned by hand: take the object
    // ids below them first so a spawned gimmick never gets the same id.
    while server.ids.object.get_next_id().expect("object id") < 0x400 {}
    crate::systems::skills::bridges::install(&server);
    let world = test_world(&server);
    let client = crate::test_support::TestClient::new(&server, 1, 1);
    let mut c = crate::test_support::test_character(1, "Victim", victim_at.0, victim_at.1);
    c.unit.hp = 500;
    c.unit.max_hp = 1000;
    let victim = crate::test_support::spawn_player(&server, &client, c, 0x100);
    ContactFixture {
        server,
        world,
        client,
        victim,
    }
}

fn contact_template(disappear_by_collision: bool) -> GimmickTemplate {
    GimmickTemplate {
        id: CONTACT_GIMMICK,
        collision_skill_id: CONTACT_SKILL,
        collision_min_speed: 1.0,
        disappear_by_collision,
        ..Default::default()
    }
}

/// A doodad source: a prop at `at`, so it is not itself a contact target.
fn doodad_at(server: &GameServer, world: &WorldInstance, at: (f32, f32, f32)) -> WorldObject {
    let mut d = crate::models::doodad::Doodad::new(1);
    d.transform.set_xyz(at.0, at.1, at.2);
    let d = Entity::new(0x300, d);
    world.spawn(&d, &server.view_ctx());
    WorldObject::from(&d)
}

fn spawn_contact_gimmick(
    f: &ContactFixture,
    source: &WorldObject,
    velocity: [f32; 3],
) -> (GimmickRef, Arc<ActiveGimmick>) {
    let g = spawn_from_effect(
        &f.server,
        &f.world,
        &SpawnGimmickParams {
            gimmick_id: CONTACT_GIMMICK,
            offset_coordinate_id: 2,
            scale: 1.0,
            velocity_coordinate_id: 2,
            velocity_x: velocity[0],
            velocity_y: velocity[1],
            velocity_z: velocity[2],
            ..Default::default()
        },
        source,
    )
    .expect("gimmick spawned");
    let active = f
        .server
        .systems
        .npcs
        .gimmicks
        .find(g.obj_id)
        .expect("active gimmick");
    (g, active)
}

#[tokio::test]
async fn a_shared_timer_and_contact_skill_spends_the_fuse_once() {
    // A disappearing projectile can reach a unit on its fuse tick. A persistent
    // gimmick can also hit first and still be present when its fuse comes due.
    for (disappear_by_collision, life_time, skill_delay) in
        [(true, 0, 50), (true, 1_000, 50), (false, 0, 100)]
    {
        let template = GimmickTemplate {
            collision_skill_id: TIMER_SKILL,
            skill_id: TIMER_SKILL,
            skill_delay,
            life_time,
            ..contact_template(disappear_by_collision)
        };
        let f = contact_fixture_with_data(timer_data(template), (100.0, 100.0));
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let (_, active) = spawn_contact_gimmick(&f, &source, [2.0, 0.0, 0.0]);

        for now in [1_000, 1_050] {
            gimmick_tick(&f.server, &f.world, &active, Duration::from_millis(50), now);
        }

        assert_eq!(f.victim.data().unit.hp, 400, "fuse at {skill_delay} ms");
        assert_eq!(
            active.state().despawned,
            disappear_by_collision && life_time == 0
        );
    }
}

#[tokio::test]
async fn a_shared_fuse_despawns_a_disappearing_gimmick_without_a_lifetime() {
    for life_time in [0, 1_000] {
        let template = GimmickTemplate {
            collision_skill_id: TIMER_SKILL,
            skill_id: TIMER_SKILL,
            skill_delay: 100,
            life_time,
            ..contact_template(true)
        };
        let f = contact_fixture_with_data(timer_data(template), (100.0, 100.0));
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        // Keep the gimmick below the collision speed threshold so only its fuse can cast.
        let (g, active) = spawn_contact_gimmick(&f, &source, [0.0; 3]);
        let manager = &f.server.systems.npcs.gimmicks;
        let dt = Duration::from_millis(50);

        gimmick_tick(&f.server, &f.world, &active, dt, 1_000);
        assert_eq!(f.victim.data().unit.hp, 500);
        assert!(f.world.gimmick(g.obj_id).is_some());
        assert!(manager.find(g.obj_id).is_some());

        gimmick_tick(&f.server, &f.world, &active, dt, 1_050);
        assert_eq!(f.victim.data().unit.hp, 400, "the fuse deals one hit");
        assert_eq!(
            f.world.gimmick(g.obj_id).is_none(),
            life_time == 0,
            "world lookup after the fuse with lifetime {life_time}"
        );
        assert_eq!(
            manager.find(g.obj_id).is_none(),
            life_time == 0,
            "manager lookup after the fuse with lifetime {life_time}"
        );

        gimmick_tick(&f.server, &f.world, &active, dt, 1_100);
        assert_eq!(f.victim.data().unit.hp, 400, "the fuse stays spent");
    }
}

#[tokio::test]
async fn contact_gimmick_hits_a_unit_in_its_path_once() {
    let f = contact_fixture(contact_template(false), (100.0, 100.0));
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let (g, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
    let dt = Duration::from_millis(50);
    gimmick_tick(&f.server, &f.world, &active, dt, 1_000);
    assert_eq!(
        f.victim.data().unit.hp,
        400,
        "the touched unit takes the hit"
    );
    assert!((g.data().unit.transform.x - 100.25).abs() < 1e-3);
    // The same gimmick never hits the same unit again, however long it stays in reach.
    gimmick_tick(&f.server, &f.world, &active, dt, 1_050);
    gimmick_tick(&f.server, &f.world, &active, dt, 1_100);
    assert_eq!(f.victim.data().unit.hp, 400);
    // `disappear_by_collision = false`: it keeps rolling.
    assert!(f.world.gimmick(g.obj_id).is_some());
}

#[tokio::test]
async fn effect_velocity_is_authored_in_the_casters_local_frame() {
    let f = contact_fixture(contact_template(false), (300.0, 300.0));
    // A caster facing east: its local +Y (forward) is world +X.
    let mut d = crate::models::doodad::Doodad::new(1);
    d.transform.set_xyz(100.0, 100.0, 100.0);
    d.transform.rot_z = -std::f32::consts::FRAC_PI_2;
    let source = Entity::new(0x300, d);
    f.world.spawn(&source, &f.server.view_ctx());
    let g = spawn_from_effect(
        &f.server,
        &f.world,
        &SpawnGimmickParams {
            gimmick_id: CONTACT_GIMMICK,
            offset_coordinate_id: 2,
            scale: 1.0,
            velocity_coordinate_id: 2,
            velocity_x: 0.0,
            velocity_y: 5.0,
            velocity_z: 1.0,
            ang_vel_coordinate_id: 2,
            ang_vel_x: 1.0,
            ang_vel_y: 0.0,
            ang_vel_z: 2.0,
            ..Default::default()
        },
        &WorldObject::from(&source),
    )
    .expect("gimmick spawned");
    {
        let data = g.data();
        assert!((data.vel[0] - 5.0).abs() < 1e-4, "vel {:?}", data.vel);
        assert!(data.vel[1].abs() < 1e-4, "vel {:?}", data.vel);
        assert!((data.vel[2] - 1.0).abs() < 1e-4, "vel {:?}", data.vel);
        // Local +X (right) rotates to world -Y.
        assert!(data.ang_vel[0].abs() < 1e-4, "angVel {:?}", data.ang_vel);
        assert!(
            (data.ang_vel[1] + 1.0).abs() < 1e-4,
            "angVel {:?}",
            data.ang_vel
        );
        assert!(
            (data.ang_vel[2] - 2.0).abs() < 1e-4,
            "angVel {:?}",
            data.ang_vel
        );
    }
    let active = f.server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(50),
        1_000,
    );
    let t = g.data().unit.transform;
    assert!(t.x > 100.0, "launched east: x={}", t.x);
    assert!(
        (t.y - 100.0).abs() < 1e-4,
        "no north/south drift: y={}",
        t.y
    );
}

/// The world direction of a proxy's cylinder axis under a transform's rotation.
fn world_axis(t: &crate::models::transform::Transform, proxy: &CylinderProxy) -> [f32; 3] {
    t.local().to_quaternion().rotate(proxy.axis)
}

#[tokio::test]
async fn armory_barrage_barrel_falls_to_the_targets_floor() {
    let f = contact_fixture(
        GimmickTemplate {
            id: SORVELL_BARREL_TEMPLATE_ID,
            model_path: r"Objects\Env\01_nuia\001_ndeco\old\barrel_oak_b.cgf".into(),
            mass: 0.7,
            gravity: 9.8,
            life_time: 6_000,
            ..Default::default()
        },
        (140.0, 120.0),
    );
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 20.0));
    // `spawn_gimmick_effects` 105: a world-offset drop that tumbles about all three axes.
    let barrel = spawn_from_effect(
        &f.server,
        &f.world,
        &SpawnGimmickParams {
            gimmick_id: SORVELL_BARREL_TEMPLATE_ID,
            target_position: Some(PositionAndRotation::new(140.0, 120.0, 21.0, 0.0, 0.0, 0.0)),
            offset_coordinate_id: 1,
            offset_x: 2.0,
            offset_y: 2.0,
            offset_z: 20.0,
            scale: 1.0,
            velocity_coordinate_id: 2,
            velocity_z: -5.0,
            ang_vel_coordinate_id: 1,
            ang_vel_x: 5.0,
            ang_vel_y: 10.0,
            ang_vel_z: 5.0,
            ..Default::default()
        },
        &source,
    )
    .unwrap();
    let start = barrel.data().unit.transform;
    assert_eq!((start.x, start.y, start.z), (142.0, 122.0, 41.0));
    let active = f.server.systems.npcs.gimmicks.find(barrel.obj_id).unwrap();
    // No client files: the barrel_oak_b proxy, standing on the model origin.
    let proxy = OAK_BARREL_PROXY_CYLINDER;
    let mut bounced = false;
    let mut contact_clearance: f32 = 0.0;
    let mut was_falling = false;
    for timestep in 1..=100 {
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            timestep * 50,
        );
        let data = barrel.data();
        let t = data.unit.transform;
        let lowest = t.z - proxy.floor_clearance([t.rot_x, t.rot_y, t.rot_z]);
        assert!(
            lowest >= 21.0 - 1e-3,
            "the barrel's lowest point stays on the floor: {lowest} at {t:?}"
        );
        if was_falling && data.vel[2] > 0.0 {
            // A bounce: the tilted proxy touched the floor with the origin this high.
            bounced = true;
            contact_clearance = contact_clearance.max(t.z - 21.0);
        }
        was_falling = data.vel[2] < 0.0;
        assert_eq!((t.x, t.y), (142.0, 122.0), "a vertical drop does not glide");
    }
    assert!(bounced, "the first impact rebounds");
    assert!(
        contact_clearance > 0.1,
        "the tumbling barrel touches down on its rim or side, origin {contact_clearance} up"
    );
    let data = barrel.data();
    let t = data.unit.transform;
    assert_eq!(data.vel, [0.0; 3]);
    assert_eq!(data.ang_vel, [0.0; 3]);
    // At rest on an end or on its side, with that face on the floor.
    let axis = world_axis(&t, &proxy);
    let clearance = proxy.floor_clearance([t.rot_x, t.rot_y, t.rot_z]);
    assert!(
        axis[2].abs() > 0.999 || axis[2].abs() < 1e-3,
        "stable pose, axis {axis:?}"
    );
    assert!((t.z - (21.0 + clearance)).abs() < 1e-4, "{t:?}");
    let expected = if axis[2] > 0.999 {
        0.0 // upright on its base
    } else if axis[2] < -0.999 {
        2.0 * proxy.hh // upside down
    } else {
        proxy.r // on its side
    };
    assert!((clearance - expected).abs() < 1e-4, "{clearance} {axis:?}");
}

/// Every stable pose of the Armory barrel rests its proxy on the floor.
#[test]
fn barrel_rest_poses_rest_their_proxy_on_the_floor() {
    use std::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI};
    let p = OAK_BARREL_PROXY_CYLINDER;
    // Upright: base on the floor.
    assert!(p.floor_clearance([0.0, 0.0, 1.0]).abs() < 1e-5);
    // Tipped 45°: the base rim is lowest, r·sin 45° below the origin.
    let tipped = p.floor_clearance([FRAC_PI_4, 0.0, 0.0]);
    assert!((tipped - p.r * FRAC_PI_4.sin()).abs() < 1e-4, "{tipped}");
    for (rot, pose, clearance) in [
        ([0.3, -0.2, 1.0], Some([0.0, 0.0, 1.0]), 0.0),
        ([PI - 0.3, 0.2, 1.0], Some([PI, 0.0, 1.0]), 2.0 * p.hh),
        ([FRAC_PI_2 - 0.2, 0.1, 1.0], None, p.r),
    ] {
        let rest = p.rest_pose(rot);
        if let Some(pose) = pose {
            assert_eq!(rest, pose);
        }
        let q = PositionAndRotation::new(0.0, 0.0, 0.0, rest[0], rest[1], rest[2]);
        let axis = q.to_quaternion().rotate(p.axis);
        assert!(axis[2].abs() > 0.999 || axis[2].abs() < 1e-4, "{axis:?}");
        assert!(
            (p.floor_clearance(rest) - clearance).abs() < 1e-4,
            "{rest:?}"
        );
    }
    // A keg lying across its heading rests one radius up for every spin.
    for spin in [0.0, 0.7, FRAC_PI_2, 2.5] {
        let keg = RollingKeg {
            spin,
            ..RollingKeg::new(KEG_PROXY_CYLINDER, 0.4)
        };
        assert!(
            (keg.clearance() - KEG_PROXY_CYLINDER.r).abs() < 1e-4,
            "{spin}"
        );
    }
}

struct HowlingTestFloor;

impl crate::systems::ai::collision::terrain::TerrainSource for HowlingTestFloor {
    fn sample(&self, _: i32, _: i32) -> f32 {
        100.0
    }

    fn size(&self) -> (i32, i32) {
        (512, 512)
    }
}

#[tokio::test]
async fn howling_bomber_keg_falls_and_bounces_off_the_floor() {
    let f = contact_fixture(
        GimmickTemplate {
            id: 49,
            model_path: "objects/env/01_nuia/001_ndeco/old/barrel_grape_e.cgf ".into(),
            gravity: 9.8,
            life_time: 6_150,
            ..Default::default()
        },
        (150.0, 150.0),
    );
    let collision = &f.server.systems.ai.collision;
    collision
        .test_files
        .set(Arc::new(Default::default()))
        .unwrap();
    assert!(collision
        .test_terrain
        .set(Arc::new(HowlingTestFloor))
        .is_ok());
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let barrel = spawn_from_effect(
        &f.server,
        &f.world,
        &SpawnGimmickParams {
            gimmick_id: 49,
            offset_coordinate_id: 2,
            offset_z: 3.0,
            velocity_coordinate_id: 2,
            velocity_y: 8.0,
            scale: 0.7,
            ..Default::default()
        },
        &source,
    )
    .unwrap();
    let active = f.server.systems.npcs.gimmicks.find(barrel.obj_id).unwrap();
    // No client files here: the keg uses the barrel_grape_e proxy at the authored scale.
    let proxy = KEG_PROXY_CYLINDER.scaled(0.7);
    let radius = proxy.r;
    let across = |t: &crate::models::transform::Transform| {
        let axis = world_axis(t, &proxy);
        assert!(axis[2].abs() < 1e-5, "lies on its side: {axis:?}");
        assert!(
            (axis[0].abs() - 1.0).abs() < 1e-5,
            "axis across +Y: {axis:?}"
        );
    };
    across(&barrel.data().unit.transform);
    let mut bounced = false;
    let mut rolled = false;
    let mut last_spin = barrel.data().unit.transform.local().to_quaternion();
    for step in 1..=60 {
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            step * 50,
        );
        let data = barrel.data();
        across(&data.unit.transform);
        assert!(data.unit.transform.z >= 100.0 + radius - 0.02);
        bounced |= data.vel[2] > 0.0;
        if data.vel[2] == 0.0 && data.vel[1] > 0.1 {
            // Rolling forward: the turn about the cross axis follows the distance.
            rolled = true;
            assert!((data.unit.transform.z - (100.0 + radius)).abs() < 0.02);
            assert!(data.ang_vel[0] < 0.0, "angVel {:?}", data.ang_vel);
            assert!(
                (data.ang_vel[0] + data.vel[1] / radius).abs() < 1e-3,
                "angVel {:?} vel {:?}",
                data.ang_vel,
                data.vel
            );
            // The turn since the last tick is about the negative cross axis (-X), the way
            // a body rolling towards +Y turns: the sent quaternion rolls the keg forward.
            let now = data.unit.transform.local().to_quaternion();
            let d = now
                * Quaternion {
                    x: -last_spin.x,
                    y: -last_spin.y,
                    z: -last_spin.z,
                    w: last_spin.w,
                };
            let s = d.w.signum();
            assert!(d.x * s < -1e-4, "turn {d:?}");
            assert!(d.y.abs() + d.z.abs() < 0.1 * d.x.abs(), "turn {d:?}");
        }
        last_spin = data.unit.transform.local().to_quaternion();
    }
    let data = barrel.data();
    assert!(bounced);
    assert!(rolled);
    assert!(data.unit.transform.y > 105.0);
    assert!((data.unit.transform.z - (100.0 + radius)).abs() < 0.02);
}

#[tokio::test]
async fn howling_triestes_bolt_sweeps_through_units_between_ticks() {
    let f = contact_fixture(
        GimmickTemplate {
            id: 66,
            mass: 1.0,
            gravity: 9.8,
            life_time: 10_200,
            ..contact_template(true)
        },
        (103.0, 100.0),
    );
    f.victim.data().unit.transform.set_height(100.0);
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let bolt = spawn_from_effect(
        &f.server,
        &f.world,
        &SpawnGimmickParams {
            gimmick_id: 66,
            offset_coordinate_id: 2,
            velocity_coordinate_id: 2,
            velocity_x: 25.0,
            scale: 1.0,
            ..Default::default()
        },
        &source,
    )
    .unwrap();
    let active = f.server.systems.npcs.gimmicks.find(bolt.obj_id).unwrap();
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(240),
        240,
    );
    assert!(bolt.data().unit.transform.x > 105.0);
    assert!(
        active.state().despawned,
        "the bolt collided before its fuse"
    );
    assert!(f.victim.data().unit.hp < 500);
}

#[tokio::test]
async fn moving_gimmick_updates_its_world_region() {
    let f = contact_fixture(contact_template(false), (300.0, 300.0));
    let source = doodad_at(&f.server, &f.world, (63.5, 100.0, 100.0));
    let (g, active) = spawn_contact_gimmick(&f, &source, [10.0, 0.0, 0.0]);
    let me = WorldObject::from(&g);
    assert_eq!(me.region(), Some((0, 0, 1)));
    // 1 s at 10 m/s crosses the 64 m sector boundary.
    for i in 0..20 {
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000 + i * 50,
        );
    }
    assert_eq!(
        me.region(),
        Some((0, 1, 1)),
        "grid region follows the gimmick"
    );
    assert_eq!(f.world.region_of(&me), Some((1, 1)));
}

#[tokio::test]
async fn effect_gimmick_faction_comes_from_the_caster() {
    // A player-thrown bomb belongs to the player's side: the hostile-only collision effect
    // hits the hostile NPC and leaves the friendly caster alone.
    use crate::data::world_data::{FactionRelation, SystemFaction};
    use crate::systems::skills::types::RelationState;
    let faction = |id: u32, mother: u32| SystemFaction {
        id,
        mother_id: mother,
        ..SystemFaction::default()
    };
    let mut data = contact_data(contact_template(false), 5);
    for f in [faction(101, 148), faction(115, 0), faction(148, 0)] {
        data.factions.add(f);
    }
    data.factions.relations.push(FactionRelation {
        id: 148,
        id2: 115,
        state: RelationState::Hostile as u8,
        exp_time: 0,
    });
    let server = crate::test_support::test_server_with(data);
    while server.ids.object.get_next_id().expect("object id") < 0x400 {}
    crate::systems::skills::bridges::install(&server);
    let world = test_world(&server);
    let caster_client = crate::test_support::TestClient::new(&server, 2, 2);
    let mut c = crate::test_support::test_character(2, "Caster", 100.0, 100.0);
    c.unit.hp = 500;
    c.unit.max_hp = 1000;
    let caster = crate::test_support::spawn_player(&server, &caster_client, c, 0x102);
    let mut n = crate::models::npc::Npc::new(1);
    n.unit.name = "Hostile".into();
    n.unit.hp = 1000;
    n.unit.max_hp = 1000;
    n.unit.faction_id = 115;
    n.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let npc = Entity::new(0x300, n);
    world.spawn(&npc, &server.view_ctx());
    let g = spawn_from_effect(
        &server,
        &world,
        &SpawnGimmickParams {
            gimmick_id: CONTACT_GIMMICK,
            offset_coordinate_id: 2,
            scale: 1.0,
            velocity_coordinate_id: 2,
            velocity_x: 5.0,
            ..Default::default()
        },
        &WorldObject::from(&caster),
    )
    .expect("gimmick spawned");
    assert_eq!(g.data().unit.faction_id, 101, "copied from the caster");
    let active = server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
    gimmick_tick(&server, &world, &active, Duration::from_millis(50), 1_000);
    assert_eq!(npc.data().unit.hp, 900, "the hostile NPC is hit");
    assert_eq!(caster.data().unit.hp, 500, "the friendly caster is not");
}

/// The x velocity (f32 at byte 44: obj id, time, two i64 coordinates, z, the quaternion) of
/// every movement packet a client received.
fn movement_velocities(client: &mut crate::test_support::TestClient) -> Vec<f32> {
    client
        .drain()
        .into_iter()
        .filter(|(opcode, _)| *opcode == crate::opcodes::sc::SCGimmickMovementPacket)
        .map(|(_, body)| f32::from_le_bytes([body[44], body[45], body[46], body[47]]))
        .collect()
}

#[tokio::test]
async fn a_gimmick_stopping_at_the_travel_cap_broadcasts_the_stop() {
    // A moving gimmick that reaches the cap keeps its position, so only the velocity change
    // tells the clients to stop extrapolating the last non-zero velocity.
    let mut f = contact_fixture(
        GimmickTemplate {
            id: CONTACT_GIMMICK,
            ..Default::default()
        },
        (200.0, 100.0),
    );
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let (_, active) = spawn_contact_gimmick(&f, &source, [10.0, 0.0, 0.0]);
    f.client.drain();
    for i in 0..500 {
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000 + i * 50,
        );
    }
    let velocities = movement_velocities(&mut f.client);
    assert!(
        velocities.iter().any(|v| *v > 9.0),
        "the gimmick was broadcast while moving: {velocities:?}"
    );
    assert_eq!(
        velocities.last().copied(),
        Some(0.0),
        "the last packet is the stop (zero velocity)"
    );
    // Further ticks after the stop send nothing.
    for i in 0..10 {
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            40_000 + i * 50,
        );
    }
    assert!(
        movement_velocities(&mut f.client).is_empty(),
        "a stopped gimmick stays quiet"
    );
}

#[tokio::test]
async fn authored_motion_stops_after_the_travel_cap() {
    let f = contact_fixture(contact_template(false), (300.0, 300.0));
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let (g, active) = spawn_contact_gimmick(&f, &source, [10.0, 0.0, 0.0]);
    // 25 s at 10 m/s would be 250 m; the 200 m cap stops it at x = 300.
    for i in 0..500 {
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000 + i * 50,
        );
    }
    let x = g.data().unit.transform.x;
    assert!((x - 300.0).abs() < 0.6, "stopped at the 200 m cap: x={x}");
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(50),
        40_000,
    );
    assert_eq!(g.data().unit.transform.x, x, "no motion after the cap");
}

#[tokio::test]
async fn effect_gimmick_out_of_the_world_grid_despawns() {
    let f = contact_fixture(contact_template(false), (300.0, 300.0));
    // The synthetic world is 2x2 cells (2048 m): launch it past the east edge.
    let source = doodad_at(&f.server, &f.world, (2040.0, 100.0, 100.0));
    let (g, active) = spawn_contact_gimmick(&f, &source, [100.0, 0.0, 0.0]);
    for i in 0..20 {
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000 + i * 50,
        );
    }
    assert!(
        f.world.gimmick(g.obj_id).is_none(),
        "left the world grid and despawned"
    );
}

#[tokio::test]
async fn spin_only_gimmick_broadcasts_at_a_lower_rate() {
    let mut f = contact_fixture(contact_template(false), (100.0, 100.0));
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let g = spawn_from_effect(
        &f.server,
        &f.world,
        &SpawnGimmickParams {
            gimmick_id: CONTACT_GIMMICK,
            offset_coordinate_id: 2,
            scale: 1.0,
            ang_vel_coordinate_id: 2,
            ang_vel_z: 2.0,
            ..Default::default()
        },
        &source,
    )
    .expect("gimmick spawned");
    let active = f.server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
    f.client.drain();
    for i in 0..10 {
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000 + i * 50,
        );
    }
    let movements = f
        .client
        .drain()
        .into_iter()
        .filter(|(opcode, _)| *opcode == crate::opcodes::sc::SCGimmickMovementPacket)
        .count();
    assert!(
        (2..=4).contains(&movements),
        "spin broadcast rate: {movements} packets in 500 ms"
    );
    assert!(g.data().unit.transform.rot_z > 0.0, "the spin continues");
}

#[tokio::test]
async fn delayed_gimmick_skill_fires_at_the_moved_position() {
    // A barrel-bomb style gimmick: a position skill 1 s after spawn, launched 10 m/s
    // forward. The cast must happen from the gimmick's current position (10 m out), not
    // from the thrower (the old caster), whose max_range is 5.
    let template = GimmickTemplate {
        id: 901,
        skill_id: TIMER_SKILL,
        skill_delay: 1_000,
        ..Default::default()
    };
    let server = crate::test_support::test_server_with(timer_data(template));
    while server.ids.object.get_next_id().expect("object id") < 0x400 {}
    crate::systems::skills::bridges::install(&server);
    let world = test_world(&server);
    let client = crate::test_support::TestClient::new(&server, 1, 1);
    let mut c = crate::test_support::test_character(1, "Thrower", 100.0, 100.0);
    c.unit.hp = 500;
    c.unit.max_hp = 1000;
    let thrower = crate::test_support::spawn_player(&server, &client, c, 0x100);
    let victim_client = crate::test_support::TestClient::new(&server, 2, 2);
    let mut v = crate::test_support::test_character(2, "Victim", 100.0, 110.0);
    v.unit.hp = 500;
    v.unit.max_hp = 1000;
    let victim = crate::test_support::spawn_player(&server, &victim_client, v, 0x101);
    let g = spawn_from_effect(
        &server,
        &world,
        &SpawnGimmickParams {
            gimmick_id: 901,
            offset_coordinate_id: 2,
            scale: 1.0,
            velocity_coordinate_id: 2,
            velocity_y: 10.0,
            ..Default::default()
        },
        &WorldObject::from(&thrower),
    )
    .expect("gimmick spawned");
    let active = server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
    // 20 x 50 ms: the gimmick is at (100, 110) when the 1000 ms delay expires.
    for i in 0..20 {
        gimmick_tick(
            &server,
            &world,
            &active,
            Duration::from_millis(50),
            1_000 + i * 50,
        );
    }
    assert!((g.data().unit.transform.y - 110.0).abs() < 1e-3);
    assert_eq!(victim.data().unit.hp, 400, "hit at the moved position");
    assert_eq!(thrower.data().unit.hp, 500, "not hit at the spawn position");
}

#[tokio::test]
async fn contact_gimmick_does_not_latch_a_unit_merely_inside_the_area() {
    // A unit inside the skill's area but outside the contact radius is not hit and must
    // still trigger the firing when it later touches (only a firing latches units).
    let f = contact_fixture_with_area(contact_template(false), (104.0, 100.0), 5);
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let (_, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(50),
        1_000,
    );
    assert_eq!(f.victim.data().unit.hp, 500, "not hit at 4 m");
    f.victim.data().unit.transform.set_xyz(100.5, 100.0, 100.0);
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(50),
        1_050,
    );
    assert_eq!(f.victim.data().unit.hp, 400, "hit on touch");
}

#[tokio::test]
async fn contact_gimmick_keeps_the_hit_of_a_unit_the_cast_did_not_reach() {
    // The skill centres its r5 area on the touched unit (AnyUnit). Unit A touches the
    // gimmick 1 m behind it; unit B stands 6 m ahead of the gimmick, 7 m from A: inside a
    // sphere around the gimmick that is larger than the area, but outside the cast. B was not
    // hit, so it must not be latched, and its own touch later still fires the skill.
    let mut f = contact_fixture_with_area(contact_template(false), (99.0, 100.0), 5);
    let (_client_b, b) = control_player(&f, 2, (106.0, 100.0));
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let (_, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
    f.client.drain();
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(50),
        1_000,
    );
    assert_eq!(f.victim.data().unit.hp, 400, "A touched it and was hit");
    assert_eq!(b.data().unit.hp, 500, "B is outside the cast's area");
    // A walks off; B reaches the gimmick: its first touch fires (a cast centred on B, 10 m
    // from A) and A, latched and out of that area, is not hit again.
    f.victim.data().unit.transform.set_xyz(90.0, 100.0, 100.0);
    b.data().unit.transform.set_xyz(100.6, 100.0, 100.0);
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(50),
        1_050,
    );
    assert_eq!(b.data().unit.hp, 400, "B's first touch still hits");
    assert_eq!(f.victim.data().unit.hp, 400, "A keeps its single hit");
}

/// The latch is the gimmick's collision immunity (who may fire it), not a damage immunity: a
/// later firing hits every unit in its own area, a latched one included (PORTING.md, "One
/// touch fires one skill").
#[tokio::test]
async fn a_latched_unit_inside_a_later_firings_area_is_hit_by_it_again() {
    let mut f = contact_fixture_with_area(contact_template(false), (99.0, 100.0), 5);
    let (_client_b, b) = control_player(&f, 2, (106.0, 100.0));
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let (_, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
    f.client.drain();
    let dt = Duration::from_millis(50);
    gimmick_tick(&f.server, &f.world, &active, dt, 1_000);
    assert_eq!(f.victim.data().unit.hp, 400, "A touched it and was hit");
    assert_eq!(b.data().unit.hp, 500, "B is outside the cast's area");
    // A stays 1.6 m behind the gimmick, inside the area of the cast B's touch fires; A is
    // latched, so it cannot fire the skill again, but it is hit by B's firing.
    b.data().unit.transform.set_xyz(100.6, 100.0, 100.0);
    gimmick_tick(&f.server, &f.world, &active, dt, 1_050);
    assert_eq!(b.data().unit.hp, 400, "B's touch fires");
    assert_eq!(
        f.victim.data().unit.hp,
        300,
        "A, latched but inside the new area, is hit by this firing as well"
    );
    // Nothing fires again while both stay put: both are latched.
    gimmick_tick(&f.server, &f.world, &active, dt, 1_100);
    assert_eq!(b.data().unit.hp, 400);
    assert_eq!(f.victim.data().unit.hp, 300);
}

#[tokio::test]
async fn contact_gimmick_fires_one_area_cast_for_many_touching_units() {
    // Five units touch in one tick. The area skill (r5) must fire once, not once per unit:
    // each unit takes 100 damage, not 500.
    let f = contact_fixture_with_area(contact_template(false), (100.0, 100.0), 5);
    let mut clients = Vec::new();
    let mut units = vec![f.victim.clone()];
    for id in 2..=5 {
        let (client, p) = control_player(&f, id, (100.0, 100.0));
        clients.push(client);
        units.push(p);
    }
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let (_, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(50),
        1_000,
    );
    for (i, unit) in units.iter().enumerate() {
        assert_eq!(
            unit.data().unit.hp,
            400,
            "unit {i} must take exactly one area hit"
        );
    }
}

/// Another player (its own connection) for the multi-unit and control assertions.
fn control_player(
    f: &ContactFixture,
    id: u32,
    at: (f32, f32),
) -> (crate::test_support::TestClient, crate::world::PlayerRef) {
    let client = crate::test_support::TestClient::new(&f.server, id, id);
    let mut c = crate::test_support::test_character(id, &format!("Control{id}"), at.0, at.1);
    c.unit.hp = 500;
    c.unit.max_hp = 1000;
    let p = crate::test_support::spawn_player(&f.server, &client, c, 0x100 + id);
    (client, p)
}

#[tokio::test]
async fn contact_gimmick_does_not_hit_a_unit_outside_its_radius() {
    let f = contact_fixture(contact_template(false), (105.0, 100.0));
    let (_control_client, control) = control_player(&f, 2, (100.0, 100.0));
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let (_, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
    // One 50 ms tick moves it to x = 100.25, 4.75 m from the victim (> 1.5 m radius).
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(50),
        1_000,
    );
    assert_eq!(
        control.data().unit.hp,
        400,
        "the unit at the gimmick is hit"
    );
    assert_eq!(f.victim.data().unit.hp, 500);
}

#[tokio::test]
async fn contact_gimmick_hits_an_npc_when_the_skill_allows_it() {
    // The bystander player is in the gimmick's neighbourhood (a gimmick with no player
    // around is not scanned) but out of reach.
    let f = contact_fixture(contact_template(false), (110.0, 100.0));
    let mut n = crate::models::npc::Npc::new(1);
    n.unit.name = "Dummy".into();
    n.unit.hp = 1000;
    n.unit.max_hp = 1000;
    n.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let npc = Entity::new(0x300, n);
    f.world.spawn(&npc, &f.server.view_ctx());
    let source = WorldObject::from(&npc);
    let (_, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(50),
        1_000,
    );
    assert_eq!(npc.data().unit.hp, 900);
    assert_eq!(f.victim.data().unit.hp, 500);
}

#[tokio::test]
async fn contact_gimmick_skips_dead_and_invisible_units() {
    // A dead unit is not a contact target; the living control at the same spot is.
    let dead = contact_fixture(contact_template(false), (100.0, 100.0));
    dead.victim.data().unit.hp = 0;
    let (_control_client, control) = control_player(&dead, 2, (100.0, 100.0));
    let source = doodad_at(&dead.server, &dead.world, (100.0, 100.0, 100.0));
    let (_, active) = spawn_contact_gimmick(&dead, &source, [5.0, 0.0, 0.0]);
    gimmick_tick(
        &dead.server,
        &dead.world,
        &active,
        Duration::from_millis(50),
        1_000,
    );
    assert_eq!(dead.victim.data().unit.hp, 0);
    assert_eq!(control.data().unit.hp, 400);

    // A GM-invisible unit is not a contact target either; the control is.
    let hidden = contact_fixture(contact_template(false), (100.0, 100.0));
    hidden.victim.data().unit.invisible = true;
    let (_control_client, control) = control_player(&hidden, 2, (100.0, 100.0));
    let source = doodad_at(&hidden.server, &hidden.world, (100.0, 100.0, 100.0));
    let (_, active) = spawn_contact_gimmick(&hidden, &source, [5.0, 0.0, 0.0]);
    gimmick_tick(
        &hidden.server,
        &hidden.world,
        &active,
        Duration::from_millis(50),
        1_000,
    );
    assert_eq!(hidden.victim.data().unit.hp, 500);
    assert_eq!(control.data().unit.hp, 400);
}

#[tokio::test]
async fn contact_gimmick_despawns_when_the_template_says_so() {
    let f = contact_fixture(contact_template(true), (100.0, 100.0));
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let (g, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(50),
        1_000,
    );
    assert_eq!(
        f.victim.data().unit.hp,
        400,
        "the hit is cast before the despawn"
    );
    assert!(f.world.gimmick(g.obj_id).is_none());
    assert!(f.server.systems.npcs.gimmicks.find(g.obj_id).is_none());
}

#[tokio::test]
async fn elevator_without_a_collision_skill_never_hits() {
    // The fixture's victim is the living control for the contact gimmick; the rider stands
    // on the elevator, which has no template and therefore no collision skill.
    let f = contact_fixture(contact_template(false), (200.0, 200.0));
    let rider = control_player(&f, 2, (100.0, 200.0));
    let elevator = spawn_from_spawner(&f.server, &f.world, &elevator_spawner()).expect("spawned");
    let active = f
        .server
        .systems
        .npcs
        .gimmicks
        .find(elevator.obj_id)
        .unwrap();
    assert!(active.template.is_none());
    for i in 0..5 {
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000 + i * 50,
        );
    }
    assert_eq!(rider.1.data().unit.hp, 500, "the elevator rider is not hit");
    let source = doodad_at(&f.server, &f.world, (200.0, 200.0, 100.0));
    let (_, contact) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
    gimmick_tick(
        &f.server,
        &f.world,
        &contact,
        Duration::from_millis(50),
        1_400,
    );
    assert_eq!(f.victim.data().unit.hp, 400, "a contact gimmick still hits");
}

// ----- review fixes: owner, cooldowns, refused casts, caps, dynamic gimmicks -----

/// The factions of a player-thrown gimmick: the thrower's side 101 (mother 148) is hostile to
/// the NPC side 115.
fn with_thrower_factions(mut data: crate::data::GameData) -> crate::data::GameData {
    use crate::data::world_data::{FactionRelation, SystemFaction};
    use crate::systems::skills::types::RelationState;
    for (id, mother) in [(101, 148), (115, 0), (148, 0)] {
        data.factions.add(SystemFaction {
            id,
            mother_id: mother,
            ..SystemFaction::default()
        });
    }
    data.factions.relations.push(FactionRelation {
        id: 148,
        id2: 115,
        state: RelationState::Hostile as u8,
        exp_time: 0,
    });
    data
}

/// A hostile (faction 115) NPC with 1000 HP standing at `x`, y = 100.
fn hostile_npc(
    server: &GameServer,
    world: &WorldInstance,
    obj_id: u32,
    x: f32,
) -> crate::world::NpcRef {
    let mut n = crate::models::npc::Npc::new(1);
    n.unit.name = "Hostile".into();
    n.unit.hp = 1000;
    n.unit.max_hp = 1000;
    n.unit.faction_id = 115;
    n.unit.transform.set_xyz(x, 100.0, 100.0);
    let npc = Entity::new(obj_id, n);
    world.spawn(&npc, &server.view_ctx());
    npc
}

/// A player (faction 101) at (100, 100): the owner of the gimmicks its skills spawn.
fn thrower_in(server: &GameServer) -> (crate::test_support::TestClient, crate::world::PlayerRef) {
    let client = crate::test_support::TestClient::new(server, 2, 2);
    let mut c = crate::test_support::test_character(2, "Thrower", 100.0, 100.0);
    c.unit.hp = 500;
    c.unit.max_hp = 1000;
    let player = crate::test_support::spawn_player(server, &client, c, 0x102);
    (client, player)
}

/// A gimmick that keeps rolling hits every unit it touches. The collision skill's global
/// cooldown (23 of the 25 shipped ones) and its own cooldown (12006: 26 s) do not apply to a
/// gimmick caster, although `owner_character` maps a player-thrown gimmick to its thrower, and
/// the hit tags the thrower for the NPC.
#[tokio::test]
async fn a_rolling_gimmick_of_a_player_hits_each_unit_it_touches_and_tags_the_thrower() {
    let data = with_thrower_factions(contact_data_with(
        contact_template(false),
        0,
        crate::systems::skills::tests::damage_effect(1, 100),
        |s| {
            s.default_gcd = true;
            s.cooldown_time = 26_000;
        },
    ));
    let server = crate::test_support::test_server_with(data);
    while server.ids.object.get_next_id().expect("object id") < 0x400 {}
    crate::systems::skills::bridges::install(&server);
    let world = test_world(&server);
    let (_client, thrower) = thrower_in(&server);
    let first = hostile_npc(&server, &world, 0x300, 105.2);
    let second = hostile_npc(&server, &world, 0x301, 110.0);
    // 5 m in front of the thrower, rolling east at 5 m/s: the first NPC is touched on the
    // first tick, the second 14 ticks (700 ms) later.
    let g = spawn_from_effect(
        &server,
        &world,
        &SpawnGimmickParams {
            gimmick_id: CONTACT_GIMMICK,
            offset_coordinate_id: 2,
            offset_x: 5.0,
            scale: 1.0,
            velocity_coordinate_id: 2,
            velocity_x: 5.0,
            ..Default::default()
        },
        &WorldObject::from(&thrower),
    )
    .expect("gimmick spawned");
    let active = server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
    for i in 0..20 {
        gimmick_tick(
            &server,
            &world,
            &active,
            Duration::from_millis(50),
            1_000 + i * 50,
        );
    }
    assert_eq!(first.data().unit.hp, 900, "touched at once, hit once");
    assert_eq!(
        second.data().unit.hp,
        900,
        "touched 700 ms later, hit although the skill has a global cooldown and a cooldown"
    );
    {
        let d = first.data();
        let tagging = &d.unit.skill_state.tagging;
        assert_eq!(tagging.tagger, thrower.obj_id, "the thrower tags the NPC");
        assert_eq!(tagging.taggers, vec![(thrower.obj_id, 100)]);
    }
    assert_eq!(thrower.data().unit.hp, 500, "the thrower is out of reach");
}

/// A gimmick a player threw goes with its thrower. `owner_character` finds the owner through
/// the live spawner unit, so a gimmick that outlived its thrower would be ownerless and skip
/// Peace / no-PvP / siege hostility: a disconnect must not lift the protection of a player a
/// bomb is about to reach. A gimmick no character stands behind (a doodad's here) stays.
#[tokio::test]
async fn a_player_thrown_gimmick_is_removed_when_its_thrower_leaves_the_world() {
    let data = with_thrower_factions(contact_data(contact_template(false), 0));
    let server = crate::test_support::test_server_with(data);
    while server.ids.object.get_next_id().expect("object id") < 0x400 {}
    crate::systems::skills::bridges::install(&server);
    let world = test_world(&server);
    let (_client, thrower) = thrower_in(&server);
    let doodad = doodad_at(&server, &world, (200.0, 200.0, 100.0));
    let victim = hostile_npc(&server, &world, 0x301, 105.2);
    let throw = |source: &WorldObject| {
        let g = spawn_from_effect(
            &server,
            &world,
            &SpawnGimmickParams {
                gimmick_id: CONTACT_GIMMICK,
                offset_coordinate_id: 2,
                offset_x: 5.0,
                scale: 1.0,
                velocity_coordinate_id: 2,
                velocity_x: 5.0,
                ..Default::default()
            },
            source,
        )
        .expect("gimmick spawned");
        let active = server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
        (g, active)
    };
    // 5 m in front of the thrower, rolling east at 5 m/s: the first tick touches the victim.
    let (thrown, thrown_active) = throw(&WorldObject::from(&thrower));
    let (kept, kept_active) = throw(&doodad);
    assert_eq!(
        thrown_active.state().owner_character_id,
        thrower.character_id,
        "the thrower's character is recorded when the gimmick is spawned"
    );
    assert_eq!(
        kept_active.state().owner_character_id,
        0,
        "a doodad has none"
    );
    assert!(server.world.character_by_id(thrower.character_id).is_some());

    // The thrower leaves the way `leave_world` does: out of the grid, then out of the
    // character registry.
    world.remove_object(thrower.obj_id);
    assert!(server.world.try_remove_character(thrower.obj_id));

    let dt = Duration::from_millis(50);
    gimmick_tick(&server, &world, &thrown_active, dt, 1_000);
    gimmick_tick(&server, &world, &kept_active, dt, 1_000);
    assert!(
        world.gimmick(thrown.obj_id).is_none(),
        "the gimmick left the world with its thrower"
    );
    assert!(server.systems.npcs.gimmicks.find(thrown.obj_id).is_none());
    assert_eq!(
        victim.data().unit.hp,
        1000,
        "it was removed before its contact could hit as an ownerless gimmick"
    );
    assert!(world.gimmick(kept.obj_id).is_some());
    assert!(server.systems.npcs.gimmicks.find(kept.obj_id).is_some());
}

/// A refused cast leaves everything as it was: nothing is latched and a
/// `disappear_by_collision` gimmick is not removed for a cast that never ran. The skill here
/// only accepts a dead target, so every living contact is refused.
#[tokio::test]
async fn a_refused_contact_cast_latches_nothing_and_keeps_the_gimmick() {
    let data = contact_data_with(
        contact_template(true),
        0,
        crate::systems::skills::tests::damage_effect(1, 100),
        |s| {
            s.target_alive = false;
            s.target_dead = true;
        },
    );
    let f = contact_fixture_with_data(data, (100.0, 100.0));
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let (g, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(50),
        1_000,
    );
    assert_eq!(f.victim.data().unit.hp, 500, "the cast was refused");
    assert!(
        active.state().collision_hits.is_empty(),
        "a refused cast latches no unit"
    );
    assert!(
        f.world.gimmick(g.obj_id).is_some(),
        "a refused cast does not despawn the gimmick"
    );
}

/// The latch is capped; a full latch ignores further touches instead of firing the skill on
/// every tick for a unit that can no longer be latched.
#[tokio::test]
async fn a_full_contact_latch_ignores_further_touches() {
    let f = contact_fixture(contact_template(true), (100.0, 100.0));
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let (g, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
    active.state().collision_hits = (0..MAX_COLLISION_HITS as u32)
        .map(|i| 0x10_0000 + i)
        .collect();
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(50),
        1_000,
    );
    assert_eq!(f.victim.data().unit.hp, 500, "no cast after the cap");
    assert!(f.world.gimmick(g.obj_id).is_some());
    assert_eq!(active.state().collision_hits.len(), MAX_COLLISION_HITS);
}

/// r208022 has no skill 14768, 18424, 23476 or 24080, which five gimmicks name as their
/// collision skill: such a gimmick is skipped, not despawned for a cast that cannot happen.
#[tokio::test]
async fn a_gimmick_whose_collision_skill_does_not_exist_is_skipped() {
    let template = GimmickTemplate {
        collision_skill_id: 999_999,
        ..contact_template(true)
    };
    let f = contact_fixture(template, (100.0, 100.0));
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let (g, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(50),
        1_000,
    );
    assert_eq!(f.victim.data().unit.hp, 500);
    assert!(
        f.world.gimmick(g.obj_id).is_some(),
        "not despawned for nothing"
    );
    assert!(active.state().collision_hits.is_empty());
}

/// An instance nobody looks at is dormant: a gimmick with no player in its neighbourhood is
/// not scanned, and it starts hitting once a player is around.
#[tokio::test]
async fn a_contact_gimmick_with_no_player_nearby_is_not_scanned() {
    let f = contact_fixture(contact_template(false), (300.0, 300.0));
    let mut n = crate::models::npc::Npc::new(1);
    n.unit.hp = 1000;
    n.unit.max_hp = 1000;
    n.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let npc = Entity::new(0x300, n);
    f.world.spawn(&npc, &f.server.view_ctx());
    let (_, active) = spawn_contact_gimmick(&f, &WorldObject::from(&npc), [5.0, 0.0, 0.0]);
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(50),
        1_000,
    );
    assert_eq!(npc.data().unit.hp, 1000, "the only player is 280 m away");
    // The player arrives in the neighbourhood (out of reach of the gimmick).
    f.victim.data().unit.transform.set_xyz(110.0, 100.0, 100.0);
    crate::systems::ai::npc::check_moved_position(&f.server, &WorldObject::from(&f.victim));
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(50),
        1_050,
    );
    assert_eq!(npc.data().unit.hp, 900, "scanned once a player is around");
    assert_eq!(f.victim.data().unit.hp, 500);
}

/// The six `use_level_damage` rows of the collision skills (518, 542, 743, 858, 1334, 1686:
/// skills 11931, 12006, 12518, 12944, 14102, 14873) scale with the caster's `LevelDps` stat.
/// A gimmick has no stats, so such a row evaluates to 0 and lands the 1 HP minimum; level 1
/// does not change that (PORTING.md: the retail value is not known). This pins today's
/// behaviour: it changes only if gimmicks ever get stats.
#[tokio::test]
async fn a_level_scaled_damage_row_lands_the_one_hp_minimum_from_a_gimmick() {
    let mut row = crate::systems::skills::tests::damage_effect(1, 0);
    row.use_fixed_damage = false;
    row.use_level_damage = true;
    row.level_md = 40.0;
    let f = contact_fixture_with_data(
        contact_data_with(contact_template(false), 0, row, |_| {}),
        (100.0, 100.0),
    );
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let (_, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
    gimmick_tick(
        &f.server,
        &f.world,
        &active,
        Duration::from_millis(50),
        1_000,
    );
    assert_eq!(f.victim.data().unit.hp, 499, "1 HP, not a level-scaled hit");
}

/// Only a kinematic gimmick (`mass <= 0`) is launched by its authored linear velocity; a
/// dynamic one (`mass > 0`) would fly straight until the runaway cap (no gravity or ground
/// model) and stays where it spawns.
#[tokio::test]
async fn only_a_kinematic_gimmick_is_launched() {
    for (mass, launched) in [(-1.0f32, true), (0.0, true), (2.0, false)] {
        let template = GimmickTemplate {
            mass,
            ..contact_template(false)
        };
        let f = contact_fixture(template, (300.0, 300.0));
        let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
        let (g, active) = spawn_contact_gimmick(&f, &source, [5.0, 0.0, 0.0]);
        assert_eq!(active.state().authored_motion, launched, "mass {mass}");
        for i in 0..4 {
            gimmick_tick(
                &f.server,
                &f.world,
                &active,
                Duration::from_millis(50),
                1_000 + i * 50,
            );
        }
        let x = g.data().unit.transform.x;
        if launched {
            assert!((x - 101.0).abs() < 1e-3, "mass {mass}: x={x}");
        } else {
            assert!((x - 100.0).abs() < 1e-4, "mass {mass}: x={x}");
            assert!(g.data().vel.iter().all(|v| v.abs() < 1e-6));
        }
    }
}

/// A dynamic gimmick keeps its authored angular velocity (a mine spinning in place).
#[tokio::test]
async fn a_dynamic_gimmick_still_spins() {
    let template = GimmickTemplate {
        mass: 2.0,
        ..contact_template(false)
    };
    let f = contact_fixture(template, (300.0, 300.0));
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let g = spawn_from_effect(
        &f.server,
        &f.world,
        &SpawnGimmickParams {
            gimmick_id: CONTACT_GIMMICK,
            offset_coordinate_id: 2,
            scale: 1.0,
            velocity_coordinate_id: 2,
            velocity_y: 5.0,
            ang_vel_coordinate_id: 2,
            ang_vel_z: 2.0,
            ..Default::default()
        },
        &source,
    )
    .expect("gimmick spawned");
    let active = f.server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
    assert!(active.state().authored_motion);
    for i in 0..4 {
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000 + i * 50,
        );
    }
    let t = g.data().unit.transform;
    assert!(t.rot_z > 0.0, "it spins");
    assert!((t.y - 100.0).abs() < 1e-4, "it is not launched: y={}", t.y);
}

/// The 30 s time cap of authored motion (the 200 m distance cap is not reached at 1 m/s).
#[tokio::test]
async fn authored_motion_stops_after_the_time_cap() {
    let f = contact_fixture(contact_template(false), (300.0, 300.0));
    let source = doodad_at(&f.server, &f.world, (100.0, 100.0, 100.0));
    let (g, active) = spawn_contact_gimmick(&f, &source, [1.0, 0.0, 0.0]);
    for i in 0..700 {
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000 + i * 50,
        );
    }
    let x = g.data().unit.transform.x;
    assert!(
        (x - 130.0).abs() < 0.1,
        "stopped by the 30 s cap after 30 m: x={x}"
    );
    for i in 700..800 {
        gimmick_tick(
            &f.server,
            &f.world,
            &active,
            Duration::from_millis(50),
            1_000 + i * 50,
        );
    }
    assert!(
        (g.data().unit.transform.x - x).abs() < 1e-4,
        "no motion after the cap"
    );
}

/// A gimmick spawned by a doodad has no spawner unit: the C# cast its timer skill from that
/// missing unit and never fired. It is cast from the gimmick at its position.
#[tokio::test]
async fn a_doodad_spawned_gimmick_casts_its_timer_skill() {
    let template = GimmickTemplate {
        id: 901,
        skill_id: TIMER_SKILL,
        skill_delay: 1_000,
        ..Default::default()
    };
    let server = crate::test_support::test_server_with(timer_data(template));
    while server.ids.object.get_next_id().expect("object id") < 0x400 {}
    crate::systems::skills::bridges::install(&server);
    let world = test_world(&server);
    let client = crate::test_support::TestClient::new(&server, 2, 2);
    let mut v = crate::test_support::test_character(2, "Victim", 100.0, 100.0);
    v.unit.hp = 500;
    v.unit.max_hp = 1000;
    let victim = crate::test_support::spawn_player(&server, &client, v, 0x101);
    let mut d = crate::models::doodad::Doodad::new(100);
    d.transform.set_xyz(100.0, 100.0, 100.0);
    let doodad = Entity::new(0x500, d);
    let g = spawn_from_doodad(
        &server,
        &world,
        &WorldObject::from(&doodad),
        &crate::systems::npcs::doodad::templates::DoodadFuncSpawnGimmick {
            gimmick_id: 901,
            scale: 1.0,
            ..Default::default()
        },
    )
    .expect("gimmick spawned");
    assert_eq!(g.data().spawner_unit_id, 0x500, "the doodad is the spawner");
    let active = server.systems.npcs.gimmicks.find(g.obj_id).unwrap();
    for i in 0..20 {
        gimmick_tick(
            &server,
            &world,
            &active,
            Duration::from_millis(50),
            1_000 + i * 50,
        );
    }
    assert_eq!(victim.data().unit.hp, 400, "the timer skill fired");
}

#[tokio::test]
async fn unknown_template_or_origin_is_not_spawned() {
    let server = test_server();
    let world = test_world(&server);
    let s = GimmickSpawner {
        unit_id: 999_999,
        ..elevator_spawner()
    };
    assert!(spawn_from_spawner(&server, &world, &s).is_none());
    let s = GimmickSpawner {
        position: Some(Default::default()),
        ..elevator_spawner()
    };
    assert!(spawn_from_spawner(&server, &world, &s).is_none());
    let s = GimmickSpawner {
        position: None,
        ..elevator_spawner()
    };
    assert!(spawn_from_spawner(&server, &world, &s).is_none());
}

#[test]
fn time_of_day() {
    assert_eq!(time_of_day_ms(86_400_000 + 1234), 1234);
}
