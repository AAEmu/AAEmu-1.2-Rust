//! Server-side cast restrictions, checked before GCD, buffs, plots or resources change.

use crate::server::GameServer;
use crate::world::WorldObject;

use super::geometry::{world_pos, Target};
use super::state::SkillRun;
use super::unit_reqs::skill_result_for_key;

/// Extra range for a doodad target, covering the distance from its origin to its edge.
const DOODAD_RANGE_ALLOWANCE: f64 = 15.0;
/// Extra range for a slave target (large ships are centred far from their hull edge).
const SLAVE_RANGE_ALLOWANCE: f64 = 40.0;

fn fail(key: &str) -> Result<(), (u8, u32)> {
    Err((skill_result_for_key(key), 0))
}

/// Item-caster tags belong to the owned item's server-side template, never the wire hint.
pub(super) fn check_item_zone(
    server: &GameServer,
    caster: &WorldObject,
    source: &super::types::SkillCaster,
) -> Result<(), (u8, u32)> {
    if let (Some(player), super::types::SkillCaster::Item { item_id, .. }) =
        (caster.as_player(), source)
    {
        if let Some((template, _)) = server
            .systems
            .skills
            .ext
            .items()
            .owned_item(server, player, *item_id)
        {
            if !crate::systems::economy::inventory::item_use_zone_allowed(server, player, template)
            {
                return fail("SKILL_CANNOT_USE_HERE");
            }
        }
    }
    Ok(())
}

/// The physics collider kind of a world object (matches the keys `ai::collision` registers).
fn collision_kind(obj: &WorldObject) -> crate::systems::ai::collision::world::ObjKind {
    use crate::systems::ai::collision::world::ObjKind;
    match obj {
        WorldObject::Doodad(_) => ObjKind::Doodad,
        WorldObject::House(_) => ObjKind::House,
        WorldObject::Slave(_) | WorldObject::Shipyard(_) => ObjKind::Ship,
        WorldObject::Gimmick(_) => ObjKind::Gimmick,
        _ => ObjKind::Unit,
    }
}

fn in_water(server: &GameServer, target: &Target) -> bool {
    if target
        .as_obj()
        .and_then(|o| o.with_unit(|u| u.unit().is_under_water))
        .unwrap_or(false)
    {
        return true;
    }
    let pos = world_pos(server, target);
    let instance = match target {
        Target::Obj(obj) => obj.transform().instance_id,
        Target::Pos(pos) => pos.instance_id,
    };
    crate::systems::ai::physics::is_water(
        server,
        instance,
        super::controllers::Vec3::new(pos.x, pos.y, pos.z + 0.1),
    )
    .is_some()
}

pub(super) fn labor_cost(skill: &SkillRun, player: &crate::world::PlayerRef) -> i32 {
    let labor = skill.template.consume_labor_power.max(0);
    let step = player
        .data()
        .actability
        .actabilities
        .iter()
        .find(|ability| i64::from(ability.id) == i64::from(skill.template.actability_group_id))
        .map(|ability| ability.step);
    let cost = step.map_or(labor, |step| {
        (f64::from(labor) * f64::from(super::progress::labor_cost_multiplier(step)))
            .round_ties_even() as i32
    });
    if labor > 0 {
        cost.max(1)
    } else {
        0
    }
}

pub(super) fn check_resources(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
) -> Result<(), (u8, u32)> {
    if super::skill::mana_cost(server, skill, caster)
        > caster.with_unit(|u| u.unit().mp).unwrap_or(0)
    {
        return fail("skill_lack_mana");
    }
    let Some(player) = caster.as_player() else {
        return Ok(());
    };
    if labor_cost(skill, player) > i32::from(player.data().labor_power) {
        return fail("skill_need_labor_power");
    }
    let reagents = server.game_data.systems.skills.reagents_for(skill.id);
    if !reagents.is_empty() {
        let items = server.systems.skills.ext.items();
        let enough = |reagent: &super::data::SkillItemAmount| {
            items.item_count(server, player, reagent.item_id) >= i32::from(reagent.amount)
        };
        let available = if skill.template.first_reagent_only {
            reagents.iter().any(enough)
        } else {
            reagents.iter().all(enough)
        };
        if !available {
            return fail("skill_need_reagent");
        }
    }
    Ok(())
}

/// Parity deviation (`PORTING.md`): the construction step of an unfinished house needs the
/// consume item of its step skill's effects (e.g. 1 Lumber for the Scarecrow Garden). The C#
/// consumed an effect-level item only when present and built for free otherwise; retail refuses
/// the cast with the reagent error. Every other skill keeps the soft behaviour.
pub(super) fn check_build_materials(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
    target: &Target,
) -> Result<(), (u8, u32)> {
    let Some(player) = caster.as_player() else {
        return Ok(());
    };
    let Some(house) = target.as_obj().and_then(WorldObject::as_house) else {
        return Ok(());
    };
    let (finished, step) = {
        let h = house.data();
        (h.current_step < 0, h.current_step)
    };
    if finished {
        return Ok(());
    }
    let Some(template) = crate::systems::housing::manager::template_of(server, house) else {
        return Ok(());
    };
    if template.build_steps.get(&step).map(|s| s.skill_id) != Some(skill.id) {
        return Ok(());
    }
    let items = server.systems.skills.ext.items();
    let missing = skill.template.effects.iter().any(|e| {
        e.consume_item_id != 0
            && e.consume_item_count > 0
            && items.item_count(server, player, e.consume_item_id) < e.consume_item_count
    });
    if missing {
        return fail("skill_need_reagent");
    }
    Ok(())
}

pub(super) fn check(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
    target: &Target,
) -> Result<(), (u8, u32)> {
    let template = &skill.template;
    // Like the other cast restrictions, this also applies to administrators. A GM's
    // cooldown override does not grant permission to use forbidden tags.
    if caster.as_player().is_some()
        && super::data::zone_rules::current_group(server, caster).is_some_and(|group| {
            let data = &server.game_data.systems.skills;
            data.zone_rules.bans_any(
                group,
                data.skill_tags.get(&skill.id).map_or(&[], Vec::as_slice),
            )
        })
    {
        // Native 1.2 x2game12.c:1983345: SkillResult 58 = ZONE_BANNED.
        return fail("SKILL_CANNOT_USE_HERE");
    }
    if let Some(doodad) = target.as_obj().and_then(WorldObject::as_doodad) {
        if !crate::systems::npcs::doodad::gathering::allows_skill(server, doodad, template, caster)
        {
            return fail("skill_invalid_target");
        }
    }
    let (hp, buff_ids, moving) = caster
        .with_unit(|u| {
            let unit = u.unit();
            (
                unit.hp,
                unit.buffs
                    .effects
                    .iter()
                    .filter(|b| !b.is_ended())
                    .map(|b| b.template.id)
                    .collect::<Vec<_>>(),
                unit.skill_state.moving,
            )
        })
        .ok_or((skill_result_for_key("skill_invalid_source"), 0))?;
    if hp <= 0 && template.source_alive && !template.source_dead {
        return fail("skill_source_died");
    }
    if hp > 0 && template.source_dead && !template.source_alive {
        return fail("skill_source_alive");
    }
    if template.source_not_swim && in_water(server, &Target::Obj(caster.clone())) {
        return fail("skill_cannot_cast_in_swimming");
    }
    if template.source_cannot_use_while_walk && moving {
        return fail("skill_cannot_cast_while_walking");
    }
    for id in buff_ids {
        let Some(buff) = server.game_data.systems.skills.buff(id) else {
            continue;
        };
        if !template.source_stun && (buff.stun || buff.sleep || buff.knockdown) {
            return fail("skill_cannot_cast_in_stun");
        }
        if buff.blank_minded && !template.source_stun {
            return fail("skill_blank_minded");
        }
    }
    if let Some(target_hp) = target.as_obj().and_then(|o| o.with_unit(|u| u.unit().hp)) {
        if target_hp <= 0 && template.target_alive && !template.target_dead {
            return fail("skill_target_died");
        }
        if target_hp > 0 && template.target_dead && !template.target_alive {
            return fail("skill_target_alive");
        }
    }
    if template.target_only_water && !in_water(server, target) {
        return fail("skill_invalid_location");
    }
    let source = world_pos(server, &Target::Obj(caster.clone()));
    let destination = world_pos(server, target);
    if ![
        source.x,
        source.y,
        source.z,
        destination.x,
        destination.y,
        destination.z,
    ]
    .iter()
    .all(|v| v.is_finite())
    {
        return fail("skill_invalid_target");
    }
    if template.target_angle > 0
        && !super::geometry::in_angle(&source, &destination, template.target_angle as f32)
    {
        return fail("skill_outof_angle");
    }
    let distance = f64::from(super::geometry::distance_to(
        server,
        &Target::Obj(caster.clone()),
        target,
        true,
    ));
    let mut min_range = f64::from(template.min_range);
    let mut max_range = super::skill::apply_skill_modifiers(
        server,
        skill,
        caster,
        crate::models::unit::SkillAttribute::Range,
        f64::from(template.max_range),
    );
    if template.target_type == super::types::SkillTargetType::Doodad as i32
        && template.min_range >= 100
    {
        min_range /= 100.0;
    }
    if template.weapon_slot_for_range_id > 0 {
        let ranges = server
            .systems
            .skills
            .ext
            .items()
            .weapon_holdable(server, caster, template.weapon_slot_for_range_id)
            .map_or((0.0, 3.0), |h| (h.min_range, h.max_range));
        min_range = f64::from(ranges.0);
        max_range = f64::from(ranges.1);
    }
    if distance < min_range {
        return fail("skill_too_close_range");
    }
    // A position-targeted skill is cast at a spot on the ground; its template may carry
    // max_range 0 because the client decides where the placement is legal (10.x
    // `unboundedPlacement`; summon skill 15802 rejected every boat at 3.4 m without it).
    let unbounded_placement = matches!(target, Target::Pos(_)) && template.max_range <= 0;
    // Doodads have no model radius and a ship's origin is far from its hull, so these targets
    // get an allowance on top of the skill's range (C# exempted them entirely).
    let extent = target.as_obj().map_or(0.0, |o| {
        if o.as_doodad().is_some() {
            DOODAD_RANGE_ALLOWANCE
        } else if o.as_slave().is_some() {
            SLAVE_RANGE_ALLOWANCE
        } else {
            0.0
        }
    });
    if distance > max_range + extent && !unbounded_placement {
        return fail("skill_too_far_range");
    }
    if template.check_obstacle && !target.as_obj().is_some_and(|o| o.same(caster)) {
        // 1.2 client `FUN_390B5650` (x2game.dll.c:149951): shorten the end point by the
        // target radius and cast no ray when `dist <= radius`; `FUN_390B5540`
        // (x2game.dll.c:149877) uses objtypes 7 (0x107 with `check_terrain`), ray flags
        // 0x2048F (collision class 2) and one hit, mapping a hit to skill result 0xC.
        // Unit targets (`FUN_390B5C60` -> `FUN_3989C0D0`) use the target radius at target
        // +0x80 and skip the source and target colliders; position targets
        // (`FUN_390B5A30` -> `FUN_390B5790`) use the constant 0.5 (DAT_3999C47C) and skip
        // only the source.
        let dx = destination.x - source.x;
        let dy = destination.y - source.y;
        let dz = destination.z - source.z;
        let len = (dx * dx + dy * dy + dz * dz).sqrt();
        let (target_radius, skip_target) = match target {
            Target::Pos(_) => (0.5, false),
            Target::Obj(_) => (super::geometry::model_size(server, target), true),
        };
        if len > target_radius {
            let mut to = [destination.x, destination.y, destination.z];
            if target_radius > 0.0 {
                to[0] -= dx / len * target_radius;
                to[1] -= dy / len * target_radius;
                to[2] -= dz / len * target_radius;
            }
            let mut ignored = vec![(collision_kind(caster), caster.obj_id())];
            if skip_target {
                if let Some(obj) = target.as_obj() {
                    ignored.push((collision_kind(obj), obj.obj_id()));
                }
            }
            let objtypes = if template.check_terrain {
                crate::systems::ai::collision::query::mask::SOLID_TERRAIN
            } else {
                crate::systems::ai::collision::query::mask::SOLID
            };
            if let Some(Some(hit)) = crate::systems::ai::collision::raycast_ignoring_objects(
                server,
                caster.transform().instance_id,
                [source.x, source.y, source.z],
                to,
                objtypes,
                crate::systems::ai::collision::query::ray_flags::SKILL_OBSTACLE,
                &ignored,
            ) {
                // NPC AI retries blocked casts, so only player rejections are warnings.
                if let Some(player) = caster.as_player() {
                    let character = player.data().unit.name.clone();
                    tracing::warn!(
                        character = %character,
                        skill = template.id,
                        target = target.obj_id(),
                        result_key = "skill_obstacle",
                        check_terrain = template.check_terrain,
                        from = ?source,
                        to = ?to,
                        ?hit,
                        "Skill line of sight blocked"
                    );
                } else {
                    tracing::debug!(
                        caster = caster.obj_id(),
                        skill = template.id,
                        target = target.obj_id(),
                        from = ?source,
                        to = ?to,
                        ?hit,
                        "Skill line of sight blocked"
                    );
                }
                return fail("skill_obstacle");
            }
        }
    }
    Ok(())
}
