//! `game_pak` operations: list, extract, add (append-only with a restore point) and restore.
//!
//! The pak layout is `[file data][encrypted file table][padding][0x200 header]`. Adding files
//! writes the new data starting where the old file table began and then writes a new file
//! table + header behind it. That overwrites the old file table, so before the first change a
//! *restore point* is saved next to the pak (`<pak>.aaemu-restore`: the original table + header
//! region and the original length). [`restore`] writes it back and truncates, which returns the
//! pak to its exact original bytes. Existing file data is never modified: a changed file is
//! appended as a new copy and the old extent is only listed as an unused entry.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use aaemu_commons::aapak::{AAPak, PakFileInfo};
use anyhow::{bail, ensure, Context, Result};
use md5::{Digest, Md5};
use sha2::Sha256;

const RESTORE_MAGIC: &[u8; 16] = b"AAEMU-PAKRST-001";
/// How much of the unmodified data region is fingerprinted into the restore point.
const FINGERPRINT_LEN: u64 = 64 * 1024;

/// Lower-cases and normalises a pak path (`/` separators, no leading slash).
pub fn normalize(path: &str) -> String {
    path.trim()
        .replace('\\', "/")
        .trim_start_matches('/')
        .to_lowercase()
}

/// Path of the restore point of `pak`.
pub fn restore_path(pak: &Path) -> PathBuf {
    let mut s = pak.as_os_str().to_os_string();
    s.push(".aaemu-restore");
    PathBuf::from(s)
}

pub fn md5_of_file(path: &Path) -> Result<[u8; 16]> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Md5::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().into())
}

/// MD5 of a pak entry. Original paks often leave the table's MD5 unset (all zeros); then the
/// entry's data is hashed instead.
fn stored_md5(pak: &AAPak, name: &str, info: &PakFileInfo) -> Result<[u8; 16]> {
    if info.md5 != [0u8; 16] {
        return Ok(info.md5);
    }
    let data = pak
        .read_file(name)?
        .with_context(|| format!("{name} vanished from the pak"))?;
    Ok(Md5::digest(&data).into())
}

pub fn hex16(b: &[u8; 16]) -> String {
    hex::encode(b)
}

/// Case-insensitive view of the used files of a pak: lower-case path -> stored name.
pub struct PakIndex {
    by_lower: HashMap<String, String>,
}

impl PakIndex {
    pub fn new(pak: &AAPak) -> Self {
        let mut by_lower = HashMap::with_capacity(pak.file_count());
        for name in pak.file_names() {
            by_lower.insert(normalize(name), name.to_string());
        }
        Self { by_lower }
    }

    /// Stored name of `path` (any case / separator).
    pub fn stored_name(&self, path: &str) -> Option<&str> {
        self.by_lower.get(&normalize(path)).map(String::as_str)
    }

    pub fn contains(&self, path: &str) -> bool {
        self.by_lower.contains_key(&normalize(path))
    }
}

pub fn open_read(path: &Path) -> Result<AAPak> {
    AAPak::open(path, true).with_context(|| format!("open pak {}", path.display()))
}

// ---- info -------------------------------------------------------------------------------------

/// One-line description of the pak layout (type, counts, where the file table starts).
pub fn info(pak: &AAPak, len: u64) -> String {
    format!(
        "type={:?} files={} unused={} fat_offset={} length={} restore_point={}",
        pak.pak_type(),
        pak.file_count(),
        pak.extra_files.len(),
        pak.header.first_file_info_offset,
        len,
        restore_path(pak.path()).exists()
    )
}

// ---- list -------------------------------------------------------------------------------------

/// Used files whose lower-cased name starts with `prefix` (case-insensitive), sorted by name.
pub fn list(pak: &AAPak, prefix: &str) -> Vec<PakFileInfo> {
    let prefix = normalize(prefix);
    let mut out: Vec<PakFileInfo> = pak
        .files()
        .filter(|f| normalize(&f.name).starts_with(&prefix))
        .cloned()
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Writes `name<TAB>size<TAB>md5<TAB>offset` lines.
pub fn write_listing(files: &[PakFileInfo], w: &mut impl Write) -> Result<()> {
    for f in files {
        writeln!(w, "{}\t{}\t{}\t{}", f.name, f.size, hex16(&f.md5), f.offset)?;
    }
    Ok(())
}

// ---- extract ----------------------------------------------------------------------------------

/// Names selected by `spec`: the exact file, or every file below the directory prefix `spec`.
pub fn select(pak: &AAPak, spec: &str) -> Vec<String> {
    let spec = normalize(spec);
    let dir = format!("{}/", spec.trim_end_matches('/'));
    let mut out: Vec<String> = pak
        .file_names()
        .filter(|n| {
            let l = normalize(n);
            l == spec || l.starts_with(&dir)
        })
        .map(str::to_string)
        .collect();
    out.sort();
    out
}

fn output_path(outdir: &Path, name: &str) -> Result<PathBuf> {
    let rel = Path::new(name);
    let mut p = outdir.to_path_buf();
    for c in rel.components() {
        match c {
            Component::Normal(part) => p.push(part),
            other => bail!("unsafe pak path {name:?} ({other:?})"),
        }
    }
    ensure!(!name.contains(':'), "unsafe pak path {name:?}");
    Ok(p)
}

/// Extracts `names` (stored names) below `outdir`, one file at a time. `lowercase` lower-cases
/// the output paths. Returns the written `(stored name, output path)` pairs.
pub fn extract_names(
    pak: &AAPak,
    names: &[String],
    outdir: &Path,
    lowercase: bool,
) -> Result<Vec<(String, PathBuf)>> {
    let mut written = Vec::with_capacity(names.len());
    for name in names {
        let data = pak
            .read_file(name)
            .with_context(|| format!("read {name} from pak"))?
            .with_context(|| format!("{name} not in pak"))?;
        let rel = if lowercase {
            normalize(name)
        } else {
            name.replace('\\', "/")
        };
        let dest = output_path(outdir, &rel)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &data).with_context(|| format!("write {}", dest.display()))?;
        written.push((name.clone(), dest));
    }
    Ok(written)
}

/// `pak extract`: exact file or directory prefix.
pub fn extract(pak: &AAPak, spec: &str, outdir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let names = select(pak, spec);
    ensure!(!names.is_empty(), "no file or prefix {spec:?} in the pak");
    extract_names(pak, &names, outdir, false)
}

// ---- add --------------------------------------------------------------------------------------

/// Files below `srcdir` as `(lower-cased pak path, source file)`, sorted.
pub fn collect_dir(srcdir: &Path) -> Result<Vec<(String, PathBuf)>> {
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, PathBuf>) -> Result<()> {
        for e in std::fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
            let e = e?;
            let p = e.path();
            if e.file_type()?.is_dir() {
                walk(&p, base, out)?;
            } else {
                let rel = p.strip_prefix(base)?.to_string_lossy().replace('\\', "/");
                out.insert(normalize(&rel), p);
            }
        }
        Ok(())
    }
    let mut map = BTreeMap::new();
    walk(srcdir, srcdir, &mut map)?;
    Ok(map.into_iter().collect())
}

#[derive(Debug, Default, Clone)]
pub struct AddReport {
    pub added: Vec<String>,
    pub replaced: Vec<String>,
    pub unchanged: usize,
    pub bytes: u64,
    pub restore_point: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
pub struct AddOptions {
    /// Allow files that exist with different content (appended as a new copy). When false a
    /// differing file is an error.
    pub replace: bool,
}

impl Default for AddOptions {
    fn default() -> Self {
        Self { replace: true }
    }
}

fn fingerprint(file: &mut File, data_len: u64) -> Result<[u8; 32]> {
    let n = data_len.min(FINGERPRINT_LEN) as usize;
    let mut buf = vec![0u8; n];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut buf)?;
    Ok(Sha256::digest(&buf).into())
}

/// Saves the restore point unless one exists (then it must belong to this pak).
fn save_restore_point(pak_path: &Path, pak: &AAPak) -> Result<PathBuf> {
    let rp = restore_path(pak_path);
    let mut file = File::open(pak_path)?;
    if rp.exists() {
        // Keep the earliest restore point: it describes the pristine pak.
        let rest = read_restore(&rp)?;
        ensure!(
            fingerprint(&mut file, rest.fat_offset)? == rest.fingerprint,
            "{} was taken from a different pak (data fingerprint differs); delete it or run `pak restore`",
            rp.display()
        );
        return Ok(rp);
    }
    let fat_offset = pak.header.first_file_info_offset as u64;
    let fp = fingerprint(&mut file, fat_offset)?;
    let region = pak.header.fat.clone();
    let orig_len = fat_offset + region.len() as u64;
    ensure!(
        file.metadata()?.len() == orig_len,
        "pak length does not match header/file table size; refusing to patch"
    );
    let mut buf = Vec::with_capacity(region.len() + 80);
    buf.extend_from_slice(RESTORE_MAGIC);
    buf.extend_from_slice(&orig_len.to_le_bytes());
    buf.extend_from_slice(&fat_offset.to_le_bytes());
    buf.extend_from_slice(&fp);
    buf.extend_from_slice(&(region.len() as u64).to_le_bytes());
    buf.extend_from_slice(&region);
    let tmp = rp.with_extension("aaemu-restore.tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&buf)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &rp)?;
    tracing::info!(
        "saved restore point {} ({} bytes of file table)",
        rp.display(),
        region.len()
    );
    Ok(rp)
}

struct RestorePoint {
    orig_len: u64,
    fat_offset: u64,
    fingerprint: [u8; 32],
    region: Vec<u8>,
}

fn read_restore(path: &Path) -> Result<RestorePoint> {
    let buf = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    ensure!(
        buf.len() >= 16 + 8 + 8 + 32 + 8 && &buf[..16] == RESTORE_MAGIC,
        "{} is not a restore point",
        path.display()
    );
    let u64_at = |o: usize| u64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
    let orig_len = u64_at(16);
    let fat_offset = u64_at(24);
    let mut fingerprint = [0u8; 32];
    fingerprint.copy_from_slice(&buf[32..64]);
    let n = u64_at(64) as usize;
    ensure!(buf.len() == 72 + n, "{} is truncated", path.display());
    Ok(RestorePoint {
        orig_len,
        fat_offset,
        fingerprint,
        region: buf[72..].to_vec(),
    })
}

/// Adds / replaces `files` (`(pak path, source)`; pak paths are lower-cased) in the pak.
/// Identical files (size and MD5) are skipped; nothing is written if nothing changed.
pub fn add_files(
    pak_path: &Path,
    files: &[(String, PathBuf)],
    opts: AddOptions,
) -> Result<AddReport> {
    let mut pak = AAPak::open(pak_path, false)
        .with_context(|| format!("open pak {} for writing", pak_path.display()))?;
    let index = PakIndex::new(&pak);
    let mut report = AddReport::default();
    // (stored name to remove first, new name, source, size)
    let mut todo: Vec<(Option<String>, String, PathBuf, u64)> = Vec::new();
    let mut conflicts = Vec::new();
    for (name, src) in files {
        let name = normalize(name);
        let size = std::fs::metadata(src)
            .with_context(|| format!("stat {}", src.display()))?
            .len();
        match index.stored_name(&name) {
            Some(stored) => {
                let info = pak.get_file_by_name(stored).context("index out of sync")?;
                if info.size as u64 == size && stored_md5(&pak, stored, info)? == md5_of_file(src)?
                {
                    report.unchanged += 1;
                } else if opts.replace {
                    todo.push((Some(stored.to_string()), name, src.clone(), size));
                } else {
                    conflicts.push(name);
                }
            }
            None => todo.push((None, name, src.clone(), size)),
        }
    }
    ensure!(
        conflicts.is_empty(),
        "{} file(s) already exist in the pak with different content (use replace to append new copies): {}",
        conflicts.len(),
        conflicts.join(", ")
    );
    if todo.is_empty() {
        tracing::info!(
            "pak add: nothing to do ({} files already present)",
            report.unchanged
        );
        return Ok(report);
    }

    report.restore_point = Some(save_restore_point(pak_path, &pak)?);

    // Never reuse "unused" extents: that would overwrite old bytes the restore point does not hold.
    let mut extras = std::mem::take(&mut pak.extra_files);
    let mut failure: Option<anyhow::Error> = None;
    for (old, name, src, size) in &todo {
        if let Some(old) = old {
            pak.delete_file(old);
            extras.append(&mut pak.extra_files);
        }
        match pak.add_file_from_file(src, name, false) {
            Ok(_) => {
                report.bytes += size;
                if old.is_some() {
                    report.replaced.push(name.clone());
                } else {
                    report.added.push(name.clone());
                }
            }
            Err(e) => {
                // Drop a half-written entry so the table written below stays consistent.
                pak.delete_file(name);
                extras.append(&mut pak.extra_files);
                failure = Some(anyhow::Error::new(e).context(format!("add {name}")));
                break;
            }
        }
    }
    pak.extra_files = extras;
    pak.close().context("write file table")?;
    if let Some(e) = failure {
        return Err(e.context(format!(
            "pak add stopped early; the pak holds the files added before the failure and can be \
             reset with `pak restore {}`",
            pak_path.display()
        )));
    }

    // Verify through a fresh read of the written table.
    let check = open_read(pak_path)?;
    for (_, name, src, size) in &todo {
        let info = check
            .get_file_by_name(name)
            .with_context(|| format!("{name} missing after write"))?;
        ensure!(
            info.size as u64 == *size,
            "{name}: size mismatch after write"
        );
        ensure!(
            info.md5 == md5_of_file(src)?,
            "{name}: md5 mismatch after write"
        );
    }
    tracing::info!(
        "pak add: {} added, {} replaced, {} unchanged, {} bytes",
        report.added.len(),
        report.replaced.len(),
        report.unchanged,
        report.bytes
    );
    Ok(report)
}

/// `pak add <pak> <srcdir>`.
pub fn add_dir(pak_path: &Path, srcdir: &Path, opts: AddOptions) -> Result<AddReport> {
    let files = collect_dir(srcdir)?;
    ensure!(!files.is_empty(), "{} contains no files", srcdir.display());
    add_files(pak_path, &files, opts)
}

// ---- restore ----------------------------------------------------------------------------------

/// Writes the saved file table back and truncates the pak to its original length.
pub fn restore(pak_path: &Path) -> Result<()> {
    let rp = restore_path(pak_path);
    ensure!(rp.exists(), "no restore point {}", rp.display());
    let rest = read_restore(&rp)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(pak_path)
        .with_context(|| format!("open {}", pak_path.display()))?;
    ensure!(
        file.metadata()?.len() >= rest.fat_offset,
        "pak is shorter than the restore point's file table offset"
    );
    ensure!(
        fingerprint(&mut file, rest.fat_offset)? == rest.fingerprint,
        "pak data differs from the one the restore point was taken from"
    );
    file.seek(SeekFrom::Start(rest.fat_offset))?;
    file.write_all(&rest.region)?;
    file.set_len(rest.orig_len)?;
    file.sync_all()?;
    drop(file);
    std::fs::remove_file(&rp)?;
    tracing::info!("restored {} to {} bytes", pak_path.display(), rest.orig_len);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;
    use chrono::{TimeZone, Utc};
    use std::io::Cursor;

    fn bytes(n: usize, seed: usize) -> Vec<u8> {
        (0..n).map(|i| ((i * 13 + seed) % 251) as u8).collect()
    }

    /// Synthetic pak with a few "original" files, as a stand-in for the real client pak.
    fn make_pak(dir: &Path) -> PathBuf {
        let path = dir.join("game_pak");
        let t = Utc.with_ymd_and_hms(2020, 1, 2, 3, 4, 5).unwrap();
        let mut pak = AAPak::create(&path).unwrap();
        for (i, n) in ["game/a/one.bin", "game/a/two.bin", "game/b/three.txt"]
            .iter()
            .enumerate()
        {
            pak.add_file_from_stream(n, &mut Cursor::new(bytes(1000 + i * 700, i)), t, t, true)
                .unwrap();
        }
        pak.close().unwrap();
        path
    }

    fn write_src(dir: &Path, rel: &str, data: &[u8]) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    #[test]
    fn add_list_extract_restore_round_trip() {
        let tmp = TempDir::new("pak_rt");
        let pak_path = make_pak(tmp.path());
        let original = std::fs::read(&pak_path).unwrap();

        let src = tmp.path().join("src");
        write_src(&src, "Game/New/Alpha.BIN", &bytes(5000, 9));
        write_src(&src, "game/new/sub/beta.txt", b"hello beta");
        write_src(&src, "game/a/one.bin", &bytes(1000, 0)); // identical to the pak copy

        let r = add_dir(&pak_path, &src, AddOptions::default()).unwrap();
        assert_eq!(r.added, vec!["game/new/alpha.bin", "game/new/sub/beta.txt"]);
        assert_eq!(r.unchanged, 1);
        assert!(restore_path(&pak_path).exists());

        // list
        let pak = open_read(&pak_path).unwrap();
        let listed = list(&pak, "GAME/NEW");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].size, 5000);
        assert_eq!(listed[1].md5, <[u8; 16]>::from(Md5::digest(b"hello beta")));
        assert_eq!(pak.file_count(), 5);
        let mut text = Vec::new();
        write_listing(&listed, &mut text).unwrap();
        assert!(String::from_utf8(text)
            .unwrap()
            .contains("game/new/alpha.bin\t5000\t"));

        // extract (single file and prefix)
        let out = tmp.path().join("out");
        let got = extract(&pak, "game/new/sub/beta.txt", &out).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(
            std::fs::read(out.join("game/new/sub/beta.txt")).unwrap(),
            b"hello beta"
        );
        let got = extract(&pak, "game/new", &out).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(
            std::fs::read(out.join("game/new/alpha.bin")).unwrap(),
            bytes(5000, 9)
        );
        // original files still readable and unchanged
        assert_eq!(
            pak.read_file("game/a/two.bin").unwrap().unwrap(),
            bytes(1700, 1)
        );
        drop(pak);

        // idempotent: re-adding is a no-op and leaves the file untouched
        let before = std::fs::read(&pak_path).unwrap();
        let r = add_dir(&pak_path, &src, AddOptions::default()).unwrap();
        assert!(r.added.is_empty() && r.replaced.is_empty());
        assert_eq!(r.unchanged, 3);
        assert_eq!(std::fs::read(&pak_path).unwrap(), before);

        // restore returns the exact original bytes
        restore(&pak_path).unwrap();
        assert_eq!(std::fs::read(&pak_path).unwrap(), original);
        assert!(!restore_path(&pak_path).exists());
    }

    #[test]
    fn replace_appends_and_restore_is_exact() {
        let tmp = TempDir::new("pak_repl");
        let pak_path = make_pak(tmp.path());
        let original = std::fs::read(&pak_path).unwrap();

        let src = tmp.path().join("src");
        write_src(&src, "game/a/one.bin", &bytes(300, 77)); // smaller, different content
        let strict = AddOptions { replace: false };
        let err = add_dir(&pak_path, &src, strict).unwrap_err().to_string();
        assert!(err.contains("different content"), "{err}");
        assert_eq!(std::fs::read(&pak_path).unwrap(), original);

        let r = add_dir(&pak_path, &src, AddOptions::default()).unwrap();
        assert_eq!(r.replaced, vec!["game/a/one.bin"]);
        // second change keeps the first restore point
        write_src(&src, "game/c/new.bin", &bytes(40, 1));
        add_dir(&pak_path, &src, AddOptions::default()).unwrap();
        let pak = open_read(&pak_path).unwrap();
        assert_eq!(
            pak.read_file("game/a/one.bin").unwrap().unwrap(),
            bytes(300, 77)
        );
        assert!(pak.file_exists("game/c/new.bin"));
        drop(pak);

        restore(&pak_path).unwrap();
        assert_eq!(std::fs::read(&pak_path).unwrap(), original);
    }

    /// Original paks often store an all-zero MD5; identical files must still count as unchanged.
    #[test]
    fn unset_table_md5_is_compared_by_content() {
        let tmp = TempDir::new("pak_zero_md5");
        let pak_path = make_pak(tmp.path());
        {
            let mut pak = AAPak::open(&pak_path, false).unwrap();
            for n in ["game/a/one.bin", "game/a/two.bin"] {
                assert!(pak.set_md5(n, &[0u8; 16]));
            }
            pak.close().unwrap();
        }
        let src = tmp.path().join("src");
        write_src(&src, "game/a/one.bin", &bytes(1000, 0));
        write_src(&src, "game/a/two.bin", &bytes(1700, 99)); // same size, other content
        let before = std::fs::read(&pak_path).unwrap();
        let strict = AddOptions { replace: false };
        let err = add_dir(&pak_path, &src, strict).unwrap_err().to_string();
        assert!(
            err.contains("game/a/two.bin") && !err.contains("one.bin"),
            "{err}"
        );
        assert_eq!(std::fs::read(&pak_path).unwrap(), before);
    }

    #[test]
    fn extract_rejects_unknown_and_unsafe() {
        let tmp = TempDir::new("pak_ex");
        let pak_path = make_pak(tmp.path());
        let pak = open_read(&pak_path).unwrap();
        assert!(extract(&pak, "game/nothing", tmp.path()).is_err());
        assert!(output_path(tmp.path(), "../evil").is_err());
        assert!(output_path(tmp.path(), "c:/evil").is_err());
    }
}
