//! Pure conversion of the 10.x editor `npc.xml` `<aiPath>` entities into server route files.
//!
//! An `aiPath` entity has a position (`Pos`), an optional rotation (`Rotate`, a quaternion
//! written w,x,y,z), an optional `Scale` and `Points` relative to the entity. Absolute paths
//! (quest and AI command routes) carry the zone-local entity position; the spawner-attached
//! ones (`AIPath_<zone>_<npc>_<n>`) sit near the zone origin and are relative to their spawner,
//! so they are not converted here.

use std::collections::{BTreeMap, BTreeSet, HashMap};

pub type Vec3 = [f64; 3];

/// Edge length of one world cell in metres (zone origins are given in cells).
pub const CELL_SIZE: f64 = 1024.0;

/// An absolute entity whose `Pos` is closer than this to the zone origin (both axes) is a
/// spawner-relative path.
pub const RELATIVE_POS_LIMIT: f64 = 100.0;

/// Which way the `Rotate` quaternion turns the entity-local points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationConvention {
    /// `q * v * q^-1`.
    Direct,
    /// `q^-1 * v * q`.
    Inverse,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AiPathEntity {
    pub name: String,
    pub pos: Vec3,
    /// `Rotate` as (w, x, y, z).
    pub rotate: Option<[f64; 4]>,
    pub scale: Vec3,
    pub points: Vec<Vec3>,
    /// `Rec._PathType` (0 when absent).
    pub path_type: String,
    /// `Closed="1"`.
    pub closed: bool,
}

impl AiPathEntity {
    /// True for the spawner-relative paths.
    pub fn is_spawner_relative(&self) -> bool {
        self.pos[0].abs() < RELATIVE_POS_LIMIT && self.pos[1].abs() < RELATIVE_POS_LIMIT
    }

    /// True when `Rotate` actually turns the points.
    pub fn is_rotated(&self) -> bool {
        self.rotate
            .is_some_and(|q| q[1].abs() > 1e-6 || q[2].abs() > 1e-6 || q[3].abs() > 1e-6)
    }
}

fn parse_floats(text: &str) -> Option<Vec<f64>> {
    text.split(',')
        .map(|t| t.trim().parse::<f64>().ok())
        .collect()
}

fn vec3(text: &str) -> Option<Vec3> {
    match parse_floats(text)?.as_slice() {
        [x, y, z] => Some([*x, *y, *z]),
        _ => None,
    }
}

/// The result of reading one `npc.xml`.
#[derive(Debug, Default)]
pub struct ParsedNpcXml {
    pub entities: Vec<AiPathEntity>,
    /// Entities that were skipped because of malformed attributes.
    pub malformed: Vec<String>,
}

/// Reads every `<aiPath>` of an editor `npc.xml`.
pub fn parse_npc_xml(xml: &str) -> Result<ParsedNpcXml, roxmltree::Error> {
    let xml = xml.strip_prefix('\u{feff}').unwrap_or(xml);
    let doc = roxmltree::Document::parse(xml)?;
    let mut out = ParsedNpcXml::default();
    for node in doc.descendants().filter(|n| n.has_tag_name("aiPath")) {
        let name = node.attribute("Name").unwrap_or_default().to_string();
        let Some(pos) = node.attribute("Pos").and_then(vec3) else {
            out.malformed.push(name);
            continue;
        };
        let rotate = match node.attribute("Rotate") {
            None => None,
            Some(t) => match parse_floats(t).as_deref() {
                Some([w, x, y, z]) => Some([*w, *x, *y, *z]),
                _ => {
                    out.malformed.push(name);
                    continue;
                }
            },
        };
        let scale = match node.attribute("Scale") {
            None => [1.0, 1.0, 1.0],
            Some(t) => match parse_floats(t).as_deref() {
                Some([s]) => [*s, *s, *s],
                Some([x, y, z]) => [*x, *y, *z],
                _ => {
                    out.malformed.push(name);
                    continue;
                }
            },
        };
        let mut points = Vec::new();
        let mut bad = false;
        for p in node.descendants().filter(|n| n.has_tag_name("Point")) {
            match p.attribute("Pos").and_then(vec3) {
                Some(v) => points.push(v),
                None => bad = true,
            }
        }
        if bad {
            out.malformed.push(name);
            continue;
        }
        out.entities.push(AiPathEntity {
            name,
            pos,
            rotate,
            scale,
            points,
            path_type: node.attribute("Rec._PathType").unwrap_or("0").to_string(),
            closed: node.attribute("Closed") == Some("1"),
        });
    }
    Ok(out)
}

/// `<Zone id=".." originX=".." originY="..">` of a `world.xml`: zone key to origin cell.
pub fn parse_world_origins(xml: &str) -> Result<HashMap<u32, (i32, i32)>, roxmltree::Error> {
    let xml = xml.strip_prefix('\u{feff}').unwrap_or(xml);
    let doc = roxmltree::Document::parse(xml)?;
    let mut out = HashMap::new();
    for z in doc.descendants().filter(|n| n.has_tag_name("Zone")) {
        let id = z.attribute("id").and_then(|v| v.parse::<u32>().ok());
        let ox = z.attribute("originX").and_then(|v| v.parse::<i32>().ok());
        let oy = z.attribute("originY").and_then(|v| v.parse::<i32>().ok());
        if let (Some(id), Some(ox), Some(oy)) = (id, ox, oy) {
            out.insert(id, (ox, oy));
        }
    }
    Ok(out)
}

fn rotate_vec(q: [f64; 4], v: Vec3) -> Vec3 {
    let norm = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if norm < 1e-12 {
        return v;
    }
    let (w, x, y, z) = (q[0] / norm, q[1] / norm, q[2] / norm, q[3] / norm);
    // t = 2 * cross(q.xyz, v); v' = v + w * t + cross(q.xyz, t)
    let tx = 2.0 * (y * v[2] - z * v[1]);
    let ty = 2.0 * (z * v[0] - x * v[2]);
    let tz = 2.0 * (x * v[1] - y * v[0]);
    [
        v[0] + w * tx + (y * tz - z * ty),
        v[1] + w * ty + (z * tx - x * tz),
        v[2] + w * tz + (x * ty - y * tx),
    ]
}

/// World coordinates of the entity's points: the zone origin cell, the entity position and the
/// scaled and rotated local point.
pub fn to_world(
    entity: &AiPathEntity,
    origin: (i32, i32),
    convention: RotationConvention,
) -> Vec<Vec3> {
    let q = entity.rotate.map(|q| match convention {
        RotationConvention::Direct => q,
        RotationConvention::Inverse => [q[0], -q[1], -q[2], -q[3]],
    });
    let ox = f64::from(origin.0) * CELL_SIZE;
    let oy = f64::from(origin.1) * CELL_SIZE;
    entity
        .points
        .iter()
        .map(|p| {
            let mut v = [
                p[0] * entity.scale[0],
                p[1] * entity.scale[1],
                p[2] * entity.scale[2],
            ];
            if let Some(q) = q {
                v = rotate_vec(q, v);
            }
            [
                ox + entity.pos[0] + v[0],
                oy + entity.pos[1] + v[1],
                entity.pos[2] + v[2],
            ]
        })
        .collect()
}

fn fmt_coord(v: f64) -> String {
    let s = format!("{v:.3}");
    if s == "-0.000" {
        "0.000".to_string()
    } else {
        s
    }
}

/// The `.path` file text: one `|x|y|z|` line per point, LF line ends.
pub fn format_path(points: &[Vec3]) -> String {
    let mut out = String::new();
    for p in points {
        out.push_str(&format!(
            "|{}|{}|{}|\n",
            fmt_coord(p[0]),
            fmt_coord(p[1]),
            fmt_coord(p[2])
        ));
    }
    out
}

/// The points of an existing `.path` file (`|x|y|z|` lines; other lines are skipped).
pub fn parse_path_points(text: &str) -> Vec<Vec3> {
    let mut out = Vec::new();
    for line in text.lines() {
        let c: Vec<&str> = line.split('|').collect();
        if c.len() != 5 {
            continue;
        }
        let f = |i: usize| c[i].trim().parse::<f64>().unwrap_or(0.0);
        out.push([f(1), f(2), f(3)]);
    }
    out
}

fn dist_to_segment(p: Vec3, a: Vec3, b: Vec3) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0)
    };
    (p[0] - a[0] - t * dx).hypot(p[1] - a[1] - t * dy)
}

/// Horizontal distance from `p` to the polyline (or the single point).
pub fn dist_to_polyline(p: Vec3, line: &[Vec3]) -> f64 {
    match line {
        [] => f64::INFINITY,
        [a] => (p[0] - a[0]).hypot(p[1] - a[1]),
        _ => line
            .windows(2)
            .map(|w| dist_to_segment(p, w[0], w[1]))
            .fold(f64::INFINITY, f64::min),
    }
}

/// How far an existing (hand recorded) route is from a generated one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Distance {
    pub max: f64,
    pub mean: f64,
}

/// Horizontal distance of every point of `existing` to the `generated` polyline.
pub fn route_distance(existing: &[Vec3], generated: &[Vec3]) -> Option<Distance> {
    if existing.is_empty() || generated.is_empty() {
        return None;
    }
    let d: Vec<f64> = existing
        .iter()
        .map(|p| dist_to_polyline(*p, generated))
        .collect();
    Some(Distance {
        max: d.iter().copied().fold(0.0, f64::max),
        mean: d.iter().sum::<f64>() / d.len() as f64,
    })
}

/// One entity found in a zone of a world.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub world: String,
    pub zone: u32,
    pub entity: AiPathEntity,
}

/// Zone origins: world name to (zone key to origin cell).
pub type WorldOrigins = BTreeMap<String, HashMap<u32, (i32, i32)>>;

#[derive(Debug, Clone, Copy)]
pub struct PlanOptions {
    pub convention: RotationConvention,
    /// Leave out the entities that have a real `Rotate`.
    pub skip_rotated: bool,
}

/// Why a candidate or a name was not converted.
#[derive(Debug, Default)]
pub struct Report {
    /// Demanded names that have no entity in the 10.x data at all.
    pub missing_in_10x: BTreeSet<String>,
    /// Reason to the demanded names (spelling as in the database) it applies to.
    pub skipped: BTreeMap<String, BTreeSet<String>>,
    /// File stems that were not written because a file of that name already exists.
    pub existing_kept: BTreeSet<String>,
    /// Demanded names with a `Rec._PathType` other than 0 or a `Closed` flag.
    pub unusual: BTreeSet<String>,
    /// Number of files with a plain name and number of `name@zone` variants.
    pub unique_files: usize,
    pub variant_files: usize,
}

/// The files to write (stem to text) and the report.
#[derive(Debug, Default)]
pub struct Plan {
    pub files: BTreeMap<String, String>,
    pub report: Report,
}

fn skip(report: &mut Report, reason: &str, spelling: &str) {
    report
        .skipped
        .entry(reason.to_string())
        .or_default()
        .insert(spelling.to_string());
}

/// Chooses what to write.
///
/// * `demand`: lower-case name to the spellings used by the database.
/// * `candidates`: lower-case entity name to every entity of that name in the 10.x pak.
/// * `origins12` / `origins10`: zone origins of the 1.2 and 10.x `world.xml` files; a zone is
///   converted only when the 1.2 data knows it and both versions agree on its origin.
/// * `existing`: lower-case stems of the files already in the output folder (never overwritten).
pub fn plan(
    demand: &BTreeMap<String, BTreeSet<String>>,
    candidates: &HashMap<String, Vec<Candidate>>,
    origins12: &WorldOrigins,
    origins10: &WorldOrigins,
    existing: &BTreeSet<String>,
    options: PlanOptions,
) -> Plan {
    let mut plan = Plan::default();
    for (lower, spellings) in demand {
        let first = spellings.iter().next().cloned().unwrap_or_default();
        let Some(cands) = candidates.get(lower) else {
            plan.report.missing_in_10x.insert(first);
            continue;
        };
        if existing.contains(lower) {
            for s in spellings {
                plan.report.existing_kept.insert(s.clone());
            }
            // The server prefers `<name>.path`, so no variants either.
        }
        // Lowest zone key of every distinct route text.
        let mut groups: BTreeMap<String, u32> = BTreeMap::new();
        let mut any_usable = false;
        // Why candidates were dropped; reported only when none of the name's entities is usable.
        let mut dropped: BTreeSet<&str> = BTreeSet::new();
        for c in cands {
            let (Some(o12), Some(o10)) = (
                origins12.get(&c.world).and_then(|z| z.get(&c.zone)),
                origins10.get(&c.world).and_then(|z| z.get(&c.zone)),
            ) else {
                dropped.insert("world or zone not in the 1.2 world.xml");
                continue;
            };
            if o12 != o10 {
                dropped.insert("zone origin differs between 1.2 and 10.x");
                continue;
            }
            if c.entity.is_spawner_relative() {
                dropped.insert("spawner-relative path");
                continue;
            }
            if options.skip_rotated && c.entity.is_rotated() {
                dropped.insert("rotated, left out");
                continue;
            }
            if c.entity.points.len() < 2 {
                dropped.insert("fewer than two points");
                continue;
            }
            any_usable = true;
            if c.entity.path_type != "0" || c.entity.closed {
                plan.report.unusual.insert(first.clone());
            }
            let text = format_path(&to_world(&c.entity, *o12, options.convention));
            let zone = groups.entry(text).or_insert(c.zone);
            *zone = (*zone).min(c.zone);
        }
        if !any_usable {
            for reason in dropped {
                skip(&mut plan.report, reason, &first);
            }
            continue;
        }
        if existing.contains(lower) {
            continue;
        }
        if groups.len() == 1 {
            let (text, _) = groups.into_iter().next().expect("one group");
            for s in spellings {
                plan.files.insert(s.clone(), text.clone());
                plan.report.unique_files += 1;
            }
        } else {
            for (text, zone) in groups {
                for s in spellings {
                    plan.files.insert(format!("{s}@{zone}"), text.clone());
                    plan.report.variant_files += 1;
                }
            }
        }
    }
    plan
}

/// Distance of each existing route to the generated route of the same entity name (the closest
/// of the candidates when the name occurs several times).
pub fn compare_existing(
    existing: &BTreeMap<String, Vec<Vec3>>,
    candidates: &HashMap<String, Vec<Candidate>>,
    origins12: &WorldOrigins,
    options: PlanOptions,
) -> BTreeMap<String, Distance> {
    let mut out = BTreeMap::new();
    for (stem, points) in existing {
        let Some(cands) = candidates.get(&stem.to_lowercase()) else {
            continue;
        };
        let mut best: Option<Distance> = None;
        for c in cands {
            let Some(origin) = origins12.get(&c.world).and_then(|z| z.get(&c.zone)) else {
                continue;
            };
            if c.entity.is_spawner_relative() {
                continue;
            }
            let generated = to_world(&c.entity, *origin, options.convention);
            if let Some(d) = route_distance(points, &generated) {
                if best.is_none_or(|b| d.max < b.max) {
                    best = Some(d);
                }
            }
        }
        if let Some(d) = best {
            out.insert(stem.clone(), d);
        }
    }
    out
}
