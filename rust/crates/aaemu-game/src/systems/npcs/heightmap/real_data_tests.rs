//! `AAEMU_GAME_PAK=D:\AA1.2\game_pak cargo test -p aaemu-game real_heightmaps -- --ignored
//! --nocapture`: loads main_world cells from the client pak and compares the interpolated
//! height with the Z of the NPC spawns of `Data/Worlds/main_world/npc_spawns.json`.
use std::collections::BTreeMap;
use std::path::PathBuf;

use super::*;
use crate::systems::npcs::json;

pub(crate) fn app_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("app")
}

/// `AAEMU_GAME_PAK=D:\AA1.2\game_pak cargo test -p aaemu-game real_terrain_holes -- --ignored
/// --nocapture`: the `hasHoles` leaf counts of the AABevy survey (`aa-world.md`: cells 20,8 /
/// 21,8 / 8,13 have 7 / 3 / 2), the verified hole point (21204.25, 9026.5), and the height
/// difference between the C# `data >> 4` decode and the native `raw & 0xFFE0` one.
#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_terrain_holes() {
    let Ok(pak) = std::env::var("AAEMU_GAME_PAK") else {
        return;
    };
    let app = app_path();
    let files = Arc::new(ClientFileManager::from_sources(&[pak], &app));
    let worlds = WorldTemplates::load(&files, &app).expect("world templates");
    let main = worlds.get_world_template_by_name("main_world").unwrap();
    let h = HeightmapWorlds::new(
        files.clone(),
        &worlds,
        HeightmapConfig {
            heightmaps_enable: true,
            ..Default::default()
        },
    );
    let t = h.template(main.id).unwrap();
    for ((cx, cy), want) in [((20, 8), 7usize), ((21, 8), 3), ((8, 13), 2)] {
        let bytes = files
            .read_file(&format!(
                "game/worlds/main_world/cells/{cx:03}_{cy:03}/client/terrain/heightmap.dat"
            ))
            .unwrap();
        let hm = hmap::Hmap::read(&bytes, false).unwrap().unwrap();
        let leaves: Vec<_> = hm
            .nodes
            .iter()
            .filter(|n| !n.p_hm_data.is_empty())
            .collect();
        let flagged = leaves.iter().filter(|n| n.b_has_holes != 0).count();
        let with_sid31 = leaves
            .iter()
            .filter(|n| n.surface.contains(&hmap::HOLE_SURFACE_ID))
            .count();
        let (mut grid, mut versions) = (0, std::collections::BTreeSet::new());
        for gx in 0..CELL_HMAP_RESOLUTION {
            for gy in 0..CELL_HMAP_RESOLUTION {
                grid += usize::from(t.grid_is_hole(
                    &files,
                    cx * CELL_HMAP_RESOLUTION + gx,
                    cy * CELL_HMAP_RESOLUTION + gy,
                ));
            }
        }
        for n in &leaves {
            versions.insert(n.version);
        }
        println!(
                "cell {cx},{cy}: {} leaves, hasHoles {flagged}, leaves with SID 31 {with_sid31},                  hole grid points {grid}, node versions {versions:?}",
                leaves.len()
            );
        assert_eq!(flagged, want, "hasHoles leaves of cell {cx},{cy}");
        assert!(grid > 0, "no hole grid points in cell {cx},{cy}");
    }
    // AABevy's verified global control: (21204.25, 9026.5) is a hole.
    let (ix, iy) = (
        (21204.25f32 / 2.0).floor() as i32,
        (9026.5f32 / 2.0).floor() as i32,
    );
    assert!(
        t.grid_is_hole(&files, ix, iy),
        "21204.25,9026.5 must be a hole"
    );
    let around: Vec<bool> = (-2..=2)
        .map(|d| t.grid_is_hole(&files, ix + d * 8, iy))
        .collect();
    println!("holes along x around the control point (step 16 m): {around:?}");

    // Height decode: `RawDataToHeight` (data >> 4 after RescaleToInt) against the native
    // `offset + (raw & 0xFFE0) * range`, on the raw samples of three cells.
    for (cx, cy) in [(20, 8), (21, 8), (8, 13), (16, 16)] {
        let Some(bytes) = files.read_file(&format!(
            "game/worlds/main_world/cells/{cx:03}_{cy:03}/client/terrain/heightmap.dat"
        )) else {
            continue;
        };
        let raw_nodes = hmap::Hmap::read(&bytes, true).unwrap().unwrap();
        let conv = hmap::Hmap::read(&bytes, false).unwrap().unwrap();
        let (mut max, mut sum, mut n) = (0f64, 0f64, 0usize);
        for (a, b) in raw_nodes.nodes.iter().zip(conv.nodes.iter()) {
            if a.n_size != 33 {
                continue;
            }
            for i in 0..a.p_hm_data.len() {
                let native = f64::from(a.f_offset)
                    + f64::from(a.p_hm_data[i] & 0xFFE0) * f64::from(a.f_range);
                let Ok(cs) = b.get_height_by_index(i as u32) else {
                    continue;
                };
                let d = (native - f64::from(cs)).abs();
                max = max.max(d);
                sum += d;
                n += 1;
            }
        }
        println!(
                "cell {cx},{cy}: C# vs native height over {n} samples (33x33 leaves): mean {:.4} m, max {:.4} m",
                sum / n.max(1) as f64,
                max
            );
    }
}

/// `AAEMU_GAME_PAK=D:\AA1.2\game_pak AAEMU_HEIGHT_QUERY=q.json AAEMU_HEIGHT_OUT=h.json cargo
/// test -p aaemu-game dump_terrain_heights -- --ignored`: terrain height and zone id for the
/// points `[{"id", "world", "x", "y"}]` of the query file (`rust/tools/spawn_import`).
#[test]
#[ignore = "needs AAEMU_GAME_PAK, AAEMU_HEIGHT_QUERY and AAEMU_HEIGHT_OUT"]
fn dump_terrain_heights() {
    let (Ok(pak), Ok(query), Ok(out)) = (
        std::env::var("AAEMU_GAME_PAK"),
        std::env::var("AAEMU_HEIGHT_QUERY"),
        std::env::var("AAEMU_HEIGHT_OUT"),
    ) else {
        return;
    };
    let app = app_path();
    let files = Arc::new(ClientFileManager::from_sources(&[pak], &app));
    let worlds = WorldTemplates::load(&files, &app).expect("world templates");
    let h = HeightmapWorlds::new(
        files.clone(),
        &worlds,
        HeightmapConfig {
            heightmaps_enable: true,
            ..Default::default()
        },
    );
    let doc = json::parse(&std::fs::read_to_string(query).unwrap()).unwrap();
    let mut rows = Vec::new();
    for q in json::array(&doc).unwrap() {
        let o = q.as_object().unwrap();
        let id = json::prop(o, "id").and_then(|v| v.as_str()).unwrap();
        let world = json::prop(o, "world").and_then(|v| v.as_str()).unwrap();
        let x = json::f32_or(o, "x", 0.0).unwrap();
        let y = json::f32_or(o, "y", 0.0).unwrap();
        let Some(template) = worlds.get_world_template_by_name(world) else {
            rows.push(format!(r#"{{"id":"{id}","error":"no world template"}}"#));
            continue;
        };
        let zone = worlds.get_zone_id(Some(template), x, y);
        let Some(t) = h.template(template.id) else {
            rows.push(format!(
                r#"{{"id":"{id}","zone":{zone},"error":"no heightmap"}}"#
            ));
            continue;
        };
        let (cx, cy) = (x as i32 / CELL_SIZE, y as i32 / CELL_SIZE);
        let loaded = t.verify_cell_loaded(&files, cx, cy).is_ok() && t.cell_has_height_map(cx, cy);
        match t.get_height(&files, x, y) {
            Ok(z) if loaded => rows.push(format!(r#"{{"id":"{id}","zone":{zone},"h":{z}}}"#)),
            _ => rows.push(format!(
                r#"{{"id":"{id}","zone":{zone},"error":"no cell"}}"#
            )),
        }
    }
    std::fs::write(out, format!("[{}]", rows.join(","))).unwrap();
}

#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_heightmaps() {
    let Ok(pak) = std::env::var("AAEMU_GAME_PAK") else {
        return;
    };
    let app = app_path();
    let files = Arc::new(ClientFileManager::from_sources(&[pak], &app));
    let worlds = WorldTemplates::load(&files, &app).expect("world templates");
    let main = worlds.get_world_template_by_name("main_world").unwrap();
    let h = HeightmapWorlds::new(
        files.clone(),
        &worlds,
        HeightmapConfig {
            heightmaps_enable: true,
            ..Default::default()
        },
    );
    let text = std::fs::read_to_string(app.join("Data/Worlds/main_world/npc_spawns.json")).unwrap();
    let doc = json::parse(&text).unwrap();
    // Group the spawns by cell and keep the 12 cells with the most spawns.
    let mut by_cell: BTreeMap<(i32, i32), Vec<[f32; 3]>> = BTreeMap::new();
    for e in json::array(&doc).unwrap() {
        let Some(p) = e
            .as_object()
            .and_then(|o| json::prop(o, "Position"))
            .and_then(|p| p.as_object())
        else {
            continue;
        };
        let x = json::f32_or(p, "X", 0.0).unwrap();
        let y = json::f32_or(p, "Y", 0.0).unwrap();
        let z = json::f32_or(p, "Z", 0.0).unwrap();
        by_cell
            .entry((x as i32 / CELL_SIZE, y as i32 / CELL_SIZE))
            .or_default()
            .push([x, y, z]);
    }
    let mut cells: Vec<_> = by_cell.into_iter().collect();
    cells.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    cells.truncate(12);
    let t = h.template(main.id).unwrap();
    let started = std::time::Instant::now();
    let mut errors = Vec::new();
    let mut failed_cells = 0;
    for ((cx, cy), spawns) in &cells {
        if t.verify_cell_loaded(&files, *cx, *cy).is_err() {
            failed_cells += 1;
            continue;
        }
        for s in spawns {
            let terrain = t.get_height(&files, s[0], s[1]).unwrap();
            errors.push(s[2] - terrain);
        }
    }
    let elapsed = started.elapsed();
    assert_eq!(failed_cells, 0);
    let mut abs: Vec<f32> = errors.iter().map(|e| e.abs()).collect();
    abs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = abs.len();
    let pct = |p: f64| abs[((n as f64 - 1.0) * p) as usize];
    let within = |d: f32| abs.iter().filter(|&&e| e <= d).count() as f64 / n as f64 * 100.0;
    eprintln!(
            "{} cells loaded in {elapsed:?}, {n} spawns: |spawnZ - height| median {:.3} p90 {:.3} p99 {:.3} max {:.3}; <=0.5m {:.1}% <=2m {:.1}% <=10m {:.1}%; mean signed {:.3}",
            t.loaded_cell_count(),
            pct(0.5),
            pct(0.9),
            pct(0.99),
            abs[n - 1],
            within(0.5),
            within(2.0),
            within(10.0),
            errors.iter().sum::<f32>() / n as f32,
        );
    assert!(n > 1000);
    assert!(pct(0.5) < 1.0, "median error {}", pct(0.5));
    // WorldManager.GetHeight(zoneKey) path on the same data.
    let s = cells[0].1[0];
    let zone = worlds.get_zone_id(Some(main), s[0], s[1]);
    assert_eq!(
        h.get_height(zone, s[0], s[1], s[2]),
        t.get_height(&files, s[0], s[1]).unwrap()
    );
}

/// `AAEMU_GAME_PAK=D:\AA1.2\game_pak cargo test -p aaemu-game main_world_spawn_terrain_clamp
/// -- --ignored --nocapture`: counts main_world NPC spawns whose terrain sits at the old
/// 1024 m clamp (coefficient `max_height / 4`) and at the new one.
#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn main_world_spawn_terrain_clamp() {
    let Ok(pak) = std::env::var("AAEMU_GAME_PAK") else {
        return;
    };
    let app = app_path();
    let files = Arc::new(ClientFileManager::from_sources(&[pak], &app));
    let worlds = WorldTemplates::load(&files, &app).expect("world templates");
    let main = worlds.get_world_template_by_name("main_world").unwrap();
    let new_coef = main.height_max_coefficient;
    let old_coef = f64::from(u16::MAX) / (f64::from(main.max_height) / 4.0);
    let make =
        |coef| WorldHeightmap::with_size(main.id, &main.name, main.cell_x, main.cell_y, coef);
    let (old, new) = (make(old_coef), make(new_coef));
    let text = std::fs::read_to_string(app.join("Data/Worlds/main_world/npc_spawns.json")).unwrap();
    let doc = json::parse(&text).unwrap();
    let (mut n, mut old_clamped, mut new_clamped, mut max_new) = (0, 0, 0, 0f32);
    for e in json::array(&doc).unwrap() {
        let Some(p) = e
            .as_object()
            .and_then(|o| json::prop(o, "Position"))
            .and_then(|p| p.as_object())
        else {
            continue;
        };
        let x = json::f32_or(p, "X", 0.0).unwrap();
        let y = json::f32_or(p, "Y", 0.0).unwrap();
        let (Ok(a), Ok(b)) = (old.get_height(&files, x, y), new.get_height(&files, x, y)) else {
            continue;
        };
        n += 1;
        old_clamped += usize::from(a >= main.max_height / 4.0 - 0.05);
        new_clamped += usize::from(b >= main.max_height - 0.05);
        max_new = max_new.max(b);
    }
    eprintln!(
            "{n} spawns: terrain at the old 1024 m clamp {old_clamped}, at the new ceiling {new_clamped}; max terrain {max_new:.2} m"
        );
    assert!(n > 1000);
    assert_eq!(new_clamped, 0);
}

/// `AAEMU_GAME_PAK=D:\AA1.2\game_pak cargo test -p aaemu-game
/// real_heights_above_1024_m -- --ignored --nocapture`: the three highest points of the
/// 2026-11-08 client/server terrain cross-check
/// (`D:/aaemu-audits/terrain-3way-2026-11-08/REPORT.md`, `points.csv`: Bevy / bake). The
/// C# `HeightMaxCoefficient` read 1024.0 m at all three; the whole 0..=4096 m range must
/// round-trip now.
#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_heights_above_1024_m() {
    let Ok(pak) = std::env::var("AAEMU_GAME_PAK") else {
        return;
    };
    let app = app_path();
    let files = Arc::new(ClientFileManager::from_sources(&[pak], &app));
    let worlds = WorldTemplates::load(&files, &app).expect("world templates");
    let main = worlds.get_world_template_by_name("main_world").unwrap();
    let h = HeightmapWorlds::new(
        files.clone(),
        &worlds,
        HeightmapConfig {
            heightmaps_enable: true,
            ..Default::default()
        },
    );
    let t = h.template(main.id).unwrap();
    // (x, y, Bevy `height_at` as f32, bake) from `points.csv`.
    let points: [(f32, f32, f32, f64); 3] = [
        (26893.904, 9839.206, 1551.6947, 1551.589796),
        (26850.271, 10020.228, 1551.0603, 1551.002063),
        (26621.588, 10108.499, 1468.8883, 1469.144192),
    ];
    for (x, y, bevy, bake) in points {
        let z = f64::from(t.get_height(&files, x, y).unwrap());
        println!("({x}, {y}): server {z}, Bevy {bevy}, bake {bake}");
        assert!(z > 1024.0, "({x}, {y}) still saturates at {z}");
        // The server's bilinear rule and Bevy's native triangle rule differ by the local
        // slope change (the audit's bound is 0.10 + 0.35 * twist); at these points they
        // agree within 0.1 m. The 4 m bake is a coarser bilinear sample.
        assert!(
            (z - f64::from(bevy)).abs() <= 0.1,
            "({x}, {y}): server {z}, Bevy {bevy}, bake {bake}"
        );
    }
}

/// `AAEMU_GAME_PAK=D:\AA1.2\game_pak cargo test -p aaemu-game
/// real_low_resolution_node_heights -- --ignored --nocapture`: the twelve Bevy-only points
/// of the 2026-11-08 terrain cross-check whose 2 m quad touches a low-resolution sector
/// node (n_size 5, 9 or 17), plus the sector-boundary case. The original 1.2 client agrees
/// with Bevy there (median 0.0001 m, max 0.066 m) while the C# `UpScale` quirk put the
/// server a median 10.6 m (max 25.6 m) away (`recheck-results.csv`). The client's resampling
/// rule must land within 0.15 m of the original client: the resampling itself is exact
/// (f32 bilinear of the node's samples), and the remaining floor is the existing
/// `rescale_to_int` truncation of each source sample, up to `i_step * 5 cm` (0.0985 m at
/// worst here). A wrong rule misses by metres.
#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_low_resolution_node_heights() {
    let Ok(pak) = std::env::var("AAEMU_GAME_PAK") else {
        return;
    };
    let app = app_path();
    let files = Arc::new(ClientFileManager::from_sources(&[pak], &app));
    let worlds = WorldTemplates::load(&files, &app).expect("world templates");
    let main = worlds.get_world_template_by_name("main_world").unwrap();
    let h = HeightmapWorlds::new(
        files.clone(),
        &worlds,
        HeightmapConfig {
            heightmaps_enable: true,
            ..Default::default()
        },
    );
    let t = h.template(main.id).unwrap();
    // (x, y, original-client `System.GetTerrainElevation`) from `recheck-results.csv`.
    let points: [(f64, f64, f64); 12] = [
        (23731.848, 28045.190, 186.2100),
        (22061.431, 32225.733, 297.3200),
        (24468.402, 25835.133, 121.1429),
        (22951.330, 27138.477, 143.5613),
        (10559.033, 14422.889, 590.6133),
        (22276.248, 32309.476, 252.6515),
        (9623.759, 15653.809, 518.2814),
        (25080.578, 26982.832, 231.5654),
        (24168.769, 25735.047, 121.4420),
        (21159.459, 28314.062, 128.1005),
        (21697.676, 32621.940, 297.7408),
        (21090.287, 28251.780, 104.0008),
    ];
    let mut matches = 0;
    for (x, y, original) in points {
        let z = f64::from(t.get_height(&files, x as f32, y as f32).unwrap());
        let diff = (z - original).abs();
        println!("({x}, {y}): server {z}, original {original}, diff {diff}");
        // 0.15 m, not 0.1: the source samples are already floored by the existing
        // `rescale_to_int` (up to i_step * 5 cm); the resampling adds no step of its own.
        assert!(
            diff <= 0.15,
            "({x}, {y}): server {z}, original {original}, diff {diff}"
        );
        matches += 1;
    }
    assert_eq!(matches, 12, "the twelve low-resolution-node points");
}
