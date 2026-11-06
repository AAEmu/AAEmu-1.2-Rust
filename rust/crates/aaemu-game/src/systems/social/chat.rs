//! `Core/Managers/ChatManager` + `Models/Game/Chat/ChatChannel`: faction / nation / court room /
//! zone / party / raid / guild / family channels with join / leave / send and
//! `CleanUpChannels`. (Moved here from `managers::chat`, which re-exports this module.)

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::data::world_data::factions;
use crate::managers::id::IdManager;
use crate::models::character::Character;
use crate::models::item::UnixTime;
use crate::net::packet::ServerPacket;
use crate::packets::world::{
    ChatSender, SCChatMessagePacket, SCErrorMsgPacket, SCJoinedChatChannelPacket,
    SCLeavedChatChannelPacket,
};
use crate::systems::social::config::ChatConfig;
use crate::systems::social::util::ChatFailure;
use crate::world::PlayerRef;

/// `ChatType`.
pub mod chat_type {
    pub const WHISPERED: i16 = -4;
    pub const WHISPER: i16 = -3;
    pub const SYSTEM: i16 = -2;
    pub const NOTICE: i16 = -1;
    pub const WHITE: i16 = 0;
    pub const SHOUT: i16 = 1;
    pub const TRADE: i16 = 2;
    pub const GROUP_FIND: i16 = 3;
    pub const PARTY: i16 = 4;
    pub const RAID: i16 = 5;
    pub const REGION: i16 = 6;
    pub const CLAN: i16 = 7;
    pub const SYSTEM2: i16 = 8;
    pub const FAMILY: i16 = 9;
    pub const RAID_LEADER: i16 = 10;
    pub const JUDGE: i16 = 11;
    pub const ALLY: i16 = 14;
    pub const USER: i16 = 15;
}

/// `SCChatSpamDelayPacket`'s error: the 1.2 client passes localized message `0x108` when its own
/// `FUN_393C2540` throttle refuses a message.
pub const CHAT_SPAM_DELAY_ERROR: i16 = 0x108;

/// The client's `FUN_393C2540` rate limit: chats `0..=14` map to a group, and a message is
/// refused while the previous one's delay has not elapsed. Negative chat types (whispers) and
/// user channels (`>= 15`) are not limited. The server keeps the same per-session `last_send`
/// array so a modified client cannot bypass the limit.
///
/// The client stamps `last_send` when it sends; the server stamps it when the message arrives, so
/// network jitter can make a stock client's message at the delay arrive early. A small grace
/// (`min(250 ms, delay / 4)`) absorbs that without moving the boundary for a real flood.
pub fn chat_spam_allows(
    config: &ChatConfig,
    last_send: &mut [Option<Instant>; 15],
    chat_type: i16,
    now: Instant,
) -> bool {
    let Ok(chat_type) = usize::try_from(chat_type) else {
        return true;
    };
    if chat_type > 14 {
        return true;
    }
    let group = config.type_group(chat_type);
    let delay_ms = config.delay_ms(group);
    let grace = Duration::from_millis(u64::from((delay_ms / 4).min(250)));
    if let Some(last) = last_send[group] {
        if now + grace < last {
            return false;
        }
    }
    last_send[group] = Some(now + Duration::from_millis(u64::from(delay_ms)));
    true
}

/// Access level from which a character is treated as a GM: the same threshold as the
/// closed-zone bypass (`world::zone_access`), so a character with it is exempt from
/// `Chat.MinLevelByChatType`. Account-level elevation alone is not consulted here (the chat
/// path is synchronous and does not read the account row).
pub const GM_ACCESS_LEVEL: i32 = 100;

/// Why an outgoing chat message was refused before routing (SL4b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRefusal {
    /// Below `Chat.MinLevelByChatType` for this chat type.
    LevelTooLow,
    /// `characters.chat_restricted_until` is still in the future (`/restrict_chat`).
    /// `remaining_seconds` is `until - now` at the refusal (the client shows
    /// `duration / 60 + 1` minutes), `until` the persisted end.
    Muted {
        remaining_seconds: u32,
        until: UnixTime,
    },
}

/// The SL4b gate: the configured per-type minimum level (GMs exempt) and the persisted mute.
///
/// Both checks read (and the mute expiry clears) the same [`Character`], so the caller must
/// hold its `data()` lock once and pass the guard's character here. The mute check runs after
/// the level check; both rules are independent, and the order is not evidenced.
pub fn chat_refusal(
    config: &ChatConfig,
    c: &mut Character,
    chat_type: i16,
    now: UnixTime,
) -> Option<ChatRefusal> {
    if c.access_level < GM_ACCESS_LEVEL && c.unit.level < config.min_level(chat_type) {
        return Some(ChatRefusal::LevelTooLow);
    }
    if c.chat_restricted(now) {
        let remaining_seconds =
            u32::try_from(c.chat_restricted_until.saturating_sub(now)).unwrap_or(u32::MAX);
        return Some(ChatRefusal::Muted {
            remaining_seconds,
            until: c.chat_restricted_until,
        });
    }
    None
}

/// Tells the sender why the message was dropped. The level refusal is
/// `CHAT_CANNOT_SEND_SINCE_LEVEL_LOW` 786 (the localized text asks for `$1`, the level; the
/// `SCErrorMsgPacket` argument mechanism is UNKNOWN, so no argument is sent). The mute refusal
/// is `SCRestrictInfoPacket`, which the client's `FUN_393c4170` turns into the
/// `restrict_remain_time` system line ("chat banned, released in `$1` minutes") using the
/// remaining seconds. Retail's trigger for that notice is UNKNOWN; the refused chat attempt is
/// INFERRED (`CHAT-RESTRICT-1`).
pub fn send_chat_refusal(p: &PlayerRef, refusal: ChatRefusal) {
    match refusal {
        ChatRefusal::LevelTooLow => p.send(SCErrorMsgPacket::new(
            crate::systems::economy::error_message::CHAT_CANNOT_SEND_SINCE_LEVEL_LOW,
            0,
            true,
        )),
        ChatRefusal::Muted {
            remaining_seconds,
            until,
        } => {
            let chname = p.data().unit.name.clone();
            p.send(super::packets::chat::SCRestrictInfoPacket {
                chname,
                index: 0,
                code: 0,
                duration: remaining_seconds,
                // The mute start is not persisted; the client ignores both dates (INFERRED).
                start: until.saturating_sub(i64::from(remaining_seconds)),
                end: until,
            })
        }
    }
}

/// `CourtRoomRegion`.
pub mod court_room_region {
    pub const NUIAN: u8 = 0;
    pub const HARANYAN: u8 = 1;
    pub const INVALID: u8 = 0xFF;
}

/// `ChatChannel`.
#[derive(Debug)]
pub struct ChatChannel {
    pub chat_type: i16,
    pub sub_type: i16,
    pub faction: u32,
    pub internal_id: u32,
    /// `InternalName` (party / raid channels rename themselves on every lookup).
    internal_name: Mutex<String>,
    members: Mutex<Vec<PlayerRef>>,
}

impl ChatChannel {
    fn new(chat_type: i16, sub_type: i16, faction: u32, internal_id: u32, name: String) -> Self {
        Self {
            chat_type,
            sub_type,
            faction,
            internal_id,
            internal_name: Mutex::new(name),
            members: Mutex::new(Vec::new()),
        }
    }

    pub fn internal_name(&self) -> String {
        self.internal_name.lock().expect("channel poisoned").clone()
    }

    fn set_internal_name(&self, name: String) {
        *self.internal_name.lock().expect("channel poisoned") = name;
    }

    /// `GetMembersSnapshot`.
    pub fn members(&self) -> Vec<PlayerRef> {
        self.members.lock().expect("channel poisoned").clone()
    }

    pub fn contains(&self, p: &PlayerRef) -> bool {
        self.members
            .lock()
            .expect("channel poisoned")
            .iter()
            .any(|x| Arc::ptr_eq(x, p))
    }

    /// `JoinChannel`.
    pub fn join(&self, p: &PlayerRef) -> bool {
        {
            let mut m = self.members.lock().expect("channel poisoned");
            if m.iter().any(|x| Arc::ptr_eq(x, p)) {
                return false;
            }
            m.push(p.clone());
        }
        if self.chat_type == chat_type::USER {
            p.send(super::packets::chat::user_joined(self));
        } else {
            p.send(SCJoinedChatChannelPacket {
                chat_type: self.chat_type,
                sub_type: self.sub_type,
                faction_id: self.faction,
            });
        }
        true
    }

    /// `LeaveChannel`.
    pub fn leave(&self, p: &PlayerRef) -> bool {
        let removed = {
            let mut m = self.members.lock().expect("channel poisoned");
            match m.iter().position(|x| Arc::ptr_eq(x, p)) {
                Some(i) => {
                    m.remove(i);
                    true
                }
                None => false,
            }
        };
        if removed {
            p.send(SCLeavedChatChannelPacket {
                chat_type: self.chat_type,
                sub_type: self.sub_type,
                faction_id: self.faction,
            });
        }
        removed
    }

    /// `SendMessage(origin, msg, ability, languageType)`: `origin ?? member` as sender. The
    /// channel's sub type / faction select the chat identity of every copy.
    pub fn send_message(
        &self,
        origin: Option<&ChatSender>,
        member_sender: impl Fn(&PlayerRef) -> ChatSender,
        msg: &str,
        ability: i32,
        language_type: u8,
    ) -> usize {
        let members = self.members();
        for m in &members {
            let message = SCChatMessagePacket {
                chat_type: self.chat_type,
                sender: Some(origin.cloned().unwrap_or_else(|| member_sender(m))),
                message: msg.to_string(),
                ability,
                language_type,
            };
            m.send(super::packets::chat::channel_message(self, &message));
        }
        members.len()
    }

    /// `SendPacket` for a chat message: the channel identity is wrapped once and the generic
    /// `send_packet` fan-out is reused.
    pub fn send_chat_packet(&self, message: &SCChatMessagePacket) -> usize {
        let packet = super::packets::chat::channel_message(self, message);
        self.send_packet(&packet)
    }

    /// `SendPacket(packet)`.
    pub fn send_packet(&self, packet: &dyn ServerPacket) -> usize {
        let members = self.members();
        let mut out = crate::net::connection::Fanout::new(packet);
        for m in &members {
            out.send(&m.conn);
        }
        members.len()
    }

    fn is_empty(&self) -> bool {
        self.members.lock().expect("channel poisoned").is_empty()
    }
}

pub type ChannelRef = Arc<ChatChannel>;

/// `Race` values used by the nation channels.
pub mod race {
    pub const NUIAN: u8 = 1;
    pub const HARIHARAN: u8 = 5;
}

type Channels<K> = Mutex<BTreeMap<K, ChannelRef>>;

// Explicit emulator budgets, not established retail 1.2 limits. Bound decoded UTF-8 bytes
// (including replacement characters from malformed input), stored state and client frames.
pub const USER_CHANNEL_NAME_BYTES: usize = 48;
pub const USER_CHANNEL_PASSWORD_BYTES: usize = 64;
pub const USER_CHANNELS_PER_SESSION: usize = 8;
pub const USER_CHANNELS_TOTAL: usize = 1024;

/// Temporary user channels have no persistent owner or administration policy.
struct UserChannel {
    channel: ChannelRef,
    password: String,
}

impl std::fmt::Debug for UserChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserChannel")
            .field("channel", &self.channel)
            .finish_non_exhaustive()
    }
}

/// `ChatManager`.
#[derive(Debug)]
pub struct ChatManager {
    null_channel: ChannelRef,
    faction_channels: Channels<u32>,
    nation_channels: Channels<i64>,
    zone_channels: Channels<u32>,
    party_channels: Channels<u32>,
    raid_channels: Channels<u32>,
    guild_channels: Channels<u32>,
    family_channels: Channels<u32>,
    court_room_channels: Channels<u8>,
    user_channels: Mutex<BTreeMap<u32, UserChannel>>,
}

impl ChatManager {
    /// `new` + `Initialize()` (faction, nation and court room channels).
    pub fn new(chat_ids: &IdManager) -> Self {
        let next = || chat_ids.get_next_id().unwrap_or(0);
        let m = Self {
            null_channel: Arc::new(ChatChannel::new(chat_type::WHITE, 0, 0, 0, "Null".into())),
            faction_channels: Mutex::default(),
            nation_channels: Mutex::default(),
            zone_channels: Mutex::default(),
            party_channels: Mutex::default(),
            raid_channels: Mutex::default(),
            guild_channels: Mutex::default(),
            family_channels: Mutex::default(),
            court_room_channels: Mutex::default(),
            user_channels: Mutex::default(),
        };
        {
            let mut f = m.faction_channels.lock().expect("chat poisoned");
            for (id, name) in [
                (factions::NUIA_ALLIANCE, "Nuia"),
                (factions::HARANYA_ALLIANCE, "Haranya"),
                (factions::PIRATE, "Pirate"),
            ] {
                f.insert(
                    id,
                    Arc::new(ChatChannel::new(
                        chat_type::ALLY,
                        0,
                        id,
                        next(),
                        name.into(),
                    )),
                );
            }
            let mut n = m.nation_channels.lock().expect("chat poisoned");
            for (r, fid, name) in [
                (race::NUIAN, factions::NUIA_ALLIANCE, "Nuian-Elf-Dwarf"),
                (
                    race::HARIHARAN,
                    factions::HARANYA_ALLIANCE,
                    "Harani-Firran-Warborn",
                ),
            ] {
                n.insert(
                    Self::nation_key(r),
                    Arc::new(ChatChannel::new(
                        chat_type::REGION,
                        0,
                        fid,
                        next(),
                        name.into(),
                    )),
                );
            }
            let mut c = m.court_room_channels.lock().expect("chat poisoned");
            for (region, fid, name) in [
                (
                    court_room_region::NUIAN,
                    factions::NUIA_ALLIANCE,
                    "Nuian Court",
                ),
                (
                    court_room_region::HARANYAN,
                    factions::HARANYA_ALLIANCE,
                    "Haranyan Court",
                ),
            ] {
                c.insert(
                    region,
                    Arc::new(ChatChannel::new(
                        chat_type::JUDGE,
                        0,
                        fid,
                        next(),
                        name.into(),
                    )),
                );
            }
        }
        m
    }

    /// `((byte)race - 1) & 0xFC` (birth continent).
    fn nation_key(race: u8) -> i64 {
        (i64::from(race) - 1) & 0xFC
    }

    /// `ListAllChannels` (null, faction, nation, court room, zone, party, raid, guild, family).
    pub fn list_all_channels(&self) -> Vec<ChannelRef> {
        let mut res = vec![self.null_channel.clone()];
        res.extend(values(&self.faction_channels));
        res.extend(values(&self.nation_channels));
        res.extend(values(&self.court_room_channels));
        res.extend(values(&self.zone_channels));
        res.extend(values(&self.party_channels));
        res.extend(values(&self.raid_channels));
        res.extend(values(&self.guild_channels));
        res.extend(values(&self.family_channels));
        res.extend(
            self.user_channels
                .lock()
                .expect("chat poisoned")
                .values()
                .map(|c| c.channel.clone()),
        );
        res
    }

    /// `GetFactionChat(motherId)`.
    pub fn faction_chat(&self, mother_id: u32) -> ChannelRef {
        self.faction_channels
            .lock()
            .expect("chat poisoned")
            .get(&mother_id)
            .cloned()
            .unwrap_or_else(|| self.null_channel.clone())
    }

    /// `GetNationChat(race)`.
    pub fn nation_chat(&self, race: u8) -> ChannelRef {
        self.nation_channels
            .lock()
            .expect("chat poisoned")
            .get(&Self::nation_key(race))
            .cloned()
            .unwrap_or_else(|| self.null_channel.clone())
    }

    /// `GetZoneChat(zoneKey)` (channels are keyed by zone group; created on demand).
    pub fn zone_chat(
        &self,
        zone_group_id: u32,
        group_name: Option<&str>,
        chat_ids: &IdManager,
    ) -> ChannelRef {
        self.zone_channels
            .lock()
            .expect("chat poisoned")
            .entry(zone_group_id)
            .or_insert_with(|| {
                let name = group_name
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("ZoneGroup({zone_group_id})"));
                Arc::new(ChatChannel::new(
                    chat_type::SHOUT,
                    zone_group_id as i16,
                    0,
                    chat_ids.get_next_id().unwrap_or(0),
                    name,
                ))
            })
            .clone()
    }

    /// `GetGuildChat(expedition)`.
    pub fn guild_chat(&self, guild_id: u32, name: &str, chat_ids: &IdManager) -> ChannelRef {
        self.guild_channels
            .lock()
            .expect("chat poisoned")
            .entry(guild_id)
            .or_insert_with(|| {
                Arc::new(ChatChannel::new(
                    chat_type::CLAN,
                    guild_id as i16,
                    0,
                    chat_ids.get_next_id().unwrap_or(0),
                    name.to_string(),
                ))
            })
            .clone()
    }

    /// Renames the channel of a renamed guild (an existing channel keeps the name it was
    /// created with otherwise).
    pub fn rename_guild_chat(&self, guild_id: u32, name: &str) {
        if let Some(c) = self
            .guild_channels
            .lock()
            .expect("chat poisoned")
            .get(&guild_id)
        {
            c.set_internal_name(name.to_string());
        }
    }

    /// `GetFamilyChat(familyId)`.
    pub fn family_chat(&self, family_id: u32, chat_ids: &IdManager) -> ChannelRef {
        self.family_channels
            .lock()
            .expect("chat poisoned")
            .entry(family_id)
            .or_insert_with(|| {
                Arc::new(ChatChannel::new(
                    chat_type::FAMILY,
                    family_id as i16,
                    0,
                    chat_ids.get_next_id().unwrap_or(0),
                    format!("Family {family_id}"),
                ))
            })
            .clone()
    }

    /// `GetPartyChat(party, myChar)`: `party_number` is the member's slot / 5 (0 when not a
    /// member), `owner_name` the online owner's name (`" ???"` otherwise).
    pub fn party_chat(
        &self,
        team_id: u32,
        party_number: u32,
        owner_name: Option<&str>,
        chat_ids: &IdManager,
    ) -> ChannelRef {
        let party_id = (team_id << 6).wrapping_add(party_number);
        let ch = self
            .party_channels
            .lock()
            .expect("chat poisoned")
            .entry(party_id)
            .or_insert_with(|| {
                Arc::new(ChatChannel::new(
                    chat_type::PARTY,
                    party_id as i16,
                    0,
                    chat_ids.get_next_id().unwrap_or(0),
                    format!("Party({party_id})"),
                ))
            })
            .clone();
        ch.set_internal_name(format!(
            "Party {} of {}",
            party_number + 1,
            owner_name.unwrap_or(" ???")
        ));
        ch
    }

    /// `GetRaidChat(party)`.
    pub fn raid_chat(
        &self,
        team_id: u32,
        owner_name: Option<&str>,
        chat_ids: &IdManager,
    ) -> ChannelRef {
        let ch = self
            .raid_channels
            .lock()
            .expect("chat poisoned")
            .entry(team_id)
            .or_insert_with(|| {
                Arc::new(ChatChannel::new(
                    chat_type::RAID,
                    team_id as i16,
                    0,
                    chat_ids.get_next_id().unwrap_or(0),
                    format!("Raid({team_id})"),
                ))
            })
            .clone();
        ch.set_internal_name(format!("Raid of {}", owner_name.unwrap_or(" ???")));
        ch
    }

    /// `GetTrialChat(courtRegion)` (`None` for regions without a court channel).
    pub fn court_chat(&self, region: u8) -> Option<ChannelRef> {
        self.court_room_channels
            .lock()
            .expect("chat poisoned")
            .get(&region)
            .cloned()
    }

    /// Emulator policy: exact names/passwords, temporary server-global channels. The 1.2
    /// identity is USER / subtype zero / allocated chat id; it is opaque to the client.
    pub fn join_user_channel(
        &self,
        p: &PlayerRef,
        name: &str,
        password: &str,
        create: bool,
        chat_ids: &IdManager,
    ) -> Result<(), ChatFailure> {
        use super::util::error;
        if name.is_empty()
            || name.len() > USER_CHANNEL_NAME_BYTES
            || name.chars().any(char::is_control)
            || password.len() > USER_CHANNEL_PASSWORD_BYTES
            || password.chars().any(char::is_control)
        {
            return Err(ChatFailure::new(error::CHAT_NO_CHANNEL, name));
        }
        let mut channels = self.user_channels.lock().expect("chat poisoned");
        let joined_count = || channels.values().filter(|c| c.channel.contains(p)).count();
        let existing = channels
            .values()
            .find(|c| c.channel.internal_name() == name);
        if let Some(existing) = existing {
            if create {
                return Err(ChatFailure::new(error::CHAT_CHANNEL_ALREADY_EXISTS, name));
            }
            if existing.channel.contains(p) {
                return Err(ChatFailure::new(error::CHAT_ALREADY_JOINED_CHANNEL, name));
            }
            if !existing.password.is_empty() && password.is_empty() {
                return Err(ChatFailure::new(error::CHAT_PRIVATE_CHANNEL, name));
            }
            if existing.password != password {
                return Err(ChatFailure::new(error::CHAT_WRONG_PASSWORD, name));
            }
            if joined_count() >= USER_CHANNELS_PER_SESSION {
                return Err(ChatFailure::new(error::CHAT_NO_CHANNEL, name));
            }
            existing.channel.join(p);
            return Ok(());
        }
        if !create
            || joined_count() >= USER_CHANNELS_PER_SESSION
            || channels.len() >= USER_CHANNELS_TOTAL
        {
            return Err(ChatFailure::new(error::CHAT_NO_CHANNEL, name));
        }
        let id = chat_ids
            .get_next_id()
            .map_err(|_| ChatFailure::new(error::CHAT_NO_CHANNEL, name))?;
        let channel = Arc::new(ChatChannel::new(
            chat_type::USER,
            0,
            id,
            id,
            name.to_string(),
        ));
        channel.join(p);
        channels.insert(
            id,
            UserChannel {
                channel,
                password: password.to_string(),
            },
        );
        Ok(())
    }

    /// Only a member of the exact opaque USER identity can leave it. Client requests never
    /// alter party/family/faction channels, which their owning systems control.
    pub fn leave_user_channel(
        &self,
        p: &PlayerRef,
        identity: i64,
        chat_ids: &IdManager,
    ) -> Result<(), ChatFailure> {
        use super::util::error;
        let mut channels = self.user_channels.lock().expect("chat poisoned");
        let id = (identity as u64 >> 32) as u32;
        let entry = channels
            .get(&id)
            .filter(|c| user_channel_identity(&c.channel) == identity)
            .ok_or_else(|| ChatFailure::new(error::CHAT_NO_CHANNEL, ""))?;
        if !entry.channel.leave(p) {
            return Err(ChatFailure::new(
                error::CHAT_NOT_JOINED_CHANNEL,
                entry.channel.internal_name(),
            ));
        }
        if entry.channel.is_empty() {
            channels.remove(&id);
            chat_ids.release_id(id);
        }
        Ok(())
    }

    /// Membership uses the current PlayerRef, not a reusable character/object id.
    pub fn send_user_message(
        &self,
        p: &PlayerRef,
        sub_type: i16,
        channel_id: u32,
        message: &SCChatMessagePacket,
    ) -> Result<usize, ChatFailure> {
        use super::util::error;
        let channels = self.user_channels.lock().expect("chat poisoned");
        let entry = channels
            .get(&channel_id)
            .filter(|c| c.channel.sub_type == sub_type)
            .ok_or_else(|| ChatFailure::new(error::CHAT_NO_CHANNEL, ""))?;
        if !entry.channel.contains(p) {
            return Err(ChatFailure::new(
                error::CHAT_NOT_JOINED_CHANNEL,
                entry.channel.internal_name(),
            ));
        }
        if message.message.len() > i16::MAX as usize {
            return Err(ChatFailure::new(
                error::CHAT_NO_CHANNEL,
                entry.channel.internal_name(),
            ));
        }
        let packet = super::packets::chat::user_message(&entry.channel, message);
        // The six-byte plain packet header is part of the transport's u16 frame length.
        if packet.body.len() + 6 > u16::MAX as usize {
            return Err(ChatFailure::new(
                error::CHAT_NO_CHANNEL,
                entry.channel.internal_name(),
            ));
        }
        Ok(entry.channel.send_packet(&packet))
    }

    pub fn leave_user_channels(&self, p: &PlayerRef, chat_ids: &IdManager) {
        let mut channels = self.user_channels.lock().expect("chat poisoned");
        let empty: Vec<u32> = channels
            .iter()
            .filter_map(|(id, c)| {
                c.channel.leave(p);
                c.channel.is_empty().then_some(*id)
            })
            .collect();
        for id in empty {
            channels.remove(&id);
            chat_ids.release_id(id);
        }
    }

    /// `LeaveAllChannels` (faction, court rooms, nation, zone, party, raid, guild, family; each
    /// in key order — the C# iterates `ConcurrentDictionary`s, whose order is unspecified).
    pub fn leave_all_channels(&self, p: &PlayerRef) {
        let lists: [Vec<ChannelRef>; 8] = [
            values(&self.faction_channels),
            values(&self.court_room_channels),
            values(&self.nation_channels),
            values(&self.zone_channels),
            values(&self.party_channels),
            values(&self.raid_channels),
            values(&self.guild_channels),
            values(&self.family_channels),
        ];
        for list in lists {
            for c in list {
                c.leave(p);
            }
        }
        for channel in self.user_channels.lock().expect("chat poisoned").values() {
            channel.channel.leave(p);
        }
    }

    /// `CleanUpChannels`: removes the empty zone / party / raid / guild / family channels and
    /// releases their ids. Returns the number of removed channels.
    pub fn clean_up_channels(&self, chat_ids: &IdManager) -> usize {
        let mut users = self.user_channels.lock().expect("chat poisoned");
        let empty: Vec<u32> = users
            .iter()
            .filter_map(|(id, c)| c.channel.is_empty().then_some(*id))
            .collect();
        for id in &empty {
            users.remove(id);
            chat_ids.release_id(*id);
        }
        clean_up(&self.zone_channels, chat_ids)
            + clean_up(&self.party_channels, chat_ids)
            + clean_up(&self.raid_channels, chat_ids)
            + clean_up(&self.guild_channels, chat_ids)
            + clean_up(&self.family_channels, chat_ids)
            + empty.len()
    }
}

pub fn user_channel_identity(channel: &ChatChannel) -> i64 {
    ((u64::from(channel.faction) << 32)
        | (u64::from(channel.sub_type as u16) << 16)
        | u64::from(channel.chat_type as u16)) as i64
}

fn values<K>(m: &Channels<K>) -> Vec<ChannelRef> {
    m.lock().expect("chat poisoned").values().cloned().collect()
}

fn clean_up<K: Ord + Copy>(m: &Channels<K>, chat_ids: &IdManager) -> usize {
    let mut map = m.lock().expect("chat poisoned");
    let empty: Vec<K> = map
        .iter()
        .filter(|(_, c)| c.is_empty())
        .map(|(k, _)| *k)
        .collect();
    for k in &empty {
        if let Some(c) = map.remove(k) {
            chat_ids.release_id(c.internal_id);
        }
    }
    empty.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_allocates_fixed_channels_in_csharp_order() {
        let all_ids = crate::managers::id::IdManagers::default();
        let ids = &all_ids.chat;
        let m = ChatManager::new(ids);
        let all = m.list_all_channels();
        // null + 3 faction + 2 nation + 2 court
        assert_eq!(all.len(), 8);
        let got: Vec<(i16, u32, u32, String)> = all
            .iter()
            .map(|c| (c.chat_type, c.faction, c.internal_id, c.internal_name()))
            .collect();
        assert_eq!(got[0], (chat_type::WHITE, 0, 0, "Null".into()));
        assert_eq!(got[1], (chat_type::ALLY, 148, 1, "Nuia".into()));
        assert_eq!(
            got[4],
            (chat_type::REGION, 148, 4, "Nuian-Elf-Dwarf".into())
        );
        assert_eq!(got[6], (chat_type::JUDGE, 148, 6, "Nuian Court".into()));
        assert_eq!(got[7], (chat_type::JUDGE, 149, 7, "Haranyan Court".into()));
        assert!(m.court_chat(court_room_region::INVALID).is_none());
    }

    #[test]
    fn party_and_raid_channels_and_cleanup() {
        let all_ids = crate::managers::id::IdManagers::default();
        let ids = &all_ids.chat;
        let m = ChatManager::new(ids);
        let p = m.party_chat(3, 1, Some("Bob"), ids);
        assert_eq!((p.chat_type, p.sub_type), (chat_type::PARTY, (3 << 6) + 1));
        assert_eq!(p.internal_name(), "Party 2 of Bob");
        let r = m.raid_chat(3, None, ids);
        assert_eq!((r.chat_type, r.sub_type), (chat_type::RAID, 3));
        assert_eq!(r.internal_name(), "Raid of  ???");
        let id = r.internal_id;
        assert_eq!(m.clean_up_channels(ids), 2);
        // The released id is handed out again.
        let again = m.raid_chat(9, Some("x"), ids);
        assert!(again.internal_id == id || again.internal_id == p.internal_id);
    }

    #[tokio::test]
    async fn user_channel_global_budget_allows_existing_join_and_recovers_capacity() {
        use crate::net::packet::encode_raw;
        use crate::opcodes::{cs, sc};
        use crate::packets::world::SCErrorMsgPacket;
        use crate::systems::social::packets::body;
        use crate::systems::social::util::error;
        use crate::test_support::{spawn_player, test_character, test_server, TestClient};
        use aaemu_commons::PacketStream;
        let server = test_server();
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Budget", 100.0, 100.0),
            0x101,
        );
        client.conn.active_char = Some(player.clone());
        client.drain();
        // Lightweight fixture for a full map of channels awaiting the cleanup tick after
        // their members left. No thousands of simulated clients or live connections needed.
        let mut first = 0;
        {
            let mut channels = server.chat.user_channels.lock().unwrap();
            for index in 0..USER_CHANNELS_TOTAL {
                let id = server.ids.chat.get_next_id().unwrap();
                if index == 0 {
                    first = id;
                }
                channels.insert(
                    id,
                    UserChannel {
                        channel: Arc::new(ChatChannel::new(
                            chat_type::USER,
                            0,
                            id,
                            id,
                            format!("Channel{index}"),
                        )),
                        password: String::new(),
                    },
                );
            }
        }
        let mut join = PacketStream::new();
        join.write_str("Channel0").write_str("").write_bool(false);
        server
            .handle_frame(
                &mut client.conn,
                &encode_raw(1, cs::CSJoinUserChatChannelPacket, join.as_bytes()),
            )
            .await
            .unwrap();
        assert_eq!(client.drain_ops(), vec![sc::SCJoinedChatChannelPacket]);
        let mut create = PacketStream::new();
        create.write_str("New").write_str("").write_bool(true);
        server
            .handle_frame(
                &mut client.conn,
                &encode_raw(1, cs::CSJoinUserChatChannelPacket, create.as_bytes()),
            )
            .await
            .unwrap();
        assert_eq!(
            client.drain(),
            vec![(
                sc::SCErrorMsgPacket,
                body(&SCErrorMsgPacket::new(error::CHAT_NO_CHANNEL, 0, true))
            )]
        );
        assert_eq!(server.chat.user_channels.lock().unwrap().len(), 1024);
        let channel = server
            .chat
            .user_channels
            .lock()
            .unwrap()
            .get(&first)
            .unwrap()
            .channel
            .clone();
        let mut leave = PacketStream::new();
        leave.write_i64(user_channel_identity(&channel));
        server
            .handle_frame(
                &mut client.conn,
                &encode_raw(1, cs::CSLeaveChatChannelPacket, leave.as_bytes()),
            )
            .await
            .unwrap();
        assert_eq!(client.drain_ops(), vec![sc::SCLeavedChatChannelPacket]);
        server
            .handle_frame(
                &mut client.conn,
                &encode_raw(1, cs::CSJoinUserChatChannelPacket, create.as_bytes()),
            )
            .await
            .unwrap();
        assert_eq!(client.drain_ops(), vec![sc::SCJoinedChatChannelPacket]);
        let replacement = server
            .chat
            .user_channels
            .lock()
            .unwrap()
            .get(&first)
            .unwrap()
            .channel
            .clone();
        assert_eq!(replacement.internal_name(), "New");
        assert!(replacement.contains(&player));
        assert_eq!(server.chat.clean_up_channels(&server.ids.chat), 1023);
        assert_eq!(server.chat.user_channels.lock().unwrap().len(), 1);
        server.chat.leave_user_channels(&player, &server.ids.chat);
        assert!(server.chat.user_channels.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
mod spam_tests {
    use super::*;

    fn config() -> ChatConfig {
        ChatConfig {
            chat_type_groups: vec![0, 0, 1, 1, 2, 2, 1, 2, 2, 2, 2, 2, 2, 2, 2],
            chat_group_delays_ms: vec![1000, 3000, 0],
            ..Default::default()
        }
    }

    #[test]
    fn chat_spam_limits_follow_the_group_delay() {
        let cfg = config();
        let t0 = Instant::now();
        let mut last = [None; 15];
        // Say (group 0): allowed, then refused inside the delay and allowed at its boundary.
        assert!(chat_spam_allows(&cfg, &mut last, chat_type::WHITE, t0));
        assert!(!chat_spam_allows(
            &cfg,
            &mut last,
            chat_type::WHITE,
            t0 + Duration::from_millis(500)
        ));
        assert!(chat_spam_allows(
            &cfg,
            &mut last,
            chat_type::WHITE,
            t0 + Duration::from_millis(1000)
        ));
    }

    #[test]
    fn chat_spam_grace_absorbs_network_jitter_but_not_a_flood() {
        let cfg = config();
        let t0 = Instant::now();
        let mut last = [None; 15];
        assert!(chat_spam_allows(&cfg, &mut last, chat_type::WHITE, t0));
        // 80 ms before the 1 s delay: inside `min(250 ms, delay / 4)`.
        assert!(chat_spam_allows(
            &cfg,
            &mut last,
            chat_type::WHITE,
            t0 + Duration::from_millis(920)
        ));
        // Clearly early: half the delay.
        let t1 = Instant::now() + Duration::from_secs(10);
        let mut last = [None; 15];
        assert!(chat_spam_allows(&cfg, &mut last, chat_type::WHITE, t1));
        assert!(!chat_spam_allows(
            &cfg,
            &mut last,
            chat_type::WHITE,
            t1 + Duration::from_millis(500)
        ));
    }

    #[test]
    fn chat_spam_keeps_groups_and_unlimited_types() {
        let cfg = config();
        let t0 = Instant::now();
        let mut last = [None; 15];
        assert!(chat_spam_allows(&cfg, &mut last, chat_type::WHITE, t0));
        // Shout shares group 0 with say; trade (group 1) has its own window.
        assert!(!chat_spam_allows(
            &cfg,
            &mut last,
            chat_type::SHOUT,
            t0 + Duration::from_millis(500)
        ));
        assert!(chat_spam_allows(
            &cfg,
            &mut last,
            chat_type::TRADE,
            t0 + Duration::from_millis(500)
        ));
        // Party (group 2) has no delay at all.
        assert!(chat_spam_allows(
            &cfg,
            &mut last,
            chat_type::PARTY,
            t0 + Duration::from_millis(500)
        ));
        assert!(chat_spam_allows(
            &cfg,
            &mut last,
            chat_type::PARTY,
            t0 + Duration::from_millis(900)
        ));
        // Whisper and user channels are outside the client's limited range.
        let slow = ChatConfig {
            chat_group_delays_ms: vec![60_000; 15],
            ..Default::default()
        };
        assert!(chat_spam_allows(&slow, &mut last, chat_type::WHISPER, t0));
        assert!(chat_spam_allows(&slow, &mut last, chat_type::WHISPER, t0));
        assert!(chat_spam_allows(&slow, &mut last, chat_type::USER, t0));
        assert!(chat_spam_allows(&slow, &mut last, chat_type::USER, t0));
    }
}
