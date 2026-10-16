//! `Models/Game/AI/AStar/PathNode`: the reusable A* pathfinder over the navmesh nodes and
//! links of the `.bai` files.
//!
//! Engine behaviour instead of the C# (see `PORTING.md` "geodata deviations"):
//! * the search runs on the merged [`NavGraph`](super::graph::NavGraph), so it crosses sector
//!   and cell borders (the cells a node's triangle touches are loaded before its links are
//!   followed);
//! * the start / goal node is the triangle enclosing the point (`GetEnclosing`), the C#
//!   nearest node only when no triangle encloses it;
//! * links whose `maxPassRadius` is below the agent radius, and blocked links (negative
//!   radius, -1 in the files), are not traversed. [`PathNode::find_path`] uses
//!   [`DEFAULT_AGENT_RADIUS`] (the X2 walkability probe radius 0.25);
//!   [`PathNode::find_path_with_radius`] takes the caller's;
//! * the cost is a real A* cost: `G` is the accumulated 2D length of the route (node to the
//!   middle of the shared edge to the next node) and `H` the 2D distance from the node to the
//!   goal node, which never overestimates. The C# used `G` = distance from the link's *source*
//!   node to the goal and 3D distances, i.e. a greedy search on node heights: the node z of a
//!   huge triangle is unreliable (a centroid may sit 100 m below the ground that triangle
//!   covers), which sent chasing NPCs on 100 m detours around a 12 m gap;
//! * [`PathNode::find_smooth_path`] turns the triangle corridor into the shortest path inside
//!   it (funnel / string pulling, [`super::funnel`]) instead of walking centroid to centroid,
//!   and answers a straight line right away when it crosses only linked triangles.
//!
//! C# quirks kept: the open set pops the lowest `F` (the oldest of equals), the search stops
//! after a bounded number of iterations and returns the best path so far.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

use crate::systems::skills::controllers::Vec3;

use super::funnel::{corridor_portals, straight_corridor, string_pull};
use super::graph::{NavGraph, DEFAULT_AGENT_RADIUS};
use super::manager::{douglas_peucker_reduction, AiGeoData};

/// `PathNode` (the AI's pathfinding state).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PathNode {
    /// `ZoneKey`.
    pub zone_key: u32,
    /// `CurrentTargetPos`.
    pub current_target_pos: Vec3,
    /// `StartPointPos`.
    pub start_point_pos: Vec3,
    /// `EndPointPos`.
    pub end_point_pos: Vec3,
    /// `FoundPath`.
    pub found_path: VecDeque<Vec3>,
    /// `Position`.
    pub position: Vec3,
}

/// Why `FindPath` threw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathError(pub String);

/// The result of one A* search: the chain of graph nodes from the start to the goal.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    /// The graph nodes, start first (empty when the start had no navmesh node).
    pub nodes: Vec<u32>,
    /// The node positions (the start point first).
    pub positions: Vec<Vec3>,
    /// Whether the last node is the goal node (false: the iteration limit cut the search).
    pub complete: bool,
    /// The goal node's position (the C# `EndPointPos`).
    pub goal: Vec3,
}

/// A smoothed route (see [`PathNode::find_smooth_path`]).
#[derive(Debug, Clone, PartialEq)]
pub struct SmoothPath {
    /// The way points: the start point, the corners, the goal point.
    pub points: Vec<Vec3>,
    /// The number of triangles of the corridor the route follows.
    pub corridor_len: usize,
    /// Whether the straight segment was walked without a search.
    pub direct: bool,
}

/// An open-set entry: the lowest `F` first, then the oldest.
struct Open {
    f: f32,
    seq: u32,
    node: u32,
}

impl PartialEq for Open {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Open {}
impl PartialOrd for Open {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Open {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .f
            .total_cmp(&self.f)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

/// 2D distance (node heights are unreliable on large triangles).
fn dist2d(a: Vec3, b: Vec3) -> f32 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

/// The cost of walking from `from` into `to`: through the middle of their shared edge when
/// both are triangles, else straight between the node positions.
fn step_cost(graph: &NavGraph, from: u32, to: u32) -> f32 {
    let (a, b) = (graph.nodes[from as usize].pos, graph.nodes[to as usize].pos);
    match graph.portal(from, to) {
        Some((v1, v2)) => {
            let (p, q) = (graph.vertices[v1 as usize], graph.vertices[v2 as usize]);
            let mid = Vec3::new((p.x + q.x) / 2.0, (p.y + q.y) / 2.0, 0.0);
            dist2d(a, mid) + dist2d(mid, b)
        }
        None => dist2d(a, b),
    }
}

/// One search entry: the cost so far and the node it was reached from.
struct Entry {
    g: f32,
    came_from: Option<u32>,
}

impl PathNode {
    /// `FindPath(world, start, goal)` for an NPC-sized agent ([`DEFAULT_AGENT_RADIUS`]).
    pub fn find_path(
        &mut self,
        geo: &AiGeoData,
        start: Vec3,
        goal: Vec3,
    ) -> Result<Vec<Vec3>, PathError> {
        self.find_path_with_radius(geo, start, goal, DEFAULT_AGENT_RADIUS)
    }

    /// `FindPath(world, start, goal)` for an agent of `agent_radius`: links narrower than the
    /// radius (and blocked links) are not traversed. The node positions, then the goal node's
    /// position, Douglas-Peucker reduced (2 m); empty when there is no route.
    pub fn find_path_with_radius(
        &mut self,
        geo: &AiGeoData,
        start: Vec3,
        goal: Vec3,
        agent_radius: f32,
    ) -> Result<Vec<Vec3>, PathError> {
        let Some(route) = self.find_route(geo, start, goal, agent_radius)? else {
            return Ok(Vec::new());
        };
        let mut result = route.positions;
        result.push(self.end_point_pos);
        let result = douglas_peucker_reduction(&result, 2.0);
        self.position = result[0];
        self.current_target_pos = Vec3::default();
        Ok(result)
    }

    /// The A* search itself: `None` when the open set ran dry (no route, or no navmesh node at
    /// the goal). Sets `end_point_pos` to the goal node's position.
    pub fn find_route(
        &mut self,
        geo: &AiGeoData,
        start: Vec3,
        goal: Vec3,
        agent_radius: f32,
    ) -> Result<Option<Route>, PathError> {
        let mut start = start;
        let mut goal = goal;
        // the navmesh nodes replace the start / end points
        let start_node = geo.resolve_nav_node(self.zone_key, start);
        if let Some((_, p)) = start_node {
            start = p;
        }
        let goal_node = geo.resolve_nav_node(self.zone_key, goal);
        if let Some((_, p)) = goal_node {
            goal = p;
        }
        self.end_point_pos = goal;
        // Check which navmesh file is valid at the start (loads the cell)
        if geo.bai_by_pos(start).is_err() {
            return Err(PathError(
                "Index was outside the bounds of the array.".into(),
            ));
        }
        let Some((start_id, _)) = start_node else {
            // no node to leave from
            return Ok((start == goal).then(|| Route {
                nodes: Vec::new(),
                positions: vec![start],
                complete: true,
                goal,
            }));
        };
        let Some((goal_id, _)) = goal_node else {
            return Ok(None);
        };
        // bounded like the C# (a failed search must not scan the whole world), but enough
        // for a long route through fine triangles
        let mut max_loops_left = (dist2d(start, goal) * 20.0).ceil() as i64 + 500;

        let mut entries: HashMap<u32, Entry> = HashMap::new();
        let mut closed: HashSet<u32> = HashSet::new();
        let mut open: BinaryHeap<Open> = BinaryHeap::new();
        let mut seq = 0u32;
        entries.insert(
            start_id,
            Entry {
                g: 0.0,
                came_from: None,
            },
        );
        open.push(Open {
            f: dist2d(start, goal),
            seq,
            node: start_id,
        });
        let build = |entries: &HashMap<u32, Entry>, last: u32, complete: bool| {
            let mut nodes = vec![last];
            let mut cur = last;
            while let Some(prev) = entries[&cur].came_from {
                nodes.push(prev);
                cur = prev;
            }
            nodes.reverse();
            let graph = geo.graph();
            let positions = nodes.iter().map(|n| graph.nodes[*n as usize].pos).collect();
            Route {
                nodes,
                positions,
                complete,
                goal,
            }
        };
        while let Some(Open { node: current, .. }) = open.pop() {
            if closed.contains(&current) {
                continue; // a stale entry of a node re-opened with a lower cost
            }
            max_loops_left -= 1;
            if current == goal_id {
                return Ok(Some(build(&entries, current, true)));
            }
            if max_loops_left <= 0 {
                return Ok(Some(build(&entries, current, false)));
            }
            closed.insert(current);
            let g = entries[&current].g;
            for (target, step, h) in self.neighbours(geo, current, agent_radius, goal)? {
                if closed.contains(&target) {
                    continue;
                }
                let ng = g + step;
                if entries.get(&target).is_none_or(|e| ng < e.g) {
                    entries.insert(
                        target,
                        Entry {
                            g: ng,
                            came_from: Some(current),
                        },
                    );
                    seq += 1;
                    open.push(Open {
                        f: ng + h,
                        seq,
                        node: target,
                    });
                }
            }
        }
        Ok(None)
    }

    /// `GetNeighbours(world, pathNode)` over the merged graph: the reachable nodes with their
    /// step cost and their `H`.
    fn neighbours(
        &self,
        geo: &AiGeoData,
        node: u32,
        agent_radius: f32,
        goal: Vec3,
    ) -> Result<Vec<(u32, f32, f32)>, PathError> {
        let at = geo.graph().nodes[node as usize].pos;
        // Check which navmesh file is valid at this position (loads the cell)
        match geo.bai_by_pos(at) {
            Ok(Some(_)) => {}
            Ok(None) => return Ok(Vec::new()),
            Err(_) => {
                return Err(PathError(
                    "Index was outside the bounds of the array.".into(),
                ))
            }
        }
        // the neighbouring cells' copies of a border triangle carry its cross-border links
        geo.ensure_node_cells(node);
        let candidates: Vec<(u32, Vec3, f32)> = {
            let graph = geo.graph();
            graph.nodes[node as usize]
                .links
                .iter()
                .filter(|l| l.passable(agent_radius))
                .map(|l| {
                    (
                        l.target,
                        graph.nodes[l.target as usize].pos,
                        step_cost(&graph, node, l.target),
                    )
                })
                .collect()
        };
        let mut result = Vec::new();
        for (target, target_pos, step) in candidates {
            // Checking that the point falls within the forbidden area where it is not allowed
            if geo.check_impossible_walk(target_pos) {
                continue;
            }
            result.push((target, step, dist2d(target_pos, goal)));
        }
        Ok(result)
    }

    /// The route the AI follows from `start` to `goal`: `None` when the navmesh cannot give
    /// one (no triangle at the start or goal, no route, a corridor whose edges are unknown, a
    /// path crossing a forbidden area), so the caller falls back to [`Self::find_path`].
    ///
    /// 1. A straight segment that crosses only linked, passable, non-forbidden triangles is the
    ///    route (no search).
    /// 2. Otherwise A* gives a corridor of triangles and the funnel algorithm pulls the string
    ///    tight through the shared edges, kept `agent_radius` away from the edge corners.
    ///
    /// The way points keep the start and goal points; the heights of the corners in between
    /// are the terrain's ([`AiGeoData::set_corner_heights`]).
    pub fn find_smooth_path(
        &mut self,
        geo: &AiGeoData,
        start: Vec3,
        goal: Vec3,
        agent_radius: f32,
    ) -> Result<Option<SmoothPath>, PathError> {
        // resolving loads the cells
        let (Some((start_id, _)), Some(_)) = (
            geo.resolve_nav_node(self.zone_key, start),
            geo.resolve_nav_node(self.zone_key, goal),
        ) else {
            return Ok(None);
        };
        let clear =
            |a: Vec3, b: Vec3| matches!(geo.line_passes_through_forbidden_area(a, b), Ok(false));
        let direct = {
            let graph = geo.graph();
            straight_corridor(&graph, start, goal, start_id, agent_radius)
        };
        if let Some(corridor) = direct {
            if clear(start, goal) {
                self.end_point_pos = goal;
                return Ok(Some(SmoothPath {
                    points: vec![start, goal],
                    corridor_len: corridor.len(),
                    direct: true,
                }));
            }
        }
        let Some(route) = self.find_route(geo, start, goal, agent_radius)? else {
            return Ok(None);
        };
        self.end_point_pos = goal;
        if !route.complete {
            return Ok(None);
        }
        let portals = {
            let graph = geo.graph();
            corridor_portals(&graph, &route.nodes)
        };
        let Some(portals) = portals else {
            return Ok(None);
        };
        let portals: Vec<_> = portals
            .into_iter()
            .map(|p| p.shrunk(agent_radius))
            .collect();
        let Some(mut points) = string_pull(start, goal, &portals) else {
            return Ok(None);
        };
        // the string may not cut a forbidden area
        if !points.windows(2).all(|w| clear(w[0], w[1])) {
            return Ok(None);
        }
        geo.set_corner_heights(&mut points);
        Ok(Some(SmoothPath {
            points,
            corridor_len: route.nodes.len(),
            direct: false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::super::bai::test_files::{
        areas_mission, net_mission, net_mission_full, verts_mission, TLink, TNode,
    };
    use super::super::bai::{BaiFiles, UsedAreaNames};
    use super::super::funnel::{corridor_portals, string_pull};
    use super::*;
    use crate::data::world_templates::WorldTemplate;

    /// An in-memory client file set.
    pub struct MemFiles(pub HashMap<String, Vec<u8>>);

    impl BaiFiles for MemFiles {
        fn files_in_directory(&self, dir: &str, pattern: &str) -> Vec<String> {
            let dir = dir.to_lowercase();
            let pat = pattern.to_lowercase().replace('*', "");
            let mut v: Vec<String> = self
                .0
                .keys()
                .filter(|k| k.to_lowercase().starts_with(&dir))
                .filter(|k| {
                    let name = k.rsplit('/').next().unwrap_or("").to_lowercase();
                    let (pre, post) = pat.split_once('.').unwrap_or((&pat, ""));
                    name.starts_with(pre) && name.ends_with(&format!(".{post}"))
                })
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

    /// A 2x2-cell world whose path block (0, 0) has a 3x3 grid of nodes 10 m apart, linked to
    /// their 4 neighbours, with a forbidden square around the centre node.
    fn geo(with_forbidden: bool) -> AiGeoData {
        let t = WorldTemplate::synthetic(0, "main_world", 2, 2, 100);
        let mut nodes = Vec::new();
        let mut id = 1;
        for x in 0..3 {
            for y in 0..3 {
                nodes.push((id, [10.0 + x as f32 * 10.0, 10.0 + y as f32 * 10.0, 5.0]));
                id += 1;
            }
        }
        let idx = |x: i32, y: i32| (x * 3 + y + 1) as u32;
        let mut links = Vec::new();
        for x in 0..3 {
            for y in 0..3 {
                for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                    let (nx, ny) = (x + dx, y + dy);
                    if (0..3).contains(&nx) && (0..3).contains(&ny) {
                        links.push((idx(x, y), idx(nx, ny)));
                    }
                }
            }
        }
        let mut files = HashMap::new();
        files.insert(
            "game/worlds/main_world/paths/000_000/netmission0.bai".to_string(),
            net_mission(&nodes, &links),
        );
        if with_forbidden {
            files.insert(
                "game/worlds/main_world/paths/000_000/areasmission0.bai".to_string(),
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
        AiGeoData::new(
            &t,
            true,
            Arc::new(MemFiles(files)),
            Arc::new(UsedAreaNames::default()),
            None,
        )
    }

    #[test]
    fn finds_a_path_over_the_grid() {
        let g = geo(false);
        let mut pn = PathNode::default();
        let path = pn
            .find_path(&g, Vec3::new(10.0, 10.0, 5.0), Vec3::new(30.0, 30.0, 5.0))
            .unwrap();
        assert_eq!(path.first(), Some(&Vec3::new(10.0, 10.0, 5.0)));
        assert_eq!(path.last(), Some(&Vec3::new(30.0, 30.0, 5.0)));
        assert_eq!(pn.end_point_pos, Vec3::new(30.0, 30.0, 5.0));
        // every kept point is a grid node
        for p in &path {
            assert!(
                (p.x - 10.0) % 10.0 == 0.0 && (p.y - 10.0) % 10.0 == 0.0,
                "{p:?}"
            );
        }
        assert_eq!(g.get_height(Vec3::new(11.0, 11.0, 100.0)), 5.0);
    }

    #[test]
    fn forbidden_areas_are_avoided() {
        let g = geo(true);
        assert!(g.check_impossible_walk(Vec3::new(20.0, 20.0, 5.0)));
        let mut pn = PathNode::default();
        let path = pn
            .find_path(&g, Vec3::new(20.0, 10.0, 5.0), Vec3::new(20.0, 30.0, 5.0))
            .unwrap();
        assert!(!path.contains(&Vec3::new(20.0, 20.0, 5.0)), "{path:?}");
        assert_eq!(path.last(), Some(&Vec3::new(20.0, 30.0, 5.0)));
        // the straight line through the forbidden square is not kept by ReducePath
        let reduced = g
            .reduce_path(
                &[Vec3::new(20.0, 10.0, 5.0), Vec3::new(20.0, 30.0, 5.0)],
                10,
            )
            .unwrap();
        assert_eq!(reduced.len(), 2);
    }

    /// Column x positions of a strip of 10 m squares (y 10..20) across the cell border at
    /// x = 1024; each square is split into two triangles, z = (x - 1000) / 10.
    const XS: [f32; 5] = [1004.0, 1014.0, 1024.0, 1034.0, 1044.0];

    fn z_at(x: f32) -> f32 {
        (x - 1000.0) / 10.0
    }

    /// The verts / net files of a 256 m sector at `offset` holding the given squares;
    /// `between` = the pass radius of the links between consecutive squares.
    fn strip_sector(
        offset: f32,
        squares: &[usize],
        between: impl Fn(usize) -> f32,
    ) -> (Vec<u8>, Vec<u8>) {
        let c0 = squares[0];
        let mut verts = Vec::new();
        for x in &XS[c0..=squares[squares.len() - 1] + 1] {
            for y in [10.0f32, 20.0] {
                verts.push([x - offset, y, z_at(*x)]);
            }
        }
        let vi = |c: usize, r: usize| ((c - c0) * 2 + r) as i32;
        let centroid = |t: [i32; 3]| {
            let p = t.map(|i| verts[i as usize]);
            [
                (p[0][0] + p[1][0] + p[2][0]) / 3.0,
                (p[0][1] + p[1][1] + p[2][1]) / 3.0,
                (p[0][2] + p[1][2] + p[2][2]) / 3.0,
            ]
        };
        let mut nodes = Vec::new();
        let mut links = Vec::new();
        for (k, &c) in squares.iter().enumerate() {
            let ta = [vi(c, 0), vi(c + 1, 0), vi(c + 1, 1)];
            let tb = [vi(c, 0), vi(c + 1, 1), vi(c, 1)];
            let (ia, ib) = (2 * k as i32 + 1, 2 * k as i32 + 2);
            nodes.push(TNode::tri(ia, centroid(ta), ta));
            nodes.push(TNode::tri(ib, centroid(tb), tb));
            links.push(TLink::new(ia as u32, ib as u32, 128.0));
            links.push(TLink::new(ib as u32, ia as u32, 128.0));
            if k + 1 < squares.len() {
                let next_b = ib as u32 + 2;
                links.push(TLink::new(ia as u32, next_b, between(c)));
                links.push(TLink::new(next_b, ia as u32, between(c)));
            }
        }
        (verts_mission(&verts), net_mission_full(&nodes, &links))
    }

    /// Sector 003_000 (cell 0) holds squares 0..=2, sector 004_000 (cell 1) squares 1..=3: the
    /// border squares 1 and 2 are in both, like the engine's overlapping border triangles.
    fn strip_geo(between: impl Fn(usize) -> f32 + Copy) -> AiGeoData {
        let t = WorldTemplate::synthetic(0, "main_world", 2, 2, 100);
        let mut files = HashMap::new();
        for (folder, offset, squares) in [
            ("003_000", 768.0, [0, 1, 2]),
            ("004_000", 1024.0, [1, 2, 3]),
        ] {
            let (v, n) = strip_sector(offset, &squares, between);
            let dir = format!("game/worlds/main_world/paths/{folder}");
            files.insert(format!("{dir}/vertsmission0.bai"), v);
            files.insert(format!("{dir}/netmission0.bai"), n);
        }
        AiGeoData::new(
            &t,
            true,
            Arc::new(MemFiles(files)),
            Arc::new(UsedAreaNames::default()),
            None,
        )
    }

    #[test]
    fn path_crosses_sector_and_cell_borders() {
        let g = strip_geo(|_| 1.0);
        let mut pn = PathNode::default();
        let path = pn
            .find_path(
                &g,
                Vec3::new(1012.0, 11.0, 0.0),
                Vec3::new(1036.0, 18.0, 0.0),
            )
            .unwrap();
        assert_eq!(
            g.loaded_cells(),
            2,
            "the second cell is loaded during the search"
        );
        // start / goal are the enclosing triangles' centroids
        let start = path[0];
        assert!(
            (start.x - (1004.0 + 2.0 * 1014.0) / 3.0).abs() < 1e-3,
            "{path:?}"
        );
        let goal = *path.last().unwrap();
        assert!(
            (goal.x - (2.0 * 1034.0 + 1044.0) / 3.0).abs() < 1e-3,
            "{path:?}"
        );
        assert_eq!(goal, pn.end_point_pos);
        let graph = g.graph();
        // 4 squares, 8 triangles; the border squares are merged
        assert_eq!(graph.nodes.len(), 8);
        assert_eq!(graph.vertices.len(), 10);
        assert_eq!(graph.stats.nodes_merged, 4);
        assert_eq!(graph.stats.vertices_merged, 6);
        // square 1's first triangle links to its twin and to square 2 (read in both sectors)
        let n = graph.enclosing(Vec3::new(1023.0, 11.0, 0.0)).unwrap().0;
        assert_eq!(graph.nodes[n as usize].links.len(), 2);
    }

    #[test]
    fn narrow_and_blocked_links_are_not_traversed() {
        let (a, b) = (Vec3::new(1012.0, 11.0, 0.0), Vec3::new(1036.0, 18.0, 0.0));
        // the links between squares 1 and 2 (at the cell border) are 0.2 m wide
        let g = strip_geo(|c| if c == 1 { 0.2 } else { 1.0 });
        let mut pn = PathNode::default();
        assert!(
            pn.find_path(&g, a, b).unwrap().is_empty(),
            "0.25 m NPC does not fit"
        );
        let path = pn.find_path_with_radius(&g, a, b, 0.1).unwrap();
        assert!(!path.is_empty());
        assert!((path.last().unwrap().x - 1037.333).abs() < 1e-2);
        // -1 = blocked, whatever the radius
        let g = strip_geo(|c| if c == 1 { -1.0 } else { 1.0 });
        assert!(pn.find_path_with_radius(&g, a, b, 0.0).unwrap().is_empty());
    }

    #[test]
    fn height_is_interpolated_on_the_enclosing_triangle() {
        let g = strip_geo(|_| 1.0);
        let z = g.get_height(Vec3::new(1037.5, 15.0, 100.0));
        assert!((z - 3.75).abs() < 1e-4, "{z}");
        let z = g.get_height(Vec3::new(1004.5, 19.0, 0.0));
        assert!((z - 0.45).abs() < 1e-4, "{z}");
        // outside every triangle: the C# nearest node / obstacle vertex (corner 1044, 20)
        assert_eq!(g.get_height(Vec3::new(1050.0, 50.0, 0.0)), z_at(1044.0));
    }

    /// A mesh from triangles given as world corner triples (`pos` = the node position, whose z
    /// is deliberately independent of the corners) and the pairs of triangle indices to link
    /// (both directions, pass radius `radius`), in the 256 m sector `folder` at `offset`.
    fn mesh_geo(
        folder: &str,
        offset: Vec3,
        corners: &[[f32; 3]],
        tris: &[([usize; 3], [f32; 3])],
        links: &[(usize, usize)],
        radius: f32,
    ) -> AiGeoData {
        let local = |p: [f32; 3]| [p[0] - offset.x, p[1] - offset.y, p[2]];
        let verts: Vec<[f32; 3]> = corners.iter().map(|c| local(*c)).collect();
        let nodes: Vec<TNode> = tris
            .iter()
            .enumerate()
            .map(|(i, (t, pos))| TNode::tri(i as i32 + 1, local(*pos), t.map(|v| v as i32)))
            .collect();
        let mut tl = Vec::new();
        for &(a, b) in links {
            tl.push(TLink::new(a as u32 + 1, b as u32 + 1, radius));
            tl.push(TLink::new(b as u32 + 1, a as u32 + 1, radius));
        }
        let dir = format!("game/worlds/main_world/paths/{folder}");
        let mut files = HashMap::new();
        files.insert(format!("{dir}/vertsmission0.bai"), verts_mission(&verts));
        files.insert(
            format!("{dir}/netmission0.bai"),
            net_mission_full(&nodes, &tl),
        );
        AiGeoData::new(
            &WorldTemplate::synthetic(0, "main_world", 16, 16, 100),
            true,
            Arc::new(MemFiles(files)),
            Arc::new(UsedAreaNames::default()),
            None,
        )
    }

    /// 10 m squares at the given grid cells, each split in two triangles, neighbours linked.
    fn grid_geo(squares: &[(i32, i32)]) -> AiGeoData {
        let mut corners: Vec<[f32; 3]> = Vec::new();
        let mut idx = |x: f32, y: f32| {
            corners
                .iter()
                .position(|c| c[0] == x && c[1] == y)
                .unwrap_or_else(|| {
                    corners.push([x, y, 0.0]);
                    corners.len() - 1
                })
        };
        let mut tris: Vec<([usize; 3], [f32; 3])> = Vec::new();
        let mut links = Vec::new();
        for &(cx, cy) in squares {
            let (x0, y0) = (10.0 * cx as f32, 10.0 * cy as f32);
            let (a, b, c, d) = (
                idx(x0, y0),
                idx(x0 + 10.0, y0),
                idx(x0 + 10.0, y0 + 10.0),
                idx(x0, y0 + 10.0),
            );
            let t0 = tris.len();
            tris.push(([a, b, c], [x0 + 20.0 / 3.0, y0 + 10.0 / 3.0, 0.0]));
            tris.push(([a, c, d], [x0 + 10.0 / 3.0, y0 + 20.0 / 3.0, 0.0]));
            links.push((t0, t0 + 1));
        }
        for i in 0..tris.len() {
            for j in i + 1..tris.len() {
                let shared = tris[i].0.iter().filter(|v| tris[j].0.contains(v)).count();
                if shared == 2 && !links.contains(&(i, j)) {
                    links.push((i, j));
                }
            }
        }
        mesh_geo("000_000", Vec3::default(), &corners, &tris, &links, 1.0)
    }

    #[test]
    fn open_ground_is_a_direct_line() {
        let g = grid_geo(&[(0, 0), (1, 0), (2, 0)]);
        let mut pn = PathNode::default();
        let (a, b) = (Vec3::new(2.0, 3.0, 0.0), Vec3::new(28.0, 6.0, 0.0));
        let s = pn.find_smooth_path(&g, a, b, 0.25).unwrap().unwrap();
        assert!(s.direct, "{s:?}");
        assert_eq!(s.points, vec![a, b]);
        // a single triangle
        let s = pn
            .find_smooth_path(&g, Vec3::new(5.0, 1.0, 0.0), Vec3::new(9.0, 2.0, 0.0), 0.25)
            .unwrap()
            .unwrap();
        assert_eq!((s.points.len(), s.corridor_len), (2, 1));
    }

    #[test]
    fn l_shaped_corridor_bends_around_the_inner_corner() {
        // squares (0,0) (1,0) (1,1): the segment from the top left of the first to the top
        // right of the last leaves the mesh above the first square
        let g = grid_geo(&[(0, 0), (1, 0), (1, 1)]);
        let mut pn = PathNode::default();
        let (a, b) = (Vec3::new(1.0, 9.0, 0.0), Vec3::new(19.0, 19.0, 0.0));
        let s = pn.find_smooth_path(&g, a, b, 0.25).unwrap().unwrap();
        assert!(!s.direct);
        // the string bends at the inner corner (10, 10); the two shrunk portals meeting there
        // may put it in two points a radius apart
        let (first, last) = (s.points[0], *s.points.last().unwrap());
        assert_eq!((first, last), (a, b));
        let corners = &s.points[1..s.points.len() - 1];
        assert!((1..=2).contains(&corners.len()), "{:?}", s.points);
        for c in corners {
            assert!(
                (c.x - 10.0).abs() < 0.3 && (c.y - 10.0).abs() < 0.3,
                "{c:?}"
            );
        }
        // the plain A* route walks the triangle centroids
        let raw = pn.find_path(&g, a, b).unwrap();
        assert!(raw.len() >= 3);
    }

    /// The Case A geometry of the harpy report (Solzreed Peninsula, main_world): two huge
    /// triangles `S` (7349) and `G` (7263) fanned around the apex `B`, linked through the
    /// slivers 7195 / 7265 and the centroid-z-11 triangle 7266 (the near corridor), and
    /// through an eastern chain of small triangles whose node z is near the goal's. The old
    /// greedy search on 3D node distances took the eastern chain (a 100 m detour for a 12.5 m
    /// gap). Positions are the real file values; `links` indexes the triangles below.
    fn fan_geo(links: &[(usize, usize)]) -> AiGeoData {
        let corners = [
            [14969.534, 14052.867, 17.5],  // 0 A
            [15064.908, 14072.474, 155.3], // 1 B (the apex)
            [14919.844, 14063.59, 7.8],    // 2 C
            [14918.608, 14063.784, 7.6],   // 3 D
            [14848.0, 14080.0, 116.1],     // 4 E
            [15066.334, 14072.828, 155.7], // 5 B2
            [15104.0, 14080.0, 167.0],     // 6 F
            [15030.8, 14042.0, 155.5],     // 7 G1
            [15086.877, 14069.357, 163.1], // 8 H1
        ];
        let tris = [
            ([0, 1, 2], [14992.358, 14064.556, 141.0]), // 0 S
            ([1, 3, 2], [14991.755, 14068.062, 140.2]), // 1 sliver
            ([4, 3, 1], [14956.166, 14072.79, 11.5]),   // 2 M (node z far below)
            ([4, 1, 5], [14957.178, 14076.313, 10.9]),  // 3 sliver
            ([6, 4, 5], [14976.14, 14078.204, 134.7]),  // 4 G
            ([0, 1, 7], [15021.0, 14055.0, 150.0]),     // 5 east chain
            ([1, 7, 8], [15060.0, 14061.0, 157.0]),     // 6
            ([1, 8, 5], [15072.7, 14071.5, 159.0]),     // 7
            ([5, 8, 6], [15085.5, 14074.6, 161.8]),     // 8
        ];
        mesh_geo(
            "058_054",
            Vec3::new(14848.0, 13824.0, 0.0),
            &corners,
            &tris,
            links,
            50.0,
        )
    }

    const NEAR: [(usize, usize); 4] = [(0, 1), (1, 2), (2, 3), (3, 4)];
    const EAST: [(usize, usize); 5] = [(0, 5), (5, 6), (6, 7), (7, 8), (8, 4)];
    const FAN_START: Vec3 = Vec3::new(14987.004, 14063.533, 145.42401);
    const FAN_GOAL: Vec3 = Vec3::new(14984.287, 14075.668, 137.0428);

    fn path_len(p: &[Vec3]) -> f32 {
        p.windows(2).map(|w| dist2d(w[0], w[1])).sum()
    }

    #[test]
    fn near_low_z_corridor_beats_the_far_high_z_chain() {
        let links: Vec<_> = NEAR.iter().chain(&EAST).copied().collect();
        let g = fan_geo(&links);
        let mut pn = PathNode::default();
        let route = pn
            .find_route(&g, FAN_START, FAN_GOAL, 0.25)
            .unwrap()
            .unwrap();
        assert!(route.complete);
        // S, sliver, M, sliver, G (graph nodes in file order)
        assert_eq!(route.nodes, vec![0, 1, 2, 3, 4], "{:?}", route.positions);
        // the funnel through that corridor is the straight line
        let (portals, pulled) = {
            let graph = g.graph();
            let p = corridor_portals(&graph, &route.nodes).unwrap();
            (p.len(), string_pull(FAN_START, FAN_GOAL, &p).unwrap())
        };
        assert_eq!(portals, 4);
        assert_eq!(pulled, vec![FAN_START, FAN_GOAL]);
        // the walk along the straight segment finds the same corridor without a search
        let s = pn
            .find_smooth_path(&g, FAN_START, FAN_GOAL, 0.25)
            .unwrap()
            .unwrap();
        assert!(s.direct);
        assert_eq!(s.points, vec![FAN_START, FAN_GOAL]);
        assert!(path_len(&s.points) < 13.0);
        // the legacy node-position route is bounded by the short corridor as well
        let raw = pn.find_path(&g, FAN_START, FAN_GOAL).unwrap();
        assert!(path_len(&raw) < 120.0, "{raw:?}");
    }

    #[test]
    fn only_the_eastern_chain_linked_is_followed_without_a_direct_line() {
        let g = fan_geo(&EAST);
        let mut pn = PathNode::default();
        let s = pn
            .find_smooth_path(&g, FAN_START, FAN_GOAL, 0.25)
            .unwrap()
            .unwrap();
        assert!(!s.direct);
        assert_eq!(s.corridor_len, 6);
        assert!(s.points.len() >= 3, "{:?}", s.points);
        assert_eq!(s.points[0], FAN_START);
        assert_eq!(*s.points.last().unwrap(), FAN_GOAL);
    }

    #[test]
    fn search_uses_accumulated_2d_costs_not_node_heights() {
        // the same squares at wildly different heights: the shortest route is the straight
        // strip, whatever the start and goal heights say
        let g = grid_geo(&[
            (0, 0),
            (1, 0),
            (2, 0),
            (3, 0),
            (0, 1),
            (1, 1),
            (2, 1),
            (3, 1),
        ]);
        let mut pn = PathNode::default();
        let r = pn
            .find_route(
                &g,
                Vec3::new(1.0, 1.0, 500.0),
                Vec3::new(39.0, 1.0, -500.0),
                0.25,
            )
            .unwrap()
            .unwrap();
        assert!(r.complete);
        // along the lower row: 2 triangles per square, 4 squares
        assert_eq!(r.nodes.len(), 7, "{:?}", r.positions);
        assert!(r.positions.iter().all(|p| p.y < 10.0), "{:?}", r.positions);
    }
}
