"""Shared helpers of the spawn import tools."""
import json
import math
import re

_STR = re.compile(r'"(?:\\.|[^"\\])*"|//[^\r\n]*|/\*.*?\*/', re.S)


def tolerant_json(text):
    """The spawn files are C#-tolerant JSON: comments, trailing commas, BOM."""
    if text.startswith("\ufeff"):
        text = text[1:]
    text = _STR.sub(lambda m: m.group(0) if m.group(0).startswith('"') else "", text)
    text = re.sub(r",(\s*[\]}])", r"\1", text)
    return json.loads(text)


def load_spawn_file(path):
    """[(UnitId, x, y, z)] of an existing spawn file (missing file = empty)."""
    try:
        text = open(path, encoding="utf-8-sig").read()
    except FileNotFoundError:
        return []
    out = []
    for e in tolerant_json(text):
        p = e.get("Position") or {}
        uid = e.get("UnitId", e.get("Id"))
        if uid is None or not p:
            continue
        out.append((int(uid), float(p.get("X", 0)), float(p.get("Y", 0)), float(p.get("Z", 0))))
    return out


class Grid:
    """Spatial hash of existing spawns (cell = 64 m)."""

    CELL = 64.0

    def __init__(self):
        self.cells = {}

    def add(self, uid, x, y, z):
        self.cells.setdefault((int(x // self.CELL), int(y // self.CELL)), []).append((uid, x, y, z))

    def near(self, x, y, radius):
        cx, cy = int(x // self.CELL), int(y // self.CELL)
        r = int(radius // self.CELL) + 1
        for i in range(cx - r, cx + r + 1):
            for j in range(cy - r, cy + r + 1):
                for e in self.cells.get((i, j), ()):
                    if math.hypot(e[1] - x, e[2] - y) <= radius:
                        yield e
