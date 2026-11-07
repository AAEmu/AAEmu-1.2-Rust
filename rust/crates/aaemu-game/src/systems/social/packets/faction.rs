//! Faction packets (`SCFactionListPacket` / `SCFactionRelationListPacket` live in
//! `packets::world`, used by the select path).

use chrono::{DateTime, Utc};

use super::{active_char, cs_packet, sc_packet};
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::PacketRegistry;
use crate::opcodes::cs;
use crate::systems::social::nation;

sc_packet!(
    /// `SCUnitFactionChangedPacket(unitId, unitName, id, id2, temp)`.
    SCUnitFactionChangedPacket { obj_id: u32, unit_name: String, id: u32, id2: u32, temp: bool } |self, s| {
        s.write_bc(self.obj_id)
            .write_str(&self.unit_name)
            .write_u32(self.id)
            .write_u32(self.id2)
            .write_bool(self.temp);
    }
);

// ---- Faction (nation / guild) results. The 1.2 client registers a handler for each; the
// C# server had no nation manager and never sent them. ----

sc_packet!(
    /// `SCFactionDeclareHostileResultPacket(result, id, id2)` (not sent by the 1.2 C#).
    SCFactionDeclareHostileResultPacket { result: bool, id: u32, id2: u32 } |self, s| {
        s.write_bool(self.result).write_u32(self.id).write_u32(self.id2);
    }
);

sc_packet!(
    /// `SCFactionImmigrateInvitePacket(id, id2)` (`FUN_392fe4f0`; `id` = nation faction, `id2` =
    /// the invite nonce, NATION-11).
    SCFactionImmigrateInvitePacket { id: u32, id2: u32 } |self, s| {
        s.write_u32(self.id).write_u32(self.id2);
    }
);

sc_packet!(
    /// `SCFactionImmigrateInviteResultPacket(id, charName, answer)` (`FUN_392fe5b0`).
    SCFactionImmigrateInviteResultPacket { id: u32, char_name: String, answer: bool } |self, s| {
        s.write_u32(self.id).write_str(&self.char_name).write_bool(self.answer);
    }
);

sc_packet!(
    /// `SCFactionImmigrateToOriginResultPacket(charName, id)` (`FUN_392fe650`).
    SCFactionImmigrateToOriginResultPacket { char_name: String, id: u32 } |self, s| {
        s.write_str(&self.char_name).write_u32(self.id);
    }
);

sc_packet!(
    /// `SCFactionIndependencePacket(id, id2, name, cTime)` (not sent by the 1.2 C#; the time is
    /// written as `Helpers.UnixTime`).
    SCFactionIndependencePacket { id: u32, id2: u32, name: String, c_time: DateTime<Utc> } |self, s| {
        s.write_u32(self.id)
            .write_u32(self.id2)
            .write_str(&self.name)
            .write_date_time(self.c_time);
    }
);

sc_packet!(
    /// `SCFactionKickToOriginResultPacket(tgtName, id, id2, errorMessage)` (`FUN_392fe6c0`).
    /// The client's error slot is 4 bytes wide in the decompiled handler, but its read width is
    /// not visible there; this keeps the C#-derived `i16` (NATION-12).
    SCFactionKickToOriginResultPacket { tgt_name: String, id: u32, id2: u32, error_message: i16 } |self, s| {
        s.write_str(&self.tgt_name)
            .write_u32(self.id)
            .write_u32(self.id2)
            .write_i16(self.error_message);
    }
);

sc_packet!(
    /// `SCFactionOwnerChangedPacket(id, id2, newOwnerName)` (not sent by the 1.2 C#).
    SCFactionOwnerChangedPacket { id: u32, id2: u32, new_owner_name: String } |self, s| {
        s.write_u32(self.id).write_u32(self.id2).write_str(&self.new_owner_name);
    }
);

sc_packet!(
    /// `SCFactionRelationExpiredPacket(id, id2, prevState, currState)` (not sent by the 1.2 C#).
    SCFactionRelationExpiredPacket { id: u32, id2: u32, prev_state: u8, curr_state: u8 } |self, s| {
        s.write_u32(self.id)
            .write_u32(self.id2)
            .write_u8(self.prev_state)
            .write_u8(self.curr_state);
    }
);

sc_packet!(
    /// `SCFactionRenamedPacket(id, name, byGm)`. Never sent by the 1.2 C#; the 1.2 client's
    /// `OnFactionRenamed` updates the cached part and shows the "expedition name ... changed"
    /// notice (`FUN_392fdce0`). `byGm` false also clears `allow_change_name` client-side.
    SCFactionRenamedPacket { id: u32, name: String, by_gm: bool } |self, s| {
        s.write_u32(self.id).write_str(&self.name).write_bool(self.by_gm);
    }
);

sc_packet!(
    /// `SCFactionRetryRenamePacket(xpdt, errorMessage, name)`. Never sent by the 1.2 C#; the
    /// 1.2 client's `OnFactionRetryRename` re-opens the rename dialog with `name` pre-filled
    /// (`FUN_391e5520`), `xpdt` non-zero selecting the expedition dialog.
    SCFactionRetryRenamePacket { xpdt: bool, error_message: i16, name: String } |self, s| {
        s.write_bool(self.xpdt).write_i16(self.error_message).write_str(&self.name);
    }
);

sc_packet!(
    /// `SCNationShowRenameUIPacket()` (0x010, empty body). `FUN_391d2380` reads nothing and
    /// calls `FUN_392f8e80(0)`, which opens the nation rename dialog only when the player owns
    /// a user faction. Never sent by the 1.2 C#; the production trigger is capture-pending
    /// (RENAME-7).
    SCNationShowRenameUIPacket {} |self, s| {}
);

sc_packet!(
    /// `SCFactionSetRelationStatePacket(id, id2, state, expireTime, nextState)` (not sent by the
    /// 1.2 C#).
    SCFactionSetRelationStatePacket {
        id: u32,
        id2: u32,
        state: u8,
        expire_time: DateTime<Utc>,
        next_state: u8,
    } |self, s| {
        s.write_u32(self.id)
            .write_u32(self.id2)
            .write_u8(self.state)
            .write_date_time(self.expire_time)
            .write_u8(self.next_state);
    }
);

cs_packet!(
    /// `CSFactionImmigrationInvitePacket`: the nation king invites a candidate
    /// (`X2Nation.InviteToFaction` -> `FUN_392fdff0`, `{name: string}`).
    CSFactionImmigrationInvitePacket { invitee: String }
    read |s| { Ok(Self { invitee: s.read_string()? }) }
    handle |self, server, conn| {
        tracing::debug!("FactionImmigrationInvite, {}", self.invitee);
        let p = active_char!(conn);
        nation::invite_citizen(server, &p, &self.invitee);
        Ok(())
    }
);

cs_packet!(
    /// `CSFactionImmigrationInviteReplyPacket`: the invitee answers (`FUN_397ab510`,
    /// `{id: u32 nation, id2: u32 nonce, answer: u8}`; NATION-11).
    CSFactionImmigrationInviteReplyPacket { id: u32, id2: u32, answer: bool }
    read |s| { Ok(Self { id: s.read_u32()?, id2: s.read_u32()?, answer: s.read_bool()? }) }
    handle |self, server, conn| {
        tracing::debug!("FactionImmigrationInviteReply, Id: {}, Id2: {}, Answer: {}", self.id, self.id2, self.answer);
        let p = active_char!(conn);
        nation::reply_invite(server, &p, self.id, self.id2, self.answer).await;
        Ok(())
    }
);

cs_packet!(
    /// `CSFactionImmigrateToOriginPacket` (empty body): a citizen withdraws to the race's
    /// start faction (`FUN_392f92f0`).
    CSFactionImmigrateToOriginPacket {}
    read |s| { Ok(Self {}) }
    handle |self, server, conn| {
        tracing::debug!("FactionImmigrateToOrigin");
        let p = active_char!(conn);
        nation::immigrate_to_origin(server, &p).await;
        Ok(())
    }
);

cs_packet!(
    /// `CSFactionKickToOriginPacket`: the king kicks a citizen (`FUN_392f9380`,
    /// `{name: string}`).
    CSFactionKickToOriginPacket { name: String }
    read |s| { Ok(Self { name: s.read_string()? }) }
    handle |self, server, conn| {
        tracing::debug!("FactionKickToOrigin, Name: {}", self.name);
        let p = active_char!(conn);
        nation::kick_to_origin(server, &p, &self.name).await;
        Ok(())
    }
);

cs_packet!(
    /// `CSFactionDeclareHostilePacket` (0x019, `FUN_390537b0` / `FUN_392f9430`): the nation
    /// king declares a diplomacy target hostile.
    CSFactionDeclareHostilePacket { id: u32 }
    read |s| { Ok(Self { id: s.read_u32()? }) }
    handle |self, server, conn| {
        tracing::debug!("FactionDeclareHostile, Id: {}", self.id);
        let p = active_char!(conn);
        if let Err(code) = nation::declare_hostile(server, &p, self.id) {
            crate::systems::social::util::send_error(&p, code);
        }
        Ok(())
    }
);

pub fn register(reg: &mut PacketRegistry) {
    reg.register::<CSFactionImmigrationInvitePacket>(
        LEVEL_PLAIN,
        cs::CSFactionImmigrationInvitePacket,
    );
    reg.register::<CSFactionImmigrationInviteReplyPacket>(
        LEVEL_PLAIN,
        cs::CSFactionImmigrationInviteReplyPacket,
    );
    reg.register::<CSFactionImmigrateToOriginPacket>(
        LEVEL_PLAIN,
        cs::CSFactionImmigrateToOriginPacket,
    );
    reg.register::<CSFactionKickToOriginPacket>(LEVEL_PLAIN, cs::CSFactionKickToOriginPacket);
    reg.register::<CSFactionDeclareHostilePacket>(LEVEL_PLAIN, cs::CSFactionDeclareHostilePacket);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::systems::social::packets::body;

    #[test]
    fn unit_faction_changed_is_byte_exact() {
        assert_eq!(
            body(&SCUnitFactionChangedPacket {
                obj_id: 0x10,
                unit_name: "A".into(),
                id: 101,
                id2: 161,
                temp: false
            }),
            vec![0x10, 0, 0, 1, 0, b'A', 101, 0, 0, 0, 161, 0, 0, 0, 0]
        );
    }

    #[test]
    fn immigration_cs_packets_read_the_client_layout() {
        use crate::net::registry::ClientPacket;
        let mut s = aaemu_commons::PacketStream::new();
        s.write_str("Citizen");
        let packet = CSFactionImmigrationInvitePacket::read(
            &mut aaemu_commons::PacketStream::from_bytes(s.into_bytes()),
        )
        .expect("invite");
        assert_eq!(packet.invitee, "Citizen");

        let mut s = aaemu_commons::PacketStream::new();
        s.write_u32(1001).write_u32(7).write_u8(1);
        let packet = CSFactionImmigrationInviteReplyPacket::read(
            &mut aaemu_commons::PacketStream::from_bytes(s.into_bytes()),
        )
        .expect("reply");
        assert_eq!((packet.id, packet.id2, packet.answer), (1001, 7, true));

        // 0x017 has an empty body.
        assert!(CSFactionImmigrateToOriginPacket::read(
            &mut aaemu_commons::PacketStream::from_bytes(Vec::new())
        )
        .is_ok());

        let mut s = aaemu_commons::PacketStream::new();
        s.write_str("Citizen");
        let packet = CSFactionKickToOriginPacket::read(
            &mut aaemu_commons::PacketStream::from_bytes(s.into_bytes()),
        )
        .expect("kick");
        assert_eq!(packet.name, "Citizen");

        // 0x019 `{id u32}` (`FUN_390537b0` / `FUN_397ab550`).
        let mut s = aaemu_commons::PacketStream::new();
        s.write_u32(149);
        let packet = CSFactionDeclareHostilePacket::read(
            &mut aaemu_commons::PacketStream::from_bytes(s.into_bytes()),
        )
        .expect("hostile");
        assert_eq!(packet.id, 149);
    }

    #[test]
    fn faction_result_packets_are_byte_exact() {
        use chrono::TimeZone;
        let t = Utc.timestamp_opt(0x01020304, 0).unwrap();
        let ts = 0x01020304i64.to_le_bytes();
        assert_eq!(
            body(&SCFactionDeclareHostileResultPacket {
                result: true,
                id: 1,
                id2: 2
            }),
            vec![1, 1, 0, 0, 0, 2, 0, 0, 0]
        );
        assert_eq!(
            body(&SCFactionImmigrateInvitePacket { id: 1, id2: 2 }),
            vec![1, 0, 0, 0, 2, 0, 0, 0]
        );
        assert_eq!(
            body(&SCFactionImmigrateInviteResultPacket {
                id: 1,
                char_name: "Al".into(),
                answer: true
            }),
            vec![1, 0, 0, 0, 2, 0, b'A', b'l', 1]
        );
        assert_eq!(
            body(&SCFactionImmigrateToOriginResultPacket {
                char_name: "A".into(),
                id: 3
            }),
            vec![1, 0, b'A', 3, 0, 0, 0]
        );
        let mut e = vec![1, 0, 0, 0, 2, 0, 0, 0, 1, 0, b'N'];
        e.extend_from_slice(&ts);
        assert_eq!(
            body(&SCFactionIndependencePacket {
                id: 1,
                id2: 2,
                name: "N".into(),
                c_time: t
            }),
            e
        );
        assert_eq!(
            body(&SCFactionKickToOriginResultPacket {
                tgt_name: "A".into(),
                id: 1,
                id2: 2,
                error_message: -2
            }),
            vec![1, 0, b'A', 1, 0, 0, 0, 2, 0, 0, 0, 0xFE, 0xFF]
        );
        assert_eq!(
            body(&SCFactionOwnerChangedPacket {
                id: 1,
                id2: 2,
                new_owner_name: "B".into()
            }),
            vec![1, 0, 0, 0, 2, 0, 0, 0, 1, 0, b'B']
        );
        assert_eq!(
            body(&SCFactionRelationExpiredPacket {
                id: 1,
                id2: 2,
                prev_state: 1,
                curr_state: 3
            }),
            vec![1, 0, 0, 0, 2, 0, 0, 0, 1, 3]
        );
        assert_eq!(
            body(&SCFactionRenamedPacket {
                id: 5,
                name: "G".into(),
                by_gm: true
            }),
            vec![5, 0, 0, 0, 1, 0, b'G', 1]
        );
        assert_eq!(
            body(&SCFactionRetryRenamePacket {
                xpdt: true,
                error_message: 297,
                name: "G".into()
            }),
            vec![1, 0x29, 0x01, 1, 0, b'G']
        );
        let mut e = vec![1, 0, 0, 0, 2, 0, 0, 0, 1];
        e.extend_from_slice(&ts);
        e.push(2);
        assert_eq!(
            body(&SCFactionSetRelationStatePacket {
                id: 1,
                id2: 2,
                state: 1,
                expire_time: t,
                next_state: 2
            }),
            e
        );
        // `Helpers.UnixTime` clamps times before the epoch to 0.
        let old = Utc.timestamp_opt(-5, 0).unwrap();
        let b = body(&SCFactionSetRelationStatePacket {
            id: 0,
            id2: 0,
            state: 0,
            expire_time: old,
            next_state: 0,
        });
        assert_eq!(&b[9..17], &[0; 8]);
    }
}
