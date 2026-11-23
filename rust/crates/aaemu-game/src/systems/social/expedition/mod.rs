//! `Core/Managers/ExpeditionManager` + `Models/Game/Expeditions` (guilds): loading guilds /
//! members / role policies, `GetExpedition`, `SendExpeditions`, `SendExpeditionInfo`, member
//! status refresh on login / logout / zone change, and guild management (create, invite /
//! reply, leave, kick, member roles, owner change, role policies, disband) with the
//! `Expedition.Save` persistence. (Moved here from `managers::expedition`, which re-exports
//! this module.)

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError, RwLock};

use aaemu_commons::{Marshal, PacketStream};
use sqlx::mysql::MySqlPool;
use sqlx::{MySql, Row, Transaction};

use crate::data::world_data::SystemFaction;
use crate::db::{dt, from_unix, int, unix};
use crate::error::Result;
use crate::models::character::convert_long;
use crate::models::item::UnixTime;
use crate::packets::world::{
    SCExpeditionMemberListPacket, SCExpeditionMemberStatusChangedPacket,
    SCExpeditionRolePolicyListPacket, SCFactionListPacket,
};
use crate::server::GameServer;
use crate::systems::social::nation::Nation;
use crate::systems::social::packets::expedition::*;
use crate::systems::social::packets::misc::{invitation_canceled_kind, SCInvitationCanceledPacket};
use crate::systems::social::rename;
use crate::systems::social::util::{self, error, send_error};
use crate::world::{broadcast, PlayerRef};

use super::nation;

mod creation;
mod invitations;
mod membership;
mod naming;
mod ownership;
mod persistence;

pub use self::creation::{create_expedition, valid_sponsors};
pub use self::invitations::{invite, reply_invite};
use self::membership::remove_member;
pub use self::membership::{
    change_member_role, change_role_policy, kick, leave, leave_forced, leave_offline,
};
pub use self::naming::{
    gm_rename, grant_rename, offer_expedition_rename_ui, prompt_rename_if_allowed, rename,
};
pub use self::ownership::{change_owner, disband};
use self::ownership::{dismiss_expedition, forced_owner_leave, transfer_ownership};
use self::persistence::save_expedition_state;
pub use self::persistence::{save, save_expedition};

/// `ExpeditionMember`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpeditionMember {
    pub expedition_id: u32,
    pub character_id: u32,
    pub in_party: bool,
    pub is_online: bool,
    pub last_world_leave_time: UnixTime,
    pub name: String,
    pub level: u8,
    pub zone_id: u32,
    pub id3: u32,
    pub abilities: [u8; 3],
    pub role: u8,
    pub position: (f32, f32, f32),
    pub memo: String,
    pub transfer_requested_time: UnixTime,
}

impl Marshal for ExpeditionMember {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.expedition_id)
            .write_u32(self.character_id)
            .write_bool(self.in_party)
            .write_bool(self.is_online)
            .write_i64(self.last_world_leave_time)
            .write_str(&self.name)
            .write_u8(self.level)
            .write_u32(self.zone_id)
            .write_u32(self.id3);
        for a in self.abilities {
            s.write_u8(a);
        }
        s.write_u8(self.role)
            .write_i64(convert_long(self.position.0))
            .write_i64(convert_long(self.position.1))
            .write_f32(self.position.2)
            .write_str(&self.memo)
            .write_i64(self.transfer_requested_time);
    }
}

/// `ExpeditionRolePolicy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpeditionRolePolicy {
    pub expedition_id: u32,
    pub role: u8,
    pub name: String,
    pub dominion_declare: bool,
    pub invite: bool,
    pub expel: bool,
    pub promote: bool,
    pub dismiss: bool,
    pub chat: bool,
    pub manager_chat: bool,
    pub siege_master: bool,
    pub join_siege: bool,
}

impl Marshal for ExpeditionRolePolicy {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.expedition_id)
            .write_u8(self.role)
            .write_str(&self.name)
            .write_bool(self.dominion_declare)
            .write_bool(self.invite)
            .write_bool(self.expel)
            .write_bool(self.promote)
            .write_bool(self.dismiss)
            .write_bool(self.chat)
            .write_bool(self.manager_chat)
            .write_bool(self.siege_master)
            .write_bool(self.join_siege);
    }
}

impl ExpeditionRolePolicy {
    /// `ExpeditionRolePolicy.Read`.
    pub fn read(s: &mut PacketStream) -> std::result::Result<Self, aaemu_commons::MarshalError> {
        Ok(Self {
            expedition_id: s.read_u32()?,
            role: s.read_u8()?,
            name: s.read_string()?,
            dominion_declare: s.read_bool()?,
            invite: s.read_bool()?,
            expel: s.read_bool()?,
            promote: s.read_bool()?,
            dismiss: s.read_bool()?,
            chat: s.read_bool()?,
            manager_chat: s.read_bool()?,
            siege_master: s.read_bool()?,
            join_siege: s.read_bool()?,
        })
    }
}

/// `Expedition` (a `SystemFaction` with members and policies).
#[derive(Debug, Clone, PartialEq)]
pub struct Expedition {
    pub faction: SystemFaction,
    pub members: Vec<ExpeditionMember>,
    pub policies: Vec<ExpeditionRolePolicy>,
    /// `_removedMembers` (character ids, flushed by `Save`).
    pub removed_members: Vec<u32>,
    /// `isDisbanded`.
    pub is_disbanded: bool,
}

impl Expedition {
    /// `GetPolicyByRole(role)`.
    pub fn policy_by_role(&self, role: u8) -> Option<&ExpeditionRolePolicy> {
        self.policies.iter().find(|p| p.role == role)
    }

    /// `GetMember(characterId)`.
    pub fn member(&self, character_id: u32) -> Option<&ExpeditionMember> {
        self.members.iter().find(|m| m.character_id == character_id)
    }

    fn member_mut(&mut self, character_id: u32) -> Option<&mut ExpeditionMember> {
        self.members
            .iter_mut()
            .find(|m| m.character_id == character_id)
    }

    /// The member list of `Members.Remove(member)` + `_removedMembers.Add(member.CharacterId)`
    /// (the guild chat part of `RemoveMember` is done by the caller).
    fn remove_member(&mut self, character_id: u32) {
        if let Some(i) = self
            .members
            .iter()
            .position(|m| m.character_id == character_id)
        {
            self.members.remove(i);
        }
        self.removed_members.push(character_id);
    }

    pub fn id(&self) -> u32 {
        self.faction.id
    }

    pub fn member_ids(&self) -> Vec<u32> {
        self.members.iter().map(|m| m.character_id).collect()
    }
}

/// Values a member refresh copies from the character (`ExpeditionMember.Refresh`).
#[derive(Debug, Clone)]
pub struct MemberRefresh {
    pub character_id: u32,
    pub name: String,
    pub level: u8,
    pub zone_id: u32,
    pub abilities: [u8; 3],
    pub position: (f32, f32, f32),
}

/// `ExpeditionManager`.
#[derive(Debug, Default)]
pub struct ExpeditionManager {
    /// `_expeditions.Values` in insertion order.
    expeditions: RwLock<Vec<Expedition>>,
    /// Pending guild invitations by invited character id. Deviation from the C# manager, which
    /// kept none and let any client join any guild (see PORTING.md).
    pub(crate) invitations: Mutex<HashMap<u32, PendingInvite>>,
    /// Characters the server has offered a guild rename to (`SCExpeditionShowRenameUI`), and
    /// the guild the offer belongs to. The 1.2 client can only send
    /// `CSRenameExpeditionPacket` from that dialog, so an offer is the server-side proof that
    /// the rename came through the intended flow (and cannot outlive the guild it was made for).
    rename_offers: Mutex<HashMap<u32, u32>>,
}

/// `ExpeditionMember.Role` of the guild leader.
const LEADER_ROLE: u8 = 255;

/// How long a guild invitation can be answered (same as the party / raid invitations).
pub(crate) const INVITATION_TIMEOUT_MS: i64 = 60_000;
/// How often the guild-invitation expiry sweep runs (the C# pruned only on the next invite).
const INVITATION_SWEEP_MS: u64 = 1_000;

/// Upper bound of pending rename offers (GM-triggered); a full set is cleared so it never grows
/// without bound when characters leave without a logout packet.
const MAX_RENAME_OFFERS: usize = 256;

/// An unanswered `SCExpeditionInvitationPacket`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingInvite {
    expedition_id: u32,
    /// The character that sent the invitation (told when it is refused).
    inviter_id: u32,
    pub(crate) time_ms: i64,
}

impl ExpeditionManager {
    /// `Load()`.
    pub async fn load(&self, pool: &MySqlPool) -> Result<()> {
        let mut list = Vec::new();
        for r in sqlx::query(
            "SELECT CAST(id AS SIGNED) AS id, CAST(mother AS SIGNED) AS mother, name, CAST(owner AS SIGNED) AS owner, owner_name, created_at, CAST(allow_change_name AS SIGNED) AS allow_change_name FROM expeditions",
        )
        .fetch_all(pool)
        .await?
        {
            list.push(Expedition {
                faction: SystemFaction {
                    id: int(&r, "id")? as u32,
                    mother_id: int(&r, "mother")? as u32,
                    name: r.try_get("name")?,
                    owner_id: int(&r, "owner")? as u32,
                    owner_name: r.try_get("owner_name")?,
                    unit_owner_type: 0,
                    political_system: 1,
                    created: unix(dt(&r, "created_at")),
                    aggro_link: false,
                    diplomacy_target: false,
                    diplomacy_link_id: 0,
                    guard_help: false,
                    allow_change_name: u8::from(int(&r, "allow_change_name")? != 0),
                },
                members: Vec::new(),
                policies: Vec::new(),
                removed_members: Vec::new(),
                is_disbanded: false,
            });
        }
        for e in &mut list {
            for r in sqlx::query(
                "SELECT CAST(character_id AS SIGNED) AS character_id, CAST(expedition_id AS SIGNED) AS expedition_id, \
                 CAST(role AS SIGNED) AS role, memo, last_leave_time, name, CAST(level AS SIGNED) AS level, \
                 CAST(ability1 AS SIGNED) AS ability1, CAST(ability2 AS SIGNED) AS ability2, CAST(ability3 AS SIGNED) AS ability3 \
                 FROM expedition_members WHERE expedition_id = ?",
            )
            .bind(e.faction.id)
            .fetch_all(pool)
            .await?
            {
                e.members.push(ExpeditionMember {
                    character_id: int(&r, "character_id")? as u32,
                    expedition_id: int(&r, "expedition_id")? as u32,
                    role: int(&r, "role")? as u8,
                    memo: r.try_get("memo")?,
                    last_world_leave_time: unix(dt(&r, "last_leave_time")),
                    name: r.try_get("name")?,
                    level: int(&r, "level")? as u8,
                    abilities: [
                        int(&r, "ability1")? as u8,
                        int(&r, "ability2")? as u8,
                        int(&r, "ability3")? as u8,
                    ],
                    is_online: false,
                    in_party: false,
                    zone_id: 0,
                    id3: 0,
                    position: (0.0, 0.0, 0.0),
                    transfer_requested_time: 0,
                });
            }
            for r in sqlx::query(
                "SELECT CAST(expedition_id AS SIGNED) AS expedition_id, CAST(role AS SIGNED) AS role, name, \
                 CAST(dominion_declare AS SIGNED) AS dominion_declare, CAST(invite AS SIGNED) AS invite, CAST(expel AS SIGNED) AS expel, \
                 CAST(promote AS SIGNED) AS promote, CAST(dismiss AS SIGNED) AS dismiss, CAST(chat AS SIGNED) AS chat, \
                 CAST(manager_chat AS SIGNED) AS manager_chat, CAST(siege_master AS SIGNED) AS siege_master, CAST(join_siege AS SIGNED) AS join_siege \
                 FROM expedition_role_policies WHERE expedition_id = ?",
            )
            .bind(e.faction.id)
            .fetch_all(pool)
            .await?
            {
                let b = |c: &str| -> Result<bool> { Ok(int(&r, c)? != 0) };
                e.policies.push(ExpeditionRolePolicy {
                    expedition_id: int(&r, "expedition_id")? as u32,
                    role: int(&r, "role")? as u8,
                    name: r.try_get("name")?,
                    dominion_declare: b("dominion_declare")?,
                    invite: b("invite")?,
                    expel: b("expel")?,
                    promote: b("promote")?,
                    dismiss: b("dismiss")?,
                    chat: b("chat")?,
                    manager_chat: b("manager_chat")?,
                    siege_master: b("siege_master")?,
                    join_siege: b("join_siege")?,
                });
            }
        }
        *self.expeditions.write().expect("expeditions poisoned") = list;
        Ok(())
    }

    /// Adds an expedition (`_expeditions.Add`).
    pub fn add(&self, e: Expedition) {
        self.expeditions
            .write()
            .expect("expeditions poisoned")
            .push(e);
    }

    /// `GetExpedition(id) != null`.
    pub fn exists(&self, id: u32) -> bool {
        self.expeditions
            .read()
            .expect("expeditions poisoned")
            .iter()
            .any(|e| e.faction.id == id)
    }

    /// `GetExpedition(id)` (a copy).
    pub fn get(&self, id: u32) -> Option<Expedition> {
        self.expeditions
            .read()
            .expect("expeditions poisoned")
            .iter()
            .find(|e| e.faction.id == id)
            .cloned()
    }

    /// Runs `f` on the expedition `id` under the manager's write lock.
    pub fn with_mut<R>(&self, id: u32, f: impl FnOnce(&mut Expedition) -> R) -> Option<R> {
        let mut list = self.expeditions.write().expect("expeditions poisoned");
        list.iter_mut().find(|e| e.faction.id == id).map(f)
    }

    /// Grants `character_id` the right to answer the rename dialog
    /// (`SCExpeditionShowRenameUI` was sent to it), bound to the guild the dialog was opened
    /// for.
    pub fn offer_rename(&self, character_id: u32, expedition_id: u32) {
        let mut offers = self
            .rename_offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if offers.len() >= MAX_RENAME_OFFERS {
            offers.clear();
        }
        offers.insert(character_id, expedition_id);
    }

    /// Whether `SCExpeditionShowRenameUI` is outstanding for `character_id` *for `expedition_id`*.
    pub fn has_rename_offer(&self, character_id: u32, expedition_id: u32) -> bool {
        self.rename_offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&character_id)
            .is_some_and(|id| *id == expedition_id)
    }

    /// Consumes the rename offer after a successful rename.
    pub fn take_rename_offer(&self, character_id: u32) {
        self.rename_offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&character_id);
    }

    /// Drops the rename offer (logout, refusal, or a character that can no longer rename).
    pub fn clear_rename_offer(&self, character_id: u32) {
        self.take_rename_offer(character_id);
    }

    /// Drops every offer made for guild `expedition_id` (disband, owner change).
    pub fn clear_rename_offers_for(&self, expedition_id: u32) {
        self.rename_offers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|_, id| *id != expedition_id);
    }

    /// Every expedition (a copy, insertion order).
    pub fn all(&self) -> Vec<Expedition> {
        self.expeditions
            .read()
            .expect("expeditions poisoned")
            .clone()
    }

    /// The factions `SendExpeditions` writes, in chunks of 20.
    pub fn faction_chunks(&self) -> Vec<Vec<SystemFaction>> {
        let list = self.expeditions.read().expect("expeditions poisoned");
        // A dismissed guild stays in the list (as `$deleted-guild-N`) but is not announced.
        let live: Vec<&Expedition> = list.iter().filter(|e| !is_deleted(e)).collect();
        live.chunks(20)
            .map(|c| c.iter().map(|e| e.faction.clone()).collect())
            .collect()
    }

    /// Forgets the pending invitation of an invited character (it left the world).
    pub fn drop_invitation(&self, character_id: u32) {
        self.invitations
            .lock()
            .expect("expedition invitations poisoned")
            .remove(&character_id);
    }

    /// Sweeps the unanswered guild invitations: removes the ones older than
    /// `INVITATION_TIMEOUT_MS` and sends `SCInvitationCanceled` (kind 4) to each invitee.
    ///
    /// The invitation map is keyed by invitee and never holds a second invitation for the same
    /// character, so no other expedition invitation (popup type 3) can still be pending for an
    /// expired invitee.
    pub(crate) fn expire_invitations(&self, server: &GameServer, now_ms: i64) {
        let expired: Vec<u32> = {
            let mut invitations = self
                .invitations
                .lock()
                .expect("expedition invitations poisoned");
            let ids: Vec<u32> = invitations
                .iter()
                .filter(|(_, p)| p.time_ms + INVITATION_TIMEOUT_MS < now_ms)
                .map(|(&id, _)| id)
                .collect();
            for id in &ids {
                invitations.remove(id);
            }
            ids
        };
        for id in expired {
            if let Some(invitee) = server.world.character_by_id(id) {
                invitee.send(SCInvitationCanceledPacket {
                    kind: invitation_canceled_kind::EXPEDITION,
                });
            }
        }
    }

    /// `GetExpeditionOfCharacter(characterId)` (0 = none).
    pub fn expedition_of_character(&self, character_id: u32) -> u32 {
        self.expeditions
            .read()
            .expect("expeditions poisoned")
            .iter()
            .find(|e| e.members.iter().any(|m| m.character_id == character_id))
            .map_or(0, |e| e.faction.id)
    }

    /// `ExpeditionMember.Refresh` + status packet payload for `OnCharacterLogin` /
    /// `OnCharacterRefresh`. Returns the refreshed member and the member ids to notify.
    pub fn refresh_member(
        &self,
        expedition_id: u32,
        r: &MemberRefresh,
        now: UnixTime,
    ) -> Option<(ExpeditionMember, Vec<u32>)> {
        let mut list = self.expeditions.write().expect("expeditions poisoned");
        let e = list.iter_mut().find(|e| e.faction.id == expedition_id)?;
        let m = e.member_mut(r.character_id)?;
        m.is_online = true;
        m.position = r.position;
        m.zone_id = r.zone_id;
        m.abilities = r.abilities;
        m.level = r.level;
        m.name = r.name.clone();
        m.last_world_leave_time = now;
        let m = m.clone();
        Some((m, e.member_ids()))
    }

    /// `OnCharacterLogout` member update.
    pub fn logout_member(
        &self,
        expedition_id: u32,
        character_id: u32,
        now: UnixTime,
    ) -> Option<(ExpeditionMember, Vec<u32>)> {
        let mut list = self.expeditions.write().expect("expeditions poisoned");
        let e = list.iter_mut().find(|e| e.faction.id == expedition_id)?;
        let m = e.member_mut(character_id)?;
        m.is_online = false;
        m.last_world_leave_time = now;
        let m = m.clone();
        Some((m, e.member_ids()))
    }
}

// ---------------------------------------------------------------------------------------------
// Guild management (ExpeditionManager / Expedition behaviour)
// ---------------------------------------------------------------------------------------------

/// `Expedition.SendPacket`: every member that is in the world.
pub fn send_to_members(
    server: &GameServer,
    members: &[u32],
    packet: &dyn crate::net::packet::ServerPacket,
) {
    let mut out = crate::net::connection::Fanout::new(packet);
    for id in members {
        if let Some(p) = server.world.character_by_id(*id) {
            out.send(&p.conn);
        }
    }
}

/// `GameObject.BroadcastPacket(packet, true)` for a player.
fn broadcast_self(
    server: &GameServer,
    p: &PlayerRef,
    packet: &dyn crate::net::packet::ServerPacket,
) {
    let inst = p.data().unit.transform.instance_id;
    match server.world.world(inst) {
        Some(w) => broadcast(&w, p, packet, true),
        None => p.send_dyn(packet),
    }
}

/// `ChatManager.GetGuildChat(expedition)`.
pub fn guild_chat(server: &GameServer, id: u32, name: &str) -> crate::managers::chat::ChannelRef {
    server.chat.guild_chat(id, name, &server.ids.chat)
}

/// `ExpeditionManager.GetDefaultPolicies(expeditionId)`.
pub fn default_policies(server: &GameServer, expedition_id: u32) -> Vec<ExpeditionRolePolicy> {
    server
        .config
        .expedition
        .role_policies
        .iter()
        .map(|p| p.to_policy(expedition_id))
        .collect()
}

/// `ExpeditionManager.GetMemberFromCharacter(expedition, character, owner)`.
pub fn member_from_character(
    server: &GameServer,
    expedition_id: u32,
    p: &PlayerRef,
    owner: bool,
) -> ExpeditionMember {
    let position = util::world_position(server, p);
    let c = p.data();
    ExpeditionMember {
        is_online: true,
        name: c.unit.name.clone(),
        level: c.unit.level,
        role: if owner { 255 } else { 0 },
        memo: String::new(),
        position,
        zone_id: c.unit.transform.zone_id,
        abilities: [c.ability1, c.ability2, c.ability3],
        expedition_id,
        character_id: c.id,
        last_world_leave_time: util::now_secs(),
        in_party: false,
        id3: 0,
        transfer_requested_time: 0,
    }
}

/// `ExpeditionManager.SendExpeditionInfo(character)`.
pub fn send_expedition_info(p: &PlayerRef, e: &Expedition) {
    p.send(SCExpeditionRolePolicyListPacket {
        policies: e.policies.clone(),
    });
    let total = e.members.len() as u32;
    for chunk in e.members.chunks(20) {
        p.send(SCExpeditionMemberListPacket {
            total,
            id: e.faction.id,
            members: chunk.to_vec(),
        });
    }
}

/// `ExpeditionManager.SendExpeditions(character)` (lists every guild, then an empty policy
/// list).
pub fn send_expeditions(server: &GameServer, p: &PlayerRef) {
    for chunk in server.expeditions.faction_chunks() {
        p.send(SCFactionListPacket { factions: chunk });
    }
    p.send(SCExpeditionRolePolicyListPacket {
        policies: Vec::new(),
    });
}

fn member_refresh(server: &GameServer, p: &PlayerRef) -> MemberRefresh {
    let position = util::world_position(server, p);
    let c = p.data();
    MemberRefresh {
        character_id: c.id,
        name: c.unit.name.clone(),
        level: c.unit.level,
        zone_id: c.unit.transform.zone_id,
        abilities: [c.ability1, c.ability2, c.ability3],
        position,
    }
}

/// `Expedition.OnCharacterRefresh(character)` (refresh + status packet to the guild).
pub fn on_character_refresh(server: &GameServer, p: &PlayerRef, expedition_id: u32) {
    let r = member_refresh(server, p);
    if let Some((member, members)) =
        server
            .expeditions
            .refresh_member(expedition_id, &r, util::now_secs())
    {
        send_to_members(
            server,
            &members,
            &SCExpeditionMemberStatusChangedPacket { member, flag: 0 },
        );
    }
}

/// `Expedition.OnCharacterLogin(character)` (refresh + status packet + guild chat join).
pub fn on_character_login(server: &GameServer, p: &PlayerRef, expedition_id: u32) {
    let r = member_refresh(server, p);
    let Some((member, members)) =
        server
            .expeditions
            .refresh_member(expedition_id, &r, util::now_secs())
    else {
        return;
    };
    send_to_members(
        server,
        &members,
        &SCExpeditionMemberStatusChangedPacket { member, flag: 0 },
    );
    if let Some(e) = server.expeditions.get(expedition_id) {
        guild_chat(server, e.id(), &e.faction.name).join(p);
    }
}

/// `Expedition.OnCharacterLogout(character)`.
pub fn on_character_logout(server: &GameServer, p: &PlayerRef, expedition_id: u32) {
    if let Some((member, members)) =
        server
            .expeditions
            .logout_member(expedition_id, p.character_id, util::now_secs())
    {
        send_to_members(
            server,
            &members,
            &SCExpeditionMemberStatusChangedPacket { member, flag: 0 },
        );
    }
    if let Some(e) = server.expeditions.get(expedition_id) {
        guild_chat(server, e.id(), &e.faction.name).leave(p);
    }
}

/// `Character.Expedition` (the id when the guild exists, as `GetExpedition` would return null).
fn expedition_of(server: &GameServer, p: &PlayerRef) -> Option<Expedition> {
    let id = p.data().unit.expedition_id;
    if id == 0 {
        return None;
    }
    server.expeditions.get(id)
}

/// Whether a guild name (or a family title) is on the character-name block list
/// (`blocked_texts.check_name`, the same rule the lobby applies: case-insensitive, whole string
/// or partial match).
pub(crate) fn name_is_blocked(server: &GameServer, name: &str) -> bool {
    let folded = name.to_lowercase();
    server
        .game_data
        .characters
        .blocked_names
        .iter()
        .any(|rule| {
            let text = rule.text.to_lowercase();
            if rule.partial_match {
                folded.contains(&text)
            } else {
                folded == text
            }
        })
}

/// Whether the guild is dismissed (its record stays in the list, renamed `$deleted-guild-N`).
fn is_deleted(e: &Expedition) -> bool {
    e.is_disbanded || e.faction.name.starts_with("$deleted-")
}

/// The name-shape rules shared by guild create / rename, independence and nation rename, in the
/// order the client's dialog reports them: UTF-16 length > 32 `EXPEDITION_NAME_LENGTH`, then
/// the configured regex `EXPEDITION_NAME_CHARACTER`. Returns the `ErrorMessageType`.
pub fn validate_name_shape(server: &GameServer, name: &str) -> std::result::Result<(), i16> {
    if name.encode_utf16().count() > 32 {
        return Err(error::EXPEDITION_NAME_LENGTH);
    }
    let matches = match regex::Regex::new(&server.config.expedition.name_regex) {
        Ok(r) => r.is_match(name),
        Err(e) => {
            tracing::error!("Expedition.NameRegex: {e}");
            false
        }
    };
    if !matches {
        return Err(error::EXPEDITION_NAME_CHARACTER);
    }
    Ok(())
}

/// The name rules of creating and renaming a guild: [`validate_name_shape`], the block list
/// `EXPEDITION_NAME_BADLIST`, and a name another guild already uses (case-insensitive,
/// dismissed guilds do not count; `own_id` is the guild being renamed)
/// `EXPEDITION_NAME_EXIST`. Returns the `ErrorMessageType`.
pub fn validate_name(server: &GameServer, name: &str, own_id: u32) -> std::result::Result<(), i16> {
    validate_name_shape(server, name)?;
    if name_is_blocked(server, name) {
        return Err(error::EXPEDITION_NAME_BADLIST);
    }
    let folded = name.to_lowercase();
    if server
        .expeditions
        .all()
        .iter()
        .any(|e| e.id() != own_id && !is_deleted(e) && e.faction.name.to_lowercase() == folded)
    {
        return Err(error::EXPEDITION_NAME_EXIST);
    }
    Ok(())
}

/// Subscribes the guild-invitation expiry sweep to the shared `TickManager` (1 s; the C# only
/// pruned the map on the next invite, which left the invitee's popup open).
pub fn start_tick(server: &std::sync::Arc<GameServer>) {
    let weak = std::sync::Arc::downgrade(server);
    server.systems.services.tick.on_tick.subscribe(
        move |_delta| {
            let Some(server) = weak.upgrade() else {
                return;
            };
            server
                .expeditions
                .expire_invitations(&server, util::now_ms());
        },
        std::time::Duration::from_millis(INVITATION_SWEEP_MS),
        false,
    );
}
