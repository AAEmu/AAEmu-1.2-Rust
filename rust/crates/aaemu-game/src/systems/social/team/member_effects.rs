use super::{friend, verify_chat, GameServer, PlayerRef, Team};
#[cfg(test)]
use super::{interleave, Interleave};

/// `Character.InParty` setter (`FriendMananger.SendStatusChange(this, false, value)`).
pub fn set_in_party(server: &GameServer, p: &PlayerRef, value: bool) {
    if p.data().in_party == value {
        return;
    }
    friend::send_status_change(server, p, false, value);
    p.data().in_party = value;
    if value {
        // ENROLL_PARTY / ENROLL_RAID_GROUP: queued like the family and guild joins, the report
        // runs from the flush at the end of the operation. The team manager's guard is never
        // held here: every caller runs after dropping it.
        crate::systems::refresh_achievements_later(server, p);
    }
}

fn owner_name(server: &GameServer, owner_id: u32) -> Option<String> {
    server
        .world
        .character_by_id(owner_id)
        .map(|p| p.data().unit.name.clone())
}

/// `ChatManager.GetPartyChat(team, character)`.
pub fn party_chat(
    server: &GameServer,
    team: &Team,
    character_id: u32,
) -> crate::managers::chat::ChannelRef {
    party_channel(
        server,
        team.id,
        team.party_number_of(character_id),
        team.owner_id,
    )
}

/// [`party_chat`] from the ids a caller captured under the team lock (no `Team` borrow).
pub fn party_channel(
    server: &GameServer,
    team_id: u32,
    party_number: u32,
    owner_id: u32,
) -> crate::managers::chat::ChannelRef {
    let name = owner_name(server, owner_id);
    server
        .chat
        .party_chat(team_id, party_number, name.as_deref(), &server.ids.chat)
}

/// `ChatManager.GetRaidChat(team)`.
pub fn raid_chat(server: &GameServer, team: &Team) -> crate::managers::chat::ChannelRef {
    raid_channel(server, team.id, team.owner_id)
}

/// Whether `p` still is a member of `team_id` after one of its join effects was applied (one
/// short state lock, nothing else under it: [`TeamManager::is_member_now`]).
fn still_member(server: &GameServer, team_id: u32, p: &PlayerRef) -> bool {
    let teams = &server.systems.social.teams;
    #[cfg(test)]
    interleave(
        teams,
        Interleave::JoinApplied {
            team_id,
            character_id: p.character_id,
        },
    );
    teams.is_member_now(team_id, p.character_id)
}

/// Joins `p` to the raid `channel` of `team_id` and verifies it against the team state.
pub(super) fn join_raid_chat_verified(
    server: &GameServer,
    team_id: u32,
    channel: &crate::managers::chat::ChannelRef,
    p: &PlayerRef,
) {
    channel.join(p);
    verify_chat(server, team_id, None, channel, p, true);
}

/// Removes `p` from the raid `channel` of `team_id` and verifies it against the team state.
pub(super) fn leave_raid_chat_verified(
    server: &GameServer,
    team_id: u32,
    channel: &crate::managers::chat::ChannelRef,
    p: &PlayerRef,
) {
    channel.leave(p);
    verify_chat(server, team_id, None, channel, p, false);
}

/// [`set_in_party`] for a character that joined `team_id`: a `true` is taken back when the
/// character is not a member any more ("apply, then verify", see the module header).
pub(super) fn set_in_party_verified(server: &GameServer, team_id: u32, p: &PlayerRef, value: bool) {
    set_in_party(server, p, value);
    if value && !still_member(server, team_id, p) {
        settle_in_party(server, p);
    }
}

/// Settles `InParty` to what the team state says now: a character that left a team is "in party"
/// only when it is a member of another team by now (that team's accept can run its join effects
/// before this cleanup does, and the cleanup must not clear them). Each pass writes the flag from
/// one membership read and reads the membership again; the pass that runs last, after the last
/// membership change, ends on the current state (module header).
pub(super) fn settle_in_party(server: &GameServer, p: &PlayerRef) {
    let teams = &server.systems.social.teams;
    for _ in 0..3 {
        let member = teams.is_in_any_team(p.character_id);
        set_in_party(server, p, member);
        #[cfg(test)]
        interleave(
            teams,
            Interleave::InPartyWritten {
                character_id: p.character_id,
            },
        );
        if teams.is_in_any_team(p.character_id) == member {
            return;
        }
    }
}

/// Joins `p` to party chat `party_number` of `team_id` and verifies it against the team state:
/// the member may have left the team or been moved to another party since the state change this
/// join belongs to, and the leave of its old channel already ran, so a stale join would keep it
/// in two party chats.
pub(super) fn join_party_chat_verified(
    server: &GameServer,
    team_id: u32,
    owner_id: u32,
    party_number: u32,
    p: &PlayerRef,
) {
    let channel = party_channel(server, team_id, party_number, owner_id);
    channel.join(p);
    verify_chat(server, team_id, Some(party_number), &channel, p, true);
}

/// Removes `p` from party chat `party_number` of `team_id` and verifies it against the team
/// state: a member that was invited back (to the same party) before this cleanup ran keeps the
/// membership its newer join restored.
pub(super) fn leave_party_chat_verified(
    server: &GameServer,
    team_id: u32,
    owner_id: u32,
    party_number: u32,
    p: &PlayerRef,
) {
    let channel = party_channel(server, team_id, party_number, owner_id);
    channel.leave(p);
    verify_chat(server, team_id, Some(party_number), &channel, p, false);
}

/// [`raid_chat`] from the ids a caller captured under the team lock (no `Team` borrow).
pub fn raid_channel(
    server: &GameServer,
    team_id: u32,
    owner_id: u32,
) -> crate::managers::chat::ChannelRef {
    let name = owner_name(server, owner_id);
    server
        .chat
        .raid_chat(team_id, name.as_deref(), &server.ids.chat)
}
