# Build performance of the Rust workspace

- Audience: contributors and agents who build `rust/` (Windows first, CI notes at the end)
- Measured: 2026-11-10 on one machine (AMD Ryzen 7 255, 8 cores / 16 threads, 30 GB RAM, NTFS,
  Windows 11, MSVC Build Tools 17.14, rustc/cargo 1.99.1, Microsoft Defender real-time protection on),
  repository at `139a18c50` (`main` before the 2026-11-10 PR batch)
- Method: every number below is one run of the command shown, in a fresh target directory unless
  stated, with a small harness that records wall time, peak used RAM (minimum free memory, sampled
  every second), average CPU and the size of the target dir. One run per row: **treat differences
  below ~10 % as noise.** The `--timings` reports of the cold builds were used for the critical-path
  analysis.

## 1. Summary

**Are the lane settings right?** Yes: what `Scripts/lane.sh` writes (`jobs = 4`, `rust-lld`,
`debug = "line-tables-only"`) is as fast as anything else tried (a cold test build is ~285 s) and the
debuginfo setting is what keeps it from being 388 s / 17 GB. Builds still "take forever" because of five
things that no setting changes: one 530k-line crate on the critical path, a second full build of that
crate for every command shape, Defender scanning every fresh executable, 98 test executables, and
CI/non-lane builds that use the stock profile.

1. **One crate is the build.** `aaemu-game` is ~530,000 lines (a third of it test code) compiled
   twice per test build (as a library and with `--test`). The third-party dependencies and the other
   eleven crates are finished after ~65 s of a ~285 s cold build; the remaining ~220 s is the single
   `aaemu-game` test-build unit, and about three quarters of that is single-threaded compiler work
   (type check, borrow check, monomorphization). More cores (`jobs`), a faster linker, less debuginfo detail and
   sccache therefore **do not shorten a cold build** (section 3.5).
2. **Do not run the same crate in many shapes.** Cargo builds a separate copy of ~25 dependencies
   and of `aaemu-game` for every distinct feature set, profile and mode. In this workspace
   `--workspace` and `-p aaemu-game` resolve different features (`cc/parallel`, pulled in by
   `aaemu-pak` only), and `cargo build` and `cargo test` differ again (`tokio/test-util`). The first
   `cargo build -p aaemu-game` after a `cargo test --workspace` recompiled 26 crates in 241 s; the first
   `cargo test -p aaemu-game --no-run` after a workspace build took 232 s instead of 42 s (section 3.1).
3. **Full debuginfo (the repository default) costs 35 % time, 45 % RAM and 2.2x disk.** The
   `line-tables-only` setting that `Scripts/lane.sh` already writes into every lane is the right
   one; CI and anyone building outside a lane gets the stock profile (section 3.2).
4. **On Windows, Defender is a large hidden cost.** The first launch of every freshly built
   executable takes 1.7 s (small test binaries) to 10.9 s (the 103 MB `aaemu-game` test binary) and
   0.01-0.05 s afterwards; 98 test executables make ~165 s of the 210 s that `cargo test --workspace`
   spends *running* tests that need 34 s. Defender also burned 100-330 CPU-seconds per cold build.
   Excluding the build directories fixes both (section 3.3).
5. **Cheap fixes, ranked by what they save per developer day:** use `cargo test --workspace --lib
   <filter>` (or `-p aaemu-game --lib`) in the edit loop and never plain `cargo test -p aaemu-game`;
   exclude the lane directories from Defender; keep one command shape per lane; consolidate the 63
   `aaemu-physics` integration-test binaries; later, split `aaemu-game`.

## 2. What was measured

### 2.1 Cold build, `cargo test --workspace --no-run --locked --timings`

| Run | Configuration | Wall | Peak RAM | Avg CPU | Target dir | Defender CPU |
|---|---|---:|---:|---:|---:|---:|
| E0 | **what `lane.sh` writes**: `jobs=4`, `rust-lld`, `debug = line-tables-only` | 272 s | 10.7 GB | 37 % | 8.0 GB | 334 s |
| E1 | E0 with `jobs=16` (all logical CPUs) | 287 s | 12.0 GB | 55 % | 8.0 GB | 234 s |
| E3 | E1 with MSVC `link.exe` instead of `rust-lld` | 282 s | 9.9 GB | 46 % | 8.0 GB | 117 s |
| E4 | E1 with `debug = 0` | 301 s | 9.8 GB | 40 % | 6.2 GB | 131 s |
| E8 | E1 with `CARGO_INCREMENTAL=0` | 303 s | 10.7 GB | 45 % | **3.7 GB** | 160 s |
| E9 | E1 with `RUSTC_BOOTSTRAP=1 RUSTFLAGS=-Zthreads=8` (nightly-only parallel front-end, experiment) | 260 s | - | 75 % | 8.0 GB | 289 s |
| E5 | E1 + sccache, empty cache | 317 s | 10.3 GB | 43 % | 8.0 GB | 157 s |
| E6 | E1 + sccache, **new** target dir, warm cache (92.8 % hits) | 301 s | 10.3 GB | 39 % | 8.0 GB | 128 s |
| E2 | **stock repo config**: no local config (`link.exe`, full debuginfo, `jobs=16`) | **388 s** | **17.4 GB** | 45 % | **17.4 GB** | 104 s |

Idle machine for comparison: Defender used 7 CPU-seconds in 32 s (0.2 core).

### 2.2 Where the time goes (E1 `--timings`, 433 units, 286 s wall)

| Unit | Duration | Started | Finished |
|---|---:|---:|---:|
| all third-party crates + the 11 other workspace crates | | 0 s | ~65 s |
| `aaemu-game` library (what bins, integration tests and `aaemu-testclient` link against) | 134 s | 60 s | 194 s |
| **`aaemu-game` library with `--test`** | **222 s** | 64 s | **286 s** |
| `aaemu-testclient`, `aaemu-game` bin and its 3 integration tests (after the library) | 12 s | 194 s | 206 s |
| `aaemu-physics`: 64 test executables (63 integration tests + unit tests) | 136 s CPU in total | 34 s | 67 s |

Sum of all unit durations is 1,242 s, i.e. an average parallelism of 4.3: four jobs are enough (E0
against E1), because the critical path is one rustc process.

`rustc -Ztime-passes` on that unit (non-incremental, dependencies already built, 227 s, peak 9.2 GB):

| Phase | Time | Threading |
|---|---:|---|
| type check | 44 s | single |
| MIR borrow check | 56 s | single |
| macro expansion + name resolution + coherence | ~26 s | single |
| monomorphization collection + LLVM IR generation | ~71 s | single |
| LLVM optimisation passes | 60 s | parallel (256 codegen units) |

`aaemu-game` is 530k lines: `systems/` is 465k of them (economy 67k, npcs 66k, skills 64k, social 58k,
services 53k, ai 46k, vehicles 35k, housing 33k, quests 25k, instances 16k) and 176k lines (33 %) are
test-only code. The test-cfg compile is therefore ~65 % bigger than the library compile.

### 2.3 Edit-compile-test loops (E7, warm target from E1, lane configuration)

| Step | Command | Wall |
|---|---|---:|
| nothing changed | `cargo test --workspace --no-run` | 1-9 s |
| after a one-line edit in a leaf function | `cargo test -p aaemu-game --no-run` | **232 s** |
| same kind of edit | `cargo test -p aaemu-game --lib --no-run` | **42 s** |
| edit in `models/unit.rs` (widely used type) | `cargo test -p aaemu-game --lib --no-run` | 43 s |
| first clippy after the test build | `cargo clippy --workspace --all-targets -- -D warnings` | 185 s |
| clippy after an edit (includes the `-p` variant build, see 3.1) | `cargo clippy -p aaemu-game --all-targets -- -D warnings` | 135 s |
| rustfmt | `cargo fmt --all --check` | 8 s |
| run the tests (binaries already built) | `cargo test --workspace --no-fail-fast` | 210 s |

The 232 s row is a feature-variant effect, not an edit effect (section 3.1). The controlled repeat
(E13, lane configuration, every step follows a one-line edit in a leaf function):

| Step | Command | Wall |
|---|---|---:|
| 1 | `cargo test --workspace --lib --no-run` (same selection as the build before) | 51 s |
| 2 | `cargo test -p aaemu-game --lib --no-run` (**first time**) | **277 s** |
| 3 | the same command again | 54 s |
| 4 | back to `cargo test --workspace --lib --no-run` | 54 s |

Cheaper feedback loops (E12, fresh target, lane configuration, selection `-p aaemu-game --lib`):

| Command | Wall |
|---|---:|
| `cargo check -p aaemu-game --lib`, cold (dependency metadata + the crate) | 146 s, 4.6 GB |
| the same after a one-line edit | **18.5 s** |
| `cargo clippy -p aaemu-game --lib -- -D warnings` after that (first clippy: other wrapper, lints) | 123 s |

`cargo check` does not compile `#[cfg(test)]` code: use it for the compile-error loop, the test
build when you need to run tests, clippy once per change set (`lane.sh check` does it).

### 2.4 The test phase: 98 executables

`cargo test --workspace` starts 98 test executables. The tests themselves need 34 s in total; the
rest of the 210 s is process start-up. Launching a **freshly built** executable (`--list`, which runs
no test):

| Executable | First launch | Second launch |
|---|---:|---:|
| 14 test binaries (1-10 MB) | 1.69 s average | 0.01 s |
| `aaemu_game` library test binary (103 MB) | 10.9 s | 0.05 s |

That is the Defender / SmartScreen first-execution scan: it repeats after every rebuild of the
executable. 98 x 1.7 s = ~165 s, which accounts for almost the whole gap.

## 3. Findings and recommendations

### 3.1 One command shape per lane (feature variants)

Cargo's feature resolver (`resolver = "2"`) unifies the features of a dependency over the packages
you *select* on the command line, and activates dev-dependency features only when it builds tests.
Each distinct result is a separate artifact set. In this workspace:

- `cargo tree -p aaemu-game -e normal,build` and `cargo tree --workspace -e normal,build` differ
  only by the zstd stack (`zstd`, `zstd-safe`, `zstd-sys`, `jobserver`, `getrandom`), which only
  `aaemu-pak` needs, and by one feature on a shared crate: `cc` is `[parallel]` for `--workspace` and
  `[]` for `-p aaemu-game` (`zstd-sys` enables it). `cc` is a build-dependency of `ring` and
  `libsqlite3-sys`, so the difference ripples through `rustls`, `rusqlite`, `twilight-*`... and, because
  the dependencies' hashes change, `aaemu-game` is rebuilt as well.
- `aaemu-game` and `aaemu-login` enable `tokio/test-util` in `[dev-dependencies]`. `cargo test`
  and `clippy --all-targets` build with it, `cargo build` / `cargo run` do not, so `tokio` and everything
  above it (sqlx, hyper, axum, twilight, `aaemu-commons`, `aaemu-game`) exist twice.

Measured: `cargo test --workspace --no-run`, then `cargo build -p aaemu-game` (stock configuration):
**241 s and 26 crates recompiled** for a build with no source change; going back to the workspace
command took 8 s because Cargo keeps both variants side by side. So the price is *one extra full
build per variant per lane* (3-5 minutes and several GB), paid again in every new lane.

CI shows the same: after `cargo build --workspace --all-targets`, the step `cargo run -p aaemu-game --
compiler-check` prints `Compiling aaemu-commons` and `Compiling aaemu-game` and takes 88 s on Linux and
126 s on Windows, in every job.

What to do:

1. In a lane use one selection for everything: `cargo test --workspace --lib <filter>` has the same
   feature set as `Scripts/lane.sh check` (`cargo xtask ci` uses `--workspace`), `-p aaemu-game --lib` is
   a different variant. Do not mix `cargo build`/`run` with `cargo test`/`clippy --all-targets` in one lane
   (the live stack builds with its own `live` profile anyway).
2. CI: run the already built binary instead of a second `cargo run` (`target/debug/aaemu-game
   compiler-check`), saving ~90-125 s per test job.
3. Repository fix: unify the features so that every selection resolves the same graph. The standard
   tool is a `workspace-hack` crate maintained by [cargo-hakari](https://docs.rs/cargo-hakari/); for the
   single `cc/parallel` difference found here a one-line dependency would also do (see E13).

Tested fix for the selection variant (E13): add `zstd.workspace = true` to
`crates/aaemu-game/Cargo.toml`, an otherwise unused dependency that only makes `cc/parallel` active
for `-p aaemu-game` too (comment it, or let cargo-hakari generate the equivalent for every crate).
With it, the same sequence gave **50 s instead of 277 s** for the first `-p aaemu-game --lib` after
an edit, and 48 s for going back to `--workspace`. Adopting it costs one rebuild per lane (270 s).
It does not touch the `tokio/test-util` variant (`cargo build` vs `cargo test`, not tested), which is
why CI still needs item 2.

### 3.2 Debuginfo: keep `line-tables-only`, and make it the default

E2 (stock) against E1: 388 s vs 287 s (+35 %), 17.4 vs 12.0 GB peak RAM (+45 %), 17.4 vs 8.0 GB on disk
(2.2x). `debug = 0` (E4) gains another 1.8 GB of disk and no time. `lane.sh` writes
`debug = "line-tables-only"` for `dev` and `test` into each lane's untracked `rust/.cargo/config.toml`;
that is the right choice, backtraces and panics still name `file:line`. The same snippet is **not** in the
committed `rust/Cargo.toml`, so CI, the integration checkout and every other contributor build the
388 s / 17 GB variant. Proposed (not applied, it changes everyone's debugger experience; override
locally with `CARGO_PROFILE_DEV_DEBUG=true`):

```toml
# rust/Cargo.toml
[profile.dev]
debug = "line-tables-only"
[profile.test]
debug = "line-tables-only"
```

### 3.3 Windows: Defender (needs an administrator, nothing here was changed)

Evidence: section 2.1 (100-330 CPU-seconds per cold build), 2.4 (1.7-10.9 s per fresh executable).
Real-time protection was on and the exclusion list could not be read without elevation. Excluding the
directories that only hold compiler output should remove the first-launch scans and most of the CPU
use (expected, not measured: changing exclusions needs an administrator and was not done).
Elevated PowerShell (adjust the paths; exclusions lower protection for those folders, keep them narrow):

```powershell
Add-MpPreference -ExclusionPath 'C:\aa-re\lanes\targets','C:\aa-re\lanes\worktrees',
  "$env:USERPROFILE\.cargo","$env:USERPROFILE\.rustup"
Add-MpPreference -ExclusionProcess 'cargo.exe','rustc.exe','link.exe','lld-link.exe','rust-lld.exe','cl.exe'
```

To verify: re-run `cargo test --workspace --no-run`, then time `target\debug\deps\<test>.exe --list`
twice; the first launch should drop from ~2-11 s to ~0.05 s. A **Dev Drive** (ReFS volume with
Defender performance mode, Windows 11) is the supported alternative; Microsoft reports up to ~30 %
faster builds, and recommends putting `CARGO_HOME` and the source on it
([Dev Drive](https://devblogs.microsoft.com/engineering-at-microsoft/dev-drive-and-copy-on-write-for-developer-performance/),
[performance mode](https://learn.microsoft.com/en-us/defender-endpoint/microsoft-defender-endpoint-antivirus-performance-mode)).
That figure is Microsoft's, not measured here.

### 3.4 Fewer, bigger test executables

`aaemu-physics` has 63 files under `tests/`, each a separate executable (Cargo's autotests): 63
links and 63 first-launch scans (~107 s per fresh `cargo test --workspace`). Merge them into one
target (`tests/main.rs` with `mod p2a1_scenarios; ...`, or `autotests = false` plus one `[[test]]`);
`aaemu-commons` (4), `aaemu-login` (2) and `aaemu-game` (3) can follow. `cargo nextest` does not help
here: it schedules tests better but still starts every binary
([how nextest works](https://nexte.st/book/how-it-works.html)).

### 3.5 Settings that do not help (and what the lane defaults already get right)

| Setting | Result | Verdict |
|---|---|---|
| `jobs = 4` vs 16 | 272 s vs 287 s | irrelevant; 4 is right when several lanes build at once (RAM is the limit) |
| `rust-lld` vs `link.exe` | 287 s vs 282 s | irrelevant for the cold build; harmless |
| `debug = 0` vs `line-tables-only` | no faster, -1.8 GB | optional |
| sccache with a warm cache | 301 s vs 287 s; 244 of 263 compilations were hits | no gain: the only slow unit is a workspace crate, which sccache cannot cache while incremental compilation is on (and the key contains the worktree path) |
| `CARGO_INCREMENTAL=0` | 303 s (same), target dir **3.7 GB instead of 8.0 GB** | use for lanes that are built once (CI-style); keep incremental where you iterate (an edit needs 42 s incremental, a full recompile otherwise) |

### 3.6 Structural options (bigger changes)

- **Split `aaemu-game`.** With the stable toolchain the only thing that shortens the critical path
  is compiling the code in pieces. `systems/` is 88 % of the crate and its subsystems already talk through the hook traits in
  `systems/mod.rs`; leaf-like candidates are `systems/services` (53k lines, owns the axum and twilight
  dependencies), `data`, `packets` and generated tables, then `economy`, `npcs`, `skills`. Splitting also makes
  incremental rebuilds and clippy smaller. This is a refactor, not a configuration change.
- **Parallel front-end (nightly only).** About 100 s of the critical unit are type and borrow check,
  which the nightly `-Zthreads=N` front-end parallelises. Measured with `RUSTC_BOOTSTRAP=1
  RUSTFLAGS=-Zthreads=8` on the stable compiler (E9): the critical unit went from 222 s to 178 s
  (-20 %) and the cold build from 287 s to 260 s (-10 %), at 75 % instead of 55 % average CPU. A real
  but modest gain that needs a nightly toolchain: low priority next to the items above.
- The test-cfg compile is 65 % larger than the library compile; moving rarely changed generated or
  data-heavy test modules into integration-test crates would take them off the critical path.

### 3.7 CI (GitHub Actions, run 42892304356 for #82, all jobs green, `Swatinem/rust-cache` hits)

| Job | Time | Biggest steps |
|---|---:|---|
| build & test (windows) | 10.1 min | Build 320 s, **Scripts compile check 126 s**, Test 96 s, lane regressions 33 s |
| build & test (ubuntu) | 6.5 min | Build 197 s, **Scripts compile check 88 s**, Test 84 s |
| coverage (ubuntu) | 6.2 min | instrumented rebuild |
| clippy (windows / ubuntu) | 3.9 / 2.0 min | |

Expected savings, not measured: the compile-check fix of 3.1 (-90 to -125 s per test job);
`CARGO_PROFILE_DEV_DEBUG=line-tables-only` / `CARGO_PROFILE_TEST_DEBUG=line-tables-only` in the workflow
`env` (shorter build and link, smaller cache); fewer test executables (3.4). PR runs take 10-15 minutes
because the Windows test job is the slowest.

## 4. Recommended configuration

Lane (`Scripts/lane.sh new` already writes this; keep it):

```toml
# rust/.cargo/config.toml  (untracked)
[build]
jobs = 4                      # several lanes at once; 8-16 gives nothing on a cold build
target-dir = "<lane target dir>"
[target.x86_64-pc-windows-msvc]
linker = "rust-lld"           # optional
[profile.dev]
debug = "line-tables-only"
[profile.test]
debug = "line-tables-only"
```

Commands:

```text
compile-error loop (~20 s after an edit)    cargo check -p aaemu-game --lib
run tests (~50 s after an edit)             cargo test --workspace --lib <filter>
never in the loop                           cargo test -p aaemu-game <filter>   # builds every target: 4 min
once per lane before landing                Scripts/lane.sh check
look at a slow build                        cargo build --timings               # target/cargo-timings/*.html
compare feature sets                        cargo tree -p aaemu-game -e normal,build -f "{p} [{f}]" --prefix none | sort -u
```

`-p aaemu-game ...` and `--workspace ...` are different dependency variants until the feature
unification of 3.1 is in place: the first use of the other shape in a lane costs one extra build
(~4 minutes). `cargo test -p aaemu-game --lib <filter>` is equally fast once that build exists.

## 5. Reproducing

For each configuration: delete the target directory, run
`cargo test --workspace --no-run --locked --timings`, and record the wall time, the minimum free
memory during the run, the average CPU, the size of the target directory and `Process\% Processor Time`
of `MsMpEng` (readable without elevation through the performance counter). The critical-path table
comes from the `UNIT_DATA` array inside `target/cargo-timings/cargo-timing-*.html`; the phase table from
`RUSTC_BOOTSTRAP=1 cargo rustc -p aaemu-game --lib --profile test -- -Ztime-passes` (a measurement
hack: `-Z` flags need nightly or this variable).

## 6. Sources

- Cargo: [build timings](https://doc.rust-lang.org/cargo/reference/timings.html),
  [feature resolver 2](https://doc.rust-lang.org/cargo/reference/resolver.html#features),
  [profiles](https://doc.rust-lang.org/cargo/reference/profiles.html)
- [The Rust Performance Book, compile times](https://nnethercote.github.io/perf-book/compile-times.html)
- [cargo-hakari](https://docs.rs/cargo-hakari/) (workspace-hack crates)
- [sccache](https://github.com/mozilla/sccache) (incremental builds are not cached; paths are part of the key)
- Microsoft: [Dev Drive](https://devblogs.microsoft.com/engineering-at-microsoft/dev-drive-and-copy-on-write-for-developer-performance/),
  [Defender performance mode](https://learn.microsoft.com/en-us/defender-endpoint/microsoft-defender-endpoint-antivirus-performance-mode)
- [cargo-nextest, how it works](https://nexte.st/book/how-it-works.html)
