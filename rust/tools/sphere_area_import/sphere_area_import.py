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
`pos` is zone-local, not world: the 10.x `quest_area_sphere.g` positions are in the same
zone-local frame as the 1.2 `quest_sign_sphere.g` positions (662 same-quest pairs in the same
zone agree; 165 of them within 5 m). The committed output holds WORLD coordinates:

    [{"sphere_id": 2245, "zone": 141, "x": 11851.69, "y": 7192.23, "z": 132.562, "radius": 15.0}]

with `x = source_zone_origin_x * 1024 + local_x`, `y = source_zone_origin_y * 1024 + local_y`
and `z` unchanged: exactly the formula the 1.2 sign-sphere loader applies
(`systems/quests/spheres.rs`: zone origin cell * 1024 + local). The origin cell comes from the
10.x `world.xml` of the zone, because the 10.x file is anchored at the 10.x origin. That differs
from the 1.2 origin in 10 zones (e.g. zones 192 and 193 gained a cell row in 10.x; using the 1.2
origin would move their spheres a cell south, outside the 1.2 zone). The 1.2 `world.xml`
supplies the target zone rectangle.

Data-version guard (the 10.x world is newer than 1.2): a block is kept only when

  * its world exists in the 1.2 client worlds (`game/worlds/<world>/world.xml`),
  * its zone key exists in that 1.2 `world.xml`,
  * its `stype` exists in the 1.2 `compact.sqlite3` `spheres` table,
  * the converted world position lies inside the 1.2 zone's cell rectangle
    (`position_outside_zone` otherwise: a 10.x zone may own cells the 1.2 zone does not),

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
from dataclasses import dataclass
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

#: `WorldManager.CELL_SIZE`: the zone origin cell is this many metres wide/tall.
CELL_SIZE = 1024

DROP_REASONS = (
    "world_not_in_1_2",
    "zone_not_in_1_2",
    "zone_frame_missing",
    "position_outside_zone",
    "sphere_not_in_1_2",
    "invalid_block",
    "duplicate",
)


@dataclass(frozen=True)
class ZoneFrame:
    """One `world.xml` `<Zone>`: origin cell and the cells it owns."""

    origin_x: int
    origin_y: int
    cells: tuple[tuple[int, int], ...] = ()

    def to_world(self, x: float, y: float) -> tuple[float, float]:
        """Zone-local (x, y) plus the origin cell; the caller keeps z as is."""
        return (self.origin_x * CELL_SIZE + x, self.origin_y * CELL_SIZE + y)

    def contains_world(self, x: float, y: float) -> bool:
        """Is world (x, y) inside the zone's cell rectangle? A zone without cells accepts."""
        if not self.cells:
            return True
        min_x = min(c[0] for c in self.cells)
        min_y = min(c[1] for c in self.cells)
        max_x = max(c[0] for c in self.cells)
        max_y = max(c[1] for c in self.cells)
        return (
            min_x * CELL_SIZE <= x <= (max_x + 1) * CELL_SIZE
            and min_y * CELL_SIZE <= y <= (max_y + 1) * CELL_SIZE
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


def parse_zone_frames(data: bytes) -> dict[int, ZoneFrame]:
    """Zone key -> origin cell and cells of a `world.xml`."""
    try:
        root = ET.fromstring(data)
    except ET.ParseError:
        out: dict[int, ZoneFrame] = {}
        for m in re.finditer(rb"<Zone\b([^>]*)>(.*?)</Zone>", data, re.S):
            attrs = {
                k.decode(): v.decode()
                for k, v in re.findall(rb'(\w+)="([^"]*)"', m.group(1))
            }
            if "id" not in attrs:
                continue
            cells = tuple(
                (int(x), int(y))
                for x, y in re.findall(rb'<cell\s+x="(\d+)"\s+y="(\d+)"', m.group(2))
            )
            out[int(attrs["id"])] = ZoneFrame(
                int(attrs.get("originX", "0")),
                int(attrs.get("originY", "0")),
                cells,
            )
        return out
    out = {}
    for zone in root.iter():
        if zone.tag != "Zone" or not zone.get("id"):
            continue
        cells = tuple(
            (int(cell.get("x", "0")), int(cell.get("y", "0")))
            for cell in zone.iter()
            if cell.tag == "cell"
        )
        out[int(zone.get("id", "0"))] = ZoneFrame(
            int(zone.get("originX", "0")),
            int(zone.get("originY", "0")),
            cells,
        )
    return out


def load_world_frames(
    pak: Pak | None,
    index: dict[str, tuple[int, int]] | None,
    client: Path | None,
) -> dict[str, dict[int, ZoneFrame]]:
    """`lower-cased world directory name -> zone frames` from the `world.xml` files."""
    worlds: dict[str, dict[int, ZoneFrame]] = {}
    if pak is not None and index is not None:
        for name in index:
            parts = name.split("/")
            if len(parts) == 4 and parts[0] == "game" and parts[1] == "worlds" and parts[3] == "world.xml":
                worlds[parts[2]] = parse_zone_frames(pak_read(pak, index, name))
    else:
        assert client is not None
        for path in sorted(client.glob("game/worlds/*/world.xml")):
            worlds[path.parent.name.lower()] = parse_zone_frames(path.read_bytes())
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
    zones12 = load_world_frames(pak12, index12, client12)
    # The 10.x `pos` is anchored at the 10.x zone origin cell. That frame is the source of the
    # local coordinates (the 1.2 origin differs for some zones, e.g. 192/193).
    zones10 = load_world_frames(pak10, index10, None)
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

        if world not in zones12:
            drop("world_not_in_1_2")
            continue
        target = zones12[world].get(zone)
        if target is None:
            drop("zone_not_in_1_2")
            continue
        if not {"stype", "pos", "radius"} <= block.keys() or block["radius"] <= 0:
            drop("invalid_block")
            continue
        sphere_id = block["stype"]
        if sphere_id not in sphere_ids:
            drop("sphere_not_in_1_2")
            continue
        frame = zones10.get(world, {}).get(zone)
        if frame is None:
            drop("zone_frame_missing")
            continue
        local_x, local_y, z = block["pos"]
        x, y = frame.to_world(local_x, local_y)
        if not target.contains_world(x, y):
            drop("position_outside_zone")
            continue
        # The source has at most 5 decimals; adding the integer origin cell would otherwise
        # print binary-float noise (15981.119999999999).
        x, y = round(x, 5), round(y, 5)
        radius = block["radius"]
        key = (zone, sphere_id, round(x, 5), round(y, 5), round(z, 5), round(radius, 3))
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
