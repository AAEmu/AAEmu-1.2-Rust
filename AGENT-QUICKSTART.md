# Agent quickstart: run the stack and the real client

Short handoff for agents (and humans) working on this machine. Details live in
[`rust/LIVE-TESTING.md`](rust/LIVE-TESTING.md); conventions in [`AGENTS.md`](AGENTS.md).
Both clients, the 10.x zone reference binaries, IDA databases and all reverse-engineering notes:
[`REVERSING.md`](REVERSING.md).

## TL;DR

Every session works in its own lane (rules: [`AGENTS.md`](AGENTS.md) → "Sessions work in lanes"):

```bash
Scripts/lane.sh gc                  # leftovers of old sessions (dry run; --apply deletes them)
git fetch aaemu12 main
Scripts/lane.sh new <name> --type fix --base aaemu12/main
cd "<worktree printed by lane.sh>"
Scripts/lane.sh info                # this lane's paths and ports
Scripts/lane.sh sync && Scripts/lane.sh check   # merge the lane's base and run local CI
# Push the committed lane branch to aaemu12 and open a PR against main.
Scripts/lane.sh done --keep-branch  # remove lane resources after publishing
Scripts/lane.sh list                # what lanes exist (yours must be gone when you are done)
```

The live stack, from inside the lane (it picks up the lane's ports, dirs and databases):

```bash
Scripts/live-stack.sh build    # `live` profile build of all binaries (~12 min cold, ~2.5 min after an edit)
                               # + aadriver.dll and its native tests (needs MSVC x86; only warns without it)
Scripts/live-stack.sh sync     # copy rust/crates/*/app configs + data (+ shared compact.sqlite3) into the runtime dirs
Scripts/live-stack.sh start    # copies the binaries to <runtime>/bin, creates the lane DBs on first start,
                               # starts login + game; returns when the game registered (~1 min)
Scripts/live-stack.sh client   # starts the real 1.2 client against this lane, already logged in
Scripts/live-stack.sh status   # what is up + last log lines
Scripts/live-stack.sh shot     # screenshot -> <runtime>/shots/now.png (open it to see the client)
Scripts/live-stack.sh stop     # this lane's client + servers only (MySQL stays up for everyone)
```

After a schema change or when the databases are in a bad state: `stop`, then `db-reset`, then
`start` (this wipes the lane's throwaway test accounts and characters). Without a lane (the
integration checkout) the same commands drive the old shared stack in `D:\aaemu-live` on 21xxx;
sessions do not use it.

## Local paths

The paths below are defaults or reference-machine examples. Use `lane.sh info`
for the active worktree, target, runtime, and MySQL port. Set `AAEMU_LANES`,
`AAEMU_WT_ROOT`, `AAEMU_TARGET_ROOT`, and `AAEMU_LIVE_PREFIX` consistently
when using a different layout. For live testing, set `AAEMU_LIVE_TEMPLATE`,
`AAEMU_MYSQL_BIN`, and `AAEMU_MYSQL_PORT` for the private instance and use
`AAEMU_CLIENT`, `AAEMU_CLIENT_PAK`, and `AAEMU_COMPACT_DB` for local assets.

| What | Path |
|---|---|
| Repository and PR base | `https://github.com/AAEmu/AAEmu-1.2-Rust`, remote `aaemu12`, branch `main`. See [AGENTS.md](AGENTS.md) for the integration checkout's rules. |
| Lane worktrees / registry | `D:\aaemu-wt\<name>` / `D:\aaemu-lanes\<name>.env` |
| Cargo build output | `D:\aaemu-target\<lane>` (lane's untracked `rust/.cargo/config.toml`; the integration checkout uses `D:\aaemu-target\main`) |
| Lane runtime dirs (configs, binaries, logs, shots) | `D:\aaemu-live-<lane>\{rs-login,rs-game,bin,logs,shots}` |
| Shared MySQL data (private instance) | `D:\aaemu-live\mysql` |
| Secrets (DB password, test account password) | `D:\aaemu-live\cfg\` and the runtime `Config.Local.json` files — never print or commit them |
| 1.2 client (read-only) | `D:\AA1.2` (`bin32\archeage.exe`, `game_pak`) |
| Server game DB | `rust/crates/aaemu-game/app/Data/compact.sqlite3` (git-ignored; source archive: `C:\Users\messe\Documents\MEGA downloads\ArcheAge_Server_Compact_r208088_v1.2.4.13_update_2026-01-23.7z`) |
| 10.x reference client / zone server (read-only) | `D:\ArcheAge Returns 10.0.2.13 - 8yx - r575 - 2026-06-18` |
| Reverse-engineering outputs | `D:\aa-re\` (CryPhysics, zone, client12) |

## Ports (live stack)

| Service | Address |
|---|---|
| Private MySQL (shared by all lanes) | `127.0.0.1:3317` (root without password, loopback only) |
| Lane stack: login (client) / internal | `127.0.0.1:<p>237` / `<p>234`, `<p>` = the lane's prefix 24–39 (`lane.sh info`) |
| Lane stack: game / stream / web API | `127.0.0.1:<p>239` / `<p>250` / `<p>280` |
| Old shared stack (integration checkout, not for sessions) | `21xxx`; `22xxx`/`23xxx` are taken by older ad-hoc stacks |

## The real client

- `client` runs `aaemu-launcher`, which hands the login ticket to `archeage.exe` (Trion 1.2
  protocol: RC4 ticket in shared memory, `-handle MAP:EVENT`). Nothing is typed into the game.
  Account `clienttest` is auto-created (`AutoAccount`).
- The client loads ~60–80 s, then shows **Select Server** (`LiveTest`). It goes fullscreen
  1920×1080. Left idle at login it disconnects after 1–2.5 min, so act promptly.
- To drive it: screenshots (`shot`) + clicks/keys via PowerShell (`SetCursorPos`/`mouse_event`,
  `SendKeys`), or run Lua inside it with the client driver
  ([`rust/docs/client/LUA-DRIVER.md`](rust/docs/client/LUA-DRIVER.md):
  `aaemu-clientdriver inject`, then `lua "<expr>"`).
- Confirm each step on the server side: `rs-login.log` (`clienttest connected.`), `rs-game.log`
  (X2EnterWorld, CSListCharacter, CSSelectCharacter, CSMoveUnit, chat).

## Rules that bite

- **D: is nearly full and target dirs are huge** (10–90 GB each). Everything a session creates
  lives in its lane and goes away with `lane.sh land` / `lane.sh done`; never leave a target dir,
  runtime dir, database or dump behind. The lane's untracked `rust/.cargo/config.toml` sets its
  own `target-dir`, `jobs = 4`, the `rust-lld` linker and line-tables-only debug info. The live
  stack runs the `live` profile (opt-level 1, incremental); `AAEMU_PROFILE=release` switches it.
- **Other sessions' stacks and clients run on the same machine.** Only stop your own
  (`live-stack.sh stop`, `lane.sh done`); never `taskkill /IM aaemu-game.exe` / `archeage.exe`
  and never shut down the shared MySQL (`stop --mysql` exists for the machine owner only).
- **A fresh worktree has no `compact.sqlite3`** (git-ignored). `live-stack.sh sync` copies the shared
  one from `D:\aaemu-live\rs-game\Data\`; real-data tests can point `AAEMU_COMPACT_DB` at it.
- **The user's MySQL on port 3306 is off-limits.** Only use the private instance on 3317.
- **`D:\AA1.2` and the 10.x folder are read-only.** The 10.x `dedicatedserver.exe` writes its log
  into the user's real `Documents\ArcheAge` — don't run it casually.
- **Don't type passwords into any app UI**; use the launcher's ticket (`--password-file`).
- Long `cargo test` runs: use `--release` for `aaemu-physics`; real-data tests are `#[ignore]`d and
  need `AAEMU_COMPACT_DB=D:/aaemu-live/rs-game/Data/compact.sqlite3` (`-- --ignored`).
- CryPhysics oracle tests need `AAEMU_PHYS_ORACLE` (the real DLL harness, see
  `rust/crates/aaemu-physics/README.md`); without it they replay checked-in goldens.

## Reference verification

- Rust port complete (C# removed in `a2ab80abd`); CryPhysics ported bit-exact and integrated
  (`Configurations/Physics.json`, see `rust/crates/aaemu-game/PORTING.md` → systems::ai collision).
- Verified: real 1.2 client logs in, creates a character, walks, chats, logs out on the Rust
  servers; packet parity with the old C# servers on scripted scenarios.
- See [the feature backlog](FEATURE-GAP-TASKS.md) for open work and
  [PORTING.md](rust/crates/aaemu-game/PORTING.md) for implementation limits.
