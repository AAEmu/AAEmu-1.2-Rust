# Live Verification Suites

- Audience: Contributors and testers
- Last verified against: `rust-rewrite` on October 20, 2026
- Target: ArcheAge 1.2 (`r208022`)

## Overview

`Scripts/verify.sh` checks server fixes end to end on a live lane stack. It is optional tooling,
not part of `cargo xtask ci`. A suite is one
`rust/verify/*.vtest` file: it declares a few bots, gives each bot a script section, and runs the
sections concurrently. Bots log in through the real login and game protocols, so a suite can
assert server-to-client packets, game-server log lines and game-database rows.

Run it inside a lane worktree (see `AGENTS.md`). The script refuses to run
outside a lane, because it builds, restarts and writes to the lane's own stack, runtime
directory and databases.

```sh
Scripts/verify.sh                      # every suite
Scripts/verify.sh --only guild         # suites whose file name contains "guild"
Scripts/verify.sh --report D:/tmp/vr   # report directory
```

What it does, in order:

1. `Scripts/live-stack.sh build` and builds `aaemu-testclient` for the lane target directory.
1. `Scripts/live-stack.sh sync` and patches the **runtime** `Configurations/Expedition.json`
   with `Cost 0 / Level 1 / PartyMemberCount 2` (never the repository's `app/` copy).
1. Restarts this lane's stack with `aaemu_game::systems::services::express_text=debug` added to
   `RUST_LOG` (the emote guard logs at debug), then waits for the game registration.
1. Provisions every declared bot account: a login-only `aaemu-testclient` run creates it
   (`AutoAccount`), then `access_level` is set in this lane's game database
   (`UPDATE <game db>.accounts SET access_level=100` for `gm` bots, `0` otherwise).
1. Clears the bot accounts' persisted guild rows so suites can be rerun.
1. Runs each suite with `aaemu-testclient verify`, then writes the report.

Environment it exports to the bots:

| Variable | Meaning |
| --- | --- |
| `AAEMU_VERIFY_GAME_LOG` | Game-server log read by `expectlog` (`<lane runtime>/logs/rs-game.log`) |
| `AAEMU_VERIFY_MYSQL` | `mysql.exe` plus its arguments and the lane's game database, used by `expectdb` |
| `AAEMU_VERIFY_PASSWORD` | Bot account password (default `testpass`) |

## The `.vtest` format

Plain text, one directive or script command per line; blank lines and `#` comments are ignored.

```text
# optional display name (default: the file stem)
suite emote-spoof

# one line per bot: <id> [gm]; ids are ASCII letters and digits
bot sender
bot watcher
bot logwatch

# a section per bot; its lines are script commands (see below)
[sender]
sync start 3
pump 2

[watcher]
sync start 3
expectpkt SCEmotionExpressedPacket 15
```

Rules:

- `suite`, `bot` and `[id]` go before the bot sections; a section holds only script commands.
- Every declared bot needs a non-empty section.
- `${<bot id>}` in a command expands to that bot's character name, so invites and checks can
  name their peers.
- The account and character name is `v` + the first eight alphanumerics of the file stem + the
  bot id, for example `vguildgualeader`. It stays within the server's 18-character
  character-name limit. Keep bot ids short.
- All bots are started at the same time and meet at the character spawn point. Use `sync` to
  rendezvous before an action another bot must observe.
- Bots run the normal login, character list/create, select and spawn flow first; their script
  starts in the world.

## Script commands for verification

The full command list is in `rust/crates/aaemu-testclient/src/scenario/script.rs`. The
verification commands are:

| Command | Meaning |
| --- | --- |
| `expectpkt <PacketName> [secs]` | Fail unless that server packet arrives within `secs` (default 5) |
| `expectnopkt <PacketName> [secs]` | Fail if that server packet arrives within `secs` (default 5) |
| `expectlog <regex> [secs]` | Fail unless a matching line is written to the game log after the command starts (default 5) |
| `expectdb "<sql>" <value> [secs]` | Run the SQL against the lane game database and fail unless its value equals `value` (default: one check; with `secs`, re-check until the deadline) |
| `sync <token> [count]` | Suite barrier: wait until `count` bots reached the same token (default: every bot) |

A failed expectation prints a `FAIL` line like

```text
FAIL guild-guards: bot leader: [game.script] at script line 17 (`expectdb "SELECT ..." 1`): expected "1" from ...; got "0"
```

and the run exits non-zero. Because `expectlog` only counts lines written after the command
starts, start it before the action that should log (usually from another bot, after a `sync`).

## Adding a suite

1. Create `rust/verify/<name>.vtest` with the declarations and the bot sections.
1. Keep GM use minimal: mark only the bots that need `/commands` (`bot <id> gm`).
1. Make it rerunnable: use `sync` for ordering, and `expectdb` for state that a
   previous run could have left behind. `Scripts/verify.sh` clears guild rows for suite accounts
   but nothing else.
1. Run `Scripts/verify.sh --only <name>` when working on the area it covers.
1. Consider adding the suite in the same change as the fix it guards against, so the regression
   stays covered.

The seed suites:

| Suite | Guards |
| --- | --- |
| `emote-spoof` | `CSExpressEmotionPacket` for another object is ignored (`systems/services/express_text.rs`), while the sender's own emote is announced |
| `guild-guards` | A guild-invitation reply without a pending invite is refused (`systems/social/expedition.rs`); a normal invite + accept joins |

## Reports

`Scripts/verify.sh` writes, by default into `<lane runtime>/verify-report/`:

- `<suite>.log` — the full test-client output of that suite;
- `report.md` — one row per suite plus the failing line and relevant game-log lines;
- `report.json` — the same data for tooling:

```json
{
  "lane": "verify-suite",
  "generated_at": "2026-10-20 20:15:00",
  "login": "127.0.0.1:25237",
  "game_log": "D:/aaemu-live-verify-suite/logs/rs-game.log",
  "suites": [
    { "suite": "emote-spoof", "passed": true, "duration_ms": 12345,
      "failing_line": "", "log_lines": [] }
  ]
}
```

The script exits non-zero when any suite fails; `--only` selects a subset, and the report always
describes the suites actually run.
