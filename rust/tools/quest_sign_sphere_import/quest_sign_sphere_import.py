#!/usr/bin/env python3
"""Import the missing 1.2 quest sign-sphere geometry of the WP-QU4 quests from the 10.x pak.

The 1.2 client pack has no `quest_sign_sphere.g` area for 21 objective-sphere acts of 19
quests (`Docs/wiki/Quest-Audit.md`). The 10.x pak still carries the old geometry in two files
per zone:

    game/worlds/<world>/level_design/zone/<zone>/client/quest_sign_sphere.g
        area
            qtype <quest id>
            ctype <component id>
            pos ( x ..., y ..., z ... )
            radius ...

    game/worlds/<world>/level_design/zone/<zone>/world_server/quest_area_sphere.g
        area
            kind 1
            stype <10.x spheres.id>
            pos ( x ..., y ..., z ... )
            radius ...

Both file kinds are zone-local (CryEngine level coordinates); the 10.x `quest_sign_sphere.g`
positions were checked against the 1.2 `mission_mission0.xml` `AreaSphere` entities and agree
where both exist. The committed output holds WORLD coordinates, `origin * 1024 + local` (z as
is), exactly the formula the 1.2 sign-sphere loader applies (`systems/quests/spheres.rs`). The
origin comes from the 10.x `world.xml` of the source file; for the zones kept below it equals
the 1.2 origin.

Data-version guard (the 10.x world is newer than 1.2). A block is kept only when

  * it belongs to one of the 19 WP-QU4 quests and to an objective sphere act
    (`QuestActObjSphere` / `QuestActCheckSphere`) that exists in the 1.2 compact data, and the
    1.2 act references the same `spheres.id` (component agreement);
  * the 10.x and 1.2 `spheres` / `sphere_quests` rows name the same detail id and the same
    quest id (`sphere_mapping_mismatch` otherwise; e.g. 10.x reused sphere 945 for quest 5804
    while 1.2 uses it for quest 1033);
  * the block's world and zone exist in the 1.2 client `world.xml` files and the 1.2 `zones`
    row is open (`zones.closed = 'f'`);
  * a 1.2 client sign sphere for the same (quest id, component id) does not exist (never
    override the client pack);
  * when the 1.2 `mission_mission0.xml` of the block's zone has an `AreaSphere` entity with the
    same `value1` (= `spheres.id`) within 1.5 m, its radius agrees (`radius_mismatch`
    otherwise). Without such a 1.2 entity the 10.x authored radius is the only value; the entry
    records `"radius_source": "10x_area"` or `"10x_client"` instead of pretending to have a
    1.2 check;
  * for a main_world quest whose `quest_contexts.zone_id` maps to a zone key, blocks of the
    same (quest id, component id) outside that zone are dropped (`zone_mismatch`, e.g. sphere
    933 exists in zones 159 and 186 and quest 5716 is authored in zone 186).

The output is one `<out>/<world>/quest_sign_sphere_fill.json` per 1.2 world with the kept
blocks in world coordinates (`x = origin_x * 1024 + local_x`, `y` likewise, z unchanged):

    [{"quest_id": 1650, "component_id": 7764, "sphere_id": 650, "zone": 142,
      "x": 14253.21, "y": 14999.57, "z": 121.855, "radius": 6.0,
      "source": "quest_area_sphere.g", "radius_source": "10x_area"}]

The 10.x pak and server database are read-only inputs and are never a runtime input: only the
written JSON is committed. Owner approval for 10.x-derived committed data: 2026-11-06 (same
policy as `doodad_spawns_quest_fill.json` and `sphere_area_fill.json`).

Usage:
  python -I quest_sign_sphere_import.py --pak10 <10.x game_pak> \
      --db10 <10.x server_decrypted.sqlite3> --compact <1.2 compact.sqlite3> \
      --client12 <extracted 1.2 client root> --out <app/Data/Worlds> [--dry-run]
  python -I quest_sign_sphere_import.py --extracted10 <10.x extracted root> ... 
"""

from __future__ import annotations

import argparse
import json
import re
import sqlite3
import sys
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "opcode_coverage"))
from gamepak import Pak  # noqa: E402

#: The `Docs/wiki/Quest-Audit.md` 19 quests whose objective sphere has no 1.2 client geometry.
TARGET_QUESTS = frozenset(
    {
        1012, 1024, 1033, 1039, 1372, 1392, 1398, 1400, 1650, 1897,
        5272, 5439, 5716, 5898, 5979, 6039, 6050, 6213, 6216,
    }
)

OBJECTIVE_ACTS = ("QuestActObjSphere", "QuestActCheckSphere")

AREA_RE = re.compile(
    r"^game/worlds/([^/]+)/level_design/zone/(\d+)/world_server/quest_area_sphere\.g$",
    re.I,
)
SIGN_RE = re.compile(
    r"^game/worlds/([^/]+)/level_design/zone/(\d+)/client/quest_sign_sphere\.g$",
    re.I,
)
POS_RE = re.compile(
    r"^pos\s*\(\s*x\s*([-+0-9.eE]+)\s*,\s*y\s*([-+0-9.eE]+)\s*,\s*z\s*([-+0-9.eE]+)\s*\)$",
    re.I,
)
MISSION_RADIUS_TOLERANCE = 0.11
MISSION_POSITION_TOLERANCE = 1.5
#: `WorldManager.CELL_SIZE`: the zone origin cell is this many metres wide/tall.
CELL_SIZE = 1024

DROP_REASONS = (
    "invalid_block",
    "sphere_not_in_1_2",
    "sphere_not_quest",
    "sphere_mapping_mismatch",
    "quest_not_target",
    "component_not_in_1_2",
    "zone_not_in_1_2",
    "zone_closed",
    "client_sphere_exists",
    "radius_mismatch",
    "zone_mismatch",
    "duplicate",
)


# ---------------------------------------------------------------------------------------------
# Parsers
# ---------------------------------------------------------------------------------------------

@dataclass
class Block:
    """One `area` block of a 10.x quest sphere file."""

    source: str
    world: str
    zone: int
    xyz: tuple[float, float, float]
    radius: float
    stype: int | None = None
    quest_id: int | None = None
    component_id: int | None = None

    def key(self) -> tuple:
        return (
            self.quest_id,
            self.component_id,
            self.world,
            self.zone,
            round(self.xyz[0], 3),
            round(self.xyz[1], 3),
            round(self.xyz[2], 3),
            round(self.radius, 3),
        )


def parse_area_spheres(text: str) -> list[dict]:
    """`area / kind / stype / pos / radius` blocks (missing fields stay absent)."""
    return _parse_blocks(text, "stype")


def parse_quest_sign_spheres(text: str) -> list[dict]:
    """`area / qtype / ctype / pos / radius` blocks (missing fields stay absent)."""
    return _parse_blocks(text, "qtype")


def _parse_blocks(text: str, id_key: str) -> list[dict]:
    blocks: list[dict] = []
    cur: dict | None = None
    for raw in text.splitlines():
        line = raw.strip().lower()
        if line == "area":
            if cur is not None:
                blocks.append(cur)
            cur = {}
            continue
        if cur is None or not line:
            continue
        if line.startswith(id_key + " "):
            try:
                cur[id_key] = int(line[len(id_key) + 1 :].strip())
            except ValueError:
                pass
        elif line.startswith("ctype "):
            try:
                cur["ctype"] = int(line[len("ctype ") :].strip())
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


def block_from(parsed: dict, source: str, world: str, zone: int) -> Block | None:
    if "pos" not in parsed or "radius" not in parsed:
        return None
    radius = parsed["radius"]
    if not (radius > 0 and radius < 1000):
        return None
    if source == "quest_area_sphere.g":
        if "stype" not in parsed:
            return None
        return Block(source, world, zone, parsed["pos"], radius, stype=parsed["stype"])
    if "qtype" not in parsed or "ctype" not in parsed:
        return None
    return Block(
        source,
        world,
        zone,
        parsed["pos"],
        radius,
        quest_id=parsed["qtype"],
        component_id=parsed["ctype"],
    )


# ---------------------------------------------------------------------------------------------
# 1.2 / 10.x data
# ---------------------------------------------------------------------------------------------

def parse_zone_keys(data: bytes) -> dict[int, tuple[int, int]]:
    """Zone key -> (origin x, origin y) of a `world.xml`."""
    try:
        root = ET.fromstring(data)
    except ET.ParseError:
        return {
            int(z): (int(ox), int(oy))
            for z, ox, oy in re.findall(
                rb'<Zone\b[^>]*\bid="(\d+)"[^>]*\boriginX="(-?\d+)"[^>]*\boriginY="(-?\d+)"',
                data,
            )
        }
    out = {}
    for zone in root.iter():
        if zone.tag == "Zone" and zone.get("id"):
            out[int(zone.get("id", "0"))] = (
                int(zone.get("originX", "0")),
                int(zone.get("originY", "0")),
            )
    return out


def load_worlds_12(client12: Path) -> dict[str, dict[int, tuple[int, int]]]:
    worlds: dict[str, dict[int, tuple[int, int]]] = {}
    for path in sorted(client12.glob("game/worlds/*/world.xml")):
        worlds[path.parent.name.lower()] = parse_zone_keys(path.read_bytes())
    return worlds


def load_worlds_10(
    pak10: Pak | None,
    index10: dict[str, tuple[int, int]] | None,
    extracted10: Path | None,
) -> dict[str, dict[int, tuple[int, int]]]:
    """`lower-cased world directory name -> (origin x, origin y)` from the 10.x world.xml."""
    worlds: dict[str, dict[int, tuple[int, int]]] = {}
    if extracted10 is not None:
        base = extracted10 / "game" / "worlds"
        if not base.is_dir():
            base = extracted10 / "worlds"
        for path in sorted(base.glob("*/world.xml")):
            worlds[path.parent.name.lower()] = parse_zone_keys(path.read_bytes())
        return worlds
    assert pak10 is not None and index10 is not None
    for name in index10:
        parts = name.split("/")
        if len(parts) == 4 and parts[0] == "game" and parts[1] == "worlds" and parts[3] == "world.xml":
            worlds[parts[2]] = parse_zone_keys(pak_read(pak10, index10, name))
    return worlds


def read_pak_index(pak: Pak) -> dict[str, tuple[int, int]]:
    return {name.lower(): (off, size) for name, off, size in pak.entries}


def pak_read(pak: Pak, index: dict[str, tuple[int, int]], name: str) -> bytes:
    off, size = index[name.lower()]
    pak.f.seek(off)
    return pak.f.read(size)


@dataclass
class Data12:
    zones: dict[int, tuple[int, str]] = field(default_factory=dict)  # zone key -> (id, closed)
    quest_zone: dict[int, int] = field(default_factory=dict)  # quest -> zones.id
    sphere: dict[int, tuple[int, str]] = field(default_factory=dict)  # id -> (detail, type)
    sphere_quest: dict[int, int] = field(default_factory=dict)  # detail -> quest
    acts: dict[tuple[int, int], list[int]] = field(default_factory=dict)  # (q,c) -> sphere ids
    client_spheres: dict[tuple[int, int], list[dict]] = field(default_factory=dict)


def load_12(compact: Path, client12: Path) -> Data12:
    d = Data12()
    c = sqlite3.connect(f"file:{compact}?mode=ro", uri=True)
    try:
        for zone_key, zone_id, closed in c.execute(
            "SELECT zone_key, id, closed FROM zones"
        ):
            d.zones[int(zone_key)] = (int(zone_id), str(closed))
        for q, z in c.execute("SELECT id, zone_id FROM quest_contexts"):
            d.quest_zone[int(q)] = int(z)
        for i, detail, typ in c.execute(
            "SELECT id, sphere_detail_id, sphere_detail_type FROM spheres"
        ):
            d.sphere[int(i)] = (int(detail), str(typ))
        for i, q in c.execute("SELECT id, quest_id FROM sphere_quests"):
            d.sphere_quest[int(i)] = int(q)
        for act in OBJECTIVE_ACTS:
            table = (
                "quest_act_obj_spheres"
                if act == "QuestActObjSphere"
                else "quest_act_check_spheres"
            )
            for q, comp, sid in c.execute(
                f"""SELECT qc.quest_context_id, qa.quest_component_id, d.sphere_id
                    FROM quest_acts qa
                    JOIN quest_components qc ON qc.id = qa.quest_component_id
                    JOIN {table} d ON d.id = qa.act_detail_id
                    WHERE qa.act_detail_type = ?""",
                (act,),
            ):
                d.acts.setdefault((int(q), int(comp)), []).append(int(sid))
    finally:
        c.close()
    # 1.2 client sign spheres, keyed by (quest, component): never override these.
    for path in sorted(client12.glob("game/worlds/*/level_design/zone/*/client/quest_sign_sphere.g")):
        zone = int(path.parent.parent.name)
        for raw in parse_quest_sign_spheres(path.read_text(encoding="utf-8-sig", errors="replace")):
            if "qtype" in raw and "ctype" in raw and "pos" in raw and "radius" in raw:
                d.client_spheres.setdefault((raw["qtype"], raw["ctype"]), []).append(
                    {"zone": zone, "pos": raw["pos"], "radius": raw["radius"]}
                )
    return d


@dataclass
class Data10:
    sphere: dict[int, tuple[int, str]] = field(default_factory=dict)
    sphere_quest: dict[int, int] = field(default_factory=dict)
    acts: dict[int, list[tuple[int, int]]] = field(default_factory=dict)  # sphere -> (q,c)


def load_10(db10: Path) -> Data10:
    d = Data10()
    c = sqlite3.connect(f"file:{db10}?mode=ro", uri=True)
    try:
        for i, detail, typ in c.execute(
            "SELECT id, sphere_detail_id, sphere_detail_type FROM spheres"
        ):
            d.sphere[int(i)] = (int(detail), str(typ))
        for i, q in c.execute("SELECT id, quest_id FROM sphere_quests"):
            d.sphere_quest[int(i)] = int(q)
        for act, table in (
            ("QuestActObjSphere", "quest_act_obj_spheres"),
            ("QuestActCheckSphere", "quest_act_check_spheres"),
        ):
            for q, comp, sid in c.execute(
                f"""SELECT qc.quest_context_id, qa.quest_component_id, d.sphere_id
                    FROM quest_acts qa
                    JOIN quest_components qc ON qc.id = qa.quest_component_id
                    JOIN {table} d ON d.id = qa.act_detail_id
                    WHERE qa.act_detail_type = ?""",
                (act,),
            ):
                d.acts.setdefault(int(sid), []).append((int(q), int(comp)))
    finally:
        c.close()
    return d


def load_mission_entities(client12: Path) -> dict[tuple[str, int], list[tuple]]:
    """`(world, zone) -> [(pos, value1, radius)]` from the 1.2 `mission_mission0.xml` files."""
    out: dict[tuple[str, int], list[tuple]] = {}
    for path in sorted(client12.glob("game/worlds/*/zone/*/mission_mission0.xml")):
        world = path.parent.parent.parent.name.lower()
        zone = int(path.parent.name)
        text = path.read_text(encoding="utf-8-sig", errors="replace")
        rows = []
        for m in re.finditer(
            r'<Entity\b[^>]*EntityClass="AreaSphere"[^>]*>(.*?)</Entity>', text, re.S
        ):
            body = m.group(0)
            head = body[: body.find(">")]
            pos = re.search(r'Pos="([^"]+)"', head)
            value = re.search(r'value1="(\d+)"', body)
            radius = re.search(r'SphereRadius="([0-9.]+)"', body)
            if pos:
                rows.append(
                    (
                        tuple(float(x) for x in pos.group(1).split(",")),
                        int(value.group(1)) if value else 0,
                        float(radius.group(1)) if radius else 0.0,
                    )
                )
        out[(world, zone)] = rows
    return out


# ---------------------------------------------------------------------------------------------
# Convert
# ---------------------------------------------------------------------------------------------

@dataclass
class Result:
    kept: list[dict] = field(default_factory=list)
    drops: dict[str, int] = field(default_factory=dict)
    kept_by_quest: dict[int, int] = field(default_factory=dict)
    dropped_by_quest: dict[int, dict[str, int]] = field(default_factory=dict)


def _distance(a: tuple[float, float, float], b: tuple[float, float, float]) -> float:
    return sum((x - y) ** 2 for x, y in zip(a, b)) ** 0.5


def plan(
    blocks: list[Block | None],
    d12: Data12,
    d10: Data10,
    worlds: dict[str, dict[int, tuple[int, int]]],
    missions: dict[tuple[str, int], list[tuple]],
    target_quests: frozenset[int] = TARGET_QUESTS,
) -> Result:
    result = Result(drops={r: 0 for r in DROP_REASONS})
    target_spheres = {
        sid for (q, _), ids in d12.acts.items() if q in target_quests for sid in ids
    }

    def drop(quest: int | None, reason: str) -> None:
        result.drops[reason] += 1
        if quest in target_quests:
            per = result.dropped_by_quest.setdefault(quest, {})
            per[reason] = per.get(reason, 0) + 1

    groups: dict[tuple[int, int], list[Block]] = {}
    for block in blocks:
        if block is None:
            drop(None, "invalid_block")
            continue
        if block.source == "quest_sign_sphere.g":
            quest, component = block.quest_id, block.component_id
            sphere_ids = d12.acts.get((quest, component), [])
            if quest not in target_quests:
                continue
            if not sphere_ids:
                drop(quest, "component_not_in_1_2")
                continue
            sphere_id = sphere_ids[0]
            block.stype = sphere_id
        else:
            sphere_id = block.stype
            assert sphere_id is not None
            if sphere_id not in target_spheres:
                continue
            candidates = [
                (q, c)
                for q, c in d10.acts.get(sphere_id, [])
                if q in target_quests and sphere_id in d12.acts.get((q, c), [])
            ]
            if not candidates:
                if not any(
                    sphere_id in ids
                    for (q, _), ids in d12.acts.items()
                    if q in target_quests
                ):
                    drop(None, "sphere_not_in_1_2")
                else:
                    drop(None, "component_not_in_1_2")
                continue
            quest, component = candidates[0]
            block.quest_id, block.component_id = quest, component

        # `spheres` / `sphere_quests` mapping must agree between the two data versions.
        s10 = d10.sphere.get(sphere_id)
        s12 = d12.sphere.get(sphere_id)
        q10 = d10.sphere_quest.get(s10[0]) if s10 else None
        q12 = d12.sphere_quest.get(s12[0]) if s12 else None
        if s12 is None:
            drop(quest, "sphere_not_in_1_2")
            continue
        if s12[1] != "SphereQuest" or (s10 is not None and s10[1] != "SphereQuest"):
            drop(quest, "sphere_not_quest")
            continue
        if s10 is None:
            # The 10.x client sign sphere source need not have a 10.x `spheres` row; the 1.2
            # mapping is still checked below.
            pass
        elif s10 != s12 or q10 != q12:
            drop(quest, "sphere_mapping_mismatch")
            continue
        if q12 != quest:
            drop(quest, "sphere_mapping_mismatch")
            continue

        # Zone checks (world.xml + zones.closed).
        zone_ok = block.world in worlds and block.zone in worlds[block.world]
        zone = d12.zones.get(block.zone)
        if not zone_ok:
            drop(quest, "zone_not_in_1_2")
            continue
        if zone is not None and zone[1] == "t":
            drop(quest, "zone_closed")
            continue
        if zone is None:
            drop(quest, "zone_not_in_1_2")
            continue

        # Never override the 1.2 client pack.
        if (quest, component) in d12.client_spheres:
            drop(quest, "client_sphere_exists")
            continue

        # Radius agreement where the 1.2 client pack has the same sphere.
        radius_ok = True
        for pos, value, radius in missions.get((block.world, block.zone), []):
            if value == sphere_id and _distance(pos, block.xyz) <= MISSION_POSITION_TOLERANCE:
                if abs(radius - block.radius) > MISSION_RADIUS_TOLERANCE:
                    radius_ok = False
        if not radius_ok:
            drop(quest, "radius_mismatch")
            continue
        groups.setdefault((quest, component), []).append(block)

    # Prefer the block in the quest's authored zone when several main_world blocks exist.
    for (quest, component), list_ in groups.items():
        block_zone_id = d12.quest_zone.get(quest)
        quest_zone_key = None
        if block_zone_id is not None:
            for key, (zone_id, _closed) in d12.zones.items():
                if zone_id == block_zone_id:
                    quest_zone_key = key
                    break
        main_blocks = [b for b in list_ if b.world == "main_world"]
        matching = [
            b for b in main_blocks if b.zone == quest_zone_key
        ] if quest_zone_key is not None else []
        if matching and len(list_) > 1:
            for b in list_:
                if b not in matching:
                    drop(quest, "zone_mismatch")
            list_ = matching
        for block in list_:
            entry = {
                "quest_id": block.quest_id,
                "component_id": block.component_id,
                "sphere_id": block.stype,
                "zone": block.zone,
                "x": block.xyz[0],
                "y": block.xyz[1],
                "z": block.xyz[2],
                "radius": block.radius,
                "source": block.source,
                "radius_source": "1_2_mission"
                if any(
                    value == block.stype
                    and _distance(pos, block.xyz) <= MISSION_POSITION_TOLERANCE
                    for pos, value, _r in missions.get((block.world, block.zone), [])
                )
                else ("10x_area" if block.source == "quest_area_sphere.g" else "10x_client"),
            }
            result.kept.append(entry)
            result.kept_by_quest[quest] = result.kept_by_quest.get(quest, 0) + 1

    # Drop exact duplicates (same world/zone/position/radius for a component).
    seen = set()
    unique = []
    for entry in result.kept:
        world = next(
            (
                w
                for w, zones in worlds.items()
                if entry["zone"] in zones
            ),
            "",
        )
        key = (
            entry["quest_id"],
            entry["component_id"],
            world,
            entry["zone"],
            round(entry["x"], 3),
            round(entry["y"], 3),
            round(entry["z"], 3),
            round(entry["radius"], 3),
        )
        if key in seen:
            result.drops["duplicate"] += 1
            per = result.dropped_by_quest.setdefault(entry["quest_id"], {})
            per["duplicate"] = per.get("duplicate", 0) + 1
            continue
        seen.add(key)
        unique.append(entry)
    result.kept = sorted(
        unique,
        key=lambda e: (e["zone"], e["quest_id"], e["component_id"], e["x"], e["y"], e["z"]),
    )
    return result


def collect_blocks(
    pak10: Pak | None,
    index10: dict[str, tuple[int, int]] | None,
    extracted10: Path | None,
) -> tuple[list[Block | None], int, int]:
    blocks: list[Block | None] = []
    files = 0
    missing = 0
    for source, regex in (
        ("quest_area_sphere.g", AREA_RE),
        ("quest_sign_sphere.g", SIGN_RE),
    ):
        if extracted10 is not None:
            base = extracted10 / "game" / "worlds"
            if not base.is_dir():
                base = extracted10 / "worlds"
            names = [
                str(p.relative_to(extracted10)).replace("\\", "/")
                for p in base.rglob("*")
                if p.is_file()
            ]
            names = sorted(n for n in names if regex.match(n))
        else:
            assert pak10 is not None and index10 is not None
            names = sorted(n for n in index10 if regex.match(n))
        for name in names:
            m = regex.match(name)
            if not m:
                continue
            files += 1
            if extracted10 is not None:
                text = (extracted10 / name).read_text(encoding="utf-8-sig", errors="replace")
            else:
                text = pak_read(pak10, index10, name).decode("utf-8-sig", errors="replace")
            parsed = (
                parse_area_spheres(text)
                if source == "quest_area_sphere.g"
                else parse_quest_sign_spheres(text)
            )
            for raw in parsed:
                b = block_from(raw, source, m.group(1).lower(), int(m.group(2)))
                if b is None:
                    missing += 1
                blocks.append(b)
    return blocks, files, missing


def to_world_entry(
    entry: dict,
    world: str,
    worlds10: dict[str, dict[int, tuple[int, int]]],
    worlds12: dict[str, dict[int, tuple[int, int]]],
) -> dict | None:
    """Convert one zone-local entry to world coordinates; `None` without a zone origin."""
    origin = worlds10.get(world, {}).get(entry["zone"])
    if origin is None:
        # Fall back to the 1.2 origin only when the 10.x world.xml has no such zone; the kept
        # zones have identical origins in both versions.
        origin = worlds12.get(world, {}).get(entry["zone"])
    if origin is None:
        return None
    converted = dict(entry)
    converted["x"] = round(origin[0] * CELL_SIZE + entry["x"], 5)
    converted["y"] = round(origin[1] * CELL_SIZE + entry["y"], 5)
    return converted


def write_files(
    result: Result,
    worlds: dict[str, dict[int, tuple[int, int]]],
    worlds10: dict[str, dict[int, tuple[int, int]]],
    out: Path,
) -> None:
    by_world: dict[str, list[dict]] = {}
    for entry in result.kept:
        world = next((w for w, zones in worlds.items() if entry["zone"] in zones), None)
        if world is None:
            continue
        converted = to_world_entry(entry, world, worlds10, worlds)
        if converted is None:
            print(f"WARNING: no zone origin for quest {entry['quest_id']} zone {entry['zone']}")
            continue
        by_world.setdefault(world, []).append(converted)
    for world, entries in sorted(by_world.items()):
        dest = out / world / "quest_sign_sphere_fill.json"
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(json.dumps(entries, indent=2) + "\n", encoding="utf-8")
        print(f"{dest}: {len(entries)} spheres")


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--pak10", type=Path, help="10.x game_pak")
    src.add_argument("--extracted10", type=Path, help="extracted 10.x root (game/worlds)")
    ap.add_argument("--db10", required=True, type=Path, help="10.x server_decrypted.sqlite3")
    ap.add_argument("--compact", required=True, type=Path, help="1.2 compact.sqlite3")
    ap.add_argument("--client12", required=True, type=Path, help="extracted 1.2 client root")
    ap.add_argument("--out", required=True, type=Path, help="Data/Worlds output directory")
    ap.add_argument("--dry-run", action="store_true", help="report counts, write nothing")
    args = ap.parse_args(argv)

    pak10 = index10 = None
    if args.pak10:
        pak10 = Pak(args.pak10)
        index10 = read_pak_index(pak10)
    blocks, files, missing = collect_blocks(pak10, index10, args.extracted10)

    worlds = load_worlds_12(args.client12)
    worlds10 = load_worlds_10(pak10, index10, args.extracted10)
    d12 = load_12(args.compact, args.client12)
    d10 = load_10(args.db10)
    missions = load_mission_entities(args.client12)
    result = plan(blocks, d12, d10, worlds, missions)

    print(f"read {files} 10.x files, {len(blocks)} blocks ({missing} invalid)")
    print(f"kept {len(result.kept)} spheres in {len(result.kept_by_quest)} quests")
    for q in sorted(result.kept_by_quest):
        print(f"  quest {q}: {result.kept_by_quest[q]}")
    for reason in DROP_REASONS:
        print(f"dropped {reason}: {result.drops[reason]}")
    for q in sorted(result.dropped_by_quest):
        detail = ", ".join(
            f"{r} {n}" for r, n in sorted(result.dropped_by_quest[q].items())
        )
        print(f"  quest {q}: {detail}")
    if not args.dry_run:
        write_files(result, worlds, worlds10, args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
