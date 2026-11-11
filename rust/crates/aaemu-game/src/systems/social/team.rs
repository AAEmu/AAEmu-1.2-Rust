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
/// `ForcedResyncIntervalMs`.
const FORCED_RESYNC_INTERVAL_MS: i64 = 5000;
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

/// `LootingRule`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LootingRule {
    pub loot_method: u8,
    pub minimum_grade: u8,
    pub loot_master: u32,
    pub roll_for_bind_on_pickup: bool,
}

impl Default for LootingRule {
    fn default() -> Self {
        Self {
            loot_method: looting_method::ROTATE_WINNER,
            minimum_grade: 2, // Grand+
            loot_master: 0,
            roll_for_bind_on_pickup: true,
        }
    }
}

impl LootingRule {
    /// `LootingRule.Write`.
    pub fn write(&self, s: &mut aaemu_commons::PacketStream) {
        s.write_u8(self.loot_method)
            .write_u8(self.minimum_grade)
            .write_u32(self.loot_master)
            .write_bool(self.roll_for_bind_on_pickup);
    }

    /// `LootingRule.Read`.
    pub fn read(
        s: &mut aaemu_commons::PacketStream,
    ) -> std::result::Result<Self, aaemu_commons::MarshalError> {
        Ok(Self {
            loot_method: s.read_u8()?,
            minimum_grade: s.read_u8()?,
            loot_master: s.read_u32()?,
            roll_for_bind_on_pickup: s.read_bool()?,
        })
    }
}

/// `TeamMember`.
#[derive(Debug, Clone)]
pub struct TeamMember {
    /// `Character` (the object of the last session; replaced on reconnect by `ChangeStatus`).
    pub character: PlayerRef,
    /// The member's `Character.IsOnline` as last reported to the manager by
    /// `UpdateAtLogin` / `SetOffline` (a joining member is online). Kept here so the manager can
    /// decide and broadcast without taking a character data lock.
    pub online: bool,
    pub role: u8,
    pub has_gone_round_robin: bool,
}

/// `Team`.
#[derive(Debug, Clone)]
pub struct Team {
    pub id: u32,
    pub owner_id: u32,
    pub is_party: bool,
    /// `Members` (50 slots).
    pub members: Vec<Option<TeamMember>>,
    pub looting_rule: LootingRule,
    /// `MarksList` (type, obj / character id).
    pub marks: [(u8, u32); 12],
    /// `PingPosition` (x, y, z).
    pub ping_position: (f32, f32, f32),
    /// `AllOfflineSince` (unix ms; `None` = `DateTime.MinValue`).
    pub all_offline_since_ms: Option<i64>,
}

impl Team {
    pub fn new(id: u32, owner_id: u32, is_party: bool) -> Self {
        Self {
            id,
            owner_id,
            is_party,
            members: vec![None; 50],
            looting_rule: LootingRule::default(),
            marks: [(0, 0); 12],
            ping_position: (0.0, 0.0, 0.0),
            all_offline_since_ms: None,
        }
    }

    /// `ResetMarks`.
    pub fn reset_marks(&mut self) {
        self.marks = [(0, 0); 12];
    }

    /// `IsMarked(id)`.
    pub fn is_marked(&self, id: u32) -> bool {
        self.marks.iter().any(|&(_, obj)| obj == id)
    }

    /// `MembersCount`.
    pub fn members_count(&self) -> usize {
        self.members.iter().flatten().count()
    }

    /// Whether the team takes no more members (`MembersCount() >= (IsParty ? 5 : 50)`).
    pub fn is_full(&self) -> bool {
        self.members_count() >= if self.is_party { 5 } else { 50 }
    }

    /// `MembersOnlineCount`.
    pub fn members_online_count(&self) -> usize {
        self.members.iter().flatten().filter(|m| m.online).count()
    }

    /// `IsMember(id)`.
    pub fn is_member(&self, id: u32) -> bool {
        self.members
            .iter()
            .flatten()
            .any(|m| m.character.character_id == id)
    }

    /// `GetNewOwner` (first online member that is not the owner; 0 = none).
    pub fn new_owner(&self) -> u32 {
        self.members
            .iter()
            .flatten()
            .find(|m| m.character.character_id != self.owner_id && m.online)
            .map_or(0, |m| m.character.character_id)
    }

    /// `ChangeRole(id, role)`.
    pub fn change_role(&mut self, id: u32, role: u8) -> bool {
        for m in self.members.iter_mut().flatten() {
            if m.character.character_id != id {
                continue;
            }
            if m.role == role {
                return false;
            }
            m.role = role;
            return true;
        }
        false
    }

    /// `AddMember(unit)`: (slot index, party index) of the new member.
    pub fn add_member(&mut self, p: &PlayerRef) -> Option<(usize, usize)> {
        let i = self.members.iter().position(Option::is_none)?;
        self.members[i] = Some(TeamMember {
            character: p.clone(),
            // A slot is only ever filled from a live invitation / login / GM session.
            online: true,
            role: member_role::UNDECIDED,
            has_gone_round_robin: false,
        });
        Some((i, Self::party_of(i)))
    }

    /// `RemoveMember(id)`.
    pub fn remove_member(&mut self, id: u32) -> bool {
        match self.index_of(id) {
            Some(i) => {
                self.members[i] = None;
                true
            }
            None => false,
        }
    }

    /// `MoveMember(id, id2, from, to)` (out of range indices fail like the C# `catch`).
    pub fn move_member(&mut self, from: u8, to: u8) -> bool {
        let (from, to) = (usize::from(from), usize::from(to));
        if from >= self.members.len() || to >= self.members.len() {
            return false;
        }
        self.members.swap(from, to);
        true
    }

    /// `ChangeStatus(unit)`: points the member at the character's current object. The caller
    /// updates [`TeamMember::online`] with the login / logout it is handling.
    pub fn change_status(&mut self, p: &PlayerRef) -> Option<usize> {
        let i = self.index_of(p.character_id)?;
        if let Some(m) = self.members[i].as_mut() {
            m.character = p.clone();
        }
        Some(i)
    }

    /// `GetIndex(id)`.
    pub fn index_of(&self, id: u32) -> Option<usize> {
        self.members
            .iter()
            .position(|m| m.as_ref().is_some_and(|m| m.character.character_id == id))
    }

    /// `Team.GetParty(index)`.
    pub fn party_of(index: usize) -> usize {
        if index < 5 {
            0
        } else {
            index / 5
        }
    }

    /// `GetPartyCounts`.
    pub fn party_counts(&self) -> [u8; 10] {
        let mut res = [0u8; 10];
        for (i, m) in self.members.iter().enumerate() {
            if m.is_some() {
                res[Self::party_of(i)] += 1;
            }
        }
        res
    }

    /// The online member characters (slot order).
    pub fn online_members(&self) -> Vec<PlayerRef> {
        self.members
            .iter()
            .flatten()
            .filter(|m| m.online)
            .map(|m| m.character.clone())
            .collect()
    }

    /// `BroadcastPacket(packet, id)`: every online member except `exclude`.
    pub fn broadcast(&self, packet: &dyn crate::net::packet::ServerPacket, exclude: u32) {
        let mut out = crate::net::connection::Fanout::new(packet);
        for m in self.members.iter().flatten() {
            if m.character.character_id == exclude || !m.online {
                continue;
            }
            out.send(&m.character.conn);
        }
    }

    /// The party number of a character (`GetPartyChat`: slot / 5, 0 when not a member).
    pub fn party_number_of(&self, character_id: u32) -> u32 {
        self.index_of(character_id).map_or(0, |i| (i / 5) as u32)
    }
}

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

/// `MemberSyncState`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MemberSyncState {
    hp: i32,
    max_hp: i32,
    mp: i32,
    max_mp: i32,
    x: f32,
    y: f32,
    z: f32,
    obj_id: u32,
    last_sync_ms: i64,
}

impl MemberSyncState {
    /// `PositionEpsilon`.
    const POSITION_EPSILON: f32 = 0.01;

    /// `HasSameSnapshotAs`.
    fn same_snapshot(&self, o: &Self) -> bool {
        self.hp == o.hp
            && self.max_hp == o.max_hp
            && self.mp == o.mp
            && self.max_mp == o.max_mp
            && (self.x - o.x).abs() < Self::POSITION_EPSILON
            && (self.y - o.y).abs() < Self::POSITION_EPSILON
            && (self.z - o.z).abs() < Self::POSITION_EPSILON
            && self.obj_id == o.obj_id
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

/// A member snapshot for `TeamMember.Write` / `WritePerson`.
pub fn member_info(server: &GameServer, m: &TeamMember) -> TeamMemberInfo {
    let pos = util::world_position(server, &m.character);
    let c = m.character.data();
    TeamMemberInfo {
        id: c.id,
        name: c.unit.name.clone(),
        race: c.race,
        gender: c.gender,
        level: c.unit.level,
        role: m.role,
        obj_id: m.character.obj_id,
        is_online: c.is_online,
        zone_id: c.unit.transform.zone_id,
        instance_id: c.unit.transform.instance_id,
        hp: c.unit.hp,
        max_hp: c.unit.max_hp,
        mp: c.unit.mp,
        max_mp: c.unit.max_mp,
        position: pos,
        abilities: [c.ability1, c.ability2, c.ability3],
    }
}

/// `Team.Write` data.
pub fn team_info(team: &Team) -> TeamInfo {
    TeamInfo {
        id: team.id,
        owner_id: team.owner_id,
        is_party: team.is_party,
        party_counts: team.party_counts(),
        slots: team
            .members
            .iter()
            .map(|m| match m {
                Some(m) => (m.character.character_id, m.online),
                None => (0, false),
            })
            .collect(),
        marks: team.marks,
        looting_rule: team.looting_rule,
    }
}

/// `new SCJoinedTeamPacket(team)`.
fn joined_team_packet(server: &GameServer, team: &Team) -> SCJoinedTeamPacket {
    SCJoinedTeamPacket {
        team: team_info(team),
        members: team
            .members
            .iter()
            .flatten()
            .map(|m| member_info(server, m))
            .collect(),
    }
}

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

/// `AreInRenderDistance(a, b)`: the same or a neighbouring region (2-region neighbourhood).
pub fn are_in_render_distance(a: &PlayerRef, b: &PlayerRef) -> bool {
    let (Some(ra), Some(rb)) = (a.region(), b.region()) else {
        return false;
    };
    ra.0 == rb.0 && (ra.1 - rb.1).abs() <= 2 && (ra.2 - rb.2).abs() <= 2
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

/// Joins `p` to the raid `channel` of `team_id` and verifies it against the team state.
fn join_raid_chat_verified(
    server: &GameServer,
    team_id: u32,
    channel: &crate::managers::chat::ChannelRef,
    p: &PlayerRef,
) {
    channel.join(p);
    verify_chat(server, team_id, None, channel, p, true);
}

/// Removes `p` from the raid `channel` of `team_id` and verifies it against the team state.
fn leave_raid_chat_verified(
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
fn set_in_party_verified(server: &GameServer, team_id: u32, p: &PlayerRef, value: bool) {
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
fn settle_in_party(server: &GameServer, p: &PlayerRef) {
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
fn join_party_chat_verified(
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
fn leave_party_chat_verified(
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

/// `TryClaimSnapshot(recipient, source, now)`.
fn try_claim_snapshot(
    st: &mut State,
    recipient: &TeamMemberInfo,
    source: &TeamMemberInfo,
    now_ms: i64,
) -> bool {
    let key = (recipient.id, source.id);
    let next = MemberSyncState {
        hp: source.hp,
        max_hp: source.max_hp,
        mp: source.mp,
        max_mp: source.max_mp,
        x: source.position.0,
        y: source.position.1,
        z: source.position.2,
        obj_id: source.obj_id,
        last_sync_ms: now_ms,
    };
    if let Some(prev) = st.last_sync.get(&key) {
        if prev.same_snapshot(&next) && now_ms - prev.last_sync_ms < FORCED_RESYNC_INTERVAL_MS {
            return false;
        }
    }
    st.last_sync.insert(key, next);
    true
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

/// `ClearSyncState(characterId)`.
fn clear_sync_state(st: &mut State, character_id: u32) {
    st.last_sync
        .retain(|&(r, s), _| r != character_id && s != character_id);
}

/// Sends `packet` to the online members captured under the state lock, without cloning a
/// [`Team`] just to broadcast after the guard is dropped.
fn send_to_players(recipients: &[PlayerRef], packet: &dyn crate::net::packet::ServerPacket) {
    let mut out = crate::net::connection::Fanout::new(packet);
    for p in recipients {
        out.send(&p.conn);
    }
}

/// `RefreshTeamMemberObjIds(team, newMember, force)`.
fn refresh_team_member_obj_ids(team: &Team, new_member: &PlayerRef, force: bool) {
    for m in team.members.iter().flatten() {
        let c = &m.character;
        if c.character_id == new_member.character_id || !m.online {
            continue;
        }
        if !force && are_in_render_distance(c, new_member) {
            continue;
        }
        c.send(SCRefreshTeamMemberPacket {
            team_id: team.id,
            member_id: new_member.character_id,
            obj_id: new_member.obj_id,
        });
        new_member.send(SCRefreshTeamMemberPacket {
            team_id: team.id,
            member_id: c.character_id,
            obj_id: c.obj_id,
        });
    }
}

/// `RefreshAllTeamMemberObjIds(team)` (upper-triangle pairs).
fn refresh_all_team_member_obj_ids(team: &Team) {
    let online = team.online_members();
    for i in 0..online.len() {
        for j in i + 1..online.len() {
            if are_in_render_distance(&online[i], &online[j]) {
                continue;
            }
            online[i].send(SCRefreshTeamMemberPacket {
                team_id: team.id,
                member_id: online[j].character_id,
                obj_id: online[j].obj_id,
            });
            online[j].send(SCRefreshTeamMemberPacket {
                team_id: team.id,
                member_id: online[i].character_id,
                obj_id: online[i].obj_id,
            });
        }
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
mod deadlock_tests {
    //! Reproductions of the 2026-11-11 live freeze: the party-accept path held the manager
    //! state mutex and then took the inviter's character data lock, while the achievement
    //! refresh held a character data lock and then took the manager mutex. Each scenario runs
    //! on its own thread so a regression fails the test on a timeout instead of hanging the
    //! suite (the stuck threads are leaked, like the `astar_deadlock_tests`).
    use std::sync::{mpsc, TryLockError};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::systems::social::achievement_hooks;
    use crate::test_support::{
        finishes_in_time, spawn_player, test_character, test_server, TestClient,
    };

    fn spawn_pair(server: &GameServer) -> (TestClient, TestClient, PlayerRef, PlayerRef) {
        let ca = TestClient::new(server, 1, 1001);
        let cb = TestClient::new(server, 2, 1002);
        let a = spawn_player(server, &ca, test_character(1, "Owner", 100.0, 100.0), 0x100);
        let b = spawn_player(
            server,
            &cb,
            test_character(2, "Target", 101.0, 100.0),
            0x101,
        );
        a.data().is_online = true;
        b.data().is_online = true;
        (ca, cb, a, b)
    }

    /// Party accept + achievement refresh + AoE damage tagging, all at once: the cycle from
    /// the freeze needs a thread that holds the team lock while the owner's character lock is
    /// held, and the refresh as the second order.
    #[test]
    fn party_accept_refresh_and_aggro_tagging_do_not_deadlock() {
        let (tx, rx) = mpsc::channel();
        let scenario = std::thread::spawn(move || {
            // The lazily connected MySQL pool needs a Tokio context even though nothing here
            // reaches the database.
            let rt = tokio::runtime::Runtime::new().expect("runtime");
            let _guard = rt.enter();
            let server = test_server();
            crate::systems::social::install_hooks(&server);
            let (_ca, _cb, a, b) = spawn_pair(&server);
            let teams = &server.systems.social.teams;
            teams.ask_to_join(&server, &a, "Target", 0, true, None);
            // Without the invitation the accept returns before it reaches the owner's lock and
            // the scenario would pass on the old code as well.
            assert_eq!(teams.invitation_count(), 1, "the invitation was issued");
            std::thread::scope(|scope| {
                let owner_lock = a.data();
                let accept = scope.spawn(|| {
                    teams.reply_to_join_team(&server, &b, 0, true, 1, false, "", false);
                });
                // Let the accept path reach the manager lock first (the old order took it
                // before the owner's data lock); the fixed order waits for the data lock
                // before it ever takes the team lock.
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline && teams.state.try_lock().is_ok() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                // The achievement refresh used to hold the character lock and then take the
                // team lock. With the owner lock still held and the accept holding the team
                // lock, that is the two-thread cycle of the freeze.
                achievement_hooks::refresh_current(&server, &b, chrono::Utc::now());
                // AoE damage tagging on the same character takes the team lock through
                // `team_of`; it must not stay blocked behind the accept.
                let damaged = WorldObject::from(&b);
                for _ in 0..50 {
                    crate::systems::skills::combat::add_damage_aggro(
                        &server, &damaged, &damaged, 10, 10,
                    );
                }
                drop(owner_lock);
                accept.join().expect("accept thread");
            });
            // The accept went through (the owner lock is released): both are in the party.
            assert!(
                teams.active_team_by_unit(1).is_some_and(|t| t.is_member(2)),
                "the accept created the team"
            );
            assert!(
                a.data().in_party && b.data().in_party,
                "both members are marked InParty"
            );
            let _ = tx.send(());
        });
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(()) => {}
            // The scenario panicked (a postcondition failed): show its own message.
            Err(mpsc::RecvTimeoutError::Disconnected) => match scenario.join() {
                Err(panic) => std::panic::resume_unwind(panic),
                Ok(()) => panic!("the scenario ended without reporting"),
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("party accept / achievement refresh / aggro tagging deadlocked")
            }
        }
    }

    /// Deterministic order probe for the manager side: while the character lock is held,
    /// `CreateSoloTeam` must not take the team lock (the old code did, then waited for the
    /// character lock).
    #[test]
    fn create_solo_team_reads_character_data_before_the_team_lock() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = rt.enter();
        let server = test_server();
        let _c = TestClient::new(&server, 1, 1001);
        let p = spawn_player(&server, &_c, test_character(1, "Solo", 100.0, 100.0), 0x100);
        p.data().is_online = true;
        let teams = &server.systems.social.teams;
        std::thread::scope(|scope| {
            let character_lock = p.data();
            let worker = scope.spawn(|| teams.create_solo_team(&server, &p, true));
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                assert!(
                    teams.state.try_lock().is_ok(),
                    "CreateSoloTeam took the team lock while the character lock was held"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            drop(character_lock);
            worker.join().expect("solo team thread");
        });
    }

    /// Deterministic order probe for the achievement side: while the team lock is held, the
    /// refresh must not take the character lock (the old code did, then waited for the team
    /// lock).
    #[test]
    fn achievement_refresh_takes_the_team_before_the_character_lock() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = rt.enter();
        let server = test_server();
        let _c = TestClient::new(&server, 1, 1001);
        let p = spawn_player(
            &server,
            &_c,
            test_character(1, "Refresh", 100.0, 100.0),
            0x100,
        );
        p.data().is_online = true;
        let teams = &server.systems.social.teams;
        let team_lock = teams.lock();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                achievement_hooks::refresh_current(&server, &p, chrono::Utc::now());
            });
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                assert!(
                    p.data.try_lock().is_ok(),
                    "refresh_current took the character lock while waiting for the team lock"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            drop(team_lock);
            worker.join().expect("refresh thread");
        });
    }

    /// The same probe for the entry snapshot (`AchievementContent::initial_snapshot`, run by
    /// `CSNotifyInGame`): the old code took the economy lock and the character lock first and
    /// then waited for the team lock, which is the second cycle of the freeze.
    #[test]
    fn achievement_initial_snapshot_takes_the_team_before_the_character_lock() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = rt.enter();
        let server = test_server();
        let _c = TestClient::new(&server, 1, 1001);
        let p = spawn_player(
            &server,
            &_c,
            test_character(1, "Snapshot", 100.0, 100.0),
            0x100,
        );
        p.data().is_online = true;
        let teams = &server.systems.social.teams;
        let team_lock = teams.lock();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                achievement_hooks::initial_snapshot(&server, &p, chrono::Utc::now());
            });
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                assert!(
                    p.data.try_lock().is_ok(),
                    "initial_snapshot took the character lock while waiting for the team lock"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            drop(team_lock);
            worker.join().expect("snapshot thread");
        });
    }

    /// The id of the fixture team (the id manager hands out small ids, so no collision).
    const TEAM: u32 = 100;
    /// How long a worker is watched, counted from its start.
    const PROBE_WINDOW: Duration = Duration::from_millis(150);
    /// How long the state mutex may stay unavailable before it counts as held across a wait. A
    /// legitimate hold is a few microseconds of state-only work.
    const PROBE_HELD: Duration = Duration::from_millis(50);

    /// A (id 1, owner of [`TEAM`]), B (2) and C (3) are its members, D (4) is nobody's member.
    /// A and D stand together, B and C far away from them (out of render distance).
    fn lineup(server: &GameServer) -> (Vec<TestClient>, Vec<PlayerRef>) {
        let spots = [
            (100.0, 100.0),
            (1900.0, 1900.0),
            (1901.0, 1900.0),
            (101.0, 100.0),
        ];
        let mut clients = Vec::new();
        let mut players = Vec::new();
        for (i, (x, y)) in spots.into_iter().enumerate() {
            let id = i as u32 + 1;
            let client = TestClient::new(server, id, 1000 + id);
            let p = spawn_player(
                server,
                &client,
                test_character(id, &format!("P{id}"), x, y),
                0x100 + id,
            );
            p.data().is_online = true;
            clients.push(client);
            players.push(p);
        }
        let mut team = Team::new(TEAM, 1, true);
        for p in &players[..3] {
            team.add_member(p);
        }
        server.systems.social.teams.insert_team_for_test(team);
        (clients, players)
    }

    fn teams(server: &GameServer) -> &TeamManager {
        &server.systems.social.teams
    }

    fn nothing(_: &GameServer, _: &[PlayerRef]) {}

    fn always(_: &GameServer, _: &[PlayerRef]) -> bool {
        true
    }

    /// Whether member `id` of [`TEAM`] is cached online / offline.
    fn member_online(server: &GameServer, id: u32) -> Option<bool> {
        teams(server)
            .active_team(TEAM)?
            .members
            .iter()
            .flatten()
            .find(|m| m.character.character_id == id)
            .map(|m| m.online)
    }

    /// One public mutator of the manager under the generic lock-order probe.
    struct Case {
        name: &'static str,
        /// The players (indices into [`lineup`]) whose data lock the test thread holds while
        /// the mutator runs. A mutator that reads a character before its first state section
        /// (the invite and reply paths) leaves that character free, or it would block before it
        /// ever reaches the guard: those cases hold the two bystander members (B, C).
        held: &'static [usize],
        /// Runs before any lock is held.
        setup: fn(&GameServer, &[PlayerRef]),
        /// The mutator.
        op: fn(&GameServer, &[PlayerRef]),
        /// Whether the mutator reached its effect (checked once the locks are released), so a
        /// case cannot pass because the mutator returned at its first check.
        done: fn(&GameServer, &[PlayerRef]) -> bool,
    }

    fn cases() -> Vec<Case> {
        const ALL: &[usize] = &[0, 1, 2, 3];
        const BYSTANDERS: &[usize] = &[1, 2];
        // The invitation of A to D (a pending invitation for the reply / expiry cases).
        fn invite_d(s: &GameServer, p: &[PlayerRef]) {
            teams(s).ask_to_join(s, &p[0], "", 0, true, Some(&p[3]));
        }
        vec![
            Case {
                name: "ask_to_join",
                held: BYSTANDERS,
                setup: nothing,
                op: |s, p| teams(s).ask_to_join(s, &p[0], "", 0, true, Some(&p[3])),
                done: |s, _| teams(s).invitation_count() == 1,
            },
            Case {
                name: "invite_area_to_team",
                held: BYSTANDERS,
                setup: nothing,
                op: |s, p| teams(s).invite_area_to_team(s, &p[0], TEAM, true),
                // The cooldown is claimed in the guarded section after every gate.
                done: |s, _| teams(s).lock().area_invite_until.contains_key(&1),
            },
            Case {
                name: "reply_to_join_team (late join)",
                held: BYSTANDERS,
                setup: invite_d,
                op: |s, p| teams(s).reply_to_join_team(s, &p[3], 0, true, 1, false, "", false),
                done: |s, _| teams(s).is_member_now(TEAM, 4),
            },
            Case {
                name: "create_solo_team",
                held: &[0, 1, 2],
                setup: nothing,
                op: |s, p| teams(s).create_solo_team(s, &p[3], true),
                done: |s, _| teams(s).active_team_by_unit(4).is_some(),
            },
            Case {
                name: "expire_invitations",
                held: ALL,
                setup: invite_d,
                op: |s, _| teams(s).expire_invitations(util::now_ms() + INVITATION_TIMEOUT_MS + 1),
                done: |s, _| teams(s).invitation_count() == 0,
            },
            Case {
                name: "make_team_owner",
                held: ALL,
                setup: nothing,
                op: |s, p| teams(s).make_team_owner(&p[0], TEAM, 2),
                done: |s, _| teams(s).active_team(TEAM).is_some_and(|t| t.owner_id == 2),
            },
            Case {
                name: "set_team_member_role",
                held: ALL,
                setup: nothing,
                op: |s, p| teams(s).set_team_member_role(&p[1], TEAM, 2, member_role::TANK),
                done: |s, _| {
                    teams(s).active_team(TEAM).is_some_and(|t| {
                        t.members
                            .iter()
                            .flatten()
                            .any(|m| m.role == member_role::TANK)
                    })
                },
            },
            Case {
                name: "set_over_head_marker",
                held: ALL,
                setup: nothing,
                op: |s, p| teams(s).set_over_head_marker(&p[0], TEAM, 3, 2, 0x102),
                done: |s, _| {
                    teams(s)
                        .active_team(TEAM)
                        .is_some_and(|t| t.marks[3] == (2, 0x102))
                },
            },
            Case {
                name: "change_looting_rule",
                held: ALL,
                setup: nothing,
                op: |s, p| {
                    let rule = LootingRule {
                        loot_method: looting_method::FREE_FOR_ALL,
                        ..LootingRule::default()
                    };
                    teams(s).change_looting_rule(&p[0], TEAM, 0x01, rule)
                },
                done: |s, _| {
                    teams(s)
                        .active_team(TEAM)
                        .is_some_and(|t| t.looting_rule.loot_method == looting_method::FREE_FOR_ALL)
                },
            },
            Case {
                name: "set_ping_pos",
                held: ALL,
                setup: nothing,
                op: |s, p| teams(s).set_ping_pos(&p[0], TEAM, true, (1.0, 2.0, 3.0), 0),
                done: |s, _| {
                    teams(s)
                        .active_team(TEAM)
                        .is_some_and(|t| t.ping_position.0 > 0.5)
                },
            },
            Case {
                name: "convert_to_raid",
                held: ALL,
                setup: nothing,
                op: |s, p| teams(s).convert_to_raid(s, &p[0], TEAM),
                done: |s, _| teams(s).active_team(TEAM).is_some_and(|t| !t.is_party),
            },
            Case {
                name: "move_team_member",
                held: ALL,
                setup: nothing,
                op: |s, p| teams(s).move_team_member(s, &p[0], TEAM, 2, 3, 1, 2),
                done: |s, _| {
                    teams(s).active_team(TEAM).is_some_and(|t| {
                        t.members[1]
                            .as_ref()
                            .is_some_and(|m| m.character.character_id == 3)
                    })
                },
            },
            Case {
                name: "ask_risky_team (leave)",
                held: ALL,
                setup: nothing,
                op: |s, p| teams(s).ask_risky_team(s, &p[1], TEAM, 2, risky_action::LEAVE),
                done: |s, _| !teams(s).is_member_now(TEAM, 2),
            },
            Case {
                name: "ask_risky_team (kick)",
                held: ALL,
                setup: nothing,
                op: |s, p| teams(s).ask_risky_team(s, &p[0], TEAM, 3, risky_action::KICK),
                done: |s, _| !teams(s).is_member_now(TEAM, 3),
            },
            Case {
                name: "ask_risky_team (dismiss)",
                held: ALL,
                setup: nothing,
                op: |s, p| teams(s).ask_risky_team(s, &p[0], TEAM, 0, risky_action::DISMISS),
                done: |s, _| teams(s).active_team(TEAM).is_none(),
            },
            Case {
                name: "set_offline",
                held: ALL,
                setup: nothing,
                op: |s, p| teams(s).set_offline(s, &p[1]),
                done: |s, _| member_online(s, 2) == Some(false),
            },
            Case {
                name: "member_remove_for_faction_change",
                held: ALL,
                setup: nothing,
                op: |s, p| teams(s).member_remove_for_faction_change(s, &p[1]),
                done: |s, _| !teams(s).is_member_now(TEAM, 2),
            },
            Case {
                name: "update_position",
                held: ALL,
                setup: nothing,
                op: |s, _| teams(s).update_position(s, 2),
                done: always,
            },
            Case {
                name: "update_at_login",
                held: ALL,
                // B logged out before (no data lock is held yet).
                setup: |s, p| teams(s).set_offline(s, &p[1]),
                op: |s, p| teams(s).update_at_login(s, &p[1]),
                done: |s, _| member_online(s, 2) == Some(true),
            },
            Case {
                name: "update_all_teams",
                held: ALL,
                setup: nothing,
                op: |s, _| teams(s).update_all_teams(s, util::now_ms()),
                // The delta claims run in the second state section, after the rows were built.
                done: |s, _| !teams(s).lock().last_sync.is_empty(),
            },
            Case {
                name: "next_loot_winner",
                held: ALL,
                setup: nothing,
                op: |s, _| {
                    assert!(teams(s).next_loot_winner(TEAM, &[1, 2, 3]).is_some());
                },
                done: always,
            },
        ]
    }

    /// Runs one case: the test thread holds the data lock of every player in `case.held`, the
    /// mutator runs on a worker, and the state mutex must not stay held while the worker waits
    /// for one of those locks. The state is polled with `try_lock`; a few microseconds of
    /// legitimate state-only work show up as a failed poll now and then, a lock held across a
    /// wait for a character stays unavailable for good.
    fn run_case(case: Case) -> Result<(), String> {
        let server = test_server();
        let (_clients, players) = lineup(&server);
        let state = &teams(&server).state;
        (case.setup)(&server, &players);
        let verdict = std::thread::scope(|scope| {
            let locks: Vec<_> = case.held.iter().map(|&i| players[i].data()).collect();
            let (started_tx, started_rx) = mpsc::channel();
            let (server_ref, players_ref, op) = (&server, &players, case.op);
            let worker = scope.spawn(move || {
                let _ = started_tx.send(());
                op(server_ref, players_ref);
            });
            started_rx.recv().expect("the worker started");
            let end = Instant::now() + PROBE_WINDOW;
            let mut held_since: Option<Instant> = None;
            let mut verdict = Ok(());
            while Instant::now() < end && !worker.is_finished() {
                match state.try_lock() {
                    Ok(guard) => {
                        drop(guard);
                        held_since = None;
                    }
                    Err(TryLockError::WouldBlock) => {
                        let since = *held_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= PROBE_HELD {
                            verdict = Err(format!(
                                "{}: the team state mutex stayed held while the mutator waited \
                                 for a character data lock (a character lock or another manager \
                                 is taken under the guard)",
                                case.name
                            ));
                            break;
                        }
                    }
                    Err(TryLockError::Poisoned(_)) => {
                        verdict = Err(format!("{}: the team state is poisoned", case.name));
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            // Let the mutator finish: it may wait for the locks, but never forever.
            drop(locks);
            if worker.join().is_err() && verdict.is_ok() {
                verdict = Err(format!("{}: the mutator panicked", case.name));
            }
            verdict
        });
        verdict?;
        if !(case.done)(&server, &players) {
            return Err(format!(
                "{}: the mutator never reached its effect",
                case.name
            ));
        }
        Ok(())
    }

    /// Generic lock-order probe: no public mutator of the manager holds its state mutex while
    /// it waits for a character data lock. `member_info`, `set_in_party`, a chat channel or an
    /// instance event back under a guard would block on the locks the test thread holds and
    /// keep the mutex held, which is the cycle of the freeze.
    #[tokio::test]
    async fn no_public_mutator_holds_the_team_state_while_it_waits_for_a_character() {
        for case in cases() {
            let name = case.name;
            if let Err(why) = finishes_in_time(name, move || run_case(case)) {
                panic!("{why}");
            }
        }
    }
}

#[cfg(test)]
mod interleave_tests {
    //! Operations that run concurrently on the server, landed deterministically through the
    //! `interleave` seam (no thread, no timing): the deferred join effects (chat channels,
    //! `InParty`) are applied after the guard is dropped and then verified against the team
    //! state (module header), and a reply reads the inviter's online flag between two lock
    //! sections. The hook runs on the test thread exactly between the two steps.
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use crate::managers::chat::ChannelRef;
    use crate::opcodes::sc;
    use crate::test_support::{spawn_player, test_character, test_server, TestClient};

    /// Three online characters (ids 1..=3, owner first) in render distance of each other.
    fn players(server: &GameServer) -> (Vec<TestClient>, Vec<PlayerRef>) {
        let mut clients = Vec::new();
        let mut players = Vec::new();
        for id in 1..=3u32 {
            let client = TestClient::new(server, id, 1000 + id);
            let p = spawn_player(
                server,
                &client,
                test_character(id, &format!("P{id}"), 100.0 + id as f32, 100.0),
                0x100 + id,
            );
            p.data().is_online = true;
            clients.push(client);
            players.push(p);
        }
        (clients, players)
    }

    /// `owner` invites `target`, who accepts (the production flow, no hook installed).
    fn invite_and_accept(server: &GameServer, owner: &PlayerRef, target: &PlayerRef) {
        let teams = &server.systems.social.teams;
        teams.ask_to_join(server, owner, "", 0, true, Some(target));
        teams.reply_to_join_team(
            server,
            target,
            0,
            true,
            owner.character_id,
            false,
            "",
            false,
        );
    }

    /// The hook installed on this thread; it is removed when the value is dropped.
    struct Installed;

    fn install(hook: impl Fn(&TeamManager, Interleave) + 'static) -> Installed {
        let hook: InterleaveHook = Rc::new(hook);
        INTERLEAVE.with(|h| *h.borrow_mut() = Some(hook));
        Installed
    }

    impl Drop for Installed {
        fn drop(&mut self) {
            INTERLEAVE.with(|h| *h.borrow_mut() = None);
        }
    }

    /// Removes `victim` from its team the first time one of its join effects is verified (what
    /// a kick on another thread does between the apply and the verify).
    struct KickWindow {
        fired: Rc<Cell<bool>>,
        _installed: Installed,
    }

    impl KickWindow {
        fn open(victim: u32) -> Self {
            let fired = Rc::new(Cell::new(false));
            let seen = fired.clone();
            let installed = install(move |teams, at| {
                let Interleave::JoinApplied {
                    team_id,
                    character_id,
                } = at
                else {
                    return;
                };
                if character_id != victim || seen.replace(true) {
                    return;
                }
                let removed = teams
                    .lock()
                    .teams
                    .get_mut(&team_id)
                    .expect("team")
                    .remove_member(victim);
                assert!(removed, "the victim was a member until the kick");
            });
            Self {
                fired,
                _installed: installed,
            }
        }

        /// Whether a join effect of the victim reached its verify (the seam was exercised).
        fn fired(&self) -> bool {
            self.fired.get()
        }
    }

    /// Party 0 chat channel of the team (character 1 owns it; the owner only names the channel).
    fn party_zero(server: &GameServer, team_id: u32) -> ChannelRef {
        party_channel(server, team_id, 0, 1)
    }

    /// The chat frames (join / leave) of a drained connection, in order.
    fn chat_ops(client: &mut TestClient) -> Vec<u16> {
        client
            .drain_ops()
            .into_iter()
            .filter(|op| {
                *op == sc::SCJoinedChatChannelPacket || *op == sc::SCLeavedChatChannelPacket
            })
            .collect()
    }

    #[tokio::test]
    async fn is_member_now_follows_the_team_state() {
        let server = test_server();
        let (_clients, p) = players(&server);
        let teams = &server.systems.social.teams;
        invite_and_accept(&server, &p[0], &p[1]);
        let team_id = teams.active_team_by_unit(1).expect("team").id;
        assert!(teams.is_member_now(team_id, 1));
        assert!(teams.is_member_now(team_id, 2));
        assert!(!teams.is_member_now(team_id, 3), "not invited");
        assert!(!teams.is_member_now(team_id + 100, 1), "no such team");
        assert!(!teams.is_member_now(0, 1), "team 0 is nobody's team");
        teams.ask_risky_team(&server, &p[0], team_id, 2, risky_action::KICK);
        assert!(!teams.is_member_now(team_id, 2), "kicked");
    }

    /// A member that stays a member keeps every effect (the verify must not undo the normal
    /// path): the party chat and `InParty` of the whole party.
    #[tokio::test]
    async fn effects_of_members_are_kept() {
        let server = test_server();
        let (mut clients, p) = players(&server);
        invite_and_accept(&server, &p[0], &p[1]);
        invite_and_accept(&server, &p[0], &p[2]);
        let team_id = server
            .systems
            .social
            .teams
            .active_team_by_unit(1)
            .expect("team")
            .id;
        let channel = party_zero(&server, team_id);
        for (client, player) in clients.iter_mut().zip(&p) {
            assert!(player.data().in_party);
            assert!(channel.contains(player));
            assert_eq!(
                chat_ops(client),
                vec![sc::SCJoinedChatChannelPacket],
                "joined once and never left"
            );
        }
    }

    /// `reply_to_join_team` into an existing team: the accepted member is kicked after its
    /// state change and before its effects are verified.
    #[tokio::test]
    async fn a_late_join_of_a_member_that_left_meanwhile_is_taken_back() {
        let server = test_server();
        let (mut clients, p) = players(&server);
        invite_and_accept(&server, &p[0], &p[1]);
        let team_id = server
            .systems
            .social
            .teams
            .active_team_by_unit(1)
            .expect("team")
            .id;
        clients.iter_mut().for_each(|c| {
            c.drain();
        });
        let window = KickWindow::open(3);
        invite_and_accept(&server, &p[0], &p[2]);
        assert!(window.fired(), "the late joiner reached its verify");
        let channel = party_zero(&server, team_id);
        assert!(!p[2].data().in_party, "InParty is taken back");
        assert!(!channel.contains(&p[2]), "the party chat is taken back");
        assert_eq!(
            chat_ops(&mut clients[2]),
            vec![sc::SCJoinedChatChannelPacket, sc::SCLeavedChatChannelPacket]
        );
        // The members that are still in the team keep theirs.
        assert!(channel.contains(&p[0]) && channel.contains(&p[1]));
        assert!(p[0].data().in_party && p[1].data().in_party);
    }

    /// `CreateNewTeam` through `reply_to_join_team`: the invited member is kicked before its
    /// effects are verified; the owner keeps its own.
    #[tokio::test]
    async fn a_new_team_member_that_left_meanwhile_is_taken_back() {
        let server = test_server();
        let (_clients, p) = players(&server);
        let teams = &server.systems.social.teams;
        let window = KickWindow::open(2);
        invite_and_accept(&server, &p[0], &p[1]);
        assert!(window.fired());
        let team_id = teams.active_team_by_unit(1).expect("team").id;
        let channel = party_zero(&server, team_id);
        assert!(!p[1].data().in_party && !channel.contains(&p[1]));
        assert!(p[0].data().in_party && channel.contains(&p[0]));
    }

    /// `UpdateAtLogin` of a member that was kicked in the meantime.
    #[tokio::test]
    async fn a_login_of_a_member_that_left_meanwhile_is_taken_back() {
        let server = test_server();
        let (_clients, p) = players(&server);
        let teams = &server.systems.social.teams;
        invite_and_accept(&server, &p[0], &p[1]);
        let team_id = teams.active_team_by_unit(1).expect("team").id;
        let window = KickWindow::open(2);
        teams.update_at_login(&server, &p[1]);
        assert!(window.fired());
        let channel = party_zero(&server, team_id);
        assert!(!p[1].data().in_party && !channel.contains(&p[1]));
        assert!(p[0].data().in_party && channel.contains(&p[0]));
    }

    /// `ConvertToRaid` joins every member to the raid chat; one was kicked meanwhile.
    #[tokio::test]
    async fn a_raid_conversion_skips_a_member_that_left_meanwhile() {
        let server = test_server();
        let (_clients, p) = players(&server);
        let teams = &server.systems.social.teams;
        invite_and_accept(&server, &p[0], &p[1]);
        let team_id = teams.active_team_by_unit(1).expect("team").id;
        let window = KickWindow::open(2);
        teams.convert_to_raid(&server, &p[0], team_id);
        assert!(window.fired());
        let raid = raid_channel(&server, team_id, 1);
        assert!(raid.contains(&p[0]));
        assert!(!raid.contains(&p[1]), "the raid chat is taken back");
    }

    /// `CreateSoloTeam` (GM `soloparty`) for a character that left its team meanwhile.
    #[tokio::test]
    async fn a_solo_team_member_that_left_meanwhile_is_taken_back() {
        let server = test_server();
        let (_clients, p) = players(&server);
        let teams = &server.systems.social.teams;
        let window = KickWindow::open(1);
        teams.create_solo_team(&server, &p[0], true);
        assert!(window.fired());
        assert!(!p[0].data().in_party);
        let channels = server.chat.list_all_channels();
        assert!(channels.iter().all(|c| !c.contains(&p[0])));
    }

    /// `MoveTeamMember` joins the two characters the client names to their new party chat; the
    /// client can name any online character, so only members may keep the join.
    #[tokio::test]
    async fn a_member_move_never_joins_a_non_member_to_the_party_chat() {
        let server = test_server();
        let (_clients, p) = players(&server);
        let teams = &server.systems.social.teams;
        invite_and_accept(&server, &p[0], &p[1]);
        let team_id = teams.active_team_by_unit(1).expect("team").id;
        // Slot 1 <-> 0 with a member and an outsider (character 3) named as the two targets.
        teams.move_team_member(&server, &p[0], team_id, 2, 3, 1, 0);
        let channel = party_zero(&server, team_id);
        assert!(channel.contains(&p[1]), "the named member is re-joined");
        assert!(!channel.contains(&p[2]), "the outsider is not");
        // The kicked-in-the-window variant: the named member leaves before the verify.
        let window = KickWindow::open(2);
        teams.move_team_member(&server, &p[0], team_id, 2, 0, 0, 1);
        assert!(window.fired());
        assert!(!channel.contains(&p[1]), "the move join is taken back");
    }

    /// A `MoveTeamMember` that moves nobody (a slot out of range) used to take its two characters
    /// out of their party chat anyway; the verify of the leave puts them back.
    #[tokio::test]
    async fn a_move_that_moves_nobody_keeps_the_party_chat() {
        let server = test_server();
        let (_clients, p) = players(&server);
        let teams = &server.systems.social.teams;
        invite_and_accept(&server, &p[0], &p[1]);
        let team_id = teams.active_team_by_unit(1).expect("team").id;
        let channel = party_zero(&server, team_id);
        assert!(channel.contains(&p[0]) && channel.contains(&p[1]));
        teams.move_team_member(&server, &p[0], team_id, 1, 2, 0, 200);
        assert!(
            channel.contains(&p[0]) && channel.contains(&p[1]),
            "nobody moved, nobody left the party chat"
        );
    }

    /// A pending invitation of `inviter` for `target` (what `invite_locked` inserts).
    fn invitation(inviter: &PlayerRef, target: &PlayerRef) -> Invitation {
        Invitation {
            team_id: 0,
            owner: inviter.clone(),
            target: target.clone(),
            time_ms: util::now_ms(),
            is_party: true,
            is_area: false,
        }
    }

    /// `ReplyToJoinTeam` reads the inviter's online flag between two lock sections. When the
    /// invitation is replaced by another inviter's in between, the flag belongs to the old
    /// inviter: the reply reads it again instead of answering `TEAM_INVITOR_OFFLINE` and
    /// dropping the fresh invitation.
    #[tokio::test]
    async fn a_reply_reads_the_inviter_flag_again_when_the_invitation_was_replaced() {
        let server = test_server();
        let (mut clients, p) = players(&server);
        let teams = &server.systems.social.teams;
        // P1 invites P2 and goes offline: its flag is what the first read sees.
        teams.ask_to_join(&server, &p[0], "", 0, true, Some(&p[1]));
        p[0].data().is_online = false;
        // P3's invitation replaces it between the two lock sections of P2's reply.
        let replaced = Rc::new(Cell::new(false));
        let seen = replaced.clone();
        let (invitee, inviter) = (p[1].clone(), p[2].clone());
        let _installed = install(move |teams, at| {
            if at == (Interleave::InviterRead { target_id: 2 }) && !seen.replace(true) {
                let fresh = invitation(&inviter, &invitee);
                teams.lock().invitations.insert(2, fresh);
            }
        });
        clients[1].drain();
        teams.reply_to_join_team(&server, &p[1], 0, true, 3, false, "", false);
        assert!(
            replaced.get(),
            "the replacement landed between the sections"
        );
        let team = teams.active_team_by_unit(2).expect("P2 joined P3's team");
        assert!(team.is_member(3) && !team.is_member(1));
        assert!(
            !clients[1].drain_ops().contains(&sc::SCErrorMsgPacket),
            "no TEAM_INVITOR_OFFLINE for the stale flag"
        );
        assert_eq!(teams.invitation_count(), 0, "the invitation was consumed");
    }

    /// An invitation that keeps changing under the reply is not answered at all (like the
    /// answer of a stale popup): no team, no error, the pending invitation stays.
    #[tokio::test]
    async fn a_reply_is_ignored_when_the_invitation_keeps_changing() {
        let server = test_server();
        let (mut clients, p) = players(&server);
        let teams = &server.systems.social.teams;
        teams.ask_to_join(&server, &p[0], "", 0, true, Some(&p[1]));
        let reads = Rc::new(Cell::new(0u32));
        let counted = reads.clone();
        let (invitee, inviters) = (p[1].clone(), [p[2].clone(), p[0].clone()]);
        let _installed = install(move |teams, at| {
            if at != (Interleave::InviterRead { target_id: 2 }) {
                return;
            }
            // P3, then P1 again: the invitation differs from every snapshot.
            let n = counted.replace(counted.get() + 1) as usize;
            let fresh = invitation(&inviters[n % 2], &invitee);
            teams.lock().invitations.insert(2, fresh);
        });
        clients[1].drain();
        teams.reply_to_join_team(&server, &p[1], 0, true, 3, false, "", false);
        assert_eq!(reads.get(), 2, "one retry, then it gives up");
        assert!(teams.active_team_by_unit(2).is_none());
        assert!(clients[1].drain().is_empty());
        assert_eq!(teams.invitation_count(), 1, "the pending invitation stays");
    }

    /// What another team's accept does between a removal's state change and the removal's
    /// effects: the victim becomes a member of a new team (`team_id`, owned by `owner`) and that
    /// team's join effects ran (`InParty`). Fires once, when the effects of the next operation
    /// start.
    struct AdoptWindow {
        fired: Rc<Cell<bool>>,
        _installed: Installed,
    }

    impl AdoptWindow {
        fn open(victim: &PlayerRef, owner: &PlayerRef, team_id: u32) -> Self {
            let fired = Rc::new(Cell::new(false));
            let seen = fired.clone();
            let (victim, owner) = (victim.clone(), owner.clone());
            let installed = install(move |teams, at| {
                if at != Interleave::EffectsStart || seen.replace(true) {
                    return;
                }
                let mut team = Team::new(team_id, owner.character_id, true);
                team.add_member(&owner);
                team.add_member(&victim);
                teams.lock().teams.insert(team_id, team);
                victim.data().in_party = true;
            });
            Self {
                fired,
                _installed: installed,
            }
        }

        fn fired(&self) -> bool {
            self.fired.get()
        }
    }

    /// A kick's effects run after its guard is dropped, so another team can take the character
    /// first. The cleanup of the old team must not clear the flag of the team that has it now.
    #[tokio::test]
    async fn a_kick_cleanup_keeps_the_flag_of_the_team_that_took_the_character_meanwhile() {
        let server = test_server();
        let (_clients, p) = players(&server);
        let teams = &server.systems.social.teams;
        invite_and_accept(&server, &p[0], &p[1]);
        let team_a = teams.active_team_by_unit(1).expect("team").id;
        let window = AdoptWindow::open(&p[1], &p[2], 900);
        teams.ask_risky_team(&server, &p[0], team_a, 2, risky_action::KICK);
        assert!(window.fired(), "team 900 took P2 before the effects ran");
        assert!(teams.is_member_now(900, 2) && !teams.is_member_now(team_a, 2));
        assert!(
            p[1].data().in_party,
            "InParty follows the current membership"
        );
        // The party fell apart with its last follower: the kicker's flag is cleared.
        assert!(!p[0].data().in_party);
    }

    /// The same for a dismissal (the disband loop of `ask_risky_team`).
    #[tokio::test]
    async fn a_dismissal_cleanup_keeps_the_flag_of_the_team_that_took_the_character_meanwhile() {
        let server = test_server();
        let (_clients, p) = players(&server);
        let teams = &server.systems.social.teams;
        invite_and_accept(&server, &p[0], &p[1]);
        let team_a = teams.active_team_by_unit(1).expect("team").id;
        let window = AdoptWindow::open(&p[1], &p[2], 900);
        teams.ask_risky_team(&server, &p[0], team_a, 0, risky_action::DISMISS);
        assert!(window.fired(), "team 900 took P2 before the effects ran");
        assert!(teams.active_team(team_a).is_none());
        assert!(
            p[1].data().in_party,
            "InParty follows the current membership"
        );
        assert!(!p[0].data().in_party);
    }

    /// The same for the zombie-team disband of the 500 ms tick.
    #[tokio::test]
    async fn a_zombie_disband_cleanup_keeps_the_flag_of_the_team_that_took_the_character_meanwhile()
    {
        let server = test_server();
        let (_clients, p) = players(&server);
        let teams = &server.systems.social.teams;
        invite_and_accept(&server, &p[0], &p[1]);
        let team_a = teams.active_team_by_unit(1).expect("team").id;
        crate::world::session::set_online(&server, &p[0], false);
        crate::world::session::set_online(&server, &p[1], false);
        let now = util::now_ms();
        teams.update_all_teams(&server, now);
        assert!(
            teams.active_team(team_a).is_some(),
            "inside the grace period"
        );
        let window = AdoptWindow::open(&p[1], &p[2], 900);
        teams.update_all_teams(&server, now + OFFLINE_TEAM_DISBAND_GRACE_MS);
        assert!(window.fired(), "team 900 took P2 before the effects ran");
        assert!(teams.active_team(team_a).is_none());
        assert!(
            p[1].data().in_party,
            "InParty follows the current membership"
        );
        assert!(!p[0].data().in_party);
    }

    /// A join effect that is undone because its character is no longer a member of the team must
    /// leave `InParty` at the truth too: the character is a member of another team now.
    #[tokio::test]
    async fn an_undone_join_keeps_the_flag_of_the_team_the_character_is_in_now() {
        let server = test_server();
        let (_clients, p) = players(&server);
        let teams = &server.systems.social.teams;
        invite_and_accept(&server, &p[0], &p[1]);
        let team_a = teams.active_team_by_unit(1).expect("team").id;
        // P3's accept into team A is verified after P3 was kicked and taken by team 900.
        let moved = Rc::new(Cell::new(false));
        let seen = moved.clone();
        let victim = p[2].clone();
        let _installed = install(move |teams, at| {
            let Interleave::JoinApplied {
                team_id,
                character_id: 3,
            } = at
            else {
                return;
            };
            if seen.replace(true) {
                return;
            }
            let mut st = teams.lock();
            assert!(st.teams.get_mut(&team_id).expect("team").remove_member(3));
            let mut team = Team::new(900, victim.character_id, true);
            team.add_member(&victim);
            st.teams.insert(900, team);
        });
        invite_and_accept(&server, &p[0], &p[2]);
        assert!(moved.get());
        assert!(teams.is_member_now(900, 3) && !teams.is_member_now(team_a, 3));
        assert!(p[2].data().in_party, "a member of team 900 stays in party");
    }

    /// A kick's chat leaves run after its guard is dropped, so the member can be invited back
    /// first (another marked raid member invites it and it accepts). The cleanup must not remove
    /// the chat memberships that newer join restored.
    #[tokio::test]
    async fn a_kick_cleanup_keeps_the_chat_a_member_regained_meanwhile() {
        let server = test_server();
        let (_clients, p) = players(&server);
        let teams = &server.systems.social.teams;
        invite_and_accept(&server, &p[0], &p[1]);
        let team_id = teams.active_team_by_unit(1).expect("team").id;
        teams.convert_to_raid(&server, &p[0], team_id);
        let raid = raid_channel(&server, team_id, 1);
        let party = party_zero(&server, team_id);
        assert!(raid.contains(&p[1]) && party.contains(&p[1]));
        // The newer join: P2 is a member again (the freed slot, so the same party) and its join
        // effects ran before the kick's effects start.
        let landed = Rc::new(Cell::new(false));
        let seen = landed.clone();
        let (rejoined, raid_now, party_now) = (p[1].clone(), raid.clone(), party.clone());
        let _installed = install(move |teams, at| {
            if at != Interleave::EffectsStart || seen.replace(true) {
                return;
            }
            let slot = teams
                .lock()
                .teams
                .get_mut(&team_id)
                .expect("team")
                .add_member(&rejoined);
            assert!(slot.is_some(), "the kicked member's slot is free");
            raid_now.join(&rejoined);
            party_now.join(&rejoined);
        });
        teams.ask_risky_team(&server, &p[0], team_id, 2, risky_action::KICK);
        assert!(landed.get(), "the rejoin landed before the kick's effects");
        assert!(teams.is_member_now(team_id, 2));
        assert!(
            raid.contains(&p[1]) && party.contains(&p[1]),
            "the old cleanup keeps the memberships of the newer join"
        );
    }

    /// `settle_in_party` reads the membership again after its write: a team that took the
    /// character in between (its own effects not run yet) is followed, not overwritten.
    #[tokio::test]
    async fn settling_follows_a_membership_change_that_lands_after_the_write() {
        let server = test_server();
        let (_clients, p) = players(&server);
        let landed = Rc::new(Cell::new(false));
        let seen = landed.clone();
        let victim = p[1].clone();
        let _installed = install(move |teams, at| {
            if at != (Interleave::InPartyWritten { character_id: 2 }) || seen.replace(true) {
                return;
            }
            let mut team = Team::new(900, 2, true);
            team.add_member(&victim);
            teams.lock().teams.insert(900, team);
        });
        // The flag of a team the character has left.
        p[1].data().in_party = true;
        settle_in_party(&server, &p[1]);
        assert!(
            landed.get(),
            "a team took the character after the first write"
        );
        assert!(
            p[1].data().in_party,
            "the second pass wrote the new membership"
        );
    }

    /// A raid with P1 and P2 where P2 sits in party 1 (slot 5): a party chat join prepared for
    /// party 0 (the slot P2 had when the join was queued) is stale, one for party 1 is not, and a
    /// character that is no member never keeps a join.
    #[tokio::test]
    async fn a_party_chat_join_for_a_party_the_member_left_is_taken_back() {
        let server = test_server();
        let (_clients, p) = players(&server);
        let teams = &server.systems.social.teams;
        invite_and_accept(&server, &p[0], &p[1]);
        let team_id = teams.active_team_by_unit(1).expect("team").id;
        teams.convert_to_raid(&server, &p[0], team_id);
        assert!(teams
            .lock()
            .teams
            .get_mut(&team_id)
            .expect("team")
            .move_member(1, 5));
        assert_eq!(teams.party_number_now(team_id, 2), Some(1));
        assert_eq!(teams.party_number_now(team_id, 3), None, "not a member");
        let (zero, one) = (
            party_channel(&server, team_id, 0, 1),
            party_channel(&server, team_id, 1, 1),
        );
        // The move's own effects: P2 left its old party chat.
        zero.leave(&p[1]);

        join_party_chat_verified(&server, team_id, 1, 0, &p[1]);
        assert!(
            !zero.contains(&p[1]),
            "the stale join of party 0 is taken back"
        );
        join_party_chat_verified(&server, team_id, 1, 1, &p[1]);
        assert!(one.contains(&p[1]), "the join of its current party stays");
        join_party_chat_verified(&server, team_id, 1, 0, &p[2]);
        assert!(!zero.contains(&p[2]), "a non-member keeps nothing");
    }

    /// The accept of a raid member runs its effects while the raid owner moves the member to
    /// another party: the move's leave of the old party chat has already run, so the accept's
    /// join of the old party (from its team copy) is stale and is taken back.
    #[tokio::test]
    async fn an_accept_does_not_keep_the_old_party_chat_of_a_member_moved_meanwhile() {
        let server = test_server();
        let (_clients, p) = players(&server);
        let teams = &server.systems.social.teams;
        invite_and_accept(&server, &p[0], &p[1]);
        let team_id = teams.active_team_by_unit(1).expect("team").id;
        teams.convert_to_raid(&server, &p[0], team_id);
        let zero = party_channel(&server, team_id, 0, 1);
        let moved = Rc::new(Cell::new(false));
        let seen = moved.clone();
        let (channel, joiner) = (zero.clone(), p[2].clone());
        let _installed = install(move |teams, at| {
            let Interleave::JoinApplied {
                team_id,
                character_id: 3,
            } = at
            else {
                return;
            };
            // The stale join of party 0 was just applied; the raid owner's move lands now.
            if !channel.contains(&joiner) || seen.replace(true) {
                return;
            }
            let mut st = teams.lock();
            let team = st.teams.get_mut(&team_id).expect("team");
            let slot = team.index_of(3).expect("member");
            assert!(team.move_member(slot as u8, 5));
        });
        invite_and_accept(&server, &p[0], &p[2]);
        assert!(
            moved.get(),
            "the move landed between the join and its verify"
        );
        assert_eq!(teams.party_number_now(team_id, 3), Some(1));
        assert!(!zero.contains(&p[2]), "the old party chat is not kept");
    }
}
