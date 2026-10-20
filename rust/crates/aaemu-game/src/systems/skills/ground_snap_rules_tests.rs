//! Tests of [`super`], ported from the C# `GroundSnapRulesTests` of commit `c78b3a659`.
//!
//! move_to_ground (type 73) lands the unit on the floor, through the shared snap policy. No
//! heightmap sample means no floor, and the unit must not be moved.

use super as ground_snap;

#[test]
fn no_heightmap_there_is_no_floor_to_move_to() {
    // Heightmaps off (or a missing cell) answer 0. Moving a unit there would drop it into the sea.
    assert_eq!(ground_snap::try_get_floor_z(120.0, 0.0, false, 0.0), None);
    assert_eq!(
        ground_snap::choose_unit_floor_z(120.0, 0.0, false, 0.0),
        120.0
    );
}

#[test]
fn ground_under_the_unit_snaps_with_clearance() {
    let z = ground_snap::try_get_floor_z(100.5, 100.0, false, 0.0).unwrap();
    assert!((z - (100.0 + ground_snap::CLEARANCE_METRES)).abs() < 0.001);
}

#[test]
fn unit_buried_in_terrain_is_lifted_onto_the_floor() {
    let z = ground_snap::try_get_floor_z(95.0, 100.0, false, 0.0).unwrap();
    assert!((z - (100.0 + ground_snap::CLEARANCE_METRES)).abs() < 0.001);
}

#[test]
fn over_water_the_surface_wins_over_the_seabed() {
    // Open ocean samples the seabed; dropping a teleporting character onto it is visibly wrong.
    let z = ground_snap::try_get_floor_z(120.0, 60.0, true, 100.0).unwrap();
    assert!((z - (100.0 + ground_snap::CLEARANCE_METRES)).abs() < 0.001);
}

#[test]
fn water_that_is_not_above_the_seabed_does_not_win() {
    // The surface has to clear the seabed by more than a metre.
    let z = ground_snap::try_get_floor_z(60.5, 60.0, true, 60.5).unwrap();
    assert!((z - (60.0 + ground_snap::CLEARANCE_METRES)).abs() < 0.001);
}

#[test]
fn a_wild_drop_keeps_the_raw_z() {
    // Further than the drop cap: a bad sample or a cliff, and the raw Z is kept rather than trusted.
    let z = ground_snap::try_get_floor_z(200.0, 100.0, false, 0.0).unwrap();
    assert_eq!(z, 200.0);
}

#[test]
fn a_wild_lift_keeps_the_raw_z() {
    // Further than the lift cap: the sample is a cliff lip, not a floor under the unit.
    let z = ground_snap::try_get_floor_z(100.0, 200.0, false, 0.0).unwrap();
    assert_eq!(z, 100.0);
}
