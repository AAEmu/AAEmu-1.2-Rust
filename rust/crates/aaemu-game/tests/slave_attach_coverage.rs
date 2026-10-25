//! Native 1.2 attach-data regressions. Source nodes and SHA-256s:
//! `rust/tools/slave_attach_points.provenance.json`; generator/conventions:
//! `rust/tools/slave_attach_points.py`. No client bytes are fixtures here.
//!
//! `AAEMU_COMPACT_DB=<compact.sqlite3> cargo test -p aaemu-game --test slave_attach_coverage -- --ignored`

use std::collections::{BTreeMap, BTreeSet};

use aaemu_game::data::world_templates::WorldSpawnPosition;
use aaemu_game::systems::vehicles::data::parse_attach_points;
use rusqlite::{Connection, OpenFlags};

const REPAIRED: &[(u32, &[u8])] = &[
    (5, &[24, 35, 81]),
    (867, &[35]),
    (892, &[9]),
    (898, &[9]),
    (899, &[9]),
    (
        1024,
        &[
            9, 10, 11, 12, 13, 14, 15, 16, 17, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 35, 36,
            37, 38, 39, 46, 58, 59,
        ],
    ),
    (1287, &[2, 35, 36]),
    (1323, &[21, 36, 60, 61]),
    (1371, &[21, 36, 60, 61]),
    (
        1385,
        &[
            21, 22, 24, 25, 36, 37, 38, 39, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71,
            76,
        ],
    ),
    (1442, &[9, 10]),
    (1444, &[21, 36, 60, 61]),
    (1460, &[21, 36, 60, 61]),
    (1461, &[21, 36, 60, 61]),
    (1462, &[21, 36, 60, 61]),
];

fn shipped() -> BTreeMap<(u32, u8), WorldSpawnPosition> {
    let text = include_str!("../app/Data/slave_attach_points.json");
    let mut points = BTreeMap::new();
    for (model, entries) in parse_attach_points(text).expect("shipped attach JSON") {
        for (point, location) in entries {
            assert!(
                points.insert((model, point), location).is_none(),
                "duplicate attach point ({model}, {point})"
            );
        }
    }
    points
}

#[test]
fn recovered_native_attach_points_are_shipped() {
    let points = shipped();
    assert_eq!(REPAIRED.iter().map(|(_, p)| p.len()).sum::<usize>(), 87);
    for &(model, indexes) in REPAIRED {
        for &point in indexes {
            let p = points
                .get(&(model, point))
                .unwrap_or_else(|| panic!("missing recovered native attach ({model}, {point})"));
            assert_ne!(point, 0, "None is not an attachment helper");
            assert!([p.x, p.y, p.z, p.roll, p.pitch, p.yaw]
                .into_iter()
                .all(f32::is_finite));
            assert!(p.x != 0.0 || p.y != 0.0 || p.z != 0.0, "origin placeholder");
        }
    }
}

#[test]
fn native_attach_literal_transforms_preserve_units_axes_and_parent_space() {
    let points = shipped();
    // Literal native 1.2 bind poses, independently inspected in the CHRs/CGFs
    // (not a serializer roundtrip or a comparison to generated provenance).
    // Tank cannon0 includes Bip01/Pelvis/bone_yaw/pocenter/bone_rail already.
    for (model, point, expected) in [
        (892, 9, [0.804_689_94, -2.049_110_2, 2.161_2]),
        (1323, 21, [0.0, 0.870_579_96, 1.181_04]),
        (1323, 60, [-0.520_054_64, -2.167_765, 0.683_005_2]),
        (1442, 10, [-0.123_01, -3.090_71, 3.204_7]),
        // Direct prefab $helms overrides the embedded CHR's different $helms.
        (5, 35, [-0.399_810_8, 2.351_196_3, 15.238_708]),
        // Existing 1406 CGF-derived entry from a9dc41b8 remains intact. CHR's
        // heal_point0 differs from its sibling CGF; don't copy this into cars.
        (1406, 36, [0.671_114, -0.043_841, 1.030_234]),
    ] {
        let actual = points[&(model, point)];
        for (got, want) in [actual.x, actual.y, actual.z].into_iter().zip(expected) {
            assert!(
                (got - want).abs() < 0.000_01,
                "({model}, {point}): {got} != {want}"
            );
        }
    }
    let box0 = points[&(1323, 60)];
    for (got, degrees) in [box0.roll, box0.pitch, box0.yaw]
        .into_iter()
        .zip([-90.0_f32, -55.0, -90.0])
    {
        assert!((got - degrees.to_radians()).abs() < 0.000_01);
    }
    let ladder = points[&(5, 24)];
    assert!((ladder.roll - 7.895_808_f32.to_radians()).abs() < 0.000_01);
    assert!((ladder.yaw.abs() - std::f32::consts::PI).abs() < 0.000_01);
}

#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_compact_attach_bindings_and_equipment_have_only_documented_content_gaps() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut query = conn
        .prepare(
            "SELECT s.model_id, b.attach_point_id FROM slave_doodad_bindings b JOIN slaves s ON s.id=b.owner_id
             UNION SELECT s.model_id, b.attach_point_id FROM slave_bindings b JOIN slaves s ON s.id=b.owner_id WHERE b.owner_type='Slave'
             UNION SELECT s.model_id, b.attach_point_id FROM slave_healing_point_doodads b JOIN slaves s ON s.id=b.owner_id
             UNION SELECT s.model_id, b.attach_point_id FROM slave_equip_slots b JOIN slaves s ON s.id=b.slave_id",
        )
        .unwrap();
    let required: BTreeSet<(u32, u8)> = query
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        // Index0 is None, not a helper, in five of the audit's 27 models.
        .filter(|(_, point)| *point != 0)
        .collect();
    assert!(!required.is_empty());
    let points = shipped();
    let missing: BTreeSet<_> = required
        .into_iter()
        .filter(|key| !points.contains_key(key))
        .collect();
    // Absent native assets/prefabs or absent helpers. Keep these explicit and
    // open; notably the smallboat has NO cannon0/cannon1 for slave55's 9/10.
    let gaps: &[(u32, &[u8])] = &[
        (115, &[9, 10, 11, 12, 18]),
        (129, &[9, 10]),
        (130, &[9]),
        (
            512,
            &[
                2, 3, 9, 10, 11, 12, 13, 14, 15, 16, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29,
                35, 36, 37, 38, 39,
            ],
        ),
        (857, &[2]),
        (866, &[35]),
        (1402, &[21]),
        (1427, &[21, 36, 60, 61]),
    ];
    let expected: BTreeSet<_> = gaps
        .iter()
        .flat_map(|(model, indexes)| indexes.iter().map(move |point| (*model, *point)))
        .collect();
    assert_eq!(missing, expected, "unreviewed native attach content gaps");
    assert_eq!(missing.len(), 41);
}
