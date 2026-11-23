use super::{
    broadcast_self, dismiss_expedition, error, expedition_of, forced_owner_leave, guild_chat, save,
    save_expedition_state, send_error, send_to_members, transfer_ownership, ExpeditionMember,
    ExpeditionRolePolicy, GameServer, PlayerRef, SCExpeditionRoleChangedPacket,
    SCExpeditionRolePolicyChangedPacket, SCUnitExpeditionChangedPacket, LEADER_ROLE,
};

/// `Expedition.RemoveMember(member)`: leaves the guild chat (when online) and records the id.
pub(super) fn remove_member(server: &GameServer, expedition_id: u32, character_id: u32) {
    let name = server
        .expeditions
        .with_mut(expedition_id, |e| {
            e.remove_member(character_id);
            e.faction.name.clone()
        })
        .unwrap_or_default();
    if let Some(c) = server.world.character_by_id(character_id) {
        guild_chat(server, expedition_id, &name).leave(&c);
    }
}

/// `ExpeditionManager.ChangeExpeditionRolePolicy(connection, policy)`.
///
/// Deviation from the C# (see PORTING.md "Guild functions (2026-11-05)"): only the leader (role
/// 255) edits policies, and only those of the roles below it (the C# let anybody with the Expel
/// right edit any role, even its own), else `FACTION_CHANGE_MEMBER_ROLE_RIGHTS`; a role name the
/// guild name rules refuse is `EXPEDITION_NAME_CHARACTER`. The 1.2 dialog edits the role name
/// and the invite / expel / promote / siege-join flags only, so the other flags keep their
/// stored value, and the stored policy (not the packet) is what every member is sent.
/// Capture-pending: no 1.2 server-side oracle for the rules themselves (the client UI is the
/// evidence).
pub async fn change_role_policy(server: &GameServer, p: &PlayerRef, policy: ExpeditionRolePolicy) {
    let Some(e) = expedition_of(server, p) else {
        send_error(p, error::ONLY_EXPEDITION_MEMBER);
        return;
    };
    let id = e.id();
    let Some(member) = e.member(p.character_id) else {
        send_error(p, error::ONLY_EXPEDITION_MEMBER);
        return;
    };
    if policy.expedition_id != id
        || member.role != LEADER_ROLE
        || policy.role == LEADER_ROLE
        || e.policy_by_role(policy.role).is_none()
    {
        send_error(p, error::FACTION_CHANGE_MEMBER_ROLE_RIGHTS);
        return;
    }
    let name_ok = policy.name.encode_utf16().count() <= 32
        && regex::Regex::new(&server.config.expedition.name_regex)
            .map(|r| r.is_match(&policy.name))
            .unwrap_or(false);
    if !name_ok {
        send_error(p, error::EXPEDITION_NAME_CHARACTER);
        return;
    }
    let updated = server.expeditions.with_mut(id, |e| {
        let members = e.member_ids();
        e.policies
            .iter_mut()
            .find(|x| x.role == policy.role)
            .map(|cur| {
                cur.name = policy.name.clone();
                cur.invite = policy.invite;
                cur.join_siege = policy.join_siege;
                cur.promote = policy.promote;
                cur.expel = policy.expel;
                (cur.clone(), members)
            })
    });
    let Some(Some((stored, members))) = updated else {
        return;
    };
    send_to_members(
        server,
        &members,
        &SCExpeditionRolePolicyChangedPacket {
            policy: stored,
            success: true,
        },
    );
    save(server, id).await;
}

/// `ExpeditionManager.Leave(character)`.
///
/// Deviation from the C#, which let the leader leave and so left the guild without one: the
/// leader must hand the guild over (`ChangeOwner`) or dismiss it first, as the 1.2 client's
/// `EXPEDITION_OWNER_CANNOT_LEAVE` text ("you can leave after delegating the leader position")
/// says. Failures are reported to the character.
pub async fn leave(server: &GameServer, p: &PlayerRef) {
    let Some(e) = expedition_of(server, p) else {
        send_error(p, error::ONLY_EXPEDITION_MEMBER);
        return;
    };
    let id = e.id();
    let Some(me) = e.member(p.character_id) else {
        // C#: RemoveMember(null) throws a NullReferenceException.
        tracing::error!(
            "Leave: {} is not a member of expedition {id}",
            p.character_id
        );
        return;
    };
    if me.role == LEADER_ROLE || e.faction.owner_id == p.character_id {
        send_error(p, error::EXPEDITION_OWNER_CANNOT_LEAVE);
        return;
    }
    remove_member_and_announce(server, p, id).await;
}

/// The member `p` leaves the guild `id` of its own accord (or is made to): `SCUnitExpeditionChanged`
/// with `expel = false` to the character's surroundings and the remaining members, then the save.
async fn remove_member_and_announce(server: &GameServer, p: &PlayerRef, id: u32) {
    remove_member(server, id, p.character_id);
    let unit_name = p.data().unit.name.clone();
    let packet = SCUnitExpeditionChangedPacket {
        obj_id: p.obj_id,
        character_id: p.character_id,
        kicker: String::new(),
        unit_name,
        id,
        expedition_id: 0,
        expel: false,
    };
    p.data().unit.expedition_id = 0;
    broadcast_self(server, p, &packet);
    let members = server
        .expeditions
        .get(id)
        .map(|e| e.member_ids())
        .unwrap_or_default();
    send_to_members(server, &members, &packet);
    save(server, id).await;
}

/// A character that has to leave its guild without any right check (`Character.SetFaction`
/// into another alliance). The C# called `Kick(p, p)`, which needs the Expel right and a
/// strictly higher rank, so an ordinary member never left (and a leader that did would have
/// left the guild without one).
///
/// The guild **owner** leaving (the pirate case, PORTING.md PIRATE-1) follows the owner
/// decision of 2026-11-03: the leadership first passes to the highest-`role` remaining member
/// whose faction is still the guild mother (ties: the lowest `character_id`, the stable stand-in
/// for the longest membership), the old owner is removed in the same write section, and the
/// guild keeps its castle and nation. Only an owner who is the last eligible member dismisses
/// the guild (then releasing its dominion / siege).
///
/// Everything else is upstream's rule: a member with the leader role but a different recorded
/// owner hands the guild to the highest-ranked member (ties: first in the member list) and then
/// leaves; a sole member's guild is dismissed even with territory or a siege. The leave is
/// announced like a voluntary one.
pub async fn leave_forced(server: &GameServer, p: &PlayerRef) {
    loop {
        let Some(e) = expedition_of(server, p) else {
            return;
        };
        let id = e.id();
        let Some(me) = e.member(p.character_id) else {
            return;
        };
        if e.faction.owner_id == p.character_id {
            if forced_owner_leave(server, p, id).await {
                return;
            }
            // The owner changed while we decided: re-evaluate against the fresh guild.
            continue;
        }
        if me.role == LEADER_ROLE {
            let successor = e
                .members
                .iter()
                .filter(|m| m.character_id != p.character_id)
                .fold(None::<&ExpeditionMember>, |best, m| match best {
                    Some(b) if b.role >= m.role => Some(b),
                    _ => Some(m),
                })
                .map(|m| m.character_id);
            match successor {
                Some(new_id) => transfer_ownership(server, id, p.character_id, new_id).await,
                None => {
                    let _housing_operation = server
                        .systems
                        .housing
                        .operation_lock
                        .lock()
                        .expect("housing poisoned");
                    let nations = dismiss_expedition(server, id);
                    drop(_housing_operation);
                    for nation in &nations {
                        crate::systems::social::nation::NationManager::broadcast_owner_changed(
                            server, nation,
                        );
                    }
                    let nation_ids: Vec<u32> = nations.iter().map(|n| n.id).collect();
                    save_expedition_state(server, id, &nation_ids, None).await;
                    return;
                }
            }
        }
        remove_member_and_announce(server, p, id).await;
        return;
    }
}

/// `ExpeditionManager.Kick(connection, kickedId)`.
///
/// Failures are reported to the kicker (a deviation from the C#), except for a character
/// "kicking" itself: `Character.SetFaction` calls this with the character as both parties.
pub async fn kick(server: &GameServer, kicker: &PlayerRef, kicked_id: u32) {
    let fail = |error: i16| {
        if kicker.character_id != kicked_id {
            send_error(kicker, error);
        }
    };
    let Some(e) = expedition_of(server, kicker) else {
        fail(error::ONLY_EXPEDITION_MEMBER);
        return;
    };
    let id = e.id();
    let Some(me) = e.member(kicker.character_id) else {
        fail(error::ONLY_EXPEDITION_MEMBER);
        return;
    };
    match e.policy_by_role(me.role) {
        Some(x) if x.expel => {}
        Some(_) => {
            fail(error::FACTION_KICK_RIGHTS);
            return;
        }
        None => {
            tracing::error!("Expedition {id} has no policy for role {}", me.role);
            return;
        }
    }
    let Some(kicked) = e.member(kicked_id).cloned() else {
        fail(error::EXPEDITION_NO_TARGET);
        return;
    };
    // Deviation from C#: only a strictly higher rank can expel (so the leader never can be).
    if me.role <= kicked.role {
        tracing::warn!(
            "Kick: member {} (role {}) tried to expel {kicked_id} (role {}) of expedition {id}",
            kicker.character_id,
            me.role,
            kicked.role
        );
        fail(error::FACTION_KICK_RIGHTS);
        return;
    }
    remove_member(server, id, kicked_id);
    let kicked_char = server.world.character_by_id(kicked_id);
    let packet = SCUnitExpeditionChangedPacket {
        obj_id: kicked_char.as_ref().map_or(0, |c| c.obj_id),
        character_id: kicked.character_id,
        kicker: kicker.data().unit.name.clone(),
        unit_name: kicked.name.clone(),
        id,
        expedition_id: 0,
        expel: true,
    };
    if let Some(k) = &kicked_char {
        k.data().unit.expedition_id = 0;
        broadcast_self(server, k, &packet);
    }
    let members = server
        .expeditions
        .get(id)
        .map(|e| e.member_ids())
        .unwrap_or_default();
    send_to_members(server, &members, &packet);
    save(server, id).await;
}

/// `ExpeditionManager.ChangeMemberRole(connection, newRole, changedId)`.
///
/// Deviation from the C# (see PORTING.md "Guild functions (2026-11-05)"), which returned
/// silently: the caller needs the Promote right and the target and the new role must both rank
/// strictly below the caller, and the new role must exist and not be the leader's (else
/// `FACTION_CHANGE_MEMBER_ROLE_RIGHTS`, the client's own check); an unknown target is
/// `EXPEDITION_NO_TARGET`.
pub async fn change_member_role(server: &GameServer, p: &PlayerRef, new_role: u8, changed_id: u32) {
    let Some(e) = expedition_of(server, p) else {
        send_error(p, error::ONLY_EXPEDITION_MEMBER);
        return;
    };
    let id = e.id();
    let Some(changer) = e.member(p.character_id) else {
        send_error(p, error::ONLY_EXPEDITION_MEMBER);
        return;
    };
    match e.policy_by_role(changer.role) {
        Some(x) if x.promote => {}
        Some(_) => {
            send_error(p, error::FACTION_CHANGE_MEMBER_ROLE_RIGHTS);
            return;
        }
        None => {
            tracing::error!("Expedition {id} has no policy for role {}", changer.role);
            return;
        }
    }
    let Some(target) = e.member(changed_id) else {
        send_error(p, error::EXPEDITION_NO_TARGET);
        return;
    };
    if target.role >= changer.role
        || new_role >= changer.role
        || new_role == LEADER_ROLE
        || e.policy_by_role(new_role).is_none()
    {
        tracing::warn!(
            "ChangeMemberRole: member {} (role {}) tried to set role {new_role} on {changed_id} (role {}) of expedition {id}",
            p.character_id,
            changer.role,
            target.role
        );
        send_error(p, error::FACTION_CHANGE_MEMBER_ROLE_RIGHTS);
        return;
    }
    let changed = server.expeditions.with_mut(id, |e| {
        let members = e.member_ids();
        e.member_mut(changed_id).map(|m| {
            m.role = new_role;
            (m.character_id, m.role, m.name.clone(), members)
        })
    });
    let Some(Some((cid, role, name, members))) = changed else {
        return;
    };
    send_to_members(
        server,
        &members,
        &SCExpeditionRoleChangedPacket {
            id: cid,
            role,
            char_name: name,
        },
    );
    save(server, id).await;
}

/// `ExpeditionManager.Leave(character)` for a character that is not in the world (character
/// deletion, `CharacterManager.DeleteCharacterAssets`): the member is removed, the guild gets
/// `SCUnitExpeditionChangedPacket` and the guild is saved. Note: the C# also sends the packet
/// to the (lobby) character itself through `BroadcastPacket(.., true)`; there is no world
/// object to do that from here.
pub async fn leave_offline(server: &GameServer, character_id: u32, name: &str, expedition_id: u32) {
    let Some(e) = server.expeditions.get(expedition_id) else {
        return;
    };
    if e.member(character_id).is_none() {
        tracing::error!("Leave: {character_id} is not a member of expedition {expedition_id}");
        return;
    }
    remove_member(server, expedition_id, character_id);
    let packet = SCUnitExpeditionChangedPacket {
        obj_id: 0,
        character_id,
        kicker: String::new(),
        unit_name: name.to_string(),
        id: expedition_id,
        expedition_id: 0,
        expel: false,
    };
    let members = server
        .expeditions
        .get(expedition_id)
        .map(|e| e.member_ids())
        .unwrap_or_default();
    send_to_members(server, &members, &packet);
    save(server, expedition_id).await;
}
