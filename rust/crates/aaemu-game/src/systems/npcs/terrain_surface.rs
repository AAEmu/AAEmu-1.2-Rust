//! Terrain place material of a world: `game/worlds/<world>/surface.xml` (world surface id ->
//! `DetailMaterial`) plus each detail material `.mtl` (`SurfaceType`), reduced to the four place
//! kinds of the 1.2 client's place-skin rule (`FUN_393a8dd0`).
//!
//! Evidence (`re-surface.md`): the client's placement ray returns the physics surface index of
//! the terrain hit, which is the `SurfaceType` of the detail-layer material under the spot; the
//! client maps the engine surface types `mat_default` -> 1, `mat_grass` -> 2,
//! `mat_grass_tall` -> 3 and `mat_mudmat_sand_dry` -> 4. A material with more than two
//! sub-materials uses sub-material 2 (`CTerrain::UpdateHeightfieldSurfaces`).
//!
//! INFERRED items (recorded in `PORTING.md`):
//! * a missing or unparsable `.mtl`, and an empty `DetailMaterial`, use `mat_default` (the
//!   engine substitutes its default material when the load fails);
//! * the permissive deviation maps `mat_sand_dry` and `mat_mud` to kind 4. The client string is
//!   almost certainly a concatenation mistake and no engine surface is named
//!   `mat_mudmat_sand_dry`, so the exact client never produces kind 4. This makes the server
//!   more permissive, never stricter;
//! * the sample between two 2 m grid lines is not traced: `WorldHeightmap::material_at` uses
//!   the lower-left sample, like the server's terrain-hole query.
//!
//! The world table is parsed lazily on the first placement check of a world; each referenced
//! `.mtl` is read lazily and only its resolved kind is cached.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::data::client_files::{decode_text, ClientFileManager};

/// The place kinds of `FUN_393a8dd0` (`doodad_place_skin_kind_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceKind {
    Default = 1,
    Grass = 2,
    GrassTall = 3,
    /// The client's `mat_mudmat_sand_dry` (never registered by the 1.2 engine). The server also
    /// maps `mat_sand_dry` and `mat_mud` here (permissive deviation).
    MudSand = 4,
}

impl SurfaceKind {
    /// The engine surface-type name -> place kind. Case-insensitive (`XlCompareStringI`).
    pub fn from_surface_type(name: &str) -> Option<Self> {
        let name = name.trim();
        if name.eq_ignore_ascii_case("mat_default") {
            Some(Self::Default)
        } else if name.eq_ignore_ascii_case("mat_grass") {
            Some(Self::Grass)
        } else if name.eq_ignore_ascii_case("mat_grass_tall") {
            Some(Self::GrassTall)
        } else if name.eq_ignore_ascii_case("mat_mudmat_sand_dry")
            || name.eq_ignore_ascii_case("mat_sand_dry")
            || name.eq_ignore_ascii_case("mat_mud")
        {
            Some(Self::MudSand)
        } else {
            None
        }
    }

    pub const fn kind_id(self) -> u32 {
        self as u32
    }
}

/// The client's terrain place material at a spot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceMaterial {
    /// No usable surface data: the world has no `surface.xml`, the cell has no heightmap, the
    /// sample is a hole, or the world surface id is not in `surface.xml`. The client's ray has no
    /// surface there, so the placement rule does nothing (permissive).
    Unknown,
    /// The material's engine surface type is one of the client's place-skin names.
    Kind(SurfaceKind),
    /// The material has a known world surface but its engine surface type is not one of the
    /// four names. The client's map lookup fails and it refuses the placement.
    Unmapped,
}

#[derive(Debug, Clone, Copy)]
enum Resolved {
    Kind(SurfaceKind),
    Unmapped,
}

impl From<Resolved> for SurfaceMaterial {
    fn from(value: Resolved) -> Self {
        match value {
            Resolved::Kind(kind) => Self::Kind(kind),
            Resolved::Unmapped => Self::Unmapped,
        }
    }
}

/// One world's `surface.xml` world-surface id -> `DetailMaterial` table, with the material names
/// resolved lazily and cached.
#[derive(Debug)]
pub struct WorldSurfaceTable {
    /// `DetailMaterial` by world surface id: `None` = the id is not in `surface.xml`, `Some("")`
    /// = empty attribute (-> `mat_default`).
    entries: Vec<Option<String>>,
    resolved: Mutex<HashMap<u32, Resolved>>,
}

/// Guards the sparse ids of a modded `surface.xml` against a huge allocation.
const MAX_SURFACE_ID: u32 = 65_535;

impl WorldSurfaceTable {
    /// Parses `game/worlds/<world>/surface.xml` (`None` = no usable `SurfaceTypes` data).
    pub fn load(files: &ClientFileManager, world: &str) -> Option<Self> {
        let path = format!("game/worlds/{world}/surface.xml");
        let text = read_client_text(files, &path)?;
        let doc = roxmltree::Document::parse(text.trim_start_matches('\u{feff}')).ok()?;
        let root = doc.root_element();
        let types = root
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "SurfaceTypes")?;
        let mut entries: Vec<Option<String>> = Vec::new();
        for node in types
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "SurfaceType")
        {
            let Some(id) = node
                .attribute("Id")
                .and_then(|v| v.trim().parse::<u32>().ok())
            else {
                continue;
            };
            if id > MAX_SURFACE_ID {
                continue;
            }
            let index = id as usize;
            if entries.len() <= index {
                entries.resize(index + 1, None);
            }
            if entries[index].is_none() {
                let material = node
                    .attribute("DetailMaterial")
                    .map(str::trim)
                    .unwrap_or("")
                    .to_string();
                entries[index] = Some(material);
            }
        }
        if entries.is_empty() {
            return None;
        }
        Some(Self {
            entries,
            resolved: Mutex::new(HashMap::new()),
        })
    }

    /// The place material of a world surface id.
    pub fn material(&self, world_surface_id: u32, files: &ClientFileManager) -> SurfaceMaterial {
        let Some(entry) = self.entries.get(world_surface_id as usize) else {
            return SurfaceMaterial::Unknown;
        };
        let Some(material) = entry else {
            return SurfaceMaterial::Unknown;
        };
        if material.is_empty() {
            // INFERRED: `DetailMaterial=""` loads the engine default material.
            return SurfaceMaterial::Kind(SurfaceKind::Default);
        }
        if let Some(resolved) = self
            .resolved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&world_surface_id)
            .copied()
        {
            return resolved.into();
        }
        let resolved = resolve_material(files, material);
        self.resolved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(world_surface_id, resolved);
        resolved.into()
    }
}

/// The `.mtl` `SurfaceType` of a detail material, reduced to a place kind.
fn resolve_material(files: &ClientFileManager, material: &str) -> Resolved {
    let Some(text) = read_material(files, material) else {
        // INFERRED: the engine replaces a material that fails to load with its default one.
        return Resolved::Kind(SurfaceKind::Default);
    };
    match surface_type_of_mtl(&text) {
        Some(name) => match SurfaceKind::from_surface_type(&name) {
            Some(kind) => Resolved::Kind(kind),
            None => Resolved::Unmapped,
        },
        // INFERRED: a material without a usable `SurfaceType` is the default material.
        None => Resolved::Kind(SurfaceKind::Default),
    }
}

/// The `SurfaceType` of a `.mtl`: sub-material 2 when the material has more than two, else the
/// root `Material` (`CTerrain::UpdateHeightfieldSurfaces`). Empty/missing -> `mat_default`.
pub fn surface_type_of_mtl(text: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(text.trim_start_matches('\u{feff}')).ok()?;
    let root = doc.root_element();
    if root.tag_name().name() != "Material" {
        return None;
    }
    let sub_materials: Vec<_> = root
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "SubMaterials")
        .map(|node| {
            node.children()
                .filter(|n| n.is_element() && n.tag_name().name() == "Material")
                .collect()
        })
        .unwrap_or_default();
    let material = if sub_materials.len() > 2 {
        sub_materials[2]
    } else {
        root
    };
    let surface_type = material
        .attribute("SurfaceType")
        .map(str::trim)
        .unwrap_or("");
    Some(if surface_type.is_empty() {
        "mat_default".to_string()
    } else {
        surface_type.to_string()
    })
}

/// Reads a detail `.mtl` (`<DetailMaterial>.mtl`, with or without the `game/` prefix and with a
/// case-insensitive path).
fn read_material(files: &ClientFileManager, material: &str) -> Option<String> {
    let mut base = material.trim().replace('\\', "/");
    if base.is_empty() {
        return None;
    }
    if !base.to_ascii_lowercase().ends_with(".mtl") {
        base.push_str(".mtl");
    }
    let mut candidates = vec![base.clone()];
    if !base.to_ascii_lowercase().starts_with("game/") {
        candidates.push(format!("game/{base}"));
    }
    // The pak stores `Materials/Terrain/x` as `game/materials/terrain/x.mtl`.
    let lowered: Vec<String> = candidates
        .iter()
        .map(|candidate| candidate.to_ascii_lowercase())
        .collect();
    candidates.extend(lowered);
    for candidate in candidates {
        if let Some(text) = read_client_text(files, &candidate) {
            return Some(text);
        }
    }
    None
}

/// Reads a client text file, falling back to a case-insensitive file-name match.
fn read_client_text(files: &ClientFileManager, path: &str) -> Option<String> {
    if let Some(bytes) = files.read_file(path) {
        return Some(decode_text(&bytes));
    }
    let found = find_case_insensitive(files, path)?;
    files.read_file(&found).map(|bytes| decode_text(&bytes))
}

fn find_case_insensitive(files: &ClientFileManager, path: &str) -> Option<String> {
    let (directory, name) = path.rsplit_once('/')?;
    for candidate in files.files_in_directory(directory, "*", false) {
        let base = candidate.rsplit(['/', '\\']).next().unwrap_or(&candidate);
        if base.eq_ignore_ascii_case(name) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::data::pak::test_util::temp_path;

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn surface_xml() -> &'static str {
        r#"<Surface>
 <SurfaceTypes>
  <SurfaceType Name="plain" DetailMaterial="materials/plain" Id="0"/>
  <SurfaceType Name="multi" DetailMaterial="Materials/Multi" Id="1"/>
  <SurfaceType Name="missing" DetailMaterial="game/materials/missing" Id="2"/>
  <SurfaceType Name="tall" DetailMaterial="GAME/MATERIALS/Grass_Tall.MTL" Id="3"/>
  <SurfaceType Name="empty" DetailMaterial="" Id="4"/>
  <SurfaceType Name="two" DetailMaterial="materials/two" Id="5"/>
  <SurfaceType Name="sand" DetailMaterial="materials/sand" Id="6"/>
  <SurfaceType Name="dirt" DetailMaterial="materials/dirt" Id="7"/>
  <SurfaceType Name="mixed" DetailMaterial="materials/MixedCase" Id="8"/>
  <SurfaceType Name="absent"> </SurfaceType>
  <SurfaceType Name="out" DetailMaterial="materials/plain" Id="99"/>
 </SurfaceTypes>
</Surface>"#
    }

    fn material(surface_type: &str) -> String {
        format!("<Material Shader=\"Terrain.Layer\" SurfaceType=\"{surface_type}\"/>")
    }

    fn multi(surface_types: &[&str]) -> String {
        let mut text = String::from("<Material Shader=\"Terrain.Layer\"><SubMaterials>");
        for surface_type in surface_types {
            text.push_str(&format!("<Material SurfaceType=\"{surface_type}\"/>"));
        }
        text.push_str("</SubMaterials></Material>");
        text
    }

    fn fixture() -> (PathBuf, ClientFileManager) {
        let root = temp_path("surface");
        let _ = std::fs::remove_dir_all(&root);
        write(&root, "game/worlds/test_world/surface.xml", surface_xml());
        write(&root, "game/materials/plain.mtl", &material("mat_grass"));
        write(
            &root,
            "game/materials/multi.mtl",
            &multi(&["mat_grass", "mat_dirt", "mat_rock"]),
        );
        write(
            &root,
            "game/materials/grass_tall.mtl",
            &material("mat_grass_tall"),
        );
        write(
            &root,
            "game/materials/two.mtl",
            &multi(&["mat_grass", "mat_rock"]),
        );
        write(&root, "game/materials/sand.mtl", &material("mat_sand_dry"));
        write(&root, "game/materials/dirt.mtl", &material("mat_dirt"));
        write(
            &root,
            "game/materials/MixedCase.MTL",
            &material("mat_grass"),
        );
        let files = ClientFileManager::from_sources(&[root.to_string_lossy().into_owned()], &root);
        (root, files)
    }

    #[test]
    fn surface_types_use_the_root_or_sub_material_two_and_permissive_sand_and_mud() {
        let (root, files) = fixture();
        let table = WorldSurfaceTable::load(&files, "test_world").expect("surface.xml");
        assert_eq!(
            table.material(0, &files),
            SurfaceMaterial::Kind(SurfaceKind::Grass)
        );
        assert_eq!(table.material(1, &files), SurfaceMaterial::Unmapped);
        assert_eq!(
            table.material(2, &files),
            SurfaceMaterial::Kind(SurfaceKind::Default)
        );
        assert_eq!(
            table.material(3, &files),
            SurfaceMaterial::Kind(SurfaceKind::GrassTall)
        );
        assert_eq!(
            table.material(4, &files),
            SurfaceMaterial::Kind(SurfaceKind::Default)
        );
        // Exactly two sub-materials: the root attribute (none) -> mat_default, not sub-material 2.
        assert_eq!(
            table.material(5, &files),
            SurfaceMaterial::Kind(SurfaceKind::Default)
        );
        assert_eq!(
            table.material(6, &files),
            SurfaceMaterial::Kind(SurfaceKind::MudSand)
        );
        assert_eq!(table.material(7, &files), SurfaceMaterial::Unmapped);
        // Case-insensitive fallback for a file whose on-disk case differs from the path.
        assert_eq!(
            table.material(8, &files),
            SurfaceMaterial::Kind(SurfaceKind::Grass)
        );
        // An id that is not in surface.xml is unknown (permissive), and so is an invalid id.
        assert_eq!(table.material(50, &files), SurfaceMaterial::Unknown);
        assert_eq!(table.material(1000, &files), SurfaceMaterial::Unknown);
        // Parsed once and cached (the empty-attribute id answers without loading a file).
        assert_eq!(table.resolved.lock().unwrap().len(), 8);
        assert_eq!(table.material(1, &files), SurfaceMaterial::Unmapped);
        assert_eq!(table.resolved.lock().unwrap().len(), 8);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn mtl_with_up_to_two_sub_materials_uses_the_root_surface_type() {
        assert_eq!(
            surface_type_of_mtl("<Material SurfaceType=\"mat_grass\"/>").as_deref(),
            Some("mat_grass")
        );
        assert_eq!(
            surface_type_of_mtl(
                "<Material SurfaceType=\"mat_grass\"><SubMaterials>\
                 <Material SurfaceType=\"mat_rock\"/>\
                 <Material SurfaceType=\"mat_dirt\"/>\
                 </SubMaterials></Material>"
            )
            .as_deref(),
            Some("mat_grass")
        );
        assert_eq!(
            surface_type_of_mtl(
                "<Material><SubMaterials>\
                 <Material SurfaceType=\"mat_grass\"/>\
                 <Material SurfaceType=\"mat_dirt\"/>\
                 <Material SurfaceType=\"mat_rock\"/>\
                 </SubMaterials></Material>"
            )
            .as_deref(),
            Some("mat_rock")
        );
        assert_eq!(
            surface_type_of_mtl(
                "<Material><SubMaterials><Material/><Material/><Material/></SubMaterials></Material>"
            )
            .as_deref(),
            Some("mat_default")
        );
        assert_eq!(
            surface_type_of_mtl("<Material SurfaceType=\" MAT_MUD \"/>").as_deref(),
            Some("MAT_MUD")
        );
        assert_eq!(
            SurfaceKind::from_surface_type("MAT_MUD"),
            Some(SurfaceKind::MudSand)
        );
    }

    #[test]
    fn a_world_without_surface_xml_has_no_material_data() {
        let root = temp_path("no_surface");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let files = ClientFileManager::from_sources(&[root.to_string_lossy().into_owned()], &root);
        assert!(WorldSurfaceTable::load(&files, "test_world").is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod real_data_tests {
    use std::path::PathBuf;

    use super::*;
    use crate::data::world_templates::WorldTemplates;
    use crate::systems::npcs::heightmap::{HeightmapConfig, HeightmapWorlds};

    fn app_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("app")
    }

    /// `AAEMU_GAME_PAK=D:\AA1.2\game_pak cargo test -p aaemu-game real_main_world_surface --
    /// --ignored --nocapture`: the 1.2 `main_world` surface names and the place material of a
    /// few coordinates pinned from `scratch/surface/worldsurface_to_place_kind.tsv`:
    /// world surface 301 = `mat_grass` (kind 2) in cell 010_010, 168 = `mat_dirt` (unmapped),
    /// 194 = `mat_mud` (permissive kind 4) in cell 011_011, plus the verified hole point.
    #[test]
    #[ignore = "needs AAEMU_GAME_PAK"]
    fn real_main_world_surface() {
        let Ok(pak) = std::env::var("AAEMU_GAME_PAK") else {
            return;
        };
        let app = app_path();
        let files = std::sync::Arc::new(ClientFileManager::from_sources(&[pak], &app));
        let worlds = WorldTemplates::load(&files, &app).expect("world templates");
        let main = worlds.get_world_template_by_name("main_world").unwrap();
        let maps = HeightmapWorlds::new(
            files.clone(),
            &worlds,
            HeightmapConfig {
                heightmaps_enable: true,
                placement_strict_surface: true,
                ..Default::default()
            },
        );
        let map = maps.template(main.id).unwrap();

        // Direct name / sub-material resolution of real surface ids.
        let started = std::time::Instant::now();
        let table = WorldSurfaceTable::load(&files, "main_world").expect("surface.xml");
        eprintln!(
            "surface.xml parsed in {:?}, {} entries retained",
            started.elapsed(),
            table.entries.len()
        );
        assert_eq!(
            table.material(3, &files),
            SurfaceMaterial::Kind(SurfaceKind::Grass)
        );
        assert_eq!(table.material(1, &files), SurfaceMaterial::Unmapped);
        assert_eq!(
            table.material(420, &files),
            SurfaceMaterial::Kind(SurfaceKind::GrassTall)
        );
        // No .mtl in the pak: the engine default material.
        assert_eq!(
            table.material(4, &files),
            SurfaceMaterial::Kind(SurfaceKind::Default)
        );

        // world (10241, 10241) -> grid (5120, 5120) -> local table id 301 -> mat_grass.
        let started = std::time::Instant::now();
        assert_eq!(map.grid_surface_id(&files, 5120, 5120), Some(301));
        eprintln!(
            "first heightmap cell from the pak in {:?}",
            started.elapsed()
        );
        assert_eq!(
            map.material_at(&files, 10241.0, 10241.0),
            SurfaceMaterial::Kind(SurfaceKind::Grass)
        );
        // world (10321, 10241) -> grid (5160, 5120) -> id 463 -> mat_rock (client refuses).
        assert_eq!(
            map.material_at(&files, 10321.0, 10241.0),
            SurfaceMaterial::Unmapped
        );
        // world (11173, 10303) -> grid (5586, 5151) -> id 180 -> mat_sand_dry (permissive 4).
        assert_eq!(
            map.material_at(&files, 11173.0, 10303.0),
            SurfaceMaterial::Kind(SurfaceKind::MudSand)
        );
        // The verified hole point (21204.25, 9026.5) has no surface (local id 31).
        assert_eq!(
            map.material_at(&files, 21204.25, 9026.5),
            SurfaceMaterial::Unknown
        );
    }
}
