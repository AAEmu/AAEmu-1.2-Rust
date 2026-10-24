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
| `AAEMU_VERIFY_CAPTURE_DIR` | Optional, read by `aaemu-testclient verify` (not exported by `verify.sh`): saves every bot's packet capture as `<suite code>-<bot id>.jsonl` in this directory. The captures contain the bots' login tickets (password hash): keep them out of git |

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
| `lootexpect took\|failed [npc obj]` | Loot the last killed NPC and fail unless `SCLootItemTook` (took) / `SCLootItemFailed` (failed) arrives |
| `rezexpect inplace\|temple ok\|refused` | Send `CSResurrectCharacter` and fail unless `SCCharacterResurrected` for the bot's own unit does (ok) / does not (refused) arrive |

Useful companions: `waitpeer [secs] [name]` waits until another player is in sight, `await
<Event> [secs]` waits for a party / guild event (`TeamJoined`, `GuildInvite`, ...), `rawcs
<opcode> <body hex>` sends an arbitrary client packet (`SELF` and `PEER` in the body stand for
the bot's own and the first other player's object id), and `greply <faction id> <0|1>` answers
a guild invitation without waiting for one.

A failed expectation prints a `FAIL` line like

```text
FAIL guild-guards: bot leader: [game.script] at script line 17 (`expectdb "SELECT ..." 1`): expected "1" from ...; got "0"
```

and the run exits non-zero. Because `expectlog` only counts lines written after the command
starts, start it before the action that should log (usually from another bot, after a `sync`).

### Look (appearance) commands

The three native appearance operations (item look conversion `CS049`, Skinize, Cleanup) need exact
request bytes, the item ids / grades / made-unit values the server just handed out, and an ordered,
timed view of the answers. Four commands cover that (`scenario/script.rs`, `scenario/look.rs`):

| Command | Meaning |
| --- | --- |
| `item <template> <count> [as <label>]` | `/item add self`, then remember the stack the server created (`slot type`, `slot`, `id`, `grade`, `flags`, `count`, `made unit`, image template) under `<label>`; a merge into an existing stack refreshes every entry of that id |
| `lookcs <opcode hex> <body…> [wait=<secs>] [until=ended\|none] [grace=<ms>]` | Send a client packet whose body is built from literal hex and placeholders, then capture every answer with its arrival time (relative to the send) until `wait` (default 8 s) or, with `until=ended`, `grace` (default 700 ms) after the first `SCSkillEnded` of the bot's own cast |
| `lookexpect <expectation…>` \| `none` | Check the capture of the last `lookcs`: the relevant packets (skill started / fired / ended / stopped, `SCErrorMsg`, `SCItemTaskSuccess`) must match the list exactly and in order; `none` means no relevant packet at all |
| `lookgap <from> <to> <min> <max>` | Assert the gap in milliseconds between two packets of the last capture (`send`, `started`, `fired`, `ended`, `err`, `task`, optionally `#n` for the n-th) |

Body placeholders (`{...}` between literal hex bytes; `SELF` and `PEER` are the 3-byte object ids):
`{id:R}` / `{item:R}` item id (u64), `{tpl:R}` template (u32), `{grade:R}`, `{flags:R}`, `{slot:R}`,
`{st:R}` (u8), `{unit:R}` made unit (u32), `{count:R}` (i32) of the remembered item `R`, and
`{u8:N}` / `{u16:N}` / `{u32:N}` / `{u64:N}` literal numbers (decimal or `0x`).

Expectations: `started(skill[,castms][,echo][,fail=n])`, `fired(skill[,echo])`, `ended`,
`err(code[,param])` and `task(reason[:row,…])` with rows `bits(R,hex)` (SetFlagsBits), `seize(R)`,
`count(R,delta)` (AddStack delta), `image(R,tpl)` (UpdateDetail with that image; the other detail
bytes must equal the creation), `create(R)`, `remove(R)`; a bare `task(reason)` is an empty receipt.
`@lo-hi` after any token bounds its arrival time in milliseconds after the send. Skill packets of
other players in sight are listed in the transcript but never counted (skill packets are broadcast).
The transcript of every `lookcs` (request hex, per-packet `+ms` line with the body hex, ignored
packets) is printed, and a mismatch embeds it in the error.

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
| `authority-guards` | A loot take from 5+ m is refused while a melee-range take works (`systems/economy/looting.rs`); an in-place resurrection without an offer is refused, with a scroll offer (item 18456) it works once, and the resurrection point always works (`systems/skills/resurrect.rs`) |

The look suites guard the server side of the Bevy client's appearance UI. They are strict
(request bytes, packet order, row bytes, timing windows, database rows) and record the server's
CURRENT behaviour, known gaps included; none of them proposes a policy. Offline, the same behaviour
is pinned by `systems/economy/look_appearance_tests.rs` (`cargo test -p aaemu-game look_appearance`;
add `-- --ignored` with `AAEMU_COMPACT_DB` to run it on the real catalog).

| Suite | Guards |
| --- | --- |
| `look-appearance` | One GM character through the whole chain: Skinize the donor, `CS049` without powder (`SCErrorMsg` 722 naming the powder), Skinize an already skinized item (739), `CS049` with 12 powder, Cleanup, Cleanup again (737) |
| `look-skinize` | Skinize (skill 23741) with a powder stack of 1 / 2 / 3: `SetFlagsBits` 0x01 to 0x09 (reason 113), the powder is taken twice (stack ends at max(N-2, 0)), cast 5000 ms |
| `look-cleanup` | Cleanup (skill 23457) of a converted base with a consumable stack of 1 / 2 / 3: image 0 (reason 111), the consumable is taken twice, cast 3000 ms |
| `look-convert` | `CS049` with 9 / 10 / 12 powder: powder paid with reason 40 (Seize + empty receipt when used up), `Update` of the base with the donor's template as image (reason 111), donor Seize; no acknowledgement packet |
| `look-refusals` | Unknown ids (207), non-equipment base (737), item-cast admission (`CSStartSkill` for an item the character does not own, or with another item's skill id, is dropped without any packet) and cancellation (another object id or a missing target item: started, fired, ended, no rows) |
| `look-silent` | A base template without a recipe: no packet at all (the handler only logs `has no ItemLookConvert`); a second Cleanup inside the cooldown: `SCSkillStarted` with result 8 and timeline id 0 |
| `look-gaps` | Expected-current gaps: 8 powder convert and take all 8, cloaks cost 15 powder, a glider base is refused 737, base equal to image destroys the item |
| `look-quirks` | The item class is not policed: an armor donor and a non-equipment image item convert, Skinize flags any owned item |

The suites read database rows with `expectdb` shortly after an action, so the **runtime**
`Configurations/World.json` of the lane needs a short `AutoSaveInterval` (the QA lane uses `0.02`
minutes); `verify.sh` does not patch it. Each suite uses its own `gm` bots on fresh characters, so
they can be rerun; they never touch other accounts.

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
