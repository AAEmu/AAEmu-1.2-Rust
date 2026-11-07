"""Tests for `quest_sign_sphere_import.py` with a deliberately small 10.x / 1.2 sample.

Run from this directory: python -I test_quest_sign_sphere_import.py
"""

from __future__ import annotations

import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from quest_sign_sphere_import import (  # noqa: E402
    Block,
    Data10,
    Data12,
    collect_blocks,
    parse_area_spheres,
    parse_quest_sign_spheres,
    plan,
    to_world_entry,
)

QUEST = 1650
COMPONENT = 7764
SPHERE = 650
ZONE = 142  # main_world, open
CLOSED_ZONE = 154


def area(stype: int, x: float, y: float, z: float, radius: float) -> str:
    return (
        "area\r\n"
        "    kind 1\r\n"
        f"    stype {stype}\r\n"
        f"    pos ( x {x}, y {y}, z {z} )\r\n"
        f"    radius {radius}\r\n"
    )


def sign(quest: int, component: int, x: float, y: float, z: float, radius: float) -> str:
    return (
        "area\r\n"
        f"    qtype {quest}\r\n"
        f"    ctype {component}\r\n"
        f"    pos ( x {x}, y {y}, z {z} )\r\n"
        f"    radius {radius}\r\n"
    )


def block(source: str = "quest_area_sphere.g", **kw) -> Block:
    base = dict(
        source=source,
        world="main_world",
        zone=ZONE,
        xyz=(10.0, 20.0, 30.0),
        radius=5.0,
        stype=SPHERE if source == "quest_area_sphere.g" else None,
        quest_id=QUEST,
        component_id=COMPONENT if source == "quest_sign_sphere.g" else None,
    )
    base.update(kw)
    return Block(**base)


class ParseTest(unittest.TestCase):
    def test_area_blocks(self) -> None:
        text = "__none__\r\n" + area(1001, 1.5, 2, 3, 4) + area(1002, 5, 6, 7, 8.5)
        self.assertEqual(
            parse_area_spheres(text),
            [
                {"kind": 1, "stype": 1001, "pos": (1.5, 2.0, 3.0), "radius": 4.0},
                {"kind": 1, "stype": 1002, "pos": (5.0, 6.0, 7.0), "radius": 8.5},
            ],
        )

    def test_sign_blocks_and_missing_radius(self) -> None:
        self.assertEqual(
            parse_quest_sign_spheres(sign(QUEST, COMPONENT, 1, 2, 3, 4)),
            [
                {
                    "qtype": QUEST,
                    "ctype": COMPONENT,
                    "pos": (1.0, 2.0, 3.0),
                    "radius": 4.0,
                }
            ],
        )
        self.assertNotIn(
            "radius",
            parse_quest_sign_spheres("area\nqtype 1\nctype 2\npos (1,2,3)")[0],
        )


class PlanTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.worlds = {
            "main_world": {ZONE: (12, 13), CLOSED_ZONE: (5, 11)},
        }
        self.missions: dict[tuple[str, int], list[tuple]] = {}

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def data12(
        self,
        closed: str = "f",
        detail: int = 541,
        sphere_quest: int = QUEST,
        act_quest: int = QUEST,
        act_component: int = COMPONENT,
        act_sphere: int = SPHERE,
        client_spheres: dict | None = None,
        zone: int = ZONE,
    ) -> Data12:
        return Data12(
            zones={zone: (9, closed)},
            quest_zone={act_quest: 9},
            sphere={act_sphere: (detail, "SphereQuest")},
            sphere_quest={detail: sphere_quest},
            acts={(act_quest, act_component): [act_sphere]},
            client_spheres=client_spheres or {},
        )

    def data10(
        self,
        detail: int = 541,
        sphere_quest: int = QUEST,
        quest: int = QUEST,
        component: int = COMPONENT,
        sphere: int = SPHERE,
    ) -> Data10:
        return Data10(
            sphere={sphere: (detail, "SphereQuest")},
            sphere_quest={detail: sphere_quest},
            acts={sphere: [(quest, component)]},
        )

    def test_area_block_is_kept(self) -> None:
        result = plan(
            [block()], self.data12(), self.data10(), self.worlds, self.missions
        )
        self.assertEqual(result.drops["sphere_mapping_mismatch"], 0)
        self.assertEqual(len(result.kept), 1)
        self.assertEqual(result.kept[0]["radius_source"], "10x_area")
        self.assertEqual(result.kept_by_quest[QUEST], 1)

    def test_sphere_not_in_1_2(self) -> None:
        d12 = self.data12()
        d12.sphere.clear()
        result = plan([block()], d12, self.data10(), self.worlds, self.missions)
        self.assertEqual(result.drops["sphere_not_in_1_2"], 1)
        self.assertFalse(result.kept)

    def test_sphere_not_quest(self) -> None:
        d12 = self.data12()
        d12.sphere[SPHERE] = (541, "SphereBubble")
        result = plan([block()], d12, self.data10(), self.worlds, self.missions)
        self.assertEqual(result.drops["sphere_not_quest"], 1)
        self.assertFalse(result.kept)

    def test_sphere_mapping_mismatch(self) -> None:
        # 10.x reused the sphere for another quest (the 945 / 5804 case); the 1.2 act is
        # still present, so only the sphere_quests mapping can reject the block.
        d10 = self.data10(sphere_quest=5804)
        d10.acts[SPHERE].append((QUEST, COMPONENT))
        result = plan([block()], self.data12(), d10, self.worlds, self.missions)
        self.assertEqual(result.drops["sphere_mapping_mismatch"], 1)
        self.assertFalse(result.kept)

    def test_component_not_in_1_2(self) -> None:
        result = plan(
            [block()],
            self.data12(act_component=9999),
            self.data10(),
            self.worlds,
            self.missions,
        )
        self.assertEqual(result.drops["component_not_in_1_2"], 1)
        self.assertFalse(result.kept)

    def test_zone_not_in_1_2(self) -> None:
        result = plan(
            [block(zone=999)],
            self.data12(zone=999),
            self.data10(),
            self.worlds,
            self.missions,
        )
        self.assertEqual(result.drops["zone_not_in_1_2"], 1)
        self.assertFalse(result.kept)

    def test_zone_closed(self) -> None:
        d12 = self.data12()
        d12.zones[CLOSED_ZONE] = (19, "t")
        result = plan(
            [block(zone=CLOSED_ZONE)],
            d12,
            self.data10(),
            self.worlds,
            self.missions,
        )
        self.assertEqual(result.drops["zone_closed"], 1)
        self.assertFalse(result.kept)

    def test_client_sphere_is_never_overridden(self) -> None:
        result = plan(
            [block()],
            self.data12(client_spheres={(QUEST, COMPONENT): [{"zone": ZONE}]}),
            self.data10(),
            self.worlds,
            self.missions,
        )
        self.assertEqual(result.drops["client_sphere_exists"], 1)
        self.assertFalse(result.kept)

    def test_radius_must_agree_with_the_1_2_mission_data(self) -> None:
        self.missions[("main_world", ZONE)] = [((10.0, 20.0, 30.0), SPHERE, 9.0)]
        result = plan(
            [block()], self.data12(), self.data10(), self.worlds, self.missions
        )
        self.assertEqual(result.drops["radius_mismatch"], 1)
        self.assertFalse(result.kept)

        self.missions[("main_world", ZONE)] = [((10.0, 20.0, 30.0), SPHERE, 5.0)]
        result = plan(
            [block()], self.data12(), self.data10(), self.worlds, self.missions
        )
        self.assertEqual(len(result.kept), 1)
        self.assertEqual(result.kept[0]["radius_source"], "1_2_mission")

    def test_zone_mismatch_prefers_the_quest_zone(self) -> None:
        blocks = [block(zone=ZONE), block(zone=186)]
        d12 = self.data12()
        d12.zones[186] = (132, "f")
        self.worlds["main_world"][186] = (12, 9)
        result = plan(blocks, d12, self.data10(), self.worlds, self.missions)
        self.assertEqual(len(result.kept), 1)
        self.assertEqual(result.kept[0]["zone"], ZONE)
        self.assertEqual(result.drops["zone_mismatch"], 1)

    def test_duplicate_is_rejected(self) -> None:
        result = plan(
            [block(), block()],
            self.data12(),
            self.data10(),
            self.worlds,
            self.missions,
        )
        self.assertEqual(len(result.kept), 1)
        self.assertEqual(result.drops["duplicate"], 1)

    def test_quest_not_target_is_ignored(self) -> None:
        result = plan(
            [block(stype=SPHERE)],
            self.data12(act_quest=17),
            self.data10(quest=17),
            self.worlds,
            self.missions,
        )
        self.assertFalse(result.kept)

    def test_sign_sphere_source_is_kept(self) -> None:
        result = plan(
            [block(source="quest_sign_sphere.g")],
            self.data12(),
            self.data10(),
            self.worlds,
            self.missions,
        )
        self.assertEqual(len(result.kept), 1)
        self.assertEqual(result.kept[0]["source"], "quest_sign_sphere.g")
        self.assertEqual(result.kept[0]["radius_source"], "10x_client")


class WorldConversionTest(unittest.TestCase):
    def test_local_plus_source_origin_is_world(self) -> None:
        entry = {
            "quest_id": QUEST,
            "component_id": COMPONENT,
            "sphere_id": SPHERE,
            "zone": ZONE,
            "x": 10.0,
            "y": 20.0,
            "z": 30.0,
            "radius": 5.0,
        }
        worlds12 = {"main_world": {ZONE: (12, 13)}}
        worlds10 = {"main_world": {ZONE: (12, 12)}}
        out = to_world_entry(entry, "main_world", worlds10, worlds12)
        assert out is not None
        self.assertEqual((out["x"], out["y"], out["z"]), (12 * 1024 + 10, 12 * 1024 + 20, 30))
        # Without a 10.x frame the 1.2 origin is the fallback.
        out = to_world_entry(entry, "main_world", {}, worlds12)
        assert out is not None
        self.assertEqual((out["x"], out["y"]), (12 * 1024 + 10, 13 * 1024 + 20))
        self.assertIsNone(to_world_entry(entry, "main_world", {}, {}))


class CollectTest(unittest.TestCase):
    def test_extracted_files_are_read(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            area_dir = (
                root
                / "game"
                / "worlds"
                / "main_world"
                / "level_design"
                / "zone"
                / str(ZONE)
                / "world_server"
            )
            sign_dir = area_dir.parent / "client"
            area_dir.mkdir(parents=True)
            sign_dir.mkdir(parents=True)
            (area_dir / "quest_area_sphere.g").write_text(
                area(SPHERE, 1, 2, 3, 4), encoding="utf-8"
            )
            (sign_dir / "quest_sign_sphere.g").write_text(
                sign(QUEST, COMPONENT, 5, 6, 7, 8), encoding="utf-8"
            )
            blocks, files, missing = collect_blocks(None, None, root)
            self.assertEqual(files, 2)
            self.assertEqual(missing, 0)
            self.assertEqual(len(blocks), 2)
            sources = {b.source for b in blocks if b is not None}
            self.assertEqual(sources, {"quest_area_sphere.g", "quest_sign_sphere.g"})


if __name__ == "__main__":
    unittest.main()
