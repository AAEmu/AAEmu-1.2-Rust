//! Decoded-but-unsent quest S->C packets (WP-UI7).
//!
//! The layouts come from the 1.2 client readers only (cited per packet). The former C# tree
//! declared only the opcode constants (`SCOffsets.cs:420` and `:321`), never the packet classes.
//! Nothing has a known sender, so no code constructs these types: they exist so a later capture
//! only has to add the trigger. Both stay in `rust/tools/opcode_coverage/sc_baseline.json` as
//! typed-but-never-sent.

use aaemu_commons::{Marshal, PacketStream};

use crate::net::packet::LEVEL_PLAIN;
use crate::opcodes::sc;
use crate::packets::packet_def;

/// `SCShowQuestAreaPacket(questType, area, radius)`.
///
/// sent_by: none (capture-pending). Client reader `FUN_397b7940`
/// (`x2game.dll.c:1710613`): `u32 questType`, then the `Area` block `FUN_3961e270`
/// (`x2game.dll.c:1300621`) = two `i64` and one `f32`, then `f32 radius`. The two `i64`
/// values use the client's long-coordinate convention and the `f32` is the height
/// (INFERRED: the reader does not name the `Area` fields). Handler `FUN_391df7b0` ->
/// `FUN_393840b0(1, questType, &area, radius)` pushes a 0x14-byte entry onto the client's
/// quest-area list. The trigger is UNKNOWN (CAP-QU-2): the client already loads
/// `client/quest_sign_sphere.g` itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SCShowQuestAreaPacket {
    pub quest_type: u32,
    pub x: i64,
    pub y: i64,
    pub z: f32,
    pub radius: f32,
}
packet_def!(
    SCShowQuestAreaPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCShowQuestAreaPacket
);
impl Marshal for SCShowQuestAreaPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.quest_type)
            .write_i64(self.x)
            .write_i64(self.y)
            .write_f32(self.z)
            .write_f32(self.radius);
    }
}

/// `SCQuestAcceptConditional(sphereId, questContextId)`: the server offers a quest that the
/// client shows as a start dialog for a sphere starter.
///
/// sent_by: `spheres::SphereQuestManager::tick_area_spheres` on entry into a
/// `SphereAcceptQuest` sphere (`sphere_area_fill.json` carries the areas of 1040, 1453 and
/// 2236; the 1.2 `quest_sign_sphere.g` pack has none for these quests). The entry trigger is
/// INFERRED (CAP-QU-2): the area and the `sphere_accept_quest_quests` link exist, but no
/// capture shows the retail send. Client reader `FUN_397b5d80`
/// (`x2game.dll.c:1709358`) reads two `u32` values; handler `FUN_391dbed0` ->
/// `FUN_39376690(a, b)` raises `START_QUEST_CONTEXT_SPHERE` (event 0xec) with the arguments
/// in `(b, a)` order: `b` is the quest type the dialog looks up and `a` is the sphere id the
/// accept reply must carry (the 1.2 `quest_context.alb` sphere handler calls
/// `StartQuestContext(3, qtype, sphere, ...)`; the 0xeb doodad sibling was cross-checked).
/// The client answers `CSStartQuestContext` 0xd5 with that sphere id
/// (`systems/quests/events.rs::start_quest_context`).
///
/// Naming: the 1.2 opcode table stores this S->C opcode as
/// `sc::CSQuestAcceptConditionalPacket` (0x14a), a misnomer. `opcodes.rs` is never edited, so
/// the constant is referenced as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCQuestAcceptConditionalPacket {
    /// `spheres.id` of the `SphereAcceptQuest` sphere the player entered.
    pub sphere_id: u32,
    /// `quest_contexts.id` of the offered quest.
    pub quest_id: u32,
}
packet_def!(
    SCQuestAcceptConditionalPacket,
    level = LEVEL_PLAIN,
    opcode = sc::CSQuestAcceptConditionalPacket
);
impl Marshal for SCQuestAcceptConditionalPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.sphere_id).write_u32(self.quest_id);
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
        assert_eq!(SCShowQuestAreaPacket::OPCODE, 0x1af);
        assert_eq!(SCQuestAcceptConditionalPacket::OPCODE, 0x14a);
    }

    #[test]
    fn show_quest_area_is_type_area_and_radius() {
        let p = SCShowQuestAreaPacket {
            quest_type: 0x0102_0304,
            x: -7,
            y: 11,
            z: 12.5,
            radius: 25.0,
        };
        let want: Vec<u8> = vec![
            0x04, 0x03, 0x02, 0x01, // questType
            0xf9, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // x = -7
            0x0b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // y = 11
            0x00, 0x00, 0x48, 0x41, // z = 12.5f
            0x00, 0x00, 0xc8, 0x41, // radius = 25.0f
        ];
        assert_eq!(body(&p), want);
    }

    #[test]
    fn quest_accept_conditional_is_sphere_then_quest() {
        // Reader FUN_397b5d80: `u32 a` (sphere id), then `u32 b` (quest context id).
        let p = SCQuestAcceptConditionalPacket {
            sphere_id: 0x1122_3344,
            quest_id: 0x5566_7788,
        };
        assert_eq!(
            body(&p),
            vec![0x44, 0x33, 0x22, 0x11, 0x88, 0x77, 0x66, 0x55]
        );
    }
}
