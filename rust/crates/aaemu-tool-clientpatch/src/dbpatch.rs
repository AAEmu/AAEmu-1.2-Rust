//! Idempotent SQL patch applier for `compact.sqlite3`.
//!
//! Every patch is applied in its own transaction together with its bookkeeping row in
//! `aaemu_applied_patches(name, sha256, applied_at)`. A patch with the same name and hash is
//! skipped; the same name with a different hash is an error (the patch changed after it was
//! applied). Patch files must not contain their own `BEGIN` / `COMMIT`.
//!
//! SQLite has no `ADD COLUMN IF NOT EXISTS`, so a patch can declare columns with a directive
//! line: `-- aaemu:add-column <table> <column> <declaration>`. The column is added (before the
//! patch's statements run) only if the table does not have it yet.

use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

pub const PATCH_TABLE: &str = "aaemu_applied_patches";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchOutcome {
    Applied,
    AlreadyApplied,
    /// `dry_run`: the patch applied cleanly and was rolled back.
    DryRunOk,
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn open(path: &Path) -> Result<Connection> {
    if !path.is_file() {
        bail!("database {} does not exist", path.display());
    }
    let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    // The server's compact.sqlite3 declares foreign keys its own data already violates (e.g.
    // `guard_tower_setting_id = 0`), and the bundled SQLite enforces them by default.
    conn.pragma_update(None, "foreign_keys", false)?;
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {PATCH_TABLE} (\
            name TEXT PRIMARY KEY, sha256 TEXT NOT NULL, applied_at TEXT NOT NULL)"
    ))?;
    Ok(conn)
}

/// Applies one patch (`name` is the identity recorded in the bookkeeping table).
pub fn apply_patch(
    conn: &mut Connection,
    name: &str,
    sql: &str,
    dry_run: bool,
) -> Result<PatchOutcome> {
    let hash = sha256_hex(sql.as_bytes());
    let recorded: Option<String> = conn
        .query_row(
            &format!("SELECT sha256 FROM {PATCH_TABLE} WHERE name = ?1"),
            params![name],
            |r| r.get(0),
        )
        .optional()?;
    match recorded {
        Some(h) if h == hash => return Ok(PatchOutcome::AlreadyApplied),
        Some(h) => bail!(
            "patch {name} was already applied with different content (recorded sha256 {h}, now {hash})"
        ),
        None => {}
    }
    let tx = conn.transaction()?;
    for (table, column, decl) in add_column_directives(sql)? {
        let present: bool = tx
            .prepare(&format!("PRAGMA table_info(\"{table}\")"))?
            .query_map([], |r| r.get::<_, String>(1))?
            .any(|c| c.is_ok_and(|c| c.eq_ignore_ascii_case(&column)));
        if !present {
            tx.execute_batch(&format!(
                "ALTER TABLE \"{table}\" ADD COLUMN \"{column}\" {decl}"
            ))
            .with_context(|| format!("patch {name}: add column {table}.{column}"))?;
        }
    }
    tx.execute_batch(sql)
        .with_context(|| format!("applying patch {name}"))?;
    tx.execute(
        &format!(
            "INSERT INTO {PATCH_TABLE} (name, sha256, applied_at) VALUES (?1, ?2, datetime('now'))"
        ),
        params![name, hash],
    )?;
    if dry_run {
        tx.rollback()?;
        return Ok(PatchOutcome::DryRunOk);
    }
    tx.commit()?;
    Ok(PatchOutcome::Applied)
}

/// `-- aaemu:add-column <table> <column> <declaration>` lines of a patch.
fn add_column_directives(sql: &str) -> Result<Vec<(String, String, String)>> {
    let mut out = Vec::new();
    for line in sql.lines() {
        let Some(rest) = line.trim().strip_prefix("-- aaemu:add-column ") else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let (Some(table), Some(column)) = (parts.next(), parts.next()) else {
            bail!("bad add-column directive: {line}");
        };
        let decl = parts.collect::<Vec<_>>().join(" ");
        let ident = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !ident(table) || !ident(column) || decl.contains([';', '"', '\'']) {
            bail!("bad add-column directive: {line}");
        }
        out.push((table.to_string(), column.to_string(), decl));
    }
    Ok(out)
}

/// `db apply`: applies `files` in the given order, named after their file names.
pub fn apply_files(
    db: &Path,
    files: &[std::path::PathBuf],
    dry_run: bool,
) -> Result<Vec<(String, PatchOutcome)>> {
    let mut conn = open(db)?;
    let mut out = Vec::new();
    for f in files {
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .with_context(|| format!("bad patch path {}", f.display()))?;
        let sql = std::fs::read_to_string(f).with_context(|| format!("read {}", f.display()))?;
        let outcome = apply_patch(&mut conn, &name, &sql, dry_run)?;
        tracing::info!("{}: {name}: {outcome:?}", db.display());
        out.push((name, outcome));
    }
    Ok(out)
}

/// `PRAGMA integrity_check` (+ foreign key violations, informational) -> list of problems.
pub fn integrity_problems(db: &Path) -> Result<Vec<String>> {
    let conn = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare("PRAGMA integrity_check")?;
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows.into_iter().filter(|r| r != "ok").collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    fn temp_db(dir: &Path) -> std::path::PathBuf {
        let p = dir.join("compact.sqlite3");
        let c = Connection::open(&p).unwrap();
        c.execute_batch("CREATE TABLE things (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO things VALUES (1,'one');")
            .unwrap();
        p
    }

    fn count(db: &Path) -> i64 {
        Connection::open(db)
            .unwrap()
            .query_row("SELECT count(*) FROM things", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn applies_once_and_records() {
        let tmp = TempDir::new("db_apply");
        let db = temp_db(tmp.path());
        let sql = tmp.path().join("001_add.sql");
        std::fs::write(&sql, "INSERT INTO things VALUES (2,'two'),(3,'three');").unwrap();

        let r = apply_files(&db, std::slice::from_ref(&sql), false).unwrap();
        assert_eq!(r[0].1, PatchOutcome::Applied);
        assert_eq!(count(&db), 3);
        let r = apply_files(&db, std::slice::from_ref(&sql), false).unwrap();
        assert_eq!(r[0].1, PatchOutcome::AlreadyApplied);
        assert_eq!(count(&db), 3);

        let conn = Connection::open(&db).unwrap();
        let (hash, at): (String, String) = conn
            .query_row(
                "SELECT sha256, applied_at FROM aaemu_applied_patches WHERE name='001_add.sql'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            hash,
            sha256_hex(b"INSERT INTO things VALUES (2,'two'),(3,'three');")
        );
        assert!(!at.is_empty());
    }

    #[test]
    fn changed_patch_with_same_name_is_an_error() {
        let tmp = TempDir::new("db_changed");
        let db = temp_db(tmp.path());
        let sql = tmp.path().join("p.sql");
        std::fs::write(&sql, "INSERT INTO things VALUES (2,'two');").unwrap();
        apply_files(&db, std::slice::from_ref(&sql), false).unwrap();
        std::fs::write(&sql, "INSERT INTO things VALUES (9,'nine');").unwrap();
        let err = apply_files(&db, std::slice::from_ref(&sql), false).unwrap_err();
        assert!(err.to_string().contains("different content"), "{err}");
        assert_eq!(count(&db), 2);
    }

    #[test]
    fn failing_patch_rolls_back_entirely() {
        let tmp = TempDir::new("db_fail");
        let db = temp_db(tmp.path());
        let sql = tmp.path().join("bad.sql");
        // second statement collides with id 1
        std::fs::write(
            &sql,
            "INSERT INTO things VALUES (5,'five'); INSERT INTO things VALUES (1,'dup');",
        )
        .unwrap();
        assert!(apply_files(&db, std::slice::from_ref(&sql), false).is_err());
        assert_eq!(count(&db), 1);
        let conn = Connection::open(&db).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM aaemu_applied_patches", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn add_column_directive_is_idempotent() {
        let tmp = TempDir::new("db_col");
        let db = temp_db(tmp.path());
        let a = tmp.path().join("a.sql");
        let b = tmp.path().join("b.sql");
        std::fs::write(
            &a,
            "-- aaemu:add-column things extra INT
UPDATE things SET extra = 7 WHERE id = 1;",
        )
        .unwrap();
        // a second patch declaring the same column must not fail
        std::fs::write(
            &b,
            "-- aaemu:add-column things extra INT
UPDATE things SET extra = extra + 1 WHERE id = 1;",
        )
        .unwrap();
        apply_files(&db, &[a, b], false).unwrap();
        let v: i64 = Connection::open(&db)
            .unwrap()
            .query_row("SELECT extra FROM things WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 8);
        assert!(add_column_directives("-- aaemu:add-column t c INT; DROP TABLE x").is_err());
    }

    #[test]
    fn dry_run_rolls_back() {
        let tmp = TempDir::new("db_dry");
        let db = temp_db(tmp.path());
        let sql = tmp.path().join("p.sql");
        std::fs::write(&sql, "INSERT INTO things VALUES (2,'two');").unwrap();
        let r = apply_files(&db, std::slice::from_ref(&sql), true).unwrap();
        assert_eq!(r[0].1, PatchOutcome::DryRunOk);
        assert_eq!(count(&db), 1);
        assert!(integrity_problems(&db).unwrap().is_empty());
    }
}
