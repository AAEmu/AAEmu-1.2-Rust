//! `verify`: runs one `.vtest` suite — several named bots whose script sections run
//! concurrently and synchronise through the `sync` script command.
//!
//! The format (documented in `Docs/wiki/Verification.md`):
//!
//! ```text
//! # comments
//! suite <display name>            # optional, defaults to the file stem
//! bot <id> [gm]                   # one per bot; `gm` is provisioned by Scripts/verify.sh
//!
//! [<id>]
//! <script command>                # everything else is script.rs syntax
//! ```
//!
//! `${<id>}` in a command expands to that bot's character name. The account / character name
//! is `v` + the first 8 alphanumerics of the file stem + the bot id (at most 18 characters,
//! the server's character-name limit).
//!
//! Every bot logs in on its own, so the suite exercises the real login and game protocols.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};

use crate::record::Recorder;
use crate::scenario::{self, BotConfig, ScriptInput, StepResult};

/// One declared bot.
#[derive(Clone, Debug)]
pub struct BotDecl {
    pub id: String,
    /// The suite runner does not grant access; `Scripts/verify.sh` reads this through
    /// `verify --list-bots` and sets `accounts.access_level` before the run.
    pub gm: bool,
}

/// A parsed suite.
#[derive(Clone, Debug)]
pub struct Suite {
    /// Display name (`suite` directive, else the file stem).
    pub name: String,
    /// Account / character prefix derived from the file stem.
    pub code: String,
    pub bots: Vec<BotDecl>,
    scripts: BTreeMap<String, Vec<String>>,
}

/// One bot's result.
#[derive(Clone, Debug)]
pub struct BotReport {
    pub id: String,
    pub ok: bool,
    pub error: Option<String>,
    pub steps: Vec<StepResult>,
}

/// The result of one suite run.
#[derive(Clone, Debug)]
pub struct SuiteReport {
    pub name: String,
    pub passed: bool,
    pub duration_ms: u64,
    pub bots: Vec<BotReport>,
}

impl SuiteReport {
    /// First failure as a single line (for reports).
    pub fn failing_line(&self) -> Option<String> {
        self.bots
            .iter()
            .find(|b| !b.ok)
            .map(|b| format!("bot {}: {}", b.id, b.error.as_deref().unwrap_or("failed")))
    }
}

fn code_of(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("suite")
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Account / character name of a bot (`v` + suite code + bot id).
pub fn account_name(code: &str, bot: &str) -> String {
    format!("v{code}{bot}")
}

impl Suite {
    /// Parses a `.vtest` file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("suite")
            .to_string();
        let code = code_of(path);
        ensure_code(&code)?;
        let mut name = stem;
        let mut bots: Vec<BotDecl> = Vec::new();
        let mut scripts: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut current: Option<String> = None;
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(id) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                let id = id.trim();
                if !bots.iter().any(|b| b.id == id) {
                    bail!("{}:{}: unknown bot section [{id}]", path.display(), n + 1);
                }
                scripts.entry(id.to_string()).or_default();
                current = Some(id.to_string());
                continue;
            }
            if line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("suite ") {
                if current.is_some() {
                    bail!(
                        "{}:{}: `suite` must come before the bot sections",
                        path.display(),
                        n + 1
                    );
                }
                name = rest.trim().to_string();
                continue;
            }
            if let Some(rest) = line.strip_prefix("bot ") {
                if current.is_some() {
                    bail!(
                        "{}:{}: `bot` must come before the bot sections",
                        path.display(),
                        n + 1
                    );
                }
                let mut parts = rest.split_whitespace();
                let id = parts
                    .next()
                    .with_context(|| format!("{}:{}: bot needs an id", path.display(), n + 1))?;
                if !id.chars().all(|c| c.is_ascii_alphanumeric()) {
                    bail!(
                        "{}:{}: bot id '{id}' must be ASCII letters and digits",
                        path.display(),
                        n + 1
                    );
                }
                if bots.iter().any(|b| b.id == id) {
                    bail!("{}:{}: duplicate bot '{id}'", path.display(), n + 1);
                }
                let gm = match parts.next() {
                    None => false,
                    Some("gm") => true,
                    Some(other) => bail!(
                        "{}:{}: expected `bot <id> [gm]`, not '{other}'",
                        path.display(),
                        n + 1
                    ),
                };
                if account_name(&code, id).len() > 18 {
                    bail!(
                        "{}:{}: account {} is longer than the 18-character character-name limit",
                        path.display(),
                        n + 1,
                        account_name(&code, id)
                    );
                }
                bots.push(BotDecl {
                    id: id.to_string(),
                    gm,
                });
                continue;
            }
            match &current {
                Some(id) => scripts
                    .get_mut(id)
                    .expect("declared")
                    .push(line.to_string()),
                None => bail!(
                    "{}:{}: '{line}' is outside a bot section",
                    path.display(),
                    n + 1
                ),
            }
        }
        if bots.is_empty() {
            bail!("{}: no `bot <id>` declared", path.display());
        }
        for b in &bots {
            if scripts.get(&b.id).is_none_or(Vec::is_empty) {
                bail!(
                    "{}: bot '{}' has no [{}] section",
                    path.display(),
                    b.id,
                    b.id
                );
            }
        }
        Ok(Self {
            name,
            code,
            bots,
            scripts,
        })
    }

    /// Account / character name of a bot in this suite.
    pub fn account(&self, bot: &str) -> String {
        account_name(&self.code, bot)
    }

    fn script(&self, bot: &str, names: &BTreeMap<String, String>) -> String {
        let mut text = String::new();
        for line in self.scripts.get(bot).into_iter().flatten() {
            let mut expanded = line.clone();
            for (id, name) in names {
                expanded = expanded.replace(&format!("${{{id}}}"), name);
            }
            text.push_str(&expanded);
            text.push('\n');
        }
        text
    }
}

fn ensure_code(code: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !code.is_empty() && code.chars().all(|c| c.is_ascii_alphanumeric()),
        "suite file stem must contain letters or digits"
    );
    Ok(())
}

/// `verify --list-bots`: one `<account> <0|1>` line per bot (Scripts/verify.sh provisions them).
pub fn print_bots(path: &Path) -> anyhow::Result<()> {
    let suite = Suite::load(path)?;
    for bot in &suite.bots {
        println!("{} {}", suite.account(&bot.id), if bot.gm { 1 } else { 0 });
    }
    Ok(())
}

fn bot_config(login: &str, password: &str, user: &str) -> anyhow::Result<BotConfig> {
    Ok(serde_json::from_value(serde_json::json!({
        "login": login,
        "user": user,
        "password": password,
        "character": user,
    }))?)
}

/// Runs one suite file; each bot is a concurrent task.
pub async fn run(path: &Path, login: &str, password: &str) -> anyhow::Result<SuiteReport> {
    let suite = Suite::load(path)?;
    let start = Instant::now();
    let names: BTreeMap<String, String> = suite
        .bots
        .iter()
        .map(|b| (b.id.clone(), suite.account(&b.id)))
        .collect();
    let sync_dir = std::env::temp_dir().join(format!(
        "aaemu-verify-{}-{}",
        std::process::id(),
        suite.code
    ));
    let _ = std::fs::remove_dir_all(&sync_dir);
    std::fs::create_dir_all(&sync_dir)?;
    let label = path.display().to_string();
    let bots = suite.bots.len();
    let mut tasks = Vec::new();
    for bot in &suite.bots {
        let cfg = bot_config(login, password, &suite.account(&bot.id))?;
        let text = suite.script(&bot.id, &names);
        let bot_id = bot.id.clone();
        let label = label.clone();
        let sync_dir: PathBuf = sync_dir.clone();
        tasks.push(tokio::spawn(async move {
            let out = {
                let input = ScriptInput {
                    text: &text,
                    label: &label,
                    bot: &bot_id,
                    bots,
                    sync_dir: Some(sync_dir.as_path()),
                };
                let rec = Recorder::new(true);
                scenario::run_script(&cfg, &rec, &input).await
            };
            BotReport {
                id: bot_id,
                ok: out.ok(),
                error: out.error,
                steps: out.steps,
            }
        }));
    }
    println!("suite {}: {bots} bot(s)", suite.name);
    let mut reports = Vec::new();
    for t in tasks {
        match t.await {
            Ok(r) => {
                for s in &r.steps {
                    println!(
                        "[{}] {:<5} {:<20} {:>6} ms  {}",
                        r.id,
                        if s.ok { "ok" } else { "FAIL" },
                        s.name,
                        s.ms,
                        s.note
                    );
                }
                reports.push(r);
            }
            Err(e) => reports.push(BotReport {
                id: "?".into(),
                ok: false,
                error: Some(format!("bot task failed: {e}")),
                steps: Vec::new(),
            }),
        }
    }
    let _ = std::fs::remove_dir_all(&sync_dir);
    let duration_ms = start.elapsed().as_millis() as u64;
    let passed = reports.iter().all(|r| r.ok);
    for r in reports.iter().filter(|r| !r.ok) {
        println!(
            "FAIL {}: bot {}: {}",
            suite.name,
            r.id,
            r.error.as_deref().unwrap_or("failed")
        );
    }
    println!(
        "{} {} ({bots} bot(s), {:.1} s)",
        if passed { "PASS" } else { "FAIL" },
        suite.name,
        Duration::from_millis(duration_ms).as_secs_f64()
    );
    Ok(SuiteReport {
        name: suite.name,
        passed,
        duration_ms,
        bots: reports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suite_file(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aaemu-verify-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn parses_bots_sections_and_expands_names() {
        let path = suite_file(
            "guild-guards.vtest",
            "# c\nsuite Guild guards\nbot leader gm\nbot partner\n\n[leader]\n\
             partyinvite ${partner}\nsync ready 2\n\n[partner]\npartyaccept\n",
        );
        let suite = Suite::load(&path).unwrap();
        assert_eq!(suite.name, "Guild guards");
        assert_eq!(suite.code, "guildgua");
        assert_eq!(suite.bots.len(), 2);
        assert!(suite.bots[0].gm);
        assert!(!suite.bots[1].gm);
        assert_eq!(suite.account("leader"), "vguildgualeader");
        let names = BTreeMap::from([
            ("leader".to_string(), "vguildgualeader".to_string()),
            ("partner".to_string(), "vguildguapartner".to_string()),
        ]);
        assert_eq!(
            suite.script("leader", &names),
            "partyinvite vguildguapartner\nsync ready 2\n"
        );
    }

    #[test]
    fn rejects_unknown_section_and_oversized_account() {
        let path = suite_file("x.vtest", "bot a\n[a]\nwaitpeer 1 b\n[b]\nwaitpeer 1 a\n");
        assert!(Suite::load(&path).is_err());
        let path = suite_file("long-name.vtest", "bot abcdefghij\n[abcdefghij]\nme\n");
        assert!(Suite::load(&path).is_err());
    }
}
