//! Decoded-but-unsent UI / notification S->C packets (WP-UI7).
//!
//! The layouts come from the 1.2 client readers only (cited per packet). The former C# tree
//! declared only the opcode constants (`SCOffsets.cs`), never the packet classes. Nothing here
//! has a known sender, so no code constructs these types: they exist so a later capture only has
//! to add the trigger. They stay in `rust/tools/opcode_coverage/sc_baseline.json` as
//! typed-but-never-sent. `SCWorldMessagePacket` (0x211) is not repeated here: it is already
//! defined in `packets/g2c_misc.rs:169` and is likewise never sent.

use aaemu_commons::{Marshal, PacketStream};

use crate::net::packet::LEVEL_PLAIN;
use crate::opcodes::sc;
use crate::packets::packet_def;

/// `SCScheduledEventStartedPacket`: a schedule record plus the opening message.
///
/// sent_by: none (capture-pending). The trigger (a `game_schedules` window opening) is
/// INFERRED and the record content is not understood (CAP-UI-1), so the packet cannot be
/// filled faithfully yet. Client reader `FUN_397b01f0` (`x2game.dll.c:1704749`), which calls
/// the `ScheduleRecord` reader `FUN_398a7ed0` (`x2game.dll.c:1942981`): `u32 id`, 21 x `i32`
/// (`FUN_398a7d80`, `x2game.dll.c:1942923`), `i32 changes` (a 21-bit mask of those values),
/// then the five `u8` fields priority / startDate / periodDate / two unnamed, `i32 periodMin`,
/// `u8 weekDay`, and finally `string openMsg` (named reader, `<= 0x7f`). The 21 `i32` values
/// and the five bytes are documented as opaque: the decompile gives them no names and
/// `game_schedules` (`day_of_week_id`, `start_time`, `end_time`, `st_*` / `ed_*`) does not map
/// onto them.
#[derive(Debug, Clone, PartialEq)]
pub struct SCScheduledEventStartedPacket {
    pub id: u32,
    /// Opaque 21-value schedule block (meaning UNKNOWN, CAP-UI-1).
    pub values: [i32; 21],
    /// 21-bit mask of the values that changed.
    pub changes: i32,
    /// Opaque five bytes: priority, startDate, periodDate and two unnamed bytes.
    pub flags: [u8; 5],
    pub period_min: i32,
    pub week_day: u8,
    pub open_msg: String,
}
packet_def!(
    SCScheduledEventStartedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCScheduledEventStartedPacket
);
impl Marshal for SCScheduledEventStartedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.id);
        for value in &self.values {
            s.write_i32(*value);
        }
        s.write_i32(self.changes).write_raw(&self.flags);
        s.write_i32(self.period_min)
            .write_u8(self.week_day)
            .write_str(&self.open_msg);
    }
}

/// `SCScheduledEventEndedPacket`: the record id plus the closing message.
///
/// sent_by: none (capture-pending; trigger UNKNOWN, CAP-UI-1). Client reader `FUN_397b0260`
/// (`x2game.dll.c:1704772`): `u32 id`, then `string closeMsg` (named reader, `<= 0x7f`). Handler
/// `FUN_391e1f30` updates the client's current day-event record and raises `EVENT_SCHEDULE_STOP`
/// (0x257) with the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCScheduledEventEndedPacket {
    pub id: u32,
    pub close_msg: String,
}
packet_def!(
    SCScheduledEventEndedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCScheduledEventEndedPacket
);
impl Marshal for SCScheduledEventEndedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.id).write_str(&self.close_msg);
    }
}

/// `SCNotifyUIMessagePacket(msgType, value, argv0, argv1)`.
///
/// sent_by: none (capture-pending; trigger UNKNOWN). Client reader `FUN_397afe30`
/// (`x2game.dll.c:1704561`): `i32 msgType`, a second unnamed `i32` (`DAT_39a128c8` in the
/// decompile, hence exposed as an opaque `value`), `string argv0` (`<= 0xff`) and
/// `string argv1` (`<= 0xff`). No client handler for this opcode was found in
/// `strings-xrefs.tsv`, so what it shows is UNKNOWN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCNotifyUIMessagePacket {
    pub msg_type: i32,
    pub value: i32,
    pub argv0: String,
    pub argv1: String,
}
packet_def!(
    SCNotifyUIMessagePacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCNotifyUIMessagePacket
);
impl Marshal for SCNotifyUIMessagePacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_i32(self.msg_type)
            .write_i32(self.value)
            .write_str(&self.argv0)
            .write_str(&self.argv1);
    }
}

/// `SCUnitLocationPacket(unitId, x, y, z)`: point the camera / minimap at a unit.
///
/// sent_by: none (capture-pending; trigger UNKNOWN). Client reader `FUN_397c6f90`
/// (`x2game.dll.c:1721705`): `unitId` as the 3-byte `bc` (`FUN_397b27d0`,
/// `x2game.dll.c:1706761`), then the 9-byte packed world position (`FUN_3961e2c0`,
/// `x2game.dll.c:1300633`) -- the same block [`PacketStream::write_position`] emits. Handler
/// `FUN_391e0940` (registered as `OnUnitLocation`) positions the camera and minimap on that
/// unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SCUnitLocationPacket {
    pub unit_id: u32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}
packet_def!(
    SCUnitLocationPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCUnitLocationPacket
);
impl Marshal for SCUnitLocationPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.unit_id)
            .write_position(self.x, self.y, self.z);
    }
}

/// `SCChatLocalizedMessagePacket`: a localized chat line with up to two parameters.
///
/// sent_by: none (capture-pending; no trigger is known). Client reader `FUN_397c2730`
/// (`x2game.dll.c:1718316`): `i64 id`, `bc unitId`, `u8 LanguageType`, `i32 category`, a
/// `string` (`<= 0x400`), a second `string` (`<= 0xff`), `i32 numParam`, `string param`
/// (`<= 0x80`) and `string param2` (`<= 0x80`). Both strings are unnamed in the reader
/// (`DAT_3999da20` / `DAT_399c1dc8`); the handler looks the second one up as the locale key
/// with `category` (INFERRED), so they are exposed as `text` and `key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCChatLocalizedMessagePacket {
    pub id: i64,
    pub unit_id: u32,
    pub language_type: u8,
    pub category: i32,
    pub text: String,
    pub key: String,
    pub num_param: i32,
    pub param: String,
    pub param2: String,
}
packet_def!(
    SCChatLocalizedMessagePacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCChatLocalizedMessagePacket
);
impl Marshal for SCChatLocalizedMessagePacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_i64(self.id)
            .write_bc(self.unit_id)
            .write_u8(self.language_type)
            .write_i32(self.category)
            .write_str(&self.text)
            .write_str(&self.key)
            .write_i32(self.num_param)
            .write_str(&self.param)
            .write_str(&self.param2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::packet::PacketDef;

    fn body(p: &impl Marshal) -> Vec<u8> {
        let mut s = PacketStream::new();
        p.write(&mut s);
        s.into_bytes()
    }

    #[test]
    fn opcodes_are_the_1_2_ones() {
        assert_eq!(SCScheduledEventStartedPacket::OPCODE, 0x1ec);
        assert_eq!(SCScheduledEventEndedPacket::OPCODE, 0x1ed);
        assert_eq!(SCNotifyUIMessagePacket::OPCODE, 0x1cc);
        assert_eq!(SCUnitLocationPacket::OPCODE, 0x1c7);
        assert_eq!(SCChatLocalizedMessagePacket::OPCODE, 0x0d2);
    }

    #[test]
    fn scheduled_event_started_is_id_21_ints_mask_flags_period_weekday_message() {
        let p = SCScheduledEventStartedPacket {
            id: 0x1122_3344,
            values: [
                3, 6, 9, 12, 15, 18, 21, 24, 27, 30, 33, 36, 39, 42, 45, 48, 51, 54, 57, 60, 63,
            ],
            changes: -2,
            flags: [1, 2, 3, 4, 5],
            period_min: 90,
            week_day: 6,
            open_msg: "open".into(),
        };
        let mut want = vec![0x44, 0x33, 0x22, 0x11];
        for i in 1..=21i32 {
            want.extend_from_slice(&(i * 3).to_le_bytes());
        }
        want.extend_from_slice(&[0xfe, 0xff, 0xff, 0xff]); // changes = -2
        want.extend_from_slice(&[1, 2, 3, 4, 5]); // flags
        want.extend_from_slice(&[90, 0, 0, 0]); // periodMin
        want.push(6); // weekDay
        want.extend_from_slice(&[4, 0, b'o', b'p', b'e', b'n']);
        assert_eq!(body(&p), want);
    }

    #[test]
    fn scheduled_event_ended_is_id_and_message() {
        let p = SCScheduledEventEndedPacket {
            id: 0xdead_beef,
            close_msg: "end".into(),
        };
        assert_eq!(
            body(&p),
            vec![0xef, 0xbe, 0xad, 0xde, 3, 0, b'e', b'n', b'd']
        );
    }

    #[test]
    fn notify_ui_message_is_two_ints_and_two_strings() {
        let p = SCNotifyUIMessagePacket {
            msg_type: 7,
            value: 9,
            argv0: "ab".into(),
            argv1: "c".into(),
        };
        assert_eq!(
            body(&p),
            vec![7, 0, 0, 0, 9, 0, 0, 0, 2, 0, b'a', b'b', 1, 0, b'c']
        );
    }

    #[test]
    fn unit_location_is_bc_unit_id_and_packed_position() {
        let p = SCUnitLocationPacket {
            unit_id: 0x000a_0b0c,
            x: 100.0,
            y: -200.0,
            z: 50.0,
        };
        let want = vec![
            0x0c, 0x0b, 0x0a, // unitId (bc)
            0x00, 0xc8, 0x00, // x = 100.0
            0x00, 0x90, 0x01, // y = -200.0
            0xb3, 0x49, 0x42, // z = 50.0 + the sign bits
        ];
        assert_eq!(body(&p), want);
    }

    #[test]
    fn chat_localized_message_is_id_unit_language_category_two_strings_and_params() {
        let p = SCChatLocalizedMessagePacket {
            id: -2,
            unit_id: 0x000a_0b0c,
            language_type: 3,
            category: 4,
            text: "Hi".into(),
            key: "You".into(),
            num_param: 2,
            param: "p".into(),
            param2: "q".into(),
        };
        let want = vec![
            0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // id = -2
            0x0c, 0x0b, 0x0a, // unitId (bc)
            0x03, // LanguageType
            0x04, 0x00, 0x00, 0x00, // category
            0x02, 0x00, b'H', b'i', // text
            0x03, 0x00, b'Y', b'o', b'u', // key
            0x02, 0x00, 0x00, 0x00, // numParam
            0x01, 0x00, b'p', // param
            0x01, 0x00, b'q', // param2
        ];
        assert_eq!(body(&p), want);
    }
}
