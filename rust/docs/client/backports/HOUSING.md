# Housing backport: Thatched Farmhouse upgrades, then all 10.x houses

First feature run through the process in [`../BACKPORTING.md`](../BACKPORTING.md). Audit date
2026-10-15. Evidence, extracted files and scripts: `D:\aa-re\house-backport-20261015\`
(`data\`, `assets\`, `ui\`; each has a `FINDINGS.md`). Nothing below has been tried on a live
client yet.

## What 10.x has that 1.2 lacks

**The Thatched Farmhouse itself is already in 1.2**: housing 171, design item 21165, model 530,
same prefab GUIDs in `game/prefabs/housing_big.xml`. What is missing is the 10.x **rebuild**
("upgrade") system and the tier-2 farms it leads to.

| | Design (placeable) | `_complete` | `_rebuild` (upgrade target) | Rebuild pack |
|---|---|---|---|---|
| Wilderness Farm | 402 | 417 | 432 | 88 |
| Prairie Farm | 403 | 418 | 433 | 89 |
| Lode Farm (the "miner's house") | 404 | 419 | 434 | 90 |

- Thatched (171, and 10.x-only as-built 330) has `housing_rebuilding_pack_id = 2` → pack 2 lists
  rebuildings 3/4/5 → targets 432/433/434, skill **28829** "Extend building performance".
  Materials per upgrade (rebuilding 5 → 434): item 34983 ×15, 47083 ×6, 37896 ×60, 37893 ×30,
  37894 ×30; labor 0.
- Sidegrades between the tier-2 farms (packs 88/89/90 → rebuildings 33/34/35) use skill
  **28828** "Change building": 34983 ×15, 37896 ×45, 37893 ×23, 37894 ×23.
- Each `_rebuild` target has one build step (step 0, model 1085, skill 29291 "Completed",
  consumes item 8329) — i.e. the rebuilt house appears as a construction stage the owner
  finishes. *(Inferred from the rows; 1085 is also the final step model of design 402.)*
- Tier-2 stats: taxation 12 (250,000; 1.2 has taxation ids 1–11), housing size 5 (garden
  radius 11.0, view 24 m), hp 1500, category 18, deco limit 80.
- Data chain: `housings.housing_rebuilding_pack_id` → `housing_rebuilding_packs` →
  `housing_rebuilding_pack_rebuildings` (position) → `housing_rebuildings`
  (`skill_id`, target `housing_id`, `labor_power`, `actability_group_id`) →
  `housing_rebuilding_materials` (`item_id`, `count`). 10.x totals: 223 rebuildings, 183 packs,
  1486 material rows.

### 10.x upgrade flow (reference for our UI)

Maintain window → **Rebuild** button (`maintain_window_view.lua:435`, shown when
`X2Player:GetFeatureSet().rebuildHouse`; enabled when the house has a rebuild pack, is not for
sale, not in a siege period, not tax-overdue) → `rebuild_housing.lua` window, which sends
`CSRebuildHouseTaxInfo` (10.x CS 0x1AB, u16 house id) and fills on `SCRebuildHouseTaxInfo`
(10.x SC 0x2BA) → list of targets with materials, tax comparison and a 3D "look-ahead" preview →
OK → `StartRebuild(type)` casts the rebuild skill with a 10.x-only skill object (type 7:
rebuilding id, position, yaw). Extracted files: `ui\x10\game\ui\...`.

## What 1.2 can and cannot do

| Layer | Verdict |
|---|---|
| Client DB | Loose `game\db\compact.sqlite3` = server compact, row for row. No `housing_rebuilding*`, `housing_sizes`, `housing_view_sizes` tables; `housings` lacks 21 10.x columns (`housing_size_id` replaced `garden_radius`); `housing_binding_doodads` keyed by `owner_id`+`owner_type` instead of `housing_id` |
| Client code | **No rebuild support at all** in 1.2 x2game: no strings, loaders, natives, events or opcodes. Special-effect type 122 (`RebuildHousing`) is above the 1.2 maximum (118). Skill object type 7 means `ItemGradeEnchantingSupport` in 1.2. |
| Client UI | No rebuild UI; the 1.2 maintain window has no function-button module; `GetHouseType`/`GetHouseId` natives missing |
| Assets | Compatible as far as static checks go (CGF 0x744/0x745, DXT/ATI2 only, all shaders present). Tier-2 farms use the new prefab group `addon_housing_farm` and `addonfarm_*.cgf` meshes (models 1755–1757) |
| Server | Loads housing templates by column name; no rebuild loader; `RebuildHousing` special effect only logs (`skills/effects/special/mod.rs:145`); no code changes a live house's template |

Consequence: the tier-2 farms can be added as **data + assets**, but the **upgrade must be driven
by our server** and its **UI must be a new 1.2 Lua module**.

## Design

1. **Data**: additive merge of the thatched-family closure (`data\closure.json`, produced by
   `closure.py --housings 171,330`): 11 housings, ~3,230 rows + ~2,107 localized texts, 14 new
   bound doodads (all 9xxx + 10641). Keep 1.2's version of every shared row (housing 171,
   doodads 2392/5539 which are `cgf://` in 1.2 but `prefab://` in 10.x, build skills, item 16473).
   Map `housing_size_id` back onto 1.2's `garden_radius`; translate `housing_binding_doodads`
   to `owner_id`/`owner_type='Housing'`; key `item_housings` on `(item_id, design_id)` and
   localized texts on `(tbl, col, idx)`. Add the four `housing_rebuilding*` tables to the
   **server** compact only (the 1.2 client has no loader for them). Add taxation 12. Add
   positions for the new bound doodads to `app/Data/housing_bindings*.json` (from the prefabs).
   Skip the referenced daily jar quests (7057–7060, 7071–7072) unless wanted.
2. **Assets**: `house_deps.py --housing-id 171 330 402 403 404 417 418 419 432 433 434 --extract
   … --subset-libs _x10`: 30 new files (2.7 MB) plus new prefab libraries holding only the used
   prefabs. **Do not overwrite `housing_big.xml`** (10.x changes 33 existing prefabs, 10 of them
   need CGFs 1.2 lacks); point the new rows at the subset library via the ref-map. Two
   referenced effects are missing in 1.2 (`X2_EFFECTS_P.doodad.housing_watermill_water`,
   `x2_sounds.housing.farm_floor`) — check whether they fail silently.
3. **Server rebuild** (`systems/housing/`):
   - `data.rs`: load the rebuild tables into the housing data (pack → ordered targets with
     skill, labor, materials).
   - `ops.rs`: `rebuild(house, rebuilding_id)` — owner-only; not for sale, not tax-overdue,
     not in a siege; consume materials and labor; mutate the **same** `House` entity
     (keeps id, obj id, owner, position, permissions, dates, furniture and chest doodads):
     recompute `template_id`, `all_action`, `build_step_count`, `tax`, `max_hp`/`hp`,
     `model_id`, default name; remove old-template bound doodads explicitly
     (`reconcile_bound_doodads` matches on `(doodad_id, attach_point)`), set step 0; re-run
     `collision::objects::sync_house`, `fort::refresh_for_house`, `update_tax_info`; then
     `world.hide` + `world.show` the house (a template change has no in-place packet; whether a
     second `SCHouseState` alone redraws is untested) and send `SCMyHouse` to the owner. Mail
     type `HOUSING_REBUILD = 30` and achievement `REBUILD_HOUSING = 68` already exist.
   - Finishing the `_rebuild` construction step reuses the existing build-step path
     (`ops::craft_building`).
   - Tests on the `tests.rs` fixture pattern; `load_rows_and_persistent_bound_doodads`
     (`tests.rs:1275`) is the model for a template swap on a loaded house.
4. **Trigger**: a GM command first (`systems/services/commands/`, next to `test_house`), then a
   player `/` command with full validation (`/rebuild <houseId> <rebuildingId>`), which is
   what the UI sends via `X2Chat:SendChatMessage`. The 10.x skill-cast path is not usable from
   1.2 (no skill object type, no effect 122).
5. **UI**: new module `scriptsbin/x2ui/<house_rebuild>/` with its own `toc.g`; on
   `HOUSE_INTERACTION_START` add a Rebuild button to the maintain window; the window lists
   targets, materials and tax (static tables generated from the DB at build time), confirm →
   `/rebuild`. Prototype through the client driver first.
6. **With the client extension layer** ([`../CLIENT-EXTENSIONS.md`](../CLIENT-EXTENSIONS.md)):
   replace the static tables and `/rebuild` with extension messages that mirror the 10.x pair
   (`RebuildHouseTaxInfo` request/reply, `StartRebuild`), and expose natives named like the
   10.x ones (`X2House:GetHousingRebuildingPackInfo`, `…BaseInfo`, `…MaterialInfo`,
   `…TaxInfo`, `CanRebuild`, `StartRebuild`, plus `GetHouseId`/`GetHouseType`) so the 10.x
   `rebuild_housing.lua` logic ports with layout changes only. The 3D look-ahead preview is a
   later native hook. Clients without the extension keep the `/rebuild` path.

## Tasks (in order)

| # | Task | Proves |
|---|---|---|
| 0 | Tooling from `BACKPORTING.md` §7: pak add/replace CLI, compact patch applier | repeatable patching |
| 1 | Data patch for the thatched family → server + client copy; server boots | data merge rules |
| 2 | Assets into a copied pak; GM-place design 404 (Lode Farm) directly and screenshot | asset compatibility end to end (cheapest proof, no upgrade code) |
| 3 | Server rebuild op + rebuild data loader + GM command + tests | in-place template swap and client redraw |
| 4 | Player `/rebuild` with validation, materials, labor, mail/achievement | gameplay |
| 5 | 1.2 Lua rebuild window | UI delivery route |
| 6 | `aaemu-pak diff` update package + loose DB on a clean compressed install | player distribution |
| 7 | Client extension layer (natives, extension packet, version handshake) and the 10.x-style rebuild natives/window on top of it | real data flow, closer 10.x port |
| 8 | All houses, in batches by housing group | scale |

## Scale of "all houses"

568 of 837 10.x housings are absent from 1.2 (all 269 1.2 housings exist in 10.x). Closure:
679 housings, ~26,700 rows, ~11,800 localized texts, 354 doodads (59 already in 1.2); unresolved
externals 158 items, 238 skills, 74 quests, 52 craft packs (`data\closure_all.json`,
`rebuild_graph_all.json`). Assets: 3,353 files / 949 MB, of which 1,726 new (625 MB) and 248
differing (44 MB); 87 of 1,291 shader permutations unseen in 1.2. Batch by housing group so each
batch can be proven in the client before the next; decide per batch whether to pull in its
quests/skills or cut them.

## Open questions

- Does the 1.2 client accept a tool-written (appended) pak file table?
- Does it compile the unseen shader permutations, and what happens with the two missing effects?
- Does it ignore unknown tables/columns in its loose DB (expected: it only runs its own queries)?
- Does it fall back to the pak's encrypted DB when the loose one is missing?
- Does a hide/show cycle redraw a house whose template changed, or is a relog needed?
- Does plain-source Lua in an `.alb` load, or is a 32-bit-float `luac` needed?
- Should 10.x `zh_cn` texts (machine English) seed missing `en_us` names?
