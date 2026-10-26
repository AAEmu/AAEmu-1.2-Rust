"""Backport valid 10.x doodad / NPC spawns into the 1.2 main_world spawn data.

Two phases around the ignored Rust test `dump_terrain_heights` (needs the 1.2 `game_pak`):

    python -I backport_10x.py prepare --src10x <AAEmu-108 Data/Worlds/main_world> \
        --world12 <app/Data/Worlds/main_world> --compact <compact.sqlite3> --work <dir>
    AAEMU_GAME_PAK=D:/AA1.2/game_pak AAEMU_HEIGHT_QUERY=<dir>/query.json \
        AAEMU_HEIGHT_OUT=<dir>/heights.json cargo test -p aaemu-game dump_terrain_heights -- --ignored
    python -I backport_10x.py finish --world12 <app/Data/Worlds/main_world> --compact <compact.sqlite3> \
        --work <dir>

`prepare` applies the template and title filters and writes the terrain query; `finish` applies the
zone, terrain / neighbour, duplicate and exclusion filters and writes `npc_spawns_10x.json` and
`doodad_spawns_10x.json` into the 1.2 world folder (the loader reads every `npc_spawns*.json` /
`doodad_spawns*.json`). Rules follow PORTING.md "Spawn data import".
"""
import argparse
import collections
import json
import math
import os
import re
import sqlite3
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from lib import Grid, load_spawn_file, tolerant_json  # noqa: E402

sys.stdout.reconfigure(encoding="utf-8")

TERRAIN_TOL = 5.0
CLIP = 1023.5
NEIGHBOR_R = 40.0
NEIGHBOR_DZ = 8.0
DUP_R = 2.0  # among accepted 10.x entries
MATCH_R = {"npc": 20.0, "doodad": 10.0}  # one-to-one match to existing 1.2 spawns

# Doodad names that are markers or 10.x-only systems rather than world content.
EXCLUDE_DOODAD_NAMES = [
    re.compile(r"restricted area", re.I),
    re.compile(r"sunken treasure chest", re.I),
]
IRON_VEIN = 1671
# zone_groups.name (display name): Rookborne Basin, Feuille Sound (sea), White Arden, Tigerspine
# Mountains, Silent Forest, Sanddeep
REPORT_GROUPS = [
    "e_lokas_checkers",
    "s_crescent_sea",
    "w_white_forest",
    "e_tiger_spine_mountains",
    "e_ancient_forest",
    "w_long_sand",
]


def norm(s):
    return re.sub(r"\s+", " ", (s or "").strip()).casefold()


def names(db, tbl):
    return {
        r[0]: r[1] or ""
        for r in db.execute(
            "select idx, en_us from localized_texts where tbl_name=? and tbl_column_name='name'", (tbl,)
        )
    }


def load_db(path):
    db = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    return {
        "npc_ids": {r[0] for r in db.execute("select id from npcs")},
        "doodad_ids": {r[0] for r in db.execute("select id from doodad_almighties")},
        "npc_names": names(db, "npcs"),
        "doodad_names": names(db, "doodad_almighties"),
        "zone_group": {
            r[0]: r[1]
            for r in db.execute(
                "select z.zone_key, g.name from zones z join zone_groups g on g.id = z.group_id"
            )
        },
    }


def read(path):
    return tolerant_json(open(path, encoding="utf-8-sig").read())


def prepare(a):
    db = load_db(a.compact)
    stats = collections.OrderedDict()
    cands, query = [], []
    for kind, fname, ids, nm in (
        ("npc", "npc_spawns.json", db["npc_ids"], db["npc_names"]),
        ("doodad", "doodad_spawns.json", db["doodad_ids"], db["doodad_names"]),
    ):
        src = read(os.path.join(a.src10x, fname))
        s = collections.Counter(total=len(src))
        for e in src:
            uid = e.get("UnitId")
            p = e.get("Position")
            if uid is None or not p:
                s["no position"] += 1
                continue
            if uid not in ids:
                s["f1 template missing in 1.2"] += 1
                continue
            if norm(e.get("Title")) != norm(nm.get(uid)):
                s["f2 title != 1.2 name"] += 1
                continue
            i = len(cands)
            cands.append({"kind": kind, "e": e})
            query.append({"id": str(i), "world": "main_world", "x": p["X"], "y": p["Y"]})
        stats[kind] = s
    for i, (uid, x, y, _z) in enumerate(load_spawn_file(os.path.join(a.world12, "doodad_spawns.json"))):
        if uid == IRON_VEIN:
            query.append({"id": f"e{i}", "world": "main_world", "x": x, "y": y})
    os.makedirs(a.work, exist_ok=True)
    json.dump(cands, open(os.path.join(a.work, "cands.json"), "w", encoding="utf-8"))
    json.dump(query, open(os.path.join(a.work, "query.json"), "w"))
    json.dump(stats, open(os.path.join(a.work, "prepare_stats.json"), "w"))
    for k, s in stats.items():
        print(k, dict(s))
    print("queries:", len(query))


def entry(c):
    e = c["e"]
    p = e["Position"]
    pos = {"X": round(p["X"], 4), "Y": round(p["Y"], 4), "Z": round(p["Z"], 4)}
    for k in ("Roll", "Pitch", "Yaw"):
        if p.get(k):
            pos[k] = p[k]
    out = {"UnitId": e["UnitId"], "Title": e.get("Title", ""), "Position": pos}
    if e.get("Scale") and e["Scale"] != 1.0:
        out["Scale"] = e["Scale"]
    return out


def finish(a):
    db = load_db(a.compact)
    cands = json.load(open(os.path.join(a.work, "cands.json"), encoding="utf-8"))
    heights = {r["id"]: r for r in json.load(open(os.path.join(a.work, "heights.json")))}
    prep = json.load(open(os.path.join(a.work, "prepare_stats.json")))
    grids = {}  # kind -> Grid of existing 1.2 spawns of that kind
    anyg = Grid()
    for kind, fname in (("npc", "npc_spawns.json"), ("doodad", "doodad_spawns.json")):
        g = Grid()
        for uid, x, y, z in load_spawn_file(os.path.join(a.world12, fname)):
            g.add(uid, x, y, z)
            anyg.add(uid, x, y, z)
        grids[kind] = g
    stats = {"npc": collections.Counter(prep["npc"]), "doodad": collections.Counter(prep["doodad"])}
    kept = {"npc": [], "doodad": []}
    seen = {"npc": Grid(), "doodad": Grid()}  # accepted 10.x entries, to dedup among themselves
    excluded = collections.Counter()

    # Filter 5: one-to-one match per template. Every 10.x candidate that passed filters 1-3 and 6
    # (terrain not yet applied) is paired greedily, by ascending XY distance, with the nearest still
    # unmatched existing 1.2 spawn of the same template within MATCH_R; matched candidates are the
    # same spawn slightly moved and are dropped, only the surplus survives.
    matched = set()
    for kind, fname in (("npc", "npc_spawns.json"), ("doodad", "doodad_spawns.json")):
        eg = Grid()
        for j, (uid, x, y, z) in enumerate(load_spawn_file(os.path.join(a.world12, fname))):
            eg.add(uid, x, y, j)
        pairs = []
        for i, c in enumerate(cands):
            e = c["e"]
            if c["kind"] != kind or not heights.get(str(i), {}).get("zone"):
                continue
            if kind == "doodad" and any(r.search(e.get("Title", "")) for r in EXCLUDE_DOODAD_NAMES):
                continue
            x, y = e["Position"]["X"], e["Position"]["Y"]
            for n in eg.near(x, y, MATCH_R[kind]):
                if n[0] == e["UnitId"]:
                    pairs.append((math.hypot(n[1] - x, n[2] - y), i, n[3]))
        pairs.sort()
        used = set()
        for d, i, j in pairs:
            if i not in matched and j not in used:
                matched.add(i)
                used.add(j)

    for i, c in enumerate(cands):
        kind, e = c["kind"], c["e"]
        s = stats[kind]
        uid, p = e["UnitId"], e["Position"]
        x, y, z = p["X"], p["Y"], p["Z"]
        h = heights.get(str(i), {})
        if not h.get("zone"):
            s["f3 zone 0 / not in 1.2 world"] += 1
            continue
        title = e.get("Title", "")
        if kind == "doodad" and any(r.search(title) for r in EXCLUDE_DOODAD_NAMES):
            s["f6 excluded doodad name"] += 1
            excluded[title] += 1
            continue
        if i in matched:
            s["f5 one-to-one match to existing 1.2 spawn"] += 1
            continue
        ok = False
        if "h" in h:
            if h["h"] >= CLIP:
                ok = True  # clipped ground: height unverifiable
            elif abs(z - h["h"]) <= TERRAIN_TOL:
                ok = True
        if not ok:
            ok = any(abs(n[3] - z) <= NEIGHBOR_DZ for n in anyg.near(x, y, NEIGHBOR_R))
        if not ok:
            s["f4 terrain / neighbour"] += 1
            continue
        if any(n[0] == uid for n in seen[kind].near(x, y, DUP_R)):
            s["f5b duplicate of another 10.x entry within 2 m"] += 1
            continue
        seen[kind].add(uid, x, y, z)
        kept[kind].append((entry(c), h["zone"]))
        s["survivors"] += 1
    for kind, fname in (("npc", "npc_spawns_10x.json"), ("doodad", "doodad_spawns_10x.json")):
        rows = [r for r, _ in kept[kind]]
        rows.sort(key=lambda r: (r["UnitId"], r["Position"]["X"], r["Position"]["Y"], r["Position"]["Z"]))
        with open(os.path.join(a.world12, fname), "w", encoding="utf-8", newline="\n") as f:
            json.dump(rows, f, indent=2, ensure_ascii=False)
            f.write("\n")
    for kind in ("npc", "doodad"):
        print(kind, dict(stats[kind]))
        top = collections.Counter((r["UnitId"], r["Title"]) for r, _ in kept[kind]).most_common(20)
        print(f"top {kind} templates:", top)
    for kind, fname in (("npc", "npc_spawns.json"), ("doodad", "doodad_spawns.json")):
        by = collections.defaultdict(list)
        for uid, x, y, _z in load_spawn_file(os.path.join(a.world12, fname)):
            by[uid].append((x, y))
        hist = collections.Counter()
        for r, _ in kept[kind]:
            px, py = r["Position"]["X"], r["Position"]["Y"]
            d = min((math.hypot(px - x, py - y) for x, y in by.get(r["UnitId"], ())), default=None)
            hist["none" if d is None else "<5" if d < 5 else "<15" if d < 15 else "<50" if d < 50 else ">=50"] += 1
        print(f"{kind} survivors by nearest same-template 1.2 spawn:", dict(hist))
    print("excluded doodad names:", dict(excluded))
    # Iron Vein per zone group before / after
    zg = db["zone_group"]
    before, after = collections.Counter(), collections.Counter()
    added = collections.Counter(zg.get(zone, "?") for r, zone in kept["doodad"] if r["UnitId"] == IRON_VEIN)
    for k, r in heights.items():
        if k.startswith("e"):
            before[zg.get(r.get("zone"), "?")] += 1
    print("Iron Vein (1671) per zone group: before(1.2) / added / after")
    for g in REPORT_GROUPS:
        print(f"  {g}: {before[g]} / {added[g]} / {before[g] + added[g]}")
    print("zones of added iron veins:", dict(added))


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("phase", choices=["prepare", "finish"])
    ap.add_argument("--src10x")
    ap.add_argument("--world12", required=True)
    ap.add_argument("--compact", required=True)
    ap.add_argument("--work", required=True)
    a = ap.parse_args()
    {"prepare": prepare, "finish": finish}[a.phase](a)
