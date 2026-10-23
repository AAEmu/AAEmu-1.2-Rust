//! The convenience API ([`aaemu_physics::world::api`]) reproduces the scenario goldens.

use aaemu_physics::geometry::primitives::{BoxPrim, Primitive};
use aaemu_physics::math::{Quat, Vec3};
use aaemu_physics::oracle::check_scenario_golden;
use aaemu_physics::trace::Trace;
use aaemu_physics::world::api::PartDesc;
use aaemu_physics::world::{EntIdx, World};

fn boxp(h: f32) -> Primitive {
    Primitive::Box(BoxPrim {
        size: Vec3::new(h, h, h),
        ..BoxPrim::default()
    })
}

#[test]
fn box_on_ground_through_the_api() {
    let mut w = World::with_grid(Vec3::new(0.0, 0.0, -100.0), 64, 4.0);
    let ground = w
        .geometry(&Primitive::Box(BoxPrim {
            size: Vec3::new(50.0, 50.0, 0.5),
            ..BoxPrim::default()
        }))
        .unwrap();
    let cube = w.geometry(&boxp(0.5)).unwrap();
    w.add_static(
        Vec3::new(32.0, 32.0, -0.5),
        Quat::IDENTITY,
        &[PartDesc::new(ground, 0.0)],
        -1,
    );
    let b = w
        .add_rigid(
            Vec3::new(32.0, 32.0, 5.0),
            Quat::IDENTITY,
            &[PartDesc::new(cube, 10.0)],
            -1,
        )
        .unwrap();
    let dt = f32::from_bits(0x3d08_8889);
    let golden = Trace::parse(&check_scenario_golden("box_on_ground").unwrap()).unwrap();
    for n in 1..=90u32 {
        w.step(dt);
        let s = w.body_state(b);
        let g = golden
            .entity_states("box")
            .into_iter()
            .find(|(k, _)| *k == n)
            .unwrap()
            .1
            .clone();
        assert_eq!(s.pos.to_bits(), g.pos_f().to_bits(), "step {n}");
        assert_eq!(s.q.to_bits(), g.q_f().to_bits(), "step {n}");
    }
}

#[test]
fn floating_box_through_the_api() {
    let mut w = World::with_grid(Vec3::new(0.0, 0.0, -100.0), 64, 4.0);
    let ground = w
        .geometry(&Primitive::Box(BoxPrim {
            size: Vec3::new(50.0, 50.0, 0.5),
            ..BoxPrim::default()
        }))
        .unwrap();
    let cube = w.geometry(&boxp(0.5)).unwrap();
    w.add_static(
        Vec3::new(32.0, 32.0, -20.0),
        Quat::IDENTITY,
        &[PartDesc::new(ground, 0.0)],
        -1,
    );
    let b = w
        .add_rigid(
            Vec3::new(32.0, 32.0, 5.0),
            Quat::IDENTITY,
            &[PartDesc::new(cube, 300.0)],
            -1,
        )
        .unwrap();
    w.set_ocean(2.0, 1000.0, 1000.0);
    w.set_body_buoyancy(b, 1000.0, 1000.0);
    let golden = Trace::parse(&check_scenario_golden("box_float_300kg").unwrap()).unwrap();
    for n in 1..=300u32 {
        w.step(f32::from_bits(0x3d08_8889));
        let s = w.body_state(b);
        let g = golden
            .entity_states("box")
            .into_iter()
            .find(|(k, _)| *k == n)
            .unwrap()
            .1
            .clone();
        assert_eq!(s.pos.to_bits(), g.pos_f().to_bits(), "step {n}");
    }
}

fn static_contact_world() -> (World, EntIdx, EntIdx) {
    let mut w = World::with_grid(Vec3::new(0.0, 0.0, -100.0), 64, 4.0);
    let ground = w
        .geometry(&Primitive::Box(BoxPrim {
            size: Vec3::new(10.0, 10.0, 0.5),
            ..BoxPrim::default()
        }))
        .unwrap();
    let cube = w.geometry(&boxp(0.5)).unwrap();
    let floor = w
        .add_static(
            Vec3::new(32.0, 32.0, -0.5),
            Quat::IDENTITY,
            &[PartDesc::new(ground, 0.0)],
            -1,
        )
        .unwrap();
    let body = w
        .add_rigid(
            Vec3::new(32.0, 32.0, 1.0),
            Quat::IDENTITY,
            &[PartDesc::new(cube, 10.0)],
            -1,
        )
        .unwrap();
    let dt = 1.0 / 60.0;
    for _ in 0..180 {
        w.step(dt);
        if aaemu_physics::entities::has_contacts_with(&w, body, floor) {
            break;
        }
    }
    assert!(
        w.ent(body).colliders.contains(&floor)
            && aaemu_physics::entities::has_contacts_with(&w, body, floor),
        "the normal contact solver must register the static collider"
    );
    (w, floor, body)
}

fn assert_live_rigid_contacts(w: &World, body: EntIdx) {
    let ent = w.ent(body);
    let rigid = ent.rigid().unwrap();
    assert_eq!(ent.colliders.len(), rigid.collider_contacts.len());
    assert_eq!(ent.colliders.len(), rigid.collider_masks.len());
    assert!(ent.colliders.iter().all(|c| w.ents[c.0 as usize].is_some()));
    let state = w.body_state(body);
    assert!(state.pos.x.is_finite() && state.pos.y.is_finite() && state.pos.z.is_finite());
}

#[test]
fn sleeping_rigid_survives_static_collider_cleanup_and_wake() {
    let (mut w, floor, body) = static_contact_world();
    let dt = 1.0 / 60.0;
    aaemu_physics::entities::awake(&mut w, body, 0, 1);
    assert_eq!(w.body_state(body).sim_class, 1);
    assert!(!w.body_state(body).awake);

    assert_eq!(w.destroy_physical_entity(floor, 0), 1);
    for _ in 0..8 {
        w.step(dt); // includes PumpLoggedEvents and normal deferred entity cleanup
        if w.ents[floor.0 as usize].is_none() {
            break;
        }
    }
    assert!(w.ents[floor.0 as usize].is_none(), "static was reclaimed");

    // A sleeping rigid used to retain the freed static index. Awake's collider walk then
    // panicked at World::ent's `expect("live entity")`, just like the live ship failure.
    aaemu_physics::entities::awake(&mut w, body, 1, 1);
    for _ in 0..8 {
        w.step(dt);
        assert_live_rigid_contacts(&w, body);
    }
}

#[test]
fn surviving_rigid_recontacts_repeated_static_replacements_after_another_user_is_freed() {
    let (mut w, mut floor, first) = static_contact_world();
    let ground = w.ent(floor).parts[0].pgeom.clone();
    let cube = w.ent(first).parts[0].pgeom.clone();
    let survivor = w
        .add_rigid(
            Vec3::new(36.0, 32.0, 1.0),
            Quat::IDENTITY,
            &[PartDesc::new(cube, 10.0)],
            -1,
        )
        .unwrap();
    let dt = 1.0 / 60.0;
    for _ in 0..180 {
        w.step(dt);
        if aaemu_physics::entities::has_contacts_with(&w, survivor, floor) {
            break;
        }
    }
    for body in [first, survivor] {
        assert!(w.ent(body).colliders.contains(&floor));
        assert!(aaemu_physics::entities::has_contacts_with(&w, body, floor));
        aaemu_physics::entities::awake(&mut w, body, 0, 1);
        assert!(!w.body_state(body).awake);
    }

    // Removing one user must unregister it before the static's later destruction tries
    // to detach or wake its surviving users. Reclaim through the ordinary cleanup path.
    assert_eq!(w.destroy_physical_entity(first, 0), 1);
    for _ in 0..8 {
        w.step(dt);
    }
    assert!(w.ents[first.0 as usize].is_none());
    assert_live_rigid_contacts(&w, survivor);

    for _ in 0..4 {
        let old = floor;
        assert_eq!(w.destroy_physical_entity(old, 0), 1);
        assert!(
            w.body_state(survivor).awake,
            "lost support wakes the sleeper"
        );
        floor = w
            .add_static(
                Vec3::new(32.0, 32.0, -0.5),
                Quat::IDENTITY,
                &[PartDesc::new(ground.clone(), 0.0)],
                -1,
            )
            .unwrap();
        for _ in 0..180 {
            w.step(dt);
            assert_live_rigid_contacts(&w, survivor);
            if w.ents[old.0 as usize].is_none()
                && aaemu_physics::entities::has_contacts_with(&w, survivor, floor)
            {
                break;
            }
        }
        assert!(w.ents[old.0 as usize].is_none());
        assert!(w.ent(survivor).colliders.contains(&floor));
        assert!(aaemu_physics::entities::has_contacts_with(
            &w, survivor, floor
        ));
        aaemu_physics::entities::awake(&mut w, survivor, 0, 1);
        assert!(!w.body_state(survivor).awake);
    }

    // Also exercise the explicit contact removal path before the last user is freed.
    assert!(aaemu_physics::entities::remove_collider(&mut w, survivor, floor, true) >= 0);
    assert_live_rigid_contacts(&w, survivor);
    assert_eq!(w.destroy_physical_entity(survivor, 0), 1);
    for _ in 0..8 {
        w.step(dt);
    }
    assert!(w.ents[survivor.0 as usize].is_none());
    assert_eq!(w.destroy_physical_entity(floor, 0), 1);
    for _ in 0..8 {
        w.step(dt);
    }
    assert!(w.ents[floor.0 as usize].is_none());
}
