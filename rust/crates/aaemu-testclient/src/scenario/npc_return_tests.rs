use super::npc_return::{quest_rows, read_quest_row};
use super::*;
use aaemu_commons::Marshal;
use aaemu_game::models::char_data::ActiveQuest;
use aaemu_game::systems::quests::model::status;
use aaemu_game::systems::quests::model::{acceptor, kind};
use aaemu_game::systems::quests::packets::{
    SCQuestContextStartedPacket, SCQuestContextUpdatedPacket,
};

use crate::game::LEVEL_GAME;

fn quest(template_id: u32, quest_status: u8) -> ActiveQuest {
    ActiveQuest {
        id: 7,
        template_id,
        status: quest_status,
        objectives: [1, 0, 0, 0, 0],
        step: kind::PROGRESS,
        acceptor_type: acceptor::NPC,
        component_id: 1001,
        acceptor_id: 3597,
        time_ms: ActiveQuest::DATETIME_MIN_MS,
    }
}

#[test]
fn sc_quests_packet_rows_decode_the_active_quest_layout() {
    let mut body = PacketStream::new();
    body.write_i32(1);
    quest(330, status::PROGRESS).write(&mut body, 0);
    assert_eq!(
        quest_rows(body.as_bytes()).unwrap(),
        vec![(330, status::PROGRESS)]
    );
}

#[test]
fn sc_quests_packet_rows_decode_multiple_rows_in_order() {
    let mut body = PacketStream::new();
    body.write_i32(2);
    quest(330, status::PROGRESS).write(&mut body, 0);
    quest(331, status::READY).write(&mut body, 0);
    assert_eq!(
        quest_rows(body.as_bytes()).unwrap(),
        vec![(330, status::PROGRESS), (331, status::READY)]
    );
}

#[test]
fn sc_quests_packet_rejects_negative_and_malformed_counts() {
    let mut negative = PacketStream::new();
    negative.write_i32(-1);
    assert!(quest_rows(negative.as_bytes()).is_err());

    assert!(quest_rows(&[]).is_err());

    let mut body = PacketStream::new();
    body.write_i32(2);
    quest(330, status::PROGRESS).write(&mut body, 0);
    quest(331, status::READY).write(&mut body, 0);
    let mut bytes = body.into_bytes();
    bytes.truncate(bytes.len() - 8);
    assert!(quest_rows(&bytes).is_err());
}

#[test]
fn read_quest_row_parses_incrementally_and_errors_on_truncation() {
    let mut body = PacketStream::new();
    quest(330, status::PROGRESS).write(&mut body, 0);
    quest(331, status::READY).write(&mut body, 0);
    let mut bytes = body.into_bytes();
    bytes.truncate(bytes.len() - 8);

    let mut s = PacketStream::from_bytes(bytes);
    assert_eq!(read_quest_row(&mut s).unwrap(), (330, status::PROGRESS));
    assert!(read_quest_row(&mut s).is_err());
}

#[test]
fn quest_started_and_updated_bodies_identify_the_quest_template() {
    let mut body = PacketStream::new();
    SCQuestContextStartedPacket {
        quest: quest(330, status::INVALID),
        component_id: 1001,
        now_ms: 0,
    }
    .write(&mut body);
    let started = Packet {
        level: LEVEL_GAME,
        opcode: sc::SCQuestContextStartedPacket,
        body: body.into_bytes(),
    };
    assert_eq!(quest_template_id(&started).unwrap(), 330);

    let mut body = PacketStream::new();
    SCQuestContextUpdatedPacket::new(quest(330, status::PROGRESS), 0, 0).write(&mut body);
    let updated = Packet {
        level: LEVEL_GAME,
        opcode: sc::SCQuestContextUpdatedPacket,
        body: body.into_bytes(),
    };
    assert_eq!(quest_template_id(&updated).unwrap(), 330);
}

#[test]
fn movement_echo_object_id_is_the_leading_bc() {
    let mut body = PacketStream::new();
    body.write_bc(0x1234).write_u8(1);
    let p = Packet {
        level: LEVEL_GAME,
        opcode: sc::SCOneUnitMovementPacket,
        body: body.into_bytes(),
    };
    assert_eq!(packet_obj_id(&p).unwrap(), 0x1234);
}
