//! Game network core: framing, registry, per-connection state and the listener
//! (`AAEmu.Game.Core.Network.Game` + `Core.Network.Connections`).

pub mod connection;
pub mod listener;
pub mod outbox;
pub mod packet;
pub mod registry;
pub mod reply_delay;

/// Tracing target of the opt-in client-packet field log
/// (`RUST_LOG=aaemu_game::net::fields=trace`).
pub const FIELDS_TRACE_TARGET: &str = "aaemu_game::net::fields";

/// Full hex dump of a client packet body for [`FIELDS_TRACE_TARGET`]. Unlike
/// [`aaemu_commons::net::hex_dump`] this is not capped, because the trace is opt-in and
/// exists to compare complete packets.
pub(crate) fn body_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(bytes.len().saturating_mul(3));
    for b in bytes {
        let _ = write!(out, "{b:02x} ");
    }
    out
}

/// The `body=` text of the opt-in C->S trace. The second password packets carry the typed
/// password and the virtual-keyboard tables, so their bodies are never logged.
pub(crate) fn trace_body(level: u8, opcode: u16, bytes: &[u8]) -> String {
    if level == crate::net::packet::LEVEL_PLAIN
        && matches!(
            opcode,
            crate::opcodes::cs::CSCreateSecondPasswordPacket
                | crate::opcodes::cs::CSChangeSecondPasswordPacket
                | crate::opcodes::cs::CSClearSecondPasswordPacket
                | crate::opcodes::cs::CSCheckSecondPasswordPacket
        )
    {
        return "<redacted: second password>".to_string();
    }
    body_hex(bytes)
}

#[cfg(test)]
mod tests {
    #[test]
    fn trace_body_redacts_the_second_password_packets() {
        use crate::net::packet::LEVEL_PLAIN;
        use crate::opcodes::cs;

        for opcode in [
            cs::CSCreateSecondPasswordPacket,
            cs::CSChangeSecondPasswordPacket,
            cs::CSClearSecondPasswordPacket,
            cs::CSCheckSecondPasswordPacket,
        ] {
            assert_eq!(
                super::trace_body(LEVEL_PLAIN, opcode, b"secret-password"),
                "<redacted: second password>"
            );
        }
        assert_eq!(
            super::trace_body(LEVEL_PLAIN, cs::CSRankCharacterPacket, &[0xAB]),
            "ab "
        );
        assert_eq!(
            super::trace_body(0, cs::CSCheckSecondPasswordPacket, &[0xAB]),
            "ab ",
            "only the plain level carries the game's second password packets"
        );
    }

    #[test]
    fn body_hex_is_not_capped() {
        let bytes: Vec<u8> = (0..100).collect();
        let output = super::body_hex(&bytes);
        assert_eq!(output.matches("0a ").count(), 1);
        assert_eq!(output.len(), bytes.len() * 3);
        assert!(output.ends_with("63 "));
    }
}
