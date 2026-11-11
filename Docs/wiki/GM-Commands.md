# GM Commands

- Audience: Testers and server administrators
- Last verified against: `main` on November 11, 2026 (audit suite `rust/verify/gm-commands.vtest`)
- Target: ArcheAge 1.2 (`r208022`)

Every command is a chat line that starts with `/`. Type `/help` to list the commands your
access level allows, `/help <command>` for one command, or `/<command> help` (for commands
with subcommands such as `/item add help`). Arguments in `<...>` are required, `[...]` are
optional, and `a|b` means "one of". Wrap text that contains spaces in double quotes:
`/mail admin Bob "Welcome back" "Your gear is in the attachment" 250`. Many commands accept
`self`, a `target` you selected, or a character name; commands that only take a name say so.

## Becoming a GM (access levels)

Commands check your account's access level:

| Level | Typical use | Examples |
| --- | --- | --- |
| 0 | everyone | `/help`, `/dungeon status`, `/online`, `/soloparty`, `/joinsiege`, `/leavesiege`, `/collectdominion` |
| 50 | moderator | `/fly`, `/teleport`, `/heal`, `/kill`, `/kick`, `/spawn`, `/despawn`, `/announce` |
| 100 | administrator | everything else (`/item`, `/gold`, `/level`, `/world`, `/dominion`, `/mail`, `/scripts`, …) |

On a fresh server the first account/character gets level 100
(`Account.AccessLevelFirstAccount` / `Account.AccessLevelFirstCharacter` in
`Configurations/CharacterSettings.json`). For test servers you can also set
`Account.AccessLevelDefault` to `100` there (it applies to characters created afterwards) and
restart the game server, or grant it once in the game database (never on a shared/production
database). The account must have logged in to the game once so that its `accounts` row exists;
login names live in the login database:

```sql
UPDATE aaemu_game.accounts SET access_level = 100
 WHERE account_id = (SELECT id FROM aaemu_login.users WHERE username = '<login name>');
```

Commands not listed in `Configurations/AccessLevels.json` default to level 100, so a missing
entry makes a command admin-only rather than public.

## Items and money

| Command | What it does | Example |
| --- | --- | --- |
| `/item add <player name\|target\|self> <template id> [amount=1] [grade=0]` | Adds an item (amount 1-1000, grade 0-11). It is silent on success; open the bag to check. | `/item add self 23633 10` (10 Gilda Stars) |
| `/item expire <item id> [minutes=0.5]` | Makes a bag item expire in `minutes` (or in 30 seconds). | `/item expire 16777226 60` |
| `/item unwrap <item id> [minutes=-1]` | Removes the unwrap delay, or sets it to `minutes`. | `/item unwrap 16777226` |
| `/gold <add\|set\|remove> <player name\|target\|self> <gold> [silver] [copper]` | Changes money. `set` currently adds like `add` (original behaviour); `remove` subtracts the given amount. | `/gold add self 10 50 0` |
| `/labor (target) <amount> [vocationSkillId]` | Adds (or with a negative amount removes) labor. | `/labor 1000` |
| `/xp (target) <xp>` | Adds experience. | `/xp 15000` |
| `/vocation (target) <points>` | Adds vocation (badge) points. | `/vocation 100` |
| `/kit (target) <kitname>` | Adds all items of a kit. `/kit` shows how many kits exist, `/kit ?` lists them. | `/kit consumables` |
| `/premium <account id\|character name\|self> <on\|off\|days> [days]` | Grants premium until now + days (bare `on` = 30 days) or removes it. | `/premium self on 7` |

Item ids for `/item expire` and `/item unwrap` are the ids in the inventory tooltip / bag slot,
not the item template id. The template id is the number in the item database
(`compact.sqlite3`).

## Level and skills

| Command | What it does | Example |
| --- | --- | --- |
| `/level (target) <level>` | Sets a character level by adding the needed experience (2-50; pets 2-50). | `/level 30` |
| `/buff [[View]\|[AsTarget] <buffId>] <ab_level>` | Applies a buff to your target (a negative id removes it). `View` lists the target's buffs. | `/buff 1000` |
| `/ignoreskillcds <true\|false>` | Turns skill cooldowns on/off for yourself. | `/ignoreskillcds true` |
| `/resetcd` | Clears your skill cooldowns. | `/resetcd` |
| `/failskill <skill result> [ushort] [uint]` | Forces a skill failure with the given result code (packet testing). | `/failskill 1` |
| `/useskill [target\|area] <skillId>` | Forces a skill cast (target or area form). | `/useskill 11285` |
| `/godmode <true\|false>` | Makes you immortal to other players. | `/godmode true` |
| `/invisible <true\|false>` | Hides you from other players. | `/invisible true` |

## Movement and teleport

| Command | What it does | Example |
| --- | --- | --- |
| `/teleport [location]` | Teleports to a named location; with no argument it lists the names; `.` uses your map marker. | `/teleport solis` |
| `/move (target) [<x> <y> <z>\|ToMe]` | Moves you or a player to coordinates (or to you). | `/move 15578 15382 126` |
| `/moveall` | Moves every online player to your position. | `/moveall` |
| `/moveto <rec\|save file\|go file\|back file\|stop\|run\|walk>` | Records/replays a walking route. | `/moveto rec` |
| `/nudge (distance)` | Steps forward by the given distance (default 5 m). | `/nudge 10` |
| `/fly (target) [true\|false]` | Turns fly mode on/off. | `/fly true` |
| `/run (target)` | Toggles the run buff. | `/run` |
| `/position (player)` | Prints position, rotation and zone. | `/position` |
| `/height (target)` | Prints your/target's height and the floor height. | `/height` |
| `/distance [objId]` | Prints raw and model-adjusted distance to the target. | `/distance` |
| `/pingpos` | Shows your party's map marker (use `/soloparty` first). | `/soloparty` then `/pingpos` |
| `/setpos (ObjID) <x> <y> <z>` | Moves any object; use `*` (or `x`, `y`, `z`) to keep a value. | `/setpos 45145 * * 130` |
| `/setrot (ObjID) <x> <y> <z>` | Sets an object's rotation in degrees; `*` keeps a value. | `/setrot 45145 0 0 90` |
| `/rotate [angle]` | Turns the target unit towards you (the angle argument is ignored, as before). | `/rotate` |

`ObjID` for `/setpos` and `/setrot` is the object id from `/position`, `/npc info` or
`/findobject`, not a template id.

## NPCs and doodads

| Command | What it does | Example |
| --- | --- | --- |
| `/spawn <npc\|doodad\|remove> <templateId> [rotationZ]` | Spawns an NPC or doodad 3 m in front of you, or removes a doodad by object id. | `/spawn npc 3597` |
| `/npc spawn <templateId> [yaw=degrees]` | Spawns an NPC in front of you. | `/npc spawn 3597 yaw=90` |
| `/npc info <target\|id> [object id]` | Shows template, id, position and rotation. | `/npc info id 45145` |
| `/npc pos <target\|id> [object id] [x= y= z= roll= pitch= yaw=]` | Moves/rotates an NPC. | `/npc pos id 45145 x=15570 y=15370` |
| `/npc save <all\|id> [object id]` | Writes NPC positions to the world spawn file. | `/npc save all` |
| `/npc remove <target\|id> [object id]` | Removes an NPC. | `/npc remove id 45145` |
| `/doodad spawn <templateId> [yaw=degrees]` | Spawns a doodad in front of you. | `/doodad spawn 4252` |
| `/doodad chain <templateId>` | Prints the doodad's phase chain (details in the log). | `/doodad chain 4252` |
| `/doodad phase list <objId>` | Lists the available phases. | `/doodad phase list 8780` |
| `/doodad phase change <objId> <phaseId>` | Changes a doodad phase. | `/doodad phase change 8780 2` |
| `/doodad pos <objId> [x= y= z= roll= pitch= yaw=]` | Moves/rotates a doodad. | `/doodad pos 8780 x=15570` |
| `/doodad age <objId> <seconds>` | Sets a doodad's age (1 h = 3600). | `/doodad age 8780 3600` |
| `/doodad save <all\|id> [objId]` | Writes doodad state to the world file. | `/doodad save id 8780` |
| `/doodad remove <all\|id> [objId]` | Removes doodads. | `/doodad remove id 8780` |
| `/doodad removes <radius>` | Removes the nearest doodads in a radius. | `/doodad removes 20` |
| `/slave spawn <templateId> [yaw=degrees]` | Spawns a vehicle/pet (slave). | `/slave spawn 10` |
| `/slave info <target\|id> [object id]`, `/slave pos …`, `/slave save …`, `/slave remove …` | Same pattern as NPCs. | `/slave info id 45145` |
| `/gimmick spawn <templateId>` | Spawns a gimmick (moving object). | `/gimmick spawn 10` |
| `/spawngrid <npc\|doodad> <templateId> <columns> <rows> <spacing>` | Spawns a grid of NPCs/doodads. | `/spawngrid npc 3597 3 3 5` |
| `/despawn <npc\|doodad> <objId\|templateId> [radius]` | Despawns one object, or every match of a template inside a radius. | `/despawn npc 3597 50` |
| `/despawnall` | De-spawns every world object. Use last; the world repopulates on restart. | `/despawnall` |
| `/around <doodad\|npc\|player\|all> [radius] [verbose]` | Lists objects around you (default 30 m). | `/around npc 50` |
| `/findobject <doodad\|npc\|gimmick> <templateId> [max]` | Lists the nearest objects of a template. | `/findobject npc 3597` |
| `/stackednpcs [<radius>\|all]` / `/stackednpcs fix [<radius>\|all]` | Finds (or separates) NPCs standing on each other. | `/stackednpcs 100` |
| `/tickdoodad <objId>` | Moves a doodad to its next phase. | `/tickdoodad 8780` |
| `/nwrite (target) [all\|<doodad\|npc> <objId>]` | Writes positions/rotations to JSON. | `/nwrite all` |
| `/doodad_location <objId> <x> <y> <z>` | Moves a doodad; `*` keeps a coordinate. | `/doodad_location 8780 15570 * *` |
| `/testnavmesh` | Shows the navmesh route to the target. | `/testnavmesh` |

Template ids come from the game database (`compact.sqlite3`); object ids change every spawn and
are printed by the spawn/info/find commands.

## World state: time, zones, events, sieges

| Command | What it does | Example |
| --- | --- | --- |
| `/time set <hour> [minute]` | Sets the world time. Values outside 0-23 are accepted (original behaviour); use 0-23. | `/time set 12 30` |
| `/world set exprate\|lootrate\|growthrate\|honorrate\|vocationrate <value>` | Sets a server rate. | `/world set exprate 2` |
| `/world set motd\|logoutmessage <text>` | Sets the MOTD / logout message. | `/world set motd Welcome` |
| `/world set geodatamode\|autosaveinterval <value>` | GeoData mode / autosave interval. | `/world set autosaveinterval 5` |
| `/zonestate <State> [zoneId]` | Sets a zone state (0 Tension … 7 Peace). Only zones with conflict data in the database can change. | `/zonestate 5` |
| `/feature check` / `/feature set <id> <true\|false>` | Reads/sets account feature flags. | `/feature set 1 true` |
| `/fishfinder set <start>` | Fish-finder debug toggle. | `/fishfinder set start` |
| `/towerdef list` / `/towerdef start <id>` / `/towerdef end <id>` / `/towerdef next <id> <wave>` | Tower-defense events. | `/towerdef start 14` |
| `/dungeon status` / `/dungeon reset <zoneId>` | Shows dungeon bindings / resets an empty dungeon you own. | `/dungeon reset 1` |
| `/snow <true\|false>` | Toggles the snow effect. | `/snow true` |
| `/waterdebug info` / `/waterdebug reload` | Water-zone debug. | `/waterdebug info` |
| `/shipbarrier status` / `/shipbarrier reset [world\|cell] [ingest]` | Ship-barrier debug. | `/shipbarrier status` |
| `/reloadconfig` | Reloads the configuration files. | `/reloadconfig` |
| `/reloadauction` | Reloads the auction manager. | `/reloadauction` |
| `/scripts reload force` | Reloads the command scripts. | `/scripts reload force` |

### Sieges and dominions

| Command | What it does | Example |
| --- | --- | --- |
| `/dominion info` | Lists territories (owner, tower, tax, siege state). | `/dominion info` |
| `/dominion clock <unixSeconds\|off>` | Sets a test clock for the siege calendar, or restores real time. | `/dominion clock off` |
| `/dominion schedule <zoneGroupId> <attackerExpeditionId> <delaySeconds>` | Schedules a siege. | `/dominion schedule 5 12 3600` |
| `/dominion register <zoneGroupId> <character> attacker\|defender` | Registers a participant. | `/dominion register 5 Bob attacker` |
| `/dominion unregister <zoneGroupId> <character>` | Removes a participant. | `/dominion unregister 5 Bob` |
| `/dominion headquarters <zoneGroupId> attacker\|defender <doodadObjId>` | Binds a headquarters doodad. | `/dominion headquarters 5 attacker 8780` |
| `/dominion cancel <zoneGroupId>` | Cancels a scheduled/running siege. | `/dominion cancel 5` |
| `/dominion collect <zoneGroupId>` | Collects property tax as the owning guild leader. | `/dominion collect 5` |
| `/schedule_siege <zoneGroupId> <year> <month> <day> <hour> <minute>` | Schedules the accepted declaration at a wall-clock time. | `/schedule_siege 5 2026 12 1 20 0` |
| `/stop_siege <zoneGroupId>` | Stops a scheduled/running siege. | `/stop_siege 5` |
| `/delete_dominion <character> <zoneGroupId>` | Deletes a territory record. | `/delete_dominion Bob 5` |
| `/change_dominion_owner <zoneGroupId> <expeditionId>` | Changes the owner to an existing guild. | `/change_dominion_owner 5 12` |
| `/bestow_dominion <zoneGroupId> <expeditionId>` | Bestows a territory on a guild. | `/bestow_dominion 5 12` |
| `/set_dominion_non_pvp <zoneGroupId> [minutes]` | Starts the peace window (default 120 min). | `/set_dominion_non_pvp 5` |
| `/set_dominion_limit <limit>` | Sets the claim limit for new territories. | `/set_dominion_limit 3` |
| `/joinsiege [zoneGroupId]` / `/leavesiege [zoneGroupId]` | Joins/leaves your guild's scheduled siege. | `/joinsiege` |
| `/collectdominion <zoneGroupId>` | Collects property tax (guild leader). | `/collectdominion 5` |
| `/auction_post_siege_items [duration] [startPrice] [buyoutPrice]` | Posts the six Siege Camp system lots. | `/auction_post_siege_items` |
| `/auction_cancel <lotId>` | Cancels an unbid system auction lot. | `/auction_cancel 42` |

What a tester meets during a running siege: a roster entry (`/joinsiege`, `/dominion register`)
hands out the side's Guardian or Marching Seal at the next one-second sweep, not instantly;
inside the siege area the two sides are hostile to each other even within one nation; and a
non-participant inside the area is moved to the nearest return point outside it with "Not a siege
participant". Characters with access level 100 are exempt from that eviction, and
`Housing.SiegeEvictNonParticipants: false` turns it off. The castle zones are `closed` in the 1.2
data, so only GMs can stand in a castle area at the moment.

`zoneGroupId` is the zone group of the territory (`/dominion info` and `/zonestate` help identify
it). A guild must exist before owner/bestow/register commands work.

### Nations, crime and factions

| Command | What it does | Example |
| --- | --- | --- |
| `/nation info` | Lists player nations. | `/nation info` |
| `/nation create [force]` | Declares a nation from your guild (needs a dominion). | `/nation create` |
| `/nation hostile <faction>` | Declares hostility. | `/nation hostile 101` |
| `/nation relation <faction> <state> <minutes> <next>` | Sets a relation. | `/nation relation 101 1 60 2` |
| `/nation treaty <faction>` | Makes peace. | `/nation treaty 101` |
| `/nation rename [name]` | Renames your nation. | `/nation rename Aurelia` |
| `/nation tax <zone> <rate>` | Sets nation tax. | `/nation tax 5 10` |
| `/nation monument place <zone> [x y z]` / `monument remove <zone>` | Places/removes the monument. | `/nation monument place 5` |
| `/nation dominion <zone>` | Shows the zone's dominion. | `/nation dominion 5` |
| `/crime points <player\|target\|self> [crime] [infamy] [jury]` | Changes justice points. | `/crime points Bob crime=10` |
| `/crime ask_guilty <player\|target\|self> <crime=N> <time=N>` | Starts a guilty plea. | `/crime ask_guilty Bob crime=10 time=5` |
| `/crime jury_invite <player\|target\|self> [defendant=] [trial=]` | Summons a juror. | `/crime jury_invite Bob` |
| `/crime court <player\|target\|self> <none\|list\|jail> [court=] [trial=]` | Court actions. | `/crime court Bob list court=1` |
| `/crime trial_state <player\|target\|self> <trial= trialId> <state= stateId> <jury= N> <time= N>` | Sets a trial state. | `/crime trial_state Bob trial=1 state=1 jury=3 time=5` |
| `/crime create <templateId> <owner= playerId> <victim= playerId> [source= doodadTemplateId]` | Spawns crime evidence (bloodstain 877/878, footprints 3313/3314). | `/crime create 877 owner=1 victim=2` |
| `/crime fake <player> <player> <evidence= N>` | Runs a fake trial. | `/crime fake Bob Victim evidence=1` |
| `/setfaction <faction name\|id>` | Sets the selected unit's faction. | `/setfaction 101` |
| `/set_crime_value <character> <value>` | Sets crime points. | `/set_crime_value Bob 50` |
| `/clear_crime_status <character>` | Clears crime and infamy. | `/clear_crime_status Bob` |
| `/set_bounty_money <character> <amount>` | Places a bounty (copper). | `/set_bounty_money Bob 10000` |
| `/set_jury <character> <trialId>` | Seats a juror. | `/set_jury Bob 1` |
| `/imprison <character>` | Moves an online player to their court's holding cell. | `/imprison Bob` |
| `/restrict_chat <character> [minutes]` | Mutes (0 lifts the mute). | `/restrict_chat Bob 60` |

## Housing and mail

| Command | What it does | Example |
| --- | --- | --- |
| `/build [stepcount]` | Advances the targeted house construction. | `/build 10` |
| `/house info` / `/house taxmail` | Shows house info / creates a tax mail for the targeted house. | `/house info` |
| `/house_binding_move <attachPointId> <x> <y> <z>` | Moves a house binding point (testing). | `/house_binding_move 1 0 0 0` |
| `/delay_tax_due_date <character> [days]` | Postpones the tax due date (default 7 days). | `/delay_tax_due_date Bob 14` |
| `/resend_house_tax_mail <character>` | Sends a new unpaid tax bill. | `/resend_house_tax_mail Bob` |
| `/coffer <view\|close> [objId]` | Views/force-closes a nearby coffer (4 m; object id optional). | `/coffer view` |
| `/mail admin <receiver> <title> <text> [money] [[itemId count] ...]` | Sends a system (Admin) mail, online or offline. `money` is copper; up to 10 item stacks then follow as id/count pairs. | `/mail admin Bob "Welcome" "Take this" 250 23633 5` |

`/mail admin` needs the title and body as separate arguments; quote each one that contains
spaces. Without trailing numbers it sends only the letter.

## Accounts, characters and chat

| Command | What it does | Example |
| --- | --- | --- |
| `/online [filter]` | Lists online players (optional name filter, `*` shows up to 10). | `/online *` |
| `/kick_player <name\|id> <reason> <message>` | Kicks a player. | `/kick_player Bob 1 "GM kick"` |
| `/rename <old> <new>` | Renames an online or offline character (online character names are normalised). | `/rename Bob Robert` |
| `/allowguildrename <guild id\|name>` | Lets a guild leader rename the guild once. | `/allowguildrename 12` |
| `/renameguild <guild id> <new name>` | Renames a guild directly. | `/renameguild 12 NewName` |
| `/announce <type> <color> <visibleMs> <message>` | Server-wide notice (type 1-3; colour as RGB/ARGB hex or a name). | `/announce 3 FF00FF00 5000 Server restart in 10 minutes` |
| `/title <titleId>` | Grants an appellation. | `/title 42` |
| `/echo <text>` | Repeats text in chat (command test). | `/echo hello` |
| `/inventory (target) [containerId] [fix]` | Shows a container: 1 equipment, 2 inventory, 3 bank, 4 trade, 5 mail. With an NPC target it shows the NPC's equipment. | `/inventory 2` |
| `/disconnectme` | Drops your connection immediately. The character's world entry stays until the game server restarts (original behaviour), so prefer a normal logout. | `/disconnectme` |
| `/scripts <reload\|reboot\|save\|shutdown>` | Script/server actions; `/scripts reload force` reloads all commands. | `/scripts reload force` |

## Combat and test hooks

| Command | What it does | Example |
| --- | --- | --- |
| `/heal (target)` | Heals you or the target to full HP/MP. | `/heal` |
| `/kill (target)` | Kills the selected unit (NPC or player). | `/kill` |
| `/revive (target)` | Revives a dead target. | `/revive` |
| `/damage <damage> [%]` | Damages you or your target (raw or % of max HP). | `/damage 50 %` |
| `/clearcombat` | Sends the clear-combat packet (does not clear the server flag). | `/clearcombat` |
| `/getattribute [target] <attrId\|attrName\|all\|used>` | Prints unit attributes. | `/getattribute all` |
| `/failskill`, `/useskill` | See [Level and skills](#level-and-skills). | `/useskill 11285` |
| `/testai <action> [options]` | AI debug: `list`, `info`, `set_behavior`, `load_path`, `queue_skill`, `follow_npc`, `clear`. Needs a target. | `/testai list` |
| `/testcombat <engaged\|cleared\|first_hit\|text>` | Combat packet tests. | `/testcombat cleared` |
| `/testchatchannel <list [id]\|clean\|join\|leave> …` | Chat-channel packet tests. | `/testchatchannel list` |
| `/testfsets` | Prints the active feature sets. | `/testfsets` |
| `/testguild [rename]` | Guild dialog tests. | `/testguild` |
| `/testheightvisualizer (target) [testpos\|mark\|line]` | Heightmap test; `testpos` moves you to a test position. | `/testheightvisualizer` |
| `/testmail <action\|type> …` | Mail tests (`list`, `clear`, or a dummy mail). | `/testmail list` |
| `/testslave` | Spawns the test slave. | `/testslave` |
| `/testtracker (target) [objId]` | Toggles movement debug. | `/testtracker` |
| `/testtransfer` | Disabled (access level 999). | – |
| `/stackednpcs`, `/tickdoodad`, `/waterdebug`, `/shipbarrier` | See [NPCs and doodads](#npcs-and-doodads) and [World state](#world-state-time-zones-events-sieges). | – |

## Command behaviour every tester should know

- **Silent success is normal.** Many add/remove commands (`/item add`, `/labor`, `/xp`,
  `/vocation`, `/buff`, `/damage`, `/kill`, `/resetcd`, `/snow`, `/reloadauction`, …) send no
  chat reply; check the bag, stats, buff bar or server log instead. Error messages are shown in
  red.
- **Most invalid input answers with red text** (unknown templates, missing characters,
  out-of-range levels, item amounts and grades); an unknown command does nothing. A few
  commands stay silent on an invalid number, as in the original: `/labor`, `/xp`, `/vocation`,
  `/nudge`, `/damage`, `/quest add` and `/useskill` ignore the call or treat the number as 0.
- **Object ids are not template ids.** `/position`, `/npc info`, `/findobject` and every spawn
  reply print the object id. `/item expire` needs an item id from the inventory.
- **Two quoted arguments are fine.** `/mail admin Bob "Title" "Body" 100` and other quoted
  forms work (the parser used to insert a space argument between them).
- **`/despawnall` is world-wide.** Run it last; NPCs/doodads come back on the next server start.
- **`/time set` does not clamp.** Use 0-23 for hours (outside values are stored as given).
- **`/rotate` ignores its angle argument** and turns the target towards you (original
  behaviour).
- **`/kit` on yourself reports how many items it added.**

## Rerunning the audit

The tester audit lives in `rust/verify/gm-commands.vtest`. On a lane stack:

```sh
Scripts/verify.sh --only gm-commands
```

It logs in with a GM bot, runs every command with a normal and an invalid call, prints the
replies and the server log carries the error lines. It is an audit for a human to read, not an
assertion suite (a wedged server shows up as empty replies), and it changes server-global state
(exprate, motd, time, snow, the cash shop, zone states, a nation, dominions and sieges, and at the
end `despawnall`) and takes several minutes. That is why `Scripts/verify.sh` skips it in the
default run: select it with `--only gm-commands` on a throw-away lane stack. See [Live
Verification Suites](Verification) for the lane setup.

This guide does not cover every registered command yet: `/quest`, `/ics` (`/ingamecashshop`),
`/sphere`, `/register_portal`, `/packet` and `/settradepackmaildelay` have no entry; `/help` in
game lists everything your access level may run.

## Related

- [Live Verification Suites](Verification)
- [Working with the Config.json files and server listings](Working-with-the-Config.json-files-and-server-listings)
- [Code Terminology](Code-Terminology)
- [Home](Home)
