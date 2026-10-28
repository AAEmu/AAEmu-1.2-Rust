//! Game network core: framing, registry, per-connection state and the listener
//! (`AAEmu.Game.Core.Network.Game` + `Core.Network.Connections`).

pub mod connection;
pub mod listener;
pub mod outbox;
pub mod packet;
pub mod registry;

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

#[cfg(test)]
mod tests {
    #[test]
    fn body_hex_is_not_capped() {
        let bytes: Vec<u8> = (0..100).collect();
        let output = super::body_hex(&bytes);
        assert_eq!(output.matches("0a ").count(), 1);
        assert_eq!(output.len(), bytes.len() * 3);
        assert!(output.ends_with("63 "));
    }
}
