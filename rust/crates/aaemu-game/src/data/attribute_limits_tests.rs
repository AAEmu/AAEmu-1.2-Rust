use super::*;

// Native 1.2 compact rows, selected read-only from the table consumed by 39577A20.
// Deliberately literal fixtures; neither the production clamp nor a round trip
// generates expected values.
pub(crate) const UNIT_ROWS: [(u8, i32, i32); 21] = [
    (6, -2_000_000_000, 10_000_000),
    (8, 0, 1_000_000),
    (10, -10_000, 3_000),
    (16, 0, 2_000_000_000),
    (21, 0, 2_000_000_000),
    (22, 0, 2_000_000_000),
    (25, 0, 2_000_000_000),
    (30, 0, 2_000_000_000),
    (33, 0, 10_000_000),
    (34, 0, 10_000_000),
    (35, 0, 10_000_000),
    (51, -1_000, 5_000),
    (52, -1_000, 5_000),
    (53, -1_000, 5_000),
    (54, -800, 4_000),
    (55, -800, 4_000),
    (71, -600, 4_000),
    (74, -800, 4_000),
    (95, 0, 500),
    (119, -800, 4_000),
    (120, -800, 5_000),
];

pub(crate) fn fixture() -> (UnitAttributeLimits, CharacterPStatLimits) {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE unit_attribute_limits (unit_attribute_id INTEGER, minimum INTEGER, maximum INTEGER);
         CREATE TABLE character_p_stat_limits (p_stat_id INTEGER, min INTEGER, max INTEGER);",
    )
    .unwrap();
    for (id, min, max) in UNIT_ROWS {
        conn.execute(
            "INSERT INTO unit_attribute_limits VALUES (?1,?2,?3)",
            (id, min, max),
        )
        .unwrap();
    }
    for id in 1..=5 {
        conn.execute(
            "INSERT INTO character_p_stat_limits VALUES (?1,0,10000)",
            [id],
        )
        .unwrap();
    }
    (
        UnitAttributeLimits::load(&conn).unwrap(),
        CharacterPStatLimits::load(&conn).unwrap(),
    )
}

#[test]
fn attribute_limits_native_boundaries_and_finite_numeric_policy() {
    let (limits, primary) = fixture();
    assert_eq!(limits.attributes.len(), 21);
    for (id, min, max) in UNIT_ROWS {
        let attr = UnitAttribute::from_u8(id).unwrap();
        for (value, expected) in [
            (f64::from(min) - 1.0, f64::from(min)),
            (f64::from(min), f64::from(min)),
            (f64::from(min) + 0.5, f64::from(min) + 0.5),
            (f64::from(max) - 0.5, f64::from(max) - 0.5),
            (f64::from(max), f64::from(max)),
            (f64::from(max) + 1.0, f64::from(max)),
            (f64::INFINITY, f64::from(max)),
            (f64::NEG_INFINITY, f64::from(min)),
            (f64::NAN, 0.0),
        ] {
            assert_eq!(limits.clamp(attr, value), expected, "{attr:?}: {value}");
        }
    }
    // The five pStat ranges bound permanent contributions only, not these final
    // unit attributes. Fai has no character_p_stat_limits row at all.
    assert_eq!(primary.contributions.len(), 5);
    for id in 1..=5 {
        let range = primary.contributions[&id];
        assert_eq!(
            range,
            AttributeLimit {
                minimum: 0,
                maximum: 10_000
            }
        );
        for (input, expected) in [
            (-1.0, 0.0),
            (0.0, 0.0),
            (10_000.0, 10_000.0),
            (10_001.0, 10_000.0),
        ] {
            assert_eq!(range.clamp(input), expected);
        }
    }
    assert!(!primary.contributions.contains_key(&0));
    assert!(!primary.contributions.contains_key(&6));
    assert_eq!(limits.clamp(UnitAttribute::Str, 10_001.0), 10_001.0);
    assert_eq!(limits.clamp(UnitAttribute::Str, -1.0), -1.0);
    assert_eq!(
        limits.clamp(UnitAttribute::Fai, f64::INFINITY),
        2_000_000_000.0
    );
}

#[test]
fn attribute_limits_reject_malformed_ranges_ids_and_duplicates() {
    for (table, columns, values, primary) in [
        (
            "unit_attribute_limits",
            "unit_attribute_id, minimum, maximum",
            "8,10,0",
            false,
        ),
        (
            "unit_attribute_limits",
            "unit_attribute_id, minimum, maximum",
            "9,0,10",
            false,
        ),
        (
            "unit_attribute_limits",
            "unit_attribute_id, minimum, maximum",
            "256,0,10",
            false,
        ),
        (
            "unit_attribute_limits",
            "unit_attribute_id, minimum, maximum",
            "8,0,2147483648",
            false,
        ),
        (
            "unit_attribute_limits",
            "unit_attribute_id, minimum, maximum",
            "8,NULL,10",
            false,
        ),
        (
            "unit_attribute_limits",
            "unit_attribute_id, minimum, maximum",
            "8,0,10),(8,0,20",
            false,
        ),
        (
            "character_p_stat_limits",
            "p_stat_id, max, min",
            "1,0,10",
            true,
        ),
        (
            "character_p_stat_limits",
            "p_stat_id, max, min",
            "0,10,0",
            true,
        ),
        (
            "character_p_stat_limits",
            "p_stat_id, max, min",
            "6,10,0",
            true,
        ),
        (
            "character_p_stat_limits",
            "p_stat_id, max, min",
            "1,10,0),(1,20,0",
            true,
        ),
    ] {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE {table} ({columns}); INSERT INTO {table} VALUES ({values});"
        ))
        .unwrap();
        let failed = if primary {
            CharacterPStatLimits::load(&conn).is_err()
        } else {
            UnitAttributeLimits::load(&conn).is_err()
        };
        assert!(failed, "accepted {table} ({values})");
    }
}

#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only)"]
fn attribute_limits_real_compact_db_pins_all_21_and_5_rows() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let (unit, primary) = fixture();
    assert_eq!(UnitAttributeLimits::load(&conn).unwrap(), unit);
    assert_eq!(CharacterPStatLimits::load(&conn).unwrap(), primary);
}
