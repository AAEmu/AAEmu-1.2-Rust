//! `return-points` mode: `Data/Portal/worldgates.json` entries from the 10.x editor
//! `return_point.xml` files.
//!
//! The `Return` special effect (type 25) with a non-zero `value1` teleports to the world gate
//! whose `Id` is that `return_points.id`. The retail coordinates are not in the database or the
//! 1.2 client; the 10.x `game_pak` keeps them in
//! `game/worlds/main_world/level_design/zone/<zone key>/editor/return_point.xml` as
//! `LevelDesignAreaSphere` objects named `ReturnPoint_<return_points.editor_name>`, with a
//! zone-local `Pos` and a `Rotate` quaternion (w, x, y, z). The world position is the zone origin
//! cell (1.2 `world.xml`) times 1024 plus `Pos`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use aaemu_commons::aapak::AAPak;
use anyhow::{bail, Context, Result};

use crate::convert::{Vec3, CELL_SIZE};

pub const USAGE: &str = "Usage: aaemu-aipaths return-points --pak10 <10.x game_pak> \
--pak12 <1.2 game_pak> --compact <compact.sqlite3> --worldgates <Data/Portal/worldgates.json> \
[--world <instance_world>]... [--worlds-dir <Data/Worlds>] [--skip <id,id,...>] [--update]";

/// First line of the generated section of `worldgates.json`.
pub const BEGIN_MARKER: &str = "//----------------- Generated from the 10.x return_point.xml \
by aaemu-aipaths return-points -----------------";
/// Last line of the generated section.
pub const END_MARKER: &str =
    "//----------------- end of the generated worldgates -----------------";

/// Distance in metres from which an existing, hand recorded gate is reported as different.
pub const MISMATCH_METRES: f64 = 5.0;

const MAIN_WORLD: &str = "main_world";

/// A `ReturnPoint_<name>` sphere of a `return_point.xml`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnPoint {
    /// The name without the `ReturnPoint_` prefix.
    pub name: String,
    /// Zone-local position.
    pub pos: Vec3,
    /// `Rotate` as (w, x, y, z).
    pub rotate: Option<[f64; 4]>,
}

fn floats(text: &str) -> Option<Vec<f64>> {
    text.split(',')
        .map(|t| t.trim().parse::<f64>().ok())
        .collect()
}

/// Reads every `ReturnPoint_*` object of a `return_point.xml`; malformed ones are skipped.
pub fn parse_return_points(xml: &str) -> Result<Vec<ReturnPoint>, roxmltree::Error> {
    let xml = xml.strip_prefix('\u{feff}').unwrap_or(xml);
    let doc = roxmltree::Document::parse(xml)?;
    let mut out = Vec::new();
    for node in doc.descendants().filter(|n| n.has_tag_name("Object")) {
        let Some(name) = node.attribute("Name") else {
            continue;
        };
        let Some(name) = name
            .get(..12)
            .filter(|p| p.eq_ignore_ascii_case("ReturnPoint_"))
            .map(|_| &name[12..])
        else {
            continue;
        };
        let Some(pos) = node
            .attribute("Pos")
            .and_then(floats)
            .and_then(|v| <[f64; 3]>::try_from(v).ok())
        else {
            continue;
        };
        let rotate = match node.attribute("Rotate") {
            None => None,
            Some(t) => match floats(t).and_then(|v| <[f64; 4]>::try_from(v).ok()) {
                Some(q) => Some(q),
                None => continue,
            },
        };
        out.push(ReturnPoint {
            name: name.to_string(),
            pos,
            rotate,
        });
    }
    Ok(out)
}

/// Heading about the vertical axis in degrees, (-180, 180], from the (w, x, y, z) quaternion.
pub fn yaw_degrees(rotate: Option<[f64; 4]>) -> f64 {
    let Some([w, x, y, z]) = rotate else {
        return 0.0;
    };
    let norm = (w * w + x * x + y * y + z * z).sqrt();
    if norm < 1e-12 {
        return 0.0;
    }
    let (w, x, y, z) = (w / norm, x / norm, y / norm, z / norm);
    let yaw = (2.0 * (w * z + x * y))
        .atan2(1.0 - 2.0 * (y * y + z * z))
        .to_degrees();
    // `Rotate` is stored as a float quaternion: snap the noise of whole degree headings.
    let yaw = (yaw * 10.0).round() / 10.0;
    if yaw == -180.0 {
        180.0
    } else {
        yaw + 0.0
    }
}

/// A zone of a `world.xml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    pub name: String,
    pub origin: (i32, i32),
}

/// `<Zone name id originX originY>` of a `world.xml`.
pub fn parse_zones(xml: &str) -> Result<HashMap<u32, Zone>, roxmltree::Error> {
    let xml = xml.strip_prefix('\u{feff}').unwrap_or(xml);
    let doc = roxmltree::Document::parse(xml)?;
    let mut out = HashMap::new();
    for z in doc.descendants().filter(|n| n.has_tag_name("Zone")) {
        let num = |a: &str| z.attribute(a).and_then(|v| v.parse::<i32>().ok());
        if let (Some(id), Some(ox), Some(oy)) = (num("id"), num("originX"), num("originY")) {
            if let Ok(id) = u32::try_from(id) {
                out.insert(
                    id,
                    Zone {
                        name: z.attribute("name").unwrap_or_default().to_string(),
                        origin: (ox, oy),
                    },
                );
            }
        }
    }
    Ok(out)
}

/// A `return_points` row that a `Return` special effect points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Demand {
    pub id: u32,
    pub editor_name: String,
    /// `return_points.name` (the Korean display name).
    pub display_name: String,
}

/// The `return_points` that `Return` special effects (type 25) with a non-zero `value1` use.
pub fn load_demand(compact: &Path) -> Result<Vec<Demand>> {
    let conn =
        rusqlite::Connection::open_with_flags(compact, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open {}", compact.display()))?;
    let mut stmt = conn.prepare(
        "SELECT DISTINCT rp.id, rp.editor_name, rp.name FROM special_effects se \
         JOIN return_points rp ON rp.id = se.value1 \
         WHERE se.special_effect_type_id = 25 AND se.value1 <> 0 ORDER BY rp.id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Demand {
            id: r.get(0)?,
            editor_name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            display_name: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The zone names of the compact database by `zones.zone_key`, the `world.xml` zone id (and the
/// `ZoneId` of `worldgates.json`; `zones.id` is a different number).
pub fn load_zone_names(compact: &Path) -> Result<HashMap<u32, String>> {
    let conn =
        rusqlite::Connection::open_with_flags(compact, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open {}", compact.display()))?;
    let mut stmt = conn.prepare("SELECT zone_key, name FROM zones")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, u32>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (id, name) = row?;
        out.insert(id, name.unwrap_or_default());
    }
    Ok(out)
}

/// A hand recorded or generated gate of `worldgates.json` (the fields the tool looks at).
#[derive(Debug, Clone, PartialEq)]
pub struct GateRow {
    pub id: u32,
    pub zone_id: u32,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub sub_zone_id: u32,
}

/// A generated gate.
#[derive(Debug, Clone, PartialEq)]
pub struct Gate {
    pub id: u32,
    pub name: String,
    pub display_name: String,
    pub zone_id: u32,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f64,
    pub sub_zone_id: u32,
}

/// Where a point was found: `zone key` and the point.
pub type Found = (u32, ReturnPoint);

/// Horizontal distance in metres to the nearest 1.2 spawn of an instance world beyond which a
/// generated instance gate is reported as suspicious.
pub const SPAWN_WARN_METRES: f64 = 40.0;

/// An instance world (`--world`): its own zone origins, its own `return_point.xml` points and the
/// 1.2 spawn positions that stand in for the terrain check.
#[derive(Debug, Default)]
pub struct InstanceWorld {
    pub name: String,
    pub zones12: HashMap<u32, Zone>,
    pub zones10: HashMap<u32, Zone>,
    /// Lower-case point name to every `return_point.xml` entry of this world.
    pub points: HashMap<String, Vec<Found>>,
    /// World positions of the 1.2 NPC and doodad spawns of the world.
    pub spawns: Vec<Vec3>,
}

/// Why a demanded return point has no generated gate.
#[derive(Debug, Default)]
pub struct Report {
    /// Reason to the ids it applies to.
    pub skipped: BTreeMap<String, BTreeSet<u32>>,
    /// Instance gates that were generated but look suspicious (id, text).
    pub warnings: Vec<(u32, String)>,
}

impl Report {
    fn skip(&mut self, reason: &str, id: u32) {
        self.skipped
            .entry(reason.to_string())
            .or_default()
            .insert(id);
    }
}

/// World position (x, y, z) of a zone-local point.
pub fn world_position(zone: &Zone, p: &ReturnPoint) -> Vec3 {
    [
        f64::from(zone.origin.0) * CELL_SIZE + p.pos[0],
        f64::from(zone.origin.1) * CELL_SIZE + p.pos[1],
        p.pos[2],
    ]
}

/// Round to one decimal (the precision of `worldgates.json`), without a negative zero.
fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0 + 0.0
}

/// Inputs of [`plan`].
pub struct PlanInput<'a> {
    pub demand: &'a [Demand],
    /// Lower-case point name to every `return_point.xml` entry of main_world of that name.
    pub points: &'a HashMap<String, Vec<Found>>,
    pub zones12: &'a HashMap<u32, Zone>,
    pub zones10: &'a HashMap<u32, Zone>,
    /// `zones.zone_key` to `zones.name` of the compact database.
    pub db_zones: &'a HashMap<u32, String>,
    /// Instance worlds searched for the ids the main_world data does not know.
    pub instances: &'a [InstanceWorld],
    /// The hand curated gates (never changed, their ids and sub zones are taken).
    pub existing: &'a [GateRow],
    /// Ids left out on purpose (`--skip`).
    pub skip: &'a BTreeSet<u32>,
}

/// `id`, or `100000 + id` when another gate already holds that number as its sub zone.
fn take_sub_zone(used: &mut BTreeSet<u32>, id: u32) -> u32 {
    let sub = if used.contains(&id) { 100_000 + id } else { id };
    used.insert(sub);
    sub
}

/// Horizontal distance from (x, y) to the nearest of `spawns`.
fn nearest_spawn(spawns: &[Vec3], x: f64, y: f64) -> Option<f64> {
    spawns
        .iter()
        .map(|s| (s[0] - x).hypot(s[1] - y))
        .min_by(f64::total_cmp)
}

/// The gate of a demanded id from the `--world` instance worlds: the point must be unique, its
/// zone must have the same id and origin cell in the 1.2 and 10.x `world.xml` of that world
/// (the `SubZoneId` is filled in by the caller). The terrain check of main_world is replaced by
/// the distance to the nearest 1.2 spawn, reported as a warning only. `None` (nothing reported)
/// when no instance world knows the name; a problem is recorded in the report and `None` too.
fn plan_instance(input: &PlanInput<'_>, d: &Demand, report: &mut Report) -> Option<Gate> {
    let key = d.editor_name.to_lowercase();
    let mut hits: Vec<(&InstanceWorld, u32, &ReturnPoint)> = Vec::new();
    for w in input.instances {
        for (zone, p) in w.points.get(&key).into_iter().flatten() {
            hits.push((w, *zone, p));
        }
    }
    let (world, zone_key, point) = *hits.first()?;
    let (Some(z12), Some(z10)) = (world.zones12.get(&zone_key), world.zones10.get(&zone_key))
    else {
        report.skip("zone not in both world.xml files", d.id);
        return None;
    };
    if z12.origin != z10.origin {
        report.skip("zone origin differs between 1.2 and 10.x", d.id);
        return None;
    }
    let here = world_position(z12, point);
    let ambiguous = hits.iter().skip(1).any(|(w, k, p)| {
        let other = w
            .zones12
            .get(k)
            .map_or([f64::NAN; 3], |z| world_position(z, p));
        w.name != world.name
            || *k != zone_key
            || (other[0] - here[0]).hypot(other[1] - here[1]) > 0.5
    });
    if ambiguous {
        report.skip("several different positions", d.id);
        return None;
    }
    match nearest_spawn(&world.spawns, here[0], here[1]) {
        Some(m) if m <= SPAWN_WARN_METRES => {}
        Some(m) => report.warnings.push((
            d.id,
            format!(
                "{}: nearest 1.2 spawn of {} is {m:.1} m away",
                d.editor_name, world.name
            ),
        )),
        None => report.warnings.push((
            d.id,
            format!("{}: {} has no 1.2 spawns", d.editor_name, world.name),
        )),
    }
    Some(Gate {
        id: d.id,
        name: d.editor_name.clone(),
        display_name: d.display_name.clone(),
        zone_id: zone_key,
        x: round1(here[0]),
        y: round1(here[1]),
        z: round1(here[2]),
        yaw: yaw_degrees(point.rotate),
        sub_zone_id: d.id,
    })
}

/// The gates to generate (sorted by id) and the report.
///
/// A demanded id gets a gate when it is missing from `existing`, has exactly one position in the
/// 10.x main_world data, and its zone is the same zone in 1.2 and 10.x (same name, same origin
/// cell, known to the compact database).
///
/// `SubZoneId` is the gate id: the server only uses it as the dictionary key of the gate (see
/// `PortalIndex`), and the id is already unique. When another gate holds that number as its sub
/// zone, `100000 + id` is used.
pub fn plan(input: &PlanInput<'_>) -> (Vec<Gate>, Report) {
    let mut report = Report::default();
    let mut gates = Vec::new();
    let have_ids: BTreeSet<u32> = input.existing.iter().map(|g| g.id).collect();
    let mut used_sub: BTreeSet<u32> = input.existing.iter().map(|g| g.sub_zone_id).collect();
    for d in input.demand {
        if have_ids.contains(&d.id) {
            report.skip("already in worldgates.json", d.id);
            continue;
        }
        if input.skip.contains(&d.id) {
            report.skip("left out with --skip", d.id);
            continue;
        }
        let Some(found) = input.points.get(&d.editor_name.to_lowercase()) else {
            match plan_instance(input, d, &mut report) {
                Some(mut gate) => {
                    gate.sub_zone_id = take_sub_zone(&mut used_sub, d.id);
                    gates.push(gate);
                }
                None => report.skip("no main_world return point in the 10.x data", d.id),
            }
            continue;
        };
        let (zone_key, point) = &found[0];
        let (Some(z12), Some(z10)) = (input.zones12.get(zone_key), input.zones10.get(zone_key))
        else {
            report.skip("zone not in both world.xml files", d.id);
            continue;
        };
        if z12.origin != z10.origin {
            report.skip("zone origin differs between 1.2 and 10.x", d.id);
            continue;
        }
        let same_name = z12.name.eq_ignore_ascii_case(&z10.name)
            && input
                .db_zones
                .get(zone_key)
                .is_some_and(|n| n.eq_ignore_ascii_case(&z12.name));
        if !same_name {
            report.skip("zone is not the same zone in 1.2 and 10.x", d.id);
            continue;
        }
        let here = world_position(z12, point);
        let ambiguous = found.iter().skip(1).any(|(k, p)| {
            let other = input
                .zones12
                .get(k)
                .map_or([f64::NAN; 3], |z| world_position(z, p));
            (other[0] - here[0]).hypot(other[1] - here[1]) > 0.5
        });
        if ambiguous {
            report.skip("several different positions", d.id);
            continue;
        }
        let sub_zone_id = take_sub_zone(&mut used_sub, d.id);
        gates.push(Gate {
            id: d.id,
            name: d.editor_name.clone(),
            display_name: d.display_name.clone(),
            zone_id: *zone_key,
            x: round1(here[0]),
            y: round1(here[1]),
            z: round1(here[2]),
            yaw: yaw_degrees(point.rotate),
            sub_zone_id,
        });
    }
    gates.sort_by_key(|g| g.id);
    (gates, report)
}

/// Hand recorded gates that are not where the 10.x data puts their return point: (id, zone key
/// of the file, horizontal metres, vertical metres).
pub fn mismatches(
    existing: &[GateRow],
    demand: &[Demand],
    points: &HashMap<String, Vec<Found>>,
    zones12: &HashMap<u32, Zone>,
) -> Vec<(u32, u32, f64, f64)> {
    let mut out = Vec::new();
    for g in existing {
        let Some(d) = demand.iter().find(|d| d.id == g.id) else {
            continue;
        };
        let Some(found) = points.get(&d.editor_name.to_lowercase()) else {
            continue;
        };
        // The closest of the entries of that name.
        let best = found
            .iter()
            .filter_map(|(k, p)| zones12.get(k).map(|z| (*k, world_position(z, p))))
            .map(|(k, w)| (k, (w[0] - g.x).hypot(w[1] - g.y), (w[2] - g.z).abs()))
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((k, h, v)) = best {
            if h > MISMATCH_METRES || v > MISMATCH_METRES {
                out.push((g.id, k, h, v));
            }
        }
    }
    out
}

fn num(v: f64) -> String {
    let s = format!("{v:.1}");
    if s == "-0.0" {
        "0.0".to_string()
    } else {
        s
    }
}

/// The generated section of `worldgates.json`: marker, one commented entry per gate, end marker.
/// Four space indentation like the hand written entries; `eol` ends every line.
pub fn format_section(
    gates: &[Gate],
    skip: &BTreeSet<u32>,
    worlds: &[String],
    eol: &str,
) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push_str(eol);
    };
    line(format!("    {BEGIN_MARKER}"));
    line(
        "    // Run the tool again to regenerate this section; do not edit it by hand.".to_string(),
    );
    line(
        "    // Position = zone origin cell * 1024 + the zone-local ReturnPoint_<name> position;"
            .to_string(),
    );
    line(
        "    // SubZoneId is only the dictionary key of the gate (the gate Id, see the tool)."
            .to_string(),
    );
    if !worlds.is_empty() {
        line(format!(
            "    // Instance worlds read with --world (zone origins per world): {}",
            worlds.join(" ")
        ));
    }
    if !skip.is_empty() {
        let ids: Vec<String> = skip.iter().map(u32::to_string).collect();
        line(format!(
            "    // Left out with --skip (not on the 1.2 terrain or navmesh): {}",
            ids.join(" ")
        ));
    }
    for (i, g) in gates.iter().enumerate() {
        let comma = if i + 1 == gates.len() { "" } else { "," };
        line("    {".to_string());
        line(format!(
            "        \"Id\": {}, // {}",
            g.id,
            g.display_name.replace(['\r', '\n'], " ")
        ));
        line(format!("        \"Name\": \"{}\",", json_escape(&g.name)));
        line(format!("        \"ZoneId\": {},", g.zone_id));
        line(format!("        \"X\": {},", num(g.x)));
        line(format!("        \"Y\": {},", num(g.y)));
        line(format!("        \"Z\": {},", num(g.z)));
        line(format!("        \"Yaw\": {},", num(g.yaw)));
        line(format!("        \"SubZoneId\": {}", g.sub_zone_id));
        line(format!("    }}{comma}"));
    }
    line(format!("    {END_MARKER}"));
    out
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `text` without `//` and `/* */` comments (string contents are kept).
pub fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => {
                    if let Some(n) = chars.next() {
                        out.push(n);
                    }
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// The gates of a `worldgates.json` text (comments allowed, `[` ... `]`).
pub fn parse_gates(text: &str) -> Result<Vec<GateRow>> {
    let clean = strip_comments(text.strip_prefix('\u{feff}').unwrap_or(text));
    let value: serde_json::Value = serde_json::from_str(&clean).context("parse worldgates.json")?;
    let Some(items) = value.as_array() else {
        bail!("worldgates.json is not an array");
    };
    let mut out = Vec::new();
    for item in items {
        let int = |k: &str| item.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0) as u32;
        let flt = |k: &str| {
            item.get(k)
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0)
        };
        out.push(GateRow {
            id: int("Id"),
            zone_id: int("ZoneId"),
            x: flt("X"),
            y: flt("Y"),
            z: flt("Z"),
            sub_zone_id: int("SubZoneId"),
        });
    }
    Ok(out)
}

/// Splits a `worldgates.json` text into the hand curated part (everything before the generated
/// section or the closing `]`, without trailing white space or comma) and the text after the
/// closing `]`.
fn split_file(text: &str) -> Result<(String, String)> {
    let close = text
        .rfind(']')
        .context("worldgates.json has no closing ]")?;
    let tail = text[close + 1..].to_string();
    let head_end = match text.find(BEGIN_MARKER) {
        Some(at) => text[..at].rfind('\n').map_or(0, |n| n + 1).min(close),
        None => close,
    };
    let head = text[..head_end]
        .trim_end()
        .trim_end_matches(',')
        .to_string();
    Ok((head, tail))
}

/// The hand curated gates of a `worldgates.json` (the generated section is ignored).
pub fn hand_curated(text: &str) -> Result<Vec<GateRow>> {
    let (head, _) = split_file(text)?;
    parse_gates(&format!("{head}\n]"))
}

/// `text` with its generated section replaced by `gates` (appended before the closing `]` when
/// there is none; removed when `gates` is empty).
pub fn splice(
    text: &str,
    gates: &[Gate],
    skip: &BTreeSet<u32>,
    worlds: &[String],
) -> Result<String> {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let (head, tail) = split_file(text)?;
    if gates.is_empty() {
        return Ok(format!("{head}{eol}]{tail}"));
    }
    let section = format_section(gates, skip, worlds, eol);
    Ok(format!("{head},{eol}{eol}{section}]{tail}"))
}

fn read_text(pak: &AAPak, name: &str) -> Option<String> {
    let bytes = pak.read_file(name).ok().flatten()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Every `game/worlds/<world>/level_design/zone/<key>/editor/return_point.xml`.
fn return_point_files(pak: &AAPak, world: &str) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    for f in pak.files() {
        let lower = f.name.to_lowercase().replace('\\', "/");
        let Some(rest) = lower.strip_prefix("game/worlds/") else {
            continue;
        };
        let parts: Vec<&str> = rest.split('/').collect();
        if let [w, "level_design", "zone", key, "editor", "return_point.xml"] = parts.as_slice() {
            if *w != world.to_lowercase() {
                continue;
            }
            if let Ok(zone) = key.parse::<u32>() {
                out.push((f.name.clone(), zone));
            }
        }
    }
    out.sort_by_key(|f| f.1);
    out
}

/// World positions of the `"Position": {"X", "Y", "Z"}` entries of the `*_spawns.json` files of
/// `dir` (comments allowed).
pub fn load_spawns(dir: &Path) -> Vec<Vec3> {
    let mut out = Vec::new();
    for name in ["npc_spawns.json", "doodad_spawns.json"] {
        let Ok(text) = fs::read_to_string(dir.join(name)) else {
            continue;
        };
        let clean = strip_comments(text.strip_prefix('\u{feff}').unwrap_or(&text));
        let Ok(serde_json::Value::Array(items)) = serde_json::from_str(&clean) else {
            continue;
        };
        for item in items {
            let Some(p) = item.get("Position") else {
                continue;
            };
            let c = |k: &str| p.get(k).and_then(serde_json::Value::as_f64);
            if let (Some(x), Some(y), Some(z)) = (c("X"), c("Y"), c("Z")) {
                out.push([x, y, z]);
            }
        }
    }
    out
}

/// Reads the `world.xml` of `world` from both paks and the `return_point.xml` files of the 10.x
/// pak, and the 1.2 spawns from `worlds_dir`.
fn load_instance_world(
    pak10: &AAPak,
    pak12: &AAPak,
    world: &str,
    worlds_dir: &Path,
) -> Result<InstanceWorld> {
    let world_xml = format!("game/worlds/{world}/world.xml");
    let zones10 = parse_zones(
        &read_text(pak10, &world_xml).with_context(|| format!("10.x {world} world.xml"))?,
    )?;
    let zones12 = parse_zones(
        &read_text(pak12, &world_xml).with_context(|| format!("1.2 {world} world.xml"))?,
    )?;
    let mut points: HashMap<String, Vec<Found>> = HashMap::new();
    for (name, zone) in return_point_files(pak10, world) {
        let Some(xml) = read_text(pak10, &name) else {
            eprintln!("warning: cannot read {name}");
            continue;
        };
        match parse_return_points(&xml) {
            Ok(list) => {
                for p in list {
                    points
                        .entry(p.name.to_lowercase())
                        .or_default()
                        .push((zone, p));
                }
            }
            Err(e) => eprintln!("warning: {name}: {e}"),
        }
    }
    let spawns = load_spawns(&worlds_dir.join(world));
    if spawns.is_empty() {
        eprintln!(
            "warning: no 1.2 spawns for {world} in {}",
            worlds_dir.display()
        );
    }
    eprintln!(
        "{world}: {} zones, {} point names, {} spawns",
        zones12.len(),
        points.len(),
        spawns.len()
    );
    Ok(InstanceWorld {
        name: world.to_string(),
        zones12,
        zones10,
        points,
        spawns,
    })
}

#[derive(Debug)]
pub struct Options {
    pub pak10: PathBuf,
    pub pak12: PathBuf,
    pub compact: PathBuf,
    pub worldgates: PathBuf,
    /// Instance worlds to read (`--world`, repeatable).
    pub worlds: Vec<String>,
    /// `Data/Worlds` of the game app (the 1.2 spawns); default: next to the `Portal` folder.
    pub worlds_dir: Option<PathBuf>,
    pub skip: BTreeSet<u32>,
    pub update: bool,
}

pub fn parse_args(args: &[String]) -> Result<Options> {
    let (mut pak10, mut pak12, mut compact, mut worldgates) = (None, None, None, None);
    let mut update = false;
    let mut skip = BTreeSet::new();
    let mut worlds: Vec<String> = Vec::new();
    let mut worlds_dir = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| -> Result<PathBuf> {
            it.next()
                .map(PathBuf::from)
                .with_context(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--pak10" => pak10 = Some(value("--pak10")?),
            "--pak12" => pak12 = Some(value("--pak12")?),
            "--compact" => compact = Some(value("--compact")?),
            "--worldgates" => worldgates = Some(value("--worldgates")?),
            "--world" => {
                let w = value("--world")?.to_string_lossy().into_owned();
                if w.eq_ignore_ascii_case(MAIN_WORLD) {
                    bail!("--world: {MAIN_WORLD} is always read, name an instance world");
                }
                if !worlds.contains(&w) {
                    worlds.push(w);
                }
            }
            "--worlds-dir" => worlds_dir = Some(value("--worlds-dir")?),
            "--skip" => {
                for id in value("--skip")?.to_string_lossy().split(',') {
                    skip.insert(
                        id.trim()
                            .parse::<u32>()
                            .with_context(|| format!("--skip: bad id {id:?}"))?,
                    );
                }
            }
            "--update" => update = true,
            other => bail!("unknown argument {other}\n{USAGE}"),
        }
    }
    Ok(Options {
        pak10: pak10.with_context(|| USAGE)?,
        pak12: pak12.with_context(|| USAGE)?,
        compact: compact.with_context(|| USAGE)?,
        worldgates: worldgates.with_context(|| USAGE)?,
        worlds,
        worlds_dir,
        skip,
        update,
    })
}

/// Runs the mode. The generated section goes to stdout (or into the file with `--update`), the
/// report to stderr.
pub fn run(opts: &Options) -> Result<()> {
    let text = fs::read_to_string(&opts.worldgates)
        .with_context(|| format!("read {}", opts.worldgates.display()))?;
    let existing = hand_curated(&text)?;
    let demand = load_demand(&opts.compact)?;
    let db_zones = load_zone_names(&opts.compact)?;

    let pak10 = AAPak::open(&opts.pak10, true).context("open the 10.x game_pak")?;
    let pak12 = AAPak::open(&opts.pak12, true).context("open the 1.2 game_pak")?;
    let world_xml = format!("game/worlds/{MAIN_WORLD}/world.xml");
    let zones10 =
        parse_zones(&read_text(&pak10, &world_xml).context("10.x main_world world.xml")?)?;
    let zones12 = parse_zones(&read_text(&pak12, &world_xml).context("1.2 main_world world.xml")?)?;

    let mut points: HashMap<String, Vec<Found>> = HashMap::new();
    for (name, zone) in return_point_files(&pak10, MAIN_WORLD) {
        let Some(xml) = read_text(&pak10, &name) else {
            eprintln!("warning: cannot read {name}");
            continue;
        };
        match parse_return_points(&xml) {
            Ok(list) => {
                for p in list {
                    points
                        .entry(p.name.to_lowercase())
                        .or_default()
                        .push((zone, p));
                }
            }
            Err(e) => eprintln!("warning: {name}: {e}"),
        }
    }
    eprintln!(
        "Return effects use {} return points; {} names in the 10.x return_point.xml files; \
         {} hand curated gates",
        demand.len(),
        points.len(),
        existing.len()
    );

    let worlds_dir = opts.worlds_dir.clone().unwrap_or_else(|| {
        opts.worldgates
            .parent()
            .and_then(Path::parent)
            .map_or_else(|| PathBuf::from("Worlds"), |d| d.join("Worlds"))
    });
    let instances = opts
        .worlds
        .iter()
        .map(|w| load_instance_world(&pak10, &pak12, w, &worlds_dir))
        .collect::<Result<Vec<_>>>()?;

    let (gates, report) = plan(&PlanInput {
        instances: &instances,
        demand: &demand,
        points: &points,
        zones12: &zones12,
        zones10: &zones10,
        db_zones: &db_zones,
        existing: &existing,
        skip: &opts.skip,
    });
    eprintln!("generated gates: {}", gates.len());
    for (reason, ids) in &report.skipped {
        let list: Vec<String> = ids.iter().map(u32::to_string).collect();
        eprintln!("skipped, {reason}: {} ({})", ids.len(), list.join(" "));
    }
    for (id, text) in &report.warnings {
        eprintln!("warning: gate {id}: {text}");
    }
    eprintln!(
        "hand curated gates more than {MISMATCH_METRES} m from the 10.x return point \
         (id, 10.x zone, horizontal m, vertical m):"
    );
    for (id, zone, h, v) in mismatches(&existing, &demand, &points, &zones12) {
        eprintln!("  {id}: zone {zone}, {h:.1}, {v:.1}");
    }

    if opts.update {
        let new = splice(&text, &gates, &opts.skip, &opts.worlds)?;
        fs::write(&opts.worldgates, new)
            .with_context(|| format!("write {}", opts.worldgates.display()))?;
        eprintln!("updated {}", opts.worldgates.display());
    } else {
        print!("{}", format_section(&gates, &opts.skip, &opts.worlds, "\n"));
    }
    Ok(())
}
