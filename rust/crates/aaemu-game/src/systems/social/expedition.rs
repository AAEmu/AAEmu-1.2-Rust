//! `Core/Managers/ExpeditionManager` + `Models/Game/Expeditions` (guilds): loading guilds /
//! members / role policies, `GetExpedition`, `SendExpeditions`, `SendExpeditionInfo`, member
//! status refresh on login / logout / zone change, and guild management (create, invite /
//! reply, leave, kick, member roles, owner change, role policies, disband) with the
//! `Expedition.Save` persistence. (Moved here from `managers::expedition`, which re-exports
//! this module.)

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};

use aaemu_commons::{Marshal, PacketStream};
use sqlx::mysql::MySqlPool;
use sqlx::Row;

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
use crate::systems::social::packets::expedition::*;
use crate::systems::social::packets::faction::{
    SCFactionRenamedPacket, SCFactionRetryRenamePacket,
};
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
}

/// `ExpeditionMember.Role` of the guild leader.
const LEADER_ROLE: u8 = 255;

/// How long a guild invitation can be answered (same as the party / raid invitations).
const INVITATION_TIMEOUT_MS: i64 = 60_000;

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

/// The name rules of creating and renaming a guild, in the order the client's dialog reports
/// them: UTF-16 length > 32 `EXPEDITION_NAME_LENGTH`, the configured regex
/// `EXPEDITION_NAME_CHARACTER`, the block list `EXPEDITION_NAME_BADLIST`, and a name another
/// guild already uses (case-insensitive, dismissed guilds do not count; `own_id` is the guild
/// being renamed) `EXPEDITION_NAME_EXIST`. Returns the `ErrorMessageType`.
pub fn validate_name(server: &GameServer, name: &str, own_id: u32) -> std::result::Result<(), i16> {
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
    let owner_faction = owner.data().unit.faction_id;
    let owner_alliance = util::alliance_id(server, owner_faction);
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
                c.unit.faction_id,
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
        if util::alliance_id(server, faction) != owner_alliance {
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
            guard_help: false,
            allow_change_name: 0,
        },
        members: vec![member_from_character(server, id, owner, true)],
        policies: default_policies(server, id),
        removed_members: Vec::new(),
        is_disbanded: false,
    };
    server.expeditions.add(expedition.clone());
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
    // Deviation from the C# (see PORTING.md "Guild functions (2026-11-05)"): the client has a
    // text for each of these, so the inviter is told.
    if util::alliance_id(server, invited.data().unit.faction_id)
        != util::alliance_id(server, e.faction.mother_id)
    {
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

/// A character that has to leave its guild without any right check (`Character.SetFaction` into
/// another alliance). The C# called `Kick(p, p)`, which needs the Expel right and a strictly
/// higher rank, so an ordinary member never left (and a leader that did would have left the
/// guild without one).
///
/// A leader of a guild with other members first hands the guild to the highest-ranked remaining
/// member (ties: the one that has been in the guild longest, i.e. first in the member list) and
/// then leaves; the guild of a sole member is dismissed (even with territory or a siege: the
/// dominion then has no live owner). The leave is announced like a voluntary one.
pub async fn leave_forced(server: &GameServer, p: &PlayerRef) {
    let Some(e) = expedition_of(server, p) else {
        return;
    };
    let id = e.id();
    let Some(me) = e.member(p.character_id) else {
        return;
    };
    if me.role == LEADER_ROLE || e.faction.owner_id == p.character_id {
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
                dismiss(server, &e);
                drop(_housing_operation);
                save(server, id).await;
                return;
            }
        }
    }
    remove_member_and_announce(server, p, id).await;
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
/// both roles, the three packets and the save. No checks.
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
        Some((old, new, members))
    });
    let Some(Some((old, new, members))) = res else {
        return;
    };
    send_to_members(
        server,
        &members,
        &SCExpeditionOwnerChangedPacket {
            id: old.0,
            id2: new.0,
            char_name: new.2.clone(),
        },
    );
    send_to_members(
        server,
        &members,
        &SCExpeditionRoleChangedPacket {
            id: old.0,
            role: old.1,
            char_name: old.2,
        },
    );
    send_to_members(
        server,
        &members,
        &SCExpeditionRoleChangedPacket {
            id: new.0,
            role: new.1,
            char_name: new.2,
        },
    );
    save(server, id).await;
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

/// Dismisses the guild: every member leaves (online ones are told), and the record stays in
/// the list renamed `$deleted-guild-<id>` with owner 0, as in C#. No checks.
fn dismiss(server: &GameServer, e: &Expedition) {
    let id = e.id();
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
}

/// `ExpeditionManager.Disband(owner)`. The guild stays in the list (renamed
/// `$deleted-guild-<id>`, owner 0) as in C#, but is no longer announced (`SCFactionList`).
///
/// Deviation from the C#: a guild that owns territory is refused with `EXPEDITION_OWNER_DISMISS`
/// and one registered in a siege with `EXPEDITION_DISMISS_IN_SIEGE` (the 1.2 texts; this server
/// answered with a chat line).
pub async fn disband(server: &GameServer, owner: &PlayerRef) -> bool {
    let id = {
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
        dismiss(server, &e);
        id
    };
    save(server, id).await;
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
        p.send(SCExpeditionShowRenameUIPacket {});
    }
}

/// Renames the guild `id` in the manager, the guild chat channel and for every client
/// (`SCFactionRenamed`), and saves it. `by_gm` is the flag the client uses to keep its
/// "may rename" right (a GM rename does not spend the leader's right); otherwise the right is
/// spent. The name must have been validated.
async fn apply_rename(server: &GameServer, id: u32, name: &str, by_gm: bool) {
    let renamed = server.expeditions.with_mut(id, |e| {
        e.faction.name = name.to_string();
        if !by_gm {
            e.faction.allow_change_name = 0;
        }
    });
    if renamed.is_none() {
        return;
    }
    server.chat.rename_guild_chat(id, name);
    server.world.broadcast_to_server(&SCFactionRenamedPacket {
        id,
        name: name.to_string(),
        by_gm,
    });
    save(server, id).await;
}

/// `CSRenameExpedition(id, name, isExpedition)`: the guild leader spends a granted rename right
/// (`expeditions.allow_change_name`, set by the GM command `/allowguildrename`).
///
/// Not in the C# (it logged the packet and never sent the dialog). Not a member `ONLY_EXPEDITION_
/// MEMBER`, not the leader of the named guild `ONLY_EXPEDITION_OWNER`, no pending right (or the
/// nation variant, which this server has no nations for) `FACTION_RIGHTS`; a name that breaks
/// the create rules reopens the dialog through `SCFactionRetryRename`.
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
    if let Err(code) = validate_name(server, name, id) {
        p.send(SCFactionRetryRenamePacket {
            xpdt: true,
            error_message: code,
            name: name.to_string(),
        });
        return;
    }
    apply_rename(server, id, name, false).await;
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
            owner.send(SCExpeditionShowRenameUIPacket {});
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
    apply_rename(server, id, name, true).await;
    Ok(())
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

/// `Expedition.Save(connection, transaction)`.
pub async fn save_expedition(pool: &MySqlPool, e: &Expedition) -> Result<()> {
    let mut tx = pool.begin().await?;
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
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!(
            "UPDATE characters SET expedition_id = 0 WHERE `characters`.`id` IN ({list})"
        ))
        .execute(&mut *tx)
        .await?;
    }
    if e.is_disbanded {
        sqlx::query("DELETE FROM expeditions WHERE `id` = ?")
            .bind(e.faction.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM expedition_role_policies WHERE `expedition_id` = ?")
            .bind(e.faction.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM expedition_members WHERE `expedition_id` = ?")
            .bind(e.faction.id)
            .execute(&mut *tx)
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
            .execute(&mut *tx)
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
                .execute(&mut *tx)
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
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
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
