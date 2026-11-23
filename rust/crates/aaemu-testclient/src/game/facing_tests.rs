use super::*;

#[test]
fn movement_faces_each_compass_target_using_the_1_2_yaw() {
    let origin = (0.0, 0.0, 0.0);
    for (target, direction) in [
        ((0.0, 1.0, 0.0), 0),
        ((-1.0, 0.0, 0.0), 32),
        ((1.0, 0.0, 0.0), -32),
        ((0.0, -1.0, 0.0), 64),
    ] {
        assert_eq!(facing_direction(origin, target), direction);
    }
}
