use super::*;

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
                    // C#: (ushort)(height * Template.HeightMaxCoefficient), float * double,
                    // saturating conversion (.NET 9+). The coefficient covers the whole
                    // `max_height` range here (see PORTING.md), so only the unit rounding differs.
                    let value = (f64::from(height) * height_max_coefficient).round() as u16;
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
    pub(super) fn verify_cell_loaded(
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
pub(super) fn lerp(start: f32, end: f32, target: f32) -> f32 {
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
