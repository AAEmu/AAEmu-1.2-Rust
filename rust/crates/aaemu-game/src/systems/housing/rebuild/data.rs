//! The 10.x housing rebuild ("upgrade") tables.
//!
//! `housings.housing_rebuilding_pack_id` -> `housing_rebuilding_packs` ->
//! `housing_rebuilding_pack_rebuildings` (ordered by `position`) -> `housing_rebuildings`
//! (`skill_id`, target `housing_id`, `labor_power`, `actability_group_id`, `change_point_desc`)
//! -> `housing_rebuilding_materials` (`item_id`, `count`).
//!
//! The vanilla 1.2 `compact.sqlite3` has none of these tables and no `housing_rebuilding_pack_id`
//! column; a data patch adds them to the server database. Every part is optional: a missing
//! table or column yields an empty rebuild set, never a load error.

use std::collections::{BTreeMap, HashMap};

use rusqlite::Connection;

use crate::data::sqlite::for_each_row;
use crate::data::GameDataError;

/// One material row of a rebuilding (`housing_rebuilding_materials`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuildMaterial {
    pub item_id: u32,
    pub count: i32,
}

/// `housing_rebuildings`: turning a house into the template `housing_id`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rebuilding {
    pub id: u32,
    pub name: String,
    pub skill_id: u32,
    /// The target housing template.
    pub housing_id: u32,
    pub labor_power: i32,
    pub actability_group_id: u32,
    pub change_point_desc: String,
    pub materials: Vec<RebuildMaterial>,
}

/// All rebuild tables.
#[derive(Debug, Clone, Default)]
pub struct RebuildData {
    /// `housing_rebuildings` by id.
    rebuildings: HashMap<u32, Rebuilding>,
    /// Pack id -> rebuilding ids in `position` order.
    packs: HashMap<u32, Vec<u32>>,
    /// Housing template id -> `housing_rebuilding_pack_id`.
    house_packs: HashMap<u32, u32>,
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
        [table],
        |_| Ok(()),
    )
    .is_ok()
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info(\"{table}\")")) else {
        return false;
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(1)) else {
        return false;
    };
    let names: Vec<String> = rows.flatten().collect();
    names.iter().any(|n| n.eq_ignore_ascii_case(column))
}

impl RebuildData {
    /// Loads whatever rebuild tables the database has. A malformed row of a present table is an
    /// error (like every other loader); an absent table or column is not.
    pub fn load(conn: &Connection, language: &str) -> Result<Self, GameDataError> {
        let mut d = Self::default();
        if column_exists(conn, "housings", "housing_rebuilding_pack_id") {
            for_each_row(
                conn,
                "SELECT id, housing_rebuilding_pack_id FROM housings \
                 WHERE housing_rebuilding_pack_id IS NOT NULL AND housing_rebuilding_pack_id <> 0",
                [],
                |r| {
                    d.house_packs
                        .insert(r.u32("id")?, r.u32("housing_rebuilding_pack_id")?);
                    Ok(())
                },
            )?;
        }
        if table_exists(conn, "housing_rebuildings") {
            for_each_row(conn, "SELECT * FROM housing_rebuildings", [], |r| {
                let b = Rebuilding {
                    id: r.u32("id")?,
                    name: r.string_or("name", "")?,
                    skill_id: r.u32_or("skill_id", 0)?,
                    housing_id: r.u32("housing_id")?,
                    labor_power: r.i32_or("labor_power", 0)?,
                    actability_group_id: if r.has("actability_group_id") {
                        r.u32_or("actability_group_id", 0)?
                    } else {
                        0
                    },
                    change_point_desc: if r.has("change_point_desc") {
                        r.string_or("change_point_desc", "")?
                    } else {
                        String::new()
                    },
                    materials: Vec::new(),
                };
                d.rebuildings.insert(b.id, b);
                Ok(())
            })?;
        }
        if table_exists(conn, "housing_rebuilding_materials") {
            for_each_row(
                conn,
                "SELECT * FROM housing_rebuilding_materials ORDER BY id",
                [],
                |r| {
                    let id = r.u32("housing_rebuilding_id")?;
                    let material = RebuildMaterial {
                        item_id: r.u32("item_id")?,
                        count: r.i32_or("count", 1)?,
                    };
                    match d.rebuildings.get_mut(&id) {
                        Some(b) => b.materials.push(material),
                        None => tracing::warn!(
                            "housing_rebuilding_materials: unknown rebuilding {id}, skipped"
                        ),
                    }
                    Ok(())
                },
            )?;
        }
        if table_exists(conn, "housing_rebuilding_pack_rebuildings") {
            let mut rows: BTreeMap<u32, Vec<(i64, u32, u32)>> = BTreeMap::new();
            for_each_row(
                conn,
                "SELECT * FROM housing_rebuilding_pack_rebuildings",
                [],
                |r| {
                    rows.entry(r.u32("housing_rebuilding_pack_id")?)
                        .or_default()
                        .push((
                            r.i64_or("position", 0)?,
                            r.u32("id")?,
                            r.u32("housing_rebuilding_id")?,
                        ));
                    Ok(())
                },
            )?;
            for (pack, mut list) in rows {
                list.sort();
                d.packs
                    .insert(pack, list.into_iter().map(|(_, _, id)| id).collect());
            }
        }
        d.load_localized(conn, language);
        if !d.rebuildings.is_empty() {
            tracing::info!(
                "Loaded {} housing rebuildings in {} packs for {} housing templates",
                d.rebuildings.len(),
                d.packs.len(),
                d.house_packs.len()
            );
        }
        Ok(d)
    }

    /// `localized_texts` of `housing_rebuildings.name` / `change_point_desc` (optional).
    fn load_localized(&mut self, conn: &Connection, language: &str) {
        if self.rebuildings.is_empty() || !table_exists(conn, "localized_texts") {
            return;
        }
        let col = language.replace('"', "");
        let sql = format!(
            "SELECT tbl_column_name, idx, \"{col}\" AS txt FROM localized_texts \
             WHERE tbl_name = 'housing_rebuildings'"
        );
        let res = for_each_row(conn, &sql, [], |r| {
            let text = r.string_or("txt", "")?;
            if text.is_empty() {
                return Ok(());
            }
            if let Some(b) = self.rebuildings.get_mut(&r.u32("idx")?) {
                match r.string("tbl_column_name")?.as_str() {
                    "name" => b.name = text,
                    "change_point_desc" => b.change_point_desc = text,
                    _ => {}
                }
            }
            Ok(())
        });
        if let Err(e) = res {
            tracing::debug!("housing rebuilding translations not loaded: {e}");
        }
    }

    /// True when no rebuilding is configured (vanilla 1.2 data).
    pub fn is_empty(&self) -> bool {
        self.rebuildings.is_empty()
    }

    pub fn rebuilding_count(&self) -> usize {
        self.rebuildings.len()
    }

    pub fn rebuilding(&self, id: u32) -> Option<&Rebuilding> {
        self.rebuildings.get(&id)
    }

    /// `housings.housing_rebuilding_pack_id` of a template.
    pub fn pack_of(&self, template_id: u32) -> Option<u32> {
        self.house_packs.get(&template_id).copied()
    }

    /// The rebuildings offered for houses of `template_id`, in pack order (rows whose
    /// `housing_rebuildings` entry is missing are skipped).
    pub fn options_for(&self, template_id: u32) -> Vec<&Rebuilding> {
        self.pack_of(template_id)
            .and_then(|p| self.packs.get(&p))
            .into_iter()
            .flatten()
            .filter_map(|id| self.rebuildings.get(id))
            .collect()
    }

    /// The rebuilding `id` when it is offered for `template_id`.
    pub fn option_for(&self, template_id: u32, id: u32) -> Option<&Rebuilding> {
        self.options_for(template_id)
            .into_iter()
            .find(|b| b.id == id)
    }

    /// Adds a pack mapping and its rebuildings (tests / synthetic data).
    pub fn add(&mut self, template_id: u32, pack_id: u32, rebuildings: Vec<Rebuilding>) {
        self.house_packs.insert(template_id, pack_id);
        let ids = self.packs.entry(pack_id).or_default();
        for b in rebuildings {
            if !ids.contains(&b.id) {
                ids.push(b.id);
            }
            self.rebuildings.insert(b.id, b);
        }
    }
}
