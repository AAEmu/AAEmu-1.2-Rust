//! Stacked NPCs (Rust only, server owner's request 2026-11-08): the spawn data places every NPC
//! of a spawner, and several spawner entries, on identical coordinates, so NPCs sit on top of
//! each other and never separate (idle NPCs hold their home position).
//!
//! * [`scatter_position`]: used by `NpcSpawner` when an NPC would spawn on top of another live
//!   NPC; picks a ground-snapped spot on a ring around it (`World.NpcSpawnScatter*`).
//! * [`find_stacks`] / [`scan_once`] / [`start`]: the periodic check that logs a stack that
//!   persists (`World.StackedNpc*`), one warning per stack.
//! * [`separate_npc`]: moves one NPC off its stack (used by the `stackednpcs fix` GM command).
//!
//! Locking: every NPC is read with one short `data()` lock; no lock is held across world calls.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use crate::config::WorldConfig;
use crate::data::world_templates::REGION_SIZE;
use crate::server::GameServer;
use crate::systems::npcs::npc::spawner::{random_float, spawn_ground_z};
use crate::systems::npcs::spawn_manager;
use crate::world::{NpcRef, WorldInstance, WorldObject};

/// Candidate angles tried per scatter.
const SCATTER_TRIES: usize = 16;
/// The golden angle (rad): successive candidates spread evenly around the ring.
const GOLDEN_ANGLE: f32 = 2.399_963;
/// Largest height change (m) a scattered spot may have against the original spawn height.
pub const SCATTER_MAX_STEP: f32 = 1.5;
/// `StackedNpcMinSeconds` is honoured per stack; a logged stack with an unchanged member set is
/// logged again after this long.
const RELOG_AFTER: Duration = Duration::from_secs(3600);

/// Defaults used by `stackednpcs fix` when scattering is disabled in the config.
pub const DEFAULT_SCATTER_MIN: f32 = 1.5;
pub const DEFAULT_SCATTER_MAX: f32 = 3.0;

// ---------------------------------------------------------------------------------------------
// scatter
// ---------------------------------------------------------------------------------------------

/// The scatter ring and trigger distance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScatterParams {
    /// An NPC within this distance (3D) of the spawn spot makes it a stack.
    pub trigger: f32,
    /// Inner / outer radius of the ring.
    pub min: f32,
    pub max: f32,
}

impl ScatterParams {
    /// The spawn-time parameters; `None` when `World.NpcSpawnScatterMax` is 0 (scattering off).
    pub fn from_config(cfg: &WorldConfig) -> Option<Self> {
        (cfg.npc_spawn_scatter_max > 0.0).then(|| Self {
            trigger: cfg.npc_spawn_scatter_trigger.max(0.0),
            min: cfg.npc_spawn_scatter_min.max(0.0),
            max: cfg.npc_spawn_scatter_max,
        })
    }

    /// The parameters of `stackednpcs fix`: the configured ring (the defaults when scattering is
    /// disabled) and a trigger of at least the stack `radius`.
    pub fn for_fix(cfg: &WorldConfig, radius: f32) -> Self {
        let configured = Self::from_config(cfg).unwrap_or(Self {
            trigger: cfg.npc_spawn_scatter_trigger.max(0.0),
            min: DEFAULT_SCATTER_MIN,
            max: DEFAULT_SCATTER_MAX,
        });
        Self {
            trigger: configured.trigger.max(radius),
            ..configured
        }
    }
}

/// Positions of the live, unparented NPCs of `world` within `reach` (2D) of `(x, y)`, except
/// `exclude`. Reads the sectors that overlap the square around the point.
fn nearby_npc_positions(
    world: &WorldInstance,
    exclude: u32,
    x: f32,
    y: f32,
    reach: f32,
) -> Vec<(f32, f32, f32)> {
    let size = REGION_SIZE as f32;
    let (x0, x1) = (
        ((x - reach) / size).floor() as i32,
        ((x + reach) / size).floor() as i32,
    );
    let (y0, y1) = (
        ((y - reach) / size).floor() as i32,
        ((y + reach) / size).floor() as i32,
    );
    let mut out = Vec::new();
    for rx in x0..=x1 {
        for ry in y0..=y1 {
            if !world.template.valid_region(rx, ry) {
                continue;
            }
            for o in world.objects_in_region((rx, ry)) {
                let Some(n) = o.as_npc() else { continue };
                if n.obj_id == exclude {
                    continue;
                }
                let d = n.data();
                let t = d.unit.transform;
                if d.unit.is_dead() || t.has_parent() {
                    continue;
                }
                out.push((t.x, t.y, t.z));
            }
        }
    }
    out
}

fn dist3(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    let (dx, dy, dz) = (a.0 - b.0, a.1 - b.1, a.2 - b.2);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// The height of a scatter candidate: the spawn ground snap, accepted within
/// [`SCATTER_MAX_STEP`] of `z`. A candidate whose ground cannot be resolved keeps `z` only when
/// the original spot could not be resolved either (authored height kept, e.g. inside a hall);
/// otherwise it is rejected.
fn candidate_z(
    server: &GameServer,
    world: &WorldInstance,
    x: f32,
    y: f32,
    z: f32,
    original_resolved: bool,
) -> Option<f32> {
    match spawn_ground_z(server, world, x, y, z) {
        Some(floor) => ((floor - z).abs() <= SCATTER_MAX_STEP).then_some(floor),
        None => (!original_resolved).then_some(z),
    }
}

/// Where an NPC that would stand at `pos` should stand instead: `None` keeps `pos` (no other
/// live NPC within the trigger distance, or no acceptable spot was found).
///
/// A ring of `min..max` m at evenly spread angles (random start) is searched for a spot that is
/// ground-snapped like a spawn and at least `max(min, trigger)` from every other NPC; failing
/// that, the best spot that is merely outside the trigger distance. `exclude` is the NPC being
/// moved (0 for a spawn that is not in the world yet).
pub fn scatter_position(
    server: &GameServer,
    world: &WorldInstance,
    exclude: u32,
    pos: (f32, f32, f32),
    params: ScatterParams,
) -> Option<(f32, f32, f32)> {
    let reach = params.max + params.trigger.max(params.min);
    let others = nearby_npc_positions(world, exclude, pos.0, pos.1, reach);
    let nearest = |p: (f32, f32, f32)| {
        others
            .iter()
            .map(|o| dist3(*o, p))
            .fold(f32::INFINITY, f32::min)
    };
    if nearest(pos) > params.trigger {
        return None;
    }
    let original_resolved = spawn_ground_z(server, world, pos.0, pos.1, pos.2).is_some();
    let accept = params.min.max(params.trigger);
    let (lo, hi) = (params.min.min(params.max), params.min.max(params.max));
    let start = random_float(0.0, std::f32::consts::TAU);
    let mut best: Option<(f32, (f32, f32, f32))> = None;
    for i in 0..SCATTER_TRIES {
        let angle = start + GOLDEN_ANGLE * i as f32;
        let radius = random_float(lo, hi);
        let (x, y) = (pos.0 + radius * angle.cos(), pos.1 + radius * angle.sin());
        let Some(z) = candidate_z(server, world, x, y, pos.2, original_resolved) else {
            continue;
        };
        let spot = (x, y, z);
        let score = nearest(spot);
        if score >= accept {
            return Some(spot);
        }
        if score > params.trigger && best.is_none_or(|(s, _)| score > s) {
            best = Some((score, spot));
        }
    }
    best.map(|(_, spot)| spot)
}

/// Moves `npc` (already in the world) off its stack: scatters it, moves its transform, updates
/// the region / visibility, sets the AI home position and broadcasts the stop-move at the new
/// position to the clients that see it. Returns the new position, `None` when it stayed.
pub fn separate_npc(
    server: &GameServer,
    world: &WorldInstance,
    npc: &NpcRef,
    params: ScatterParams,
) -> Option<(f32, f32, f32)> {
    let (pos, parented) = {
        let t = npc.data().unit.transform;
        ((t.x, t.y, t.z), t.has_parent())
    };
    if parented {
        return None;
    }
    let spot = scatter_position(server, world, npc.obj_id, pos, params)?;
    let zone = server
        .game_data
        .worlds
        .get_zone_id(Some(&world.template), spot.0, spot.1);
    {
        let mut d = npc.data();
        d.unit.transform.set_xyz(spot.0, spot.1, spot.2);
        d.unit.transform.zone_id = zone;
    }
    let obj = WorldObject::from(npc);
    crate::systems::ai::npc::check_moved_position(server, &obj);
    if let Some(ai) = server.systems.hooks.ai.get() {
        ai.npc_home_moved(server, npc);
    }
    crate::systems::ai::npc::stop_movement(server, npc);
    Some(spot)
}

// ---------------------------------------------------------------------------------------------
// detection
// ---------------------------------------------------------------------------------------------

/// One NPC of a stack.
#[derive(Debug, Clone)]
pub struct StackMember {
    pub npc: NpcRef,
    pub obj_id: u32,
    pub template_id: u32,
    pub pos: (f32, f32, f32),
}

/// The stacks of one world instance: groups (sorted by obj id) of at least two alive,
/// unparented NPCs without an aggro target, chained by a 3D distance of at most `radius`.
pub fn find_stacks(world: &WorldInstance, radius: f32) -> Vec<Vec<StackMember>> {
    let radius = radius.max(0.0);
    let cell = radius.max(2.0);
    let mut npcs: Vec<StackMember> = Vec::new();
    for npc in world.all_npcs() {
        let (template_id, t, skip) = {
            let d = npc.data();
            let t = d.unit.transform;
            (
                d.unit.template_id,
                t,
                d.unit.is_dead()
                    || t.has_parent()
                    || d.current_aggro_target != 0
                    || d.unit.is_in_battle,
            )
        };
        if skip {
            continue;
        }
        npcs.push(StackMember {
            obj_id: npc.obj_id,
            npc,
            template_id,
            pos: (t.x, t.y, t.z),
        });
    }
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    for (i, n) in npcs.iter().enumerate() {
        grid.entry((
            (n.pos.0 / cell).floor() as i32,
            (n.pos.1 / cell).floor() as i32,
        ))
        .or_default()
        .push(i);
    }
    let mut parent: Vec<usize> = (0..npcs.len()).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    for (i, n) in npcs.iter().enumerate() {
        let (cx, cy) = (
            (n.pos.0 / cell).floor() as i32,
            (n.pos.1 / cell).floor() as i32,
        );
        for dx in -1..=1 {
            for dy in -1..=1 {
                let Some(list) = grid.get(&(cx + dx, cy + dy)) else {
                    continue;
                };
                for &j in list {
                    if j > i && dist3(n.pos, npcs[j].pos) <= radius {
                        let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                        if a != b {
                            parent[a] = b;
                        }
                    }
                }
            }
        }
    }
    let mut groups: HashMap<usize, Vec<StackMember>> = HashMap::new();
    for (i, n) in npcs.iter().enumerate() {
        let root = find(&mut parent, i);
        groups.entry(root).or_default().push(n.clone());
    }
    let mut stacks: Vec<Vec<StackMember>> = groups
        .into_values()
        .filter(|g| g.len() >= 2)
        .map(|mut g| {
            g.sort_by_key(|m| m.obj_id);
            g
        })
        .collect();
    stacks.sort_by_key(|g| g[0].obj_id);
    stacks
}

/// What a stack member is, for log and command lines.
#[derive(Debug, Clone)]
pub struct MemberInfo {
    pub obj_id: u32,
    pub template_id: u32,
    pub name: String,
    /// `Npc::spawner_id` (the spawner's index), 0 = no spawner.
    pub spawner_id: u32,
    /// The spawner's template id (`NpcSpawner::spawner_id`).
    pub spawner_template_id: u32,
    pub pos: (f32, f32, f32),
}

impl std::fmt::Display for MemberInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "obj {} npc {} \"{}\" spawner {} (template {}) at ({:.1}, {:.1}, {:.1})",
            self.obj_id,
            self.template_id,
            self.name,
            self.spawner_id,
            self.spawner_template_id,
            self.pos.0,
            self.pos.1,
            self.pos.2
        )
    }
}

/// The member lines of a stack and whether all of them come from one placement (one spawner
/// entry, `Arc::ptr_eq` on the shared state).
pub fn describe_stack(server: &GameServer, stack: &[StackMember]) -> (Vec<MemberInfo>, bool) {
    let mut infos = Vec::with_capacity(stack.len());
    let mut shared = Vec::with_capacity(stack.len());
    for m in stack {
        let (unit_name, spawner_id, pos) = {
            let d = m.npc.data();
            let t = d.unit.transform;
            (d.unit.name.clone(), d.spawner_id, (t.x, t.y, t.z))
        };
        let name = server
            .game_data
            .systems
            .npcs
            .npc
            .template(m.template_id)
            .map(|t| t.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or(unit_name);
        let spawner = server.systems.npcs.spawns.npc_spawner_of(m.obj_id);
        infos.push(MemberInfo {
            obj_id: m.obj_id,
            template_id: m.template_id,
            name,
            spawner_id,
            spawner_template_id: spawner.as_ref().map_or(0, |s| s.spawner_id),
            pos,
        });
        shared.push(spawner.map(|s| s.shared().clone()));
    }
    let same = shared
        .first()
        .and_then(Option::as_ref)
        .is_some_and(|first| {
            shared
                .iter()
                .all(|s| s.as_ref().is_some_and(|s| Arc::ptr_eq(s, first)))
        });
    (infos, same)
}

// ---------------------------------------------------------------------------------------------
// tracking and the periodic task
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
struct StackRec {
    world: u32,
    first_seen: Instant,
    seen_pass: u64,
    /// The member set and time of the last warning.
    warned: Option<(Vec<u32>, Instant)>,
}

#[derive(Debug, Default)]
struct TrackerInner {
    pass: u64,
    next_id: u64,
    by_obj: HashMap<u32, u64>,
    stacks: HashMap<u64, StackRec>,
}

/// Tracks stacks across check passes (`server.systems.npcs.stacked`).
#[derive(Debug, Default)]
pub struct StackTracker {
    inner: Mutex<TrackerInner>,
}

impl StackTracker {
    /// Feeds one world's stacks (member obj ids, sorted) of one pass; returns the indices of the
    /// stacks to log now: persisted at least `min_age`, and either never logged, logged with
    /// another member set, or logged more than an hour ago. Stacks of `world` that are gone are
    /// forgotten.
    pub fn update(
        &self,
        world: u32,
        stacks: &[Vec<u32>],
        now: Instant,
        min_age: Duration,
    ) -> Vec<usize> {
        let mut g = self.inner.lock().expect("stack tracker poisoned");
        g.pass += 1;
        let pass = g.pass;
        let mut due = Vec::new();
        for (idx, members) in stacks.iter().enumerate() {
            // A stack continues the record of its lowest-numbered known member.
            let known = members
                .iter()
                .filter_map(|m| g.by_obj.get(m).copied())
                .filter(|id| g.stacks.get(id).is_some_and(|r| r.world == world))
                .min();
            let id = known.unwrap_or_else(|| {
                g.next_id += 1;
                let id = g.next_id;
                g.stacks.insert(
                    id,
                    StackRec {
                        world,
                        first_seen: now,
                        seen_pass: pass,
                        warned: None,
                    },
                );
                id
            });
            for m in members {
                g.by_obj.insert(*m, id);
            }
            let rec = g.stacks.get_mut(&id).expect("record exists");
            rec.seen_pass = pass;
            if now.saturating_duration_since(rec.first_seen) < min_age {
                continue;
            }
            let again = match &rec.warned {
                None => true,
                Some((set, at)) => {
                    set != members || now.saturating_duration_since(*at) >= RELOG_AFTER
                }
            };
            if again {
                rec.warned = Some((members.clone(), now));
                due.push(idx);
            }
        }
        let gone: Vec<u64> = g
            .stacks
            .iter()
            .filter(|(_, r)| r.world == world && r.seen_pass != pass)
            .map(|(id, _)| *id)
            .collect();
        for id in &gone {
            g.stacks.remove(id);
        }
        if !gone.is_empty() {
            let alive = &g.stacks;
            let keep: HashSet<u64> = alive.keys().copied().collect();
            g.by_obj.retain(|_, id| keep.contains(id));
        }
        due
    }
}

/// One pass over every world instance at time `now`: logs the stacks that are due and returns
/// how many were logged.
pub fn scan_once(server: &GameServer, now: Instant) -> usize {
    let cfg = &server.config.world;
    let radius = cfg.stacked_npc_radius;
    let min_age = Duration::from_secs(cfg.stacked_npc_min_seconds);
    let mut logged = 0;
    for world in server.world.worlds() {
        let stacks = find_stacks(&world, radius);
        let ids: Vec<Vec<u32>> = stacks
            .iter()
            .map(|s| s.iter().map(|m| m.obj_id).collect())
            .collect();
        let due = server
            .systems
            .npcs
            .stacked
            .update(world.id, &ids, now, min_age);
        tracing::debug!(
            "StackedNpcCheck: instance {}: {} stack(s), {} due",
            world.id,
            stacks.len(),
            due.len()
        );
        for idx in due {
            let (infos, same) = describe_stack(server, &stacks[idx]);
            let p = infos[0].pos;
            let members: Vec<String> = infos.iter().map(|m| m.to_string()).collect();
            tracing::warn!(
                "Stacked NPCs in {} (instance {}) at ({:.0}, {:.0}, {:.0}): {} NPCs within {:.2} m for at least {} s, shared placement: {} - {}",
                world.template.name,
                world.id,
                p.0,
                p.1,
                p.2,
                infos.len(),
                radius,
                min_age.as_secs(),
                if same { "yes" } else { "no" },
                members.join("; ")
            );
            logged += 1;
        }
    }
    logged
}

/// Starts the periodic check (once per server); the interval is re-read every pass
/// (`World.StackedNpcCheckSeconds`, 0 = off for now).
pub fn start(server: &Arc<GameServer>) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let weak = Arc::downgrade(server);
    rt.spawn(async move {
        loop {
            let secs = match weak.upgrade() {
                Some(s) => s.config.world.stacked_npc_check_seconds,
                None => break,
            };
            // A disabled check wakes up every 10 s to notice a config reload.
            tokio::time::sleep(Duration::from_secs(if secs == 0 { 10 } else { secs })).await;
            let Some(server) = weak.upgrade() else {
                break;
            };
            if server.config.world.stacked_npc_check_seconds == 0 {
                continue;
            }
            let start = std::time::Instant::now();
            crate::systems::run_blocking(&server, |s| {
                scan_once(s, spawn_manager::now());
            })
            .await;
            if start.elapsed() > Duration::from_millis(100) {
                tracing::warn!("StackedNpcCheck took {} ms", start.elapsed().as_millis());
            }
        }
    });
}

#[cfg(test)]
#[path = "stacked_npcs_tests.rs"]
mod tests;
