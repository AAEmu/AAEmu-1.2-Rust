# Test server content (Marketplace catalogue and GM kits)

- Audience: Operators and testers of a private test server
- Last verified against: `main` on November 11, 2026
- Prerequisites: a running game server and database ([Installation & Setup](Installation-&-Setup))

This page is for a **private test server**: the goal is to make every system reachable quickly,
not to balance an economy. It ships three things:

- `SQL/examples/example-ics-test.sql` — a generated Marketplace catalogue where every item costs
  **0 or 1 credit**. It is only loaded by hand; no installer, compose file or script imports it.
- `rust/crates/aaemu-game/app/Scripts/Commands/kits.json` — bulk GM kits (`/kit <name>`). Unlike
  the other two files this one is **shipped data that every server loads**: `/kit` (access
  level 50) lists and hands out these kits on any server. Remove the test kits from your own
  `kits.json` if a production server should not offer them.
- `rust/crates/aaemu-game/app/ExampleConfig.TestServer.json` — an example of the existing
  `Credits.Default` / `Loyalty.Default` settings (starting Credits/Loyalty for new accounts). The
  game ignores this file unless you merge its sections into your own configuration.

Do not use the catalogue or the credit example on a public server.

## Marketplace catalogue

The file is generated from the 1.2 `compact.sqlite3`; do not hand-edit it. Import it **after**
`SQL/aaemu_game.sql`, with your own MySQL host, port and user:

```sh
mysql -h<host> -P<port> -u<user> -p aaemu_game < SQL/examples/example-ics-test.sql
```

When the test catalogue replaces another catalogue (for example
`example-ics-eu-1.2.sql`), clear the three tables first, otherwise both sets stay visible:

```sql
DELETE FROM ics_menu;
DELETE FROM ics_shop_items;
DELETE FROM ics_skus;
```

Then import the file, or run `/ics reload` on a running server instead of restarting it.

`ics_menu` uses the 1.2 client's own tab labels (read from `ui_texts`; sub-tab 3 / 4 has no label
there). The generated groups are:

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
    --compact <path to the 1.2 compact.sqlite3>
```

The default output is `SQL/examples/example-ics-test.sql`; the script prints the shop/SKU/menu
counts and the counts per tab. `--kits-out <file>` writes the generated `itemkits` JSON as well
(`kits.generated.json`, without comments). The SQL and the kit list are committed, so a normal
build does not run Python; the committed kit block is that generated list plus the item-name
comments and `// --- kit ---` markers that were added to it by hand, so merge a regenerated list
into `kits.json` instead of replacing the block blindly.

## GM kits

The test kits are at the end of `rust/crates/aaemu-game/app/Scripts/Commands/kits.json`. The file
is read when the command scripts are (re)loaded: at server start, or with `/scripts reload`.
`/kit` only prints how many kits exist and `/kit ?` lists their names; neither re-reads the file.

| Kit | Contents |
| --- | --- |
| `vocation_alchemy` … `vocation_carpentry` | the eight most used materials of each vocation craft (100 each), from `crafts`/`craft_materials` |
| `vocation_fishing`, `vocation_mining` | the 1.2 craft table has no recipes for these two, so the kit carries bait/hooks/lines and ores/stone |
| `tradepack_materials` | the fifteen most used trade-pack materials (100 each) |
| `seeds` | the sixteen crop seeds (20 each) |
| `saplings` | the sixteen tree saplings (20 each) |
| `livestock` | young animals and eggs: 3 of each animal that does not stack, 10 of each that stacks to 10 |
| `regrade_test` | weapon/armor/accessory regrade scrolls, charms, blank scroll and items to regrade |
| `lunagem_set` | lunagems, lunadrops and the Insightful/Vital lunafrosts used by the live tests |
| `gear_10` … `gear_50` | a cloth, leather and plate set plus five weapons for the level bracket, at heroic (grade 5) |
| `tax_certificates` | tax certificate, bound tax certificate and the 10-pack |

Use `/kit <name>` (or `/kit <player> <name>`), for example `/kit gear_30`. A kit adds every entry
to the default bag, entry by entry and all-or-nothing: an entry that does not fit the free slots
fails with a red "Item could not be created!" and the command goes on with the next one (there
is no mail fallback). The kits are sized for an empty 50-slot bag: each needs at most 26 slots
(`gear_10` … `gear_50` and `livestock` are the largest), so a fresh character can take any one
kit; empty the bag between kits.

## Starting Credits/Loyalty for test accounts

The game creates the `accounts` row on an account's **first login** and copies
`Credits.Default` and `Loyalty.Default` into it. Both default to `0`; the test-server override is:

```json
{
    "Credits": { "Default": 1000000 },
    "Loyalty": { "Default": 5000 }
}
```

Merge the two sections of `rust/crates/aaemu-game/app/ExampleConfig.TestServer.json` into the
game server's `Config.Local.json`; do not copy the file over it, because your `Config.Local.json`
also holds your connections, client data sources and login network settings. The settings
themselves already exist in the game (the shipped values are `0`). Because this only runs on the insert,
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

## Related

- [Home](Home)
- [Installation & Setup](Installation-&-Setup)
- [Working with the Config.json files and server listings](Working-with-the-Config.json-files-and-server-listings)
