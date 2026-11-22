//! Geodata (navmesh) for the AI: the CryEngine `.bai` files ([`bai`]), `AiGeoDataManager`
//! ([`manager`]) and the A* `PathNode` ([`astar`]), plus the world-side entry points the AI and
//! the height queries use (`Npc.FindPath`, `WorldTemplate.GeoData.GetHeight`).
//!
//! Every world template gets an `AiGeoData` at startup (`WorldManager.LoadWorlds`); the `.bai`
//! files are only read with `World.GeoDataMode` (zone folders at startup like
//! `LoadZoneBaiFiles`, the 4x4 path blocks of a cell when the cell is first used).

pub mod astar;
pub mod bai;
pub mod funnel;
pub mod graph;
pub mod manager;
#[cfg(test)]
mod real_data;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::data::client_files::ClientFileManager;
use crate::server::GameServer;
use crate::systems::npcs::heightmap::HeightmapWorlds;
use crate::systems::skills::controllers::Vec3;
use crate::world::WorldObject;

use self::bai::{BaiFiles, UsedAreaNames};
use self::graph::DEFAULT_AGENT_RADIUS;
use self::manager::{AiGeoData, GeoError, TerrainHeights};
use super::framework::AiCtx;

/// The client files as a bai source.
pub struct ClientBaiFiles(pub Arc<ClientFileManager>);

impl BaiFiles for ClientBaiFiles {
    fn files_in_directory(&self, dir: &str, pattern: &str) -> Vec<String> {
        self.0.files_in_directory(dir, pattern, true)
    }

    fn file_exists(&self, name: &str) -> bool {
        self.0.file_exists(name)
    }

    fn read_file(&self, name: &str) -> Option<Vec<u8>> {
        self.0.read_file(name)
    }
}

/// The heightmap of one world template as the geodata's terrain fallback.
pub struct HeightmapTerrain {
    pub worlds: Arc<HeightmapWorlds>,
    pub template_id: u32,
}

impl TerrainHeights for HeightmapTerrain {
    fn raw_height(&self, x: i32, y: i32) -> Result<f32, GeoError> {
        match self.worlds.template(self.template_id) {
            Some(t) => t
                .get_raw_height_map_height(self.worlds.files(), x, y)
                .map_err(|_| GeoError),
            None => Ok(0.0),
        }
    }

    fn height(&self, x: f32, y: f32) -> Result<f32, GeoError> {
        self.worlds
            .world_height(self.template_id, x, y)
            .map_err(|_| GeoError)
    }
}

/// `WorldTemplate.GeoData` of every template (`server.systems.ai.geodata`).
#[derive(Debug, Default)]
pub struct GeoDataStore {
    by_template: RwLock<HashMap<u32, Arc<AiGeoData>>>,
}

impl GeoDataStore {
    /// The geodata of a world template.
    pub fn get(&self, template_id: u32) -> Option<Arc<AiGeoData>> {
        self.by_template
            .read()
            .expect("geodata poisoned")
            .get(&template_id)
            .cloned()
    }

    /// Installs one template's geodata (startup / tests).
    pub fn install(&self, g: Arc<AiGeoData>) {
        self.by_template
            .write()
            .expect("geodata poisoned")
            .insert(g.template_id, g);
    }

    pub fn len(&self) -> usize {
        self.by_template.read().expect("geodata poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Startup: an `AiGeoDataManager` per world template (`worldTemplate.GeoData.Load()` is a
/// no-op) and, with `GeoDataMode`, `LoadZoneBaiFiles` for every template.
pub fn start(server: &Arc<GameServer>) {
    let Some(heights) = server.systems.npcs.heightmaps.get() else {
        return;
    };
    let enabled = server.config.world.geo_data_mode;
    let files: Arc<dyn BaiFiles + Send + Sync> = Arc::new(ClientBaiFiles(heights.files().clone()));
    let used = Arc::new(UsedAreaNames::default());
    let mut all: HashMap<u32, Arc<AiGeoData>> = HashMap::new();
    for t in server.game_data.worlds.templates() {
        let terrain: Arc<dyn TerrainHeights> = Arc::new(HeightmapTerrain {
            worlds: heights.clone(),
            template_id: t.id,
        });
        let g = Arc::new(AiGeoData::new(
            t,
            enabled,
            files.clone(),
            used.clone(),
            Some(terrain),
        ));
        if enabled {
            g.zone_bai();
        }
        all.insert(t.id, g.clone());
        server.systems.ai.geodata.install(g);
    }
    // WorldManager.GetHeight: the navmesh first with GeoDataMode
    heights.install_geo_data(Arc::new(move |template_id, x, y, z| {
        all.get(&template_id)
            .map_or(0.0, |g| g.get_height(Vec3::new(x, y, z)))
    }));
}

/// The four cells sharing an edge with a player's current cell.
const BAI_PREWARM_NEIGHBOURS: [(i32, i32); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];

/// Warm one cell per pass, independently of the AI and collision ticks. Await each load so a
/// slow read cannot queue duplicate or stale requests. Route lookups can still race a prewarm
/// and wait for the same cell to finish loading.
pub(super) fn start_bai_prewarm(server: &Arc<GameServer>) {
    if !(server.config.world.geo_data_mode && server.config.world.prewarm_navmesh) {
        return;
    }
    let weak = Arc::downgrade(server);
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_millis(500));
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            iv.tick().await;
            let Some(server) = weak.upgrade() else { return };
            let candidate = bai_prewarm_candidate(&server);
            drop(server);
            let Some((geo, x, y)) = candidate else {
                continue;
            };
            if tokio::task::spawn_blocking(move || {
                geo.cell_bai(x, y);
                tracing::debug!(world = %geo.world_name, cell = ?(x, y), "navmesh cell prewarmed");
            })
            .await
            .is_err()
            {
                tracing::warn!(cell = ?(x, y), "navmesh prewarm task panicked");
            }
        }
    });
}

fn bai_prewarm_candidate(server: &GameServer) -> Option<(Arc<AiGeoData>, i32, i32)> {
    let mut focus = Vec::new();
    for world in server.world.worlds() {
        let Some(geo) = server.systems.ai.geodata.get(world.template.id) else {
            continue;
        };
        for p in world.all_characters() {
            let pos = world.world_pos_rot(&WorldObject::from(&p));
            let (cx, cy) = manager::to_cell_index(Vec3::new(pos.x, pos.y, pos.z));
            if geo.is_cell_prewarm_needed(cx, cy) {
                return Some((geo, cx, cy));
            }
            focus.push((geo.clone(), cx, cy));
        }
    }
    for (dx, dy) in BAI_PREWARM_NEIGHBOURS {
        for (geo, cx, cy) in &focus {
            let (x, y) = (cx + dx, cy + dy);
            if geo.is_cell_prewarm_needed(x, y) {
                return Some((geo.clone(), x, y));
            }
        }
    }
    None
}

/// The geodata of the world an object is in.
fn geo_of(server: &GameServer, obj: &WorldObject) -> Option<Arc<AiGeoData>> {
    let world = crate::systems::skills::geometry::world_of(server, obj)?;
    server.systems.ai.geodata.get(world.template.id)
}

/// `ParentWorld.Template.GeoData.GetHeight(pos)` (raw heightmap fallback; 0 without data).
pub fn geo_data_height(server: &GameServer, obj: &WorldObject, pos: Vec3) -> f32 {
    match geo_of(server, obj) {
        Some(g) => g.get_height(pos),
        None => 0.0,
    }
}

/// The route from the NPC to `target` as `FoundPath` (see [`npc_find_path`]); the number of
/// raw steps, `None` when there is no geodata or the search failed (`FoundPath` is then empty).
/// Also used for the walk home of a returning NPC, which has no victim.
pub fn plan_route(c: &mut AiCtx<'_>, target: Vec3) -> Option<usize> {
    let me = super::npc::world_pos(c.server, &c.obj);
    c.ai.path_node.start_point_pos = me;
    c.ai.path_node.end_point_pos = target;
    c.ai.path_node.zone_key = c.obj.transform().zone_id;
    c.ai.path_node.found_path.clear();
    let geo = geo_of(c.server, &c.obj)?;
    let (start, end) = (c.ai.path_node.start_point_pos, c.ai.path_node.end_point_pos);
    // A local ground route can be shorter than the authored navmesh when the victim
    // is outside its triangles. Require continuous terrain support and a wall-free route.
    if matches!(
        geo.line_passes_through_forbidden_area(start, end),
        Ok(false)
    ) && direct_ground_route(c.server, c.obj.transform().instance_id, start, end)
    {
        c.ai.path_node.found_path.push_back(end);
        return Some(1);
    }
    let smooth = match c
        .ai
        .path_node
        .find_smooth_path(&geo, start, end, DEFAULT_AGENT_RADIUS)
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("FindPath for npc {}: {}", c.npc.obj_id, e.0);
            return None;
        }
    };
    let raw_steps = match smooth {
        Some(s) => {
            c.ai.path_node.found_path = s.points.into();
            s.corridor_len
        }
        None => fallback_path(c, &geo, start, end, me, target)?,
    };
    // Chase regeneration compares with the victim's actual position, not a node's centroid.
    c.ai.path_node.end_point_pos = target;
    Some(raw_steps)
}

fn direct_ground_route(server: &GameServer, instance_id: u32, start: Vec3, end: Vec3) -> bool {
    use super::collision::{
        ground_height_probe,
        walk::{terrain_supports_step, walk_blocked},
    };

    let distance = (end.x - start.x).hypot(end.y - start.y);
    if !(0.25..=24.0).contains(&distance) || (end.z - start.z).abs() > 4.0 {
        return false;
    }
    let segments = (distance / 3.0).ceil() as u32;
    let mut from = [start.x, start.y, start.z];
    for step in 1..=segments {
        let t = step as f32 / segments as f32;
        let x = start.x + t * (end.x - start.x);
        let y = start.y + t * (end.y - start.y);
        let Some(Some(z)) = ground_height_probe(server, instance_id, x, y, from[2], 1.0, 1.0)
        else {
            return false;
        };
        let to = [x, y, z];
        if !terrain_supports_step(server, instance_id, from, to)
            || walk_blocked(server, instance_id, from, to) != Some(false)
        {
            return false;
        }
        from = to;
    }
    (from[2] - end.z).abs() <= 1.0
}

/// `Npc.FindPath(abuser)`: a route from the NPC to the abuser as `FoundPath`; a chased player
/// gets the debug chat lines the C# sends.
///
/// The route is the navmesh corridor pulled tight with the funnel algorithm (or the straight
/// line when it crosses only linked triangles), see [`PathNode::find_smooth_path`]. When the
/// navmesh cannot give one (waypoint zones, edges unknown, forbidden areas in the way) the C#
/// pipeline remains: the A* node positions, `ReducePath`.
pub fn npc_find_path(c: &mut AiCtx<'_>, abuser: &WorldObject) {
    let target = super::npc::world_pos(c.server, abuser);
    let Some(raw_steps) = plan_route(c, target) else {
        return;
    };
    if let Some(p) = abuser.as_player() {
        crate::world::session::send_message(
            p,
            -2,
            &format!(
                "Aggro from {}, getting attack path in {}/{} steps",
                c.npc.obj_id,
                c.ai.path_node.found_path.len(),
                raw_steps
            ),
        );
        for v in &c.ai.path_node.found_path {
            crate::world::session::send_message(
                p,
                -2,
                &format!("Path step -> <{}, {}, {}>", v.x, v.y, v.z),
            );
        }
    }
}

/// The C# route: the A* node positions with the NPC and the target as end points, reduced by
/// `ReducePath`. Returns the number of raw steps; `None` after logging an exception.
fn fallback_path(
    c: &mut AiCtx<'_>,
    geo: &AiGeoData,
    start: Vec3,
    end: Vec3,
    me: Vec3,
    target: Vec3,
) -> Option<usize> {
    let mut res_list = match c.ai.path_node.find_path(geo, start, end) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("FindPath for npc {}: {}", c.npc.obj_id, e.0);
            return None;
        }
    };
    // The enclosing nodes select topology, not mandatory detours through the start
    // and goal triangle centroids. Native direct paths (0x31062728) use actor positions.
    if let Some(first) = res_list.first_mut() {
        *first = me;
    }
    if res_list.len() > 1 {
        *res_list.last_mut().expect("nonempty path") = target;
    }
    if res_list.last() != Some(&target) {
        res_list.push(target);
    }
    let instance_id = c.obj.transform().instance_id;
    let zone_key = c.obj.transform().zone_id;
    if let Some(r) = walkable_reduction(c.server, geo, zone_key, instance_id, &res_list, 10) {
        c.ai.path_node.found_path = r;
        return Some(res_list.len());
    }
    match geo.reduce_path(&res_list, 10) {
        Ok(r) => c.ai.path_node.found_path = r,
        Err(GeoError) => {
            tracing::error!(
                "ReducePath for npc {}: NullReferenceException (no bai at the start node)",
                c.npc.obj_id
            );
            return None;
        }
    }
    Some(res_list.len())
}

/// `ReducePath` for a server with the collision service: from each kept node, skip ahead to the
/// farthest of the next `max_node_skip` nodes that a walker reaches in a straight line
/// ([`straight_walk_is_clear`]). The C# skip only tested the slope and the forbidden areas, so
/// in the indoor waypoint graphs (Howling Abyss) the 10-node skip cut the stairs from a
/// corridor up to a platform and the chaser walked into the wall under the platform (deviation,
/// see `PORTING.md`). `None` without the collision service (the caller keeps `ReducePath`).
///
/// A shortcut must also stay clear of the bai's forbidden areas, boundaries and designer
/// forbidden areas, as in `ReducePath`, unless both of its ends are indoor (their nearest
/// navigation node is a waypoint, [`AiGeoData::is_indoor`]): the forbidden areas are 2D
/// outlines of the outdoor triangulation, and an instance hall lies inside the outline of the
/// terrain under it.
fn walkable_reduction(
    server: &GameServer,
    geo: &AiGeoData,
    zone_key: u32,
    instance_id: u32,
    found: &[Vec3],
    max_node_skip: usize,
) -> Option<std::collections::VecDeque<Vec3>> {
    super::collision::phys_world(server, instance_id)?;
    let indoor: Vec<bool> = found.iter().map(|p| geo.is_indoor(zone_key, *p)).collect();
    let shortcut = |from: usize, to: usize| {
        (indoor[from] && indoor[to]
            || matches!(
                geo.line_passes_through_forbidden_area(found[from], found[to]),
                Ok(false)
            ))
            && straight_walk_is_clear(server, instance_id, found[from], found[to])
    };
    let mut res = std::collections::VecDeque::new();
    let mut start = 0usize;
    while start < found.len() {
        res.push_back(found[start]);
        let last = (start + max_node_skip).min(found.len() - 1);
        let next = (start + 1..=last)
            .rev()
            .find(|&end| end == start + 1 || shortcut(start, end))
            .unwrap_or(start + 1);
        start = next;
    }
    Some(res)
}

/// The steepest climb (metres per horizontal metre, 45 degrees) of [`straight_walk_is_clear`].
/// The first corridor's stairs rise at most 0.6 m per metre.
const MAX_WALK_CLIMB: f32 = 1.0;
/// Allowance (m) for a stair tread edge between two samples.
const WALK_CLIMB_SLACK: f32 = 0.25;

/// Whether a walker gets from `a` to `b` in a straight line: no wall in the way and a floor
/// every metre that is neither steeper than [`MAX_WALK_CLIMB`] upwards nor a drop deeper than
/// the probe's reach (stairs and ramps pass, a ledge or a wall under a platform does not).
///
/// The climb has to be limited per metre, not only by the 2 m probe: the side of the Howling
/// Abyss entrance platform is a steep lip (180.1 -> 181.5 -> 183.2 -> 184.75 over 3 m, about
/// 56 degrees) that the probe climbs step by step, so the route up from the first corridor
/// went straight over it instead of the stairs beside it (live check of #2).
fn straight_walk_is_clear(server: &GameServer, instance_id: u32, a: Vec3, b: Vec3) -> bool {
    use super::collision::{ground_height_probe, walk::walk_blocked, GROUND_PROBE_UP};

    let distance = (b.x - a.x).hypot(b.y - a.y);
    let samples = distance.ceil().max(1.0) as u32;
    let mut z = a.z;
    let mut from = [a.x, a.y, a.z];
    for i in 1..=samples {
        let t = i as f32 / samples as f32;
        let (x, y) = (a.x + t * (b.x - a.x), a.y + t * (b.y - a.y));
        match ground_height_probe(
            server,
            instance_id,
            x,
            y,
            z,
            GROUND_PROBE_UP,
            GROUND_PROBE_UP,
        ) {
            Some(Some(h)) => {
                if h - z > MAX_WALK_CLIMB * distance / samples as f32 + WALK_CLIMB_SLACK {
                    return false;
                }
                z = h;
            }
            Some(None) => return false,
            None => return true,
        }
        let to = [x, y, z];
        if walk_blocked(server, instance_id, from, to) == Some(true) {
            return false;
        }
        from = to;
    }
    (z - b.z).abs() <= GROUND_PROBE_UP
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::systems::ai::collision::{
        ground_height_probe, terrain::TerrainSource, walk::walk_blocked,
    };

    struct FlatTerrain {
        hole: bool,
    }

    impl TerrainSource for FlatTerrain {
        fn sample(&self, _: i32, _: i32) -> f32 {
            100.0
        }

        fn size(&self) -> (i32, i32) {
            (512, 512)
        }

        fn is_hole(&self, x: i32, y: i32) -> bool {
            self.hole && (x, y) == (51, 50)
        }
    }

    #[tokio::test]
    async fn direct_ground_route_rejects_a_hole_between_floor_probes() {
        let start = Vec3::new(101.5, 101.0, 100.0);
        let end = Vec3::new(104.5, 101.0, 100.0);
        for hole in [false, true] {
            let server = crate::test_support::test_server_with(crate::data::GameData::empty());
            server
                .systems
                .ai
                .collision
                .test_files
                .set(Arc::new(HashMap::new()))
                .expect("test collision files");
            server
                .systems
                .ai
                .collision
                .test_terrain
                .set(Arc::new(FlatTerrain { hole }))
                .unwrap_or_else(|_| panic!("terrain is already set"));
            // A 2 m hole lies strictly inside this 3 m step. Both of the old
            // sample positions still see the same floor, with no wall between them.
            for point in [start, end] {
                assert_eq!(
                    ground_height_probe(&server, 0, point.x, point.y, point.z, 1.0, 1.0),
                    Some(Some(100.0)),
                );
            }
            assert_eq!(
                walk_blocked(
                    &server,
                    0,
                    [start.x, start.y, start.z],
                    [end.x, end.y, end.z],
                ),
                Some(false)
            );
            assert_eq!(direct_ground_route(&server, 0, start, end), !hole);
        }
    }

    /// Client files in memory.
    struct MemBai(HashMap<String, Vec<u8>>);

    impl BaiFiles for MemBai {
        fn files_in_directory(&self, dir: &str, _pattern: &str) -> Vec<String> {
            let mut v: Vec<String> = self
                .0
                .keys()
                .filter(|k| k.starts_with(dir))
                .cloned()
                .collect();
            v.sort();
            v
        }

        fn file_exists(&self, name: &str) -> bool {
            self.0.contains_key(name)
        }

        fn read_file(&self, name: &str) -> Option<Vec<u8>> {
            self.0.get(name).cloned()
        }
    }

    /// A route around a forbidden square at (20, 20) over flat ground: the three route nodes
    /// are navigation nodes of `nav_type` (2 = outdoor triangles, 4 = indoor waypoints).
    fn forbidden_square_geo(nav_type: u16, forbidden: bool) -> manager::AiGeoData {
        use self::bai::test_files::{areas_mission, net_mission_versioned, TLink, TNode};
        let t = crate::data::world_templates::WorldTemplate::synthetic(0, "main_world", 2, 2, 100);
        let nodes: Vec<TNode> = [
            [20.0, 10.0, 100.0],
            [10.0, 20.0, 100.0],
            [20.0, 30.0, 100.0],
        ]
        .into_iter()
        .enumerate()
        .map(|(i, pos)| TNode {
            id: i as i32 + 1,
            pos,
            tri: [-1, -1, -1],
            nav_type,
            flags: 0,
        })
        .collect();
        let links = [
            TLink::new(1, 2, 3.0),
            TLink::new(2, 1, 3.0),
            TLink::new(2, 3, 3.0),
            TLink::new(3, 2, 3.0),
        ];
        let dir = "game/worlds/main_world/paths/000_000";
        let mut files = HashMap::new();
        files.insert(
            format!("{dir}/netmission0.bai"),
            net_mission_versioned(55, &nodes, &links),
        );
        if forbidden {
            files.insert(
                format!("{dir}/areasmission0.bai"),
                areas_mission(&[(
                    "block",
                    vec![
                        [18.0, 18.0, 0.0],
                        [22.0, 18.0, 0.0],
                        [22.0, 22.0, 0.0],
                        [18.0, 22.0, 0.0],
                    ],
                )]),
            );
        }
        manager::AiGeoData::new(
            &t,
            true,
            Arc::new(MemBai(files)),
            Arc::new(UsedAreaNames::default()),
            None,
        )
    }

    /// The collision shortcut of a fallback route keeps `ReducePath`'s forbidden-area test
    /// outdoors: the straight walk (20, 10) -> (20, 30) is clear ground but crosses the
    /// forbidden square, so the detour node stays. Indoors the forbidden outlines belong to the
    /// terrain under the hall and the shortcut is taken.
    #[tokio::test]
    async fn walkable_reduction_keeps_outdoor_routes_out_of_forbidden_areas() {
        let server = crate::test_support::test_server_with(crate::data::GameData::empty());
        server
            .systems
            .ai
            .collision
            .test_files
            .set(Arc::new(HashMap::new()))
            .expect("test collision files");
        server
            .systems
            .ai
            .collision
            .test_terrain
            .set(Arc::new(FlatTerrain { hole: false }))
            .unwrap_or_else(|_| panic!("terrain is already set"));
        let route = [
            Vec3::new(20.0, 10.0, 100.0),
            Vec3::new(10.0, 20.0, 100.0),
            Vec3::new(20.0, 30.0, 100.0),
        ];
        let shortcut = vec![route[0], route[2]];
        let reduce = |nav_type: u16, forbidden: bool| -> Vec<Vec3> {
            let geo = forbidden_square_geo(nav_type, forbidden);
            assert_eq!(
                geo.is_indoor(100, route[1]),
                nav_type != manager::NAV_TRIANGULAR
            );
            walkable_reduction(&server, &geo, 100, 0, &route, 10)
                .expect("collision service")
                .into_iter()
                .collect()
        };
        // Clear ground without forbidden areas: the walk itself allows the shortcut.
        assert_eq!(reduce(manager::NAV_TRIANGULAR, false), shortcut);
        // Outdoors the forbidden square keeps the detour.
        assert_eq!(reduce(manager::NAV_TRIANGULAR, true), route.to_vec());
        // Indoor waypoints are exempt.
        assert_eq!(reduce(4, true), shortcut);
    }
}
