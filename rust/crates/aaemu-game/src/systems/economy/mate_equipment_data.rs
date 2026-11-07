//! `MateEquipmentData`: the 1.2 client's mate equipment tables (`mate_equip_slot_packs`,
//! `mate_equip_pack_groups` x `mate_equip_pack_items`).
//!
//! The slot pack rule is EVIDENCED and enforced on `CSChangeMateEquipmentPacket` (see
//! [`super::mate_equipment`]): the client's `IsMateEquipableSlot` (`FUN_3933a9c0`) tests the
//! `mate_equip_slot_packs` row loaded by `LoadMateEquipSlotPackDescs` (`FUN_396ce630`, SQL
//! `SELECT id, chest, feet, head, name, waist FROM mate_equip_slot_packs`) and answers exactly
//! the four 1.2 equip-slot indices head 0, chest 2, waist 3 and feet 6.
//!
//! The per-mate item list is the client's whitelist, enforced only while FeatureSet bit 78 is
//! set. The client loads the `(npc_id, item_id)` pairs of `mate_equip_pack_groups` x
//! `mate_equip_pack_items` in `FUN_395fbbc0` (SQL at `0x399f9490`) and reads them through
//! `FUN_395ac760` from the item-use / equip validation `FUN_394e41f0` (`dll.c:1021538`). The
//! call is gated on `(*(DAT_39b02294 + 0x1c) & 0x4000)`, which is the client's copy of the
//! server FeatureSet: fset byte 9 bit 6 = feature 78 (`flag_9_6`). `DEFAULT_FSET[9] = 0x0c`
//! leaves it clear, so the whitelist is off until the server enables the feature. The real data
//! also contains 1058 group/item rows whose item slot the mate's own slot pack forbids (see
//! PORTING.md).

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::data::sqlite::for_each_row;
use crate::data::GameDataError;

/// One `mate_equip_slot_packs` row (`LoadMateEquipSlotPackDescs`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MateSlotPack {
    pub head: bool,
    pub chest: bool,
    pub waist: bool,
    pub feet: bool,
}

impl MateSlotPack {
    /// `IsMateEquipableSlot` (`FUN_3933a9c0`, `dll.c:667569`): the 1.2 equip-slot indices
    /// head 0, chest 2, waist 3 and feet 6; every other slot is not equipable on a mate. The
    /// reader uses the row fields the loader fills: chest `+0`, feet `+1`, head `+2`,
    /// waist `+8` (`FUN_396ce630`, `dll.c:1479277`).
    pub fn allows(&self, equip_slot: i32) -> bool {
        match equip_slot {
            0 => self.head,
            2 => self.chest,
            3 => self.waist,
            6 => self.feet,
            _ => false,
        }
    }
}

/// The mate equipment tables of `compact.sqlite3`.
#[derive(Debug, Default)]
pub struct MateEquipmentData {
    /// `mate_equip_slot_packs` by id (1 riding, 2 combat, 3 pet in the 1.2 data).
    pub slot_packs: HashMap<i32, MateSlotPack>,
    /// `mate_equip_pack_groups` joined with `mate_equip_pack_items`, by mate NPC template id.
    /// Loaded for the INFERRED whitelist logging only (see the module docs).
    pub pack_items: HashMap<u32, HashSet<u32>>,
}

fn has_table(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

impl MateEquipmentData {
    /// Loads the tables; a database without them (e.g. a test fixture) yields the defaults.
    pub fn load(conn: &Connection) -> Result<Self, GameDataError> {
        let mut data = Self::default();
        if !has_table(conn, "mate_equip_slot_packs") {
            return Ok(data);
        }
        for_each_row(conn, "SELECT * FROM mate_equip_slot_packs", [], |r| {
            let id = r.i32_or("id", 0)?;
            if id == 0 {
                return Ok(()); // NULL / 0: not a pack id the client can select
            }
            data.slot_packs.insert(
                id,
                MateSlotPack {
                    // `'t'` / `'f'` text columns; NULL reads as false like the client.
                    head: r.bool_str("head")?,
                    chest: r.bool_str("chest")?,
                    waist: r.bool_str("waist")?,
                    feet: r.bool_str("feet")?,
                },
            );
            Ok(())
        })?;
        if !has_table(conn, "mate_equip_pack_groups") || !has_table(conn, "mate_equip_pack_items") {
            return Ok(data);
        }
        // The same join the 1.2 client runs in `FUN_395fbbc0`; a NULL or 0 pair is skipped.
        for_each_row(
            conn,
            "SELECT d.npc_id AS npc_id, c.item_id AS item_id \
             FROM mate_equip_pack_groups AS d \
             INNER JOIN (SELECT a.id AS id, b.item_id AS item_id \
                         FROM mate_equip_packs a \
                         INNER JOIN mate_equip_pack_items b \
                         ON a.id = b.mate_equip_pack_id) AS c \
             ON d.mate_equip_pack_id = c.id",
            [],
            |r| {
                let npc_id = r.u32_or("npc_id", 0)?;
                let item_id = r.u32_or("item_id", 0)?;
                if npc_id != 0 && item_id != 0 {
                    data.pack_items.entry(npc_id).or_default().insert(item_id);
                }
                Ok(())
            },
        )?;
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE mate_equip_slot_packs (id, name, head, chest, waist, feet);
             INSERT INTO mate_equip_slot_packs VALUES (1, 'riding', 't', 'f', 't', 't');
             INSERT INTO mate_equip_slot_packs VALUES (2, 'combat', 't', 't', 'f', 't');
             INSERT INTO mate_equip_slot_packs VALUES (3, 'pet', 'f', 'f', 'f', 'f');
             CREATE TABLE mate_equip_packs (id, name);
             INSERT INTO mate_equip_packs VALUES (1, 'pack');
             CREATE TABLE mate_equip_pack_items (id, mate_equip_pack_id, item_id);
             INSERT INTO mate_equip_pack_items VALUES (1, 1, 100);
             INSERT INTO mate_equip_pack_items VALUES (2, 1, 101);
             CREATE TABLE mate_equip_pack_groups (id, npc_id, mate_equip_pack_id);
             INSERT INTO mate_equip_pack_groups VALUES (1, 500, 1);
             INSERT INTO mate_equip_pack_groups VALUES (2, 501, 1);",
        )
        .unwrap();
        c
    }

    #[test]
    fn loads_the_slot_packs_and_the_client_item_join() {
        let data = MateEquipmentData::load(&db()).unwrap();
        assert_eq!(
            data.slot_packs.get(&1),
            Some(&MateSlotPack {
                head: true,
                chest: false,
                waist: true,
                feet: true
            })
        );
        assert!(!data.slot_packs[&3].allows(0));
        assert!(data.slot_packs[&2].allows(2) && !data.slot_packs[&2].allows(3));
        assert_eq!(data.pack_items[&500], HashSet::from([100_u32, 101_u32]));
        assert_eq!(data.pack_items[&501], HashSet::from([100_u32, 101_u32]));
    }

    #[test]
    fn a_database_without_the_tables_yields_no_rules() {
        let c = Connection::open_in_memory().unwrap();
        let data = MateEquipmentData::load(&c).unwrap();
        assert!(data.slot_packs.is_empty() && data.pack_items.is_empty());
    }

    #[test]
    fn null_columns_and_pairs_are_tolerated() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE mate_equip_slot_packs (id, head, chest, waist, feet);
             INSERT INTO mate_equip_slot_packs VALUES (NULL, NULL, NULL, NULL, NULL);
             CREATE TABLE mate_equip_packs (id);
             INSERT INTO mate_equip_packs VALUES (1);
             CREATE TABLE mate_equip_pack_items (mate_equip_pack_id, item_id);
             INSERT INTO mate_equip_pack_items VALUES (1, NULL);
             INSERT INTO mate_equip_pack_items VALUES (1, 100);
             CREATE TABLE mate_equip_pack_groups (npc_id, mate_equip_pack_id);
             INSERT INTO mate_equip_pack_groups VALUES (NULL, 1);
             INSERT INTO mate_equip_pack_groups VALUES (500, 1);",
        )
        .unwrap();
        let data = MateEquipmentData::load(&c).unwrap();
        assert!(data.slot_packs.is_empty());
        assert!(!data.slot_packs.contains_key(&0));
        assert_eq!(data.pack_items[&500], HashSet::from([100_u32]));
        assert!(!data.pack_items.contains_key(&0));
    }

    #[test]
    #[ignore = "needs AAEMU_COMPACT_DB (read-only)"]
    fn real_compact_db_mate_equipment() {
        let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
            return;
        };
        let conn =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let data = MateEquipmentData::load(&conn).unwrap();
        assert_eq!(
            data.slot_packs[&1],
            MateSlotPack {
                head: true,
                chest: false,
                waist: true,
                feet: true
            }
        );
        assert!(data.slot_packs[&2].chest && !data.slot_packs[&2].waist);
        assert!(data.slot_packs[&3] == MateSlotPack::default());
        // 101 mate templates have at least one item in their packs; 3599 (the starting horse)
        // is one of them.
        assert!(data.pack_items.len() >= 101);
        assert!(data.pack_items[&3599].contains(&23090));
    }
}
