use super::*;
use rusqlite::Connection;

#[test]
fn armor_durability_formula() {
    let mut d = ItemData::default();
    d.config.durability_const = 1.0;
    d.grades.insert(
        0,
        GradeTemplate {
            grade: 0,
            grade_order: 0,
            durability: 1.0,
        },
    );
    let t = ItemTemplate::new(
        1,
        TemplateKind::Armor(EquipTemplate {
            coverage: 10,
            durability_ratio: 1.0,
            durability_multiplier: 100,
            ..EquipTemplate::default()
        }),
    );
    // (int)(1000 * 1000 * 1 * 1000 * 1e-8) = 10; *1 -> 10; round(10 * 100 * 0.01) = 10
    assert_eq!(d.max_durability(&t, 0), Some(10));
    assert_eq!(d.max_durability(&t, 1), None);
}

/// `items.grade_enchantable` is loaded into every template. The real 1.2 data has 4 564
/// `'t'` rows (all equipment) and 16 918 `'f'` rows.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only)"]
fn real_compact_db_loads_grade_enchantable() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let data = ItemData::load(&conn).unwrap();
    let mut stmt = conn
        .prepare("SELECT id, grade_enchantable FROM items")
        .unwrap();
    let mut rows = stmt.query([]).unwrap();
    let (mut on, mut off) = (0, 0);
    while let Some(row) = rows.next().unwrap() {
        let id: u32 = row.get(0).unwrap();
        let flag: String = row.get(1).unwrap();
        let expected = flag == "t";
        let t = data
            .template(id)
            .unwrap_or_else(|| panic!("item {id} has no template"));
        assert_eq!(t.grade_enchantable, expected, "item {id}");
        if expected {
            on += 1;
        } else {
            off += 1;
        }
    }
    assert_eq!((on, off), (4564, 16918));
    // A first-step leather cap is enchantable; NPC-only gear is not.
    assert!(data.template(6735).unwrap().grade_enchantable);
    assert!(!data.template(8559).unwrap().grade_enchantable);
}
