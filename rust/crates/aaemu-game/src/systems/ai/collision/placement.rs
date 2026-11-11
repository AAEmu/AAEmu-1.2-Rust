//! The 1.2 client's footprint ground-unevenness rule (`FUN_393a7720` -> `FUN_393a73a0`),
//! shared by the doodad seed builder (`FUN_393a8fa0`) and the house builder (`FUN_39332180`).
//!
//! The client samples the terrain elevation (`GetTerrainElevation`: terrain only, no objects)
//! at the four rotated bottom-face corners and the centre of the ghost's render OBB, and
//! refuses when `max - min > 6.0` m (strict, `0x3999ecf4`). The server has no render OBB, so
//! the callers pass the model's collision bounds (`objects::shape_bounds`), the documented
//! proxy. Whether the render box is larger than the collision bounds (which would make the
//! proxy under-refuse) is **not verified**. A sample the terrain service answers 0 for
//! (outside its grid, x or y below 2 m, an unreadable heightmap cell) reads as height 0, like
//! the engine does, so a footprint straddling such a spot can be refused; only an unavailable
//! service (no heightmaps) skips the rule.

use aaemu_physics::geometry::primitives::BoxPrim;

use crate::server::GameServer;

/// `0x3999ecf4` (static 6.0): the client refuses a footprint spread greater than 6 m.
pub const MAX_FOOTPRINT_SPREAD: f32 = 6.0;

/// The five world-space `(x, y)` points the client samples the terrain at, in its order: the
/// four rotated bottom-face corners of `bounds` and their centroid (the client's fifth sample,
/// `* 0.25`). `bounds` is in model-local space (`objects::shape_bounds` returns an aligned box,
/// `size` holds half-extents), `pos` is the placed object's world position and `yaw` its Z
/// rotation. Pure geometry, so it is unit-tested without a server.
pub fn footprint_samples(pos: (f32, f32, f32), yaw: f32, bounds: &BoxPrim) -> [(f32, f32); 5] {
    let (sin, cos) = yaw.sin_cos();
    let rotate = |x: f32, y: f32| (x * cos - y * sin, x * sin + y * cos);
    let (cx, cy) = (bounds.center.x, bounds.center.y);
    let (hx, hy) = (bounds.size.x, bounds.size.y);
    // The client rotates each bottom-face corner `(X, Y, zmin)` by the ghost's orientation;
    // placement carries yaw only, so the rotation leaves zmin alone and does not change the
    // sampled (x, y).
    let local = [
        (cx - hx, cy + hy),
        (cx + hx, cy + hy),
        (cx + hx, cy - hy),
        (cx - hx, cy - hy),
    ];
    let mut points = [(0.0f32, 0.0f32); 5];
    let (mut sum_x, mut sum_y) = (0.0f32, 0.0f32);
    for (slot, (lx, ly)) in points.iter_mut().zip(local) {
        let (rx, ry) = rotate(lx, ly);
        let (x, y) = (pos.0 + rx, pos.1 + ry);
        sum_x += x;
        sum_y += y;
        *slot = (x, y);
    }
    points[4] = (sum_x * 0.25, sum_y * 0.25);
    points
}

/// The terrain heights at the four rotated bottom corners of `bounds` and at their centroid
/// (see [`footprint_samples`]), as `(min, max)`. `None` when the terrain service is
/// unavailable (physics off, no heightmaps, tests).
pub fn footprint_terrain_spread(
    server: &GameServer,
    instance_id: u32,
    pos: (f32, f32, f32),
    yaw: f32,
    bounds: &BoxPrim,
) -> Option<(f32, f32)> {
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for (x, y) in footprint_samples(pos, yaw, bounds) {
        let height = super::terrain_height(server, instance_id, x, y)?;
        min = min.min(height);
        max = max.max(height);
    }
    Some((min, max))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaemu_physics::math::Vec3;

    fn near(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3
    }

    fn assert_samples(got: [(f32, f32); 5], want: [(f32, f32); 5]) {
        for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
            assert!(
                near(*g, *w),
                "sample {i}: got {g:?}, want {w:?} (all: {got:?})"
            );
        }
    }

    /// No rotation, centred box: the corners are `pos +- half extents` and the fifth sample is
    /// the position itself.
    #[test]
    fn an_unrotated_centred_box_samples_its_corners_and_centre() {
        let bounds = BoxPrim::aligned(Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 2.0, 0.5));
        assert_samples(
            footprint_samples((10.0, 20.0, 5.0), 0.0, &bounds),
            [
                (9.0, 22.0),
                (11.0, 22.0),
                (11.0, 18.0),
                (9.0, 18.0),
                (10.0, 20.0),
            ],
        );
    }

    /// A quarter turn counter-clockwise (`x' = x cos - y sin`) swaps the long and short axis of
    /// a 2 x 10 box; a flipped rotation sign would mirror every corner.
    #[test]
    fn a_quarter_turn_rotates_the_corners_counter_clockwise() {
        let bounds = BoxPrim::aligned(Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 5.0, 1.0));
        assert_samples(
            footprint_samples((100.0, 200.0, 0.0), std::f32::consts::FRAC_PI_2, &bounds),
            [
                (95.0, 199.0),
                (95.0, 201.0),
                (105.0, 201.0),
                (105.0, 199.0),
                (100.0, 200.0),
            ],
        );
    }

    /// A box whose centre is off the model origin moves with the rotation (`bounds.center` is
    /// rotated like the corners): centre (3, 0) turned a quarter turn sits at (0, 3).
    #[test]
    fn an_off_centre_box_rotates_around_the_model_origin() {
        let bounds = BoxPrim::aligned(Vec3::new(3.0, 0.0, 0.0), Vec3::new(1.0, 1.0, 1.0));
        assert_samples(
            footprint_samples((0.0, 0.0, 0.0), std::f32::consts::FRAC_PI_2, &bounds),
            [(-1.0, 2.0), (-1.0, 4.0), (1.0, 4.0), (1.0, 2.0), (0.0, 3.0)],
        );
    }
}
