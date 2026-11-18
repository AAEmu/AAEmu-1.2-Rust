//! Wall check for the straight walks of NPC AI (roaming, returning home, authored paths).
//!
//! The AI moves NPCs in straight lines and only fixes their Z, so a roam target behind a wall
//! (or a way home through a building) walked the NPC through it. The check is one horizontal
//! ray against static geometry (mask 0x001, no terrain) half a metre above the ground, from
//! the start to the target with the Z interpolated between the two ground heights:
//!
//! * terrain is not part of the mask, so slopes and hills never block;
//! * a hit on a floor-like surface (`|normal.z| > ` [`FLOOR_NORMAL_Z`]: ramps, stairs treads,
//!   floors) is not a wall, the ray continues behind it;
//! * the ray measured 4-10 times cheaper than `displacement::clip_move` (capsule blink) and
//!   blocked fewer walks in open terrain, where the blink stops at steep terrain steps the NPC
//!   walks over.
//!
//! `None` from any function means the collision service is off: the caller keeps the C#
//! behaviour (straight walk).

use crate::server::GameServer;

use super::query::{mask, ray_flags};
use super::terrain::INV_UNIT_SIZE;

/// Height above the ground of the probe ray (m).
pub const WALK_PROBE_HEIGHT: f32 = 0.5;
/// A hit surface with `|normal.z|` above this is a floor / ramp / ceiling, not a wall (~45 deg).
pub const FLOOR_NORMAL_Z: f32 = 0.7;
/// Floor-like hits the ray skips before the walk is reported blocked anyway.
const MAX_FLOOR_HITS: usize = 4;

/// The horizontal distance (m) from `from` to the first wall on the straight walk to `to`
/// (feet positions, ground Z at both ends). `None` without the collision service,
/// `Some(None)` when the walk is clear.
pub fn wall_distance(
    server: &GameServer,
    instance_id: u32,
    from: [f32; 3],
    to: [f32; 3],
) -> Option<Option<f32>> {
    let mut a = [from[0], from[1], from[2] + WALK_PROBE_HEIGHT];
    let b = [to[0], to[1], to[2] + WALK_PROBE_HEIGHT];
    let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2) + (b[2] - a[2]).powi(2)).sqrt();
    if len < 1e-3 {
        return super::phys_world(server, instance_id).map(|_| None);
    }
    let dir = [
        (b[0] - a[0]) / len,
        (b[1] - a[1]) / len,
        (b[2] - a[2]) / len,
    ];
    for _ in 0..=MAX_FLOOR_HITS {
        let hit = super::raycast(server, instance_id, a, b, mask::STATIC, ray_flags::DEFAULT)?;
        let Some(h) = hit else {
            return Some(None);
        };
        if h.normal.z.abs() <= FLOOR_NORMAL_Z {
            let d = ((h.pos.x - from[0]).powi(2) + (h.pos.y - from[1]).powi(2)).sqrt();
            return Some(Some(d));
        }
        // Step over the floor-like surface and go on.
        a = [
            h.pos.x + dir[0] * 0.05,
            h.pos.y + dir[1] * 0.05,
            h.pos.z + dir[2] * 0.05,
        ];
        let rest =
            ((b[0] - a[0]) * dir[0] + (b[1] - a[1]) * dir[1] + (b[2] - a[2]) * dir[2]).max(0.0);
        if rest < 0.05 {
            return Some(None);
        }
    }
    // Too many floor-like surfaces in a row: a stack of floors in the way, treat as blocked at
    // the last hit.
    Some(Some(
        ((a[0] - from[0]).powi(2) + (a[1] - from[1]).powi(2)).sqrt(),
    ))
}

/// [`wall_distance`] as a yes/no; `true` when a wall is in the way.
pub fn walk_blocked(
    server: &GameServer,
    instance_id: u32,
    from: [f32; 3],
    to: [f32; 3],
) -> Option<bool> {
    wall_distance(server, instance_id, from, to).map(|d| d.is_some())
}

/// Prove that an NPC's short step has continuous terrain support. Inspect the full
/// bounding rectangle of terrain cells, including cells the step only clips at a corner.
/// Routes that rely on raised floors or bridge terrain holes use authored navigation.
pub(crate) fn terrain_supports_step(
    server: &GameServer,
    instance_id: u32,
    from: [f32; 3],
    to: [f32; 3],
) -> bool {
    let Some(world) = super::phys_world(server, instance_id) else {
        return false;
    };
    let Some(terrain) = world.terrain.as_ref() else {
        return false;
    };
    let grid = |v: f32| (v * INV_UNIT_SIZE).floor() as i32;
    let (x0, x1) = (grid(from[0].min(to[0])), grid(from[0].max(to[0])));
    let (y0, y1) = (grid(from[1].min(to[1])), grid(from[1].max(to[1])));
    let (nx, ny) = terrain.size();
    if x0 < 0 || y0 < 0 || x1 >= nx || y1 >= ny {
        return false;
    }
    // Terrain height is interpolated from the corners of each cell. Every corner
    // must be in the same height band as the two nearby collision floor probes.
    let heights = (from[2].min(to[2]) - 1.0)..=(from[2].max(to[2]) + 1.0);
    (x0..=x1).all(|x| (y0..=y1).all(|y| !terrain.is_hole(x, y)))
        && (x0..=x1 + 1).all(|x| {
            (y0..=y1 + 1).all(|y| {
                let z = terrain.sample(x, y);
                z > 0.0 && heights.contains(&z)
            })
        })
}
