use sqlx::Row;

use super::{
    error_message, faction, faction_of_id, invitation_canceled_kind, now_ms, send_error,
    GameServer, PendingInvite, PlayerRef, SCFactionImmigrateInvitePacket,
    SCFactionImmigrateInviteResultPacket, SCFactionImmigrateToOriginResultPacket,
    SCFactionKickToOriginResultPacket, SCInvitationCanceledPacket, IMMIGRATE_MIN_LEVEL,
    INVITE_TIMEOUT_MS,
};

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
pub(in crate::systems::social) enum OfflineKick {
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
pub(in crate::systems::social) async fn kick_offline(
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
