use super::*;
use aaemu_commons::Marshal;
use aaemu_game::systems::npcs::doodad::packets::SCDoodadPhaseMsgPacket;

#[test]
fn observe_records_the_doodad_object_id_and_func_group_id() {
    let mut state = GameState::default();
    let mut body = PacketStream::new();
    SCDoodadPhaseMsgPacket {
        doodad_obj_id: 0x300,
        func_group_id: 3333,
    }
    .write(&mut body);
    state
        .observe(&Packet {
            level: LEVEL_GAME,
            opcode: sc::SCDoodadPhaseMsgPacket,
            body: body.into_bytes(),
        })
        .unwrap();
    assert_eq!(state.doodad_phase_msgs, vec![(0x300, 3333)]);
}
