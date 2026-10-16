# Backporting 10.x content into the 1.2 client and server

How to take a feature that exists in the **10.x** reference client (10.0.2.13 r575) and make it work
on the **1.2** client (r208022) we serve: game data, assets, UI and server logic, and how the
patched client data reaches players. Machine paths and the RE tool chain are in
[`REVERSING.md`](../../../REVERSING.md); the first worked example is the housing backport in
[`backports/HOUSING.md`](backports/HOUSING.md).

> Status: written from a static audit (2026-10-15). Steps marked **unverified** have not yet
> been exercised against a live 1.2 client. Update this file as each one is proven.

## 1. The five layers of a backport

Every backported feature touches some of these. Audit all five before writing code.

| Layer | 1.2 location | 10.x location | Usual verdict |
|---|---|---|---|
| **Game data** | server `rust/crates/aaemu-game/app/Data/compact.sqlite3` **and** the client's loose `D:\AA1.2\game\db\compact.sqlite3` (identical row for row) | `game/db/compact.sqlite3` inside the 10.x `game_pak` (plain sqlite; extracted copy `D:\aa-re\dwarf-backport-20261014\compact-10.sqlite3`) | Additive merge of new rows; new tables/columns only where the 1.2 client ignores them |
| **Assets** | `D:\AA1.2\game_pak` (24.9 GB, AES-encrypted file table, raw file data) | 10.x `game_pak` (68 GB, same container) | CGF/DDS/MTL are format-compatible so far; copy files into a patched pak |
| **UI** | compiled Lua 5.1 `.alb` under `game/scriptsbin/x2ui/*` in the pak | plain `.lua` under `game/ui/` in the 10.x pak | Cannot be copied; rewrite against the 1.2 Lua API |
| **Native client code** (x2game.dll: Lua API, packet handlers, DB loaders) | Ghidra `ghidra-work/proj-aa12u` | `Bin64\decompiled\x2game-dev_dedicate.c` | Not patchable by data; missing natives mean either a 1.2-only design or an `aadriver` native hook |
| **Protocol + server** | `opcodes.rs`, `systems/<area>/` | `OPCODES_10x.md`, 10.x packet RTTI | Never invent opcodes; reuse 1.2 packets / skills / commands |

The rule that decides most designs: **the 1.2 client only does what its x2game.dll knows.**
New DB tables the 1.2 client has no loader for are server-only knowledge; a 10.x UI that calls
natives 1.2 lacks cannot run. Check the 1.2 binary first (step 2.3) before planning a port.

## 2. Audit (read-only)

Work in a dated folder `D:\aa-re\<feature>-<yyyymmdd>\{data,assets,ui,server}`. Never write into
`D:\AA1.2` or the 10.x folder; never copy the 10.x `game_pak`.

### 2.1 Data closure

1. Find the feature's root rows in the 10.x DB (names via `localized_texts`).
2. Build the **dependency closure**: every table/row the root references, transitively
   (items, skills, skill_effects, doodads + funcs/phases, models, localized texts, quests, …).
   Reusable tool: `D:\aa-re\house-backport-20261015\data\closure.py` (housing-specific roots,
   generic FK walk; copy and change the roots for another feature).
3. **Schema diff** each table in the closure 10.x vs 1.2 (`data\schemadiff.py`): missing
   tables, added/removed/renamed columns, restructured tables.
4. **Collision check** (`data\collisions.py`): ids present in both DBs. 10.x has rewritten
   most shared rows, so the merge must be **additive only** — never overwrite a 1.2 row with
   its 10.x version unless the change is the point of the backport and every dependant is
   checked. Some keys are not stable across versions (e.g. `item_housings.id`,
   `localized_texts.id`): merge those on their natural key.
5. List the **unresolved externals**: referenced items/skills/quests that are neither in the
   closure nor in 1.2. Each is either pulled into the closure or cut.

### 2.2 Asset closure and compatibility

1. Map the closure's model/prefab/icon references to pak paths (pak listings:
   `D:\aa-re\dwarf-backport-20261014\pak-{10,12}.tsv`; extractor
   `D:\aa-re\zone\agent_geom\tools\pakx.py`; housing walker
   `D:\aa-re\house-backport-20261015\assets\house_deps.py` follows prefab → CGF → MTL → DDS).
2. Classify every file: **new** (absent in 1.2), **identical**, or **differing** (exists in
   1.2 with other bytes). Differing files are dangerous: overwriting them changes existing 1.2
   content. Prefer new side-by-side files (e.g. `house_deps.py --subset-libs` writes new prefab
   libraries holding only the used prefabs plus a ref-map for the DB).
3. Compatibility checks (scripts in `D:\aa-re\house-backport-20261015\assets\`):
   - CGF: file/chunk versions (`cgf_compare.py`). Seen so far: 0x744/0x745 in both, same chunk
     types; only `MtlName` v0x802 is 10.x-only.
   - DDS: FourCC (`dds_survey.py`). No DX10/BC6H/BC7 in either pak; DXT1/3/5 + ATI2 only.
   - MTL: shader names and generation-mask permutations vs 1.2 (`mtl_compat.py`,
     `perm_check.py`). Unseen permutations may or may not compile at runtime (**unverified**).
   - Particles/sounds referenced by prefabs vs the 1.2 libraries (`particle_check.py`).

### 2.3 Client code and UI

1. Find the 10.x UI files (`game/ui/...lua`) and list the `X2*:` natives and UI events they use.
2. Search the unpacked 1.2 x2game (Ghidra `proj-aa12u`, strings in `D:\aa-re\client12\`) for
   each native, event, opcode and DB-loader SQL string. Anything missing is a design constraint.
3. Decide the delivery route:
   1. **Server-only** — skills, items, doodad interactions, NPC dialogs or `/` commands the 1.2
      client already supports. Works for every client, including unmodified ones.
   2. **New 1.2 Lua UI** that uses only natives 1.2 has and talks to the server through an
      existing packet (`X2Chat:SendChatMessage("/cmd …")` reaches the server command handler,
      `systems/social/packets/chat.rs`). Good for a first version.
   3. **Client extension layer** in the injected `aadriver`
      ([`CLIENT-EXTENSIONS.md`](CLIENT-EXTENSIONS.md)): our own Lua natives, one custom
      bidirectional extension packet and native hooks into the client. This is the long-term
      foundation for anything that needs real data flow (server-pushed lists, 10.x-style
      natives so 10.x UI logic ports closely) or engine behaviour (previews, redraws). Player
      distributions already ship the driver; unmodified clients do not get these features, so
      the server must detect the extension version and degrade gracefully.

### 2.4 Server

Find the owning subsystem (`AGENTS.md` → "Where to change X"), check what its data loader reads
(loaders `SELECT *` and look columns up by name: extra columns are fine, a missing one is a
startup error, duplicate ids are fatal) and which packets announce the changed objects.

## 3. Patching game data

The server DB and the client's loose DB are the same file today and must stay in lockstep:
the client renders from its own copy (models, prefabs, build steps, items, skills), the server
simulates from its copy, and a row only one side knows causes desyncs or crashes.

- **One merge, two outputs.** Write the merge as a checked-in, idempotent SQL patch
  (`SQL/patches/compact/<yyyy-mm-dd>_<feature>.sql`), applied to both the server
  `compact.sqlite3` and the client's loose `game\db\compact.sqlite3`. `INSERT` only new rows;
  `ALTER TABLE … ADD COLUMN` / `CREATE TABLE` for 10.x structures the server needs (the 1.2
  client ignores unknown tables/columns it never queries — **unverified per table**, test it).
- Nothing applies `SQL/patches/compact/` automatically today (no consumer in Rust, scripts or
  Docker); applying it is manual `sqlite3 compact.sqlite3 < patch.sql`. A small `xtask`
  applier that records applied patches would make this repeatable — build it with the first
  backport that needs it.
- **Client DB location**: the client reads the **loose** `game\db\compact.sqlite3`, not the
  encrypted copy inside `game_pak` (startup pak-read trace: no read touched the pak's DB
  extent). Whether it falls back to the pak copy if the loose file is missing is **unverified**.
- Server-only JSON (`app/Data/housing_bindings*.json`, spawns, …) may also need entries.

## 4. Patching client assets (`game_pak`)

The 1.2 client mounts one `game_pak` and has no loose-file override or extra-pak mechanism for
`game/` assets, so new files must go **inside** the pak.

1. **Work on a copy.** `D:\AA1.2` is read-only. Make a client copy on D: (`bin32`, `game`,
   `game_pak` ≈ 25 GB; check free space first).
2. **Write the files into the copied pak.** The Rust `aapak`
   (`rust/crates/aaemu-commons/src/aapak/`) is a full port of the C# AAPak writer
   (`add_file_from_file`, `replace_file`, `delete_file`, `save_header`). Gaps: no CLI uses it,
   names are not lower-cased on add, and the in-place update is not atomic. A
   `pak add/replace` subcommand (e.g. in `aaemu-pak` or `xtask`) is the missing tool.
   Until then the Python prototype
   `D:\aa-re\house-backport-20261015\assets\pak_tail_patch.py --apply-to` appends new files plus
   a new file table and footer (**unverified** against the live client). Append rather than
   rewrite: it keeps all existing chunks byte-identical, which keeps update packages small.
3. **Distribute** with the existing lossless pipeline ([`Docs/wiki/Client.md`](../../../Docs/wiki/Client.md)
   → "Lossless compressed client packs"):
   ```powershell
   aaemu-pak diff D:\Patched\game_pak D:\ArcheAgeCompressed\game.aacm D:\UpdatePackage
   aaemu-pak apply D:\ArcheAgeCompressed\game.aacm D:\UpdatePackage
   ```
   The receipt binds base and new manifest hashes; `Scripts/build-client-distribution.ps1`
   builds the player ZIP. The client DB is a loose file and ships next to it (a full copy, or a
   patch applied by the same SQL as the server).

## 5. Patching UI

- 1.2 UI is Lua 5.1 bytecode in `.alb` files; the engine's Lua numbers are **32-bit floats**,
  so stock `luac` output does not load. `lua_use_binary` maps `scripts/X.lua` →
  `scriptsbin/X.alb`.
- No central manifest: the loader enumerates every directory under `scriptsbin/x2ui` and loads
  its `toc.g` (Ghidra `FUN_392129b0` / `FUN_39211c40`). A **new directory** with a `toc.g` and
  its files is picked up automatically, so add new UI side by side instead of editing existing
  `.alb` files. Plain-source Lua in an `.alb` probably loads (Lua's loader accepts both) —
  **unverified**. An `addon/<name>/toc.g` route takes plain `.lua` but is gated by a flag whose
  setter is unknown.
- Hook into existing windows from the new module by listening to existing UI events
  (e.g. `HOUSE_INTERACTION_START`) rather than by patching the original files.
- Prototype UI live with the client driver (`aaemu-clientdriver lua "<expr>"`) before packing.
- The 10.x helper library (`W_MODULE`, `W_CTRL`, `F_MONEY`, `F_COLOR`, `F_TEXT`) does not exist in
  1.2; 10.x UI code is a reference for layout and flow only.

## 6. Validation ladder

1. Static: closure has no unresolved externals; schema patch applies to a copy of both DBs;
   server starts with the patched compact (`cargo xtask dev` / `live-stack.sh`).
2. Server tests: data-loader test for the new rows, flow tests for the feature
   (`test_support`, subsystem `tests.rs`).
3. Client data: patched copy boots to the world; place/spawn the new object with a GM command;
   screenshot (`live-stack.sh shot`). Watch for missing-shader/texture fallbacks and crashes.
4. UI: driver-injected Lua first, then the packed module.
5. Distribution: `aaemu-pak diff` → `apply` on a clean compressed install → boot.

Record every new finding (verified or disproven) back into this file.

## 7. Tooling still to build

| Tool | Purpose |
|---|---|
| `pak add/replace/delete` CLI over `aapak` | Write new/changed files into a copied original-format pak (lower-case names, atomic temp-file + rename) |
| `compact` patch applier (`xtask`) | Apply `SQL/patches/compact/*.sql` to server + client DBs, record applied patches |
| Generalised closure tool | `closure.py` with pluggable roots (houses, races, items, quests) checked into `rust/tools/` |
| Asset closure + compat report | `house_deps.py` + checks generalised and checked into `rust/tools/` |
| `.alb` compiler | Lua 5.1 with 32-bit float numbers, if plain source in `.alb` turns out not to load |
| Client extension layer | `aadriver` natives + extension packet + server `ext` module, see [`CLIENT-EXTENSIONS.md`](CLIENT-EXTENSIONS.md) |
