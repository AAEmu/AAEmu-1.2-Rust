use std::collections::{BTreeSet, HashMap};

use crate::returnpoints::*;

const XML: &str = r#"<Objects>
 <Object Type="LevelDesignAreaSphere" Layer="Main" Name="ReturnPoint_Liania" Pos="1226.5029,873.17493,111.43375" Rotate="0.087155745,0,0,-0.9961947" Radius="3" EntityClass="AreaSphere"/>
 <Object Type="LevelDesignAreaSphere" Name="ReturnPoint_north" Pos="10,20,30" Rotate="0.70710678,0,0,0.70710678"/>
 <Object Type="LevelDesignAreaSphere" Name="returnpoint_Plain" Pos="1,2,3"/>
 <Object Type="LevelDesignAreaSphere" Name="ReturnPoint_broken" Pos="1,2"/>
 <Object Type="LevelDesignAreaSphere" Name="Graveyard_other" Pos="5,5,5"/>
</Objects>"#;

const WORLD: &str = r#"<World name="main_world">
 <ZoneList>
  <Zone name="w_cross_plains_1" id="148" originX="13" originY="10"><cellList/></Zone>
  <Zone name="w_other" id="149" originX="1" originY="2"><cellList/></Zone>
  <Zone name="w_moved" id="150" originX="3" originY="3"><cellList/></Zone>
  <Zone name="w_renamed" id="151" originX="4" originY="4"><cellList/></Zone>
 </ZoneList>
</World>"#;

fn point(name: &str, pos: [f64; 3]) -> ReturnPoint {
    ReturnPoint {
        name: name.to_string(),
        pos,
        rotate: None,
    }
}

fn demand(id: u32, name: &str) -> Demand {
    Demand {
        id,
        editor_name: name.to_string(),
        display_name: format!("display {name}"),
    }
}

fn zones(text: &str) -> HashMap<u32, Zone> {
    parse_zones(text).unwrap()
}

struct Fixture {
    points: HashMap<String, Vec<Found>>,
    zones12: HashMap<u32, Zone>,
    zones10: HashMap<u32, Zone>,
    db_zones: HashMap<u32, String>,
}

fn fixture() -> Fixture {
    let mut points: HashMap<String, Vec<Found>> = HashMap::new();
    let mut add = |zone: u32, p: ReturnPoint| {
        points
            .entry(p.name.to_lowercase())
            .or_default()
            .push((zone, p));
    };
    add(148, point("Liania", [1226.5, 873.2, 111.4]));
    add(149, point("other", [100.0, 100.0, 50.0]));
    add(150, point("moved", [1.0, 1.0, 1.0]));
    add(151, point("renamed", [1.0, 1.0, 1.0]));
    // The same name twice at (almost) the same spot, and at different spots.
    add(148, point("twin", [10.0, 10.0, 5.0]));
    add(148, point("twin", [10.2, 10.1, 5.0]));
    add(148, point("apart", [10.0, 10.0, 5.0]));
    add(149, point("apart", [10.0, 10.0, 5.0]));
    let zones12 = zones(WORLD);
    let mut zones10 = zones12.clone();
    zones10.get_mut(&150).unwrap().origin = (9, 9);
    zones10.get_mut(&151).unwrap().name = "w_old_name".to_string();
    let db_zones = HashMap::from([
        (148, "w_cross_plains_1".to_string()),
        (149, "W_OTHER".to_string()),
        (150, "w_moved".to_string()),
        (151, "w_renamed".to_string()),
    ]);
    Fixture {
        points,
        zones12,
        zones10,
        db_zones,
    }
}

fn row(id: u32, sub: u32) -> GateRow {
    GateRow {
        id,
        zone_id: 148,
        x: 0.0,
        y: 0.0,
        z: 0.0,
        sub_zone_id: sub,
    }
}

fn run_plan(f: &Fixture, demand: &[Demand], existing: &[GateRow]) -> (Vec<Gate>, Report) {
    plan(&PlanInput {
        demand,
        points: &f.points,
        zones12: &f.zones12,
        zones10: &f.zones10,
        db_zones: &f.db_zones,
        existing,
        skip: &BTreeSet::new(),
    })
}

#[test]
fn parses_return_point_objects() {
    let list = parse_return_points(XML).unwrap();
    let names: Vec<&str> = list.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["Liania", "north", "Plain"]);
    assert_eq!(list[0].pos, [1226.5029, 873.17493, 111.43375]);
    assert!(list[2].rotate.is_none());
}

#[test]
fn yaw_comes_from_the_quaternion_about_the_vertical_axis() {
    let list = parse_return_points(XML).unwrap();
    assert_eq!(yaw_degrees(list[0].rotate), -170.0);
    assert_eq!(yaw_degrees(list[1].rotate), 90.0);
    assert_eq!(yaw_degrees(None), 0.0);
    assert_eq!(yaw_degrees(Some([0.0, 0.0, 0.0, 1.0])), 180.0);
    assert_eq!(yaw_degrees(Some([1.0, 0.0, 0.0, 0.0])), 0.0);
}

#[test]
fn zone_origins_and_names_are_read() {
    let z = zones(WORLD);
    assert_eq!(z[&148].origin, (13, 10));
    assert_eq!(z[&148].name, "w_cross_plains_1");
    let p = &parse_return_points(XML).unwrap()[0];
    let w = world_position(&z[&148], p);
    assert!((w[0] - 14538.5029).abs() < 1e-3 && (w[1] - 11113.17493).abs() < 1e-3);
    assert!((w[2] - 111.43375).abs() < 1e-9);
}

#[test]
fn plan_generates_missing_gates_sorted_by_id() {
    let f = fixture();
    let d = [demand(30, "Liania"), demand(9, "other")];
    let (gates, report) = run_plan(&f, &d, &[]);
    assert_eq!(gates.iter().map(|g| g.id).collect::<Vec<_>>(), [9, 30]);
    let g = &gates[1];
    assert_eq!(g.name, "Liania");
    assert_eq!(g.zone_id, 148);
    assert_eq!((g.x, g.y, g.z), (14538.5, 11113.2, 111.4));
    assert_eq!(g.sub_zone_id, 30);
    assert!(report.skipped.is_empty());
}

#[test]
fn plan_reports_every_reason_it_leaves_an_id_out() {
    let f = fixture();
    let d = [
        demand(1, "Liania"),
        demand(2, "nothing"),
        demand(3, "moved"),
        demand(4, "renamed"),
        demand(5, "apart"),
        demand(6, "twin"),
    ];
    let (gates, report) = run_plan(&f, &d, &[row(1, 77)]);
    // `twin` is twice at (almost) the same spot: one gate.
    assert_eq!(gates.iter().map(|g| g.id).collect::<Vec<_>>(), [6]);
    let reasons: Vec<(&str, Vec<u32>)> = report
        .skipped
        .iter()
        .map(|(k, v)| (k.as_str(), v.iter().copied().collect()))
        .collect();
    assert_eq!(
        reasons,
        [
            ("already in worldgates.json", vec![1]),
            ("no main_world return point in the 10.x data", vec![2]),
            ("several different positions", vec![5]),
            ("zone is not the same zone in 1.2 and 10.x", vec![4]),
            ("zone origin differs between 1.2 and 10.x", vec![3]),
        ]
    );
}

#[test]
fn sub_zone_ids_never_collide() {
    let f = fixture();
    // 30 is taken as a sub zone by a hand curated gate.
    let (gates, _) = run_plan(&f, &[demand(30, "Liania")], &[row(5, 30)]);
    assert_eq!(gates[0].sub_zone_id, 100_030);
}

#[test]
fn mismatching_hand_curated_gates_are_listed() {
    let f = fixture();
    let d = [demand(30, "Liania"), demand(9, "other")];
    let near = GateRow {
        id: 30,
        zone_id: 148,
        x: 14541.0,
        y: 11113.0,
        z: 110.0,
        sub_zone_id: 1,
    };
    let far = GateRow {
        id: 9,
        zone_id: 149,
        x: 2000.0,
        y: 2000.0,
        z: 50.0,
        sub_zone_id: 2,
    };
    let m = mismatches(&[near, far], &d, &f.points, &f.zones12);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].0, 9);
    assert!(m[0].2 > 800.0);
}

const FILE: &str = "[\r\n    // hand curated\r\n    {\r\n        \"Id\": 1, /* inline */\r\n        \"Name\": \"a // not a comment\",\r\n        \"ZoneId\": 148,\r\n        \"X\": 1.5,\r\n        \"Y\": 2.5,\r\n        \"Z\": 3.5,\r\n        \"Yaw\": 0,\r\n        \"SubZoneId\": 10\r\n    }\r\n]";

fn gate(id: u32) -> Gate {
    Gate {
        id,
        name: format!("g{id}"),
        display_name: "disp".to_string(),
        zone_id: 148,
        x: 1.0,
        y: -0.04,
        z: 3.0,
        yaw: -90.0,
        sub_zone_id: id,
    }
}

fn none() -> BTreeSet<u32> {
    BTreeSet::new()
}

fn ids(text: &str) -> Vec<u32> {
    parse_gates(text).unwrap().iter().map(|g| g.id).collect()
}

#[test]
fn comments_are_stripped_outside_strings_only() {
    let rows = parse_gates(FILE).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].id, rows[0].sub_zone_id, rows[0].zone_id),
        (1, 10, 148)
    );
    assert_eq!((rows[0].x, rows[0].y, rows[0].z), (1.5, 2.5, 3.5));
}

#[test]
fn section_is_commented_and_valid_json_with_the_file() {
    let text = format_section(&[gate(5), gate(6)], &BTreeSet::new(), "\n");
    assert!(text.starts_with("    //----"));
    assert!(text.contains("\"Y\": 0.0,"));
    assert!(text.contains("\"Yaw\": -90.0,"));
    assert!(text.contains("    },\n    {"));
    assert!(text.trim_end().ends_with(END_MARKER));
    let spliced = splice(FILE, &[gate(5), gate(6)], &none()).unwrap();
    assert_eq!(ids(&spliced), [1, 5, 6]);
    assert!(spliced.contains(BEGIN_MARKER));
    // The file keeps its line endings and its end.
    assert!(!spliced.replace("\r\n", "").contains('\n'));
    assert!(spliced.ends_with(']'));
}

#[test]
fn splice_is_idempotent_and_replaces_the_old_section() {
    let once = splice(FILE, &[gate(5), gate(6)], &none()).unwrap();
    assert_eq!(splice(&once, &[gate(5), gate(6)], &none()).unwrap(), once);
    let replaced = splice(&once, &[gate(7)], &none()).unwrap();
    assert_eq!(ids(&replaced), [1, 7]);
    // Only the hand curated gate counts as existing, whatever the section holds.
    let hand: Vec<u32> = hand_curated(&once).unwrap().iter().map(|g| g.id).collect();
    assert_eq!(hand, [1]);
    // No gates: the section disappears and the original text comes back.
    assert_eq!(splice(&once, &[], &none()).unwrap(), FILE);
}

#[test]
fn unknown_arguments_are_rejected() {
    let args: Vec<String> = ["--pak10", "a", "--bogus"].map(String::from).to_vec();
    assert!(parse_args(&args).is_err());
}

#[test]
fn skipped_ids_are_left_out_and_recorded_in_the_section() {
    let f = fixture();
    let skip = BTreeSet::from([30]);
    let (gates, report) = plan(&PlanInput {
        demand: &[demand(30, "Liania"), demand(9, "other")],
        points: &f.points,
        zones12: &f.zones12,
        zones10: &f.zones10,
        db_zones: &f.db_zones,
        existing: &[],
        skip: &skip,
    });
    assert_eq!(gates.iter().map(|g| g.id).collect::<Vec<_>>(), [9]);
    assert_eq!(report.skipped["left out with --skip"], BTreeSet::from([30]));
    let section = format_section(&gates, &skip, "\n");
    assert!(section.contains("--skip (not on the 1.2 terrain or navmesh): 30"));
    let args: Vec<String> = [
        "--pak10",
        "a",
        "--pak12",
        "b",
        "--compact",
        "c",
        "--worldgates",
        "d",
        "--skip",
        "5, 7",
    ]
    .map(String::from)
    .to_vec();
    assert_eq!(parse_args(&args).unwrap().skip, BTreeSet::from([5, 7]));
}
