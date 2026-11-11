//! Sharpwind Mines (`instance_cuttingwind_deadmine`, zone 262) scripted-doodad placements:
//! the Nerta door and stone lion (spec item 5), the two powder-keg rock walls (6 and 9), the
//! stalactite corridor (10), the exit portal, the Okape chest and the attack-ban doodads (12).
//!
//! Placements come from the 10.x level editor (`doodad_rows_from_editor_level.json` of the
//! 2026-11-11 Sharpwind audit; **Inferred** for the 1.2 server, see `PORTING.md`); templates,
//! families and start phases come from the 1.2 `compact.sqlite3`. The two death-skill chests
//! 6130 / 6197 are spawned by their skills and must not be placed statically.

use crate::systems::npcs::doodad::data::DoodadData;
use crate::systems::npcs::doodad::phase;
use crate::systems::npcs::doodad::spawner::{parse_list, DoodadSpawnerDef};

type Row = (u32, u32, f32, f32, f32);

fn spawn_defs() -> Vec<DoodadSpawnerDef> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("app/Data/Worlds/instance_cuttingwind_deadmine/doodad_spawns.json");
    let text = std::fs::read_to_string(path).expect("doodad_spawns.json");
    parse_list(&text, DoodadSpawnerDef::from_json).expect("doodad_spawns.json parses")
}

/// Every scripted doodad of items 5, 6, 9, 10 and 12 (plus the existing keg sets reconciled
/// with the level) is placed exactly once at its authored position and family.
#[test]
fn sharpwind_scripted_doodad_spawn_placements() {
    let defs = spawn_defs();
    let rows: Vec<(Row, f32)> = defs
        .iter()
        .map(|d| {
            let p = &d.base.position;
            ((d.base.unit_id, d.family_id, p.x, p.y, p.z), d.scale)
        })
        .collect();

    // (template, family, x, y, z); family 0 where the level row has none.
    let mut expected: Vec<(Row, &str)> = vec![
        (
            (5058, 50_000, 679.0, 326.45, 166.9),
            "Derelict Bridge family",
        ),
        // Item 5: the locked door behind Nerta and its stone lion.
        (
            (5541, 50_009, 487.015, 327.783, 165.839),
            "Guild Storage Door",
        ),
        ((5542, 50_010, 489.476, 323.844, 167.294), "Stone Lion"),
        // Items 6 and 9: the two powder-keg rock walls.
        ((5280, 0, 519.932, 365.755, 169.029), "Rock (ogre corridor)"),
        (
            (5280, 0, 615.935, 469.958, 155.602),
            "Rock (exit, scale 1.3)",
        ),
        // Item 10: eight falling and four shattered stalactites, families 50011-50022.
        (
            (5365, 50_011, 619.57, 486.884, 143.004),
            "Falling Stalactite",
        ),
        (
            (5365, 50_012, 609.904, 485.285, 143.639),
            "Falling Stalactite",
        ),
        (
            (5365, 50_013, 615.338, 504.739, 142.163),
            "Falling Stalactite",
        ),
        (
            (5365, 50_014, 609.877, 504.739, 142.417),
            "Falling Stalactite",
        ),
        (
            (5365, 50_015, 608.196, 553.711, 143.686),
            "Falling Stalactite",
        ),
        (
            (5365, 50_016, 619.733, 553.711, 144.106),
            "Falling Stalactite",
        ),
        (
            (5365, 50_017, 615.56, 556.969, 144.651),
            "Falling Stalactite",
        ),
        (
            (5365, 50_018, 606.936, 556.969, 145.129),
            "Falling Stalactite",
        ),
        (
            (5364, 50_019, 608.075, 544.148, 124.334),
            "Shattered Stalactite",
        ),
        (
            (5364, 50_020, 608.853, 534.413, 124.334),
            "Shattered Stalactite",
        ),
        (
            (5364, 50_021, 618.977, 535.999, 122.93),
            "Shattered Stalactite",
        ),
        (
            (5364, 50_022, 615.082, 540.069, 121.76),
            "Shattered Stalactite",
        ),
    ];

    // Items 6 and 9: the keg sets (18 powder kegs, 4 dormant kegs, 5 explosives boxes).
    expected.extend([
        ((5282, 0, 435.011, 324.398, 167.243), "Powder Keg"),
        ((5282, 0, 435.459, 325.041, 167.243), "Powder Keg"),
        ((5282, 0, 435.827, 324.221, 167.243), "Powder Keg"),
        ((5282, 0, 436.201, 324.882, 167.243), "Powder Keg"),
        ((5282, 0, 436.618, 324.115, 167.243), "Powder Keg"),
        ((5282, 0, 436.955, 324.818, 167.243), "Powder Keg"),
        ((5282, 0, 437.388, 323.929, 167.243), "Powder Keg"),
        ((5282, 0, 437.647, 324.742, 167.243), "Powder Keg"),
        ((5282, 0, 438.166, 323.869, 167.243), "Powder Keg"),
        ((5282, 0, 445.668, 323.026, 167.238), "Powder Keg"),
        ((5282, 0, 445.785, 322.264, 167.238), "Powder Keg"),
        ((5282, 0, 446.304, 322.684, 167.238), "Powder Keg"),
        ((5282, 0, 620.929, 457.734, 149.594), "Powder Keg (exit)"),
        ((5282, 0, 620.929, 456.275, 149.778), "Powder Keg (exit)"),
        ((5282, 0, 620.929, 457.021, 149.678), "Powder Keg (exit)"),
        ((5282, 0, 620.962, 458.48, 149.488), "Powder Keg (exit)"),
        ((5282, 0, 620.962, 459.23, 149.344), "Powder Keg (exit)"),
        ((5282, 0, 620.962, 460.021, 149.335), "Powder Keg (exit)"),
        ((5606, 0, 462.074, 336.983, 168.051), "dormant Powder Keg"),
        ((5606, 0, 462.905, 337.897, 168.051), "dormant Powder Keg"),
        ((5606, 0, 462.961, 337.082, 168.051), "dormant Powder Keg"),
        ((5606, 0, 462.989, 336.205, 168.051), "dormant Powder Keg"),
        ((5568, 0, 441.561, 322.983, 167.619), "Explosives Box"),
        ((5568, 0, 444.202, 322.299, 167.619), "Explosives Box"),
        ((5568, 0, 465.236, 337.6, 168.414), "Explosives Box"),
        ((5568, 0, 622.661, 458.79, 149.942), "Explosives Box (exit)"),
        ((5568, 0, 622.68, 456.454, 150.156), "Explosives Box (exit)"),
    ]);

    // Item 12: the exit portal, the invisible Okape chest and the three attack-ban doodads.
    expected.extend([
        ((5724, 0, 613.364, 642.913, 139.813), "Sharpwind Mines Exit"),
        ((7522, 0, 614.247, 636.359, 139.486), "Okape's Secret Chest"),
        ((5957, 0, 602.285, 569.626, 135.686), "attack-ban area"),
        ((5957, 0, 608.286, 570.06, 135.026), "attack-ban area"),
        ((5957, 0, 615.62, 570.379, 134.464), "attack-ban area"),
    ]);

    for ((template, family, x, y, z), what) in expected {
        let matches: Vec<_> = rows
            .iter()
            .filter(|((t, f, rx, ry, rz), _)| {
                *t == template
                    && *f == family
                    && (rx - x).abs() < 0.001
                    && (ry - y).abs() < 0.001
                    && (rz - z).abs() < 0.001
            })
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "{what} {template} family {family} at ({x}, {y}, {z}): {} rows",
            matches.len()
        );
    }

    // The level's keg sets replace the old approximations: no extra 5282 / 5606 / 5568 row.
    for (template, count) in [(5282, 18), (5606, 4), (5568, 5), (5365, 8), (5364, 4)] {
        let placed = rows.iter().filter(|(r, _)| r.0 == template).count();
        assert_eq!(placed, count, "template {template} rows");
    }
    let walls = rows.iter().filter(|(r, _)| r.0 == 5280).count();
    assert_eq!(walls, 2, "Rock (5280) rows");
    let exit_wall = rows
        .iter()
        .find(|(r, _)| r.0 == 5280 && (r.2 - 615.935).abs() < 0.001)
        .expect("exit Rock");
    assert!(
        (exit_wall.1 - 1.3).abs() < 0.001,
        "exit Rock scale {}",
        exit_wall.1
    );

    // Item 5: the wrong-template Howling Abyss lion statue 5602 is gone.
    assert!(
        !rows.iter().any(|(r, _)| r.0 == 5602),
        "5602 must not stand in for the stone lion 5542"
    );
    // Items 4 and 12: the death skills spawn chests 6130 / 6197, never the spawn file.
    for template in [6130, 6197] {
        assert!(
            !rows.iter().any(|(r, _)| r.0 == template),
            "chest {template} is skill-spawned and must not be placed statically"
        );
    }
}

/// The added templates exist in the 1.2 `compact.sqlite3` and resolve to the authored start
/// phase (5541's and 5542's family wiring, 7522's invisible start group 20831, 5724's closed
/// exit, the stalactites' area-trigger starts).
///
/// `AAEMU_COMPACT_DB=... cargo test -p aaemu-game --lib
/// real_compact_db_sharpwind_scripted_doodad_spawn_templates -- --ignored`
#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only 1.2 compact.sqlite3)"]
fn real_compact_db_sharpwind_scripted_doodad_spawn_templates() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open compact.sqlite3");
    let data = DoodadData::load_from(&conn).expect("doodad data");

    for (template, start_group) in [
        (5058, 13_147),
        (5280, 13_665),
        (5282, 13_668),
        (5364, 13_883),
        (5365, 13_885),
        (5541, 14_240),
        (5542, 14_984),
        (5568, 14_347),
        (5606, 14_985),
        (5724, 14_673),
        (5957, 15_348),
        (7522, 20_831),
    ] {
        assert!(
            data.template(template).is_some(),
            "missing doodad template {template} in compact"
        );
        assert_eq!(
            phase::get_func_group_id(&data, template),
            start_group,
            "template {template} start phase"
        );
    }
}
