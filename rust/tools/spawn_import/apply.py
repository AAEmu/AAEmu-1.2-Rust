"""Step 3: append the accepted candidates of `evaluate.py` to the world spawn files.

    python -I apply.py <decisions.json> <Data/Worlds dir> [--dry-run]

* Editor-tier candidates (1.2 spawner rows placed by the 10.x editor files, 10.x doodad cells,
  sphere centres) win over git-history-only positions: git positions are only added for an id that
  has no accepted editor-tier position in that world.
* An entry is skipped when the file already holds the same UnitId within 2 m (the loader's own
  duplicate rule for NPCs; doodads use 0.01 m, but 2 m keeps re-runs idempotent).
* Entries are appended in the style of the file (indent, CRLF) with a short `Title` naming the
  basis; the loader ignores unknown/extra fields.
"""
import collections
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from lib import load_spawn_file, tolerant_json  # noqa: E402

DUP = 2.0


def fmt(v):
    r = round(float(v), 3)
    return repr(int(r)) if r == int(r) else repr(r)


def render(entry, ind, unit, nl):
    inner, deep = ind + unit, ind + unit + unit
    lines = [
        ind + "{",
        f'{inner}"UnitId": {entry["id"]},',
        f'{inner}"Title": {json.dumps(entry["title"])},',
        inner + '"Position": {',
        f'{deep}"X": {fmt(entry["x"])},',
        f'{deep}"Y": {fmt(entry["y"])},',
        f'{deep}"Z": {fmt(entry["z"])},',
        f'{deep}"Yaw": {fmt(entry["yaw"])}',
        inner + "}",
        ind + "}",
    ]
    return nl.join(lines)


def append_entries(path, entries):
    raw = open(path, "rb").read().decode("utf-8")
    nl = "\n" if ("\n" in raw and "\r\n" not in raw) else "\r\n"  # the repo files are CRLF
    m = re.search(r'\n([ \t]*)\{', raw)
    ind = m.group(1) if m else "  "
    m2 = re.search(r'\n([ \t]*)"(?:UnitId|Id)"', raw)
    unit = m2.group(1)[len(ind):] if m2 and m2.group(1).startswith(ind) and len(m2.group(1)) > len(ind) else "  "
    end = raw.rindex("]")
    head = raw[:end].rstrip()
    body = ("," + nl).join(render(e, ind, unit, nl) for e in entries)
    sep = nl if head.endswith("[") else ("," if not head.endswith(",") else "") + nl
    out = head + sep + body + nl + raw[end:]
    tolerant_json(out)  # must stay parseable
    open(path, "wb").write(out.encode("utf-8"))


def main(dec_path, worlds_dir, dry):
    dec = json.load(open(dec_path))
    chosen = collections.defaultdict(list)  # (world, kind) -> entries
    editor_ids = {(d["world"], d["kind"], d["id"]) for d in dec if d["accepted"] and d["tier"] == "editor"}
    for d in dec:
        if not d["accepted"]:
            continue
        if d["tier"] == "git" and (d["world"], d["kind"], d["id"]) in editor_ids:
            continue
        ev = d["chosen"]
        title = "spawn-import: %s %s" % (d["basis"], d["confidence"])
        chosen[(d["world"], d["kind"])].append(
            {"id": d["id"], "x": ev["x"], "y": ev["y"], "z": d["z"], "yaw": d["yaw"], "title": title}
        )
    summary = {}
    for (world, kind), entries in sorted(chosen.items()):
        fname = "npc_spawns.json" if kind == "npc" else "doodad_spawns.json"
        path = os.path.join(worlds_dir, world, fname)
        if not os.path.isdir(os.path.join(worlds_dir, world)):
            summary[f"{world}/{kind}"] = "no world dir"
            continue
        existing = collections.defaultdict(list)
        for uid, x, y, z in load_spawn_file(path):
            existing[uid].append((x, y, z))
        new = []
        for e in sorted(entries, key=lambda e: e["id"]):
            if any(abs(x - e["x"]) < DUP and abs(y - e["y"]) < DUP and abs(z - e["z"]) < DUP for x, y, z in existing[e["id"]]):
                continue
            existing[e["id"]].append((e["x"], e["y"], e["z"]))
            new.append(e)
        summary[f"{world}/{kind}"] = {"entries": len(new), "ids": len({e["id"] for e in new})}
        if new and not dry:
            append_entries(path, new)
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2], "--dry-run" in sys.argv)
