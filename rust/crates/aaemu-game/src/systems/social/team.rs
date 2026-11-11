//! `Core/Managers/TeamManager` + `Models/Game/Team` (parties and raids): invitations, team
//! creation, leave / kick / dismiss (`AskRiskyTeam`), owner / role / marker / looting rule /
//! ping changes, raid conversion, member moves, offline / reconnect handling and the 500 ms
//! remote-member sync with its delta cache and the zombie-team disband.
//!
//! Locking: the manager's state mutex is never held across an object data lock or a call into
//! another manager. Work that needs character data (packet bodies, chat channels, `InParty`,
//! instance events) is collected while the state is mutated and run after the guard is dropped.
//! Membership online state is kept on [`TeamMember`] (like `ExpeditionMember`), so the manager
//! can decide and broadcast without reading a `Character`. Nothing calls into the manager while
//! holding a player's data lock.

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
        // ENROLL_PARTY / ENROLL_RAID_GROUP: queued, the team manager's lock is held here.
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
        // The inviter's online flag is read between two state-lock sections: the manager must
        // not take a character data lock while it holds its mutex. The invitation itself is
        // re-read and revalidated under the second lock, so only the flag is a snapshot.
        let inviter = self
            .lock()
            .invitations
            .get(&target.character_id)
            .map(|inv| inv.owner.clone());
        let owner_online = inviter.as_ref().is_some_and(|p| p.data().is_online);
        let mut st = self.lock();
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
                        set_in_party(server, &target, true);
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
                            raid_chat(server, &team).join(&target);
                        }
                        party_chat(server, &team, target.character_id).join(&target);
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
            set_in_party(server, &owner, true);
            target.send(joined_team_packet(server, &team));
            set_in_party(server, &target, true);
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
                raid_chat(server, &team).join(&owner);
                raid_chat(server, &team).join(&target);
            }
            party_chat(server, &team, owner.character_id).join(&owner);
            party_chat(server, &team, target.character_id).join(&target);
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
        set_in_party(server, &p, as_party);
        team.broadcast(
            &SCTeamPingPosPacket {
                has_ping: true,
                position: ping,
                ins_id: 0,
            },
            0,
        );
        if !team.is_party {
            raid_chat(server, &team).join(&p);
        }
        party_chat(server, &team, p.character_id).join(&p);
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
                        raid_channel(server, owners.0, owners.1).leave(&c);
                    }
                    party_channel(server, owners.0, party_number, owners.1).leave(&c);
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
                    set_in_party(server, &target, false);
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
                        raid_channel(server, owners.0, owners.1).leave(&c);
                    }
                    party_channel(server, owners.0, party_number, owners.1).leave(&c);
                    if online {
                        c.send(SCLeavedTeamPacket {
                            team_id,
                            kicked: false,
                            dismissed: true,
                        });
                        set_in_party(server, &c, false);
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
            raid_chat(server, &team).join(&m.character);
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
                party_channel(server, owners.0, party_number, owners.1).leave(&t);
            }
        }
        if moved {
            for (id, party_number) in new_party {
                if let Some(t) = server.world.character_by_id(id) {
                    party_channel(server, owners.0, party_number, owners.1).join(&t);
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

    /// `MemberRemoveFromTeam(unit, source, leaveType)`.
    pub fn member_remove_from_team(
        &self,
        server: &GameServer,
        unit: &PlayerRef,
        source: &PlayerRef,
        leave_type: u8,
    ) {
        let mut deferred = Deferred::default();
        let mut st = self.lock();
        let Some(team_id) = Self::team_id_by_unit(&st, unit.character_id) else {
            drop(st);
            deferred.run(server);
            return;
        };
        let team = st.teams.get_mut(&team_id).expect("team");
        if team.change_status(unit).is_none() {
            return;
        }
        // `activeTeam.ChangeStatus(source)` (source must be in the same team object).
        if team.change_status(source).is_none() {
            return;
        }
        if Self::team_id_by_unit(&st, source.character_id) != Some(team_id) {
            return;
        }
        let team = &st.teams[&team_id];
        let owners = (team.id, team.owner_id, team.is_party);
        let party_number = team.party_number_of(unit.character_id);
        let leaving = unit.clone();
        deferred.push(move |server| {
            if !owners.2 {
                raid_channel(server, owners.0, owners.1).leave(&leaving);
            }
            party_channel(server, owners.0, party_number, owners.1).leave(&leaving);
        });
        self.ask_risky_team_locked(
            server,
            &mut st,
            source,
            team_id,
            unit.character_id,
            leave_type,
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
        set_in_party(server, &unit, true);
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
            raid_chat(server, &team).join(&unit);
        }
        party_chat(server, &team, unit.character_id).join(&unit);
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
            deferred.push(move |server| set_in_party(server, &c, false));
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

    /// `GetNextEligibleLooter(teamId, owner)`: the first member within 200 m of `owner_pos`
    /// that has not looted in this round, else resets the round to the first member.
    pub fn next_eligible_looter(
        &self,
        server: &GameServer,
        team_id: u32,
        owner_pos: (f32, f32, f32),
    ) -> Option<PlayerRef> {
        // Positions need character data: collect the round-robin candidates first, read the
        // positions without the team lock, then mark the chosen member under a second section
        // that revalidates by character id.
        let candidates: Vec<(PlayerRef, bool)> = {
            let st = self.lock();
            st.teams
                .get(&team_id)?
                .members
                .iter()
                .flatten()
                .map(|m| (m.character.clone(), m.has_gone_round_robin))
                .collect()
        };
        let mut chosen = None;
        for (p, gone) in &candidates {
            if *gone {
                continue;
            }
            let pos = util::world_position(server, p);
            if util::distance(pos, owner_pos, true) >= 200.0 {
                continue;
            }
            chosen = Some(p.character_id);
            break;
        }
        let mut st = self.lock();
        let team = st.teams.get_mut(&team_id)?;
        if let Some(id) = chosen {
            if let Some(m) = team
                .members
                .iter_mut()
                .flatten()
                .find(|m| m.character.character_id == id)
            {
                m.has_gone_round_robin = true;
                return Some(m.character.clone());
            }
        }
        let mut ret: Option<PlayerRef> = None;
        for m in team.members.iter_mut().flatten() {
            m.has_gone_round_robin = ret.is_none();
            if ret.is_none() {
                ret = Some(m.character.clone());
            }
        }
        ret
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
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::systems::social::achievement_hooks;
    use crate::test_support::{spawn_player, test_character, test_server, TestClient};

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
        std::thread::spawn(move || {
            // The lazily connected MySQL pool needs a Tokio context even though nothing here
            // reaches the database.
            let rt = tokio::runtime::Runtime::new().expect("runtime");
            let _guard = rt.enter();
            let server = test_server();
            crate::systems::social::install_hooks(&server);
            let (_ca, _cb, a, b) = spawn_pair(&server);
            server
                .systems
                .social
                .teams
                .ask_to_join(&server, &a, "Target", 0, true, None);
            let teams = &server.systems.social.teams;
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
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(20)).is_ok(),
            "party accept / achievement refresh / aggro tagging deadlocked"
        );
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
}
