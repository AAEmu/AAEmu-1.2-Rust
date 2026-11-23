use super::{error_message, send_error, GameServer, NationRenameFailure, PlayerRef};

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
