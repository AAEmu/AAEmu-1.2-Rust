//! Burnt Castle Armory: Sorvell, Hieronimus and the portal waves.

// ---- keys

/// The Armory's world template.
pub const WORLD: &str = "instance_burntcastle_armory";
/// The Armory's zone.
pub const ZONE: u32 = 236;

// ---- tables

use super::registry::PlotGround;

/// Hieronimus's portal adds (8964..=8969) in the Armory.
pub(super) fn is_wave_add(zone_id: u32, template_id: u32) -> bool {
    zone_id == 236 && (8964..=8969).contains(&template_id)
}

/// The Armory command sets that play an encounter intro (103, 106, 107).
pub(super) fn is_encounter_intro(cmd_set_id: u32) -> bool {
    matches!(cmd_set_id, 103 | 106 | 107)
}

/// Command set 103 uses the low-health staging actor for its opening lines.
/// Finish with its authored vanish skill once the combat boss has appeared.
///
/// `template_id` is only asked for command set 103.
pub(super) fn appended_intro_skill(
    cmd_set_id: u32,
    template_id: impl FnOnce() -> u32,
) -> Option<u32> {
    (cmd_set_id == 103 && template_id() == 10518).then_some(17196)
}

/// Hieronimus (cinema 8, `id_236_01`) reaches the players of his room through an area skill
/// that needs a playback cue.
pub(super) fn plays_cinema_directly(cinema_id: u32) -> bool {
    cinema_id == 8
}

/// The Burnt Castle Howl ward: summoned by its marker NPC, its protection runs as the doodad.
pub(super) fn is_howl_ward(template_id: u32, buff_id: u32) -> bool {
    template_id == 4626 && buff_id == 3615
}

/// Sorvell's barrel bomb, the gimmick that falls from the ceiling.
pub const FALLING_BARREL: u32 = 39;

/// Earthen Grip is marked Others, which the area filter interprets as neutral only.
/// Both it and Sorvell's barrel barrage select enemies that can receive their effects.
pub(super) fn hostile_area_skill(skill_id: u32) -> bool {
    matches!(skill_id, 15592 | 15668)
}

/// Hieronimus casts Howl while a separate effect places the ward: this cue runs once from the
/// caster.
pub(super) fn applies_to_source(skill_id: u32, effect_id: u32) -> bool {
    matches!((skill_id, effect_id), (17635, 19275))
}

/// Compact skill 15667 is shared by the two 1.2 Armory exit doodads (groups 8794 and 12257).
pub const ARMORY_EXIT_SKILL: u32 = 15667;

/// Leave Burnt Castle Armory also applies Return directly. It is offered on the
/// entrance exit and on the rear exit only after skill 18024 reveals it.
pub(super) fn is_exit_doodad(template_id: u32, func_group_id: u32) -> bool {
    matches!((template_id, func_group_id), (3673, 8794) | (4923, 12257))
}

/// Hell Lightning's initial target has only aimed the spell. Its placement step can
/// pick that player, then the event's hit history excludes them on later ticks.
pub(super) fn keeps_first_random_target(event_id: u32) -> bool {
    event_id == 1452
}

/// Thundering Strike's 24-second stack window is the length of four authored
/// cooldowns. Renew the window on a hit so the fifth one can petrify the target.
pub(super) fn refreshes_stack_window(buff_id: u32) -> bool {
    buff_id == 2407
}

/// Hieronimus's meteor shower (15629): its launch event (2668) and its impact event (1508).
pub(super) fn plot_ground(skill_id: u32, event_id: u32) -> Option<PlotGround> {
    let hieronimus_meteor = skill_id == 15629;
    let meteor_impact = hieronimus_meteor && event_id == 1508;
    if event_id == 2668 && hieronimus_meteor {
        Some(PlotGround::MeteorLaunch)
    } else if meteor_impact {
        Some(PlotGround::MeteorImpact)
    } else {
        None
    }
}

// ---- hooks

use crate::server::GameServer;
use crate::systems::ai::behaviors::{
    base_move_speed, delta_seconds, is_using_skill, npc_move_towards, owner_pos,
};
use crate::systems::ai::framework::{AiCtx, BehaviorKind};
use crate::systems::ai::npc;
use crate::systems::npcs::bridges::npc_aggro_target;
use crate::systems::npcs::gimmick::manager::SpawnGimmickParams;
use crate::systems::skills::controllers::{calculate_distance, Vec3};
use crate::systems::skills::data::effects::SpawnGimmickEffect;
use crate::systems::skills::data::SkillTemplate;
use crate::systems::skills::effects::EffectArgs;
use crate::systems::skills::geometry::{world_of, world_pos, Target};
use crate::systems::skills::relation::can_attack;
use crate::systems::skills::state::SkillRef;
use crate::systems::skills::types::skill_result;
use crate::world::{WorldInstance, WorldObject};

use super::registry as encounters;

/// Sorvell, the Armory boss who casts Earthen Grip and the barrel bombs.
pub const SORVELL: u32 = 8960;

fn is_armory_wave_add(c: &AiCtx<'_>) -> bool {
    let npc = c.npc.data();
    encounters::is_wave_add(npc.unit.transform.zone_id, npc.unit.template_id)
}

pub(super) fn advance_armory_wave_from_portal(
    c: &mut AiCtx<'_>,
    k: BehaviorKind,
    delta_ms: i64,
) -> bool {
    if !is_armory_wave_add(c) || c.st(k).armory_portal_cleared {
        return false;
    }
    let pos = owner_pos(c);
    let center = Vec3::new(200.0, 312.0, pos.z);
    if calculate_distance(pos, center, false) <= 34.0 {
        c.st(k).armory_portal_cleared = true;
        return false;
    }
    // Ranged adds can otherwise begin their attacks from the portal recess.
    // All three entrances have a clear approach toward the center.
    if !is_using_skill(c)
        && !npc::is_shackled_or_snared(c.server, &c.obj)
        && !npc::skill_controller_running(&c.obj)
    {
        let speed = npc::real_movement_speed(&c.obj, f64::from(base_move_speed(c)));
        npc_move_towards(
            c,
            center,
            (speed * delta_seconds(delta_ms)) as f32,
            npc::real_movement_flags(speed),
            1.0,
        );
    }
    true
}

/// `ShouldReturn` of a wave add: it leashes to the arena, not to its portal.
pub(super) fn leash(c: &AiCtx<'_>, me: Vec3, target_position: Vec3) -> Option<bool> {
    if is_armory_wave_add(c) {
        // Hieronimus's wave portals sit on the edge of the room. A 50 m
        // leash measured from a portal rejects players on the far side.
        let in_arena =
            |p: Vec3| (p.x - 200.0).hypot(p.y - 312.0) <= 50.0 && (18.0..=26.0).contains(&p.z);
        return Some(!in_arena(me) || !in_arena(target_position));
    }
    None
}

/// Winters's steps by the prison plinth.
pub(super) fn walker_floor(
    template_id: u32,
    zone_id: u32,
    new: Vec3,
    spawner_z: Option<f32>,
    mut target_z: f32,
) -> f32 {
    // The Armory's prison mesh exposes the plinth's base to ground rays.
    // The authored Winters path remains at platform height through x 284.724.
    if template_id == 12190
        && zone_id == 236
        && (284.7..=291.5).contains(&new.x)
        && (233.0..=239.5).contains(&new.y)
    {
        if let Some(stage) = spawner_z {
            target_z = target_z.max(stage);
        }
    }
    target_z
}

fn sorvell_earthen_grip(skill_id: u32, caster: &WorldObject) -> bool {
    skill_id == 15592
        && caster
            .as_npc()
            .is_some_and(|npc| npc.data().unit.template_id == SORVELL)
}

fn earthen_grip_line_of_sight(
    server: &GameServer,
    caster: &WorldObject,
    target: &WorldObject,
) -> bool {
    let from = world_pos(server, &Target::Obj(caster.clone()));
    let to = world_pos(server, &Target::Obj(target.clone()));
    crate::systems::ai::collision::line_of_sight(
        server,
        caster.transform().instance_id,
        [from.x, from.y, from.z + 1.0],
        [to.x, to.y, to.z + 1.0],
    ) != Some(false)
}

fn earthen_grip_has_aggro(server: &GameServer, caster: &WorldObject) -> bool {
    let Some(npc) = caster.as_npc() else {
        return false;
    };
    let target_id = {
        let n = npc.data();
        let target_id = n.current_aggro_target;
        // The basic-attack fallback can pick a self-targeted combat skill
        // without consulting the boss's authored health window.
        if n.unit.max_hp <= 0
            || n.unit.hp <= 0
            || i64::from(n.unit.hp) * 100 >= i64::from(n.unit.max_hp) * 34
            || !n.unit.skill_state.aggro_table.contains_key(&target_id)
        {
            return false;
        }
        target_id
    };
    world_of(server, caster)
        .and_then(|world| world.unit(target_id))
        .is_some_and(|target| {
            crate::systems::ai::npc::can_see_target(server, caster, &target)
                && target.with_unit(|unit| unit.unit().hp > 0) == Some(true)
                && crate::systems::ai::npc::distance_to(server, caster, &target, false) <= 30.0
                && can_attack(
                    server,
                    &Target::Obj(caster.clone()),
                    &Target::Obj(target.clone()),
                )
                && earthen_grip_line_of_sight(server, caster, &target)
        })
}

/// Sorvell does not cast Earthen Grip without an aggro target it can reach.
pub(super) fn cast_gate(
    server: &GameServer,
    skill: &SkillRef,
    caster: &WorldObject,
) -> Option<(u8, u32)> {
    if sorvell_earthen_grip(skill.id, caster) && !earthen_grip_has_aggro(server, caster) {
        return Some((skill_result::NO_TARGET, 0));
    }
    None
}

/// Earthen Grip's effects are skipped once it has no aggro target it can reach.
pub(super) fn on_apply_effects(
    server: &GameServer,
    t: &SkillTemplate,
    caster: &WorldObject,
) -> bool {
    let earthen_grip = sorvell_earthen_grip(t.id, caster);
    if earthen_grip && !earthen_grip_has_aggro(server, caster) {
        return true;
    }
    false
}

/// Earthen Grip only takes the units in Sorvell's sight.
pub(super) fn area_targets(
    server: &GameServer,
    caster: &WorldObject,
    t: &SkillTemplate,
    units: &mut Vec<WorldObject>,
) {
    let earthen_grip = sorvell_earthen_grip(t.id, caster);
    if earthen_grip {
        units.retain(|unit| earthen_grip_line_of_sight(server, caster, unit));
    }
}

/// Sorvell's barrels fall on his victim.
pub(super) fn gimmick_spawn_target(
    server: &GameServer,
    e: &SpawnGimmickEffect,
    args: &EffectArgs<'_>,
    caster: &WorldObject,
    world: &WorldInstance,
    params: &mut SpawnGimmickParams,
) {
    // Sorvell's high-health barrel is a self-cast. Its fixed eight-meter
    // offset has no relation to where he's fighting. Use his current victim
    // for that cast and the selected victim for the low-health barrage.
    if e.gimmick_id == FALLING_BARREL
        && caster
            .as_npc()
            .is_some_and(|n| n.data().unit.template_id == SORVELL)
    {
        let target = args
            .target
            .as_obj()
            .filter(|o| o.obj_id() != caster.obj_id() && o.is_unit())
            .cloned()
            .or_else(|| npc_aggro_target(server, world, caster));
        if let Some(target) = target {
            params.target_position = Some(world.world_pos_rot(&target));
            params.offset_from_source = false;
            params.offset_x = 0.0;
            params.offset_y = 0.0;
        }
    }
}

/// Burnt Castle Armory's encounter hooks.
pub struct Armory;

impl super::hooks::Encounter for Armory {
    fn combat_step(&self, c: &mut AiCtx<'_>, k: BehaviorKind, delta_ms: i64) -> bool {
        advance_armory_wave_from_portal(c, k, delta_ms)
    }

    fn leash(&self, c: &AiCtx<'_>, me: Vec3, target: Vec3) -> Option<bool> {
        leash(c, me, target)
    }

    fn walker_floor(
        &self,
        template_id: u32,
        zone_id: u32,
        _pos: Vec3,
        new: Vec3,
        spawner_z: Option<f32>,
        target_z: f32,
    ) -> f32 {
        walker_floor(template_id, zone_id, new, spawner_z, target_z)
    }

    fn cast_gate(
        &self,
        server: &GameServer,
        skill: &SkillRef,
        caster: &WorldObject,
    ) -> Option<(u8, u32)> {
        cast_gate(server, skill, caster)
    }

    fn on_apply_effects(
        &self,
        server: &GameServer,
        t: &SkillTemplate,
        caster: &WorldObject,
    ) -> bool {
        on_apply_effects(server, t, caster)
    }

    fn area_targets(
        &self,
        server: &GameServer,
        caster: &WorldObject,
        t: &SkillTemplate,
        units: &mut Vec<WorldObject>,
    ) {
        area_targets(server, caster, t, units)
    }

    fn gimmick_spawn_target(
        &self,
        server: &GameServer,
        e: &SpawnGimmickEffect,
        args: &EffectArgs<'_>,
        caster: &WorldObject,
        world: &WorldInstance,
        params: &mut SpawnGimmickParams,
    ) {
        gimmick_spawn_target(server, e, args, caster, world, params)
    }
}
