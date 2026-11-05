//! GM character rename and `SCCharacterRenamedPacket` (WP-AC2).
//!
//! No 1.2 client packet starts a character rename: the only client rename packet is
//! `CSRenameExpeditionPacket` (guild / nation), and the 1.2 lobby has no force-rename UI. The
//! trigger is therefore server side (the `/rename` GM command).
//!
//! `SCCharacterRenamedPacket` 0x05a is `{ u32 charId, string oldName, string newName }`
//! (client serializer `FUN_397b7820`). The handler `FUN_391df6e0` updates the unit-name cache
//! (`FUN_393c8520`), the chat / friend caches (`FUN_393de080`) and the expedition / family
//! member names (`FUN_392fb550`). The recipient set (viewers, friends, guild and family) is
//! INFERRED: the C# never sent the packet, and the 1.2 client handler has no sender to copy.
//!
//! The rename check, the `characters.name` write and the name-cache swap share one critical
//! section with character create / edit (`SaveManager::transaction_lock`), so a concurrent
//! create cannot claim the new name between the check and the write and the name cache never
//! holds neither or both names for the character.
//!
//! Persisted copies of a character name (2026-11-06 database):
//! * `characters.name` - the authoritative copy; updated here.
//! * `mails.sender_name` / `mails.receiver_name` - read back and shown in the mail list;
//!   updated here.
//! * `expedition_members.name`, `expeditions.owner_name`, `family_members.name` - loaded at
//!   server start and shown in the guild / family lists; updated here (the in-memory copies
//!   too).
//! * `nations.owner_name` - loaded at server start, held in `NationManager` and saved from it;
//!   updated here in the database and in memory.
//! * `shipyards.owner_name` - loaded at server start and used to resolve the owner by name
//!   (`server.world.character_by_name`) when the build completes / decays; updated here, in
//!   the database and in the live shipyard (its unit name too).
//! * `auction_house.client_name` / `bidder_name` - loaded into `AuctionManager` and saved from
//!   its lots; updated here in the database and in memory.
//! * `character_rank_scores.account` - the character name the client shows where the login
//!   account is unknown; loaded into `RankingManager` and saved from it; updated here in the
//!   database and in memory.
//! * `housings` stores only `owner`; the house owner name is resolved from the name cache
//!   (`housing/ops.rs`), so no row changes. `friends` / `blocked` store ids only. `mates.name`
//!   and `slaves.name` are pet / vehicle names, not character names.
//!
//! The in-memory copies (mail list, nations, auction lots, ranking scores, guild / family /
//! shipyard caches) are updated in [`apply_renamed`] before [`rename_character`] releases the
//! save transaction lock, so no save can capture the old names between the database commit and
//! the memory swap.

use std::collections::HashSet;

use sqlx::{MySql, Row, Transaction};

use crate::managers::name::{normalize_name, CharacterCreateError};
use crate::server::GameServer;
use crate::systems::social::packets::misc::SCCharacterRenamedPacket;
use crate::world::{PlayerRef, WorldObject};

/// Why [`rename_character`] refused a rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameError {
    /// No character with that id is registered in the name cache.
    Missing,
    /// The character row is deleted or its deletion is pending.
    Deleted,
    /// The requested name is the current name (case-insensitive).
    SameName,
    /// Another character already holds the name.
    NameTaken,
    /// The name fails the regex / blocked-name rules.
    InvalidName,
    /// The database could not read or write the rename.
    Database(String),
}

impl std::fmt::Display for RenameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("character not found"),
            Self::Deleted => f.write_str("the character is deleted or being deleted"),
            Self::SameName => f.write_str("the new name is the same as the old name"),
            Self::NameTaken => f.write_str("the name is already in use"),
            Self::InvalidName => f.write_str("the name contains invalid characters"),
            Self::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

fn db_error(e: sqlx::Error) -> RenameError {
    RenameError::Database(e.to_string())
}

/// Validates `new_name` against the current name of `character_id` and the shared name rules.
/// Checks the same-name refusal first, so it is reported as such and not as a duplicate.
fn validate_new_name(
    server: &GameServer,
    old_name: &str,
    new_name: &str,
) -> Result<String, RenameError> {
    let normalized = normalize_name(new_name);
    if normalized.to_lowercase() == old_name.to_lowercase() {
        return Err(RenameError::SameName);
    }
    let code = server.names.validate_character_name_with_blocks(
        &normalized,
        &server.game_data.characters.blocked_names,
        || false,
    );
    match code {
        CharacterCreateError::Ok => Ok(normalized),
        CharacterCreateError::NameAlreadyExists => Err(RenameError::NameTaken),
        _ => Err(RenameError::InvalidName),
    }
}

/// Renames character `character_id` to `new_name` and returns the old display name.
///
/// Works online and offline. The name check, the `characters.name` write and the name-cache
/// swap run under `SaveManager::transaction_lock`, the same critical section character create
/// and the lobby edit use.
pub async fn rename_character(
    server: &GameServer,
    character_id: u32,
    new_name: &str,
) -> Result<String, RenameError> {
    // Refuse malformed / taken names before touching the database. The command path relies on
    // this for bad input; the same checks run again under the lock below.
    let map_old = server
        .names
        .get_character_name(character_id)
        .ok_or(RenameError::Missing)?;
    validate_new_name(server, &map_old, new_name)?;

    let _transaction = server.save.transaction_lock().await;
    // Re-read under the lock: the character may have been renamed or removed while waiting.
    let map_old = server
        .names
        .get_character_name(character_id)
        .ok_or(RenameError::Missing)?;
    let normalized = validate_new_name(server, &map_old, new_name)?;

    let row = sqlx::query(
        "SELECT name, CAST(deleted AS SIGNED) AS deleted, \
         CAST(UNIX_TIMESTAMP(delete_request_time) AS SIGNED) AS delete_request_time, \
         CAST(UNIX_TIMESTAMP(delete_time) AS SIGNED) AS delete_time \
         FROM characters WHERE id = ?",
    )
    .bind(character_id)
    .fetch_optional(&server.db)
    .await
    .map_err(db_error)?;
    let Some(row) = row else {
        return Err(RenameError::Missing);
    };
    let db_name: String = row.try_get("name").map_err(db_error)?;
    let deleted: i64 = row.try_get("deleted").map_err(db_error)?;
    let delete_request_time: i64 = row.try_get("delete_request_time").map_err(db_error)?;
    let delete_time: i64 = row.try_get("delete_time").map_err(db_error)?;
    if deleted != 0 || delete_request_time != 0 || delete_time != 0 {
        return Err(RenameError::Deleted);
    }
    // The persisted row is the display name; it can differ from the cache in case.
    if db_name.to_lowercase() == normalized.to_lowercase() {
        return Err(RenameError::SameName);
    }

    let mut tx = server.db.begin().await.map_err(db_error)?;
    let affected = sqlx::query("UPDATE characters SET name = ? WHERE id = ? AND deleted = 0")
        .bind(&normalized)
        .bind(character_id)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    if affected.rows_affected() != 1 {
        let _ = tx.rollback().await;
        return Err(RenameError::Deleted);
    }
    update_name_copies(&mut tx, character_id, &normalized).await?;
    tx.commit().await.map_err(db_error)?;

    apply_renamed(server, character_id, &db_name, &normalized);
    Ok(db_name)
}

/// Updates every persisted character-name copy the server reads back (see the module docs).
async fn update_name_copies(
    tx: &mut Transaction<'_, MySql>,
    character_id: u32,
    new_name: &str,
) -> Result<(), RenameError> {
    for sql in [
        "UPDATE expedition_members SET name = ? WHERE character_id = ?",
        "UPDATE expeditions SET owner_name = ? WHERE owner = ?",
        "UPDATE family_members SET name = ? WHERE character_id = ?",
        "UPDATE nations SET owner_name = ? WHERE owner_character_id = ?",
        "UPDATE shipyards SET owner_name = ? WHERE owner_id = ?",
        "UPDATE mails SET sender_name = ? WHERE sender_id = ?",
        "UPDATE mails SET receiver_name = ? WHERE receiver_id = ?",
        "UPDATE character_rank_scores SET account = ? WHERE character_id = ?",
    ] {
        sqlx::query(sql)
            .bind(new_name)
            .bind(character_id)
            .execute(&mut **tx)
            .await
            .map_err(db_error)?;
    }
    sqlx::query("UPDATE auction_house SET client_name = ? WHERE client_id = ?")
        .bind(new_name)
        .bind(character_id)
        .execute(&mut **tx)
        .await
        .map_err(db_error)?;
    sqlx::query("UPDATE auction_house SET bidder_name = ? WHERE bidder_id = ?")
        .bind(new_name)
        .bind(character_id)
        .execute(&mut **tx)
        .await
        .map_err(db_error)?;
    Ok(())
}

/// The post-commit side effects of a rename: swaps the name cache, updates the online unit and
/// the in-memory guild / family / shipyard name copies, then tells every recipient.
///
/// `old_name` is the persisted old display name; an online character's live unit name wins (it
/// is what the viewers know).
pub fn apply_renamed(server: &GameServer, character_id: u32, old_name: &str, new_name: &str) {
    if !server.names.rename_character(character_id, new_name) {
        tracing::error!(
            "Character {character_id} renamed in the database but the name cache refused {new_name:?}"
        );
        return;
    }
    let online = server.world.character_by_id(character_id);
    let old_name = match &online {
        Some(p) => {
            let c = p.data();
            c.unit.name.clone()
        }
        None => old_name.to_string(),
    };
    if let Some(p) = &online {
        p.conn.observe_character_name(new_name);
        let mut c = p.data();
        c.unit.name = new_name.to_string();
    }
    // `rename_character` calls this while it still owns `SaveManager::transaction_lock`, so the
    // managers' next save cannot snapshot the old names between the commit and these swaps.
    server
        .systems
        .economy
        .mail
        .rename_character(character_id, new_name);
    server
        .systems
        .social
        .nations
        .rename_character(character_id, new_name);
    server
        .systems
        .economy
        .auction
        .rename_character(character_id, new_name);
    server
        .systems
        .social
        .ranking
        .rename_character(character_id, new_name);
    server
        .systems
        .social
        .trials
        .rename_character(character_id, new_name);
    server
        .systems
        .social
        .crimes
        .rename_character(character_id, new_name);
    update_expedition_names(server, character_id, new_name);
    server
        .systems
        .social
        .families
        .rename_member(character_id, new_name);
    update_shipyard_names(server, character_id, new_name);
    broadcast_renamed(server, character_id, &old_name, new_name);
}

fn update_expedition_names(server: &GameServer, character_id: u32, new_name: &str) {
    let expedition_id = server.expeditions.expedition_of_character(character_id);
    if expedition_id == 0 {
        return;
    }
    server.expeditions.with_mut(expedition_id, |e| {
        if e.faction.owner_id == character_id {
            e.faction.owner_name = new_name.to_string();
        }
        if let Some(m) = e
            .members
            .iter_mut()
            .find(|m| m.character_id == character_id)
        {
            m.name = new_name.to_string();
        }
    });
}

fn update_shipyard_names(server: &GameServer, character_id: u32, new_name: &str) {
    for shipyard in server.systems.vehicles.shipyards.all() {
        let is_owner = {
            let s = shipyard.data();
            s.unit.owner_id == character_id
        };
        if !is_owner {
            continue;
        }
        let mut s = shipyard.data();
        s.data.owner_name = new_name.to_string();
        s.unit.name = new_name.to_string();
    }
}

/// Sends `SCCharacterRenamedPacket` to every viewer of the character, its friends, guild and
/// family members, and itself. One packet per recipient (the sets overlap).
pub fn broadcast_renamed(server: &GameServer, character_id: u32, old_name: &str, new_name: &str) {
    let mut recipients: HashSet<u32> = HashSet::new();
    if let Some(p) = online_player(server, character_id) {
        recipients.insert(character_id);
        if let Some(world) = world_of(server, &p) {
            for viewer in world.players_around(WorldObject::from(&p)) {
                recipients.insert(viewer.character_id);
            }
        }
    }
    recipients.extend(server.friends.owners_of(character_id));
    let expedition_id = server.expeditions.expedition_of_character(character_id);
    if expedition_id != 0 {
        if let Some(e) = server.expeditions.get(expedition_id) {
            recipients.extend(e.member_ids());
        }
    }
    let family_id = server
        .systems
        .social
        .families
        .family_of_character(character_id);
    if family_id != 0 {
        if let Some(family) = server.systems.social.families.family(family_id) {
            recipients.extend(family.members.iter().map(|m| m.id));
        }
    }
    let packet = SCCharacterRenamedPacket {
        character_id,
        old_name: old_name.to_string(),
        new_name: new_name.to_string(),
    };
    for id in recipients {
        if let Some(p) = server.world.character_by_id(id) {
            p.send(packet.clone());
        }
    }
}

fn online_player(server: &GameServer, character_id: u32) -> Option<PlayerRef> {
    server.world.character_by_id(character_id)
}

fn world_of(
    server: &GameServer,
    p: &PlayerRef,
) -> Option<std::sync::Arc<crate::world::WorldInstance>> {
    let obj = WorldObject::from(p);
    server.world.world_of(&obj)
}

#[cfg(test)]
#[path = "character_rename_tests.rs"]
mod character_rename_tests;
