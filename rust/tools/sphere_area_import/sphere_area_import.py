#!/usr/bin/env python3
"""Convert the 10.x `world_server/quest_area_sphere.g` areas into committed 1.2 data.

The 1.2 client pack has no `quest_area_sphere` file (only the per-zone `quest_sign_sphere.g`
used by the quest sign-sphere triggers). The 10.x pack has one
`game/worlds/<world>/level_design/zone/<zone>/world_server/quest_area_sphere.g` per zone with
blocks of the form

    area
        kind 1
        stype 2245
        pos ( x 2679.69, y 3384.23, z 132.562 )
        radius 15

`stype` is the `spheres.id` / `sphere_detail_id` numbering of the 1.2 compact `spheres` table.
The output is one `<out>/<world>/sphere_area_fill.json` per 1.2 world with the kept blocks:

    [{"sphere_id": 2245, "zone": 141, "x": 2679.69, "y": 3384.23, "z": 132.562, "radius": 15.0}]

Data-version guard (the 10.x world is newer than 1.2): a block is kept only when

  * its world exists in the 1.2 client worlds (`game/worlds/<world>/world.xml`),
  * its zone key exists in that 1.2 `world.xml`,
  * its `stype` exists in the 1.2 `compact.sqlite3` `spheres` table,

and it is not an exact duplicate of an earlier kept block of the same world. Dropped blocks are
reported per reason. A block with a malformed missing field is `invalid_block`.

The 10.x and 1.2 packs are read directly (AAPak Type A, see `../opcode_coverage/gamepak.py`);
an already extracted 1.2 client tree can be given with `--client12` instead of `--pak12`.
The pack itself is never a runtime input: only the written JSON is committed.

Owner approval for 10.x-derived committed data: 2026-11-05 (same policy as
`doodad_spawns_quest_fill.json`).

Usage:
  python -I sphere_area_import.py --pak10 <10.x game_pak> --pak12 <1.2 game_pak> \
      --compact <1.2 compact.sqlite3> --out <app/Data/Worlds> [--dry-run]
  python -I sphere_area_import.py ... --client12 <extracted 1.2 client root> ...
"""

from __future__ import annotations

import argparse
import json
import re
import sqlite3
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "opcode_coverage"))
from gamepak import Pak  # noqa: E402

AREA_RE = re.compile(
    r"^game/worlds/([^/]+)/level_design/zone/(\d+)/world_server/quest_area_sphere\.g$",
    re.I,
)
POS_RE = re.compile(
    r"^pos\s*\(\s*x\s*([-+0-9.eE]+)\s*,\s*y\s*([-+0-9.eE]+)\s*,\s*z\s*([-+0-9.eE]+)\s*\)$",
    re.I,
)

DROP_REASONS = (
    "world_not_in_1_2",
    "zone_not_in_1_2",
    "sphere_not_in_1_2",
    "invalid_block",
    "duplicate",
)


def parse_area_spheres(text: str) -> list[dict]:
    """The `area / kind / stype / pos / radius` blocks of one file.

    Unknown keys and the `__none__` filler lines are ignored; a block missing `stype`, `pos`
    or `radius` is returned with the missing fields absent (the caller drops it as invalid).
    """
    blocks: list[dict] = []
    cur: dict | None = None
    for raw in text.splitlines():
        line = raw.strip().lower()
        if not line:
            continue
        if line == "area":
            if cur is not None:
                blocks.append(cur)
            cur = {}
            continue
        if cur is None:
            continue
        if line.startswith("stype "):
            try:
                cur["stype"] = int(line[len("stype ") :].strip())
            except ValueError:
                pass
        elif line.startswith("kind "):
            try:
                cur["kind"] = int(line[len("kind ") :].strip())
            except ValueError:
                pass
        elif line.startswith("radius "):
            try:
                cur["radius"] = float(line[len("radius ") :].strip())
            except ValueError:
                pass
        elif line.startswith("pos"):
            m = POS_RE.match(line)
            if m:
                cur["pos"] = tuple(float(v) for v in m.groups())
    if cur is not None:
        blocks.append(cur)
    return blocks


def pak_index(pak: Pak) -> dict[str, tuple[int, int]]:
    """Lower-cased entry name -> `(offset, size)`; `Pak.read` scans all 200k+ entries."""
    return {name.lower(): (off, size) for name, off, size in pak.entries}


def pak_read(pak: Pak, index: dict[str, tuple[int, int]], name: str) -> bytes:
    off, size = index[name.lower()]
    pak.f.seek(off)
    return pak.f.read(size)


def parse_zone_keys(data: bytes) -> set[int]:
    """Zone keys of a 1.2 `world.xml` (`<Zone .. id="129" ..>`)."""
    try:
        root = ET.fromstring(data)
    except ET.ParseError:
        return {int(z) for z in re.findall(rb'<Zone\b[^>]*\bid="(\d+)"', data)}
    return {
        int(zone.get("id", "0"))
        for zone in root.iter()
        if zone.tag == "Zone" and zone.get("id")
    }


def load_12_zone_keys(
    pak12: Pak | None,
    index12: dict[str, tuple[int, int]] | None,
    client12: Path | None,
) -> dict[str, set[int]]:
    """`lower-cased world directory name -> zone keys` from the 1.2 `world.xml` files."""
    worlds: dict[str, set[int]] = {}
    if pak12 is not None and index12 is not None:
        for name in index12:
            parts = name.split("/")
            if len(parts) == 4 and parts[0] == "game" and parts[1] == "worlds" and parts[3] == "world.xml":
                worlds[parts[2]] = parse_zone_keys(pak_read(pak12, index12, name))
    else:
        assert client12 is not None
        for path in sorted(client12.glob("game/worlds/*/world.xml")):
            worlds[path.parent.name.lower()] = parse_zone_keys(path.read_bytes())
    return worlds


def load_sphere_ids(compact: Path) -> set[int]:
    conn = sqlite3.connect(f"file:{compact}?mode=ro", uri=True)
    try:
        return {int(r[0]) for r in conn.execute("SELECT id FROM spheres")}
    finally:
        conn.close()


def iter_10x_blocks(pak10: Pak, index10: dict[str, tuple[int, int]]):
    """Yields `(world, zone, block)` for every 10.x area file, in file order."""
    names = sorted(
        (n for n in index10 if AREA_RE.match(n)),
        key=lambda n: n,
    )
    for name in names:
        m = AREA_RE.match(name)
        assert m is not None
        world = m.group(1).lower()
        zone = int(m.group(2))
        text = pak_read(pak10, index10, name).decode("utf-8-sig", errors="replace")
        for block in parse_area_spheres(text):
            yield world, zone, block


def convert(
    pak10: Pak,
    index10: dict[str, tuple[int, int]],
    pak12: Pak | None,
    index12: dict[str, tuple[int, int]] | None,
    client12: Path | None,
    compact: Path,
) -> tuple[dict[str, list[dict]], dict[str, int], dict[str, dict[str, int]]]:
    """Returns `(kept by world, drops by reason, drops by reason per world)`."""
    zone_keys = load_12_zone_keys(pak12, index12, client12)
    sphere_ids = load_sphere_ids(compact)
    kept: dict[str, list[dict]] = {}
    drops = {reason: 0 for reason in DROP_REASONS}
    drops_by_world: dict[str, dict[str, int]] = {}
    seen: dict[str, set[tuple]] = {}

    for world, zone, block in iter_10x_blocks(pak10, index10):
        world_drops = drops_by_world.setdefault(
            world, {reason: 0 for reason in DROP_REASONS}
        )

        def drop(reason: str) -> None:
            drops[reason] += 1
            world_drops[reason] += 1

        if world not in zone_keys:
            drop("world_not_in_1_2")
            continue
        if zone not in zone_keys[world]:
            drop("zone_not_in_1_2")
            continue
        if not {"stype", "pos", "radius"} <= block.keys() or block["radius"] <= 0:
            drop("invalid_block")
            continue
        sphere_id = block["stype"]
        if sphere_id not in sphere_ids:
            drop("sphere_not_in_1_2")
            continue
        x, y, z = block["pos"]
        radius = block["radius"]
        key = (sphere_id, round(x, 3), round(y, 3), round(z, 3), round(radius, 3))
        if key in seen.setdefault(world, set()):
            drop("duplicate")
            continue
        seen[world].add(key)
        kept.setdefault(world, []).append(
            {
                "sphere_id": sphere_id,
                "zone": zone,
                "x": x,
                "y": y,
                "z": z,
                "radius": radius,
            }
        )

    for blocks in kept.values():
        blocks.sort(key=lambda b: (b["zone"], b["sphere_id"], b["x"], b["y"], b["z"]))
    return kept, drops, drops_by_world


def write_files(kept: dict[str, list[dict]], out: Path) -> None:
    for world, blocks in sorted(kept.items()):
        dest = out / world / "sphere_area_fill.json"
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(
            json.dumps(blocks, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
        )
        print(f"{dest}: {len(blocks)} areas")


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--pak10", required=True, type=Path, help="10.x game_pak")
    group = ap.add_mutually_exclusive_group(required=True)
    group.add_argument("--pak12", type=Path, help="1.2 game_pak")
    group.add_argument(
        "--client12", type=Path, help="extracted 1.2 client root (holds game/worlds)"
    )
    ap.add_argument("--compact", required=True, type=Path, help="1.2 compact.sqlite3")
    ap.add_argument("--out", required=True, type=Path, help="Data/Worlds output directory")
    ap.add_argument("--dry-run", action="store_true", help="report counts, write nothing")
    args = ap.parse_args(argv)

    pak10 = Pak(args.pak10)
    index10 = pak_index(pak10)
    if args.pak12:
        pak12 = Pak(args.pak12)
        index12 = pak_index(pak12)
    else:
        pak12, index12 = None, None
    kept, drops, drops_by_world = convert(
        pak10, index10, pak12, index12, args.client12, args.compact
    )

    total_kept = sum(len(v) for v in kept.values())
    print(f"kept {total_kept} areas in {len(kept)} worlds")
    for reason in DROP_REASONS:
        print(f"dropped {reason}: {drops[reason]}")
    for world in sorted(drops_by_world):
        per = drops_by_world[world]
        if any(per.values()):
            detail = ", ".join(f"{r} {per[r]}" for r in DROP_REASONS if per[r])
            print(f"  {world}: {detail}")
    if not args.dry_run:
        write_files(kept, args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
