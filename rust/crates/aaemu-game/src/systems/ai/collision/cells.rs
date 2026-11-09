//! The per-cell object files the zone loads with `Seamless_LoadCellObject` (cry3dengine
//! 0x30163AC0) and the world's `vegetation.xml`:
//!
//! | file | layout | engine |
//! |---|---|---|
//! | `cells/X_Y/client/statobjs.dat` | `u32 n; char name[256] × n` | brush model table (index = `m_nObjectTypeID`) |
//! | `cells/X_Y/client/brush.dat` | `u32 ver = 1; u32 off[256]; u32 size[256]`; 16×16 sectors of 64 m; 128-byte `SBrushChunk` | cell load 0x3017CC30 → sector 0x3017BA50 → `_LoadBrush` 0x3021E840 |
//! | `cells/X_Y/client/vegetation.dat` | `u32 ver = 1; u32 off[16]; u32 size[16]`; 4×4 sectors of 256 m; 64-byte `SVegetationChunk` | `CVegetation` |
//! | `worlds/W/vegetation.xml` | `<group id modelFileName …/>` | vegetation groups |
//!
//! Record layouts: `sub_cgf.md` §3 (typeinfo-confirmed). Positions in both files are
//! sector-local; the loader adds `(sx·64, sy·64, 0)` (brushes, 0x3017BA50) and the cell origin.
//! The sector index order (`i = sx·16 + sy` or `sy·16 + sx`) is not visible in the decompile
//! (the sector table is filled by the stream loader); [`SECTOR_X_MAJOR`] was fixed with the
//! real data (see `real_data_tests`: brushes and trees sit on the terrain only with this order).

use aaemu_physics::math::{Mat33, Vec3};

/// Sector index → (sx, sy): `i / n` is x when true (the data check in `real_data_tests`).
pub const SECTOR_X_MAJOR: bool = true;

/// `(sx, sy)` of sector `i` in a cell of `n × n` sectors.
pub fn sector_xy(i: usize, n: usize) -> (usize, usize) {
    if SECTOR_X_MAJOR {
        (i / n, i % n)
    } else {
        (i % n, i / n)
    }
}

/// Normalises a model path from a `.dat` / `.xml` reference the way the pak names it:
/// backslashes → `/`, lower case, trimmed (three statobjs.dat names carry a trailing space),
/// doubled slashes collapsed, `game/` prefixed.
pub fn normalize_model_path(p: &str) -> String {
    let mut s = p.trim().replace('\\', "/").to_ascii_lowercase();
    while s.contains("//") {
        s = s.replace("//", "/");
    }
    let s = s.trim_start_matches('/').to_string();
    if s.starts_with("game/") {
        s
    } else {
        format!("game/{s}")
    }
}

fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn f32_at(d: &[u8], o: usize) -> Option<f32> {
    u32_at(d, o).map(f32::from_bits)
}

fn vec3_at(d: &[u8], o: usize) -> Option<Vec3> {
    Some(Vec3::new(
        f32_at(d, o)?,
        f32_at(d, o + 4)?,
        f32_at(d, o + 8)?,
    ))
}

/// `statobjs.dat`: the cell's model names (normalised, see [`normalize_model_path`]).
pub fn read_statobjs(d: &[u8]) -> Vec<String> {
    let Some(n) = u32_at(d, 0) else {
        return Vec::new();
    };
    (0..n as usize)
        .map_while(|i| {
            let rec = d.get(4 + i * 256..4 + i * 256 + 256)?;
            let end = rec.iter().position(|&c| c == 0).unwrap_or(256);
            Some(normalize_model_path(&String::from_utf8_lossy(&rec[..end])))
        })
        .collect()
}

/// One `SBrushChunk` (128 bytes) with its sector offset applied (cell-local metres).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrushRecord {
    /// `m_WSBBox` (cell-local).
    pub bbox: [Vec3; 2],
    /// `m_dwRndFlags` (+32).
    pub rnd_flags: u32,
    /// `m_Matrix` rotation·scale rows (+67, row-major 3×4).
    pub rot_scale: Mat33,
    /// `m_Matrix` translation (cell-local).
    pub pos: Vec3,
    /// `m_nMaterialId` (+115).
    pub material_id: i32,
    /// `m_nObjectTypeID` (+123): index into `statobjs.dat`.
    pub statobj: i32,
}

/// Errors of the `.dat` readers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DatError {
    #[error("bad version {0}")]
    Version(u32),
    #[error("truncated")]
    Truncated,
}

/// One `SBrushChunk` (128 bytes) with `offset` added to its bbox and position (the sector
/// offset in `brush.dat`, zero in `visareas.dat` / `object.dat`).
fn brush_record(rec: &[u8], offset: Vec3) -> Option<BrushRecord> {
    let m: Vec<f32> = (0..12)
        .map(|j| f32_at(rec, 67 + 4 * j))
        .collect::<Option<_>>()?;
    Some(BrushRecord {
        bbox: [vec3_at(rec, 0)? + offset, vec3_at(rec, 12)? + offset],
        rnd_flags: u32_at(rec, 32)?,
        rot_scale: Mat33::from_array([m[0], m[1], m[2], m[4], m[5], m[6], m[8], m[9], m[10]]),
        pos: Vec3::new(m[3], m[7], m[11]) + offset,
        material_id: u32_at(rec, 115)? as i32,
        statobj: u32_at(rec, 123)? as i32,
    })
}

/// One `SVegetationChunk` (64 bytes) with `offset` added.
fn vegetation_record(rec: &[u8], offset: Vec3) -> Option<VegetationRecord> {
    Some(VegetationRecord {
        bbox: [vec3_at(rec, 0)? + offset, vec3_at(rec, 12)? + offset],
        rnd_flags: u32_at(rec, 32)?,
        pos: vec3_at(rec, 39)? + offset,
        scale: f32_at(rec, 51)?,
        group: u16::from_le_bytes([*rec.get(55)?, *rec.get(56)?]),
        angle_x: *rec.get(57)?,
        angle_y: *rec.get(58)?,
        angle_z: *rec.get(60)?,
    })
}

/// `brush.dat`.
pub fn read_brushes(d: &[u8]) -> Result<Vec<BrushRecord>, DatError> {
    let ver = u32_at(d, 0).ok_or(DatError::Truncated)?;
    if ver != 1 {
        return Err(DatError::Version(ver));
    }
    let mut out = Vec::new();
    for i in 0..256 {
        let off = u32_at(d, 4 + 4 * i).ok_or(DatError::Truncated)? as usize;
        let size = u32_at(d, 1028 + 4 * i).ok_or(DatError::Truncated)? as usize;
        let (sx, sy) = sector_xy(i, 16);
        let so = Vec3::new(sx as f32 * 64.0, sy as f32 * 64.0, 0.0);
        for k in 0..size / 128 {
            let r = off + k * 128;
            let rec = d.get(r..r + 128).ok_or(DatError::Truncated)?;
            out.push(brush_record(rec, so).ok_or(DatError::Truncated)?);
        }
    }
    Ok(out)
}

/// One `SVegetationChunk` (64 bytes) with its sector offset applied (cell-local metres).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VegetationRecord {
    pub bbox: [Vec3; 2],
    pub rnd_flags: u32,
    /// `m_vPos` (+39).
    pub pos: Vec3,
    /// `m_fScale` (+51).
    pub scale: f32,
    /// `m_nObjectTypeID` (+55): the `vegetation.xml` group id.
    pub group: u16,
    /// `m_ucAngleX` (+57), `m_ucAngleY` (+58), `m_ucAngle` (+60, z).
    pub angle_x: u8,
    pub angle_y: u8,
    pub angle_z: u8,
}

/// `vegetation.dat`.
pub fn read_vegetation(d: &[u8]) -> Result<Vec<VegetationRecord>, DatError> {
    let ver = u32_at(d, 0).ok_or(DatError::Truncated)?;
    if ver != 1 {
        return Err(DatError::Version(ver));
    }
    let mut out = Vec::new();
    for i in 0..16 {
        let off = u32_at(d, 4 + 4 * i).ok_or(DatError::Truncated)? as usize;
        let size = u32_at(d, 68 + 4 * i).ok_or(DatError::Truncated)? as usize;
        let (sx, sy) = sector_xy(i, 4);
        let so = Vec3::new(sx as f32 * 256.0, sy as f32 * 256.0, 0.0);
        for k in 0..size / 64 {
            let r = off + k * 64;
            let rec = d.get(r..r + 64).ok_or(DatError::Truncated)?;
            out.push(vegetation_record(rec, so).ok_or(DatError::Truncated)?);
        }
    }
    Ok(out)
}

/// The rotation of a vegetation instance (`CVegetation` matrix, 0x30102840): each byte angle
/// is `b · 1.4117647° (= 360/255)`, `Matrix33::CreateRotationXYZ(Ang3(x, y, z))`
/// (`sub_30106900`), then the uniform scale. The terrain alignment of groups with flag 8
/// (`+52 & 8`, only when the x / y angles are 0) is not applied (see `PORTING.md`).
pub fn vegetation_rotation(r: &VegetationRecord) -> Mat33 {
    let ang = |b: u8| -> f32 {
        if b == 0 {
            0.0
        } else {
            (f32::from(b) * 1.411_764_7) * 0.017_453_292
        }
    };
    let (ax, ay, az) = (ang(r.angle_x), ang(r.angle_y), ang(r.angle_z));
    rotation_xyz(ax, ay, az)
}

/// `Matrix33::CreateRotationXYZ` (`Rz · Ry · Rx`).
pub fn rotation_xyz(ax: f32, ay: f32, az: f32) -> Mat33 {
    let (sx, cx) = ax.sin_cos();
    let (sy, cy) = ay.sin_cos();
    let (sz, cz) = az.sin_cos();
    Mat33::from_array([
        cy * cz,
        cz * sx * sy - cx * sz,
        cx * cz * sy + sx * sz,
        cy * sz,
        cx * cz + sx * sy * sz,
        -cz * sx + cx * sy * sz,
        -sy,
        cy * sx,
        cx * cy,
    ])
}

/// A `vegetation.xml` group.
#[derive(Debug, Clone, PartialEq)]
pub struct VegetationGroup {
    pub id: u16,
    /// Normalised model path (empty = none).
    pub model: String,
    /// `bHideability` / `bHideabilitySecondary` (passed through to the physics in the engine,
    /// *(inferred)* group flag byte +24 bits 0/1).
    pub hideability: bool,
    pub hideability_secondary: bool,
    /// `bAlignToTerrain` (*(inferred)* the group flag `+52 & 8` of 0x30102840).
    pub align_to_terrain: bool,
}

/// Parses `vegetation.xml` (`<group id="…" modelFileName="…" …/>` anywhere in the document).
pub fn read_vegetation_groups(xml: &str) -> Vec<VegetationGroup> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    doc.descendants()
        .filter(|n| n.has_tag_name("group") || n.has_tag_name("Group"))
        .filter_map(|n| {
            let id = n.attribute("id")?.trim().parse::<u16>().ok()?;
            let model = n
                .attribute("modelFileName")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(normalize_model_path)
                .unwrap_or_default();
            let flag = |a: &str| n.attribute(a).is_some_and(|v| v.trim() == "1");
            Some(VegetationGroup {
                id,
                model,
                hideability: flag("bHideability"),
                hideability_secondary: flag("bHideabilitySecondary"),
                align_to_terrain: flag("bAlignToTerrain"),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// visareas.dat / object.dat: the cell octrees (`COctreeNode::LoadCellObject`)
// ---------------------------------------------------------------------------------------------

/// A failure of the strict octree readers ([`read_visareas`], [`read_object_dat`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OctreeError {
    #[error("truncated at {0}")]
    Truncated(usize),
    #[error("bad chunk version {0} (expected 4)")]
    ChunkVersion(u32),
    #[error("chunk size {found} != {expected} bytes remaining")]
    ChunkSize { found: u32, expected: usize },
    #[error("visarea chunk version {0} at {1}")]
    AreaVersion(u32, usize),
    #[error("octree node version {0} at {1}")]
    NodeVersion(u32, usize),
    #[error("octree nesting too deep at {0}")]
    TooDeep(usize),
    #[error("unknown object type {0} at {1}")]
    UnknownType(u32, usize),
    #[error("voxel version {0} at {1}")]
    VoxelVersion(i32, usize),
    #[error("object block overruns its node at {0}")]
    BlockOverrun(usize),
    #[error("implausible table size {0}")]
    TableSize(u32),
    #[error("{0} bytes left after the last area")]
    Leftover(usize),
}

/// `eERType` of the octree object records (`sub_30225050`).
pub mod er_type {
    pub const BRUSH: u32 = 1;
    pub const VEGETATION: u32 = 2;
    pub const VOXEL: u32 = 6;
    pub const DECAL: u32 = 9;
    pub const WATER_VOLUME: u32 = 11;
    pub const WATER_WAVE: u32 = 12;
    pub const ROAD: u32 = 13;
    pub const DISTANCE_CLOUD: u32 = 14;
    pub const AUTO_CUBEMAP: u32 = 16;
    pub const LIGHT_SHAPE: u32 = 23;
}

/// The fixed part of a voxel record (`SVoxelObjectChunkVer5`).
const VOXEL_HEADER: usize = 198_775;
/// `m_Matrix` inside the voxel record.
const VOXEL_MATRIX: usize = 198_727;

/// A voxel object (cave): a static trimesh the cell file stores finished, inside a
/// zlib-compressed CGF block (`CVoxelObject::SetCompiledMeshData`, 0x301BC4D0).
#[derive(Debug, Clone, PartialEq)]
pub struct VoxelRecord {
    /// `m_WSBBox` (cell-local).
    pub bbox: [Vec3; 2],
    pub rnd_flags: u32,
    /// `m_Matrix` rotation·scale rows (`+198727`, row-major 3×4) and translation (cell-local).
    pub rot_scale: Mat33,
    pub pos: Vec3,
    /// Uncompressed size and the zlib stream of the first compiled block (the second block is
    /// the same geometry); `None` without compiled blocks.
    pub block: Option<(usize, Vec<u8>)>,
}

impl VoxelRecord {
    /// The first compiled block: a CGF 0x745 container whose `MeshPhysicsData` is the trimesh.
    pub fn inflate(&self) -> Option<Vec<u8>> {
        let (size, z) = self.block.as_ref()?;
        let out = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(z, *size).ok()?;
        (out.len() == *size).then_some(out)
    }
}

/// What the strict walker found in one `visareas.dat`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VisAreaFile {
    /// The file's own model table (normalised paths, empty = unused slot); `statobj` indexes it.
    pub objects: Vec<String>,
    pub areas: usize,
    pub brushes: Vec<BrushRecord>,
    pub vegetation: Vec<VegetationRecord>,
    pub voxels: Vec<VoxelRecord>,
    /// Records of other types (decals, water volumes, …), walked and skipped.
    pub other: usize,
}

struct Cursor<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], OctreeError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.d.len())
            .ok_or(OctreeError::Truncated(self.pos))?;
        let s = &self.d[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32, OctreeError> {
        let p = self.pos;
        u32_at(self.take(4)?, 0).ok_or(OctreeError::Truncated(p))
    }
}

/// The model / material tables at the start of `visareas.dat` and `object.dat`:
/// `u32 n; n × {u32; char[256]}`, twice. Returns the normalised model names.
fn read_name_tables(c: &mut Cursor<'_>) -> Result<Vec<String>, OctreeError> {
    let mut names = Vec::new();
    for table in 0..2 {
        let n = c.u32()?;
        if n > 100_000 {
            return Err(OctreeError::TableSize(n));
        }
        for _ in 0..n {
            let e = c.take(260)?;
            if table == 0 {
                let raw = &e[4..];
                let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
                let s = String::from_utf8_lossy(&raw[..end]);
                names.push(if s.trim().is_empty() {
                    String::new()
                } else {
                    normalize_model_path(&s)
                });
            }
        }
    }
    Ok(names)
}

/// Receives the object records of an octree walk (offsets cell-local).
trait ObjectSink {
    fn brush(&mut self, rec: &[u8]);
    fn vegetation(&mut self, rec: &[u8]);
    fn voxel(&mut self, rec: VoxelRecord);
    fn other(&mut self) {}
}

fn walk_node(
    c: &mut Cursor<'_>,
    sink: &mut dyn ObjectSink,
    depth: usize,
) -> Result<(), OctreeError> {
    if depth > 16 {
        return Err(OctreeError::TooDeep(c.pos));
    }
    let start = c.pos;
    let h = c.take(33)?;
    let version = u32_at(h, 0).unwrap_or(0);
    if version != 2 {
        return Err(OctreeError::NodeVersion(version, start));
    }
    let block = u32_at(h, 28).unwrap_or(0) as usize;
    let mask = h[32];
    if block > 0 {
        let end = c
            .pos
            .checked_add(block)
            .filter(|&e| e <= c.d.len())
            .ok_or(OctreeError::Truncated(c.pos))?;
        while c.pos < end {
            let at = c.pos;
            let t = c.u32()?;
            walk_object(c, sink, t, at)?;
        }
        if c.pos != end {
            return Err(OctreeError::BlockOverrun(start));
        }
    }
    for i in 0..8 {
        if mask & (1 << i) != 0 {
            walk_node(c, sink, depth + 1)?;
        }
    }
    Ok(())
}

fn walk_object(
    c: &mut Cursor<'_>,
    sink: &mut dyn ObjectSink,
    t: u32,
    at: usize,
) -> Result<(), OctreeError> {
    use er_type::*;
    // Fixed payload size and the offset of a point count (`count × 12` bytes follow).
    let (fixed, count_at) = match t {
        BRUSH => {
            sink.brush(c.take(128)?);
            return Ok(());
        }
        VEGETATION => {
            sink.vegetation(c.take(64)?);
            return Ok(());
        }
        VOXEL => return walk_voxel(c, sink, at),
        DECAL => (111, None),
        WATER_VOLUME => (119, Some(103)),
        WATER_WAVE => (151, Some(91)),
        ROAD => (63, Some(39)),
        DISTANCE_CLOUD => (79, None),
        AUTO_CUBEMAP => (95, None),
        LIGHT_SHAPE => (91, None),
        _ => return Err(OctreeError::UnknownType(t, at)),
    };
    let rec = c.take(fixed)?;
    let mut points = 0usize;
    if let Some(o) = count_at {
        points = u32_at(rec, o).unwrap_or(0) as usize;
        // The water volume has a second point list (count at +115).
        if t == WATER_VOLUME {
            points = points.saturating_add(u32_at(rec, 115).unwrap_or(0) as usize);
        }
    }
    c.take(
        points
            .checked_mul(12)
            .ok_or(OctreeError::Truncated(c.pos))?,
    )?;
    sink.other();
    Ok(())
}

fn walk_voxel(c: &mut Cursor<'_>, sink: &mut dyn ObjectSink, at: usize) -> Result<(), OctreeError> {
    let rec = c.take(VOXEL_HEADER)?;
    let version = u32_at(rec, 39).unwrap_or(0) as i32;
    if version != 5 {
        return Err(OctreeError::VoxelVersion(version, at));
    }
    let mut block = None;
    if rec[67] & 0x20 != 0 {
        let n = c.u32()?;
        for i in 0..n {
            let size = c.u32()? as usize;
            let b = c.take(size)?;
            // `S` = compressed size + 4: the uncompressed size, then the zlib stream.
            if i == 0 && size >= 4 {
                block = u32_at(b, 0).map(|u| (u as usize, b[4..].to_vec()));
            }
        }
    }
    let m: Vec<f32> = (0..12)
        .map(|j| f32_at(rec, VOXEL_MATRIX + 4 * j))
        .collect::<Option<_>>()
        .ok_or(OctreeError::Truncated(at))?;
    sink.voxel(VoxelRecord {
        bbox: [
            vec3_at(rec, 0).ok_or(OctreeError::Truncated(at))?,
            vec3_at(rec, 12).ok_or(OctreeError::Truncated(at))?,
        ],
        rnd_flags: u32_at(rec, 32).unwrap_or(0),
        rot_scale: Mat33::from_array([m[0], m[1], m[2], m[4], m[5], m[6], m[8], m[9], m[10]]),
        pos: Vec3::new(m[3], m[7], m[11]),
        block,
    });
    Ok(())
}

impl ObjectSink for VisAreaFile {
    fn brush(&mut self, rec: &[u8]) {
        if let Some(b) = brush_record(rec, Vec3::ZERO) {
            self.brushes.push(b);
        }
    }
    fn vegetation(&mut self, rec: &[u8]) {
        if let Some(v) = vegetation_record(rec, Vec3::ZERO) {
            self.vegetation.push(v);
        }
    }
    fn voxel(&mut self, rec: VoxelRecord) {
        self.voxels.push(rec);
    }
    fn other(&mut self) {
        self.other += 1;
    }
}

/// What [`read_object_dat`] returns of an `object.dat`: its voxel meshes and the type-2
/// vegetation records (cell-local positions). Its type-1 brushes duplicate `brush.dat` and are
/// not returned.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ObjectDatFile {
    pub voxels: Vec<VoxelRecord>,
    pub vegetation: Vec<VegetationRecord>,
}

impl ObjectSink for ObjectDatFile {
    fn brush(&mut self, _: &[u8]) {}
    fn vegetation(&mut self, rec: &[u8]) {
        if let Some(v) = vegetation_record(rec, Vec3::ZERO) {
            self.vegetation.push(v);
        }
    }
    fn voxel(&mut self, rec: VoxelRecord) {
        self.voxels.push(rec);
    }
}

/// `cells/X_Y/client/visareas.dat` (`C3DEngine::LoadCellVisAreas`, `CVisAreaManager::
/// SetCompiledData`): the cell's model table, then `nVisAreas + nPortals + nOcclAreas` areas,
/// each `SVisAreaChunk` (264 B) [+ 12 B from version 3] + point list + octree of objects
/// (+ 4 B tail at version 4). Positions are cell-local (no sector offset; add the cell origin).
/// Strict: any deviation from the grammar, leftover bytes included, is an error.
pub fn read_visareas(d: &[u8]) -> Result<VisAreaFile, OctreeError> {
    let mut c = Cursor { d, pos: 0 };
    let objects = read_name_tables(&mut c)?;
    let header_at = c.pos;
    let h = c.take(20)?;
    let word = |i: usize| u32_at(h, 4 * i).unwrap_or(0);
    if word(0) != 4 {
        return Err(OctreeError::ChunkVersion(word(0)));
    }
    let remaining = d.len() - header_at;
    if word(1) as usize != remaining {
        return Err(OctreeError::ChunkSize {
            found: word(1),
            expected: remaining,
        });
    }
    let n_areas = [word(2), word(3), word(4)]
        .iter()
        .try_fold(0u32, |a, &n| a.checked_add(n))
        .filter(|&n| n <= 100_000)
        .ok_or(OctreeError::TableSize(word(2)))?;
    let mut out = VisAreaFile {
        objects,
        areas: n_areas as usize,
        ..VisAreaFile::default()
    };
    for _ in 0..n_areas {
        let at = c.pos;
        let a = c.take(264)?;
        let version = u32_at(a, 0).unwrap_or(0);
        if !(2..=4).contains(&version) {
            return Err(OctreeError::AreaVersion(version, at));
        }
        let objects_block = u32_at(a, 84).unwrap_or(0);
        if version >= 3 {
            c.take(12)?;
        }
        let n_points = c.u32()? as usize;
        c.take(
            n_points
                .checked_mul(12)
                .ok_or(OctreeError::Truncated(c.pos))?,
        )?;
        if objects_block > 4 && c.pos < d.len() {
            walk_node(&mut c, &mut out, 0)?;
        }
        if version >= 4 {
            c.take(4)?;
        }
    }
    if c.pos != d.len() {
        return Err(OctreeError::Leftover(d.len() - c.pos));
    }
    Ok(out)
}

/// The voxel and vegetation objects of a `cells/X_Y/client/object.dat`: the same name tables
/// and octree grammar as `visareas.dat` (its brushes duplicate `brush.dat` and are not
/// returned; other record types are walked and skipped). Its vegetation records are not in
/// `vegetation.dat` (same 64-byte layout, cell-local). Strict: the octree must end at the end
/// of the file.
pub fn read_object_dat(d: &[u8]) -> Result<ObjectDatFile, OctreeError> {
    let mut c = Cursor { d, pos: 0 };
    read_name_tables(&mut c)?;
    let mut sink = ObjectDatFile::default();
    if c.pos < d.len() {
        walk_node(&mut c, &mut sink, 0)?;
    }
    if c.pos != d.len() {
        return Err(OctreeError::Leftover(d.len() - c.pos));
    }
    Ok(sink)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_paths_normalise() {
        assert_eq!(
            normalize_model_path("Objects\\Env\\Rock.cgf "),
            "game/objects/env/rock.cgf"
        );
        assert_eq!(
            normalize_model_path("game/worlds/main_world/cells/007_010//brush/solid_a.cgf"),
            "game/worlds/main_world/cells/007_010/brush/solid_a.cgf"
        );
    }

    fn brush_file(sector: usize, recs: &[(Vec3, u32, i32)]) -> Vec<u8> {
        let mut d = vec![0u8; 4 + 2048];
        d[..4].copy_from_slice(&1u32.to_le_bytes());
        let off = d.len() as u32;
        d[4 + 4 * sector..8 + 4 * sector].copy_from_slice(&off.to_le_bytes());
        let size = (recs.len() * 128) as u32;
        d[1028 + 4 * sector..1032 + 4 * sector].copy_from_slice(&size.to_le_bytes());
        for (p, flags, so) in recs {
            let mut r = vec![0u8; 128];
            r[32..36].copy_from_slice(&flags.to_le_bytes());
            let m = [
                1.0f32, 0.0, 0.0, p.x, 0.0, 1.0, 0.0, p.y, 0.0, 0.0, 1.0, p.z,
            ];
            for (j, v) in m.iter().enumerate() {
                r[67 + 4 * j..71 + 4 * j].copy_from_slice(&v.to_le_bytes());
            }
            r[123..127].copy_from_slice(&so.to_le_bytes());
            d.extend_from_slice(&r);
        }
        d
    }

    #[test]
    fn brush_records_get_the_sector_offset() {
        let d = brush_file(17, &[(Vec3::new(1.0, 2.0, 3.0), 0x2000_0008, 4)]);
        let b = read_brushes(&d).unwrap();
        assert_eq!(b.len(), 1);
        let (sx, sy) = sector_xy(17, 16);
        assert_eq!(
            b[0].pos,
            Vec3::new(1.0 + 64.0 * sx as f32, 2.0 + 64.0 * sy as f32, 3.0)
        );
        assert_eq!(b[0].rnd_flags, 0x2000_0008);
        assert_eq!(b[0].statobj, 4);
        assert!(b[0].rot_scale.is_identity());
        assert_eq!(
            read_brushes(&[2, 0, 0, 0]).unwrap_err(),
            DatError::Version(2)
        );
    }

    #[test]
    fn vegetation_records_and_groups() {
        let mut d = vec![0u8; 132];
        d[..4].copy_from_slice(&1u32.to_le_bytes());
        d[4 + 4 * 5..8 + 4 * 5].copy_from_slice(&132u32.to_le_bytes());
        d[68 + 4 * 5..72 + 4 * 5].copy_from_slice(&64u32.to_le_bytes());
        let mut r = vec![0u8; 64];
        for (j, v) in [10.0f32, 20.0, 30.0, 1.5].iter().enumerate() {
            r[39 + 4 * j..43 + 4 * j].copy_from_slice(&v.to_le_bytes());
        }
        r[55..57].copy_from_slice(&77u16.to_le_bytes());
        r[60] = 64;
        d.extend_from_slice(&r);
        let v = read_vegetation(&d).unwrap();
        assert_eq!(v.len(), 1);
        let (sx, sy) = sector_xy(5, 4);
        assert_eq!(
            v[0].pos,
            Vec3::new(10.0 + 256.0 * sx as f32, 20.0 + 256.0 * sy as f32, 30.0)
        );
        assert_eq!(v[0].scale, 1.5);
        assert_eq!(v[0].group, 77);
        // 64 · 360/255 ≈ 90.35°
        let m = vegetation_rotation(&v[0]);
        assert!((m.row(0).x - (90.353f32.to_radians()).cos()).abs() < 1e-4);

        let g = read_vegetation_groups(
            r#"<Vegetation><group id="77" modelFileName="Objects\Tree.cgf" bHideability="0"/><group id="3"/></Vegetation>"#,
        );
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].model, "game/objects/tree.cgf");
        assert_eq!(g[1].model, "");
    }

    #[test]
    fn statobjs_names() {
        let mut d = 2u32.to_le_bytes().to_vec();
        for n in ["Objects/A.cgf", "objects\\b.cgf "] {
            let mut rec = vec![0u8; 256];
            rec[..n.len()].copy_from_slice(n.as_bytes());
            d.extend_from_slice(&rec);
        }
        assert_eq!(
            read_statobjs(&d),
            vec![
                "game/objects/a.cgf".to_string(),
                "game/objects/b.cgf".to_string()
            ]
        );
    }

    // ---- visareas.dat / object.dat ----

    fn put_u32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    /// Name tables: one model, no materials.
    fn name_tables(name: &str) -> Vec<u8> {
        let mut v = Vec::new();
        put_u32(&mut v, 1);
        put_u32(&mut v, 0);
        let mut n = name.as_bytes().to_vec();
        n.resize(256, 0);
        v.extend_from_slice(&n);
        put_u32(&mut v, 0);
        v
    }

    fn brush_payload(statobj: u32, pos: [f32; 3]) -> Vec<u8> {
        let mut r = vec![0u8; 128];
        let mut m = [0f32; 12];
        m[0] = 2.0;
        m[5] = 2.0;
        m[10] = 2.0;
        m[3] = pos[0];
        m[7] = pos[1];
        m[11] = pos[2];
        for (j, x) in m.iter().enumerate() {
            r[67 + 4 * j..71 + 4 * j].copy_from_slice(&x.to_le_bytes());
        }
        r[123..127].copy_from_slice(&statobj.to_le_bytes());
        r
    }

    fn voxel_payload(pos: [f32; 3], zlib: Option<(u32, &[u8])>) -> Vec<u8> {
        let mut r = vec![0u8; VOXEL_HEADER];
        r[39..43].copy_from_slice(&5u32.to_le_bytes());
        for (j, x) in [
            1.0f32, 0.0, 0.0, pos[0], 0.0, 1.0, 0.0, pos[1], 0.0, 0.0, 1.0, pos[2],
        ]
        .iter()
        .enumerate()
        {
            r[VOXEL_MATRIX + 4 * j..VOXEL_MATRIX + 4 * j + 4].copy_from_slice(&x.to_le_bytes());
        }
        if let Some((u, z)) = zlib {
            r[67] |= 0x20;
            put_u32(&mut r, 2);
            for _ in 0..2 {
                put_u32(&mut r, z.len() as u32 + 4);
                put_u32(&mut r, u);
                r.extend_from_slice(z);
            }
        }
        r
    }

    /// One node (version 2) holding `objects`, no children.
    fn node(objects: &[u8]) -> Vec<u8> {
        let mut n = vec![0u8; 33];
        n[0] = 2;
        n[28..32].copy_from_slice(&(objects.len() as u32).to_le_bytes());
        n.extend_from_slice(objects);
        n
    }

    /// A `visareas.dat` with one area (version 4) holding `octree`.
    fn visareas(octree: &[u8]) -> Vec<u8> {
        let mut v = name_tables("Objects/Hall.cgf");
        let mut body = Vec::new();
        let mut area = vec![0u8; 264];
        area[0] = 4;
        area[84..88].copy_from_slice(&(octree.len() as u32 + 4).to_le_bytes());
        body.extend_from_slice(&area);
        body.extend_from_slice(&[0u8; 12]);
        put_u32(&mut body, 1); // one point
        body.extend_from_slice(&[0u8; 12]);
        body.extend_from_slice(octree);
        body.extend_from_slice(&[0u8; 4]);
        put_u32(&mut v, 4);
        put_u32(&mut v, 20 + body.len() as u32);
        put_u32(&mut v, 1);
        put_u32(&mut v, 0);
        put_u32(&mut v, 0);
        v.extend_from_slice(&body);
        v
    }

    #[test]
    fn visareas_walks_brushes_voxels_and_skips_other_records() {
        let mut objs = Vec::new();
        put_u32(&mut objs, er_type::BRUSH);
        objs.extend_from_slice(&brush_payload(0, [1.0, 2.0, 3.0]));
        put_u32(&mut objs, er_type::DECAL);
        objs.extend_from_slice(&[0u8; 111]);
        put_u32(&mut objs, er_type::ROAD);
        let mut road = vec![0u8; 63];
        road[39..43].copy_from_slice(&2u32.to_le_bytes());
        objs.extend_from_slice(&road);
        objs.extend_from_slice(&[0u8; 24]);
        put_u32(&mut objs, er_type::VOXEL);
        objs.extend_from_slice(&voxel_payload([5.0, 6.0, 7.0], Some((3, b"xyz"))));
        let f = read_visareas(&visareas(&node(&objs))).expect("parse");
        assert_eq!(f.objects, vec!["game/objects/hall.cgf".to_string()]);
        assert_eq!(
            (f.areas, f.brushes.len(), f.voxels.len(), f.other),
            (1, 1, 1, 2)
        );
        let b = &f.brushes[0];
        assert_eq!(b.statobj, 0);
        assert_eq!(b.pos, Vec3::new(1.0, 2.0, 3.0));
        let v = &f.voxels[0];
        assert_eq!(v.pos, Vec3::new(5.0, 6.0, 7.0));
        assert_eq!(v.block, Some((3, b"xyz".to_vec())));
    }

    #[test]
    fn visareas_is_strict() {
        let mut objs = Vec::new();
        put_u32(&mut objs, er_type::BRUSH);
        objs.extend_from_slice(&brush_payload(0, [0.0; 3]));
        let good = visareas(&node(&objs));
        assert!(read_visareas(&good).is_ok());
        // Leftover bytes (the chunk size no longer matches).
        let mut extra = good.clone();
        extra.push(0);
        assert!(matches!(
            read_visareas(&extra),
            Err(OctreeError::ChunkSize { .. })
        ));
        // An unknown object type.
        let mut bad = Vec::new();
        put_u32(&mut bad, 99);
        bad.extend_from_slice(&[0u8; 16]);
        assert!(matches!(
            read_visareas(&visareas(&node(&bad))),
            Err(OctreeError::UnknownType(99, _))
        ));
        // Truncated.
        assert!(read_visareas(&good[..good.len() - 40]).is_err());
    }

    #[test]
    fn object_dat_voxels_are_found_through_children() {
        // Root with no objects, one child (mask bit 3) holding a voxel.
        let mut vox = Vec::new();
        put_u32(&mut vox, er_type::VOXEL);
        vox.extend_from_slice(&voxel_payload([1.0, 1.0, 1.0], None));
        let mut root = node(&[]);
        root[32] = 1 << 3;
        root.extend_from_slice(&node(&vox));
        let mut d = name_tables("a.cgf");
        d.extend_from_slice(&root);
        let list = read_object_dat(&d).expect("parse").voxels;
        assert_eq!(list.len(), 1);
        assert!(list[0].inflate().is_none());
        d.push(0);
        assert!(matches!(read_object_dat(&d), Err(OctreeError::Leftover(1))));
    }

    #[test]
    fn object_dat_returns_vegetation_and_skips_brushes() {
        let mut veg = vec![0u8; 64];
        for (j, v) in [10.0f32, 20.0, 30.0, 1.5].iter().enumerate() {
            veg[39 + 4 * j..43 + 4 * j].copy_from_slice(&v.to_le_bytes());
        }
        veg[55..57].copy_from_slice(&77u16.to_le_bytes());
        let mut objs = Vec::new();
        put_u32(&mut objs, er_type::BRUSH);
        objs.extend_from_slice(&[0u8; 128]);
        put_u32(&mut objs, er_type::VEGETATION);
        objs.extend_from_slice(&veg);
        put_u32(&mut objs, er_type::VEGETATION);
        objs.extend_from_slice(&veg);
        let mut d = name_tables("a.cgf");
        d.extend_from_slice(&node(&objs));
        let f = read_object_dat(&d).expect("parse");
        assert!(f.voxels.is_empty());
        assert_eq!(f.vegetation.len(), 2);
        assert_eq!(f.vegetation[0].group, 77);
        assert_eq!(f.vegetation[0].pos, Vec3::new(10.0, 20.0, 30.0));
        assert_eq!(f.vegetation[0].scale, 1.5);
        // a truncated vegetation record is an error, not a silent drop
        assert!(read_object_dat(&d[..d.len() - 10]).is_err());
    }

    #[test]
    fn voxel_blocks_inflate() {
        let raw = b"physics blob bytes".to_vec();
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6);
        let rec = VoxelRecord {
            bbox: [Vec3::ZERO; 2],
            rnd_flags: 0,
            rot_scale: Mat33::from_array([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]),
            pos: Vec3::ZERO,
            block: Some((raw.len(), z)),
        };
        assert_eq!(rec.inflate().as_deref(), Some(&raw[..]));
    }
}
