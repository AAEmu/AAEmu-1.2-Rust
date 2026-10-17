"""Step 2: judge every candidate against the 1.2 terrain and the existing spawns.

    python -I evaluate.py <spawn-sources dir> <heights.json> <Data/Worlds dir> <decisions.json>

Evidence per candidate and frame (the 10.x frame "as" and, for the ten zones whose origin differs
between the 1.2 and 10.x world.xml, the 1.2 frame "12"):
  terrain    |Z - 1.2 terrain height| <= 5 m on a heightmap point below its 1024 m ceiling
             (heights are stored as u16 / 64, so higher ground is clipped and proves nothing)
  clip       the point lies on clipped ground (>= 1023.5 m), Z is at least that high, the candidate is
             editor-tier and its (world, editor zone, frame) is verified by other candidates: has >= 3
             unclipped-terrain candidates within 5 m and they are >= 50 % of its unclipped ones. Such
             positions are accepted but remain unverified in Z
  neighbor   an existing spawn (NPC or doodad) of the same world within 40 m horizontally and 8 m
             in Z - covers buildings, caves, bridges and interiors where Z is far from the terrain
The 1.2 zone id at the point is recorded (`zone12`) but not a criterion: the zones whose origin moved
between 1.2 and 10.x keep their content at the 10.x position (the 1.2 frame never matches).
A candidate is accepted when terrain, neighbor or clip evidence holds; with both frames possible the frame
with the best evidence wins. Per id, candidates from the editor/10.x data win over git-history-only
positions (which are only used when the id has no accepted editor-tier candidate).
"""
import collections
import json
import math
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from lib import Grid, load_spawn_file  # noqa: E402

TERRAIN_TOL = 5.0
CLIP = 1023.5
NEIGHBOR_R = 40.0
NEIGHBOR_DZ = 8.0
EDITOR_TIERS = ("exact-1.2", "10x-verified", "inferred-from-sphere")


def tier_of(c):
    base = c["confidence"].replace("moved-zone/", "")
    return "editor" if base.split("/")[0] in EDITOR_TIERS else "git"


def main(src, heights_path, worlds_dir, out):
    cands = json.load(open(os.path.join(src, "candidates.json"), encoding="utf-8"))
    heights = {r["id"]: r for r in json.load(open(heights_path))}
    grids = {}

    def grid(world):
        if world not in grids:
            g = Grid()
            for name in ("npc_spawns.json", "doodad_spawns.json"):
                for uid, x, y, z in load_spawn_file(os.path.join(worlds_dir, world, name)):
                    g.add(uid, x, y, z)
            grids[world] = g
        return grids[world]

    # pass 1: evidence per candidate and frame
    decisions = []
    for kind, key in (("npc", "npcs"), ("doodad", "doodads")):
        for uid, entry in cands[key].items():
            for i, c in enumerate(entry["candidates"]):
                if c.get("world_in_repo") is False:
                    continue
                o12, o10 = c.get("zone_origin_1_2"), c.get("zone_origin_10x")
                frames = {"as": (c["x"], c["y"])}
                if o12 and o10 and o12 != o10:
                    frames["12"] = (c["x"] + (o12[0] - o10[0]) * 1024, c["y"] + (o12[1] - o10[1]) * 1024)
                evals = {}
                for frame, (x, y) in frames.items():
                    h = heights.get(f"{kind}:{uid}:{i}:{frame}")
                    ev = {"x": x, "y": y, "frame": frame, "terrain": False, "clipped": False}
                    if h is None or "h" not in h:
                        ev["why"] = (h or {}).get("error", "no height")
                        ev["no_world"] = ev["why"] == "no world template"
                    else:
                        dz = c["z"] - h["h"]
                        ev.update(h=round(h["h"], 2), dz=round(dz, 2), zone12=h.get("zone"))
                        ev["clipped"] = h["h"] >= CLIP
                        ev["terrain"] = (not ev["clipped"]) and abs(dz) <= TERRAIN_TOL
                    ev["neighbor"] = any(
                        abs(e[3] - c["z"]) <= NEIGHBOR_DZ for e in grid(c["world"]).near(x, y, NEIGHBOR_R)
                    )
                    evals[frame] = ev
                decisions.append(
                    {
                        "kind": kind,
                        "id": int(uid),
                        "world": c["world"],
                        "tier": tier_of(c),
                        "confidence": c["confidence"],
                        "source": c["source"],
                        "z": c["z"],
                        "yaw": c.get("yaw", 0.0),
                        "zone": c.get("zone"),
                        "evals": evals,
                    }
                )

    # region verification of the clipped ground: (world, editor zone, frame) -> [ok, total]
    region = collections.defaultdict(lambda: [0, 0])
    for d in decisions:
        if d["tier"] != "editor" or d["zone"] is None:
            continue
        for frame, ev in d["evals"].items():
            if "dz" in ev and not ev["clipped"]:
                r = region[(d["world"], d["zone"], frame)]
                r[1] += 1
                r[0] += 1 if ev["terrain"] else 0

    def region_ok(d, frame):
        zone = d["zone"] if d["zone"] is not None else d["evals"][frame].get("zone12")
        ok, total = region.get((d["world"], zone, frame), (0, 0))
        return ok >= 3 and ok / total >= 0.5

    # pass 2: pick the frame with the best evidence
    rank = {"terrain": 3, "neighbor": 2, "clip": 1}
    for d in decisions:
        best = None
        for frame, ev in d["evals"].items():
            if ev.get("no_world"):
                ev["basis"] = None  # the 1.2 client data has no such world: the server cannot load it
            elif ev["terrain"]:
                ev["basis"] = "terrain"
            elif ev["neighbor"]:
                ev["basis"] = "neighbor"
            elif ev["clipped"] and d["tier"] == "editor" and d["z"] >= ev["h"] - TERRAIN_TOL and region_ok(d, frame):
                ev["basis"] = "clip"
            else:
                ev["basis"] = None
            key = (rank.get(ev["basis"], 0), -abs(ev.get("dz", 1e9)))
            if best is None or key > best[0]:
                best = (key, ev)
        d["chosen"] = best[1]
        d["accepted"] = best[1]["basis"] is not None
        d["basis"] = best[1]["basis"]
    json.dump(decisions, open(out, "w"), indent=1)
    cnt = collections.Counter()
    for d in decisions:
        cnt[(d["kind"], d["world"], d["tier"], d["basis"])] += 1
    for k, v in sorted(cnt.items(), key=str):
        print(k, v)
    print("regions", {k: v for k, v in sorted(region.items(), key=str) if v[1] >= 3})


if __name__ == "__main__":
    main(*sys.argv[1:5])
