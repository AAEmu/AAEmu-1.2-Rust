use super::{
    broadcast_self, error, expedition_of, member_from_character, on_character_login, save,
    send_error, send_expedition_info, util, GameServer, PendingInvite, PlayerRef,
    SCExpeditionInvitationPacket, SCUnitExpeditionChangedPacket, INVITATION_TIMEOUT_MS,
};

/// `ExpeditionManager.Invite(connection, invitedName)`.
///
/// Deviation from the C#, which returned silently on every failure: the inviter is told why
/// (the 1.2 client has a text for each), and a character that is deciding on another invitation
/// cannot be invited again until it answers or the invitation expires.
pub fn invite(server: &GameServer, inviter: &PlayerRef, invited_name: &str) {
    let Some(e) = expedition_of(server, inviter) else {
        send_error(inviter, error::ONLY_EXPEDITION_MEMBER);
        return;
    };
    let Some(m) = e.member(inviter.character_id) else {
        send_error(inviter, error::ONLY_EXPEDITION_MEMBER);
        return;
    };
    match e.policy_by_role(m.role) {
        Some(p) if p.invite => {}
        Some(_) => {
            send_error(inviter, error::FACTION_INVITE_RIGHTS);
            return;
        }
        None => {
            // C#: GetPolicyByRole(..).Invite on a null policy throws.
            tracing::error!("Expedition {} has no policy for role {}", e.id(), m.role);
            return;
        }
    }
    let Some(invited) = server.world.character_by_name(invited_name) else {
        send_error(inviter, error::FACTION_INVITE_OFFLINE);
        return;
    };
    // The invited player receives nothing when the inviter is on their block list
    // (capture-pending for the missing 1.2 sender-side reply, case `BLOCK-LIST-1`). The block
    // check runs before the C# error replies, so a blocked inviter learns nothing.
    if crate::systems::social::friend::is_blocked_by(&invited, inviter.character_id) {
        return;
    }
    if invited.character_id == inviter.character_id {
        send_error(inviter, error::FACTION_INVITE_SELF);
        return;
    }
    if let Some(other) = expedition_of(server, &invited) {
        send_error(
            inviter,
            if other.id() == e.id() {
                error::FACTION_INVITE_SAME
            } else {
                error::EXPEDITION_OTHER_EXPEDITION
            },
        );
        return;
    }
    // The guild belongs to its mother's top-level faction (`Character.SetFaction` expels a
    // member of another mother), with the runtime nations overlaid: a nation citizen can
    // therefore only join a guild of that nation; the C# checked this at creation only
    // (PORTING.md, NATION-13). The saved faction is compared (NATION-14). The inviter is told
    // (deviation from the C#, see PORTING.md "Guild functions (2026-11-05)").
    let invited_faction = crate::db_character::persisted_faction(&invited.data());
    let guild_top = super::nation::top_level_faction(server, e.faction.mother_id)
        .unwrap_or_else(|| util::alliance_id(server, e.faction.mother_id));
    let invited_top = super::nation::top_level_faction(server, invited_faction)
        .unwrap_or_else(|| util::alliance_id(server, invited_faction));
    if guild_top != invited_top {
        send_error(inviter, error::EXPEDITION_BAD_FACTION);
        return;
    }
    if invited.data().unit.is_dead() {
        send_error(inviter, error::EXPEDITION_BAD_STATUS);
        return;
    }
    if server
        .systems
        .instances
        .instant_games
        .current(&invited)
        .is_some()
    {
        send_error(inviter, error::FACTION_INVITE_BATTLEFIELD);
        return;
    }
    {
        let mut invitations = server
            .expeditions
            .invitations
            .lock()
            .expect("expedition invitations poisoned");
        // Drop the invitations nobody answered in time so the map cannot grow without bound.
        let now = util::now_ms();
        invitations.retain(|_, p| p.time_ms + INVITATION_TIMEOUT_MS >= now);
        if invitations.contains_key(&invited.character_id) {
            drop(invitations);
            send_error(inviter, error::EXPEDITION_OTHER_INVITATION);
            return;
        }
        invitations.insert(
            invited.character_id,
            PendingInvite {
                expedition_id: e.id(),
                inviter_id: inviter.character_id,
                time_ms: now,
            },
        );
    }
    let inviter_name = inviter.data().unit.name.clone();
    invited.send(SCExpeditionInvitationPacket {
        invitor_id: inviter.character_id,
        invitor_name: inviter_name,
        faction_id: e.id(),
        faction_name: e.faction.name.clone(),
    });
}

/// `ExpeditionManager.ReplyInvite(connection, id1, id2, reply)`.
///
/// Deviation from the C# (see PORTING.md): only a pending, unexpired invitation to this very
/// guild can be accepted, by a character that is not in a guild yet; the character is told why
/// an acceptance fails and the inviter is told about a refusal.
pub async fn reply_invite(
    server: &GameServer,
    invited: &PlayerRef,
    id1: u32,
    _id2: u32,
    reply: bool,
) {
    // The invitation is consumed by any answer.
    let pending = server
        .expeditions
        .invitations
        .lock()
        .expect("expedition invitations poisoned")
        .remove(&invited.character_id);
    if !reply {
        if let Some(inviter) = pending.and_then(|p| server.world.character_by_id(p.inviter_id)) {
            send_error(&inviter, error::FACTION_INVITE_REJECTED);
        }
        return;
    }
    let refuse = |why: &str, error: i16| {
        tracing::warn!(
            "ReplyExpeditionInvitation: rejected character {} for expedition {id1} ({why}; pending {:?}, in expedition {})",
            invited.character_id,
            pending.map(|p| p.expedition_id),
            invited.data().unit.expedition_id
        );
        send_error(invited, error);
    };
    match pending {
        Some(p) if p.expedition_id == id1 => {
            if p.time_ms + INVITATION_TIMEOUT_MS < util::now_ms() {
                refuse("expired", error::INVITATION_CANCELED_TIMEOUT);
                return;
            }
        }
        _ => {
            refuse("no such invitation", error::FACTION_JOIN_NO_INVITOR);
            return;
        }
    }
    if invited.data().unit.expedition_id != 0 {
        refuse("already in a guild", error::EXPEDITION_ALREADY_MEMBER);
        return;
    }
    // See `invite`: the guild mother's top-level faction must contain the applicant
    // (PORTING.md, NATION-13).
    let guild_mother = server.expeditions.get(id1).map(|e| e.faction.mother_id);
    if super::nation::top_level_faction(server, guild_mother.unwrap_or(0))
        != super::nation::top_level_faction(
            server,
            crate::db_character::persisted_faction(&invited.data()),
        )
    {
        tracing::warn!(
            "ReplyExpeditionInvitation: character {} may not join expedition {id1} of another top-level faction",
            invited.character_id
        );
        return;
    }
    let member = member_from_character(server, id1, invited, false);
    // C#: `_expeditions[id1]` throws KeyNotFoundException; a disbanded guild stays in the list
    // (see `disband`) but cannot be joined.
    let Some(snapshot) = server
        .expeditions
        .with_mut(id1, |e| {
            if e.is_disbanded {
                return None;
            }
            e.members.push(member);
            Some(e.clone())
        })
        .flatten()
    else {
        refuse("no such guild", error::FACTION_JOIN);
        return;
    };
    invited.data().unit.expedition_id = id1;
    crate::systems::refresh_achievements_later(server, invited);
    let unit_name = invited.data().unit.name.clone();
    broadcast_self(
        server,
        invited,
        &SCUnitExpeditionChangedPacket {
            obj_id: invited.obj_id,
            character_id: invited.character_id,
            kicker: String::new(),
            unit_name,
            id: 0,
            expedition_id: id1,
            expel: false,
        },
    );
    send_expedition_info(invited, &snapshot);
    on_character_login(server, invited, id1);
    save(server, id1).await;
}
