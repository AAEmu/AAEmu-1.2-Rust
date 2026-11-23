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

mod cells;

#[cfg(test)]
use self::cells::lerp;
pub use self::cells::WorldHeightmap;

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
pub mod hmap;
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
mod tests;

#[cfg(test)]
mod real_data_tests;
