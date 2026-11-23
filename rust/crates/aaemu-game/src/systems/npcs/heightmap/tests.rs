use super::*;

const COEF: f64 = 65535.0 / 4096.0;

#[test]
fn node_surface_ids_follow_the_native_lower_sample_rule() {
    use hmap::{NodeCell, HOLE_SURFACE_ID};
    // 33 x 33: the surface id is the low five bits of the raw sample (x-major).
    let mut data = vec![0x1234u16; 33 * 33];
    data[3 * 33 + 4] = 0xFFE0 | 31;
    data[5 * 33 + 6] = 0x00A0 | 30;
    let mut n = NodeCell::default();
    n.version = 5;
    n.n_size = 33;
    n.p_hm_data = data;
    let surface = n.read_surface();
    assert_eq!(surface.len(), 33 * 33);
    assert_eq!(surface[3 * 33 + 4], HOLE_SURFACE_ID);
    assert_eq!(surface[3 * 33 + 4], 31);
    assert_eq!(surface[5 * 33 + 6], 30);
    assert_eq!(u16::from(surface[0]), 0x1234 & 0x1F);
    // 3 x 3 (32 m spacing): sector points 16..=31 read sample 1 (shift, not rounding),
    // 32 reads sample 2.
    let mut data = vec![0u16; 9];
    data[3] = 31;
    let mut n = NodeCell::default();
    n.version = 5;
    n.n_size = 3;
    n.p_hm_data = data;
    let surface = n.read_surface();
    assert_eq!(surface[15 * 33], 0);
    assert_eq!(surface[16 * 33], 31);
    assert_eq!(surface[31 * 33 + 15], 31);
    assert_eq!(surface[31 * 33 + 16], 0);
    assert_eq!(surface[32 * 33], 0);
}

#[test]
fn synthetic_hole_bits_are_per_grid_point() {
    let t = WorldHeightmap::with_size(0, "test_world", 2, 2, COEF);
    assert!(t.insert_cell_with_holes(1, 0, vec![1; CELL_POINTS], &[(0, 5), (511, 511)]));
    let files = ClientFileManager::new();
    let (bx, by) = (CELL_HMAP_RESOLUTION, 0);
    assert!(t.grid_is_hole(&files, bx, by + 5));
    assert!(t.grid_is_hole(&files, bx + 511, by + 511));
    assert!(!t.grid_is_hole(&files, bx + 1, by + 5));
    assert!(!t.grid_is_hole(&files, 0, 5));
    assert!(!t.grid_is_hole(&files, -1, 5));
    assert!(!t.grid_is_hole(&files, 4000, 5));
}

#[test]
fn heights_above_1024m_round_trip() {
    // The old coefficient (max_height / 4) saturated at 1023.98 m; now the u16 covers max_height.
    let t = WorldHeightmap::with_size(0, "test_world", 2, 2, COEF);
    let mut h = vec![0u16; CELL_POINTS];
    for (i, m) in [1500.0f64, 2000.0, 4095.0].into_iter().enumerate() {
        h[i] = (m * COEF).round() as u16;
    }
    assert!(t.insert_cell(0, 0, h));
    for (i, m) in [1500.0f32, 2000.0, 4095.0].into_iter().enumerate() {
        let got = t.get_height_map_data_in_cell(0, 0, 0, i as i32).unwrap();
        assert!((got - m).abs() <= 1.0 / COEF as f32, "{m} -> {got}");
    }
    assert!(COEF.recip() < 0.07, "precision is at least 1/16 m");
}

#[test]
fn synthetic_template_covers_max_height() {
    let t = WorldTemplate::synthetic(0, "main_world", 1, 1, 1);
    assert_eq!(t.height_max_coefficient, f64::from(u16::MAX) / 4096.0);
    assert_eq!((4095.0 * t.height_max_coefficient).round() as u16, 65519);
}

fn synthetic() -> HeightmapWorlds {
    let t = WorldHeightmap::with_size(0, "test_world", 2, 2, COEF);
    // Cell (0,0): height(x, y) = x_index (in metres: 2 m grid → value = index units).
    let mut h = vec![0u16; CELL_POINTS];
    for x in 0..CELL_HMAP_RESOLUTION {
        for y in 0..CELL_HMAP_RESOLUTION {
            h[(x * CELL_HMAP_RESOLUTION + y) as usize] =
                (f64::from(x + 2 * y) * COEF).round() as u16;
        }
    }
    assert!(t.insert_cell(0, 0, h));
    let mut zones = HashMap::new();
    zones.insert(100, 0);
    HeightmapWorlds::from_parts(
        Arc::new(ClientFileManager::new()),
        vec![t],
        zones,
        HeightmapConfig {
            heightmaps_enable: true,
            ..Default::default()
        },
    )
}

#[test]
fn raw_and_interpolated_heights() {
    let w = synthetic();
    let t = w.template(0).unwrap();
    let f = w.files().clone();
    // grid point (5, 7) -> world (10, 14)
    let raw = t.get_raw_height_map_height(&f, 10, 14).unwrap();
    let expect = ((f64::from(5 + 14) * COEF).round() as u16) as f64 / COEF;
    assert_eq!(raw, expect as f32);
    // odd coordinates use the same grid point (x % 1024 / 2)
    assert_eq!(t.get_raw_height_map_height(&f, 11, 15).unwrap(), raw);
    // Interpolation half-way between (10,14),(12,14),(10,16),(12,16)
    let h = t.get_height(&f, 11.0, 15.0).unwrap();
    let tl = t.get_raw_height_map_height(&f, 10, 14).unwrap();
    let tr = t.get_raw_height_map_height(&f, 12, 14).unwrap();
    let bl = t.get_raw_height_map_height(&f, 10, 16).unwrap();
    let br = t.get_raw_height_map_height(&f, 12, 16).unwrap();
    assert_eq!(h, lerp(lerp(tl, tr, 0.5), lerp(bl, br, 0.5), 0.5));
    // The u16 unit is 1/16 m.
    assert!((h - 20.5).abs() < 0.1, "{h}");
    // Exactly on a grid point: offsets 0.
    assert_eq!(t.get_height(&f, 10.0, 14.0).unwrap(), tl);
}

#[test]
fn csharp_bounds_quirks() {
    let w = synthetic();
    let t = w.template(0).unwrap();
    let f = w.files().clone();
    // cellX > CellX -> 0, cellX == CellX -> IndexOutOfRange (C# bound check bug)
    assert_eq!(t.get_raw_height_map_height(&f, 3 * 1024, 0), Ok(0.0));
    assert_eq!(
        t.get_raw_height_map_height(&f, 2 * 1024, 0),
        Err(HeightError::IndexOutOfRange)
    );
    // negative: x = -1 -> cell 0, sx 0; x = -2 -> sx -1 -> throws
    assert!(t.get_raw_height_map_height(&f, -1, 0).is_ok());
    assert_eq!(
        t.get_raw_height_map_height(&f, -2, 0),
        Err(HeightError::IndexOutOfRange)
    );
    assert_eq!(t.get_raw_height_map_height(&f, -1024, 0), Ok(0.0));
    // A cell without client data is "loaded" with zeros.
    assert_eq!(t.get_raw_height_map_height(&f, 1500, 100), Ok(0.0));
    assert!(t.is_cell_loaded(1, 0));
    // WorldManager.GetHeight catches -> 0; unknown zone -> 0
    assert_eq!(w.get_height(100, 2048.0, 5.0, 1.0), 0.0);
    assert_eq!(w.get_height(999, 10.0, 14.0, 1.0), 0.0);
    assert!(w.get_height(100, 10.0, 14.0, 1.0) > 0.0);
    // GetHeight(transform) falls back to Z on exception.
    assert_eq!(w.get_height_at(Some(0), 2048.0, 5.0, 77.0), 77.0);
    assert_eq!(w.get_height_at(None, 10.0, 14.0, 77.0), 0.0);
    // cell data accessors
    assert_eq!(
        t.get_height_map_data_in_cell(0, 0, 512, 0),
        Err(HeightError::IndexOutOfRange)
    );
    assert_eq!(t.get_height_map_data_in_cell(0, 0, 513, 0), Ok(0.0));
    assert_eq!(
        t.get_cell_height(0, 0, 10, 14).unwrap(),
        t.get_raw_height_map_height(&f, 10, 14).unwrap()
    );
}

#[test]
fn disabled_and_reference_height() {
    let mut w = synthetic();
    let terrain = w.get_height(100, 10.0, 14.0, 0.0);
    // no AI -> terrain
    assert_eq!(w.get_reference_height(None, 10.0, 14.0, 50.0, 100), terrain);
    let ai = ReferenceHeightAi {
        can_fly: false,
        hold_or_idle: false,
        spawner_z: Some(33.0),
        world_z: 40.0,
    };
    assert_eq!(
        w.get_reference_height(Some(&ai), 10.0, 14.0, 50.0, 100),
        terrain
    );
    // terrain 0 -> spawner
    assert_eq!(
        w.get_reference_height(Some(&ai), 1500.0, 14.0, 50.0, 100),
        33.0
    );
    let fly = ReferenceHeightAi {
        can_fly: true,
        ..ai
    };
    assert_eq!(
        w.get_reference_height(Some(&fly), 10.0, 14.0, 50.0, 100),
        33.0
    );
    let idle = ReferenceHeightAi {
        hold_or_idle: true,
        spawner_z: None,
        ..ai
    };
    assert_eq!(
        w.get_reference_height(Some(&idle), 10.0, 14.0, 50.0, 100),
        40.0
    );
    // HeightMapsEnable = false: GetHeight is 0 (C# returns before reading heightmaps)
    w.config.heightmaps_enable = false;
    assert_eq!(w.get_height(100, 10.0, 14.0, 0.0), 0.0);
    assert_eq!(w.get_reference_height(None, 10.0, 14.0, 50.0, 100), 0.0);
    assert_eq!(w.get_height_at(Some(0), 10.0, 14.0, 9.0), 9.0);
}

fn node_bytes(
    version: u8,
    n_size: i32,
    offset: f32,
    range: f32,
    data: &[u16],
    min_xy: (f32, f32),
) -> Vec<u8> {
    let mut v = vec![version, 0, 0, 0];
    for f in [min_xy.0, min_xy.1, 0.0, 1.0, 1.0, 1.0] {
        v.extend_from_slice(&f32::to_le_bytes(f));
    }
    v.push(0);
    v.extend_from_slice(&offset.to_le_bytes());
    v.extend_from_slice(&range.to_le_bytes());
    v.extend_from_slice(&n_size.to_le_bytes());
    v.extend_from_slice(&2i32.to_le_bytes()); // unkCount
    for d in data {
        v.extend_from_slice(&d.to_le_bytes());
    }
    v.extend_from_slice(&[0u8; 4 + 16]);
    v.extend_from_slice(&[0u8; 36 + 2]);
    v
}

fn file(nodes: &[Vec<u8>]) -> Vec<u8> {
    file_with(23, None, nodes)
}

fn file_with(version: u8, table: Option<[u32; 32]>, nodes: &[Vec<u8>]) -> Vec<u8> {
    let mut f = vec![version, 0, 0, 0];
    let table_len = if version >= 24 { 128 } else { 0 };
    let chunk = 4 + 5 * 4 + 8 + table_len + nodes.iter().map(Vec::len).sum::<usize>();
    f.extend_from_slice(&(chunk as i32).to_le_bytes());
    for v in [0i32, 0, 0, 0] {
        f.extend_from_slice(&v.to_le_bytes());
    }
    f.extend_from_slice(&1.5f32.to_le_bytes());
    f.extend_from_slice(&0.0f32.to_le_bytes());
    if let Some(table) = table {
        for value in table {
            f.extend_from_slice(&value.to_le_bytes());
        }
    }
    for n in nodes {
        f.extend_from_slice(n);
    }
    f
}

#[test]
fn version_24_keeps_the_32_entry_surface_table_without_moving_the_nodes() {
    let table = std::array::from_fn(|i| 1000 + i as u32);
    let n1 = node_bytes(7, 2, 10.0, 0.01, &[16, 32, 48, 64], (5.0, 0.0));
    let n2 = node_bytes(7, 0, 0.0, 0.0, &[], (1.0, 0.0));
    let with_table = file_with(24, Some(table), &[n1.clone(), n2.clone()]);
    let without = file_with(23, None, &[n1, n2]);
    let a = hmap::Hmap::read(&with_table, false).unwrap().unwrap();
    let b = hmap::Hmap::read(&without, false).unwrap().unwrap();
    assert_eq!(a.surface_table, Some(table));
    assert_eq!(b.surface_table, None);
    // The 128-byte table advances the reader exactly like the old skip: same nodes, heights
    // and surfaces.
    assert_eq!(a.nodes.len(), b.nodes.len());
    for (na, nb) in a.nodes.iter().zip(b.nodes.iter()) {
        assert_eq!(na.n_size, nb.n_size);
        assert_eq!(na.p_hm_data, nb.p_hm_data);
        assert_eq!(na.surface, nb.surface);
        // A sample in the node's range decodes to the same height.
        let i = na.p_hm_data.len().saturating_sub(1) as u32;
        assert_eq!(na.get_height_by_index(i), nb.get_height_by_index(i));
    }
    // A truncated table keeps the old clamping: the table stays, the nodes fail to read.
    let short = &with_table[..40];
    let h = hmap::Hmap::read(short, false).unwrap();
    assert!(h.is_none());
}

/// The original client's resampling rule
/// (`AABevy1.2/crates/aa-world/src/heightmap.rs:240-255`, `sample_node`), in raw
/// `data >> 4` units: target `i` -> source `i * (size - 1) / 32`, bilinear over the node's
/// own samples, upper index clamped to `size - 2` (the far edge is the last sample at
/// fraction 1). Independent of the implementation (h-dec units, no height decode).
fn client_upscaled_hdec(data: &[u16], size: usize, x: usize, y: usize) -> f32 {
    let s = (size - 1) as f32 / 32.0;
    let (fx, fy) = (x as f32 * s, y as f32 * s);
    let (ix, iy) = (
        (fx.floor() as usize).min(size - 2),
        (fy.floor() as usize).min(size - 2),
    );
    let (tx, ty) = (fx - ix as f32, fy - iy as f32);
    let h = |a: usize, b: usize| f32::from(data[a * size + b] >> 4);
    let top = h(ix, iy) * (1.0 - tx) + h(ix + 1, iy) * tx;
    let bottom = h(ix, iy + 1) * (1.0 - tx) + h(ix + 1, iy + 1) * tx;
    top * (1.0 - ty) + bottom * ty
}

#[test]
fn low_resolution_nodes_resample_like_the_client() {
    // Sizes 2, 5, 9 and 17 (the low-resolution nodes of the 1.2 data). The node keeps its
    // own samples and `get_height` interpolates them in f32, so the only difference to the
    // rule is the source samples' existing 5 cm decode.
    for size in [2usize, 5, 9, 17] {
        let data: Vec<u16> = (0..size * size)
            .map(|i| ((i as u16 * 37 + 8) % 7 * 32) << 4)
            .collect();
        // Version 7, offset 0, range 0.0001 -> i_step 1, so raw >> 4 is the sample value.
        let n1 = node_bytes(7, size as i32, 0.0, 0.0001, &data, (0.0, 0.0));
        let n2 = node_bytes(7, 0, 0.0, 0.0, &[], (1.0, 0.0));
        let file = file(&[n1, n2]);
        let h = hmap::Hmap::read(&file, false).unwrap().unwrap();
        let n = h.sorted_nodes()[0];
        assert_eq!(n.n_size, size as i32);
        // The node is not upscaled up front; the samples stay as read.
        assert_eq!(n.p_hm_data, data);
        for x in 0..33usize {
            for y in 0..33usize {
                let want = client_upscaled_hdec(&data, size, x, y) * 0.05;
                let got = n.get_height(x as u16, y as u16).unwrap();
                assert!(
                    (got - want).abs() <= 1e-3,
                    "size {size} height ({x}, {y}): {got}, rule {want}"
                );
            }
        }
        // The sample lattice: every source sample maps to its own grid target.
        for i in 0..size {
            let target = (i * 32) / (size - 1);
            assert_eq!(
                n.get_height(target as u16, 0).unwrap(),
                f32::from(data[i * size] >> 4) * 0.05,
                "size {size} lattice x {i}"
            );
            assert_eq!(
                n.get_height(0, target as u16).unwrap(),
                f32::from(data[i] >> 4) * 0.05,
                "size {size} lattice y {i}"
            );
        }
        // The far edge is the last sample (fraction 1), not a duplicated inner row.
        assert_eq!(
            n.get_height(32, 32).unwrap(),
            f32::from(data[(size - 1) * size + size - 1] >> 4) * 0.05
        );
    }
    // The errors of the 2 x 2 node test are unchanged by the resampling.
    let n1 = node_bytes(7, 2, 10.0, 0.01, &[16, 32, 48, 64], (5.0, 0.0));
    let n2 = node_bytes(7, 0, 0.0, 0.0, &[], (1.0, 0.0));
    let data = file(&[n1, n2]);
    // truncated file: a node fails -> Read returns -1 (Ok(None))
    assert!(hmap::Hmap::read(&data[..data.len() - 1], false)
        .unwrap()
        .is_none());
    // truncated header -> exception
    assert!(hmap::Hmap::read(&data[..10], false).is_err());
}

/// A client cell whose samples are at 1551.7 m must survive the u16 grid. The C#
/// `HeightMaxCoefficient` (`65535 / (4096 / 4)`) could only store up to 1024 m, so every
/// sample above it read back as 1024.0; the port's coefficient covers 0..=4096 m.
#[test]
fn heights_above_1024_m_round_trip_through_the_grid() {
    use crate::data::pak::test_util::temp_path;
    use crate::data::world_templates::WorldTemplate;

    let root = temp_path("heightmap_above_1024");
    let cell_dir = root.join("game/worlds/test_world/cells/000_000/client/terrain");
    std::fs::create_dir_all(&cell_dir).unwrap();
    // Full 33 x 33 sector nodes. `f_offset` 1500 + `f_range` 0.001 gives 0.05 m steps
    // (`i_step` 1), so raw 0x40A0 decodes to 1500 + 1034 * 0.05 = 1551.7 m.
    let data = vec![0x40A0u16; 33 * 33];
    let nodes: Vec<Vec<u8>> = (0..SECTORS_PER_CELL * SECTORS_PER_CELL)
        .map(|i| {
            let (sx, sy) = (i / SECTORS_PER_CELL, i % SECTORS_PER_CELL);
            node_bytes(7, 33, 1500.0, 0.001, &data, (sx as f32, sy as f32))
        })
        .collect();
    std::fs::write(cell_dir.join("heightmap.dat"), file(&nodes)).unwrap();
    let files = ClientFileManager::from_sources(&[root.to_string_lossy().into_owned()], &root);
    let template = WorldTemplate::synthetic(0, "test_world", 1, 1, 100);
    let t = WorldHeightmap::new(&template);

    // One u16 step is 1 / (65535 / 4096) m = 6.25 cm.
    let tolerance = (1.0 / COEF) as f32;
    let raw = t.get_raw_height_map_height(&files, 0, 0).unwrap();
    assert!((raw - 1551.7).abs() < tolerance, "{raw}");
    assert!((t.grid_sample(&files, 0, 0) - 1551.7).abs() < tolerance);
    assert!((t.get_height(&files, 0.0, 0.0).unwrap() - 1551.7).abs() < tolerance);
    std::fs::remove_dir_all(root).ok();
}

/// A sample exactly half a storage step (`2048 * (65535 / 4096)` = 32767.5) must round up
/// to 32768. The C# truncating cast stored 32767 and read 2047.97 m instead of 2048.03 m.
#[test]
fn height_at_half_a_storage_step_rounds_up() {
    use crate::data::pak::test_util::temp_path;
    use crate::data::world_templates::WorldTemplate;

    let root = temp_path("heightmap_half_step");
    let cell_dir = root.join("game/worlds/test_world/cells/000_000/client/terrain");
    std::fs::create_dir_all(&cell_dir).unwrap();
    // `f_offset` 2048 decodes every sample to exactly 2048.0 m (raw 0, `i_step` 1).
    let data = vec![0u16; 33 * 33];
    let nodes: Vec<Vec<u8>> = (0..SECTORS_PER_CELL * SECTORS_PER_CELL)
        .map(|i| {
            let (sx, sy) = (i / SECTORS_PER_CELL, i % SECTORS_PER_CELL);
            node_bytes(7, 33, 2048.0, 0.001, &data, (sx as f32, sy as f32))
        })
        .collect();
    std::fs::write(cell_dir.join("heightmap.dat"), file(&nodes)).unwrap();
    let files = ClientFileManager::from_sources(&[root.to_string_lossy().into_owned()], &root);
    let template = WorldTemplate::synthetic(0, "test_world", 1, 1, 100);
    let t = WorldHeightmap::new(&template);

    // 2048 * (65535 / 4096) = 32767.5 exactly: rounding stores 32768, truncation 32767.
    assert_eq!(f64::from(2048.0f32) * COEF, 32767.5);
    let raw = t.get_raw_height_map_height(&files, 0, 0).unwrap();
    assert_eq!(raw, (32768.0 / COEF) as f32);
    std::fs::remove_dir_all(root).ok();
}
