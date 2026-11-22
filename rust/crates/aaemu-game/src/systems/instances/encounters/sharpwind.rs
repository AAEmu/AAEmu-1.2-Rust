//! Sharpwind Mines: the tornadoes, Okape, the Illusionist, Nerta, the Stone Brother (Colossus)
//! and the powder kegs.

// ---- keys

/// Sharpwind Mines' world template.
pub const WORLD: &str = "instance_cuttingwind_deadmine";
/// Sharpwind Mines' zone.
pub const ZONE: u32 = 262;

// ---- tables

use super::registry::PlotGround;

/// These self-target plots project their strikes from the caster's heading.
pub(super) fn is_directional_self_skill(skill_id: u32) -> bool {
    matches!(skill_id, 15935 | 19980 | 18374 | 18401 | 18406 | 19248)
}

/// Keep incidental assistance near the pull.
pub(super) fn helps_only_near_the_pull(world: &str) -> bool {
    world == WORLD
}

/// Alistair's routes are run, with actor flags 4.
pub(super) fn route_runs_with_flags(route: &str) -> Option<u8> {
    route.starts_with("aipath_alistair").then_some(4)
}

/// At half health the Illusionist splits once the authored cooldown permits it.
pub(super) fn is_priority_combat_skill(skill_type: u32) -> bool {
    skill_type == 18485
}

/// The tornado (12014) and Okape (12188) run their spawn events as soon as they spawn.
pub(super) fn runs_spawn_events_on_spawn(template_id: u32) -> bool {
    matches!(template_id, 12014 | 12188)
}

/// The arena cue finds its audience and the tornado starts its hazard
/// buff once the casting unit is registered in the world.
pub(super) fn waits_for_visible_caster(skill_id: u32) -> bool {
    matches!(skill_id, 18983 | 19535)
}

/// Sharpwind Mines casts its final-room cue (cinema 51, `id_262_01`) on an NPC.
/// Play it for characters in the same room on this instance.
pub(super) fn broadcasts_room_cinema(cinema_id: u32) -> bool {
    cinema_id == 51
}

/// The Illusionist's frozen zone: its clout is cast by the NPC that summoned the doodad.
pub(super) fn is_frozen_zone(template_id: u32, clout_id: u32) -> bool {
    template_id == 5150 && clout_id == 1152
}

/// The powder keg, a worn pack, and the doodad it is dropped as.
pub const POWDER_KEG_DOODAD: u32 = 5951;
/// The powder keg item (equipped in the backpack slot).
pub const POWDER_KEG_ITEM: u32 = 25002;

/// The final-room cinematic timer belongs to the invisible cue actor, and the bandits' staff
/// pickups run once from the caster.
pub(super) fn applies_to_source(skill_id: u32, effect_id: u32) -> bool {
    matches!(
        (skill_id, effect_id),
        (19535, 24195) | (19019, 24232) | (19022, 24231)
    )
}

/// Nerta's Bomb Drop buff.
pub const NERTA_BOMB_BUFF: u32 = 4099;
/// The second Bomb Drop buff of the same name.
pub const NERTA_BOMB_BUFF_2: u32 = 4631;

/// Nerta (11362) under one of her Bomb Drop buffs. `npc_template_id` (the buff owner's NPC
/// template, `None` for another unit) is only asked for those buffs.
pub(super) fn is_nerta_bomb_drop(
    buff_id: u32,
    npc_template_id: impl FnOnce() -> Option<u32>,
) -> bool {
    matches!(buff_id, 4099 | 4631) && npc_template_id().is_some_and(|id| id == 11362)
}

/// The powder keg skill: the summon interaction consumes the actual equipped powder keg.
pub const POWDER_KEG_SKILL: u32 = 18715;

/// The Colossus's hand smashes in Sharpwind Mines. The near hand-smash circle (events 3199,
/// 3201) covers the fist in front of the body. The second circle and outer impact remain
/// distinct parts of the wind-up. `zone_id` is only asked for those skills.
pub(super) fn plot_ground(
    skill_id: u32,
    event_id: u32,
    zone_id: impl FnOnce() -> u32,
) -> Option<PlotGround> {
    let colossus = matches!(skill_id, 18374 | 18401 | 18406 | 19248) && zone_id() == 262;
    colossus.then_some(PlotGround::ColossusFist {
        near: matches!(event_id, 3199 | 3201),
    })
}

// ---- hooks

use std::sync::Arc;

use crate::server::GameServer;
use crate::systems::ai::behaviors::{delta_seconds, npc_move_towards};
use crate::systems::ai::framework::{AiCtx, BehaviorKind};
use crate::systems::ai::npc;
use crate::systems::npcs::doodad::runtime::DoodadCtx;
use crate::systems::npcs::doodad::templates::DoodadFuncSpawnGimmick;
use crate::systems::npcs::gimmick::manager::{despawn, do_gimmick_skill, spawn_from_doodad};
use crate::systems::skills::controllers::Vec3;
use crate::world::{now_ms, WorldInstance, WorldObject};

use BehaviorKind as B;

/// The powder keg gimmick a placed keg spawns when its fuse has burnt.
pub const POWDER_KEG_GIMMICK: u32 = 56;
/// The keg's explosion, gimmick 56's contact skill.
pub const KEG_BLAST_SKILL: u32 = 18717;
/// The phase of a keg that has exploded.
pub const KEG_SPENT_PHASE: i32 = 15332;

/// The tornado (12014) crosses the arena while its hazard buff (4225) is active.
pub(super) fn ai_tick(c: &AiCtx<'_>, k: BehaviorKind, delta_ms: i64) -> bool {
    if let Some(start) = (k != B::Spawning
        && c.npc.data().unit.template_id == 12014
        && c.obj.transform().zone_id == 262)
        .then(|| {
            c.obj.with_base_unit(|unit| {
                unit.buffs()
                    .effects
                    .iter()
                    .find(|buff| buff.template.id == 4225 && !buff.is_ended())
                    .map(|buff| buff.start_time_ms)
            })
        })
        .flatten()
    {
        // The event spawners place each vortex on a different side of the arena.
        // Traverse toward the center and back while its authored hazard buff is active.
        let home = c.ai.idle_position;
        let center = Vec3::new(613.275, 634.419, home.z);
        let elapsed = (now_ms() - start).max(0) as f32 / 1000.0;
        let phase = elapsed / 15.0;
        let fraction = 1.0 - (phase.rem_euclid(2.0) - 1.0).abs();
        let waypoint = Vec3::new(
            home.x + (center.x - home.x) * fraction,
            home.y + (center.y - home.y) * fraction,
            home.z,
        );
        let pace = npc::real_movement_speed(&c.obj, 3.0);
        npc_move_towards(
            c,
            waypoint,
            (pace * delta_seconds(delta_ms)) as f32,
            4,
            0.05,
        );
        return true;
    }
    false
}

fn is_stonebrother(c: &AiCtx<'_>) -> bool {
    c.npc.data().unit.template_id == 11363 && c.obj.transform().zone_id == 262
}

/// `ShouldReturn` of the Colossus: it leashes to its room.
pub(super) fn leash(c: &AiCtx<'_>, me: Vec3, target_position: Vec3) -> Option<bool> {
    if is_stonebrother(c) {
        return Some(
            !npc::in_stonebrother_arena(me) || !npc::in_stonebrother_arena(target_position),
        );
    }
    None
}

/// The Colossus chases on its arena floor.
pub(super) fn chase(c: &AiCtx<'_>, target_pos: Vec3, speed: f64, move_flags: u8) -> bool {
    if is_stonebrother(c) && npc::in_stonebrother_arena(target_pos) {
        // The Colossus's footprint cannot use the small-NPC corridor route.
        // Chase on the arena floor while the room leash permits it.
        npc_move_towards(c, target_pos, speed as f32, move_flags, 1.0);
        return true;
    }
    false
}

/// The Colossus's steps stay on its room's floor.
pub(super) fn walker_floor(
    template_id: u32,
    zone_id: u32,
    pos: Vec3,
    new: Vec3,
    spawner_z: Option<f32>,
    mut target_z: f32,
) -> f32 {
    if template_id == 11363
        && zone_id == 262
        && in_stonebrother_arena(Vec3::new(new.x, new.y, pos.z))
    {
        // Keep the large ground actor on the room's level if a ray hits the
        // floor beneath the arena or the collision mesh above its feet.
        if let Some(floor) = spawner_z.filter(|_| !(149.0..=156.0).contains(&target_z)) {
            target_z = floor;
        }
    }
    target_z
}

/// The Colossus room around the authored spawn, above the passages underneath.
pub(crate) fn in_stonebrother_arena(pos: Vec3) -> bool {
    (pos.x - 614.4).hypot(pos.y - 423.4) <= 31.0 && (149.0..=160.0).contains(&pos.z)
}

/// A placed powder keg's `SpawnGimmick` detonates the keg.
pub(super) fn spawn_gimmick(
    server: &GameServer,
    ctx: &DoodadCtx<'_>,
    f: &DoodadFuncSpawnGimmick,
) -> Option<bool> {
    if ctx.doodad.data().template_id == POWDER_KEG_DOODAD && f.gimmick_id == POWDER_KEG_GIMMICK {
        if detonate_powder_keg(server, &ctx.world, &ctx.obj(), f) {
            ctx.doodad.data().phase.override_phase = KEG_SPENT_PHASE;
            return Some(true);
        }
        return Some(false);
    }
    None
}

/// The placed keg's five-second fuse has fired. Gimmick 56 carries the explosion
/// as its contact skill, but a stationary keg does not make a physics contact.
pub(crate) fn detonate_powder_keg(
    server: &GameServer,
    world: &Arc<WorldInstance>,
    source: &WorldObject,
    template: &crate::systems::npcs::doodad::templates::DoodadFuncSpawnGimmick,
) -> bool {
    let Some(gimmick) = spawn_from_doodad(server, world, source, template) else {
        return false;
    };
    let Some(active) = server.systems.npcs.gimmicks.find(gimmick.obj_id) else {
        return false;
    };
    do_gimmick_skill(server, world, &active, KEG_BLAST_SKILL);
    active.state().despawned = true;
    despawn(server, &active);
    true
}

/// Sharpwind Mines' encounter hooks.
pub struct SharpwindMines;

impl super::hooks::Encounter for SharpwindMines {
    fn ai_tick(&self, c: &AiCtx<'_>, k: BehaviorKind, delta_ms: i64) -> bool {
        ai_tick(c, k, delta_ms)
    }

    fn leash(&self, c: &AiCtx<'_>, me: Vec3, target: Vec3) -> Option<bool> {
        leash(c, me, target)
    }

    fn chase(&self, c: &AiCtx<'_>, target: Vec3, speed: f64, move_flags: u8) -> bool {
        chase(c, target, speed, move_flags)
    }

    fn walker_floor(
        &self,
        template_id: u32,
        zone_id: u32,
        pos: Vec3,
        new: Vec3,
        spawner_z: Option<f32>,
        target_z: f32,
    ) -> f32 {
        walker_floor(template_id, zone_id, pos, new, spawner_z, target_z)
    }

    fn spawn_gimmick(
        &self,
        server: &GameServer,
        ctx: &DoodadCtx<'_>,
        f: &DoodadFuncSpawnGimmick,
    ) -> Option<bool> {
        spawn_gimmick(server, ctx, f)
    }
}
