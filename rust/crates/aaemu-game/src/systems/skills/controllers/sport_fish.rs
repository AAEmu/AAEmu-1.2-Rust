use crate::server::GameServer;
use crate::world::WorldObject;

use super::super::data::SkillControllerTemplate;
use super::super::geometry::{world_of, Target};
use super::motion::duration_sec;
use super::{
    add_distance_to_front_between, calculate_distance, leap_animation_speed, positions, Behavior,
    LeapArc, LinearMove, MoveDirection, Vec3,
};

/// The 1.2 hooked-fish plots encode swims with Leap and reel pulls with a negative Dash
/// offset. Resolve those motions in the angler's frame, with depth measured from the
/// water surface instead of compounding a relative dive on every cue.
pub(super) fn sport_fish_movement(
    server: &GameServer,
    template: &SkillControllerTemplate,
    fish: &WorldObject,
    target: &Target,
) -> Option<Behavior> {
    let id = template.id;
    let reel = matches!(id, 7064..=7065 | 7114..=7121);
    let jump = id == 7040;
    if !reel && !jump && !matches!(id, 7008..=7011 | 7127..=7129) {
        return None;
    }
    let npc = fish.as_npc()?;
    let angler_id = crate::systems::economy::fishing::owned_sport_fish_owner(server, npc)?;
    let angler = WorldObject::from(server.world.character_by_id(angler_id)?);
    let world = world_of(server, fish)?;
    if angler.transform().instance_id != world.id
        || (reel && !target.as_obj().is_some_and(|obj| obj.same(&angler)))
    {
        return None;
    }
    let (start, destination) = positions(server, fish, target);
    let p = world.world_pos_rot(&angler);
    let origin = Vec3::new(p.x, p.y, start.z);
    let range = calculate_distance(origin, start, false);
    let away = if range > 0.001 {
        Vec3::new(
            (start.x - origin.x) / range,
            (start.y - origin.y) / range,
            0.0,
        )
    } else {
        Vec3::new(-p.rot_z.sin(), p.rot_z.cos(), 0.0)
    };
    let side = match id {
        7008 | 7127 => 80.0_f32.to_radians(),
        7009 | 7128 => -80.0_f32.to_radians(),
        _ => 0.0,
    };
    let distance = if reel {
        // Negative offsets in the reel rows mean pulling the fish back toward the rod.
        -(range - 2.0)
            .max(0.0)
            .min(template.value[3].unsigned_abs() as f32 / 1000.0)
    } else if jump || matches!(id, 7011 | 7129) {
        template.value[3].unsigned_abs() as f32 / 1000.0
    } else {
        calculate_distance(start, destination, false)
    };
    let (sin, cos) = side.sin_cos();
    let (dx, dy) = if jump {
        let yaw = world.world_pos_rot(fish).rot_z;
        (-yaw.sin(), yaw.cos())
    } else {
        (away.x * cos + away.y * sin, away.y * cos - away.x * sin)
    };
    let mut end = Vec3::new(start.x + dx * distance, start.y + dy * distance, start.z);
    // Start casts can reach 30 m. A fish can use the line available to it without
    // extending it indefinitely. Keep existing slack if the angler has moved.
    let limit = range.max(30.0);
    let end_range = calculate_distance(origin, end, false);
    if end_range > limit {
        if jump {
            // A jump clipped near the end of the line can be shorter than one tick.
            // Turn back toward the angler to keep enough room to reach the surface.
            end.x = start.x - away.x * distance;
            end.y = start.y - away.y * distance;
        } else {
            end = add_distance_to_front_between(limit, end_range, origin, end);
        }
    }
    let probe_z = npc.data().spawner_z.unwrap_or(start.z).max(start.z);
    let probe = Vec3::new(end.x, end.y, probe_z);
    let (surface, _) = crate::systems::ai::physics::water_surface(server, world.id, probe)?;
    let floor = crate::systems::npcs::heightmap::world_height(server, &world, end.x, end.y)
        .unwrap_or(f32::NEG_INFINITY);
    if floor < surface {
        let depth = match id {
            7010 => 8.0,
            7008 | 7009 | 7127 | 7128 => 2.0,
            7011 | 7129 => (surface - start.z).clamp(0.2, 8.0),
            _ => 0.2,
        };
        end.z = (surface - depth).max((floor + 0.2).min(surface - 0.2));
        if jump {
            // The bounded destination already incorporates the authored offset.
            let mut config = template.clone();
            config.value[3] = 0;
            // Finish the last partial step at the bounded destination.
            config.value[7] = 0;
            let arc = LeapArc::new(&config, start, end, None, leap_animation_speed(fish))?;
            return Some(Behavior::BallisticLeap(arc));
        }
    } else {
        end = start;
    }
    let duration = template.value[2];
    Some(Behavior::FishSwim(LinearMove {
        duration,
        distance_offset: template.value[3],
        direction: MoveDirection::Both,
        end_position: end,
        calculated_speed: calculate_distance(start, end, true) / duration_sec(duration),
    }))
}
