//! Character persistence: `GameConnection.LoadAccount`, `Character.Load(connection, id, account)`,
//! `Character.Load()` (the select-time sub loads), `Character.Save` /
//! `SaveDirectlyToDatabase`, the deletion queries of `CharacterManager`, and the friend /
//! blocked info lookups.
//!
//! Integer columns are cast to SIGNED / UNSIGNED because sqlx checks MySQL integer widths
//! strictly, while the C# reader converts.

use chrono::{NaiveDateTime, Utc};
use sqlx::mysql::{MySqlPool, MySqlRow};
use sqlx::Row;

use aaemu_commons::PacketStream;

use crate::db::{dt, from_unix, get_account_details, int, is_set, unix};
use crate::error::{GameError, Result};
use crate::models::buff::{Buff, SkillCaster};
use crate::models::char_data::*;
use crate::models::character::{Character, Inventory, Transform, DEFAULT_INSTANCE_ID};
use crate::models::unit_model::UnitCustomModelParams;
use crate::server::GameServer;

mod save;
mod skills;
pub use save::{persisted_faction, save_character, save_directly_to_database};
pub(crate) use save::{save_edited_character_locked, save_new_character_locked};
use skills::stored_skill_allowed;
pub use skills::{
    ab_level, add_buff_by_id, apply_active_buffs, apply_cooldown_rows, apply_passive_buff,
    cooldown_rows, fetch_active_buffs, fetch_character_cooldowns, learn_skill, SavedBuffRow,
    SavedCooldownRow, COOLDOWN_TYPE_SKILL, COOLDOWN_TYPE_TAG,
};

/// `CrimeManager.PirateCrimePointThreshold`.
pub const PIRATE_CRIME_POINT_THRESHOLD: i32 = 3000;
/// `CrimeManager.WantedCrimePointThreshold`.
pub const WANTED_CRIME_POINT_THRESHOLD: i16 = 50;
/// `BuffConstants.Wanted`.
pub const BUFF_WANTED: u32 = 3710;
/// `BuffConstants.Contemptuous`.
pub const BUFF_CONTEMPTUOUS: u32 = 4832;
/// `FactionsEnum.Pirate`.
pub const FACTION_PIRATE: u32 = 161;

const CHARACTER_COLUMNS: &str = "CAST(id AS SIGNED) AS id, CAST(account_id AS SIGNED) AS account_id, name, \
     CAST(access_level AS SIGNED) AS access_level, CAST(race AS SIGNED) AS race, \
     CAST(gender AS SIGNED) AS gender, unit_model_params, CAST(level AS SIGNED) AS level, \
     CAST(experience AS SIGNED) AS experience, CAST(recoverable_exp AS SIGNED) AS recoverable_exp, CAST(hp AS SIGNED) AS hp, \
     CAST(mp AS SIGNED) AS mp, CAST(consumed_lp AS SIGNED) AS consumed_lp, CAST(ability1 AS SIGNED) AS ability1, \
     CAST(ability2 AS SIGNED) AS ability2, CAST(ability3 AS SIGNED) AS ability3, CAST(world_id AS SIGNED) AS world_id, \
     CAST(zone_id AS SIGNED) AS zone_id, x, y, z, yaw, pitch, roll, CAST(faction_id AS SIGNED) AS faction_id, \
     faction_name, CAST(expedition_id AS SIGNED) AS expedition_id, CAST(family AS SIGNED) AS family, \
     CAST(dead_count AS SIGNED) AS dead_count, dead_time, CAST(rez_wait_duration AS SIGNED) AS rez_wait_duration, \
     rez_time, CAST(rez_penalty_duration AS SIGNED) AS rez_penalty_duration, leave_time, \
     chat_restricted_until, \
     CAST(money AS SIGNED) AS money, CAST(money2 AS SIGNED) AS money2, CAST(honor_point AS SIGNED) AS honor_point, \
     CAST(vocation_point AS SIGNED) AS vocation_point, \
     CAST(crime_point AS SIGNED) AS crime_point, CAST(crime_record AS SIGNED) AS crime_record, \
     CAST(jury_point AS SIGNED) AS jury_point, CAST(hostile_faction_kills AS SIGNED) AS hostile_faction_kills, \
     CAST(pvp_honor AS SIGNED) AS pvp_honor, CAST(died_in_pvp AS SIGNED) AS died_in_pvp, \
     CAST(died_in_pvp_war_zone AS SIGNED) AS died_in_pvp_war_zone, transfer_request_time, \
     delete_request_time, delete_time, CAST(auto_use_aapoint AS SIGNED) AS auto_use_aapoint, \
     CAST(prev_point AS SIGNED) AS prev_point, CAST(point AS SIGNED) AS point, CAST(gift AS SIGNED) AS gift, \
     CAST(num_inv_slot AS SIGNED) AS num_inv_slot, CAST(num_bank_slot AS SIGNED) AS num_bank_slot, \
     CAST(expanded_expert AS SIGNED) AS expanded_expert, created_at, updated_at, \
     CAST(return_district AS SIGNED) AS return_district, CAST(online_time AS SIGNED) AS online_time, \
     CAST(arrest_count AS SIGNED) AS arrest_count, CAST(accept_guilty_count AS SIGNED) AS accept_guilty_count, \
     CAST(accept_trial_count AS SIGNED) AS accept_trial_count, CAST(not_guilty_count AS SIGNED) AS not_guilty_count, \
     CAST(guilty_count AS SIGNED) AS guilty_count, CAST(evidence_reported_count AS SIGNED) AS evidence_reported_count, \
     CAST(bot_reported_count AS SIGNED) AS bot_reported_count, CAST(reported_as_bot_count AS SIGNED) AS reported_as_bot_count, \
     CAST(offline_guilty_time AS SIGNED) AS offline_guilty_time, CAST(offline_guilty_region AS SIGNED) AS offline_guilty_region, \
     slots";

fn opt_int(r: &MySqlRow, col: &str) -> Result<i64> {
    Ok(r.try_get::<Option<i64>, _>(col)?.unwrap_or(0))
}

/// The faction consequences of a crime-point change, decided under the character's data lock by
/// [`Character::check_wanted_threshold`].
///
/// The caller must run
/// [`systems::social::faction::apply_crime_faction_change`](crate::systems::social::faction::apply_crime_faction_change)
/// **after** releasing the character's data lock: both changes broadcast and touch houses /
/// guild / party.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CrimeFactionChange {
    /// Infamy crossed 3000 while the saved faction is not Pirate: run `SetFaction(Pirate)`.
    pub became_pirate: bool,
    /// The saved faction is Pirate and infamy reached 0: return to the race/gender template
    /// faction (the home faction) and credit the "Repentant" achievement (kind 49).
    pub pardoned: bool,
}

impl CrimeFactionChange {
    pub fn merge(self, other: Self) -> Self {
        Self {
            became_pirate: self.became_pirate || other.became_pirate,
            pardoned: self.pardoned || other.pardoned,
        }
    }

    pub fn is_empty(self) -> bool {
        !self.became_pirate && !self.pardoned
    }
}

impl Character {
    /// `CheckWantedThreshold` (the buff part) and the pirate / pardon decision. The faction field
    /// itself is deliberately left unchanged here: the caller runs
    /// [`systems::social::faction::apply_crime_faction_change`](crate::systems::social::faction::apply_crime_faction_change)
    /// after releasing the character's data lock, because the change broadcasts and touches
    /// houses / guild / party.
    ///
    /// The decision uses the saved faction ([`persisted_faction`]), not the live one: a duel /
    /// battlefield corps shows a temporary Red / Blue / corps faction, and testing that would
    /// re-fire `SetFaction(Pirate)` for a character who is already a pirate (ending the
    /// temporary faction and kicking the party). It also means a pirate whose infamy reaches 0
    /// while showing a temporary faction is pardoned (the saved faction is still Pirate).
    ///
    /// Pardon rule (owner-approved 2026-11-06, research `pirate-return.md`): a character whose
    /// **saved** faction is Pirate and whose infamy reaches exactly 0 returns to the home
    /// faction. Below 3000 does not release a pirate; only 0 does.
    #[must_use = "run faction::apply_crime_faction_change after releasing the character's data lock"]
    pub fn check_wanted_threshold(
        &mut self,
        server: &GameServer,
        now_ms: i64,
    ) -> CrimeFactionChange {
        let add = |c: &mut Character, id: u32| {
            if c.unit.buffs.check_buff(id) {
                return;
            }
            match server.game_data.skills.buff(id) {
                Some(t) => {
                    let caster = SkillCaster {
                        kind: 0,
                        obj_id: c.obj_id,
                    };
                    let b = Buff::new(t.clone(), caster, Some((c.id, c.unit.level)), now_ms);
                    c.unit.buffs.add_buff(b, 0, 0, now_ms);
                }
                None => tracing::error!("CheckWantedThreshold: no buff template {id}"),
            }
        };
        let mut change = CrimeFactionChange::default();
        if self.infamy_point >= PIRATE_CRIME_POINT_THRESHOLD {
            add(self, BUFF_WANTED);
            add(self, BUFF_CONTEMPTUOUS);
            if persisted_faction(self) != FACTION_PIRATE {
                tracing::warn!(
                    "Character {} ({}) has {} infamy: SetFaction(Pirate)",
                    self.id,
                    self.unit.name,
                    self.infamy_point
                );
                change.became_pirate = true;
            }
        } else {
            if self.crime_point >= WANTED_CRIME_POINT_THRESHOLD {
                add(self, BUFF_WANTED);
            } else {
                self.unit.buffs.remove_buff(BUFF_WANTED);
            }
            self.unit.buffs.remove_buff(BUFF_CONTEMPTUOUS);
            if self.infamy_point <= 0 && persisted_faction(self) == FACTION_PIRATE {
                tracing::info!(
                    "Character {} ({}) reached 0 infamy: set_faction to the home faction",
                    self.id,
                    self.unit.name
                );
                change.pardoned = true;
            }
        }
        change
    }
}

/// `Character.Load(connection, characterId, accountId)`. Returns the character and its raw
/// `delete_time`.
pub async fn load_character(
    server: &GameServer,
    character_id: u32,
    account_id: u32,
) -> Result<Option<(Character, Option<NaiveDateTime>)>> {
    let pool = &server.db;
    let account = get_account_details(pool, &server.config, account_id).await;
    let row = sqlx::query(&format!(
        "SELECT {CHARACTER_COLUMNS} FROM characters WHERE `id` = ? AND `account_id` = ? and `deleted`=0"
    ))
    .bind(character_id)
    .bind(account_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };

    let blob: Vec<u8> = row.try_get("unit_model_params")?;
    let model_params: UnitCustomModelParams = PacketStream::from_bytes(blob).read()?;
    let id = int(&row, "id")? as u32;
    let mut c = Character::new(id, account_id, model_params);
    c.unit.name = row.try_get("name")?;
    c.access_level = int(&row, "access_level")? as i32;
    c.race = int(&row, "race")? as u8;
    c.gender = int(&row, "gender")? as u8;
    c.unit.level = int(&row, "level")? as u8;
    c.experience = int(&row, "experience")? as i32;
    c.recoverable_exp = int(&row, "recoverable_exp")? as i32;
    c.unit.hp = int(&row, "hp")? as i32;
    c.unit.mp = int(&row, "mp")? as i32;
    c.saved_hp = c.unit.hp;
    c.saved_mp = c.unit.mp;
    c.labor_power = account.labor;
    c.labor_power_modified = unix(account.last_updated);
    c.consumed_labor_power = int(&row, "consumed_lp")? as i32;
    c.ability1 = int(&row, "ability1")? as u8;
    c.ability2 = int(&row, "ability2")? as u8;
    c.ability3 = int(&row, "ability3")? as u8;
    c.server_id = int(&row, "world_id")? as u32;
    // new Transform(character, null, zone_id, DefaultInstanceId, x, y, z, yaw, pitch, roll):
    // the constructor's (roll, pitch, yaw) parameters receive the yaw / pitch / roll columns.
    c.unit.transform = Transform {
        instance_id: DEFAULT_INSTANCE_ID,
        world_id: server.game_data.worlds.default_world_template_id(),
        zone_id: int(&row, "zone_id")? as u32,
        x: row.try_get("x")?,
        y: row.try_get("y")?,
        z: row.try_get("z")?,
        rot_x: row.try_get("yaw")?,
        rot_y: row.try_get("pitch")?,
        rot_z: row.try_get("roll")?,
        parent_obj_id: 0,
    };
    // Later phase: FactionManager.GetFaction returns null for unknown ids (Write would throw).
    c.unit.faction_id = int(&row, "faction_id")? as u32;
    c.faction_name = row.try_get("faction_name")?;
    let expedition_id = int(&row, "expedition_id")? as u32;
    c.unit.expedition_id = if server.expeditions.exists(expedition_id) {
        expedition_id
    } else {
        0
    };
    c.family = int(&row, "family")? as u32;
    c.dead_count = int(&row, "dead_count")? as i16;
    c.dead_time = unix(dt(&row, "dead_time"));
    c.rez_wait_duration = int(&row, "rez_wait_duration")? as i32;
    c.rez_time = unix(dt(&row, "rez_time"));
    c.rez_penalty_duration = int(&row, "rez_penalty_duration")? as i32;
    c.leave_time = unix(dt(&row, "leave_time"));
    c.chat_restricted_until = unix(dt(&row, "chat_restricted_until"));
    c.money = int(&row, "money")?;
    c.money2 = int(&row, "money2")?;
    c.honor_point = int(&row, "honor_point")? as i32;
    c.vocation_point = int(&row, "vocation_point")? as i32;
    let now_ms = Utc::now().timestamp_millis();
    // CrimePoint / InfamyPoint setters: clamp to >= 0 and CheckWantedThreshold (only on change).
    // The load only restores the wanted buffs; the `SetFaction(Pirate)` the threshold asks for
    // needs the live world and runs in `world::session::select_loaded` (`Character.Load` is also
    // used for the lobby list / deletion checks, where there is no player to notify).
    let crime = (int(&row, "crime_point")? as i16).max(0);
    if crime != 0 {
        c.crime_point = crime;
        let _ = c.check_wanted_threshold(server, now_ms);
    }
    let infamy = (int(&row, "crime_record")? as i32).max(0);
    if infamy != 0 {
        c.infamy_point = infamy;
        let _ = c.check_wanted_threshold(server, now_ms);
    }
    c.jury_point = int(&row, "jury_point")? as i32;
    c.hostile_faction_kills = int(&row, "hostile_faction_kills")? as u32;
    c.honor_gained_in_combat = int(&row, "pvp_honor")? as u32;
    c.died_in_pvp = int(&row, "died_in_pvp")? != 0;
    c.died_in_pvp_war_zone = int(&row, "died_in_pvp_war_zone")? != 0;
    c.transfer_request_time = unix(dt(&row, "transfer_request_time"));
    let delete_time = dt(&row, "delete_time");
    c.delete_request_time = unix(dt(&row, "delete_request_time"));
    c.delete_time = unix(delete_time);
    c.auto_use_aa_point = int(&row, "auto_use_aapoint")? != 0;
    c.prev_point = int(&row, "prev_point")? as i32;
    c.point = int(&row, "point")? as i32;
    c.gift = int(&row, "gift")? as i32;
    c.num_inventory_slots = int(&row, "num_inv_slot")? as u8;
    c.num_bank_slots = int(&row, "num_bank_slot")? as i16;
    c.expanded_expert = int(&row, "expanded_expert")? as u8;
    c.created = unix(dt(&row, "created_at"));
    c.updated = unix(dt(&row, "updated_at"));
    c.return_district_id = int(&row, "return_district")? as u32;
    c.online_time = int(&row, "online_time")? as u32;
    c.arrest_count = opt_int(&row, "arrest_count")? as i32;
    c.accept_guilty_count = opt_int(&row, "accept_guilty_count")? as i32;
    c.accept_trial_count = opt_int(&row, "accept_trial_count")? as i32;
    c.not_guilty_count = opt_int(&row, "not_guilty_count")? as i32;
    c.guilty_count = opt_int(&row, "guilty_count")? as i32;
    c.evidence_reported_count = opt_int(&row, "evidence_reported_count")? as i32;
    c.bot_reported_count = opt_int(&row, "bot_reported_count")? as i32;
    c.reported_as_bot_count = opt_int(&row, "reported_as_bot_count")? as i32;
    c.offline_guilty_time = opt_int(&row, "offline_guilty_time")? as i32;
    c.offline_guilty_region = opt_int(&row, "offline_guilty_region")? as i32;

    c.inventory = Inventory::new(
        id,
        c.num_inventory_slots,
        c.num_bank_slots,
        &server.items,
        &server.ids.container,
    );
    let slots: Vec<u8> = row.try_get("slots")?;
    read_action_slots(&mut c.slots, &slots);

    c.bm_point = i64::from(
        get_account_details(pool, &server.config, account_id)
            .await
            .loyalty,
    );

    c.clamp_hp_mp(&server.game_data.formulas);
    // ValidateAndFixExpAndLevel.
    let (level, experience) = server
        .game_data
        .experience
        .validate_and_fix(c.unit.level, c.experience)
        .map_err(|e| GameError::Protocol(format!("Character.Load({id}): {e}")))?;
    c.unit.level = level;
    c.experience = experience;
    // PostUpdateCurrentHp(character, 0, Hp, Unknown): no HP triggers on characters.

    let rows = sqlx::query("SELECT `key`, `value` FROM `options` WHERE `owner` = ?")
        .bind(character_id)
        .fetch_all(pool)
        .await?;
    for r in rows {
        let key: String = r.try_get("key")?;
        let value: String = r.try_get("value")?;
        let key = key
            .trim()
            .parse::<u16>()
            .map_err(|e| GameError::Protocol(format!("options key {key:?}: {e}")))?;
        c.options.insert(key, value);
    }
    Ok(Some((c, delete_time)))
}

/// `GameConnection.LoadAccount` (characters; houses are a later phase). Characters whose
/// deletion time has passed are finalised (`CheckForDeletedCharactersDeletion`) and left out;
/// the packets it sends to the account's connection are returned.
pub async fn load_account_characters(
    server: &GameServer,
    account_id: u32,
) -> Result<(
    Vec<Character>,
    Vec<Box<dyn crate::net::packet::ServerPacket>>,
)> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM characters WHERE `account_id` = ? and `deleted`=0",
    )
    .bind(account_id)
    .fetch_all(&server.db)
    .await?;

    let mut out = Vec::with_capacity(ids.len());
    let mut packets = Vec::new();
    for id in ids {
        let Some((character, _)) = load_character(server, id as u32, account_id).await? else {
            continue; // TODO (C#) ...
        };
        if !check_for_deleted_characters_deletion(server, &character, Some(&mut packets)).await? {
            out.push(character);
        }
    }
    Ok((out, packets))
}

/// `CharacterManager.CheckForDeletedCharactersDeletion`. `packets` receives what C# sends to the
/// game connection (when there is one).
pub async fn check_for_deleted_characters_deletion(
    server: &GameServer,
    character: &Character,
    packets: Option<&mut Vec<Box<dyn crate::net::packet::ServerPacket>>>,
) -> Result<bool> {
    let now = Utc::now().timestamp();
    if character.delete_time > 0 && character.delete_time <= now {
        tracing::info!(
            "CheckForDeletedCharactersDeletion - Deleting Account:{} Id:{} Name:{}",
            character.account_id,
            character.id,
            character.unit.name
        );
        let mut deleted_name = character.unit.name.clone();
        if server.config.account.delete_release_name {
            deleted_name = format!("!{}", character.unit.name);
            server.names.remove_character_id(character.id);
            server
                .names
                .add_character(character.id, &deleted_name, character.account_id);
        }
        let res = sqlx::query(
            "UPDATE `characters` SET `deleted`='1', `delete_time`=?, `name`=? WHERE `id`=? and `account_id`=?",
        )
        .bind(crate::db::min_value())
        .bind(&deleted_name)
        .bind(character.id)
        .bind(character.account_id)
        .execute(&server.db)
        .await?
        .rows_affected();
        if res > 0 {
            // DeleteCharacterAssets(character, false): houses, then guild and family (social);
            // mail returns are a later phase.
            crate::systems::housing::delete_character_assets(server, character.id);
            crate::systems::social::session::delete_character_assets(server, character).await;
            if let Some(p) = packets {
                p.push(Box::new(crate::packets::world::SCCharacterDeletedPacket {
                    character_id: character.id,
                    name: character.unit.name.clone(),
                }));
                p.push(Box::new(
                    crate::packets::world::SCDeleteCharacterResponsePacket {
                        character_id: character.id,
                        status: 1,
                        delete_request_time: character.delete_request_time,
                        delete_time: character.delete_time,
                    },
                ));
            }
        }
        return Ok(res > 0);
    } else if character.delete_request_time > 0 {
        tracing::warn!(
            "CheckForDeletedCharactersDeletion - Delete request for Account:{} Id:{} Name:{}, but character is no longer marked for deletion (possibly cancelled delete)",
            character.account_id,
            character.id,
            character.unit.name
        );
    }
    Ok(false)
}

/// `CharacterManager.CheckForDeletedCharacters`: finalises due deletions and returns the next
/// pending deletion time (unix seconds) to schedule `CharacterDeleteTask` for.
pub async fn check_for_deleted_characters(server: &GameServer) -> Result<Option<i64>> {
    let rows = sqlx::query(
        "SELECT CAST(`id` AS SIGNED) AS id, `name`, CAST(`account_id` AS SIGNED) AS account_id, `delete_time` FROM characters WHERE `deleted`=0",
    )
    .fetch_all(&server.db)
    .await?;
    let now = Utc::now().naive_utc();
    let mut next: Option<NaiveDateTime> = None;
    let mut due = Vec::new();
    for r in rows {
        let t = dt(&r, "delete_time");
        if is_set(t) {
            let t = t.expect("set");
            if t <= now {
                due.push((int(&r, "id")? as u32, int(&r, "account_id")? as u32));
            } else if next.is_none_or(|n| t < n) {
                next = Some(t);
            }
        }
    }
    for (char_id, account_id) in due {
        match load_character(server, char_id, account_id).await? {
            Some((c, _)) => {
                let conn = server.connections.get_by_account(account_id);
                let mut packets = Vec::new();
                if check_for_deleted_characters_deletion(server, &c, Some(&mut packets)).await? {
                    if let Some(conn) = conn {
                        for p in &packets {
                            conn.send_dyn(p.as_ref());
                        }
                    }
                    tracing::info!("CheckForDeletedCharacters - Delete charId:{char_id}");
                } else {
                    tracing::error!(
                        "CheckForDeletedCharacters - Failed to delete character for deletion charId:{char_id}"
                    );
                }
            }
            None => tracing::error!(
                "CheckForDeletedCharacters - Failed to load character for deletion charId:{char_id}"
            ),
        }
    }
    Ok(next.map(|n| n.and_utc().timestamp()))
}

/// `Character.Load()`: the per-character tables read when the character is selected.
pub async fn load_character_details(server: &GameServer, c: &mut Character) -> Result<()> {
    let pool = &server.db;
    let data = &server.game_data;
    let id = c.id;
    let now_ms = Utc::now().timestamp_millis();
    if let Some(t) = data.characters.template(c.race, c.gender) {
        c.unit.model_id = t.model_id;
    } else {
        return Err(GameError::Protocol(format!(
            "no character template for race {} gender {}",
            c.race, c.gender
        )));
    }
    c.slots = vec![ActionSlot::default(); MAX_ACTION_SLOTS];

    // Abilities
    c.abilities = CharacterAbilities::default();
    let actives = [c.ability1, c.ability2, c.ability3];
    for r in sqlx::query("SELECT CAST(id AS SIGNED) AS id, CAST(exp AS SIGNED) AS exp FROM abilities WHERE `owner` = ?")
        .bind(id)
        .fetch_all(pool)
        .await?
    {
        c.abilities
            .load_row(int(&r, "id")? as u8, int(&r, "exp")? as i32, actives);
    }

    // Actability
    c.actability = CharacterActability::default();
    for r in sqlx::query("SELECT CAST(id AS SIGNED) AS id, CAST(point AS SIGNED) AS point, CAST(step AS SIGNED) AS step FROM actabilities WHERE `owner` = ?")
        .bind(id)
        .fetch_all(pool)
        .await?
    {
        let aid = int(&r, "id")? as u32;
        if data.characters.actability(aid).is_none() {
            // `_actabilities[id]` throws KeyNotFoundException.
            return Err(GameError::Protocol(format!("unknown actability {aid}")));
        }
        c.actability.actabilities.push(Actability {
            id: aid,
            point: int(&r, "point")? as i32,
            step: int(&r, "step")? as u8,
        });
    }

    // Skills (each stored skill is re-learned through AddSkill(skillId), which re-checks the
    // ability tree and the skill points and recomputes the level).
    c.skills = CharacterSkills::default();
    let mut dropped_skills: Vec<u32> = Vec::new();
    let mut dropped_buffs: Vec<u32> = Vec::new();
    for r in sqlx::query("SELECT CAST(id AS SIGNED) AS id, CAST(level AS SIGNED) AS level, CAST(`type` AS CHAR) AS type FROM skills WHERE `owner` = ?")
        .bind(id)
        .fetch_all(pool)
        .await?
    {
        let ty: String = r.try_get("type")?;
        let sid = int(&r, "id")? as u32;
        if ty.eq_ignore_ascii_case("skill") || ty == "1" {
            if !stored_skill_allowed(server, c, sid) {
                dropped_skills.push(sid);
                continue;
            }
            learn_skill(server, c, sid);
        } else if ty.eq_ignore_ascii_case("buff") || ty == "2" {
            if data.skills.passive_buff(sid).is_some_and(|t| t.ability_id == 0) {
                dropped_buffs.push(sid);
                continue;
            }
            c.skills.passive_buffs.push(sid);
            apply_passive_buff(server, c, sid, now_ms);
        } else {
            return Err(GameError::Protocol(format!("unknown skill type {ty:?}")));
        }
    }

    // Skills and passive buffs no ability tree offers (ability-less NPC / GM / internal rows a
    // forged CSLearnSkill / CSLearnBuff could store before the learn check existed) are dropped
    // here and deleted; `save` only upserts, so the rows would otherwise come back every load.
    for (kind, ids) in [("Skill", &dropped_skills), ("Buff", &dropped_buffs)] {
        if ids.is_empty() {
            continue;
        }
        tracing::warn!(
            "Character {} ({}): dropped stored {} ids {:?} on load, they are not learnable class              {}s of the active abilities",
            c.unit.name,
            id,
            kind.to_ascii_lowercase(),
            ids,
            kind.to_ascii_lowercase()
        );
        let list = ids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        if let Err(e) = sqlx::query(&format!(
            "DELETE FROM skills WHERE owner = ? AND `type` = '{kind}' AND id IN({list})"
        ))
        .bind(id)
        .execute(pool)
        .await
        {
            tracing::warn!("Character {id}: could not delete the dropped {kind} rows: {e}");
        }
    }

    // Appellations
    c.appellations = CharacterAppellations::default();
    for r in sqlx::query("SELECT CAST(id AS SIGNED) AS id, CAST(active AS SIGNED) AS active FROM appellations WHERE `owner` = ?")
        .bind(id)
        .fetch_all(pool)
        .await?
    {
        let aid = int(&r, "id")? as u32;
        c.appellations.appellations.push(aid);
        if int(&r, "active")? != 0 {
            c.appellations.active = aid;
            // addBuff(titleId)
            let buff_id = data.characters.appellations.get(&aid).copied().unwrap_or(0);
            if buff_id != 0 {
                add_buff_by_id(server, c, buff_id, now_ms);
            }
            tracing::info!("title: {aid} giving buff {buff_id}");
        }
    }

    // Portals
    c.portals = CharacterPortals::default();
    for r in sqlx::query("SELECT CAST(id AS SIGNED) AS id, name, (x + 0E0) AS x, (y + 0E0) AS y, (z + 0E0) AS z, \
         CAST(zone_id AS SIGNED) AS zone_id, (z_rot + 0E0) AS z_rot, CAST(sub_zone_id AS SIGNED) AS sub_zone_id, CAST(owner AS SIGNED) AS owner \
         FROM portal_book_coords WHERE `owner` = ?")
        .bind(id)
        .fetch_all(pool)
        .await?
    {
        let f = |c: &str| -> Result<f32> { Ok(r.try_get::<Option<f64>, _>(c)?.unwrap_or(0.0) as f32) };
        c.portals.private_portals.push(crate::data::world_data::Portal {
            id: int(&r, "id")? as u32,
            name: r.try_get("name")?,
            x: f("x")?,
            y: f("y")?,
            z: f("z")?,
            zone_id: opt_int(&r, "zone_id")? as u32,
            z_rot: f("z_rot")?,
            sub_zone_id: opt_int(&r, "sub_zone_id")? as u32,
            owner: int(&r, "owner")? as u32,
            ..Default::default()
        });
    }
    for r in sqlx::query("SELECT CAST(id AS SIGNED) AS id, CAST(subzone AS SIGNED) AS subzone, CAST(owner AS SIGNED) AS owner FROM portal_visited_district WHERE `owner` = ?")
        .bind(id)
        .fetch_all(pool)
        .await?
    {
        let v = VisitedDistrict {
            id: int(&r, "id")? as u32,
            sub_zone: int(&r, "subzone")? as u32,
            owner: int(&r, "owner")? as u32,
        };
        if c.portals.visited_districts.iter().any(|x| x.sub_zone == v.sub_zone) {
            return Err(GameError::Protocol(format!("duplicate visited district {}", v.sub_zone)));
        }
        c.portals.visited_districts.push(v);
    }
    c.portals.populate_district_portals(&data.portals);

    // Friends / blocked
    c.friends.clear();
    for r in sqlx::query("SELECT CAST(id AS SIGNED) AS id, CAST(friend_id AS SIGNED) AS friend_id, CAST(owner AS SIGNED) AS owner FROM friends WHERE `owner` = ?")
        .bind(id)
        .fetch_all(pool)
        .await?
    {
        let f = FriendEntry {
            id: int(&r, "id")? as u32,
            friend_id: int(&r, "friend_id")? as u32,
            owner: int(&r, "owner")? as u32,
        };
        if c.friends.iter().any(|x| x.friend_id == f.friend_id) {
            return Err(GameError::Protocol(format!("duplicate friend {}", f.friend_id)));
        }
        c.friends.push(f);
    }
    c.blocked.clear();
    for r in sqlx::query("SELECT CAST(owner AS SIGNED) AS owner, CAST(blocked_id AS SIGNED) AS blocked_id FROM blocked WHERE `owner` = ?")
        .bind(id)
        .fetch_all(pool)
        .await?
    {
        c.blocked
            .push((int(&r, "owner")? as u32, int(&r, "blocked_id")? as u32));
    }

    // Quests (rows only: templates / evaluation / daily resets are a later phase)
    c.quests = CharacterQuests::default();
    for r in
        sqlx::query("SELECT CAST(id AS SIGNED) AS id, data FROM completed_quests WHERE `owner` = ?")
            .bind(id)
            .fetch_all(pool)
            .await?
    {
        c.quests.completed.push(CompletedQuest {
            id: int(&r, "id")? as u32,
            body: r.try_get("data")?,
        });
    }
    for r in sqlx::query("SELECT CAST(id AS SIGNED) AS id, CAST(template_id AS SIGNED) AS template_id, data, CAST(status AS SIGNED) AS status FROM quests WHERE `owner` = ?")
        .bind(id)
        .fetch_all(pool)
        .await?
    {
        let mut q = ActiveQuest {
            id: int(&r, "id")? as u32,
            template_id: int(&r, "template_id")? as u32,
            status: int(&r, "status")? as u8,
            objectives: [0; 5],
            step: 0,
            acceptor_type: 0,
            component_id: 0,
            acceptor_id: 0,
            time_ms: ActiveQuest::DATETIME_MIN_MS,
        };
        let data: Vec<u8> = r.try_get("data")?;
        if let Err(e) = q.read_data(&data) {
            tracing::error!("Quest {} data of {}: {e}", q.template_id, c.unit.name);
        }
        c.quests.active.push(q);
    }

    // Achievements (records + progress rows). The gameplay producers report into them through
    // `systems::report_achievement`, and `CSNotifyInGamePacket` pushes the entry snapshot.
    crate::systems::social::achievement_persistence::load_character_achievements(pool, c).await?;

    // Crafts learned from recipe items / TrainCraftEffect (`character_crafts`).
    c.learned_crafts =
        crate::systems::economy::craft_learning::load_character_crafts(pool, id).await?;

    // LoadActionSlots(connection)
    if let Ok(Some(r)) =
        sqlx::query("SELECT slots FROM `characters` WHERE `id` = ? AND `account_id` = ?")
            .bind(id)
            .bind(c.account_id)
            .fetch_optional(pool)
            .await
    {
        let blob: Vec<u8> = r.try_get("slots")?;
        read_action_slots(&mut c.slots, &blob);
    }

    // Running skill / shared-tag cooldowns. The C# `UnitCooldowns` was memory-only; this port
    // persists the absolute expiries in `character_cooldowns` so a relog keeps the countdown and
    // still refuses a cast until it runs out.
    let cooldown_rows = fetch_character_cooldowns(pool, id).await;
    apply_cooldown_rows(c, &cooldown_rows, Utc::now().timestamp_millis());

    // Mails: the counters start at 0 here; `world::session::select_loaded` fills them with the
    // real unread counts (the C# `GetCurrentMailList` ran before the character was in the world
    // and always left them 0).
    c.unread_mail = CountUnreadMail::default();
    Ok(())
}

fn join(ids: &[u32]) -> String {
    ids.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
}

/// `CharacterManager.IsCharacterPendingDeletion(name)`.
pub async fn is_character_pending_deletion(pool: &MySqlPool, name: &str) -> Result<bool> {
    let row = sqlx::query(
        "SELECT CAST(deleted AS SIGNED) AS deleted, delete_request_time FROM characters WHERE `name` = ?",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        Some(r) => int(&r, "deleted")? != 0 || is_set(dt(&r, "delete_request_time")),
        None => false,
    })
}

/// The `UPDATE characters SET delete_request_time, delete_time` of `SetDeleteCharacter` /
/// `SetRestoreCharacter`. Returns the affected rows.
pub async fn set_delete_times(pool: &MySqlPool, id: u32, request: i64, delete: i64) -> Result<u64> {
    Ok(sqlx::query(
        "UPDATE characters SET `delete_request_time` = ?, `delete_time` = ? WHERE `id` = ?",
    )
    .bind(from_unix(request))
    .bind(from_unix(delete))
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected())
}

/// `FriendMananger.GetFriendInfo(ids)` for the offline part (online friends are formatted from
/// the world by the caller).
pub async fn offline_friend_infos(pool: &MySqlPool, ids: &[u32]) -> Result<Vec<FriendInfo>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(&format!(
        "SELECT CAST(id AS SIGNED) AS id, name, CAST(zone_id AS SIGNED) AS zone_id, x, y, z, CAST(race AS SIGNED) AS race, \
         CAST(level AS SIGNED) AS level, leave_time, CAST(hp AS SIGNED) AS hp, CAST(ability1 AS SIGNED) AS ability1, \
         CAST(ability2 AS SIGNED) AS ability2, CAST(ability3 AS SIGNED) AS ability3 FROM characters WHERE id IN({})",
        join(ids)
    ))
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for r in rows {
        out.push(FriendInfo {
            character_id: int(&r, "id")? as u32,
            name: r.try_get("name")?,
            race: int(&r, "race")? as u8,
            level: int(&r, "level")? as u8,
            health: int(&r, "hp")? as i32,
            abilities: [
                int(&r, "ability1")? as u8,
                int(&r, "ability2")? as u8,
                int(&r, "ability3")? as u8,
            ],
            x: r.try_get("x")?,
            y: r.try_get("y")?,
            z: r.try_get("z")?,
            zone_id: int(&r, "zone_id")? as u32,
            in_party: false,
            is_online: false,
            last_world_leave_time: unix(dt(&r, "leave_time")),
        });
    }
    Ok(out)
}

/// `CharacterBlocked.GetBlockedInfo` (offline part).
pub async fn offline_blocked_infos(pool: &MySqlPool, ids: &[u32]) -> Result<Vec<BlockedInfo>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(&format!(
        "SELECT CAST(id AS SIGNED) AS id, name FROM characters WHERE id IN({})",
        join(ids)
    ))
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for r in rows {
        out.push(BlockedInfo {
            character_id: int(&r, "id")? as u32,
            name: r.try_get("name")?,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
