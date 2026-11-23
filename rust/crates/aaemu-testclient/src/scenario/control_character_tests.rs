use super::control_character;
use crate::game::CharInfo;

// Literal SCCharacterList wire body: last=true, count=2. First entry has a full
// equipment item (55-byte detail), nonzero BodyPart ids, faction text, and a Face
// model with a three-byte modifier. Second entry has a Hair model. No server encoder
// or preconstructed CharInfo vector participates in this regression.
const TWO_CHARACTERS: &str = concat!(
    "01020a0000000500466972737401020a6f000000de0000004d00000054000000070046616374696f6e04000000050000",
    "004f1a0000887766554433221101020100000001000102030405060708090a0b0c0d0e0f101112131415161718191a1b",
    "1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435360b000000000000000c0000000d0000000e0f000000",
    "000000001000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000fb030000fc030000fd030000",
    "fe030000ff0300000004000001040000000000000000000001020300a000000000000000400100000000000000f04103",
    "5b0000005c0000005d0000005e0000000000003f0000803f0000803e01000200640000000000003f650000000000003f",
    "660000000000003f670000000000003f6e0000006f000000700000000000403f78000000790000007a0000007b000000",
    "7c000000030007080964006500000000000000020067000000000000006800000069000000000000006a0000006b0000",
    "00000000006c000000000000006d000000000000006e006f000000700071000000000000007200000000000000730000",
    "0000000000740000007500000000000000760000000000000077000000000000000178000000790000007a0000007b00",
    "000000000000002a00000005004f776e657201020a6f000000de0000004d00000054000000070046616374696f6e0400",
    "000005000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
    "00000000000000000000000000000000000000000000000000000000000000000000fb030000fc030000fd030000fe03",
    "0000ff0300000004000001040000000000000000000001020300a000000000000000400100000000000000f041015b00",
    "000064006500000000000000020067000000000000006800000069000000000000006a0000006b000000000000006c00",
    "0000000000006d000000000000006e006f00000070007100000000000000720000000000000073000000000000007400",
    "00007500000000000000760000000000000077000000000000000178000000790000007a0000007b0000000000000000",
);

#[test]
fn native_two_character_packet_selects_its_second_existing_id() {
    use crate::game::{GameState, Packet, LEVEL_GAME};
    use aaemu_game::opcodes::sc;
    let body = hex::decode(TWO_CHARACTERS).unwrap();
    let mut state = GameState::default();
    state
        .observe(&Packet {
            level: LEVEL_GAME,
            opcode: sc::SCCharacterListPacket,
            body: body.clone(),
        })
        .unwrap();
    assert!(state.char_list_done);
    assert_eq!(
        state.characters.iter().map(|c| c.id).collect::<Vec<_>>(),
        [10, 42]
    );
    assert_eq!(
        control_character(&state.characters, "Owner", None).unwrap(),
        42
    );
    assert_eq!(
        control_character(&state.characters, "ignored", Some(42)).unwrap(),
        42
    );
    assert_eq!(state.characters[1].name, "Owner");
    let mut truncated = GameState::default();
    assert!(truncated
        .observe(&Packet {
            level: LEVEL_GAME,
            opcode: sc::SCCharacterListPacket,
            body: body[..body.len() - 1].to_vec()
        })
        .is_err());
    assert!(truncated.characters.is_empty());
    assert!(!truncated.char_list_done);
}

#[test]
fn control_never_falls_back_to_first_or_creates_a_missing_character() {
    let characters = [
        CharInfo {
            id: 10,
            name: "Unrelated".into(),
            appearance: Default::default(),
        },
        CharInfo {
            id: 42,
            name: "Owner".into(),
            appearance: Default::default(),
        },
    ];
    assert_eq!(control_character(&characters, "owner", None).unwrap(), 42);
    assert!(control_character(&characters, "missing", None).is_err());
    assert!(control_character(&[], "Owner", None).is_err());
    assert_eq!(
        control_character(&characters, "renamed", Some(42)).unwrap(),
        42
    );
    assert!(control_character(&characters, "Owner", Some(43)).is_err());
    assert!(control_character(&characters, "Owner", Some(0)).is_err());
}
