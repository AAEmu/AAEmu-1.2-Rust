//! Item and craft notification packets: `SCCraftItemUnlockPacket` 0x0f5,
//! `SCItemAcquisitionPacket` 0x097, `SCItemDetailUpdatedPacket` 0x093 and
//! `SCItemTaskNotifyPacket` 0x091.
//!
//! The layouts come from the 1.2 client readers only (cited per packet). `SCItemAcquisitionPacket`
//! is sent for loot pickups behind `Loot:AnnounceToParty` (default off, WP-IT1); the zone
//! binaries still show no trigger for the other three, so the server deliberately never sends
//! them: they stay in the S->C coverage baseline as typed-but-never-sent and are recorded as
//! capture-pending in `PORTING.md` (`ITEM-NOTIFY-1`, `-3`, `-4`). A future capture that shows a
//! trigger only has to construct the packet; the wire formats are test-locked here.

use aaemu_commons::{Marshal, PacketStream};

use crate::models::item::Item;
use crate::models::item_task::ItemTask;
use crate::net::packet::LEVEL_PLAIN;
use crate::opcodes::sc;
use crate::packets::packet_def;

/// `ItemDetail`: `u8 detailType` followed by the fixed body of that type, exactly as the 1.2
/// client's item detail reader `FUN_3961edf0` (`x2game.dll.c:1301101`) consumes it: 55 bytes for
/// equipment, 29 slave, 6 mate, 9 UCC, 24 treasure, 16 big fish / decoration, 8 music sheet and
/// 4 glider; `0` (invalid) reads no body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemDetail {
    pub detail_type: u8,
    pub body: Vec<u8>,
}
impl Marshal for ItemDetail {
    fn write(&self, s: &mut PacketStream) {
        s.write_u8(self.detail_type).write_raw(&self.body);
    }
}

/// `SCCraftItemUnlockPacket`: two single bytes.
///
/// Client reader `FUN_397b4c60` (`x2game.dll.c:1708523`, wrapper `FUN_391ae560` 0x0f5,
/// `x2game.dll.c:329499`) zeroes `+0xc` and `+0xe`, then reads one `u8` into `+0xd` and one `u8`
/// into `+0xf`; nothing else is on the wire. The handler `FUN_391d9700`
/// (`x2game.dll.c:367435`) packs `+0xc..+0xf` into an item-slot value and unlocks it, so the pair
/// is INFERRED to be (slot type, slot index). Trigger UNKNOWN-3: our server never locks craft
/// materials, so there is nothing to unlock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCCraftItemUnlockPacket {
    pub slot_type: u8,
    pub slot: u8,
}
packet_def!(
    SCCraftItemUnlockPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCCraftItemUnlockPacket
);
impl Marshal for SCCraftItemUnlockPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u8(self.slot_type).write_u8(self.slot);
    }
}

/// `SCItemAcquisitionPacket(unkId, charName, items)`: `u8` type, a string, `u8` count, then that
/// many item records.
///
/// Client reader `FUN_397c1f80` (`x2game.dll.c:1717976`, wrapper `FUN_391ab510` 0x097,
/// `x2game.dll.c:325922`): `u8` at `+0xc` (the reader's unnamed `DAT_3999d00c` field), the string
/// `"charName"` (maximum 128 bytes), `u8` `"count"` at `+0x91`, then `count` records read by the
/// standard item reader `FUN_3961f500` (`x2game.dll.c:1301392`, the same record `Item::write`
/// emits for `SCItemTaskSuccessPacket`). The handler acts only for type `1` and builds an item
/// link line naming `charName` (INFERRED: a party/raid member obtained an item). Trigger
/// (INFERRED, WP-IT1): `systems::economy::looting` sends one packet per loot pickup to the other
/// team members in loot range when `Loot:AnnounceToParty` is enabled; which grades / sources the
/// client announces is still decided client-side (`FUN_39695dc0`).
#[derive(Debug, Clone, PartialEq)]
pub struct SCItemAcquisitionPacket {
    pub kind: u8,
    pub char_name: String,
    pub items: Vec<Item>,
}
packet_def!(
    SCItemAcquisitionPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCItemAcquisitionPacket
);
impl Marshal for SCItemAcquisitionPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u8(self.kind).write_str(&self.char_name);
        s.write_u8(self.items.len() as u8);
        for item in &self.items {
            item.write(s);
        }
    }
}

/// `SCItemDetailUpdatedPacket(itemId, x, index, detail)`: `u64`, two single bytes, then an
/// [`ItemDetail`].
///
/// Client reader `FUN_397b3840` (`x2game.dll.c:1707583`, wrapper `FUN_391aaeb0` 0x093,
/// `x2game.dll.c:325579`): `u64` at `+0x10`, `u8` at `+0x19` (the reader's unnamed
/// `DAT_3999d00c` field), `u8` `"index"` at `+0x1b`, then `FUN_3961edf0` reads the detail at
/// `+0x1c`. The bytes at `+0x18` and `+0x1a` are zeroed in memory and never read, so no padding
/// is written. (Slot type, slot index) is INFERRED. Trigger UNKNOWN-5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCItemDetailUpdatedPacket {
    pub item_id: u64,
    pub slot_type: u8,
    pub slot: u8,
    pub detail: ItemDetail,
}
packet_def!(
    SCItemDetailUpdatedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCItemDetailUpdatedPacket
);
impl Marshal for SCItemDetailUpdatedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u64(self.item_id)
            .write_u8(self.slot_type)
            .write_u8(self.slot);
        self.detail.write(s);
    }
}

/// `SCItemTaskNotifyPacket(unkId, action, tasks, forceRemove, typeId)`: a `u32` followed by the
/// same body as `SCItemTaskSuccessPacket`.
///
/// Client reader `FUN_397b3810` (`x2game.dll.c:1707572`, wrapper `FUN_391e3930` 0x091,
/// `x2game.dll.c:375686`): `u32` at `+0x4948`, then `FUN_397d1d00`
/// (`x2game.dll.c:1729938`, the `SCItemTaskSuccessPacket` reader): `u8` action, `u8` count
/// (capped at 30), that many item tasks (`FUN_397d1b20`), `u8` force-remove count (values above
/// 30 fall back to the maximum), that many `u64`s and a final `u32`. Trigger UNKNOWN-6
/// (probably item tasks for an owner other than the viewer).
#[derive(Debug, Clone, PartialEq)]
pub struct SCItemTaskNotifyPacket {
    pub unk_id: u32,
    pub action: u8,
    pub tasks: Vec<ItemTask>,
    pub force_remove: Vec<u64>,
    pub type_id: u32,
}
packet_def!(
    SCItemTaskNotifyPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCItemTaskNotifyPacket
);
impl Marshal for SCItemTaskNotifyPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.unk_id)
            .write_u8(self.action)
            .write_u8(self.tasks.len() as u8);
        for task in &self.tasks {
            task.write(s);
        }
        s.write_u8(self.force_remove.len() as u8);
        for id in &self.force_remove {
            s.write_u64(*id);
        }
        s.write_u32(self.type_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::items::{ItemData, ItemTemplate};
    use crate::net::packet::PacketDef;

    fn body(p: &impl Marshal) -> Vec<u8> {
        let mut s = PacketStream::new();
        p.write(&mut s);
        s.into_bytes()
    }

    #[test]
    fn opcodes_are_the_1_2_ones() {
        assert_eq!(SCCraftItemUnlockPacket::OPCODE, 0x0f5);
        assert_eq!(SCItemAcquisitionPacket::OPCODE, 0x097);
        assert_eq!(SCItemDetailUpdatedPacket::OPCODE, 0x093);
        assert_eq!(SCItemTaskNotifyPacket::OPCODE, 0x091);
    }

    #[test]
    fn craft_item_unlock_is_two_single_bytes() {
        let p = SCCraftItemUnlockPacket {
            slot_type: 3,
            slot: 7,
        };
        assert_eq!(body(&p), vec![3, 7]);
    }

    #[test]
    fn item_acquisition_is_type_name_count_and_items() {
        let item = Item::create(
            0x0102_0304_0506_0708,
            &ItemTemplate::plain(0x1122_3344, 100),
            1,
            3,
            5,
            &ItemData::default(),
            1000,
        );
        let p = SCItemAcquisitionPacket {
            kind: 1,
            char_name: "Alice".into(),
            items: vec![item],
        };
        let mut want = vec![1, 5, 0, b'A', b'l', b'i', b'c', b'e', 1];
        want.extend_from_slice(&0x1122_3344u32.to_le_bytes());
        want.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        want.push(3); // grade
        want.push(0); // flags
        want.extend_from_slice(&1i32.to_le_bytes()); // stackSize
        want.push(0); // detailType (plain Item)
        want.extend_from_slice(&1000i64.to_le_bytes()); // creationTime
        want.extend_from_slice(&0i32.to_le_bytes()); // lifespanMins
        want.extend_from_slice(&0u32.to_le_bytes()); // madeUnitId
        want.push(5); // worldId
        want.extend_from_slice(&0i64.to_le_bytes()); // unsecureDateTime
        want.extend_from_slice(&0i64.to_le_bytes()); // unpackDateTime
        assert_eq!(body(&p), want);
    }

    #[test]
    fn item_detail_updated_is_item_id_two_bytes_and_detail() {
        let p = SCItemDetailUpdatedPacket {
            item_id: 0x1122_3344_5566_7788,
            slot_type: 2,
            slot: 9,
            detail: ItemDetail {
                detail_type: 8, // music sheet: 8 body bytes (client reader FUN_3961edf0)
                body: vec![1, 2, 3, 4, 5, 6, 7, 8],
            },
        };
        let mut want = 0x1122_3344_5566_7788u64.to_le_bytes().to_vec();
        want.extend_from_slice(&[2, 9, 8, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(body(&p), want);
    }

    #[test]
    fn item_task_notify_is_u32_plus_the_task_success_body() {
        let p = SCItemTaskNotifyPacket {
            unk_id: 0x0102_0304,
            action: 83,
            tasks: vec![ItemTask::MoneyChange(-5)],
            force_remove: vec![9],
            type_id: 0x0a0b_0c0d,
        };
        let mut want = 0x0102_0304u32.to_le_bytes().to_vec();
        want.extend_from_slice(&[83, 1, 1]); // action, task count, MoneyChange
        want.extend_from_slice(&(-5i32).to_le_bytes());
        want.push(1); // forceRemoveCount
        want.extend_from_slice(&9u64.to_le_bytes());
        want.extend_from_slice(&0x0a0b_0c0du32.to_le_bytes());
        assert_eq!(body(&p), want);
    }
}
