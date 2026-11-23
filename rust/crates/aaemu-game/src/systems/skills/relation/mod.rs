//! Faction relations and attack permission: `SystemFaction.GetRelationState`,
//! `BaseUnit.GetRelationStateTo`, `BaseUnit.CanAttack`, `GetOwnerCharacter`,
//! `SkillTargetingUtil.FilterWithRelation` / `IsRelationValid`.

use crate::server::GameServer;
use crate::world::{now_ms, PlayerRef, WorldObject};

use super::geometry::Target;
use super::types::{RelationState, SkillTargetRelation};
use super::{buffs, geometry};

mod ownership;
mod protection;

pub use self::ownership::{credited_player, owner_character, world_slave_driver};
pub use self::protection::{can_attack, protected_from, ramming_protected};

#[cfg(test)]
use crate::data::world_data::conflict_type;

/// `FactionsEnum` root factions.
const FACTION_NEUTRAL: u32 = 2;

/// `WorldManager.DefaultCombatTimeout` (seconds).
pub const DEFAULT_COMBAT_TIMEOUT_MS: i64 = 15_000;

fn relation_from_raw(v: u8) -> RelationState {
    RelationState::from_raw(i64::from(v)).unwrap_or(RelationState::Neutral)
}

/// `SystemFaction.GetRelationState(other)` for faction ids (0 / unknown ids = null factions).
/// Runtime nations are resolved by the social subsystem's shared implementation.
pub fn faction_relation(server: &GameServer, own: u32, other: u32) -> RelationState {
    relation_from_raw(crate::systems::social::faction::relation_state_live(
        server, own, other,
    ))
}

/// The faction id of a target (0 for position units / no faction).
pub fn faction_of(t: &Target) -> u32 {
    match t {
        Target::Obj(o) => o.with_base_unit(|b| b.faction_id()),
        Target::Pos(_) => 0,
    }
}

/// `BaseUnit.GetRelationStateTo(unit)`.
///
/// Inside the area of a running siege the two siege parties decide first: the attacking and the
/// defending participants are `Hostile` to each other (even two characters of one faction), the
/// participants of one party are `Friendly` (even of hostile factions), and everybody who is no
/// participant, or stands outside the area, keeps the faction relation
/// (`housing::dominion::hostility`). Every reader of the relation sees it: the skill target
/// filters, the hostile-only / friendly-only effect checks, the crime check and the PvP honor.
pub fn relation_to(server: &GameServer, a: &Target, b: &Target) -> RelationState {
    match crate::systems::housing::dominion::hostility::relation(server, a, b) {
        Some(siege) => siege.relation_state(),
        None => faction_relation(server, faction_of(a), faction_of(b)),
    }
}

/// `Character.IsActivelyHostile(target)`.
pub fn is_actively_hostile(me: &PlayerRef, target_obj_id: u32) -> bool {
    let now = now_ms();
    me.data()
        .skill_char
        .hostile_players
        .iter()
        .find(|(id, _)| *id == target_obj_id)
        .is_some_and(|(_, t)| t + DEFAULT_COMBAT_TIMEOUT_MS > now)
}

/// `SkillTargetingUtil.FilterWithRelation(relation, caster, units)`.
pub fn filter_with_relation(
    server: &GameServer,
    relation: SkillTargetRelation,
    caster: &Target,
    units: Vec<WorldObject>,
) -> Vec<WorldObject> {
    match relation {
        SkillTargetRelation::Any | SkillTargetRelation::Party => units,
        SkillTargetRelation::Friendly => units
            .into_iter()
            .filter(|o| {
                let t = Target::Obj(o.clone());
                relation_to(server, caster, &t) == RelationState::Friendly
                    && !can_attack(server, caster, &t)
            })
            .collect(),
        SkillTargetRelation::Hostile => units
            .into_iter()
            .filter(|o| can_attack(server, caster, &Target::Obj(o.clone())))
            .collect(),
        SkillTargetRelation::Raid => {
            let social = server.systems.skills.ext.social();
            let team = social.team_of(server, caster.obj_id());
            let mut out: Vec<WorldObject> = if team == 0 {
                // units.Where(o => o.ObjId == mate?.ObjId): the first active mate
                let mate = caster
                    .as_obj()
                    .and_then(WorldObject::as_player)
                    .and_then(|p| crate::systems::vehicles::mates::first_active_mate(server, p))
                    .map(|m| m.obj_id);
                units
                    .into_iter()
                    .filter(|o| Some(o.obj_id()) == mate)
                    .collect()
            } else {
                units
                    .into_iter()
                    .filter(|o| social.is_team_obj_member(server, team, o.obj_id()))
                    .collect()
            };
            if let Target::Obj(c) = caster {
                out.push(c.clone()); // .Append(caster)
            }
            out
        }
        SkillTargetRelation::Others => units
            .into_iter()
            .filter(|o| {
                relation_to(server, caster, &Target::Obj(o.clone())) == RelationState::Neutral
            })
            .collect(),
    }
}

/// `tick_area_relation_id` 6 (Arc Lightning / Electric Shock 250, 5847, 7465: "Inflicts damage on
/// you and allies within 2m"). It is not a `SkillTargetRelation` of the C# enum (the C# fell
/// through `default => units`, so every unit in the radius, the caster and its mount included,
/// received the spread DoT and the damage). The buff text says the DoT jumps from its holder to
/// the HOLDER's allies, so the relation is read from the holder, not from the buff's caster.
pub const TICK_AREA_HOLDER_ALLIES: u32 = 6;

/// `tick_area_relation_id == TICK_AREA_HOLDER_ALLIES`: the holder itself and the units around it
/// that are friendly to the holder. The caster, its mates / slaves and anything else owned by the
/// caster's character never qualify, and the caster must be allowed to attack the unit (a harmful
/// spread never lands on a unit the caster may not hit, so it cannot raise Retribution).
pub fn filter_holder_allies(
    server: &GameServer,
    holder: &WorldObject,
    caster: &WorldObject,
    units: Vec<WorldObject>,
) -> Vec<WorldObject> {
    let holder_t = Target::Obj(holder.clone());
    let caster_t = Target::Obj(caster.clone());
    let caster_owner = owner_character(server, caster).map(|p| p.obj_id);
    units
        .into_iter()
        .filter(|u| {
            if u.same(holder) {
                return true;
            }
            if u.same(caster)
                || caster_owner.is_some_and(|c| {
                    u.obj_id() == c || owner_character(server, u).is_some_and(|o| o.obj_id == c)
                })
            {
                return false;
            }
            let t = Target::Obj(u.clone());
            relation_to(server, &holder_t, &t) == RelationState::Friendly
                && !can_attack(server, &holder_t, &t)
                && can_attack(server, &caster_t, &t)
        })
        .collect()
}

/// `SkillTargetingUtil.IsRelationValid(relation, caster, target)`.
pub fn is_relation_valid(
    server: &GameServer,
    relation: SkillTargetRelation,
    caster: &Target,
    target: &Target,
) -> bool {
    match relation {
        SkillTargetRelation::Any | SkillTargetRelation::Party => true,
        SkillTargetRelation::Friendly => {
            relation_to(server, caster, target) == RelationState::Friendly
                && !can_attack(server, caster, target)
        }
        SkillTargetRelation::Hostile => can_attack(server, caster, target),
        SkillTargetRelation::Raid => {
            if caster.same(target) {
                return true;
            }
            let social = server.systems.skills.ext.social();
            let team = social.team_of(server, caster.obj_id());
            team != 0 && social.is_team_obj_member(server, team, target.obj_id())
        }
        SkillTargetRelation::Others => caster.obj_id() != target.obj_id(),
    }
}

#[cfg(test)]
mod tests;
