//! Player nations: a top-level user faction (`mother_id == 0`) created from an expedition by
//! the 1.2 "declare independence" skill (18540, special effect 71).
//!
//! Evidence and unknowns are recorded in `PORTING.md` (NATION-1..): the client creates the
//! new faction from `SCFactionIndependence` (`FUN_392fb2d0`), so the server keeps the nation
//! record, the founding expedition's `mother` and the members' `faction_id` in sync; the
//! nation name carrier is unknown and the founding expedition's name is used (NATION-1).
//!
//! Nation ids come from the expedition id manager: the 1.2 client keeps a single faction map
//! and `FUN_398a5280` refuses a duplicate id (`managers/id.rs`, `EXPEDITION_ID` also extracts
//! `nations.id`). A new nation copies faction 166's `system_faction_relations` rows (NATION-2).

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, PoisonError, RwLock};

use chrono::Utc;
use sqlx::{MySql, MySqlPool, Row, Transaction};

use crate::data::items::{backpack_type, TemplateKind};
use crate::data::world_data::{FactionRelation, SystemFaction};
use crate::error::Result;
use crate::models::container::equip_slot;
use crate::packets::world::{SCFactionListPacket, SCFactionRelationListPacket};
use crate::server::GameServer;
use crate::systems::economy::error_message;
use crate::systems::skills::effects::EffectArgs;
use crate::systems::skills::types::{CastAction, SkillCaster};
use crate::world::PlayerRef;

use super::faction;
use super::packets::faction::{
    SCFactionImmigrateInvitePacket, SCFactionImmigrateInviteResultPacket,
    SCFactionImmigrateToOriginResultPacket, SCFactionIndependencePacket,
    SCFactionKickToOriginResultPacket, SCFactionOwnerChangedPacket,
};
use super::util::{now_ms, send_error};

/// Skill "국가 독립" (declare independence), 20 s cast, CASTLE_CLAIM backpack (equip slot 26).
pub const DECLARE_INDEPENDENCE_SKILL: u32 = 18_540;
/// `system_factions` 166 "default faction setup for independence": the relation template of a
/// new nation (46 rows in the 1.2 compact database).
pub const INDEPENDENCE_TEMPLATE_FACTION: u32 = 166;
/// `item_backpacks.backpack_type_id` of the CASTLE_CLAIM backpacks (skill 18540's
/// `link_backpack_type_id`).
pub const CASTLE_CLAIM_BACKPACK_TYPE: u32 = backpack_type::CASTLE_CLAIM;

/// `content_configs` 49 `NationImmigrateLevelMin` (the client reads it in `FUN_3906cb80`;
/// the 1.2 database value is 50).
pub const IMMIGRATE_MIN_LEVEL: u8 = 50;

/// Pending nation invite lifetime. The 1.2 client has no visible invite timer (UNKNOWN);
/// this mirrors the 60 s of the port's expedition / team invitations (NATION-11).
pub const INVITE_TIMEOUT_MS: i64 = 60_000;

/// Upper bound of the pending-invite list so it cannot grow without bound (NATION-11).
pub const MAX_PENDING_INVITES: usize = 1024;

/// A pending immigration invitation: the king invited `invitee_character_id` to
/// `nation_id`. Memory only (PORTING.md, NATION-11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInvite {
    pub nation_id: u32,
    pub inviter_character_id: u32,
    pub invitee_character_id: u32,
    pub expires_at_ms: i64,
}

impl PendingInvite {
    fn is_valid_at(&self, now_ms: i64) -> bool {
        self.expires_at_ms >= now_ms
    }
}

/// One relation row of a nation. `other_faction_id` is always the other side (the stored rows
/// are normalised); `expire_at` / `next_state` are reserved for the stage-3 relation expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NationRelation {
    pub other_faction_id: u32,
    pub state: u8,
    pub expire_at: i64,
    pub next_state: u8,
}

impl NationRelation {
    /// The `FactionRelation` row the login / independence relation lists carry.
    pub fn wire(&self, nation_id: u32) -> FactionRelation {
        FactionRelation {
            id: nation_id,
            id2: self.other_faction_id,
            state: self.state,
            exp_time: self.expire_at,
        }
    }
}

/// A player nation (`nations` row plus its relation overlay).
#[derive(Debug, Clone, PartialEq)]
pub struct Nation {
    pub id: u32,
    /// The founding expedition; its `mother` becomes this nation.
    pub expedition_id: u32,
    /// The king: the founding expedition's owner.
    pub owner_character_id: u32,
    pub owner_name: String,
    pub name: String,
    pub created_at: i64,
    pub allow_change_name: u8,
    pub relations: Vec<NationRelation>,
    /// Expedition members that were offline when the nation was created. Their
    /// `characters.faction_id` is rewritten with the founding expedition's `mother`
    /// (runtime only; a loaded nation does not need it again).
    pub offline_members: Vec<u32>,
}

impl Nation {
    /// The `SystemFaction` record the client receives (`SCFactionList`). The client's
    /// independence handler clones the founding expedition and sets `mother = 0`, the name,
    /// the created time and `diplomacy_target = 1` (`FUN_392fb2d0`); the remaining fields
    /// follow the cloned expedition record.
    pub fn system_faction(&self) -> SystemFaction {
        SystemFaction {
            id: self.id,
            name: self.name.clone(),
            owner_id: self.owner_character_id,
            owner_name: self.owner_name.clone(),
            mother_id: 0,
            unit_owner_type: 0,
            political_system: 1,
            diplomacy_target: true,
            aggro_link: false,
            guard_help: false,
            allow_change_name: self.allow_change_name,
            created: self.created_at,
        }
    }

    /// The relation rows of this nation in wire form (`(nation, other, state)`).
    pub fn wire_relations(&self) -> Vec<FactionRelation> {
        self.relations.iter().map(|r| r.wire(self.id)).collect()
    }
}

/// `NationManager`: the runtime nations, their relation overlay and the pending immigration
/// invitations.
#[derive(Debug, Default)]
pub struct NationManager {
    nations: RwLock<Vec<Nation>>,
    invites: Mutex<Vec<PendingInvite>>,
}

impl NationManager {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<Nation>> {
        self.nations.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Vec<Nation>> {
        self.nations.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Loads `nations` and `nation_relations`. A relation row for an unknown nation is skipped
    /// (a bad row must not fail the catalog).
    pub async fn load(&self, pool: &MySqlPool) -> Result<()> {
        let mut nations = Vec::new();
        for r in sqlx::query(
            "SELECT CAST(n.id AS SIGNED) AS id,              CAST(CASE WHEN e.id IS NULL OR e.mother <> n.id THEN 0 ELSE n.expedition_id END                   AS SIGNED) AS expedition_id,              CAST(n.owner_character_id AS SIGNED) AS owner_character_id, n.owner_name, n.name,              n.created_at, CAST(n.allow_change_name AS SIGNED) AS allow_change_name              FROM nations n LEFT JOIN expeditions e ON e.id = n.expedition_id ORDER BY n.id",
        )
        .fetch_all(pool)
        .await?
        {
            nations.push(Nation {
                id: r.try_get::<i64, _>("id")? as u32,
                expedition_id: r.try_get::<i64, _>("expedition_id")? as u32,
                owner_character_id: r.try_get::<i64, _>("owner_character_id")? as u32,
                owner_name: r.try_get("owner_name")?,
                name: r.try_get("name")?,
                created_at: r.try_get("created_at")?,
                allow_change_name: r.try_get::<i64, _>("allow_change_name")? as u8,
                relations: Vec::new(),
                offline_members: Vec::new(),
            });
        }
        let mut index = HashMap::new();
        for (i, nation) in nations.iter().enumerate() {
            index.insert(nation.id, i);
        }
        for r in sqlx::query(
            "SELECT CAST(nation_id AS SIGNED) AS nation_id, \
             CAST(other_faction_id AS SIGNED) AS other_faction_id, \
             CAST(state AS SIGNED) AS state, expire_at, CAST(next_state AS SIGNED) AS next_state \
             FROM nation_relations ORDER BY nation_id, other_faction_id",
        )
        .fetch_all(pool)
        .await?
        {
            let nation_id = r.try_get::<i64, _>("nation_id")? as u32;
            let Some(&i) = index.get(&nation_id) else {
                tracing::warn!("nation_relations row for unknown nation {nation_id} skipped");
                continue;
            };
            nations[i].relations.push(NationRelation {
                other_faction_id: r.try_get::<i64, _>("other_faction_id")? as u32,
                state: r.try_get::<i64, _>("state")? as u8,
                expire_at: r.try_get("expire_at")?,
                next_state: r.try_get::<i64, _>("next_state")? as u8,
            });
        }
        let count = nations.len();
        *self.write() = nations;
        tracing::info!("Loaded {count} nations");
        Ok(())
    }

    pub fn all(&self) -> Vec<Nation> {
        self.read().clone()
    }

    pub fn get(&self, id: u32) -> Option<Nation> {
        self.read().iter().find(|n| n.id == id).cloned()
    }

    pub fn is_nation(&self, id: u32) -> bool {
        let nations = self.read();
        if nations.is_empty() {
            return false;
        }
        nations.iter().any(|n| n.id == id)
    }

    /// `(id, mother_id)` of a nation without cloning it (the relation hot path).
    pub fn faction_pair(&self, id: u32) -> Option<(u32, u32)> {
        let nations = self.read();
        if nations.is_empty() {
            return None;
        }
        nations.iter().find(|n| n.id == id).map(|n| (n.id, 0))
    }

    /// The nation founded by `expedition_id` (`0` = none).
    pub fn for_expedition(&self, expedition_id: u32) -> Option<Nation> {
        if expedition_id == 0 {
            return None;
        }
        self.read()
            .iter()
            .find(|n| n.expedition_id == expedition_id)
            .cloned()
    }

    /// The nation `character_id` is the king of (the 1.2 client's "is nation owner" scan:
    /// `owner_id == characterId` on a top-level user faction).
    pub fn owned_by(&self, character_id: u32) -> Option<Nation> {
        if character_id == 0 {
            return None;
        }
        self.read()
            .iter()
            .find(|n| n.owner_character_id == character_id)
            .cloned()
    }

    /// Stores a pending invite, replacing an earlier one for the same invitee (the 1.2 client
    /// shows one reply dialog). Expired entries are dropped and the list is capped
    /// (NATION-11).
    pub fn add_invite(&self, pending: PendingInvite, now_ms: i64) {
        let mut invites = self.invites.lock().unwrap_or_else(PoisonError::into_inner);
        invites.retain(|i| {
            i.is_valid_at(now_ms) && i.invitee_character_id != pending.invitee_character_id
        });
        if invites.len() >= MAX_PENDING_INVITES {
            // Drop the oldest entry; the list is chronological (insertion at the end).
            invites.remove(0);
        }
        invites.push(pending);
    }

    /// Consumes the pending invite of `invitee_character_id`. `None` when there is none, it has
    /// expired, or it names another nation (a forged / replayed reply, NATION-11).
    pub fn take_invite(
        &self,
        nation_id: u32,
        invitee_character_id: u32,
        now_ms: i64,
    ) -> Option<PendingInvite> {
        let mut invites = self.invites.lock().unwrap_or_else(PoisonError::into_inner);
        let index = invites
            .iter()
            .position(|i| i.invitee_character_id == invitee_character_id)?;
        let pending = invites.remove(index);
        (pending.nation_id == nation_id && pending.is_valid_at(now_ms)).then_some(pending)
    }

    pub fn pending_invite_count(&self) -> usize {
        self.invites
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether a valid invite to `invitee_character_id` is already pending. A repeated invite
    /// is dropped instead of spamming the reply dialog (NATION-11).
    pub fn has_pending_invite(&self, invitee_character_id: u32, now_ms: i64) -> bool {
        self.invites
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|i| i.invitee_character_id == invitee_character_id && i.is_valid_at(now_ms))
    }

    /// The founding expedition handed its leadership to `character_id`: the nation's king
    /// (`owner_character_id`, the faction record's `owner_id`) follows the guild owner. Called
    /// from `expedition::change_owner`, both for the client transfer and for the forced one when
    /// a pirate owner is removed (owner decision 2026-11-03, PIRATE-1). The nation, its
    /// relations and the founding guild stay unchanged; the caller broadcasts the returned
    /// faction and writes it with the expedition save.
    pub fn on_expedition_owner_changed(
        &self,
        expedition_id: u32,
        character_id: u32,
        name: &str,
    ) -> Vec<Nation> {
        if expedition_id == 0 || character_id == 0 {
            return Vec::new();
        }
        let mut changed = Vec::new();
        for nation in self
            .write()
            .iter_mut()
            .filter(|n| n.expedition_id == expedition_id)
        {
            nation.owner_character_id = character_id;
            nation.owner_name = name.to_string();
            changed.push(nation.clone());
        }
        changed
    }

    /// The founding expedition disbanded: the nation stays as a kingless nation (NATION-5),
    /// but its stale expedition id must not capture a guild that later reuses the id, and a
    /// pirate must not keep the king's invite / kick rights through `owned_by`.
    pub fn on_expedition_disbanded(&self, expedition_id: u32) -> Vec<Nation> {
        if expedition_id == 0 {
            return Vec::new();
        }
        let mut changed = Vec::new();
        for nation in self
            .write()
            .iter_mut()
            .filter(|n| n.expedition_id == expedition_id)
        {
            tracing::info!(
                "Nation {} lost its founding expedition {expedition_id}; it stays kingless",
                nation.id
            );
            nation.expedition_id = 0;
            nation.owner_character_id = 0;
            nation.owner_name.clear();
            changed.push(nation.clone());
        }
        changed
    }

    /// The narrow runtime update the 1.2 client applies to a faction owner: `OnFactionOwnerChanged`
    /// (`x2game.dll.c` handler `FUN_391d2570` -> `FUN_392f82a0`: faction id, owner character id,
    /// owner name; the login list is `SCFactionList` instead). Broadcasting this avoids resending
    /// every nation / relation row for one king change.
    pub fn broadcast_owner_changed(server: &GameServer, nation: &Nation) {
        server
            .world
            .broadcast_to_server(&SCFactionOwnerChangedPacket {
                id: nation.id,
                id2: nation.owner_character_id,
                new_owner_name: nation.owner_name.clone(),
            });
    }

    /// Persists only the nation's identity / king row, for the expedition transaction that
    /// changes the founding guild's owner. Relation rows and offline member rewrites stay with
    /// the periodic [`NationManager::save`].
    pub(crate) async fn write_nation_owner(
        tx: &mut Transaction<'_, MySql>,
        nation: &Nation,
    ) -> Result<()> {
        sqlx::query(
            "REPLACE INTO nations(`id`,`expedition_id`,`owner_character_id`,`owner_name`,`name`,\
             `created_at`,`allow_change_name`) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(nation.id)
        .bind(nation.expedition_id)
        .bind(nation.owner_character_id)
        .bind(&nation.owner_name)
        .bind(&nation.name)
        .bind(nation.created_at)
        .bind(nation.allow_change_name)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// The `SystemFaction` of a nation id (`None` for every other id).
    pub fn faction(&self, id: u32) -> Option<SystemFaction> {
        self.get(id).map(|n| n.system_faction())
    }

    /// The stored relation between `a` and `b` when either side is a nation (the rows are
    /// mirrored for lookups, the client list stays directional).
    pub fn relation(&self, a: u32, b: u32) -> Option<u8> {
        let nations = self.read();
        if nations.is_empty() {
            return None;
        }
        for nation in nations.iter() {
            if nation.id == a {
                return nation
                    .relations
                    .iter()
                    .find(|r| r.other_faction_id == b)
                    .map(|r| r.state);
            }
            if nation.id == b {
                return nation
                    .relations
                    .iter()
                    .find(|r| r.other_faction_id == a)
                    .map(|r| r.state);
            }
        }
        None
    }

    /// The relation rows of every nation in wire form, for the login `SCFactionRelationList`.
    pub fn relation_rows(&self) -> Vec<FactionRelation> {
        self.read()
            .iter()
            .flat_map(Nation::wire_relations)
            .collect()
    }

    /// `SCFactionList` chunks (20 records each, as the C# `SendFactions`).
    pub fn faction_chunks(&self) -> Vec<Vec<SystemFaction>> {
        let nations: Vec<SystemFaction> = self.read().iter().map(Nation::system_faction).collect();
        nations.chunks(20).map(<[SystemFaction]>::to_vec).collect()
    }

    /// `SCFactionRelationList` chunks (200 rows each; the packet count is a byte).
    pub fn relation_chunks(&self) -> Vec<Vec<FactionRelation>> {
        let rows = self.relation_rows();
        rows.chunks(200).map(<[FactionRelation]>::to_vec).collect()
    }

    pub fn add(&self, nation: Nation) {
        let mut nations = self.write();
        match nations.iter_mut().find(|n| n.id == nation.id) {
            Some(slot) => *slot = nation,
            None => nations.push(nation),
        }
    }

    /// After the creation write committed, the offline members need no further rewrites (their
    /// `characters.faction_id` is already the nation). A failed creation write keeps the list so
    /// the periodic save retries it.
    pub fn clear_offline_members(&self, id: u32) {
        if let Some(nation) = self.write().iter_mut().find(|n| n.id == id) {
            nation.offline_members.clear();
        }
    }

    /// This subsystem's part of `SaveManager.DoSave`: nations, their relation rows, the
    /// founding expedition's `mother` and the offline members' `faction_id`, in one
    /// transaction.
    pub async fn save(
        &self,
        server: &GameServer,
        tx: &mut Transaction<'_, MySql>,
    ) -> Result<usize> {
        let nations = self.all();
        let online: HashSet<u32> = server
            .world
            .all_characters()
            .iter()
            .map(|p| p.character_id)
            .collect();
        let mut written = 0;
        for nation in &nations {
            written += write_nation(server, tx, nation, &online).await?;
        }
        Ok(written)
    }
}

/// Writes a newly created nation immediately (the GM command awaits it; the independence effect
/// spawns it). The periodic save writes the same rows again.
pub async fn save_creation(server: &GameServer, nation: &Nation) -> Result<()> {
    let online: HashSet<u32> = server
        .world
        .all_characters()
        .iter()
        .map(|p| p.character_id)
        .collect();
    // The periodic save must not touch `expeditions` (a reused guild id would be captured);
    // only the creation transaction writes the founding guild's mother.
    let _save = server.save.transaction_lock().await;
    let mut tx = server.db.begin().await?;
    write_nation(server, &mut tx, nation, &online).await?;
    if nation.expedition_id != 0 {
        sqlx::query("UPDATE expeditions SET mother = ? WHERE id = ?")
            .bind(nation.id)
            .bind(nation.expedition_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    server
        .systems
        .social
        .nations
        .clear_offline_members(nation.id);
    Ok(())
}

/// `REPLACE nations` + the nation's relation rows + `expeditions.mother` + the offline
/// members' `characters.faction_id`. Called inside the caller's transaction so the membership
/// pointers move together.
async fn write_nation(
    server: &GameServer,
    tx: &mut Transaction<'_, MySql>,
    nation: &Nation,
    online: &HashSet<u32>,
) -> Result<usize> {
    sqlx::query(
        "REPLACE INTO nations(`id`,`expedition_id`,`owner_character_id`,`owner_name`,`name`,\
         `created_at`,`allow_change_name`) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(nation.id)
    .bind(nation.expedition_id)
    .bind(nation.owner_character_id)
    .bind(&nation.owner_name)
    .bind(&nation.name)
    .bind(nation.created_at)
    .bind(nation.allow_change_name)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM nation_relations WHERE nation_id = ?")
        .bind(nation.id)
        .execute(&mut **tx)
        .await?;
    for relation in &nation.relations {
        sqlx::query(
            "INSERT INTO nation_relations(`nation_id`,`other_faction_id`,`state`,`expire_at`,\
             `next_state`) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(nation.id)
        .bind(relation.other_faction_id)
        .bind(relation.state)
        .bind(relation.expire_at)
        .bind(relation.next_state)
        .execute(&mut **tx)
        .await?;
    }
    let offline: Vec<u32> = nation
        .offline_members
        .iter()
        .copied()
        .filter(|id| !online.contains(id))
        .collect();
    if !offline.is_empty() {
        let list = offline
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        sqlx::query(&format!(
            "UPDATE characters SET faction_id = ? WHERE id IN ({list})"
        ))
        .bind(nation.id)
        .execute(&mut **tx)
        .await?;
        sqlx::query(&format!(
            "UPDATE housings SET faction_id = ? WHERE owner IN ({list})"
        ))
        .bind(nation.id)
        .execute(&mut **tx)
        .await?;
        // Every `housings` row is also loaded into the housing manager at startup, so an
        // offline owner has live HouseRefs that must move too; otherwise the next house save
        // writes the old faction back (NATION-5 review follow-up).
        for member in &offline {
            crate::systems::housing::update_owned_housing_faction(server, *member, nation.id);
        }
    }
    Ok(1 + nation.relations.len() + offline.len())
}

/// `(id, mother_id)` of a static faction or a runtime nation (`None` = an unknown faction).
pub fn faction_of_id(server: &GameServer, id: u32) -> Option<(u32, u32)> {
    if let Some(f) = server.game_data.factions.get(id) {
        return Some((f.id, f.mother_id));
    }
    server.systems.social.nations.faction_pair(id)
}

/// Whether `id` is a static faction or a runtime nation.
pub fn faction_exists(server: &GameServer, id: u32) -> bool {
    server.game_data.factions.get(id).is_some() || server.systems.social.nations.is_nation(id)
}

/// The relation between `a` and `b`, preferring a nation's runtime rows over the static table.
/// Both loaded forms of the static table are consulted (the social manager's map and the raw
/// `FactionData` rows), because fixtures and loaders fill one or the other.
pub fn relation_between(server: &GameServer, a: u32, b: u32) -> Option<u8> {
    if let Some(state) = server.systems.social.nations.relation(a, b) {
        return Some(state);
    }
    if let Some(state) = server.game_data.systems.social.relations.get(a, b) {
        return Some(state);
    }
    server
        .game_data
        .factions
        .relations
        .iter()
        .find(|r| (r.id == a && r.id2 == b) || (r.id == b && r.id2 == a))
        .map(|r| r.state)
}

/// The relation template of a new nation: the `system_faction_relations` rows that name faction
/// 166, normalised to `(nation, other)` and with the template itself removed.
pub fn template_relations(server: &GameServer) -> Vec<NationRelation> {
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for relation in &server.game_data.factions.relations {
        let other = if relation.id == INDEPENDENCE_TEMPLATE_FACTION {
            relation.id2
        } else if relation.id2 == INDEPENDENCE_TEMPLATE_FACTION {
            relation.id
        } else {
            continue;
        };
        if other == INDEPENDENCE_TEMPLATE_FACTION || !seen.insert(other) {
            continue;
        }
        rows.push(NationRelation {
            other_faction_id: other,
            state: relation.state,
            expire_at: 0,
            next_state: 0,
        });
    }
    rows
}

/// `declare_independence`: validation, the nation record, the founding expedition's mother,
/// the online members' faction change and the independence / relation packets. Returns the
/// created nation. The caller persists it (`save_creation` / the periodic save).
///
/// `require_dominion` is the 1.2 skill prerequisite (the caster's expedition owns a dominion);
/// the GM command may skip it for tests (see `PORTING.md`, NATION-4).
pub fn declare_independence(
    server: &GameServer,
    founder: &PlayerRef,
    expedition_id: u32,
    name: &str,
    require_dominion: bool,
) -> std::result::Result<Nation, i16> {
    let (character_id, current_expedition) = {
        let c = founder.data();
        (c.id, c.unit.expedition_id)
    };
    if expedition_id == 0 || current_expedition != expedition_id {
        return Err(error_message::FACTION_SEPARATE_RIGHTS);
    }
    let Some(expedition) = server
        .expeditions
        .get(expedition_id)
        .filter(|e| !e.is_disbanded)
    else {
        return Err(error_message::FACTION_SEPARATE_RIGHTS);
    };
    if expedition.faction.owner_id != character_id {
        return Err(error_message::FACTION_SEPARATE_RIGHTS);
    }
    if server
        .systems
        .social
        .nations
        .for_expedition(expedition_id)
        .is_some()
    {
        // FACTION_SEPARATE_MOTHER: the expedition already belongs to a nation.
        return Err(error_message::FACTION_SEPARATE_MOTHER);
    }
    if require_dominion
        && !server
            .systems
            .housing
            .dominions
            .all()
            .iter()
            .any(|dominion| dominion.owner_expedition_id == expedition_id)
    {
        // No dedicated 1.2 error id is known for this check (PORTING.md, NATION-4).
        return Err(error_message::FACTION_SEPARATE_RIGHTS);
    }
    if name.is_empty()
        || server
            .game_data
            .factions
            .factions
            .iter()
            .any(|f| f.name == name)
        || server
            .systems
            .social
            .nations
            .all()
            .iter()
            .any(|n| n.name == name)
    {
        return Err(error_message::FACTION_DUPLICATE_NAME);
    }
    let id = loop {
        let id = match server.ids.expedition.get_next_id() {
            Ok(id) => id,
            Err(error) => {
                tracing::error!("{error}");
                return Err(error_message::FACTION_CREATE);
            }
        };
        // The allocator normally knows both tables; the checks keep a test fixture that adds
        // an expedition directly from handing out a duplicate faction id.
        if !server.expeditions.exists(id) && server.systems.social.nations.get(id).is_none() {
            break id;
        }
    };
    let members = expedition.member_ids();
    let online: HashSet<u32> = server
        .world
        .all_characters()
        .iter()
        .map(|p| p.character_id)
        .collect();
    let now = Utc::now();
    let nation = Nation {
        id,
        expedition_id,
        owner_character_id: expedition.faction.owner_id,
        owner_name: expedition.faction.owner_name.clone(),
        name: name.to_string(),
        created_at: now.timestamp(),
        allow_change_name: 0,
        relations: template_relations(server),
        offline_members: members
            .iter()
            .copied()
            .filter(|id| !online.contains(id))
            .collect(),
    };
    server.systems.social.nations.add(nation.clone());
    // The client's handler rewrites the founding expedition's mother; the server must do the
    // same before re-factioning its members or `set_faction` sees a mother mismatch and kicks.
    server
        .expeditions
        .with_mut(expedition_id, |e| e.faction.mother_id = id);
    server
        .world
        .broadcast_to_server(&SCFactionIndependencePacket {
            id: expedition_id,
            id2: id,
            name: nation.name.clone(),
            c_time: now,
        });
    server
        .world
        .broadcast_to_server(&SCFactionRelationListPacket {
            relations: nation.wire_relations(),
        });
    for member_id in members {
        if let Some(member) = server.world.character_by_id(member_id) {
            faction::set_faction_persistent(server, &member, id);
        }
    }
    Ok(nation)
}

/// Special effect 71 `DeclareIndependence`: validates the accepted cast of skill 18540 and the
/// CASTLE_CLAIM backpack, then runs [`declare_independence`] and persists the new nation.
pub fn declare_independence_effect(server: &GameServer, args: &EffectArgs<'_>) {
    let Some(caster) = args.caster.as_ref() else {
        return;
    };
    let Some(player) = caster.as_player() else {
        return;
    };
    let Some(source) = args.source.skill.as_ref() else {
        return;
    };
    let Some(canonical) = server
        .game_data
        .systems
        .skills
        .skill(DECLARE_INDEPENDENCE_SKILL)
    else {
        return;
    };
    let accepted = {
        let state = source.state();
        state.generation != 0 && !state.cancelled
    };
    if !std::sync::Arc::ptr_eq(canonical, &source.template)
        || source.id != DECLARE_INDEPENDENCE_SKILL
        || args.source.buff.is_some()
        || args.source.is_trigger
        || !accepted
        || !matches!(args.caster_caster, SkillCaster::Unit(id) if id == player.obj_id)
        || !matches!(
            args.cast,
            CastAction::Skill { skill_id, tl_id }
                if skill_id == DECLARE_INDEPENDENCE_SKILL && tl_id == source.tl_id()
        )
    {
        return;
    }
    if !has_castle_claim_backpack(server, player) {
        send_error(player, error_message::FACTION_SEPARATE_RIGHTS);
        return;
    }
    let expedition_id = player.data().unit.expedition_id;
    let Some(expedition) = server.expeditions.get(expedition_id) else {
        send_error(player, error_message::FACTION_SEPARATE_RIGHTS);
        return;
    };
    let name = expedition.faction.name.clone();
    match declare_independence(server, player, expedition_id, &name, true) {
        Ok(nation) => {
            if let Some(server) = server.self_arc() {
                tokio::spawn(async move {
                    if let Err(error) = save_creation(&server, &nation).await {
                        tracing::error!("Nation {} save failed: {error}", nation.id);
                    }
                });
            }
        }
        Err(error) => send_error(player, error),
    }
}

// ---- Immigration (stage 2): invite / reply / return to origin / kick ----

/// The race's start faction (`character_templates.faction_id`, the same data the client's
/// `FUN_398a2660(race)` returns).
fn start_faction(server: &GameServer, race: u8, gender: u8) -> Option<u32> {
    server
        .game_data
        .characters
        .template(race, gender)
        .map(|t| t.faction_id)
        .filter(|id| *id != 0)
}

/// The top-level faction of a static faction or a runtime nation (`None` = unknown id).
/// A nation is top-level (`mother_id == 0`), so the client's "top-level faction" comparison
/// (`FUN_398a44d0` / `FUN_398a4620`) resolves a nation id to itself.
pub fn top_level_faction(server: &GameServer, id: u32) -> Option<u32> {
    let mut current = id;
    for _ in 0..8 {
        let (faction, mother) = faction_of_id(server, current)?;
        if mother == 0 {
            return Some(faction);
        }
        current = mother;
    }
    Some(current)
}

/// The client's "I own my own user faction" scan (`FUN_398a2f40`): a top-level user faction
/// whose owner is this character. Only runtime nations qualify in this server.
fn owns_user_faction(server: &GameServer, character_id: u32) -> bool {
    server
        .systems
        .social
        .nations
        .owned_by(character_id)
        .is_some()
}

/// Online citizens of a nation.
fn online_nation_members(server: &GameServer, nation_id: u32) -> Vec<u32> {
    server
        .world
        .all_characters()
        .iter()
        .filter(|p| p.data().unit.faction_id == nation_id)
        .map(|p| p.character_id)
        .collect()
}

/// The client's reply checks in its own order (`FUN_398a44d0`): combat 770, pirate 694,
/// unknown 693, already a member 730, owns a user faction 731, in an expedition 732, level
/// below `content_configs` 49 728.
fn immigration_error(
    server: &GameServer,
    p: &PlayerRef,
    nation_id: u32,
) -> std::result::Result<(), i16> {
    // Every faction check uses the saved faction (RPA-09): a duel / battlefield team faction is
    // temporary and is never what the player really is (PORTING.md, NATION-14).
    let (in_battle, temporary, faction, expedition, level, character_id) = {
        let c = p.data();
        (
            c.unit.is_in_battle,
            c.unit.faction_temporary,
            crate::db_character::persisted_faction(&c),
            c.unit.expedition_id,
            c.unit.level,
            c.id,
        )
    };
    if in_battle {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_AS_COMBAT);
    }
    if faction == crate::db_character::FACTION_PIRATE {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_AS_PIRATE);
    }
    if temporary {
        // No 1.2 code exists for "in a duel / battlefield": the general refusal is used
        // (NATION-14). The client's other checks below still run on the saved faction.
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_AS_GENERAL);
    }
    let Some(my_top) = top_level_faction(server, faction) else {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_AS_GENERAL);
    };
    if server.systems.social.nations.get(nation_id).is_none() {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_AS_GENERAL);
    }
    if my_top == nation_id {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_TO_USER_AS_ALREADY_MEMBER);
    }
    if owns_user_faction(server, character_id) {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_TO_USER_AS_FACTION_OWNER);
    }
    if expedition != 0 {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_TO_USER_AS_EXPEDITION_MEMBER);
    }
    if level < IMMIGRATE_MIN_LEVEL {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_AS_LEVEL_LOW);
    }
    Ok(())
}

/// The client's return-to-origin checks in its own order (`FUN_398a4620`): combat 770,
/// pirate 694, unknown 693, already at the origin 733, owns a user faction 734, in an
/// expedition 735. Returns the race's start faction.
fn origin_error(server: &GameServer, p: &PlayerRef) -> std::result::Result<u32, i16> {
    // The saved faction is the real one (RPA-09); a temporary team faction must not make a
    // pirate look like 160 or a civilian look like a nation (PORTING.md, NATION-14).
    let (in_battle, temporary, faction, expedition, race, gender, character_id) = {
        let c = p.data();
        (
            c.unit.is_in_battle,
            c.unit.faction_temporary,
            crate::db_character::persisted_faction(&c),
            c.unit.expedition_id,
            c.race,
            c.gender,
            c.id,
        )
    };
    if in_battle {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_AS_COMBAT);
    }
    if faction == crate::db_character::FACTION_PIRATE {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_AS_PIRATE);
    }
    if temporary {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_AS_GENERAL);
    }
    let Some(my_top) = top_level_faction(server, faction) else {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_AS_GENERAL);
    };
    let Some(origin) = start_faction(server, race, gender) else {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_AS_GENERAL);
    };
    let Some(origin_top) = top_level_faction(server, origin) else {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_AS_GENERAL);
    };
    if my_top == origin_top {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_TO_ORIG_AS_ALREADY_MEMBER);
    }
    if owns_user_faction(server, character_id) {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_TO_ORIG_AS_FACTION_OWNER);
    }
    if expedition != 0 {
        return Err(error_message::FACTION_CANNOT_IMMIGRATE_TO_ORIG_AS_EXPEDITION_MEMBER);
    }
    Ok(origin)
}

fn send_invite_result(
    server: &GameServer,
    pending: &PendingInvite,
    invitee_name: &str,
    answer: bool,
) {
    let packet = SCFactionImmigrateInviteResultPacket {
        id: pending.nation_id,
        char_name: invitee_name.to_string(),
        answer,
    };
    if let Some(inviter) = server.world.character_by_id(pending.inviter_character_id) {
        inviter.send(packet.clone());
    }
    if let Some(invitee) = server.world.character_by_id(pending.invitee_character_id) {
        invitee.send(packet);
    }
}

fn send_kick_result(
    requester: &PlayerRef,
    target_id: u32,
    target_name: &str,
    nation_id: u32,
    error: i16,
) {
    requester.send(SCFactionKickToOriginResultPacket {
        tgt_name: target_name.to_string(),
        id: target_id,
        id2: nation_id,
        error_message: error,
    });
}

/// `CSFactionImmigrationInvite` (0x015): the nation king invites a candidate. The client's
/// pre-send checks are 697 (not the king) and 736 (oneself); the server re-runs the reply
/// checks on the invitee (`FUN_398a44d0`) and answers the king with the same codes.
pub fn invite_citizen(server: &GameServer, king: &PlayerRef, invitee_name: &str) {
    let Some(nation) = server.systems.social.nations.owned_by(king.character_id) else {
        send_error(
            king,
            error_message::FACTION_CANNOT_INVITE_IMMIGRATION_AS_NO_PERMISSION,
        );
        return;
    };
    let king_name = king.data().unit.name.clone();
    // `character_by_name` is case-insensitive; the client's self check must be too.
    if king_name.to_lowercase() == invitee_name.to_lowercase() {
        send_error(king, error_message::FACTION_CANNOT_IMMIGRATION_AS_ONESELF);
        return;
    }
    let Some(invitee) = server.world.character_by_name(invitee_name) else {
        send_error(king, error_message::FACTION_INVITE_OFFLINE);
        return;
    };
    // A blocked inviter gets no invitation, as in the expedition / team / family invites
    // (capture-pending for the missing 1.2 sender-side reply, case `BLOCK-LIST-1`).
    // A blocked inviter gets no invitation, as in the expedition / team / family invites
    // (capture-pending for the missing 1.2 sender-side reply, case `BLOCK-LIST-1`).
    if super::friend::is_blocked_by(&invitee, king.character_id) {
        return;
    }
    let now = now_ms();
    if server
        .systems
        .social
        .nations
        .has_pending_invite(invitee.character_id, now)
    {
        // The reply dialog is already open; do not send a second copy (NATION-11).
        return;
    }
    if let Err(code) = immigration_error(server, &invitee, nation.id) {
        send_error(king, code);
        return;
    }
    server.systems.social.nations.add_invite(
        PendingInvite {
            nation_id: nation.id,
            inviter_character_id: king.character_id,
            invitee_character_id: invitee.character_id,
            expires_at_ms: now + INVITE_TIMEOUT_MS,
        },
        now,
    );
    // `id2` is UNKNOWN in the 1.2 client (PORTING.md, NATION-11): the invitee character id is
    // used as a nonce the reply must echo.
    invitee.send(SCFactionImmigrateInvitePacket {
        id: nation.id,
        id2: invitee.character_id,
    });
}

/// `CSFactionImmigrationInviteReply` (0x016): the invitee answers a pending invite. A decline
/// needs no eligibility check; an accept re-runs the client's reply checks and, on success,
/// moves the player to the nation through `faction::set_faction` and persists the faction.
pub async fn reply_invite(
    server: &GameServer,
    invitee: &PlayerRef,
    nation_id: u32,
    nonce: u32,
    answer: bool,
) {
    let now = now_ms();
    let Some(pending) =
        server
            .systems
            .social
            .nations
            .take_invite(nation_id, invitee.character_id, now)
    else {
        // No pending invite, an expired one, or one naming another nation.
        send_error(invitee, error_message::FACTION_CANNOT_IMMIGRATE_AS_GENERAL);
        return;
    };
    if nonce != invitee.character_id {
        send_error(invitee, error_message::FACTION_CANNOT_IMMIGRATE_AS_GENERAL);
        return;
    }
    let Some(nation) = server.systems.social.nations.get(nation_id) else {
        send_error(invitee, error_message::FACTION_CANNOT_IMMIGRATE_AS_GENERAL);
        return;
    };
    if nation.owner_character_id != pending.inviter_character_id {
        // The inviter is no longer the king; the old invitation is void.
        send_error(invitee, error_message::FACTION_CANNOT_IMMIGRATE_AS_GENERAL);
        return;
    }
    let invitee_name = invitee.data().unit.name.clone();
    if !answer {
        send_invite_result(server, &pending, &invitee_name, false);
        return;
    }
    if let Err(code) = immigration_error(server, invitee, nation_id) {
        send_error(invitee, code);
        return;
    }
    faction::set_faction(server, invitee, nation_id).await;
    persist_faction(server, invitee.character_id, nation_id).await;
    send_invite_result(server, &pending, &invitee_name, true);
}

/// `CSFactionImmigrateToOrigin` (0x017, empty body): a citizen returns to their race's start
/// faction. The result goes to the leaver and the nation's online members.
pub async fn immigrate_to_origin(server: &GameServer, p: &PlayerRef) {
    let origin = match origin_error(server, p) {
        Ok(origin) => origin,
        Err(code) => {
            send_error(p, code);
            return;
        }
    };
    let (faction, name, character_id) = {
        let c = p.data();
        (c.unit.faction_id, c.unit.name.clone(), c.id)
    };
    let nation_id = server
        .systems
        .social
        .nations
        .get(faction)
        .map(|n| n.id)
        .or_else(|| top_level_faction(server, faction));
    let members = if server.systems.social.nations.is_nation(faction) {
        online_nation_members(server, faction)
    } else {
        vec![character_id]
    };
    faction::set_faction(server, p, origin).await;
    persist_faction(server, character_id, origin).await;
    let packet = SCFactionImmigrateToOriginResultPacket {
        char_name: name,
        id: nation_id.unwrap_or(faction),
    };
    for member in members {
        if let Some(member) = server.world.character_by_id(member) {
            member.send(packet.clone());
        }
    }
}

/// An offline kick target read from `characters`.
struct OfflineTarget {
    character_id: u32,
    name: String,
    faction_id: u32,
    expedition_id: u32,
    race: u8,
    gender: u8,
}

async fn offline_target_by_name(
    server: &GameServer,
    name: &str,
) -> std::result::Result<Option<OfflineTarget>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT CAST(id AS SIGNED) AS id, name, CAST(faction_id AS SIGNED) AS faction_id, \
         CAST(expedition_id AS SIGNED) AS expedition_id, CAST(race AS SIGNED) AS race, \
         CAST(gender AS SIGNED) AS gender FROM characters \
         WHERE name = ? AND deleted = 0 LIMIT 1",
    )
    .bind(name)
    .fetch_optional(&server.db)
    .await?;
    Ok(row.map(|r| OfflineTarget {
        character_id: r.get::<i64, _>("id") as u32,
        name: r.get("name"),
        faction_id: r.get::<i64, _>("faction_id") as u32,
        expedition_id: r.get::<i64, _>("expedition_id") as u32,
        race: r.get::<i64, _>("race") as u8,
        gender: r.get::<i64, _>("gender") as u8,
    }))
}

/// What [`kick_offline`] did when it held the save lock.
pub(super) enum OfflineKick {
    /// The stored faction and houses were moved.
    Done,
    /// The target logged in between the offline lookup and the save lock.
    TargetOnline,
    /// The stored faction is no longer the nation (another writer changed it).
    FactionChanged,
}

/// The offline half of a kick: `characters.faction_id` (guarded by the expected nation) and
/// the owner's stored houses move in one transaction. The owner's houses are loaded in the
/// housing manager too, so their live copies move under the same save lock (a later save would
/// otherwise write the old faction back). A target that logged in in the meantime is reported
/// so the caller runs the online path instead of clobbering the session.
pub(super) async fn kick_offline(
    server: &GameServer,
    character_id: u32,
    nation_id: u32,
    origin: u32,
) -> std::result::Result<OfflineKick, sqlx::Error> {
    let _save = server.save.transaction_lock().await;
    if server.world.character_by_id(character_id).is_some() {
        return Ok(OfflineKick::TargetOnline);
    }
    let mut tx = server.db.begin().await?;
    let updated =
        sqlx::query("UPDATE characters SET faction_id = ? WHERE id = ? AND faction_id = ?")
            .bind(origin)
            .bind(character_id)
            .bind(nation_id)
            .execute(&mut *tx)
            .await?;
    if updated.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(OfflineKick::FactionChanged);
    }
    sqlx::query("UPDATE housings SET faction_id = ? WHERE owner = ?")
        .bind(origin)
        .bind(character_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    // The offline owner keeps its loaded HouseRefs; move them while the save lock is still
    // held, so no concurrent save writes the old faction back (NATION-5 follow-up).
    crate::systems::housing::update_owned_housing_faction(server, character_id, origin);
    Ok(OfflineKick::Done)
}

/// Persists one character's faction immediately, serialised with `SaveManager` so an
/// in-flight save cannot write the old value afterwards. Best effort: the periodic save
/// writes the same column again.
pub async fn persist_faction(server: &GameServer, character_id: u32, faction_id: u32) {
    let _save = server.save.transaction_lock().await;
    if let Err(error) = sqlx::query("UPDATE characters SET faction_id = ? WHERE id = ?")
        .bind(faction_id)
        .bind(character_id)
        .execute(&server.db)
        .await
    {
        tracing::warn!("Persisting the faction of character {character_id} failed: {error}");
    }
}

/// `CSFactionKickToOrigin` (0x018): the king sends a citizen back to their race's start
/// faction. The client's pre-send checks are 64 (not the king) and 65 (oneself); server-only
/// refusals are 704 (unknown target), 726 (expedition member) and 705 (other faction). The
/// result packet carries the error (`FUN_392fe6c0` shows a message box when it is set), so
/// every outcome is answered with it.
pub async fn kick_to_origin(server: &GameServer, king: &PlayerRef, target_name: &str) {
    let Some(nation) = server.systems.social.nations.owned_by(king.character_id) else {
        send_kick_result(king, 0, target_name, 0, error_message::FACTION_KICK_RIGHTS);
        return;
    };
    let king_name = king.data().unit.name.clone();
    if king_name.to_lowercase() == target_name.to_lowercase() {
        send_kick_result(
            king,
            0,
            target_name,
            nation.id,
            error_message::FACTION_KICK_SELF,
        );
        return;
    }
    if let Some(target) = server.world.character_by_name(target_name) {
        kick_online_target(server, king, &target, nation.id).await;
        return;
    }
    let target = match offline_target_by_name(server, target_name).await {
        Ok(Some(target)) => target,
        Ok(None) => {
            send_kick_result(
                king,
                0,
                target_name,
                nation.id,
                error_message::FACTION_KICK_TARGET_NOT_FOUND,
            );
            return;
        }
        Err(error) => {
            tracing::error!("KickToOrigin: looking up {target_name} failed: {error}");
            send_kick_result(
                king,
                0,
                target_name,
                nation.id,
                error_message::FACTION_KICK_TARGET_NOT_FOUND,
            );
            return;
        }
    };
    if target.character_id == king.character_id {
        send_kick_result(
            king,
            target.character_id,
            &target.name,
            nation.id,
            error_message::FACTION_KICK_SELF,
        );
        return;
    }
    if target.expedition_id != 0 {
        send_kick_result(
            king,
            target.character_id,
            &target.name,
            nation.id,
            error_message::FACTION_KICK_EXPEDITION_MEMBER,
        );
        return;
    }
    if target.faction_id != nation.id {
        send_kick_result(
            king,
            target.character_id,
            &target.name,
            nation.id,
            error_message::FACTION_KICK_OTHER_FACTION,
        );
        return;
    }
    let Some(origin) = start_faction(server, target.race, target.gender) else {
        tracing::error!(
            "KickToOrigin: character {} ({}) has no start faction",
            target.character_id,
            target.name
        );
        send_kick_result(
            king,
            target.character_id,
            &target.name,
            nation.id,
            error_message::FACTION_KICK,
        );
        return;
    };
    match kick_offline(server, target.character_id, nation.id, origin).await {
        Ok(OfflineKick::Done) => {
            send_kick_result(king, target.character_id, &target.name, nation.id, 0);
        }
        Ok(OfflineKick::TargetOnline) => {
            // The target logged in after the lookup: run the online path on the live session.
            if let Some(target) = server.world.character_by_id(target.character_id) {
                kick_online_target(server, king, &target, nation.id).await;
            } else {
                send_kick_result(
                    king,
                    target.character_id,
                    &target.name,
                    nation.id,
                    error_message::FACTION_KICK_TARGET_NOT_FOUND,
                );
            }
        }
        Ok(OfflineKick::FactionChanged) => {
            send_kick_result(
                king,
                target.character_id,
                &target.name,
                nation.id,
                error_message::FACTION_KICK_OTHER_FACTION,
            );
        }
        Err(error) => {
            tracing::error!(
                "KickToOrigin: moving offline character {} failed: {error}",
                target.character_id
            );
            send_kick_result(
                king,
                target.character_id,
                &target.name,
                nation.id,
                error_message::FACTION_KICK,
            );
        }
    }
}

/// The online half of a kick: re-reads the live character, applies the same refusals and moves
/// it through `faction::set_faction` (houses, party side effects) and the faction save.
async fn kick_online_target(
    server: &GameServer,
    king: &PlayerRef,
    target: &PlayerRef,
    nation_id: u32,
) {
    let (target_id, name, faction, expedition, race, gender) = {
        let c = target.data();
        (
            c.id,
            c.unit.name.clone(),
            c.unit.faction_id,
            c.unit.expedition_id,
            c.race,
            c.gender,
        )
    };
    if target_id == king.character_id {
        send_kick_result(
            king,
            target_id,
            &name,
            nation_id,
            error_message::FACTION_KICK_SELF,
        );
        return;
    }
    if expedition != 0 {
        send_kick_result(
            king,
            target_id,
            &name,
            nation_id,
            error_message::FACTION_KICK_EXPEDITION_MEMBER,
        );
        return;
    }
    if faction != nation_id {
        send_kick_result(
            king,
            target_id,
            &name,
            nation_id,
            error_message::FACTION_KICK_OTHER_FACTION,
        );
        return;
    }
    let Some(origin) = start_faction(server, race, gender) else {
        tracing::error!("KickToOrigin: character {target_id} ({name}) has no start faction");
        send_kick_result(
            king,
            target_id,
            &name,
            nation_id,
            error_message::FACTION_KICK,
        );
        return;
    };
    faction::set_faction(server, target, origin).await;
    persist_faction(server, target_id, origin).await;
    let result = SCFactionKickToOriginResultPacket {
        tgt_name: name,
        id: target_id,
        id2: nation_id,
        error_message: 0,
    };
    target.send(result.clone());
    king.send(result);
}

/// The skill's CASTLE_CLAIM backpack (type 1 in the equip slot 26). Skill 18540's `unit_reqs`
/// row only checks that slot 26 is occupied (`UnitReq` 43379, kind 8); the backpack type is the
/// authored restriction.
fn has_castle_claim_backpack(server: &GameServer, player: &PlayerRef) -> bool {
    let equipment = player.data().inventory.equipment().clone();
    let item = equipment
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_item_by_slot(equip_slot::BACKPACK)
        .cloned();
    item.and_then(|item| server.game_data.items.template(item.template_id))
        .is_some_and(|template| {
            matches!(template.kind, TemplateKind::Backpack { backpack_type } if backpack_type == CASTLE_CLAIM_BACKPACK_TYPE)
        })
}

/// The `SCFactionList` records of all nations (`None` when there is no nation).
pub fn login_factions(server: &GameServer) -> Vec<Vec<SystemFaction>> {
    server.systems.social.nations.faction_chunks()
}

/// The `SCFactionRelationList` rows of all nations (`uiRequest = false`).
pub fn login_relations(server: &GameServer) -> Vec<Vec<FactionRelation>> {
    server.systems.social.nations.relation_chunks()
}

/// Sends the nation records and relation rows of the login burst after the static lists.
pub fn send_login_lists(server: &GameServer, conn: &mut crate::net::connection::GameConnection) {
    for chunk in login_factions(server) {
        conn.send(&SCFactionListPacket { factions: chunk });
    }
    for chunk in login_relations(server) {
        conn.send(&SCFactionRelationListPacket { relations: chunk });
    }
}
