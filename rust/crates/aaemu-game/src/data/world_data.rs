//! Smaller game-data loaders used by the world / character code:
//!
//! * `LocalizationManager` (only the `system_factions.name` and `return_points.name` texts)
//! * `FactionManager.Load` (`system_factions`, `system_faction_relations`)
//! * `ZoneManager.Load` (`zones`, `zone_groups`, `conflict_zones`; banned tags and climates are
//!   later phases)
//! * `PortalManager` recall points (`Data/Portal/recalls.json`) and `district_return_points`

use std::collections::HashMap;
use std::path::Path;

use rusqlite::Connection;
use serde::Deserialize;

use super::sqlite::for_each_row;

/// `FactionsEnum` values referenced by the code.
pub mod factions {
    pub const INVALID: u32 = 0;
    pub const FRIENDLY: u32 = 1;
    pub const NEUTRAL: u32 = 2;
    pub const HOSTILE: u32 = 3;
    pub const NUIA_ALLIANCE: u32 = 148;
    pub const HARANYA_ALLIANCE: u32 = 149;
    pub const PIRATE: u32 = 161;
}

/// `LocalizationManager` lookups (`Get(tbl, column, idx, fallback)`).
#[derive(Debug, Clone, Default)]
pub struct Localization {
    texts: HashMap<(String, String, i64), String>,
}

impl Localization {
    /// Loads the rows of `localized_texts` this phase needs, using `DefaultLanguage`.
    pub fn load(conn: &Connection, language: &str) -> rusqlite::Result<Self> {
        let mut l = Localization::default();
        // The column name comes from the configuration; quote it as an identifier.
        let sql = format!(
            "SELECT tbl_name, tbl_column_name, idx, \"{}\" AS txt FROM localized_texts \
             WHERE tbl_name IN ('system_factions', 'return_points')",
            language.replace('"', "")
        );
        for_each_row(conn, &sql, [], |r| {
            let text = r.string_or("txt", "")?;
            l.texts.insert(
                (
                    r.string("tbl_name")?,
                    r.string("tbl_column_name")?,
                    r.i64("idx")?,
                ),
                text,
            );
            Ok(())
        })?;
        Ok(l)
    }

    /// `Get`: empty translations fall back too.
    pub fn get(&self, tbl: &str, column: &str, idx: i64, fallback: &str) -> String {
        match self.texts.get(&(tbl.to_string(), column.to_string(), idx)) {
            Some(t) if !t.is_empty() => t.clone(),
            _ => fallback.to_string(),
        }
    }
}

/// `SystemFaction` (as loaded from `system_factions`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemFaction {
    pub id: u32,
    pub name: String,
    pub owner_id: u32,
    pub owner_name: String,
    pub mother_id: u32,
    pub unit_owner_type: i8,
    pub political_system: u8,
    pub diplomacy_target: bool,
    /// `diplomacy_link_id`: the alliance whose relation a nation guard / front faction follows
    /// (134 -> 149, 135 -> 148, 167 -> 148, ...; stage 3, NATION-16).
    pub diplomacy_link_id: u32,
    pub aggro_link: bool,
    pub guard_help: bool,
    pub allow_change_name: u8,
    /// `Created` (unix seconds; never set for system factions -> 0).
    pub created: i64,
}

/// `FactionRelation`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FactionRelation {
    pub id: u32,
    pub id2: u32,
    pub state: u8,
    /// `ExpTime` (never set -> `DateTime.MinValue` -> 0).
    pub exp_time: i64,
}

#[derive(Debug, Clone, Default)]
pub struct FactionData {
    /// `_systemFactions.Values` in insertion order.
    pub factions: Vec<SystemFaction>,
    index: HashMap<u32, usize>,
    pub relations: Vec<FactionRelation>,
}

impl FactionData {
    pub fn get(&self, id: u32) -> Option<&SystemFaction> {
        self.index.get(&id).map(|&i| &self.factions[i])
    }

    pub fn add(&mut self, f: SystemFaction) {
        self.index.insert(f.id, self.factions.len());
        self.factions.push(f);
    }

    /// `FactionManager.Load`.
    pub fn load(conn: &Connection, loc: &Localization) -> rusqlite::Result<Self> {
        let mut d = FactionData::default();
        let mut tmp = Vec::new();
        for_each_row(conn, "SELECT * FROM system_factions", [], |r| {
            let id = r.u32("id")?;
            tmp.push(SystemFaction {
                id,
                name: loc.get("system_factions", "name", i64::from(id), ""),
                owner_name: r.string("owner_name")?,
                unit_owner_type: r.i16("owner_type_id")? as i8,
                owner_id: r.u32("owner_id")?,
                political_system: r.u8("political_system_id")?,
                mother_id: r.u32("mother_id")?,
                aggro_link: r.bool_str("aggro_link")?,
                guard_help: r.bool_str("guard_help")?,
                diplomacy_target: r.bool_str("is_diplomacy_tgt")?,
                diplomacy_link_id: r.u32("diplomacy_link_id")?,
                allow_change_name: 0,
                created: 0,
            });
            Ok(())
        })?;
        for f in tmp {
            if d.index.contains_key(&f.id) {
                return Err(dup("system faction", f.id));
            }
            d.add(f);
        }
        tracing::info!("Loaded {} system factions", d.factions.len());
        for_each_row(conn, "SELECT * FROM system_faction_relations", [], |r| {
            d.relations.push(FactionRelation {
                id: r.u32("faction1_id")?,
                id2: r.u32("faction2_id")?,
                state: r.u8("state_id")?,
                exp_time: 0,
            });
            Ok(())
        })?;
        // `_systemFactions[relation.Id]` throws for unknown factions.
        for rel in &d.relations {
            if d.get(rel.id).is_none() || d.get(rel.id2).is_none() {
                return Err(dup("faction relation to unknown faction", rel.id));
            }
        }
        tracing::info!("Loaded {} faction relations", d.relations.len());
        Ok(d)
    }
}

fn dup(what: &str, id: u32) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(format!("{what} {id}"))
}

/// `Zone`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Zone {
    pub id: u32,
    pub name: String,
    pub zone_key: u32,
    pub group_id: u32,
    pub closed: bool,
    pub faction_id: u32,
    pub zone_climate_id: u32,
}

/// `ZoneGroup` (subset).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ZoneGroup {
    pub id: u32,
    pub name: String,
    pub target_id: u32,
    pub faction_chat_region_id: u32,
    pub pirate_desperado: bool,
}

/// `ZoneConflictType`.
pub mod conflict_type {
    pub const TENSION: u8 = 0;
    pub const DANGER: u8 = 1;
    pub const DISPUTE: u8 = 2;
    pub const UNREST: u8 = 3;
    pub const CRISIS: u8 = 4;
    pub const CONFLICT: u8 = 5;
    pub const WAR: u8 = 6;
    pub const PEACE: u8 = 7;
}

/// `ZoneConflict` (static data + current state).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ZoneConflict {
    pub zone_group_id: u16,
    pub num_kills: [i32; 5],
    pub no_kill_min: [i32; 5],
    pub conflict_min: i32,
    pub war_min: i32,
    pub peace_min: i32,
    pub peace_protected_faction_id: u32,
    pub nuia_return_point_id: u32,
    pub harihara_return_point_id: u32,
    pub war_tower_def_id: u32,
    pub closed: bool,
    pub current_state: u8,
    /// `NextStateTime` (unix seconds, 0 = `DateTime.MinValue`).
    pub next_state_time: i64,
    /// `KillCount`.
    pub kill_count: u32,
}

#[derive(Debug, Clone, Default)]
pub struct ZoneData {
    pub zones: HashMap<u32, Zone>,
    pub zone_id_to_key: HashMap<u32, u32>,
    pub groups: HashMap<u32, ZoneGroup>,
    /// `_conflicts.Values` (insertion order).
    pub conflicts: Vec<ZoneConflict>,
}

impl ZoneData {
    /// `GetZoneByKey`.
    pub fn zone_by_key(&self, key: u32) -> Option<&Zone> {
        self.zones.get(&key)
    }

    pub fn group(&self, id: u32) -> Option<&ZoneGroup> {
        self.groups.get(&id)
    }

    /// `ZoneManager.Load`. Open conflict zones start at `Tension` with no kill count and no
    /// timer, exactly like the state the cycle returns to after `Peace` (`SetState(Tension)`
    /// clears `NextStateTime`); closed zones keep the same values.
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let mut d = ZoneData::default();
        for_each_row(conn, "SELECT * FROM zones", [], |r| {
            let z = Zone {
                id: r.u32("id")?,
                name: r.string("name")?,
                zone_key: r.u32("zone_key")?,
                group_id: r.u32_or("group_id", 0)?,
                closed: r.bool_str("closed")?,
                faction_id: r.u32_or("faction_id", 0)?,
                zone_climate_id: r.u32_or("zone_climate_id", 0)?,
            };
            if d.zone_id_to_key.insert(z.id, z.zone_key).is_some() {
                return Err(dup("zone id", z.id));
            }
            if d.zones.insert(z.zone_key, z.clone()).is_some() {
                return Err(dup("zone key", z.zone_key));
            }
            Ok(())
        })?;
        tracing::info!("Loaded {} zones", d.zones.len());
        for_each_row(conn, "SELECT * FROM zone_groups", [], |r| {
            let g = ZoneGroup {
                id: r.u32("id")?,
                name: r.string("name")?,
                target_id: r.u32("target_id")?,
                faction_chat_region_id: r.u32("faction_chat_region_id")?,
                pirate_desperado: r.bool_str("pirate_desperado")?,
            };
            if d.groups.insert(g.id, g.clone()).is_some() {
                return Err(dup("zone group", g.id));
            }
            Ok(())
        })?;
        tracing::info!("Loaded {} groups", d.groups.len());
        for_each_row(conn, "SELECT * FROM conflict_zones", [], |r| {
            let gid = r.i16("zone_group_id")? as u16;
            if !d.groups.contains_key(&u32::from(gid)) {
                tracing::warn!("ZoneGroupId: {gid} doesn't exist for conflict");
                return Ok(());
            }
            let mut c = ZoneConflict {
                zone_group_id: gid,
                conflict_min: r.i32("conflict_min")?,
                war_min: r.i32("war_min")?,
                peace_min: r.i32("peace_min")?,
                peace_protected_faction_id: r.u32_or("peace_protected_faction_id", 0)?,
                nuia_return_point_id: r.u32_or("nuia_return_point_id", 0)?,
                harihara_return_point_id: r.u32_or("harihara_return_point_id", 0)?,
                war_tower_def_id: r.u32_or("war_tower_def_id", 0)?,
                // GetBoolean("closed", true)
                closed: r.is_null("closed")? || r.bool_str("closed")?,
                // A server start begins the cycle at Tension (owner recollection 2026-11-10:
                // after maintenance every zone was back at danger stage 1 with 0 kills).
                current_state: conflict_type::TENSION,
                ..ZoneConflict::default()
            };
            for i in 0..5 {
                c.num_kills[i] = r.i32(&format!("num_kills_{i}"))?;
                c.no_kill_min[i] = r.i32(&format!("no_kill_min_{i}"))?;
            }
            if d.conflicts.iter().any(|x| x.zone_group_id == gid) {
                return Err(dup("conflict zone", u32::from(gid)));
            }
            d.conflicts.push(c);
            Ok(())
        })?;
        Ok(d)
    }
}

/// `Models.Game.Portal` (recall / private portals).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Portal {
    pub id: u32,
    pub name: String,
    pub zone_id: u32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    #[serde(rename = "ZRot")]
    pub z_rot: f32,
    pub yaw: f32,
    pub sub_zone_id: u32,
    pub owner: u32,
    pub world_id: u32,
}

/// `DistrictReturnPoints`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DistrictReturnPoint {
    pub id: u32,
    pub district_id: u32,
    pub faction_id: u32,
    pub return_point_id: u32,
}

/// The recall half of `PortalManager`.
#[derive(Debug, Clone, Default)]
pub struct PortalData {
    /// `_recalls` (sub zone -> portals in file order).
    pub recalls: HashMap<u32, Vec<Portal>>,
    /// `_districtReturnPoints.Values` (insertion order).
    pub district_return_points: Vec<DistrictReturnPoint>,
}

impl PortalData {
    /// `GetRecallBySubZoneId`.
    pub fn recall_by_sub_zone(&self, sub_zone: u32) -> Option<&[Portal]> {
        self.recalls.get(&sub_zone).map(Vec::as_slice)
    }

    /// `GetDistrictReturnPoint(districtId, factionId)` (0 when none).
    pub fn district_return_point(&self, district_id: u32, faction_id: u32) -> u32 {
        self.district_return_points
            .iter()
            .find(|p| p.district_id == district_id && p.faction_id == faction_id)
            .map_or(0, |p| p.return_point_id)
    }

    /// `PortalManager.Load` (recalls.json + district_return_points). As in C#, a missing or empty
    /// recalls.json is fatal.
    pub fn load(
        conn: &Connection,
        app_path: &Path,
        loc: &Localization,
    ) -> Result<Self, super::GameDataError> {
        let mut d = PortalData::default();
        let path = app_path.join("Data").join("Portal").join("recalls.json");
        let text = std::fs::read_to_string(&path).map_err(|e| {
            super::GameDataError::File(format!("File {} doesn't exists ! ({e})", path.display()))
        })?;
        if text.trim().is_empty() {
            return Err(super::GameDataError::File(format!(
                "File {} is empty !",
                path.display()
            )));
        }
        let recalls: Vec<Portal> = parse_lenient_json(&text).map_err(|e| {
            super::GameDataError::File(format!(
                "PortalManager: Parse {} file ({e})",
                path.display()
            ))
        })?;
        for mut recall in recalls {
            recall.name = loc.get("return_points", "name", i64::from(recall.id), &recall.name);
            d.recalls
                .entry(recall.sub_zone_id)
                .or_default()
                .push(recall);
        }
        tracing::info!("Loaded {} Recall Portals", d.recalls.len());
        for_each_row(conn, "SELECT * FROM district_return_points", [], |r| {
            let p = DistrictReturnPoint {
                id: r.u32("id")?,
                district_id: r.u32("district_id")?,
                faction_id: r.u32("faction_id")?,
                return_point_id: r.u32("return_point_id")?,
            };
            if !d.district_return_points.iter().any(|x| x.id == p.id) {
                d.district_return_points.push(p);
            }
            Ok(())
        })?;
        Ok(d)
    }
}

/// Newtonsoft-style JSON parsing: comments and trailing commas allowed, property names matched
/// case-insensitively (keys are normalised to the PascalCase spelling of the first letter).
pub fn parse_lenient_json<T: serde::de::DeserializeOwned>(text: &str) -> serde_json::Result<T> {
    let v: serde_json::Value = serde_json::from_str(&crate::config::relax_json(
        text.trim_start_matches('\u{feff}'),
    ))?;
    serde_json::from_value(pascal_keys(v))
}

fn pascal_keys(v: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| {
                    let mut c = k.chars();
                    let k = match c.next() {
                        Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                        None => k,
                    };
                    (k, pascal_keys(v))
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(pascal_keys).collect()),
        o => o,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// `zones` / `zone_groups` / `conflict_zones` rows: group 7 is open (Tension thresholds
    /// 2/4/6/8/10, as the combat tests use) and group 8 is closed.
    pub(crate) fn conflict_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE zones (id INT, name TEXT, zone_key INT, group_id INT, closed TEXT,
                                 faction_id INT, zone_climate_id INT);
             CREATE TABLE zone_groups (id INT, name TEXT, target_id INT,
                                       faction_chat_region_id INT, pirate_desperado TEXT);
             CREATE TABLE conflict_zones (
                 zone_group_id INT,
                 num_kills_0 INT, num_kills_1 INT, num_kills_2 INT, num_kills_3 INT, num_kills_4 INT,
                 no_kill_min_0 INT, no_kill_min_1 INT, no_kill_min_2 INT, no_kill_min_3 INT, no_kill_min_4 INT,
                 conflict_min INT, war_min INT, peace_min INT,
                 peace_protected_faction_id INT, nuia_return_point_id INT,
                 harihara_return_point_id INT, war_tower_def_id INT, closed TEXT);
             INSERT INTO zones VALUES (1, 'Open', 100, 7, 0, 0, 0), (2, 'Closed', 101, 8, 1, 0, 0);
             INSERT INTO zone_groups VALUES (7, 'Open', 0, 0, 0), (8, 'Closed', 0, 0, 0);
             INSERT INTO conflict_zones VALUES
                 (7, 2, 4, 6, 8, 10, 30, 30, 30, 30, 30, 5, 80, 120, 0, 0, 0, 18, 0),
                 (8, 1, 2, 3, 4, 5, 30, 30, 30, 30, 30, 5, 80, 120, 0, 0, 0, 0, 1);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn open_conflict_zones_load_at_tension_and_closed_zones_are_unchanged() {
        let d = ZoneData::load(&conflict_fixture()).unwrap();
        let open = d.conflicts.iter().find(|c| c.zone_group_id == 7).unwrap();
        assert_eq!(
            (open.current_state, open.kill_count, open.next_state_time),
            (conflict_type::TENSION, 0, 0)
        );
        assert_eq!(open.num_kills, [2, 4, 6, 8, 10]);
        let closed = d.conflicts.iter().find(|c| c.zone_group_id == 8).unwrap();
        assert!(closed.closed);
        assert_eq!(
            (
                closed.current_state,
                closed.kill_count,
                closed.next_state_time
            ),
            (conflict_type::TENSION, 0, 0)
        );
    }

    #[test]
    fn loaded_open_zone_climbs_to_danger_with_num_kills() {
        use crate::systems::social::zone_conflict::ZoneConflictManager;

        let d = ZoneData::load(&conflict_fixture()).unwrap();
        let conflicts = ZoneConflictManager::default();
        // Tension has no NextStateTime, so the load schedules no timer for it.
        assert!(conflicts.init(&d.conflicts, 1_000_000_000_000).is_empty());
        // 2 kills are at the Tension threshold, not above it.
        assert!(conflicts
            .add_zone_kill_at(7, 2, 1_000_000_000_000)
            .is_none());
        let s = conflicts
            .add_zone_kill_at(7, 1, 1_000_000_000_000)
            .expect("the third kill reaches Danger");
        assert_eq!(s.state, conflict_type::DANGER);
        let c = conflicts.get(7).unwrap();
        assert_eq!(
            (c.current_state, c.kill_count, c.next_state_time),
            (conflict_type::DANGER, 3, 0)
        );
    }

    /// The shipped 1.2 data: a fresh load is the start of the cycle for every conflict zone.
    #[test]
    #[ignore = "needs AAEMU_COMPACT_DB (read-only)"]
    fn real_compact_db_conflict_zones_start_at_tension() {
        let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
            return;
        };
        let conn =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let d = ZoneData::load(&conn).unwrap();
        assert!(!d.conflicts.is_empty());
        for c in &d.conflicts {
            assert_eq!(
                (c.current_state, c.kill_count, c.next_state_time),
                (conflict_type::TENSION, 0, 0),
                "zone group {}",
                c.zone_group_id
            );
        }
    }
}
