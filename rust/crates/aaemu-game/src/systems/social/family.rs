//! `Core/Managers/FamilyManager` + `Models/Game/Family` (`Family`, `FamilyMember`): loading,
//! invitations, creation, joining, leaving, kicking, titles, steward changes, login / logout
//! status and `Family.Save`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard};

use aaemu_commons::PacketStream;
use sqlx::mysql::MySqlPool;
use sqlx::Row;

use crate::db::int;
use crate::error::Result;
use crate::managers::name::normalize_name;
use crate::net::packet::ServerPacket;
use crate::server::GameServer;
use crate::systems::social::packets::family::*;
use crate::systems::social::util::{error, send_error};
use crate::world::PlayerRef;

/// `FamilyMember`.
#[derive(Debug, Clone)]
pub struct FamilyMember {
    /// `Character` (set while the member is logged in; `Online => Character != null`).
    pub character: Option<PlayerRef>,
    pub id: u32,
    pub name: String,
    /// 1 = steward (owner).
    pub role: u8,
    pub title: String,
}

impl FamilyMember {
    pub fn online(&self) -> bool {
        self.character.is_some()
    }

    /// `FamilyMember.Write`.
    pub fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.id)
            .write_str(&self.name)
            .write_u8(self.role)
            .write_bool(self.online())
            .write_str(&self.title);
    }
}

/// `Family`.
#[derive(Debug, Clone, Default)]
pub struct Family {
    pub id: u32,
    pub members: Vec<FamilyMember>,
    /// `_removedMembers` (flushed by `Save`).
    pub removed_members: Vec<u32>,
}

/// What `Family.Write` writes (a snapshot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyInfo {
    pub id: u32,
    /// (id, name, role, online, title)
    pub members: Vec<(u32, String, u8, bool, String)>,
}

impl FamilyInfo {
    /// `Family.Write`.
    pub fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.id).write_i32(self.members.len() as i32);
        for (id, name, role, online, title) in &self.members {
            s.write_u32(*id)
                .write_str(name)
                .write_u8(*role)
                .write_bool(*online)
                .write_str(title);
        }
    }
}

impl Family {
    pub fn info(&self) -> FamilyInfo {
        FamilyInfo {
            id: self.id,
            members: self
                .members
                .iter()
                .map(|m| (m.id, m.name.clone(), m.role, m.online(), m.title.clone()))
                .collect(),
        }
    }

    /// `GetMember(character)`.
    pub fn member(&self, id: u32) -> Option<&FamilyMember> {
        self.members.iter().find(|m| m.id == id)
    }

    fn member_mut(&mut self, id: u32) -> Option<&mut FamilyMember> {
        self.members.iter_mut().find(|m| m.id == id)
    }

    /// `RemoveMember(member)`.
    fn remove_member(&mut self, id: u32) {
        if let Some(i) = self.members.iter().position(|m| m.id == id) {
            self.members.remove(i);
        }
        self.removed_members.push(id);
    }

    /// `SendPacket(packet, exclude)`: every logged-in member except `exclude`.
    pub fn send_packet(&self, packet: &dyn ServerPacket, exclude: u32) {
        let mut out = crate::net::connection::Fanout::new(packet);
        for m in &self.members {
            if m.id != exclude {
                if let Some(c) = &m.character {
                    out.send(&c.conn);
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct State {
    /// `_families`.
    families: BTreeMap<u32, Family>,
    /// `_familyMembers` (character id -> family id).
    family_members: HashMap<u32, u32>,
}

/// How long a family invitation can be answered (same as the guild / party invitations).
pub(crate) const INVITATION_TIMEOUT_MS: i64 = 60_000;

/// The longest family title, in UTF-16 units (the client counts `wchar_t`); the shortest is 1.
///
/// The title rule is the client's `VNT_FAMILY_TITLE`, `NamePolicy` type 3: the 1.2 `x2game.dll`
/// registers `VNT_CHAR` 0, `VNT_SUMMONS` 1, `VNT_FACTION` 2, `VNT_FAMILY_TITLE` 3,
/// `VNT_CHAT_TAB` 4 and `VNT_PORTAL` 5 (type 5, the permissive row that allows spaces and
/// punctuation, belongs to portals). Its row in the 1.2 `xlcommon.dll` `NamePolicyInfo` table
/// (RVA 0x5db78, 24-byte rows indexed by type + 6 * locale) is minimum 1 / maximum 12 for the
/// Korean locale 0 (2 / 26 for locale 2, `en_us`) with only the mixed-case flag (0x4): neither
/// the space flag (0x1) nor the any-non-letter flag (0x10) is set, so a title is letters only.
/// The retail `ui_texts` 2324 (`FAMILY_TITLE`) reads "Enter 1 to 12 characters for the title
/// (excluding special characters)." in both languages, so the server keeps 1..=12.
pub(crate) const TITLE_MAX_CHARS: usize = 12;

/// The most members the client shows in a family: the 1.2 `x2game.dll` family reader (RVA
/// 0x8b85b0) clamps the member count it reads to 8 (`min(count, 8)`), so a ninth member would
/// never reach a client's list. The C# `Family.Write` only carries `// TODO max length 8`.
pub(crate) const MAX_FAMILY_MEMBERS: usize = 8;

/// The family member cap. The 1.2 compact has no family tables or `content_configs` keys for
/// them, and the 1.2 `Family.Write` only carries the comment `// TODO max length 8` (it never
/// enforced it). The 10.x server reads `family_max_count` from `content_configs` and its tests
/// use 8 for the base family, so the default is 8 and the value is configurable
/// (`Social:FamilyMaxMembers`, `Configurations/Social.json`). A family starts with two
/// members, so a configured value below 2 is treated as 2, and one above the client's limit
/// ([`MAX_FAMILY_MEMBERS`]) as 8.
pub fn max_members(server: &GameServer) -> usize {
    server
        .config
        .social
        .family_max_members
        .clamp(2, MAX_FAMILY_MEMBERS)
}

/// Whether `c` may appear in a family title: a letter (the type 3 row allows no spaces, digits
/// or punctuation) that the `family_members.title` column (`varchar(45)`, utf8mb3) can store.
/// A character outside the BMP (4 UTF-8 bytes: an emoji, CJK extension B, ...) would make the
/// `REPLACE INTO` fail and roll back the whole family's `save_family_rows` transaction.
fn title_char_allowed(c: char) -> bool {
    c.is_alphabetic() && c.len_utf8() <= 3
}

/// The client's title refusal (`FAMILY_TITLE` / `FAMILY_TITLE_BAD`), or `None` when `title` is
/// acceptable: 1 to [`TITLE_MAX_CHARS`] letters (see there). `FAMILY_TITLE_BAD` reuses the
/// `blocked_texts.check_name` list of the lobby, character names and guild names; that list is a
/// server rule, not part of the client's name policy.
fn validate_title(server: &GameServer, title: &str) -> Option<i16> {
    if !(1..=TITLE_MAX_CHARS).contains(&title.encode_utf16().count())
        || !title.chars().all(title_char_allowed)
    {
        return Some(error::FAMILY_TITLE);
    }
    crate::systems::social::expedition::name_is_blocked(server, title)
        .then_some(error::FAMILY_TITLE_BAD)
}

/// One pending family invitation: the inviter, the title it was sent with (validated at invite
/// time) and when it expires.
#[derive(Debug, Clone)]
struct FamilyInvitation {
    inviter: u32,
    title: String,
    sent_at: i64,
}

/// `FamilyManager`.
#[derive(Debug, Default)]
pub struct FamilyManager {
    state: Mutex<State>,
    /// Pending family invitations by invited character id. Deviation from the C# manager, which
    /// kept none and let any client join the family of any online character by naming it as the
    /// inviter (see PORTING.md). The title is stored with the invitation so a reply cannot
    /// change it or invalidate an invitation that was valid when it was sent.
    invitations: Mutex<HashMap<u32, FamilyInvitation>>,
}

/// The `family_members` rows of one family (`Family.Load`).
async fn load_members(pool: &MySqlPool, family_id: u32) -> Result<Vec<FamilyMember>> {
    let mut out = Vec::new();
    for r in sqlx::query(
        "SELECT CAST(character_id AS SIGNED) AS character_id, name, CAST(role AS SIGNED) AS role, title FROM family_members WHERE family_id = ?",
    )
    .bind(family_id)
    .fetch_all(pool)
    .await?
    {
        out.push(FamilyMember {
            character: None,
            id: int(&r, "character_id")? as u32,
            name: r.try_get("name")?,
            role: int(&r, "role")? as u8,
            title: r.try_get("title")?,
        });
    }
    Ok(out)
}

impl FamilyManager {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("families poisoned")
    }

    /// Records that `inviter` invited `invited` with `title` (replacing an older invitation of
    /// that character) and drops the expired ones, so the map stays bounded by the online
    /// players.
    fn record_invitation(&self, invited: u32, inviter: u32, title: &str, now_ms: i64) {
        let mut inv = self
            .invitations
            .lock()
            .expect("family invitations poisoned");
        inv.retain(|_, i| i.sent_at + INVITATION_TIMEOUT_MS >= now_ms);
        inv.insert(
            invited,
            FamilyInvitation {
                inviter,
                title: title.to_string(),
                sent_at: now_ms,
            },
        );
    }

    /// Consumes the pending invitation of `invited` when it names `inviter` and has not
    /// expired, returning the title stored when the invitation was sent. Any reply (accept or
    /// decline) ends the invitation; the reply packet's own title is not used.
    fn take_invitation(&self, invited: u32, inviter: u32, now_ms: i64) -> Option<String> {
        let mut inv = self
            .invitations
            .lock()
            .expect("family invitations poisoned");
        if inv.get(&invited)?.inviter != inviter {
            return None;
        }
        let stored = inv.remove(&invited)?;
        (stored.sent_at + INVITATION_TIMEOUT_MS >= now_ms).then_some(stored.title)
    }

    /// Whether `inviter` may invite `invited`, under one lock: the target must not be in a
    /// family (`FAMILY_OTHER_FAMILY`), and the inviter must either have no family or be its
    /// steward (role 1, the rule of kicking and titling) with room left (`FAMILY_MAXIMUM`).
    /// Returns the `ErrorMessageType` of the refusal.
    fn check_invite(
        &self,
        inviter: u32,
        invited: u32,
        max_members: usize,
    ) -> std::result::Result<(), i16> {
        let st = self.lock();
        if st.family_members.contains_key(&invited) {
            return Err(error::FAMILY_OTHER_FAMILY);
        }
        let Some(family_id) = st.family_members.get(&inviter).copied() else {
            return Ok(());
        };
        let Some(family) = st.families.get(&family_id) else {
            return Err(error::FAMILY_NOT_EXIST);
        };
        match family.member(inviter) {
            Some(m) if m.role == 1 => {
                if family.members.len() >= max_members {
                    Err(error::FAMILY_MAXIMUM)
                } else {
                    Ok(())
                }
            }
            _ => Err(error::FAMILY_NOT_OWNER),
        }
    }

    /// `Load()`: one family per distinct non-zero `characters.family`. Note (C#): a character
    /// listed in two families aborts the load (`_familyMembers.Add`).
    pub async fn load(&self, pool: &MySqlPool) -> Result<()> {
        tracing::info!("Loading families...");
        let ids: Vec<u32> =
            sqlx::query("SELECT DISTINCT CAST(family AS SIGNED) AS family FROM characters")
                .fetch_all(pool)
                .await?
                .iter()
                .map(|r| int(r, "family").map(|v| v as u32))
                .collect::<Result<_>>()?;
        let mut st = State::default();
        for id in ids {
            if id == 0 {
                continue;
            }
            let members = load_members(pool, id).await?;
            for m in &members {
                if st.family_members.insert(m.id, id).is_some() {
                    return Err(crate::error::GameError::Protocol(format!(
                        "family member {} is in two families",
                        m.id
                    )));
                }
            }
            st.families.insert(
                id,
                Family {
                    id,
                    members,
                    removed_members: Vec::new(),
                },
            );
        }
        tracing::info!("Loaded {} families", st.families.len());
        *self.lock() = st;
        Ok(())
    }

    /// Adds a loaded family (tests).
    pub fn insert(&self, family: Family) {
        let mut st = self.lock();
        for m in &family.members {
            st.family_members.insert(m.id, family.id);
        }
        st.families.insert(family.id, family);
    }

    /// `GetFamily(id)` (a copy).
    pub fn family(&self, id: u32) -> Option<Family> {
        self.lock().families.get(&id).cloned()
    }

    /// Updates a member's cached name (character rename); returns the family id (0 = none).
    ///
    /// The other rename path, the pre-select lobby edit (`managers::character::edit`), only
    /// reaches a character that never entered the world (its row has no play/leave time), so
    /// it cannot be a family member; the GM `/rename` path calls this.
    pub fn rename_member(&self, character_id: u32, new_name: &str) -> u32 {
        let mut st = self.lock();
        let Some(family_id) = st.family_members.get(&character_id).copied() else {
            return 0;
        };
        if let Some(family) = st.families.get_mut(&family_id) {
            if let Some(member) = family.members.iter_mut().find(|m| m.id == character_id) {
                member.name = new_name.to_string();
            }
        }
        family_id
    }

    /// `GetFamilyOfCharacter(characterId)` (0 = none).
    pub fn family_of_character(&self, character_id: u32) -> u32 {
        self.lock()
            .families
            .values()
            .find(|f| f.members.iter().any(|m| m.id == character_id))
            .map_or(0, |f| f.id)
    }

    pub fn count(&self) -> usize {
        self.lock().families.len()
    }

    /// A copy of every family (`SaveAllFamilies`).
    fn take_snapshot(&self, id: u32) -> Option<Family> {
        let mut st = self.lock();
        let f = st.families.get_mut(&id)?;
        let snap = f.clone();
        f.removed_members.clear();
        Some(snap)
    }
}

fn family_chat(server: &GameServer, id: u32) -> crate::managers::chat::ChannelRef {
    server.chat.family_chat(id, &server.ids.chat)
}

/// `AddFamilyMember(family, character, title)`: the first member is the steward (role 1, empty
/// title). Returns `false` when the character is already registered in a family (C#:
/// `_familyMembers.Add` throws after the member was appended to the family list; here the check
/// comes first, so a refused member never lingers in the list).
fn add_family_member(
    server: &GameServer,
    st: &mut State,
    family_id: u32,
    p: &PlayerRef,
    title: Option<&str>,
) -> bool {
    if st.family_members.contains_key(&p.character_id) {
        tracing::error!(
            "AddFamilyMember: {} is already a member of family {}",
            p.character_id,
            st.family_members[&p.character_id]
        );
        return false;
    }
    let name = p.data().unit.name.clone();
    let family = st.families.get_mut(&family_id).expect("family");
    let is_owner = family.members.is_empty();
    let title = if is_owner { "" } else { title.unwrap_or("") };
    family.members.push(FamilyMember {
        character: Some(p.clone()),
        id: p.character_id,
        name,
        role: u8::from(is_owner),
        title: title.to_string(),
    });
    st.family_members.insert(p.character_id, family_id);
    p.data().family = family_id;
    family_chat(server, family_id).join(p);
    // ENROLL_FAMILY: queued, the family manager's lock is held here.
    crate::systems::refresh_achievements_later(server, p);
    true
}

/// `FamilyManager.SaveFamily(family)` (`Family.Save` in its own transaction).
pub async fn save_family(server: &GameServer, id: u32) {
    let Some(f) = server.systems.social.families.take_snapshot(id) else {
        return;
    };
    if let Err(e) = save_family_rows(&server.db, &f).await {
        tracing::error!("Family {id} save failed: {e}");
    }
}

/// `Family.Save(connection, transaction)`.
pub async fn save_family_rows(pool: &MySqlPool, f: &Family) -> Result<()> {
    let mut tx = pool.begin().await?;
    if !f.removed_members.is_empty() {
        let list = f
            .removed_members
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        sqlx::query(&format!(
            "DELETE FROM family_members WHERE character_id IN ({list})"
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!(
            "UPDATE characters SET family = 0 WHERE `characters`.`id` IN ({list})"
        ))
        .execute(&mut *tx)
        .await?;
    }
    for m in &f.members {
        sqlx::query("REPLACE INTO family_members(`character_id`,`family_id`,`name`,`role`,`title`) VALUES (?,?,?,?,?)")
            .bind(m.id)
            .bind(f.id)
            .bind(&m.name)
            .bind(m.role)
            .bind(&m.title)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// `InviteToFamily(inviter, invitedCharacterName, title)`. Note (C#): the family id sent is
/// always 1.
pub fn invite_to_family(server: &GameServer, inviter: &PlayerRef, invited_name: &str, title: &str) {
    // `normalize_name` trims the name for the registry lookup below, so the online lookup trims
    // it too; otherwise "Bob " would report an online Bob as offline.
    let invited_name = invited_name.trim();
    let Some(invited) = server.world.character_by_name(invited_name) else {
        // An offline character is still registered in the `NameManager`; a name that is not
        // there belongs to nobody (the same split the lobby uses for a new character name).
        // An offline character has no loaded block list, so this reply cannot honour a block
        // (the same limit as `friend::add_friend`); only an online target drops it silently.
        let known = server.names.get_character_id(&normalize_name(invited_name)) != 0;
        send_error(
            inviter,
            if known {
                error::FAMILY_INVITE_OFFLINE
            } else {
                error::FAMILY_NO_TARGET
            },
        );
        return;
    };
    if invited.character_id == inviter.character_id {
        send_error(inviter, error::FAMILY_SELF);
        return;
    }
    // Silently drop the invitation when the inviter is blocked (capture-pending, case
    // `BLOCK-LIST-1`).
    if crate::systems::social::friend::is_blocked_by(&invited, inviter.character_id) {
        return;
    }
    let fm = &server.systems.social.families;
    if let Err(code) = fm.check_invite(
        inviter.character_id,
        invited.character_id,
        max_members(server),
    ) {
        send_error(inviter, code);
        return;
    }
    if let Some(code) = validate_title(server, title) {
        send_error(inviter, code);
        return;
    }
    fm.record_invitation(
        invited.character_id,
        inviter.character_id,
        title,
        crate::world::now_ms(),
    );
    let inviter_name = inviter.data().unit.name.clone();
    invited.send(SCFamilyInvitationPacket {
        invitor_id: inviter.character_id,
        invitor_name: inviter_name,
        family_id: 1,
        role: title.to_string(),
    });
}

/// `ReplyToInvite(invitorId, invitedChar, join, title)`.
pub async fn reply_to_invite(
    server: &GameServer,
    invitor_id: u32,
    invited: &PlayerRef,
    join: bool,
    reply_title: &str,
) {
    // The reply must answer an invitation that `invitor_id` sent this character; the id in the
    // packet alone proves nothing. The title stored with the invitation is the one validated at
    // invite time, so the reply cannot change it or invalidate an accepted invitation.
    let fm = &server.systems.social.families;
    let Some(invite_title) =
        fm.take_invitation(invited.character_id, invitor_id, crate::world::now_ms())
    else {
        tracing::warn!(
            character = %invited.data().unit.name,
            invitor_id,
            join,
            "FamilyReplyInvitation without a pending invitation"
        );
        // `FAMILY_NO_INVITOR` ("That player has disconnected.") answers an accept only; a
        // decline of an invitation that is gone needs no reply.
        if join {
            send_error(invited, error::FAMILY_NO_INVITOR);
        }
        return;
    };
    if !join {
        // The invited character declined: the inviter sees why the invitation ended.
        if let Some(invitor) = server.world.character_by_id(invitor_id) {
            send_error(&invitor, error::FAMILY_REJECT);
        }
        return;
    }
    if fm.family_of_character(invited.character_id) != 0 {
        send_error(invited, error::FAMILY_HAS_FAMILY);
        return;
    }
    let Some(invitor) = server.world.character_by_id(invitor_id) else {
        send_error(invited, error::FAMILY_NO_INVITOR);
        return;
    };
    if reply_title != invite_title {
        tracing::debug!(
            character = %invited.data().unit.name,
            invitor_id,
            reply_title = %reply_title,
            invite_title = %invite_title,
            "FamilyReplyInvitation title differs from the invitation; using the invitation title"
        );
    }
    let cap = max_members(server);
    let saved = {
        let mut st = fm.lock();
        // The invited character may have joined another family since the invitation; the check
        // and the add run under the one family lock, so a family cannot fill or change between.
        if st.family_members.contains_key(&invited.character_id) {
            drop(st);
            send_error(invited, error::FAMILY_HAS_FAMILY);
            return;
        }
        let invitor_family = st.family_members.get(&invitor_id).copied().unwrap_or(0);
        if invitor_family == 0 {
            // CreateFamily(invitor, invitedChar, title)
            let id = match server.ids.family.get_next_id() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("{e}");
                    drop(st);
                    send_error(invited, error::FAMILY_CREATE);
                    return;
                }
            };
            st.families.insert(
                id,
                Family {
                    id,
                    members: Vec::new(),
                    removed_members: Vec::new(),
                },
            );
            if !add_family_member(server, &mut st, id, &invitor, None)
                || !add_family_member(server, &mut st, id, invited, Some(&invite_title))
            {
                tracing::error!("ReplyToInvite: creating family {id} failed");
                drop(st);
                send_error(invited, error::FAMILY_CREATE);
                return;
            }
            let f = &st.families[&id];
            f.send_packet(&SCFamilyCreatedPacket { family: f.info() }, 0);
            id
        } else {
            let (steward, full) = match st.families.get(&invitor_family) {
                Some(f) => (
                    f.member(invitor_id).is_some_and(|m| m.role == 1),
                    f.members.len() >= cap,
                ),
                None => {
                    // C#: `_families[invitor.Family]` throws.
                    tracing::error!("ReplyToInvite: no family {invitor_family}");
                    drop(st);
                    send_error(invited, error::FAMILY_NOT_EXIST);
                    return;
                }
            };
            if !steward {
                // The inviter lost the stewardship since it invited: it is told, the invited
                // character gets no reply (`FAMILY_NO_INVITOR` says the inviter disconnected,
                // which is not what happened, and no other text fits).
                drop(st);
                send_error(&invitor, error::FAMILY_NOT_OWNER);
                return;
            }
            if full {
                drop(st);
                send_error(invited, error::FAMILY_MAXIMUM);
                send_error(&invitor, error::FAMILY_MAXIMUM);
                return;
            }
            if !add_family_member(
                server,
                &mut st,
                invitor_family,
                invited,
                Some(&invite_title),
            ) {
                drop(st);
                send_error(invited, error::FAMILY_CREATE);
                return;
            }
            let f = &st.families[&invitor_family];
            f.send_packet(
                &SCFamilyMemberAddedPacket {
                    family: f.info(),
                    added_index: f.members.len() as i32 - 1,
                },
                0,
            );
            invitor_family
        }
    };
    save_family(server, saved).await;
}

/// `OnCharacterLogin(character)`: family description to the character, online status to the
/// family (the character included).
pub fn on_character_login(server: &GameServer, p: &PlayerRef) {
    let family_id = p.data().family;
    let mut st = server.systems.social.families.lock();
    let member_family = st.family_members.get(&p.character_id).copied();
    let valid = st.families.contains_key(&family_id) && member_family.is_some();
    if !valid {
        // Family no longer valid
        drop(st);
        p.data().family = 0;
        return;
    }
    // `_familyMembers[character.Id]` is the member object of its own family.
    let mf = member_family.expect("checked");
    if let Some(m) = st
        .families
        .get_mut(&mf)
        .and_then(|f| f.member_mut(p.character_id))
    {
        m.character = Some(p.clone());
    }
    let f = &st.families[&family_id];
    family_chat(server, f.id).join(p);
    p.send(SCFamilyDescPacket { family: f.info() });
    f.send_packet(
        &SCFamilyMemberOnlinePacket {
            family_id: f.id,
            member_id: p.character_id,
            online: true,
        },
        0,
    );
}

/// `OnCharacterLogout(character)`.
pub fn on_character_logout(server: &GameServer, p: &PlayerRef) {
    let family_id = p.data().family;
    let mut st = server.systems.social.families.lock();
    let Some(f) = st.families.get_mut(&family_id) else {
        // C#: `_families[character.Family]` throws.
        tracing::error!("OnCharacterLogout: no family {family_id}");
        return;
    };
    match f.member_mut(p.character_id) {
        Some(m) => m.character = None,
        None => {
            tracing::error!(
                "OnCharacterLogout: {} is not in family {family_id}",
                p.character_id
            );
            return;
        }
    }
    family_chat(server, f.id).leave(p);
    f.send_packet(
        &SCFamilyMemberOnlinePacket {
            family_id: f.id,
            member_id: p.character_id,
            online: false,
        },
        p.character_id,
    );
}

/// `DisbandFamily(family)` (with the state lock held); returns the family to save.
fn disband_family(server: &GameServer, st: &mut State, family_id: u32) {
    let f = st.families.get_mut(&family_id).expect("family");
    let removed = SCFamilyRemovedPacket { family_id };
    let members: Vec<FamilyMember> = f.members.iter().rev().cloned().collect();
    for m in &members {
        if let Some(c) = &m.character {
            family_chat(server, family_id).leave(c);
            c.send(removed.clone());
            c.data().family = 0;
        }
        f.remove_member(m.id);
    }
    for m in &members {
        st.family_members.remove(&m.id);
    }
}

/// Saves (or, after a disband, saves and removes) a family.
async fn save_or_finish_disband(server: &GameServer, family_id: u32, disbanded: bool) {
    save_family(server, family_id).await;
    if disbanded {
        server
            .systems
            .social
            .families
            .lock()
            .families
            .remove(&family_id);
    }
}

/// `LeaveFamily(character)`.
pub async fn leave_family(server: &GameServer, p: &PlayerRef) {
    let family_id = p.data().family;
    if family_id == 0 {
        // The client can send CSFamilyLeave from a character that is not in a family; that is
        // an expected reply, not an inconsistent server state.
        tracing::debug!("LeaveFamily: {} has no family", p.character_id);
        send_error(p, error::FAMILY_NOT_EXIST);
        return;
    }
    let disband = {
        let mut st = server.systems.social.families.lock();
        let missing = st
            .families
            .get(&family_id)
            .is_none_or(|f| f.member(p.character_id).is_none());
        if missing {
            tracing::error!("LeaveFamily: {} has no family {family_id}", p.character_id);
            drop(st);
            send_error(p, error::FAMILY_NOT_EXIST);
            return;
        }
        let f = st.families.get_mut(&family_id).expect("checked");
        p.data().family = 0;
        f.remove_member(p.character_id);
        st.family_members.remove(&p.character_id);
        let f = &st.families[&family_id];
        p.send(SCFamilyRemovedPacket { family_id });
        f.send_packet(
            &SCFamilyMemberRemovedPacket {
                family_id,
                kicked: false,
                member_id: p.character_id,
            },
            0,
        );
        family_chat(server, family_id).leave(p);
        let disband = f.members.len() < 2;
        if disband {
            disband_family(server, &mut st, family_id);
        }
        disband
    };
    save_or_finish_disband(server, family_id, disband).await;
}

/// `Character.Load(id) != null` for an offline character (`KickMember`).
async fn character_exists(server: &GameServer, id: u32) -> bool {
    match sqlx::query("SELECT CAST(id AS SIGNED) AS id FROM characters WHERE id = ?")
        .bind(id)
        .fetch_optional(&server.db)
        .await
    {
        Ok(r) => r.is_some(),
        Err(e) => {
            tracing::error!("Character.Load({id}): {e}");
            false
        }
    }
}

/// `KickMember(kicker, kickedId)`: steward only; disbands the family when fewer than two
/// members remain.
pub async fn kick_member(server: &GameServer, kicker: &PlayerRef, kicked_id: u32) {
    let family_id = kicker.data().family;
    if family_id == 0 {
        send_error(kicker, error::FAMILY_NOT_EXIST);
        return;
    }
    if kicked_id == kicker.character_id {
        send_error(kicker, error::FAMILY_KICK_SELF);
        return;
    }
    let kicked_is_member = {
        let st = server.systems.social.families.lock();
        let Some(f) = st.families.get(&family_id) else {
            tracing::error!("KickMember: no family {family_id}");
            drop(st);
            send_error(kicker, error::FAMILY_NOT_EXIST);
            return;
        };
        match f.member(kicker.character_id) {
            Some(m) if m.role == 1 => {}
            Some(_) => {
                // Only the steward can kick
                drop(st);
                send_error(kicker, error::FAMILY_NOT_OWNER);
                return;
            }
            None => {
                tracing::error!("KickMember: kicker {} not in family", kicker.character_id);
                drop(st);
                send_error(kicker, error::FAMILY_NOT_EXIST);
                return;
            }
        }
        f.member(kicked_id).is_some()
    };
    let online = server.world.character_by_id(kicked_id);
    // A member needs no `Character.Load` check (and one whose character row is gone must stay
    // kickable, or it would hold a cap slot forever); only a stranger is looked up.
    if !kicked_is_member && online.is_none() && !character_exists(server, kicked_id).await {
        send_error(kicker, error::FAMILY_NO_TARGET);
        return;
    }
    let disband = {
        let mut st = server.systems.social.families.lock();
        let Some(f) = st.families.get_mut(&family_id) else {
            drop(st);
            send_error(kicker, error::FAMILY_NOT_EXIST);
            return;
        };
        // The `character_exists` await above released the lock: re-check the kicker's role
        // under the same lock as the removal.
        if !f.member(kicker.character_id).is_some_and(|m| m.role == 1) {
            drop(st);
            send_error(kicker, error::FAMILY_NOT_OWNER);
            return;
        }
        if f.member(kicked_id).is_none() {
            // C#: RemoveMember(null) throws.
            tracing::error!("KickMember: {kicked_id} is not in family {family_id}");
            drop(st);
            send_error(kicker, error::FAMILY_NO_TARGET);
            return;
        }
        if let Some(k) = &online {
            k.data().family = 0;
        }
        f.remove_member(kicked_id);
        st.family_members.remove(&kicked_id);
        if let Some(k) = &online {
            family_chat(server, family_id).leave(k);
            k.send(SCFamilyRemovedPacket { family_id });
        }
        let f = &st.families[&family_id];
        f.send_packet(
            &SCFamilyMemberRemovedPacket {
                family_id,
                kicked: true,
                member_id: kicked_id,
            },
            0,
        );
        let disband = f.members.len() < 2;
        if disband {
            disband_family(server, &mut st, family_id);
        }
        disband
    };
    save_or_finish_disband(server, family_id, disband).await;
}

/// `ChangeTitle(owner, memberId, newTitle)` (steward only).
pub fn change_title(server: &GameServer, owner: &PlayerRef, member_id: u32, new_title: &str) {
    let _ = update_title(server, owner, member_id, new_title);
}

/// Applies an authorized title change and persists the resulting family snapshot.
pub async fn change_title_and_save(
    server: &GameServer,
    owner: &PlayerRef,
    member_id: u32,
    new_title: &str,
) {
    if let Some(id) = update_title(server, owner, member_id, new_title) {
        save_family(server, id).await;
    }
}

fn update_title(
    server: &GameServer,
    owner: &PlayerRef,
    member_id: u32,
    new_title: &str,
) -> Option<u32> {
    let family_id = owner.data().family;
    if family_id == 0 {
        send_error(owner, error::FAMILY_NOT_EXIST);
        return None;
    }
    let mut st = server.systems.social.families.lock();
    let Some(f) = st.families.get(&family_id) else {
        tracing::error!("ChangeTitle: no family {family_id}");
        drop(st);
        send_error(owner, error::FAMILY_NOT_EXIST);
        return None;
    };
    match f.member(owner.character_id) {
        Some(m) if m.role == 1 => {}
        Some(_) => {
            drop(st);
            send_error(owner, error::FAMILY_NOT_OWNER);
            return None;
        }
        None => {
            tracing::error!("ChangeTitle: {} not in family", owner.character_id);
            drop(st);
            send_error(owner, error::FAMILY_NOT_EXIST);
            return None;
        }
    }
    if member_id == owner.character_id {
        drop(st);
        send_error(owner, error::FAMILY_OWNER_TITLE);
        return None;
    }
    // The target must belong to the steward's own family.
    let Some(&mf) = st.family_members.get(&member_id) else {
        tracing::error!("ChangeTitle: unknown member {member_id}");
        drop(st);
        send_error(owner, error::FAMILY_NO_TARGET);
        return None;
    };
    if mf != family_id {
        drop(st);
        send_error(owner, error::FAMILY_OTHER_FAMILY);
        return None;
    }
    if let Some(code) = validate_title(server, new_title) {
        drop(st);
        send_error(owner, code);
        return None;
    }
    if let Some(m) = st
        .families
        .get_mut(&mf)
        .and_then(|f| f.member_mut(member_id))
    {
        m.title = new_title.to_string();
    }
    st.families[&family_id].send_packet(
        &SCFamilyTitleChangedPacket {
            family_id,
            member_id,
            title: new_title.to_string(),
        },
        0,
    );
    Some(family_id)
}

/// `ChangeOwner(previousOwner, memberId)` (steward only).
pub fn change_owner(server: &GameServer, previous: &PlayerRef, member_id: u32) {
    let _ = update_owner(server, previous, member_id);
}

/// Applies an authorized steward change and persists the resulting family snapshot.
pub async fn change_owner_and_save(server: &GameServer, previous: &PlayerRef, member_id: u32) {
    if let Some(id) = update_owner(server, previous, member_id) {
        save_family(server, id).await;
    }
}

fn update_owner(server: &GameServer, previous: &PlayerRef, member_id: u32) -> Option<u32> {
    let family_id = previous.data().family;
    if family_id == 0 {
        send_error(previous, error::FAMILY_NOT_EXIST);
        return None;
    }
    let mut st = server.systems.social.families.lock();
    let Some(f) = st.families.get(&family_id) else {
        tracing::error!("ChangeOwner: no family {family_id}");
        drop(st);
        send_error(previous, error::FAMILY_NOT_EXIST);
        return None;
    };
    match f.member(previous.character_id) {
        Some(m) if m.role == 1 => {}
        Some(_) => {
            drop(st);
            send_error(previous, error::FAMILY_NOT_OWNER);
            return None;
        }
        None => {
            tracing::error!("ChangeOwner: {} not in family", previous.character_id);
            drop(st);
            send_error(previous, error::FAMILY_NOT_EXIST);
            return None;
        }
    }
    if member_id == previous.character_id {
        drop(st);
        send_error(previous, error::FAMILY_ALREADY_OWNER);
        return None;
    }
    let Some(&mf) = st.family_members.get(&member_id) else {
        tracing::error!("ChangeOwner: unknown member {member_id}");
        drop(st);
        send_error(previous, error::FAMILY_NO_TARGET);
        return None;
    };
    if mf != family_id {
        drop(st);
        send_error(previous, error::FAMILY_OTHER_FAMILY);
        return None;
    }
    if let Some(m) = st
        .families
        .get_mut(&mf)
        .and_then(|f| f.member_mut(member_id))
    {
        m.role = 1;
    }
    if let Some(m) = st
        .families
        .get_mut(&family_id)
        .and_then(|f| f.member_mut(previous.character_id))
    {
        m.role = 0;
    }
    let f = &st.families[&family_id];
    f.send_packet(
        &SCFamilyOwnerChangedPacket {
            family_id,
            member_id,
        },
        0,
    );
    f.send_packet(&SCFamilyDescPacket { family: f.info() }, 0);
    Some(family_id)
}

/// `FamilyManager.LeaveFamily(character)` for a character that is not in the world (character
/// deletion). The C# also sends `SCFamilyRemovedPacket` to the (lobby) character.
pub async fn leave_family_offline(server: &GameServer, character_id: u32, family_id: u32) {
    let disband = {
        let mut st = server.systems.social.families.lock();
        let Some(f) = st.families.get_mut(&family_id) else {
            tracing::error!("LeaveFamily: no family {family_id}");
            return;
        };
        if f.member(character_id).is_none() {
            tracing::error!("LeaveFamily: {character_id} is not in family {family_id}");
            return;
        }
        f.remove_member(character_id);
        st.family_members.remove(&character_id);
        let f = &st.families[&family_id];
        f.send_packet(
            &SCFamilyMemberRemovedPacket {
                family_id,
                kicked: false,
                member_id: character_id,
            },
            0,
        );
        let disband = f.members.len() < 2;
        if disband {
            disband_family(server, &mut st, family_id);
        }
        disband
    };
    save_or_finish_disband(server, family_id, disband).await;
}
