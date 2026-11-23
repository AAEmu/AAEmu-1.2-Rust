use super::*;
use crate::game::LEVEL_GAME;

#[test]
fn npc_casts_and_other_skills_cannot_satisfy_a_player_response() {
    let packet = Packet {
        level: LEVEL_GAME,
        opcode: sc::SCSkillFiredPacket,
        body: vec![2, 0, 0, 0, 10, 0, 0, 1, 0, 0],
    };
    assert!(own_skill_response(&packet, 2, 1));
    assert!(!own_skill_response(&packet, 2, 2));
    assert!(!own_skill_response(&packet, 3, 1));
}

#[test]
fn skill_result_precedes_the_optional_uint_in_the_1_2_packet() {
    let packet = Packet {
        level: LEVEL_GAME,
        opcode: sc::SCSkillStartedPacket,
        body: vec![
            2, 0, 0, 0, 10, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 5, 15, 0x34, 0x12, 0, 0,
        ],
    };
    assert_eq!(skill_response_result(&packet).unwrap(), 15);
    let truncated = Packet {
        body: vec![2, 0, 0],
        ..packet
    };
    assert!(skill_response_result(&truncated).is_err());
}
