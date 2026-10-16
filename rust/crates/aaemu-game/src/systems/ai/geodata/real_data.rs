//! Gated navmesh tests on real `.bai` data:
//!
//! * `AAEMU_BAI_SAMPLES=D:\aa-re\zone\agent_geom\data` — the extracted sample sectors
//!   (`bai\{036_056,038_058,039_059}` from the 10.x pak, `bai12\038_058` from the 1.2 pak);
//! * `AAEMU_GAME_PAK=<1.2 game_pak>` — whole main_world cells through `AiGeoData`.
//!
//! `cargo test -p aaemu-game real_nav -- --ignored --nocapture`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::data::client_files::ClientFileManager;
use crate::data::world_templates::{WorldTemplate, WorldTemplates};
use crate::systems::skills::controllers::Vec3;

use super::astar::PathNode;
use super::bai::{AreasMission, BaiFiles, NetMission, UsedAreaNames, VertexMission};
use super::graph::DEFAULT_AGENT_RADIUS;
use super::manager::AiGeoData;
use super::ClientBaiFiles;

fn samples() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var("AAEMU_BAI_SAMPLES").ok()?);
    p.is_dir().then_some(p)
}

/// The sample folders as `game/worlds/main_world/paths/<folder>/<file>`.
struct SampleFiles(HashMap<String, Vec<u8>>);

impl SampleFiles {
    fn load(dir: &std::path::Path) -> Self {
        let mut m = HashMap::new();
        for sector in std::fs::read_dir(dir).unwrap().filter_map(Result::ok) {
            let folder = sector.file_name().to_string_lossy().to_string();
            for f in std::fs::read_dir(sector.path())
                .unwrap()
                .filter_map(Result::ok)
            {
                let name = f.file_name().to_string_lossy().to_lowercase();
                m.insert(
                    format!("game/worlds/main_world/paths/{folder}/{name}"),
                    std::fs::read(f.path()).unwrap(),
                );
            }
        }
        Self(m)
    }
}

impl BaiFiles for SampleFiles {
    fn files_in_directory(&self, dir: &str, pattern: &str) -> Vec<String> {
        let pre = pattern.split('*').next().unwrap_or("");
        let mut v: Vec<String> = self
            .0
            .keys()
            .filter(|k| k.starts_with(dir))
            .filter(|k| k.rsplit('/').next().unwrap_or("").starts_with(pre))
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

/// The documented counts (`sub_navmesh.md` §2): (dir, nodes, links, verts, areas version,
/// forbidden areas, nav modifiers).
/// (dir, nodes, links, verts, areas version, (forbidden areas, nav modifiers)).
type Expected = (
    &'static str,
    usize,
    usize,
    usize,
    i32,
    Option<(usize, usize)>,
);

const EXPECTED: [Expected; 4] = [
    ("bai/038_058", 3661, 10960, 1840, 22, Some((124, 2))),
    ("bai/039_059", 2030, 6126, 1013, 22, Some((286, 9))),
    ("bai/036_056", 12, 28, 11, 22, None),
    ("bai12/038_058", 3653, 10936, 1836, 21, None),
];

#[test]
#[ignore = "needs AAEMU_BAI_SAMPLES"]
fn real_nav_samples_parse_like_the_engine() {
    let Some(root) = samples() else { return };
    for (dir, nodes, links, verts, areas_version, areas) in EXPECTED {
        let d = root.join(dir);
        if !d.is_dir() {
            continue;
        }
        let (_, px, py) = super::bai::zone_and_offsets_by_name(&dir[dir.len() - 7..]);
        let off = Vec3::new(px as f32 * 256.0, py as f32 * 256.0, 0.0);
        let read = |n: &str| std::fs::read(d.join(n)).unwrap();
        let net = NetMission::read(&read("netmission0.bai"), 0, off).unwrap();
        assert_eq!((net.nodes.len(), net.links.len()), (nodes, links), "{dir}");
        // +42: 10.x mostly 9 (also 8 / 10), 1.2 mostly 1 (also 0 / 2): bit 0, not `== 1`
        let set = net
            .links
            .iter()
            .filter(|l| l.is_pure_triangular_link)
            .count();
        println!("{dir}: +42 bit 0 set on {set} of {} links", net.links.len());
        assert!(set * 100 >= net.links.len() * 95, "{dir}");
        let v = VertexMission::read(&read("vertsmission0.bai"), 0, off).unwrap();
        assert_eq!(v.obstacles.len(), verts, "{dir}");
        // every triangle corner index is inside the verts file
        for n in net.nodes.iter().filter(|n| n.nav_type() == 2) {
            assert!(
                n.obstacle.iter().all(|&i| i >= 0 && (i as usize) < verts),
                "{dir}: {n:?}"
            );
        }
        let data = read("areasmission0.bai");
        assert_eq!(
            i32::from_le_bytes(data[..4].try_into().unwrap()),
            areas_version
        );
        let a = AreasMission::read(&data, 0, off, &UsedAreaNames::default())
            .unwrap_or_else(|e| panic!("{dir}: {e:?}"));
        if let Some((forbidden, mods)) = areas {
            assert_eq!(
                (a.forbidden_areas.len(), a.navigation_modifiers.len()),
                (forbidden, mods),
                "{dir}"
            );
        }
        for m in &a.navigation_modifiers {
            println!(
                "{dir} navmod {}: type {} wpc {} altered {} h {} auto {} z {}..{} bld {} light {}",
                m.name,
                m.mission_type,
                m.waypoint_connections,
                m.altered,
                m.height,
                m.node_auto_connect_distance,
                m.min_z,
                m.max_z,
                m.building_id,
                m.ai_light_level
            );
            assert!(m.min_z <= m.max_z, "{dir}: {m:?}");
        }
        assert_eq!(
            (net.trailing_bytes, v.trailing_bytes, a.trailing_bytes),
            (0, 0, 0),
            "{dir}"
        );
        if let Ok(hide) = std::fs::read(d.join("hidemission0.bai")) {
            // a v54 graph with the v55 layout (not loaded by `load_from_folder`)
            let h = NetMission::read(&hide, 0, off).unwrap();
            assert_eq!((h.version, h.trailing_bytes), (54, 0), "{dir}");
        }
        println!("{dir}: ok");
    }
}

/// The 10.x samples (sectors 038_058 and 039_059 share a corner) merged through `AiGeoData`:
/// the navmesh height at every triangle centroid is the centroid's z, and A* routes across the
/// sector with the default NPC radius.
#[test]
#[ignore = "needs AAEMU_BAI_SAMPLES"]
fn real_nav_samples_graph_height_and_route() {
    let Some(root) = samples() else { return };
    for set in ["bai", "bai12"] {
        let dir = root.join(set);
        if !dir.is_dir() {
            continue;
        }
        let t = WorldTemplate::synthetic(0, "main_world", 12, 16, 100);
        let g = AiGeoData::new(
            &t,
            true,
            Arc::new(SampleFiles::load(&dir)),
            Arc::new(UsedAreaNames::default()),
            None,
        );
        g.cell_bai(9, 14).expect("cell");
        let (centroids, stats) = {
            let graph = g.graph();
            let c: Vec<Vec3> = graph
                .nodes
                .iter()
                .filter(|n| n.triangle.is_some())
                .map(|n| n.pos)
                .collect();
            (c, graph.stats)
        };
        println!("{set}: {stats:?}");
        assert!(!centroids.is_empty());
        // the node positions lie inside their triangles but ~0.2 m above the plane (median;
        // large triangles over uneven ground are off by up to tens of metres)
        let mut errs: Vec<f32> = centroids
            .iter()
            .map(|c| (g.get_height(Vec3::new(c.x, c.y, c.z + 50.0)) - c.z).abs())
            .collect();
        errs.sort_by(f32::total_cmp);
        let (median, p90) = (errs[errs.len() / 2], errs[errs.len() * 9 / 10]);
        println!(
            "{set}: {} triangle nodes, node z vs plane height: median {median}, p90 {p90}, max {}",
            errs.len(),
            errs[errs.len() - 1]
        );
        assert!(median < 0.3 && p90 < 2.0, "{set}");
        // routes inside 038_058 (world 9728..9984, 14848..15104)
        let pairs = [
            ((9900.0, 14950.0), (9970.0, 15090.0)),
            ((9800.0, 14950.0), (9950.0, 15050.0)),
            ((9900.0, 14950.0), (9750.0, 15050.0)),
            ((9740.0, 14860.0), (9970.0, 15090.0)),
        ];
        let mut routed = 0;
        for ((ax, ay), (bx, by)) in pairs {
            let mut pn = PathNode::default();
            let path = pn
                .find_path_with_radius(
                    &g,
                    Vec3::new(ax, ay, 0.0),
                    Vec3::new(bx, by, 0.0),
                    DEFAULT_AGENT_RADIUS,
                )
                .unwrap();
            println!("{set}: ({ax},{ay}) -> ({bx},{by}): {} points", path.len());
            if path.len() >= 2 {
                routed += 1;
                assert_eq!(path.last().copied(), Some(pn.end_point_pos));
            }
        }
        assert!(routed >= 2, "{set}");
    }
}

/// A whole main_world cell of the 1.2 pak: the 16 sectors merge (border triangles are
/// de-duplicated) and a route crosses from one sector into the next.
#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_nav_pak_cell_merges_sectors() {
    let Ok(pak) = std::env::var("AAEMU_GAME_PAK") else {
        return;
    };
    let app = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("app");
    let files = Arc::new(ClientFileManager::from_sources(&[pak], &app));
    let worlds = WorldTemplates::load(&files, &app).expect("world templates");
    let main = worlds
        .get_world_template_by_name("main_world")
        .expect("main_world");
    let bai: Arc<dyn BaiFiles + Send + Sync> = Arc::new(ClientBaiFiles(files.clone()));
    let g = AiGeoData::new(main, true, bai, Arc::new(UsedAreaNames::default()), None);
    // cell (9, 14): sectors 36..39 x 56..59 (w_gweonid_forest)
    g.cell_bai(9, 14).expect("cell");
    let stats = g.graph().stats;
    println!("cell (9,14): {stats:?}");
    assert_eq!(stats.folders, 16);
    assert!(stats.nodes_merged > 0, "border triangles are shared");
    assert!(stats.vertices_merged > 0);
    // from sector 038_058 into 039_058 (x border 9984)
    let (a, b) = (
        Vec3::new(9900.0, 14950.0, 0.0),
        Vec3::new(10060.0, 14950.0, 0.0),
    );
    let mut pn = PathNode::default();
    let path = pn.find_path(&g, a, b).unwrap();
    println!("route {} points: {path:?}", path.len());
    assert!(path.len() >= 2);
    assert!(path.iter().any(|p| p.x < 9984.0) && path.iter().any(|p| p.x > 9984.0));
    let z = g.get_height(Vec3::new(9984.0, 14950.0, 500.0));
    println!("height at the sector border: {z}");
    assert!(z > 100.0 && z < 400.0);
}

/// The 1.2 pak's older path sectors (net v54 + verts v1, areas below v21) use the same layouts
/// as net v55 / verts v2 / areas v21: every file parses with 0 trailing bytes.
#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_nav_pak_12_old_sector_versions_parse() {
    let Ok(pak) = std::env::var("AAEMU_GAME_PAK") else {
        return;
    };
    let app = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("app");
    let files = ClientFileManager::from_sources(&[pak], &app);
    let names: Vec<String> = files.sources()[0]
        .game_pak()
        .expect("pak")
        .file_names()
        .filter(|n| n.to_lowercase().contains("/paths/") && n.to_lowercase().ends_with(".bai"))
        .map(str::to_string)
        .collect();
    let used = UsedAreaNames::default();
    // (kind, version) -> (files, files with trailing bytes, failures)
    let mut by: std::collections::BTreeMap<(&str, i32), (usize, usize, usize)> =
        std::collections::BTreeMap::new();
    let mut v54_sectors = std::collections::BTreeSet::new();
    let mut v1_sectors = std::collections::BTreeSet::new();
    for n in &names {
        let lower = n.to_lowercase();
        let file = lower.rsplit('/').next().unwrap_or("");
        let folder = super::bai::parent_folder_name(n);
        let data = files.read_file(n).expect("read");
        if data.len() < 4 {
            continue;
        }
        let version = i32::from_le_bytes(data[..4].try_into().unwrap());
        let (kind, res): (&str, Result<usize, String>) = if file.starts_with("netmission") {
            (
                "net",
                NetMission::read(&data, 0, Vec3::default())
                    .map(|m| m.trailing_bytes)
                    .map_err(|e| e.0),
            )
        } else if file.starts_with("vertsmission") {
            (
                "verts",
                VertexMission::read(&data, 0, Vec3::default())
                    .map(|m| m.trailing_bytes)
                    .map_err(|e| e.0),
            )
        } else if file.starts_with("areasmission") && data.len() > 20 {
            // parse the old versions with the v21 layout
            let mut d = data.clone();
            if version < 21 {
                d[..4].copy_from_slice(&21i32.to_le_bytes());
            }
            (
                "areas",
                AreasMission::read(&d, 0, Vec3::default(), &used)
                    .map(|m| m.trailing_bytes)
                    .map_err(|e| e.0),
            )
        } else {
            continue;
        };
        if kind == "net" && version == 54 {
            v54_sectors.insert(folder.clone());
        }
        if kind == "verts" && version == 1 {
            v1_sectors.insert(folder);
        }
        let e = by.entry((kind, version)).or_default();
        e.0 += 1;
        match res {
            Ok(0) => {}
            Ok(_) => e.1 += 1,
            Err(_) => e.2 += 1,
        }
    }
    println!("(kind, version) -> (files, with trailing bytes, failures): {by:?}");
    println!(
        "{} net v54 sectors, {} verts v1 sectors, {} in both",
        v54_sectors.len(),
        v1_sectors.len(),
        v54_sectors.intersection(&v1_sectors).count()
    );
    for ((kind, version), (_, trailing, failed)) in &by {
        if matches!((*kind, *version), ("net", 54) | ("verts", 1)) {
            assert_eq!((*trailing, *failed), (0, 0), "{kind} v{version}");
        }
    }
    assert!(by.get(&("net", 54)).is_some_and(|e| e.0 > 0));
}

fn pak_geo() -> Option<AiGeoData> {
    let pak = std::env::var("AAEMU_GAME_PAK").ok()?;
    let app = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("app");
    let files = Arc::new(ClientFileManager::from_sources(&[pak], &app));
    let worlds = WorldTemplates::load(&files, &app).expect("world templates");
    let main = worlds
        .get_world_template_by_name("main_world")
        .expect("main_world");
    let bai: Arc<dyn BaiFiles + Send + Sync> = Arc::new(ClientBaiFiles(files.clone()));
    Some(AiGeoData::new(
        main,
        true,
        bai,
        Arc::new(UsedAreaNames::default()),
        None,
    ))
}

fn len2d(p: &[Vec3]) -> f32 {
    p.windows(2)
        .map(|w| ((w[1].x - w[0].x).powi(2) + (w[1].y - w[0].y).powi(2)).sqrt())
        .sum()
}

/// What `Npc.FindPath` sends: the smooth route, else the C# node route reduced by `ReducePath`.
fn npc_route(g: &AiGeoData, a: Vec3, b: Vec3) -> Vec<Vec3> {
    let mut pn = PathNode::default();
    if let Some(s) = pn
        .find_smooth_path(g, a, b, DEFAULT_AGENT_RADIUS)
        .expect("smooth path")
    {
        return s.points;
    }
    legacy_route(g, a, b)
}

/// The C# pipeline: A* node positions, the end points replaced by the actors, `ReducePath`.
fn legacy_route(g: &AiGeoData, a: Vec3, b: Vec3) -> Vec<Vec3> {
    let mut pn = PathNode::default();
    let mut res = pn.find_path(g, a, b).expect("find path");
    if let Some(f) = res.first_mut() {
        *f = a;
    }
    if res.len() > 1 {
        *res.last_mut().unwrap() = b;
    }
    if res.last() != Some(&b) {
        res.push(b);
    }
    g.reduce_path(&res, 10).expect("reduce").into()
}

/// The harpy report (Solzreed Peninsula): a 12.5 m gap used to be routed 100 m east and back
/// because the greedy search took node heights at face value.
#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_nav_pak_chase_routes_do_not_detour() {
    let Some(g) = pak_geo() else { return };
    // (start, goal): case A (4/10 steps), case B (2/9), case C (2/2) of the report
    let cases = [
        (
            Vec3::new(14987.004, 14063.533, 145.42401),
            Vec3::new(14984.287, 14075.668, 137.0428),
        ),
        (
            Vec3::new(15025.753, 14067.261, 150.975),
            Vec3::new(14987.668, 14074.25, 138.06),
        ),
        (
            Vec3::new(14977.554, 14078.733, 141.42),
            Vec3::new(14980.535, 14085.406, 134.955),
        ),
    ];
    for (i, (a, b)) in cases.into_iter().enumerate() {
        let straight = len2d(&[a, b]);
        let route = npc_route(&g, a, b);
        let total = len2d(&route);
        println!("case {i}: straight {straight:.1} m, route {total:.1} m: {route:?}");
        assert_eq!(route.first(), Some(&a));
        assert_eq!(route.last(), Some(&b));
        assert!(total < straight * 1.3 + 0.01, "case {i}: {route:?}");
        if i == 0 {
            assert!(route.len() <= 3 && total < 20.0, "case A: {route:?}");
        }
        // the C# node route is bounded too (the search itself is a real A*)
        let old = legacy_route(&g, a, b);
        println!("case {i}: node route {:.1} m", len2d(&old));
        assert!(len2d(&old) < 4.0 * straight + 30.0, "case {i}: {old:?}");
    }
}

/// Random chases of 3 to 40 m around the harpy hill: route length against the straight line.
#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_nav_pak_random_chase_routes_stay_short() {
    let Some(g) = pak_geo() else { return };
    let mut seed = 0x2545_f491_u32;
    let mut rnd = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / (1u32 << 24) as f32
    };
    let (mut worst_new, mut worst_old) = (0f32, 0f32);
    let (mut sum_new, mut sum_old, mut n) = (0f32, 0f32, 0);
    for _ in 0..20 {
        let (x, y) = (
            14987.0 + (rnd() - 0.5) * 120.0,
            14063.0 + (rnd() - 0.5) * 120.0,
        );
        let (ang, dist) = (rnd() * std::f32::consts::TAU, 3.0 + rnd() * 37.0);
        let (gx, gy) = (x + ang.cos() * dist, y + ang.sin() * dist);
        let a = Vec3::new(x, y, g.get_height(Vec3::new(x, y, 0.0)));
        let b = Vec3::new(gx, gy, g.get_height(Vec3::new(gx, gy, 0.0)));
        let straight = len2d(&[a, b]);
        let (new, old) = (npc_route(&g, a, b), legacy_route(&g, a, b));
        let (rn, ro) = (len2d(&new) / straight, len2d(&old) / straight);
        println!(
            "({x:.1},{y:.1}) -> ({gx:.1},{gy:.1}) straight {straight:.1}: route {:.1} m (x{rn:.2}, {} pts), node route {:.1} m (x{ro:.2}, {} pts)",
            len2d(&new),
            new.len(),
            len2d(&old),
            old.len()
        );
        worst_new = worst_new.max(rn);
        worst_old = worst_old.max(ro);
        sum_new += rn;
        sum_old += ro;
        n += 1;
    }
    println!(
        "{n} pairs: route max x{worst_new:.2} mean x{:.2}; node route max x{worst_old:.2} mean x{:.2}",
        sum_new / n as f32,
        sum_old / n as f32
    );
    assert!(worst_new <= 1.5, "worst route x{worst_new}");
}

/// Wall time of the lazy navmesh cell loads the AI tick runs on first contact with a cell:
/// the three `files_in_directory` scans per path folder and the whole `cell_bai` load.
/// Run with `--release` for production-like numbers:
/// `AAEMU_GAME_PAK=D:\AA1.2\game_pak cargo test -p aaemu-game --release real_nav_pak_cell_load_timing -- --ignored --nocapture`
#[test]
#[ignore = "needs AAEMU_GAME_PAK"]
fn real_nav_pak_cell_load_timing() {
    let Ok(pak) = std::env::var("AAEMU_GAME_PAK") else {
        return;
    };
    let app = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("app");
    let files = Arc::new(ClientFileManager::from_sources(&[pak], &app));
    let worlds = WorldTemplates::load(&files, &app).expect("world templates");
    let main = worlds
        .get_world_template_by_name("main_world")
        .expect("main_world");
    let t = std::time::Instant::now();
    for _ in 0..10 {
        let n = files
            .files_in_directory(
                "game/worlds/main_world/paths/036_056",
                "netmission*.bai",
                true,
            )
            .len();
        assert!(n > 0);
    }
    println!(
        "files_in_directory (one folder, one pattern): {:?} per call",
        t.elapsed() / 10
    );
    let bai: Arc<dyn BaiFiles + Send + Sync> = Arc::new(ClientBaiFiles(files.clone()));
    let g = AiGeoData::new(main, true, bai, Arc::new(UsedAreaNames::default()), None);
    for cell in [(9, 14), (10, 14), (9, 15), (12, 13), (20, 20)] {
        let t = std::time::Instant::now();
        g.cell_bai(cell.0, cell.1).expect("cell");
        println!("cell_bai{cell:?}: {:?}", t.elapsed());
    }
    println!("graph: {:?}", g.graph().stats);
}
