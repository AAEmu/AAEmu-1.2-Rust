//! Shared faction-name rules and rename packets for the guild (`expedition`) and, later, the
//! nation path.
//!
//! The 1.2 client opens a rename dialog only when the server sends
//! `SCExpeditionShowRenameUI` (`FUN_391d2310` -> `FUN_392f8db0`); its confirm button sends
//! `CSRenameExpeditionPacket` through `X2Faction.RenameExpedition` (`FUN_39472d70` ->
//! `FUN_392f8f50`, which sets the `is_expedition` byte). `SCFactionRenamedPacket` updates the
//! cached faction name (`FUN_392fdce0`) and `SCFactionRetryRenamePacket` re-opens the dialog
//! pre-filled with the rejected name (`FUN_391e5520`, `xpdt` != 0 = expedition).
//!
//! Validation mirrors the expedition creation path: the same regex and the same 32-UTF-16-unit
//! limit, with the same `ErrorMessageType` codes (296 / 297), so the create and rename dialogs
//! reject the same names.

use crate::server::GameServer;
use crate::world::PlayerRef;

use super::packets::expedition::SCExpeditionShowRenameUIPacket;
use super::packets::faction::{SCFactionRenamedPacket, SCFactionRetryRenamePacket};
use super::util::error;

/// Validates a faction (expedition / nation) name with the expedition creation rules
/// (`ExpeditionManager.CreateExpedition`): at most 32 UTF-16 units, then the configured
/// `Expedition.NameRegex`. Returns the `ErrorMessageType` to report.
pub fn validate_faction_name(server: &GameServer, name: &str) -> Result<(), i16> {
    if name.encode_utf16().count() > 32 {
        return Err(error::EXPEDITION_NAME_LENGTH);
    }
    let matches = match regex::Regex::new(&server.config.expedition.name_regex) {
        Ok(r) => r.is_match(name),
        Err(e) => {
            tracing::error!("Expedition.NameRegex: {e}");
            false
        }
    };
    if !matches {
        return Err(error::EXPEDITION_NAME_CHARACTER);
    }
    Ok(())
}

/// `SCExpeditionShowRenameUIPacket` (empty body): asks the client to open the guild rename
/// dialog, pre-filled with the current guild name.
pub fn send_expedition_rename_ui(p: &PlayerRef) {
    p.send(SCExpeditionShowRenameUIPacket {});
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
