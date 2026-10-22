# aaemu-aipaths

Generates `Data/Path/<name>.path` AI route files for the game server from the 10.x client.

The retail routes are not in the 1.2 client. The 10.x `game_pak` keeps the designers' AI paths in
`game/worlds/<world>/level_design/zone/<zone key>/editor/npc.xml` (`<aiPath>` entities). The tool
reads the route names the compact database asks for (`quest_components.ai_path_name` and the
FollowPath `ai_commands`), converts the matching paths to world coordinates (`|x|y|z|` lines, zone
origin from the 1.2 `world.xml`) and writes them to the output folder.

```sh
cd rust
cargo run --release -p aaemu-tool-aipaths -- \
  --pak10 "<10.x>/game_pak" --pak12 "<1.2>/game_pak" \
  --compact crates/aaemu-game/app/Data/compact.sqlite3 \
  --out crates/aaemu-game/app/Data/Path
```

* An existing `<name>.path` is never overwritten (the hand recorded routes stay); the tool prints
  how far each existing file is from the generated route instead.
* A name used by several zones (`public_farm`, `tuto_move`, ...) is written as
  `<name>@<zone key>.path` variants; the server picks the variant nearest to the NPC.
* The summary lists the names that were not converted (missing in 10.x, zone origin differs
  between 1.2 and 10.x, spawner-relative paths).
* `--dry-run` writes nothing, `--skip-rotated` leaves out the paths with a `Rotate`,
  `--rotation direct|inverse` selects the quaternion direction (direct is verified against the 1.2
  terrain).
* Output is deterministic: sorted names, three decimals, LF line ends.

Tests: `cargo test -p aaemu-tool-aipaths`; with the paks,
`AAEMU_PAK10=... AAEMU_GAME_PAK=... AAEMU_COMPACT_DB=... cargo test -p aaemu-tool-aipaths real_data -- --ignored`.
The ground check of the committed routes is
`AAEMU_GAME_PAK=... cargo test -p aaemu-game real_ai_generated_paths -- --ignored`.

## `return-points`: worldgates.json entries

The `Return` special effect (type 25) with a non-zero `value1` teleports to the world gate of
`Data/Portal/worldgates.json` with that `Id` (a `return_points.id`). The coordinates are not in
the database or the 1.2 client. The 10.x `game_pak` keeps them in
`game/worlds/main_world/level_design/zone/<zone key>/editor/return_point.xml`: spheres named
`ReturnPoint_<return_points.editor_name>` with a zone-local `Pos` and a `Rotate` quaternion.

```sh
cd rust
cargo run --release -p aaemu-tool-aipaths -- return-points \
  --pak10 "<10.x>/game_pak" --pak12 "<1.2>/game_pak" \
  --compact crates/aaemu-game/app/Data/compact.sqlite3 \
  --worldgates crates/aaemu-game/app/Data/Portal/worldgates.json \
  [--world <instance_world>]... [--worlds-dir <Data/Worlds>] [--skip <id,id,...>] [--update]
```

* A gate is generated for every `return_points` row that a `Return` effect uses, that is not in
  the hand curated part of `worldgates.json`, and that has exactly one position in the 10.x
  main_world data. World X/Y = zone origin cell (`world.xml`) * 1024 + `Pos`; Z is `Pos.z`; Yaw is
  the heading of `Rotate` in degrees; positions are rounded to 0.1 m.
* A zone is used only when the 1.2 and 10.x `world.xml` agree on its name and origin cell and the
  compact `zones.zone_key` knows it (like the route converter). The `ZoneId` is that zone key.
* `SubZoneId` is the gate `Id` (the server only uses it as the dictionary key; ids are unique).
  If a hand curated gate already has that number as its sub zone, `100000 + Id` is used.
* Without `--update` the generated section goes to stdout; with `--update` it is written into
  `worldgates.json` between the two marker comments (replaced when present, appended before the
  closing `]` otherwise). The hand curated entries are never touched and decide which ids count as
  existing, so a rerun gives the same section. Entries are sorted by `Id`, one decimal, the file's
  line ends.
* stderr lists the ids that were left out and why (no main_world point in the 10.x data, zone
  differs, several positions) and the hand curated gates more than 5 m from the 10.x point.
* `--skip` leaves ids out on purpose; the ids are recorded in the section's header comment. The
  committed file was generated with `--skip 399,400,533,627,681`, the gates the ground check
  rejects (not on the 1.2 terrain or navmesh).
* `--world <name>` (repeatable) also reads an instance world: `game/worlds/<name>/world.xml` of
  both paks (instance zones have their own origin cells) and the 10.x
  `game/worlds/<name>/level_design/zone/<key>/editor/return_point.xml` files. Only ids that the
  main_world data does not know are looked up there, by the 1.2 `return_points.editor_name`
  (never by id: ids 68/69 and 670/671 mean other points in 3.0/10.x). The zone must have the same
  id and origin cell in the 1.2 and 10.x `world.xml` of that world, and the name must be unique
  in the given worlds. The terrain check does not apply to instance worlds; instead the horizontal
  distance to the nearest 1.2 spawn (`npc_spawns.json` / `doodad_spawns.json` of
  `--worlds-dir`/`<world>`, default `Data/Worlds` next to the `Portal` folder) is printed as a
  warning when it exceeds 40 m. The section header records the worlds, and a rerun needs the same
  `--world` list (the section is regenerated as a whole). The committed file was generated with
  `--world instance_library --world instance_nachashgar --world instance_nachashgar_easy
  --world instance_howling_abyss --world instance_howling_abyss_2
  --world instance_cradle_of_destruction`, which adds 67 gates (Nachashgar 524-532, 546-553,
  561, 625, 626; Nachashgar easy 572-581; library 630-647, 654-669, 679; Howling Abyss 484, 525;
  Cradle of Destruction 488). Immortal Isle (555, 567: 134 m below the nearest spawn) is not read.

The ground check of the committed gates is
`AAEMU_GAME_PAK=... cargo test -p aaemu-game real_worldgates -- --ignored`.
