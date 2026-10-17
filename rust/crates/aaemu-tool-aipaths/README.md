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
