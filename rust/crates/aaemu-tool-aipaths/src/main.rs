//! `aaemu-aipaths`: generates `Data/Path/<name>.path` AI route files from the 10.x client.
//!
//! The 10.x `game_pak` keeps the designers' AI paths in
//! `game/worlds/<world>/level_design/zone/<zone key>/editor/npc.xml`. The route names the 1.2
//! database asks for (`quest_components.ai_path_name`, `ai_commands` FollowPath, `npc_control_effects` FollowPath) are looked up
//! there and written as `|x|y|z|` files in world coordinates. The `return-points` mode writes
//! the `worldgates.json` entries of the `Return` special effects from the 10.x
//! `return_point.xml` files. See `README.md`.

mod convert;
#[cfg(test)]
mod returnpoint_tests;
mod returnpoints;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use aaemu_commons::aapak::AAPak;
use anyhow::{bail, Context, Result};

use crate::convert::{
    compare_existing, parse_npc_xml, parse_path_points, parse_world_origins, plan, Candidate,
    PlanOptions, RotationConvention, WorldOrigins,
};

const USAGE: &str = "Usage: aaemu-aipaths --pak10 <10.x game_pak> --pak12 <1.2 game_pak> \
--compact <compact.sqlite3> --out <Data/Path folder> [--dry-run] [--skip-rotated] \
[--rotation direct|inverse]";

#[derive(Debug)]
struct Options {
    pak10: PathBuf,
    pak12: PathBuf,
    compact: PathBuf,
    out: PathBuf,
    dry_run: bool,
    skip_rotated: bool,
    convention: RotationConvention,
}

fn parse_args(args: &[String]) -> Result<Options> {
    let mut pak10 = None;
    let mut pak12 = None;
    let mut compact = None;
    let mut out = None;
    let mut dry_run = false;
    let mut skip_rotated = false;
    let mut convention = RotationConvention::Direct;
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
            "--out" => out = Some(value("--out")?),
            "--dry-run" => dry_run = true,
            "--skip-rotated" => skip_rotated = true,
            "--rotation" => {
                convention = match value("--rotation")?.to_string_lossy().as_ref() {
                    "direct" => RotationConvention::Direct,
                    "inverse" => RotationConvention::Inverse,
                    other => bail!("unknown rotation convention {other}"),
                }
            }
            other => bail!("unknown argument {other}\n{USAGE}"),
        }
    }
    Ok(Options {
        pak10: pak10.with_context(|| USAGE)?,
        pak12: pak12.with_context(|| USAGE)?,
        compact: compact.with_context(|| USAGE)?,
        out: out.with_context(|| USAGE)?,
        dry_run,
        skip_rotated,
        convention,
    })
}

/// The route names the database asks for: lower-case name to the spellings in use.
pub fn load_demand(compact: &Path) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let conn =
        rusqlite::Connection::open_with_flags(compact, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open {}", compact.display()))?;
    demand_from(&conn)
}

fn demand_from(conn: &rusqlite::Connection) -> Result<BTreeMap<String, BTreeSet<String>>> {
    use rusqlite::types::Value;
    let mut names: BTreeSet<String> = BTreeSet::new();
    let mut stmt =
        conn.prepare("SELECT ai_path_name FROM quest_components WHERE ai_path_name IS NOT NULL")?;
    for row in stmt.query_map([], |r| r.get::<_, Value>(0))? {
        if let Value::Text(t) = row? {
            names.insert(t.trim().to_string());
        }
    }
    // cmd_id 2 = FollowPath; param2 is the route name (a number for the other commands).
    let mut stmt = conn.prepare("SELECT param2 FROM ai_commands WHERE cmd_id = 2")?;
    for row in stmt.query_map([], |r| r.get::<_, Value>(0))? {
        if let Value::Text(t) = row? {
            names.insert(t.trim().to_string());
        }
    }
    // category_id 2 = FollowPath; param_string is the route name (NpcControllEffect).
    let mut stmt =
        conn.prepare("SELECT param_string FROM npc_control_effects WHERE category_id = 2")?;
    for row in stmt.query_map([], |r| r.get::<_, Value>(0))? {
        if let Value::Text(t) = row? {
            names.insert(t.trim().to_string());
        }
    }
    let mut demand: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for n in names {
        if n.is_empty() || n.parse::<i64>().is_ok() {
            continue;
        }
        demand.entry(n.to_lowercase()).or_default().insert(n);
    }
    Ok(demand)
}

fn read_text(pak: &AAPak, name: &str) -> Option<String> {
    let bytes = pak.read_file(name).ok().flatten()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Every `level_design/zone/<key>/editor/npc.xml` of the pak: (pak file name, world, zone key).
fn npc_xml_files(pak: &AAPak) -> Vec<(String, String, u32)> {
    let mut out = Vec::new();
    for f in pak.files() {
        let lower = f.name.to_lowercase().replace('\\', "/");
        let Some(rest) = lower.strip_prefix("game/worlds/") else {
            continue;
        };
        let parts: Vec<&str> = rest.split('/').collect();
        if let [_, "level_design", "zone", key, "editor", "npc.xml"] = parts.as_slice() {
            if let Ok(zone) = key.parse::<u32>() {
                out.push((f.name.clone(), parts[0].to_string(), zone));
            }
        }
    }
    out.sort_by(|a, b| (&a.1, a.2).cmp(&(&b.1, b.2)));
    out
}

fn world_origins(pak: &AAPak, worlds: &BTreeSet<String>) -> WorldOrigins {
    let mut out = WorldOrigins::new();
    for w in worlds {
        let Some(text) = read_text(pak, &format!("game/worlds/{w}/world.xml")) else {
            continue;
        };
        match parse_world_origins(&text) {
            Ok(o) => {
                out.insert(w.clone(), o);
            }
            Err(e) => eprintln!("warning: {w}/world.xml: {e}"),
        }
    }
    out
}

fn existing_stems(out: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut stems = BTreeMap::new();
    if !out.is_dir() {
        return Ok(stems);
    }
    for e in fs::read_dir(out).with_context(|| format!("read {}", out.display()))? {
        let p = e?.path();
        let is_path = p
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("path"));
        if let (true, Some(stem)) = (is_path, p.file_stem().and_then(|s| s.to_str())) {
            stems.insert(stem.to_string(), p.clone());
        }
    }
    Ok(stems)
}

fn run(opts: &Options) -> Result<()> {
    let demand = load_demand(&opts.compact)?;
    println!("demanded route names: {}", demand.len());

    let pak10 = AAPak::open(&opts.pak10, true).context("open the 10.x game_pak")?;
    let pak12 = AAPak::open(&opts.pak12, true).context("open the 1.2 game_pak")?;

    let files = npc_xml_files(&pak10);
    let worlds: BTreeSet<String> = files.iter().map(|f| f.1.clone()).collect();
    println!(
        "10.x npc.xml files: {} in {} worlds",
        files.len(),
        worlds.len()
    );
    let origins10 = world_origins(&pak10, &worlds);
    let origins12 = world_origins(&pak12, &worlds);
    println!(
        "worlds with zone origins: {} in 10.x, {} in 1.2",
        origins10.len(),
        origins12.len()
    );

    let mut candidates: HashMap<String, Vec<Candidate>> = HashMap::new();
    let mut malformed = 0usize;
    for (name, world, zone) in &files {
        let Some(text) = read_text(&pak10, name) else {
            eprintln!("warning: cannot read {name}");
            continue;
        };
        let parsed = match parse_npc_xml(&text) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("warning: {name}: {e}");
                continue;
            }
        };
        malformed += parsed.malformed.len();
        for entity in parsed.entities {
            candidates
                .entry(entity.name.to_lowercase())
                .or_default()
                .push(Candidate {
                    world: world.clone(),
                    zone: *zone,
                    entity,
                });
        }
    }
    println!(
        "aiPath names in 10.x: {} ({malformed} malformed entities skipped)",
        candidates.len()
    );

    let existing = existing_stems(&opts.out)?;
    let existing_lower: BTreeSet<String> = existing.keys().map(|s| s.to_lowercase()).collect();
    let options = PlanOptions {
        convention: opts.convention,
        skip_rotated: opts.skip_rotated,
    };
    let plan = plan(
        &demand,
        &candidates,
        &origins12,
        &origins10,
        &existing_lower,
        options,
    );

    // Distance of the hand recorded files to the generated route of the same name.
    let mut recorded: BTreeMap<String, Vec<[f64; 3]>> = BTreeMap::new();
    for (stem, file) in &existing {
        let text = fs::read(file).map(|b| String::from_utf8_lossy(&b).into_owned());
        recorded.insert(stem.clone(), parse_path_points(&text.unwrap_or_default()));
    }
    let distances = compare_existing(&recorded, &candidates, &origins12, options);

    if !opts.dry_run {
        fs::create_dir_all(&opts.out)?;
        for (stem, text) in &plan.files {
            let target = opts.out.join(format!("{stem}.path"));
            // `create_new`: an existing file is never overwritten.
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
            {
                Ok(mut f) => f.write_all(text.as_bytes())?,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    eprintln!("kept existing {}", target.display());
                }
                Err(e) => return Err(e).with_context(|| target.display().to_string()),
            }
        }
    }

    let r = &plan.report;
    println!(
        "files{}: {} ({} plain, {} name@zone variants)",
        if opts.dry_run {
            " (dry run)"
        } else {
            " written"
        },
        plan.files.len(),
        r.unique_files,
        r.variant_files
    );
    println!("kept existing (demanded): {}", r.existing_kept.len());
    println!("missing in 10.x: {}", r.missing_in_10x.len());
    for n in &r.missing_in_10x {
        println!("  missing {n}");
    }
    for (reason, names) in &r.skipped {
        println!("skipped, {reason}: {}", names.len());
        for n in names {
            println!("  {n}");
        }
    }
    println!(
        "Rec._PathType other than 0 or Closed set (not encoded): {}",
        r.unusual.len()
    );
    println!("existing files vs generated route (horizontal metres):");
    for (stem, d) in &distances {
        println!("  {stem}: max {:.1}, mean {:.1}", d.max, d.mean);
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("return-points") => {
            returnpoints::parse_args(&args[1..]).and_then(|o| returnpoints::run(&o))
        }
        _ => parse_args(&args).and_then(|o| run(&o)),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
