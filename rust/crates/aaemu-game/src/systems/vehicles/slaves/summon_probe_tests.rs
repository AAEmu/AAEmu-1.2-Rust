use aaemu_physics::geometry::primitives::BoxPrim;
use aaemu_physics::math::Vec3;

use super::{fallback_summon_point, hull_footprint, hull_probe_points, overhead_clear};

// The authored proxy unions of the 1.2 assets from `cry::tests`
// (`create_entity_builds_the_clipped_hull_for_longliner_and_cutter`).
fn longliner() -> (BoxPrim, f32, f32) {
    (
        BoxPrim::aligned(
            Vec3::new(0.0, 1.935_895_9, 3.577_293_2),
            Vec3::new(4.577_419, 14.026_104, 5.289_681_4),
        ),
        5.0,
        11.0,
    )
}

fn cutter() -> (BoxPrim, f32, f32) {
    (
        BoxPrim::aligned(
            Vec3::new(0.000_734_329_2, 5.385_618, 17.994_78),
            Vec3::new(5.960_227, 22.946_87, 23.838_594),
        ),
        6.5,
        30.0,
    )
}

/// (along the yaw's length axis, lateral right of it) of `p` from `origin`.
fn project(p: (f32, f32), origin: (f32, f32), yaw: f32) -> (f32, f32) {
    let (dx, dy) = (p.0 - origin.0, p.1 - origin.1);
    (
        dx * -yaw.sin() + dy * yaw.cos(),
        dx * yaw.cos() + dy * yaw.sin(),
    )
}

#[test]
fn probes_lie_on_the_authored_length_axis_at_the_final_ship_yaw() {
    let origin = (0.0, 0.0);
    // Player yaw 0: the ship's final yaw is +90 degrees, so the hull's length axis lies on
    // world X (the player's facing would put it on Y).
    let final_yaw = std::f32::consts::FRAC_PI_2;
    for (name, (bounds, beam, length)) in [("Longliner", longliner()), ("Cutter", cutter())] {
        let fp = hull_footprint(Some(&bounds), beam, length, 1.0);
        assert!((fp.half_beam - bounds.size.x).abs() < 1e-5, "{name}");
        let authored = (
            bounds.center.y - bounds.size.y,
            bounds.center.y + bounds.size.y,
        );
        assert!(
            (fp.length.0 - authored.0).abs() < 1e-4,
            "{name}: {:?}",
            fp.length
        );
        assert!(
            (fp.length.1 - authored.1).abs() < 1e-4,
            "{name}: {:?}",
            fp.length
        );
        // Asymmetric: the authored bow/stern extents are not the mass box's half length.
        assert!(
            fp.length.0 < -length * 0.5 && fp.length.1 > length * 0.5,
            "{name}: {:?}",
            fp.length
        );
        let points = hull_probe_points(origin.0, origin.1, final_yaw, fp);
        for (i, p) in points.iter().enumerate() {
            let (along, lateral) = project(*p, origin, final_yaw);
            let expected_along = [fp.length.0, fp.mid_length(), fp.length.1][i / 3];
            let expected_lateral = if i % 3 == 0 { 0.0 } else { fp.half_beam };
            assert!(
                (along - expected_along).abs() < 1e-4,
                "{name} point {i}: along {along}, expected {expected_along}"
            );
            assert!(
                (lateral.abs() - expected_lateral).abs() < 1e-4,
                "{name} point {i}: lateral {lateral}, expected {expected_lateral}"
            );
            // Final yaw +90 means the length axis is world X: these are just the hull
            // extents mirrored by the forward direction.
            assert!(
                (p.1 - (if i % 3 == 1 {
                    -fp.half_beam
                } else if i % 3 == 2 {
                    fp.half_beam
                } else {
                    0.0
                }))
                .abs()
                    < 1e-4,
                "{name} point {i}: {p:?}"
            );
            assert!(
                (p.0 + expected_along).abs() < 1e-4,
                "{name} point {i}: {p:?}"
            );
        }
    }
}

#[test]
fn probes_follow_the_rotated_final_yaw() {
    let player_yaw = 0.3;
    let final_yaw = player_yaw + std::f32::consts::FRAC_PI_2;
    let origin = (120.0, -40.0);
    let fp = hull_footprint(Some(&longliner().0), 5.0, 11.0, 1.0);
    for (i, p) in hull_probe_points(origin.0, origin.1, final_yaw, fp)
        .iter()
        .enumerate()
    {
        let (along, lateral) = project(*p, origin, final_yaw);
        let expected_along = [fp.length.0, fp.mid_length(), fp.length.1][i / 3];
        assert!((along - expected_along).abs() < 1e-3, "point {i}");
        assert!(
            (lateral.abs() - if i % 3 == 0 { 0.0 } else { fp.half_beam }).abs() < 1e-3,
            "point {i}"
        );
        // Probing along the player's facing (90 degrees off) would not hit these points.
        let (_, player_lateral) = project(*p, origin, player_yaw);
        assert!(
            (player_lateral.abs() - lateral.abs()).abs() > 0.1,
            "point {i} is ambiguous between the player's and the ship's axes"
        );
    }
}

#[test]
fn without_authored_bounds_the_probes_use_the_mass_box() {
    let fp = hull_footprint(None, 5.0, 11.0, 1.0);
    assert_eq!(fp.half_beam, 2.5);
    assert_eq!(fp.length, (-5.5, 5.5));
    let scaled = hull_footprint(None, 5.0, 11.0, 2.0);
    assert_eq!(scaled.half_beam, 5.0);
    assert_eq!(scaled.length, (-11.0, 11.0));
}

#[test]
fn overhead_structures_above_the_water_block_the_spot() {
    let water = 100.0;
    // Open water (no hit) and no collision service are clear.
    assert!(overhead_clear(Some(None), water));
    assert!(overhead_clear(None, water));
    // A pier deck, bridge or deck above the surface blocks the spot.
    assert!(!overhead_clear(Some(Some(103.8)), water));
    assert!(!overhead_clear(Some(Some(100.1)), water));
    // The surface itself (inside the stop margin) does not.
    assert!(overhead_clear(Some(Some(100.0)), water));
}

#[test]
fn the_middle_of_the_long_sides_is_probed() {
    let fp = hull_footprint(Some(&cutter().0), 6.5, 30.0, 1.0);
    let points = hull_probe_points(0.0, 0.0, std::f32::consts::FRAC_PI_2, fp);
    let mid = fp.mid_length();
    // Stations: stern, midpoint, bow; each centreline + both beam edges.
    for (i, p) in points.iter().enumerate() {
        let (along, lateral) = project(*p, (0.0, 0.0), std::f32::consts::FRAC_PI_2);
        let expected_along = [fp.length.0, mid, fp.length.1][i / 3];
        assert!((along - expected_along).abs() < 1e-4, "point {i}");
        let expected_lateral = if i % 3 == 0 { 0.0 } else { fp.half_beam };
        assert!((lateral.abs() - expected_lateral).abs() < 1e-4, "point {i}");
    }
    // The mid station is at the hull's longitudinal middle, not at an end.
    assert!((points[3].0 + mid).abs() < 1e-4);
}

#[test]
fn fallback_uses_the_water_surface_not_the_owner_height() {
    let pier_z = 112.0;
    let water = 100.0;
    let (x, y, z) = fallback_summon_point(100.0, 200.0, 0.0, |qx, qy| {
        assert_eq!((qx, qy), (100.0, 205.0));
        water
    });
    assert_eq!(z, water);
    assert_ne!(z, pier_z);
    assert!((x - 100.0).abs() < 1e-4 && (y - 205.0).abs() < 1e-4);
    // Turned player: the fallback still follows the player's facing.
    let (x, y, _) = fallback_summon_point(100.0, 200.0, std::f32::consts::FRAC_PI_2, |_, _| water);
    assert!((x - 95.0).abs() < 1e-4 && (y - 200.0).abs() < 1e-4);
}
