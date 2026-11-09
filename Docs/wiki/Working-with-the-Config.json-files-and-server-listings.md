# Working with Config Files and Server Listings

- Audience: Contributors, players, and testers
- Last verified against: `main` on November 8, 2026
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
- `PreSelectCharacterPeriod` (default `false`): the character pre-select
  period. When on, the client's Start button opens the character-edit window
  instead of entering the world (`CSEditCharacter`), so the player can still
  change a freshly created character's name, appearance, body items and ability
  before playing; world entry (`CSSelectCharacter`) is refused until the period
  ends. The login reports this value to the game server when it registers, and
  the game applies it, so the two servers cannot disagree. Only the account's
  own, not-deleted character that never entered the world can be edited. Leave it off for
  normal play.

Purchased character slots (`accounts.extra_character_slots`, from the slot
increase item) are sent by the game server's `SCGetSlotCount` as the purchased
count only; the client adds them to the login base. The game server's create
check enforces the same sum, so an account can create up to
`MaxCharactersPerAccount + extra_character_slots` characters. Nothing
cross-checks the two servers' configs: set both base values equally and confirm
the effective values in the `Character slots: base ...` startup log line of each
server. If the game server cannot read `extra_character_slots` from MySQL, it
logs the error and falls back to the base alone.

### Two-factor authentication (`TwoFactor`)

The login server reads `user_2fa` rows for OTP, PC-certificate PIN and ARS (phone
callback). OTP works in-tree with a TOTP app:

```json
{
  "TwoFactor": {
    "MaxOtpFailures": 5,
    "OtpLockoutMinutes": 15
  }
}
```

ARS needs an **operator provider**. The repository has the `ArsPhoneProvider` seam
and a `DisabledArsPhoneProvider` default that refuses ARS logins with a warning;
placing the call needs telephone integration, so an operator must supply that
implementation. It is not a configuration toggle.

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
count the login server advertises and the pre-select period:

```json
{
  "Characters": {
    "MaxPerAccount": 6,
    "PreSelectPeriod": false
  }
}
```

- `MaxPerAccount` (default `6`): must equal the login server's
  `Login:MaxCharactersPerAccount`. The two values are not cross-checked across
  processes; the startup log prints the effective value on each server.
  Character creation is refused once the account already has
  `MaxPerAccount + extra_character_slots` characters (a MySQL read error on the
  extra slots falls back to the base).
- `PreSelectPeriod` (default `false`): fallback for the login server's
  `Login:PreSelectCharacterPeriod`. The game learns the effective value from the
  login's game-server registration reply and applies it (a mismatch is logged as
  a warning); this local value is only used when the login does not report it
  (an older login). While the period is on, `CSEditCharacter` is applied for the
  account's own, undeleted character that never entered the world, and world
  entry is refused. The edit re-runs character creation with the edited name,
  appearance, body items and ability. The startup log prints `Character
  pre-select period on/off`. A refused edit sends no reply (the 1.2 client has
  no failure path for it) and logs the reason.

Both can also be overridden in the game's `Config.Local.json` or with the
environment variables `Characters__MaxPerAccount` and `Characters__PreSelectPeriod`.

## Server era profile (`World:Profile`)

The game server emulates a retail era. The owner decision of 2026-11-08 makes the Trion EU
"Auroria" era the default: the Trion 1.2 build from the Conquest of Auroria (2014-11-04 NA /
2014-11-06 EU) to the day before Secrets of Ayanad (2015-03-10).

```json
{
  "World": {
    "Profile": "EuAuroria"
  }
}
```

| Value | Era | Player level cap | Player nations | Sieges / Auroria claims |
| --- | --- | --- | --- | --- |
| `EuAuroria` (default) | Trion EU Auroria, 2014-11-04/06 to 2015-03-09 | 50 | off | on |
| `EuLaunch` | Trion NA/EU launch, 2014-09-12/16 to 2014-11-03 | 50 | off | off |
| `Kr12` | XL KR 1.2 data (`r208022`) | 55 | on | on |

The profile only supplies **defaults**; explicit keys win:

- `Social:PlayerNations` (client switch and server gate) defaults from the profile; set it to
  `true` or `false` to override. The same value drives the client `FeatureSet.nations` bit.
- `Housing:SiegeCalendar` defaults from the profile; set it to `true` or `false` to override.
  It switches the inferred weekly siege calendar only. Auroria claims and the client
  `FeatureSet.siege` bit follow the profile (`on` for `EuAuroria` / `Kr12`, `off` for
  `EuLaunch`): under `EuAuroria`, `Housing:SiegeCalendar=false` (GM-scheduled sieges only)
  still allows claims and shows the siege UI. An explicit `FeatureSet.siege` value still wins
  for the client bit.
- The player level limit (client `FeatureSet` byte 1) follows the profile cap. A character
  saved above a lowered cap keeps its level and gains no further levels; experience is trimmed
  at that character's level, exactly as it is at the cap today. Mates keep their own cap of 50.
- A nation already stored in the database is not deleted when a profile without nations is
  selected; the nation features refuse new actions instead.
- **Upgrade note:** before the era profile existed, `Housing:SiegeCalendar` defaulted to
  `false`. A configuration without the key now gets the weekly calendar from the default
  profile (`EuAuroria`: on). The calendar rules are inferred from the 1.2 tables with
  corroborated times; set `Housing:SiegeCalendar=false` to keep the old explicit-GM-only
  calendar behaviour. That does not disable Auroria claims or the client siege UI.

Set the key in `Configurations/World.json`, `Config.Local.json`, with the environment variable
`World__Profile`, or on the command line (`--World:Profile=Kr12`). The active profile is logged
once at startup. Trion dates and sources: `rust/crates/aaemu-game/PORTING.md`
("Server era profile") and the audit research `research/trion-1.2-eu-na.md`.

## Account payment and premium (`Account:DefaultPayment`)

The game server loads each account's payment state from `aaemu_game.accounts`
(`pay_method`, `pay_location`, `pay_start`, `pay_end`, `premium_point`). A row
whose `pay_method` is still `0` uses the configured default:

```json
{
  "Account": {
    "DefaultPayment": "premium"
  }
}
```

`premium` (default) keeps the old C# behaviour: the account is permanently
premium (no end date), gets the premium grade from the premium point
(`premium_grades`, grade 2 at point 1) and the premium max labor from
`premium_benefits` (5000 instead of 2000). `none` makes new and unset accounts
normal; `demo` is the third 1.2 payment method. The numeric values are the
client's `SCAccountInfo.payMethod`: 1 Premium, 3 Demo, 5 None.

Server owners can grant or remove premium in game with
`/premium <account id|character name|self> <on|off|days> [days]` (access level
100): `on` grants 30 days, a number grants that many days, `off` removes it.
The grant has its own end date; the configured default does not. The grant
writes the account row and pushes `SCAccountInfo` and `SCUpdatePremiumPoint`
to the account.

## Cash shop (ICS) content is operator policy

The cash shop reads `aaemu_game.ics_menu`, `ics_shop_items` and `ics_skus`. The base
schema creates the tables but ships no catalogue, because no 1.2 retail content is
evidenced; the tabs, SKUs, prices, stock limits and currencies are server-owner
policy. `SQL/examples/example-ics-default-en.sql` is an optional example catalogue
(English labels and example prices) that can be imported and edited; it is not part
of the base schema or the required updates. Check each SKU's `currency` (credits,
AA points, loyalty or coins) and price before importing. AA points are not
supported: AA-priced SKUs (`currency = 1`) are refused with error 592 and nothing is
debited.

## Account restrictions and idle kick (`Account:Restrictions`, `Account:IdleKickTime`)

`SCInitialConfig` carries seven `canXxx` booleans and an `idleKickTime` value. The 1.2 client
only uses the booleans to gate its own UI (place house, pay tax, auction, trade, mail, bank,
copper); the server has no restriction store and does not enforce them. They are configured per
effective payment method, and every default is `true` (the bytes the server sent before these
settings existed):

```json
{
  "Account": {
    "Restrictions": {
      "Premium": { "CanTrade": true },
      "Demo": {
        "CanPlaceHouse": false,
        "CanUseAuction": false,
        "CanTrade": false,
        "CanUseCopper": false
      },
      "None": {}
    },
    "IdleKickTime": 0
  }
}
```

A flag that is not listed keeps its default (`true`). The per-method source is inferred
(`ACCOUNT-RESTRICT-1`, capture-pending); the client reader only names the flags.

`IdleKickTime` is the raw `u16` sent to the client (`0` = off). Its unit is **UNKNOWN**: the
client performs the idle kick itself and the `CGame::Update` consumer was not located, so it is
not documented as minutes or seconds. The server never kicks an idle player and sends no
`SCIdleKick`. The default `0` keeps the previous bytes.

The unit needs one real-client run, because the kick is decided by the real client and
the headless `aaemu-testclient` does not emulate it: set `Account:IdleKickTime` to `1`,
log in with the 1.2 client and watch how long it stays; about 1 second means seconds,
about 1 minute means minutes. Until then the value stays raw.

Both settings can be overridden in `Config.Local.json` or with the environment variables
`Account__IdleKickTime` and e.g. `Account__Restrictions__Demo__CanTrade`.

## Trion web URLs (`Account:Trion`)

`SCTrionConfig` (sent in the lobby) carries an `activate` flag and the three URLs the client's
embedded browser panels load. The defaults are the former hard-coded test URLs:

```json
{
  "Account": {
    "Trion": {
      "Activate": true,
      "Auth_Url": "http://localhost/aaemu/login",
      "Platform_Url": "http://localhost/aaemu/platform",
      "Commerce_Url": "http://localhost/aaemu/shop"
    }
  }
}
```

`Activate` is the packet's `activate` flag (`true` keeps the previous bytes).
`Auth_Url`, `Platform_Url` and `Commerce_Url` are the login, platform and shop folder URLs.
What the client does with `Activate: false` is **UNKNOWN** (CAP-UI-5: the 1.2 consumer of the
struct was not traced); it is intended as the way to hide the panels once verified.

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

### Read-only monitoring endpoints

Two tooling routes, added in Rust only (the 1.2 client has no web API), also require
the same Bearer credential as the `/api` routes:

* `GET /online` — the characters currently in the world:

  ```json
  {
    "Count": 2,
    "Characters": [
      {
        "Id": 1,
        "Name": "Example",
        "Level": 55,
        "Race": 1,
        "FactionId": 101,
        "FactionName": "",
        "ExpeditionId": 7,
        "ExpeditionName": "Example Guild",
        "ZoneId": 15
      }
    ]
  }
  ```

  Characters ordered by id. `ExpeditionName` is `null` without a guild. Only the
  zone id is exposed — no account data, addresses or positions.

* `GET /dominions` — one object per persisted dominion zone group, ordered by zone
  group id:

  ```json
  [
    {
      "ZoneGroupId": 25,
      "ZoneGroupName": "Solzreed",
      "OwnerExpeditionId": 7,
      "OwnerExpeditionName": "Example Guild",
      "SiegePeriod": 3,
      "NextSiegeStart": "2026-11-08T20:30:00Z",
      "NextSiegeEnd": "2026-11-08T22:30:00Z",
      "DeclarationAttacker": { "ExpeditionId": 9, "Name": "Challengers" }
    }
  ]
  ```

  `OwnerExpeditionId` / `OwnerExpeditionName` are `null` for an unowned territory
  (`SiegePeriod` 0). `SiegePeriod` is the 1.2 period id: 0 no dominion, 1 auction,
  2 declare, 3 warmup, 4 siege, 5 peace. `NextSiegeStart` / `NextSiegeEnd` describe
  an explicit scheduled siege, or the declared calendar cycle's authored siege
  window, and are `null` when neither applies. `DeclarationAttacker` is `null` when
  no expedition has an accepted declaration. These endpoints are read-only and do
  not change any manager state.

## Tower defense event schedules

Tower defense controllers are reserved for events instead of spawning permanently.
With no `Events` list, the server derives the 1.2 open-world events from the data and
runs them by default: the Halcyona war (18, started when zone group 22 reaches War) and
its victory follow-ups (19, 20), the Crimson Rift (3, 5), the Grimghast Rift (13, 15),
the Hasla Rift (12) and the Oblivion Rift / Clockwork Rebellion (28, 29, 31, 32).
`"Enabled": false` turns every event off; a non-empty `Events` list replaces the
derived schedule, and an event the data cannot run (no authored controller positions
or a closed zone) is skipped at startup with a warning and is not listed as enabled.
The startup log prints the enabled list with each event's zone group and zone.
A level-100 GM can use `/towerdef list`
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
| 8, 27 | 63 | 288 |
| 12 (Hasla) | 23 | 274 |
| 18 (Golden Plains war), 19, 20 | 22 | 206 |
| 28, 31 (Oblivion Rift) | 6 | 144 |
| 29, 32 (Clockwork Rebellion) | 24 | 158 |

Events sharing NPC controllers cannot run together. Select the definitions you
intend to test; this preflight checks content availability, not live client gameplay.
Definitions 19 and 20 are the victory follow-ups of the Golden Plains war (18): the
death of the losing side's relic in its last wave starts them in the same zone, so do not
put them in `Events` (a configured follow-up is never started by the clock).
Definitions 6 (Sungold Fields), 8 and 27 (Reedwind) pass this preflight but stand in a
closed 1.2 zone, so the derived defaults leave them off; they still run if listed
explicitly in an open world. The other definitions of the 1.2 data have no authored
positions and stay unavailable:
4, 14, 16, 17, 21, 23, 24, 25 and 26 (`/towerdef list` names the missing spawner), and
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
Wave spawners whose members are NPC groups (the Oblivion Rift, Clockwork Rebellion)
spawn every group member at the group's sites listed in `npc_spawns.json`.
Doodad waves use authored doodad positions in the event's zone group and are removed
according to `despawn_on_next_step` or when the event ends. Missing authored NPC or
doodad positions are rejected before the event starts. Definition 26 is a developer
test in the 1.2 compact; its doodad 7297 and NPC controller lack authored positions.
The only event chain is the Golden Plains victory follow-up described above; no other
rewards are synthesized. Existing NPC skills, AI and quests
handle controller behavior and ordinary drops; confirm the event's gameplay with
the target 1.2 client before exposing it to players.

## Scheduled event announcements (`Events:ScheduledEvents`)

The server can announce the opening and closing of a `game_schedules` window; the 1.2 client
shows `SCScheduledEventStartedPacket` (0x1ec) as a centre banner and turns on its day-event HUD
button, and `SCScheduledEventEndedPacket` (0x1ed) closes it. The feature is **off by default**
and announces only allow-listed schedule ids. The packet layout is taken from the 1.2 client;
the original zone server's trigger and texts are not captured, so the behaviour below is
**inferred**.

Add this to the game's `Config.Local.json`:

```json
{
  "Events": {
    "ScheduledEvents": {
      "Enabled": true,
      "Events": [
        {
          "ScheduleId": 14,
          "OpenMsg": "The weekend dungeon is open.",
          "CloseMsg": "The weekend dungeon has closed."
        }
      ]
    }
  }
}
```

`ScheduleId` is a `game_schedules.id` of `compact.sqlite3` (14 is the shipped
`[주말던전] 영원의 섬` weekend-dungeon window; query the file with any SQLite tool).
`OpenMsg` / `CloseMsg` are sent verbatim and cut to 127 UTF-8 bytes (the client's buffer).
The list is validated at startup: an unknown schedule id is skipped with a warning, a
duplicate id keeps the first entry, and an overlength message is warned about.
`Enabled: false` or an empty `Events` list (the default) disables the feature.

The list is evaluated every 30 seconds with the schedule check that already gates scheduled
NPCs, doodads and quests. The first evaluation after a restart only records the current state,
so a running event is not re-announced; players entering the world while it is open receive
Started. The 1.2 client keeps **one** active event at a time: with overlapping windows a later
Started replaces the previous one and the earlier id's Ended is ignored, so allow-list one
event per time window. Rows without a dated end (the all-zero placeholder rows) stay open
forever under this check; use rows with real dates.

## Dungeon PvP and resurrection flags

`compact.sqlite3` `indun_zones` stores `pvp` and `has_graveyard` per dungeon zone
group, and the 1.2 client reads both. The server now follows them. Two switches in
`Configurations/Dungeons.json` (or `Config.Local.json`) control the new rules; both
default to `true` and can be turned off to restore the old faction-only behaviour:

```json
{
  "Dungeons": {
    "EnforceNoPvp": true,
    "NoGraveyardUseReturnPortal": true
  }
}
```

- `EnforceNoPvp`: in an instance whose `indun_zones.pvp` is `f` (Mirage Isle 49,
  Immortal Isle 62/64, the Library 70-76), a player, pet or slave cannot attack
  another player-owned unit. The 1.2 client already treats such pairs as
  non-hostile; the server rule is inferred from that client behaviour. NPC targets
  and the faction relation used for buffs and heals are unchanged.
- `NoGraveyardUseReturnPortal`: in an instance whose `indun_zones.has_graveyard` is
  `f` (Mirage Isle 49, the prologue 68), a temple resurrection sends the character
  to the closest open-world return portal (the ordinary "Revive" the client shows
  for those zones) and leaves the instance. If no return portal exists, the Temple
  Priestess, then the instance entry point, then an in-place revive are used (with
  a server warning). `has_graveyard = t` keeps the entrance/Priestess behaviour.
  The destination is inferred; the client evidence is only which revive window it
  shows.

## Logging

Logging is not configured in `Config.json`. Both servers log to stdout; the
level is set with the `RUST_LOG` environment variable (default `info`, for
example `RUST_LOG=debug` or `RUST_LOG=aaemu_game=debug,info`). `NO_COLOR=1`
disables colored output. `AAEMU_LOG_MS=1` adds six fractional digits to the time of every
line (`HH:MM:SS.ffffff`); without it the format stays `HH:MM:SS`.

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

## NPC interaction reach (`Configurations/Interaction.json`)

The 1.2 client opens and keeps an NPC's interaction window only within 3.0 m of
the target's shape (`FUN_393bef10` / `FUN_3932a3b0`, compared against the
squared constant 9.0), but it does not gate the `CSStartInteraction` send, so a
forged request can arrive from any distance. The game server refuses a request
farther than the reach silently: no 1.2 error message for this case is
evidenced.

| Key | Default | Meaning |
| --- | --- | --- |
| `NpcReachCheck` | `true` | Refuse `CSStartInteraction` farther than the reach; `false` restores the old behaviour (no check) |
| `NpcReachToleranceMeters` | `2.0` | Extra metres on top of the 3.0 m client reach for movement and latency (**inferred**; the original zone server's answer is not captured) |

The distance is measured with the same model-radius helper the skill range
checks use, so a large NPC is not refused at its edge. Override it in
`Config.Local.json`:

```json
{
  "Interaction": {
    "NpcReachToleranceMeters": 1.0
  }
}
```

## Social feature switches (`Configurations/Social.json`)

Rust-only switches for social content whose 1.2 behaviour is not fully
evidenced. Defaults are shipped in `Configurations/Social.json` and can be
overridden in `Config.Local.json` or with `Social__BountyBoard`.

| Key | Default | Meaning |
| --- | --- | --- |
| `BountyBoard` | `false` | Fill the `CSNaviOpenBounty` answer with the wanted-player board (characters that have an unpaid bounty) instead of the empty list. Rows are capped at the client's 20 entries and online / offline characters are both listed. `false` keeps the C# server's empty list. |
| `PlayerNations` | `World:Profile` | Player nations (independence, diplomacy, national tax, monuments). The default follows the era profile (`on` for `Kr12`, `off` for `EuAuroria` / `EuLaunch`); an explicit `true` / `false` wins. When off, founding uses the existing precondition refusal and the client `FeatureSet.nations` bit is off. |

**Capture-pending:** the C# server noted that a filled board broke the 1.2
client Lua and the cause was never confirmed. Keep `BountyBoard` off until an
original-client run opens the board without a Lua error. Row details and the
open question are recorded in `rust/crates/aaemu-game/PORTING.md`
(BOUNTY-board).

## Client feature switches (`Configurations/FeatureSet.json`)

The client FeatureSet is the 11-byte blob the game server sends in
`SCInitialConfig.fsets`; the 1.2 client turns it into the Lua table behind
`X2Player:GetFeatureSet()` (`FUN_39496b20`, `x2game.dll.c:956456`), where each
named key is one bit. The game server reads `Configurations/FeatureSet.json`
(section `FeatureSet`) at startup and applies it after loading the level caps.
Keys that follow the era profile are omitted from the shipped file, so the
`World:Profile` value decides them; every other shipped key reproduces the
previous bytes. Override a key in `Config.Local.json` or with the environment
form `FeatureSet__Siege=false`.

```json
{
  "FeatureSet": {
    "siege": false,
    "aaPoint": false,
    "ExtraBits": { "78": true }
  }
}
```

| Key | Bit | Default | What the client uses it for |
| --- | --- | --- | --- |
| `siege` | 0 | `World:Profile` | Siege / dominion UI. The exact window is INFERRED from the Lua key. The default follows the profile (on for `EuAuroria` / `Kr12`, off for `EuLaunch`), independently of `Housing:SiegeCalendar`; an explicit value wins. |
| `nations` | 6 | `Social:PlayerNations` (profile) | Player-nation UI. The default is the resolved server flag (on for `Kr12`, off for the EU profiles); an explicit value wins. |
| `premium` | 4 | `true` | Premium / Patron HUD and labor UI. |
| `itemSecure` | 45 | `true` | Item-secure / lock handling and UI. |
| `secondpass` | 46 | `false` | Second-password entry and its opcode path. The second password is not supported, so keep it off. |
| `ranking` | 36 | `true` | Ranking window. |
| `slave_customize` | 48 | `true` | Vehicle / ship customization UI (INFERRED). |
| `bm_mileage` | 43 | `true` | Cash-shop mileage display (INFERRED; Korean business model). |
| `ingamecashshop` | 38 | `true` | In-game cash-shop entry and open guards. |
| `customsaveload` | 40 | `true` | Custom UI save/load (INFERRED). |
| `beautyshopBypass` | 52 | `true` | Beauty-salon bypass entry (INFERRED). |
| `ingameshopSecondpass` | 55 | `false` | Second-password gate on cash-shop purchases (INFERRED). Not supported, so keep it off. |
| `backpackProfitShare` | 56 | `true` | Trade-pack sale profit share and sale mails. |
| `sensitiveOpeartion` | 58 | `false` | Sensitive-operation confirmation prompts (INFERRED; the key's spelling is the client's). |
| `taxItem` | 59 | `true` | Tax-item payment UI; the server also uses this bit for its tax-item behavior. |
| `achievement` | 63 | `true` | Achievement window. |
| `dwarfWarborn` | 74 | `false` | Dwarf / Warborn race selection. The shipped `characters` data has both races `creatable = 'f'`, so turning the bit on does not make them creatable by itself. |
| `mailCoolTime` | 75 | `true` | Mail send cooldown display (INFERRED). |
| `hudAuctionButton` | 81 | `true` | Auction-house button in the HUD. |
| `auctionPostBuff` | 83 | `true` | Auction posting buff / UI (INFERRED). |
| `aaPoint` | 44 | `false` | AA-point amounts in price and currency displays. AA points are not supported, so keep it off. |
| `ExtraBits` | any | `{}` | `bit -> bool` overrides for the unnamed `flag_x_y` bits (`0..=87`); a `false` value turns a default-on bit off. Bit 78 is the mate equipment whitelist. The C++ reads 32, 34, 41, 42, 47, 60 and 73 too, but their purpose is unresolved. |

The C#-named bits that the client does not expose through `GetFeatureSet()`
(`allowFamilyChanges` 1, `houseSale` 3, `customUiButton` 39, `freeLpRaise` 53,
`houseTaxPrepay` 84) have no named key; they keep their current defaults, and
`ExtraBits` can override them like any other bit. `nations` (6) has a named key
because it follows the era profile (see `World:Profile`).

The player level limit (byte 1) and mate level limit (byte 8) are not part of
this section: they keep coming from the `levels` table and the era profile. The runtime GM command
`/feature set` still changes bits after startup; `FeatureSet.json` is only the
startup baseline. Every key's bit number and the client line that reads it are
recorded in `rust/crates/aaemu-game/PORTING.md` (Client FeatureSet
configuration).

## Chat feature switches (`Configurations/Chat.json`)

Rust-only switches for chat whose retail 1.2 behaviour is not fully evidenced.
They ship in `Configurations/Chat.json` and can be overridden in
`Config.Local.json` or with `Chat__ChatFailedPacket`.

| Key | Default | Meaning |
| --- | --- | --- |
| `ChatFailedPacket` | `false` | Answer channel-bound chat failures with `SCChatFailedPacket(error, channel name)` instead of the `SCErrorMsgPacket` toast: the user-channel join / leave / message errors, and the "not in party / raid / expedition / family" errors. Every other chat failure keeps `SCErrorMsgPacket`. `false` keeps the previous frames. |
| `MinLevelByChatType` | `{}` | Minimum character level per chat type (`"0"` Say, `"1"` Shout, `"-3"` Whisper; a missing entry or `0` means no gate). A sender below the level is refused with error 786 and the message is not routed; characters with `access_level >= 100` are exempt. |

**Capture-pending:** which failures retail 1.2 sent as `SCChatFailed` is
unknown. Keep `ChatFailedPacket` off until a capture of the original server
shows the form it used. The packet layout and the open question are recorded
in `rust/crates/aaemu-game/PORTING.md` (CHAT-FAILED-1). The retail per-chat-type
levels are unknown too, so `MinLevelByChatType` ships empty (CHAT-LEVEL-1).

### Chat mute (`/restrict_chat`)

The retail GM command `/restrict_chat <character> [minutes]` mutes an online
or offline character for the given number of minutes (default 60; `0` lifts
the mute). The retail argument shape is unknown, so the duration and the
default are assumed. The mute is stored in `characters.chat_restricted_until`
(UTC), survives restarts, and expires automatically. A muted sender's message
is not routed and a plain system message tells them they are muted; the retail
mute error is unknown (CHAT-RESTRICT-1).

## Loot announcements (`Configurations/Loot.json`)

Rust-only switch for the party/raid loot chat line. The shipped default keeps
the previous behaviour (no announcement):

| Key | Default | Meaning |
| --- | --- | --- |
| `AnnounceToParty` | `false` | When a character picks up loot (corpse / loot bag, loot-all, or party-distributed loot such as a dice win), send `SCItemAcquisition` (kind 1) with the looted item stacks to the other party/raid members within the loot range (200 m), never to the looter. The 1.2 client decides which items appear in the chat line. |

```json
{
  "Loot": {
    "AnnounceToParty": true
  }
}
```

Override it in `Config.Local.json` or with `Loot__AnnounceToParty`. The kind-1
semantics and the loot sources are INFERRED from the 1.2 client (WP-IT1), not
evidenced; see `rust/crates/aaemu-game/PORTING.md`.

## Quest reward scaling (`Quest:RewardScaling`)

Rust-only switch that makes the paid quest exp / copper match the number the 1.2
client's quest window shows. It is off by default: with `false` the server pays
today's unscaled values.

| Key | Default | Meaning |
| --- | --- | --- |
| `RewardScaling` | `false` | Scale the level-based `quest_supplies` exp and copper by the quest degree (0 / 1 -> x0.5, 2 -> x0.75, 3 -> x1.0, 4 -> x1.25, 5 and above -> x1.5), and exp (a Reward component's `SupplyExp` act or the scaled supply) by the quest-vs-player level gap. The gap pays in full when the quest is at most 5 levels below the player (up to player level 21), then 90 / 80 / 70 / 60 %, and 50 % at 10+ levels below; from player level 22 it never reduces. A `SupplyCopper` act is paid unchanged; copper has no gap factor. The let-it-done ratios are unchanged. |

```json
{
  "Quest": {
    "RewardScaling": true

## Music upload limits (`Music`)

`CSSaveUserMusicNotes` is stored only when it could come from the 1.2 client: the
`song_size` field must equal the song's UTF-16 code-unit length, `item_id` must
be an item the character owns whose template is in `instrument_sounds` (the
shipped table has 199 instruments), and the song must stay within the server
cap. A refused upload sends no packet (the client has no evidenced failure
path); the server logs a warning and keeps the previous queued song.

| Key | Default | Meaning |
| --- | --- | --- |
| `Music.MaxSongChars` | `3000` | Longest accepted `song` string, in UTF-16 code units. The client enforces its own `music_note_limits`; keep this value above the lengths your players compose. |

```json
{
  "Music": {
    "MaxSongChars": 3000
  }
}
```

Override it in `Config.Local.json` or with `Quest__RewardScaling`. The server
decides the paid amount (the quest packets carry no reward figure; the client
formula only computes the displayed number). Whether the original zone server
applied the same factors is **inferred**, not captured: the 1.2 client scales
its window, but the 2014 archeagedatabase.net export lists unscaled supply
values. **Capture-pending (`porting-md#2`):** complete a degree-2 quest and a
quest far below the character's level with the real client, compare the quest
window's reward with the exp / copper actually gained, and only then consider
turning the default on. Details in `rust/crates/aaemu-game/PORTING.md`.

Override it in `Config.Local.json` or with `Music__MaxSongChars`. The note
grammar is not parsed; see `rust/crates/aaemu-game/PORTING.md` (WP-M1).

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
It leaves Peace Fund untouched. Hunting tax, nation splits, and automatic historical
payout schedules are not added. Territory tax changes have the original one-hour cooldown.

## Siege-time shop gate

The two 1.2 merchant packs named "defense-side goods sold during a siege" (224) and
"siege-side goods sold during a siege" (225) can be restricted to the siege itself. The
switch is off by default and keeps the previous behaviour (both packs sell at any time to
anyone):

```json
{
  "Housing": {
    "SiegeShopGate": true
  }
}
```

With the switch on, pack 224 sells only to registered defenders and pack 225 only to
registered attackers, and only during the warmup or siege period (3 / 4) of the NPC's
territory. A refused line is silent like the other `CSBuyItems` catalog refusals; an allowed
line is still charged in honor through the item's honor price.

**Capture-pending (INFERRED):** the restriction and its side rule come from the pack names
only, not from a capture of the retail server. Keep the switch off until an original-client
capture shows how the retail shop behaved. The implementation and the open question are
recorded in `rust/crates/aaemu-game/PORTING.md` (WP-SD4a).

## Siege calendar and territory claim-window timezone

`Housing.SiegeCalendarTimezone` is the wall-clock offset of the `siege_zones` weekday/hour
columns. It defaults to **UTC** (`""` or `utc`); `local` selects the server's own offset and
an explicit `+HH:MM` / `-HH:MM` value overrides both:

```json
{
  "Housing": {
    "SiegeCalendarTimezone": "local"
  }
}
```

The same offset drives two things, even though `Housing.SiegeCalendar` (off by default) only
gates the weekly siege periods:

* the weekly auction / declare / warmup / siege periods when `Housing.SiegeCalendar` is on;
* the authored `open_weekday` / `open_hour` / `open_duration_hours` territory **claim window**,
  always (with or without the switch).

A server that leaves the setting empty therefore runs both on UTC; this is a behaviour change
from earlier releases, where an empty value meant the host's local time. The resolved offset
and whether it came from the configuration or the default are logged once at startup. The UTC
default follows the Western 2014 schedule (grade C in `round6-siege.md`); the original data's
zone is not proven, so use `local` or an explicit offset if your players' clock differs.

## Strict doodad placement surface check

The 1.2 client checks the terrain material under a player doodad placement (its
"place skin" step): the terrain detail material must be one of the four place
kinds the doodad template lists in `doodad_place_skins`. The server can make the
same check from the client files. It is off by default and keeps the previous
placement behaviour:

```json
{
  "Housing": {
    "PlacementStrictSurface": true
  }
}
```

With the switch on, a placement whose terrain material is not allowed by the
template is refused with `DOODAD_SIM_AREA` (276, "Can't be installed here.") and
the seed item is kept. The existing skips stay: templates 1218/1219, group 37,
and a spot inside a house box the player may build on. Worlds without surface
data, cells without a heightmap, terrain holes and unknown surface ids are
allowed (permissive). The check reads `game/worlds/<world>/surface.xml` and the
detail material `.mtl` files lazily; see `rust/crates/aaemu-game/PORTING.md`
(D1d place-skin section) for the evidence, the documented permissive deviation
(`mat_sand_dry` / `mat_mud` count as kind 4) and the memory cost.

## Migration note from old setup docs

Older instructions referenced `aaemu_login.game_servers` and SQL inserts.
That is no longer the source of truth for server listings.

Use login `GameServers` configuration instead.

## Related

- [Installation & Setup](Installation-&-Setup)
- [Local Development Guide](Local-Development-Guide)
- [Mini troubleshoot guide](Mini-troubleshoot-guide)
- [FAQ](FAQ)
