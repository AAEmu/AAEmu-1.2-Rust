use super::*;

#[test]
fn server_observation_does_not_report_a_locally_predicted_position() {
    let mut state = GameState {
        self_obj: 42,
        self_pos: (900.0, 900.0, 900.0),
        ..GameState::default()
    };
    assert_eq!(state.observed_self_pos, None);
    let mut body = PacketStream::new();
    body.write_bc(42)
        .write_u8(1)
        .write_u32(123)
        .write_u8(0)
        .write_position(10.0, 20.0, 30.0);
    state
        .observe(&Packet {
            level: LEVEL_GAME,
            opcode: sc::SCOneUnitMovementPacket,
            body: body.as_bytes().to_vec(),
        })
        .unwrap();
    // The native position codec quantizes the authored Z=30 to 29.9995.
    assert_eq!(state.observed_self_pos, Some((10.0, 20.0, 29.9995)));
    assert_eq!(state.self_pos, (900.0, 900.0, 900.0));
    assert_eq!(state.observation_seq, 1);
}
