//! Rename packets for the guild (`expedition`) and, later, the nation path.
//!
//! The 1.2 client opens a rename dialog only when the server sends
//! `SCExpeditionShowRenameUI` (`FUN_391d2310` -> `FUN_392f8db0`); its confirm button sends
//! `CSRenameExpeditionPacket` through `X2Faction.RenameExpedition` (`FUN_39472d70` ->
//! `FUN_392f8f50`, which sets the `is_expedition` byte). `SCFactionRenamedPacket` updates the
//! cached faction name (`FUN_392fdce0`) and `SCFactionRetryRenamePacket` re-opens the dialog
//! pre-filled with the rejected name (`FUN_391e5520`, `xpdt` != 0 = expedition).
//!
//! The name rules: guild create / rename use `expedition::validate_name` (shape + block list +
//! guild uniqueness). The shape-only part (`expedition::validate_name_shape`) is shared with
//! independence and nation rename, which additionally use the cross-store
//! [`faction_name_in_use`] namespace check (RENAME-7).

use std::sync::PoisonError;

use crate::server::GameServer;
use crate::world::PlayerRef;

use super::packets::expedition::SCExpeditionShowRenameUIPacket;
use super::packets::faction::{
    SCFactionRenamedPacket, SCFactionRetryRenamePacket, SCNationShowRenameUIPacket,
};

/// Validates a faction (expedition / nation) name with the shape rules of the expedition
/// creation path (at most 32 UTF-16 units, then the configured `Expedition.NameRegex`).
/// Independence and nation rename use this; guild create / rename use the fuller
/// `expedition::validate_name` (shape + block list + guild uniqueness).
pub fn validate_faction_name(server: &GameServer, name: &str) -> Result<(), i16> {
    super::expedition::validate_name_shape(server, name)
}

/// Which faction currently holds a name, so a rename can exclude itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameOwner {
    Expedition(u32),
    Nation(u32),
}

/// The exclusive guard for every faction-name change (guild create, guild rename,
/// independence, nation rename). The expedition and nation stores have separate locks, so
/// this serialises the "is the name free" check with the write across both and removes the
/// create-vs-rename race (RENAME-6, RENAME-3).
pub fn lock_names(server: &GameServer) -> std::sync::MutexGuard<'_, ()> {
    server
        .systems
        .social
        .name_lock
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Whether `name` is already used by a static faction, an expedition or a nation. The
/// comparison is exact, like `ExpeditionManager.CreateExpedition` (`name.Equals`; RENAME-4).
///
/// `exclude` names the faction being renamed: a guild may keep its current name, and a new
/// nation may carry the founding expedition's name (NATION-1). Call it under [`lock_names`]
/// when the result guards a write.
pub fn faction_name_in_use(server: &GameServer, name: &str, exclude: Option<NameOwner>) -> bool {
    server
        .game_data
        .factions
        .factions
        .iter()
        .any(|f| f.name == name)
        || server.expeditions.all().iter().any(|e| {
            e.faction.name == name
                && !matches!(exclude, Some(NameOwner::Expedition(id)) if id == e.faction.id)
        })
        || server.systems.social.nations.all().iter().any(|n| {
            n.name == name && !matches!(exclude, Some(NameOwner::Nation(id)) if id == n.id)
        })
}

/// `SCExpeditionShowRenameUIPacket` (empty body): asks the client to open the guild rename
/// dialog, pre-filled with the current guild name.
pub fn send_expedition_rename_ui(p: &PlayerRef) {
    p.send(SCExpeditionShowRenameUIPacket {});
}

/// `SCNationShowRenameUIPacket` (0x010, empty body): the 1.2 client's `FUN_391d2380` calls
/// `FUN_392f8e80(0)`, which opens the nation rename dialog only when the player owns a user
/// faction. Only the server can open it; the production trigger is capture-pending (RENAME-7).
pub fn send_nation_rename_ui(p: &PlayerRef) {
    p.send(SCNationShowRenameUIPacket {});
}

/// `SCFactionRenamedPacket` to every online player: the guild list (`SCFactionListPacket`) is
/// server-wide, so every client's cached faction name has to change, not only the members'.
pub fn broadcast_renamed(server: &GameServer, id: u32, name: &str, by_gm: bool) {
    server.world.broadcast_to_server(&SCFactionRenamedPacket {
        id,
        name: name.to_string(),
        by_gm,
    });
}

/// `SCFactionRetryRenamePacket`: re-opens the rename dialog with `name` pre-filled. `xpdt` is
/// the packet's expedition flag (the 1.2 client treats non-zero as expedition, zero as nation).
pub fn send_retry(p: &PlayerRef, is_expedition: bool, error_message: i16, name: &str) {
    p.send(SCFactionRetryRenamePacket {
        xpdt: is_expedition,
        error_message,
        name: name.to_string(),
    });
}
