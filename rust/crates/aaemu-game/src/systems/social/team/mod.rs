//! `Core/Managers/TeamManager` + `Models/Game/Team` (parties and raids): invitations, team
//! creation, leave / kick / dismiss (`AskRiskyTeam`), owner / role / marker / looting rule /
//! ping changes, raid conversion, member moves, offline / reconnect handling and the 500 ms
//! remote-member sync with its delta cache and the zombie-team disband.
//!
//! Locking: the manager's state mutex is never held across an object data lock or a call into
//! another manager. Work that needs character data (packet bodies, chat channels, `InParty`,
//! instance events) is collected while the state is mutated and run after the guard is dropped.
//! Membership online state is kept on [`TeamMember`] (like `ExpeditionMember`), so the manager
//! can decide and broadcast without reading a `Character`. Calling the manager's read APIs while
//! holding a player's data lock is fine: the leaf property is what keeps that order safe.
//!
//! Ordering of the deferred effects: the state change of an operation is atomic, its effects
//! (run after the guard) are not ordered against a second operation on the same character. The
//! join effects (chat channels, `InParty`) are therefore applied and then verified with
//! [`TeamManager::is_member_now`]; a character that is no longer a member gets them taken
//! back. That is race-free because a removal queues its own leave effects after its state
//! change: either the verify sees the removal and undoes the join, or the removal's leave runs
//! after the join. The party chat join is verified against the character's party number, so a
//! member moved to another party meanwhile does not keep the stale join; a chat leave is verified
//! the same way ([`verify_chat`]), so a member that was invited back before the cleanup ran keeps
//! the memberships of its newer join; a leave's `InParty = false` is settled to the character's
//! current membership ([`settle_in_party`]), so the cleanup of an older team does not clear the
//! flag of a team that accepted the character first.
//! Residual: the packets. The `SCJoinedTeam` of a deferred join is a snapshot
//! of the team at its state change, so the broadcast of a concurrent member change can reach
//! the new member before it (its client shows a ghost / missing member until it logs in
//! again), and the dungeon `OnTeamJoin` event is not taken back.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::packets::world::SCUnitsRemovedPacket;
use crate::server::GameServer;
use crate::systems::social::faction::{self, relation_state};
use crate::systems::social::friend;
use crate::systems::social::packets::misc::{invitation_canceled_kind, SCInvitationCanceledPacket};
use crate::systems::social::packets::team::*;
use crate::systems::social::util::{self, error, send_error};
use crate::world::{PlayerRef, WorldObject};

mod member_effects;
mod member_packets;
mod roster;
mod sync;

use self::member_effects::{
    join_party_chat_verified, join_raid_chat_verified, leave_party_chat_verified,
    leave_raid_chat_verified, set_in_party_verified, settle_in_party,
};
pub use self::member_effects::{party_channel, party_chat, raid_channel, raid_chat, set_in_party};
pub use self::member_packets::{are_in_render_distance, member_info, team_info};
use self::member_packets::{
    joined_team_packet, refresh_all_team_member_obj_ids, refresh_team_member_obj_ids,
    send_to_players,
};
pub use self::roster::{LootingRule, Team, TeamMember};
use self::sync::{clear_sync_state, try_claim_snapshot, MemberSyncState};

/// `RiskyAction`.
pub mod risky_action {
    pub const LEAVE: u8 = 0;
    pub const KICK: u8 = 1;
    pub const DISMISS: u8 = 2;
}

/// `MemberRole` (0..=3 are defined).
pub mod member_role {
    pub const UNDECIDED: u8 = 0;
    pub const TANK: u8 = 1;
    pub const HEALER: u8 = 2;
    pub const ATTACKER: u8 = 3;
}

/// `LootingRuleMethod`.
pub mod looting_method {
    pub const FREE_FOR_ALL: u8 = 0;
    pub const ROTATE_WINNER: u8 = 1;
    pub const LOOT_MASTER: u8 = 2;
    pub const PUBLIC: u8 = 0xFF;
}

/// `OverHeadMark.ResetAll`.
pub const OVER_HEAD_MARK_RESET_ALL: i32 = -2;

/// `TeamSyncIntervalMs`.
pub const TEAM_SYNC_INTERVAL_MS: u64 = 500;
/// `OfflineTeamDisbandGrace` (5 minutes).
pub const OFFLINE_TEAM_DISBAND_GRACE_MS: i64 = 5 * 60 * 1000;
/// Invitation timeout (`activeInvitation.Time.AddSeconds(60)`).
pub(crate) const INVITATION_TIMEOUT_MS: i64 = 60_000;
/// Server-side area-invite cooldown. The 1.2 client gates its own button for 30000 ms
/// (`FUN_39304ef0`, constant 30000.0) and the locale text says "(30 sec cooldown)".
pub(crate) const AREA_INVITE_COOLDOWN_MS: i64 = 30_000;
/// Hard cap on pending invitations. One invitation per target and the 60 s sweep keep the map
/// bounded by the online players; the cap is the defensive limit for a server that never ticks.
const MAX_PENDING_INVITATIONS: usize = 10_000;

/// `InvitationTemplate`.
#[derive(Debug, Clone)]
struct Invitation {
    team_id: u32,
    owner: PlayerRef,
    target: PlayerRef,
    time_ms: i64,
    is_party: bool,
    /// `SCAskToJoinTeamArea` was sent (an area invitation is replaced by a later invitation and
    /// skipped silently by the area loop).
    is_area: bool,
}

impl Invitation {
    /// `activeInvitation.Time.AddSeconds(60) < DateTime.UtcNow`.
    fn expired(&self, now_ms: i64) -> bool {
        self.time_ms + INVITATION_TIMEOUT_MS < now_ms
    }
}

#[derive(Debug, Default)]
struct State {
    /// `_activeTeams` (by id; the C# `ConcurrentDictionary` order is unspecified).
    teams: BTreeMap<u32, Team>,
    /// `_activeInvitations` (target id -> invitation).
    invitations: HashMap<u32, Invitation>,
    /// `_lastSyncState`.
    last_sync: HashMap<(u32, u32), MemberSyncState>,
    /// Area-invite cooldown deadline (unix ms) per inviter (`FUN_39304ef0` client 30 s gate).
    area_invite_until: HashMap<u32, i64>,
}

/// A point between two steps of a manager operation where a concurrent operation can land
/// (tests only, see [`interleave`]).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Interleave {
    /// A join effect for `character_id` was applied and is about to be verified against `team_id`.
    JoinApplied { team_id: u32, character_id: u32 },
    /// `reply_to_join_team` read the inviter's online flag of `target_id`'s invitation and is
    /// about to lock the state again.
    InviterRead { target_id: u32 },
    /// The deferred effects of an operation are about to run (its state change is done and its
    /// guard is dropped): a second operation can land before any of them.
    EffectsStart,
    /// [`settle_in_party`] wrote the flag of `character_id` and is about to read its membership
    /// again.
    InPartyWritten { character_id: u32 },
}

#[cfg(test)]
type InterleaveHook = std::rc::Rc<dyn Fn(&TeamManager, Interleave)>;

#[cfg(test)]
thread_local! {
    static INTERLEAVE: std::cell::RefCell<Option<InterleaveHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Test seam: runs the hook installed on this thread (if any). It lets a test land another
/// operation exactly between two steps (the removal of a member between the apply and the
/// verify of a join effect, a replaced invitation between the two lock sections of a reply),
/// which is the window a concurrent thread hits, without threads or timing.
#[cfg(test)]
fn interleave(teams: &TeamManager, at: Interleave) {
    let hook = INTERLEAVE.with(|h| h.borrow().clone());
    if let Some(hook) = hook {
        hook(teams, at);
    }
}

/// Which chat group of a team a channel is: the raid channel (`None`, the whole team) or the
/// chat of party `n` (`Some(n)`).
type ChatGroup = Option<u32>;

/// Makes the chat membership of `p` agree with the team state after a chat effect was applied
/// ("apply, then verify", see the module header). `applied` is what the effect left (`true` =
/// joined): when the team state says the opposite by now, the opposite is written, up to three
/// passes. That takes back a join that outlived its member (kicked, moved to another party) and
/// restores what a leave that outlived a newer join of the same member removed (it was invited
/// back before the cleanup ran).
fn verify_chat(
    server: &GameServer,
    team_id: u32,
    group: ChatGroup,
    channel: &crate::managers::chat::ChannelRef,
    p: &PlayerRef,
    mut applied: bool,
) {
    let teams = &server.systems.social.teams;
    for _ in 0..3 {
        #[cfg(test)]
        interleave(
            teams,
            Interleave::JoinApplied {
                team_id,
                character_id: p.character_id,
            },
        );
        let member = teams.chat_group_now(team_id, p.character_id, group);
        if member == applied {
            return;
        }
        if member {
            channel.join(p);
        } else {
            channel.leave(p);
        }
        applied = member;
    }
}

/// External work collected while the manager's state is mutated and run after its guard is
/// dropped: packet bodies ([`member_info`]), chat channels, `InParty` and instance events all
/// read character data or another manager and must not run under the team lock.
type DeferredFn = Box<dyn FnOnce(&GameServer)>;

#[derive(Default)]
struct Deferred(Vec<DeferredFn>);

impl Deferred {
    fn push(&mut self, f: impl FnOnce(&GameServer) + 'static) {
        self.0.push(Box::new(f));
    }

    fn run(self, server: &GameServer) {
        #[cfg(test)]
        interleave(&server.systems.social.teams, Interleave::EffectsStart);
        for f in self.0 {
            f(server);
        }
    }
}

/// `TeamManager`.
#[derive(Debug, Default)]
pub struct TeamManager {
    state: Mutex<State>,
}

impl TeamManager {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("teams poisoned")
    }

    fn team_id_by_unit(st: &State, unit_id: u32) -> Option<u32> {
        st.teams
            .values()
            .find(|t| t.is_member(unit_id))
            .map(|t| t.id)
    }

    /// `GetActiveTeamByUnit(unitId)` (a copy).
    pub fn active_team_by_unit(&self, unit_id: u32) -> Option<Team> {
        let st = self.lock();
        let id = Self::team_id_by_unit(&st, unit_id)?;
        st.teams.get(&id).cloned()
    }

    /// `GetTeamByObjId(objId)` (a copy).
    pub fn team_by_obj_id(&self, obj_id: u32) -> Option<Team> {
        self.lock()
            .teams
            .values()
            .find(|t| {
                t.members
                    .iter()
                    .flatten()
                    .any(|m| m.character.obj_id == obj_id)
            })
            .cloned()
    }

    /// `GetActiveTeam(teamId)` (a copy; `None` for 0).
    pub fn active_team(&self, team_id: u32) -> Option<Team> {
        if team_id == 0 {
            return None;
        }
        self.lock().teams.get(&team_id).cloned()
    }

    /// `AreTeamMembers(unit1, unit2)`.
    pub fn are_team_members(&self, unit1: u32, unit2: u32) -> bool {
        let st = self.lock();
        Self::team_id_by_unit(&st, unit1)
            .and_then(|id| st.teams.get(&id))
            .is_some_and(|t| t.is_member(unit2))
    }

    /// The member characters of `unit_id`'s team in slot order (`ExpeditionManager` uses the
    /// whole list, offline members included).
    pub fn team_characters_of(&self, unit_id: u32) -> Option<Vec<PlayerRef>> {
        let st = self.lock();
        let id = Self::team_id_by_unit(&st, unit_id)?;
        Some(
            st.teams[&id]
                .members
                .iter()
                .flatten()
                .map(|m| m.character.clone())
                .collect(),
        )
    }

    /// `Team.IsObjMember(objId)` for the character obj ids of a team (mates: the skills / mates
    /// subsystem checks the owner itself).
    pub fn is_obj_member(&self, team_id: u32, obj_id: u32) -> bool {
        self.lock().teams.get(&team_id).is_some_and(|t| {
            t.members
                .iter()
                .flatten()
                .any(|m| m.character.obj_id == obj_id)
        })
    }

    /// Whether `character_id` is a member of team `team_id` right now: one short state lock and
    /// nothing else under it. The deferred join effects (chat channels, `InParty`) call it after
    /// they were applied and take them back for a character that left meanwhile.
    pub fn is_member_now(&self, team_id: u32, character_id: u32) -> bool {
        self.lock()
            .teams
            .get(&team_id)
            .is_some_and(|t| t.is_member(character_id))
    }

    /// Whether `character_id` is a member of any team right now (one short state lock, nothing
    /// else under it): what a leave's `InParty = false` is settled against ([`settle_in_party`]).
    pub fn is_in_any_team(&self, character_id: u32) -> bool {
        Self::team_id_by_unit(&self.lock(), character_id).is_some()
    }

    /// The party number of `character_id` in team `team_id` right now (`None` when it is not a
    /// member): what a deferred party chat join is verified against
    /// ([`join_party_chat_verified`]). One short state lock, nothing else under it.
    pub fn party_number_now(&self, team_id: u32, character_id: u32) -> Option<u32> {
        self.lock()
            .teams
            .get(&team_id)
            .filter(|t| t.is_member(character_id))
            .map(|t| t.party_number_of(character_id))
    }

    /// Whether `character_id` belongs to chat group `group` of team `team_id` right now: the
    /// raid channel (`None`) is the whole team, the chat of party `n` its members of party `n`.
    /// What a deferred chat join or leave is verified against.
    pub fn chat_group_now(&self, team_id: u32, character_id: u32, group: Option<u32>) -> bool {
        match group {
            None => self.is_member_now(team_id, character_id),
            Some(n) => self.party_number_now(team_id, character_id) == Some(n),
        }
    }

    /// Number of active teams.
    pub fn team_count(&self) -> usize {
        self.lock().teams.len()
    }

    /// Number of pending invitations (tests).
    #[cfg(test)]
    pub(crate) fn invitation_count(&self) -> usize {
        self.lock().invitations.len()
    }

    /// Makes every pending invitation `ms` older (tests of the invitation timeout).
    #[cfg(test)]
    pub(crate) fn age_invitations(&self, ms: i64) {
        for i in self.lock().invitations.values_mut() {
            i.time_ms -= ms;
        }
    }

    /// `InviteAreaToTeam(owner, teamId, isParty)`: every player within 100 m that is not in a
    /// team, is not blocked and is not hostile gets `SCAskToJoinTeamArea` (the 1.2 client log:
    /// "now inviting characters into raidteam in range of 100 meters"). The inviter gets
    /// `SCTeamAreaInvited`.
    ///
    /// The client gates its own button for 30 s; the server keeps the same per-inviter cooldown
    /// and answers a second request with `SCTeamAreaInvited { r: remaining_ms, s: false }`
    /// (the client shows "Can use area invite after %.1f seconds" using `r * 0.001`). With
    /// `s = true` the client ignores `r`, so the success reply carries 0.
    ///
    /// The 1.2 "refuse area invitation" option is client-local (`X2Team.SetRefuseAreaInvitation`
    /// only stores a flag; `OnAskToJoinTeamArea` drops the popup when it is set), so it cannot
    /// be honoured on the server. See PORTING.md.
    pub fn invite_area_to_team(
        &self,
        server: &GameServer,
        owner: &PlayerRef,
        _team_id: u32,
        is_party: bool,
    ) {
        let Some(world) = server.world.world(owner.data().unit.transform.instance_id) else {
            return;
        };
        // A full team refuses once, not once per candidate.
        if self
            .active_team_by_unit(owner.character_id)
            .is_some_and(|t| t.is_full())
        {
            send_error(owner, error::TEAM_FULL);
            return;
        }
        let me = util::world_position(server, owner);
        let owner_name = owner.data().unit.name.clone();
        // Everything that reads character data is resolved before the manager lock: the manager
        // never takes an object data lock while it holds its own mutex.
        let around: Vec<(PlayerRef, bool, bool, bool, bool)> = world
            .players_around(owner)
            .into_iter()
            .filter_map(|p| {
                let pos = util::world_position(server, &p);
                let (dx, dy) = (pos.0 - me.0, pos.1 - me.1);
                // The area loop never targets the inviter itself, and `Region.GetList`
                // compares the squared x/y distance with `< sqrad`.
                if p.character_id == owner.character_id || dx * dx + dy * dy >= 100.0 * 100.0 {
                    return None;
                }
                let rel = faction::player_relation(server, owner, &p);
                let in_party = p.data().in_party;
                let blocked = friend::is_blocked_by(&p, owner.character_id);
                let acceptable = matches!(rel, relation_state::FRIENDLY | relation_state::NEUTRAL);
                Some((
                    p,
                    in_party,
                    blocked,
                    acceptable,
                    rel == relation_state::HOSTILE,
                ))
            })
            .collect();
        let now = util::now_ms();
        let mut st = self.lock();
        let mut canceled = prune_expired_invitations(&mut st, now);
        st.area_invite_until.retain(|_, until| *until > now);
        if let Some(until) = st.area_invite_until.get(&owner.character_id) {
            let remaining = (*until - now).clamp(0, i64::from(u32::MAX)) as u32;
            drop(st);
            send_invitation_canceled(canceled);
            owner.send(SCTeamAreaInvitedPacket {
                r: remaining,
                s: false,
            });
            return;
        }
        // The team is the inviter's current team, never the client-sent id (a crafted id = 0
        // used to bypass the permission check).
        let team = Self::team_id_by_unit(&st, owner.character_id)
            .and_then(|id| st.teams.get(&id))
            .map(|t| {
                (
                    t.id,
                    t.is_party,
                    t.owner_id,
                    t.is_marked(owner.character_id),
                    t.is_full(),
                )
            });
        // `AskToJoin` would answer "no rights" once per target; check it once here instead.
        if let Some((_, team_is_party, team_owner, marked, _)) = team {
            let allowed = team_owner == owner.character_id || (!team_is_party && marked);
            if !allowed {
                drop(st);
                send_invitation_canceled(canceled);
                send_error(owner, error::TEAM_NO_RIGHTS);
                return;
            }
        }
        let invite_is_party = team.map_or(is_party, |t| t.1);
        st.area_invite_until
            .insert(owner.character_id, now + AREA_INVITE_COOLDOWN_MS);
        let mut invited = 0u32;
        for (c, in_party, blocked, acceptable, hostile) in around {
            if in_party || !acceptable || Self::team_id_by_unit(&st, c.character_id).is_some() {
                continue;
            }
            if self.invite_locked(
                server,
                &mut st,
                owner,
                "",
                &owner_name,
                team,
                invite_is_party,
                &c,
                blocked,
                hostile,
                true,
            ) {
                invited += 1;
            }
        }
        tracing::debug!(
            "Area invitation from {} reached {} player(s)",
            owner.character_id,
            invited
        );
        // An expired invitee that was invited again in this loop already has a new popup;
        // do not close it (the cancellation has no invitation id).
        canceled.retain(|(target, _)| !st.invitations.contains_key(&target.character_id));
        drop(st);
        send_invitation_canceled(canceled);
        owner.send(SCTeamAreaInvitedPacket { r: 0, s: true });
    }

    /// `AskToJoin(owner, targetName, teamId, isParty, targetObj)`.
    ///
    /// Deviations from the C# (see PORTING.md): the inviter is told why an invitation fails
    /// (missing / offline target, yourself, a target that is in a team already, no right to
    /// invite, a full team) instead of getting nothing; the team is the inviter's own (the C#
    /// believed the client's team id, so a member could invite without the owner's right by
    /// sending 0); an invitation that nobody answered within a minute no longer blocks later
    /// invitations of the same target.
    pub fn ask_to_join(
        &self,
        server: &GameServer,
        owner: &PlayerRef,
        target_name: &str,
        _team_id: u32,
        is_party: bool,
        target_obj: Option<&PlayerRef>,
    ) {
        let target = match target_obj {
            Some(t) => t.clone(),
            None => match server.world.character_by_name(target_name) {
                Some(t) => t,
                None => {
                    send_error(owner, error::TEAM_INVITEE_OFFLINE);
                    return;
                }
            },
        };
        let owner_name = owner.data().unit.name.clone();
        let blocked = friend::is_blocked_by(&target, owner.character_id);
        let hostile = faction::player_relation(server, owner, &target) == relation_state::HOSTILE;
        let mut st = self.lock();
        let mut canceled = prune_expired_invitations(&mut st, util::now_ms());
        // The invitation belongs to the inviter's current team; the client-sent team id is
        // ignored so a stale/crafted id cannot invite into someone else's team.
        let team = Self::team_id_by_unit(&st, owner.character_id)
            .and_then(|id| st.teams.get(&id))
            .map(|t| {
                (
                    t.id,
                    t.is_party,
                    t.owner_id,
                    t.is_marked(owner.character_id),
                    t.is_full(),
                )
            });
        self.invite_locked(
            server,
            &mut st,
            owner,
            target_name,
            &owner_name,
            team,
            team.map_or(is_party, |t| t.1),
            &target,
            blocked,
            hostile,
            false,
        );
        // The target may be the same character whose expired invitation was pruned above; a
        // new popup is open, so the cancellation must not close it.
        canceled.retain(|(target, _)| !st.invitations.contains_key(&target.character_id));
        drop(st);
        send_invitation_canceled(canceled);
    }

    /// The invitation body of `AskToJoin` (the caller holds the manager lock): block list,
    /// yourself, hostility, the invitee's team, the pending-invitation slot, the team permission
    /// and the invitation insert. Sends `SCAskToJoinTeamArea` for an area invitation and
    /// `SCAskToJoinTeam` otherwise. A new invitation replaces a pending area invitation; an
    /// area invitation skips an already-pending target silently.
    #[allow(clippy::too_many_arguments)]
    fn invite_locked(
        &self,
        _server: &GameServer,
        st: &mut State,
        owner: &PlayerRef,
        target_name: &str,
        owner_name: &str,
        team: Option<(u32, bool, u32, bool, bool)>,
        is_party: bool,
        target: &PlayerRef,
        blocked: bool,
        hostile: bool,
        is_area: bool,
    ) -> bool {
        // A blocked inviter gets no invitation (named and area invites both end here). The
        // 1.2 client has no "target blocked you" error, so the owner gets no reply
        // (capture-pending, case `BLOCK-LIST-1`).
        if blocked {
            return false;
        }
        // The C# `TEAM_YOURSELF` answer; the area loop excludes the inviter itself.
        if target.character_id == owner.character_id {
            send_error(owner, error::TEAM_YOURSELF);
            return false;
        }
        // Only hostile players cannot be invited.
        if hostile {
            send_error(owner, error::TEAM_INVITE_REFUSED);
            return false;
        }
        // `TEAM_INVITEE_MEMBER` / `TEAM_INVITEE_IN_TEAM` are the inviter-facing codes for a
        // target already in the inviter's own / another team (the area loop filters these out,
        // so only a named invite answers).
        if let Some(target_team) = Self::team_id_by_unit(st, target.character_id) {
            if !is_area {
                send_error(
                    owner,
                    if team.map(|t| t.0) == Some(target_team) {
                        error::TEAM_INVITEE_MEMBER
                    } else {
                        error::TEAM_INVITEE_IN_TEAM
                    },
                );
            }
            return false;
        }
        if let Some(existing) = st.invitations.get(&target.character_id) {
            if existing.is_area {
                // A refused/ignored area invitation must not block a real invitation.
                st.invitations.remove(&target.character_id);
            } else if is_area {
                // The area loop must not spam the inviter with empty-name rejections.
                return false;
            } else {
                owner.send(SCRejectedTeamPacket {
                    name: target_name.to_string(),
                    party: is_party,
                });
                return false;
            }
        }
        if let Some((_, team_is_party, team_owner, marked, full)) = team {
            let allowed = team_owner == owner.character_id || (!team_is_party && marked);
            if !allowed {
                send_error(owner, error::TEAM_NO_RIGHTS);
                return false;
            }
            if full {
                send_error(owner, error::TEAM_FULL);
                return false;
            }
        }
        if st.invitations.len() >= MAX_PENDING_INVITATIONS {
            tracing::warn!(
                "Dropping team invitation of {}: {} pending invitations",
                target.character_id,
                st.invitations.len()
            );
            return false;
        }
        let (inv_team_id, inv_team_is_party) = team.map_or((0, is_party), |t| (t.0, t.1));
        st.invitations.insert(
            target.character_id,
            Invitation {
                owner: owner.clone(),
                target: target.clone(),
                is_party: inv_team_is_party,
                is_area,
                time_ms: util::now_ms(),
                team_id: inv_team_id,
            },
        );
        if is_area {
            target.send(SCAskToJoinTeamAreaPacket {
                team_id: inv_team_id,
                sender_id: owner.character_id,
                sender_name: owner_name.to_string(),
                is_party: inv_team_is_party,
            });
        } else {
            target.send(SCAskToJoinTeamPacket {
                team_id: inv_team_id,
                sender_id: owner.character_id,
                sender_name: owner_name.to_string(),
                is_party: inv_team_is_party,
            });
        }
        true
    }

    /// Locks the state with the pending invitation of `target_id` and whether its inviter is
    /// online. The flag is a character data read, so it is taken between two state-lock
    /// sections (the manager must not take a character data lock while it holds its mutex). The
    /// invitation is looked at again under the second lock: when it names another inviter than
    /// the flag was read for (it was replaced, or appeared, in between) the flag is not the
    /// invitation's own and the read is repeated once. `None` when that fails again.
    fn lock_with_inviter_online(&self, target_id: u32) -> Option<(MutexGuard<'_, State>, bool)> {
        for _ in 0..2 {
            let inviter = self
                .lock()
                .invitations
                .get(&target_id)
                .map(|inv| inv.owner.clone());
            let online = inviter.as_ref().is_some_and(|p| p.data().is_online);
            #[cfg(test)]
            interleave(self, Interleave::InviterRead { target_id });
            let st = self.lock();
            let current = st
                .invitations
                .get(&target_id)
                .map(|inv| inv.owner.character_id);
            if current == inviter.map(|p| p.character_id) {
                return Some((st, online));
            }
        }
        None
    }

    /// `ReplyToJoinTeam(target, teamId, isParty, ownerId, isReject, charName, isArea)`.
    ///
    /// Deviations from the C# (see PORTING.md): an acceptance that fails is reported (no / expired
    /// invitation, the inviter's team is gone or full), a character that is in a team already
    /// is refused instead of ending up in two teams, and a late joiner joins the team's chat
    /// channels.
    #[allow(clippy::too_many_arguments)]
    pub fn reply_to_join_team(
        &self,
        server: &GameServer,
        target: &PlayerRef,
        team_id: u32,
        _is_party: bool,
        owner_id: u32,
        is_reject: bool,
        _char_name: &str,
        _is_area: bool,
    ) {
        let target_name = target.data().unit.name.clone();
        let Some((mut st, owner_online)) = self.lock_with_inviter_online(target.character_id)
        else {
            // The invitation was replaced twice while this reply ran: ignore it like the answer
            // of a stale popup.
            return;
        };
        let Some(inv) = st.invitations.get(&target.character_id).cloned() else {
            // Cancelled (the inviter left, or nobody answered for a minute) or never sent.
            drop(st);
            if !is_reject {
                send_error(target, error::TEAM_JOIN);
            }
            return;
        };
        // A stale popup from another inviter must not consume (or reject) this invitation.
        if owner_id != inv.owner.character_id {
            return;
        }
        if team_id != 0 && team_id != inv.team_id {
            return;
        }
        if is_reject || inv.expired(util::now_ms()) {
            inv.owner.send(SCRejectedTeamPacket {
                name: target_name,
                party: inv.is_party,
            });
            if !is_reject {
                send_error(target, error::INVITATION_CANCELED_TIMEOUT);
            }
            st.invitations.remove(&target.character_id);
            return;
        }
        // The client's own accept check (`FUN_393075a0` -> `FUN_39428370(0x52)`): an invitee
        // already in a group gets `TEAM_JOIN_ALONE`; the inviter is told whether it is the same
        // team or another one.
        if let Some(own) = Self::team_id_by_unit(&st, target.character_id) {
            let same = Self::team_id_by_unit(&st, inv.owner.character_id) == Some(own);
            send_error(target, error::TEAM_JOIN_ALONE);
            send_error(
                &inv.owner,
                if same {
                    error::TEAM_INVITEE_MEMBER
                } else {
                    error::TEAM_INVITEE_IN_TEAM
                },
            );
            st.invitations.remove(&target.character_id);
            return;
        }
        // The inviter must still be online and (when the invitation named a team) still in
        // that same team with the rights to invite.
        if !owner_online {
            send_error(target, error::TEAM_INVITOR_OFFLINE);
            st.invitations.remove(&target.character_id);
            return;
        }
        let owner_team = Self::team_id_by_unit(&st, inv.owner.character_id);
        if inv.team_id != 0 && owner_team != Some(inv.team_id) {
            send_error(target, error::TEAM_INVITOR_MOVED);
            st.invitations.remove(&target.character_id);
            return;
        }
        if let Some(tid) = owner_team {
            let team = st.teams.get(&tid).expect("team");
            let allowed = team.owner_id == inv.owner.character_id
                || (!team.is_party && team.is_marked(inv.owner.character_id));
            if !allowed {
                send_error(&inv.owner, error::TEAM_NO_RIGHTS);
                st.invitations.remove(&target.character_id);
                return;
            }
        }
        // Everything below is state-only; the packets, chat channels, `InParty` flags and
        // instance events run after the guard is dropped.
        let mut deferred = Deferred::default();
        // An area invitation is answered like a named one; the invitation carries the team.
        match owner_team {
            None => {
                // `inv.team_id == 0` here: the inviter had no team when the invitation was
                // issued and still has none.
                self.create_new_team(server, &mut st, &inv, &mut deferred);
            }
            Some(team_id) => {
                let team = st.teams.get_mut(&team_id).expect("team");
                if team.is_full() {
                    send_error(target, error::TEAM_FULL);
                    send_error(&inv.owner, error::TEAM_FULL);
                    st.invitations.remove(&inv.target.character_id);
                    return;
                }
                if let Some((slot, party)) = team.add_member(target) {
                    let team = st.teams.get(&team_id).expect("team").clone();
                    let target = target.clone();
                    deferred.push(move |server| {
                        target.send(joined_team_packet(server, &team));
                        set_in_party_verified(server, team.id, &target, true);
                        target.send(SCTeamPingPosPacket {
                            has_ping: true,
                            position: team.ping_position,
                            ins_id: 0,
                        });
                        let member = team.members[slot].as_ref().expect("added");
                        team.broadcast(
                            &SCTeamMemberJoinedPacket {
                                team_id: team.id,
                                member: member_info(server, member),
                                party: party as i32,
                            },
                            target.character_id,
                        );
                        // Deviation (C#: the new member never joined the party / raid chat, so
                        // it could talk into the channel but not hear it until its next login).
                        if !team.is_party {
                            join_raid_chat_verified(
                                server,
                                team.id,
                                &raid_chat(server, &team),
                                &target,
                            );
                        }
                        let party_number = team.party_number_of(target.character_id);
                        join_party_chat_verified(
                            server,
                            team.id,
                            team.owner_id,
                            party_number,
                            &target,
                        );
                        refresh_team_member_obj_ids(&team, &target, false);
                    });
                }
            }
        }
        st.invitations.remove(&inv.target.character_id);
        drop(st);
        deferred.run(server);
    }

    /// Drops the invitations nobody answered within [`INVITATION_TIMEOUT_MS`] (the 500 ms tick
    /// calls this); the inviter is told that the invitation was cancelled and the invitee's
    /// popup is closed with `SCInvitationCanceled`.
    pub fn expire_invitations(&self, now_ms: i64) {
        let mut st = self.lock();
        let expired: Vec<u32> = st
            .invitations
            .iter()
            .filter(|(_, i)| i.expired(now_ms))
            .map(|(target, _)| *target)
            .collect();
        let mut canceled = Vec::with_capacity(expired.len());
        for target in expired {
            if let Some(inv) = st.invitations.remove(&target) {
                send_error(&inv.owner, error::INVITATION_CANCELED_TIMEOUT);
                canceled.push((inv.target, inv.is_area));
            }
        }
        drop(st);
        send_invitation_canceled(canceled);
    }

    /// Forgets the invitations of a character that went offline: the ones it received, and the
    /// ones it sent (their targets are told that the inviter is gone).
    fn drop_invitations(st: &mut State, character_id: u32) {
        let gone: Vec<u32> = st
            .invitations
            .iter()
            .filter(|(target, i)| **target == character_id || i.owner.character_id == character_id)
            .map(|(target, _)| *target)
            .collect();
        for target in gone {
            if let Some(inv) = st.invitations.remove(&target) {
                if inv.owner.character_id == character_id {
                    send_error(&inv.target, error::TEAM_INVITOR_OFFLINE);
                    inv.target.send(SCInvitationCanceledPacket {
                        kind: if inv.is_area {
                            invitation_canceled_kind::TEAM_AREA
                        } else {
                            invitation_canceled_kind::TEAM
                        },
                    });
                }
            }
        }
    }

    /// `CreateNewTeam(activeInvitation)` (the caller checked both members under the same lock).
    fn create_new_team(
        &self,
        server: &GameServer,
        st: &mut State,
        inv: &Invitation,
        deferred: &mut Deferred,
    ) {
        if Self::team_id_by_unit(st, inv.owner.character_id).is_some()
            || Self::team_id_by_unit(st, inv.target.character_id).is_some()
        {
            // Unreachable from `reply_to_join_team` (it checks both under this lock).
            tracing::warn!(
                "CreateNewTeam: {} or {} already has a team",
                inv.owner.character_id,
                inv.target.character_id
            );
            return;
        }
        let id = match server.ids.team.get_next_id() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("{e}");
                return;
            }
        };
        let mut team = Team::new(id, inv.owner.character_id, inv.is_party);
        if team.add_member(&inv.owner).is_none() || team.add_member(&inv.target).is_none() {
            return;
        }
        st.teams.insert(id, team);
        let team = st.teams[&id].clone();
        let owner = inv.owner.clone();
        let target = inv.target.clone();
        deferred.push(move |server| {
            owner.send(joined_team_packet(server, &team));
            set_in_party_verified(server, team.id, &owner, true);
            target.send(joined_team_packet(server, &team));
            set_in_party_verified(server, team.id, &target, true);
            let ping = owner.data().local_ping_position;
            team.broadcast(
                &SCTeamPingPosPacket {
                    has_ping: true,
                    position: ping,
                    ins_id: 0,
                },
                0,
            );
            if !team.is_party {
                join_raid_chat_verified(server, team.id, &raid_chat(server, &team), &owner);
                join_raid_chat_verified(server, team.id, &raid_chat(server, &team), &target);
            }
            for p in [&owner, &target] {
                let party_number = team.party_number_of(p.character_id);
                join_party_chat_verified(server, team.id, team.owner_id, party_number, p);
            }
            // Trigger events (systems::instances dungeons).
            crate::systems::instances::events::on_team_join(server, &owner, &team);
            crate::systems::instances::events::on_team_join(server, &target, &team);
            refresh_team_member_obj_ids(&team, &target, false);
        });
    }

    /// `CreateSoloTeam(character, asParty)` (GM command `soloparty`).
    pub fn create_solo_team(&self, server: &GameServer, p: &PlayerRef, as_party: bool) {
        let ping = p.data().local_ping_position;
        let mut st = self.lock();
        if Self::team_id_by_unit(&st, p.character_id).is_some() {
            send_error(p, error::TEAM_JOIN_ALONE);
            return;
        }
        let id = match server.ids.team.get_next_id() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("{e}");
                return;
            }
        };
        let mut team = Team::new(id, p.character_id, true);
        if team.add_member(p).is_none() {
            return;
        }
        st.teams.insert(id, team);
        let team = st.teams[&id].clone();
        let p = p.clone();
        drop(st);
        p.send(joined_team_packet(server, &team));
        set_in_party_verified(server, team.id, &p, as_party);
        team.broadcast(
            &SCTeamPingPosPacket {
                has_ping: true,
                position: ping,
                ins_id: 0,
            },
            0,
        );
        if !team.is_party {
            join_raid_chat_verified(server, team.id, &raid_chat(server, &team), &p);
        }
        let party_number = team.party_number_of(p.character_id);
        join_party_chat_verified(server, team.id, team.owner_id, party_number, &p);
        // Trigger events (systems::instances dungeons).
        crate::systems::instances::events::on_team_join(server, &p, &team);
    }

    /// `AskRiskyTeam(unit, teamId, targetId, riskyAction)`: leave, kick or dismiss.
    ///
    /// Note (C#): the chat channels are left by `unit` (the kicker, for a kick). Destructive
    /// requests require membership; kicking and dismissing require the current owner.
    pub fn ask_risky_team(
        &self,
        server: &GameServer,
        unit: &PlayerRef,
        team_id: u32,
        target_id: u32,
        risky_action: u8,
    ) {
        let mut st = self.lock();
        let mut deferred = Deferred::default();
        self.ask_risky_team_locked(
            server,
            &mut st,
            unit,
            team_id,
            target_id,
            risky_action,
            &mut deferred,
        );
        drop(st);
        deferred.run(server);
    }

    #[allow(clippy::too_many_arguments)]
    fn ask_risky_team_locked(
        &self,
        server: &GameServer,
        st: &mut State,
        unit: &PlayerRef,
        team_id: u32,
        target_id: u32,
        risky_action: u8,
        deferred: &mut Deferred,
    ) {
        self.ask_risky_team_locked_from(
            server,
            st,
            unit,
            team_id,
            target_id,
            risky_action,
            false,
            deferred,
        );
    }

    /// [`ask_risky_team_locked`](Self::ask_risky_team_locked) with the server-forced variant:
    /// `server_forced` lifts the owner requirement for a self-removal (`Character.SetFaction`'s
    /// `MemberRemoveFromTeam(this, this, Kick)`), which is not a client chooseable action.
    #[allow(clippy::too_many_arguments)]
    fn ask_risky_team_locked_from(
        &self,
        _server: &GameServer,
        st: &mut State,
        unit: &PlayerRef,
        team_id: u32,
        target_id: u32,
        risky_action: u8,
        server_forced: bool,
        deferred: &mut Deferred,
    ) {
        let Some(team) = (team_id != 0).then(|| st.teams.get_mut(&team_id)).flatten() else {
            return;
        };
        if !matches!(
            risky_action,
            risky_action::LEAVE | risky_action::KICK | risky_action::DISMISS
        ) || !team.is_member(unit.character_id)
            || (matches!(risky_action, risky_action::KICK | risky_action::DISMISS)
                && team.owner_id != unit.character_id
                && !(server_forced && unit.character_id == target_id))
            || (risky_action == risky_action::LEAVE && unit.character_id != target_id)
            || (risky_action != risky_action::DISMISS && !team.is_member(target_id))
        {
            return;
        }
        let mut is_auto_disband = false;
        // The member that leaves (the acting unit itself for a leave; the kicked member for a
        // kick) leaves the party / raid chat channels, while the slot still names its party.
        // Deviation from the C#, whose acting unit left them: the kicker lost its party chat and
        // the kicked member kept hearing the party (a dismissal leaves everybody below).
        if matches!(risky_action, risky_action::LEAVE | risky_action::KICK) {
            let leaving = team
                .members
                .iter()
                .flatten()
                .find(|m| m.character.character_id == target_id)
                .map(|m| m.character.clone());
            if let Some(c) = leaving {
                let owners = (team.id, team.owner_id, team.is_party);
                let party_number = team.party_number_of(target_id);
                deferred.push(move |server| {
                    if !owners.2 {
                        let raid = raid_channel(server, owners.0, owners.1);
                        leave_raid_chat_verified(server, owners.0, &raid, &c);
                    }
                    leave_party_chat_verified(server, owners.0, owners.1, party_number, &c);
                });
            }
        }

        let mut cleared = Vec::new();
        if (risky_action == risky_action::LEAVE || risky_action == risky_action::KICK)
            && team.remove_member(target_id)
        {
            if target_id == team.owner_id {
                let new_owner = team.new_owner();
                if new_owner != 0 {
                    team.owner_id = new_owner;
                    team.broadcast(
                        &SCTeamOwnerChangedPacket {
                            team_id,
                            id: new_owner,
                        },
                        0,
                    );
                } else if team.is_party {
                    // couldn't find a new leader, only party will auto-disband
                    is_auto_disband = true;
                }
            }
            team.broadcast(
                &SCTeamMemberLeavedPacket {
                    team_id,
                    id: target_id,
                    kicked: risky_action == risky_action::KICK,
                },
                0,
            );
            let kicked = risky_action == risky_action::KICK;
            deferred.push(move |server| {
                if let Some(target) = server.world.character_by_id(target_id) {
                    settle_in_party(server, &target);
                    target.send(SCLeavedTeamPacket {
                        team_id,
                        kicked,
                        dismissed: false,
                    });
                    // Events.OnTeamLeave (systems::instances dungeons).
                    crate::systems::instances::events::on_team_leave(server, &target, team_id);
                }
            });
            cleared.push(target_id);
        }
        if team.is_party && team.members_count() <= 1 {
            is_auto_disband = true;
        }
        if team.members_online_count() == 0 {
            is_auto_disband = true;
        }
        // TODO (C#): Need to find why we need this
        team.broadcast(
            &SCTeamAckRiskyActionPacket {
                team_id,
                id: target_id,
                risky_action,
                w: 0,
                error_message: 0,
            },
            0,
        );
        let mut remove = false;
        if is_auto_disband || risky_action == risky_action::DISMISS {
            team.broadcast(&SCTeamDismissedPacket { team_id }, 0);
            let members: Vec<(PlayerRef, bool, u32)> = team
                .members
                .iter()
                .flatten()
                .map(|m| {
                    (
                        m.character.clone(),
                        m.online,
                        team.party_number_of(m.character.character_id),
                    )
                })
                .collect();
            let owners = (team.id, team.owner_id, team.is_party);
            for (c, online, party_number) in members {
                cleared.push(c.character_id);
                deferred.push(move |server| {
                    if !owners.2 {
                        let raid = raid_channel(server, owners.0, owners.1);
                        leave_raid_chat_verified(server, owners.0, &raid, &c);
                    }
                    leave_party_chat_verified(server, owners.0, owners.1, party_number, &c);
                    if online {
                        c.send(SCLeavedTeamPacket {
                            team_id,
                            kicked: false,
                            dismissed: true,
                        });
                        settle_in_party(server, &c);
                        // trigger event (systems::instances dungeons)
                        crate::systems::instances::events::on_team_leave(server, &c, team_id);
                    }
                });
            }
            remove = true;
        }
        for id in cleared {
            clear_sync_state(st, id);
        }
        if remove {
            st.teams.remove(&team_id);
        }
        // TODO (C#): Add this to a timer or trigger instead of calling on a party/raid disband.
        deferred.push(|server| {
            server.chat.clean_up_channels(&server.ids.chat);
        });
    }

    /// `MakeTeamOwner(unit, teamId, memberId)`.
    pub fn make_team_owner(&self, unit: &PlayerRef, team_id: u32, member_id: u32) {
        let mut st = self.lock();
        let Some(team) = (team_id != 0).then(|| st.teams.get_mut(&team_id)).flatten() else {
            return;
        };
        if team.owner_id != unit.character_id || team.owner_id == member_id {
            return;
        }
        if team.is_member(member_id) {
            team.owner_id = member_id;
        }
        team.broadcast(
            &SCTeamOwnerChangedPacket {
                team_id: team.id,
                id: team.owner_id,
            },
            0,
        );
    }

    /// `ConvertToRaid(owner, teamId)`.
    pub fn convert_to_raid(&self, server: &GameServer, owner: &PlayerRef, team_id: u32) {
        let mut st = self.lock();
        let Some(team) = (team_id != 0).then(|| st.teams.get_mut(&team_id)).flatten() else {
            return;
        };
        if team.owner_id != owner.character_id {
            return;
        }
        team.is_party = false;
        let team = team.clone();
        drop(st);
        team.broadcast(&SCTeamBecameRaidTeamPacket { team_id: team.id }, 0);
        for m in team.members.iter().flatten() {
            join_raid_chat_verified(server, team.id, &raid_chat(server, &team), &m.character);
        }
        refresh_all_team_member_obj_ids(&team);
        // TODO (C#): Handle raids in dungeons
    }

    /// `SetTeamMemberRole(unit, teamId, memberId, role)`.
    pub fn set_team_member_role(&self, unit: &PlayerRef, team_id: u32, member_id: u32, role: u8) {
        let role = if role <= member_role::ATTACKER {
            role
        } else {
            member_role::UNDECIDED
        };
        let mut st = self.lock();
        let Some(team) = (team_id != 0).then(|| st.teams.get_mut(&team_id)).flatten() else {
            return;
        };
        if unit.character_id != member_id {
            return;
        }
        if team.change_role(member_id, role) {
            team.broadcast(
                &SCTeamMemberRoleChangedPacket {
                    team_id: team.id,
                    member_id,
                    role,
                },
                0,
            );
        }
    }

    /// `SetOverHeadMarker(unit, teamId, index, type, targetId)`.
    pub fn set_over_head_marker(
        &self,
        unit: &PlayerRef,
        team_id: u32,
        mut index: i32,
        mut kind: u8,
        mut target_id: u32,
    ) {
        let mut st = self.lock();
        let Some(team) = (team_id != 0).then(|| st.teams.get_mut(&team_id)).flatten() else {
            return;
        };
        if team.owner_id != unit.character_id && !team.is_party {
            return;
        }
        // Deviation from C#: the sender must belong to the team it names (a party lets any
        // member mark; the team id alone let any client mark another party's targets).
        if !team.is_member(unit.character_id) {
            return;
        }
        // Enum.IsDefined(OverHeadMark): -2 and 0..=11.
        if (0..=11).contains(&index) && kind <= 2 {
            team.marks[index as usize] = (kind, if kind != 0 { target_id } else { 0 });
        } else {
            team.reset_marks();
            index = OVER_HEAD_MARK_RESET_ALL;
            kind = 100;
            target_id = 0;
        }
        team.broadcast(
            &SCOverHeadMarkerSetPacket {
                team_id,
                index,
                is_obj_id: kind == 2,
                id: target_id,
            },
            0,
        );
    }

    /// `ChangeLootingRule(owner, teamId, flags, method, grade, master, rollForBop)`.
    pub fn change_looting_rule(
        &self,
        owner: &PlayerRef,
        team_id: u32,
        flags: u8,
        rule: LootingRule,
    ) {
        let mut st = self.lock();
        let Some(team) = (team_id != 0).then(|| st.teams.get_mut(&team_id)).flatten() else {
            return;
        };
        if team.owner_id != owner.character_id {
            return;
        }
        if flags & 0x08 != 0 {
            team.looting_rule.roll_for_bind_on_pickup = rule.roll_for_bind_on_pickup;
        }
        if flags & 0x04 != 0 {
            // Deviation from C#: the loot master must be a member of the team.
            if rule.loot_master == 0 || team.is_member(rule.loot_master) {
                team.looting_rule.loot_master = rule.loot_master;
            } else {
                tracing::warn!(
                    "ChangeLootingRule: {} named non-member {} as loot master of team {team_id}",
                    owner.character_id,
                    rule.loot_master
                );
            }
        }
        if flags & 0x02 != 0 {
            team.looting_rule.minimum_grade = rule.minimum_grade;
        }
        if flags & 0x01 != 0 {
            team.looting_rule.loot_method = rule.loot_method;
            if team.looting_rule.loot_method != looting_method::LOOT_MASTER {
                team.looting_rule.loot_master = 0;
            }
        }
        team.broadcast(
            &SCTeamLootingRuleChangedPacket {
                team_id,
                looting_rule: team.looting_rule,
                change_flags: flags,
            },
            0,
        );
    }

    /// `SetPingPos(unit, teamId, hasPing, position, insId)`.
    pub fn set_ping_pos(
        &self,
        unit: &PlayerRef,
        team_id: u32,
        has_ping: bool,
        position: (f32, f32, f32),
        ins_id: u32,
    ) {
        let mut st = self.lock();
        let Some(team) = (team_id != 0).then(|| st.teams.get_mut(&team_id)).flatten() else {
            return;
        };
        if team.owner_id != unit.character_id && !team.is_marked(unit.character_id) {
            return;
        }
        team.ping_position = position;
        team.broadcast(
            &SCTeamPingPosPacket {
                has_ping,
                position,
                ins_id,
            },
            0,
        );
    }

    /// `MoveTeamMember(owner, teamId, targetId, target2Id, fromIndex, toIndex)`.
    #[allow(clippy::too_many_arguments)]
    pub fn move_team_member(
        &self,
        server: &GameServer,
        owner: &PlayerRef,
        team_id: u32,
        target_id: u32,
        target2_id: u32,
        from: u8,
        to: u8,
    ) {
        let mut st = self.lock();
        let Some(team) = (team_id != 0).then(|| st.teams.get_mut(&team_id)).flatten() else {
            return;
        };
        if team.owner_id != owner.character_id {
            return;
        }
        let owners = (team.id, team.owner_id, team.is_party);
        let old_party = [
            (target_id, team.party_number_of(target_id)),
            (target2_id, team.party_number_of(target2_id)),
        ];
        let moved = team.move_member(from, to);
        let new_party = [
            (target_id, team.party_number_of(target_id)),
            (target2_id, team.party_number_of(target2_id)),
        ];
        if moved {
            team.broadcast(
                &SCTeamMemberMovedPacket {
                    team_id,
                    id_from: target_id,
                    id_to: target2_id,
                    from,
                    to,
                },
                0,
            );
        }
        drop(st);
        for (id, party_number) in old_party {
            if let Some(t) = server.world.character_by_id(id) {
                leave_party_chat_verified(server, owners.0, owners.1, party_number, &t);
            }
        }
        if moved {
            for (id, party_number) in new_party {
                if let Some(t) = server.world.character_by_id(id) {
                    join_party_chat_verified(server, owners.0, owners.1, party_number, &t);
                }
            }
        }
    }

    /// `SetOffline(unit)` (from the `Character.IsOnline` setter, after the flag is cleared).
    pub fn set_offline(&self, server: &GameServer, unit: &PlayerRef) {
        let mut deferred = Deferred::default();
        let mut st = self.lock();
        // Deviation: a party invitation does not outlive the session of the inviter or invitee.
        Self::drop_invitations(&mut st, unit.character_id);
        // Its area-invite cooldown dies with the session too.
        st.area_invite_until.remove(&unit.character_id);
        let Some(team_id) = Self::team_id_by_unit(&st, unit.character_id) else {
            drop(st);
            deferred.run(server);
            return;
        };
        let team = st.teams.get_mut(&team_id).expect("team");
        let Some(slot) = team.change_status(unit) else {
            drop(st);
            deferred.run(server);
            return;
        };
        if let Some(m) = team.members[slot].as_mut() {
            m.online = false;
        }
        if team.owner_id == unit.character_id {
            let new_owner = team.new_owner();
            if new_owner != 0 {
                team.owner_id = new_owner;
                team.broadcast(
                    &SCTeamOwnerChangedPacket {
                        team_id,
                        id: new_owner,
                    },
                    unit.character_id,
                );
            }
        }
        let member = team.members[slot].as_ref().expect("member").clone();
        let others: Vec<PlayerRef> = team
            .members
            .iter()
            .flatten()
            .filter(|m| {
                m.character.character_id != unit.character_id
                    && m.online
                    && !are_in_render_distance(&m.character, unit)
            })
            .map(|m| m.character.clone())
            .collect();
        let all_offline = team.members_online_count() == 0;
        if all_offline {
            team.all_offline_since_ms = Some(util::now_ms());
        }
        let recipients: Vec<PlayerRef> = team
            .members
            .iter()
            .flatten()
            .filter(|m| m.character.character_id != unit.character_id && m.online)
            .map(|m| m.character.clone())
            .collect();
        let unit_id = unit.character_id;
        let unit_obj = unit.obj_id;
        deferred.push(move |server| {
            send_to_players(
                &recipients,
                &SCTeamMemberDisconnectedPacket {
                    team_id,
                    id: unit_id,
                    member: member_info(server, &member),
                },
            );
            for c in others {
                c.send(SCUnitsRemovedPacket {
                    ids: vec![unit_obj],
                });
            }
        });
        clear_sync_state(&mut st, unit.character_id);
        drop(st);
        deferred.run(server);
    }

    /// `Character.SetFaction`'s party removal (`MemberRemoveFromTeam(this, this, Leave)`, as the
    /// forced leave is not a kick), looked up by membership instead of `InParty`:
    /// `CSSelectCharacter` applies an offline infamy conversion before `CSNotifyInGamePacket`
    /// runs `UpdateAtLogin` (which would otherwise re-join the new pirate), and an ordinary
    /// member could never satisfy the owner check of the client kick path.
    pub fn member_remove_for_faction_change(&self, server: &GameServer, unit: &PlayerRef) {
        let mut deferred = Deferred::default();
        let mut st = self.lock();
        let Some(team_id) = Self::team_id_by_unit(&st, unit.character_id) else {
            drop(st);
            deferred.run(server);
            return;
        };
        self.ask_risky_team_locked_from(
            server,
            &mut st,
            unit,
            team_id,
            unit.character_id,
            risky_action::LEAVE,
            true,
            &mut deferred,
        );
        drop(st);
        deferred.run(server);
    }

    /// `UpdatePosition(id)` (from `Character.SetPosition` when `InParty`).
    pub fn update_position(&self, server: &GameServer, id: u32) {
        let st = self.lock();
        let Some(team_id) = Self::team_id_by_unit(&st, id) else {
            return;
        };
        let team = &st.teams[&team_id];
        let Some(i) = team.index_of(id) else {
            return;
        };
        let member = team.members[i].as_ref().expect("member").clone();
        let recipients: Vec<PlayerRef> = team
            .members
            .iter()
            .flatten()
            .filter(|m| m.character.character_id != id && m.online)
            .map(|m| m.character.clone())
            .collect();
        drop(st);
        send_to_players(
            &recipients,
            &SCTeamRemoteMembersExPacket {
                members: vec![member_info(server, &member)],
            },
        );
    }

    /// `UpdateAtLogin(unit)` (from `CSNotifyInGamePacket`).
    pub fn update_at_login(&self, server: &GameServer, unit: &PlayerRef) {
        let mut st = self.lock();
        let Some(team_id) = Self::team_id_by_unit(&st, unit.character_id) else {
            return;
        };
        let team = st.teams.get_mut(&team_id).expect("team");
        // Cancel the zombie-team disband timer.
        team.all_offline_since_ms = None;
        if let Some(slot) = team.change_status(unit) {
            if let Some(m) = team.members[slot].as_mut() {
                m.online = true;
            }
        }
        let team = st.teams[&team_id].clone();
        let unit = unit.clone();
        drop(st);
        unit.send(joined_team_packet(server, &team));
        set_in_party_verified(server, team.id, &unit, true);
        let others: Vec<TeamMemberInfo> = team
            .members
            .iter()
            .flatten()
            .filter(|m| m.character.character_id != unit.character_id)
            .map(|m| member_info(server, m))
            .collect();
        if !others.is_empty() {
            unit.send(SCTeamRemoteMembersExPacket { members: others });
        }
        if let Some(idx) = team.index_of(unit.character_id) {
            let member = team.members[idx].as_ref().expect("member");
            let info = member_info(server, member);
            team.broadcast(
                &SCTeamMemberJoinedPacket {
                    team_id: team.id,
                    member: info.clone(),
                    party: Team::party_of(idx) as i32,
                },
                unit.character_id,
            );
            team.broadcast(
                &SCTeamRemoteMembersExPacket {
                    members: vec![info],
                },
                unit.character_id,
            );
        }
        refresh_team_member_obj_ids(&team, &unit, true);
        if !team.is_party {
            join_raid_chat_verified(server, team.id, &raid_chat(server, &team), &unit);
        }
        let party_number = team.party_number_of(unit.character_id);
        join_party_chat_verified(server, team.id, team.owner_id, party_number, &unit);
    }

    /// `UpdateAllTeams` (the 500 ms tick): zombie-team disband, then the remote-member sync
    /// for every out-of-render-distance pair of online members (delta-cached, forced every
    /// 5 s), coalesced per recipient.
    pub fn update_all_teams(&self, server: &GameServer, now_ms: i64) {
        let mut deferred = Deferred::default();
        let mut st = self.lock();
        // Pending invitations expire after 60 s; the 500 ms tick sweeps them so a target that
        // never answers cannot hold its slot (and the map stays bounded).
        let canceled = prune_expired_invitations(&mut st, now_ms);
        st.area_invite_until.retain(|_, until| *until > now_ms);
        if st.teams.is_empty() {
            drop(st);
            send_invitation_canceled(canceled);
            deferred.run(server);
            return;
        }
        let ids: Vec<u32> = st.teams.keys().copied().collect();
        let mut disbanded = false;
        let mut sync_teams: Vec<(u32, Vec<TeamMember>)> = Vec::new();
        for id in ids {
            let since = st.teams[&id].all_offline_since_ms;
            if since.is_some_and(|s| now_ms - s >= OFFLINE_TEAM_DISBAND_GRACE_MS) {
                disbanded |= Self::disband_offline_team(&mut st, id, &mut deferred);
                continue;
            }
            let team = &st.teams[&id];
            let members: Vec<TeamMember> = team
                .members
                .iter()
                .flatten()
                .filter(|m| m.online)
                .cloned()
                .collect();
            if members.len() <= 1 {
                continue;
            }
            sync_teams.push((team.id, members));
        }
        drop(st);
        send_invitation_canceled(canceled);
        // Member rows read character data, so they are built after the guard is dropped. The
        // delta claim then takes a second, short section that revalidates the membership: a
        // team whose slots moved since the snapshot is left for the next 500 ms tick.
        let rows_by_team: Vec<(u32, Vec<TeamMemberInfo>)> = sync_teams
            .iter()
            .map(|(team_id, members)| {
                let rows: Vec<TeamMemberInfo> =
                    members.iter().map(|m| member_info(server, m)).collect();
                (*team_id, rows)
            })
            .collect();
        let sends: Vec<(PlayerRef, Vec<TeamMemberInfo>)> = {
            let mut st = self.lock();
            let mut sends = Vec::new();
            for (team_id, rows) in &rows_by_team {
                let Some(team) = st.teams.get(team_id) else {
                    continue;
                };
                let online: Vec<PlayerRef> = team
                    .members
                    .iter()
                    .flatten()
                    .filter(|m| m.online)
                    .map(|m| m.character.clone())
                    .collect();
                if online.len() != rows.len()
                    || online
                        .iter()
                        .zip(rows)
                        .any(|(p, row)| p.character_id != row.id)
                {
                    continue;
                }
                // (recipient index, sources) in first-seen order.
                let mut per_recipient: Vec<(usize, Vec<usize>)> = Vec::new();
                let mut add =
                    |r: usize, s: usize| match per_recipient.iter_mut().find(|(x, _)| *x == r) {
                        Some((_, v)) => v.push(s),
                        None => per_recipient.push((r, vec![s])),
                    };
                for i in 0..online.len() {
                    for j in i + 1..online.len() {
                        if are_in_render_distance(&online[i], &online[j]) {
                            continue;
                        }
                        if try_claim_snapshot(&mut st, &rows[i], &rows[j], now_ms) {
                            add(i, j);
                        }
                        if try_claim_snapshot(&mut st, &rows[j], &rows[i], now_ms) {
                            add(j, i);
                        }
                    }
                }
                for (r, sources) in per_recipient {
                    sends.push((
                        online[r].clone(),
                        sources.into_iter().map(|s| rows[s].clone()).collect(),
                    ));
                }
            }
            sends
        };
        for (recipient, members) in sends {
            recipient.send(SCTeamRemoteMembersExPacket { members });
        }
        if disbanded {
            deferred.push(|server| {
                server.chat.clean_up_channels(&server.ids.chat);
            });
        }
        deferred.run(server);
    }

    /// `DisbandOfflineTeam(team)`: returns whether the team was removed.
    fn disband_offline_team(st: &mut State, team_id: u32, deferred: &mut Deferred) -> bool {
        let team = st.teams.get_mut(&team_id).expect("team");
        if team.members_online_count() > 0 {
            team.all_offline_since_ms = None;
            return false;
        }
        let members: Vec<(u32, PlayerRef)> = team
            .members
            .iter()
            .flatten()
            .map(|m| (m.character.character_id, m.character.clone()))
            .collect();
        st.teams.remove(&team_id);
        for (id, c) in members {
            clear_sync_state(st, id);
            deferred.push(move |server| settle_in_party(server, &c));
        }
        tracing::info!(
            "Disbanded zombie team {team_id} after {} ms of all-offline",
            OFFLINE_TEAM_DISBAND_GRACE_MS
        );
        true
    }

    /// `Team.GetNextLootWinner(eligiblePlayer, referenceLootOwner, maxRange)`: a random eligible
    /// member that has not won since the last reset (round robin).
    pub fn next_loot_winner(&self, team_id: u32, eligible: &[u32]) -> Option<PlayerRef> {
        let mut st = self.lock();
        let team = st.teams.get_mut(&team_id)?;
        let eligible_slots: Vec<usize> = team
            .members
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.as_ref()
                    .is_some_and(|m| eligible.contains(&m.character.character_id))
            })
            .map(|(i, _)| i)
            .collect();
        if eligible_slots.iter().all(|&i| {
            team.members[i]
                .as_ref()
                .is_some_and(|m| m.has_gone_round_robin)
        }) {
            for &i in &eligible_slots {
                if let Some(m) = team.members[i].as_mut() {
                    m.has_gone_round_robin = false;
                }
            }
        }
        let remaining: Vec<usize> = eligible_slots
            .into_iter()
            .filter(|&i| {
                team.members[i]
                    .as_ref()
                    .is_some_and(|m| !m.has_gone_round_robin)
            })
            .collect();
        if remaining.is_empty() {
            tracing::warn!(
                "Was unable to generate a new random looting order list for team {team_id} with leader {}",
                team.owner_id
            );
            return None;
        }
        let pick = remaining[rand::random::<usize>() % remaining.len()];
        let m = team.members[pick].as_mut().expect("member");
        m.has_gone_round_robin = true;
        Some(m.character.clone())
    }

    /// Starts the `TickManager` subscription (`Load()`): `UpdateAllTeams` every 500 ms.
    pub fn start_tick(server: &Arc<GameServer>) {
        let weak = Arc::downgrade(server);
        tokio::spawn(async move {
            let mut iv =
                tokio::time::interval(std::time::Duration::from_millis(TEAM_SYNC_INTERVAL_MS));
            loop {
                iv.tick().await;
                let Some(server) = weak.upgrade() else {
                    break;
                };
                let teams = &server.systems.social.teams;
                teams.expire_invitations(util::now_ms());
                teams.update_all_teams(&server, util::now_ms());
            }
        });
        tracing::info!("TeamManager loaded with {TEAM_SYNC_INTERVAL_MS}ms remote-sync interval");
    }
}

/// Drops the invitations older than `INVITATION_TIMEOUT_MS` and returns the invitees whose popup
/// must be closed: `(target, is_area)`.
///
/// The map holds at most one team invitation per target (a new invitation replaces an area one
/// and a named one is refused while any is pending), so an expired invitation never shares its
/// invitee with another still-pending invitation of the same popup type (type 2, shared by
/// kinds 1 and 2).
fn prune_expired_invitations(st: &mut State, now_ms: i64) -> Vec<(PlayerRef, bool)> {
    let expired: Vec<u32> = st
        .invitations
        .iter()
        .filter(|(_, inv)| inv.time_ms + INVITATION_TIMEOUT_MS < now_ms)
        .map(|(&target_id, _)| target_id)
        .collect();
    let mut canceled = Vec::with_capacity(expired.len());
    for target_id in expired {
        if let Some(inv) = st.invitations.remove(&target_id) {
            if !st.invitations.contains_key(&target_id) {
                canceled.push((inv.target, inv.is_area));
            }
        }
    }
    canceled
}

/// Sends `SCInvitationCanceled` for the invitations collected under the manager lock. Kind 1 is
/// the named and kind 2 the area team invitation: the 1-vs-2 split is UNKNOWN in the client
/// (both kinds close the same popup type 2), so this mapping is INFERRED.
fn send_invitation_canceled(canceled: Vec<(PlayerRef, bool)>) {
    for (target, is_area) in canceled {
        let kind = if is_area {
            invitation_canceled_kind::TEAM_AREA
        } else {
            invitation_canceled_kind::TEAM
        };
        target.send(SCInvitationCanceledPacket { kind });
    }
}

#[cfg(test)]
impl TeamManager {
    /// Inserts an active team (tests that need membership without the invitation flow).
    pub(crate) fn insert_team_for_test(&self, team: Team) {
        self.lock().teams.insert(team.id, team);
    }
}

/// `WorldObject` of a player (helpers for other subsystems).
pub fn as_object(p: &PlayerRef) -> WorldObject {
    WorldObject::from(p)
}

#[cfg(test)]
mod deadlock_tests;

#[cfg(test)]
mod interleave_tests;
