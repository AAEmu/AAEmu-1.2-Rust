//! Backport bundles.
//!
//! A bundle is a directory of committed text:
//!
//! ```text
//! bundle.json          {name, version, description, sql: [...], assets: {...}}
//! assets.txt           donor (10.x) pak paths to copy, one per line (`#` comments)
//! prefab_subsets.tsv   old_lib <TAB> new_lib <TAB> prefab name: new prefab libraries holding
//!                      only the listed prefabs (the 1.2 library of the same name stays untouched)
//! sql/NNN_*.sql        ordered SQL patches (additive: only rows absent in 1.2)
//! ```
//!
//! `bundle build` turns that plus the donor pak into a staging directory (lower-case pak paths
//! plus `_bundle.json`, a manifest with sizes and MD5s). `bundle apply` writes the staging files
//! into a client's `game_pak` and applies the SQL patches to the client and/or server database.
//! No client asset is ever checked into git; the staging directory is a build product.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

use crate::dbpatch::{self, PatchOutcome};
use crate::pakops::{self, AddOptions, PakIndex};

pub const MANIFEST_NAME: &str = "_bundle.json";

#[derive(Debug, Clone, Deserialize)]
pub struct BundleSpec {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub sql: Vec<SqlEntry>,
    #[serde(default)]
    pub assets: AssetsSpec,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AssetsSpec {
    /// Text file listing donor pak paths to copy.
    pub donor_files: Option<String>,
    /// TSV (`old_lib new_lib prefab`) describing subset prefab libraries to generate.
    pub prefab_subsets: Option<String>,
}

/// A SQL patch: a plain path (applies to client and server) or `{file, targets}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SqlEntry {
    Path(String),
    Full {
        file: String,
        #[serde(default)]
        targets: Option<Vec<String>>,
    },
}

impl SqlEntry {
    pub fn file(&self) -> &str {
        match self {
            SqlEntry::Path(p) => p,
            SqlEntry::Full { file, .. } => file,
        }
    }

    pub fn applies_to(&self, target: &str) -> bool {
        match self {
            SqlEntry::Path(_) => true,
            SqlEntry::Full { targets, .. } => targets
                .as_ref()
                .is_none_or(|t| t.iter().any(|x| x == target)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub size: u64,
    pub md5: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub bundle: String,
    pub version: String,
    pub files: Vec<ManifestFile>,
}

pub fn load_spec(bundle_dir: &Path) -> Result<BundleSpec> {
    let p = bundle_dir.join("bundle.json");
    let text = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    let spec: BundleSpec =
        serde_json::from_str(&text).with_context(|| format!("parse {}", p.display()))?;
    ensure!(
        !spec.name.is_empty()
            && spec
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)),
        "bundle name {:?} must be non-empty [A-Za-z0-9._-]",
        spec.name
    );
    Ok(spec)
}

/// Parses `assets.txt`: one path per line, `#` comments, blank lines ignored; lower-cased and
/// de-duplicated.
pub fn parse_asset_list(text: &str) -> Vec<String> {
    let mut seen = BTreeMap::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if !line.is_empty() {
            seen.insert(pakops::normalize(line), ());
        }
    }
    seen.into_keys().collect()
}

/// One subset library to generate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubsetLib {
    pub old_lib: String,
    pub new_lib: String,
    pub prefabs: Vec<String>,
}

/// Parses `prefab_subsets.tsv` (header line optional; `old_lib\tnew_lib\tprefab_name`).
pub fn parse_prefab_subsets(text: &str) -> Result<Vec<SubsetLib>> {
    let mut map: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty()
            || line.starts_with('#')
            || (i == 0 && line.starts_with("old_lib"))
        {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        ensure!(
            f.len() == 3,
            "prefab_subsets line {}: expected 3 tab-separated fields",
            i + 1
        );
        let e = map
            .entry((pakops::normalize(f[0]), pakops::normalize(f[1])))
            .or_default();
        if !e.iter().any(|p| p == f[2]) {
            e.push(f[2].to_string());
        }
    }
    Ok(map
        .into_iter()
        .map(|((old_lib, new_lib), prefabs)| SubsetLib {
            old_lib,
            new_lib,
            prefabs,
        })
        .collect())
}

/// Builds a prefab library holding only `wanted` prefabs of `text`, byte-preserving for the
/// kept `<Prefab>` elements. The root `Name` attribute becomes `new_name`.
pub fn subset_library(text: &str, new_name: &str, wanted: &[String]) -> Result<String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let doc = roxmltree::Document::parse(text).context("parse prefab library")?;
    let root = doc.root_element();
    let first = root
        .children()
        .next()
        .context("prefab library has no children")?;
    let header_end = first.range().start;
    let mut header = String::new();
    match root.attributes().find(|a| a.name() == "Name") {
        Some(a) => {
            let v = a.range_value();
            header.push_str(&text[..v.start]);
            header.push_str(new_name);
            header.push_str(&text[v.end..header_end]);
        }
        None => header.push_str(&text[..header_end]),
    }
    let mut out = header;
    let mut found: Vec<&str> = Vec::new();
    for el in root
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "Prefab")
    {
        if let Some(name) = el.attribute("Name") {
            if wanted.iter().any(|w| w == name) {
                if !out.ends_with('\n') && !out.ends_with(char::is_whitespace) {
                    out.push('\n');
                }
                out.push_str(&text[el.range()]);
                out.push('\n');
                found.push(name);
            }
        }
    }
    let missing: Vec<&String> = wanted
        .iter()
        .filter(|w| !found.contains(&w.as_str()))
        .collect();
    ensure!(
        missing.is_empty(),
        "prefabs not found in the donor library: {missing:?}"
    );
    out.push_str(&format!("</{}>\n", root.tag_name().name()));
    Ok(out)
}

#[derive(Debug, Default)]
pub struct BuildReport {
    pub donor_files: usize,
    pub generated_files: usize,
    pub unchanged: usize,
    pub bytes: u64,
}

fn write_if_changed(dest: &Path, data: &[u8]) -> Result<bool> {
    if let Ok(existing) = std::fs::read(dest) {
        if existing == data {
            return Ok(false);
        }
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, data).with_context(|| format!("write {}", dest.display()))?;
    Ok(true)
}

/// `bundle build`: stages the donor files and generated libraries below `out`.
pub fn build(bundle_dir: &Path, donor_pak: &Path, out: &Path) -> Result<BuildReport> {
    let spec = load_spec(bundle_dir)?;
    let list_text = match &spec.assets.donor_files {
        Some(f) => std::fs::read_to_string(bundle_dir.join(f))
            .with_context(|| format!("read asset list {f}"))?,
        None => String::new(),
    };
    let wanted = parse_asset_list(&list_text);
    let subsets = match &spec.assets.prefab_subsets {
        Some(f) => parse_prefab_subsets(
            &std::fs::read_to_string(bundle_dir.join(f))
                .with_context(|| format!("read prefab subsets {f}"))?,
        )?,
        None => Vec::new(),
    };
    for s in &subsets {
        ensure!(
            !wanted.contains(&s.new_lib),
            "{} is both a donor file and a generated library",
            s.new_lib
        );
    }
    tracing::info!(
        "bundle {} {}: {} donor files, {} generated libraries",
        spec.name,
        spec.version,
        wanted.len(),
        subsets.len()
    );

    let pak = pakops::open_read(donor_pak)?;
    let index = PakIndex::new(&pak);
    let missing: Vec<&String> = wanted.iter().filter(|w| !index.contains(w)).collect();
    ensure!(
        missing.is_empty(),
        "{} donor file(s) not in {}: {}",
        missing.len(),
        donor_pak.display(),
        missing
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    std::fs::create_dir_all(out)?;

    let mut report = BuildReport::default();
    let mut manifest = Manifest {
        bundle: spec.name.clone(),
        version: spec.version.clone(),
        files: Vec::new(),
    };
    for name in &wanted {
        let stored = index
            .stored_name(name)
            .context("index out of sync")?
            .to_string();
        let info = pak
            .get_file_by_name(&stored)
            .context("entry vanished")?
            .clone();
        let dest = out.join(name);
        let up_to_date = dest.is_file()
            && std::fs::metadata(&dest)?.len() == info.size as u64
            && pakops::md5_of_file(&dest)? == info.md5;
        if up_to_date {
            report.unchanged += 1;
        } else {
            pakops::extract_names(&pak, &[stored], out, true)?;
            report.donor_files += 1;
        }
        report.bytes += info.size as u64;
        manifest.files.push(ManifestFile {
            path: name.clone(),
            size: info.size as u64,
            md5: pakops::hex16(&info.md5),
        });
    }
    for s in &subsets {
        let stored = index
            .stored_name(&s.old_lib)
            .with_context(|| format!("library {} not in the donor pak", s.old_lib))?;
        let bytes = pak.read_file(stored)?.context("library vanished")?;
        let text = String::from_utf8(bytes).context("library is not UTF-8")?;
        let base = Path::new(&s.new_lib)
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .context("bad library name")?;
        let new_text = subset_library(&text, &base, &s.prefabs)
            .with_context(|| format!("subset of {}", s.old_lib))?;
        let dest = out.join(&s.new_lib);
        write_if_changed(&dest, new_text.as_bytes())?;
        report.generated_files += 1;
        report.bytes += new_text.len() as u64;
        manifest.files.push(ManifestFile {
            path: s.new_lib.clone(),
            size: new_text.len() as u64,
            md5: pakops::hex16(&md5_bytes(new_text.as_bytes())),
        });
    }
    manifest.files.sort_by(|a, b| a.path.cmp(&b.path));
    std::fs::write(
        out.join(MANIFEST_NAME),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    Ok(report)
}

fn md5_bytes(data: &[u8]) -> [u8; 16] {
    use md5::{Digest, Md5};
    Md5::digest(data).into()
}

/// Loads the manifest and verifies every staged file (size and MD5).
pub fn verify_staging(
    staging: &Path,
    spec: &BundleSpec,
) -> Result<(Manifest, Vec<(String, PathBuf)>)> {
    let mp = staging.join(MANIFEST_NAME);
    let manifest: Manifest = serde_json::from_str(
        &std::fs::read_to_string(&mp)
            .with_context(|| format!("read {} (run `bundle build` first)", mp.display()))?,
    )?;
    ensure!(
        manifest.bundle == spec.name,
        "staging was built for bundle {:?}, not {:?}",
        manifest.bundle,
        spec.name
    );
    ensure!(
        manifest.version == spec.version,
        "staging was built for version {}, bundle is {}; rebuild it",
        manifest.version,
        spec.version
    );
    let mut files = Vec::with_capacity(manifest.files.len());
    for f in &manifest.files {
        let p = staging.join(&f.path);
        let meta =
            std::fs::metadata(&p).with_context(|| format!("staged file {} missing", f.path))?;
        ensure!(
            meta.len() == f.size,
            "{}: size differs from the manifest",
            f.path
        );
        ensure!(
            pakops::hex16(&pakops::md5_of_file(&p)?) == f.md5,
            "{}: md5 differs from the manifest",
            f.path
        );
        files.push((f.path.clone(), p));
    }
    Ok((manifest, files))
}

#[derive(Debug, Default)]
pub struct ApplyReport {
    pub pak: Option<pakops::AddReport>,
    pub client_sql: Vec<(String, PatchOutcome)>,
    pub server_sql: Vec<(String, PatchOutcome)>,
}

#[derive(Debug, Default)]
pub struct ApplyTargets {
    /// Client root (holds `game_pak` and `game/db/compact.sqlite3`).
    pub client: Option<PathBuf>,
    /// Server `compact.sqlite3`.
    pub server_db: Option<PathBuf>,
    /// Allow replacing files that exist in the client pak with other content.
    pub allow_replace: bool,
    /// Patch only the client database, leave `game_pak` alone (client still running, or the pak
    /// is patched separately).
    pub no_pak: bool,
}

fn sql_for(bundle_dir: &Path, spec: &BundleSpec, target: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for e in spec.sql.iter().filter(|e| e.applies_to(target)) {
        let p = bundle_dir.join(e.file());
        let sql = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        out.push((format!("{}/{}", spec.name, e.file()), sql));
    }
    Ok(out)
}

/// `bundle apply`.
pub fn apply(bundle_dir: &Path, staging: &Path, targets: &ApplyTargets) -> Result<ApplyReport> {
    ensure!(
        targets.client.is_some() || targets.server_db.is_some(),
        "nothing to do: pass --client and/or --server-db"
    );
    let spec = load_spec(bundle_dir)?;
    let mut report = ApplyReport::default();

    // Read everything and check it before the first write.
    let client_sql = sql_for(bundle_dir, &spec, "client")?;
    let server_sql = sql_for(bundle_dir, &spec, "server")?;
    let staged = if targets.client.is_some() && !targets.no_pak && !spec_assets_empty(&spec) {
        Some(verify_staging(staging, &spec)?)
    } else {
        None
    };

    if let Some(root) = &targets.client {
        let pak_path = root.join("game_pak");
        let db_path = root.join("game").join("db").join("compact.sqlite3");
        ensure!(
            targets.no_pak || pak_path.is_file(),
            "{} not found",
            pak_path.display()
        );
        ensure!(db_path.is_file(), "{} not found", db_path.display());
        if let Some((_, files)) = &staged {
            let opts = AddOptions {
                replace: targets.allow_replace,
            };
            report.pak = Some(pakops::add_files(&pak_path, files, opts)?);
        }
        report.client_sql = apply_sql(&db_path, &client_sql)?;
    }
    if let Some(db) = &targets.server_db {
        report.server_sql = apply_sql(db, &server_sql)?;
    }
    Ok(report)
}

fn spec_assets_empty(spec: &BundleSpec) -> bool {
    spec.assets.donor_files.is_none() && spec.assets.prefab_subsets.is_none()
}

fn apply_sql(db: &Path, patches: &[(String, String)]) -> Result<Vec<(String, PatchOutcome)>> {
    let mut conn = dbpatch::open(db)?;
    let mut out = Vec::new();
    for (name, sql) in patches {
        let o = dbpatch::apply_patch(&mut conn, name, sql, false)
            .with_context(|| format!("database {}", db.display()))?;
        tracing::info!("{}: {name}: {o:?}", db.display());
        out.push((name.clone(), o));
    }
    Ok(out)
}

impl ApplyReport {
    pub fn summary(&self) -> String {
        let mut s = String::new();
        if let Some(p) = &self.pak {
            s += &format!(
                "pak: {} added, {} replaced, {} unchanged, {} bytes written\n",
                p.added.len(),
                p.replaced.len(),
                p.unchanged,
                p.bytes
            );
            if let Some(rp) = &p.restore_point {
                s += &format!("pak restore point: {}\n", rp.display());
            }
        }
        for (label, list) in [
            ("client db", &self.client_sql),
            ("server db", &self.server_sql),
        ] {
            for (name, o) in list {
                s += &format!("{label}: {name}: {o:?}\n");
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;
    use aaemu_commons::aapak::AAPak;
    use chrono::{TimeZone, Utc};
    use rusqlite::Connection;
    use std::io::Cursor;

    const LIB: &str = "<?xml version=\"1.0\"?>\n<PrefabsLibrary Name=\"housing_big\" Version=\"2\">\n <Prefab Name=\"a.one\" Id=\"1\">\n  <Objects><Object Prefab=\"x.cgf\"/></Objects>\n </Prefab>\n <Prefab Name=\"a.two\" Id=\"2\"><Objects/></Prefab>\n <Prefab Name=\"a.three\" Id=\"3\"/>\n</PrefabsLibrary>\n";

    #[test]
    fn subset_keeps_only_wanted_prefabs_verbatim() {
        let out =
            subset_library(LIB, "housing_big_x10", &["a.one".into(), "a.three".into()]).unwrap();
        assert!(out.contains("Name=\"housing_big_x10\""));
        assert!(out.contains("<Prefab Name=\"a.one\" Id=\"1\">\n  <Objects><Object Prefab=\"x.cgf\"/></Objects>\n </Prefab>"));
        assert!(out.contains("<Prefab Name=\"a.three\" Id=\"3\"/>"));
        assert!(!out.contains("a.two"));
        assert!(out.ends_with("</PrefabsLibrary>\n"));
        roxmltree::Document::parse(&out).unwrap();
        assert!(subset_library(LIB, "n", &["nope".into()]).is_err());
    }

    #[test]
    fn parses_lists() {
        assert_eq!(
            parse_asset_list("# c\nGame/A/B.cgf  # x\n\ngame\\a\\b.cgf\ngame/z.dds\n"),
            vec!["game/a/b.cgf", "game/z.dds"]
        );
        let s = parse_prefab_subsets("old_lib\tnew_lib\tprefab_name\ngame/p/l.xml\tgame/p/l_x.xml\tA\ngame/p/l.xml\tgame/p/l_x.xml\tB\ngame/p/l.xml\tgame/p/l_x.xml\tA\n").unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].prefabs, vec!["A", "B"]);
    }

    fn donor_pak(dir: &Path) -> PathBuf {
        let path = dir.join("donor_pak");
        let t = Utc.with_ymd_and_hms(2020, 1, 2, 3, 4, 5).unwrap();
        let mut pak = AAPak::create(&path).unwrap();
        for (n, d) in [
            ("game/objects/Mesh.cgf", vec![7u8; 3000]),
            ("game/objects/tex.dds", vec![9u8; 1500]),
            ("game/prefabs/Housing_Big.xml", LIB.as_bytes().to_vec()),
        ] {
            pak.add_file_from_stream(n, &mut Cursor::new(d), t, t, false)
                .unwrap();
        }
        pak.close().unwrap();
        path
    }

    #[test]
    fn build_then_apply_to_client_and_server() {
        let tmp = TempDir::new("bundle");
        let t = tmp.path();
        // bundle
        let b = t.join("bundle");
        std::fs::create_dir_all(b.join("sql")).unwrap();
        std::fs::write(
            b.join("bundle.json"),
            r#"{"name":"demo","version":"1","description":"d",
                "sql":["sql/001_shared.sql",{"file":"sql/002_server.sql","targets":["server"]}],
                "assets":{"donor_files":"assets.txt","prefab_subsets":"subsets.tsv"}}"#,
        )
        .unwrap();
        std::fs::write(
            b.join("assets.txt"),
            "game/objects/mesh.cgf\ngame/objects/tex.dds\n",
        )
        .unwrap();
        std::fs::write(b.join("subsets.tsv"), "old_lib\tnew_lib\tprefab_name\ngame/prefabs/housing_big.xml\tgame/prefabs/housing_big_x10.xml\ta.two\n").unwrap();
        std::fs::write(
            b.join("sql/001_shared.sql"),
            "INSERT INTO things VALUES (2,'two');",
        )
        .unwrap();
        std::fs::write(
            b.join("sql/002_server.sql"),
            "INSERT INTO things VALUES (3,'three');",
        )
        .unwrap();
        // build
        let donor = donor_pak(t);
        let staging = t.join("staging");
        let r = build(&b, &donor, &staging).unwrap();
        assert_eq!((r.donor_files, r.generated_files), (2, 1));
        assert_eq!(
            std::fs::read(staging.join("game/objects/mesh.cgf")).unwrap(),
            vec![7u8; 3000]
        );
        assert!(staging.join("game/prefabs/housing_big_x10.xml").is_file());
        let r2 = build(&b, &donor, &staging).unwrap();
        assert_eq!((r2.donor_files, r2.unchanged), (0, 2));

        // client copy: pak + loose db
        let client = t.join("client");
        std::fs::create_dir_all(client.join("game/db")).unwrap();
        let orig_pak = {
            let p = client.join("game_pak");
            let t0 = Utc.with_ymd_and_hms(2020, 1, 2, 3, 4, 5).unwrap();
            let mut pak = AAPak::create(&p).unwrap();
            pak.add_file_from_stream(
                "game/old.bin",
                &mut Cursor::new(vec![1u8; 700]),
                t0,
                t0,
                false,
            )
            .unwrap();
            pak.close().unwrap();
            std::fs::read(&p).unwrap()
        };
        let mk_db = |p: &Path| {
            Connection::open(p)
                .unwrap()
                .execute_batch("CREATE TABLE things (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO things VALUES (1,'one');")
                .unwrap();
        };
        mk_db(&client.join("game/db/compact.sqlite3"));
        let server_db = t.join("server.sqlite3");
        mk_db(&server_db);

        let targets = ApplyTargets {
            client: Some(client.clone()),
            server_db: Some(server_db.clone()),
            allow_replace: false,
            no_pak: false,
        };
        let rep = apply(&b, &staging, &targets).unwrap();
        assert_eq!(rep.pak.as_ref().unwrap().added.len(), 3);
        assert_eq!(rep.client_sql.len(), 1);
        assert_eq!(rep.server_sql.len(), 2);
        let count = |p: &Path| -> i64 {
            Connection::open(p)
                .unwrap()
                .query_row("SELECT count(*) FROM things", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count(&client.join("game/db/compact.sqlite3")), 2);
        assert_eq!(count(&server_db), 3);
        let pak = pakops::open_read(&client.join("game_pak")).unwrap();
        assert!(pak.file_exists("game/objects/tex.dds"));
        drop(pak);

        // re-apply is a no-op
        let rep = apply(&b, &staging, &targets).unwrap();
        assert!(rep.pak.unwrap().added.is_empty());
        assert!(rep
            .server_sql
            .iter()
            .all(|(_, o)| *o == PatchOutcome::AlreadyApplied));

        // restore returns the pak to its original bytes
        pakops::restore(&client.join("game_pak")).unwrap();
        assert_eq!(std::fs::read(client.join("game_pak")).unwrap(), orig_pak);
    }
}
