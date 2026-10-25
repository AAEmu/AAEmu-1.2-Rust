//! Native 1.2 attribute bounds (read-only compact data).
//!
//! `x2game12.c` 39577A20 (:1178985, :1179175-81) initializes unit limits to
//! +/-2,000,000,000 then loads `unit_attribute_limits`. 398A0BC0 (:1991333-38)
//! calls 395729F0 AFTER formula, equipment and buff arithmetic, before the raw
//! attribute is converted to a ratio, percentage, delay, etc. These are bounds on
//! the resulting raw attribute, never on each bonus or its accumulated modifier.
//!
//! `character_p_stat_limits` is separate: 39752FC0 (:1647826-32) loads max/min;
//! 3939DD10 (:765713-59) validates permanent pStat + delta. It does NOT bound final
//! primary attributes including equipment/buffs. Permanent pStats are not yet
//! modeled/persisted by this server; loading their five ranges does not implement
//! that feature or justify applying them to NPCs or to character stat getters.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::models::unit::UnitAttribute;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributeLimit {
    pub minimum: i32,
    pub maximum: i32,
}

impl Default for AttributeLimit {
    fn default() -> Self {
        Self {
            minimum: -2_000_000_000,
            maximum: 2_000_000_000,
        }
    }
}

impl AttributeLimit {
    /// Invalid numeric input cannot escape into casts/timers; infinities saturate
    /// and NaN uses zero within the authored range (server numeric-safety policy).
    pub fn clamp(self, value: f64) -> f64 {
        let value = if value.is_nan() { 0.0 } else { value };
        value.clamp(f64::from(self.minimum), f64::from(self.maximum))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitAttributeLimits {
    pub attributes: HashMap<UnitAttribute, AttributeLimit>,
}

fn invalid(message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(message.into())
}

fn limit(minimum: i32, maximum: i32) -> rusqlite::Result<AttributeLimit> {
    if minimum > maximum {
        return Err(invalid(format!(
            "inverted attribute limit {minimum}..{maximum}"
        )));
    }
    Ok(AttributeLimit { minimum, maximum })
}

impl UnitAttributeLimits {
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let mut result = Self::default();
        let mut stmt =
            conn.prepare("SELECT unit_attribute_id, minimum, maximum FROM unit_attribute_limits")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id: u8 = row.get(0)?;
            let attribute = UnitAttribute::from_u8(id)
                .ok_or_else(|| invalid(format!("unknown unit attribute limit {id}")))?;
            let range = limit(row.get(1)?, row.get(2)?)?;
            if result.attributes.insert(attribute, range).is_some() {
                return Err(invalid(format!("duplicate unit attribute limit {id}")));
            }
        }
        Ok(result)
    }

    pub fn clamp(&self, attribute: UnitAttribute, value: f64) -> f64 {
        self.attributes
            .get(&attribute)
            .copied()
            .unwrap_or_default()
            .clamp(value)
    }
}

/// Ranges for permanent character pStat contributions; not final stat caps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CharacterPStatLimits {
    pub contributions: HashMap<u8, AttributeLimit>,
}

impl CharacterPStatLimits {
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let mut result = Self::default();
        let mut stmt = conn.prepare("SELECT p_stat_id, max, min FROM character_p_stat_limits")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id: u8 = row.get(0)?;
            if !(1..=5).contains(&id) {
                return Err(invalid(format!("unknown character pStat limit {id}")));
            }
            let range = limit(row.get(2)?, row.get(1)?)?;
            if result.contributions.insert(id, range).is_some() {
                return Err(invalid(format!("duplicate character pStat limit {id}")));
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
#[path = "attribute_limits_tests.rs"]
pub(crate) mod tests;
