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
        let h =
            |x: usize, y: usize| self.raw_data_to_height(u32::from(self.p_hm_data[x * n_size + y]));
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
