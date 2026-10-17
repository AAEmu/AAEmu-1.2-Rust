"""Step 1 of the spawn import: flatten the research candidates into terrain queries.

    python -I collect.py <spawn-sources dir> <queries.json>

`<spawn-sources dir>` holds `candidates.json` (see sources.md there). Every candidate of every
world becomes one or two queries: a candidate of a `moved-zone` zone (the zone origin differs
between the 1.2 and 10.x world.xml) is queried in both frames ("10x" = the position as given,
"12" = shifted by the origin difference). The terrain heights are then produced by the ignored
test `dump_terrain_heights` (systems/npcs/heightmap.rs) and fed to `import_spawns.py`.
"""
import json
import sys


def main(src, out):
    cands = json.load(open(src + "/candidates.json", encoding="utf-8"))
    queries = []
    for kind, key in (("npc", "npcs"), ("doodad", "doodads")):
        for uid, entry in cands[key].items():
            for i, c in enumerate(entry["candidates"]):
                if c.get("world_in_repo") is False:
                    continue
                frames = {"as": (c["x"], c["y"])}
                o12, o10 = c.get("zone_origin_1_2"), c.get("zone_origin_10x")
                if o12 and o10 and o12 != o10:
                    dx, dy = (o12[0] - o10[0]) * 1024, (o12[1] - o10[1]) * 1024
                    frames["12"] = (c["x"] + dx, c["y"] + dy)
                for frame, (x, y) in frames.items():
                    queries.append(
                        {"id": f"{kind}:{uid}:{i}:{frame}", "world": c["world"], "x": x, "y": y}
                    )
    json.dump(queries, open(out, "w"))
    print(len(queries), "queries")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
