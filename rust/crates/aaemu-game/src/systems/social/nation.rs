//! Player nations: a top-level user faction (`mother_id == 0`) created from an expedition by
//! the 1.2 "declare independence" skill (18540, special effect 71).
//!
//! Evidence and unknowns are recorded in `PORTING.md` (NATION-1..17): the client creates the
//! new faction from `SCFactionIndependence` (`FUN_392fb2d0`), so the server keeps the nation
//! record, the founding expedition's `mother` and the members' `faction_id` in sync; the
//! nation name carrier is unknown and the founding expedition's name is used (NATION-1).
//! Stage 3 adds `CSFactionDeclareHostile` 0x019, the type-25 friendly treaty mail paid through
//! the client's charge-money button, and the periodic relation expiry on the shared ticker.
//!
//! Nation ids come from the expedition id manager: the 1.2 client keeps a single faction map
//! and `FUN_398a5280` refuses a duplicate id (`managers/id.rs`, `EXPEDITION_ID` also extracts
//! `nations.id`). A new nation copies faction 166's `system_faction_relations` rows (NATION-2).

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, PoisonError, RwLock};

use chrono::{DateTime, Utc};
use sqlx::{MySql, MySqlPool, Row, Transaction};

use crate::data::items::{backpack_type, TemplateKind};
use crate::data::world_data::{FactionRelation, SystemFaction};
use crate::error::Result;
use crate::models::container::equip_slot;
use crate::models::item_task::task_type;
use crate::packets::world::{SCFactionListPacket, SCFactionRelationListPacket};
use crate::server::GameServer;
use crate::systems::economy::error_message;
use crate::systems::economy::inventory::{self, ItemTxn};
use crate::systems::economy::mail::{self, mail_status, mail_type, BaseMail};
use crate::systems::economy::mail_character;
use crate::systems::economy::mail_packets::{SCChargeMoneyPaidPacket, SCMailDeletedPacket};
use crate::systems::skills::effects::EffectArgs;
use crate::systems::skills::types::{CastAction, SkillCaster};
use crate::world::{PlayerRef, WorldObject};

use super::faction;
use super::packets::faction::{
    SCFactionDeclareHostileResultPacket, SCFactionImmigrateInvitePacket,
    SCFactionImmigrateInviteResultPacket, SCFactionImmigrateToOriginResultPacket,
    SCFactionIndependencePacket, SCFactionKickToOriginResultPacket, SCFactionOwnerChangedPacket,
    SCFactionRelationExpiredPacket, SCFactionSetRelationStatePacket,
};
use super::packets::misc::{invitation_canceled_kind, SCInvitationCanceledPacket};
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

/// Upper bound of the open treaty-request mails (one per nation and diplomacy target).
pub const MAX_PENDING_TREATIES: usize = 1024;

/// Upper bound of pending nation rename offers; a full set is cleared so it cannot grow
/// without bound when characters leave without a logout packet (mirrors the guild offers).
const MAX_NATION_RENAME_OFFERS: usize = 256;

/// Nation diplomacy settings from `content_configs` (kind 19, NATION-17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NationSettings {
    /// `content_configs` 46: the friendly treaty period. The real 1.2 value is 168; the client
    /// does not read it, so the unit is INFERRED as hours (NATION-17).
    pub friend_period_hours: u32,
    /// `content_configs` 50: the no-treaty window after declaring hostility. The real value is
    /// 21; its meaning and unit are UNKNOWN (INFERRED hours, NATION-17).
    pub hostile_period_hours: u32,
    /// `content_configs` 54: how long a treaty request stays acceptable before the applied
    /// period ends (48 in 1.2; INFERRED hours, matching the mail tip's "2 days").
    pub treaty_lead_hours: u32,
    /// `content_configs` 51 `NationDiplomacyFriendCost` (2,000,000 copper in 1.2), read by the
    /// client in `FUN_3906cc00`.
    pub friend_cost_copper: i64,
}

impl Default for NationSettings {
    fn default() -> Self {
        Self {
            friend_period_hours: 168,
            hostile_period_hours: 21,
            treaty_lead_hours: 48,
            friend_cost_copper: 2_000_000,
        }
    }
}

impl NationSettings {
    /// Loads ids 46 / 50 / 51 / 54; a missing or non-positive row keeps the default (the
    /// client's value array is zero-initialised, so it also reads 0; NATION-17).
    pub fn load(conn: &rusqlite::Connection) -> rusqlite::Result<Self> {
        // `content_configs.value` by id (0 when the row is missing).
        let load_content_config = |conn: &rusqlite::Connection, id: u32| -> rusqlite::Result<i32> {
            use rusqlite::OptionalExtension;
            conn.query_row(
                "SELECT value FROM content_configs WHERE id = ?1",
                [id],
                |r| r.get::<_, i32>(0),
            )
            .optional()
            .map(|v| v.unwrap_or(0))
        };
        let mut settings = Self::default();
        let load = |id: u32, slot: &mut u32| -> rusqlite::Result<()> {
            let value = load_content_config(conn, id)?;
            if value > 0 {
                *slot = value as u32;
            }
            Ok(())
        };
        load(46, &mut settings.friend_period_hours)?;
        load(50, &mut settings.hostile_period_hours)?;
        load(54, &mut settings.treaty_lead_hours)?;
        let cost = load_content_config(conn, 51)?;
        if cost > 0 {
            settings.friend_cost_copper = i64::from(cost);
        }
        Ok(settings)
    }
}

/// An open treaty-request mail (in memory; the mail itself is authoritative and is loaded by
/// the mail manager, NATION-15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingTreatyMail {
    pub nation_id: u32,
    pub other_faction_id: u32,
    pub mail_id: i64,
    pub deadline: i64,
}

/// One relation that reached its `expire_at` on the periodic tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelationExpiry {
    pub nation_id: u32,
    pub other_faction_id: u32,
    pub prev_state: u8,
    pub state: u8,
    /// The row's post-expiry `next_state` (hostile is kept as the "offer window ended"
    /// marker, NATION-17).
    pub next_state: u8,
}

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
/// are normalised); `expire_at` / `next_state` drive the stage-3 relation expiry.
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

    /// A hostile row that a king declared (`CSFactionDeclareHostile`, [`declare_hostile`]), as
    /// opposed to one a new nation merely inherited. A declaration sets the hostile `next_state`
    /// marker, and it survives the end of the declaration window (`expire_relations`, NATION-17:
    /// `expire_at` is reset to 0, so the window itself is not the test). The faction-166 template
    /// rows every new nation copies (`template_relations`) are hostile too but keep
    /// `next_state` 0 (the same marker `send_due_treaty_mails` relies on).
    pub fn is_declared_hostile(&self) -> bool {
        self.state == faction::relation_state::HOSTILE
            && self.next_state == faction::relation_state::HOSTILE
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
            diplomacy_link_id: 0,
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
    treaties: Mutex<Vec<PendingTreatyMail>>,
    /// Last type-25 treaty offer per `(nation, target)` (persisted, NATION-15).
    offers: Mutex<HashMap<(u32, u32), i64>>,
    /// Characters the server offered a nation rename to (`SCNationShowRenameUI`), and the
    /// nation the offer belongs to. The 1.2 client can only send the nation half of
    /// `CSRenameExpedition` from that dialog (RENAME-7).
    rename_offers: Mutex<HashMap<u32, u32>>,
    /// Offline members the last periodic save wrote (`nation id`, ids); pruned from
    /// `offline_members` by [`commit_save`](NationManager::commit_save) once that save committed.
    pub(super) saved_offline: Mutex<Vec<(u32, Vec<u32>)>>,
}

/// Why a nation rename was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NationRenameFailure {
    /// The faction id is not a loaded nation.
    Missing,
    /// The requester is not the nation's king.
    NotKing,
    /// Another faction (nation, expedition or static faction) already carries the name.
    NameExists,
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
        let mut offers = HashMap::new();
        for r in sqlx::query(
            "SELECT CAST(nation_id AS SIGNED) AS nation_id, \
             CAST(other_faction_id AS SIGNED) AS other_faction_id, last_offer_at \
             FROM nation_relation_offers",
        )
        .fetch_all(pool)
        .await?
        {
            let nation_id = r.try_get::<i64, _>("nation_id")? as u32;
            let other = r.try_get::<i64, _>("other_faction_id")? as u32;
            if index.contains_key(&nation_id) {
                offers.insert((nation_id, other), r.try_get("last_offer_at")?);
            }
        }
        *self.offers.lock().unwrap_or_else(PoisonError::into_inner) = offers;
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

    /// Grants `character_id` the right to answer the nation rename dialog
    /// (`SCNationShowRenameUI` was sent to it), bound to `nation_id`.
    pub fn offer_rename(&self, character_id: u32, nation_id: u32) {
        let mut offers = self
            .rename_offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if offers.len() >= MAX_NATION_RENAME_OFFERS {
            offers.clear();
        }
        offers.insert(character_id, nation_id);
    }

    /// Whether `SCNationShowRenameUI` is outstanding for `character_id` *for `nation_id`*.
    pub fn has_rename_offer(&self, character_id: u32, nation_id: u32) -> bool {
        self.rename_offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&character_id)
            .is_some_and(|id| *id == nation_id)
    }

    /// Consumes the rename offer after a successful rename.
    pub fn take_rename_offer(&self, character_id: u32) {
        self.rename_offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&character_id);
    }

    /// Drops the rename offer (logout, refusal, or a character that can no longer rename).
    pub fn clear_rename_offer(&self, character_id: u32) {
        self.take_rename_offer(character_id);
    }

    /// Drops every offer made for `nation_id` (king change, disband).
    pub fn clear_rename_offers_for(&self, nation_id: u32) {
        self.rename_offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|_, id| *id != nation_id);
    }

    /// Renames nation `id` to `name` when `character_id` is its king and no other nation
    /// carries the name. The king check, the nation-name check and the write share one lock;
    /// the caller checks the expedition / static namespace under [`super::rename::lock_names`]
    /// and validates the name shape first. Returns whether the nation exists and the caller
    /// may rename.
    pub fn rename(
        &self,
        id: u32,
        character_id: u32,
        name: &str,
    ) -> std::result::Result<(), NationRenameFailure> {
        let mut nations = self.write();
        let index = nations
            .iter()
            .position(|n| n.id == id)
            .ok_or(NationRenameFailure::Missing)?;
        if nations[index].owner_character_id != character_id {
            return Err(NationRenameFailure::NotKing);
        }
        if nations.iter().any(|n| n.id != id && n.name == name) {
            return Err(NationRenameFailure::NameExists);
        }
        nations[index].name = name.to_string();
        Ok(())
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

    /// Removes the invitations whose timeout passed and returns their invitee character ids.
    /// The list holds at most one invitation per invitee (`add_invite` replaces), so no other
    /// nation invitation (popup type 0x29) can still be pending for an expired invitee.
    pub fn expire_invites(&self, now_ms: i64) -> Vec<u32> {
        let mut invites = self.invites.lock().unwrap_or_else(PoisonError::into_inner);
        let expired: Vec<u32> = invites
            .iter()
            .filter(|i| !i.is_valid_at(now_ms))
            .map(|i| i.invitee_character_id)
            .collect();
        invites.retain(|i| i.is_valid_at(now_ms));
        expired
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
        for nation in &changed {
            self.clear_rename_offers_for(nation.id);
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
        for nation in &changed {
            self.clear_rename_offers_for(nation.id);
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
        self.row_between(a, b).map(|r| r.state)
    }

    /// The stored row behind [`Self::relation`].
    pub fn row_between(&self, a: u32, b: u32) -> Option<NationRelation> {
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
                    .copied();
            }
            if nation.id == b {
                return nation
                    .relations
                    .iter()
                    .find(|r| r.other_faction_id == a)
                    .copied();
            }
        }
        None
    }

    /// The stored relation row of `nation_id` toward `other` (the caller's lock is released
    /// before returning).
    pub fn relation_row(&self, nation_id: u32, other: u32) -> Option<NationRelation> {
        self.read()
            .iter()
            .find(|n| n.id == nation_id)?
            .relations
            .iter()
            .find(|r| r.other_faction_id == other)
            .copied()
    }

    /// Sets a relation row (creating it for an id the template does not name). One lock covers
    /// the lookup and the write (NATION-15).
    pub fn set_relation(
        &self,
        nation_id: u32,
        other: u32,
        state: u8,
        expire_at: i64,
        next_state: u8,
    ) -> bool {
        let mut nations = self.write();
        let Some(nation) = nations.iter_mut().find(|n| n.id == nation_id) else {
            return false;
        };
        match nation
            .relations
            .iter_mut()
            .find(|r| r.other_faction_id == other)
        {
            Some(row) => {
                row.state = state;
                row.expire_at = expire_at;
                row.next_state = next_state;
            }
            None => nation.relations.push(NationRelation {
                other_faction_id: other,
                state,
                expire_at,
                next_state,
            }),
        }
        true
    }

    /// Moves every due relation to its `next_state` (a `next_state` of 0 keeps the current
    /// state: the hostile no-treaty window ends but the relation stays hostile, NATION-17).
    /// The returned events are sent after the lock is released.
    pub fn expire_relations(&self, now: i64) -> Vec<RelationExpiry> {
        let mut out = Vec::new();
        for nation in self.write().iter_mut() {
            for relation in nation
                .relations
                .iter_mut()
                .filter(|r| r.expire_at > 0 && r.expire_at <= now)
            {
                let prev_state = relation.state;
                let planned_next = relation.next_state;
                let state = if planned_next == 0 {
                    prev_state
                } else {
                    planned_next
                };
                relation.state = state;
                relation.expire_at = 0;
                // A row that explicitly named hostile as its next state keeps that marker: it
                // means "the declaration window ended" and makes the hostile side eligible for
                // a treaty request (the faction-166 template rows keep next_state 0 and are
                // never offered, NATION-17).
                relation.next_state =
                    if planned_next == state && state == faction::relation_state::HOSTILE {
                        faction::relation_state::HOSTILE
                    } else {
                        0
                    };
                out.push(RelationExpiry {
                    nation_id: nation.id,
                    other_faction_id: relation.other_faction_id,
                    prev_state,
                    state,
                    next_state: relation.next_state,
                });
            }
        }
        out
    }

    /// The tracked treaty-request mail of `(nation, other)`.
    pub fn treaty(&self, nation_id: u32, other: u32) -> Option<PendingTreatyMail> {
        self.treaties
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|p| p.nation_id == nation_id && p.other_faction_id == other)
            .copied()
    }

    /// Removes and returns the tracked treaty-request mail of `(nation, other)` (the caller
    /// deletes the mail itself, NATION-15).
    pub fn take_treaty(&self, nation_id: u32, other: u32) -> Option<PendingTreatyMail> {
        let mut treaties = self.treaties.lock().unwrap_or_else(PoisonError::into_inner);
        let index = treaties
            .iter()
            .position(|p| p.nation_id == nation_id && p.other_faction_id == other)?;
        Some(treaties.remove(index))
    }

    /// Tracks a treaty-request mail for a later accept / expiry (replace an existing entry of
    /// the pair: the caller deletes the old mail first). Expired entries are dropped and the
    /// list is capped so it cannot grow without bound (NATION-15).
    pub fn add_treaty(&self, pending: PendingTreatyMail, now: i64) {
        let mut treaties = self.treaties.lock().unwrap_or_else(PoisonError::into_inner);
        treaties.retain(|p| {
            (p.nation_id != pending.nation_id || p.other_faction_id != pending.other_faction_id)
                && (p.deadline == 0 || p.deadline > now)
                && p.mail_id != pending.mail_id
        });
        if treaties.len() >= MAX_PENDING_TREATIES {
            treaties.remove(0);
        }
        treaties.push(pending);
    }

    /// The last type-25 offer of `(nation, other)` (unix seconds), persisted.
    pub fn last_offer_at(&self, nation_id: u32, other: u32) -> Option<i64> {
        self.offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(nation_id, other))
            .copied()
    }

    /// Records an offer; the value only moves forward so a delayed persistence task cannot
    /// walk it back.
    pub fn set_offer_at(&self, nation_id: u32, other: u32, at: i64) {
        let mut offers = self.offers.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = offers.entry((nation_id, other)).or_insert(at);
        if *slot < at {
            *slot = at;
        }
    }

    pub fn offers_snapshot(&self) -> Vec<((u32, u32), i64)> {
        self.offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect()
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

    /// Updates the cached owner name of a renamed king. Returns the number of changed nations.
    /// The caller owns `SaveManager::transaction_lock`, so the periodic save cannot write the
    /// old name back after this swap.
    pub fn rename_character(&self, character_id: u32, new_name: &str) -> usize {
        let mut nations = self.write();
        let mut changed = 0;
        for nation in nations.iter_mut() {
            if nation.owner_character_id == character_id && nation.owner_name != new_name {
                nation.owner_name = new_name.to_string();
                changed += 1;
            }
        }
        changed
    }

    pub fn add(&self, nation: Nation) {
        let mut nations = self.write();
        match nations.iter_mut().find(|n| n.id == nation.id) {
            Some(slot) => *slot = nation,
            None => nations.push(nation),
        }
    }

    /// Inserts a nation when no other nation already carries the name. The check and the
    /// insert share the write lock, so two concurrent creations cannot take one name
    /// (RENAME-3). Returns false when the name is taken.
    pub fn add_if_name_free(&self, nation: Nation) -> bool {
        let mut nations = self.write();
        if nations
            .iter()
            .any(|n| n.id != nation.id && n.name == nation.name)
        {
            return false;
        }
        match nations.iter_mut().find(|n| n.id == nation.id) {
            Some(slot) => *slot = nation,
            None => nations.push(nation),
        }
        true
    }

    /// After the creation write committed, the offline members need no further rewrites (their
    /// `characters.faction_id` is already the nation). A failed creation write keeps the list so
    /// the periodic save retries it.
    pub fn clear_offline_members(&self, id: u32) {
        if let Some(nation) = self.write().iter_mut().find(|n| n.id == id) {
            nation.offline_members.clear();
        }
    }

    /// Drops one character from every nation's offline-member list (a kick moved its faction, so
    /// a later save must not write the nation back).
    pub fn remove_offline_member(&self, character_id: u32) {
        for nation in self.write().iter_mut() {
            nation.offline_members.retain(|&id| id != character_id);
        }
    }

    /// After the save transaction committed: the offline members it wrote need no further
    /// rewrites. Members added or kept since the snapshot stay untouched.
    pub fn commit_save(&self) {
        let saved = std::mem::take(&mut *self.saved_offline.lock().expect("nation saved poisoned"));
        let mut nations = self.write();
        for (id, members) in saved {
            if let Some(nation) = nations.iter_mut().find(|n| n.id == id) {
                nation.offline_members.retain(|m| !members.contains(m));
            }
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
        let mut saved = Vec::new();
        for nation in &nations {
            written += write_nation(server, tx, nation, &online).await?;
            let members: Vec<u32> = nation
                .offline_members
                .iter()
                .copied()
                .filter(|id| !online.contains(id))
                .collect();
            if !members.is_empty() {
                saved.push((nation.id, members));
            }
        }
        *self.saved_offline.lock().expect("nation saved poisoned") = saved;
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

/// The nation row that decides the relation between `a` and `b`, when a nation's runtime row
/// does (the first part of [`relation_between`], which returns this row's state).
fn nation_row_between(server: &GameServer, a: u32, b: u32) -> Option<NationRelation> {
    let nations = &server.systems.social.nations;
    // `diplomacy_link_id` mirroring comes first, before the nation's stored row: the
    // faction-166 template stores explicit HOSTILE rows for the guard / front factions
    // (134 -> 149, 135 -> 148, 157 -> 149, 167 -> 148, 168 -> 149, 171 -> 148, 173 -> 149 in the
    // 1.2 compact database), and those must follow a later treaty with their alliance
    // (INFERRED; NATION-16).
    if let Some(other) = server.game_data.factions.get(b) {
        if nations.is_nation(a) && other.diplomacy_link_id != 0 {
            if let Some(row) = nations.row_between(a, other.diplomacy_link_id) {
                return Some(row);
            }
        }
    }
    if let Some(other) = server.game_data.factions.get(a) {
        if nations.is_nation(b) && other.diplomacy_link_id != 0 {
            if let Some(row) = nations.row_between(b, other.diplomacy_link_id) {
                return Some(row);
            }
        }
    }
    nations.row_between(a, b)
}

/// Whether two characters' faction ids (`a`, `b`) are in a hostile relation that a nation
/// **declared**: the nation row that decides it (`nation_row_between`, the one
/// [`relation_between`] uses) is hostile and carries the declared marker
/// ([`NationRelation::is_declared_hostile`]). Hostility that only comes from
/// the static `system_faction_relations` or from the faction-166 template rows a new nation
/// copies does not count. A faction with a mother (an alliance member, a guild) stands for its
/// mother, exactly as in the relation lookup (`faction::relation_state_live`), so Nuia's member
/// faction 101 is matched through the Nuia Alliance 148 the rows are keyed by.
pub fn declared_hostile(server: &GameServer, a: u32, b: u32) -> bool {
    let side = |id: u32| match faction_of_id(server, id) {
        Some((_, mother)) if mother != 0 => mother,
        _ => id,
    };
    nation_row_between(server, side(a), side(b)).is_some_and(|row| row.is_declared_hostile())
}

/// The relation between `a` and `b`, preferring a nation's runtime rows over the static table.
/// Both loaded forms of the static table are consulted (the social manager's map and the raw
/// `FactionData` rows), because fixtures and loaders fill one or the other.
pub fn relation_between(server: &GameServer, a: u32, b: u32) -> Option<u8> {
    if let Some(row) = nation_row_between(server, a, b) {
        return Some(row.state);
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
    // Player nations are KR-only content (no Trion 1.2 evidence; `PORTING.md`, NATION row).
    // The refusal is the same one a failed ownership precondition uses, so no new error id is
    // introduced.
    if !server.config.player_nations_enabled() {
        return Err(error_message::FACTION_SEPARATE_RIGHTS);
    }
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
        || super::rename::faction_name_in_use(
            server,
            name,
            Some(super::rename::NameOwner::Expedition(expedition_id)),
        )
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
    {
        // Guild / nation / static names share one namespace; the lock keeps the final check
        // and the insert together (RENAME-3 / RENAME-6).
        let _names = super::rename::lock_names(server);
        if super::rename::faction_name_in_use(
            server,
            name,
            Some(super::rename::NameOwner::Expedition(expedition_id)),
        ) || !server
            .systems
            .social
            .nations
            .add_if_name_free(nation.clone())
        {
            return Err(error_message::FACTION_DUPLICATE_NAME);
        }
    }
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
    if !server.config.player_nations_enabled() {
        if let Some(player) = args.caster.as_ref().and_then(|c| c.as_player()) {
            send_error(player, error_message::FACTION_SEPARATE_RIGHTS);
        }
        return;
    }
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

// ---- Rename (stage 4) ----

/// Asks the server to offer the king a nation rename: `SCNationShowRenameUI` (0x010, empty
/// body) plus the one-shot offer that [`rename_nation`] requires. The 1.2 handler
/// (`FUN_391d2380` -> `FUN_392f8e80(0)`) opens the dialog only when the player owns a user
/// faction. The production trigger is unknown (RENAME-7); the GM hook and tests call this.
pub fn offer_nation_rename_ui(server: &GameServer, p: &PlayerRef) -> std::result::Result<(), i16> {
    if !server.config.player_nations_enabled() {
        return Err(error_message::ONLY_NATION_OWNER);
    }
    let Some(nation) = server.systems.social.nations.owned_by(p.character_id) else {
        return Err(error_message::ONLY_NATION_OWNER);
    };
    server
        .systems
        .social
        .nations
        .offer_rename(p.character_id, nation.id);
    super::rename::send_nation_rename_ui(p);
    Ok(())
}

/// `CSRenameExpeditionPacket` with `is_expedition == false`: the king renames the nation from
/// the `SCNationShowRenameUI` dialog. The offer binds the request to the nation the dialog was
/// opened for. The name shape and the shared guild / nation / static-faction namespace are
/// checked before anything changes; a rejected name re-opens the dialog via
/// `SCFactionRetryRename` (xpdt false) and keeps the offer usable (RENAME-7).
pub async fn rename_nation(server: &GameServer, p: &PlayerRef, id: u32, name: &str) {
    if !server.config.player_nations_enabled() {
        send_error(p, error_message::ONLY_NATION_OWNER);
        return;
    }
    let nations = &server.systems.social.nations;
    if !nations.has_rename_offer(p.character_id, id) {
        tracing::warn!(
            "RenameNation {} by {} without a ShowRenameUI offer refused",
            id,
            p.character_id
        );
        send_error(p, error_message::ONLY_NATION_OWNER);
        return;
    }
    if let Err(code) = super::rename::validate_faction_name(server, name) {
        super::rename::send_retry(p, false, code, name);
        return;
    }
    // Re-submitting the current name is a no-op; the nation and its founding guild may share
    // the name (NATION-1), so the shared namespace check would otherwise see the guild.
    if nations.get(id).is_some_and(|nation| nation.name == name) {
        nations.take_rename_offer(p.character_id);
        return;
    }
    let result = {
        // The nation and expedition stores have separate locks; the shared name lock keeps
        // the cross-store check and the write together (RENAME-3 / RENAME-6).
        let _names = super::rename::lock_names(server);
        if super::rename::faction_name_in_use(
            server,
            name,
            Some(super::rename::NameOwner::Nation(id)),
        ) {
            Err(NationRenameFailure::NameExists)
        } else {
            nations.rename(id, p.character_id, name)
        }
    };
    match result {
        Ok(()) => {
            nations.take_rename_offer(p.character_id);
            persist_nation_name(server, id, name).await;
            super::rename::broadcast_renamed(server, id, name, false);
        }
        Err(NationRenameFailure::NameExists) => {
            super::rename::send_retry(p, false, error_message::EXPEDITION_NAME_EXIST, name);
        }
        Err(NationRenameFailure::NotKing) => {
            nations.clear_rename_offer(p.character_id);
            send_error(p, error_message::ONLY_NATION_OWNER);
        }
        Err(NationRenameFailure::Missing) => {
            nations.clear_rename_offer(p.character_id);
            send_error(p, error_message::FACTION_RIGHTS);
        }
    }
}

/// Writes the new name immediately; the periodic nation save writes the same row again, so a
/// failed write is retried (RENAME-2 covers the guild path, which has no periodic save).
async fn persist_nation_name(server: &GameServer, id: u32, name: &str) {
    let _save = server.save.transaction_lock().await;
    if let Err(error) = sqlx::query("UPDATE nations SET name = ? WHERE id = ?")
        .bind(name)
        .bind(id)
        .execute(&server.db)
        .await
    {
        tracing::error!("Persisting the name of nation {id} failed: {error}");
    }
}

// ---- National monument (stage 4) ----

/// The 1.2 territory-statue skills 23681..23688 / 23682 carry `max_range` 20 (compact
/// `skills`); the placement skill's exact server rule is capture-pending (RENAME-9).
pub const NATIONAL_MONUMENT_RANGE: f32 = 20.0;

fn monument_distance(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    let (dx, dy, dz) = (a.0 - b.0, a.1 - b.1, a.2 - b.2);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

fn monument_error(error: crate::systems::housing::dominion::state::DominionError) -> i16 {
    use crate::systems::housing::dominion::state::DominionError;
    match error {
        DominionError::NoMoreNationalMonument => error_message::NO_MORE_NATIONAL_MONUMENT,
        DominionError::NationalMonumentDuringSiege => {
            error_message::CANT_PUT_UP_NATIONAL_MONUMENT_DURING_SIEGE_PERIOD
        }
        _ => error_message::ONLY_NATION_OWNER,
    }
}

fn broadcast_monument(
    server: &GameServer,
    state: &crate::systems::housing::dominion::state::DominionState,
) {
    let Ok(zone) = u16::try_from(state.zone_group_id) else {
        return;
    };
    server
        .world
        .broadcast_to_server(&super::packets::misc::SCNationalMonumentChangedPacket {
            id: zone,
            national_monument_db_id: state.national_monument_db_id,
            x: state.national_monument_x,
            y: state.national_monument_y,
            z: state.national_monument_z,
        });
}

/// The king of a dominion's nation places its national monument: the territory statue doodad
/// named by `siege_zones.monument_doodad_id` (7229..7234 for the six 1.2 castle zone groups;
/// the doodad names match the territory-statue skills). The gates use the client's own error
/// ids: 748 not a nation king, 757 not the king of this dominion or not at the point, 750 the
/// dominion already has one / has no statue template, 751 while a siege is scheduled or
/// active. The placement trigger (skills 23681..23688) and its cost are capture-pending, so
/// the GM hook and tests call this service (RENAME-9).
pub async fn place_national_monument(
    server: &GameServer,
    king: &PlayerRef,
    zone: u16,
    x: f32,
    y: f32,
    z: f32,
) -> std::result::Result<(), i16> {
    if !server.config.player_nations_enabled() {
        return Err(error_message::ONLY_NATION_OWNER);
    }
    if ![x, y, z].iter().all(|v| v.is_finite()) {
        return Err(error_message::ONLY_NATION_OWNER);
    }
    // Phase 1: validate and spawn the statue while holding the housing operation lock.
    let (state, world, doodad) = {
        let _items = server.systems.economy.operation_scope();
        let _operation = server
            .systems
            .housing
            .operation_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(state) = server.systems.housing.dominions.get(u32::from(zone)) else {
            return Err(error_message::ONLY_NATION_OWNER);
        };
        let Some(nation) = server.systems.social.nations.owned_by(king.character_id) else {
            return Err(error_message::ONLY_NATION_OWNER);
        };
        if state.owner_expedition_id == 0 {
            return Err(error_message::ONLY_NATION_OWNER);
        }
        if expedition_top_level_faction(server, state.owner_expedition_id) != Some(nation.id) {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        }
        if state.national_monument_db_id != 0 {
            return Err(error_message::NO_MORE_NATIONAL_MONUMENT);
        }
        if state.siege.as_ref().is_some_and(|siege| {
            siege.status != crate::systems::housing::dominion::state::SiegeStatus::Finished
        }) {
            return Err(error_message::CANT_PUT_UP_NATIONAL_MONUMENT_DURING_SIEGE_PERIOD);
        }
        let Some(world) = server.world.world(state.tower.instance_id) else {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        };
        let player = WorldObject::from(king);
        if server.world.world_of(&player).map(|w| w.id) != Some(world.id) {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        }
        let here = world.world_pos_rot(&player);
        if monument_distance((here.x, here.y, here.z), (x, y, z)) > NATIONAL_MONUMENT_RANGE {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        }
        let zone_key = world.zone_id_at(x, y);
        if server
            .game_data
            .zones
            .zone_by_key(zone_key)
            .map(|zone| zone.group_id)
            != Some(state.zone_group_id)
        {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        }
        let Some(siege_zone) = server
            .game_data
            .systems
            .housing
            .castles
            .siege_zones
            .get(&u32::from(zone))
        else {
            return Err(error_message::NO_MORE_NATIONAL_MONUMENT);
        };
        if siege_zone.monument_doodad_id == 0 {
            return Err(error_message::NO_MORE_NATIONAL_MONUMENT);
        }
        let Some(doodad) = crate::systems::npcs::doodad::manager::create(
            server,
            &world,
            0,
            siege_zone.monument_doodad_id,
            None,
            false,
        ) else {
            return Err(error_message::FACTION_CREATE);
        };
        {
            let mut data = doodad.data();
            data.transform.set_position(x, y, z, 0.0, 0.0, 0.0);
            data.transform.zone_id = zone_key;
            data.is_persistent = true;
        }
        crate::systems::npcs::doodad::manager::spawn(server, &world, &doodad);
        (state, world, doodad)
    };
    // Phase 2: write the statue row before anything can point at it (RENAME-9). The dominion
    // row is written later, so a crash leaves an orphan doodad, never a dangling db id. Unit
    // tests have no database handle (`self_arc` is None); the in-memory writer's fire-and-
    // forget form still allocates the db id there.
    let doodad_save = if server.self_arc().is_some() {
        crate::systems::npcs::doodad::db::save_now(server, &doodad).await
    } else {
        crate::systems::npcs::doodad::db::save(server, &doodad);
        Ok(())
    };
    if let Err(error) = doodad_save {
        tracing::error!("Nation monument doodad save failed: {error}");
        crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
        return Err(error_message::FACTION_CREATE);
    }
    let db_id = doodad.data().db_id;
    if db_id == 0 {
        crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
        return Err(error_message::FACTION_CREATE);
    }
    // Phase 3: re-validate and commit the reference under the same operation lock the claim /
    // capture paths use.
    let state = {
        let _items = server.systems.economy.operation_scope();
        let _operation = server
            .systems
            .housing
            .operation_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(current) = server.systems.housing.dominions.get(u32::from(zone)) else {
            crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
            return Err(error_message::ONLY_NATION_OWNER);
        };
        if current.tower != state.tower || current.owner_expedition_id != state.owner_expedition_id
        {
            crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        }
        if current.national_monument_db_id != 0 {
            crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
            return Err(error_message::NO_MORE_NATIONAL_MONUMENT);
        }
        if current.siege.as_ref().is_some_and(|siege| {
            siege.status != crate::systems::housing::dominion::state::SiegeStatus::Finished
        }) {
            crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
            return Err(error_message::CANT_PUT_UP_NATIONAL_MONUMENT_DURING_SIEGE_PERIOD);
        }
        let state = match server.systems.housing.dominions.place_national_monument(
            u32::from(zone),
            current.owner_expedition_id,
            db_id,
            x,
            y,
            z,
        ) {
            Ok(state) => state,
            Err(error) => {
                crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
                return Err(monument_error(error));
            }
        };
        broadcast_monument(server, &state);
        state
    };
    // The in-memory row stays dirty for the periodic financial save; this targeted write only
    // makes the reference durable now (the doodad row exists already).
    if server.self_arc().is_some() {
        if let Err(error) = super::dominion::persist_national_monument(
            server,
            state.zone_group_id,
            i64::from(db_id),
            x,
            y,
            z,
        )
        .await
        {
            tracing::error!("Persisting the national monument of zone {zone} failed: {error}");
        }
    }
    Ok(())
}

/// Removes the dominion's national monument (skill 23682 "statue removal"): the kingdom's king
/// must stand at the statue; the stored fields are cleared, every client receives
/// `SCNationalMonumentChanged` with db id 0, the dominion row is zeroed, and only then is the
/// doodad row deleted (a crash may leave an orphan statue, never a dangling reference).
/// Removing the same monument twice is a no-op. Siege state does not block removal (only
/// placement has 751).
pub async fn remove_national_monument(
    server: &GameServer,
    king: &PlayerRef,
    zone: u16,
) -> std::result::Result<(), i16> {
    if !server.config.player_nations_enabled() {
        return Err(error_message::ONLY_NATION_OWNER);
    }
    let (state, world, old_db_id) = {
        let _items = server.systems.economy.operation_scope();
        let _operation = server
            .systems
            .housing
            .operation_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(state) = server.systems.housing.dominions.get(u32::from(zone)) else {
            return Err(error_message::ONLY_NATION_OWNER);
        };
        let Some(nation) = server.systems.social.nations.owned_by(king.character_id) else {
            return Err(error_message::ONLY_NATION_OWNER);
        };
        if state.owner_expedition_id == 0
            || expedition_top_level_faction(server, state.owner_expedition_id) != Some(nation.id)
        {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        }
        if state.national_monument_db_id == 0 {
            return Ok(());
        }
        let Some(world) = server.world.world(state.tower.instance_id) else {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        };
        let player = WorldObject::from(king);
        if server.world.world_of(&player).map(|w| w.id) != Some(world.id) {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        }
        let here = world.world_pos_rot(&player);
        if monument_distance(
            (here.x, here.y, here.z),
            (
                state.national_monument_x,
                state.national_monument_y,
                state.national_monument_z,
            ),
        ) > NATIONAL_MONUMENT_RANGE
        {
            return Err(error_message::NATION_OWNER_AT_POS_ONLY);
        }
        let old_db_id = u32::try_from(state.national_monument_db_id.max(0)).unwrap_or(0);
        let state = match server
            .systems
            .housing
            .dominions
            .clear_national_monument(u32::from(zone), state.owner_expedition_id)
        {
            Ok(state) => state,
            Err(error) => return Err(monument_error(error)),
        };
        broadcast_monument(server, &state);
        (state, world, old_db_id)
    };
    if server.self_arc().is_some() {
        if let Err(error) = super::dominion::persist_national_monument(
            server,
            state.zone_group_id,
            0,
            0.0,
            0.0,
            0.0,
        )
        .await
        {
            tracing::error!("Clearing the national monument of zone {zone} failed: {error}");
        }
    }
    if let Some(doodad) = world.doodad_by_db_id(old_db_id) {
        crate::systems::npcs::doodad::manager::delete(server, &world, &doodad);
    } else {
        crate::systems::npcs::doodad::db::delete_row(server, old_db_id);
    }
    Ok(())
}

// ---- Diplomacy (stage 3): declare hostility, friendly treaty mails, relation expiry ----

fn hours_to_secs(hours: u32) -> i64 {
    i64::from(hours) * 3600
}

fn utc(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(secs, 0).unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// The online players of `faction`'s top-level side (a nation, or an alliance such as 148 /
/// 149 whose race factions and guilds belong to it). Duplicates are removed.
fn side_members(server: &GameServer, faction: u32) -> Vec<PlayerRef> {
    let mut seen = HashSet::new();
    server
        .world
        .all_characters()
        .into_iter()
        .filter(|p| {
            let saved = crate::db_character::persisted_faction(&p.data());
            top_level_faction(server, saved) == Some(faction) && seen.insert(p.character_id)
        })
        .collect()
}

fn send_relation_state(
    server: &GameServer,
    nation_id: u32,
    other_faction_id: u32,
    state: u8,
    expire_at: i64,
    next_state: u8,
) {
    let packet = SCFactionSetRelationStatePacket {
        id: nation_id,
        id2: other_faction_id,
        state,
        expire_time: utc(expire_at),
        next_state,
    };
    for member in side_members(server, nation_id) {
        member.send(packet.clone());
    }
    for member in side_members(server, other_faction_id) {
        member.send(packet.clone());
    }
}

/// Sends the expired notification (prev -> curr) and the new relation state to both sides.
/// The order is SetRelationState first so the client stores the new state before its message
/// (`FUN_392fcae0` shows `hostile_nation_msg` / `friendly_nation_msg` for `curr`, NATION-17).
fn notify_relation_expiry(server: &GameServer, event: &RelationExpiry) {
    send_relation_state(
        server,
        event.nation_id,
        event.other_faction_id,
        event.state,
        0,
        event.next_state,
    );
    if event.prev_state != event.state {
        let packet = SCFactionRelationExpiredPacket {
            id: event.nation_id,
            id2: event.other_faction_id,
            prev_state: event.prev_state,
            curr_state: event.state,
        };
        for member in side_members(server, event.nation_id) {
            member.send(packet.clone());
        }
        for member in side_members(server, event.other_faction_id) {
            member.send(packet.clone());
        }
    }
}

/// `CSFactionDeclareHostile` (0x019): the nation king declares hostility toward a diplomacy
/// target. The checks mirror the 1.2 client's pre-send validator (`FUN_392f9430` ->
/// `FUN_398a4750` / `FUN_398a3660`): not the king -> 32 `FACTION_RIGHTS`; anything that is not
/// an `is_diplomacy_tgt` system faction (or is already hostile) -> 718
/// `FACTION_DIPLOMACY_INVALID_SUBJECT` (NATION-16). Success sends the result to the king and
/// the new state to both sides.
pub fn declare_hostile(
    server: &GameServer,
    king: &PlayerRef,
    target: u32,
) -> std::result::Result<(), i16> {
    if !server.config.player_nations_enabled() {
        return Err(error_message::FACTION_RIGHTS);
    }
    let Some(nation) = server.systems.social.nations.owned_by(king.character_id) else {
        return Err(error_message::FACTION_RIGHTS);
    };
    let Some(target_faction) = server.game_data.factions.get(target).cloned() else {
        return Err(error_message::FACTION_DIPLOMACY_INVALID_SUBJECT);
    };
    if !target_faction.diplomacy_target || target_faction.id == nation.id {
        return Err(error_message::FACTION_DIPLOMACY_INVALID_SUBJECT);
    }
    let nations = &server.systems.social.nations;
    if nations.relation(nation.id, target) == Some(faction::relation_state::HOSTILE) {
        return Err(error_message::FACTION_DIPLOMACY_INVALID_SUBJECT);
    }
    let settings = server.game_data.systems.social.nation;
    let now = Utc::now().timestamp();
    let expire_at = now + hours_to_secs(settings.hostile_period_hours);
    // The no-treaty window ends at `expire_at`; the relation stays hostile until a treaty is
    // accepted, so `next_state` is hostile again (NATION-17).
    if !nations.set_relation(
        nation.id,
        target,
        faction::relation_state::HOSTILE,
        expire_at,
        faction::relation_state::HOSTILE,
    ) {
        return Err(error_message::FACTION_DIPLOMACY_INVALID_SUBJECT);
    }
    if let Some(old) = nations.take_treaty(nation.id, target) {
        server.systems.economy.mail.delete_mail(server, old.mail_id);
    }
    nations.set_offer_at(nation.id, target, now);
    schedule_persist_offer(server, nation.id, target, now);
    king.send(SCFactionDeclareHostileResultPacket {
        result: true,
        id: nation.id,
        id2: target,
    });
    send_relation_state(
        server,
        nation.id,
        target,
        faction::relation_state::HOSTILE,
        expire_at,
        faction::relation_state::HOSTILE,
    );
    schedule_persist_relation(
        server,
        nation.id,
        target,
        faction::relation_state::HOSTILE,
        expire_at,
        faction::relation_state::HOSTILE,
    );
    Ok(())
}

/// `content_configs` values for a target faction's relation (`None` = not a diplomacy target).
fn diplomacy_target(server: &GameServer, id: u32) -> Option<SystemFaction> {
    server
        .game_data
        .factions
        .get(id)
        .filter(|f| f.diplomacy_target)
        .cloned()
}

/// The treaty-request mail (type 25, `MAIL_NATION_REQUEST_FRIEND`) to the king, with the cost
/// `content_configs` 51 as its charge. Returns the mail id.
fn send_friend_treaty_mail(
    server: &GameServer,
    nation: &Nation,
    target: &SystemFaction,
    now: i64,
) -> Option<i64> {
    let receiver_name = server.names.get_character_name(nation.owner_character_id)?;
    let settings = server.game_data.systems.social.nation;
    let cost = i32::try_from(settings.friend_cost_copper).unwrap_or(i32::MAX);
    let mut mail = BaseMail {
        mail_type: mail_type::NATION_REQUEST_FRIEND,
        title: "Relation treaty request".into(),
        receiver_name,
        ..BaseMail::default()
    };
    mail.header.sender_id = 0;
    mail.header.sender_name = target.name.clone();
    mail.header.receiver_id = nation.owner_character_id;
    mail.header.status = mail_status::UNPAID;
    // The target faction is not a mail column; `extra` is the free tag (the house tax uses it
    // for its zone group / house id). INFERRED carrier, NATION-15.
    mail.header.extra = i64::from(target.id);
    mail.body.text = format!(
        "Make friendly relations with {}? Accepting pays {} copper.",
        target.name, settings.friend_cost_copper
    );
    mail.body.billing_amount = cost;
    mail.body.send_date = now;
    mail.body.recv_date = now;
    mail.open_date = now;
    mail.is_dirty = true;
    let mut txn = ItemTxn::begin(server);
    mail::send_mail(&mut txn, mail)
}

/// Sends one treaty-request mail now (the GM test hook; the periodic scheduler is
/// [`send_due_treaty_mails`]). Returns the mail id.
pub fn send_treaty_mail(server: &GameServer, nation_id: u32, target_id: u32) -> Option<i64> {
    if !server.config.player_nations_enabled() {
        return None;
    }
    let nation = server.systems.social.nations.get(nation_id)?;
    let target = diplomacy_target(server, target_id)?;
    let now = Utc::now().timestamp();
    if let Some(old) = server
        .systems
        .social
        .nations
        .take_treaty(nation_id, target_id)
    {
        server.systems.economy.mail.delete_mail(server, old.mail_id);
    }
    let mail_id = send_friend_treaty_mail(server, &nation, &target, now)?;
    server.systems.social.nations.add_treaty(
        PendingTreatyMail {
            nation_id,
            other_faction_id: target_id,
            mail_id,
            deadline: now + hours_to_secs(server.game_data.systems.social.nation.treaty_lead_hours),
        },
        now,
    );
    server
        .systems
        .social
        .nations
        .set_offer_at(nation_id, target_id, now);
    schedule_persist_offer(server, nation_id, target_id, now);
    Some(mail_id)
}

/// Sends the treaty-request mails that became due: `treaty_lead_hours` before a friendly
/// relation expires, and once a declared hostile window has really ended (a hostile row with
/// the hostile `next_state` marker). The faction-166 template rows (`next_state` 0) are never
/// offered. A per-(nation, target) offer cooldown of at least `treaty_lead_hours` (persisted)
/// stops a deleted / declined mail from being re-sent every tick (NATION-15).
pub fn send_due_treaty_mails(server: &GameServer, now: i64) -> usize {
    if !server.config.player_nations_enabled() {
        return 0;
    }
    let nations = &server.systems.social.nations;
    let mails = &server.systems.economy.mail;
    let settings = server.game_data.systems.social.nation;
    let lead = hours_to_secs(settings.treaty_lead_hours);

    // Snapshot the tracking, then query the mail store without the treaty lock (accepting a
    // mail holds the mail lock and takes the treaty lock).
    let tracked: Vec<PendingTreatyMail> = nations
        .treaties
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .copied()
        .collect();
    let mut stale = Vec::new();
    for entry in &tracked {
        let Some(mail) = mails.get_mail_by_id(entry.mail_id) else {
            stale.push(*entry);
            continue;
        };
        let receiver_ok = nations
            .get(entry.nation_id)
            .is_some_and(|n| n.owner_character_id == mail.header.receiver_id);
        if mail.mail_type != mail_type::NATION_REQUEST_FRIEND
            || mail.header.sender_id != 0
            || mail.header.extra != i64::from(entry.other_faction_id)
            || !receiver_ok
            || (entry.deadline > 0 && entry.deadline <= now)
        {
            stale.push(*entry);
            continue;
        }
        // A tracked mail id can be reused by a different mail after a reload.
        if mail.id != entry.mail_id {
            stale.push(*entry);
        }
    }
    if !stale.is_empty() {
        {
            let mut treaties = nations
                .treaties
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            treaties.retain(|p| {
                !stale
                    .iter()
                    .any(|s| s.nation_id == p.nation_id && s.other_faction_id == p.other_faction_id)
            });
        }
        for entry in &stale {
            mails.delete_mail(server, entry.mail_id);
        }
    }

    let mut sent = 0;
    for nation in nations.all() {
        for relation in &nation.relations {
            let Some(target) = diplomacy_target(server, relation.other_faction_id) else {
                continue;
            };
            let due = match relation.state {
                faction::relation_state::FRIENDLY => {
                    relation.expire_at > now && relation.expire_at - lead <= now
                }
                // Only a relation whose declaration window ended carries this marker; the
                // faction-166 template rows do not (NATION-17).
                faction::relation_state::HOSTILE => {
                    relation.expire_at == 0
                        && relation.next_state == faction::relation_state::HOSTILE
                }
                _ => false,
            };
            if !due || nations.treaty(nation.id, target.id).is_some() {
                continue;
            }
            if let Some(last) = nations.last_offer_at(nation.id, target.id) {
                if now < last + lead {
                    continue;
                }
            }
            let Some(mail_id) = send_friend_treaty_mail(server, &nation, &target, now) else {
                continue;
            };
            nations.add_treaty(
                PendingTreatyMail {
                    nation_id: nation.id,
                    other_faction_id: target.id,
                    mail_id,
                    // A friendly mail is valid until the applied period ends; a hostile-side
                    // offer is valid for one lead period.
                    deadline: if relation.state == faction::relation_state::FRIENDLY {
                        relation.expire_at
                    } else {
                        now + lead
                    },
                },
                now,
            );
            nations.set_offer_at(nation.id, target.id, now);
            schedule_persist_offer(server, nation.id, target.id, now);
            sent += 1;
        }
    }
    sent
}

/// Pays a nation treaty-request mail (`CSPayChargeMoney` 0x0a0, the button's
/// `X2Mail.PayCurrentMailChargeMoney` in the 1.2 `read_mail.alb`): debits the cost, deletes the
/// mail and moves the relation friendly for `friend_period_hours`, with the hostile state as
/// the next state. Only a live tracked request can be paid: a stale mail (old lead mail after a
/// hostile declaration, expired offer, reused mail id, wrong owner) is deleted and refused. The
/// mail fields beyond the charge (target tag in `extra`) are INFERRED (NATION-15).
pub fn pay_friend_treaty(server: &GameServer, payer: &PlayerRef, mail_id: i64) -> bool {
    if !server.config.player_nations_enabled() {
        // Same refusal a stale treaty mail gets; a disabled era has no treaties to pay.
        send_error(payer, error_message::MAIL_INVALID);
        return false;
    }
    let mails = &server.systems.economy.mail;
    let Some(mail) = mails.get_mail_by_id(mail_id) else {
        send_error(payer, error_message::MAIL_INVALID);
        return false;
    };
    let now = Utc::now().timestamp();
    if mail.mail_type != mail_type::NATION_REQUEST_FRIEND
        || mail.header.sender_id != 0
        || mail.header.receiver_id != payer.character_id
        || mail.header.status != mail_status::UNPAID
        || mail.body.billing_amount < 0
        || mail.body.recv_date > now
    {
        send_error(payer, error_message::MAIL_INVALID);
        return false;
    }
    let Some(target_id) = u32::try_from(mail.header.extra).ok().filter(|id| *id != 0) else {
        send_error(payer, error_message::MAIL_INVALID);
        return false;
    };
    let Some(nation) = server.systems.social.nations.owned_by(payer.character_id) else {
        send_error(payer, error_message::FACTION_RIGHTS);
        return false;
    };
    if diplomacy_target(server, target_id).is_none() {
        send_error(payer, error_message::FACTION_DIPLOMACY_INVALID_SUBJECT);
        return false;
    }
    let nations = &server.systems.social.nations;

    // An offer sent before a hostile declaration must not skip the no-treaty window.
    if let Some(relation) = nations.relation_row(nation.id, target_id) {
        if relation.state == faction::relation_state::HOSTILE && relation.expire_at > now {
            nations.take_treaty(nation.id, target_id);
            mails.delete_mail(server, mail_id);
            send_error(payer, error_message::FACTION_DIPLOMACY_INVALID_SUBJECT);
            return false;
        }
    }

    // A paid mail must still be the tracked offer for this nation / target.
    let Some(tracked) = nations.treaty(nation.id, target_id) else {
        mails.delete_mail(server, mail_id);
        send_error(payer, error_message::MAIL_INVALID);
        return false;
    };
    if tracked.mail_id != mail_id || tracked.deadline == 0 || tracked.deadline <= now {
        nations.take_treaty(nation.id, target_id);
        mails.delete_mail(server, mail_id);
        send_error(payer, error_message::MAIL_INVALID);
        return false;
    }

    let mut txn = ItemTxn::begin(server);
    let mut st = mails.state();
    let Some(current) = st.all.get(mail_id) else {
        drop(st);
        nations.take_treaty(nation.id, target_id);
        send_error(payer, error_message::MAIL_INVALID);
        return false;
    };
    if current.mail_type != mail_type::NATION_REQUEST_FRIEND
        || current.header.sender_id != 0
        || current.header.status != mail_status::UNPAID
        || current.header.receiver_id != payer.character_id
        || current.header.extra != i64::from(target_id)
        || current.body.billing_amount != mail.body.billing_amount
    {
        drop(st);
        nations.take_treaty(nation.id, target_id);
        mails.delete_mail(server, mail_id);
        send_error(payer, error_message::MAIL_INVALID);
        return false;
    }
    // INVENTORY -> NONE checks and debits under the character lock (the `subtract_money`
    // helper's NONE -> INVENTORY form does not check the balance, NATION-15).
    if !inventory::change_money(
        &mut txn.out(),
        payer,
        crate::models::container::slot_type::INVENTORY,
        crate::models::container::slot_type::NONE,
        mail.body.billing_amount,
        task_type::DEPOSIT_MONEY,
    ) {
        drop(st);
        return false;
    }
    let settings = server.game_data.systems.social.nation;
    let expire_at = now + hours_to_secs(settings.friend_period_hours);
    if !nations.set_relation(
        nation.id,
        target_id,
        faction::relation_state::FRIENDLY,
        expire_at,
        faction::relation_state::HOSTILE,
    ) {
        // The nation vanished between the checks: refund and keep the mail payable.
        inventory::add_money(
            &mut txn.out(),
            payer,
            crate::models::container::slot_type::INVENTORY,
            mail.body.billing_amount,
            task_type::DEPOSIT_MONEY,
        );
        drop(st);
        send_error(payer, error_message::FACTION_DIPLOMACY_INVALID_SUBJECT);
        return false;
    }
    nations.take_treaty(nation.id, target_id);
    let count = {
        let mut character = payer.data();
        if st
            .all
            .get(mail_id)
            .is_some_and(|m| m.header.status != mail_status::READ)
        {
            character
                .unread_mail
                .update_received(mail_type::NATION_REQUEST_FRIEND, -1);
        }
        if let Some(m) = st.all.get_mut(mail_id) {
            m.header.status = mail_status::READ;
        }
        character.unread_mail
    };
    txn.env
        .fx
        .send(payer.character_id, SCChargeMoneyPaidPacket { mail_id });
    txn.env.fx.send(
        payer.character_id,
        SCMailDeletedPacket {
            is_sent: false,
            mail_id,
            is_unread_mail_count_modified: false,
            count,
        },
    );
    mail::delete_mail_in(server, &mut st, mail_id);
    drop(st);
    mail_character::send_unread_mail_count(&mut txn.env.fx, payer);
    send_relation_state(
        server,
        nation.id,
        target_id,
        faction::relation_state::FRIENDLY,
        expire_at,
        faction::relation_state::HOSTILE,
    );
    schedule_persist_relation(
        server,
        nation.id,
        target_id,
        faction::relation_state::FRIENDLY,
        expire_at,
        faction::relation_state::HOSTILE,
    );
    true
}

/// Closes the popups of the immigration invitations that timed out (kind 7).
pub fn sweep_expired_invites(server: &GameServer, now_ms: i64) {
    for invitee_id in server.systems.social.nations.expire_invites(now_ms) {
        if let Some(invitee) = server.world.character_by_id(invitee_id) {
            invitee.send(SCInvitationCanceledPacket {
                kind: invitation_canceled_kind::NATION,
            });
        }
    }
}

/// The periodic nation tick (subscribed to the existing `TickManager`, NATION-17): applies due
/// relation expiries, persists them, and sends the treaty-request mails that became due.
pub fn run_periodic(server: &GameServer) {
    let now = Utc::now().timestamp();
    let expiries = server.systems.social.nations.expire_relations(now);
    for event in &expiries {
        notify_relation_expiry(server, event);
        let nations = &server.systems.social.nations;
        if let Some(old) = nations.take_treaty(event.nation_id, event.other_faction_id) {
            server.systems.economy.mail.delete_mail(server, old.mail_id);
        }
        // The lead mail (if any) is gone with the period; a fresh offer must wait another
        // full lead period (NATION-15).
        nations.set_offer_at(event.nation_id, event.other_faction_id, now);
        schedule_persist_offer(server, event.nation_id, event.other_faction_id, now);
    }
    if !expiries.is_empty() {
        if let Some(server) = server.self_arc() {
            tokio::spawn(async move {
                if let Err(error) = persist_expiries(&server, &expiries).await {
                    tracing::error!("Persisting nation relation expiries failed: {error}");
                }
            });
        }
    }
    let sent = send_due_treaty_mails(server, now);
    if sent > 0 {
        tracing::info!("Nation: sent {sent} treaty request mail(s)");
    }
}

/// Rebuilds the type-25 treaty-mail tracking from the mail store after a restart: entries are
/// live only when the stored mail still matches the king, the target tag and the relation's
/// current offer window. Stale mails are deleted and their send date still seeds the offer
/// cooldown (NATION-15).
pub async fn rebuild_treaty_mail_tracking(server: &GameServer) -> Result<()> {
    let mut mails: Vec<BaseMail> = server
        .systems
        .economy
        .mail
        .state()
        .all
        .iter()
        .filter(|(_, m)| m.mail_type == mail_type::NATION_REQUEST_FRIEND)
        .map(|(_, m)| m.clone())
        .collect();
    // Newest first: an old duplicate of the same (nation, target) is deleted, not tracked.
    mails.sort_by_key(|m| std::cmp::Reverse((m.body.send_date, m.id)));
    let now = Utc::now().timestamp();
    let lead = hours_to_secs(server.game_data.systems.social.nation.treaty_lead_hours);
    let nations = &server.systems.social.nations;
    let mut stale = Vec::new();
    for mail in mails {
        let Some(target_id) = u32::try_from(mail.header.extra).ok().filter(|id| *id != 0) else {
            stale.push(mail.id);
            continue;
        };
        let Some(nation) = nations.owned_by(mail.header.receiver_id) else {
            stale.push(mail.id);
            continue;
        };
        if mail.header.sender_id != 0
            || mail.header.status != mail_status::UNPAID
            || diplomacy_target(server, target_id).is_none()
        {
            stale.push(mail.id);
            continue;
        }
        let sent = mail
            .body
            .send_date
            .max(mail.body.recv_date)
            .max(mail.open_date)
            .max(0);
        if sent > 0 {
            nations.set_offer_at(nation.id, target_id, sent);
        }
        if nations.treaty(nation.id, target_id).is_some() {
            stale.push(mail.id);
            continue;
        }
        let deadline = match nations.relation_row(nation.id, target_id) {
            Some(relation)
                if relation.state == faction::relation_state::FRIENDLY
                    && relation.expire_at > now
                    && sent > 0
                    && sent >= relation.expire_at - lead =>
            {
                relation.expire_at
            }
            Some(relation)
                if relation.state == faction::relation_state::HOSTILE
                    && relation.expire_at == 0
                    && relation.next_state == faction::relation_state::HOSTILE
                    && sent > 0
                    && sent + lead > now =>
            {
                sent + lead
            }
            _ => {
                stale.push(mail.id);
                continue;
            }
        };
        nations.add_treaty(
            PendingTreatyMail {
                nation_id: nation.id,
                other_faction_id: target_id,
                mail_id: mail.id,
                deadline,
            },
            now,
        );
    }
    for id in stale {
        server.systems.economy.mail.delete_mail(server, id);
    }
    if let Some(server) = server.self_arc() {
        persist_offers(&server).await?;
    }
    Ok(())
}

/// Writes one relation row immediately after a diplomacy change (the periodic save writes it
/// again). The row must already belong to a saved nation; a nation created in the same tick is
/// written by `save_creation` first.
pub(crate) async fn persist_relation(
    server: &GameServer,
    nation_id: u32,
    other_faction_id: u32,
    state: u8,
    expire_at: i64,
    next_state: u8,
) -> Result<()> {
    let _save = server.save.transaction_lock().await;
    sqlx::query(
        "INSERT INTO nation_relations(`nation_id`,`other_faction_id`,`state`,`expire_at`,          `next_state`) VALUES (?, ?, ?, ?, ?) ON DUPLICATE KEY UPDATE `state` = ?,          `expire_at` = ?, `next_state` = ?",
    )
    .bind(nation_id)
    .bind(other_faction_id)
    .bind(state)
    .bind(expire_at)
    .bind(next_state)
    .bind(state)
    .bind(expire_at)
    .bind(next_state)
    .execute(&server.db)
    .await?;
    Ok(())
}

pub(crate) async fn persist_offer(
    server: &GameServer,
    nation_id: u32,
    other_faction_id: u32,
    at: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO nation_relation_offers(`nation_id`,`other_faction_id`,`last_offer_at`) \
         VALUES (?, ?, ?) ON DUPLICATE KEY UPDATE `last_offer_at` = \
         GREATEST(`last_offer_at`, ?)",
    )
    .bind(nation_id)
    .bind(other_faction_id)
    .bind(at)
    .bind(at)
    .execute(&server.db)
    .await?;
    Ok(())
}

pub(crate) async fn persist_offers(server: &GameServer) -> Result<()> {
    let offers = server.systems.social.nations.offers_snapshot();
    if offers.is_empty() {
        return Ok(());
    }
    let _save = server.save.transaction_lock().await;
    let mut tx = server.db.begin().await?;
    for ((nation_id, other), at) in offers {
        sqlx::query(
            "INSERT INTO nation_relation_offers(`nation_id`,`other_faction_id`,`last_offer_at`) \
             VALUES (?, ?, ?) ON DUPLICATE KEY UPDATE `last_offer_at` = \
             GREATEST(`last_offer_at`, ?)",
        )
        .bind(nation_id)
        .bind(other)
        .bind(at)
        .bind(at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn schedule_persist_offer(server: &GameServer, nation_id: u32, other_faction_id: u32, at: i64) {
    let Some(server) = server.self_arc() else {
        return;
    };
    tokio::spawn(async move {
        if let Err(error) = persist_offer(&server, nation_id, other_faction_id, at).await {
            tracing::error!(
                "Persisting nation offer {nation_id} -> {other_faction_id} failed: {error}"
            );
        }
    });
}

fn schedule_persist_relation(
    server: &GameServer,
    nation_id: u32,
    other_faction_id: u32,
    state: u8,
    expire_at: i64,
    next_state: u8,
) {
    let Some(server) = server.self_arc() else {
        return;
    };
    tokio::spawn(async move {
        if let Err(error) = persist_relation(
            &server,
            nation_id,
            other_faction_id,
            state,
            expire_at,
            next_state,
        )
        .await
        {
            tracing::error!(
                "Persisting nation relation {nation_id} -> {other_faction_id} failed: {error}"
            );
        }
    });
}

/// Writes the expired relation rows immediately (the periodic save writes them again).
pub(crate) async fn persist_expiries(
    server: &GameServer,
    expiries: &[RelationExpiry],
) -> Result<()> {
    let _save = server.save.transaction_lock().await;
    let mut tx = server.db.begin().await?;
    for event in expiries {
        sqlx::query(
            "UPDATE nation_relations SET state = ?, expire_at = 0, next_state = 0 \
             WHERE nation_id = ? AND other_faction_id = ?",
        )
        .bind(event.state)
        .bind(event.nation_id)
        .bind(event.other_faction_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Subscribes the nation periodic check to the shared `TickManager` (60 s; NATION-17) and the
/// immigration-invite expiry sweep (1 s; the invite timeout is 60 s and the list was otherwise
/// pruned only when the invitee answered).
pub fn start_tick(server: &std::sync::Arc<GameServer>) {
    let weak = std::sync::Arc::downgrade(server);
    server.systems.services.tick.on_tick.subscribe(
        move |_delta| {
            let Some(server) = weak.upgrade() else {
                return;
            };
            run_periodic(&server);
        },
        std::time::Duration::from_secs(60),
        true,
    );
    let weak = std::sync::Arc::downgrade(server);
    server.systems.services.tick.on_tick.subscribe(
        move |_delta| {
            let Some(server) = weak.upgrade() else {
                return;
            };
            sweep_expired_invites(&server, now_ms());
        },
        std::time::Duration::from_secs(1),
        false,
    );
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

/// The top-level faction of an expedition: a guild founded inside a nation resolves to that
/// nation, a guild of a race faction to its alliance. This is the client's
/// `FUN_392f8180(owner expedition)` used by the national tax / monument checks (RENAME-8/9).
pub fn expedition_top_level_faction(server: &GameServer, expedition_id: u32) -> Option<u32> {
    let mother = server
        .expeditions
        .get(expedition_id)
        .map(|e| e.faction.mother_id)
        .unwrap_or(expedition_id);
    top_level_faction(server, mother)
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
    if !server.config.player_nations_enabled() {
        send_error(
            king,
            error_message::FACTION_CANNOT_INVITE_IMMIGRATION_AS_NO_PERMISSION,
        );
        return;
    }
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
    // Close the popups of expired invites first: `add_invite` drops them silently and the
    // sweep would no longer see them (the cancel also precedes the new invitation).
    sweep_expired_invites(server, now);
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
    server
        .systems
        .social
        .nations
        .remove_offline_member(character_id);
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
    server
        .systems
        .social
        .nations
        .remove_offline_member(target_id);
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
