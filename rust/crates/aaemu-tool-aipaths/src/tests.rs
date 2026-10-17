use super::*;
use crate::convert::{
    format_path, parse_npc_xml, parse_world_origins, plan, route_distance, to_world, Candidate,
    PlanOptions, RotationConvention, WorldOrigins,
};

const XML: &str = r#"<ZoneNPCData>
 <aiPathList>
  <aiPath Type="AIPath" Name="Quest_Route" Pos="100,200,50" Rec._PathType="0" Closed="0">
   <Points>
    <Point Pos="0,0,0"/>
    <Point Pos="10,0,1"/>
    <Point Pos="10,5,2"/>
   </Points>
  </aiPath>
  <aiPath Type="AIPath" Name="Turned" Pos="100,200,50" Rotate="0.70710678,0,0,0.70710678" Scale="2,2,1" Rec._PathType="1">
   <Points>
    <Point Pos="0,0,0"/>
    <Point Pos="10,0,1"/>
   </Points>
  </aiPath>
  <aiPath Type="AIPath" Name="AIPath_5_npc_1" Pos="1.2,-0.4,0.1">
   <Points>
    <Point Pos="0,0,0"/>
    <Point Pos="3,0,0"/>
   </Points>
  </aiPath>
  <aiPath Type="AIPath" Name="Broken" Pos="x,y">
   <Points/>
  </aiPath>
 </aiPathList>
 <roamingAreaList/>
</ZoneNPCData>"#;

fn entities() -> Vec<crate::convert::AiPathEntity> {
    parse_npc_xml(XML).unwrap().entities
}

fn close(a: &[f64; 3], b: [f64; 3]) {
    for i in 0..3 {
        assert!((a[i] - b[i]).abs() < 1e-3, "{a:?} != {b:?}");
    }
}

#[test]
fn parses_entities_and_flags_the_malformed_one() {
    let parsed = parse_npc_xml(XML).unwrap();
    assert_eq!(parsed.entities.len(), 3);
    assert_eq!(parsed.malformed, vec!["Broken".to_string()]);
    let e = &parsed.entities[1];
    assert_eq!(e.name, "Turned");
    assert_eq!(e.scale, [2.0, 2.0, 1.0]);
    assert_eq!(e.path_type, "1");
    assert!(e.is_rotated());
    assert!(!parsed.entities[0].is_rotated());
    assert!(parsed.entities[2].is_spawner_relative());
    assert!(!parsed.entities[0].is_spawner_relative());
}

#[test]
fn transform_adds_origin_and_entity_position() {
    let e = &entities()[0];
    let p = to_world(e, (3, 2), RotationConvention::Direct);
    close(&p[0], [3.0 * 1024.0 + 100.0, 2.0 * 1024.0 + 200.0, 50.0]);
    close(&p[2], [3.0 * 1024.0 + 110.0, 2.0 * 1024.0 + 205.0, 52.0]);
}

#[test]
fn transform_scales_then_rotates_about_z() {
    let e = &entities()[1];
    // (10, 0, 1) scaled to (20, 0, 1), turned 90 degrees to (0, 20, 1).
    let direct = to_world(e, (0, 0), RotationConvention::Direct);
    close(&direct[1], [100.0, 220.0, 51.0]);
    let inverse = to_world(e, (0, 0), RotationConvention::Inverse);
    close(&inverse[1], [100.0, 180.0, 51.0]);
}

#[test]
fn format_is_fixed_and_has_no_negative_zero() {
    let text = format_path(&[[1.0, -0.0001, 2.5], [14165.04, 14970.5, 146.2]]);
    assert_eq!(text, "|1.000|0.000|2.500|\n|14165.040|14970.500|146.200|\n");
    assert_eq!(
        crate::convert::parse_path_points(&text),
        vec![[1.0, 0.0, 2.5], [14165.04, 14970.5, 146.2]]
    );
}

#[test]
fn world_origins_are_read_per_zone() {
    let xml = r#"<World><ZoneList><Zone name="a" id="142" originX="12" originY="13"/>
        <Zone name="b" id="9" originX="1" originY="2"/></ZoneList></World>"#;
    let o = parse_world_origins(xml).unwrap();
    assert_eq!(o[&142], (12, 13));
    assert_eq!(o[&9], (1, 2));
}

fn candidate(world: &str, zone: u32, name: &str, pos: [f64; 3]) -> Candidate {
    let mut e = entities()[0].clone();
    e.name = name.to_string();
    e.pos = pos;
    Candidate {
        world: world.to_string(),
        zone,
        entity: e,
    }
}

fn origins(entries: &[(&str, u32, (i32, i32))]) -> WorldOrigins {
    let mut o = WorldOrigins::new();
    for (w, z, v) in entries {
        o.entry((*w).to_string()).or_default().insert(*z, *v);
    }
    o
}

fn demand(names: &[&str]) -> BTreeMap<String, BTreeSet<String>> {
    let mut d: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for n in names {
        d.entry(n.to_lowercase())
            .or_default()
            .insert((*n).to_string());
    }
    d
}

const OPTS: PlanOptions = PlanOptions {
    convention: RotationConvention::Direct,
    skip_rotated: false,
};

#[test]
fn unique_names_are_plain_and_duplicates_become_zone_variants() {
    let mut c: HashMap<String, Vec<Candidate>> = HashMap::new();
    c.insert(
        "tuto_move".into(),
        vec![
            candidate("main_world", 184, "tuto_move", [100.0, 200.0, 5.0]),
            candidate("main_world", 129, "tuto_move", [300.0, 400.0, 5.0]),
        ],
    );
    // The same route in a normal and a hard instance collapses to one file.
    c.insert(
        "boss_walk".into(),
        vec![
            candidate("inst", 264, "boss_walk", [110.0, 220.0, 5.0]),
            candidate("inst_hard", 320, "boss_walk", [110.0, 220.0, 5.0]),
        ],
    );
    let o = origins(&[
        ("main_world", 184, (1, 1)),
        ("main_world", 129, (2, 2)),
        ("inst", 264, (0, 0)),
        ("inst_hard", 320, (0, 0)),
    ]);
    let p = plan(
        &demand(&["Tuto_Move", "boss_walk", "nothing_here"]),
        &c,
        &o,
        &o,
        &BTreeSet::new(),
        OPTS,
    );
    let names: Vec<&str> = p.files.keys().map(String::as_str).collect();
    assert_eq!(names, vec!["Tuto_Move@129", "Tuto_Move@184", "boss_walk"]);
    assert_eq!(p.report.unique_files, 1);
    assert_eq!(p.report.variant_files, 2);
    assert!(p.report.missing_in_10x.contains("nothing_here"));
}

#[test]
fn existing_files_are_kept_and_unusable_zones_are_reported() {
    let mut c: HashMap<String, Vec<Candidate>> = HashMap::new();
    c.insert(
        "bridge".into(),
        vec![candidate("main_world", 142, "bridge", [1.0, 2.0, 3.0])],
    );
    c.insert(
        "moved".into(),
        vec![candidate("main_world", 190, "moved", [100.0, 200.0, 3.0])],
    );
    c.insert(
        "foreign".into(),
        vec![candidate("inst_hard", 320, "foreign", [100.0, 200.0, 3.0])],
    );
    let o12 = origins(&[("main_world", 142, (1, 1)), ("main_world", 190, (21, 5))]);
    let o10 = origins(&[
        ("main_world", 142, (1, 1)),
        ("main_world", 190, (19, 4)),
        ("inst_hard", 320, (0, 0)),
    ]);
    let existing: BTreeSet<String> = ["bridge".to_string()].into();
    let p = plan(
        &demand(&["bridge", "moved", "foreign"]),
        &c,
        &o12,
        &o10,
        &existing,
        OPTS,
    );
    assert!(p.files.is_empty());
    assert!(p.report.existing_kept.contains("bridge"));
    assert!(p.report.skipped["zone origin differs between 1.2 and 10.x"].contains("moved"));
    assert!(p.report.skipped["world or zone not in the 1.2 world.xml"].contains("foreign"));
}

#[test]
fn rotated_paths_can_be_left_out_and_relative_ones_never_convert() {
    let mut c: HashMap<String, Vec<Candidate>> = HashMap::new();
    let mut turned = candidate("w", 1, "turned", [100.0, 200.0, 3.0]);
    turned.entity = entities()[1].clone();
    c.insert("turned".into(), vec![turned]);
    c.insert(
        "relative".into(),
        vec![candidate("w", 1, "relative", [1.0, 2.0, 3.0])],
    );
    let o = origins(&[("w", 1, (0, 0))]);
    let d = demand(&["turned", "relative"]);
    let all = plan(&d, &c, &o, &o, &BTreeSet::new(), OPTS);
    assert_eq!(all.files.keys().collect::<Vec<_>>(), vec!["turned"]);
    assert!(all.report.unusual.contains("turned"));
    assert!(all.report.skipped["spawner-relative path"].contains("relative"));
    let no_rot = PlanOptions {
        skip_rotated: true,
        ..OPTS
    };
    let p = plan(&d, &c, &o, &o, &BTreeSet::new(), no_rot);
    assert!(p.files.is_empty());
    assert!(p.report.skipped["rotated, left out"].contains("turned"));
}

#[test]
fn distance_to_a_recorded_route() {
    let generated = [[0.0, 0.0, 0.0], [10.0, 0.0, 0.0]];
    let recorded = [[2.0, 1.0, 0.0], [5.0, -3.0, 0.0]];
    let d = route_distance(&recorded, &generated).unwrap();
    assert!((d.max - 3.0).abs() < 1e-9);
    assert!((d.mean - 2.0).abs() < 1e-9);
    assert!(route_distance(&[], &generated).is_none());
}

/// `AAEMU_PAK10=<10.x game_pak> AAEMU_GAME_PAK=<1.2 game_pak> AAEMU_COMPACT_DB=<compact.sqlite3>
/// cargo test -p aaemu-tool-aipaths real_data -- --ignored`: runs the whole tool into a
/// scratch folder.
#[test]
#[ignore = "needs the client paks (AAEMU_PAK10, AAEMU_GAME_PAK) and AAEMU_COMPACT_DB"]
fn real_data_generates_the_demanded_routes() {
    let (Ok(pak10), Ok(pak12), Ok(compact)) = (
        std::env::var("AAEMU_PAK10"),
        std::env::var("AAEMU_GAME_PAK"),
        std::env::var("AAEMU_COMPACT_DB"),
    ) else {
        return;
    };
    let out = std::env::temp_dir().join(format!("aaemu_aipaths_{}", std::process::id()));
    let _ = fs::remove_dir_all(&out);
    fs::create_dir_all(&out).unwrap();
    let opts = Options {
        pak10: pak10.into(),
        pak12: pak12.into(),
        compact: compact.into(),
        out: out.clone(),
        dry_run: false,
        skip_rotated: false,
        convention: RotationConvention::Direct,
    };
    run(&opts).unwrap();
    let names: BTreeSet<String> = fs::read_dir(&out)
        .unwrap()
        .filter_map(|e| e.ok()?.path().file_stem()?.to_str().map(str::to_string))
        .collect();
    let _ = fs::remove_dir_all(&out);
    for must in [
        "q2459_ano2_gweo",
        "public_ranch",
        "public_farm@142",
        "tuto_move@129",
    ] {
        assert!(names.contains(must), "{must} missing from {names:?}");
    }
    assert!(
        !names.contains("public_farm"),
        "duplicates must be variants"
    );
    assert!(names.len() > 250, "only {} files", names.len());
}

#[test]
fn demand_includes_follow_path_npc_control_effects() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE quest_components (ai_path_name TEXT);
         CREATE TABLE ai_commands (cmd_id INTEGER, param2 TEXT);
         CREATE TABLE npc_control_effects (category_id INTEGER, param_string TEXT);
         INSERT INTO quest_components VALUES ('Quest_Route'), (NULL);
         INSERT INTO ai_commands VALUES (2, 'cmd_route'), (3, 'ignored');
         INSERT INTO npc_control_effects VALUES
           (2, 'Q2423_FM_Tige'), (2, ' '), (2, ''), (2, '17'), (1, 'other_category');",
    )
    .unwrap();
    let got = demand_from(&conn).unwrap();
    assert_eq!(
        got.keys().map(String::as_str).collect::<Vec<_>>(),
        ["cmd_route", "q2423_fm_tige", "quest_route"]
    );
    assert!(got["q2423_fm_tige"].contains("Q2423_FM_Tige"));
}
