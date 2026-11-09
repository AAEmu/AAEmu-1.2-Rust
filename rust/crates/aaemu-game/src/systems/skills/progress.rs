//! Character progression used by the skill code and its packets: `Character.AddExp`
//! (`Abilities.AddActiveExp`, level up), `ChangeLabor` (`Actability.AddPoint`, labor exp),
//! `ChangeGamePoints`, `CharacterSkills.AddSkill` / `AddBuff` / `Reset`,
//! `CharacterAbilities.Swap` and `PassiveBuff.Apply` / `Remove`.

use std::collections::HashMap;

use aaemu_commons::{Marshal, PacketStream};

use crate::data::formulas::FormulaKind;
use crate::models::char_data::{ability, Skill};
use crate::models::character::Character;
use crate::models::item_task::{task_type, ItemTask};
use crate::models::unit::UnitAttribute;
use crate::net::packet::LEVEL_PLAIN;
use crate::opcodes::sc;
use crate::packets::packet_def;
use crate::server::GameServer;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::world::{now_ms, PlayerRef, WorldObject};

use super::broadcast;
use super::packets::{
    SCAbilitySwappedPacket, SCBuffLearnedPacket, SCExpChangedPacket, SCSkillLearnedPacket,
    SCSkillUpgradedPacket, SCSkillsResetPacket,
};
use super::types::SkillCaster;
use crate::systems::economy::item_packets::SCItemTaskSuccessPacket;

/// `GamePointKind`.
pub const GAME_POINT_HONOR: u8 = 0;
pub const GAME_POINT_VOCATION: u8 = 1;

/// `SCCharacterLaborPowerChangedPacket(amount, action, point, step)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCCharacterLaborPowerChangedPacket {
    pub amount: i32,
    pub action: i32,
    pub point: i32,
    pub step: u8,
}
packet_def!(
    SCCharacterLaborPowerChangedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCCharacterLaborPowerChangedPacket
);
impl Marshal for SCCharacterLaborPowerChangedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_i32(self.amount)
            .write_i32(self.action)
            .write_i32(self.point)
            .write_u8(self.step);
    }
}

/// `SCRecoverableExpPacket(objId, recoverableExp, penaltiedExp, reason)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCRecoverableExpPacket {
    pub obj_id: u32,
    pub recoverable_exp: i32,
    pub penaltied_exp: i32,
    pub reason: i32,
}
packet_def!(
    SCRecoverableExpPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCRecoverableExpPacket
);
impl Marshal for SCRecoverableExpPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.obj_id)
            .write_i32(self.recoverable_exp)
            .write_i32(self.penaltied_exp)
            .write_i32(self.reason);
    }
}

/// `SCGamePointChangedPacket(kind, amount)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCGamePointChangedPacket {
    pub kind: u8,
    pub amount: i32,
}
packet_def!(
    SCGamePointChangedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCGamePointChangedPacket
);
impl Marshal for SCGamePointChangedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u8(self.kind).write_i32(self.amount);
    }
}

/// `SCLevelChangedPacket(objId, level)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCLevelChangedPacket {
    pub obj_id: u32,
    pub level: u8,
}
packet_def!(
    SCLevelChangedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCLevelChangedPacket
);
impl Marshal for SCLevelChangedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.obj_id).write_u8(self.level);
    }
}

/// `Actability.GetLaborCostMultiplier()` (1.2 values by rank).
pub fn labor_cost_multiplier(step: u8) -> f32 {
    const M: [f32; 8] = [1.00, 1.00, 0.95, 0.90, 0.85, 0.80, 0.77, 0.77];
    M[usize::from(step).min(M.len() - 1)]
}

/// `Actability.GetExpMultiplier()` (all 1.0 in 1.2).
fn actability_exp_multiplier(_step: u8) -> f32 {
    1.0
}

/// A learned skill whose level rose with its ability level (`SCSkillUpgraded`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillUpgrade {
    pub skill_id: u32,
    pub level: u8,
}

/// The outcome of [`add_exp_locked`]: what `Character.AddExp` still has to send / raise once the
/// character's lock is released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpGain {
    /// The applied delta (after `ExpRate` and the caster's XP bonuses).
    pub exp: i32,
    pub should_add_ability_exp: bool,
    /// The new level when the character leveled up.
    pub new_level: Option<u8>,
    /// Learned skills whose level rose with an ability level (sent after the lock is released;
    /// the trigger is INFERRED, CAP-UI-2).
    pub upgraded_skills: Vec<SkillUpgrade>,
}

/// The data part of `Character.AddExp(expDelta, shouldAddAbilityExp)` on a locked character
/// (experience, level, `Abilities.AddActiveExp`). `None` when nothing changed.
pub fn add_exp_locked(
    server: &GameServer,
    c: &mut Character,
    mut exp_delta: i32,
    add_ability_exp: bool,
) -> Option<ExpGain> {
    c.unit
        .bonuses
        .bind_attribute_limits(&server.game_data.characters.unit_attribute_limits);
    if exp_delta == 0 {
        return None;
    }
    if exp_delta > 0 {
        // Native 1.2 sub_398A0D60: ExpMul (95) starts at 100, unlike damage/haste's
        // per-mille attributes. Apply Bonuses to that base so Percent rows work too.
        let multiplier = c
            .unit
            .calculate_with_bonuses(100.0, UnitAttribute::ExpMul, now_ms());
        let gain =
            f64::from(exp_delta) * server.config.world.exp_rate.max(0.0) * multiplier.max(0.0)
                / 100.0;
        exp_delta = gain.max(0.0).min(f64::from(i32::MAX)) as i32;
    }
    let exp_table = &server.game_data.experience;
    let mut new_exp = if exp_delta > 0 {
        c.experience.saturating_add(exp_delta)
    } else {
        c.experience.wrapping_add(exp_delta)
    };
    let (new_level, overflow) = match exp_table.level_from_exp(new_exp, c.unit.level, false) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("AddExp for {}: {e}", c.id);
            return None;
        }
    };
    let leveled = new_level > c.unit.level;
    if new_level >= exp_table.max_player_level() {
        new_exp -= overflow;
    }
    c.experience = new_exp;
    c.unit.level = new_level;
    let mut upgraded_skills = Vec::new();
    if add_ability_exp {
        // Abilities.AddActiveExp
        let max_exp = exp_table.exp_for_level(exp_table.max_player_level(), false);
        let actives = [c.ability1, c.ability2, c.ability3];
        for a in actives {
            if a == ability::NONE {
                continue;
            }
            let (old_level, new_level) = {
                let Some(ab) = c.abilities.get_mut(a) else {
                    continue;
                };
                let old_exp = ab.exp;
                let new_exp = if exp_delta > 0 {
                    ab.exp.saturating_add(exp_delta)
                } else {
                    ab.exp + exp_delta
                }
                // An ability already above the cap (a character saved under a higher cap)
                // keeps that progress: never trim it below the larger of the cap and its
                // current exp, on gains and losses alike.
                .min(max_exp.max(old_exp));
                if new_exp == old_exp {
                    continue;
                }
                ab.exp = new_exp;
                let level_of = |exp| {
                    exp_table
                        .level_from_exp_any(exp, false)
                        .map_or(0, |(level, _)| level)
                };
                (level_of(old_exp), level_of(new_exp))
            };
            if new_level <= old_level {
                continue;
            }
            // The learned level is derived from the ability level, so the client has to be
            // told which skills crossed a level_step (INFERRED trigger, CAP-UI-2).
            for s in c.skills.skills.iter_mut() {
                let Some(t) = server.game_data.skills.skill(s.id) else {
                    continue;
                };
                if t.ability_id != a {
                    continue;
                }
                let level = t.level_for(i32::from(new_level));
                if level > t.level_for(i32::from(old_level)) {
                    s.level = level;
                    upgraded_skills.push(SkillUpgrade {
                        skill_id: s.id,
                        level,
                    });
                }
            }
        }
    }
    Some(ExpGain {
        exp: exp_delta,
        should_add_ability_exp: add_ability_exp,
        new_level: leveled.then_some(new_level),
        upgraded_skills,
    })
}

/// `SendPacket(new SCExpChangedPacket(ObjId, expDelta, shouldAddAbilityExp))`.
pub fn send_exp_changed(p: &PlayerRef, gain: &ExpGain) {
    p.send(SCExpChangedPacket {
        obj_id: p.obj_id,
        exp: gain.exp,
        should_add_ability_exp: gain.should_add_ability_exp,
    });
}

/// The world part of `Character.AddExp` after [`send_exp_changed`] (no lock held): on a level
/// up `Expedition?.OnCharacterRefresh` + the `SCLevelChangedPacket` broadcast (and the cached
/// stats of the new level), then `QuestManager.DoOnLevelUpEvents` (raised on every exp change,
/// as in C#). Learned skills whose ability level rose send `SCSkillUpgradedPacket` here, after
/// the lock (INFERRED trigger, CAP-UI-2).
pub fn add_exp_world_tail(server: &GameServer, p: &PlayerRef, gain: &ExpGain) {
    for upgrade in &gain.upgraded_skills {
        p.send(SCSkillUpgradedPacket {
            skill_id: upgrade.skill_id,
            level: upgrade.level,
        });
    }
    if let Some(level) = gain.new_level {
        let expedition_id = p.data().unit.expedition_id;
        if expedition_id != 0 {
            crate::systems::social::expedition::on_character_refresh(server, p, expedition_id);
        }
        let obj = WorldObject::from(p);
        broadcast(
            server,
            &obj,
            &SCLevelChangedPacket {
                obj_id: p.obj_id,
                level,
            },
            true,
        );
        super::stats::refresh_cached_stats(server, &obj);
    }
    crate::systems::quests::events::on_level_up(server, p);
    if gain.new_level.is_some() {
        // CHAR_LEVEL, and the ability levels the exp gain moved: re-report the current state.
        crate::systems::refresh_achievements(server, p);
    }
}

/// `Character.AddExp(expDelta, shouldAddAbilityExp)`.
pub fn add_exp(server: &GameServer, p: &PlayerRef, exp_delta: i32, add_ability_exp: bool) {
    let gain = {
        let mut c = p.data();
        add_exp_locked(server, &mut c, exp_delta, add_ability_exp)
    };
    if let Some(gain) = gain {
        send_exp_changed(p, &gain);
        add_exp_world_tail(server, p, &gain);
    }
}

/// The locked part of `Character.ChangeLabor(change, actabilityId)`: actability points, the
/// labor experience (`AddExp`) and the labor change. Returns the labor packet and the exp
/// gain to finish with [`send_exp_changed`] / [`add_exp_world_tail`]; `None` when the C#
/// throws (unknown actability).
pub fn change_labor_locked(
    server: &GameServer,
    c: &mut Character,
    change: i16,
    actability_id: i32,
) -> Option<(SCCharacterLaborPowerChangedPacket, Option<ExpGain>)> {
    let mut actability_change = 0;
    let mut actability_step = 0u8;
    let mut exp_mul = 1.0f32;
    if actability_id > 0 {
        match c
            .actability
            .actabilities
            .iter_mut()
            .find(|a| a.id == actability_id as u32)
        {
            Some(a) => {
                exp_mul = actability_exp_multiplier(a.step);
                let add = (f64::from(change).abs() * server.systems.skills.config().actability_rate)
                    as i32;
                actability_step = a.step;
                // CharacterActability.AddPoint
                let prev = a.point;
                a.point += add;
                if let Some(l) = server
                    .game_data
                    .characters
                    .expert_limit(usize::from(a.step))
                {
                    if a.point > l.up_limit {
                        a.point = l.up_limit;
                    }
                }
                actability_change = a.point - prev;
            }
            None => {
                tracing::error!(
                    "ChangeLabor: actability {actability_id} not found (KeyNotFoundException)"
                );
                return None;
            }
        }
    }
    let mut gain = None;
    if change < 0 {
        let params = HashMap::from([
            ("labor_power", f64::from(-change)),
            ("pc_level", f64::from(c.unit.level)),
        ]);
        let xp = server
            .game_data
            .formulas
            .get_formula_by_kind(FormulaKind::ExpByLaborPower)
            .map_or(0.0, |f| f.evaluate(&params));
        gain = add_exp_locked(server, c, (xp * f64::from(exp_mul)) as i32, true);
    }
    c.change_labor_power(change);
    Some((
        SCCharacterLaborPowerChangedPacket {
            amount: i32::from(change),
            action: actability_id,
            point: actability_change,
            step: actability_step,
        },
        gain,
    ))
}

/// `Character.ChangeLabor(change, actabilityId)`.
pub fn change_labor(server: &GameServer, p: &PlayerRef, change: i16, actability_id: i32) {
    let res = {
        let mut c = p.data();
        change_labor_locked(server, &mut c, change, actability_id)
    };
    let Some((packet, gain)) = res else {
        return;
    };
    // AddExp runs before the labor packet in the C#.
    if let Some(gain) = &gain {
        send_exp_changed(p, gain);
        add_exp_world_tail(server, p, gain);
    }
    p.send(packet);
    report_labor_spent(server, p, change);
    report_actability(server, p, actability_id);
}

/// GET_ACTABILITY achievement credit: the actability's current points (absolute).
pub fn report_actability(server: &GameServer, p: &PlayerRef, actability_id: i32) {
    if actability_id <= 0 {
        return;
    }
    let point = p
        .data()
        .actability
        .actabilities
        .iter()
        .find(|a| a.id == actability_id as u32)
        .map(|a| i64::from(a.point));
    if let Some(value) = point {
        crate::systems::report_achievement(
            server,
            p,
            AchievementInput::Absolute {
                kind_id: char_record_kind::GET_ACTABILITY,
                value,
                matcher: RecordMatch::Value1(actability_id as u32),
            },
        );
    }
}

/// SPEND_LABOR achievement credit: `change` labor was consumed (a negative change).
pub fn report_labor_spent(server: &GameServer, p: &PlayerRef, change: i16) {
    if change < 0 {
        crate::systems::report_achievement(
            server,
            p,
            AchievementInput::Increment {
                kind_id: char_record_kind::SPEND_LABOR,
                amount: i64::from(-i32::from(change)),
                matcher: RecordMatch::Any,
            },
        );
    }
}

/// The locked part of `Character.ChangeGamePoints(kind, change)`: the packet to send (`None`
/// for an unknown kind).
pub fn change_game_points_locked(
    c: &mut Character,
    kind: u8,
    mut change: i32,
) -> Option<SCGamePointChangedPacket> {
    match kind {
        GAME_POINT_HONOR => c.honor_point += change,
        GAME_POINT_VOCATION => {
            let now = now_ms();
            let add = c.unit.calculate_with_bonuses(
                0.0,
                crate::models::unit::UnitAttribute::LivingPointGain,
                now,
            ) as f32;
            let mul = c.unit.calculate_with_bonuses(
                0.0,
                crate::models::unit::UnitAttribute::LivingPointGainMul,
                now,
            ) as f32;
            change = (change as f32 + add).round_ties_even() as i32;
            let voc_mul = mul + 100.0;
            change = (change as f32 * (voc_mul / 100.0)).round_ties_even() as i32;
            c.vocation_point += change;
        }
        _ => {
            tracing::error!("ChangeGamePoints - Unknown Game Point Type {kind}");
            return None;
        }
    }
    Some(SCGamePointChangedPacket {
        kind,
        amount: change,
    })
}

/// `Character.ChangeGamePoints(kind, change)`.
pub fn change_game_points(server: &GameServer, p: &PlayerRef, kind: u8, change: i32) {
    let packet = {
        let mut c = p.data();
        change_game_points_locked(&mut c, kind, change)
    };
    if let Some(packet) = packet {
        let amount = packet.amount;
        p.send(packet);
        report_life_points_gained(server, p, kind, amount);
    }
}

/// The honor gain that a `UnitAttribute` add / multiplier pair belongs to (`HonorPointGain*`
/// 126-135). War is the conflict / siege / war PvP kill source, NpcKill the monster hunt
/// (buff 6442's text: "+1 honor per monster hunted; ... +1 during conflict/siege; +4 during
/// war"; the numeric mapping comes from the names and that text, INFERRED).
///
/// Trial has no grant site in the 1.2 flow; the source exists so the mapping is complete
/// (UNKNOWN which flow should call it, see PORTING.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HonorSource {
    NpcKill,
    Trial,
    War,
    Quest,
    BattleField,
}

impl HonorSource {
    fn attributes(self) -> (UnitAttribute, UnitAttribute) {
        use UnitAttribute::*;
        match self {
            HonorSource::NpcKill => (HonorPointGainNpcKill, HonorPointGainNpcKillMul),
            HonorSource::Trial => (HonorPointGainTrial, HonorPointGainTrialMul),
            HonorSource::War => (HonorPointGainWar, HonorPointGainWarMul),
            HonorSource::Quest => (HonorPointGainQuest, HonorPointGainQuestMul),
            HonorSource::BattleField => (HonorPointGainBattleField, HonorPointGainBattleFieldMul),
        }
    }
}

/// The attribute-modified amount of a positive honor gain: `round(base + add)`, then
/// `round(change * (mul + 100) / 100)`, clamped at 0 (the `LivingPointGain` pattern above;
/// buffs 6266/6267 add / multiply by 100). Non-positive changes pass through unchanged: the
/// spend and death-loss paths do not apply gain attributes.
pub fn honor_gain(c: &Character, source: HonorSource, base: i32, now: i64) -> i32 {
    if base <= 0 {
        return base;
    }
    let (add_attr, mul_attr) = source.attributes();
    let add = c.unit.calculate_with_bonuses(0.0, add_attr, now) as f32;
    let mul = c.unit.calculate_with_bonuses(0.0, mul_attr, now) as f32;
    let change = (base as f32 + add).round_ties_even() as i32;
    let change = (change as f32 * ((mul + 100.0) / 100.0)).round_ties_even() as i32;
    change.max(0)
}

/// [`honor_gain`] on a character the caller already holds the lock for (a quest reward act):
/// the `SCGamePointChanged` packet is for the caller to send.
pub fn grant_honor_locked(
    c: &mut Character,
    source: HonorSource,
    base: i32,
) -> Option<SCGamePointChangedPacket> {
    let change = honor_gain(c, source, base, now_ms());
    change_game_points_locked(c, GAME_POINT_HONOR, change)
}

/// `Character.ChangeGamePoints(GAME_POINT_HONOR, ..)` for a positive honor gain: applies the
/// source attribute pair, then credits and sends `SCGamePointChanged`.
pub fn grant_honor(server: &GameServer, p: &PlayerRef, source: HonorSource, base: i32) {
    let packet = {
        let mut c = p.data();
        grant_honor_locked(&mut c, source, base)
    };
    if let Some(packet) = packet {
        let amount = packet.amount;
        p.send(packet);
        report_life_points_gained(server, p, GAME_POINT_HONOR, amount);
    }
}

/// The honor a War-zone death costs: `base` scaled by `HonorPointLoseMul`
/// (`(mul + 100) / 100`; buff 6303 `-100` means no loss), clamped at 0 and to the character's
/// current honor. The current honor is read and debited under one character lock; returns the
/// applied loss.
pub fn lose_honor_on_death(p: &PlayerRef, base: i32) -> i32 {
    let (packet, loss) = {
        let mut c = p.data();
        let now = now_ms();
        let mul = c
            .unit
            .calculate_with_bonuses(0.0, UnitAttribute::HonorPointLoseMul, now)
            as f32;
        let scaled = (base as f32 * ((mul + 100.0) / 100.0)).round_ties_even() as i32;
        let loss = scaled.max(0).min(c.honor_point);
        let packet = if loss > 0 {
            change_game_points_locked(&mut c, GAME_POINT_HONOR, -loss)
        } else {
            None
        };
        (packet, loss)
    };
    if let Some(packet) = packet {
        p.send(packet);
    }
    loss
}

/// GET_LIFE_POINT achievement credit: vocation (living) points gained. Honor, and every spend,
/// never credits.
pub fn report_life_points_gained(server: &GameServer, p: &PlayerRef, kind: u8, amount: i32) {
    if kind == GAME_POINT_VOCATION && amount > 0 {
        crate::systems::report_achievement(
            server,
            p,
            AchievementInput::Increment {
                kind_id: char_record_kind::GET_LIFE_POINT,
                amount: i64::from(amount),
                matcher: RecordMatch::Any,
            },
        );
    }
}

fn active_abilities(p: &PlayerRef) -> [u8; 3] {
    let c = p.data();
    [c.ability1, c.ability2, c.ability3]
}

/// `CharacterSkills.AddSkill(skillId)` (`CSLearnSkillPacket`).
pub fn learn_skill(server: &GameServer, p: &PlayerRef, skill_id: u32) {
    let skills = &server.game_data.skills;
    let Some(t) = skills.skill(skill_id) else {
        tracing::error!("AddSkill: skill {skill_id} has no template (NullReferenceException)");
        return;
    };
    let actives = active_abilities(p);
    if t.ability_id > 0 && !actives.contains(&t.ability_id) {
        return;
    }
    let ab = {
        let c = p.data();
        crate::db_character::ab_level(server, &c, t.ability_id)
    };
    // The 1.2 client offers a skill only from its ability level on, so retail learns nothing
    // below it (and never the hidden ability_level 99 rows such as 10385); the C# had no check,
    // computed a wrapped byte and then failed every save (1264, see PORTING.md).
    if t.ability_level > ab {
        return;
    }
    let (level, used) = {
        let c = p.data();
        (c.unit.level, c.skills.used_skill_points(skills, 0))
    };
    let points = server.game_data.experience.skill_points_for_level(level) - used;
    if t.skill_points > points {
        return;
    }
    let existing = p
        .data()
        .skills
        .skills
        .iter()
        .find(|s| s.id == skill_id)
        .copied();
    let skill = match existing {
        Some(s) => s,
        None => {
            let s = Skill {
                id: t.id,
                level: t.level_for(ab),
            };
            p.data().skills.skills.push(s);
            s
        }
    };
    p.send(SCSkillLearnedPacket {
        skill_id: skill.id,
        level: skill.level,
    });
}

/// `CharacterSkills.AddBuff(buffId)` (`CSLearnBuffPacket`).
pub fn learn_buff(server: &GameServer, p: &PlayerRef, buff_id: u32) {
    let skills = &server.game_data.skills;
    let Some(t) = skills.passive_buff(buff_id).copied() else {
        tracing::error!("AddBuff: passive buff {buff_id} has no template (NullReferenceException)");
        return;
    };
    let actives = active_abilities(p);
    if t.ability_id > 0 && !actives.contains(&t.ability_id) {
        return;
    }
    let (level, used_all, used_tree, known) = {
        let c = p.data();
        (
            c.unit.level,
            c.skills.used_skill_points(skills, 0),
            c.skills.used_skill_points(skills, t.ability_id),
            c.skills.passive_buffs.contains(&buff_id),
        )
    };
    let points = server.game_data.experience.skill_points_for_level(level) - used_all;
    if points < 1 || used_tree < t.req_points || known {
        return;
    }
    p.data().skills.passive_buffs.push(buff_id);
    let obj = WorldObject::from(p);
    broadcast(
        server,
        &obj,
        &SCBuffLearnedPacket {
            obj_id: p.obj_id,
            buff_id,
        },
        true,
    );
    apply_passive_buff(server, p, buff_id);
}

/// `PassiveBuff.Apply(owner)`.
pub fn apply_passive_buff(server: &GameServer, p: &PlayerRef, passive_id: u32) {
    let Some(pt) = server.game_data.skills.passive_buff(passive_id).copied() else {
        return;
    };
    let Some(t) = server.game_data.systems.skills.buff(pt.buff_id).cloned() else {
        tracing::error!("PassiveBuff {passive_id}: no buff template {}", pt.buff_id);
        return;
    };
    let obj = WorldObject::from(p);
    let mut b = super::buffs::new_buff(
        server,
        &t,
        Some(&obj),
        &SkillCaster::Unit(0),
        None,
        now_ms(),
    );
    b.passive = true;
    super::buffs::add_buff(server, &obj, b, 0, 0);
}

/// `PassiveBuff.Remove(owner)`.
pub fn remove_passive_buff(server: &GameServer, p: &PlayerRef, passive_id: u32) {
    if let Some(pt) = server.game_data.skills.passive_buff(passive_id).copied() {
        super::buffs::remove_buff(server, &WorldObject::from(p), pt.buff_id);
    }
}

/// The data part of `CharacterSkills.Reset(abilityId)`, run under the caller's character lock.
/// Returns the passive buff ids to remove after the lock is released.
fn reset_skills_locked(
    skills: &crate::data::skills::SkillData,
    c: &mut Character,
    ability_id: u8,
) -> Vec<u32> {
    let mut removed = Vec::new();
    let keep: Vec<Skill> = c
        .skills
        .skills
        .iter()
        .filter(|s| {
            let ab = skills.skill(s.id).map_or(0, |t| t.ability_id);
            if ab == ability_id {
                removed.push(s.id);
                false
            } else {
                true
            }
        })
        .copied()
        .collect();
    c.skills.skills = keep;
    let mut buffs = Vec::new();
    let keep_buffs: Vec<u32> = c
        .skills
        .passive_buffs
        .iter()
        .filter(|b| {
            let ab = skills.passive_buff(**b).map_or(0, |t| t.ability_id);
            if ab == ability_id {
                buffs.push(**b);
                false
            } else {
                true
            }
        })
        .copied()
        .collect();
    c.skills.passive_buffs = keep_buffs;
    c.skills.removed.extend(removed);
    c.skills.removed.extend(buffs.iter().copied());
    buffs
}

/// `CharacterSkills.Reset(abilityId)` (`CSResetSkillsPacket`).
pub fn reset_skills(server: &GameServer, p: &PlayerRef, ability_id: u8) {
    let removed_buffs = {
        let mut c = p.data();
        reset_skills_locked(&server.game_data.skills, &mut c, ability_id)
    };
    for b in removed_buffs {
        remove_passive_buff(server, p, b);
    }
    let obj = WorldObject::from(p);
    broadcast(
        server,
        &obj,
        &SCSkillsResetPacket {
            obj_id: p.obj_id,
            ability: ability_id,
        },
        true,
    );
}

/// `CharacterAbilities.Swap(oldAbilityId, abilityId)` (`CSSwapAbilityPacket`).
///
/// The 1.2 client prices an ability change as `content_configs[39] * character level` copper
/// (`X2Ability.GetAbilityChangeCost`, `ability_change.alb` 0.8) and pays in gold
/// (`GetCurrencyForAbilityChange` -> 0 = `CURRENCY_GOLD`; the 1.2 `currency_configs` table is
/// empty). `X2Ability.SwapAbility` (`FUN_39386600`) refuses unless exactly three abilities are
/// active, the old one is active and the new one is not (`FUN_398a1580`, "all active abilities
/// must be activated" / "the ability is already active" / "the ability is already deactivated").
/// The level-10 ability activation (`X2Ability.LearnAbility` -> `FUN_39384a20`) instead sends
/// old = `AbilityType.NONE` and never calls the cost check, so activating an empty slot is free.
/// Both client paths reach this function; the checks, the debit and the swap share one character
/// lock, and the packets are sent after it.
pub fn swap_ability(server: &GameServer, p: &PlayerRef, old: u8, new: u8) {
    let (removed_buffs, cost, money) = {
        let mut c = p.data();
        let actives = [c.ability1, c.ability2, c.ability3];
        // `FUN_398a1710`: only AbilityType 1..=10 (Fight .. Love) are class abilities.
        if !(1..=10).contains(&new) {
            tracing::warn!("SwapAbility: invalid ability id {new} (accepted: 1..=10)");
            return;
        }
        if actives.contains(&new) {
            tracing::warn!("SwapAbility: ability {new} is already active");
            return;
        }
        if old == ability::NONE {
            if !actives.contains(&ability::NONE) {
                tracing::warn!("SwapAbility: no empty ability slot to activate {new}");
                return;
            }
        } else {
            if !actives.contains(&old) {
                tracing::warn!("SwapAbility: ability {old} is not active (old {old} -> {new})");
                return;
            }
            if actives.contains(&ability::NONE) {
                tracing::warn!(
                    "SwapAbility: not all three abilities are active (old {old} -> {new})"
                );
                return;
            }
        }
        // `FUN_398a19d0`: content_configs[39] * level. Filling an empty slot is the free
        // `LearnAbility` path (no cost call in `FUN_39384a20`).
        let cost = if old == ability::NONE {
            0
        } else {
            i64::from(server.game_data.systems.skills.ability_change_cost)
                .saturating_mul(i64::from(c.unit.level))
                .max(0)
        };
        if cost > 0 && c.money < cost {
            drop(c);
            tracing::warn!(
                character = p.character_id,
                old,
                new,
                cost,
                "SwapAbility: not enough money"
            );
            crate::systems::economy::inventory::send_error_message(
                p,
                crate::systems::economy::error_message::NOT_ENOUGH_MONEY,
                0,
                true,
            );
            return;
        }
        let removed_buffs = reset_skills_locked(&server.game_data.skills, &mut c, old);
        if c.ability1 == old {
            c.ability1 = new;
            if let Some(a) = c.abilities.get_mut(new) {
                a.order = 0;
            }
        } else if c.ability2 == old {
            c.ability2 = new;
            if let Some(a) = c.abilities.get_mut(new) {
                a.order = 1;
            }
            if old == ability::NONE {
                let exp1 = c
                    .abilities
                    .abilities
                    .iter()
                    .find(|a| a.id == c.ability1)
                    .map_or(0, |a| a.exp);
                let a2 = c.ability2;
                if let Some(a) = c.abilities.get_mut(a2) {
                    a.exp = exp1;
                }
            }
        } else if c.ability3 == old {
            c.ability3 = new;
            if let Some(a) = c.abilities.get_mut(new) {
                a.order = 2;
            }
            if old == ability::NONE {
                let exp1 = c
                    .abilities
                    .abilities
                    .iter()
                    .find(|a| a.id == c.ability1)
                    .map_or(0, |a| a.exp);
                let a3 = c.ability3;
                if let Some(a) = c.abilities.get_mut(a3) {
                    a.exp = exp1;
                }
                let actives = [c.ability1, c.ability2, c.ability3];
                for a in c.abilities.abilities.iter_mut() {
                    if !actives.contains(&a.id) {
                        a.exp = 42000;
                    }
                }
            }
        }
        if old != ability::NONE {
            if let Some(a) = c.abilities.get_mut(old) {
                a.order = 255;
            }
        }
        if cost > 0 {
            c.money -= cost;
        }
        (removed_buffs, cost, c.money)
    };
    for b in removed_buffs {
        remove_passive_buff(server, p, b);
    }
    let obj = WorldObject::from(p);
    broadcast(
        server,
        &obj,
        &SCSkillsResetPacket {
            obj_id: p.obj_id,
            ability: old,
        },
        true,
    );
    if cost > 0 {
        p.send(SCItemTaskSuccessPacket {
            action: task_type::ABILITY_CHANGE,
            tasks: vec![ItemTask::MoneyChange(-(cost as i32))],
            force_remove: Vec::new(),
        });
    }
    broadcast(
        server,
        &obj,
        &SCAbilitySwappedPacket {
            obj_id: p.obj_id,
            old_ability: old,
            ability: new,
        },
        true,
    );
    tracing::info!(
        character = p.character_id,
        old,
        new,
        cost,
        money,
        "SwapAbility: ability changed"
    );
    crate::systems::report_achievement(
        server,
        p,
        AchievementInput::Increment {
            kind_id: char_record_kind::ABILITY_CHANGE,
            amount: 1,
            matcher: RecordMatch::Any,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::character_templates::ExpertLimit;
    use crate::data::GameData;
    use crate::models::char_data::{Actability, CharacterAbilities};
    use crate::models::item_task::item_action;
    use crate::net::registry::ClientPacket;
    use crate::systems::economy::error_message;
    use crate::systems::skills::handlers::CSSwapAbilityPacket;
    use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};

    #[tokio::test]
    async fn learn_skill_refuses_a_skill_above_the_ability_level() {
        // The sweep path: `/level 55` then `CSLearnSkillPacket` for the retired ability_level 99
        // rows (10385, 11854, ...). The client never offers them, so retail never learns them;
        // the port used to learn them at a wrapped level 243, which blocked the character save
        // (MySQL 1264).
        let mut data = GameData::empty();
        data.experience = crate::test_support::test_experience(10);
        data.skills.add_skill(crate::data::skills::SkillTemplate {
            id: 10385,
            ability_id: 0,
            ability_level: 99,
            level_step: 3,
            skill_points: 1,
            ..Default::default()
        });
        data.skills.add_skill(crate::data::skills::SkillTemplate {
            id: 5,
            ability_id: 0,
            ability_level: 5,
            level_step: 3,
            skill_points: 0,
            ..Default::default()
        });
        let server = test_server_with(data);
        let mut client = TestClient::new(&server, 1, 1);
        let mut character = test_character(1, "Learner", 100.0, 100.0);
        character.unit.level = 10; // GetAbLevel(General) = 10: below 99, above 5
        let player = spawn_player(&server, &client, character, 0x100);
        client.drain(); // spawn traffic

        learn_skill(&server, &player, 10385);
        assert!(player.data().skills.skills.is_empty());
        assert!(client.drain().is_empty());

        learn_skill(&server, &player, 5);
        assert_eq!(player.data().skills.skills, [Skill { id: 5, level: 2 }]);
        assert!(client.drain_ops().contains(&sc::SCSkillLearnedPacket));
    }

    #[tokio::test]
    async fn labor_actability_uses_zero_based_tiers_and_reports_actual_capped_gain() {
        let mut data = GameData::empty();
        // Real 1.2 ordering: row ids start at 1; actability steps start at 0.
        data.characters.expert_limits = [10_000, 20_000, 30_000, 40_000, 50_000, 70_000]
            .into_iter()
            .enumerate()
            .map(|(index, up_limit)| ExpertLimit {
                id: index as u32 + 1,
                up_limit,
                expert_limit_count: 0,
            })
            .collect();
        let server = test_server_with(data);
        for (step, before, after, earned) in [
            (0, 9_995, 10_000, 5),
            (0, 10_000, 10_000, 0),
            (4, 49_995, 50_000, 5),
            (4, 50_000, 50_000, 0),
            (5, 50_000, 50_010, 10),
        ] {
            let mut character = test_character(1, "Fisher", 500.0, 500.0);
            character.labor_power = 500;
            character.actability.actabilities.push(Actability {
                id: 7,
                point: before,
                step,
            });
            let (packet, _) = change_labor_locked(&server, &mut character, -10, 7).unwrap();
            assert_eq!(character.actability.actabilities[0].point, after);
            assert_eq!(character.actability.actabilities[0].step, step);
            assert_eq!(character.labor_power, 490);
            assert_eq!(packet.point, earned);
            assert!(packet.point >= 0);
            let mut expected = (-10i32).to_le_bytes().to_vec();
            expected.extend(7i32.to_le_bytes());
            expected.extend(earned.to_le_bytes());
            expected.push(step);
            assert_eq!(crate::net::packet::encode(&packet)[6..], expected);
        }
    }

    fn swap_fixture(
        ability_change_cost: i32,
        level: u8,
        money: i64,
        actives: [u8; 3],
    ) -> (GameServer, TestClient, PlayerRef) {
        let mut data = GameData::empty();
        data.systems.skills.ability_change_cost = ability_change_cost;
        let server = test_server_with(data);
        let mut client = TestClient::new(&server, 1, 1);
        let mut character = test_character(1, "Swapper", 100.0, 100.0);
        character.unit.level = level;
        character.money = money;
        [character.ability1, character.ability2, character.ability3] = actives;
        character.abilities = CharacterAbilities::default();
        for (order, id) in actives.iter().enumerate() {
            character.abilities.set_ability(*id, order as u8);
        }
        let player = spawn_player(&server, &client, character, 0x100);
        client.conn.active_char = Some(player.clone());
        client.drain();
        (server, client, player)
    }

    async fn send_swap(server: &GameServer, client: &mut TestClient, old: u8, new: u8) {
        CSSwapAbilityPacket {
            obj_id: 0x100,
            old_ability: old,
            ability: new,
            auap: false,
        }
        .handle(server, &mut client.conn)
        .await
        .expect("swap packet");
    }

    #[tokio::test]
    async fn swap_ability_charges_content_config_per_level_and_swaps() {
        // content_configs 39 = 200, level 10 -> 2000 copper. The client's
        // `GetAbilityChangeCost` is `FUN_395720E0(0x27) * level` (`FUN_398a19d0`).
        let (server, mut client, player) = swap_fixture(200, 10, 5_000, [1, 2, 3]);
        send_swap(&server, &mut client, 3, 4).await;

        {
            let c = player.data();
            assert_eq!(c.money, 3_000);
            assert_eq!(c.ability3, 4);
            assert_eq!(
                c.abilities
                    .abilities
                    .iter()
                    .find(|a| a.id == 4)
                    .unwrap()
                    .order,
                2
            );
            assert_eq!(
                c.abilities
                    .abilities
                    .iter()
                    .find(|a| a.id == 3)
                    .unwrap()
                    .order,
                255
            );
        }
        let frames = client.drain();
        let (_, body) = frames
            .iter()
            .find(|(op, _)| *op == sc::SCItemTaskSuccessPacket)
            .expect("money change task");
        assert_eq!(body[0], task_type::ABILITY_CHANGE);
        assert_eq!(body[1], 1);
        assert_eq!(body[2], item_action::CHANGE_MONEY_AMOUNT);
        assert_eq!(
            i32::from_le_bytes(body[3..7].try_into().unwrap()),
            -2_000,
            "gold must decrease by content_configs[39] * level"
        );
        let (_, swapped) = frames
            .iter()
            .find(|(op, _)| *op == sc::SCAbilitySwappedPacket)
            .expect("swap broadcast");
        let mut s = PacketStream::from_bytes(swapped.clone());
        assert_eq!(s.read_bc().unwrap(), 0x100);
        assert_eq!(s.read_u8().unwrap(), 3);
        assert_eq!(s.read_u8().unwrap(), 4);
    }

    #[tokio::test]
    async fn swap_ability_refuses_insufficient_gold_without_swapping() {
        let (server, mut client, player) = swap_fixture(200, 10, 1_999, [1, 2, 3]);
        send_swap(&server, &mut client, 3, 4).await;

        let c = player.data();
        assert_eq!(c.money, 1_999);
        assert_eq!(c.ability3, 3);
        assert_eq!(
            c.abilities
                .abilities
                .iter()
                .find(|a| a.id == 4)
                .unwrap()
                .order,
            255
        );
        drop(c);
        let frames = client.drain();
        assert!(!frames
            .iter()
            .any(|(op, _)| *op == sc::SCAbilitySwappedPacket));
        assert!(!frames
            .iter()
            .any(|(op, _)| *op == sc::SCItemTaskSuccessPacket));
        let (_, error) = frames
            .iter()
            .find(|(op, _)| *op == sc::SCErrorMsgPacket)
            .expect("error response");
        assert_eq!(
            i16::from_le_bytes([error[0], error[1]]),
            error_message::NOT_ENOUGH_MONEY
        );
    }

    #[tokio::test]
    async fn swap_ability_refuses_invalid_duplicate_or_inactive_abilities() {
        let (server, mut client, player) = swap_fixture(200, 10, 5_000, [1, 2, 3]);
        for (old, new) in [
            (3, 0),  // General is not a class ability
            (3, 11), // None is not an ability
            (3, 12), // outside AbilityType 1..=10
            (3, 1),  // already active
            (3, 3),  // old == new, already active
            (5, 4),  // old is not an active slot
        ] {
            send_swap(&server, &mut client, old, new).await;
            let c = player.data();
            assert_eq!(c.ability1, 1, "old {old} -> new {new}");
            assert_eq!(c.ability2, 2, "old {old} -> new {new}");
            assert_eq!(c.ability3, 3, "old {old} -> new {new}");
            assert_eq!(c.money, 5_000, "old {old} -> new {new}");
        }
        assert!(client
            .drain()
            .iter()
            .all(|(op, _)| *op != sc::SCAbilitySwappedPacket));

        // The ability-change UI requires three active abilities (`FUN_398a1580`).
        let (server, mut client, player) = swap_fixture(200, 10, 5_000, [1, 2, ability::NONE]);
        send_swap(&server, &mut client, 1, 4).await;
        let c = player.data();
        assert_eq!(c.ability1, 1);
        assert_eq!(c.ability3, ability::NONE);
        assert_eq!(c.money, 5_000);
        assert!(!client
            .drain()
            .iter()
            .any(|(op, _)| *op == sc::SCAbilitySwappedPacket));
    }

    #[tokio::test]
    async fn swap_ability_activates_an_empty_slot_for_free() {
        // Level-10 ability activation: `X2Ability.LearnAbility` -> `FUN_39384a20` sends
        // old = None and never calls `GetAbilityChangeCost` / `CanBuyAbilityChange`.
        let (server, mut client, player) =
            swap_fixture(200, 10, 0, [1, ability::NONE, ability::NONE]);
        send_swap(&server, &mut client, ability::NONE, 2).await;

        let c = player.data();
        assert_eq!(c.ability2, 2);
        assert_eq!(c.money, 0);
        drop(c);
        let frames = client.drain();
        assert!(!frames
            .iter()
            .any(|(op, _)| *op == sc::SCItemTaskSuccessPacket));
        assert!(frames
            .iter()
            .any(|(op, _)| *op == sc::SCAbilitySwappedPacket));
    }

    #[tokio::test]
    async fn the_era_cap_stops_experience_at_50_and_keeps_an_above_cap_level() {
        let mut data = GameData::empty();
        data.experience = crate::data::ExperienceTable::from_templates(
            (1..=55u8)
                .map(|level| crate::data::LevelTemplate {
                    level,
                    total_exp: i32::from(level) * 1_000,
                    total_mate_exp: i32::from(level) * 1_000,
                    skill_points: i32::from(level),
                })
                .collect(),
            50,
        )
        .expect("cap 50 table");
        let server = test_server_with(data);
        let mut client = TestClient::new(&server, 1, 1);
        let mut character = test_character(1, "Capped", 100.0, 100.0);
        character.unit.level = 50;
        character.experience = 50_000;
        let player = spawn_player(&server, &client, character, 0x100);
        client.drain();

        // At the cap: the gain neither levels up nor accumulates beyond the level total.
        add_exp(&server, &player, 10_000, false);
        {
            let c = player.data();
            assert_eq!((c.unit.level, c.experience), (50, 50_000));
        }

        // A character saved above the cap keeps 53 and still gains no level.
        player.data().unit.level = 53;
        player.data().experience = 53_000;
        add_exp(&server, &player, 10_000, false);
        {
            let c = player.data();
            assert_eq!((c.unit.level, c.experience), (53, 53_000));
        }
    }

    #[tokio::test]
    async fn ability_experience_above_the_cap_is_not_trimmed_by_gains_or_losses() {
        let mut data = GameData::empty();
        data.experience = crate::data::ExperienceTable::from_templates(
            (1..=55u8)
                .map(|level| crate::data::LevelTemplate {
                    level,
                    total_exp: i32::from(level) * 1_000,
                    total_mate_exp: i32::from(level) * 1_000,
                    skill_points: i32::from(level),
                })
                .collect(),
            50,
        )
        .expect("cap 50 table");
        let server = test_server_with(data);
        let mut client = TestClient::new(&server, 1, 1);
        let mut character = test_character(1, "Capped", 100.0, 100.0);
        character.unit.level = 53;
        character.experience = 53_000;
        character.ability1 = 1;
        character.ability2 = ability::NONE;
        character.ability3 = ability::NONE;
        character.abilities.get_mut(1).unwrap().exp = 55_000;
        let player = spawn_player(&server, &client, character, 0x100);
        client.drain();

        let ability_exp = |p: &crate::world::PlayerRef| {
            p.data()
                .abilities
                .abilities
                .iter()
                .find(|a| a.id == 1)
                .unwrap()
                .exp
        };
        // A gain above the cap leaves the saved ability exp untouched (the old clamp
        // lowered 55k to the level-50 total, 50k, and the next save persisted it).
        add_exp(&server, &player, 1_000, true);
        assert_eq!(ability_exp(&player), 55_000);
        // A loss reduces it but does not snap it down to the cap.
        add_exp(&server, &player, -1_000, true);
        assert_eq!(ability_exp(&player), 54_000);
    }

    #[tokio::test]
    async fn an_ability_level_rise_upgrades_only_the_learned_skills_that_cross_a_step() {
        use crate::data::skills::SkillTemplate;

        let mut data = GameData::empty();
        data.experience = crate::test_support::test_experience(55);
        let template = |id, ability_id, level_step| SkillTemplate {
            id,
            ability_id,
            ability_level: 1,
            level_step,
            ..Default::default()
        };
        // Ability 1: 101 crosses (step 3) and 103 crosses (step 1); 102 does not (step 10).
        data.skills.add_skill(template(101, 1, 3));
        data.skills.add_skill(template(102, 1, 10));
        data.skills.add_skill(template(103, 1, 1));
        // Ability 2 is inactive, so its exp does not move: 201 must stay untouched.
        data.skills.add_skill(template(201, 2, 3));
        let server = test_server_with(data);
        let mut client = TestClient::new(&server, 1, 1);
        let mut character = test_character(1, "Upgrader", 100.0, 100.0);
        character.ability1 = 1;
        character.ability2 = ability::NONE;
        character.ability3 = ability::NONE;
        // level_from_exp(9_000) = 9, level_from_exp(10_000) = 10.
        character.abilities.get_mut(1).unwrap().exp = 9_000;
        character.abilities.get_mut(2).unwrap().exp = 9_000;
        character.skills.skills = vec![
            Skill { id: 101, level: 3 },
            Skill { id: 102, level: 1 },
            Skill { id: 103, level: 9 },
            Skill { id: 201, level: 3 },
        ];
        let player = spawn_player(&server, &client, character, 0x100);
        client.drain();

        add_exp(&server, &player, 1_000, true);

        let frames = client.drain();
        let mut upgraded: Vec<(u32, u8)> = frames
            .iter()
            .filter(|(op, _)| *op == sc::SCSkillUpgradedPacket)
            .map(|(_, body)| {
                (
                    u32::from_le_bytes(body[0..4].try_into().expect("skill id")),
                    body[4],
                )
            })
            .collect();
        upgraded.sort();
        assert_eq!(upgraded, [(101, 4), (103, 10)]);
        assert_eq!(
            player.data().skills.skills,
            [
                Skill { id: 101, level: 4 },
                Skill { id: 102, level: 1 },
                Skill { id: 103, level: 10 },
                Skill { id: 201, level: 3 },
            ]
        );

        // A gain that crosses no step sends nothing.
        {
            let mut c = player.data();
            c.abilities.get_mut(1).unwrap().exp = 9_000;
            c.skills.skills[0].level = 3;
            c.skills.skills[2].level = 9;
        }
        client.drain();
        add_exp(&server, &player, 1, true);
        assert!(!client.drain_ops().contains(&sc::SCSkillUpgradedPacket));
    }
}
