//! The 1.2 client's footprint ground-unevenness rule (`FUN_393a7720` -> `FUN_393a73a0`),
//! shared by the doodad seed builder (`FUN_393a8fa0`) and the house builder (`FUN_39332180`).
//!
//! The client samples the terrain elevation (`GetTerrainElevation`: terrain only, no objects)
//! at the four rotated bottom-face corners and the centre of the ghost's render OBB, and
//! refuses when `max - min > 6.0` m (strict, `0x3999ecf4`). The server has no render OBB, so
//! the callers pass the model's collision bounds (`objects::shape_bounds`), the documented
//! proxy; render bounds can be larger, so the proxy only ever under-refuses.

use aaemu_physics::geometry::primitives::BoxPrim;

use crate::server::GameServer;

/// `0x3999ecf4` (static 6.0): the client refuses a footprint spread greater than 6 m.
pub const MAX_FOOTPRINT_SPREAD: f32 = 6.0;

/// The terrain heights at the four rotated bottom corners of `bounds` and at their centroid,
/// as `(min, max)`. `bounds` is in model-local space (`objects::shape_bounds` returns an
/// aligned box) and `pos` is the placed object's world position; `yaw` is its Z rotation.
/// `None` when the terrain service is unavailable (physics off, no heightmaps, tests).
pub fn footprint_terrain_spread(
    server: &GameServer,
    instance_id: u32,
    pos: (f32, f32, f32),
    yaw: f32,
    bounds: &BoxPrim,
) -> Option<(f32, f32)> {
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
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let (mut sum_x, mut sum_y) = (0.0f32, 0.0f32);
    for (lx, ly) in local {
        let (rx, ry) = rotate(lx, ly);
        let x = pos.0 + rx;
        let y = pos.1 + ry;
        sum_x += x;
        sum_y += y;
        let height = super::terrain_height(server, instance_id, x, y)?;
        min = min.min(height);
        max = max.max(height);
    }
    // The client's fifth sample is the average of the four corner positions (`* 0.25`).
    let centre = super::terrain_height(server, instance_id, sum_x * 0.25, sum_y * 0.25)?;
    Some((min.min(centre), max.max(centre)))
}
