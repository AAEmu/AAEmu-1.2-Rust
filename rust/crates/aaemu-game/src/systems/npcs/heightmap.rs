//! `WorldCell` heightmaps and the height queries of `WorldTemplate` / `WorldInstance` /
//! `WorldManager` (`GetRawHeightMapHeight`, `GetHeight(x, y)`, `GetHeight(zoneKey, x, y, z)`,
//! `GetHeight(transform)`, `GetReferenceHeight`) plus `WorldManager.LoadHeightmaps`.
//!
//! The per-cell `client/terrain/heightmap.dat` reader ([`hmap`]) is the game's own
//! `Models/ClientData/{Hmap,NodeCell,AABB}.cs`; the binary reader and the bit-exact arithmetic
//! were taken from `aaemu-tool-worldconverter/src/hmap.rs` (port of
//! `Tools/WorldConverter/Models/Hmap.cs`). The C# `UpScale` quirk is replaced by the original
//! client's own resampling rule (see `NodeCell::get_height`, deviation in `PORTING.md`).
//!
//! Cells are loaded on demand (`WorldCell.VerifyCellLoaded`) from the client sources
//! (`ClientData.Sources`, read through [`ClientFileManager`]: the 24 GB game_pak is never
//! scanned unless `World.PreLoadTerrain` is set). With `GeoDataMode` the navmesh height of the
//! AI subsystem (`systems::ai::geodata`, installed with [`HeightmapWorlds::install_geo_data`])
//! is asked first, like `WorldTemplate.GeoData.GetHeight`. `object.dat` is not read here.
//!
//! C# exceptions inside the height lookups (`IndexOutOfRangeException` for the `cellX > CellX`
//! bound check, negative indices, a failing cell load) are [`HeightError`]s; the callers that
//! catch them in C# (`WorldManager.GetHeight`) turn them into the same fallback values.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use tracing::{debug, error, info};

use crate::data::client_files::ClientFileManager;
use crate::data::world_templates::{WorldTemplate, WorldTemplates, CELL_SIZE, SECTORS_PER_CELL};
use crate::server::GameServer;
use crate::systems::npcs::terrain_surface::{SurfaceMaterial, WorldSurfaceTable};
use crate::world::{WorldInstance, WorldObject};

/// `WorldManager.SECTOR_HMAP_RESOLUTION` (`REGION_SIZE / 2`).
pub const SECTOR_HMAP_RESOLUTION: i32 = 32;
/// `WorldManager.CELL_HMAP_RESOLUTION` (`CELL_SIZE / 2`).
pub const CELL_HMAP_RESOLUTION: i32 = CELL_SIZE / 2;

const CELL_POINTS: usize = (CELL_HMAP_RESOLUTION * CELL_HMAP_RESOLUTION) as usize;

/// An exception thrown inside a C# height lookup.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum HeightError {
    /// `IndexOutOfRangeException` (cell / heightmap array index).
    #[error("Index was outside the bounds of the array.")]
    IndexOutOfRange,
    /// An exception escaping `WorldCell.LoadCellHeightMapFromClientData`.
    #[error("loading heightmap cell {cell_x},{cell_y} of {world} failed: {reason}")]
    CellLoad {
        world: String,
        cell_x: i32,
        cell_y: i32,
        reason: String,
    },
}

// ---------------------------------------------------------------------------------------------
// Models/ClientData: Hmap / NodeCell / AABB
// ---------------------------------------------------------------------------------------------

/// `Models/ClientData/Hmap.cs` + `NodeCell.cs` + `AABB.cs` (the game server's copies).
///
/// Reader and arithmetic adapted from `aaemu-tool-worldconverter/src/hmap.rs` (port of
/// `Tools/WorldConverter/Models/Hmap.cs`). `UpScale` is **not** the C# one: a low-resolution
/// node keeps its own samples and `get_height` resamples it bilinearly in f32
/// (`AABevy1.2/crates/aa-world/src/heightmap.rs:240-255`), which the original-client recheck
/// proved right (`D:/aaemu-audits/terrain-3way-2026-11-08/REPORT.md`). The trailing-bytes
/// handling still follows the game's `NodeCell.cs`. All operand types mirror the C# (float vs
/// double vs integer promotions, saturating casts of .NET 9+).
pub mod hmap {
    use std::cmp::Ordering;

    /// An exception of `BinaryReader` / `NodeCell.Read`.
    #[derive(Debug, Clone, PartialEq, thiserror::Error)]
    pub enum HmapError {
        #[error("Unable to read beyond the end of the stream.")]
        EndOfStream,
        #[error("Arithmetic operation resulted in an overflow. (negative node size {0})")]
        NegativeSize(i32),
        #[error("Non-negative number required. (ReadBytes({0}))")]
        NegativeCount(i32),
        #[error("Index was outside the bounds of the array.")]
        IndexOutOfRange,
    }

    /// `BinaryReader` over a byte slice.
    pub struct Reader<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl<'a> Reader<'a> {
        pub fn new(data: &'a [u8]) -> Self {
            Self { data, pos: 0 }
        }

        pub fn position(&self) -> usize {
            self.pos
        }

        fn remaining(&self) -> usize {
            self.data.len() - self.pos
        }

        fn take<const N: usize>(&mut self) -> Result<[u8; N], HmapError> {
            let s = self
                .data
                .get(self.pos..self.pos + N)
                .ok_or(HmapError::EndOfStream)?;
            self.pos += N;
            let mut out = [0u8; N];
            out.copy_from_slice(s);
            Ok(out)
        }

        fn u8(&mut self) -> Result<u8, HmapError> {
            Ok(self.take::<1>()?[0])
        }
        fn u16(&mut self) -> Result<u16, HmapError> {
            Ok(u16::from_le_bytes(self.take()?))
        }
        fn i32(&mut self) -> Result<i32, HmapError> {
            Ok(i32::from_le_bytes(self.take()?))
        }
        fn f32(&mut self) -> Result<f32, HmapError> {
            Ok(f32::from_le_bytes(self.take()?))
        }

        /// `BinaryReader.ReadBytes(count)`: fewer bytes at the end of the stream (no error), a
        /// negative count throws.
        fn read_bytes(&mut self, count: i32) -> Result<(), HmapError> {
            if count < 0 {
                return Err(HmapError::NegativeCount(count));
            }
            self.pos = (self.pos + count as usize).min(self.data.len());
            Ok(())
        }

        /// The 32-entry world-surface id table at file offset 32 (`version >= 24`). It advances
        /// exactly the 128 bytes the reader used to skip; a short table is clamped to the end of
        /// the stream like `ReadBytes` and the missing entries stay 0.
        fn read_surface_table(&mut self) -> [u32; 32] {
            let start = self.pos;
            let _ = self.read_bytes(128);
            let mut table = [0u32; 32];
            for (i, value) in table.iter_mut().enumerate() {
                let at = start + i * 4;
                if let Some(bytes) = self.data.get(at..at + 4) {
                    *value = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                }
            }
            table
        }
    }

    /// `AABB`.
    #[derive(Debug, Clone, Copy, Default, PartialEq)]
    pub struct Aabb {
        pub min: [f32; 3],
        pub max: [f32; 3],
    }

    /// `Hmap`.
    #[derive(Debug, Clone, Default)]
    pub struct Hmap {
        pub version: u8,
        pub dummy: u8,
        pub flags: u8,
        pub flags2: u8,
        pub chunk_size: i32,
        pub height_map_size_in_units: i32,
        pub unit_size_in_meters: i32,
        pub sector_size_in_meters: i32,
        pub sectors_table_size_in_sectors: i32,
        pub heightmap_z_ratio: f32,
        pub ocean_water_level: f32,
        /// The 32 world-surface ids of the cell (`heightmap.dat` offset 32..159,
        /// `version >= 24`). The reader used to skip these 128 bytes; the byte count is
        /// unchanged. `None` before version 24.
        pub surface_table: Option<[u32; 32]>,
        pub nodes: Vec<NodeCell>,
    }

    impl Hmap {
        /// `Hmap.Read`. `Err` = an exception from the header reads (it escapes `Read`);
        /// `Ok(None)` = `Read` returned `-1` (a node failed to read; the C# catches that);
        /// `Ok(Some)` = success.
        pub fn read(data: &[u8], disabled_re_calc: bool) -> Result<Option<Self>, HmapError> {
            let mut br = Reader::new(data);
            let mut h = Hmap {
                version: br.u8()?,
                dummy: br.u8()?,
                flags: br.u8()?,
                flags2: br.u8()?,
                chunk_size: br.i32()?,
                height_map_size_in_units: br.i32()?,
                unit_size_in_meters: br.i32()?,
                sector_size_in_meters: br.i32()?,
                sectors_table_size_in_sectors: br.i32()?,
                heightmap_z_ratio: br.f32()?,
                ocean_water_level: br.f32()?,
                surface_table: None,
                nodes: Vec::new(),
            };
            if h.version >= 24 {
                h.surface_table = Some(br.read_surface_table());
            }
            while i64::try_from(br.position()).unwrap_or(i64::MAX) != i64::from(h.chunk_size) {
                match NodeCell::read(&mut br, disabled_re_calc) {
                    Ok(node) => h.nodes.push(node),
                    Err(_) => return Ok(None),
                }
            }
            Ok(Some(h))
        }

        /// `Nodes.OrderBy(BoxHeightmap.Min.X).ThenBy(BoxHeightmap.Min.Y)
        /// .Where(pHMData.Length > 0)` (stable, .NET float ordering).
        pub fn sorted_nodes(&self) -> Vec<&NodeCell> {
            let mut nodes: Vec<&NodeCell> = self.nodes.iter().collect();
            nodes.sort_by(|a, b| {
                dotnet_cmp_f32(a.box_heightmap.min[0], b.box_heightmap.min[0])
                    .then_with(|| dotnet_cmp_f32(a.box_heightmap.min[1], b.box_heightmap.min[1]))
            });
            nodes.retain(|n| !n.p_hm_data.is_empty());
            nodes
        }
    }

    /// `Comparer<float>.Default`: NaN sorts before everything else.
    pub fn dotnet_cmp_f32(a: f32, b: f32) -> Ordering {
        match (a.is_nan(), b.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
        }
    }

    /// The surface id of a terrain hole.
    pub const HOLE_SURFACE_ID: u8 = 31;
    const INV_5CM: i32 = 20;
    const MASK_12BIT: u32 = (1 << 12) - 1;

    /// `NodeCell`.
    #[derive(Debug, Clone, Default)]
    pub struct NodeCell {
        pub version: u8,
        pub dummy: u8,
        pub flags: u8,
        pub flags2: u8,
        pub box_heightmap: Aabb,
        pub b_has_holes: u8,
        pub f_offset: f32,
        pub f_range: f32,
        pub n_size: i32,
        pub p_hm_data: Vec<u16>,
        /// The surface id of each sample of the 33 x 33 sector grid (`x * 33 + y`, like
        /// `GetHeight`): `raw & 0x1F` (`raw & 0xF` from node version 7), 31 = terrain hole.
        /// Empty without data. Taken from the file's raw samples, before `RescaleToInt`
        /// drops the fifth surface bit.
        pub surface: Vec<u8>,
        i_offset: i32,
        i_range: i32,
        i_step: i32,
        f_min: f32,
        f_max: f32,
    }

    impl NodeCell {
        /// `NodeCell.Read`.
        pub fn read(br: &mut Reader<'_>, disabled_re_calc: bool) -> Result<Self, HmapError> {
            let mut n = NodeCell {
                version: br.u8()?,
                dummy: br.u8()?,
                flags: br.u8()?,
                flags2: br.u8()?,
                ..Default::default()
            };
            n.box_heightmap.min = [br.f32()?, br.f32()?, br.f32()?];
            n.box_heightmap.max = [br.f32()?, br.f32()?, br.f32()?];
            n.b_has_holes = br.u8()?;
            n.f_offset = br.f32()?;
            n.f_range = br.f32()?;
            n.n_size = br.i32()?;
            // new ushort[nSize * nSize] (int multiplication wraps; a negative length throws)
            let len = n.n_size.wrapping_mul(n.n_size);
            if len < 0 {
                return Err(HmapError::NegativeSize(n.n_size));
            }
            let unk_count = br.i32()?;
            // The reads below would hit the end of the stream: fail before allocating.
            if (len as usize).saturating_mul(2) > br.remaining() {
                return Err(HmapError::EndOfStream);
            }
            n.p_hm_data = (0..len).map(|_| br.u16()).collect::<Result<_, _>>()?;
            br.i32()?;
            br.f32()?;
            br.f32()?;
            br.f32()?;
            br.f32()?;
            br.read_bytes(36i32.wrapping_add(unk_count))?;

            n.surface = n.read_surface();
            n.init();
            if !disabled_re_calc && n.version < 7 {
                n.rescale_to_int();
            }
            Ok(n)
        }

        /// The surface ids of the 33 x 33 sector grid. Native `IsHole` (`cry3dengine` 0x300CAB30)
        /// takes the containing sector's lower sample; a node with fewer than 33 samples per side
        /// (spacing `64 / (size - 1)` m) is read at `floor(i * (size - 1) / 32)`, not rounded.
        pub(crate) fn read_surface(&self) -> Vec<u8> {
            let size = self.n_size;
            if !(2..=33).contains(&size) {
                return Vec::new();
            }
            let mask: u16 = if self.version < 7 { 0x1F } else { 0xF };
            let size = size as usize;
            let scale = (size - 1) as f32 / 32.0;
            let at = |i: usize| ((i as f32 * scale).floor() as usize).min(size - 1);
            let mut out = Vec::with_capacity(33 * 33);
            for x in 0..33 {
                for y in 0..33 {
                    out.push((self.p_hm_data[at(x) * size + at(y)] & mask) as u8);
                }
            }
            out
        }

        /// Whether sample `(unit_x, unit_y)` of the sector (0..=32) is a terrain hole.
        pub fn is_hole_sample(&self, unit_x: i32, unit_y: i32) -> bool {
            usize::try_from(unit_x * 33 + unit_y)
                .ok()
                .filter(|_| (0..=32).contains(&unit_x) && (0..=32).contains(&unit_y))
                .and_then(|i| self.surface.get(i))
                .is_some_and(|&s| s == HOLE_SURFACE_ID)
        }

        /// `RawDataToHeight`.
        pub fn raw_data_to_height(&self, data: u32) -> f32 {
            0.05f32 * self.i_offset as f32
                + (i64::from(data >> 4) * i64::from(self.i_step)) as f32 * 0.05f32
        }

        /// `RawDataByIndex(ushort, ushort)`: `pHMData[nX * nSize + nY]`, 0 when past the end.
        pub fn raw_data_by_index(&self, n_x: u16, n_y: u16) -> u16 {
            if self.n_size > 0 {
                let index = i32::from(n_x)
                    .wrapping_mul(self.n_size)
                    .wrapping_add(i32::from(n_y));
                if index as i64 >= self.p_hm_data.len() as i64 {
                    return 0;
                }
                return self.p_hm_data[index as usize];
            }
            0
        }

        /// `GetHeightByIndex(uint)`.
        pub fn get_height_by_index(&self, i: u32) -> Result<f32, HmapError> {
            let raw = self
                .p_hm_data
                .get(i as usize)
                .copied()
                .ok_or(HmapError::IndexOutOfRange)?;
            Ok(self.raw_data_to_height(u32::from(raw)))
        }

        /// `GetHeight(nX, nY)`. A full-resolution node holds 33 x 33 samples (x-major). A
        /// low-resolution node keeps its `size x size` samples and is resampled on read exactly
        /// like the original 1.2 client (`AABevy1.2/crates/aa-world/src/heightmap.rs:240-255`,
        /// `sample_node`): target `i` maps to source `i * (size - 1) / 32`, bilinear over the
        /// node's own samples in f32, with the upper index clamped to `size - 2` (the far edge
        /// is the last sample at fraction 1). The C# upscaled the node up front and then read
        /// the 33-wide result with the wrong stride (it reused `nSize`), so low-resolution
        /// sectors came out mirrored (deviation from C#, see `PORTING.md`). A 1 x 1 node (not
        /// present in the 1.2 data; the client treats it as invalid) is our choice to cover the
        /// sector with its one height.
        pub fn get_height(&self, n_x: u16, n_y: u16) -> Result<f32, HmapError> {
            if self.p_hm_data.len() == 33 * 33 {
                let index = i32::from(n_x).wrapping_mul(33).wrapping_add(i32::from(n_y));
                return self.get_height_by_index(index as u32);
            }
            if self.n_size <= 0 {
                return Ok(0.0);
            }
            if n_x > 32 || n_y > 32 {
                return Err(HmapError::IndexOutOfRange);
            }
            if self.n_size == 1 {
                return self.get_height_by_index(0);
            }
            let n_size = self.n_size as usize;
            if self.p_hm_data.len() != n_size * n_size {
                return Err(HmapError::IndexOutOfRange);
            }
            let source_scale = (n_size - 1) as f32 / 32.0;
            let (fx, fy) = (f32::from(n_x) * source_scale, f32::from(n_y) * source_scale);
            let (ix, iy) = (
                (fx.floor() as usize).min(n_size - 2),
                (fy.floor() as usize).min(n_size - 2),
            );
            let (tx, ty) = (fx - ix as f32, fy - iy as f32);
            let h = |x: usize, y: usize| {
                self.raw_data_to_height(u32::from(self.p_hm_data[x * n_size + y]))
            };
            let top = Self::lerp(h(ix, iy), h(ix + 1, iy), tx);
            let bottom = Self::lerp(h(ix, iy + 1), h(ix + 1, iy + 1), tx);
            Ok(Self::lerp(top, bottom, ty))
        }

        fn init(&mut self) {
            self.f_min = self.f_offset;
            self.f_max = self.f_min + 0xFFF0 as f32 * self.f_range;
            self.i_offset = (self.f_min * INV_5CM as f32) as i32;
            self.i_range = ((self.f_max - self.f_min) * INV_5CM as f32) as i32;
            self.i_step = if self.i_range > 0 {
                (i64::from(self.i_range) + i64::from(MASK_12BIT) - 1) / i64::from(MASK_12BIT)
            } else {
                1
            } as i32;
        }

        fn rescale_to_int(&mut self) {
            for i in 0..self.p_hm_data.len() {
                let hraw = self.p_hm_data[i];
                let height = self.f_min + (0xFFF0 & i32::from(hraw)) as f32 * self.f_range;
                let scaled = ((height - self.f_min) * INV_5CM as f32) as i32;
                // iStep >= 1 here (C# would throw DivideByZeroException otherwise)
                let hdec = scaled.wrapping_div(self.i_step.max(1)) as u16;
                let res = (i32::from(hraw) & 0xF) | (i32::from(hdec) << 4);
                self.p_hm_data[i] = res as u16;
            }
        }

        fn lerp(s: f32, e: f32, t: f32) -> f32 {
            s + (e - s) * t
        }
    }
}

// ---------------------------------------------------------------------------------------------
// WorldCell
// ---------------------------------------------------------------------------------------------

/// Result of `WorldCell.LoadCellHeightMapFromClientData`.
#[derive(Debug)]
enum CellLoad {
    /// Returned `true`: the heightmap (`None` = all zeros: no file in the client data), the
    /// terrain-hole bitmap (`None` = no holes), the 32-entry world-surface id table (`None`
    /// before `heightmap.dat` version 24) and the per-sample local surface ids (`None` unless
    /// the strict placement surface check keeps them).
    Loaded(
        Option<Box<[u16]>>,
        Option<Box<[u64]>>,
        Option<Box<[u32; 32]>>,
        Option<Box<[u8]>>,
    ),
    /// Returned `false` (`Hmap.Read` failed): `Loaded` stays false, the zeroed array is used
    /// and the next lookup tries again.
    Failed,
    /// An exception escaped the load.
    Exception(String),
}

/// The u16 grid value of a height sample: `round(height * coefficient)`, saturating at
/// `u16::MAX`. The C# truncated `(ushort)(height * coefficient)`, which biased every sample
/// down by up to one step (6.25 cm in `main_world`); rounding keeps the grid centred on the
/// client's samples. `max_height * coefficient` is exactly `u16::MAX`, so it still maps there.
fn encode_height(height: f32, coefficient: f64) -> u16 {
    (f64::from(height) * coefficient).round() as u16
}

/// `WorldCell.LoadCellHeightMapFromClientData` (heightmap part).
fn load_cell_height_map(
    files: &ClientFileManager,
    world_name: &str,
    cell_x: i32,
    cell_y: i32,
    height_max_coefficient: f64,
    keep_surface: bool,
) -> CellLoad {
    let file = format!(
        "game/worlds/{world_name}/cells/{cell_x:03}_{cell_y:03}/client/terrain/heightmap.dat"
    );
    if !files.file_exists(&file) {
        return CellLoad::Loaded(None, None, None, None);
    }
    let Some(bytes) = files.read_file(&file) else {
        return CellLoad::Loaded(None, None, None, None);
    };
    let hmap = match hmap::Hmap::read(&bytes, false) {
        Ok(Some(h)) => h,
        Ok(None) => {
            error!("Error reading {file}");
            return CellLoad::Failed;
        }
        Err(e) => return CellLoad::Exception(e.to_string()),
    };
    let surface_table = hmap.surface_table.map(Box::new);
    let sorted = hmap.sorted_nodes();
    let mut heights = vec![0u16; CELL_POINTS].into_boxed_slice();
    let mut holes: Option<Box<[u64]>> = None;
    let mut surface_ids = keep_surface.then(|| vec![0u8; CELL_POINTS].into_boxed_slice());
    for sector_x in 0..SECTORS_PER_CELL {
        for sector_y in 0..SECTORS_PER_CELL {
            // sortedNodes[sectorX * SECTORS_PER_CELL + sectorY] (ArgumentOutOfRange when short)
            let Some(node) = sorted.get((sector_x * SECTORS_PER_CELL + sector_y) as usize) else {
                return CellLoad::Exception(format!(
                    "only {} heightmap nodes in {file}",
                    sorted.len()
                ));
            };
            for unit_x in 0..SECTOR_HMAP_RESOLUTION {
                for unit_y in 0..SECTOR_HMAP_RESOLUTION {
                    let o_x = sector_x * SECTOR_HMAP_RESOLUTION + unit_x;
                    let o_y = sector_y * SECTOR_HMAP_RESOLUTION + unit_y;
                    let height = match node.get_height(unit_x as u16, unit_y as u16) {
                        Ok(h) => h,
                        Err(e) => return CellLoad::Exception(format!("{e} ({file})")),
                    };
                    // (ushort)(height * Template.HeightMaxCoefficient), but rounded instead of
                    // truncated (deviation from C#, see PORTING.md). `HeightMaxCoefficient`
                    // covers the whole 0..=MaxHeight range, so only samples above the world's
                    // maximum height saturate.
                    let value = encode_height(height, height_max_coefficient);
                    heights[(o_x * CELL_HMAP_RESOLUTION + o_y) as usize] = value;
                    if node.is_hole_sample(unit_x, unit_y) {
                        let bit = (o_x * CELL_HMAP_RESOLUTION + o_y) as usize;
                        let words = holes
                            .get_or_insert_with(|| vec![0u64; CELL_POINTS.div_ceil(64)].into());
                        words[bit / 64] |= 1 << (bit % 64);
                    }
                    if let Some(ids) = surface_ids.as_mut() {
                        if let Some(&local) = node.surface.get((unit_x * 33 + unit_y) as usize) {
                            ids[(o_x * CELL_HMAP_RESOLUTION + o_y) as usize] = local;
                        }
                    }
                }
            }
        }
    }
    CellLoad::Loaded(Some(heights), holes, surface_table, surface_ids)
}

/// `WorldCell` (heightmap part).
#[derive(Debug, Default)]
struct CellSlot {
    /// `Loaded` + `HeightMap` (`None` inside = all zeros).
    loaded: OnceLock<Option<Box<[u16]>>>,
    /// Terrain holes of the cell (bit `x * 512 + y`, set before `loaded`); `None` = none.
    holes: OnceLock<Option<Box<[u64]>>>,
    /// The cell's 32-entry world-surface id table (`heightmap.dat` offset 32..159); `None`
    /// before version 24 or without a heightmap.
    surface_table: OnceLock<Option<Box<[u32; 32]>>>,
    /// Per-sample local surface ids (512 x 512, x-major); kept only when
    /// [`HeightmapConfig::placement_strict_surface`] is on (`None` otherwise). 256 KiB per
    /// loaded cell.
    surface_ids: OnceLock<Option<Box<[u8]>>>,
    /// `_loadLock`.
    load_lock: Mutex<()>,
}

/// The heightmap side of one `WorldTemplate`: its `WorldCell[CellX, CellY]`.
#[derive(Debug)]
pub struct WorldHeightmap {
    pub template_id: u32,
    pub name: String,
    /// World size in cells.
    pub cell_x: i32,
    pub cell_y: i32,
    /// `HeightMaxCoefficient`.
    pub height_max_coefficient: f64,
    /// Whether the per-sample surface ids are kept for the strict placement surface check
    /// (`Housing.PlacementStrictSurface`).
    keep_surface: bool,
    /// `surface.xml` + detail `.mtl` of this world, parsed lazily on the first placement check.
    surfaces: OnceLock<Option<WorldSurfaceTable>>,
    /// `Cells[x, y]` at `x * cell_y + y`.
    cells: Vec<CellSlot>,
}

impl WorldHeightmap {
    pub fn new(template: &WorldTemplate) -> Self {
        Self::new_with_surface(template, false)
    }

    /// [`new`](Self::new) with the strict placement surface ids enabled.
    pub fn new_with_surface(template: &WorldTemplate, keep_surface: bool) -> Self {
        Self::with_size_surface(
            template.id,
            &template.name,
            template.cell_x,
            template.cell_y,
            template.height_max_coefficient,
            keep_surface,
        )
    }

    pub fn with_size(
        template_id: u32,
        name: &str,
        cell_x: i32,
        cell_y: i32,
        height_max_coefficient: f64,
    ) -> Self {
        Self::with_size_surface(
            template_id,
            name,
            cell_x,
            cell_y,
            height_max_coefficient,
            false,
        )
    }

    /// [`with_size`](Self::with_size) with the strict placement surface ids enabled.
    pub fn with_size_surface(
        template_id: u32,
        name: &str,
        cell_x: i32,
        cell_y: i32,
        height_max_coefficient: f64,
        keep_surface: bool,
    ) -> Self {
        let count = (cell_x.max(0) as usize) * (cell_y.max(0) as usize);
        Self {
            template_id,
            name: name.to_string(),
            cell_x,
            cell_y,
            height_max_coefficient,
            keep_surface,
            surfaces: OnceLock::new(),
            cells: (0..count).map(|_| CellSlot::default()).collect(),
        }
    }

    fn slot(&self, cell_x: i32, cell_y: i32) -> Option<&CellSlot> {
        if cell_x < 0 || cell_y < 0 || cell_x >= self.cell_x || cell_y >= self.cell_y {
            return None;
        }
        self.cells.get((cell_x * self.cell_y + cell_y) as usize)
    }

    /// Installs a cell's heightmap directly (`HeightMap[x, y]` at `x * 512 + y`), marking it
    /// loaded (tests and tools).
    pub fn insert_cell(&self, cell_x: i32, cell_y: i32, heights: Vec<u16>) -> bool {
        match self.slot(cell_x, cell_y) {
            Some(s) if heights.len() == CELL_POINTS => {
                s.loaded.set(Some(heights.into_boxed_slice())).is_ok()
            }
            _ => false,
        }
    }

    /// [`insert_cell`](Self::insert_cell) with terrain holes: grid points `(x, y)` (cell-local,
    /// `x * 512 + y` like the heights).
    pub fn insert_cell_with_holes(
        &self,
        cell_x: i32,
        cell_y: i32,
        heights: Vec<u16>,
        holes: &[(i32, i32)],
    ) -> bool {
        let Some(s) = self.slot(cell_x, cell_y) else {
            return false;
        };
        if !holes.is_empty() {
            let mut words = vec![0u64; CELL_POINTS.div_ceil(64)];
            for &(x, y) in holes {
                let bit = (x * CELL_HMAP_RESOLUTION + y) as usize;
                words[bit / 64] |= 1 << (bit % 64);
            }
            let _ = s.holes.set(Some(words.into()));
        }
        self.insert_cell(cell_x, cell_y, heights)
    }

    /// [`insert_cell`](Self::insert_cell) with the cell's surface table and per-sample local
    /// surface ids (`512 * 512`, x-major; tests and tools).
    pub fn insert_cell_with_surface(
        &self,
        cell_x: i32,
        cell_y: i32,
        heights: Vec<u16>,
        surface_table: [u32; 32],
        surface_ids: Vec<u8>,
    ) -> bool {
        let Some(s) = self.slot(cell_x, cell_y) else {
            return false;
        };
        if heights.len() != CELL_POINTS || surface_ids.len() != CELL_POINTS {
            return false;
        }
        let _ = s.surface_table.set(Some(Box::new(surface_table)));
        let _ = s.surface_ids.set(Some(surface_ids.into_boxed_slice()));
        s.loaded.set(Some(heights.into_boxed_slice())).is_ok()
    }

    /// Whether grid point `(ix, iy)` of the whole world (2 m grid) is a terrain hole (surface id
    /// 31 of the sample owning the 2 m square on its positive side), loading its cell on
    /// demand. False outside the map, in a cell without heightmap and for a failing cell.
    pub fn grid_is_hole(&self, files: &ClientFileManager, ix: i32, iy: i32) -> bool {
        if ix < 0 || iy < 0 {
            return false;
        }
        let (cx, cy) = (ix / CELL_HMAP_RESOLUTION, iy / CELL_HMAP_RESOLUTION);
        if cx >= self.cell_x || cy >= self.cell_y {
            return false;
        }
        if self.verify_cell_loaded(files, cx, cy).is_err() {
            return false;
        }
        let Some(Some(words)) = self.slot(cx, cy).and_then(|s| s.holes.get()) else {
            return false;
        };
        let bit = ((ix % CELL_HMAP_RESOLUTION) * CELL_HMAP_RESOLUTION + iy % CELL_HMAP_RESOLUTION)
            as usize;
        words[bit / 64] >> (bit % 64) & 1 == 1
    }

    /// `WorldCell.Loaded`.
    pub fn is_cell_loaded(&self, cell_x: i32, cell_y: i32) -> bool {
        self.slot(cell_x, cell_y)
            .is_some_and(|s| s.loaded.get().is_some())
    }

    /// `WorldCell.Loaded && HeightMap != null` (the cell's `heightmap.dat` was read; only then
    /// does `LoadCell` read the cell's `object.dat` — systems::ai water bodies).
    pub fn cell_has_height_map(&self, cell_x: i32, cell_y: i32) -> bool {
        self.slot(cell_x, cell_y)
            .is_some_and(|s| matches!(s.loaded.get(), Some(Some(_))))
    }

    /// Number of loaded cells.
    pub fn loaded_cell_count(&self) -> usize {
        self.cells
            .iter()
            .filter(|s| s.loaded.get().is_some())
            .count()
    }

    /// `WorldCell.VerifyCellLoaded()` → its `HeightMap` (`None` = all zeros). `Err` when the
    /// load throws (the cell stays unloaded and is retried next time, as in C#).
    fn verify_cell_loaded(
        &self,
        files: &ClientFileManager,
        cell_x: i32,
        cell_y: i32,
    ) -> Result<Option<&[u16]>, HeightError> {
        let slot = self
            .slot(cell_x, cell_y)
            .ok_or(HeightError::IndexOutOfRange)?;
        if let Some(h) = slot.loaded.get() {
            return Ok(h.as_deref());
        }
        let _guard = slot.load_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = slot.loaded.get() {
            return Ok(h.as_deref());
        }
        let _log_load = crate::systems::ai::geodata::manager::LoadTimer {
            what: "heightmap cell",
            world: &self.name,
            cell: (cell_x, cell_y),
            started: std::time::Instant::now(),
            min_ms: 50,
        };
        match load_cell_height_map(
            files,
            &self.name,
            cell_x,
            cell_y,
            self.height_max_coefficient,
            self.keep_surface,
        ) {
            CellLoad::Loaded(h, holes, surface_table, surface_ids) => {
                let _ = slot.holes.set(holes);
                let _ = slot.surface_table.set(surface_table);
                let _ = slot.surface_ids.set(surface_ids);
                let _ = slot.loaded.set(h);
                Ok(slot.loaded.get().and_then(|h| h.as_deref()))
            }
            CellLoad::Failed => Ok(None),
            CellLoad::Exception(reason) => Err(HeightError::CellLoad {
                world: self.name.clone(),
                cell_x,
                cell_y,
                reason,
            }),
        }
    }

    /// `WorldCell.GetHeightMapDataInCell(x, y)` (the cell must have been verified; an unloaded
    /// cell's `HeightMap` is null → 0). Note (C#): the bound check is `> CELL_HMAP_RESOLUTION`,
    /// so index 512 throws.
    pub fn get_height_map_data_in_cell(
        &self,
        cell_x: i32,
        cell_y: i32,
        data_x: i32,
        data_y: i32,
    ) -> Result<f32, HeightError> {
        let Some(slot) = self.slot(cell_x, cell_y) else {
            return Ok(0.0);
        };
        let Some(map) = slot.loaded.get() else {
            return Ok(0.0);
        };
        if !(0..=CELL_HMAP_RESOLUTION).contains(&data_x)
            || !(0..=CELL_HMAP_RESOLUTION).contains(&data_y)
        {
            return Ok(0.0);
        }
        if data_x == CELL_HMAP_RESOLUTION || data_y == CELL_HMAP_RESOLUTION {
            return Err(HeightError::IndexOutOfRange);
        }
        let v = map
            .as_deref()
            .map_or(0, |m| m[(data_x * CELL_HMAP_RESOLUTION + data_y) as usize]);
        Ok((f64::from(v) / self.height_max_coefficient) as f32)
    }

    /// `WorldCell.GetHeight(x, y)` (world position, not smoothed).
    pub fn get_cell_height(
        &self,
        cell_x: i32,
        cell_y: i32,
        x: i32,
        y: i32,
    ) -> Result<f32, HeightError> {
        // (int)(x - CellOffset.X) / 2 with a float CellOffset
        let xx = csharp_f32_to_i32(x as f32 - (cell_x * CELL_SIZE) as f32) / 2;
        let yy = csharp_f32_to_i32(y as f32 - (cell_y * CELL_SIZE) as f32) / 2;
        self.get_height_map_data_in_cell(cell_x, cell_y, xx, yy)
    }

    /// `WorldTemplate.GetRawHeightMapHeight(x, y)`. Note (C#): the bound check is
    /// `cellX > CellX`, so `cellX == CellX` throws `IndexOutOfRange`; so does a negative
    /// sub-cell index (`x % 1024 / 2 < 0` for `x <= -2`).
    pub fn get_raw_height_map_height(
        &self,
        files: &ClientFileManager,
        x: i32,
        y: i32,
    ) -> Result<f32, HeightError> {
        let cell_x = x / CELL_SIZE;
        let cell_y = y / CELL_SIZE;
        if cell_x < 0 || cell_x > self.cell_x || cell_y < 0 || cell_y > self.cell_y {
            return Ok(0.0); // out of bounds
        }
        let map = self.verify_cell_loaded(files, cell_x, cell_y)?;
        let sx = x % CELL_SIZE / 2;
        let sy = y % CELL_SIZE / 2;
        if sx < 0 || sy < 0 {
            return Err(HeightError::IndexOutOfRange);
        }
        let v = map.map_or(0, |m| m[(sx * CELL_HMAP_RESOLUTION + sy) as usize]);
        Ok((f64::from(v) / self.height_max_coefficient) as f32)
    }

    /// `WorldInstance.GetHeight(x, y)` / `WorldTemplate.GetHeight(x, y)`: bilinear
    /// interpolation between the 2 m grid points around the position.
    pub fn get_height(
        &self,
        files: &ClientFileManager,
        x: f32,
        y: f32,
    ) -> Result<f32, HeightError> {
        let (left, top) = find_nearest_significant_points(
            csharp_f64_to_i32(f64::from(x).floor()),
            csharp_f64_to_i32(f64::from(y).floor()),
        );
        let (right, bottom) = (left.wrapping_add(2), top.wrapping_add(2));
        let height_tl = self.get_raw_height_map_height(files, left, top)?;
        let height_tr = self.get_raw_height_map_height(files, right, top)?;
        let height_bl = self.get_raw_height_map_height(files, left, bottom)?;
        let height_br = self.get_raw_height_map_height(files, right, bottom)?;
        let off_x = (x - left as f32) / 2.0;
        let off_y = (y - top as f32) / 2.0;
        Ok(blerp(
            height_tl, height_tr, height_bl, height_br, off_x, off_y,
        ))
    }

    /// The height of grid point `(ix, iy)` of the whole world (2 m grid, world position
    /// `(2·ix, 2·iy)`), loading its cell on demand. 0 outside the map, in a cell without
    /// heightmap and for a failing cell (`systems::ai::collision` terrain heightfield).
    pub fn grid_sample(&self, files: &ClientFileManager, ix: i32, iy: i32) -> f32 {
        if ix < 0 || iy < 0 {
            return 0.0;
        }
        let (cx, cy) = (ix / CELL_HMAP_RESOLUTION, iy / CELL_HMAP_RESOLUTION);
        if cx >= self.cell_x || cy >= self.cell_y {
            return 0.0;
        }
        match self.verify_cell_loaded(files, cx, cy) {
            Ok(Some(map)) => {
                let (sx, sy) = (ix % CELL_HMAP_RESOLUTION, iy % CELL_HMAP_RESOLUTION);
                let v = map[(sx * CELL_HMAP_RESOLUTION + sy) as usize];
                (f64::from(v) / self.height_max_coefficient) as f32
            }
            _ => 0.0,
        }
    }

    /// The world-surface id of grid point `(ix, iy)` (2 m grid, the lower-left sample of its
    /// square, like [`grid_is_hole`](Self::grid_is_hole)): the cell's 32-entry table at the
    /// sample's local surface id (`raw & 0x1F`; `& 0xF` from node version 7). `None` outside the
    /// map, in a cell without `heightmap.dat` or version < 24, for a terrain hole (local id 31)
    /// and when the surface ids are not kept.
    pub fn grid_surface_id(&self, files: &ClientFileManager, ix: i32, iy: i32) -> Option<u32> {
        if ix < 0 || iy < 0 {
            return None;
        }
        let (cx, cy) = (ix / CELL_HMAP_RESOLUTION, iy / CELL_HMAP_RESOLUTION);
        if cx >= self.cell_x || cy >= self.cell_y {
            return None;
        }
        self.verify_cell_loaded(files, cx, cy).ok()?;
        let slot = self.slot(cx, cy)?;
        let table = slot.surface_table.get()?.as_ref()?;
        let ids = slot.surface_ids.get()?.as_ref()?;
        let local = ids[((ix % CELL_HMAP_RESOLUTION) * CELL_HMAP_RESOLUTION
            + iy % CELL_HMAP_RESOLUTION) as usize];
        if local == hmap::HOLE_SURFACE_ID {
            return None;
        }
        table.get(local as usize).copied()
    }

    /// The client's terrain place material at world `(x, y)` for the strict placement surface
    /// check: the cell's `surface.xml`/detail `.mtl` name -> place kind. [`SurfaceMaterial::Unknown`]
    /// (the rule does nothing) when the world has no surface data, the cell has no heightmap,
    /// the sample is a hole or the surface id is not in `surface.xml`. `(x / 2).floor()` is the
    /// lower-left 2 m sample of the spot's square (the client ray's sample between grid lines is
    /// INFERRED, see PORTING.md).
    pub fn material_at(&self, files: &ClientFileManager, x: f32, y: f32) -> SurfaceMaterial {
        if !self.keep_surface || !x.is_finite() || !y.is_finite() {
            return SurfaceMaterial::Unknown;
        }
        let ix = (x / 2.0).floor() as i32;
        let iy = (y / 2.0).floor() as i32;
        let Some(world_surface_id) = self.grid_surface_id(files, ix, iy) else {
            return SurfaceMaterial::Unknown;
        };
        let Some(table) = self
            .surfaces
            .get_or_init(|| WorldSurfaceTable::load(files, &self.name))
            .as_ref()
        else {
            return SurfaceMaterial::Unknown;
        };
        table.material(world_surface_id, files)
    }

    /// `VerifyCellLoaded` for every cell (`PreLoadTerrain`). Returns the number of cells whose
    /// load threw.
    pub fn preload_all(&self, files: &ClientFileManager) -> usize {
        let mut failed = 0;
        for cell_y in 0..self.cell_y {
            for cell_x in 0..self.cell_x {
                if let Err(e) = self.verify_cell_loaded(files, cell_x, cell_y) {
                    error!("{e}");
                    failed += 1;
                }
            }
        }
        failed
    }
}

/// `FindNearestSignificantPoints(x, y)` → (`Left`, `Top`) of `Rectangle(x - x % 2, y - y % 2, 2, 2)`.
fn find_nearest_significant_points(x: i32, y: i32) -> (i32, i32) {
    (x.wrapping_sub(x % 2), y.wrapping_sub(y % 2))
}

/// `Lerp`.
fn lerp(start: f32, end: f32, target: f32) -> f32 {
    start + (end - start) * target
}

/// `Blerp` (bottom-left, bottom-right, top-left, top-right).
fn blerp(c_x0y0: f32, c_x1y0: f32, c_x0y1: f32, c_x1y1: f32, tx: f32, ty: f32) -> f32 {
    lerp(lerp(c_x0y0, c_x1y0, tx), lerp(c_x0y1, c_x1y1, tx), ty)
}

/// `(int)double` with the saturating .NET 9+ conversion (NaN → 0).
fn csharp_f64_to_i32(v: f64) -> i32 {
    v as i32
}

/// `(int)float` with the saturating .NET 9+ conversion (NaN → 0).
fn csharp_f32_to_i32(v: f32) -> i32 {
    v as i32
}

// ---------------------------------------------------------------------------------------------
// WorldManager height queries
// ---------------------------------------------------------------------------------------------

/// The configuration the C# height code reads (`AppConfiguration`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeightmapConfig {
    /// `HeightMapsEnable`.
    pub heightmaps_enable: bool,
    /// `World.GeoDataMode` (navmesh heights first, see [`HeightmapWorlds::install_geo_data`]).
    pub geo_data_mode: bool,
    /// `World.PreLoadTerrain`.
    pub pre_load_terrain: bool,
    /// `Housing.PlacementStrictSurface`: keep the per-sample surface ids of every loaded cell
    /// (256 KiB per cell) so [`WorldHeightmap::material_at`] can answer.
    pub placement_strict_surface: bool,
}

/// What `WorldManager.GetReferenceHeight` reads from an `NpcAi` (`ai != null`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReferenceHeightAi {
    /// `ai.Owner.CanFly`.
    pub can_fly: bool,
    /// `ai.GetCurrentBehavior() is HoldPositionBehavior or IdleBehavior`.
    pub hold_or_idle: bool,
    /// `ai.Owner.Spawner?.Position.Z` (`None` = no spawner).
    pub spawner_z: Option<f32>,
    /// `ai.Owner.Transform.World.Position.Z`.
    pub world_z: f32,
}

/// `WorldTemplate.GeoData.GetHeight(position)` (navmesh height, `GeoDataMode` only), installed
/// by the AI subsystem: (template id, x, y, z) -> height.
pub type GeoDataHeight = dyn Fn(u32, f32, f32, f32) -> f32 + Send + Sync;

/// Every world template's heightmap plus the client files they load from.
pub struct HeightmapWorlds {
    pub config: HeightmapConfig,
    files: Arc<ClientFileManager>,
    /// By world template id.
    templates: HashMap<u32, WorldHeightmap>,
    /// `GetWorldTemplateByZoneKey(zoneKey).Id`.
    zone_worlds: HashMap<u32, u32>,
    /// The navmesh height lookup (`None` until the AI subsystem installs it).
    geo_data: RwLock<Option<Arc<GeoDataHeight>>>,
}

impl std::fmt::Debug for HeightmapWorlds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeightmapWorlds")
            .field("config", &self.config)
            .field("templates", &self.templates.len())
            .finish()
    }
}

impl HeightmapWorlds {
    /// Heightmaps for every world template of `worlds` (nothing is loaded yet).
    pub fn new(
        files: Arc<ClientFileManager>,
        worlds: &WorldTemplates,
        config: HeightmapConfig,
    ) -> Self {
        let mut templates = HashMap::new();
        let mut zone_worlds = HashMap::new();
        for t in worlds.templates() {
            templates.insert(
                t.id,
                WorldHeightmap::new_with_surface(t, config.placement_strict_surface),
            );
            for &zone_key in &t.zone_keys {
                if let Some(w) = worlds.get_world_template_by_zone_key(zone_key) {
                    zone_worlds.insert(zone_key, w.id);
                }
            }
        }
        Self {
            config,
            files,
            templates,
            zone_worlds,
            geo_data: RwLock::new(None),
        }
    }

    /// Installs the navmesh height lookup used with `GeoDataMode` (AI subsystem).
    pub fn install_geo_data(&self, f: Arc<GeoDataHeight>) {
        *self.geo_data.write().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    /// `world.GeoData.GetHeight(position)` when `GeoDataMode` is on and the navmesh is
    /// installed (0 otherwise, as without navmesh data).
    fn geo_data_height(&self, template_id: u32, x: f32, y: f32, z: f32) -> f32 {
        if !self.config.geo_data_mode {
            return 0.0;
        }
        match self
            .geo_data
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(f) => f(template_id, x, y, z),
            None => 0.0,
        }
    }

    /// From explicit parts (tests / tools).
    pub fn from_parts(
        files: Arc<ClientFileManager>,
        templates: Vec<WorldHeightmap>,
        zone_worlds: HashMap<u32, u32>,
        config: HeightmapConfig,
    ) -> Self {
        Self {
            config,
            files,
            templates: templates.into_iter().map(|t| (t.template_id, t)).collect(),
            zone_worlds,
            geo_data: RwLock::new(None),
        }
    }

    /// The client files the cells are read from.
    pub fn files(&self) -> &Arc<ClientFileManager> {
        &self.files
    }

    /// One world template's heightmap.
    pub fn template(&self, template_id: u32) -> Option<&WorldHeightmap> {
        self.templates.get(&template_id)
    }

    /// `GetWorldTemplateByZoneKey` (logs like the C# when unknown).
    fn template_by_zone_key(&self, zone_key: u32) -> Option<&WorldHeightmap> {
        match self.zone_worlds.get(&zone_key) {
            Some(id) => self.templates.get(id),
            None => {
                error!("GetWorldByZone(): No world template defined for ZoneId {zone_key}");
                None
            }
        }
    }

    /// `WorldInstance.GetHeight(x, y)` of a world template.
    pub fn world_height(&self, template_id: u32, x: f32, y: f32) -> Result<f32, HeightError> {
        match self.templates.get(&template_id) {
            Some(t) => t.get_height(&self.files, x, y),
            None => Ok(0.0),
        }
    }

    /// `WorldManager.GetHeight(zoneKey, x, y, z)`: floor height, 0 when unknown / disabled.
    pub fn get_height(&self, zone_key: u32, x: f32, y: f32, z: f32) -> f32 {
        let mut height = 0f32;
        let world = self.template_by_zone_key(zone_key);
        // GeoDataMode: `world?.GeoData.GetHeight(position)` (systems::ai)
        if let Some(w) = world {
            height = self.geo_data_height(w.template_id, x, y, z);
        }
        if height != 0.0 || !self.config.heightmaps_enable {
            return height;
        }
        match world {
            Some(w) => w.get_height(&self.files, x, y).unwrap_or(0.0),
            None => 0.0,
        }
    }

    /// `WorldManager.GetHeight(transform)` with the transform's world template (`None` = no
    /// such instance) and world position. Falls back to the transform's own Z.
    pub fn get_height_at(&self, template_id: Option<u32>, x: f32, y: f32, z: f32) -> f32 {
        let Some(template_id) = template_id else {
            return 0.0;
        };
        // GeoDataMode: `world.Template.GeoData?.GetHeight(...)` (systems::ai)
        let height = self.geo_data_height(template_id, x, y, z);
        if height != 0.0 {
            return height;
        }
        if self.config.heightmaps_enable {
            self.world_height(template_id, x, y).unwrap_or(z)
        } else {
            z
        }
    }

    /// `WorldManager.GetReferenceHeight(ai, x, y, z, zoneId)`.
    pub fn get_reference_height(
        &self,
        ai: Option<&ReferenceHeightAi>,
        x: f32,
        y: f32,
        z: f32,
        zone_id: u32,
    ) -> f32 {
        // 0. Just in case.
        let Some(ai) = ai else {
            return self.get_height(zone_id, x, y, z);
        };
        // 1. If an NPC can fly, the height is taken from the spawner's position.
        // 2. For HoldPositionBehavior and IdleBehavior, the height is taken from the spawner.
        // (C#: `Spawner.Position.Z` throws without a spawner; the unit's Z is used then.)
        if ai.can_fly || ai.hold_or_idle {
            return ai.spawner_z.unwrap_or(ai.world_z);
        }
        // 3. Terrain height retrieval
        let final_height = self.get_height(zone_id, x, y, z);
        if final_height != 0.0 {
            return final_height;
        }
        // 4. Take the default height
        ai.spawner_z.unwrap_or(ai.world_z)
    }

    /// `WorldManager.LoadHeightmaps` (after the templates are known): logs, and with
    /// `PreLoadTerrain` loads every cell. Returns the number of templates with client data.
    pub fn load_heightmaps(&self, worlds: &WorldTemplates) -> usize {
        if !self.config.heightmaps_enable {
            return 0;
        }
        info!("Loading heightmaps...");
        let mut loaded = 0;
        for t in worlds.templates() {
            info!("Loading heightmap of {}", t.name);
            // LoadZoneBaiFiles: GeoDataMode only (`systems::ai::geodata`).
            let world_xml = format!("game/worlds/{}/world.xml", t.name);
            if !self.files.file_exists(&world_xml) {
                continue;
            }
            if !self.config.pre_load_terrain {
                debug!(
                    "PreLoadTerrain disabled, heightmaps for {} will get loaded on demand.",
                    t.name
                );
                loaded += 1;
                continue;
            }
            if let Some(h) = self.templates.get(&t.id) {
                h.preload_all(&self.files);
            }
            info!("{} heightmap loaded", t.name);
            loaded += 1;
        }
        info!("Loaded {loaded}/{} heightmaps", worlds.templates().len());
        loaded
    }
}

/// `server.systems.npcs.heightmaps`: set once by [`start`].
#[derive(Debug, Default)]
pub struct Heightmaps {
    worlds: RwLock<Option<Arc<HeightmapWorlds>>>,
}

impl Heightmaps {
    /// The loaded heightmaps (`None` before [`start`]).
    pub fn get(&self) -> Option<Arc<HeightmapWorlds>> {
        self.worlds
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Installs `worlds` (startup / tests).
    pub fn install(&self, worlds: Arc<HeightmapWorlds>) {
        *self.worlds.write().unwrap_or_else(|e| e.into_inner()) = Some(worlds);
    }

    /// The client files opened at startup (`None` before [`start`]).
    pub fn client_files(&self) -> Option<Arc<ClientFileManager>> {
        self.get().map(|w| w.files.clone())
    }
}

/// `WorldManager.GetHeight(zoneKey, x, y, z)` on `server`'s heightmaps (0 before startup).
pub fn get_height(server: &GameServer, zone_key: u32, x: f32, y: f32, z: f32) -> f32 {
    server
        .systems
        .npcs
        .heightmaps
        .get()
        .map_or(0.0, |h| h.get_height(zone_key, x, y, z))
}

/// `WorldManager.GetHeight(obj.Transform)`: the floor height under the object's world position
/// (its own Z when no heightmap value is found).
pub fn get_height_of(server: &GameServer, obj: &WorldObject) -> f32 {
    let instance_id = obj.transform().instance_id;
    let world = server.world.world(instance_id);
    let pos = match &world {
        Some(w) => w.world_pos_rot(obj),
        None => return 0.0,
    };
    match server.systems.npcs.heightmaps.get() {
        Some(h) => h.get_height_at(world.as_ref().map(|w| w.template.id), pos.x, pos.y, pos.z),
        None => pos.z,
    }
}

/// `WorldInstance.GetHeight(x, y)` of `world` (`Err` = the C# exception).
pub fn world_height(
    server: &GameServer,
    world: &WorldInstance,
    x: f32,
    y: f32,
) -> Result<f32, HeightError> {
    match server.systems.npcs.heightmaps.get() {
        Some(h) => h.world_height(world.template.id, x, y),
        None => Ok(0.0),
    }
}

/// Startup (`WorldManager.LoadHeightmaps`): opens the client sources, builds the per-template
/// cell tables (loaded on demand, or all of them with `PreLoadTerrain`).
pub async fn start(server: &Arc<GameServer>) -> anyhow::Result<()> {
    let config = HeightmapConfig {
        heightmaps_enable: server.config.heightmaps_enable,
        geo_data_mode: server.config.world.geo_data_mode,
        pre_load_terrain: server.config.world.pre_load_terrain,
        placement_strict_surface: server.config.housing.placement_strict_surface,
    };
    let sources = server.config.client_data.sources.clone();
    let app_path = server.config.app_path.clone();
    let game_data = server.game_data.clone();
    let worlds = tokio::task::spawn_blocking(move || {
        let files = Arc::new(ClientFileManager::from_sources(&sources, &app_path));
        let worlds = Arc::new(HeightmapWorlds::new(files, &game_data.worlds, config));
        worlds.load_heightmaps(&game_data.worlds);
        worlds
    })
    .await?;
    server.systems.npcs.heightmaps.install(worlds);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `HeightMaxCoefficient` of a 4096 m world (the whole terrain range fits the u16 grid).
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
        assert!((h - 20.5).abs() < 0.05, "{h}");
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
}

#[cfg(test)]
mod real_data_tests {
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
            let loaded =
                t.verify_cell_loaded(&files, cx, cy).is_ok() && t.cell_has_height_map(cx, cy);
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
        let text =
            std::fs::read_to_string(app.join("Data/Worlds/main_world/npc_spawns.json")).unwrap();
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
}
