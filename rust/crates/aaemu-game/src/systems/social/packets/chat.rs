//! Chat channel packets (`CSSendChatMessagePacket` itself is registered by `packets::c2g_world`
//! and routed through [`crate::world::session::send_chat_message`]).

use super::{active_char, cs_packet, sc_packet};
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::PacketRegistry;
use crate::opcodes::{cs, sc};
use crate::packets::raw_body_packet;
use crate::packets::world::SCChatMessagePacket;
use crate::systems::social::{
    chat::{chat_refusal, chat_type, send_chat_refusal, ChatChannel},
    util::send_chat_failure,
};
use aaemu_commons::{Marshal, PacketStream};

sc_packet!(
    /// `SCChatFailedPacket` (S 0x0d1): `i16 ErrorMessage ; string chname (max 10)`.
    ///
    /// Reader `FUN_397c2950`; its handler `FUN_391d8de0` raises the client's `CHAT_FAILED(error,
    /// chname)` Lua event. The chat-event handler in `chat_msg_event.alb` displays the error
    /// text; the channel name is capped at the 10 bytes the reader accepts.
    SCChatFailedPacket {
        error: i16,
        channel_name: String
    } |self, s| {
        s.write_i16(self.error).write_str(&self.channel_name);
    }
);

impl SCChatFailedPacket {
    /// The `chname` length the 1.2 reader accepts (`FUN_397c2950`).
    pub const MAX_CHANNEL_NAME_BYTES: usize = 10;

    pub fn new(error: i16, channel_name: &str) -> Self {
        let mut end = channel_name.len().min(Self::MAX_CHANNEL_NAME_BYTES);
        while !channel_name.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            error,
            channel_name: channel_name[..end].to_string(),
        }
    }
}

sc_packet!(
    /// `SCRestrictInfoPacket` (S 0x1c8), the chat-ban notice: `string chname (max 11) ;
    /// i32 index ; u8 restrictCode ; u32 duration ; i64 startDate ; i64 endDate`.
    ///
    /// Reader `FUN_397c4570` reads `chname` (max 11 bytes), then `FUN_397beb40` reads the
    /// record in that order. Handler `FUN_391e0be0` passes only `duration` to `FUN_393c4170`,
    /// which raises the system line `restrict_remain_time` with `duration / 60 + 1` minutes;
    /// the 1.2 client reads neither `index`, `restrictCode`, `startDate` nor `endDate`. The
    /// server sends `index = 0`, `restrictCode = 0` (chat), `duration` = remaining seconds of
    /// the persisted mute, and `start`/`end` = the notice time / `chat_restricted_until`
    /// (the mute start is not persisted; both fields are ignored by the client, INFERRED).
    SCRestrictInfoPacket {
        chname: String,
        index: i32,
        code: u8,
        duration: u32,
        start: i64,
        end: i64
    } |self, s| {
        s.write_str(&self.chname)
            .write_i32(self.index)
            .write_u8(self.code)
            .write_u32(self.duration)
            .write_i64(self.start)
            .write_i64(self.end);
    }
);

cs_packet!(
    /// `CSJoinUserChatChannelPacket`: temporary user channel create/join.
    CSJoinUserChatChannelPacket { name: String, pwd: String, create: bool }
    read |s| { Ok(Self { name: s.read_string()?, pwd: s.read_string()?, create: s.read_bool()? }) }
    handle |self, server, conn| {
        let p = active_char!(conn);
        if let Err(failure) =
            server
                .chat
                .join_user_channel(&p, &self.name, &self.pwd, self.create, &server.ids.chat)
        {
            send_chat_failure(&p, &server.config.chat, failure);
        }
        Ok(())
    }
);

cs_packet!(
    /// `CSLeaveChatChannelPacket`: authenticated member leave of an opaque USER identity.
    CSLeaveChatChannelPacket { chat: i64 }
    read |s| { Ok(Self { chat: s.read_i64()? }) }
    handle |self, server, conn| {
        let p = active_char!(conn);
        if let Err(failure) = server.chat.leave_user_channel(&p, self.chat, &server.ids.chat) {
            send_chat_failure(&p, &server.config.chat, failure);
        }
        Ok(())
    }
);

raw_body_packet!(SCUserChatJoinedPacket, sc::SCJoinedChatChannelPacket);
raw_body_packet!(SCUserChatMessagePacket, sc::SCChatMessagePacket);

pub(crate) fn user_joined(channel: &ChatChannel) -> SCUserChatJoinedPacket {
    let mut body = PacketStream::new();
    body.write_i16(channel.chat_type)
        .write_i16(channel.sub_type)
        .write_u32(channel.faction)
        .write_str(&channel.internal_name());
    SCUserChatJoinedPacket {
        body: body.as_bytes().to_vec(),
    }
}

/// A chat message attributed to `channel`: the requested `message.chat_type` is preserved and
/// the packed identity's sub type / faction come from the channel.
pub(crate) fn channel_message(
    channel: &ChatChannel,
    message: &SCChatMessagePacket,
) -> SCUserChatMessagePacket {
    let mut body = PacketStream::new();
    message.write(&mut body);
    let mut body = body.as_bytes().to_vec();
    body[2..4].copy_from_slice(&channel.sub_type.to_le_bytes());
    body[4..8].copy_from_slice(&channel.faction.to_le_bytes());
    SCUserChatMessagePacket { body }
}

/// Compatibility alias for the USER-only edge.
pub(crate) fn user_message(
    channel: &ChatChannel,
    message: &SCChatMessagePacket,
) -> SCUserChatMessagePacket {
    channel_message(channel, message)
}

/// The USER-only edge preserves both opaque identity fields that ordinary chat ignores.
/// Slash commands retain the same handling order as `world::session::send_chat_message`.
pub async fn send_user_chat(
    server: &crate::server::GameServer,
    conn: &mut crate::net::connection::GameConnection,
    packet: crate::packets::c2g_world::CSSendChatMessagePacket,
) {
    let Some(p) = conn.active_char.clone() else {
        return;
    };
    if let Some(command) = packet.message.strip_prefix('/') {
        let (handled, _) = server
            .systems
            .services
            .commands
            .handle(server, &p, command.trim())
            .await;
        if handled {
            return;
        }
    }
    // SL4b: the user-channel input is the other outgoing chat path (`chat_type::USER` 15), so
    // the same level gate / mute check applies. The sender identity and the refusal share one
    // character lock.
    let (sender, refusal) = {
        let mut c = p.data();
        let refusal = chat_refusal(
            &server.config.chat,
            &mut c,
            chat_type::USER,
            chrono::Utc::now().timestamp(),
        );
        (crate::world::session::chat_sender(&c, p.obj_id), refusal)
    };
    if let Some(refusal) = refusal {
        send_chat_refusal(&p, refusal);
        return;
    }
    let message = SCChatMessagePacket {
        chat_type: chat_type::USER,
        sender: Some(sender),
        message: packet.message,
        ability: packet.ability,
        language_type: packet.language_type,
    };
    if let Err(failure) =
        server
            .chat
            .send_user_message(&p, packet.unk1, packet.unk2 as u32, &message)
    {
        send_chat_failure(&p, &server.config.chat, failure);
    }
}

pub fn register(reg: &mut PacketRegistry) {
    reg.register::<CSJoinUserChatChannelPacket>(LEVEL_PLAIN, cs::CSJoinUserChatChannelPacket);
    reg.register::<CSLeaveChatChannelPacket>(LEVEL_PLAIN, cs::CSLeaveChatChannelPacket);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managers::id::IdManagers;
    use crate::packets::world::ChatSender;
    use crate::systems::social::chat::{ChannelRef, ChatManager};

    /// The expected body from `PacketStream` primitives: literal identity bytes, sender block
    /// `(obj id, character id, language, race, faction, name)`, message and ability.
    fn expected(
        identity: [u8; 8],
        sender: Option<(u32, u32, u8, u32, &str)>,
        msg: &str,
        ability: i32,
    ) -> Vec<u8> {
        let has_sender = sender.is_some();
        let mut s = PacketStream::new();
        s.write_raw(&identity);
        match sender {
            Some((obj_id, id, race, faction, name)) => {
                s.write_bc(obj_id)
                    .write_u32(id)
                    .write_u8(0)
                    .write_u8(race)
                    .write_u32(faction)
                    .write_str(name);
            }
            None => {
                s.write_bc(0)
                    .write_u32(0)
                    .write_u8(0)
                    .write_u8(0)
                    .write_u32(0)
                    .write_str("");
            }
        }
        s.write_str(msg)
            .write_i32(if has_sender { ability } else { 0 })
            .write_i32(0);
        s.into_bytes()
    }

    fn sender() -> ChatSender {
        ChatSender {
            faction_id: 101,
            obj_id: 0x100,
            id: 5,
            race: 1,
            name: "Al".into(),
            gm: false,
        }
    }

    fn message(chat_type: i16, sender: Option<ChatSender>) -> SCChatMessagePacket {
        SCChatMessagePacket {
            chat_type,
            sender,
            message: "hi".into(),
            ability: 3,
            language_type: 0,
        }
    }

    #[test]
    fn channel_message_uses_channel_sub_type_and_faction_literally() {
        let ids = IdManagers::default();
        let m = ChatManager::new(&ids.chat);
        let zone = m.zone_chat(42, None, &ids.chat);
        let party = m.party_chat(7, 2, None, &ids.chat);
        let raid = m.raid_chat(7, None, &ids.chat);
        let ally = m.faction_chat(148);
        let nation = m.nation_chat(1);
        let family = m.family_chat(31, &ids.chat);
        let guild = m.guild_chat(19, "G", &ids.chat);
        let court = m.court_chat(0).unwrap();
        let cases: Vec<(ChannelRef, i16, [u8; 8])> = vec![
            (zone.clone(), chat_type::SHOUT, [1, 0, 42, 0, 0, 0, 0, 0]),
            (zone.clone(), chat_type::TRADE, [2, 0, 42, 0, 0, 0, 0, 0]),
            (zone, chat_type::GROUP_FIND, [3, 0, 42, 0, 0, 0, 0, 0]),
            (party, chat_type::PARTY, [4, 0, 0xC2, 1, 0, 0, 0, 0]),
            (raid.clone(), chat_type::RAID, [5, 0, 7, 0, 0, 0, 0, 0]),
            (raid, chat_type::RAID_LEADER, [10, 0, 7, 0, 0, 0, 0, 0]),
            (ally, chat_type::ALLY, [14, 0, 0, 0, 148, 0, 0, 0]),
            (nation, chat_type::REGION, [6, 0, 0, 0, 148, 0, 0, 0]),
            (family, chat_type::FAMILY, [9, 0, 31, 0, 0, 0, 0, 0]),
            (guild, chat_type::CLAN, [7, 0, 19, 0, 0, 0, 0, 0]),
            (court, chat_type::JUDGE, [11, 0, 0, 0, 148, 0, 0, 0]),
        ];
        for (channel, requested, identity) in cases {
            let msg = message(requested, Some(sender()));
            let packet = channel_message(&channel, &msg);
            assert_eq!(
                packet.body,
                expected(identity, Some((0x100, 5, 1, 101, "Al")), "hi", 3),
                "chat type {requested}"
            );
        }
    }

    #[test]
    fn direct_chat_uses_zero_identity_and_keeps_sender_metadata() {
        for t in [
            chat_type::WHITE,
            chat_type::WHISPER,
            chat_type::WHISPERED,
            chat_type::SYSTEM,
        ] {
            let msg = message(t, Some(sender()));
            let mut body = PacketStream::new();
            msg.write(&mut body);
            let identity = [t as u8, (t >> 8) as u8, 0, 0, 0, 0, 0, 0];
            assert_eq!(
                body.as_bytes(),
                expected(identity, Some((0x100, 5, 1, 101, "Al")), "hi", 3)
            );
        }
        let msg = message(chat_type::SYSTEM, None);
        let mut body = PacketStream::new();
        msg.write(&mut body);
        assert_eq!(
            body.as_bytes(),
            expected([0xFE, 0xFF, 0, 0, 0, 0, 0, 0], None, "hi", 3)
        );
    }

    #[test]
    fn restrict_info_writes_the_reader_layout() {
        // Reader `FUN_397c4570` (chname) -> `FUN_397beb40` (index, restrictCode, duration,
        // startDate, endDate); independent little-endian literals.
        let packet = SCRestrictInfoPacket {
            chname: "Crook".into(),
            index: 0,
            code: 0,
            duration: 601,
            start: 1_700_000_000,
            end: 1_700_000_601,
        };
        let mut body = PacketStream::new();
        packet.write(&mut body);
        assert_eq!(
            body.as_bytes(),
            vec![
                5, 0, b'C', b'r', b'o', b'o', b'k', // string chname
                0, 0, 0, 0, // i32 index
                0, // u8 restrictCode
                0x59, 0x02, 0, 0, // u32 duration (601 s)
                0, 0xF1, 0x53, 0x65, 0, 0, 0, 0, // i64 startDate (1_700_000_000)
                0x59, 0xF3, 0x53, 0x65, 0, 0, 0, 0, // i64 endDate (1_700_000_601)
            ]
        );
    }

    #[test]
    fn chat_failed_writes_i16_error_then_the_channel_name() {
        // Reader `FUN_397c2950`: `i16 ErrorMessage ; string chname (max 10)`.
        let packet = SCChatFailedPacket::new(467, "Party");
        let mut body = PacketStream::new();
        packet.write(&mut body);
        assert_eq!(
            body.as_bytes(),
            vec![0xD3, 0x01, 5, 0, b'P', b'a', b'r', b't', b'y']
        );
    }

    #[test]
    fn chat_failed_channel_name_is_capped_at_ten_bytes_on_a_char_boundary() {
        // The 1.2 reader's `chname` buffer is 10 bytes; a longer name must not overflow it.
        let packet = SCChatFailedPacket::new(270, "Party 1 of Al");
        assert_eq!(packet.channel_name, "Party 1 of");
        // A multi-byte character that would straddle the cap is dropped whole.
        let packet = SCChatFailedPacket::new(270, "éééééé");
        assert_eq!(packet.channel_name, "ééééé");
    }
}
