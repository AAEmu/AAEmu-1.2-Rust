//! Small helpers shared by the social managers: `ErrorMessageType` values, `SendErrorMessage`,
//! `SendMessage`, positions and the time base.

use crate::packets::world::{SCChatMessagePacket, SCErrorMsgPacket};
use crate::server::GameServer;
use crate::systems::social::config::ChatConfig;
use crate::systems::social::packets::chat::SCChatFailedPacket;
use crate::world::{PlayerRef, WorldObject};

/// `ErrorMessageType` values used by the social subsystem.
pub mod error {
    pub const TEAM_NO_RIGHTS: i16 = 72;
    pub const TEAM_FULL: i16 = 73;
    pub const TEAM_INVITEE_IN_TEAM: i16 = 77;
    pub const TEAM_INVITOR_OFFLINE: i16 = 78;
    pub const TEAM_INVITOR_MOVED: i16 = 79;
    pub const TEAM_INVITE_REFUSED: i16 = 80;
    pub const TEAM_JOIN_ALONE: i16 = 82;
    pub const EXPEDITION_CREATE_LEVEL: i16 = 293;
    pub const EXPEDITION_CREATE_MEMBER: i16 = 294;
    pub const EXPEDITION_CREATE_MONEY: i16 = 295;
    pub const EXPEDITION_NAME_CHARACTER: i16 = 296;
    pub const EXPEDITION_NAME_LENGTH: i16 = 297;
    pub const EXPEDITION_NAME_EXIST: i16 = 299;
    pub const EXPEDITION_BAD_STATUS: i16 = 307;
    pub const CHAT_NOT_IN_EXPEDITION: i16 = 311;
    /// 321: "the sponsor faction was chosen wrongly" (the C# used it for a party member of
    /// another alliance, which is [`EXPEDITION_CREATE_MEMBER_FACTION`] = 320).
    pub const EXPEDITION_CREATE_FACTION: i16 = 321;
    pub const EXPEDITION_CREATE_MEMBER_EXPEDITION: i16 = 330;
    pub const CHAT_NOT_IN_FAMILY: i16 = 412;
    pub const TRIALS_ALREADY_CLOSED: i16 = 478;
    pub const TRIALS_JURY_FULL: i16 = 479;
    pub const CHAT_NOT_RAID_OWNER: i16 = 496;
    pub const BAD_DUEL_TARGET: i16 = 504;
    pub const ALREADY_IN_DUEL: i16 = 505;
    pub const OTHER_ALREADY_IN_DUEL: i16 = 506;
    pub const TARGET_REJECTED_DUEL: i16 = 507;
    pub const BAD_DUEL_SELF: i16 = 513;
    pub const CANNOT_JOIN_TRIAL_FROM_INSTANT_ZONE: i16 = 544;
    pub const TRIALS_CANNOT_JOIN_AFTER_START: i16 = 549;
    pub const EXPEDITION_ALREADY_MEMBER: i16 = 761;
    pub const ONLY_EXPEDITION_OWNER: i16 = 807;
    pub const ONLY_EXPEDITION_MEMBER: i16 = 808;
    pub const CHAT_NOT_IN_PARTY: i16 = 267;
    pub const CHAT_NOT_IN_RAID: i16 = 268;
    pub const CHAT_CHANNEL_ALREADY_EXISTS: i16 = 269;
    pub const CHAT_NO_CHANNEL: i16 = 270;
    pub const CHAT_ALREADY_JOINED_CHANNEL: i16 = 271;
    pub const CHAT_PRIVATE_CHANNEL: i16 = 272;
    pub const CHAT_WRONG_PASSWORD: i16 = 273;
    pub const CHAT_NOT_JOINED_CHANNEL: i16 = 467;

    // The rest come straight from the generated `ErrorMessageType` table (the 1.2 client's
    // `ui_texts` carry the matching texts), so the values cannot drift.
    pub use crate::systems::economy::error_message::{
        CANNOT_DUEL_DURING_FORCE_ATTACK, CANNOT_DUEL_DURING_FORCE_ATTACK_OTHER, DUEL_BAD_LOCATION,
        DUEL_OTHER_BUSY, EXPEDITION_BAD_FACTION, EXPEDITION_CHANGE_OWNER_MY_EXPEDITION,
        EXPEDITION_CHANGE_OWNER_SELF, EXPEDITION_CREATE_MEMBER_FACTION,
        EXPEDITION_DISMISS_IN_SIEGE, EXPEDITION_NAME_BADLIST, EXPEDITION_NO_TARGET,
        EXPEDITION_OTHER_EXPEDITION, EXPEDITION_OTHER_INVITATION,
        EXPEDITION_OWNER_CANNOT_CHANGE_DURING_SIEGE, EXPEDITION_OWNER_CANNOT_LEAVE,
        EXPEDITION_OWNER_DISMISS, FACTION_CHANGE_MEMBER_ROLE_RIGHTS, FACTION_INVITE_BATTLEFIELD,
        FACTION_INVITE_OFFLINE, FACTION_INVITE_REJECTED, FACTION_INVITE_RIGHTS,
        FACTION_INVITE_SAME, FACTION_INVITE_SELF, FACTION_JOIN, FACTION_JOIN_NO_INVITOR,
        FACTION_KICK_RIGHTS, FACTION_RIGHTS, FAMILY_ALREADY, FAMILY_ALREADY_OWNER,
        FAMILY_HAS_FAMILY, FAMILY_INVITE_OFFLINE, FAMILY_KICK_SELF, FAMILY_MAXIMUM,
        FAMILY_NOT_EXIST, FAMILY_NOT_OWNER, FAMILY_NO_INVITOR, FAMILY_NO_TARGET,
        FAMILY_OTHER_FAMILY, FAMILY_OWNER_TITLE, FAMILY_REJECT, FAMILY_SELF, FAMILY_TITLE,
        FAMILY_TITLE_BAD, INVITATION_CANCELED_TIMEOUT, TARGET_TOO_FAR, TEAM_INVITEE_MEMBER,
        TEAM_INVITEE_OFFLINE, TEAM_JOIN, TEAM_YOURSELF,
    };
}

/// `Character.SendErrorMessage(type)` (`SCErrorMsgPacket(type, 0, true)`).
pub fn send_error(p: &PlayerRef, error: i16) {
    p.send(SCErrorMsgPacket::new(error, 0, true));
}

/// A refused channel-bound chat action: the error id and the channel it belongs to (the
/// `chname` of `SCChatFailedPacket`). The packet caps the name at 10 bytes when encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatFailure {
    pub error: i16,
    pub channel: String,
}

impl ChatFailure {
    pub fn new(error: i16, channel: impl Into<String>) -> Self {
        Self {
            error,
            channel: channel.into(),
        }
    }
}

/// Answers a channel-bound chat failure with `SCChatFailedPacket(error, channel name)` when
/// `Chat.ChatFailedPacket` is on, else with the `SCErrorMsgPacket` toast the server sent before
/// the flag existed. Which failures retail answered with `SCChatFailed` is not captured
/// (`CHAT-FAILED-1`, capture-pending), so the flag defaults to off.
pub fn send_chat_failure(p: &PlayerRef, config: &ChatConfig, failure: ChatFailure) {
    if config.chat_failed_packet {
        p.send(SCChatFailedPacket::new(failure.error, &failure.channel));
    } else {
        send_error(p, failure.error);
    }
}

/// `Character.SendMessage(type, message)` without a sender.
pub fn send_message(p: &PlayerRef, chat_type: i16, message: &str) {
    p.send(SCChatMessagePacket {
        chat_type,
        sender: None,
        message: message.to_string(),
        ability: 0,
        language_type: 0,
    });
}

/// `Transform.World.Position` of a player (local position unless it is attached to a parent).
pub fn world_position(server: &GameServer, p: &PlayerRef) -> (f32, f32, f32) {
    let (t, inst) = {
        let c = p.data();
        (c.unit.transform, c.unit.transform.instance_id)
    };
    if t.parent_obj_id == 0 {
        return (t.x, t.y, t.z);
    }
    match server.world.world(inst) {
        Some(w) => {
            let pr = w.world_pos_rot(&WorldObject::from(p));
            (pr.x, pr.y, pr.z)
        }
        None => (t.x, t.y, t.z),
    }
}

/// `MathUtil.CalculateDistance(a, b, includeZ)`.
pub fn distance(a: (f32, f32, f32), b: (f32, f32, f32), include_z: bool) -> f32 {
    let dx = a.0 - b.0;
    let dy = a.1 - b.1;
    if include_z {
        let dz = a.2 - b.2;
        (dx * dx + dy * dy + dz * dz).sqrt()
    } else {
        (dx * dx + dy * dy).sqrt()
    }
}

/// `DateTime.UtcNow` in unix seconds.
pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `DateTime.UtcNow` in unix milliseconds.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// `Faction.MotherId` of a faction id (0 when the faction is unknown).
pub fn mother_id(server: &GameServer, faction_id: u32) -> u32 {
    server
        .game_data
        .factions
        .get(faction_id)
        .map_or(0, |f| f.mother_id)
}

/// The alliance (nation) of a faction: its `MotherId`, or the faction itself when it has none
/// (the outlaw faction 114).
pub fn alliance_id(server: &GameServer, faction_id: u32) -> u32 {
    match mother_id(server, faction_id) {
        0 => faction_id,
        m => m,
    }
}

/// Runs `f` with the player's character data locked (one object lock, released afterwards).
pub fn with_char<R>(
    p: &PlayerRef,
    f: impl FnOnce(&mut crate::models::character::Character) -> R,
) -> R {
    let mut c = p.data();
    f(&mut c)
}
