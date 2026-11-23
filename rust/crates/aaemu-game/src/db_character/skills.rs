use crate::db::{dt, int};
use crate::models::buff::{Buff, SkillCaster};
use crate::models::character::Character;
use crate::server::GameServer;
use sqlx::mysql::MySqlPool;

/// Whether a stored `skills` row of type Skill could have been learned: a `need_learn` skill of
/// an active ability, or a server-granted default skill.
pub(super) fn stored_skill_allowed(server: &GameServer, c: &Character, skill_id: u32) -> bool {
    let data = &server.game_data.skills;
    let Some(t) = data.skill(skill_id) else {
        return true; // unknown ids keep their existing handling in `learn_skill`
    };
    data.is_default_skill(skill_id)
        || crate::systems::skills::progress::is_learnable_class_skill(
            t,
            [c.ability1, c.ability2, c.ability3],
        )
}

/// `CharacterSkills.AddSkill(uint skillId)` without the packet (no connection at load time).
pub fn learn_skill(server: &GameServer, c: &mut Character, skill_id: u32) {
    let data = &server.game_data.skills;
    let Some(t) = data.skill(skill_id) else {
        tracing::error!("AddSkill: unknown skill {skill_id}");
        return;
    };
    if t.ability_id > 0
        && t.ability_id != c.ability1
        && t.ability_id != c.ability2
        && t.ability_id != c.ability3
    {
        return;
    }
    let mut points = server
        .game_data
        .experience
        .skill_points_for_level(c.unit.level);
    points -= c.skills.used_skill_points(data, 0);
    if t.skill_points > points {
        return;
    }
    // A stored skill whose ability level the character never reached (the ability_level 99
    // rows, e.g. a row a pre-fix server could write) is not re-learned; the C# recomputed a
    // wrapped byte here and failed every save (1264, see PORTING.md).
    let ab = ab_level(server, c, t.ability_id);
    if t.ability_level > ab {
        tracing::warn!(
            "Character {} ({}): dropped stored skill {skill_id} on load, it needs ability level \
             {} (ability {}) and the character has {ab}",
            c.unit.name,
            c.id,
            t.ability_level,
            t.ability_id
        );
        return;
    }
    if c.skills.skills.iter().any(|s| s.id == skill_id) {
        return; // C# re-sends SCSkillLearnedPacket (no connection here)
    }
    c.skills.add_skill_template(data, skill_id, ab);
}

/// `GetAbLevel(type)`: `General` = `Level`, otherwise the level for the ability's exp.
pub fn ab_level(server: &GameServer, c: &Character, ability_id: u8) -> i32 {
    if ability_id == 0 {
        return i32::from(c.unit.level);
    }
    let exp = c
        .abilities
        .abilities
        .iter()
        .find(|a| a.id == ability_id)
        .map_or(0, |a| a.exp);
    i32::from(
        server
            .game_data
            .experience
            .level_from_exp_any(exp, false)
            .map_or(0, |(l, _)| l),
    )
}

/// `PassiveBuff.Apply(owner)`.
pub fn apply_passive_buff(server: &GameServer, c: &mut Character, passive_id: u32, now_ms: i64) {
    let Some(pt) = server.game_data.skills.passive_buff(passive_id) else {
        tracing::error!("PassiveBuff {passive_id}: no template");
        return;
    };
    let Some(t) = server.game_data.skills.buff(pt.buff_id) else {
        tracing::error!("PassiveBuff {passive_id}: no buff template {}", pt.buff_id);
        return;
    };
    let mut b = Buff::new(
        t.clone(),
        SkillCaster::default(),
        Some((c.id, c.unit.level)),
        now_ms,
    );
    b.passive = true;
    c.unit.buffs.add_buff(b, 0, 0, now_ms);
}

/// `Buffs.AddBuff(uint buffId, BaseUnit caster)` with the character as caster.
pub fn add_buff_by_id(server: &GameServer, c: &mut Character, buff_id: u32, now_ms: i64) {
    let Some(t) = server.game_data.skills.buff(buff_id) else {
        tracing::error!("AddBuff: no buff template {buff_id}");
        return;
    };
    let caster = SkillCaster {
        kind: 0,
        obj_id: c.obj_id,
    };
    let b = Buff::new(t.clone(), caster, Some((c.id, c.unit.level)), now_ms);
    c.unit.buffs.add_buff(b, 0, 0, now_ms);
}

/// A `character_active_buffs` row.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedBuffRow {
    pub buff_id: u32,
    pub ab_level: u32,
    pub time_left: i32,
    pub charge: i32,
    pub real_time: bool,
    /// `saved_at` (unix ms).
    pub saved_at_ms: i64,
}

/// The query half of `Buffs.LoadActiveBuffs` (errors are logged, as in C#).
pub async fn fetch_active_buffs(server: &GameServer, character_id: u32) -> Vec<SavedBuffRow> {
    let rows = match sqlx::query(
        "SELECT CAST(buff_id AS SIGNED) AS buff_id, CAST(ab_level AS SIGNED) AS ab_level, \
         CAST(time_left AS SIGNED) AS time_left, CAST(charge AS SIGNED) AS charge, \
         CAST(real_time AS SIGNED) AS real_time, saved_at FROM `character_active_buffs` WHERE `character_id` = ?",
    )
    .bind(character_id)
    .fetch_all(&server.db)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to load active buffs for character {character_id}: {e}");
            return Vec::new();
        }
    };
    rows.iter()
        .filter_map(|r| {
            Some(SavedBuffRow {
                buff_id: int(r, "buff_id").ok()? as u32,
                ab_level: int(r, "ab_level").ok()? as u32,
                time_left: int(r, "time_left").ok()? as i32,
                charge: int(r, "charge").ok()? as i32,
                real_time: int(r, "real_time").ok()? != 0,
                saved_at_ms: dt(r, "saved_at").map_or(0, |d| d.and_utc().timestamp_millis()),
            })
        })
        .collect()
}

/// The apply half of `Buffs.LoadActiveBuffs`.
pub fn apply_active_buffs(
    server: &GameServer,
    c: &mut Character,
    rows: &[SavedBuffRow],
    now_ms: i64,
) {
    let mut restored = 0;
    for row in rows {
        let Some(t) = server.game_data.skills.buff(row.buff_id) else {
            tracing::warn!(
                "LoadActiveBuffs: BuffTemplate {} not found, skipping",
                row.buff_id
            );
            continue;
        };
        let remaining = if row.real_time {
            let r = row.time_left - (now_ms - row.saved_at_ms) as i32;
            if r <= 0 {
                tracing::debug!(
                    "LoadActiveBuffs: RealTime buff {} expired offline",
                    row.buff_id
                );
                continue;
            }
            r
        } else {
            row.time_left
        };
        if remaining <= 0 {
            continue;
        }
        if c.unit.buffs.check_buff(row.buff_id) {
            tracing::debug!(
                "LoadActiveBuffs: Buff {} already active, skipping",
                row.buff_id
            );
            continue;
        }
        let caster = SkillCaster {
            kind: 0,
            obj_id: c.obj_id,
        };
        let mut b = Buff::new(t.clone(), caster, Some((c.id, c.unit.level)), now_ms);
        b.ab_level = row.ab_level;
        b.duration = remaining;
        b.charge = if row.charge > 0 {
            row.charge
        } else {
            t.init_min_charge
        };
        c.unit.buffs.add_buff(b, 0, remaining, now_ms);
        restored += 1;
    }
    if restored > 0 {
        tracing::info!("Restored {restored} active buff(s) for character {}", c.id);
    }
}

/// A `character_cooldowns` row: one running skill or shared tag cooldown of a character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedCooldownRow {
    /// `0` = skill id (`skills.id`), `1` = shared tag (`skills.cooldown_tag_id`).
    pub kind: u8,
    pub id: u32,
    /// Unix milliseconds UTC when the cooldown expires.
    pub expires_at_ms: i64,
}

/// `character_cooldowns.cooldown_type` of a per-skill cooldown.
pub const COOLDOWN_TYPE_SKILL: u8 = 0;
/// `character_cooldowns.cooldown_type` of a shared cooldown tag.
pub const COOLDOWN_TYPE_TAG: u8 = 1;

/// The rows `save_character` writes for the character's running cooldowns. Expired entries are
/// dropped instead of persisted, so a relog neither restores a finished cooldown nor keeps a
/// dead row forever. The client keeps at most 150 entries per list, but a character can only
/// start cooldowns of skills it cast, so the stores stay small without a cap here.
pub fn cooldown_rows(c: &Character, now_ms: i64) -> Vec<SavedCooldownRow> {
    let mut rows = Vec::new();
    for (&id, &end) in &c.unit.cooldowns.cooldowns {
        if end > now_ms {
            rows.push(SavedCooldownRow {
                kind: COOLDOWN_TYPE_SKILL,
                id,
                expires_at_ms: end,
            });
        }
    }
    for (&id, &end) in &c.unit.cooldowns.tag_cooldowns {
        if end > now_ms {
            rows.push(SavedCooldownRow {
                kind: COOLDOWN_TYPE_TAG,
                id,
                expires_at_ms: end,
            });
        }
    }
    rows.sort_by_key(|row| (row.kind, row.id));
    rows
}

/// Restore [`cooldown_rows`] into a character (`Character.Load`). Expired rows and rows of an
/// unknown kind are skipped; an existing longer expiry is kept.
pub fn apply_cooldown_rows(c: &mut Character, rows: &[SavedCooldownRow], now_ms: i64) {
    let mut restored = 0;
    for row in rows {
        if row.expires_at_ms <= now_ms {
            continue;
        }
        let store = match row.kind {
            COOLDOWN_TYPE_SKILL => &mut c.unit.cooldowns.cooldowns,
            COOLDOWN_TYPE_TAG => &mut c.unit.cooldowns.tag_cooldowns,
            _ => {
                tracing::warn!(
                    "LoadCharacterCooldowns: unknown cooldown type {} for character {}, skipping",
                    row.kind,
                    c.id
                );
                continue;
            }
        };
        store
            .entry(row.id)
            .and_modify(|end| *end = (*end).max(row.expires_at_ms))
            .or_insert(row.expires_at_ms);
        restored += 1;
    }
    if restored > 0 {
        tracing::info!(
            "Restored {restored} active cooldown(s) for character {}",
            c.id
        );
    }
}

/// The query half of the cooldown load (errors are logged and leave the character without
/// restored cooldowns, like the active-buff load).
pub async fn fetch_character_cooldowns(
    pool: &MySqlPool,
    character_id: u32,
) -> Vec<SavedCooldownRow> {
    let rows = match sqlx::query(
        "SELECT CAST(cooldown_type AS SIGNED) AS cooldown_type, \
         CAST(cooldown_id AS SIGNED) AS cooldown_id, expires_at_ms \
         FROM `character_cooldowns` WHERE `character_id` = ?",
    )
    .bind(character_id)
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to load cooldowns for character {character_id}: {e}");
            return Vec::new();
        }
    };
    rows.iter()
        .filter_map(|r| {
            Some(SavedCooldownRow {
                kind: int(r, "cooldown_type").ok()? as u8,
                id: int(r, "cooldown_id").ok()? as u32,
                expires_at_ms: int(r, "expires_at_ms").ok()? as i64,
            })
        })
        .collect()
}
