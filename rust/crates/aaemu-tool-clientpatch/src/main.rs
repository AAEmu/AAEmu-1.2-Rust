//! `aaemu-clientpatch`: pak and database patching for client/server backports.
//!
//! ```text
//! aaemu-clientpatch pak list <pak> [prefix]
//! aaemu-clientpatch pak info <pak>
//! aaemu-clientpatch pak extract <pak> <file|prefix> <outdir>
//! aaemu-clientpatch pak add <pak> <srcdir> [--no-replace]
//! aaemu-clientpatch pak restore <pak>
//! aaemu-clientpatch db apply <sqlite> <sql files...> [--dry-run]
//! aaemu-clientpatch db check <sqlite>
//! aaemu-clientpatch bundle build <bundle_dir> --donor-pak <pak> --out <staging_dir>
//! aaemu-clientpatch bundle apply <bundle_dir> --staging <dir> [--client <root>] [--server-db <path>] [--allow-replace] [--no-pak]
//! ```

use std::io::Write;
use std::path::PathBuf;

use aaemu_clientpatch::{bundle, dbpatch, pakops};
use anyhow::{bail, Context, Result};

const USAGE: &str = "usage:
  aaemu-clientpatch pak list <pak> [prefix]
  aaemu-clientpatch pak info <pak>
  aaemu-clientpatch pak extract <pak> <file|prefix> <outdir>
  aaemu-clientpatch pak add <pak> <srcdir> [--no-replace]
  aaemu-clientpatch pak restore <pak>
  aaemu-clientpatch db apply <sqlite> <sql files...> [--dry-run]
  aaemu-clientpatch db check <sqlite>
  aaemu-clientpatch bundle build <bundle_dir> --donor-pak <pak> --out <staging_dir>
  aaemu-clientpatch bundle apply <bundle_dir> --staging <dir> [--client <root>] [--server-db <path>] [--allow-replace] [--no-pak]";

/// Positional arguments plus `--flag [value]` options.
struct Args {
    pos: Vec<String>,
    opts: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse(raw: &[String], value_flags: &[&str]) -> Result<Self> {
        let mut pos = Vec::new();
        let mut opts = Vec::new();
        let mut it = raw.iter();
        while let Some(a) = it.next() {
            if let Some(flag) = a.strip_prefix("--") {
                if value_flags.contains(&flag) {
                    let v = it
                        .next()
                        .with_context(|| format!("--{flag} needs a value"))?;
                    opts.push((flag.to_string(), Some(v.clone())));
                } else {
                    opts.push((flag.to_string(), None));
                }
            } else {
                pos.push(a.clone());
            }
        }
        Ok(Self { pos, opts })
    }

    fn flag(&self, name: &str) -> bool {
        self.opts.iter().any(|(k, _)| k == name)
    }

    fn value(&self, name: &str) -> Option<PathBuf> {
        self.opts
            .iter()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| v.clone())
            .map(PathBuf::from)
    }

    fn need(&self, i: usize, what: &str) -> Result<&str> {
        self.pos
            .get(i)
            .map(String::as_str)
            .with_context(|| format!("missing <{what}>\n{USAGE}"))
    }
}

fn run() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (group, cmd) = match (argv.first(), argv.get(1)) {
        (Some(g), Some(c)) => (g.as_str(), c.as_str()),
        _ => bail!("{USAGE}"),
    };
    let args = Args::parse(
        &argv[2..],
        &["donor-pak", "out", "staging", "client", "server-db"],
    )?;
    match (group, cmd) {
        ("pak", "list") => {
            let pak = pakops::open_read(args.need(0, "pak")?.as_ref())?;
            let files = pakops::list(&pak, args.pos.get(1).map_or("", String::as_str));
            let mut out = std::io::stdout().lock();
            pakops::write_listing(&files, &mut out)?;
            out.flush()?;
            tracing::info!("{} files", files.len());
        }
        ("pak", "info") => {
            let path = PathBuf::from(args.need(0, "pak")?);
            let pak = pakops::open_read(&path)?;
            println!("{}", pakops::info(&pak, std::fs::metadata(&path)?.len()));
        }
        ("pak", "extract") => {
            let pak = pakops::open_read(args.need(0, "pak")?.as_ref())?;
            let done = pakops::extract(
                &pak,
                args.need(1, "file|prefix")?,
                args.need(2, "outdir")?.as_ref(),
            )?;
            tracing::info!("extracted {} files", done.len());
        }
        ("pak", "add") => {
            let opts = pakops::AddOptions {
                replace: !args.flag("no-replace"),
            };
            let r = pakops::add_dir(
                args.need(0, "pak")?.as_ref(),
                args.need(1, "srcdir")?.as_ref(),
                opts,
            )?;
            println!(
                "added {}, replaced {}, unchanged {}, {} bytes",
                r.added.len(),
                r.replaced.len(),
                r.unchanged,
                r.bytes
            );
        }
        ("pak", "restore") => pakops::restore(args.need(0, "pak")?.as_ref())?,
        ("db", "apply") => {
            let db = PathBuf::from(args.need(0, "sqlite")?);
            let files: Vec<PathBuf> = args.pos[1..].iter().map(PathBuf::from).collect();
            if files.is_empty() {
                bail!("no SQL files given\n{USAGE}");
            }
            for (name, o) in dbpatch::apply_files(&db, &files, args.flag("dry-run"))? {
                println!("{name}: {o:?}");
            }
        }
        ("db", "check") => {
            let problems = dbpatch::integrity_problems(args.need(0, "sqlite")?.as_ref())?;
            if problems.is_empty() {
                println!("integrity_check: ok");
            } else {
                bail!("integrity_check failed: {}", problems.join("; "));
            }
        }
        ("bundle", "build") => {
            let dir = PathBuf::from(args.need(0, "bundle_dir")?);
            let donor = args.value("donor-pak").context("--donor-pak is required")?;
            let out = args.value("out").context("--out is required")?;
            let r = bundle::build(&dir, &donor, &out)?;
            println!(
                "staged {} donor files ({} already up to date), {} generated, {} bytes in {}",
                r.donor_files,
                r.unchanged,
                r.generated_files,
                r.bytes,
                out.display()
            );
        }
        ("bundle", "apply") => {
            let dir = PathBuf::from(args.need(0, "bundle_dir")?);
            let staging = args.value("staging").context("--staging is required")?;
            let targets = bundle::ApplyTargets {
                client: args.value("client"),
                server_db: args.value("server-db"),
                allow_replace: args.flag("allow-replace"),
                no_pak: args.flag("no-pak"),
            };
            let r = bundle::apply(&dir, &staging, &targets)?;
            print!("{}", r.summary());
        }
        _ => bail!("unknown command {group} {cmd}\n{USAGE}"),
    }
    Ok(())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
