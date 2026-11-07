//! `content_configs(id, kind_id, value)`: the client's `LoadContentConfigValueArray` values.
//! They are a single global table keyed by **id** (not kind), shared by the housing, nation,
//! ability and item rules; the client only reads 22 of the 53 ids (`FUN_395720e0(id)`).
//!
//! The point lookup is the old `skills::data::misc::load_content_config` (re-exported there); the
//! map is loaded once into [`crate::data::GameData::content_configs`].

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension};

/// `content_configs.value` for `id` (`LoadContentConfigValueArray`). The client's value array is
/// zero-initialised, so an absent row reads as 0.
pub fn load_content_config(conn: &Connection, id: u32) -> rusqlite::Result<i32> {
    conn.query_row(
        "SELECT value FROM content_configs WHERE id = ?1",
        [id],
        |r| r.get::<_, i32>(0),
    )
    .optional()
    .map(|v| v.unwrap_or(0))
}

/// Every `content_configs` row, by id (the client's key).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContentConfigs {
    values: HashMap<u32, i64>,
}

impl ContentConfigs {
    /// `SELECT id, value FROM content_configs` (the client loads the same two columns).
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let mut contents = Self::default();
        let mut stmt = conn.prepare("SELECT id, value FROM content_configs")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (id, value) = row?;
            // Skip an id outside the client's u32 key space instead of failing the table.
            if let Ok(id) = u32::try_from(id) {
                contents.values.insert(id, value);
            } else {
                tracing::warn!("content_configs: skipping out-of-range id {id}");
            }
        }
        Ok(contents)
    }

    /// `content_configs.value` for `id` (`None` when the row is missing; the client reads 0).
    pub fn content_config(&self, id: u32) -> Option<i64> {
        self.values.get(&id).copied()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Loader / test helper: one row.
    pub fn insert(&mut self, id: u32, value: i64) -> Option<i64> {
        self.values.insert(id, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE content_configs(id INT, kind_id INT, value INT);
             INSERT INTO content_configs VALUES (37,11,1000000),(38,12,1000),(57,11,28085);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn content_config_reads_by_id_and_defaults_to_zero() {
        let conn = fixture();
        assert_eq!(load_content_config(&conn, 37).unwrap(), 1_000_000);
        assert_eq!(load_content_config(&conn, 39).unwrap(), 0);
        assert_eq!(load_content_config(&conn, 0).unwrap(), 0);
    }

    #[test]
    fn content_configs_load_every_row_by_id() {
        let contents = ContentConfigs::load(&fixture()).unwrap();
        assert_eq!(contents.len(), 3);
        assert_eq!(contents.content_config(37), Some(1_000_000));
        assert_eq!(contents.content_config(38), Some(1_000));
        assert_eq!(contents.content_config(57), Some(28_085));
        assert_eq!(contents.content_config(39), None);
    }

    /// The shipped 1.2 table: 53 rows, ids 37 / 57 hold the housing constants.
    #[test]
    #[ignore = "needs AAEMU_COMPACT_DB"]
    fn real_content_configs_load_every_row_by_id() {
        let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
            return;
        };
        let conn =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let contents = ContentConfigs::load(&conn).unwrap();
        assert_eq!(contents.len(), 53);
        assert_eq!(contents.content_config(37), Some(1_000_000));
        assert_eq!(contents.content_config(57), Some(28_085));
        // Loaded but UNKNOWN (RE-2): behaviour must not read them yet.
        assert_eq!(contents.content_config(38), Some(1_000));
        assert_eq!(contents.content_config(39), Some(200));
    }
}
