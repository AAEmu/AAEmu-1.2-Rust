#!/usr/bin/env python3
"""Audit/generate vehicle attach transforms from the original 1.2 game_pak only.

Run from any directory (Python 3 + the existing gamepak.py's pycryptodome):
  python -I rust/tools/slave_attach_points.py --compact <compact.sqlite3> \
    --pak D:/AA1.2/game_pak --report <report.json> [--write]
Without --write this is a read-only audit. --write adds missing points in the
checked-in JSON, preserving every existing point and all hand-written comments.
It never extracts assets to disk or writes the client/compact. Reports contain
asset SHA-256s, node names, parent chains, exact unresolved indexes and consumers.

Provenance / conventions (no published CryEngine source was used):
* 2026-10-25 audit F2; baseline 37e6f1aec; earlier 1406 fix a9dc41b8.
* Native 1.2 model_attach_point_strings maps IDs to actor/prefab names; loader
  sub_39678930, D:/aa-re/client12/decomp/x2game12.c:1416646. Prefab bindings
  use '$name'; CHR compiled joints also use 'name'. Only that explicit alias is
  accepted, never a different numbered helper or another model's geometry.
* CGF 0x744/745, Node 0x823: local position +148, XYZW quaternion +160,
  scale +176 after the copied 16-byte header. Conjugate authored quaternion,
  divide positions by 100, compose parents then prefab object transforms.
  Matches the native 1.2-verified collision/cga.rs::nodes/RootAnimation reader.
* CHR CompiledBones 0xACDC0000 v0x801: 48-byte header, 324-byte records,
  name +216 (48 bytes), relative parent index +264, absolute bind Matrix34
  +276. These matrices ALREADY contain parents and use metres. Do not compose
  the parent a second time. Layout checked in the actual 1.2 CHRs against
  sibling CGFs and CDF wheel positions; no 10.x geometry is consulted.
* Prefab XML Pos is metres, Rotate is WXYZ, Scale is XYZ. Direct named prefab
  helpers take precedence over embedded geometry helpers. JSON rotations are
  degrees, Z-yaw/Y-pitch/X-roll, as vehicles/data.rs::parse_attach_points reads.
* Literal native Clipper/Dinghy/1406 regression samples live in the accompanying
  tests. Some old hand-authored JSON rotations are inconsistent (e.g. Clipper
  cannon12); they are preserved, not used to invent a rotation convention.
* Point 0 is None, not a helper. Missing assets/helpers remain explicit gaps;
  no zero placeholders, neighbouring-model substitutions or compact edits.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import math
import re
import sqlite3
import struct
from pathlib import Path
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[2]
DATA = ROOT / "rust/crates/aaemu-game/app/Data/slave_attach_points.json"
AUDIT_MODELS = (2, 4, 5, 6, 115, 127, 129, 130, 392, 512, 857, 866, 867,
                892, 898, 899, 1024, 1287, 1323, 1371, 1385, 1402, 1427,
                1442, 1444, 1460, 1461, 1462)
MAX_ASSET = 32 * 1024 * 1024
MAX_CHUNKS = 8192
MAX_NODES = 4096
IDENTITY = (1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0.)


def load_json(path):
    # Preserve quoted paths/URIs while stripping the data's // comments.
    text = Path(path).read_text(encoding="utf-8")
    return json.loads(re.sub(r'"(?:\\.|[^"\\])*"|//[^\n]*',
                             lambda m: "" if m[0].startswith("//") else m[0], text))


def read_compact(path):
    conn = sqlite3.connect(Path(path).resolve().as_uri() + "?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    return conn


def compose(a, b):
    """Column-vector affine Matrix34: parent * local (including parent scale)."""
    return tuple(sum(a[r*4+k] * b[k*4+c] for k in range(3))
                 + (a[r*4+3] if c == 3 else 0.)
                 for r in range(3) for c in range(4))


def prs(position, xyzw, scale=(1., 1., 1.)):
    if len(position) != 3 or len(xyzw) != 4 or len(scale) != 3 or not all(
            math.isfinite(v) for v in (*position, *xyzw, *scale)):
        raise ValueError("invalid/nonfinite PRS")
    x, y, z, w = xyzw
    norm = math.sqrt(sum(v*v for v in xyzw))
    if not 0.99 <= norm <= 1.01:
        raise ValueError("invalid quaternion")
    x, y, z, w = (v / norm for v in (x, y, z, w))
    r = (1-2*(y*y+z*z), 2*(x*y-z*w), 2*(x*z+y*w),
         2*(x*y+z*w), 1-2*(x*x+z*z), 2*(y*z-x*w),
         2*(x*z-y*w), 2*(y*z+x*w), 1-2*(x*x+y*y))
    return tuple(position[row] if col == 3 else r[row*3+col] * scale[col]
                 for row in range(3) for col in range(4))


def location(matrix):
    if not all(math.isfinite(v) for v in matrix):
        raise ValueError("nonfinite transform")
    lengths = [math.sqrt(sum(matrix[r*4+c]**2 for r in range(3))) for c in range(3)]
    if min(lengths) < 1e-8:
        raise ValueError("singular transform")
    r = [matrix[row*4+c] / lengths[c] for row in range(3) for c in range(3)]
    if any(abs(sum(r[k*3+a]*r[k*3+b] for k in range(3))) > 1e-4
           for a, b in ((0, 1), (0, 2), (1, 2))):
        raise ValueError("sheared transform")
    determinant = (r[0]*(r[4]*r[8]-r[5]*r[7]) - r[1]*(r[3]*r[8]-r[5]*r[6])
                   + r[2]*(r[3]*r[7]-r[4]*r[6]))
    if abs(determinant-1) > 1e-4:
        raise ValueError("reflected transform")
    pitch = math.asin(max(-1., min(1., -r[6])))
    if abs(math.cos(pitch)) > 1e-6:
        roll, yaw = math.atan2(r[7], r[8]), math.atan2(r[3], r[0])
    else:
        roll, yaw = 0., math.atan2(-r[1], r[4])
    values = [matrix[3], matrix[7], matrix[11], *map(math.degrees, (roll, pitch, yaw))]
    return {k: (0. if abs(v) < (1e-7 if i < 3 else 1e-4) else round(v, 8))
            for i, (k, v) in enumerate(zip(("X", "Y", "Z", "Roll", "Pitch", "Yaw"), values))}


def chunks(data):
    if len(data) < 24 or len(data) > MAX_ASSET or data[:6] != b"CryTek":
        raise ValueError("invalid/bounded CryTek file")
    version, table = struct.unpack_from("<II", data, 12)
    if version not in (0x744, 0x745) or table > len(data)-4:
        raise ValueError("unsupported/truncated chunk table")
    count = struct.unpack_from("<I", data, table)[0]
    stride = 16 if version == 0x744 else 20
    if count > MAX_CHUNKS or table+4+count*stride > len(data):
        raise ValueError("bounded/truncated chunk table")
    entries = [struct.unpack_from("<4I" if stride == 16 else "<5I", data, table+4+i*stride)
               for i in range(count)]
    boundaries = sorted({e[2] for e in entries} | {table, len(data)})
    seen = set()
    for e in entries:
        kind, ver, offset, cid = e[:4]
        size = e[4] if stride == 20 else next((x-offset for x in boundaries if x > offset), 0)
        if cid in seen or size < 16 or offset+size > len(data):
            raise ValueError("duplicate/truncated chunk")
        seen.add(cid)
        if kind in (0xCCCC000B, 0xACDC0000):
            if struct.unpack_from("<4I", data, offset) != (kind, ver, offset, cid):
                raise ValueError("chunk header mismatch")
            yield kind, ver, cid, data[offset:offset+size]


def hierarchy(nodes, key, trail=()):
    if key in trail or len(trail) >= 128 or key not in nodes:
        raise ValueError("cyclic/deep/orphan hierarchy")
    name, parent, local = nodes[key]
    if parent is None:
        return local, [name]
    matrix, chain = hierarchy(nodes, parent, trail+(key,))
    return compose(matrix, local), chain+[name]


def geometry(data):
    """Yield (name, native bind matrix, parent chain, record kind)."""
    nodes, bones = {}, []
    for kind, version, cid, raw in chunks(data):
        if kind == 0xCCCC000B:
            if version != 0x823 or len(raw) < 220:
                raise ValueError("unsupported/truncated node")
            body = raw[16:]
            name = body[:64].split(b"\0")[0].decode("ascii")
            parent = struct.unpack_from("<i", body, 68)[0]
            p = tuple(v/100 for v in struct.unpack_from("<3f", body, 148))
            x, y, z, w = struct.unpack_from("<4f", body, 160)
            scale = struct.unpack_from("<3f", body, 176)
            nodes[cid] = (name, None if parent < 0 else parent, prs(p, (-x, -y, -z, w), scale))
        else:
            if version != 0x801 or len(raw) < 48 or (len(raw)-48) % 324:
                raise ValueError("unsupported/truncated compiled bones")
            count = (len(raw)-48)//324
            if count > MAX_NODES:
                raise ValueError("too many bones")
            for i in range(count):
                b = 48+i*324
                name = raw[b+216:b+264].split(b"\0")[0].decode("ascii")
                relative = struct.unpack_from("<i", raw, b+264)[0]
                parent = None if relative == 0 else i+relative
                bones.append((name, parent, struct.unpack_from("<12f", raw, b+276)))
    if len(nodes) > MAX_NODES:
        raise ValueError("too many nodes")
    for cid, (name, _, _) in nodes.items():
        matrix, chain = hierarchy(nodes, cid)
        yield name, matrix, chain, "cgf-local-node"
    for i, (name, parent, matrix) in enumerate(bones):
        # Stored absolute bind matrix, NOT parent-relative. Traverse only for
        # provenance/validation; multiplying these matrices doubles the parents.
        chain, visited, j = [], set(), i
        while j is not None:
            if j in visited or not 0 <= j < len(bones) or len(visited) >= 128:
                raise ValueError("cyclic/deep/orphan bone hierarchy")
            visited.add(j)
            chain.insert(0, bones[j][0])
            j = bones[j][1]
        yield name, matrix, chain, "chr-absolute-bind"


def asset_path(name):
    name = name.replace("\\", "/").lower()
    return name if name.startswith("game/") else "game/"+name


class Extractor:
    def __init__(self, pak, strings):
        self.pak = pak
        self.names = {n.lower(): (n, offset, size) for n, offset, size in pak.entries}
        self.strings = strings
        self.sources, self.issues = {}, []

    def read(self, name):
        key = asset_path(name)
        if key not in self.names:
            raise KeyError(key)
        actual, offset, size = self.names[key]
        if not 0 <= size <= MAX_ASSET or offset < 0 or offset+size > self.pak.length:
            raise ValueError("asset exceeds bounds: "+key)
        self.pak.f.seek(offset)
        data = self.pak.f.read(size)
        if len(data) != size:
            raise ValueError("truncated asset: "+key)
        self.sources[key] = {"path": actual, "sha256": hashlib.sha256(data).hexdigest()}
        return data

    def match(self, name, bone=False):
        n = name.strip().casefold()
        return [point for point, aliases in self.strings.items()
                if n in {a.casefold() for a in aliases}
                or (bone and n in {a.lstrip("$").casefold() for a in aliases})]

    def mesh(self, path, outer=IDENTITY, chain=()):
        key = asset_path(path)
        for name, matrix, parents, kind in geometry(self.read(key)):
            for point in self.match(name, bone=kind == "chr-absolute-bind"):
                yield point, {"location": location(compose(outer, matrix)), "asset": key,
                              "node": name, "parents": list(chain)+parents[:-1], "kind": kind}

    def model(self, uri):
        if not uri.startswith("prefab://"):
            result = {}
            for point, evidence in self.mesh(uri.split("://", 1)[1]):
                if point in result:
                    raise ValueError(f"ambiguous geometry helper {point} in {uri}")
                result[point] = evidence
            return result
        file, prefab = uri[len("prefab://"):].split(".xml/", 1)
        path = asset_path(file+".xml")
        root = ET.fromstring(self.read(path))
        choices = [p for p in root.findall("Prefab") if p.get("Name", "").casefold() == prefab.casefold()]
        if len(choices) != 1:
            raise ValueError("prefab missing/ambiguous: "+uri)
        objects = choices[0].findall("Objects/Object")
        if len(objects) > 512:
            raise ValueError("too many prefab objects")
        nodes, direct, result = {}, {}, {}
        for i, obj in enumerate(objects):
            vec = lambda field, default: tuple(map(float, obj.get(field, default).split(",")))
            w, x, y, z = vec("Rotate", "1,0,0,0")
            key = obj.get("Id", str(i))
            if key in nodes:
                raise ValueError("duplicate prefab object")
            nodes[key] = (obj.get("Name", ""), obj.get("Parent"),
                          prs(vec("Pos", "0,0,0"), (x, y, z, w), vec("Scale", "1,1,1")))
        for i, obj in enumerate(objects):
            outer, parents = hierarchy(nodes, obj.get("Id", str(i)))
            for point in self.match(obj.get("Name", "")):
                if point in direct:
                    raise ValueError("duplicate direct prefab helper")
                direct[point] = {"location": location(outer), "asset": path,
                                 "node": obj.get("Name"), "parents": parents[:-1], "kind": "prefab-helper"}
            model = obj.get("Prefab") if obj.get("Type") == "Brush" else None
            props = obj.find("Properties")
            if props is not None:
                model = props.get("object_Model", model)
            if not model or not model.lower().endswith((".cgf", ".chr", ".cga")):
                continue
            try:
                for point, evidence in self.mesh(model, outer, parents):
                    if point in result:
                        raise ValueError(f"ambiguous geometry helper {point} in {uri}")
                    result[point] = evidence
            except KeyError as e:
                self.issues.append("asset absent: "+str(e.args[0]))
        result.update(direct)
        return result


def requirements(conn):
    """Actual child/healing/equipment consumers (same owner rules as runtime).

    Doodads/healing are loaded by owner_id. Child slaves additionally filter
    owner_type == Slave in finish_create; Housing rows aliasing a slave ID must
    not create spurious requirements (e.g. Housing 119 != Timber Coupe 119).
    """
    result = {}
    for table in ("slave_doodad_bindings", "slave_bindings", "slave_healing_point_doodads", "slave_equip_slots"):
        owner = "slave_id" if table == "slave_equip_slots" else "owner_id"
        for row in conn.execute(f"SELECT s.model_id, s.id slave_id, b.* FROM {table} b JOIN slaves s ON s.id=b.{owner}"):
            if table == "slave_bindings" and row["owner_type"] != "Slave":
                continue
            point = row["attach_point_id"]
            consumer = {"table": table, "row": row["id"], "slave": row["slave_id"]}
            if "owner_type" in row.keys():
                consumer["owner_type"] = row["owner_type"]
            result.setdefault(row["model_id"], {}).setdefault(point, []).append(consumer)
    return result


def normal_uri(conn, mid):
    row = conn.execute("SELECT sub_type, sub_id FROM models WHERE id=?", (mid,)).fetchone()
    if row is None:
        raise ValueError(f"model descriptor {mid} absent from compact")
    table = {"VehicleModel": "vehicle_models", "ShipModel": "ship_models"}.get(row["sub_type"])
    if table is None:
        raise ValueError("unsupported model type: "+row["sub_type"])
    asset = conn.execute(f"SELECT normal FROM {table} WHERE id=?", (row["sub_id"],)).fetchone()
    if asset is None:
        raise ValueError(f"{table} descriptor {row['sub_id']} absent")
    return asset[0]


def audit(conn, pak, data, models=AUDIT_MODELS):
    strings = {r["id"]: (r["actor"], r["prefab"]) for r in conn.execute("SELECT * FROM model_attach_point_strings")}
    required = requirements(conn)
    existing = {r["ModelId"]: {int(k) for k in r["AttachPoints"]} for r in data}
    summonable = {r[0] for r in conn.execute("SELECT DISTINCT slave_id FROM item_summon_slaves")}
    ex, report, additions = Extractor(pak, strings), [], []
    for mid in models:
        slaves = [r[0] for r in conn.execute("SELECT id FROM slaves WHERE model_id=? ORDER BY id", (mid,))]
        req = required.get(mid, {})
        record = {"model": mid, "slaves": slaves, "item_summonable_slaves": sorted(summonable.intersection(slaves)),
                  "required": sorted(req), "already_present": sorted(existing.get(mid, set())),
                  "points": {}, "unresolved": {}, "issues": []}
        ex.issues = []
        try:
            record["normal"] = normal_uri(conn, mid)
            found = ex.model(record["normal"])
        except (KeyError, ValueError) as e:
            found = {}
            ex.issues.append(str(e))
        record["issues"] = list(ex.issues)
        points = {}
        for point in sorted(req):
            if point in existing.get(mid, set()):
                continue
            if point == 0:
                record["unresolved"][str(point)] = {"reason": "None (0): no named attach transform", "consumers": req[point]}
            elif point in found:
                evidence = found[point]
                points[str(point)] = evidence["location"]
                record["points"][str(point)] = {**evidence, "consumers": req[point]}
            else:
                record["unresolved"][str(point)] = {"reason": "native asset/helper absent; see issues and inspected sources",
                                                   "aliases": strings.get(point), "consumers": req[point]}
        if points:
            additions.append({"Name": "Native 1.2 model "+str(mid), "ModelId": mid, "AttachPoints": points})
        # Includes assets that contain no helpers, making negative findings auditable.
        record["inspected_assets"] = [ex.sources[key] for key in sorted(ex.sources)]
        report.append(record)
        ex.sources = {}
    return additions, {"baseline": "37e6f1aec", "format": "native 1.2 r208022; see slave_attach_points.py docstring", "models": report}


def write_additions(path, additions):
    text = path.read_text(encoding="utf-8")
    if not text.rstrip().endswith("]"):
        raise ValueError("expected JSON array")
    # Audit rejects existing-point replacement. Most additions are entirely new
    # models; partial existing models are deliberately left for explicit review.
    old = {r["ModelId"] for r in load_json(path)}
    if any(r["ModelId"] in old for r in additions):
        raise ValueError("partial existing model needs explicit review")
    if additions:
        encoded = json.dumps(additions, indent=4)[1:-1].strip("\n")
        path.write_text(text.rstrip()[:-1].rstrip()+",\n"+encoded+"\n]\n", encoding="utf-8")


def open_pak(path):
    spec = importlib.util.spec_from_file_location("gamepak", ROOT / "rust/tools/opcode_coverage/gamepak.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    pak = module.Pak(path)
    if pak.kind != "A":
        pak.f.close()
        raise ValueError("requires original 1.2 TypeA pak, not 10.x")
    return pak


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--compact", type=Path, required=True)
    ap.add_argument("--pak", type=Path, required=True)
    ap.add_argument("--report", type=Path, required=True)
    ap.add_argument("--data", type=Path, default=DATA)
    ap.add_argument("--write", action="store_true")
    args = ap.parse_args()
    pak = open_pak(args.pak)
    try:
        with read_compact(args.compact) as conn:
            additions, report = audit(conn, pak, load_json(args.data))
        args.report.write_text(json.dumps(report, indent=2)+"\n", encoding="utf-8")
        if args.write:
            write_additions(args.data, additions)
        print(f"{len(additions)} models, {sum(len(r['AttachPoints']) for r in additions)} missing transforms recovered")
        for r in report["models"]:
            print(f"model {r['model']}: recovered {','.join(r['points']) or '-'}; unresolved {','.join(r['unresolved']) or '-'}; {'; '.join(r['issues'])}")
    finally:
        pak.f.close()


if __name__ == "__main__":
    main()
