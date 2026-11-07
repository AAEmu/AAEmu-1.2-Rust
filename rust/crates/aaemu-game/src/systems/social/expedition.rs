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

/// `Expedition.RemoveMember(member)`: leaves the guild chat (when online) and records the id.
fn remove_member(server: &GameServer, expedition_id: u32, character_id: u32) {
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

/// The sponsor factions a guild can be founded under, by the owner's alliance (`MotherId` of
/// its faction; the outlaw faction is its own alliance): Nuia 148, Haranya 149, outlaws 114.
/// Taken from the 1.2 client's expedition creation dialog.
pub fn valid_sponsors(alliance: u32) -> &'static [u32] {
    match alliance {
        148 => &[101, 102, 103, 105, 106, 107],
        149 => &[108, 109, 110, 111, 113],
        114 => &[161],
        _ => &[],
    }
}

/// Whether a guild name is on the character-name block list (`blocked_texts.check_name`, the
/// same rule the lobby applies: case-insensitive, whole string or partial match).
fn name_is_blocked(server: &GameServer, name: &str) -> bool {
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

/// `ExpeditionManager.CreateExpedition(name, connection)`.
///
/// `sponsor` is the faction id the client's dialog sends (`CSCreateExpedition.id`): one of
/// [`valid_sponsors`] of the owner's alliance, and it becomes the guild's `mother_id`. Deviation
/// from the C#, which dropped it and used the owner's own faction (0 = a client that sends none:
/// the owner's faction as before). Also unlike the C#: a party member of another alliance is
/// `EXPEDITION_CREATE_MEMBER_FACTION` (320, not 321), the name block list applies and names are
/// unique case-insensitively, and only members that are online and in the world join.
pub async fn create_expedition(server: &GameServer, owner: &PlayerRef, name: &str, sponsor: u32) {
    let cfg = &server.config.expedition;
    if expedition_of(server, owner).is_some() {
        send_error(owner, error::EXPEDITION_ALREADY_MEMBER);
        return;
    }
    if let Err(code) = validate_name(server, name, 0) {
        send_error(owner, code);
        return;
    }
    if rename::faction_name_in_use(server, name, None) {
        send_error(owner, error::EXPEDITION_NAME_EXIST);
        return;
    }
    // ----------------- Conditions, can change this...
    let Some(team_members) = server
        .systems
        .social
        .teams
        .team_characters_of(owner.character_id)
    else {
        // We send the same error on number of party members when we don't have a party
        send_error(owner, error::EXPEDITION_CREATE_MEMBER);
        return;
    };
    // The saved faction, not a temporary duel / battlefield team faction (NATION-14). The
    // sponsor list is the owner's alliance; the members must share its top-level faction, with
    // the runtime nations overlaid: a nation's top-level faction is itself, so two citizens of
    // different nations are not grouped like two factions of one alliance (PORTING.md,
    // NATION-13).
    let owner_faction = crate::db_character::persisted_faction(&owner.data());
    let owner_alliance = util::alliance_id(server, owner_faction);
    let owner_top =
        super::nation::top_level_faction(server, owner_faction).unwrap_or(owner_alliance);
    let mother_id = if sponsor == 0 {
        owner_faction
    } else if valid_sponsors(owner_alliance).contains(&sponsor) {
        sponsor
    } else {
        send_error(owner, error::EXPEDITION_CREATE_FACTION);
        return;
    };
    let mut valid: Vec<PlayerRef> = Vec::new();
    for m in team_members {
        let (level, exp, faction, online) = {
            let c = m.data();
            (
                c.unit.level,
                c.unit.expedition_id,
                crate::db_character::persisted_faction(&c),
                c.is_online,
            )
        };
        // A team member that is offline (or not in the world) does not join.
        if !online && m.character_id != owner.character_id {
            continue;
        }
        if level < cfg.create.level {
            send_error(owner, error::EXPEDITION_CREATE_LEVEL);
            return;
        }
        if exp != 0 && server.expeditions.exists(exp) {
            send_error(owner, error::EXPEDITION_CREATE_MEMBER_EXPEDITION);
            return;
        }
        let member_top = super::nation::top_level_faction(server, faction)
            .unwrap_or_else(|| util::alliance_id(server, faction));
        if member_top != owner_top {
            send_error(owner, error::EXPEDITION_CREATE_MEMBER_FACTION);
            return;
        }
        valid.push(m);
    }
    if valid.len() < usize::from(cfg.create.party_member_count) {
        send_error(owner, error::EXPEDITION_CREATE_MEMBER);
        return;
    }
    let cost = i64::from(cfg.create.cost);
    {
        let mut c = owner.data();
        if c.money < cost {
            drop(c);
            send_error(owner, error::EXPEDITION_CREATE_MONEY);
            return;
        }
        c.money -= cost;
    }
    owner.send(item_task_money_packet(
        ITEM_TASK_EXPEDITION_CREATION,
        -cfg.create.cost,
    ));
    // -----------------
    let id = match server.ids.expedition.get_next_id() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("{e}");
            return;
        }
    };
    let (owner_name, owner_obj) = {
        let c = owner.data();
        (c.unit.name.clone(), owner.obj_id)
    };
    let expedition = Expedition {
        faction: SystemFaction {
            id,
            mother_id,
            name: name.to_string(),
            owner_id: owner.character_id,
            owner_name: owner_name.clone(),
            unit_owner_type: 0,
            political_system: 1,
            created: util::now_secs(),
            aggro_link: false,
            diplomacy_target: false,
            diplomacy_link_id: 0,
            guard_help: false,
            allow_change_name: 0,
        },
        members: vec![member_from_character(server, id, owner, true)],
        policies: default_policies(server, id),
        removed_members: Vec::new(),
        is_disbanded: false,
    };
    {
        // The early check above can be overtaken by a concurrent rename; the name lock makes
        // the final check and the insert one step (RENAME-6).
        let _names = rename::lock_names(server);
        if rename::faction_name_in_use(server, name, None) {
            owner.data().money += cost;
            owner.send(item_task_money_packet(
                ITEM_TASK_EXPEDITION_CREATION,
                cfg.create.cost,
            ));
            send_error(owner, error::EXPEDITION_NAME_EXIST);
            return;
        }
        server.expeditions.add(expedition.clone());
    }
    owner.data().unit.expedition_id = id;
    // ENROLL_GUILD (queued: runs once the packet is handled).
    crate::systems::refresh_achievements_later(server, owner);

    owner.send(SCFactionCreatedPacket {
        faction: expedition.faction.clone(),
        owner_obj_id: owner_obj,
        members: vec![(owner_obj, owner.character_id, owner_name.clone())],
    });
    server.world.broadcast_to_server(&SCFactionListPacket {
        factions: vec![expedition.faction.clone()],
    });
    broadcast_self(
        server,
        owner,
        &SCUnitExpeditionChangedPacket {
            obj_id: owner_obj,
            character_id: owner.character_id,
            kicker: String::new(),
            unit_name: owner_name,
            id: 0,
            expedition_id: id,
            expel: false,
        },
    );
    guild_chat(server, id, name).join(owner);
    send_expedition_info(owner, &expedition);

    for m in valid {
        if m.character_id == owner.character_id {
            continue;
        }
        let member = member_from_character(server, id, &m, false);
        m.data().unit.expedition_id = id;
        crate::systems::refresh_achievements_later(server, &m);
        let snapshot = server
            .expeditions
            .with_mut(id, |e| {
                e.members.push(member);
                e.clone()
            })
            .expect("just added");
        let unit_name = m.data().unit.name.clone();
        broadcast_self(
            server,
            &m,
            &SCUnitExpeditionChangedPacket {
                obj_id: m.obj_id,
                character_id: m.character_id,
                kicker: String::new(),
                unit_name,
                id: 0,
                expedition_id: id,
                expel: false,
            },
        );
        send_expedition_info(&m, &snapshot);
        on_character_login(server, &m, id);
    }
    save(server, id).await;
}

/// Asks the server to offer the guild owner a rename: grants the one-shot
/// `allow_change_name` right (the same right `/allowguildrename` grants), sends
/// `SCExpeditionShowRenameUI` (empty body) and records the one-shot offer that `rename`
/// requires. Used by the GM test hook; a future item / merge trigger calls the same function.
pub fn offer_expedition_rename_ui(
    server: &GameServer,
    p: &PlayerRef,
) -> std::result::Result<(), i16> {
    let id = p.data().unit.expedition_id;
    let faction = server.expeditions.with_mut(id, |e| {
        if is_deleted(e) {
            return Err(error::ONLY_EXPEDITION_MEMBER);
        }
        if e.faction.owner_id != p.character_id {
            return Err(error::ONLY_EXPEDITION_OWNER);
        }
        e.faction.allow_change_name = 1;
        Ok(e.faction.clone())
    });
    let Some(faction) = faction else {
        return Err(error::ONLY_EXPEDITION_MEMBER);
    };
    let faction = faction?;
    // The client keeps the `allow_change_name` flag with its faction record.
    server.world.broadcast_to_server(&SCFactionListPacket {
        factions: vec![faction],
    });
    server.expeditions.offer_rename(p.character_id, id);
    rename::send_expedition_rename_ui(p);
    Ok(())
}

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
async fn transfer_ownership(server: &GameServer, id: u32, old_id: u32, new_id: u32) {
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
fn dismiss_expedition(server: &GameServer, id: u32) -> Vec<Nation> {
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
async fn forced_owner_leave(server: &GameServer, p: &PlayerRef, id: u32) -> bool {
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

/// The guild leader's rename dialog (`SCExpeditionShowRenameUI`) when the guild may be renamed
/// and the leader is online: after a GM granted the right, and on every login while it is
/// pending.
pub fn prompt_rename_if_allowed(server: &GameServer, p: &PlayerRef) {
    let id = p.data().unit.expedition_id;
    if id == 0 {
        return;
    }
    let Some(e) = server.expeditions.get(id) else {
        return;
    };
    if e.faction.allow_change_name != 0 && e.faction.owner_id == p.character_id && !is_deleted(&e) {
        server.expeditions.offer_rename(p.character_id, id);
        rename::send_expedition_rename_ui(p);
    }
}

/// What one guild rename write did under the shared faction-name lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenameWrite {
    Done,
    /// An expedition, nation or static faction already carries the name (RENAME-3).
    NameTaken,
    /// The guild disappeared between the checks and the write.
    Missing,
}

/// Renames the guild `id` in the manager, the guild chat channel and for every client
/// (`SCFactionRenamed`), and saves it. `by_gm` is the flag the client uses to keep its
/// "may rename" right (a GM rename does not spend the leader's right); otherwise the right is
/// spent. The name must have been validated; the cross-store namespace check and the name
/// write share `rename::lock_names` with independence and nation rename.
async fn apply_rename(server: &GameServer, id: u32, name: &str, by_gm: bool) -> RenameWrite {
    let outcome = {
        // Guilds, nations and static factions share one namespace; the lock keeps the check
        // and the write together (RENAME-3 / RENAME-6).
        let _names = rename::lock_names(server);
        if rename::faction_name_in_use(server, name, Some(rename::NameOwner::Expedition(id))) {
            RenameWrite::NameTaken
        } else {
            match server.expeditions.with_mut(id, |e| {
                e.faction.name = name.to_string();
                if !by_gm {
                    e.faction.allow_change_name = 0;
                }
            }) {
                Some(()) => RenameWrite::Done,
                None => RenameWrite::Missing,
            }
        }
    };
    if outcome != RenameWrite::Done {
        return outcome;
    }
    server.chat.rename_guild_chat(id, name);
    rename::broadcast_renamed(server, id, name, by_gm);
    save(server, id).await;
    outcome
}

/// `CSRenameExpedition(id, name, isExpedition)`: the guild leader spends a granted rename right
/// (`expeditions.allow_change_name`, set by the GM command `/allowguildrename`).
///
/// Not in the C# (it logged the packet and never sent the dialog). Not a member `ONLY_EXPEDITION_
/// MEMBER`, not the leader of the named guild `ONLY_EXPEDITION_OWNER`, no pending right (or the
/// nation variant, which has its own rename stage) `FACTION_RIGHTS`; a granted right without the
/// dialog offer that `SCExpeditionShowRenameUI` created (a forged packet) `EXPEDITION_BAD_STATUS`
/// (307, our server-side guard: only the dialog can produce this request); a name that breaks
/// the create rules reopens the dialog through `SCFactionRetryRename` and keeps the offer.
pub async fn rename(server: &GameServer, p: &PlayerRef, id: u32, name: &str, is_expedition: bool) {
    if !is_expedition {
        send_error(p, error::FACTION_RIGHTS);
        return;
    }
    let Some(e) = expedition_of(server, p) else {
        send_error(p, error::ONLY_EXPEDITION_MEMBER);
        return;
    };
    if e.id() != id || e.faction.owner_id != p.character_id {
        send_error(p, error::ONLY_EXPEDITION_OWNER);
        return;
    }
    if e.faction.allow_change_name == 0 {
        send_error(p, error::FACTION_RIGHTS);
        return;
    }
    if !server.expeditions.has_rename_offer(p.character_id, id) {
        tracing::warn!(
            "RenameExpedition {id} by {} without a ShowRenameUI offer refused",
            p.character_id
        );
        send_error(p, error::EXPEDITION_BAD_STATUS);
        return;
    }
    if let Err(code) = validate_name(server, name, id) {
        // The client re-opens the dialog with the rejected name; the offer stays usable.
        rename::send_retry(p, true, code, name);
        return;
    }
    match apply_rename(server, id, name, false).await {
        RenameWrite::Done => server.expeditions.take_rename_offer(p.character_id),
        RenameWrite::NameTaken => {
            rename::send_retry(p, true, error::EXPEDITION_NAME_EXIST, name);
        }
        RenameWrite::Missing => send_error(p, error::EXPEDITION_BAD_STATUS),
    }
}

/// GM: gives the guild `id` the right to rename itself once; its leader gets the dialog now
/// when online. Returns the guild name, or `None` for an unknown / dismissed guild.
pub async fn grant_rename(server: &GameServer, id: u32) -> Option<String> {
    let name = server.expeditions.with_mut(id, |e| {
        if is_deleted(e) {
            return None;
        }
        e.faction.allow_change_name = 1;
        Some((
            e.faction.name.clone(),
            e.faction.owner_id,
            e.faction.clone(),
        ))
    })??;
    // The client keeps the flag with its faction record.
    server.world.broadcast_to_server(&SCFactionListPacket {
        factions: vec![name.2],
    });
    if let Some(owner) = server.world.character_by_id(name.1) {
        if owner.data().unit.expedition_id == id {
            // The dialog is the offer `rename` checks for.
            server.expeditions.offer_rename(name.1, id);
            rename::send_expedition_rename_ui(&owner);
        }
    }
    save(server, id).await;
    Some(name.0)
}

/// GM: renames the guild `id` directly (`SCFactionRenamed` with `by_gm`), applying the create
/// rules. Returns the `ErrorMessageType` of a refused name.
pub async fn gm_rename(server: &GameServer, id: u32, name: &str) -> std::result::Result<(), i16> {
    match server.expeditions.get(id) {
        Some(e) if !is_deleted(&e) => {}
        _ => return Err(error::EXPEDITION_NO_TARGET),
    }
    validate_name(server, name, id)?;
    match apply_rename(server, id, name, true).await {
        RenameWrite::Done => Ok(()),
        RenameWrite::NameTaken => Err(error::EXPEDITION_NAME_EXIST),
        RenameWrite::Missing => Err(error::EXPEDITION_NO_TARGET),
    }
}

/// `ExpeditionManager.Save(expedition)` (`Expedition.Save` in its own transaction).
pub async fn save(server: &GameServer, id: u32) {
    let Some(e) = server.expeditions.with_mut(id, |e| {
        let snapshot = e.clone();
        e.removed_members.clear();
        snapshot
    }) else {
        return;
    };
    if let Err(err) = save_expedition(&server.db, &e).await {
        tracing::error!("Expedition {id} save failed: {err}");
        // Keep the removed member ids for the next save attempt.
        server.expeditions.with_mut(id, |x| {
            for r in e.removed_members.iter().rev() {
                x.removed_members.insert(0, *r);
            }
        });
    }
}

/// The owner / nation / dominion rows that change together with one forced guild removal, in
/// one transaction. Snapshots the current expedition and nation rows first, so a concurrent
/// hand-over is not overwritten with an older king.
async fn save_expedition_state(
    server: &GameServer,
    id: u32,
    nation_ids: &[u32],
    dominions: Option<crate::systems::housing::dominion::state::SaveBatch>,
) {
    let Some(e) = server.expeditions.with_mut(id, |e| {
        let snapshot = e.clone();
        e.removed_members.clear();
        snapshot
    }) else {
        return;
    };
    let nations: Vec<Nation> = nation_ids
        .iter()
        .filter_map(|n| server.systems.social.nations.get(*n))
        .collect();
    let had_dominions = dominions.is_some();
    let result: Result<()> = async {
        let mut tx = server.db.begin().await?;
        write_expedition(&mut tx, &e).await?;
        for nation in &nations {
            crate::systems::social::nation::NationManager::write_nation_owner(&mut tx, nation)
                .await?;
        }
        if let Some(batch) = dominions {
            server
                .systems
                .housing
                .dominions
                .save_prepared(&mut tx, batch)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            if had_dominions {
                server.systems.housing.dominions.commit_save();
            }
        }
        Err(err) => {
            tracing::error!("Expedition {id} save failed: {err}");
            // Keep the removed member ids for the next save attempt.
            server.expeditions.with_mut(id, |x| {
                for r in e.removed_members.iter().rev() {
                    x.removed_members.insert(0, *r);
                }
            });
        }
    }
}

/// `Expedition.Save(connection, transaction)`.
pub async fn save_expedition(pool: &MySqlPool, e: &Expedition) -> Result<()> {
    let mut tx = pool.begin().await?;
    write_expedition(&mut tx, e).await?;
    tx.commit().await?;
    Ok(())
}

/// The `Expedition.Save` body inside the caller's transaction.
async fn write_expedition(tx: &mut Transaction<'_, MySql>, e: &Expedition) -> Result<()> {
    if !e.removed_members.is_empty() {
        let list = e
            .removed_members
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        sqlx::query(&format!(
            "DELETE FROM expedition_members WHERE character_id IN ({list})"
        ))
        .execute(&mut **tx)
        .await?;
        sqlx::query(&format!(
            "UPDATE characters SET expedition_id = 0 WHERE `characters`.`id` IN ({list})"
        ))
        .execute(&mut **tx)
        .await?;
    }
    if e.is_disbanded {
        sqlx::query("DELETE FROM expeditions WHERE `id` = ?")
            .bind(e.faction.id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM expedition_role_policies WHERE `expedition_id` = ?")
            .bind(e.faction.id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM expedition_members WHERE `expedition_id` = ?")
            .bind(e.faction.id)
            .execute(&mut **tx)
            .await?;
    } else {
        sqlx::query("REPLACE INTO expeditions(`id`,`owner`,`owner_name`,`name`,`mother`,`allow_change_name`,`created_at`) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(e.faction.id)
            .bind(e.faction.owner_id)
            .bind(&e.faction.owner_name)
            .bind(&e.faction.name)
            .bind(e.faction.mother_id)
            .bind(e.faction.allow_change_name)
            .bind(from_unix(e.faction.created))
            .execute(&mut **tx)
            .await?;
        for m in &e.members {
            sqlx::query("REPLACE INTO expedition_members(`character_id`,`expedition_id`,`name`,`level`,`role`,`last_leave_time`,`ability1`,`ability2`,`ability3`, `memo`) VALUES (?,?,?,?,?,?,?,?,?,?)")
                .bind(m.character_id)
                .bind(m.expedition_id)
                .bind(&m.name)
                .bind(m.level)
                .bind(m.role)
                .bind(from_unix(m.last_world_leave_time))
                .bind(m.abilities[0])
                .bind(m.abilities[1])
                .bind(m.abilities[2])
                .bind(&m.memo)
                .execute(&mut **tx)
                .await?;
        }
        for p in &e.policies {
            sqlx::query("REPLACE INTO expedition_role_policies(`expedition_id`,`role`,`name`,`dominion_declare`,`invite`,`expel`,`promote`,`dismiss`, `chat`, `manager_chat`, `siege_master`, `join_siege`) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)")
                .bind(p.expedition_id)
                .bind(p.role)
                .bind(&p.name)
                .bind(p.dominion_declare)
                .bind(p.invite)
                .bind(p.expel)
                .bind(p.promote)
                .bind(p.dismiss)
                .bind(p.chat)
                .bind(p.manager_chat)
                .bind(p.siege_master)
                .bind(p.join_siege)
                .execute(&mut **tx)
                .await?;
        }
    }
    Ok(())
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
