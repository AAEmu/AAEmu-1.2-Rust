use crate::server::GameServer;
use crate::world::WorldObject;

use super::super::geometry::{world_of, world_pos, Target};
use super::super::relation::{can_attack, protected_from, relation_to};
use super::super::state::SkillRun;
use super::super::types::{
    DamageType, RelationState, SkillCastTarget, SkillCaster, SkillTargetRelation, SkillTargetType,
};
use super::{CastCtx, PET_DUAL_RECOVERY};

fn lookup_base_unit(server: &GameServer, caster: &WorldObject, obj_id: u32) -> Option<WorldObject> {
    world_of(server, caster).and_then(|w| w.object(obj_id))
}

/// `Skill.SetInitialTarget(caster, targetCaster)`: a position unit.
fn set_initial_target(
    server: &GameServer,
    caster: &WorldObject,
    target_caster: &SkillCastTarget,
) -> Target {
    let caster_t = Target::Obj(caster.clone());
    let mut p = super::geometry::pos_unit_from(server, &caster_t);
    match *target_caster {
        SkillCastTarget::Position {
            x, y, z, obj_id1, ..
        } => {
            let npc_target = caster
                .as_npc()
                .map(|n| n.data().unit.current_target)
                .filter(|t| *t != 0)
                .and_then(|t| server.world.object(t));
            if let Some(t) = npc_target {
                let l = t.transform();
                p.pos.x = l.x;
                p.pos.y = l.y;
                p.pos.z = l.z;
            } else if obj_id1 != 0 {
                match lookup_base_unit(server, caster, obj_id1) {
                    Some(basis) => {
                        let bw = world_pos(server, &Target::Obj(basis.clone()));
                        let scale = basis.scale();
                        let local = [x * scale, y * scale, z * scale];
                        let r = bw.to_quaternion().rotate(local);
                        p.pos.x = r[0] + bw.x;
                        p.pos.y = r[1] + bw.y;
                        p.pos.z = r[2] + bw.z;
                    }
                    None => {
                        let w = world_pos(server, &caster_t);
                        p.pos.x = w.x;
                        p.pos.y = w.y;
                        p.pos.z = w.z;
                    }
                }
            } else {
                p.pos.x = x;
                p.pos.y = y;
                p.pos.z = z;
            }
        }
        SkillCastTarget::Position2 { x, y, z, .. } | SkillCastTarget::Position3 { x, y, z, .. } => {
            p.pos.x = x;
            p.pos.y = y;
            p.pos.z = z;
        }
        _ => {}
    }
    Target::Pos(p)
}

/// `Skill.GetInitialTarget(caster, skillCaster, targetCaster)` (`None` = null).
pub(super) fn initial_target(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
    skill_caster: &SkillCaster,
    target_caster: &mut SkillCastTarget,
) -> Option<Target> {
    if !caster.is_unit() {
        return None;
    }
    // The updated cannon UI sends its confirmed impact point. The shipped
    // self-targeted data still supports the forward shot from older clients.
    if super::validation::is_portable_harpoon_shot(skill.id)
        && matches!(
            target_caster,
            SkillCastTarget::Position { .. }
                | SkillCastTarget::Position2 { .. }
                | SkillCastTarget::Position3 { .. }
        )
    {
        return Some(set_initial_target(server, caster, target_caster));
    }
    let mut target: Option<WorldObject> = Some(caster.clone());
    if matches!(
        skill_caster,
        SkillCaster::Mount { .. } | SkillCaster::Unit(_)
    ) {
        target = world_of(server, caster).and_then(|w| w.unit(skill_caster.obj_id()));
    }
    let tt = if super::validation::is_portable_harpoon_shot(skill.id) {
        Some(SkillTargetType::SelfTarget)
    } else {
        SkillTargetType::from_raw(i64::from(skill.template.target_type))
    };
    let caster_t = Target::Obj(caster.clone());
    let lookup = |tc: &mut SkillCastTarget| -> Option<WorldObject> {
        let found = if tc.obj_id() > 0 {
            lookup_base_unit(server, caster, tc.obj_id())
        } else {
            Some(caster.clone())
        };
        if let Some(f) = &found {
            tc.set_obj_id(f.obj_id());
        }
        found
    };
    use SkillTargetType as T;
    match tt {
        Some(T::SelfTarget) => {
            if target_caster.is_unit_or_doodad() {
                if let Some(t) = &target {
                    target_caster.set_obj_id(t.obj_id());
                }
            }
        }
        Some(T::Friendly) => {
            if target_caster.is_unit_or_doodad() {
                target = lookup(target_caster);
            }
            if skill.id == PET_DUAL_RECOVERY && skill_caster.is_item() {
                // The potion button can send the owner as the friendly target. Apply it to
                // the summoned mate, or refuse before consuming the potion.
                if target.as_ref().is_some_and(|t| t.same(caster)) {
                    let mate = crate::systems::vehicles::mates::first_active_mate(
                        server,
                        caster.as_player()?,
                    )?;
                    target_caster.set_obj_id(mate.obj_id);
                    target = Some(WorldObject::from(&mate));
                }
                target.as_ref().and_then(WorldObject::as_mate)?;
            }
            if let Some(t) = &target {
                let r = relation_to(server, &caster_t, &Target::Obj(t.clone()));
                if r != RelationState::Friendly && r != RelationState::Neutral {
                    return None;
                }
            }
        }
        Some(T::Hostile) => {
            if target_caster.is_unit_or_doodad() {
                target = lookup(target_caster);
            }
            if let Some(t) = &target {
                let tt = Target::Obj(t.clone());
                let r = relation_to(server, &caster_t, &tt);
                // Main's relation check, plus the player-protection cases (Peace, no-PvP,
                // home zones): a Neutral target keeps the C# behaviour, a protected player is
                // refused.
                if r != RelationState::Hostile
                    && r != RelationState::Neutral
                    && !can_attack(server, &caster_t, &tt)
                {
                    return None;
                }
                if protected_from(server, &caster_t, &tt) {
                    return None;
                }
            }
        }
        Some(T::AnyUnit | T::Doodad) => {
            if target_caster.is_unit_or_doodad() {
                target = lookup(target_caster);
            }
        }
        Some(T::Item) => {}
        Some(T::Others | T::Building) => {
            if target_caster.is_unit_or_doodad() {
                target = lookup(target_caster);
            }
            if target.as_ref().is_some_and(|t| t.same(caster)) {
                return None;
            }
        }
        Some(T::FriendlyOthers) => {
            if target_caster.is_unit_or_doodad() {
                target = lookup(target_caster);
            }
            let t = target.as_ref()?;
            if t.same(caster) {
                return None;
            }
            let r = relation_to(server, &caster_t, &Target::Obj(t.clone()));
            if r != RelationState::Friendly && r != RelationState::Neutral {
                return None;
            }
        }
        Some(T::Pos) => {
            return Some(set_initial_target(server, caster, target_caster));
        }
        Some(T::BallisticPos | T::ArtilleryPos) => {
            // A cannon shot is an aimed impact point (`SkillCastTarget::Position`, path A of the
            // 1.2 client). The Lua bar (path B) sends a Unit target for these types; treating it
            // as a position kept the caster's own spot under `set_initial_target`, i.e. distance
            // 0, and refused the shot as too close. The 1.2 client answers NO_TARGET (0x9).
            if matches!(
                target_caster,
                SkillCastTarget::Position { .. }
                    | SkillCastTarget::Position2 { .. }
                    | SkillCastTarget::Position3 { .. }
            ) {
                return Some(set_initial_target(server, caster, target_caster));
            }
            return None;
        }
        Some(T::RelativePos) => {
            if matches!(
                target_caster,
                SkillCastTarget::Position { .. }
                    | SkillCastTarget::Position2 { .. }
                    | SkillCastTarget::Position3 { .. }
            ) {
                return Some(set_initial_target(server, caster, target_caster));
            }
        }
        Some(T::SummonPos) => {
            // 10.x `Skill.GetInitialTarget`: the chosen summon spot is the target, so range,
            // angle and obstacle checks see the placement instead of the caster.
            if matches!(
                target_caster,
                SkillCastTarget::Position { .. }
                    | SkillCastTarget::Position2 { .. }
                    | SkillCastTarget::Position3 { .. }
            ) {
                return Some(set_initial_target(server, caster, target_caster));
            }
        }
        Some(T::Pet) if caster.as_player().is_some() => {
            let mate =
                crate::systems::vehicles::mates::first_active_mate(server, caster.as_player()?)?;
            target_caster.set_obj_id(mate.obj_id);
            target = Some(WorldObject::from(&mate));
        }
        Some(T::Party | T::Raid | T::Line | T::Pet | T::SourcePos | T::CursorPos) => {}
        None => {
            tracing::error!(
                "SkillTargetType not supported {} (NotSupportedException)",
                skill.template.target_type
            );
            return None;
        }
    }
    target.map(Target::Obj)
}

/// `ShipSiegeAoEHit.AppendHostileShipsHitBySiegeHullAoE`: a siege AoE cast on a position also
/// hits the boats (with ship physics) whose mass box the siege circle touches, although their
/// pivot is outside the circle (`systems::ai::physics::siege_point_hits_ship`).
pub(super) fn append_hostile_ships_hit_by_siege_hull_aoe(
    server: &GameServer,
    t: &super::data::SkillTemplate,
    caster: &Target,
    target_self: &Target,
    ctx: &CastCtx,
    radius: f32,
    possible: &mut Vec<Target>,
) {
    use crate::systems::ai::physics::{siege_point_hits_ship, SIEGE_PIVOT_QUERY_SLACK_METERS};
    if radius <= 0.0 {
        return;
    }
    if DamageType::from_raw(i64::from(t.damage_type_id)) != Some(DamageType::Siege) {
        return;
    }
    if !matches!(
        ctx.target_caster,
        SkillCastTarget::Position { .. } | SkillCastTarget::Position2 { .. }
    ) {
        return;
    }
    let p = super::geometry::world_pos(server, target_self);
    let relation = SkillTargetRelation::from_raw(i64::from(t.target_relation))
        .unwrap_or(SkillTargetRelation::Any);
    let nearby = super::geometry::around_radius(
        server,
        target_self,
        radius + SIEGE_PIVOT_QUERY_SLACK_METERS,
        false,
    );
    for o in nearby {
        let Some(ship) = o.as_slave() else {
            continue;
        };
        if possible
            .iter()
            .any(|x| x.as_obj().is_some_and(|y| y.same(&o)))
        {
            continue;
        }
        let target = Target::Obj(o.clone());
        if !super::relation::is_relation_valid(server, relation, caster, &target) {
            continue;
        }
        // IsABoat() + RigidBody / ShipModel + the mass-box test (physics snapshot).
        if !siege_point_hits_ship(server, ship.obj_id, p.x, p.y, radius) {
            continue;
        }
        possible.push(target);
    }
}
