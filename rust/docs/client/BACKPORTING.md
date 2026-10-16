# Backporting 10.x content into the 1.2 client and server

How to take a feature that exists in the **10.x** reference client (10.0.2.13 r575) and make it work
on the **1.2** client (r208022) we serve: game data, assets, UI and server logic, and how the
patched client data reaches players. Machine paths and the RE tool chain are in
[`REVERSING.md`](../../../REVERSING.md); the first worked example is the housing backport in
[`backports/HOUSING.md`](backports/HOUSING.md).

> Status (2026-10-16): the pipeline below was exercised end to end with the housing backport on
> a real 1.2 client (patched pak + loose DB, server data, extension UI, 3D preview, rebuild).
> Steps still marked **unverified** have not been exercised yet. Record new findings here.

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

## 3. Backport bundles

Every backport is a **bundle**: committed text in `rust/backports/<name>/` plus a generator in
`rust/tools/backport/`, applied by `aaemu-clientpatch` (`rust/crates/aaemu-tool-clientpatch`).
No client asset is ever committed; assets are extracted from a donor 10.x `game_pak` at build time.

```
rust/backports/<name>/
  bundle.json          name, version, description, ordered sql list (optionally per target), assets
  assets.txt           donor pak paths to extract (only files that are new or must differ)
  prefab_subsets.tsv   generated subset prefab libraries (new files; 1.2 libraries stay untouched)
  sql/NNN_*.sql        ordered patches; "targets": ["server"] for server-only tables
  server/*.json        server-only data fragments (e.g. housing_bindings), copied next to app/Data
```

```powershell
# 1. (re)generate the bundle from the 10.x data (deterministic; see rust/tools/backport/README.md)
c:\python312\python.exe -I rust\tools\backport\housing_thatched.py --db10 <10.x compact> --db12 <1.2 loose compact> `
    --pak10 <10.x game_pak> --pak12 <1.2 game_pak> --tool <aaemu-clientpatch.exe> --out rust\backports\housing-thatched
# 2. stage the assets from the donor pak
aaemu-clientpatch bundle build rust\backports\housing-thatched --donor-pak <10.x game_pak> --out <staging>
# 3. apply (client closed, server stopped): the server compact, and a client copy (pak + loose DB)
aaemu-clientpatch bundle apply rust\backports\housing-thatched --staging <staging> --server-db <compact.sqlite3>
aaemu-clientpatch bundle apply rust\backports\housing-thatched --staging <staging> --client <client root>
#    rollback of the pak: aaemu-clientpatch pak restore <client root>\game_pak
```

Rules:

- **One merge, two outputs.** The server compact and the client's loose DB must stay in lockstep:
  the client renders from its copy (models, prefabs, build steps, items, icons, texts), the server
  simulates from its copy. Shared rows go to both; tables only the server reads (e.g.
  `housing_rebuilding*`) are marked `"targets": ["server"]`.
- **Additive only** (merge rules in `rust/tools/backport/README.md`): never overwrite a 1.2 row;
  translate 10.x columns to the 1.2 schema; fresh ids for unstable keys, matched on natural keys.
- **Applied patches are immutable.** `db apply` records each patch by name and SHA-256 in
  `aaemu_applied_patches` and refuses a changed file under the same name. Fixes are new numbered
  files (`003_...`), like `SQL/updates/`.
- **Client DB = the loose `game\db\compact.sqlite3`** (verified: backported item names and icons
  appear in the client after patching only the loose DB). The encrypted copy inside `game_pak` is
  not read. Whether the client falls back to it when the loose file is missing is unverified.
- Server-only JSON fragments go next to the server's `app/Data` files under a distinct name
  (the housing loader merges every `housing_bindings*.json`).

## 4. Patching client assets (`game_pak`) and distributing

The 1.2 client mounts one `game_pak` and has no loose-file override or extra-pak mechanism for
`game/` assets, so new files go **inside** the pak.

1. **Work on a copy.** `D:\AA1.2` is read-only. Make a client copy on D: (about 25 GB).
2. **`aaemu-clientpatch pak add`** (used by `bundle apply`) appends the new files and writes a new
   file table + footer at the end; existing data is never moved, so all original chunks stay
   byte-identical and update packages stay small. It saves `<pak>.aaemu-restore` first;
   `pak restore` truncates back exactly. Re-adding identical files is a no-op. **Verified**: the
   1.2 client loads the appended file table, the new CGF/MTL/DDS files and the generated subset
   prefab libraries (the backported Lode Farm renders correctly).
3. **Do not overwrite shared 1.2 files** with their 10.x version (e.g. `housing_big.xml` changes
   existing prefabs); reference new side-by-side files instead.
4. **Distribute** with the existing lossless pipeline ([`Docs/wiki/Client.md`](../../../Docs/wiki/Client.md)
   -> "Lossless compressed client packs"):
   ```powershell
   aaemu-pak diff D:\Patched\game_pak D:\ArcheAgeCompressed\game.aacm D:\UpdatePackage
   aaemu-pak apply D:\ArcheAgeCompressed\game.aacm D:\UpdatePackage
   ```
   The receipt binds base and new manifest hashes; `Scripts/build-client-distribution.ps1`
   builds the player ZIP. The loose client DB ships next to it (patched with the same bundle).
   Not yet exercised end to end for a backport: **unverified**.

## 5. Patching UI

**Preferred: an extension module** (`native/ext/<feature>.lua`, embedded in the driver, see
[`CLIENT-EXTENSIONS.md`](CLIENT-EXTENSIONS.md)). It needs no pak change, is re-applied whenever
the client rebuilds its UI state, can hook stock windows by wrapping their global functions
(the rebuild button wraps the maintain window's `UpdateMyHouseDefaultInfo`), and talks to the
server over its own channels. The notes below apply only when UI must be packed into the pak.

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

1. Static: the generator report has no unresolved externals you did not decide to cut; the bundle
   applies to copies of both DBs; `db check` / `PRAGMA integrity_check`; the server starts with
   the patched compact and logs the new data (e.g. `Loaded 6 housing rebuildings in 4 packs`).
2. Server tests: loader tests (feature tables missing -> empty, present -> parsed) and flow tests
   (`test_support`, subsystem `tests.rs`). Real-data tests pinned to vanilla counts change on a
   patched DB; run them against the vanilla compact.
3. Client data on a **test client copy** against an **isolated test stack** (own runtime dir,
   ports and MySQL; never the shared stack): boot, enter the world, create the object with a GM
   command (`/house spawn <housing id>` creates a finished house of any template in front of you)
   and screenshot it. Watch for missing-shader/texture fallbacks and crashes.
4. Driving the client: `aaemu-launcher --hidden --multi-client --driver-dll ...` renders
   off-screen; `aaemu-clientdriver lua --pid N -t ui "<code>"` runs Lua (GM commands:
   `X2Chat:SendChatMessage(CHAT_SAY,'/cmd ...','')`), `aaemu-clientdriver screenshot --pid N`
   saves the back buffer, and the MCP tools `client_entities` (positions of named entities),
   `client_pointer` (clicks, in screenshot pixels) and `client_input` (keys) cover the rest.
5. UI: prototype in the live client through the driver, then embed (extension module) or pack.
6. Distribution: `aaemu-pak diff` -> `apply` on a clean compressed install -> boot.

Record every new finding (verified or disproven) back into this file.

## 7. Tooling status

| Tool | State |
|---|---|
| `aaemu-clientpatch` (`pak list/extract/add/restore`, `db apply/check`, `bundle build/apply`) | done |
| Bundle generator for houses (`rust/tools/backport/housing_thatched.py --housings ...`) | done; generalise the roots for other content (races, items, quests) |
| Client extension layer (driver natives + extension packet + server `systems::ext`) | done, see [`CLIENT-EXTENSIONS.md`](CLIENT-EXTENSIONS.md) |
| `/house spawn` GM command | done |
| Checked-in isolated test stack (path- and port-parameterised `live-stack`) | todo: `Scripts/live-stack.sh` matches processes by name machine-wide, so it cannot run next to another stack |
| `.alb` compiler (Lua 5.1, 32-bit float numbers) | only if a backport needs packed UI instead of an extension module |
