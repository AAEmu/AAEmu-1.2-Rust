# Working with Config Files and Server Listings

- Audience: Contributors, players, and testers
- Last verified against: `rust-rewrite` on October 12, 2026
- Prerequisites: Basic JSON editing and AAEmu project structure familiarity

## Overview

AAEmu now stores login server listings in configuration (`GameServers`) instead
of MySQL `aaemu_login.game_servers`.

This allows consistent behavior across manual, Docker, and `cargo xtask dev`
workflows.

## Where the files are

Each server reads its configuration from its *application directory*:

| Setup | Login | Game |
| --- | --- | --- |
| Source checkout (`cargo run`) | `rust/crates/aaemu-login/app/` | `rust/crates/aaemu-game/app/` |
| Published build | `AAEmu.Login/` (next to the binary) | `AAEmu.Game/` (next to the binary) |
| Docker | `.server_files/AAEmu.Login/` | `.server_files/AAEmu.Game/` |

The directory is chosen in this order: the `AAEMU_APP_PATH` environment
variable; the executable's directory if it contains `Config.json`; the current
directory if it contains `Config.json`; otherwise the crate's `app/` folder.
The game server also accepts the directory as its first argument
(`aaemu-game <APP_PATH>`).

## Login server `Config.json` and `Config.Local.json`

The login server reads `Config.json`, then `Config.Local.json`, then
environment variables, then command-line arguments (`--Section:Key=value`).
Use `Config.Local.json` (gitignored) for machine-specific overrides;
`ExampleConfig.Local.json` is a starting point.

### `GameServers` schema

`GameServers` is a list of server entries with this shape:

```json
{
  "GameServers": [
    {
      "Id": 1,
      "Name": "AAEmu.Game",
      "Host": "127.0.0.1",
      "Port": 1239,
      "Hidden": false,
      "MaxConcurrent": 0,
      "QueueLimit": 100
    }
  ]
}
```

Field meanings:

- `Id`: unique game server id.
- `Name`: display name in server selection.
- `Host`: address reachable by clients.
- `Port`: client connection port.
- `Hidden`: whether to hide this entry from listing.
- `MaxConcurrent` (default `0`): world login queue capacity, in slots (accounts in
  the game lobby/world plus world cookies the client has not consumed yet). `0`
  disables the queue and is the behaviour before the queue existed. See
  "World login queue" below.
- `QueueLimit` (default `100`): maximum number of clients waiting in that world's
  queue. When it is reached a new enter-world request is refused with
  `ACEnterWorldDenied`. `0` means the packet field limit (`65535`); it is ignored
  while `MaxConcurrent` is `0`.

### World login queue

The login server queues `CAEnterWorld` instead of passing it on when the world is
at `MaxConcurrent` slots. The game server reports its slot occupancy (accounts in
the lobby/world plus unconsumed world cookies) in `GL_GAME_SERVER_LOAD` and in
every `GLPlayerEnter` response, so the login server knows when a slot frees and
admits the head of the FIFO queue without waiting for a client to retry.

```json
{ "Id": 1, "Name": "AAEmu.Game", "Host": "127.0.0.1", "Port": 1239,
  "MaxConcurrent": 200, "QueueLimit": 100 }
```

- While queued, the client receives `ACWorldQueue` with its 1-based position and
  the normal-queue length; every queue change sends the new positions.
- When a slot frees, the head receives `ACWorldQueue` with `myTurn = 0` and then
  the normal `ACWorldCookie`; the next client moves to position 1.
- `CACancelEnterWorld` and disconnecting remove the entry. Entries die with their
  connection.
- A reconnect token still bypasses the world list/queue path: it is issued by the
  game server only after a fully authenticated session, as before.
- Refusals use `ACEnterWorldDenied`. The 1.2 client's handler (`FUN_393d51d0`)
  ignores the reason byte and shows "error_failed_to_connect_to_server".
- **Capture-pending (account WP-H):** the premium-entrance bit of
  `CAEnterWorld.flag` is not identified, and the login server has no premium
  source in 1.2 data. v1 treats every client as normal: `isPremium = false`,
  `premiumLength = 0`.

The queue is per configured `GameServers` entry. It starts at occupancy 0 and is
updated by the game server's first load report (sent when it registers); a mixed
version game server that does not send the occupancy extension reads as 0, which
disables the gate (fail-open).

### Character slots and the pre-select period

The login server tells the client how many characters an account has:

```json
{
  "Login": {
    "MaxCharactersPerAccount": 6,
    "AdditionalCharactersPerServer": 0,
    "PreSelectCharacterPeriod": false
  }
}
```

- `MaxCharactersPerAccount` (default `6`): base character slots of every account,
  sent in `ACJoinResponse` and `ACAuthResponse`. The game server's
  `Characters:MaxPerAccount` must be the same value; it enforces the limit.
- `AdditionalCharactersPerServer` (default `0`): the expandable limit the client
  shows for slot-increase items.
- `PreSelectCharacterPeriod` (default `false`): tells the client it may edit
  characters at character select. It requires the lobby character-edit packet
  (WP-E2), which is not implemented yet: with the flag on, the client's Start
  button sends `CSEditCharacter` instead of entering the world, so the login
  server refuses a config with `PreSelectCharacterPeriod = true`.

Purchased character slots (`accounts.extra_character_slots`, from the slot
increase item) are sent by the game server's `SCGetSlotCount` as the purchased
count only; the client adds them to the login base. The game server's create
check enforces the same sum, so an account can create up to
`MaxCharactersPerAccount + extra_character_slots` characters. Nothing
cross-checks the two servers' configs: set both base values equally and confirm
the effective values in the `Character slots: base ...` startup log line of each
server. If the game server cannot read `extra_character_slots` from MySQL, it
logs the error and falls back to the base alone.

### Environment variable mapping (login)

You can define server entries via environment variables:

```text
GameServers__0__ID=1
GameServers__0__Name=AAEmu.Game
GameServers__0__Host=127.0.0.1
GameServers__0__Port=1239
GameServers__0__Hidden=false
GameServers__0__MaxConcurrent=200
GameServers__0__QueueLimit=100
```

## Game server configuration and precedence

The game server supports `Config.Local.json` as the final override layer.

Effective load order (paths relative to the game application directory):

1. `Config.json`
1. `Configurations/*.json` (all matching files)
1. `Config.Local.json`
1. Environment variables (`Section__Key`, for example
   `Connections__MySQLProvider__Password`)
1. Command-line arguments (`--Section:Key=value`)

If the same setting exists in multiple places, the later source wins, so
`Config.Local.json` overrides all JSON files. Environment variables are a good
place for passwords you do not want in a file. (A user-secrets file left over
from the C# version is still read between `Config.Local.json` and the
environment variables, but it can no longer be managed with a tool; move those
values to `Config.Local.json` or environment variables.)

### Game character settings (`Characters`)

The game server's `Configurations/CharacterSettings.json` carries the base slot
count the login server advertises:

```json
{
  "Characters": {
    "MaxPerAccount": 6
  }
}
```

- `MaxPerAccount` (default `6`): must equal the login server's
  `Login:MaxCharactersPerAccount`. The two values are not cross-checked across
  processes; the startup log prints the effective value on each server.
  Character creation is refused once the account already has
  `MaxPerAccount + extra_character_slots` characters (a MySQL read error on the
  extra slots falls back to the base).

It can also be overridden in the game's `Config.Local.json` or with the
environment variable `Characters__MaxPerAccount`.

## Web API authentication and migration

The game Web API now defaults to `127.0.0.1:1280`. Only `GET /status` is
anonymous; it returns uptime, player count and task/skill metrics. Every other
registered route requires `Authorization: Bearer <key>`, including character,
world and auction reads, mail listing, GM commands and mail with attachments.
Missing, invalid or duplicate credentials return HTTP 401 with a Bearer challenge.
Commands still use the selected online character's command access level.

For local status tooling, use this game's `Config.Local.json` override:

```json
{
  "WebApiNetwork": {
    "Host": "127.0.0.1",
    "Port": 1280,
    "Enabled": true,
    "ApiKey": null
  }
}
```

An absent, null or empty `ApiKey` disables access to every authenticated route,
including requests from localhost. To enable administrative tools, configure a
private, randomly generated key of at least 32 random bytes in the game process's
`WebApiNetwork__ApiKey` environment variable, or in the gitignored
`Config.Local.json` field `WebApiNetwork.ApiKey`. Keys accept ASCII letters,
digits and `-._~+/=`; whitespace is rejected. Keep the key separate from the
Login/Game `SecretKey`. Never put the key in URLs or commit it.

For example, generate a key privately in PowerShell before starting the game
process from the same environment (these commands do not print the key):

```powershell
$webApiRandom = [System.Security.Cryptography.RandomNumberGenerator]::Create()
$webApiBytes = New-Object byte[] 32
$webApiRandom.GetBytes($webApiBytes)
$webApiRandom.Dispose()
$env:WebApiNetwork__ApiKey = [Convert]::ToBase64String($webApiBytes)
```

An administrative caller using that same private environment can supply the header:

```powershell
$webApiHeaders = @{ Authorization = 'Bearer ' + $env:WebApiNetwork__ApiKey }
Invoke-RestMethod -Uri http://127.0.0.1:1280/api/world/logged-characters -Headers $webApiHeaders
```

Existing configurations with `Host: "*"`, `0.0.0.0`, `::`, or any other
non-loopback address now fail startup unless a nonempty valid key is configured.
For local monitoring, change the host to `127.0.0.1`; for remote administration,
configure the private key and use an HTTPS reverse proxy or SSH tunnel so the
Bearer token is encrypted in transit. The HTTP listener itself does not provide TLS.
Authentication does not make anonymous routes private: `/status` remains public
on the configured interface. Restart the game process after changing the bind,
key or `Enabled` setting; configuration reload does not restart the listener or
rotate its credential snapshot.

To disable the API even when the shipped config contains the section, set
`"WebApiNetwork": { "Enabled": false }` in `Config.Local.json`. A missing or null
section also disables it. Existing clients must add the Bearer header to every
`/api` request; local `/status` checks continue to work without credentials.

## Tower defense event schedules

Tower defense controllers are reserved for events instead of spawning permanently.
Automatic events are disabled by default. A level-100 GM can use `/towerdef list`
in the event's zone to see its world/zone/group identity, which definitions are ready,
and why others are unavailable.
`/towerdef start <id>` starts the event in the GM's current world instance and zone
group; `/towerdef next <id> <wave>` advances exactly one wave (numbered from 1),
and `/towerdef end <id>` stops it and removes its active controllers and summons.
Dead NPCs keep their normal corpse timers so wave changes and event completion do
not remove loot before players can collect it. Removing an instance clears its corpses too.
The `tower_def` spelling is also supported. Players entering a zone receive the
active event list, including the current wave.

To schedule a verified definition, add this section to the game's `Config.Local.json`
and replace the event identity with the values for its zone:

```json
{
  "Dungeons": {
    "TowerDefense": {
      "Enabled": true,
      "Events": [
        { "Id": 12, "WorldId": 0, "ZoneGroupId": 23, "ZoneId": 274 }
      ]
    }
  }
}
```

The example enables Hasla (12) at game time 18:00 using the shipped 1.2 data.
For other events use the identity printed by `/towerdef list` in their zone.
The current read-only audit of the shipped 1.2 compact and main_world spawn data
passes preflight for these identities (`WorldId: 0`):

| Definition ids | ZoneGroupId | ZoneId |
| --- | --- | --- |
| 3, 13 | 20 | 257 |
| 5, 15 | 17 | 139 |
| 6 | 56 | 275 |
| 12 (Hasla) | 23 | 274 |
| 18, 19, 20 | 22 | 206 |
| 27 | 63 | 288 |

Events sharing NPC controllers cannot run together. Select the definitions you
intend to test; this preflight checks content availability, not live client gameplay.
Mistmerrow's conflict configuration references definition 33, which is absent from
the shipped compact and still needs its event content restored.

`WorldId` is the server instance id (0 for main_world),
`ZoneId` is the zone key, and `ZoneGroupId` is its compact zone group. The server
validates every selected definition, wave, target and controller position before
starting the scheduler. Use an empty `Events` list or `Enabled: false` to leave
automatic scheduling off. Disabling the scheduler does not cancel a running event;
use `/towerdef end` for that.

Schedules follow compact `tod` / `tod_day_interval` on the accelerated game clock;
day intervals restart from day 0 when the scheduler starts. Wave delays and force-end
times use real seconds. A configured event matching a conflict zone's
`war_tower_def_id` starts on entering War and ends on leaving War, instead of using
`tod`. Events do not replay during the same clock crossing or War period.

The runtime supports NPC spawner waves, NPC kill targets and `DoodadAlmighty` waves.
Doodad waves use authored doodad positions in the event's zone group and are removed
according to `despawn_on_next_step` or when the event ends. Missing authored NPC or
doodad positions are rejected before the event starts. Definition 26 is a developer
test in the 1.2 compact; its doodad 7297 and NPC controller lack authored positions.
Victory reward event chains are not implemented. Existing NPC skills, AI and quests
handle controller behavior and ordinary drops; confirm the event's gameplay with
the target 1.2 client before exposing it to players.

## Logging

Logging is not configured in `Config.json`. Both servers log to stdout; the
level is set with the `RUST_LOG` environment variable (default `info`, for
example `RUST_LOG=debug` or `RUST_LOG=aaemu_game=debug,info`). `NO_COLOR=1`
disables colored output.

## `game_pak` configuration

Set `game_pak` source in one of these places:

- `Configurations/ClientData.json`, or
- `Config.Local.json` for local override,

or put `game_pak` into the `ClientData/` folder of the game application
directory.

For contributor workflows, prefer `Config.Local.json`.

## Physics (`Configurations/Physics.json`)

The game server builds a collision world per zone from the client data
(terrain, brushes, vegetation, doodad colliders, water) on `aaemu-physics`, a
port of the client's CryPhysics. It is used for line-of-sight, blink/teleport
targets, falls, NPC movement and ships. Settings in the `Physics` section
(all optional):

| Key | Default | Meaning |
| --- | --- | --- |
| `Enable` | `true` | Collision worlds and queries; `false` answers from the heightmap only |
| `Geometry` | `Full` | `Terrain`, `Brushes` (+ `brush.dat`) or `Full` (+ vegetation) |
| `LoadRadiusCells` | `1` | Cells (1024 m) kept loaded around players, ships and active NPCs (1 = 3x3) |
| `LoadOnQuery` | `true` | A query in an unloaded cell loads it synchronously |
| `UnloadAfterSeconds` | `600` | Unload cells nobody was near for this long (0 = never) |
| `SyncPeriodMs` | `1000` | Period of the cell residency / doodad collider sync |
| `DoodadColliders` | `true` | Colliders for doodads and houses from their models |
| `PreloadAllCells` | `false` | Load every cell at startup (slow; for measurements) |
| `MovementVerifyEnable` | `false` | The zone server's movement-hack checks |
| `FallDamageZoneCap` | `true` | Cap fall damage like the zone server |
| `CryPhysicsShips` | `true` | Simulate ships on the CryPhysics port |

Physics needs client data (`game_pak`); without it the server falls back to
the heightmap. Lower `Geometry` or `LoadRadiusCells` to save memory on small
machines. Background:
[`rust/docs/physics/`](https://github.com/AAEmu/AAEmu/tree/develop/rust/docs/physics).

## Territory property tax

Claimed Dominion provinces apply their current territory tax rate to ordinary taxable housing
in the same zone and world instance. Heavy-tax-exempt buildings also remain exempt from territory
tax. The refundable construction deposit remains twice the template's base tax.

The default world rule applies territory tax after the multiple-property surcharge. To apply it
only to the template's base tax, add this game-server override to `Config.Local.json`:

```json
{
  "Housing": {
    "DominionPropertyTaxBase": "Base"
  }
}
```

`AfterHeavy` is the default. The order is configurable; nominal surcharge-to-gold credit is an
explicit server rule because the available 1.2 client does not establish the original world calculation.
Certificate bill totals round up to whole certificates, and the client displays the amount
actually charged. Refunded deposits do not generate territory revenue.

The owning guild's current leader can collect actual property revenue through the player
command `/collectdominion <zoneGroupId>`. Collection sends a system tax receipt with gold;
If staging the receipt fails, the balance stays available. Repeated empty collection sends nothing.
It leaves Peace Fund untouched. Hunting tax, AA Points, nation splits, and automatic historical
payout schedules are not added. Territory tax changes have the original one-hour cooldown.

## Migration note from old setup docs

Older instructions referenced `aaemu_login.game_servers` and SQL inserts.
That is no longer the source of truth for server listings.

Use login `GameServers` configuration instead.

## Related

- [Installation & Setup](Installation-&-Setup)
- [Local Development Guide](Local-Development-Guide)
- [Mini troubleshoot guide](Mini-troubleshoot-guide)
- [FAQ](FAQ)
