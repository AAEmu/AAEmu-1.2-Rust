//! `AAEMU_LOG_MS`: a child process (this test binary re-run with `AAEMU_LOG_CHILD`) initialises
//! the real logging, writes one line, and the parent checks the timestamp shape in `Server.log`.

use std::path::{Path, PathBuf};
use std::process::Command;

const CHILD_ENV: &str = "AAEMU_LOG_CHILD";

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aaemu-logts-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Re-runs this test binary for `log_line_timestamps` only, with `AAEMU_LOG_MS` set to `value`
/// (or removed), and returns the lines the child logged to `Logs/Server.log`.
fn run_child(dir: &Path, value: Option<&str>) -> Vec<String> {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "log_line_timestamps",
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(CHILD_ENV, dir)
    .env_remove("RUST_LOG")
    .env_remove("AAEMU_LOG_MS");
    if let Some(value) = value {
        cmd.env("AAEMU_LOG_MS", value);
    }
    assert!(
        cmd.status().unwrap().success(),
        "the child should exit cleanly"
    );
    std::fs::read_to_string(dir.join("Logs").join("Server.log"))
        .expect("Server.log written")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// `HH:MM:SS` followed by the rest, or `HH:MM:SS.ffffff` when `micros`.
fn time_prefix(line: &str, micros: bool) -> Option<&str> {
    let b = line.as_bytes();
    let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    let clock = digit(0)
        && digit(1)
        && b.get(2) == Some(&b':')
        && digit(3)
        && digit(4)
        && b.get(5) == Some(&b':')
        && digit(6)
        && digit(7);
    if !clock {
        return None;
    }
    if micros {
        let fraction = b.get(8) == Some(&b'.') && (9..15).all(digit);
        fraction.then(|| &line[15..])
    } else {
        Some(&line[8..])
    }
}

#[test]
fn log_line_timestamps() {
    if let Ok(dir) = std::env::var(CHILD_ENV) {
        aaemu_commons::logging::init(Path::new(&dir));
        tracing::info!("timestamp probe");
        aaemu_commons::logging::flush();
        return;
    }
    let dir = temp_dir("ms");
    // opt-in: six fractional digits on every line
    for value in ["1", "true"] {
        let _ = std::fs::remove_dir_all(dir.join("Logs"));
        let lines = run_child(&dir, Some(value));
        assert!(
            lines.iter().any(|l| l.ends_with("timestamp probe")),
            "{lines:?}"
        );
        for line in &lines {
            let rest = time_prefix(line, true).unwrap_or_else(|| panic!("no microseconds: {line}"));
            assert!(rest.starts_with(" [INFO] "), "{line}");
        }
    }
    // default and anything else: unchanged HH:MM:SS
    for value in [None, Some("0"), Some("")] {
        let _ = std::fs::remove_dir_all(dir.join("Logs"));
        let lines = run_child(&dir, value);
        assert!(
            lines.iter().any(|l| l.ends_with("timestamp probe")),
            "{lines:?}"
        );
        for line in &lines {
            let rest = time_prefix(line, false).unwrap_or_else(|| panic!("bad time: {line}"));
            assert!(
                rest.starts_with(" [INFO] "),
                "default format changed: {line}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
