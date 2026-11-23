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

use super::{dominion, friend, packets, rename};

mod diplomacy;
mod immigration;
mod independence;
mod monument;
mod naming;

pub use self::diplomacy::{
    declare_hostile, pay_friend_treaty, rebuild_treaty_mail_tracking, run_periodic,
    send_due_treaty_mails, send_treaty_mail,
};
#[cfg(test)]
pub(crate) use self::diplomacy::{persist_expiries, persist_offer, persist_relation};
pub use self::immigration::{
    expedition_top_level_faction, immigrate_to_origin, invite_citizen, kick_to_origin,
    persist_faction, reply_invite, sweep_expired_invites, top_level_faction,
};
#[cfg(test)]
pub(super) use self::immigration::{kick_offline, OfflineKick};
use self::independence::write_nation;
pub use self::independence::{
    declare_independence, declare_independence_effect, save_creation, template_relations,
};
pub use self::monument::{
    place_national_monument, remove_national_monument, NATIONAL_MONUMENT_RANGE,
};
pub use self::naming::{offer_nation_rename_ui, rename_nation};

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
