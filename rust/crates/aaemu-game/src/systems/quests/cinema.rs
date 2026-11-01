//! Cinema metadata for the quest subsystem: the `cinemas` table plus, from the client files, the
//! `Flags` of each cinema's sequence (`game/worlds/<world>/cinemas/<name>.xml`).
//!
//! The 1.2 client only treats a sequence as a cut scene when flag `0x8` (`CUT_SCENE`) is set:
//! `BeginCutScene` (x2game.dll `0x3915B960`) then removes every unit except the local player and
//! clears all doodads, and nothing is restored when the cinema ends. Every other cinema (all
//! `sl_*` story videos, for one) removes nothing, so the server only has to re-send the visible
//! objects after a cut scene (see `PORTING.md`).

use std::collections::HashMap;

use rusqlite::Connection;

use crate::data::client_files::{decode_text, ClientFileManager};
use crate::data::sqlite::for_each_row;
use crate::data::GameDataError;

/// `Flags` bit of a cinema sequence that makes the client run it as a cut scene.
pub const SEQUENCE_FLAG_CUT_SCENE: u32 = 0x8;

/// Longest a `CSRequestPermissionToPlayCinemaForDirectingMode` (or `CinemaEffect`) stays
/// attributable to the next `CSStartedCinemaPacket` (ms). The client sends the request when the
/// quest dialog opens, but starts the cinema only when the player accepts / turns in, after
/// reading the bubbles (32 s in a live run, minutes are possible). A request older than 10
/// minutes belongs to a dialog the player abandoned.
pub const REQUEST_TTL_MS: i64 = 600_000;

/// One `cinemas` row with what the client files say about its sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CinemaInfo {
    pub id: u32,
    pub name: String,
    pub replay: bool,
    /// Whether the sequence is a CUT_SCENE (`Flags & 0x8`); `None` when no client file was found.
    pub cut_scene: Option<bool>,
}

/// Counts of one [`load_cinemas`] run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CinemaLoadStats {
    pub cinemas: usize,
    pub with_file: usize,
    pub cut_scenes: usize,
}

/// `Flags` attribute of the root `<Sequence ...>` tag of a cinema file; `None` when the tag or
/// the attribute is missing or not a number.
pub fn parse_sequence_flags(xml: &str) -> Option<u32> {
    let start = xml.find("<Sequence")? + "<Sequence".len();
    let rest = &xml[start..];
    // `<SequenceFoo` is another tag; a bare `<Sequence>` carries no attributes.
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let tag = &rest[..rest.find('>')?];
    attribute(tag, "Flags")?.trim().parse().ok()
}

/// Value of attribute `name` inside the text of one start tag.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(i) = tag[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        let separated = tag[..at]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace);
        if !separated {
            continue;
        }
        let Some(value) = tag[from..].trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim_start();
        let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        return value[1..].split(quote).next();
    }
    None
}

/// Name of the directory that holds `path` (`game/worlds/main_world/world.xml` ->
/// `main_world`), for pak names and OS paths alike.
fn parent_directory_name(path: &str) -> Option<&str> {
    let mut parts = path.rsplit(['/', '\\']);
    parts.next()?;
    parts.next().filter(|s| !s.is_empty())
}

/// Worlds that may hold cinemas: `main_world` first, then every `game/worlds/*/world.xml`.
fn world_names(files: &ClientFileManager) -> Vec<String> {
    let mut worlds = vec!["main_world".to_string()];
    for path in files.files_in_directory("game/worlds", "world.xml", true) {
        if let Some(world) = parent_directory_name(&path) {
            if !worlds.iter().any(|w| w == world) {
                worlds.push(world.to_string());
            }
        }
    }
    worlds
}

/// The `Flags` of cinema `name`'s sequence, from the first world that has the file.
fn sequence_flags(files: &ClientFileManager, worlds: &[String], name: &str) -> Option<u32> {
    worlds.iter().find_map(|world| {
        let bytes = files.read_file(&format!("game/worlds/{world}/cinemas/{name}.xml"))?;
        parse_sequence_flags(&decode_text(&bytes))
    })
}

/// Loads the `cinemas` table and the sequence flags of the cinemas whose xml is in the client
/// files. A cinema without a (readable) file stays in the map with `cut_scene: None`.
pub fn load_cinemas(
    conn: &Connection,
    files: &ClientFileManager,
) -> Result<HashMap<u32, CinemaInfo>, GameDataError> {
    let mut cinemas = HashMap::new();
    for_each_row(conn, "SELECT * FROM cinemas ORDER BY id", [], |r| {
        let info = CinemaInfo {
            id: r.u32("id")?,
            name: r.string_or("name", "")?,
            replay: r.bool_str("replay")?,
            cut_scene: None,
        };
        cinemas.insert(info.id, info);
        Ok(())
    })?;
    if !files.sources().is_empty() {
        let worlds = world_names(files);
        for info in cinemas.values_mut().filter(|c| !c.name.is_empty()) {
            info.cut_scene = sequence_flags(files, &worlds, &info.name)
                .map(|flags| flags & SEQUENCE_FLAG_CUT_SCENE != 0);
        }
    }
    let stats = stats(&cinemas);
    tracing::info!(
        "Loaded {} cinemas, {} with a client sequence file, {} of them cut scenes",
        stats.cinemas,
        stats.with_file,
        stats.cut_scenes
    );
    Ok(cinemas)
}

/// Counts for the startup log.
pub fn stats(cinemas: &HashMap<u32, CinemaInfo>) -> CinemaLoadStats {
    CinemaLoadStats {
        cinemas: cinemas.len(),
        with_file: cinemas.values().filter(|c| c.cut_scene.is_some()).count(),
        cut_scenes: cinemas
            .values()
            .filter(|c| c.cut_scene == Some(true))
            .count(),
    }
}
