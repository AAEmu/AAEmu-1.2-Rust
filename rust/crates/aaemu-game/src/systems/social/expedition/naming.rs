use super::{
    error, expedition_of, is_deleted, save, send_error, validate_name, GameServer, PlayerRef,
    SCFactionListPacket,
};
use crate::systems::social::rename;

/// Asks the server to offer the guild owner a rename: grants the one-shot
/// `allow_change_name` right (the same right `/allowguildrename` grants), sends
/// `SCExpeditionShowRenameUI` (empty body) and records the one-shot offer that `rename`
/// requires. Used by the GM test hook; a future item / merge trigger calls the same function.
pub fn offer_expedition_rename_ui(
    server: &GameServer,
    p: &PlayerRef,
) -> std::result::Result<(), i16> {
    let id = p.data().unit.expedition_id;
    let faction = server.expeditions.with_mut(id, |e| {
        if is_deleted(e) {
            return Err(error::ONLY_EXPEDITION_MEMBER);
        }
        if e.faction.owner_id != p.character_id {
            return Err(error::ONLY_EXPEDITION_OWNER);
        }
        e.faction.allow_change_name = 1;
        Ok(e.faction.clone())
    });
    let Some(faction) = faction else {
        return Err(error::ONLY_EXPEDITION_MEMBER);
    };
    let faction = faction?;
    // The client keeps the `allow_change_name` flag with its faction record.
    server.world.broadcast_to_server(&SCFactionListPacket {
        factions: vec![faction],
    });
    server.expeditions.offer_rename(p.character_id, id);
    rename::send_expedition_rename_ui(p);
    Ok(())
}

/// The guild leader's rename dialog (`SCExpeditionShowRenameUI`) when the guild may be renamed
/// and the leader is online: after a GM granted the right, and on every login while it is
/// pending.
pub fn prompt_rename_if_allowed(server: &GameServer, p: &PlayerRef) {
    let id = p.data().unit.expedition_id;
    if id == 0 {
        return;
    }
    let Some(e) = server.expeditions.get(id) else {
        return;
    };
    if e.faction.allow_change_name != 0 && e.faction.owner_id == p.character_id && !is_deleted(&e) {
        server.expeditions.offer_rename(p.character_id, id);
        rename::send_expedition_rename_ui(p);
    }
}

/// What one guild rename write did under the shared faction-name lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenameWrite {
    Done,
    /// An expedition, nation or static faction already carries the name (RENAME-3).
    NameTaken,
    /// The guild disappeared between the checks and the write.
    Missing,
}

/// Renames the guild `id` in the manager, the guild chat channel and for every client
/// (`SCFactionRenamed`), and saves it. `by_gm` is the flag the client uses to keep its
/// "may rename" right (a GM rename does not spend the leader's right); otherwise the right is
/// spent. The name must have been validated; the cross-store namespace check and the name
/// write share `rename::lock_names` with independence and nation rename.
async fn apply_rename(server: &GameServer, id: u32, name: &str, by_gm: bool) -> RenameWrite {
    let outcome = {
        // Guilds, nations and static factions share one namespace; the lock keeps the check
        // and the write together (RENAME-3 / RENAME-6).
        let _names = rename::lock_names(server);
        if rename::faction_name_in_use(server, name, Some(rename::NameOwner::Expedition(id))) {
            RenameWrite::NameTaken
        } else {
            match server.expeditions.with_mut(id, |e| {
                e.faction.name = name.to_string();
                if !by_gm {
                    e.faction.allow_change_name = 0;
                }
            }) {
                Some(()) => RenameWrite::Done,
                None => RenameWrite::Missing,
            }
        }
    };
    if outcome != RenameWrite::Done {
        return outcome;
    }
    server.chat.rename_guild_chat(id, name);
    rename::broadcast_renamed(server, id, name, by_gm);
    save(server, id).await;
    outcome
}

/// `CSRenameExpedition(id, name, isExpedition)`: the guild leader spends a granted rename right
/// (`expeditions.allow_change_name`, set by the GM command `/allowguildrename`).
///
/// Not in the C# (it logged the packet and never sent the dialog). Not a member `ONLY_EXPEDITION_
/// MEMBER`, not the leader of the named guild `ONLY_EXPEDITION_OWNER`, no pending right (or the
/// nation variant, which has its own rename stage) `FACTION_RIGHTS`; a granted right without the
/// dialog offer that `SCExpeditionShowRenameUI` created (a forged packet) `EXPEDITION_BAD_STATUS`
/// (307, our server-side guard: only the dialog can produce this request); a name that breaks
/// the create rules reopens the dialog through `SCFactionRetryRename` and keeps the offer.
pub async fn rename(server: &GameServer, p: &PlayerRef, id: u32, name: &str, is_expedition: bool) {
    if !is_expedition {
        send_error(p, error::FACTION_RIGHTS);
        return;
    }
    let Some(e) = expedition_of(server, p) else {
        send_error(p, error::ONLY_EXPEDITION_MEMBER);
        return;
    };
    if e.id() != id || e.faction.owner_id != p.character_id {
        send_error(p, error::ONLY_EXPEDITION_OWNER);
        return;
    }
    if e.faction.allow_change_name == 0 {
        send_error(p, error::FACTION_RIGHTS);
        return;
    }
    if !server.expeditions.has_rename_offer(p.character_id, id) {
        tracing::warn!(
            "RenameExpedition {id} by {} without a ShowRenameUI offer refused",
            p.character_id
        );
        send_error(p, error::EXPEDITION_BAD_STATUS);
        return;
    }
    if let Err(code) = validate_name(server, name, id) {
        // The client re-opens the dialog with the rejected name; the offer stays usable.
        rename::send_retry(p, true, code, name);
        return;
    }
    match apply_rename(server, id, name, false).await {
        RenameWrite::Done => server.expeditions.take_rename_offer(p.character_id),
        RenameWrite::NameTaken => {
            rename::send_retry(p, true, error::EXPEDITION_NAME_EXIST, name);
        }
        RenameWrite::Missing => send_error(p, error::EXPEDITION_BAD_STATUS),
    }
}

/// GM: gives the guild `id` the right to rename itself once; its leader gets the dialog now
/// when online. Returns the guild name, or `None` for an unknown / dismissed guild.
pub async fn grant_rename(server: &GameServer, id: u32) -> Option<String> {
    let name = server.expeditions.with_mut(id, |e| {
        if is_deleted(e) {
            return None;
        }
        e.faction.allow_change_name = 1;
        Some((
            e.faction.name.clone(),
            e.faction.owner_id,
            e.faction.clone(),
        ))
    })??;
    // The client keeps the flag with its faction record.
    server.world.broadcast_to_server(&SCFactionListPacket {
        factions: vec![name.2],
    });
    if let Some(owner) = server.world.character_by_id(name.1) {
        if owner.data().unit.expedition_id == id {
            // The dialog is the offer `rename` checks for.
            server.expeditions.offer_rename(name.1, id);
            rename::send_expedition_rename_ui(&owner);
        }
    }
    save(server, id).await;
    Some(name.0)
}

/// GM: renames the guild `id` directly (`SCFactionRenamed` with `by_gm`), applying the create
/// rules. Returns the `ErrorMessageType` of a refused name.
pub async fn gm_rename(server: &GameServer, id: u32, name: &str) -> std::result::Result<(), i16> {
    match server.expeditions.get(id) {
        Some(e) if !is_deleted(&e) => {}
        _ => return Err(error::EXPEDITION_NO_TARGET),
    }
    validate_name(server, name, id)?;
    match apply_rename(server, id, name, true).await {
        RenameWrite::Done => Ok(()),
        RenameWrite::NameTaken => Err(error::EXPEDITION_NAME_EXIST),
        RenameWrite::Missing => Err(error::EXPEDITION_NO_TARGET),
    }
}
