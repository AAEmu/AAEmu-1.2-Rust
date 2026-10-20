//! The part of `SkillManager.Load` the character lifecycle needs: `skills` (learning fields),
//! `default_skills`, `passive_buffs`, `buffs` (wire/duration/persistence fields) and the
//! derived `_startAbilitySkills` / `_commonSkills` lists. Effects, plots, modifiers, tags and
//! combat buffs are later phases.

use std::collections::HashMap;

use rusqlite::Connection;

use super::sqlite::for_each_row;

/// `AbilityType.None`.
pub const ABILITY_NONE: u8 = 11;

/// `Skill(template, owner)` level: `(abLevel - AbilityLevel) / LevelStep + 1`, truncated to a
/// byte like the C# cast (`(byte)`). This is the level of a runtime `SkillRun`, NPCs included:
/// their skill levels are deliberately unchanged (an NPC below a skill's `ability_level` still
/// wraps, exactly as in the C#).
pub fn skill_level(ability_level: i32, level_step: i32, ab_level: i32) -> u8 {
    if level_step > 0 {
        ((ab_level - ability_level) / level_step + 1) as u8
    } else {
        1
    }
}

/// [`skill_level`] saturated to `1..=127`, for the level a *character* stores and saves.
///
/// `skills.level` is a signed `TINYINT` and the ability-level 99 skills of the 1.2 data (the
/// retired / login-stage rows, e.g. 10385) are above the level cap 55: the raw formula goes
/// negative and `as u8` turned it into 243, which made every character save fail with MySQL
/// `1264 Out of range value for column 'level'`.
pub fn stored_skill_level(ability_level: i32, level_step: i32, ab_level: i32) -> u8 {
    if level_step > 0 {
        ((ab_level - ability_level) / level_step + 1).clamp(1, i32::from(i8::MAX)) as u8
    } else {
        1
    }
}

/// `SkillTemplate` (subset).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SkillTemplate {
    pub id: u32,
    pub show: bool,
    pub ability_id: u8,
    pub ability_level: i32,
    pub level_step: i32,
    pub skill_points: i32,
    pub auto_learn: bool,
    pub need_learn: bool,
}

impl SkillTemplate {
    /// The level of a skill a character learns and saves (see [`stored_skill_level`]).
    pub fn level_for(&self, ab_level: i32) -> u8 {
        stored_skill_level(self.ability_level, self.level_step, ab_level)
    }
}

/// `DefaultSkill`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultSkill {
    pub skill_id: u32,
    pub slot: u8,
    pub add_to_slot: bool,
}

/// `PassiveBuffTemplate`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassiveBuffTemplate {
    pub id: u32,
    pub ability_id: u8,
    pub level: u8,
    pub buff_id: u32,
    pub req_points: i32,
    pub active: bool,
}

/// `BuffKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuffKind {
    Good,
    Bad,
    Hidden,
    /// Any other `kind_id` (C# `GetAllBuffs` throws `NotSupportedException` on it).
    Other(i32),
}

impl From<i32> for BuffKind {
    fn from(v: i32) -> Self {
        match v {
            1 => Self::Good,
            2 => Self::Bad,
            3 => Self::Hidden,
            o => Self::Other(o),
        }
    }
}

/// `BuffSaveRuleType`.
pub mod save_rule {
    pub const DONT_SAVE: u32 = 0;
    pub const NORMAL: u32 = 1;
    pub const CHARACTER_PERSISTENT: u32 = 2;
}

/// `BuffStackRule`.
pub mod stack_rule {
    pub const REFRESH: u32 = 1;
    pub const CHARGE_REFRESH: u32 = 2;
    pub const EXTEND: u32 = 5;
}

/// `BuffTemplate` (subset).
#[derive(Debug, Clone, PartialEq)]
pub struct BuffTemplate {
    pub id: u32,
    pub kind: BuffKind,
    pub duration: i32,
    pub level_duration: i32,
    pub tick: f64,
    pub stack_rule: u32,
    pub max_stack: i32,
    pub init_min_charge: i32,
    pub save_rule_id: u32,
    pub real_time: bool,
}

impl BuffTemplate {
    /// `GetDuration(abLevel)`.
    pub fn get_duration(&self, ab_level: u32) -> i32 {
        (self.level_duration.wrapping_mul(ab_level as i32))
            .wrapping_add(self.duration)
            .max(0)
    }

    /// A template for tests.
    pub fn simple(id: u32, kind: BuffKind) -> Self {
        Self {
            id,
            kind,
            duration: 0,
            level_duration: 0,
            tick: 0.0,
            stack_rule: 0,
            max_stack: 0,
            init_min_charge: 0,
            save_rule_id: 0,
            real_time: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SkillData {
    skills: Vec<SkillTemplate>,
    skill_index: HashMap<u32, usize>,
    /// `_defaultSkills.Values` in insertion order.
    pub default_skills: Vec<DefaultSkill>,
    pub passive_buffs: HashMap<u32, PassiveBuffTemplate>,
    pub buffs: HashMap<u32, BuffTemplate>,
    /// `_startAbilitySkills` (skill ids, in `_skills` order).
    pub start_ability_skills: HashMap<u8, Vec<u32>>,
    /// `_commonSkills`.
    pub common_skills: Vec<u32>,
}

impl SkillData {
    pub fn skill(&self, id: u32) -> Option<&SkillTemplate> {
        self.skill_index.get(&id).map(|&i| &self.skills[i])
    }

    pub fn buff(&self, id: u32) -> Option<&BuffTemplate> {
        self.buffs.get(&id)
    }

    pub fn passive_buff(&self, id: u32) -> Option<&PassiveBuffTemplate> {
        self.passive_buffs.get(&id)
    }

    /// `IsDefaultSkill(id)`.
    pub fn is_default_skill(&self, id: u32) -> bool {
        self.default_skills.iter().any(|d| d.skill_id == id)
    }

    /// `IsCommonSkill(id)`.
    pub fn is_common_skill(&self, id: u32) -> bool {
        self.common_skills.contains(&id)
    }

    /// `GetStartAbilitySkills` (C# indexer: missing ability -> `KeyNotFoundException`).
    pub fn start_ability_skills(&self, ability: u8) -> Option<&[u32]> {
        self.start_ability_skills.get(&ability).map(Vec::as_slice)
    }

    pub fn add_skill(&mut self, t: SkillTemplate) {
        self.skill_index.insert(t.id, self.skills.len());
        self.skills.push(t);
    }

    /// Builds the derived lists (end of `SkillManager.Load`).
    pub fn finish(&mut self) {
        self.start_ability_skills.clear();
        self.common_skills.clear();
        for t in self.skills.iter().filter(|t| t.auto_learn) {
            if !t.need_learn
                && t.ability_id == 0
                && !self.default_skills.iter().any(|d| d.skill_id == t.id)
            {
                self.common_skills.push(t.id);
            }
            if !t.need_learn || t.ability_id == 0 || t.ability_level > 1 || !t.show {
                continue;
            }
            self.start_ability_skills
                .entry(t.ability_id)
                .or_default()
                .push(t.id);
        }
    }

    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let mut d = SkillData::default();
        let mut skills = Vec::new();
        for_each_row(conn, "SELECT * FROM skills", [], |r| {
            skills.push(SkillTemplate {
                id: r.u32("id")?,
                show: r.bool_str("show")?,
                ability_id: r.u8("ability_id")?,
                ability_level: r.i32("ability_level")?,
                level_step: r.i32("level_step")?,
                skill_points: r.i32("skill_points")?,
                auto_learn: r.bool_str("auto_learn")?,
                need_learn: r.bool_str("need_learn")?,
            });
            Ok(())
        })?;
        for s in skills {
            if d.skill_index.contains_key(&s.id) {
                return Err(rusqlite::Error::InvalidParameterName(format!(
                    "duplicate skill {} (Dictionary.Add)",
                    s.id
                )));
            }
            d.add_skill(s);
        }
        let mut defaults = Vec::new();
        for_each_row(conn, "SELECT * FROM default_skills", [], |r| {
            defaults.push(DefaultSkill {
                skill_id: r.i32("skill_id")? as u32,
                slot: r.u8("slot_index")?,
                add_to_slot: r.bool_str("add_to_slot")?,
            });
            Ok(())
        })?;
        for ds in defaults {
            if d.skill(ds.skill_id).is_none() {
                return Err(rusqlite::Error::InvalidParameterName(format!(
                    "default skill {} has no template (KeyNotFound)",
                    ds.skill_id
                )));
            }
            if d.default_skills.iter().any(|x| x.skill_id == ds.skill_id) {
                return Err(rusqlite::Error::InvalidParameterName(format!(
                    "duplicate default skill {}",
                    ds.skill_id
                )));
            }
            d.default_skills.push(ds);
        }
        for_each_row(conn, "SELECT * FROM passive_buffs", [], |r| {
            let t = PassiveBuffTemplate {
                id: r.u32("id")?,
                ability_id: r.u8("ability_id")?,
                level: r.u8("level")?,
                buff_id: r.u32("buff_id")?,
                req_points: r.i32("req_points")?,
                active: r.bool_str("active")?,
            };
            d.passive_buffs.insert(t.id, t);
            Ok(())
        })?;
        for_each_row(conn, "SELECT * FROM buffs", [], |r| {
            let t = BuffTemplate {
                id: r.u32("id")?,
                kind: BuffKind::from(r.i32("kind_id")?),
                duration: r.i32("duration")?,
                level_duration: r.i32("level_duration")?,
                tick: r.f64("tick")?,
                stack_rule: r.u32("stack_rule_id")?,
                max_stack: r.i32("max_stack")?,
                init_min_charge: r.i32("init_min_charge")?,
                save_rule_id: if r.has("save_rule_id") {
                    r.u32("save_rule_id")?
                } else {
                    0
                },
                real_time: if r.has("real_time") {
                    r.bool_str("real_time")?
                } else {
                    false
                },
            };
            d.buffs.insert(t.id, t);
            Ok(())
        })?;
        d.finish();
        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_skill_level_keeps_the_csharp_byte_and_stored_level_saturates() {
        // NPC / runtime skills: the C# `(byte)` result, unchanged (255 for -1).
        assert_eq!(skill_level(10, 3, 10 - 6), 255);
        assert_eq!(skill_level(1, 3, 55), 19);
        assert_eq!(skill_level(5, 0, 1), 1);
        // Stored character skills (the retired ability_level 99 rows) saturate to a savable level.
        assert_eq!(stored_skill_level(99, 3, 55), 1);
        assert_eq!(stored_skill_level(10, 3, 10 - 6), 1);
        assert_eq!(stored_skill_level(1, 3, 55), 19);
        assert_eq!(stored_skill_level(1, 1, 1000), 127);
        assert_eq!(stored_skill_level(5, 0, 1), 1);
    }
}
