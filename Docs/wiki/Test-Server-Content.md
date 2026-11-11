# Test server content (Marketplace catalogue and GM kits)

This page is for a **private test server**: the goal is to make every system reachable quickly,
not to balance an economy. It ships three things:

- `SQL/examples/example-ics-test.sql` — a generated Marketplace catalogue where every item costs
  **0 or 1 credit**.
- `rust/crates/aaemu-game/app/Scripts/Commands/kits.json` — bulk GM kits (`/kit <name>`).
- `rust/crates/aaemu-game/app/ExampleConfig.TestServer.json` — starting Credits/Loyalty for new
  test accounts.

Do not use these on a public server.

## Marketplace catalogue

The file is generated from the 1.2 `compact.sqlite3`; do not hand-edit it. Import it **after**
`SQL/aaemu_game.sql`:

```sh
mysql -h127.0.0.1 -P3317 -uroot aaemu_game < SQL/examples/example-ics-test.sql
```

When the test catalogue replaces another catalogue (for example
`example-ics-eu-1.2.sql`), clear the three tables first, otherwise both sets stay visible:

```sql
DELETE FROM ics_menu;
DELETE FROM ics_shop_items;
DELETE FROM ics_skus;
```

Then import the file, or run `/ics reload` on a running server instead of restarting it.

`ics_menu` uses the 1.2 client's own tab labels (read from `ui_texts`). The generated groups are:

| Tab | 1.2 label | Contents |
| --- | --- | --- |
| 2 / 2 | Consumables | potions, food, drinks, healing/defense/meditation potions (10 per purchase) |
| 2 / 3 | Luxury | regrade scrolls and charms, Lucid lunagems, lunadrops/lunafrosts |
| 2 / 4 | Character | salon certificate, gender swap, inventory expansion, character slot |
| 2 / 7 | Other | level-bracket gear (one card per bracket, one SKU per grade), instruments, fishing rods, siege scrolls/seals/weapons, a crafting-material card |
| 3 / 2 | Glider | all usable gliders |
| 3 / 3 | Mount | all mounts (horses, donkeys, snowlions, elk, leomorphs, yatas, submarines) |
| 3 / 4 | (vehicles) | all ships and carts/tractors/farm wagons |
| 4 / 2 | Tax/Sales | tax certificates, land expansion, storage chests, appraisal certificate |
| 4 / 3 | Decor | representative furniture/decor (one per furniture type) |
| 4 / 7 | Misc. | all house designs, all farms and scarecrow farms, castle/defence designs |
| 5 / 2 | Consume | all dyes |
| 5 / 3 | Costume | representative costumes |
| 6 / 2 | Items | loyalty tokens (single and packs), Teleport Book, Memory Ink |
| 6 / 5 | Pets | all battle pets and a pet-gear set per tier |

Every card is also listed in the main tab's **All** sub-tab (for example 3 / 1), like the other
example catalogues. The 1.2 item names are used; a card with several grades shows its item ids in
the detail pane.

### Selection rules

The generator (`rust/tools/test_catalogue/gen.py`) reads `compact.sqlite3` read-only and only
selects rows that exist in its 1.2 `items` table. A name in `localized_texts` whose id is not in
`items` is a leftover of another version and is never used. It also skips:

- items without an English name;
- names matching test/sample/demo/dev/unused/prototype (including the Korean `테스트` / `견본`);
- quest, test, trash and internal categories (`item_categories` ids 35, 44, 64, 71, 96, 98-105,
  107, 110, 111, 123, 134-136) and `impl_id = 10` (quest starters);
- mounts/pets without an `item_summon_mates` row, ships/vehicles without an
  `item_summon_slaves` row (and a slave name that itself passes the test-name filter);
- furniture without an `item_housing_decorations` row, lunagems without an `item_sockets` row,
  lunadrops without an `item_enchanting_gems` row.

Bulk categories keep a deterministic representative selection instead of hundreds of shop pages
(furniture, costumes, instruments, consumables). The full bulk is reachable through the GM kits
below.

### Regenerating

```sh
python -I rust/tools/test_catalogue/gen.py \
    --compact D:/aaemu-live/rs-game/Data/compact.sqlite3
```

The default output is `SQL/examples/example-ics-test.sql`; the script prints the shop/SKU/menu
counts and the counts per tab. `--kits-out <file>` writes the generated `itemkits` JSON as well.
The SQL and the kit list are committed, so a normal build does not run Python.

## GM kits

The test kits are at the end of `rust/crates/aaemu-game/app/Scripts/Commands/kits.json`; they are
loaded at server start and can be re-read with `/kit` (the command list help) and `/kit ?`.

| Kit | Contents |
| --- | --- |
| `vocation_alchemy` … `vocation_carpentry` | the eight most used materials of each vocation craft (100 each), from `crafts`/`craft_materials` |
| `vocation_fishing`, `vocation_mining` | the 1.2 craft table has no recipes for these two, so the kit carries bait/hooks/lines and ores/stone |
| `tradepack_materials` | the fifteen most used trade-pack materials (100 each) |
| `seeds` | crop seeds, tree saplings and livestock (20 each) |
| `regrade_test` | weapon/armor/accessory regrade scrolls, charms, blank scroll and items to regrade |
| `lunagem_set` | lunagems, lunadrops and the Insightful/Vital lunafrosts used by the live tests |
| `gear_10` … `gear_50` | a cloth, leather and plate set plus five weapons for the level bracket, at heroic (grade 5) |
| `tax_certificates` | tax certificate, bound tax certificate and the 10-pack |

Use `/kit <name>` (or `/kit <player> <name>`), for example `/kit gear_30`. A kit adds every entry
to the default bag; the largest kit is 44 entries, so a fresh character has enough slots.

## Starting Credits/Loyalty for test accounts

The game creates the `accounts` row on an account's **first login** and copies
`Credits.Default` and `Loyalty.Default` into it. Both default to `0`; the test-server override is:

```json
{
    "Credits": { "Default": 1000000 },
    "Loyalty": { "Default": 5000 }
}
```

Copy `rust/crates/aaemu-game/app/ExampleConfig.TestServer.json` over the game server's
`Config.Local.json` (or merge those two sections into it). Because this only runs on the insert,
an account that already exists keeps its current values; `UPDATE accounts SET credits = 1000000,
loyalty = 5000 WHERE account_id = <id>` is the operator way to top one up. The value is granted
once per account, not once per character.

## Live check

With the lane stack running and the catalogue loaded (`/ics reload` after importing it), a
headless client can buy and receive an item:

```sh
AAEMU_SCRIPT=shop.txt aaemu-testclient run --config testclient.local.json \
    --login 127.0.0.1:<p>237 --scenario script
```

```text
buyics 2400001 0 4 7
gm kit gear_30
```

`buyics` checks that the purchase succeeds and that a CHARGED Marketplace mail arrives; `/kit`
prints the item task result in the server log.
