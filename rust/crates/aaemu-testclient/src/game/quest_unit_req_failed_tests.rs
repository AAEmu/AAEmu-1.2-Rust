use super::*;
use aaemu_commons::Marshal;
use aaemu_game::systems::quests::packets::SCQuestUnitReqFailedPacket;

fn observe(packet: SCQuestUnitReqFailedPacket, state: &mut GameState) {
    let mut body = PacketStream::new();
    packet.write(&mut body);
    state
        .observe(&Packet {
            level: LEVEL_GAME,
            opcode: sc::SCQuestUnitReqFailedPacket,
            body: body.into_bytes(),
        })
        .unwrap();
}

#[test]
fn observe_decodes_the_set_flag_bag_parts() {
    let mut state = GameState::default();
    observe(
        SCQuestUnitReqFailedPacket {
            quest_id: 2563,
            unit_obj_id: 0x300,
            kind: 1,
            message_id: 0,
            value: 10,
        },
        &mut state,
    );
    // The omitted message id decodes as 0, like the client bag does.
    assert_eq!(state.quest_unit_req_failed, vec![(2563, 0x300, 1, 0, 10)]);
}

#[test]
fn observe_decodes_every_part_and_an_empty_bag() {
    let mut state = GameState::default();
    observe(
        SCQuestUnitReqFailedPacket {
            quest_id: 7,
            unit_obj_id: 1,
            kind: 0x16,
            message_id: 0x1234,
            value: 0xaabbccdd,
        },
        &mut state,
    );
    observe(
        SCQuestUnitReqFailedPacket {
            quest_id: 8,
            unit_obj_id: 2,
            kind: 0,
            message_id: 0,
            value: 0,
        },
        &mut state,
    );
    assert_eq!(
        state.quest_unit_req_failed,
        vec![(7, 1, 0x16, 0x1234, 0xaabbccdd), (8, 2, 0, 0, 0),]
    );
}
