use super::{
    broadcast_self, error, expedition_of, remove_member, save_expedition_state, send_error,
    send_to_members, util, Expedition, GameServer, HashMap, Nation, PlayerRef,
    SCExpeditionDismissedPacket, SCExpeditionOwnerChangedPacket, SCExpeditionRoleChangedPacket,
    SCUnitExpeditionChangedPacket, LEADER_ROLE,
};

/// The data one owner hand-over changed under the expedition write lock. `members` is the
/// roster before the old owner's removal (so it still receives the owner / role packets);
/// `remaining` is the roster after it (the leave packet).
struct OwnerChange {
    old: (u32, u8, String),
    new: (u32, u8, String),
    members: Vec<u32>,
    remaining: Vec<u32>,
}

/// Whether a (not finished) siege involves the guild.
fn in_siege(server: &GameServer, id: u32) -> bool {
    server.systems.housing.dominions.all().iter().any(|d| {
        d.siege.as_ref().is_some_and(|siege| {
            siege.status != crate::systems::housing::dominion::SiegeStatus::Finished
                && (siege.attacker_expedition_id == id || siege.defender_expedition_id == id)
        })
    })
}

/// Whether the guild owns a dominion.
fn owns_territory(server: &GameServer, id: u32) -> bool {
    server
        .systems
        .housing
        .dominions
        .all()
        .iter()
        .any(|d| d.owner_expedition_id == id)
}

/// Hands the guild `id` from the member `old_id` to the member `new_id`: owner id and name,
/// both roles, the packets and the save (the same hand-over the forced path uses, so a nation
/// founded by the guild moves its king and pending rename offers are dropped). No checks.
pub(super) async fn transfer_ownership(server: &GameServer, id: u32, old_id: u32, new_id: u32) {
    let res = server.expeditions.with_mut(id, |e| {
        let members = e.member_ids();
        let new = e.member_mut(new_id)?;
        new.role = LEADER_ROLE;
        let new = (new.character_id, new.role, new.name.clone());
        let old = e.member_mut(old_id)?;
        old.role = 0;
        let old = (old.character_id, old.role, old.name.clone());
        e.faction.owner_id = new_id;
        e.faction.owner_name = new.2.clone();
        Some(OwnerChange {
            old,
            new,
            remaining: members.clone(),
            members,
        })
    });
    let Some(Some(change)) = res else {
        return;
    };
    apply_owner_change(server, id, change, None).await;
}

/// Broadcasts and persists one hand-over. `removed` is the forced path's old owner, whose guild
/// membership goes in this same write section (no window in which a concurrent faction change
/// can pick it as successor again).
async fn apply_owner_change(
    server: &GameServer,
    id: u32,
    change: OwnerChange,
    removed: Option<&PlayerRef>,
) {
    let nations =
        server
            .systems
            .social
            .nations
            .on_expedition_owner_changed(id, change.new.0, &change.new.2);
    for nation in &nations {
        crate::systems::social::nation::NationManager::broadcast_owner_changed(server, nation);
    }
    // The old owner's dialog offer must not survive the hand-over.
    server.expeditions.clear_rename_offers_for(id);
    send_to_members(
        server,
        &change.members,
        &SCExpeditionOwnerChangedPacket {
            id: change.old.0,
            id2: change.new.0,
            char_name: change.new.2.clone(),
        },
    );
    send_to_members(
        server,
        &change.members,
        &SCExpeditionRoleChangedPacket {
            id: change.old.0,
            role: change.old.1,
            char_name: change.old.2,
        },
    );
    send_to_members(
        server,
        &change.members,
        &SCExpeditionRoleChangedPacket {
            id: change.new.0,
            role: change.new.1,
            char_name: change.new.2,
        },
    );
    if let Some(p) = removed {
        p.data().unit.expedition_id = 0;
        let packet = SCUnitExpeditionChangedPacket {
            obj_id: p.obj_id,
            character_id: p.character_id,
            kicker: String::new(),
            unit_name: p.data().unit.name.clone(),
            id,
            expedition_id: 0,
            expel: false,
        };
        broadcast_self(server, p, &packet);
        send_to_members(server, &change.remaining, &packet);
    }
    let nation_ids: Vec<u32> = nations.iter().map(|n| n.id).collect();
    save_expedition_state(server, id, &nation_ids, None).await;
}

/// `ExpeditionManager.ChangeOwner(connection, newOwnerId)`: keeps the persistent owner id,
/// name and roles consistent.
///
/// Deviation from the C# (see PORTING.md "Guild functions (2026-11-05)"), which returned
/// silently: a caller that is not the leader `FACTION_RIGHTS`, handing it to oneself
/// `EXPEDITION_CHANGE_OWNER_SELF`, to somebody outside the guild
/// `EXPEDITION_CHANGE_OWNER_MY_EXPEDITION`, and during a siege
/// `EXPEDITION_OWNER_CANNOT_CHANGE_DURING_SIEGE`.
pub async fn change_owner(server: &GameServer, owner: &PlayerRef, new_owner_id: u32) {
    let Some(e) = expedition_of(server, owner) else {
        send_error(owner, error::ONLY_EXPEDITION_MEMBER);
        return;
    };
    let id = e.id();
    match e.member(owner.character_id) {
        Some(m) if m.role == LEADER_ROLE => {}
        _ => {
            send_error(owner, error::FACTION_RIGHTS);
            return;
        }
    }
    if new_owner_id == owner.character_id {
        send_error(owner, error::EXPEDITION_CHANGE_OWNER_SELF);
        return;
    }
    if e.member(new_owner_id).is_none() {
        send_error(owner, error::EXPEDITION_CHANGE_OWNER_MY_EXPEDITION);
        return;
    }
    if in_siege(server, id) {
        send_error(owner, error::EXPEDITION_OWNER_CANNOT_CHANGE_DURING_SIEGE);
        return;
    }
    transfer_ownership(server, id, owner.character_id, new_owner_id).await;
}

/// `ExpeditionManager.Disband(owner)`. The guild stays in the list (renamed
/// `$deleted-guild-<id>`, owner 0) as in C#, but is no longer announced (`SCFactionList`).
///
/// Deviation from the C#: a guild that owns territory is refused with `EXPEDITION_OWNER_DISMISS`
/// and one registered in a siege with `EXPEDITION_DISMISS_IN_SIEGE` (the 1.2 texts; this server
/// answered with a chat line).
pub async fn disband(server: &GameServer, owner: &PlayerRef) -> bool {
    let (id, nations) = {
        let _housing_operation = server
            .systems
            .housing
            .operation_lock
            .lock()
            .expect("housing poisoned");
        let Some(e) = expedition_of(server, owner) else {
            send_error(owner, error::ONLY_EXPEDITION_MEMBER);
            return false;
        };
        if e.faction.owner_id != owner.character_id {
            send_error(owner, error::ONLY_EXPEDITION_OWNER);
            return false;
        }
        let id = e.id();
        if owns_territory(server, id) {
            send_error(owner, error::EXPEDITION_OWNER_DISMISS);
            return false;
        }
        if in_siege(server, id) {
            send_error(owner, error::EXPEDITION_DISMISS_IN_SIEGE);
            return false;
        }
        let nations = dismiss_expedition(server, id);
        (id, nations)
    };
    for nation in &nations {
        crate::systems::social::nation::NationManager::broadcast_owner_changed(server, nation);
    }
    let nation_ids: Vec<u32> = nations.iter().map(|n| n.id).collect();
    save_expedition_state(server, id, &nation_ids, None).await;
    true
}

/// The `Disband` body after its eligibility checks: every member gets
/// `SCExpeditionDismissedPacket`, loses the guild, and the guild row is retired
/// (`$deleted-guild-<id>`, owner 0, `isDisbanded`). Also used for the forced removal of an
/// owner on a faction change (no voluntary eligibility checks there).
pub(super) fn dismiss_expedition(server: &GameServer, id: u32) -> Vec<Nation> {
    // A dialog offer made for this guild dies with it (a later guild id must not inherit it).
    server.expeditions.clear_rename_offers_for(id);
    let Some(e) = server.expeditions.get(id) else {
        return Vec::new();
    };
    for m in e.members.iter().rev() {
        if let Some(c) = server.world.character_by_id(m.character_id) {
            let online = c.data().is_online;
            if online {
                c.send(SCExpeditionDismissedPacket { id, success: true });
            }
            c.data().unit.expedition_id = 0;
        }
        remove_member(server, id, m.character_id);
    }
    server.expeditions.with_mut(id, |e| {
        e.faction.name = format!("$deleted-guild-{id}");
        e.faction.owner_id = 0;
        e.is_disbanded = true;
    });
    // A nation founded by this guild stays as a kingless nation (expedition id and king
    // cleared), but its stale id must not capture a guild that later reuses it (NATION-5).
    server.systems.social.nations.on_expedition_disbanded(id)
}

/// One forced-removal decision under the expedition lock.
enum TransferOutcome {
    /// No expedition (or it was disbanded meanwhile).
    Gone,
    /// The character is no longer the owner.
    NotOwner,
    /// The owner is the last eligible member.
    NoSuccessor,
    /// Leadership moved and the old owner was removed in the same write section.
    Transferred(OwnerChange),
}

/// Faction of every member for successor eligibility: the live faction of an online member and
/// the saved `characters.faction_id` of an offline one. A failed read keeps those members
/// eligible; the hand-over and the old owner's removal share one write section, so a stale pick
/// cannot hand the guild back to the removed owner.
async fn member_factions(server: &GameServer, e: &Expedition) -> HashMap<u32, u32> {
    let mut factions = HashMap::new();
    let mut offline = Vec::new();
    for m in &e.members {
        match server.world.character_by_id(m.character_id) {
            Some(p) => {
                factions.insert(m.character_id, p.data().unit.faction_id);
            }
            None => offline.push(m.character_id),
        }
    }
    if !offline.is_empty() {
        let list = offline
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let rows: std::result::Result<Vec<(i64, i64)>, sqlx::Error> = sqlx::query_as(&format!(
            "SELECT CAST(id AS SIGNED), CAST(faction_id AS SIGNED) FROM characters WHERE id IN ({list})"
        ))
        .fetch_all(&server.db)
        .await;
        match rows {
            Ok(rows) => {
                for (id, faction) in rows {
                    factions.insert(id as u32, faction as u32);
                }
            }
            Err(err) => tracing::warn!(
                "successor faction lookup for expedition {} failed: {err}",
                e.id()
            ),
        }
    }
    factions
}

/// The member who inherits leadership when a forced change removes the owner: the highest
/// `role` first, then the longest membership. `ExpeditionMember` stores no join timestamp:
/// `last_world_leave_time` is the last world leave (set at join, refreshed on login / logout)
/// and measures activity, not tenure, so it cannot order members by membership. The lowest
/// `character_id` is the stable stand-in (ids are allocated in creation order, so the
/// first-created character wins); documented as an approximation in PORTING.md (PIRATE-1).
/// A member whose top-level faction is not the guild mother's (a pirate, or another alliance /
/// nation) cannot inherit: their own forced removal would immediately undo the hand-over.
fn successor_owner(
    e: &Expedition,
    exclude: u32,
    member_tops: &HashMap<u32, u32>,
    mother_top: u32,
) -> Option<u32> {
    e.members
        .iter()
        .filter(|m| m.character_id != exclude)
        .filter(|m| {
            member_tops
                .get(&m.character_id)
                .is_none_or(|&top| top == mother_top)
        })
        .min_by_key(|m| (std::cmp::Reverse(m.role), m.character_id))
        .map(|m| m.character_id)
}

/// The guild owner's forced removal (`Character.SetFaction`, PORTING.md PIRATE-1), run after
/// the character's data lock is released.
///
/// The 1.2 tips 2833 / 2848 say a member who becomes a pirate is kicked from the guild, with no
/// owner exception, and the C# `Character.SetFaction` called `ExpeditionManager.Kick`, which
/// acted only on roles whose policy has `Expel` (so ordinary members stayed). Owner decision
/// (2026-11-03): the owner hands the leadership to the remaining member chosen by
/// [`successor_owner`] and is removed too. The hand-over and the old owner's removal happen
/// under **one** expedition write lock, so a concurrent forced removal cannot pick the old,
/// already-pirate owner back as successor. Only an owner who was the last eligible member
/// dismisses the guild.
///
/// The guild's territory, siege registrations and nation stay with the transferred guild; only
/// the last-member dismissal releases a dominion / cancels its siege (the voluntary 301 / 469
/// refusal cannot apply: there is no guild left to hold it).
pub(super) async fn forced_owner_leave(server: &GameServer, p: &PlayerRef, id: u32) -> bool {
    loop {
        let Some(e) = expedition_of(server, p) else {
            return true;
        };
        if e.member(p.character_id).is_none() {
            return true;
        }
        let mother_top = super::nation::top_level_faction(server, e.faction.mother_id)
            .unwrap_or_else(|| util::alliance_id(server, e.faction.mother_id));
        // Refresh each attempt: a member can join or change faction while we decide. Compare
        // top-level factions (alliance / runtime nation), not the exact faction: a guild may be
        // founded under any sponsor faction of the alliance.
        let member_tops: HashMap<u32, u32> = member_factions(server, &e)
            .await
            .into_iter()
            .map(|(id, faction)| {
                (
                    id,
                    super::nation::top_level_faction(server, faction)
                        .unwrap_or_else(|| util::alliance_id(server, faction)),
                )
            })
            .collect();
        let outcome = server.expeditions.with_mut(id, |e| {
            if e.is_disbanded {
                return TransferOutcome::Gone;
            }
            let old_name = match e.member(p.character_id) {
                Some(m) => m.name.clone(),
                None => return TransferOutcome::Gone,
            };
            if e.faction.owner_id != p.character_id {
                return TransferOutcome::NotOwner;
            }
            let Some(new_owner_id) = successor_owner(e, p.character_id, &member_tops, mother_top)
            else {
                return TransferOutcome::NoSuccessor;
            };
            let members = e.member_ids();
            let Some(new) = e.member_mut(new_owner_id) else {
                return TransferOutcome::NoSuccessor;
            };
            new.role = 255;
            let new = (new.character_id, new.role, new.name.clone());
            // The old owner goes in this same write section: no window in which a concurrent
            // faction change sees it as an ordinary member again.
            e.remove_member(p.character_id);
            let remaining = e.member_ids();
            e.faction.owner_id = new_owner_id;
            e.faction.owner_name = new.2.clone();
            TransferOutcome::Transferred(OwnerChange {
                // The hand-over demotes the old owner to 0 before the removal, as the client
                // transfer does; the packet still names that role.
                old: (p.character_id, 0, old_name),
                new,
                members,
                remaining,
            })
        });
        match outcome {
            None | Some(TransferOutcome::Gone) => return true,
            Some(TransferOutcome::NotOwner) => return false,
            Some(TransferOutcome::Transferred(change)) => {
                apply_owner_change(server, id, change, Some(p)).await;
                return true;
            }
            Some(TransferOutcome::NoSuccessor) => {
                if dismiss_if_no_successor(server, id, p, &member_tops, mother_top).await {
                    return true;
                }
                // A valid member joined (or the owner changed) after the snapshot: retry.
            }
        }
    }
}

/// Dismisses the guild when there is still no eligible successor, re-checked under the housing
/// operation lock (a valid member can join between the successor snapshot and this branch).
/// Also does the forced release + persistence; `false` means the state changed and the caller
/// must retry the successor pick.
async fn dismiss_if_no_successor(
    server: &GameServer,
    id: u32,
    owner: &PlayerRef,
    member_tops: &HashMap<u32, u32>,
    mother_top: u32,
) -> bool {
    let _save = server.save.transaction_lock().await;
    let prepared = {
        let _housing_operation = server
            .systems
            .housing
            .operation_lock
            .lock()
            .expect("housing poisoned");
        let still_none = server.expeditions.with_mut(id, |e| {
            !e.is_disbanded
                && e.faction.owner_id == owner.character_id
                && e.member(owner.character_id).is_some()
                && successor_owner(e, owner.character_id, member_tops, mother_top).is_none()
        });
        match still_none {
            Some(true) => {
                let released = server
                    .systems
                    .housing
                    .dominions
                    .release_expedition(id, util::now_secs());
                let batch = server.systems.housing.dominions.prepare_save();
                let nations = dismiss_expedition(server, id);
                Some((released, batch, nations))
            }
            _ => None,
        }
    };
    let Some((released, batch, nations)) = prepared else {
        return false;
    };
    for (previous, state) in &released {
        crate::systems::social::dominion::apply_released(server, previous, state);
    }
    for nation in &nations {
        crate::systems::social::nation::NationManager::broadcast_owner_changed(server, nation);
    }
    let nation_ids: Vec<u32> = nations.iter().map(|n| n.id).collect();
    save_expedition_state(server, id, &nation_ids, Some(batch)).await;
    true
}
