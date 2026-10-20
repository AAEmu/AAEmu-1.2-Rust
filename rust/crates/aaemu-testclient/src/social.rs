//! The party / guild packets the verification suites use: the S->C events `await` waits for
//! and the C->S bodies the `party*` and `g*` script commands send.
//!
//! The layouts mirror the game crate (`systems/social/packets/{team,expedition}.rs`). Events
//! are recorded by [`crate::game::GameState::observe`], so `await` also finds packets an
//! earlier `drain` already read.

use aaemu_commons::PacketStream;
use aaemu_game::opcodes::{cs, sc};

use crate::game::Packet;

/// Something a party / guild packet told the bot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocialEvent {
    /// `SCAskToJoinTeamPacket`.
    TeamAsk {
        team_id: u32,
        sender_id: u32,
        sender_name: String,
        is_party: bool,
    },
    /// `SCJoinedTeamPacket` (head only).
    TeamJoined {
        team_id: u32,
        owner_id: u32,
        is_party: bool,
    },
    /// `SCExpeditionInvitationPacket` (guild invitation).
    GuildInvite {
        invitor_id: u32,
        invitor_name: String,
        faction_id: u32,
        faction_name: String,
    },
}

/// Decodes one of the events above; `None` for any other (or a malformed) packet.
pub fn decode(p: &Packet) -> Option<SocialEvent> {
    decode_inner(p).ok().flatten()
}

fn decode_inner(p: &Packet) -> anyhow::Result<Option<SocialEvent>> {
    if p.level != crate::game::LEVEL_GAME {
        return Ok(None);
    }
    let mut s = p.stream();
    let ev = match p.opcode {
        sc::SCAskToJoinTeamPacket => SocialEvent::TeamAsk {
            team_id: s.read_u32()?,
            sender_id: s.read_u32()?,
            sender_name: s.read_string()?,
            is_party: s.read_bool()?,
        },
        sc::SCJoinedTeamPacket => SocialEvent::TeamJoined {
            team_id: s.read_u32()?,
            owner_id: s.read_u32()?,
            is_party: s.read_bool()?,
        },
        sc::SCExpeditionInvitationPacket => SocialEvent::GuildInvite {
            invitor_id: s.read_u32()?,
            invitor_name: s.read_string()?,
            faction_id: s.read_u32()?,
            faction_name: s.read_string()?,
        },
        _ => return Ok(None),
    };
    Ok(Some(ev))
}

/// `CSInviteToTeamPacket` (`team_id` 0 = a new party).
pub fn invite_to_team(team_id: u32, is_party: bool, target: &str) -> (u16, Vec<u8>) {
    let mut b = PacketStream::new();
    b.write_u32(team_id).write_bool(is_party).write_str(target);
    (cs::CSInviteToTeamPacket, b.into_bytes())
}

/// `CSReplyToJoinTeamPacket`.
pub fn reply_to_join_team(
    team_id: u32,
    is_party: bool,
    owner_id: u32,
    reject: bool,
    char_name: &str,
) -> (u16, Vec<u8>) {
    let mut b = PacketStream::new();
    b.write_u32(team_id)
        .write_bool(is_party)
        .write_u32(owner_id)
        .write_bool(reject)
        .write_str(char_name)
        .write_bool(false);
    (cs::CSReplyToJoinTeamPacket, b.into_bytes())
}

/// `CSCreateExpeditionPacket` (guild).
pub fn create_expedition(name: &str) -> (u16, Vec<u8>) {
    let mut b = PacketStream::new();
    b.write_str(name).write_u32(0);
    (cs::CSCreateExpeditionPacket, b.into_bytes())
}

/// `CSInviteToExpeditionPacket`.
pub fn invite_to_expedition(name: &str) -> (u16, Vec<u8>) {
    let mut b = PacketStream::new();
    b.write_str(name);
    (cs::CSInviteToExpeditionPacket, b.into_bytes())
}

/// `CSReplyExpeditionInvitationPacket` (`id` = the faction id of the invitation).
pub fn reply_expedition_invitation(id: u32, join: bool) -> (u16, Vec<u8>) {
    let mut b = PacketStream::new();
    b.write_u32(id).write_u32(1).write_bool(join);
    (cs::CSReplyExpeditionInvitationPacket, b.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::LEVEL_GAME;

    fn pkt(opcode: u16, body: Vec<u8>) -> Packet {
        Packet {
            level: LEVEL_GAME,
            opcode,
            body,
        }
    }

    #[test]
    fn encoders_are_byte_exact() {
        let mut e = PacketStream::new();
        e.write_u32(0).write_bool(true).write_str("Bo");
        assert_eq!(
            invite_to_team(0, true, "Bo"),
            (cs::CSInviteToTeamPacket, e.into_bytes())
        );
        assert_eq!(
            reply_to_join_team(3, true, 7, false, "Al"),
            (
                cs::CSReplyToJoinTeamPacket,
                vec![3, 0, 0, 0, 1, 7, 0, 0, 0, 0, 2, 0, b'A', b'l', 0]
            )
        );
        assert_eq!(
            create_expedition("Ab"),
            (
                cs::CSCreateExpeditionPacket,
                vec![2, 0, b'A', b'b', 0, 0, 0, 0]
            )
        );
        assert_eq!(
            invite_to_expedition("Bo"),
            (cs::CSInviteToExpeditionPacket, vec![2, 0, b'B', b'o'])
        );
        assert_eq!(
            reply_expedition_invitation(7, true),
            (
                cs::CSReplyExpeditionInvitationPacket,
                vec![7, 0, 0, 0, 1, 0, 0, 0, 1]
            )
        );
    }

    #[test]
    fn encoders_round_trip_through_the_server_readers() {
        use aaemu_game::net::registry::ClientPacket;
        use aaemu_game::systems::social::packets::team::{
            CSInviteToTeamPacket, CSReplyToJoinTeamPacket,
        };
        let mut s = PacketStream::from_bytes(invite_to_team(0, true, "Bo").1);
        let i = CSInviteToTeamPacket::read(&mut s).unwrap();
        assert_eq!(
            (i.team_id, i.is_party, i.target_name.as_str()),
            (0, true, "Bo")
        );
        let mut s = PacketStream::from_bytes(reply_to_join_team(3, true, 7, true, "Al").1);
        let r = CSReplyToJoinTeamPacket::read(&mut s).unwrap();
        assert_eq!(
            (r.team_id, r.owner_id, r.is_reject, r.char_name.as_str()),
            (3, 7, true, "Al")
        );
    }

    #[test]
    fn guild_invitation_decodes() {
        assert_eq!(
            decode(&pkt(
                sc::SCExpeditionInvitationPacket,
                vec![1, 0, 0, 0, 1, 0, b'a', 2, 0, 0, 0, 1, 0, b'b']
            )),
            Some(SocialEvent::GuildInvite {
                invitor_id: 1,
                invitor_name: "a".into(),
                faction_id: 2,
                faction_name: "b".into()
            })
        );
    }

    #[test]
    fn team_events_decode() {
        let mut b = PacketStream::new();
        b.write_u32(3).write_u32(7).write_str("Al").write_bool(true);
        assert_eq!(
            decode(&pkt(sc::SCAskToJoinTeamPacket, b.into_bytes())),
            Some(SocialEvent::TeamAsk {
                team_id: 3,
                sender_id: 7,
                sender_name: "Al".into(),
                is_party: true
            })
        );
        let mut b = PacketStream::new();
        b.write_u32(3).write_u32(7).write_bool(true);
        assert_eq!(
            decode(&pkt(sc::SCJoinedTeamPacket, b.into_bytes())),
            Some(SocialEvent::TeamJoined {
                team_id: 3,
                owner_id: 7,
                is_party: true
            })
        );
    }
}
