"""Generate the TEST Marketplace catalogue (`SQL/examples/example-ics-test.sql`) and the bulk
GM kits (`rust/tools/test_catalogue/kits.generated.json`) from the 1.2 `compact.sqlite3`.

The catalogue is for a private test server only: every SKU costs 0 or 1 credit, so a tester can
buy anything the server can deliver and check that it works. It is generated, never hand-edited:

    python -I rust/tools/test_catalogue/gen.py
    python -I rust/tools/test_catalogue/gen.py --compact D:/aaemu-live/rs-game/Data/compact.sqlite3
    python -I rust/tools/test_catalogue/gen.py --kits-out rust/tools/test_catalogue/kits.generated.json

The SQL keeps the format of `SQL/examples/example-ics-eu-1.2.sql` (explicit ids, `REPLACE INTO`,
re-runnable). It uses its own id blocks (shop 2400001+, SKU 1400001+, menu 4001+) so it can be
imported together with the other example files; the docs tell the operator to clear the three
`ics_*` tables first when the catalogue replaces another one.

Selection (the queries are below, each category function names its source tables):

* only rows of the 1.2 `items` table; a `localized_texts` item name whose id is not in `items`
  is a leftover of another version and is never selected;
* `localized_texts.en_us` must be non-empty;
* names matching TEST_NAME_RE (test/sample/demo/dev/unused, Korean 테스트/견본) are skipped;
* junk/quest categories (`JUNK_CATEGORIES`) and `impl_id == ACCEPT_QUEST` are skipped;
* mounts/pets need an `item_summon_mates` row, ships/vehicles an `item_summon_slaves` row whose
  slave name also passes TEST_NAME_RE;
* furniture needs an `item_housing_decorations` row, lunagems an `item_sockets` row, lunadrops
  an `item_enchanting_gems` row.

Bulk categories (furniture 500+, costumes 300+, instruments 250+, consumables 250+) keep a
deterministic representative selection in the shop; the crafted-material bulk goes into the GM
kits where a tester can get 100 of a material at once. See
`Docs/wiki/Test-Server-Content.md` for the tab mapping and how to load the file.
"""

import argparse
import json
import os
import pathlib
import re
import sqlite3
import sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))
DEFAULT_COMPACT = os.path.join(
    ROOT, "rust", "crates", "aaemu-game", "app", "Data", "compact.sqlite3"
)
DEFAULT_OUT = os.path.join(ROOT, "SQL", "examples", "example-ics-test.sql")
DEFAULT_KITS_OUT = os.path.join(
    ROOT, "rust", "tools", "test_catalogue", "kits.generated.json"
)

SHOP_BASE = 2400001
SKU_BASE = 1400001
MENU_BASE = 4001

TEST_NAME_RE = re.compile(
    r"(?i)\b(test|testing|test-drive|sample|demo|dev|debug|gm|unused|deprecated|prototype)\b"
    r"|테스트|견본"
)

# Item categories that only hold quest, test, trash or internal rows. Ids are the 1.2
# `item_categories` ids: 35 Trash: Miscellaneous, 64 Quest, 71 Quest Equip: Weapon,
# 96 NPC Packs, 98 Trash: Craft, 99 Junk, 100 Test Item, 101-105 Trash: *,
# 107 Adventure, 110/111 Quest Equip: Armor/Accessory, 123 Body Pack,
# 134-136 Drop Equip (internal), 44 Evidence.
JUNK_CATEGORIES = {
    35, 44, 64, 71, 96, 98, 99, 100, 101, 102, 103, 104, 105, 107, 110, 111, 123,
    134, 135, 136,
}

IMPL_SLAVE_EQUIPMENT = 28
IMPL_ACCEPT_QUEST = 10
IMPL_HOUSING = 5

WEAPON_CATEGORIES = [69, 70, 72, 73, 74, 75, 76, 77, 78, 79, 127, 128, 129, 130, 131, 132]
ARMOR_CATEGORIES = [83, 84, 85]
ACCESSORY_CATEGORIES = [86, 87, 88, 121, 125]
EQUIP_PRIORITY = WEAPON_CATEGORIES + ARMOR_CATEGORIES + ACCESSORY_CATEGORIES

# `item_summon_mates`/`npcs.mate_kind_id`: 1-10 are mounts (horses, donkeys, snowlions, elk,
# leomorphs, yatas, the Carrot Dash donkey), 18 the Steamfish submarine mounts, 20 Black Arrow.
# 11-17 and 19 are battle pets (wolfhound, captured monsters, tree/kitsu/plushie pets, cats,
# cubs, bears, boars).
MOUNT_MATE_KINDS = {1, 2, 3, 5, 7, 9, 10, 18, 20}
PET_MATE_KINDS = {11, 12, 13, 14, 15, 16, 17, 19}

# `actability_groups`: 1-18 are the vocation crafts; 19 is Quest, 21-30 languages,
# 31 Commerce, 32 Anniversary, 33 Art, 34 Exploration. Only the crafts get a material kit.
VOCATIONS = {
    1: "alchemy",
    2: "construction",
    3: "cooking",
    4: "handicraft",
    5: "husbandry",
    6: "farming",
    7: "fishing",
    8: "logging",
    9: "gathering",
    10: "machining",
    11: "metalwork",
    12: "printing",
    13: "mining",
    14: "masonry",
    15: "tailoring",
    16: "leatherwork",
    17: "weaponry",
    18: "carpentry",
}

REGRADE_SCROLLS = [28296, 28297, 28298, 28299, 28305, 30804, 31927, 31928]
REGRADE_CHARMS = [31974, 31975, 31976, 31977, 31980, 31981, 34323, 34563]

FURNITURE_KEYWORDS = [
    "bed", "table", "chair", "sofa", "couch", "lamp", "carpet", "rug", "vase",
    "flowerpot", "bonsai", "bookshelf", "bookcase", "cabinet", "chest", "crate",
    "piano", "harp", "fireplace", "painting", "mirror", "statue", "fountain", "bench",
    "stove", "anvil", "loom", "workbench", "wardrobe", "desk", "screen", "curtain",
    "clock", "bathtub", "telescope", "globe", "barrel", "dresser", "chandelier",
]

COSTUME_KEYWORDS = [
    "robes", "robe", "plate", "coat", "uniform", "attire", "swimset", "dress", "gown",
    "costume", "coveralls", "top", "bottoms", "shoes", "cap", "hat", "hood", "cloak",
    "shirt", "pants", "gloves", "boots", "slippers", "frock", "jerkin", "tunic", "vest",
    "trunks", "skirt",
]

class Item:
    __slots__ = ("id", "kor", "en", "category", "impl", "level", "req", "fixed_grade", "max_stack")

    def __init__(self, row, en):
        self.id = row[0]
        self.kor = row[1] or ""
        self.en = en
        self.category = row[2]
        self.impl = row[3]
        self.level = row[4]
        self.req = row[5]
        self.fixed_grade = row[6]
        # A kit entry is all-or-nothing per bag slot: never ask for more than one stack of an
        # item that does not stack (see top_materials and the livestock kit).
        self.max_stack = max(1, row[7] or 1)

    @property
    def name(self):
        return self.en or self.kor or str(self.id)


class Data:
    def __init__(self, conn):
        names = {
            idx: (text or "")
            for idx, text in conn.execute(
                "SELECT idx, en_us FROM localized_texts "
                "WHERE tbl_name = 'items' AND tbl_column_name = 'name'"
            )
        }
        self.items = {}
        for row in conn.execute(
            "SELECT id, name, category_id, impl_id, LEVEL, level_requirement, fixed_grade, "
            "max_stack_size "
            "FROM items"
        ):
            self.items[row[0]] = Item(row, names.get(row[0], ""))

        self.mate_kinds = {
            item_id: kind
            for item_id, kind in conn.execute(
                "SELECT m.item_id, n.mate_kind_id FROM item_summon_mates m "
                "JOIN npcs n ON n.id = m.npc_id"
            )
        }
        self.slave_names = {
            item_id: (name or "")
            for item_id, name in conn.execute(
                "SELECT m.item_id, s.name FROM item_summon_slaves m "
                "JOIN slaves s ON s.id = m.slave_id"
            )
        }
        self.decorations = {
            row[0] for row in conn.execute("SELECT item_id FROM item_housing_decorations")
        }
        self.sockets = {row[0] for row in conn.execute("SELECT item_id FROM item_sockets")}
        self.enchant_gems = {
            row[0] for row in conn.execute("SELECT item_id FROM item_enchanting_gems")
        }
        self.recipes = self._materials_by_group(conn, pack_only=False)
        self.pack_materials = self._materials_by_group(conn, pack_only=True)

    @staticmethod
    def _materials_by_group(conn, pack_only):
        source = (
            "craft_pack_crafts pc JOIN craft_materials m ON m.craft_id = pc.craft_id"
            if pack_only
            else "crafts cr JOIN craft_materials m ON m.craft_id = cr.id "
            "JOIN skills s ON s.id = cr.skill_id"
        )
        where = "" if pack_only else "WHERE s.actability_group_id IS NOT NULL"
        group = "NULL" if pack_only else "s.actability_group_id"
        sql = (
            f"SELECT {group} AS grp, m.item_id, COUNT(*) AS n FROM {source} "
            f"{where} GROUP BY grp, m.item_id ORDER BY grp, n DESC, m.item_id"
        )
        out = {}
        for grp, item_id, n in conn.execute(sql):
            out.setdefault(grp, []).append((item_id, n))
        return out

    def usable(self, item_id):
        item = self.items.get(item_id)
        if item is None:
            return False
        if not item.en.strip():
            return False
        if TEST_NAME_RE.search(item.en) or TEST_NAME_RE.search(item.kor):
            return False
        if item.category in JUNK_CATEGORIES:
            return False
        if item.impl == IMPL_ACCEPT_QUEST:
            return False
        return True


def sorted_items(items):
    return sorted(items, key=lambda i: (i.name.lower(), i.id))


def others(items):
    return sorted(items, key=lambda i: i.id)


def one_per_keyword(candidates, keywords, limit):
    """Deterministically pick up to one item per keyword (then fill by id)."""
    picked = []
    seen = set()
    ordered = sorted_items(candidates)
    for keyword in keywords:
        for item in ordered:
            if item.id in seen:
                continue
            if keyword in item.name.lower():
                picked.append(item)
                seen.add(item.id)
                break
        if len(picked) >= limit:
            break
    for item in ordered:
        if len(picked) >= limit:
            break
        if item.id not in seen:
            picked.append(item)
            seen.add(item.id)
    return sorted_items(picked)


def spread(items, count):
    """Evenly spaced, deterministic sample of a sorted list."""
    if len(items) <= count:
        return list(items)
    if count <= 0:
        return []
    step = (len(items) - 1) / float(count - 1)
    return [items[round(i * step)] for i in range(count)]


# --------------------------------------------------------------------------------------------
# Category builders: one card per item, except the gear brackets (one card, one SKU per grade).
# --------------------------------------------------------------------------------------------


def build_cards(data, conn):
    def items_where(predicate):
        return [
            item
            for item in data.items.values()
            if data.usable(item.id) and predicate(item)
        ]

    cards = []

    def card(main, sub, items, name=None, price=None, count=1):
        skus = [(item.id, count) for item in items]
        cards.append(
            {
                "main": main,
                "sub": sub,
                "name": name,
                "skus": skus,
                "price": price,
            }
        )

    # --- Land / housing ------------------------------------------------------------------
    designs = [
        item
        for item in items_where(lambda i: i.impl == IMPL_HOUSING and data.usable(i.id))
        if "design" in item.en.lower()
    ]
    farms = [
        item
        for item in designs
        if any(word in item.en.lower() for word in ("farm", "garden", "aquafarm"))
    ]
    siege_designs = [
        item
        for item in designs
        if any(
            word in item.en.lower()
            for word in ("tower", "wall", "gatehouse", "rampart", "barracks", "lodestone")
        )
    ]
    houses = [item for item in designs if item not in farms and item not in siege_designs]

    for item in sorted_items(farms):
        card(4, 7, [item])
    for item in sorted_items(houses):
        card(4, 7, [item])
    for item in sorted_items(siege_designs):
        card(4, 7, [item])

    storage_and_services = [125, 126, 17630, 28723, 28085, 34826, 34765, 31891, 31892, 34825]
    for item_id in storage_and_services:
        if data.usable(item_id):
            card(4, 2, [data.items[item_id]])

    furniture = [
        item
        for item in items_where(lambda i: i.category == 7 and i.id in data.decorations)
    ]
    for item in one_per_keyword(furniture, FURNITURE_KEYWORDS, 60):
        card(4, 3, [item])

    # --- Travel --------------------------------------------------------------------------
    gliders = items_where(lambda i: i.category == 118 and i.impl == 22)
    for item in sorted_items(gliders):
        card(3, 2, [item])

    mounts = [
        item
        for item in items_where(lambda i: i.id in data.mate_kinds)
        if data.mate_kinds[item.id] in MOUNT_MATE_KINDS
    ]
    for item in sorted_items(mounts):
        card(3, 3, [item])

    ships = []
    vehicles = []
    for item in items_where(lambda i: i.id in data.slave_names):
        slave_name = data.slave_names[item.id]
        if not slave_name.strip() or TEST_NAME_RE.search(slave_name):
            continue
        if item.category == 4:
            ships.append(item)
        elif item.category == 93:
            vehicles.append(item)
    for item in sorted_items(ships):
        card(3, 4, [item])
    for item in sorted_items(vehicles):
        card(3, 4, [item])

    # --- Pets ----------------------------------------------------------------------------
    pets = [
        item
        for item in items_where(lambda i: i.id in data.mate_kinds)
        if data.mate_kinds[item.id] in PET_MATE_KINDS
    ]
    for item in sorted_items(pets):
        card(6, 5, [item])

    pet_gear = items_where(lambda i: i.category == 108)
    by_level = {}
    for item in pet_gear:
        by_level.setdefault(item.level, []).append(item)
    for level in sorted(by_level):
        for item in sorted_items(by_level[level])[:3]:
            card(6, 5, [item])

    # --- Gear: one card per level bracket, one SKU per grade present in the 1.2 data -------
    for level in (10, 20, 30, 40, 50):
        candidates = [
            item
            for item in items_where(
                lambda i: i.category in EQUIP_PRIORITY and i.level == level
            )
        ]
        if not candidates:
            continue
        priority = {category: index for index, category in enumerate(EQUIP_PRIORITY)}
        base = min(
            (item for item in candidates if item.fixed_grade < 0),
            key=lambda i: (priority.get(i.category, 999), i.id),
            default=None,
        )
        grades = sorted(
            {
                item.fixed_grade
                for item in candidates
                if 2 <= item.fixed_grade <= 7
            }
        )
        picked = []
        if base is not None:
            picked.append(base)
        for grade in grades:
            picked.append(
                min(
                    (item for item in candidates if item.fixed_grade == grade),
                    key=lambda i: (priority.get(i.category, 999), i.id),
                )
            )
        if picked:
            cards.append(
                {
                    "main": 2,
                    "sub": 7,
                    "name": "Level %d Test Gear (grades %s)"
                    % (level, ", ".join(str(g) for g in ([0] + grades))),
                    "skus": [(item.id, 1) for item in picked],
                    "price": 1,
                }
            )

    # --- Upgrades ------------------------------------------------------------------------
    for item_id in REGRADE_SCROLLS + REGRADE_CHARMS:
        if data.usable(item_id):
            card(2, 3, [data.items[item_id]])

    lunagems = [
        item
        for item in items_where(lambda i: i.id in data.sockets)
        if item.name.startswith("Lucid ")
    ]
    for item in sorted_items(lunagems):
        card(2, 3, [item])

    lunadrops = [
        item
        for item in items_where(lambda i: i.category == 20 and i.id in data.enchant_gems)
        if re.match(
            r"^(Lucid|Radiant|Blazing|Beguiling|Astral) "
            r"(Fire|Gale|Earth|Wave|Life) Lunadrop$",
            item.name,
        )
    ]
    for item in sorted_items(lunadrops):
        card(2, 3, [item])

    # 24692 (Whole Insightful Lunafrost) is the item the live `lunafrost` scenario uses.
    for item_id in (24692, 24693, 24694, 24697, 24700):
        if data.usable(item_id):
            card(2, 3, [data.items[item_id]])

    # --- Consumables ---------------------------------------------------------------------
    potions = items_where(lambda i: i.category in (12, 13, 97, 113, 114, 116))
    for category in (12, 13, 97, 113, 114, 116):
        for item in sorted_items([item for item in potions if item.category == category])[:8]:
            card(2, 2, [item], count=10)

    # --- Other: gear support, instruments, fishing, siege, crafting ----------------------
    for category in (80, 81, 82):
        items = sorted_items(items_where(lambda i: i.category == category))
        for item in spread(items, 5):
            card(2, 7, [item])

    for item in sorted_items(items_where(lambda i: i.category == 145)):
        card(2, 7, [item])

    siege = [
        item
        for item in items_where(lambda i: i.category == 15)
        if item.impl in (0, 8)
        and not any(
            word in item.name
            for word in ("Jabberwock", "Wolfhound", "Trainee", "War Meteor", "Hostile")
        )
    ]
    for item in sorted_items(siege):
        card(2, 7, [item])
    for item_id in (26397, 26424, 26428, 27300, 27302):
        if data.usable(item_id):
            card(2, 7, [data.items[item_id]])
    for item in sorted_items(
        items_where(
            lambda i: i.category == 120
            and ("Guardian Seal" in i.name or "Marching Seal" in i.name)
        )
    ):
        card(2, 7, [item])

    crafting = [
        8318, 8337, 8256, 16327, 8343, 8327, 8328, 8329, 8326, 15621,
        17776, 16356, 16357, 16358, 16324, 16328, 27545, 27278, 8376, 30903,
    ]
    crafting_items = [data.items[i] for i in crafting if data.usable(i)]
    if crafting_items:
        cards.append(
            {
                "main": 2,
                "sub": 7,
                "name": "Crafting Materials (TEST, 10 each)",
                "skus": [(item.id, 10) for item in crafting_items],
                "price": 1,
            }
        )

    # --- Cosmetics -----------------------------------------------------------------------
    dyes = items_where(lambda i: i.category == 33)
    for item in sorted_items(dyes):
        card(5, 2, [item])

    costumes = items_where(lambda i: i.category == 10)
    for item in one_per_keyword(costumes, COSTUME_KEYWORDS, 64):
        card(5, 3, [item])

    # --- Character services and Loyalty items --------------------------------------------
    for item_id in (30810, 30811, 8000025, 8000002):
        if data.usable(item_id):
            card(2, 4, [data.items[item_id]])

    loyalty = [28586, 29911, 29912, 29913, 29914, 28794, 30603, 30604, 30605, 28816]
    for item_id in loyalty:
        if data.usable(item_id):
            card(6, 2, [data.items[item_id]])
    for item_id in (4045, 4053):
        if data.usable(item_id):
            card(6, 2, [data.items[item_id]])

    # One card per item for the single-item groups; a few ids (e.g. the siege scrolls that are
    # also in `siege_items`) would otherwise be listed twice.
    seen = set()
    seen_items = set()
    deduped = []
    for card_ in cards:
        key = tuple(item_id for item_id, _count in card_["skus"])
        if key in seen or (len(key) == 1 and key[0] in seen_items):
            continue
        seen.add(key)
        seen_items.update(key)
        deduped.append(card_)
    return deduped


# --------------------------------------------------------------------------------------------
# SQL output
# --------------------------------------------------------------------------------------------


def sql_quote(text):
    return "'" + text.replace("\\", "\\\\").replace("'", "''") + "'"


def write_sql(cards, out_path, data):
    shop_id = SHOP_BASE
    sku_id = SKU_BASE
    menu_id = MENU_BASE
    shops = []
    skus = []
    menu = []
    counts = {}
    positions = {}
    for card in cards:
        sid = shop_id
        shop_id += 1
        display = card["skus"][0][0] if len(card["skus"]) > 1 else 0
        name = sql_quote(card["name"]) if card["name"] else "NULL"
        shops.append(
            "(%d, %d, %s, 0, 0, 0, 0, 0, 0, 0, 0, NULL, NULL, 0, -1)"
            % (sid, display, name)
        )
        for position, (item_id, count) in enumerate(card["skus"]):
            price = card["price"] if card["price"] is not None else (1 if card["main"] == 2 and card["sub"] in (3, 7) else 0)
            skus.append(
                "(%d, %d, %d, %d, %d, 0, %d, 0, NULL, 0, %d, 0, 0, 0)"
                % (
                    sku_id,
                    sid,
                    position,
                    item_id,
                    count,
                    1 if position == 0 else 0,
                    price,
                )
            )
            sku_id += 1
        for main, sub in ((card["main"], card["sub"]), (card["main"], 1)):
            if sub == 1 and card["sub"] == 1:
                continue
            positions[(main, sub)] = positions.get((main, sub), 0) + 1
            menu.append(
                "(%d, %d, %d, %d, %d)" % (menu_id, main, sub, positions[(main, sub)], sid)
            )
            menu_id += 1
        key = (card["main"], card["sub"])
        counts[key] = counts.get(key, 0) + 1

    lines = [
        "-- TEST Marketplace catalogue for AAEmu 1.2. Generated by",
        "-- `python -I rust/tools/test_catalogue/gen.py`; do not edit by hand.",
        "--",
        "-- For a private test server only: every SKU costs 0 or 1 credit, so testers can buy",
        "-- the item and check that the server delivers a working object. Import after",
        "-- SQL/aaemu_game.sql (and after clearing ics_menu/ics_shop_items/ics_skus when this",
        "-- file replaces another catalogue):",
        "--",
        "--   DELETE FROM ics_menu; DELETE FROM ics_shop_items; DELETE FROM ics_skus;",
        "--   mysql ... aaemu_game < SQL/examples/example-ics-test.sql",
        "--",
        "-- Sources: the 1.2 compact.sqlite3 (`items`, `localized_texts`, `item_summon_mates`,",
        "-- `item_summon_slaves`, `item_housing_decorations`, `item_sockets`,",
        "-- `item_enchanting_gems`, `crafts`/`craft_materials`). Ids: shop 2400001+, SKU",
        "-- 1400001+, menu 4001+, so this file can be imported next to the other examples.",
        "--",
        "-- Only ids present in the 1.2 `items` table are used; localized names for ids that are",
        "-- not in `items` are leftovers of another version and are never selected. Rows whose",
        "-- name looks like a test/sample/dev row, junk/quest categories, and summons without",
        "-- their mate/slave row are skipped (see the generator header).",
        "--",
        "-- Tabs (1.2 client labels from ui_texts; 3/4 has no client label, \"ships/vehicles\" is ours): 2/1 All, 2/2 Consumables, 2/3 Luxury,",
        "-- 2/4 Character, 2/7 Other; 3/1 All, 3/2 Glider, 3/3 Mount, 3/4 ships/vehicles;",
        "-- 4/1 All, 4/2 Tax/Sales, 4/3 Decor, 4/7 Misc.; 5/1 All, 5/2 Consume, 5/3 Costume;",
        "-- 6/1 All, 6/2 Items, 6/5 Pets. Counts per tab:",
    ]
    for (main, sub), count in sorted(counts.items()):
        lines.append("--   main %d / sub %d: %d shops" % (main, sub, count))
    lines.append("SET NAMES utf8mb4;")
    lines.append("")
    lines.append(
        "REPLACE INTO `ics_shop_items` (`shop_id`, `display_item_id`, `name`, `limited_type`, "
        "`limited_stock_max`, `level_min`, `level_max`, `buy_restrict_type`, `buy_restrict_id`, "
        "`is_sale`, `is_hidden`, `sale_start`, `sale_end`, `shop_buttons`, `remaining`) VALUES"
    )
    lines.append(",\n".join("\t" + row for row in shops) + ";")
    lines.append("")
    lines.append(
        "REPLACE INTO `ics_skus` (`sku`, `shop_id`, `position`, `item_id`, `item_count`, "
        "`select_type`, `is_default`, `event_type`, `event_end_date`, `currency`, `price`, "
        "`discount_price`, `bonus_item_id`, `bonus_item_count`) VALUES"
    )
    lines.append(",\n".join("\t" + row for row in skus) + ";")
    lines.append("")
    lines.append(
        "REPLACE INTO `ics_menu` (`id`, `main_tab`, `sub_tab`, `tab_pos`, `shop_id`) VALUES"
    )
    lines.append(",\n".join("\t" + row for row in menu) + ";")
    lines.append("")

    with open(out_path, "w", encoding="utf-8", newline="\n") as handle:
        handle.write("\n".join(lines))
    return len(shops), len(skus), len(menu), counts


# --------------------------------------------------------------------------------------------
# Kits
# --------------------------------------------------------------------------------------------


def top_materials(data, entries, count, limit, skip=()):
    picked = []
    seen = set(skip)
    for item_id, _n in entries:
        if item_id in seen:
            continue
        item = data.items.get(item_id)
        if item is None or not data.usable(item_id):
            continue
        picked.append({"id": item_id, "count": min(count, item.max_stack)})
        seen.add(item_id)
        if len(picked) >= limit:
            break
    return picked


def kit_entry(names, item_id, count, grade=None):
    entry = {"names": list(names), "id": item_id, "count": count}
    if grade is not None:
        entry["grade"] = grade
    return entry


def build_kits(data):
    kits = []

    # The 1.2 crafts table has no fishing or mining recipes, so those two kits carry the
    # profession's raw goods instead of recipe materials (documented in the wiki page).
    fallbacks = {
        7: [27142, 27313, 27321, 27322, 27323, 27324],
        13: [8022, 3411, 8023, 8027, 1386, 8008, 8081, 8067],
    }
    for group, vocation in sorted(VOCATIONS.items()):
        names = ["vocation_" + vocation]
        materials = top_materials(data, data.recipes.get(group, []), 100, 8)
        if not materials:
            materials = [
                {"id": item_id, "count": 100}
                for item_id in fallbacks.get(group, [])
                if data.usable(item_id)
            ]
        for entry in materials:
            kits.append(kit_entry(names, entry["id"], entry["count"]))

    trade = top_materials(data, data.pack_materials.get(None, []), 100, 15)
    for entry in trade:
        kits.append(kit_entry(["tradepack_materials"], entry["id"], entry["count"]))

    seeds = [
        item
        for item in guaranteed_sorted(data, 51)
        if data.usable(item.id)
    ][:16]
    saplings = [
        item
        for item in guaranteed_sorted(data, 8)
        if data.usable(item.id) and "sapling" in item.name.lower()
    ][:16]
    livestock = [
        item
        for item in guaranteed_sorted(data, 21)
        if data.usable(item.id)
    ][:12]
    # Three kits instead of one: 44 entries of 20 need 182 bag slots because every animal
    # item is its own slot, and a fresh character has 50. Crops and saplings stack (one slot
    # each); the animals are 3 each when they do not stack, 10 when they stack to 10.
    for item in seeds:
        kits.append(kit_entry(["seeds"], item.id, 20))
    for item in saplings:
        kits.append(kit_entry(["saplings"], item.id, 20))
    for item in livestock:
        kits.append(
            kit_entry(["livestock"], item.id, 3 if item.max_stack == 1 else min(10, item.max_stack))
        )

    for item_id in REGRADE_SCROLLS:
        if data.usable(item_id):
            kits.append(kit_entry(["regrade_test"], item_id, 10))
    for item_id in REGRADE_CHARMS:
        if data.usable(item_id):
            kits.append(kit_entry(["regrade_test"], item_id, 10))
    for item_id in (20213, 20573, 20297, 20964):
        if data.usable(item_id):
            kits.append(kit_entry(["regrade_test"], item_id, 1))

    lunagems = [
        item
        for item in data.items.values()
        if item.id in data.sockets and item.name.startswith("Lucid ") and data.usable(item.id)
    ]
    for item in sorted_items(lunagems)[:12]:
        kits.append(kit_entry(["lunagem_set"], item.id, 5))
    lunadrops = [
        item
        for item in data.items.values()
        if item.category == 20
        and item.id in data.enchant_gems
        and re.match(
            r"^(Lucid|Radiant|Blazing|Beguiling|Astral) "
            r"(Fire|Gale|Earth|Wave|Life) Lunadrop$",
            item.name,
        )
    ]
    for item in sorted_items(lunadrops)[:10]:
        kits.append(kit_entry(["lunagem_set"], item.id, 5))
    lunafrosts = [
        item
        for item in data.items.values()
        if item.id in data.enchant_gems and "Lunafrost" in item.name and data.usable(item.id)
    ]
    for item in sorted_items(lunafrosts)[:3]:
        kits.append(kit_entry(["lunagem_set"], item.id, 5))

    bracket_sets = {
        10: (
            ["Civilian Longsword", "Civilian Greatsword", "Civilian Bow",
             "Civilian Magic Staff", "Civilian Shield"],
            ["Shabby Pirate's Cap", "Shabby Pirate's Jerkin", "Shabby Pirate's Guards",
             "Shabby Pirate's Fists", "Shabby Pirate's Belt", "Shabby Pirate's Breeches",
             "Shabby Pirate's Boots"],
        ),
        20: (
            ["Apprentice's Summer Sword", "Apprentice's Desert Greatsword",
             "Apprentice's Volcano Bow", "Apprentice's Ocean Staff", "Apprentice's Quake Shield"],
            ["Apprentice's Ocean Hood", "Apprentice's Ocean Shirt", "Apprentice's Ocean Pants",
             "Apprentice's Ocean Gloves", "Apprentice's Ocean Shoes",
             "Apprentice's Ocean Sleeves", "Apprentice's Ocean Sash",
             "Apprentice's Typhoon Cap", "Apprentice's Typhoon Jerkin",
             "Apprentice's Typhoon Breeches", "Apprentice's Typhoon Fists",
             "Apprentice's Typhoon Boots", "Apprentice's Typhoon Guards",
             "Apprentice's Typhoon Belt",
             "Apprentice's Quake Helm", "Apprentice's Quake Cuirass",
             "Apprentice's Quake Greaves", "Apprentice's Quake Gauntlets",
             "Apprentice's Quake Sabatons", "Apprentice's Quake Vambraces",
             "Apprentice's Quake Tassets"],
        ),
        30: (
            ["Artificer's Summer Sword", "Artificer's Desert Greatsword",
             "Artificer's Volcano Bow", "Artificer's Ocean Staff", "Artificer's Quake Shield"],
            ["Artificer's Ocean Hood", "Artificer's Ocean Shirt", "Artificer's Ocean Pants",
             "Artificer's Ocean Gloves", "Artificer's Ocean Shoes",
             "Artificer's Ocean Sleeves", "Artificer's Ocean Sash",
             "Artificer's Typhoon Cap", "Artificer's Typhoon Jerkin",
             "Artificer's Typhoon Breeches", "Artificer's Typhoon Fists",
             "Artificer's Typhoon Boots", "Artificer's Typhoon Guards",
             "Artificer's Typhoon Belt",
             "Artificer's Quake Helm", "Artificer's Quake Cuirass",
             "Artificer's Quake Greaves", "Artificer's Quake Gauntlets",
             "Artificer's Quake Sabatons", "Artificer's Quake Vambraces",
             "Artificer's Quake Tassets"],
        ),
        40: (
            ["Conqueror's Summer Sword", "Conqueror's Desert Greatsword",
             "Conqueror's Volcano Bow", "Conqueror's Ocean Staff", "Conqueror's Quake Shield"],
            ["Conqueror's Ocean Hood", "Conqueror's Ocean Shirt", "Conqueror's Ocean Pants",
             "Conqueror's Ocean Gloves", "Conqueror's Ocean Shoes",
             "Conqueror's Ocean Sleeves", "Conqueror's Ocean Sash",
             "Conqueror's Typhoon Cap", "Conqueror's Typhoon Jerkin",
             "Conqueror's Typhoon Breeches", "Conqueror's Typhoon Fists",
             "Conqueror's Typhoon Boots", "Conqueror's Typhoon Guards",
             "Conqueror's Typhoon Belt",
             "Conqueror's Quake Helm", "Conqueror's Quake Cuirass",
             "Conqueror's Quake Greaves", "Conqueror's Quake Gauntlets",
             "Conqueror's Quake Sabatons", "Conqueror's Quake Vambraces",
             "Conqueror's Quake Tassets"],
        ),
        50: (
            ["Epherium Summer Sword", "Epherium Desert Greatsword",
             "Epherium Volcano Bow", "Epherium Ocean Staff", "Epherium Quake Shield"],
            ["Epherium Ocean Hood", "Epherium Ocean Shirt", "Epherium Ocean Pants",
             "Epherium Ocean Gloves", "Epherium Ocean Shoes",
             "Epherium Ocean Sleeves", "Epherium Ocean Sash",
             "Epherium Typhoon Cap", "Epherium Typhoon Jerkin",
             "Epherium Typhoon Breeches", "Epherium Typhoon Fists",
             "Epherium Typhoon Boots", "Epherium Typhoon Guards",
             "Epherium Typhoon Belt",
             "Epherium Quake Helm", "Epherium Quake Cuirass",
             "Epherium Quake Greaves", "Epherium Quake Gauntlets",
             "Epherium Quake Sabatons", "Epherium Quake Vambraces",
             "Epherium Quake Tassets"],
        ),
    }
    by_name = {}
    for item in data.items.values():
        by_name.setdefault(item.name, []).append(item)
    for level, (weapons, armor) in sorted(bracket_sets.items()):
        names = ["gear_%d" % level]
        for wanted in weapons + armor:
            matches = sorted(by_name.get(wanted, []), key=lambda i: i.id)
            if matches and data.usable(matches[0].id):
                kits.append(kit_entry(names, matches[0].id, 1, grade=5))

    for item_id in (31891, 31892, 34825):
        if data.usable(item_id):
            kits.append(kit_entry(["tax_certificates"], item_id, 10))

    return kits


def guaranteed_sorted(data, category):
    return sorted(
        (item for item in data.items.values() if item.category == category),
        key=lambda i: i.id,
    )


# --------------------------------------------------------------------------------------------
# Main
# --------------------------------------------------------------------------------------------


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--compact", default=DEFAULT_COMPACT)
    parser.add_argument("--out", default=DEFAULT_OUT)
    parser.add_argument("--kits-out", default=None,
                        help="write the generated GM kit entries to this JSON file")
    args = parser.parse_args(argv)

    if not os.path.isfile(args.compact):
        parser.error("compact.sqlite3 not found: %s" % args.compact)
    uri = pathlib.Path(args.compact).resolve().as_uri() + "?mode=ro"
    conn = sqlite3.connect(uri, uri=True)
    try:
        data = Data(conn)
        cards = build_cards(data, conn)
        shops, skus, menu_rows, counts = write_sql(cards, args.out, data)
    finally:
        conn.close()

    print("wrote %s" % args.out)
    print("  %d shops, %d SKUs, %d menu rows" % (shops, skus, menu_rows))
    for (main, sub), count in sorted(counts.items()):
        print("  main %d / sub %d: %d" % (main, sub, count))

    if args.kits_out:
        kits = build_kits(data)
        with open(args.kits_out, "w", encoding="utf-8", newline="\n") as handle:
            json.dump({"itemkits": kits}, handle, indent=4, ensure_ascii=False)
            handle.write("\n")
        print("wrote %s (%d kit entries)" % (args.kits_out, len(kits)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
