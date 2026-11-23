use super::*;
use aaemu_commons::Marshal;
use aaemu_game::systems::quests::packets::SCDoodadCompleteQuestPacket;

#[test]
fn observe_records_the_doodad_object_id_and_quest_id() {
    let mut state = GameState::default();
    let mut body = PacketStream::new();
    SCDoodadCompleteQuestPacket {
        doodad_obj_id: 0x300,
        quest_id: 2563,
    }
    .write(&mut body);
    state
        .observe(&Packet {
            level: LEVEL_GAME,
            opcode: sc::SCDoodadCompleteQuestPacket,
            body: body.into_bytes(),
        })
        .unwrap();
    assert_eq!(state.doodad_complete_quests, vec![(0x300, 2563)]);
}
