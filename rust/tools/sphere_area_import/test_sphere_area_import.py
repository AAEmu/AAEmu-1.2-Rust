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
from sphere_area_import import (
    DROP_REASONS,
    ZoneFrame,
    convert,
    pak_index,
    parse_area_spheres,
    parse_zone_frames,
)


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
            '<Zone name="a" id="129" originX="1" originY="1">'
            '<cellList><cell x="1" y="1"/></cellList></Zone>'
            '<Zone name="b" id="141" originX="1" originY="1"/>'
            "</ZoneList></World>",
            encoding="utf-8",
        )
        (self.root / "game" / "worlds" / "other12" / "world.xml").write_text(
            '<?xml version="1.0"?><World name="other12"><ZoneList>'
            '<Zone name="c" id="200" originX="0" originY="0">'
            '<cellList><cell x="0" y="0"/></cellList></Zone>'
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
            "game/worlds/main_world/world.xml": (
                '<?xml version="1.0"?><World name="main_world"><ZoneList>'
                '<Zone name="a" id="129" originX="1" originY="1"/>'
                '<Zone name="b" id="130" originX="0" originY="0"/>'
                "</ZoneList></World>"
            ).encode(),
            "game/worlds/other12/world.xml": (
                '<?xml version="1.0"?><World name="other12"><ZoneList>'
                '<Zone name="c" id="200" originX="0" originY="0"/>'
                "</ZoneList></World>"
            ).encode(),
            "game/worlds/main_world/level_design/zone/129/world_server/quest_area_sphere.g": (
                area(1001, 1, 2, 3, 4)  # kept, becomes world (1025, 1026)
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
                        # zone-local (1, 2) + origin cell (1, 1) * 1024
                        "x": 1025.0,
                        "y": 1026.0,
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
                "zone_frame_missing": 0,
                "position_outside_zone": 0,
                "sphere_not_in_1_2": 1,
                "invalid_block": 1,
                "duplicate": 1,
            },
        )
        self.assertEqual(set(drops), set(DROP_REASONS))

    def test_ten_x_origin_anchors_the_local_position(self) -> None:
        files = {
            "game/worlds/main_world/world.xml": (
                '<?xml version="1.0"?><World name="main_world"><ZoneList>'
                '<Zone name="a" id="129" originX="1" originY="0"/>'
                "</ZoneList></World>"
            ).encode(),
            "game/worlds/main_world/level_design/zone/129/world_server/quest_area_sphere.g": (
                area(1001, 10, 1100, 20, 5)  # 10.x local y 1100 is in cell row 13
            ).encode(),
        }
        pak10 = FakePak(files)
        kept, drops, _ = convert(
            pak10, pak_index(pak10), None, None, self.root, self.compact
        )
        # The 10.x origin (1, 0), not the 1.2 origin (1, 1), anchors the local position.
        self.assertEqual(
            (kept["main_world"][0]["x"], kept["main_world"][0]["y"]),
            (1034.0, 1100.0),
        )
        self.assertEqual(drops["position_outside_zone"], 0)

    def test_position_outside_the_1_2_zone_is_dropped(self) -> None:
        files = {
            "game/worlds/main_world/world.xml": (
                '<?xml version="1.0"?><World name="main_world"><ZoneList>'
                '<Zone name="a" id="129" originX="1" originY="1"/>'
                "</ZoneList></World>"
            ).encode(),
            "game/worlds/main_world/level_design/zone/129/world_server/quest_area_sphere.g": (
                area(1001, 5000, 5000, 20, 5)
            ).encode(),
        }
        pak10 = FakePak(files)
        kept, drops, _ = convert(
            pak10, pak_index(pak10), None, None, self.root, self.compact
        )
        self.assertFalse(kept)
        self.assertEqual(drops["position_outside_zone"], 1)

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


class ZoneFrameTest(unittest.TestCase):
    def test_local_plus_origin_is_world(self) -> None:
        frames = parse_zone_frames(
            b'<?xml version="1.0"?><World name="main_world"><ZoneList>'
            b'<Zone name="w_bronze_rock_2" id="192" originX="5" originY="12">'
            b'<cellList><cell x="5" y="12"/><cell x="5" y="13"/>'
            b'<cell x="6" y="12"/><cell x="6" y="13"/></cellList>'
            b"</Zone></ZoneList></World>"
        )
        frame = frames[192]
        x, y = frame.to_world(1055.06, 1460.9)
        self.assertAlmostEqual(x, 6175.06)
        self.assertAlmostEqual(y, 13748.9)
        self.assertTrue(frame.contains_world(x, y))
        self.assertFalse(frame.contains_world(x, y + 2048))

    def test_frame_math_matches_the_sign_sphere_loader(self) -> None:
        # `systems/quests/spheres.rs`: world = origin cell * 1024 + local, z unchanged.
        x, y = ZoneFrame(9, 14).to_world(1174.68, 1645.12)
        self.assertAlmostEqual(x, 10390.68)
        self.assertAlmostEqual(y, 15981.12)


if __name__ == "__main__":
    unittest.main()
