//! `Skill`: `Use`, `GetInitialTarget` / `SetInitialTarget`, `Cast`, `StartChanneling`,
//! `EndChanneling`, `ScheduleEffects`, `ApplyEffects`, `EndSkill`, `Stop`, `StopSkill`,
//! `RollCombatDice`, `ManaCost` / `ConsumeMana`, plus `Unit.InterruptSkills`, `Unit.UseSkill`,
//! `GetAbLevel`, auto attack (`StartAutoSkill` / `UseAutoAttackSkillTask`,
//! `SkillManager.GetAttackDelay`) and the cast / channel / apply tasks.

use std::sync::Arc;

use rand::Rng;

use crate::models::transform::PositionAndRotation;
use crate::models::unit::{SkillAttribute, UnitAttribute, UnitCooldowns};
use crate::server::GameServer;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::world::{now_ms, WorldObject};

use super::buffs;
use super::combat as unit_combat;
use super::data::{EffectKind, EffectTemplate, SkillEffect, SkillTemplate};
use super::effects::{self, EffectArgs, EffectSource};
use super::ext::TakenCastItem;
use super::geometry::{distance_to, is_front, world_of, world_pos, PosUnit, Target};
use super::packets::{
    CompressedGamePackets, SCCastingStoppedPacket, SCCooldownsPacket, SCSkillCooldownResetPacket,
    SCSkillEndedPacket, SCSkillFiredPacket, SCSkillStartedPacket, SCSkillStoppedPacket,
};
use super::relation::{can_attack, filter_with_relation, owner_character, relation_to};
use super::state::{
    AutoAttackSlot, CancelToken, PlotCooldownSnapshot, SkillRef, SkillRun, SkillTaskKind,
    SkillTaskSlot,
};
use super::stats::{equip_slot, slot_type, stat, stats, StatKind};
use super::types::{
    skill_result, ApplicationMethod, BuffRemoveOn, CastAction, DamageType, RelationState,
    SkillCastTarget, SkillCaster, SkillHitType, SkillObject, SkillTargetRelation,
    SkillTargetSelection, SkillTargetType,
};
use super::{broadcast, unit_reqs};

/// `SkillsEnum.DigUpTreasureChestMarkedOnMap`.
pub const DIG_UP_TREASURE_SKILL: u32 = 18_349;

/// The NPC a `KillNpcWithoutCorpseEffect` removes: the matching-template NPC nearest the caster,
/// restricted to the effect radius when it has one (`None` when none qualifies). Rust-only: the
/// C# effect is a no-op for a character caster, so the port must not pick the first NPC of the
/// template from the other side of the world for a player-cast quest skill.
fn nearest_kill_npc(
    server: &GameServer,
    caster: &WorldObject,
    template_id: u32,
    radius: f32,
) -> Option<crate::world::NpcRef> {
    let world = world_of(server, caster)?;
    let from = Target::Obj(caster.clone());
    let mut best: Option<(f32, crate::world::NpcRef)> = None;
    for npc in world.all_npcs() {
        if npc.data().unit.template_id != template_id {
            continue;
        }
        let to = Target::Obj(WorldObject::from(npc.clone()));
        let distance = distance_to(server, &from, &to, true);
        if radius > 0.0 && distance > radius {
            continue;
        }
        if best.as_ref().is_none_or(|(best, _)| distance < *best) {
            best = Some((distance, npc));
        }
    }
    best.map(|(_, npc)| npc)
}

/// The arguments a cast carries through its tasks.
#[derive(Debug, Clone)]
pub struct CastCtx {
    pub caster: WorldObject,
    pub caster_caster: SkillCaster,
    pub target: Target,
    pub target_caster: SkillCastTarget,
    pub skill_object: SkillObject,
}

impl CastCtx {
    fn has_live_handles(&self, server: &GameServer) -> bool {
        super::geometry::is_live_object(server, &self.caster)
            && !self
                .target
                .as_obj()
                .is_some_and(|target| !super::geometry::is_live_object(server, target))
    }
}

/// `GetAbLevel(ability)`: characters from their ability exp, other units their level (or the
/// owner-provided override).
pub fn ab_level(server: &GameServer, unit: &WorldObject, ability: u8) -> i32 {
    match unit {
        WorldObject::Player(p) => {
            let c = p.data();
            crate::db_character::ab_level(server, &c, ability)
        }
        o => o
            .with_unit(|u| {
                i32::from(
                    u.unit()
                        .skill_state
                        .ab_level_override
                        .unwrap_or(u.unit().level),
                )
            })
            .unwrap_or(0),
    }
}

/// `new Skill(template, owner)`.
pub fn new_skill_for(
    server: &GameServer,
    template: Arc<SkillTemplate>,
    owner: Option<&WorldObject>,
) -> SkillRef {
    let level = match owner {
        // A character's rank is saturated like the stored/learned one: a skill above the
        // current ability level would otherwise wrap to ~255 (`as u8`) and blow up L / T / mana.
        // NPC skill levels stay the C# byte cast.
        Some(o) if o.as_player().is_some() => {
            rank_for_level(&template, ab_level(server, o, template.ability_id))
        }
        Some(o) => SkillRun::level_for(&template, ab_level(server, o, template.ability_id)),
        None => 1,
    };
    SkillRun::new(template, level)
}

/// The 1.2 effective skill level `L` (`FUN_39897fe0`): `ability_level + (rank - 1) *
/// level_step`.
///
/// This is the level the client's own tooltip computation passes to the skill evaluator
/// (`FUN_3941e750`), not the player's ability level: a rank-1 skill uses its own
/// `ability_level`. The client never has a rank below 1 for a usable skill, so a 0 (an
/// unknown/never-learned skill) is read as rank 1.
pub fn effective_level(t: &SkillTemplate, rank: u8) -> i32 {
    t.ability_level + (i32::from(rank.max(1)) - 1) * t.level_step
}

/// The 1.2 rank for a caster level: `(level - ability_level) / level_step + 1`, at least 1;
/// `level_step <= 0` is rank 1 (the client never divides by a zero step).
pub fn rank_for_level(t: &SkillTemplate, level: i32) -> u8 {
    crate::data::skills::stored_skill_level(t.ability_level, t.level_step, level)
}

/// The `casting_inc` rank term of the 1.2 skill evaluator (`FUN_398a7710`):
/// `T = max(0, (L - ability_level) * casting_inc * 0.001)`. It multiplies both the mana
/// `mana_level_md` term and the cast time.
pub fn rank_inc(t: &SkillTemplate, level: i32) -> f64 {
    (f64::from(level - t.ability_level) * f64::from(t.casting_inc) * 0.001).max(0.0)
}

/// The 1.2 skill level `L` a cast evaluates at (`FUN_3941e750` -> `FUN_39897fe0`):
///
/// - a character's class skill (`ability_id != 0`) uses the learned rank on the `SkillRun`;
/// - an `ability_id == 0` skill takes the rank from the character's level
///   (`(charLevel - ability_level) / level_step + 1`), learned or not;
/// - a non-character caster (an NPC) keeps the effective level the old `Skill.ManaCost` used
///   as its `LevelMana` argument: the unit level or `ab_level_override`, whatever the
///   `level_step` (the server-side NPC cost is not client-visible; capture-pending).
pub fn cast_skill_level(server: &GameServer, skill: &SkillRun, caster: &WorldObject) -> i32 {
    let t = &skill.template;
    if caster.as_player().is_none() {
        ab_level(server, caster, t.ability_id)
    } else if t.ability_id == 0 {
        effective_level(t, rank_for_level(t, ab_level(server, caster, 0)))
    } else {
        effective_level(t, skill.level)
    }
}

/// The `(L, T)` pair of a cast: [`cast_skill_level`] and [`rank_inc`]. The mana charge, the
/// mana pre-check and the plot `ManaCost` effect all read this pair, so a cast the pre-check
/// allows is charged exactly the value it checked.
pub fn level_and_inc(server: &GameServer, skill: &SkillRun, caster: &WorldObject) -> (i32, f64) {
    let level = cast_skill_level(server, skill, caster);
    (level, rank_inc(&skill.template, level))
}

/// Mana per `mana_level_md` point at skill level `level`: the character `unit_formulas` rows
/// `LevelDps` (kind 12, `(level * 1.6 + 8) * 3`) and `LevelMana` (kind 13,
/// `level_dps / 3.65`) of `compact.sqlite3`, evaluated through the precompiled formula
/// expressions (no parsing per cast). 0 when the data has no such formulas.
///
/// Deviation: the C# `Skill.ManaCost` hardcodes `((abLevel - 1) * 1.6 + 8) * 3 / 3.65` with the
/// caster's ability level; the 1.2 client evaluates the data formulas at the skill level `L`
/// ([`effective_level`]), so the data and the client win (see PORTING.md).
pub fn mana_level_scale(server: &GameServer, level: i32) -> f64 {
    use crate::data::formulas::{FormulaOwnerType, UnitFormulaKind};
    let formulas = &server.game_data.formulas;
    let (Some(dps), Some(mana)) = (
        formulas.get_unit_formula(FormulaOwnerType::Character, UnitFormulaKind::LevelDps),
        formulas.get_unit_formula(FormulaOwnerType::Character, UnitFormulaKind::LevelMana),
    ) else {
        return 0.0;
    };
    let level_dps = dps.formula.evaluate(&std::collections::HashMap::from([(
        "ab_level",
        f64::from(level),
    )]));
    mana.formula
        .evaluate(&std::collections::HashMap::from([("level_dps", level_dps)]))
}

/// `Skill.ManaCost(caster)`: `TRUNC(mana_cost + mana_level_md * (1 + T) * LevelMana(L))`, then
/// the caster's skill modifiers (`FUN_398a01b0` kind 0, then `FUN_3989f900`).
///
/// The original r208022 client truncates the final value, not rounds it: 68/68 tooltip
/// readings reproduce exactly, 32 of them with a fractional mana part >= 0.5 (see PORTING.md).
pub fn mana_cost(server: &GameServer, skill: &SkillRun, caster: &WorldObject) -> i32 {
    let t = &skill.template;
    let (level, inc) = level_and_inc(server, skill, caster);
    let base = mana_level_scale(server, level);
    let cost2 = f64::from(t.mana_cost) + f64::from(t.mana_level_md) * (1.0 + inc) * base;
    let tags = server.game_data.systems.skills.skill_tags(t.id).to_vec();
    caster
        .with_unit(|u| {
            u.unit()
                .skill_modifiers
                .apply(t.id, &tags, SkillAttribute::ManaCost, cost2)
        })
        .unwrap_or(cost2) as i32
}

/// The 1.2 cast time of a skill (`FUN_398a01b0` kind 4): `(int)(casting_time * (1 + T))`,
/// then the caster's skill modifiers, then the cast-speed stat `(stat + 1000) / 1000`
/// ([`StatKind::CastTimeMul`]), truncated to ms. `CastTimeMultiplier` is applied separately
/// by the caller.
pub fn cast_time_ms(server: &GameServer, skill: &SkillRun, caster: &WorldObject) -> i32 {
    let t = &skill.template;
    if t.casting_time <= 0 {
        return 0;
    }
    let (_, inc) = level_and_inc(server, skill, caster);
    let base = (f64::from(t.casting_time) * (1.0 + inc)) as i32;
    let mul = stat(server, caster, StatKind::CastTimeMul);
    (mul * apply_skill_modifiers(
        server,
        skill,
        caster,
        SkillAttribute::CastTime,
        f64::from(base),
    )) as i32
}

/// `BaseUnit.ApplySkillModifiers(skill, attribute, value)`.
pub fn apply_skill_modifiers(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
    attr: SkillAttribute,
    value: f64,
) -> f64 {
    let tags = server
        .game_data
        .systems
        .skills
        .skill_tags(skill.template.id)
        .to_vec();
    caster
        .with_unit(|u| {
            u.unit()
                .skill_modifiers
                .apply(skill.template.id, &tags, attr, value)
        })
        .unwrap_or(value)
}

/// Network grace of the global-cooldown and per-use throttle checks in [`use_skill`]: a request
/// that arrives with at most this much of the cooldown left is still accepted, so client/server
/// clock jitter does not refuse a legitimate press. Same idea as the 250 ms tolerance of
/// `UnitCooldowns::check_cooldown`, but tighter so an over-eager client cannot press early.
const GCD_GRACE_MS: i64 = 150;

/// Sets the caster's global cooldown from the skill template (`default_gcd` / `custom_gcd`
/// scaled by the caster's `GlobalCooldownMul`). Skipped when the skill bypasses the GCD.
///
/// `cast` starts it, except for a player's timed cast, which `use_skill` starts at acceptance
/// because the 1.2 client counts the GCD from the cast start (also when the cast is cancelled).
fn start_global_cooldown(server: &GameServer, skill: &SkillRef, caster: &WorldObject, now: i64) {
    if skill.state().bypass_gcd {
        return;
    }
    let t = &skill.template;
    let mut gcd = t.custom_gcd;
    if t.default_gcd {
        gcd = if caster.as_npc().is_some() {
            1500
        } else {
            1000
        };
    }
    let mul = stat(server, caster, StatKind::GlobalCooldownMul) as f32;
    let ms = (gcd as f32 * (mul / 100.0)) as i64;
    caster.with_unit(|u| u.unit_mut().global_cooldown_ms = now + ms);
}

fn cooldown_ignored(server: &GameServer, caster: &WorldObject) -> bool {
    caster
        .as_player()
        .cloned()
        .or_else(|| owner_character(server, caster))
        .is_some_and(|owner| owner.data().skill_char.ignore_skill_cooldowns)
}

fn on_skill_cooldown(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
    now: i64,
) -> bool {
    // Preserve the existing server-controlled NPC policy: only a character or its
    // controlled unit is subject to skill cooldown admission, on that unit's own store.
    let owner = caster
        .as_player()
        .cloned()
        .or_else(|| owner_character(server, caster));
    let Some(owner) = owner else {
        return false;
    };
    if owner.data().skill_char.ignore_skill_cooldowns {
        return false;
    }
    caster
        .with_unit(|u| {
            u.unit_mut().cooldowns.check_skill_cooldown(
                skill.template.id,
                skill.template.cooldown_tag_id.max(0) as u32,
                now,
            )
        })
        .unwrap_or(false)
}

fn start_skill_cooldown(server: &GameServer, skill: &SkillRun, caster: &WorldObject, now: i64) {
    if cooldown_ignored(server, caster) {
        return;
    }
    // Native 1.2 `3989A360` -> `3989F900`, attribute 1: modify the started skill's
    // own cooldown, not each member's authored duration. Never cast a signed duration to u32.
    let duration = apply_skill_modifiers(
        server,
        skill,
        caster,
        SkillAttribute::Cooldown,
        f64::from(skill.template.cooldown_time),
    )
    .clamp(0.0, f64::from(u32::MAX)) as u32;
    if duration == 0 {
        return;
    }
    let tag = skill.template.cooldown_tag_id.max(0) as u32;
    // Native `3939b010` (OnSkillFired) starts the client-side cooldown itself when the packet's
    // f bit0 (`+0x154`, DEC 750636) and trailing flag bit1 (`+0x160`, DEC 750694) are 0 and the
    // template has `toggle_buff_id == 0` (`+0x1c0`, compared with the zero-initialised
    // DAT_3a17770c) and `timing_id != 0` (`+0x1bc`). Toggle skills' cooldown is started by
    // `OnBuffRemoved` (`39389b70` -> `39368f90`). A `plot_only` cast with a plot never sends
    // `SCSkillFired`, so the client cannot start it and the server must send the update.
    // Sending an entry the client already appended would duplicate it: `39899d40` does not
    // de-duplicate and drops additions once the list holds 150.
    let fires = !(skill.template.plot_only && skill.template.plot_id.is_some());
    let client_starts_cooldown =
        fires && (skill.template.toggle_buff_id != 0 || skill.template.timing_id != 0);
    let packet = caster.with_unit(|u| {
        let cooldowns = &mut u.unit_mut().cooldowns;
        cooldowns.add_cooldown(skill.template.id, duration, now);
        cooldowns.add_tag_cooldown(tag, duration, now);
        // Native 1.2 397B2350 keeps these lists separate; 3989A290 takes
        // max(individual, tag). Never manufacture individual entries for other members.
        // A client-started skill needs no packet at all: `39899f90` writes the individual entry
        // or the tag entry from the client's own template, so sending either one duplicates it.
        let mut affected = UnitCooldowns::default();
        if !client_starts_cooldown {
            affected
                .cooldowns
                .insert(skill.template.id, cooldowns.cooldowns[&skill.template.id]);
            if tag != 0 {
                affected
                    .tag_cooldowns
                    .insert(tag, cooldowns.tag_cooldowns[&tag]);
            }
        }
        SCCooldownsPacket::from_cooldowns(&affected, now)
    });
    if let Some(packet) = packet.filter(|p| !p.is_empty()) {
        // OnCooldowns (native 391D43C0) addresses the local character's manager;
        // unlike SkillCooldownReset, the packet has no subject object ID. A mate's
        // shared timers must not populate its owner's character cooldown store.
        if let Some(player) = caster.as_player() {
            player.send(packet);
        }
    }
}

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
fn initial_target(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
    skill_caster: &SkillCaster,
    target_caster: &mut SkillCastTarget,
) -> Option<Target> {
    if !caster.is_unit() {
        return None;
    }
    let mut target: Option<WorldObject> = Some(caster.clone());
    if matches!(
        skill_caster,
        SkillCaster::Mount { .. } | SkillCaster::Unit(_)
    ) {
        target = world_of(server, caster).and_then(|w| w.unit(skill_caster.obj_id()));
    }
    let tt = SkillTargetType::from_raw(i64::from(skill.template.target_type));
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
                if r != RelationState::Hostile
                    && r != RelationState::Neutral
                    && !can_attack(server, &caster_t, &tt)
                {
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

/// `Skill.Use(caster, casterCaster, targetCaster, skillObject, bypassGcd, out value)`:
/// (`SkillResult`, extra value).
pub fn use_skill(
    server: &GameServer,
    skill: &SkillRef,
    caster: &WorldObject,
    caster_caster: SkillCaster,
    target_caster: &mut SkillCastTarget,
    skill_object: Option<SkillObject>,
    bypass_gcd: bool,
) -> (u8, u32) {
    use_skill_impl(
        server,
        skill,
        caster,
        caster_caster,
        target_caster,
        skill_object,
        bypass_gcd,
        false,
    )
}

/// [`use_skill`] with a check-only mode: every refusal is returned exactly as a real use would
/// return it, but a passing check stops before the first cast-state mutation.
#[allow(clippy::too_many_arguments)]
fn use_skill_impl(
    server: &GameServer,
    skill: &SkillRef,
    caster: &WorldObject,
    caster_caster: SkillCaster,
    target_caster: &mut SkillCastTarget,
    skill_object: Option<SkillObject>,
    bypass_gcd: bool,
    dry_run: bool,
) -> (u8, u32) {
    if !caster.is_unit() {
        tracing::debug!(
            skill_id = skill.id,
            "Skill use refused: the caster is not a unit"
        );
        return (skill_result::INVALID_SOURCE, 0);
    }
    if let Err(result) = super::validation::check_item_zone(server, caster, &caster_caster) {
        tracing::debug!(
            skill_id = skill.id,
            result = ?result,
            "Skill use refused: the item cannot be used in this zone"
        );
        return result;
    }
    let template = skill.template.clone();
    // The client refuses a cap-scale cast on an `item_cap_scale_forbids` target before it is
    // sent (`FUN_3939dc20` -> SkillResult INVALID_TARGET 0x22); a forged cast is refused here
    // before the source item is taken, and answers `SCSkillStarted` with result 0x22.
    if let Err(result) = super::validation::check_cap_scale_target(
        server,
        caster,
        &template,
        &caster_caster,
        target_caster,
    ) {
        return result;
    }
    let resolved_target = initial_target(server, skill, caster, &caster_caster, target_caster);
    let Some(target) = &resolved_target else {
        if let Some(npc) = caster.as_npc() {
            server
                .systems
                .skills
                .ext
                .npcs()
                .npc_no_aggro_target(server, npc);
        }
        return (skill_result::NO_TARGET, 0);
    };
    if let Err(result) = super::validation::check(server, skill, caster, target)
        .and_then(|()| super::validation::check_build_materials(server, skill, caster, target))
    {
        tracing::debug!(
            skill_id = skill.id,
            result = ?result,
            target = target.obj_id(),
            "Skill use refused: the source / target validation failed"
        );
        return result;
    }
    // The 1.2 fishing reactions require the caster's own spawned NPC. OwnerId is
    // the character id; using an object id here would accept unrelated NPCs or
    // reject the fisherman. Resolve this before cast state, GCD or buff mutations.
    let owned_target = if template.target_my_npc {
        let Some(player) = caster.as_player() else {
            return (unit_reqs::skill_result_for_key("skill_not_my_npc"), 0);
        };
        let target = resolved_target.clone();
        let owner = target
            .as_ref()
            .and_then(Target::as_obj)
            .and_then(WorldObject::as_npc)
            .map(|npc| npc.data().unit.owner_id);
        if player.character_id == 0 || owner != Some(player.character_id) {
            return (unit_reqs::skill_result_for_key("skill_not_my_npc"), 0);
        }
        target
    } else {
        resolved_target
    };
    let housing_target = (template.target_type == SkillTargetType::Building as i32)
        .then(|| initial_target(server, skill, caster, &caster_caster, target_caster))
        .flatten();
    if owned_target
        .as_ref()
        .or(housing_target.as_ref())
        .is_some_and(|target| {
            !crate::systems::housing::dominion::runtime::check_skill(server, skill, caster, target)
        })
    {
        return (skill_result::INVALID_TARGET, 0);
    }
    let character = caster.as_player().cloned();
    let req = unit_reqs::can_use_skill_on_target(
        server,
        &template,
        caster,
        &caster_caster,
        owned_target.as_ref().and_then(Target::as_obj),
    );
    if !req.is_ok() {
        if let Some(c) = &character {
            let c = c.data();
            tracing::warn!(
                "{} ({}) failed requirements to use skill {} - {}",
                c.unit.name,
                c.id,
                template.id,
                req.key
            );
        }
        skill.state().cancelled = true;
        return (unit_reqs::skill_result_for_key(req.key), req.value);
    }
    // Resolve buff restrictions before cooldown writes or StartSkill buff removal:
    // removing a restrictive buff first would let the forbidden skill bypass it.
    let checked_target = if !server
        .game_data
        .systems
        .skills
        .buff_skill_requirements
        .is_empty()
    {
        let target = owned_target
            .or_else(|| initial_target(server, skill, caster, &caster_caster, target_caster));
        if let Some(req) = super::buff_reqs::failed_requirement(
            server,
            &template,
            caster,
            target.as_ref().and_then(Target::as_obj),
        ) {
            skill.state().cancelled = true;
            return (skill_result::SKILL_REQ_FAIL, req);
        }
        target
    } else {
        owned_target
    };
    let active_task = caster
        .with_unit(|u| u.unit().skill_state.skill_task.clone())
        .flatten();
    if let Some(task) = &active_task {
        match task.kind {
            SkillTaskKind::Cast => {
                tracing::debug!(
                    skill_id = skill.id,
                    active_skill = task.skill.template.id,
                    "Skill use refused: the caster is already casting"
                );
                return (unit_reqs::skill_result_for_key("skill_on_casting"), 0);
            }
            SkillTaskKind::EndChanneling { .. }
                if !task.skill.template.stop_channeling_on_start_skill =>
            {
                tracing::debug!(
                    skill_id = skill.id,
                    active_skill = task.skill.template.id,
                    "Skill use refused: the caster is channeling"
                );
                return (
                    unit_reqs::skill_result_for_key("skill_cannot_cast_in_channeling"),
                    0,
                );
            }
            _ => {}
        }
    }
    if let Err(result) = super::validation::check_resources(server, skill, caster) {
        tracing::debug!(
            skill_id = skill.id,
            result = ?result,
            "Skill use refused: the caster lacks the resources"
        );
        return result;
    }
    if let Some(c) = &character {
        if let Some(result) = portal_zone_check(server, &template, c, skill_object.as_ref()) {
            return (result, 0);
        }
    }
    let now = now_ms();
    if on_skill_cooldown(server, skill, caster, now) {
        tracing::debug!(
            skill_id = skill.id,
            "Skill use refused: the skill is on cooldown"
        );
        return (skill_result::COOLDOWN_TIME, 0);
    }
    skill.state().bypass_gcd = bypass_gcd;
    if !bypass_gcd {
        let delay: i64 = if matches!(skill.id, 2..=4) {
            if character.is_some() {
                500
            } else {
                1500
            }
        } else {
            150
        };
        let (last_used, gcd) = caster
            .with_unit(|u| (u.unit().skill_last_used_ms, u.unit().global_cooldown_ms))
            .unwrap_or((0, 0));
        // Deviation from C# (`Skill.Use` + `Helper.CheckInterval`): the C# only refused a second
        // attempt inside the 150 ms interval, so the first press during any GCD got through.
        // Refuse outright while more than the network grace of the cooldown is left.
        if last_used + delay - now > GCD_GRACE_MS {
            tracing::debug!(
                skill_id = skill.id,
                last_used,
                now,
                "Skill use refused: the previous skill was used too recently"
            );
            return (skill_result::COOLDOWN_TIME, 0);
        }
        if !template.ignore_global_cooldown && gcd - now > GCD_GRACE_MS {
            tracing::debug!(
                skill_id = skill.id,
                global_cooldown_ms = gcd,
                now,
                "Skill use refused: the global cooldown is still running"
            );
            return (skill_result::COOLDOWN_TIME, 0);
        }
    }
    if dry_run {
        return (skill_result::SUCCESS, 0);
    }
    if let Some(task) = active_task {
        if let SkillTaskKind::EndChanneling { channel_doodad } = task.kind {
            stop(server, &task.skill, caster, channel_doodad);
        }
    }
    if !bypass_gcd {
        caster.with_unit(|u| u.unit_mut().skill_last_used_ms = now);
    }
    if let Some(c) = &character {
        c.data().skill_char.skill_cancelled = false;
    }
    {
        let mut state = skill.state();
        state.generation = state.generation.wrapping_add(1);
        state.cancelled = false;
        state.effect_cancelled = false;
        state.music_started_pending = false;
        state.gcd_started = false;
        state.plot_cooldown_snapshot = None;
        state.hit_types.clear();
    }
    caster.with_unit(|u| u.unit_mut().skill_state.condition_chance = true);
    if template.cancel_ongoing_buffs {
        // `skills.keep_stealth`: a stealth skill keeps the caster's stealth buffs through its
        // own start/use cancel loop (see `buffs::trigger_remove_on_keep_stealth`).
        let trigger = |on| {
            if template.keep_stealth {
                buffs::trigger_remove_on_keep_stealth(
                    server,
                    caster,
                    on,
                    template.cancel_ongoing_buff_exception_tag_id,
                );
            } else {
                buffs::trigger_remove_on(
                    server,
                    caster,
                    on,
                    template.cancel_ongoing_buff_exception_tag_id,
                );
            }
        };
        if caster.as_mate().is_some() {
            trigger(BuffRemoveOn::UseSkill);
        }
        trigger(BuffRemoveOn::StartSkill);
    }
    let skill_object = skill_object.unwrap_or_default();
    let Some(target) = checked_target
        .or_else(|| initial_target(server, skill, caster, &caster_caster, target_caster))
    else {
        if let Some(n) = caster.as_npc() {
            server
                .systems
                .skills
                .ext
                .npcs()
                .npc_no_aggro_target(server, n);
        }
        tracing::trace!(
            "Skill: SkillResult.NoTarget! - Skill {}, Caster {}",
            template.id,
            caster.obj_id()
        );
        skill.state().initial_target = 0;
        return (skill_result::NO_TARGET, 0);
    };
    skill.state().initial_target = target.obj_id();
    // Unmount character if skill asks for it (vehicles subsystem)
    if template.unmount {
        if let Some(p) = caster.as_player() {
            crate::systems::vehicles::mates::unmount_riding_character(server, p, true);
        }
    }
    let tl = server
        .systems
        .skills
        .tl_ids
        .next_id(caster.obj_id(), template.id);
    // A reused `SkillRun` (auto attack) can still hold an id when its previous use never ended
    // (a leak path). Recover it so the pool does not run out; the counter makes it visible.
    let previous = std::mem::replace(&mut skill.state().tl_id, tl);
    if previous != 0 {
        server.systems.skills.tl_ids.release_replaced(previous);
    }
    tracing::debug!(
        "Created SkillTlId {tl} for Skill {}, Caster {} with target {}",
        template.id,
        caster.obj_id(),
        target.obj_id()
    );
    let ctx = CastCtx {
        caster: caster.clone(),
        caster_caster,
        target: target.clone(),
        target_caster: *target_caster,
        skill_object: skill_object.clone(),
    };
    if template.plot_id.is_some() {
        // Plot-only skills never reach `apply_effects`: verify the cast item is still the same
        // owned item before the plot runs (the plot itself consumes plot reagents).
        if let (Some(p), SkillCaster::Item { item_id, .. }) = (caster.as_player(), caster_caster) {
            let items = server.systems.skills.ext.items();
            let still_owned =
                items
                    .owned_item(server, p, item_id)
                    .is_some_and(|(template_id, _)| {
                        items.consume_owned_item(server, p, item_id, template_id, 0) != Some(false)
                    });
            if !still_owned {
                skill.state().cancelled = true;
                return (skill_result::INVALID_SOURCE, 0);
            }
        }
    }
    if let Some(plot_id) = template.plot_id {
        if template.plot_only {
            // Plot-only skills never enter `cast`; commit cooldowns once at acceptance,
            // before a plot can finish synchronously. A plot cancelled while still casting
            // gives them back (`refund_cancelled_plot_cooldown`).
            let tag = template.cooldown_tag_id.max(0) as u32;
            let snapshot = caster.with_unit(|u| {
                let u = u.unit_mut();
                PlotCooldownSnapshot {
                    skill_cooldown_end: u.cooldowns.cooldowns.get(&template.id).copied(),
                    tag_id: tag,
                    tag_cooldown_end: u.cooldowns.tag_cooldowns.get(&tag).copied(),
                    global_cooldown_ms: u.global_cooldown_ms,
                }
            });
            skill.state().plot_cooldown_snapshot = snapshot;
            start_global_cooldown(server, skill, caster, now);
            start_skill_cooldown(server, skill, caster, now);
        }
        super::plots::run_plot(server, plot_id, ctx.clone(), skill.clone());
        if template.plot_only {
            return (skill_result::SUCCESS, 0);
        }
    }
    // Casting time
    let mut cast_time = cast_time_ms(server, skill, caster);
    let ctm = skill.state().cast_time_multiplier;
    cast_time = (f64::from(cast_time) * f64::from(ctm)).round_ties_even() as i32;
    if cast_time > 0 {
        // The 1.2 client measures a player's global cooldown from the cast start, so start it
        // now; `cast` would only start it at the cast end, refusing a skill the client already
        // shows as ready. NPC casts keep the cast-end start (their AI pacing relies on it).
        if caster.as_npc().is_none() {
            start_global_cooldown(server, skill, caster, now);
            skill.state().gcd_started = true;
        }
        let mut p = SCSkillStartedPacket::new(
            skill.id,
            tl,
            ctx.caster_caster,
            ctx.target_caster,
            ctx.skill_object.clone(),
        );
        p.base_cast_time_div10 = (cast_time / 10) as u16;
        p.real_cast_time_div10 = (cast_time / 10) as u16;
        broadcast(server, caster, &p, true);
        let cancel = CancelToken::new();
        caster.with_unit(|u| {
            u.unit_mut().skill_state.skill_task = Some(SkillTaskSlot {
                kind: SkillTaskKind::Cast,
                skill: skill.clone(),
                cancel: cancel.clone(),
                extend_ms: 0,
            })
        });
        schedule_cast_completion(
            server,
            skill.clone(),
            ctx.clone(),
            cancel,
            i64::from(cast_time),
        );
    } else {
        if is_player_music_skill(server, &template, caster) {
            skill.state().music_started_pending = true;
        }
        cast(server, skill, &ctx);
    }
    (skill_result::SUCCESS, 0)
}

/// The `AccessLevel < 100` checks of `Skill.Use` for `Return` / `OpenPortalEffect` skills.
fn portal_zone_check(
    server: &GameServer,
    template: &SkillTemplate,
    c: &crate::world::PlayerRef,
    skill_object: Option<&SkillObject>,
) -> Option<u8> {
    let (access, district, faction, instance) = {
        let d = c.data();
        (
            d.access_level,
            d.return_district_id,
            d.unit.faction_id,
            d.unit.transform.instance_id,
        )
    };
    if access >= 100 {
        return None;
    }
    let data = &server.game_data.systems.skills;
    for effect in &template.effects {
        let Some((kind, id)) = effect.template else {
            continue;
        };
        let mut destination_instance = instance;
        let destination = match data.effect(kind, id) {
            Some(EffectTemplate::Special(s)) if s.special_effect_type_id == 25 => {
                if s.value1 > 0 {
                    crate::systems::npcs::portal::world_gates_by_id(server, s.value1 as u32)
                        .cloned()
                        .or_else(|| {
                            if instance != 0 {
                                return None;
                            }
                            let t = c.data().main_world_position?;
                            destination_instance = t.instance_id;
                            Some(crate::data::world_data::Portal {
                                zone_id: t.zone_id,
                                x: t.x,
                                y: t.y,
                                z: t.z,
                                ..Default::default()
                            })
                        })
                } else {
                    destination_instance = crate::models::character::DEFAULT_INSTANCE_ID;
                    let rp = server
                        .game_data
                        .portals
                        .district_return_point(district, faction);
                    server
                        .game_data
                        .portals
                        .recalls
                        .values()
                        .flatten()
                        .find(|p| p.id == rp)
                        .cloned()
                        .or_else(|| {
                            if rp == 0 || instance != 0 {
                                return None;
                            }
                            let t = c.data().main_world_position?;
                            destination_instance = t.instance_id;
                            Some(crate::data::world_data::Portal {
                                zone_id: t.zone_id,
                                x: t.x,
                                y: t.y,
                                z: t.z,
                                ..Default::default()
                            })
                        })
                }
            }
            Some(EffectTemplate::OpenPortal { .. }) => {
                if instance != 0 {
                    return Some(skill_result::INVALID_LOCATION);
                }
                match skill_object {
                    Some(SkillObject::PortalInfo { id, .. }) => {
                        crate::systems::npcs::portal::portal_info(&c.data().portals, *id as u32)
                            .cloned()
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(p) = destination {
            if !crate::world::session::zone_access_allowed(
                server,
                c,
                destination_instance,
                p.zone_id,
                p.x,
                p.y,
            ) {
                return Some(skill_result::NO_PERM);
            }
        }
    }
    None
}

/// `Skill.Cast(caster, casterCaster, target, targetCaster, skillObject)`.
pub fn cast(server: &GameServer, skill: &SkillRef, ctx: &CastCtx) {
    let caster = &ctx.caster;
    if !caster.is_unit() {
        return;
    }
    if skill.cancelled() {
        // A cancellation path that did not run `stop` (the id would otherwise be lost). The
        // task slot of a timed cast is cleared here too (`stop` clears it on its own path).
        caster.with_unit(|u| {
            let slot = &mut u.unit_mut().skill_state.skill_task;
            if slot
                .as_ref()
                .is_some_and(|task| Arc::ptr_eq(&task.skill, skill))
            {
                *slot = None;
            }
        });
        end_skill(server, skill, caster);
        return;
    }
    // Source, target and resources may change while the cast timer is running.
    let ready = caster
        .as_player()
        .and_then(|p| portal_zone_check(server, &skill.template, p, Some(&ctx.skill_object)))
        .map_or(Ok(()), |result| Err((result, 0)))
        .and_then(|()| super::validation::check_item_zone(server, caster, &ctx.caster_caster))
        .and_then(|()| super::validation::check(server, skill, caster, &ctx.target))
        .and_then(|()| {
            if on_skill_cooldown(server, skill, caster, now_ms()) {
                Err((skill_result::COOLDOWN_TIME, 0))
            } else {
                Ok(())
            }
        })
        .and_then(|()| super::validation::check_resources(server, skill, caster))
        .and_then(|()| {
            super::validation::check_build_materials(server, skill, caster, &ctx.target)
        });
    if let Err((result, value)) = ready {
        if let Some(player) = caster.as_player() {
            let mut packet = SCSkillStartedPacket::new(
                skill.id,
                skill.tl_id(),
                ctx.caster_caster,
                ctx.target_caster,
                ctx.skill_object.clone(),
            );
            packet.set_skill_result(result).set_result_uint(value);
            player.send(packet);
            // A craft cast that fails here never reaches `CraftEffect`: end the craft batch.
            server
                .systems
                .skills
                .ext
                .items()
                .craft_cast_failed(server, player, skill.id, result);
        }
        stop(server, skill, caster, 0);
        return;
    }
    let t = &skill.template;
    if caster.as_npc().is_some() && t.skill_controller_id != 0 {
        if let Some(sc) = server
            .game_data
            .systems
            .skills
            .skill_controller(t.skill_controller_id)
        {
            let dist = super::geometry::raw_distance(
                &world_pos(server, &Target::Obj(caster.clone())),
                &world_pos(server, &ctx.target),
                true,
            );
            if dist >= t.min_range as f32 && dist <= t.max_range as f32 {
                if let Some(sc) =
                    super::controllers::create(server, sc, caster, &ctx.target, 0.0, 0.0, 0.0, 0.0)
                {
                    super::controllers::activate(server, &sc);
                }
            }
        }
    }
    // Validate the cast item before committing mana or cooldowns.
    if let (
        Some(p),
        SkillCaster::Item {
            item_id,
            item_template_id,
            ..
        },
    ) = (caster.as_player(), ctx.caster_caster)
    {
        let items = server.systems.skills.ext.items();
        if items.use_skill_as_reagent(server, item_template_id) {
            match items.owned_item(server, p, item_id) {
                None => {
                    tracing::warn!(
                        "SkillItem does not exists {item_id} (templateId: {item_template_id})"
                    );
                    // The C# returned here and lost the timeline id (issue #753); stop instead.
                    stop(server, skill, caster, 0);
                    return;
                }
                Some((tid, _)) => {
                    if items.item_count(server, p, tid) < 1 {
                        tracing::warn!("SkillItem, player does not own enough of {item_id}");
                        stop(server, skill, caster, 0);
                        return;
                    }
                }
            }
        }
    }
    let now = now_ms();
    // A player's timed cast started the global cooldown at acceptance (see `use_skill`).
    if !std::mem::take(&mut skill.state().gcd_started) {
        start_global_cooldown(server, skill, caster, now);
    }
    // Clear only this skill's task: the timer claims the slot before calling `cast`, and a new
    // cast of another skill (or a reused auto-attack skill) must keep its own slot.
    caster.with_unit(|u| {
        let slot = &mut u.unit_mut().skill_state.skill_task;
        if slot
            .as_ref()
            .is_some_and(|task| Arc::ptr_eq(&task.skill, skill))
        {
            *slot = None;
        }
    });
    consume_mana(server, skill, caster);
    start_skill_cooldown(server, skill, caster, now);
    if t.channeling_time > 0 {
        start_channeling(server, skill, ctx);
    } else {
        schedule_effects(server, skill, ctx);
    }
}

/// What the cast timer found under the caster's task lock.
enum CastTimerAction {
    /// The task still owns the slot and owes no further wait: this call completes the cast.
    Complete,
    /// The task owns the slot and a delayed `DisturbCasting` added a wait: wake again then.
    Rearm(i64),
    /// The slot is gone or belongs to another task: a stop, a completion or a newer cast won.
    Stale,
}

/// Arms the completion timer of a direct cast. The timer reads the cast's pending `extend_ms`
/// from the caster's task slot under the slot lock; [`delay_cast`] adds to it, so a timer that
/// wakes while a delay is pending re-arms itself for that wait instead of completing. Both
/// paths act on the same slot under one lock, so a cast completes exactly once and a cancelled
/// cast never does. (A wall-clock deadline would not follow a paused test clock.)
fn schedule_cast_completion(
    server: &GameServer,
    skill: SkillRef,
    ctx: CastCtx,
    cancel: CancelToken,
    wait_ms: i64,
) {
    let skill2 = skill.clone();
    let ctx2 = ctx.clone();
    let cancel2 = cancel.clone();
    let scheduled = super::tasks::schedule(server, wait_ms, move |srv| {
        cast_completion_tick(&srv, &skill2, &ctx2, &cancel2);
    });
    if !scheduled {
        // No scheduler (server shutting down): finish the cast right away instead of leaving
        // the task slot - and the timeline id - set forever.
        cast(server, &skill, &ctx);
    }
}

/// `CastTask.Execute`: completes the cast once its (possibly delayed) end time is reached.
fn cast_completion_tick(
    server: &GameServer,
    skill: &SkillRef,
    ctx: &CastCtx,
    cancel: &CancelToken,
) {
    // A cancelled token means `stop` ran: it took the task slot and released the timeline id.
    // A skill flagged `cancelled` by something else (a cancel effect, a failed requirement)
    // still holds its id: `cast` ends it, exactly once (`end_skill` takes the id).
    if cancel.is_cancelled() {
        return;
    }
    let action = ctx
        .caster
        .with_unit(|u| {
            let slot = &mut u.unit_mut().skill_state.skill_task;
            let Some(task) = slot.as_mut() else {
                return CastTimerAction::Stale;
            };
            if task.kind != SkillTaskKind::Cast
                || !Arc::ptr_eq(&task.skill, skill)
                || task.cancel != *cancel
            {
                return CastTimerAction::Stale;
            }
            if task.extend_ms > 0 {
                return CastTimerAction::Rearm(std::mem::take(&mut task.extend_ms));
            }
            // Claim the completion under the same lock `delay_cast` extends under: an extension
            // that runs after this sees no task and does not announce a delay for a cast that
            // is already ending.
            *slot = None;
            CastTimerAction::Complete
        })
        .unwrap_or(CastTimerAction::Stale);
    match action {
        CastTimerAction::Complete => cast(server, skill, ctx),
        CastTimerAction::Rearm(wait_ms) => {
            schedule_cast_completion(server, skill.clone(), ctx.clone(), cancel.clone(), wait_ms);
        }
        CastTimerAction::Stale => {}
    }
}

/// `Skill.StartChanneling`.
pub fn start_channeling(server: &GameServer, skill: &SkillRef, ctx: &CastCtx) {
    skill.state().channel_target = ctx
        .target
        .as_obj()
        .map(|target| (target.obj_id(), target.core() as *const _ as usize));
    let caster = &ctx.caster;
    let t = &skill.template;
    // Native 1.2 sub_398A01B0, attribute 11: milliseconds, independent of CastTime.
    // The fired packet encodes duration as signed i16 (ms / 10 + 10).
    let channeling_time = apply_skill_modifiers(
        server,
        skill,
        caster,
        SkillAttribute::ChannelingTime,
        f64::from(t.channeling_time),
    )
    .max(0.0)
    .min(f64::from((i32::from(i16::MAX) - 10) * 10)) as i32;
    let data = &server.game_data.systems.skills;
    if t.channeling_buff_id != 0 {
        if let Some(b) = data.buff(t.channeling_buff_id).cloned() {
            buffs::apply_template(
                server,
                &b,
                Some(caster),
                &ctx.caster_caster,
                caster,
                Some(skill),
            );
        }
    }
    if t.channeling_target_buff_id != 0 {
        if let Some(b) = data.buff(t.channeling_target_buff_id).cloned() {
            if let Some(target) = ctx.target.as_obj() {
                buffs::apply_template(
                    server,
                    &b,
                    Some(caster),
                    &ctx.caster_caster,
                    target,
                    Some(skill),
                );
            }
        }
    }
    let doodad = if t.channeling_doodad_id > 0 {
        server
            .systems
            .skills
            .ext
            .npcs()
            .spawn_channel_doodad(server, caster, t.channeling_doodad_id)
            .unwrap_or(0)
    } else {
        0
    };
    let tl = skill.tl_id();
    broadcast(
        server,
        caster,
        &SCSkillFiredPacket {
            id: skill.id,
            tl,
            caster: ctx.caster_caster,
            target: ctx.target_caster,
            skill_object: ctx.skill_object.clone(),
            computed_delay: 0,
            channeling_time,
            fire_anim_id: t.fire_anim_id,
        },
        true,
    );
    let cancel = CancelToken::new();
    caster.with_unit(|u| {
        u.unit_mut().skill_state.skill_task = Some(SkillTaskSlot {
            kind: SkillTaskKind::EndChanneling {
                channel_doodad: doodad,
            },
            skill: skill.clone(),
            cancel: cancel.clone(),
            extend_ms: 0,
        })
    });
    if t.channeling_tick > 0 {
        schedule_channel_tick(
            server,
            skill.clone(),
            ctx.clone(),
            cancel.clone(),
            t.channeling_tick,
            channeling_time,
        );
    }
    let skill2 = skill.clone();
    let ctx2 = ctx.clone();
    let scheduled = super::tasks::schedule(server, i64::from(channeling_time), move |srv| {
        if cancel.is_cancelled() {
            return;
        }
        if !ctx2.has_live_handles(&srv) {
            stop(&srv, &skill2, &ctx2.caster, doodad);
            return;
        }
        if skill2.template.channeling_tick <= 0 {
            apply_effects(&srv, &skill2, &ctx2);
        }
        end_channeling(
            &srv,
            &skill2,
            &ctx2.caster,
            doodad,
            &ctx2.caster_caster,
            true,
        );
    });
    if !scheduled {
        // No scheduler: close the channel now instead of leaving the task slot (and the
        // timeline id) set forever.
        stop(server, skill, caster, doodad);
    }
}

fn schedule_channel_tick(
    server: &GameServer,
    skill: SkillRef,
    ctx: CastCtx,
    cancel: CancelToken,
    elapsed: i32,
    duration: i32,
) {
    let interval = skill.template.channeling_tick;
    if interval <= 0 || elapsed >= duration {
        return;
    }
    super::tasks::schedule(server, i64::from(interval), move |server| {
        if cancel.is_cancelled() || skill.cancelled() {
            return;
        }
        let active = ctx
            .caster
            .with_unit(|u| u.unit().skill_state.skill_task.clone())
            .flatten();
        if !active.is_some_and(|task| std::sync::Arc::ptr_eq(&task.skill, &skill)) {
            return;
        }
        if !ctx.has_live_handles(&server) {
            let doodad = ctx
                .caster
                .with_unit(|u| u.unit().skill_state.skill_task.clone())
                .flatten()
                .and_then(|task| match task.kind {
                    SkillTaskKind::EndChanneling { channel_doodad } => Some(channel_doodad),
                    _ => None,
                })
                .unwrap_or(0);
            stop(&server, &skill, &ctx.caster, doodad);
            return;
        }
        let cost = skill.template.channeling_mana.max(0);
        let mp = ctx.caster.with_unit(|u| u.unit().mp).unwrap_or(0);
        if cost > mp {
            let doodad = ctx
                .caster
                .with_unit(|u| u.unit().skill_state.skill_task.clone())
                .flatten()
                .and_then(|task| match task.kind {
                    SkillTaskKind::EndChanneling { channel_doodad } => Some(channel_doodad),
                    _ => None,
                })
                .unwrap_or(0);
            stop(&server, &skill, &ctx.caster, doodad);
            return;
        }
        if cost > 0 {
            unit_combat::reduce_current_mp(&server, &ctx.caster, cost);
        }
        apply_effects(&server, &skill, &ctx);
        schedule_channel_tick(
            &server,
            skill.clone(),
            ctx.clone(),
            cancel,
            elapsed.saturating_add(interval),
            duration,
        );
    });
}

/// `Skill.EndChanneling(caster, channelDoodad, casterCaster)`. `completed`: the channel ran to
/// its end (not stopped or cancelled), so the channeling target buff times out.
pub fn end_channeling(
    server: &GameServer,
    skill: &SkillRef,
    caster: &WorldObject,
    channel_doodad: u32,
    caster_caster: &SkillCaster,
    completed: bool,
) {
    if !caster.is_unit() {
        return;
    }
    let active = caster
        .with_unit(|u| {
            let slot = &mut u.unit_mut().skill_state.skill_task;
            if slot
                .as_ref()
                .is_some_and(|task| Arc::ptr_eq(&task.skill, skill))
            {
                slot.take()
            } else {
                None
            }
        })
        .flatten();
    if let Some(task) = active {
        task.cancel.cancel();
    }
    let t = &skill.template;
    if t.channeling_buff_id != 0 && super::geometry::is_live_object(server, caster) {
        buffs::remove_effect_by_skill(server, caster, t.channeling_buff_id, t.id);
    }
    let channel_target = skill.state().channel_target.take();
    if t.channeling_target_buff_id != 0 {
        if let Some(target) = channel_target.and_then(|(id, identity)| {
            server
                .world
                .object(id)
                .filter(|target| target.core() as *const _ as usize == identity)
        }) {
            if completed {
                // The channel ran to its end: the target buff ends with it, which is its
                // Timeout (quest items such as the Earth Crystal are granted by that trigger).
                buffs::finish_effect_by_skill(server, &target, t.channeling_target_buff_id, t.id);
            } else {
                buffs::remove_effect_by_skill(server, &target, t.channeling_target_buff_id, t.id)
            }
        }
    }
    if channel_doodad != 0 {
        server
            .systems
            .skills
            .ext
            .npcs()
            .delete_doodad(server, channel_doodad);
    }
    end_skill(server, skill, caster);
    if !skill.cancelled() {
        if let (
            SkillCaster::Item {
                item_id,
                item_template_id,
                ..
            },
            Some(p),
        ) = (caster_caster, caster.as_player())
        {
            if *item_template_id > 0 {
                server
                    .systems
                    .skills
                    .ext
                    .quests()
                    .item_use(server, p, *item_id);
            }
        }
    }
}

/// Special effect types of `PlayUserMusic` / `PauseUserMusic`.
const SPECIAL_PLAY_USER_MUSIC: i32 = 93;
const SPECIAL_PAUSE_USER_MUSIC: i32 = 94;

/// Whether a player's skill carries a play / pause score special effect in the static catalog
/// (`skill_effects` -> `SpecialEffect` -> `special_effects.special_effect_type_id` 93 / 94).
pub(super) fn is_player_music_skill(
    server: &GameServer,
    template: &SkillTemplate,
    caster: &WorldObject,
) -> bool {
    if caster.as_player().is_none() {
        return false;
    }
    let data = &server.game_data.systems.skills;
    template.effects.iter().any(|e| {
        e.template.is_some_and(|(kind, id)| {
            matches!(
                data.effect(kind, id),
                Some(EffectTemplate::Special(s))
                    if matches!(
                        s.special_effect_type_id,
                        SPECIAL_PLAY_USER_MUSIC | SPECIAL_PAUSE_USER_MUSIC
                    )
            )
        })
    })
}

/// `Skill.ScheduleEffects`.
pub fn schedule_effects(server: &GameServer, skill: &SkillRef, ctx: &CastCtx) {
    let caster = &ctx.caster;
    let t = &skill.template;
    let data = &server.game_data.systems.skills;
    if t.toggle_buff_id != 0 {
        if let (Some(b), Some(target)) = (data.buff(t.toggle_buff_id).cloned(), ctx.target.as_obj())
        {
            buffs::apply_template(
                server,
                &b,
                Some(caster),
                &ctx.caster_caster,
                target,
                Some(skill),
            );
        }
    }
    let mut total_delay = 0i32;
    if t.effect_delay > 0 {
        total_delay += t.effect_delay;
    }
    if t.effect_speed > 0.0 {
        let d = distance_to(server, &Target::Obj(caster.clone()), &ctx.target, false);
        total_delay += (d / t.effect_speed * 1000.0) as i32;
    }
    let fire_anim_id = fire_anim_for(t, super::plots::weapon_wield_kind(server, caster));
    if fire_anim_id != 0 && t.use_anim_time {
        if let Some(anim) = data.anims.get(fire_anim_id) {
            let mul = stat(server, caster, StatKind::GlobalCooldownMul) as f32;
            total_delay += (anim.combat_sync_time as f32 * (mul / 100.0)) as i32;
        }
    }
    let weapon_anim = weapon_attack_anim_id(server, skill, caster);
    let mut fired = SCSkillFiredPacket {
        id: skill.id,
        tl: skill.tl_id(),
        caster: ctx.caster_caster,
        target: ctx.target_caster,
        skill_object: ctx.skill_object.clone(),
        computed_delay: total_delay as i16,
        channeling_time: t.channeling_time,
        fire_anim_id,
    };
    if weapon_anim > 0 {
        fired.fire_anim_id = weapon_anim;
    }
    // The 1.2 client reacts to its own `SCSkillStarted` of a play / pause score skill (special
    // effects 93 / 94) by sending the MIDI cache / the end of the music. An instant cast has no
    // cast-time `SCSkillStarted`, so the validated cast announces itself right before its fire.
    if std::mem::take(&mut skill.state().music_started_pending) {
        broadcast(
            server,
            caster,
            &SCSkillStartedPacket::new(
                skill.id,
                fired.tl,
                ctx.caster_caster,
                ctx.target_caster,
                ctx.skill_object.clone(),
            ),
            true,
        );
    }
    broadcast(server, caster, &fired, true);
    if total_delay > 0 {
        let skill2 = skill.clone();
        let ctx2 = ctx.clone();
        let timeline = fired.tl;
        let scheduled = super::tasks::schedule(server, i64::from(total_delay), move |srv| {
            // ApplySkillTask
            if skill2.tl_id() != timeline {
                // A newer use of the same SkillRun took the timeline; it recovered this id.
                return;
            }
            if skill2.cancelled() {
                end_skill(&srv, &skill2, &ctx2.caster);
                return;
            }
            if !ctx2.has_live_handles(&srv) {
                skill2.state().cancelled = true;
                end_skill(&srv, &skill2, &ctx2.caster);
                return;
            }
            apply_effects(&srv, &skill2, &ctx2);
            end_skill(&srv, &skill2, &ctx2.caster);
        });
        if !scheduled {
            // No scheduler: apply now and close the timeline instead of leaking it.
            if ctx.has_live_handles(server) {
                apply_effects(server, skill, ctx);
            }
            end_skill(server, skill, caster);
        }
    } else {
        apply_effects(server, skill, ctx);
        end_skill(server, skill, caster);
    }
}

/// `Skill.GetWeaponAttackAnimId(caster)`.
/// The fire clip for a caster whose weapons are of `wield_kind` (`weapon_wield_kind`: 2 two-handed, 3 two weapons): the skill's
/// two-handed or dual wield variant when it has one, else `fire_anim_id`. The 1.2 client plays the clip it is sent and picks no
/// variant itself: sent only `fire_anim_id`, it swung a greatsword with Precision Strike's `onehand_co_sk_weapon_blunt_2`
/// (skill parity trace, 2026-11-03).
fn fire_anim_for(t: &SkillTemplate, wield_kind: i32) -> u32 {
    match wield_kind {
        2 if t.twohand_fire_anim_id != 0 => t.twohand_fire_anim_id,
        3 if t.dual_wield_fire_anim_id != 0 => t.dual_wield_fire_anim_id,
        _ => t.fire_anim_id,
    }
}

fn weapon_attack_anim_id(server: &GameServer, skill: &SkillRun, caster: &WorldObject) -> u32 {
    if !matches!(skill.template.id, 2..=4) {
        return 0;
    }
    let idx = {
        let mut s = skill.state();
        let i = s.auto_attack_index;
        s.auto_attack_index += 1;
        i
    };
    if caster.as_npc().is_some() {
        return (idx % 2 + 1) as u32;
    }
    if caster.as_player().is_none() {
        skill.state().auto_attack_index -= 1;
        return 0;
    }
    let slot = match skill.template.id {
        3 => equip_slot::OFFHAND,
        4 => equip_slot::RANGED,
        _ => equip_slot::MAINHAND,
    };
    let items = server.systems.skills.ext.items();
    match items.weapon_holdable(server, caster, slot) {
        Some(h) => items.weapon_attack_anim(server, h.holdable_id, idx, skill.template.id == 3),
        None => {
            if idx % 2 == 0 {
                1
            } else {
                2
            }
        }
    }
}

/// `Skill.RollCombatDice(attacker, target)`.
pub fn roll_combat_dice(
    server: &GameServer,
    skill: &SkillRun,
    attacker: &WorldObject,
    target: &WorldObject,
) -> SkillHitType {
    let damage_type = DamageType::from_raw(i64::from(skill.template.damage_type_id));
    let roll = || rand::thread_rng().gen::<f32>() * 100.0;
    let attacker_unit = attacker.is_unit();
    let target_unit = target.is_unit();
    let a = stats(
        server,
        attacker,
        &[
            StatKind::BullsEye,
            StatKind::MeleeAccuracy,
            StatKind::SpellAccuracy,
            StatKind::RangedAccuracy,
        ],
    );
    let mut always_hit = false;
    if attacker_unit {
        let bulls_eye_mod = a[0] as i32 as f32 / 1000.0 * 3.0 / 100.0;
        let ap = world_pos(server, &Target::Obj(attacker.clone()));
        let tp = world_pos(server, &Target::Obj(target.clone()));
        if !is_front(&ap, &tp) {
            always_hit = true;
        } else if target_unit {
            let tstat = stats(
                server,
                target,
                &[
                    StatKind::DodgeRate,
                    StatKind::BlockRate,
                    StatKind::MeleeParryRate,
                    StatKind::RangedParryRate,
                ],
            );
            if roll() < tstat[0] as f32 - bulls_eye_mod {
                match damage_type {
                    Some(DamageType::Melee) => return SkillHitType::MeleeDodge,
                    Some(DamageType::Ranged) => return SkillHitType::RangedDodge,
                    _ => {}
                }
            }
            if roll() < tstat[1] as f32 - bulls_eye_mod {
                match damage_type {
                    Some(DamageType::Melee) => return SkillHitType::MeleeBlock,
                    Some(DamageType::Ranged) => return SkillHitType::RangedBlock,
                    _ => {}
                }
            }
            if roll() < tstat[2] as f32 - bulls_eye_mod {
                if damage_type == Some(DamageType::Melee) {
                    return SkillHitType::MeleeParry;
                }
                if damage_type == Some(DamageType::Ranged) {
                    let dual = target.with_base_unit(|b| {
                        b.buffs()
                            .check_buff(super::types::buff_constants::EQUIP_DUALWIELD)
                            && b.buffs()
                                .check_buff(super::types::buff_constants::DUALWIELD_PROFICIENCY)
                    });
                    if dual {
                        return SkillHitType::MeleeParry;
                    }
                }
            }
            if roll() < tstat[3] as f32 - bulls_eye_mod && damage_type == Some(DamageType::Ranged) {
                return SkillHitType::RangedParry;
            }
        }
    }
    let _ = always_hit;
    match damage_type {
        Some(DamageType::Melee) => {
            if attacker_unit && roll() < a[1] as f32 {
                SkillHitType::MeleeHit
            } else {
                SkillHitType::MeleeMiss
            }
        }
        Some(DamageType::Magic) => {
            if attacker_unit && roll() < a[2] as f32 {
                SkillHitType::SpellHit
            } else {
                SkillHitType::SpellMiss
            }
        }
        Some(DamageType::Ranged) => {
            if attacker_unit && roll() < a[3] as f32 {
                SkillHitType::RangedHit
            } else {
                SkillHitType::RangedMiss
            }
        }
        Some(DamageType::Siege) => SkillHitType::RangedHit,
        _ => SkillHitType::Invalid,
    }
}

fn level_rule_hit(skill: &SkillRun) -> SkillHitType {
    match DamageType::from_raw(i64::from(skill.template.damage_type_id)) {
        Some(DamageType::Melee) => SkillHitType::MeleeHit,
        Some(DamageType::Magic | DamageType::Heal) => SkillHitType::SpellHit,
        Some(DamageType::Siege | DamageType::Ranged) => SkillHitType::RangedHit,
        None => SkillHitType::Invalid,
    }
}

/// `ShipSiegeAoEHit.AppendHostileShipsHitBySiegeHullAoE`: a siege AoE cast on a position also
/// hits the boats (with ship physics) whose mass box the siege circle touches, although their
/// pivot is outside the circle (`systems::ai::physics::siege_point_hits_ship`).
fn append_hostile_ships_hit_by_siege_hull_aoe(
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

/// Refunds the cast's taken source item unless the cast result was committed. Dropping the
/// guard refunds: an early refusal (missing reagent, failed build) or an effect that cancels
/// while nothing could be charged.
struct CastSourceDebit<'a> {
    server: &'a GameServer,
    taken: Option<TakenCastItem>,
}

impl Drop for CastSourceDebit<'_> {
    fn drop(&mut self) {
        if let Some(taken) = self.taken.take() {
            self.server
                .systems
                .skills
                .ext
                .items()
                .refund_cast_source_item(self.server, taken);
        }
    }
}

impl CastSourceDebit<'_> {
    fn commit(&mut self) {
        if let Some(taken) = self.taken.take() {
            self.server
                .systems
                .skills
                .ext
                .items()
                .commit_cast_source_item(self.server, taken);
        }
    }
}

/// `Skill.ApplyEffects(caster, casterCaster, targetSelf, targetCaster, skillObject)`.
pub fn apply_effects(server: &GameServer, skill: &SkillRef, ctx: &CastCtx) {
    let caster = &ctx.caster;
    if !caster.is_unit() || skill.cancelled() {
        tracing::debug!(
            skill_id = skill.id,
            caster_is_unit = caster.is_unit(),
            cancelled = skill.cancelled(),
            "Skill effects skipped"
        );
        return;
    }
    let t = skill.template.clone();
    let data = &server.game_data.systems.skills;
    let player = caster.as_player().cloned();
    let caster_t = Target::Obj(caster.clone());
    let target_self = &ctx.target;
    let selection = SkillTargetSelection::from_raw(i64::from(t.target_selection));
    // Native 1.2 sub_398A01B0 attribute 3 / sub_39420B30: radius is in meters.
    let area_radius = apply_skill_modifiers(
        server,
        skill,
        caster,
        SkillAttribute::AreaRadius,
        f64::from(t.target_area_radius),
    )
    .max(0.0)
    .min(f64::from(i32::MAX)) as f32;
    let mut possible: Vec<Target> = Vec::new();
    if t.target_siege
        && selection == Some(SkillTargetSelection::Source)
        && caster.as_slave().is_some()
    {
        possible.push(target_self.clone());
    } else if t.target_area_radius > 0 {
        let mut units = super::geometry::around_radius(server, target_self, area_radius, true);
        // `around_radius` compares only x/y: drop candidates far above or below the area centre
        // (a ground-target area uses the target point's z).
        let limit = super::geometry::area_vertical_limit(area_radius, t.target_valid_height);
        let origin_z = world_pos(server, target_self).z;
        units.retain(|o| (world_pos(server, &Target::Obj(o.clone())).z - origin_z).abs() <= limit);
        if selection == Some(SkillTargetSelection::Source) {
            if let Some(o) = target_self.as_obj() {
                units.push(o.clone());
            }
        } else if selection == Some(SkillTargetSelection::Target) {
            // Deviation from C#: `around_radius` excludes its origin, so a lone primary target
            // of a target-selected area skill (Concussive Arrow 11933) was never hit.
            if let Some(o) = target_self.as_obj().filter(|o| o.is_unit()) {
                units.push(o.clone());
            }
        }
        let relation = SkillTargetRelation::from_raw(i64::from(t.target_relation))
            .unwrap_or(SkillTargetRelation::Any);
        let mut units = filter_with_relation(server, relation, &caster_t, units);
        // Corpses (or the living, for a resurrection) never take an area slot. A template with
        // neither flag set (57 radius skills) is left unfiltered rather than refusing everyone.
        if t.target_alive || t.target_dead {
            units.retain(|o| {
                !o.is_unit()
                    || super::geometry::alive_dead_allows(
                        t.target_alive,
                        t.target_dead,
                        o.with_unit(|u| u.unit().hp).unwrap_or(0),
                    )
            });
        }
        possible.extend(units.into_iter().map(Target::Obj));
        if selection == Some(SkillTargetSelection::Source) && target_self.as_obj().is_none() {
            possible.push(target_self.clone());
        }
    } else {
        possible.push(target_self.clone());
    }
    append_hostile_ships_hit_by_siege_hull_aoe(
        server,
        &t,
        &caster_t,
        target_self,
        ctx,
        area_radius,
        &mut possible,
    );
    // Distinct
    let mut distinct: Vec<Target> = Vec::new();
    for p in possible {
        if !distinct.iter().any(|d| d.same(&p)) {
            distinct.push(p);
        }
    }
    let mut possible = distinct;
    if t.target_area_radius > 0 && t.target_area_angle > 0 && t.target_area_angle < 360 {
        let origin = world_pos(server, target_self);
        possible.retain(|target| {
            super::geometry::in_angle(
                &origin,
                &world_pos(server, target),
                t.target_area_angle as f32,
            )
        });
    }
    if possible.is_empty() && matches!(ctx.target_caster, SkillCastTarget::Position { .. }) {
        possible.push(caster_t.clone());
    }
    if t.target_area_count > 0 && possible.len() > t.target_area_count as usize {
        let mut with_d: Vec<(f32, Target)> = possible
            .into_iter()
            .map(|p| (distance_to(server, &p, target_self, false), p))
            .collect();
        with_d.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        possible = with_d
            .into_iter()
            .take(t.target_area_count as usize)
            .map(|(_, p)| p)
            .collect();
    }
    let hostile = t.target_type == SkillTargetType::Hostile as i32;
    for target in &possible {
        let Some(o) = target.as_obj() else { continue };
        if o.is_unit() && hostile {
            let mut dice = roll_combat_dice(server, skill, caster, o);
            if t.level_rule_no_consideration {
                dice = level_rule_hit(skill);
            }
            skill.state().hit_types.entry(o.obj_id()).or_insert(dice);
            if let Some(tp) = o.as_player() {
                let cfg = server.systems.skills.config();
                let items = server.systems.skills.ext.items();
                let mut rate = if caster.as_player().is_some() {
                    cfg.pvp_durability_loss_rate
                } else {
                    cfg.pve_durability_loss_rate
                };
                rate *= items.durability_decrement_chance(server);
                if rate > 0.0 {
                    use super::combat::durability_targets as d;
                    match dice {
                        SkillHitType::MeleeBlock | SkillHitType::RangedBlock => {
                            items.durability_loss(server, tp, 1, d::SHIELD, rate)
                        }
                        SkillHitType::RangedParry | SkillHitType::MeleeParry => {
                            items.durability_loss(server, tp, 1, d::ALL_MAIN_WEAPONS, rate)
                        }
                        _ => {}
                    }
                }
            }
        } else if o.as_doodad().is_some() {
            server
                .systems
                .skills
                .ext
                .npcs()
                .doodad_skill_hit(server, o, caster, t.id);
        }
    }

    let caster_level = caster.with_unit(|u| u.unit().level).unwrap_or(0);
    let force_attack = caster.with_unit(|u| u.unit().force_attack).unwrap_or(false);
    let caster_pos = world_pos(server, &caster_t);
    let mut to_apply: Vec<(Target, &SkillEffect)> = Vec::new();
    let mut last_applied: Option<&SkillEffect> = None;
    for effect in &t.effects {
        let effected: Vec<Target> =
            match ApplicationMethod::from_raw(i64::from(effect.application_method)) {
                Some(ApplicationMethod::Target | ApplicationMethod::SourceToPos) => {
                    possible.clone()
                }
                Some(ApplicationMethod::Source) => vec![caster_t.clone()],
                Some(ApplicationMethod::SourceOnce) => {
                    let mate_owner_mark = matches!(ctx.caster_caster, SkillCaster::Mount { .. })
                        && target_self.as_obj().is_some_and(|o| o.as_mate().is_some());
                    let slave = target_self.as_obj().is_some_and(|o| o.as_slave().is_some());
                    if mate_owner_mark || slave {
                        possible.clone()
                    } else {
                        vec![caster_t.clone()]
                    }
                }
                None => {
                    tracing::debug!(
                        skill_id = t.id,
                        effect_id = effect.effect_id,
                        application_method = effect.application_method,
                        "Skill effect skipped: unknown application method"
                    );
                    Vec::new()
                }
            };
        for target in effected {
            let relation = relation_to(server, &caster_t, &target);
            if effect.start_level > caster_level || effect.end_level < caster_level {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    target = target.obj_id(),
                    caster_level,
                    start_level = effect.start_level,
                    end_level = effect.end_level,
                    "Skill effect skipped: caster level outside the effect's level range"
                );
                continue;
            }
            if effect.friendly && !effect.non_friendly && relation != RelationState::Friendly {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    target = target.obj_id(),
                    ?relation,
                    "Skill effect skipped: the effect is friendly-only"
                );
                continue;
            }
            if !effect.friendly
                && effect.non_friendly
                && relation != RelationState::Hostile
                && ((relation == RelationState::Friendly && !force_attack)
                    || caster_t.obj_id() == target.obj_id())
            {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    target = target.obj_id(),
                    ?relation,
                    "Skill effect skipped: the effect is hostile-only"
                );
                continue;
            }
            let tp = world_pos(server, &target);
            if effect.front && !effect.back && !is_front(&caster_pos, &tp) {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    target = target.obj_id(),
                    "Skill effect skipped: the target is behind the caster"
                );
                continue;
            }
            if !effect.front && effect.back && is_front(&caster_pos, &tp) {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    target = target.obj_id(),
                    "Skill effect skipped: the effect is back-only"
                );
                continue;
            }
            if effect.source_buff_tag_id > 0
                && !buffs::check_buffs_by_tag(server, caster, effect.source_buff_tag_id)
            {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    tag = effect.source_buff_tag_id,
                    "Skill effect skipped: the caster lacks the required buff tag"
                );
                continue;
            }
            if effect.source_no_buff_tag_id > 0
                && buffs::check_buffs_by_tag(server, caster, effect.source_no_buff_tag_id)
            {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    tag = effect.source_no_buff_tag_id,
                    "Skill effect skipped: the caster has the forbidden buff tag"
                );
                continue;
            }
            if let Some(o) = target.as_obj() {
                if effect.target_buff_tag_id > 0
                    && !buffs::check_buffs_by_tag(server, o, effect.target_buff_tag_id)
                {
                    tracing::debug!(
                        skill_id = t.id,
                        effect_id = effect.effect_id,
                        target = target.obj_id(),
                        tag = effect.target_buff_tag_id,
                        "Skill effect skipped: the target lacks the required buff tag"
                    );
                    continue;
                }
                if effect.target_no_buff_tag_id > 0
                    && buffs::check_buffs_by_tag(server, o, effect.target_no_buff_tag_id)
                {
                    tracing::debug!(
                        skill_id = t.id,
                        effect_id = effect.effect_id,
                        target = target.obj_id(),
                        tag = effect.target_no_buff_tag_id,
                        "Skill effect skipped: the target has the forbidden buff tag"
                    );
                    continue;
                }
            }
            if effect.target_npc_tag_id > 0 {
                let ok = target.as_obj().and_then(|o| o.as_npc()).is_some_and(|n| {
                    let tid = n.data().unit.template_id;
                    data.npcs_by_tag(effect.target_npc_tag_id).contains(&tid)
                });
                if !ok {
                    tracing::debug!(
                        skill_id = t.id,
                        effect_id = effect.effect_id,
                        target = target.obj_id(),
                        tag = effect.target_npc_tag_id,
                        "Skill effect skipped: the target NPC is not in the required tag"
                    );
                    continue;
                }
            }
            if !effects::chance_succeeds(effect.chance) {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    target = target.obj_id(),
                    chance = effect.chance,
                    "Skill effect skipped: the chance roll failed"
                );
                continue;
            }
            if let (Some(last), Some((EffectKind::NpcSpawnerSpawnEffect, id))) =
                (last_applied, effect.template)
            {
                let spawner =
                    |e: &SkillEffect| match e.template.and_then(|(k, i)| data.effect(k, i)) {
                        Some(EffectTemplate::NpcSpawnerSpawn(s)) => Some(s.spawner_id),
                        _ => None,
                    };
                if last.effect_id == effect.effect_id && spawner(last) == spawner(effect) {
                    let _ = id;
                    continue;
                }
            }
            to_apply.push((target, effect));
            last_applied = Some(effect);
        }
    }

    let items = server.systems.skills.ext.items();
    let mut consumed_items: Vec<(u64, i32)> = Vec::new();
    let mut consumed_templates: Vec<(u32, i32)> = Vec::new();
    let paid_building = |target: &Target, effect: &SkillEffect| {
        player.is_some()
            && target.as_obj().and_then(WorldObject::as_house).is_some()
            && effect
                .template
                .and_then(|(kind, id)| data.effect(kind, id))
                .is_some_and(|template| {
                    crate::systems::housing::is_building_effect(server, template)
                })
    };
    // `SavePortal` creates an entry only after its own atomic reagent take in `misc.rs`; the
    // generic optional effect-item debit must not take a second one.
    let save_portal = t.effects.iter().any(|effect| {
        effect
            .template
            .and_then(|(kind, id)| data.effect(kind, id))
            .is_some_and(|template| {
                matches!(
                    template,
                    EffectTemplate::Special(s) if s.special_effect_type_id == 30
                )
            })
    });
    // The cast item is the source of the skill: consume it (or only re-verify it when the skill
    // consumes nothing) before the reagents, products and effects run, so an item that left the
    // bag during a cast time refuses the cast instead of firing for free.
    let mut owned_item_use: Option<(u64, u32, i32)> = None;
    if let SkillCaster::Item {
        item_id,
        item_template_id,
        ..
    } = ctx.caster_caster
    {
        let amount = last_applied.map_or(0, |last| {
            // Deviation: C# looked at the last applied effect only. Skills with mixed flags (a
            // consuming GainItem followed by an Interaction effect) never consumed the item, so
            // the last applied effect that carries `consume_source_item` / `consume_item_id`
            // wins.
            let applied = || to_apply.iter().map(|(_, e)| *e);
            if let Some(e) = applied().rev().find(|e| e.consume_source_item) {
                e.consume_item_count
            } else if items.use_skill_as_reagent(server, item_template_id) {
                last.consume_item_count
            } else {
                0
            }
        });
        owned_item_use = Some((item_id, item_template_id, amount));
    }
    if last_applied.is_some() {
        let applied = || to_apply.iter().map(|(_, e)| *e);
        let template_effect = applied()
            .rev()
            .find(|e| e.consume_item_id != 0 && e.consume_item_count > 0);
        if let Some(p) = &player {
            if let Some(e) = template_effect.filter(|e| {
                // SavePortal pays its effect items in its own atomic step; house construction
                // pays at the progress commit, including stale/rejected targets. Never fall
                // back to the optional debit for those.
                !save_portal
                    && !to_apply.iter().any(|(target, candidate)| {
                        std::ptr::eq(*candidate, *e) && paid_building(target, candidate)
                    })
            }) {
                // source item consumption, or enough of the item in the bag / equipment
                if e.consume_source_item
                    || items.item_count(server, p, e.consume_item_id) >= e.consume_item_count
                {
                    consumed_templates.push((e.consume_item_id, e.consume_item_count));
                }
            }
        }
    }
    // Debit the cast item before anything else can be granted: the owner, template and count
    // must still match and it must sit in the caster's bag or equipment. The item stays out of
    // its container until the result is committed, so no mover can reach it mid-cast; a missing
    // item refuses the whole result (no reagents, products or effects).
    let mut source_debit: Option<CastSourceDebit<'_>> = None;
    let mut source_item = None;
    let mut source_item_debited = false;
    if let (Some((item_id, template_id, amount)), Some(p)) = (owned_item_use, &player) {
        match items.take_cast_source_item(server, p, item_id, template_id, amount) {
            Some(Ok(taken)) => {
                source_item = Some(std::sync::Arc::new(taken.item.clone()));
                source_item_debited = taken.amount > 0;
                source_debit = Some(CastSourceDebit {
                    server,
                    taken: Some(taken),
                });
            }
            Some(Err(())) => {
                tracing::warn!(
                    skill_id = t.id,
                    item_id,
                    item_template_id = template_id,
                    amount,
                    "Skill cast cancelled: the source item is no longer in the caster's bag or equipment"
                );
                skill.state().cancelled = true;
                stop(server, skill, caster, 0);
                return;
            }
            None => {
                // Hooks that do not track items: keep the owner-check semantics with the read
                // API they provide.
                let ok = items
                    .owned_item(server, p, item_id)
                    .is_some_and(|(tid, count)| tid == template_id && count >= amount.max(0));
                if !ok {
                    tracing::warn!(
                        skill_id = t.id,
                        item_id,
                        item_template_id = template_id,
                        amount,
                        "Skill cast cancelled: the source item is not owned in the required template and count"
                    );
                    skill.state().cancelled = true;
                    stop(server, skill, caster, 0);
                    return;
                }
                source_item = server.items.item_by_id(item_id).map(std::sync::Arc::new);
            }
        }
    }
    let reagents = data.reagents_for(t.id);
    let products = data.products_for(t.id);
    if !save_portal && (!reagents.is_empty() || !products.is_empty()) {
        if let Some(p) = &player {
            if !reagents.is_empty() {
                let mut found = false;
                for r in &reagents {
                    if items.item_count(server, p, r.item_id) >= i32::from(r.amount) {
                        consumed_templates.push((r.item_id, i32::from(r.amount)));
                        found = true;
                        if t.first_reagent_only {
                            break;
                        }
                    } else if !t.first_reagent_only {
                        skill.state().cancelled = true;
                        return;
                    }
                }
                if !found {
                    skill.state().cancelled = true;
                    return;
                }
            }
            for prod in &products {
                items.acquire_item(server, p, prod.item_id, i32::from(prod.amount));
            }
        }
    }

    // Weighted selection
    let weighted_total: i32 = to_apply.iter().map(|(_, e)| e.weight).sum();
    let mut selected = -1i32;
    if weighted_total > 0 {
        selected = rand::thread_rng().gen_range(0..weighted_total);
    }
    let mut current_weight = 0;
    let mut packets = CompressedGamePackets::new();
    let mut effect_source = EffectSource::from_skill(skill.clone());
    effect_source.source_item = source_item.clone();
    effect_source.source_item_debited = source_item_debited;
    let tl = skill.tl_id();
    for (target, effect) in &to_apply {
        if effect.weight > 0 {
            if selected == -1 {
                continue;
            }
            current_weight += effect.weight;
            if selected >= current_weight {
                continue;
            }
            selected = -1;
        }
        let Some((kind, id)) = effect.template else {
            tracing::error!(
                "Template not found for Skill[{}] Effect[{}]",
                t.id,
                effect.effect_id
            );
            continue;
        };
        let Some(template) = data.effect(kind, id) else {
            tracing::debug!(
                skill_id = t.id,
                effect_id = effect.effect_id,
                ?kind,
                id,
                "Skill effect skipped: the effect template is missing from the game data"
            );
            continue;
        };
        // Position casts (ground / ballistic / summon placement) have no unit obj id: keep the
        // original target so effects can read the chosen coordinates and rotation.
        let this_target_caster = if matches!(
            ctx.target_caster,
            SkillCastTarget::Position { .. }
                | SkillCastTarget::Position2 { .. }
                | SkillCastTarget::Position3 { .. }
        ) || target.obj_id() == ctx.target_caster.obj_id()
        {
            ctx.target_caster
        } else {
            SkillCastTarget::Unit(target.obj_id())
        };
        let mut args = EffectArgs {
            caster: Some(caster.clone()),
            caster_caster: ctx.caster_caster,
            target: target.clone(),
            target_caster: this_target_caster,
            cast: CastAction::Skill {
                skill_id: t.id,
                tl_id: tl,
            },
            source: effect_source.clone(),
            skill_object: &ctx.skill_object,
        };
        args.source.always_hit = effect.always_hit;
        if paid_building(target, effect) {
            // This is the actual selected effect and target, rather than the router's
            // "handled" bool (which says nothing about whether construction succeeded).
            let p = player.as_ref().expect("paid building has a player");
            let house = target
                .as_obj()
                .and_then(WorldObject::as_house)
                .expect("paid building has a house");
            match crate::systems::housing::ops::craft_building_paid(
                server,
                p,
                house,
                t.id,
                effect.consume_item_id,
                effect.consume_item_count,
            ) {
                Ok(()) => {
                    let template_id = house.data().unit.template_id;
                    crate::systems::quests::events::on_doodad_interaction(
                        server,
                        p,
                        p,
                        template_id,
                    );
                }
                Err(error) => {
                    // The cast's own effect refused its payment: refund the source item.
                    skill.state().effect_cancelled = true;
                    if error == crate::systems::housing::ops::BuildActionError::MissingMaterial {
                        let mut packet = SCSkillStartedPacket::new(
                            skill.id,
                            tl,
                            ctx.caster_caster,
                            this_target_caster,
                            ctx.skill_object.clone(),
                        );
                        packet
                            .set_skill_result(unit_reqs::skill_result_for_key("skill_need_reagent"))
                            .set_result_uint(0);
                        p.send(packet);
                    }
                    stop(server, skill, caster, 0);
                    return;
                }
            }
        } else if let EffectTemplate::KillNpcWithoutCorpse(k) = template {
            let npc = nearest_kill_npc(server, caster, k.npc_id, k.radius);
            let effective = npc
                .clone()
                .or_else(|| target.as_obj().and_then(|o| o.as_npc().cloned()));
            if effective.as_ref().is_some_and(|n| n.data().unit.is_dead()) {
                // skip: KillNpcWithoutCorpse happens before death
            } else {
                if let Some(n) = npc {
                    args.caster = Some(WorldObject::from(n));
                }
                effects::apply(server, kind, template, &args, Some(&mut packets));
            }
        } else {
            effects::apply(server, kind, template, &args, Some(&mut packets));
            // Dyeing rejects through the C# `Character.SkillCancelled` flag (the hook has no
            // `SkillRun` handle). `cast` clears that flag when it accepts the cast, so a true
            // value seen here was set by one of this cast's own effects; record it in this
            // run's state instead of consulting the character flag at refund time.
            if player
                .as_ref()
                .is_some_and(|p| p.data().skill_char.skill_cancelled)
            {
                skill.state().cancelled = true;
                skill.state().effect_cancelled = true;
            }
        }
        if effect.item_set_id > 0 {
            consumed_templates.extend(items.item_set(server, effect.item_set_id));
        }
    }
    if let SkillCaster::Item {
        item_id,
        item_template_id,
        ..
    } = ctx.caster_caster
    {
        let chance = caster
            .with_unit(|u| u.unit().skill_state.condition_chance)
            .unwrap_or(false);
        if chance {
            let Some(p) = &player else {
                return;
            };
            server
                .systems
                .skills
                .ext
                .quests()
                .item_use_template(server, p, item_template_id);
            if items.use_skill_as_reagent(server, item_template_id)
                && reagents.is_empty()
                && products.is_empty()
                && consumed_items.is_empty()
                && t.effects.is_empty()
            {
                consumed_items.push((item_id, 1));
            }
        }
    }
    if !packets.is_empty() {
        broadcast(server, caster, &packets, true);
    }
    // Hack to consume TreasureMap items (as the C#).
    if let Some(p) = player.as_ref().filter(|_| t.id == DIG_UP_TREASURE_SKILL) {
        match unit_reqs::treasure_map_nearby(server, Some(p), unit_reqs::TREASURE_MAP_DIG_RANGE) {
            Some(map_id) => consumed_items.push((map_id, 1)),
            None => {
                let (name, id) = {
                    let c = p.data();
                    (c.unit.name.clone(), c.id)
                };
                tracing::error!(
                    "Unable to find a treasure map to take from user {name} ({id}) when digging up treasure"
                );
            }
        }
    }
    // An effect that cancels itself (invalid UCC/dye target, missing build material) refunds
    // the source item it could not charge for. A client / channel stop that lands while the
    // effects run (no effect cancelled) still pays for the effects that went out.
    // Only this cast's own run state decides the refund: the per-character flag can be left
    // set by another skill and must not make a successful cast refund its source.
    let effect_cancelled = skill.state().effect_cancelled;
    if let Some(mut debit) = source_debit {
        if !effect_cancelled {
            debit.commit();
        }
    }
    if !skill.cancelled() {
        if let Some(p) = &player {
            for (item, amount) in consumed_items {
                items.consume_item_by_id(server, p, item, amount);
            }
            for (tid, amount) in consumed_templates {
                items.consume_item_template(server, p, tid, amount);
            }
        }
    }
}

/// `Skill.EndSkill(caster)`.
pub fn end_skill(server: &GameServer, skill: &SkillRef, caster: &WorldObject) {
    let tl = std::mem::take(&mut skill.state().tl_id);
    if tl == 0 {
        return;
    }
    if !caster.is_unit() {
        // Not a unit any more: release the id without the unit-side effects.
        server.systems.skills.tl_ids.release(tl);
        return;
    }
    let t = &skill.template;
    if let Some(p) = caster.as_player() {
        let cancelled = skill.cancelled();
        let cost = super::validation::labor_cost(skill, p);
        let have = i32::from(p.data().labor_power);
        if cost > 0 && !cancelled && have >= cost {
            super::progress::change_labor(server, p, -(cost as i16), t.actability_group_id);
        }
        if t.gain_life_point > 0 && !cancelled {
            let rate = server.systems.skills.config().vocation_rate;
            super::progress::change_game_points(
                server,
                p,
                super::progress::GAME_POINT_VOCATION,
                (rate * f64::from(t.gain_life_point)).ceil() as i32,
            );
        }
    }
    skill.invoke_callback();
    // Unit.OnSkillEnd: no-op for every ported kind (npc AI hooks it).
    broadcast(server, caster, &SCSkillEndedPacket { tl }, true);
    server.systems.skills.tl_ids.release(tl);
    reset_cooldown_if_ignored(server, skill, caster);
    // USE_SKILL: one completed (not cancelled) cast.
    if let Some(p) = caster.as_player() {
        if !skill.cancelled() {
            crate::systems::report_achievement(
                server,
                p,
                AchievementInput::Increment {
                    kind_id: char_record_kind::USE_SKILL,
                    amount: 1,
                    matcher: RecordMatch::Value1(t.id),
                },
            );
        }
    }
}

/// A `plot_only` skill commits its GCD and cooldown when it is accepted. A plot cancelled
/// while it is still in its casting phase (before any damage or other effect landed) restores the
/// caster's cooldown state from before the use (so the shared tag cooldown returns to what it was
/// rather than being wiped) and tells the client.
pub(super) fn refund_cancelled_plot_cooldown(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
) {
    let Some(snap) = skill.state().plot_cooldown_snapshot.take() else {
        return;
    };
    let id = skill.template.id;
    let restored = caster.with_unit(|u| {
        let u = u.unit_mut();
        let restore = |map: &mut std::collections::HashMap<u32, i64>,
                       key: u32,
                       prev: Option<i64>| match prev {
            Some(end) => {
                map.insert(key, end);
            }
            None => {
                map.remove(&key);
            }
        };
        restore(&mut u.cooldowns.cooldowns, id, snap.skill_cooldown_end);
        if snap.tag_id != 0 {
            restore(
                &mut u.cooldowns.tag_cooldowns,
                snap.tag_id,
                snap.tag_cooldown_end,
            );
        }
        u.global_cooldown_ms = snap.global_cooldown_ms;
    });
    if restored.is_none() {
        return;
    }
    if let Some(o) = owner_character(server, caster) {
        for gcd in [false, true] {
            o.send(SCSkillCooldownResetPacket {
                obj_id: caster.obj_id(),
                skill_id: id,
                tag_id: if gcd { 0 } else { snap.tag_id },
                gcd,
            });
        }
    }
}

pub(super) fn reset_cooldown_if_ignored(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
) {
    if let Some(o) = owner_character(server, caster) {
        if o.data().skill_char.ignore_skill_cooldowns {
            let tag = skill.template.cooldown_tag_id.max(0) as u32;
            caster.with_unit(|u| {
                u.unit_mut()
                    .cooldowns
                    .remove_skill_cooldown(skill.template.id, tag);
            });
            o.send(SCSkillCooldownResetPacket {
                obj_id: caster.obj_id(),
                skill_id: skill.template.id,
                tag_id: tag,
                gcd: false,
            });
        }
    }
}

/// `Skill.Stop(caster, channelDoodad, casterCaster)`.
pub fn stop(server: &GameServer, skill: &SkillRef, caster: &WorldObject, channel_doodad: u32) {
    if !caster.is_unit() || skill.tl_id() == 0 {
        return;
    }
    let t = &skill.template;
    skill.state().cancelled = true;
    let task = caster
        .with_unit(|u| {
            let slot = &mut u.unit_mut().skill_state.skill_task;
            if slot
                .as_ref()
                .is_some_and(|task| Arc::ptr_eq(&task.skill, skill))
            {
                slot.take()
            } else {
                None
            }
        })
        .flatten();
    if let Some(task) = task {
        task.cancel.cancel();
    }
    let tl = skill.tl_id();
    broadcast(
        server,
        caster,
        &SCCastingStoppedPacket { tl, duration: 0 },
        true,
    );
    if t.toggle_buff_id != 0 {
        buffs::remove_effect_by_skill(server, caster, t.toggle_buff_id, t.id);
    }
    if t.channeling_time > 0 {
        buffs::fire_unit_event(
            server,
            caster.obj_id(),
            super::types::BuffEventTriggerKind::ChannelingCancel,
            0,
        );
        end_channeling(
            server,
            skill,
            caster,
            channel_doodad,
            &SkillCaster::Unit(caster.obj_id()),
            false,
        );
    } else {
        end_skill(server, skill, caster);
    }
}

/// `Skill.StopSkill(caster)` (base melee / ranged skills).
pub fn stop_skill(server: &GameServer, skill: &SkillRef, caster: &WorldObject) {
    if !caster.is_unit() {
        return;
    }
    if let Some(a) = caster
        .with_unit(|u| u.unit().skill_state.auto_attack.clone())
        .flatten()
    {
        a.cancel.cancel();
    }
    // Take-and-clear atomically (as `end_skill` does): a concurrent plot end or skill end then
    // releases 0, a no-op, instead of releasing the same id twice.
    let tl = skill.take_tl_id();
    broadcast(server, caster, &SCSkillEndedPacket { tl }, true);
    broadcast(
        server,
        caster,
        &SCSkillStoppedPacket {
            obj_id: caster.obj_id(),
            skill_id: skill.id,
        },
        true,
    );
    server.systems.skills.tl_ids.release(tl);
}

/// `Skill.ConsumeMana(caster)`.
pub fn consume_mana(server: &GameServer, skill: &SkillRun, caster: &WorldObject) {
    if !caster.is_unit() {
        return;
    }
    let cost = mana_cost(server, skill, caster);
    unit_combat::reduce_current_mp(server, caster, cost);
    if let Some(p) = caster.as_player() {
        let mut c = p.data();
        c.skill_char.last_cast_ms = now_ms();
        c.skill_char.is_in_post_cast = true;
    }
}

/// Extends a running direct cast's end by `delay_ms` (`DisturbCasting` with a delay, WP-1:
/// the 1.2 client adds the delay to the cast bar when it receives `SCCastingDelayed`). Returns
/// the cast skill's id, or `None` when the unit is not in a direct `Cast` task (a plot or a
/// channel is not extended), the task is being cancelled, or it is already completing.
pub fn delay_cast(unit: &WorldObject, delay_ms: i64) -> Option<u32> {
    if delay_ms <= 0 {
        return None;
    }
    unit.with_unit(|u| {
        let task = u.unit_mut().skill_state.skill_task.as_mut()?;
        if task.kind != SkillTaskKind::Cast || task.skill.cancelled() {
            return None;
        }
        task.extend_ms = task.extend_ms.saturating_add(delay_ms);
        Some(task.skill.id)
    })
    .flatten()
}

/// `Unit.InterruptSkills()`.
pub fn interrupt_skills(server: &GameServer, unit: &WorldObject) {
    let Some((plot, task)) = unit.with_unit(|u| {
        let s = &u.unit().skill_state;
        (s.active_plot.clone(), s.skill_task.clone())
    }) else {
        return;
    };
    if let Some(p) = plot {
        p.0.request_cancellation();
    }
    let Some(task) = task else {
        return;
    };
    match task.kind {
        SkillTaskKind::EndChanneling { channel_doodad } => {
            stop(server, &task.skill, unit, channel_doodad)
        }
        SkillTaskKind::Cast => stop(server, &task.skill, unit, 0),
    }
}

/// `Unit.UseSkill(skillId, target)`.
pub fn unit_use_skill(
    server: &GameServer,
    unit: &WorldObject,
    skill_id: u32,
    target: &WorldObject,
) -> u8 {
    unit_use_skill_impl(server, unit, skill_id, target, false).0
}

/// The checks [`unit_use_skill`] performs for `skill_id`, without casting or changing the unit.
/// Returns the first refusal exactly as the real use would return it.
pub fn unit_use_skill_check(
    server: &GameServer,
    unit: &WorldObject,
    skill_id: u32,
    target: &WorldObject,
) -> (u8, u32) {
    unit_use_skill_impl(server, unit, skill_id, target, true)
}

fn unit_use_skill_impl(
    server: &GameServer,
    unit: &WorldObject,
    skill_id: u32,
    target: &WorldObject,
    dry_run: bool,
) -> (u8, u32) {
    let Some(t) = server.game_data.systems.skills.skill(skill_id).cloned() else {
        // Retail data has quest components pointing at skills without a `skills` row (see
        // PORTING.md); warn once per id, repeats go to debug.
        let first = server
            .systems
            .skills
            .missing_skill_warned
            .lock()
            .expect("missing skill set poisoned")
            .insert(skill_id);
        let caster = unit.obj_id();
        let target = target.obj_id();
        if first {
            tracing::warn!("UseSkill: skill {skill_id} has no template (dangling retail reference), skipped; caster {caster} target {target}");
        } else {
            tracing::debug!("UseSkill: skill {skill_id} has no template (dangling retail reference), skipped; caster {caster} target {target}");
        }
        return (skill_result::INVALID_SKILL, 0);
    };
    let skill = SkillRun::new(t, 1);
    let mut sct = if target.as_doodad().is_some() {
        SkillCastTarget::Doodad(target.obj_id())
    } else {
        SkillCastTarget::Unit(target.obj_id())
    };
    use_skill_impl(
        server,
        &skill,
        unit,
        SkillCaster::Unit(unit.obj_id()),
        &mut sct,
        None,
        true,
        dry_run,
    )
}

/// `SkillManager.GetAttackDelay(template, caster, includeCooldown, additionalDelay)`.
///
/// `level` is the effective skill level `L` the caster runs the skill at (see
/// [`cast_skill_level`]), for the 1.2 `(int)(casting_time * (1 + T))` term of the
/// non-auto-attack branch.
pub fn attack_delay(
    server: &GameServer,
    t: &SkillTemplate,
    level: i32,
    caster: &WorldObject,
    include_cooldown: bool,
    additional: f64,
) -> f64 {
    let gcd_mul = stat(server, caster, StatKind::GlobalCooldownMul);
    if matches!(t.id, 2..=4) && caster.as_player().is_some() {
        let slot = match t.weapon_slot_for_auto_attack_id {
            equip_slot::MAINHAND | equip_slot::OFFHAND | equip_slot::RANGED => {
                t.weapon_slot_for_auto_attack_id
            }
            _ => match t.id {
                2 => equip_slot::MAINHAND,
                3 => equip_slot::OFFHAND,
                _ => equip_slot::RANGED,
            },
        };
        let fallback = if slot == equip_slot::RANGED {
            1800.0
        } else {
            1500.0
        };
        let speed = server
            .systems
            .skills
            .ext
            .items()
            .weapon_holdable(server, caster, slot)
            .filter(|h| h.speed.is_finite() && h.speed > 0.0)
            .map_or(fallback, |h| f64::from(h.speed));
        let attr = if slot == equip_slot::RANGED {
            UnitAttribute::RangedSpeedMul
        } else {
            UnitAttribute::MeleeSpeedMul
        };
        let haste = caster
            .with_unit(|u| u.unit().calculate_with_bonuses(0.0, attr, now_ms()))
            .unwrap_or(0.0);
        // Native 1.2 sub_398B49E0: weapon delay * 1000 / (1000 + haste),
        // guarding the denominator at 1. Offhand uses melee haste, ranged its own.
        // Only this server repeat timer applies haste; the GCD remains once-only.
        return (speed * (gcd_mul / 100.0) * 1000.0 / (1000.0 + haste.max(-999.0)))
            .max(0.0)
            .min(f64::from(i32::MAX));
    }
    let cast_mul = stat(server, caster, StatKind::CastTimeMul);
    let cast =
        f64::from((f64::from(t.casting_time) * (1.0 + rank_inc(t, level))) as i32) * cast_mul;
    let cd = if include_cooldown {
        f64::from(t.cooldown_time) * (gcd_mul / 100.0)
    } else {
        0.0
    };
    cast + cd + additional * (gcd_mul / 100.0)
}

/// `Unit.StartAutoSkill(skill)` (characters only).
pub fn start_auto_skill(server: &GameServer, caster: &WorldObject, skill: &SkillRef) {
    let Some(p) = caster.as_player() else { return };
    if p.data().unit.skill_state.auto_attack.is_some() {
        return;
    }
    let cancel = CancelToken::new();
    p.data().unit.skill_state.auto_attack = Some(AutoAttackSlot {
        skill: skill.clone(),
        cancel: cancel.clone(),
    });
    let level = cast_skill_level(server, skill, caster);
    let delay = attack_delay(server, &skill.template, level, caster, true, 1000.0) as i64;
    let obj_id = p.obj_id;
    let skill = skill.clone();
    if !schedule_auto_attack(server, obj_id, skill, cancel, delay) {
        // No scheduler: clear the slot so the stale auto-attack state does not stay behind.
        let mut c = p.data();
        c.unit.is_auto_attack = false;
        c.unit.skill_state.auto_attack = None;
    }
}

fn schedule_auto_attack(
    server: &GameServer,
    obj_id: u32,
    skill: SkillRef,
    cancel: CancelToken,
    delay: i64,
) -> bool {
    super::tasks::schedule(server, delay, move |srv| {
        if cancel.is_cancelled() {
            return;
        }
        let Some(p) = srv.world.character_by_obj_id(obj_id) else {
            return;
        };
        let next = auto_attack_tick(&srv, &p, &skill, &cancel);
        if let Some(next) = next {
            schedule_auto_attack(&srv, obj_id, skill, cancel, next);
        }
    })
}

/// `UseAutoAttackSkillTask.Execute`: `Some(next delay)` while it keeps running.
fn auto_attack_tick(
    server: &GameServer,
    p: &crate::world::PlayerRef,
    skill: &SkillRef,
    cancel: &CancelToken,
) -> Option<i64> {
    let owns_slot = p
        .data()
        .unit
        .skill_state
        .auto_attack
        .as_ref()
        .is_some_and(|slot| slot.cancel == *cancel && Arc::ptr_eq(&slot.skill, skill));
    if !owns_slot {
        cancel.cancel();
        return None;
    }
    let me = WorldObject::from(p);
    let (hp, target_id, has_task, gcd) = {
        let c = p.data();
        (
            c.unit.hp,
            c.unit.current_target,
            c.unit.skill_state.skill_task.is_some(),
            c.unit.global_cooldown_ms,
        )
    };
    let target = (target_id != 0)
        .then(|| server.world.object(target_id))
        .flatten()
        .filter(WorldObject::is_unit);
    let target_hp = target
        .as_ref()
        .and_then(|t| t.with_unit(|u| u.unit().hp))
        .unwrap_or(0);
    if hp <= 0
        || target.is_none()
        || target_hp <= 0
        || target_id == p.obj_id
        || cancel.is_cancelled()
    {
        // StopAutoAttack
        cancel.cancel();
        let mut c = p.data();
        c.unit.is_auto_attack = false;
        c.unit.skill_state.auto_attack = None;
        return None;
    }
    let target = target?;
    let level = cast_skill_level(server, skill, &me);
    let delay = attack_delay(server, &skill.template, level, &me, true, 1000.0) as i64;
    if has_task || gcd >= now_ms() {
        return Some(delay);
    }
    let me_t = Target::Obj(me.clone());
    let tt = Target::Obj(target.clone());
    if !can_attack(server, &me_t, &tt) {
        return Some(delay);
    }
    let items = server.systems.skills.ext.items();
    let slot = if skill.template.id == 4 {
        equip_slot::RANGED
    } else {
        equip_slot::MAINHAND
    };
    let max_range = items
        .weapon_holdable(server, &me, slot)
        .filter(|h| h.max_range > 0.0)
        .map(|h| h.max_range)
        .unwrap_or(if skill.template.max_range > 0 {
            skill.template.max_range as f32
        } else {
            3.0
        });
    if distance_to(server, &me_t, &tt, false) > max_range {
        return Some(delay);
    }
    let mut sct = SkillCastTarget::Unit(target.obj_id());
    use_skill(
        server,
        skill,
        &me,
        SkillCaster::Unit(me.obj_id()),
        &mut sct,
        Some(SkillObject::None),
        true,
    );
    if skill.template.id == 2 {
        let off = items.weapon_holdable(server, &me, equip_slot::OFFHAND);
        if let Some(h) = off {
            if h.slot_type_id != slot_type::SHIELD {
                if let Some(t3) = server.game_data.systems.skills.skill(3).cloned() {
                    let off_skill = SkillRun::new(t3, 1);
                    let mut sct = SkillCastTarget::Unit(target.obj_id());
                    use_skill(
                        server,
                        &off_skill,
                        &me,
                        SkillCaster::Unit(me.obj_id()),
                        &mut sct,
                        Some(SkillObject::None),
                        true,
                    );
                }
            }
        }
    }
    Some(delay)
}

/// The world position as a `PositionAndRotation` (re-export for callers).
pub fn pos(server: &GameServer, t: &Target) -> PositionAndRotation {
    world_pos(server, t)
}

/// A position unit target (for callers outside the module).
pub fn pos_target(p: PosUnit) -> Target {
    Target::Pos(p)
}

#[cfg(test)]
mod fire_anim_tests {
    use super::*;

    /// Precision Strike 12026: `onehand_co_sk_weapon_blunt_2` (132), two-handed `twohand_co_sk_weapon_blunt_2` (136); a skill
    /// without a variant keeps its own clip for every weapon.
    #[test]
    fn the_fire_clip_follows_the_weapons_held() {
        let t = SkillTemplate {
            fire_anim_id: 132,
            twohand_fire_anim_id: 136,
            dual_wield_fire_anim_id: 504,
            ..Default::default()
        };
        assert_eq!(fire_anim_for(&t, 0), 132, "no weapon");
        assert_eq!(fire_anim_for(&t, 1), 132, "one weapon");
        assert_eq!(fire_anim_for(&t, 2), 136, "a two-handed weapon");
        assert_eq!(fire_anim_for(&t, 3), 504, "two weapons");
        let plain = SkillTemplate {
            fire_anim_id: 18,
            ..Default::default()
        };
        assert_eq!(fire_anim_for(&plain, 2), 18);
        assert_eq!(fire_anim_for(&plain, 3), 18);
    }
}

#[cfg(test)]
mod auto_attack_lifecycle_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn stale_auto_attack_callback_cannot_clear_a_replacements_slot() {
        for current_callback in [false, true] {
            let (server, _client, player, npc) =
                super::super::cast_validation_tests::fixture(crate::data::GameData::empty());
            let skill = SkillRun::new(Arc::new(super::super::tests::skill_template(1)), 1);
            let current = CancelToken::new();
            let scheduled = if current_callback {
                current.clone()
            } else {
                CancelToken::new()
            };
            npc.data().unit.hp = 0;
            {
                let mut p = player.data();
                p.unit.current_target = npc.obj_id;
                p.unit.is_auto_attack = true;
                p.unit.skill_state.auto_attack = Some(AutoAttackSlot {
                    skill: skill.clone(),
                    cancel: current.clone(),
                });
            }
            schedule_auto_attack(&server, player.obj_id, skill, scheduled.clone(), 10);
            tokio::time::sleep(Duration::from_millis(40)).await;
            assert!(scheduled.is_cancelled());
            assert_eq!(current.is_cancelled(), current_callback);
            assert_eq!(
                player.data().unit.skill_state.auto_attack.is_some(),
                !current_callback
            );
            assert_eq!(player.data().unit.is_auto_attack, !current_callback);
        }
    }
}

#[cfg(test)]
#[path = "owned_npc_target_tests.rs"]
mod owned_npc_target_tests;

#[cfg(test)]
#[path = "modifier_flow_tests.rs"]
mod modifier_flow_tests;

#[cfg(test)]
#[path = "cooldown_tag_flow_tests.rs"]
mod cooldown_tag_flow_tests;
