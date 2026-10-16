# Backport tooling

Generators and the pipeline for taking 10.x content into the 1.2 client and server. Process and
background: [`rust/docs/client/BACKPORTING.md`](../../docs/client/BACKPORTING.md); first worked
example: [`backports/HOUSING.md`](../../docs/client/backports/HOUSING.md).

Layers:

| Piece | Where | Role |
|---|---|---|
| `aaemu-clientpatch` | `rust/crates/aaemu-tool-clientpatch` | `pak list/info/extract/add/restore`, `db apply/check`, `bundle build/apply` |
| bundles | `rust/backports/<name>/` | committed text only: `bundle.json`, `assets.txt`, `prefab_subsets.tsv`, `sql/NNN_*.sql`, optional `server/*.json` |
| generators | `rust/tools/backport/*.py` | read the 10.x DB/pak read-only and write a bundle |

No client asset is checked in. `bundle build` extracts the files listed in `assets.txt` from the
10.x `game_pak` (file by file, never a copy of the pak) and generates the subset prefab libraries
into a staging directory.

## Generators (Python 3.12, standard library only, run with `-I`)

* `bflib.py` - read-only DB access, the `Plan` (rows absent from 1.2, translated to the 1.2
  schema) and guarded SQL emission.
* `assetlib.py` - asset closure: prefab library -> CGF/CGA/CDF/CHR/CAL -> MTL -> DDS, classified
  `new` / `same` / `differs` against the 1.2 pak by content (the MD5 in original paks is often 0).
  Pak listings come from `aaemu-clientpatch pak list`; pass `--pak10-list/--pak12-list` to reuse a
  saved listing.
* `housing_thatched.py` - housing roots (`--housings`, default the Thatched Farmhouse family):
  housings, models and prefabs, build steps, bound doodads with their func/phase closure, design
  items, skills and effects, localized texts, rebuild packs, assets. Writes
  `001_*.sql` (shared: client and server DB), `002_*_server.sql` (server only: rebuild tables,
  `housings.housing_rebuilding_pack_id`), `assets.txt`, `prefab_subsets.tsv`, `bundle.json`.
  Prints a report: rows per table, dropped rows with reasons, unresolved references, asset
  classification.
* `housing_bindings.py` - bound-doodad positions for `app/Data/housing_bindings*.json` from the
  `$driver`, `$cannonN`, `$heal_pointN`, `$name_plate01` helper nodes of the house CGFs
  (`--check` validates the method against the existing 1.2 entry of housing 171: within 2 cm).

## Merge rules the generators follow

* Additive only. A row whose key 1.2 already has is kept as 1.2 has it; every `INSERT` is an
  anti-join on the key, so re-running a patch cannot duplicate rows (compact.sqlite3 tables have
  no constraints). `db apply` additionally records patches by name and sha256.
* 10.x columns 1.2 lacks are dropped; 1.2 columns 10.x lacks get 0 / `'f'` / `''` when 1.2 never
  stores NULL there (the server loaders read them as non-optional). Item prices come from the
  10.x `item_prices` table.
* Ids that are not stable across versions (`item_housings`, `localized_texts`,
  `housing_binding_doodads`, `prefab_elements`) get fresh ids (`max(id) + n`) when the patch runs and
  are matched on their natural key.
* Prefab references of new models/doodads point to generated *subset* libraries
  (`housing_big_x10ht.xml`, ...) holding only the used prefabs, so 1.2's libraries are not touched.
* Doodad funcs whose type 1.2 does not know, quest funcs, and effects needing buffs / craft packs
  that 1.2 lacks are dropped (the report lists them); skills, items and doodads they reference are
  pulled in when missing.
* SQL directive `-- aaemu:add-column <table> <column> <decl>`: adds the column only if absent
  (SQLite has no `ADD COLUMN IF NOT EXISTS`).

## Typical run

```powershell
$py  = 'c:\python312\python.exe'
$cp  = 'cargo run -q -p aaemu-tool-clientpatch --'      # or the built aaemu-clientpatch.exe
$p10 = 'D:\ArcheAge Returns 10.0.2.13 - 8yx - r575 - 2026-06-18\game_pak'

# regenerate the bundle from the 10.x data (read-only)
& $py -I rust\tools\backport\housing_thatched.py --db10 D:\aa-re\dwarf-backport-20261014\compact-10.sqlite3 `
  --db12 D:\AA1.2\game\db\compact.sqlite3 --pak10 $p10 --pak12 D:\AA1.2\game_pak `
  --tool <path to aaemu-clientpatch.exe> --out rust\backports\housing-thatched

# stage the assets, then patch a COPY of the client and the server database
$cp bundle build rust\backports\housing-thatched --donor-pak $p10 --out D:\staging\housing-thatched
$cp bundle apply rust\backports\housing-thatched --staging D:\staging\housing-thatched `
    --client D:\aaemu-testclient\AA1.2-ext --server-db D:\staging\compact.patched.sqlite3

# undo the pak change (restores the original file table, truncates to the original length)
$cp pak restore D:\aaemu-testclient\AA1.2-ext\game_pak
```

`pak add` never modifies existing file data: new data is appended where the old file table began
and a new table is written behind it. The first change saves `<pak>.aaemu-restore` (original table
+ length + a fingerprint of the data region). The client must be closed while the pak is written.
The write is not atomic; if it is interrupted run `pak restore`.
