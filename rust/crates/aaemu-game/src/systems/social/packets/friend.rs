//! Friend and blocked-user packets.

use super::{active_char, cs_packet, sc_packet};
use crate::models::char_data::FriendInfo;
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::PacketRegistry;
use crate::opcodes::cs;
use crate::systems::social::friend;

sc_packet!(
    /// `SCAddFriendPacket(friend, success, errorMessage)`.
    SCAddFriendPacket { friend: FriendInfo, success: bool, error_message: i16 } |self, s| {
        s.write(&self.friend)
            .write_bool(self.success)
            .write_i16(self.error_message);
    }
);

impl SCAddFriendPacket {
    /// A failed add. The client prints "$1 does not exist." for error 551 using
    /// `friend.name`; every other id shows the localized error text. The character id is 0
    /// because no list entry is created (`FriendInfo` otherwise zeroed).
    pub fn failure(error_message: i16, name: impl Into<String>) -> Self {
        Self {
            friend: FriendInfo {
                name: name.into(),
                ..FriendInfo::default()
            },
            success: false,
            error_message,
        }
    }
}

sc_packet!(
    /// `SCDeleteFriendPacket(characterId, success, friendName, errorMessage)`.
    SCDeleteFriendPacket {
        character_id: u32,
        success: bool,
        friend_name: String,
        error_message: i16,
    } |self, s| {
        s.write_u32(self.character_id)
            .write_bool(self.success)
            .write_str(&self.friend_name)
            .write_i16(self.error_message);
    }
);

impl SCDeleteFriendPacket {
    /// A failed remove. Error 551 with a non-empty name prints "$1 does not exist."; 556
    /// ("not in the friend list") and the rest show the localized text.
    pub fn failure(error_message: i16, character_id: u32, name: impl Into<String>) -> Self {
        Self {
            character_id,
            success: false,
            friend_name: name.into(),
            error_message,
        }
    }
}

sc_packet!(
    /// `SCAddBlockedUserPacket(characterId, characterName, success, errorMessage)`.
    SCAddBlockedUserPacket {
        character_id: u32,
        character_name: String,
        success: bool,
        error_message: i16,
    } |self, s| {
        s.write_u32(self.character_id)
            .write_str(&self.character_name)
            .write_bool(self.success)
            .write_i16(self.error_message);
    }
);

impl SCAddBlockedUserPacket {
    /// A failed block: 551 unknown name, 352 self, 353 already blocked (INFERRED id).
    pub fn failure(error_message: i16, character_id: u32, name: impl Into<String>) -> Self {
        Self {
            character_id,
            character_name: name.into(),
            success: false,
            error_message,
        }
    }
}

sc_packet!(
    /// `SCDeleteBlockedUserPacket(characterId, success, blockedName, errorMessage)`.
    SCDeleteBlockedUserPacket {
        character_id: u32,
        success: bool,
        blocked_name: String,
        error_message: i16,
    } |self, s| {
        s.write_u32(self.character_id)
            .write_bool(self.success)
            .write_str(&self.blocked_name)
            .write_i16(self.error_message);
    }
);

impl SCDeleteBlockedUserPacket {
    /// A failed unblock: 551 unknown name, 354 not blocked.
    pub fn failure(error_message: i16, character_id: u32, name: impl Into<String>) -> Self {
        Self {
            character_id,
            success: false,
            blocked_name: name.into(),
            error_message,
        }
    }
}

cs_packet!(
    /// `CSAddFriendPacket`.
    CSAddFriendPacket { name: String }
    read |s| { Ok(Self { name: s.read_string()? }) }
    handle |self, server, conn| {
        tracing::debug!("AddFriend, name: {}", self.name);
        let p = active_char!(conn);
        friend::add_friend(server, &p, &self.name).await;
        Ok(())
    }
);

cs_packet!(
    /// `CSDeleteFriendPacket`.
    CSDeleteFriendPacket { name: String }
    read |s| { Ok(Self { name: s.read_string()? }) }
    handle |self, server, conn| {
        tracing::info!("CSDeleteFriendPacket, {}", self.name);
        let p = active_char!(conn);
        friend::remove_friend(server, &p, &self.name).await;
        Ok(())
    }
);

cs_packet!(
    /// `CSAddBlockedUserPacket`.
    CSAddBlockedUserPacket { name: String }
    read |s| { Ok(Self { name: s.read_string()? }) }
    handle |self, server, conn| {
        tracing::warn!("AddBlockedUser, {}", self.name);
        let p = active_char!(conn);
        friend::add_blocked_user(server, &p, &self.name);
        Ok(())
    }
);

cs_packet!(
    /// `CSDeleteBlockedUserPacket`.
    CSDeleteBlockedUserPacket { name: String }
    read |s| { Ok(Self { name: s.read_string()? }) }
    handle |self, server, conn| {
        tracing::warn!("CSDeleteBlockedUserPacket, {}", self.name);
        let p = active_char!(conn);
        friend::remove_blocked_user(server, &p, &self.name);
        Ok(())
    }
);

pub fn register(reg: &mut PacketRegistry) {
    reg.register::<CSAddFriendPacket>(LEVEL_PLAIN, cs::CSAddFriendPacket);
    reg.register::<CSDeleteFriendPacket>(LEVEL_PLAIN, cs::CSDeleteFriendPacket);
    reg.register::<CSAddBlockedUserPacket>(LEVEL_PLAIN, cs::CSAddBlockedUserPacket);
    reg.register::<CSDeleteBlockedUserPacket>(LEVEL_PLAIN, cs::CSDeleteBlockedUserPacket);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::systems::social::packets::body;
    use aaemu_commons::{Marshal, PacketStream};

    #[test]
    fn friend_failure_packets_are_byte_exact() {
        // SCAddFriend: FriendInfo with id 0 and only the typed name, then `false` and the i16.
        assert_eq!(body(&SCAddFriendPacket::failure(551, "Nobody")), {
            let mut s = PacketStream::new();
            FriendInfo {
                name: "Nobody".into(),
                ..FriendInfo::default()
            }
            .write(&mut s);
            s.write_bool(false).write_i16(551);
            s.into_bytes()
        });
        assert_eq!(
            body(&SCDeleteFriendPacket::failure(556, 7, "Al")),
            vec![7, 0, 0, 0, 0, 2, 0, b'A', b'l', 0x2C, 0x02]
        );
        assert_eq!(
            body(&SCAddBlockedUserPacket::failure(353, 7, "Al")),
            vec![7, 0, 0, 0, 2, 0, b'A', b'l', 0, 0x61, 0x01]
        );
        assert_eq!(
            body(&SCDeleteBlockedUserPacket::failure(354, 0, "Nobody")),
            {
                let mut s = PacketStream::new();
                s.write_u32(0)
                    .write_bool(false)
                    .write_str("Nobody")
                    .write_i16(354);
                s.into_bytes()
            }
        );
    }

    #[test]
    fn friend_packets_are_byte_exact() {
        let f = FriendInfo {
            character_id: 3,
            name: "Al".into(),
            race: 1,
            level: 10,
            health: 100,
            abilities: [1, 2, 3],
            x: 10.0,
            y: 20.0,
            z: 30.0,
            zone_id: 100,
            in_party: true,
            is_online: true,
            last_world_leave_time: 5,
        };
        let mut s = PacketStream::new();
        f.write(&mut s);
        s.write_bool(true).write_i16(0);
        assert_eq!(
            body(&SCAddFriendPacket {
                friend: f,
                success: true,
                error_message: 0
            }),
            s.into_bytes()
        );
        assert_eq!(
            body(&SCDeleteFriendPacket {
                character_id: 3,
                success: true,
                friend_name: "Al".into(),
                error_message: 0
            }),
            vec![3, 0, 0, 0, 1, 2, 0, b'A', b'l', 0, 0]
        );
        assert_eq!(
            body(&SCAddBlockedUserPacket {
                character_id: 4,
                character_name: "B".into(),
                success: true,
                error_message: 0
            }),
            vec![4, 0, 0, 0, 1, 0, b'B', 1, 0, 0]
        );
        assert_eq!(
            body(&SCDeleteBlockedUserPacket {
                character_id: 4,
                success: true,
                blocked_name: "B".into(),
                error_message: 7
            }),
            vec![4, 0, 0, 0, 1, 1, 0, b'B', 7, 0]
        );
    }
}
