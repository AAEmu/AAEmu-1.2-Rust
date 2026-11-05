"""Tests for `sphere_area_import.py` with a deliberately small 10.x / 1.2 sample.

Run from this directory: python -I test_sphere_area_import.py
"""

from __future__ import annotations

import io
import json
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from sphere_area_import import DROP_REASONS, convert, pak_index, parse_area_spheres


class FakePak:
    """The `Pak` surface `convert` uses: `entries` plus a seekable `f`."""

    def __init__(self, files: dict[str, bytes]) -> None:
        self.entries: list[tuple[str, int, int]] = []
        blob = bytearray()
        for name, data in files.items():
            self.entries.append((name, len(blob), len(data)))
            blob += data
        self.f = io.BytesIO(bytes(blob))


def area(stype: int, x: float, y: float, z: float, radius: float) -> str:
    return (
        "area\r\n"
        "    kind 1\r\n"
        f"    stype {stype}\r\n"
        f"    pos ( x {x}, y {y}, z {z} )\r\n"
        f"    radius {radius}\r\n"
    )


class ParseTest(unittest.TestCase):
    def test_blocks_and_filler_lines(self) -> None:
        text = "__none__\r\n" + area(1001, 1.5, 2, 3, 4) + area(1002, 5, 6, 7, 8.5)
        blocks = parse_area_spheres(text)
        self.assertEqual(
            blocks,
            [
                {"kind": 1, "stype": 1001, "pos": (1.5, 2.0, 3.0), "radius": 4.0},
                {"kind": 1, "stype": 1002, "pos": (5.0, 6.0, 7.0), "radius": 8.5},
            ],
        )

    def test_last_block_and_missing_radius(self) -> None:
        text = "area\nkind 1\nstype 7\npos ( x 1, y 2, z 3 )\nradius 2"
        self.assertEqual(len(parse_area_spheres(text)), 1)
        text = "area\nkind 1\nstype 7\npos ( x 1, y 2, z 3 )"
        self.assertNotIn("radius", parse_area_spheres(text)[0])


class ConvertTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        (self.root / "game" / "worlds" / "main_world").mkdir(parents=True)
        (self.root / "game" / "worlds" / "other12").mkdir(parents=True)
        (self.root / "game" / "worlds" / "main_world" / "world.xml").write_text(
            '<?xml version="1.0"?><World name="main_world"><ZoneList>'
            '<Zone name="a" id="129" originX="0" originY="0"/>'
            '<Zone name="b" id="141" originX="1" originY="1"/>'
            "</ZoneList></World>",
            encoding="utf-8",
        )
        (self.root / "game" / "worlds" / "other12" / "world.xml").write_text(
            '<?xml version="1.0"?><World name="other12"><ZoneList>'
            '<Zone name="c" id="200" originX="0" originY="0"/>'
            "</ZoneList></World>",
            encoding="utf-8",
        )
        self.compact = self.root / "compact.sqlite3"
        conn = sqlite3.connect(self.compact)
        conn.execute("CREATE TABLE spheres (id INTEGER PRIMARY KEY)")
        conn.executemany("INSERT INTO spheres VALUES (?)", [(1001,), (1002,)])
        conn.commit()
        conn.close()

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def test_guards_keep_only_1_2_world_zone_and_sphere(self) -> None:
        files = {
            "game/worlds/main_world/level_design/zone/129/world_server/quest_area_sphere.g": (
                area(1001, 1, 2, 3, 4)  # kept
                + area(1001, 1, 2, 3, 4)  # duplicate
                + area(9999, 5, 6, 7, 8)  # not a 1.2 sphere
            ).encode(),
            "game/worlds/main_world/level_design/zone/130/world_server/quest_area_sphere.g": (
                area(1001, 9, 9, 9, 9)  # zone not in 1.2
            ).encode(),
            "game/worlds/ten_only/level_design/zone/1/world_server/quest_area_sphere.g": (
                area(1001, 4, 4, 4, 4)  # world not in 1.2
            ).encode(),
            "game/worlds/other12/level_design/zone/200/world_server/quest_area_sphere.g": (
                area(1002, 10, 11, 12, 13)  # second 1.2 world, kept
            ).encode(),
        }
        # Replace the placeholder empty entry with a malformed block in a second file path.
        files[
            "game/worlds/main_world/level_design/zone/129/world_server/quest_area_sphere.g"
        ] += b"area\r\nkind 1\r\nstype 1001\r\npos ( x 1, y 2, z 3 )\r\n"  # no radius
        pak10 = FakePak(files)
        kept, drops, _ = convert(
            pak10, pak_index(pak10), None, None, self.root, self.compact
        )
        self.assertEqual(
            kept,
            {
                "main_world": [
                    {
                        "sphere_id": 1001,
                        "zone": 129,
                        "x": 1.0,
                        "y": 2.0,
                        "z": 3.0,
                        "radius": 4.0,
                    }
                ],
                "other12": [
                    {
                        "sphere_id": 1002,
                        "zone": 200,
                        "x": 10.0,
                        "y": 11.0,
                        "z": 12.0,
                        "radius": 13.0,
                    }
                ],
            },
        )
        self.assertEqual(
            drops,
            {
                "world_not_in_1_2": 1,
                "zone_not_in_1_2": 1,
                "sphere_not_in_1_2": 1,
                "invalid_block": 1,
                "duplicate": 1,
            },
        )
        self.assertEqual(set(drops), set(DROP_REASONS))

    def test_output_json_is_the_loader_shape(self) -> None:
        from sphere_area_import import write_files

        kept = {
            "main_world": [
                {"sphere_id": 1001, "zone": 129, "x": 1.0, "y": 2.0, "z": 3.0, "radius": 4.0}
            ]
        }
        write_files(kept, self.root / "out")
        text = (self.root / "out" / "main_world" / "sphere_area_fill.json").read_text()
        self.assertEqual(json.loads(text), kept["main_world"])


if __name__ == "__main__":
    unittest.main()
