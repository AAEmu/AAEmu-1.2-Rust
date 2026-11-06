//! The UCC/crest tables (`ucc_applicables`, `emblem_patterns`) of the economy subsystem.
//!
//! * `ucc_applicables`: `kind_id` 2 (80 rows) lists the **item templates** that may receive a
//!   UCC imprint (`actual_id` joins `items`; the client tooltip names clothes that "take a
//!   picture with the Infinite Imagination dye"). Kinds 1 (18 slave template ids) and 3 (30
//!   flag doodad ids) are INFERRED from their tooltips and are **not** enforced: no UCC use
//!   path in the 1.2 client/server stamps a slave or doodad (research `housing.md` WP-U2).
//! * `emblem_patterns`: the crest pattern ids written by `Ucc.pattern1` / `pattern2`
//!   (`stream/models.rs`). Every row is accepted whatever its `kind_id` (2 / 3 / 4); 0 means
//!   "no pattern". The id space is INFERRED (the 1.2 upload values are capture-pending,
//!   CAP-UCC-1), so an empty / missing table is permissive.
//!
//! Both checks are only as strict as the loaded data: a compact without the rows keeps the
//! pre-WP-U2 behaviour (any owned item, any pattern).

use std::collections::HashSet;

use rusqlite::Connection;

use crate::data::sqlite::for_each_row;

/// `ucc_applicables.kind_id` of item templates that accept a UCC imprint.
pub const APPLICABLE_KIND_ITEM: u32 = 2;

/// The UCC tables (`server.game_data.systems.economy.ucc`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UccData {
    /// `ucc_applicables.actual_id` where `kind_id` is [`APPLICABLE_KIND_ITEM`].
    applicable_items: HashSet<u32>,
    /// Every `emblem_patterns.id`.
    patterns: HashSet<u32>,
}

impl UccData {
    /// `SELECT kind_id, actual_id FROM ucc_applicables` + `SELECT id FROM emblem_patterns`.
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let mut data = Self::default();
        for_each_row(
            conn,
            "SELECT kind_id, actual_id FROM ucc_applicables",
            [],
            |r| {
                if r.u32("kind_id")? == APPLICABLE_KIND_ITEM {
                    data.applicable_items.insert(r.u32("actual_id")?);
                }
                Ok(())
            },
        )?;
        for_each_row(conn, "SELECT id FROM emblem_patterns", [], |r| {
            data.patterns.insert(r.u32("id")?);
            Ok(())
        })?;
        Ok(data)
    }

    /// Whether `template_id` may receive a UCC imprint. An empty table is permissive.
    pub fn accepts_imprint(&self, template_id: u32) -> bool {
        self.applicable_items.is_empty() || self.applicable_items.contains(&template_id)
    }

    /// Whether `pattern_id` is an `emblem_patterns.id`; 0 ("no pattern") always is. An empty
    /// table is permissive.
    pub fn pattern_exists(&self, pattern_id: u32) -> bool {
        pattern_id == 0 || self.patterns.is_empty() || self.patterns.contains(&pattern_id)
    }

    /// Number of `ucc_applicables` kind-2 rows (tests / real-data check).
    pub fn applicable_item_count(&self) -> usize {
        self.applicable_items.len()
    }

    /// Number of `emblem_patterns` rows (tests / real-data check).
    pub fn pattern_count(&self) -> usize {
        self.patterns.len()
    }

    /// Loader / test helper: one `ucc_applicables` kind-2 row.
    #[cfg(test)]
    pub fn insert_applicable_item(&mut self, template_id: u32) {
        self.applicable_items.insert(template_id);
    }

    /// Loader / test helper: one `emblem_patterns` row.
    #[cfg(test)]
    pub fn insert_pattern(&mut self, pattern_id: u32) {
        self.patterns.insert(pattern_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE ucc_applicables(id INT, name TEXT, kind_id INT, actual_id INT,
                tooltip_msg TEXT);
             CREATE TABLE emblem_patterns(id INT, name TEXT, kind_id INT, path TEXT,
                icon_path TEXT);
             INSERT INTO ucc_applicables VALUES
                (1, 'cart', 1, 108, ''), (2, 'shirt', 2, 17653, ''),
                (3, 'shirt2', 2, 17654, ''), (4, 'flag', 3, 7216, '');
             INSERT INTO emblem_patterns VALUES
                (3, '01', 4, 'logo', ''), (8, '01', 3, 'pattern', '');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn ucc_tables_load_the_item_kind_and_every_pattern() {
        let data = UccData::load(&fixture()).unwrap();
        assert_eq!(data.applicable_item_count(), 2);
        assert!(data.accepts_imprint(17653));
        assert!(data.accepts_imprint(17654));
        // Kind 1 (slave) and kind 3 (flag doodad) rows are not item rules.
        assert!(!data.accepts_imprint(108));
        assert!(!data.accepts_imprint(7216));
        assert_eq!(data.pattern_count(), 2);
        assert!(data.pattern_exists(3));
        assert!(data.pattern_exists(8));
        assert!(data.pattern_exists(0));
        assert!(!data.pattern_exists(99));
    }

    #[test]
    fn empty_tables_keep_the_pre_wp_u2_behaviour() {
        let data = UccData::default();
        assert!(data.accepts_imprint(17653));
        assert!(data.pattern_exists(99));
    }

    /// The shipped 1.2 table: 128 `ucc_applicables` rows (80 kind 2) and 57 patterns.
    #[test]
    #[ignore = "needs AAEMU_COMPACT_DB"]
    fn real_compact_ucc_tables() {
        let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
            return;
        };
        let conn =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let data = UccData::load(&conn).unwrap();
        assert_eq!(data.applicable_item_count(), 80);
        assert_eq!(data.pattern_count(), 57);
        // A shipped kind-2 row (`ucc_applicables` id 8, cloth item 17653).
        assert!(data.accepts_imprint(17653));
        assert!(data.pattern_exists(3));
    }
}
