//! `Models/Game/Units/Route/Simulation` (+ the `Move` / `NpcDeleteTask` tasks): the old
//! "move along a recorded `Data/Path/*.path` file" NPC movement that `NpcControlEffect`
//! (`FollowPath`, `RunCommandSet` with a path) and `FollowPathBehavior` drive. Every NPC spawned
//! by a spawner has one (`npc.Simulation = new Simulation(npc)`).
//!
//! The path recording half (`StartRecord` / `Record` / `StopRecord`, GM commands for players)
//! and the `Line` / `Track` / `Combat` patrols (never instantiated) are not used by any
//! ported caller.
//!
//! C# behaviour mirrored: every `RepeatMove` schedules its own 100 ms task and nothing cancels
//! them (a stopped route still walks to its current point); `GetMinCheckPoint` never picks the
//! last point. Deviation: the C# `ReadPath` returns early for an already cached file name without
//! refreshing `MovePath`, so `OnMove` indexed the cached route with the length of another file's
//! lines and threw; here every length check and lookup uses the selected route (`Path`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::models::movement::{MoveKind, MoveType, MoveTypeActorFlags, MoveTypeFlags, UnitMove};
use crate::packets::world::SCOneUnitMovementPacket;
use crate::server::GameServer;
use crate::systems::npcs::npc::packets::SCUnitModelPostureChangedPacket;
use crate::systems::skills::controllers::{
    add_distance_to_front, add_distance_to_front_between, calculate_angle_from, calculate_distance,
    roll_pitch_yaw_sbytes_movement, Vec3,
};
use crate::world::{NpcRef, WorldObject};

use super::npc;

/// `RangeToCheckPoint`.
const RANGE_TO_CHECK_POINT: f32 = 0.5;
/// `Patrol.tolerance`.
const TOLERANCE: f32 = 0.5;

/// `Simulation` state of one NPC.
#[derive(Debug, Default)]
pub struct Simulation {
    pub cycle: bool,
    pub remove: bool,
    pub target_position: Vec3,
    pub paths: HashMap<String, Vec<Vec3>>,
    pub path: Vec<Vec3>,
    pub move_path: Vec<String>,
    pub move_to_path_enabled: bool,
    pub move_to_forward: bool,
    pub running_mode: bool,
    pub move_step_index: i32,
    old_pos: Vec3,
    pub move_file_name: String,
    pub move_file_name2: String,
    skill_id: u32,
    timeout: u32,
    /// `NpcDeleteTask != null`.
    delete_task_pending: bool,
    /// `./Data/Path/` (relative to the server's working directory).
    folder: Option<PathBuf>,
    /// `RecordPath` (the route being recorded, one `|x|y|z|` line per checkpoint).
    pub record_path: Vec<String>,
    /// `PointsCount`.
    pub points_count: i32,
    /// `SavePathEnabled`.
    pub save_path_enabled: bool,
}

/// The shared handle the move tasks keep.
pub type SimRef = Arc<Mutex<Simulation>>;

/// Locks a simulation, recovering from poisoning: a panic in one move task (a bad route) must not
/// make every later tick of that NPC panic on the poisoned lock (`simulation poisoned`) until the
/// server restarts. The state is plain data, so continuing with it is safe.
pub fn lock_sim(sim: &Mutex<Simulation>) -> MutexGuard<'_, Simulation> {
    sim.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("Simulation lock was poisoned by an earlier panic; recovering");
        sim.clear_poison();
        poisoned.into_inner()
    })
}

/// `Simulation.ExtractValue(sData, nIndex)` (`Convert.ToSingle` with the invariant culture).
pub fn extract_value(data: &str, index: usize) -> Result<f32, String> {
    let mut s = data;
    let mut j = 0;
    while j < index {
        match s.find('|') {
            Some(i) => {
                s = &s[i + 1..];
                j += 1;
            }
            None => break,
        }
    }
    if let Some(i) = s.find('|') {
        s = &s[..i];
    }
    s.trim()
        .parse::<f32>()
        .map_err(|_| "Input string was not in a correct format.".to_string())
}

impl Simulation {
    /// `new Simulation(npc)` (`TargetPosition` = the parent's position: NPCs have none).
    pub fn new(folder: Option<PathBuf>) -> Self {
        Self {
            folder,
            ..Self::default()
        }
    }

    /// `GetMinCheckPoint(npc, pointsList)`: the nearest point except the last one.
    pub fn get_min_check_point(&mut self, npc_pos: Vec3, points: &[Vec3]) -> i32 {
        let mut result = -1i32;
        let mut min_dist = -1f32;
        if points.is_empty() {
            return -1;
        }
        for (i, p) in points.iter().enumerate().take(points.len() - 1) {
            self.target_position = *p;
            let m = calculate_distance(*p, npc_pos, true);
            if m <= 0.0 {
                continue;
            }
            if result == -1 || m < min_dist {
                min_dist = m;
                result = i as i32;
            }
        }
        result
    }

    /// `ReadPath()`: `Err` = the exception message (the caller stops the route). `resolved` is
    /// the file `AiPathsManager::resolve_path_file` picked for the NPC (a `<name>@<zone>`
    /// variant for routes shared by several zones); without it `MoveFileName.path` is read.
    /// Returns the key of the route in `paths`: the file stem, so that two files never share a
    /// cache entry.
    fn read_path(&mut self, npc_name: &str, resolved: Option<PathBuf>) -> Result<String, String> {
        let key = resolved
            .as_ref()
            .and_then(|f| f.file_stem())
            .and_then(|s| s.to_str())
            .map_or_else(|| self.move_file_name.clone(), str::to_string);
        if self.paths.contains_key(&key) {
            return Ok(key);
        }
        self.move_path.clear();
        let file = resolved.or_else(|| {
            self.folder
                .as_ref()
                .map(|f| f.join(format!("{}.path", self.move_file_name)))
        });
        match file.as_ref().map(std::fs::read) {
            Some(Ok(bytes)) => {
                let text = String::from_utf8_lossy(&bytes);
                let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
                self.move_path = text.lines().map(str::to_string).collect();
            }
            _ => tracing::debug!(
                "Missing path file {:?} for NPC {npc_name}",
                file.as_ref().map(|f| f.display().to_string())
            ),
        }
        // AddPaths()
        let mut route = Vec::new();
        for s in &self.move_path {
            route.push(Vec3::new(
                extract_value(s, 1)?,
                extract_value(s, 2)?,
                extract_value(s, 3)?,
            ));
        }
        self.paths.entry(key.clone()).or_insert(route);
        Ok(key)
    }

    fn get_paths(&self, name: &str) -> Vec<Vec3> {
        self.paths.get(name).cloned().unwrap_or_default()
    }
}

/// `character.Simulation` (`CSSelectCharacterPacket`: `new Simulation(character)`): the
/// player's own simulation (the `/moveto` route recorder and the NPC it drives), created on first
/// use and kept with the player's obj id.
pub fn character_simulation(server: &GameServer, player: &crate::world::PlayerRef) -> SimRef {
    server
        .systems
        .ai
        .simulations
        .entry(player.obj_id)
        .or_insert_with(|| {
            Arc::new(Mutex::new(Simulation::new(
                server.systems.ai.paths_folder(),
            )))
        })
        .clone()
}

/// `Simulation.StartRecord(sim, ch)`: records the character's position every second until
/// [`stop_record`].
pub fn start_record(server: &GameServer, sim: &SimRef, ch: &crate::world::PlayerRef) {
    {
        let mut s = lock_sim(sim);
        if s.save_path_enabled {
            return;
        }
        if s.move_to_path_enabled {
            // while following the route, recording is not possible.
            return;
        }
        s.record_path.clear();
        s.points_count = 0;
        s.save_path_enabled = true;
    }
    repeat_to(server, sim, ch, 1000);
}

/// `RepeatTo(ch, time, sim)`: a `Record` task in `time` ms while recording.
fn repeat_to(server: &GameServer, sim: &SimRef, ch: &crate::world::PlayerRef, time_ms: u64) {
    if !lock_sim(sim).save_path_enabled {
        return;
    }
    let sim = sim.clone();
    let ch = ch.clone();
    schedule(server, Duration::from_millis(time_ms), move |server| {
        record(server, &sim, &ch);
    });
}

/// `Simulation.Record(sim, ch)`: one checkpoint (`|X|Y|Z|` of the character's world position).
fn record(server: &GameServer, sim: &SimRef, ch: &crate::world::PlayerRef) {
    use crate::systems::services::webapi::json::dotnet_single_string as f;
    let obj = WorldObject::from(ch);
    let p = match server.world.world_of(&obj) {
        Some(w) => w.world_pos_rot(&obj),
        None => obj.transform().local(),
    };
    let line = format!("|{}|{}|{}|", f(p.x), f(p.y), f(p.z)).replace(',', ".");
    {
        let mut s = lock_sim(sim);
        s.record_path.push(line);
        s.points_count += 1;
    }
    repeat_to(server, sim, ch, 1000);
}

/// `Simulation.StopRecord(sim)`: writes `RecordPath` to `Data/Path/{MoveFileName}.path` and stops
/// recording. `Err` = the exception message of the `StreamWriter` (recording stays on then,
/// like the C#).
pub fn stop_record(sim: &SimRef) -> Result<(), String> {
    let (file, lines) = {
        let s = lock_sim(sim);
        let folder = s
            .folder
            .clone()
            .unwrap_or_else(|| PathBuf::from("./Data/Path/"));
        (
            folder.join(format!("{}.path", s.move_file_name)),
            s.record_path.clone(),
        )
    };
    // StreamWriter.WriteLine (Environment.NewLine)
    let newline = if cfg!(windows) { "\r\n" } else { "\n" };
    let mut text = String::new();
    for l in &lines {
        text.push_str(l);
        text.push_str(newline);
    }
    std::fs::write(&file, text).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            let full = std::path::absolute(&file).unwrap_or(file.clone());
            format!("Could not find a part of the path '{}'.", full.display())
        }
        _ => e.to_string(),
    })?;
    lock_sim(sim).save_path_enabled = false;
    Ok(())
}

/// `Simulation.StopMove(npc)`.
pub fn stop_move(server: &GameServer, sim: &SimRef, npc: &NpcRef) {
    npc::stop_movement(server, npc);
    let (remove, timeout, pending) = {
        let mut s = lock_sim(sim);
        s.move_to_path_enabled = false;
        (s.remove, s.timeout, s.delete_task_pending)
    };
    npc.data().unit.is_in_patrol = false;
    if remove {
        if timeout > 0 {
            lock_sim(sim).delete_task_pending = true;
            schedule(server, Duration::from_millis(u64::from(timeout) * 1000), {
                let sim = sim.clone();
                let npc = npc.clone();
                move |server| {
                    lock_sim(&sim).delete_task_pending = false;
                    despawn_with_respawn(server, &npc);
                }
            });
            return;
        }
        if !pending {
            despawn_with_respawn(server, npc);
        }
    }
}

fn despawn_with_respawn(server: &GameServer, npc: &NpcRef) {
    if let Some(s) = server.systems.npcs.spawns.npc_spawner_of(npc.obj_id) {
        s.despawn_with_respawn(server, npc);
    }
}

/// `Simulation.PauseMove(npc)`.
pub fn pause_move(server: &GameServer, npc: &NpcRef) {
    npc::stop_movement(server, npc);
}

/// `TaskManager.Schedule(task, delay)` on the server's runtime.
fn schedule(server: &GameServer, delay: Duration, f: impl FnOnce(&GameServer) + Send + 'static) {
    let (Some(server), Ok(rt)) = (server.self_arc(), tokio::runtime::Handle::try_current()) else {
        return;
    };
    let weak = Arc::downgrade(&server);
    drop(server);
    rt.spawn(async move {
        tokio::time::sleep(delay).await;
        if let Some(s) = weak.upgrade() {
            f(&s);
        }
    });
}

/// `RepeatMove(sim, npc, target, time)`: a `Move` task in `time` ms (`MoveTo` while the NPC is
/// alive).
fn repeat_move(server: &GameServer, sim: &SimRef, npc: &NpcRef, target: Vec3, time_ms: f64) {
    let sim = sim.clone();
    let npc = npc.clone();
    schedule(
        server,
        Duration::from_millis(time_ms.max(0.0) as u64),
        move |server| {
            if npc.data().unit.hp > 0 {
                move_to(server, &sim, &npc, target);
            }
        },
    );
}

fn posture_off(server: &GameServer, npc: &NpcRef) {
    let p = {
        let n = npc.data();
        SCUnitModelPostureChangedPacket {
            obj_id: npc.obj_id,
            posture: crate::models::unit::UnitData::model_posture_type(&*n),
            has_loot_left: n.unit.has_loot_left,
            anim_action_id: n.anim_action_id,
            activate_animation: false,
        }
    };
    crate::systems::skills::broadcast(server, &WorldObject::from(npc), &p, true);
}

/// `Simulation.GoToPath(npc, toForward, skillId, timeout)`.
pub fn go_to_path(
    server: &GameServer,
    sim: &SimRef,
    npc: &NpcRef,
    to_forward: bool,
    skill_id: u32,
    timeout: u32,
) {
    let enabled = {
        let mut s = lock_sim(sim);
        s.move_to_path_enabled = !s.move_to_path_enabled;
        s.move_to_forward = to_forward;
        s.move_to_path_enabled
    };
    if !enabled {
        stop_move(server, sim, npc);
        return;
    }
    start_path(server, sim, npc, skill_id, timeout, 100.0);
}

/// `Simulation.GoToPath2(npc, toForward, skillId, timeout)` (no toggle; the first move waits
/// `timeout` ms).
pub fn go_to_path2(
    server: &GameServer,
    sim: &SimRef,
    npc: &NpcRef,
    to_forward: bool,
    skill_id: u32,
    timeout: u32,
) {
    lock_sim(sim).move_to_forward = to_forward;
    start_path(server, sim, npc, skill_id, timeout, f64::from(timeout));
}

fn start_path(
    server: &GameServer,
    sim: &SimRef,
    npc: &NpcRef,
    skill_id: u32,
    timeout: u32,
    first_delay: f64,
) {
    let name = npc.data().unit.name.clone();
    let pos = npc::world_pos(server, &WorldObject::from(npc));
    let file = lock_sim(sim).move_file_name.clone();
    // Routes shared by several zones are `<name>@<zone key>.path` files: pick by the NPC's position.
    let resolved = server.systems.ai.paths.resolve_path_file(&file, Some(pos));
    let read = lock_sim(sim).read_path(&name, resolved);
    let key = match read {
        Ok(key) => key,
        Err(e) => {
            tracing::warn!("Error in read MovePath: {e}");
            stop_move(server, sim, npc);
            file.clone()
        }
    };
    {
        let mut s = lock_sim(sim);
        s.skill_id = skill_id;
        s.timeout = timeout;
    }
    posture_off(server, npc);
    let pos = npc::world_pos(server, &WorldObject::from(npc));
    let target = {
        let mut s = lock_sim(sim);
        s.path = s.get_paths(&key);
        if s.path.is_empty() {
            // Only part of the retail route data is shipped (generated from the 10.x client, see
            // `Data/Path`), so for the others the NPC just stays put (as in the C#); report each
            // name once.
            if server.systems.ai.first_missing_path(&file) {
                tracing::warn!("Stop moving... no route file Data/Path/{file}.path (or {file}@<zone>.path variant) for MoveFileName={file}; the NPC stays put (reported once per file name)");
            } else {
                tracing::debug!("Stop moving... path data for MoveFileName={file} is missing");
            }
            return;
        }
        let path = s.path.clone();
        let i = s.get_min_check_point(pos, &path);
        if let Some(t) = usize::try_from(i).ok().and_then(|i| path.get(i)).copied() {
            s.move_to_path_enabled = true;
            s.move_step_index = i;
            s.target_position = t;
            if (s.old_pos.x - t.x).abs() > TOLERANCE
                && (s.old_pos.y - t.y).abs() > TOLERANCE
                && (s.old_pos.z - t.z).abs() > TOLERANCE
            {
                s.old_pos = t;
            }
            Some(t)
        } else {
            None
        }
    };
    match target {
        None => stop_move(server, sim, npc),
        Some(t) => repeat_move(server, sim, npc, t, first_delay),
    }
}

/// `Simulation.MoveTo(sim, npc, target)`: one 100 ms step.
pub fn move_to(server: &GameServer, sim: &SimRef, npc: &NpcRef, target: Vec3) {
    let obj = WorldObject::from(npc);
    let returning = server
        .systems
        .ai
        .ais
        .inspect(npc.obj_id, |a| {
            a.current_behavior() == Some(super::framework::BehaviorKind::ReturnState)
        })
        .unwrap_or(false);
    let mut distance = npc::base_move_speed(server, npc, returning) * (100.0 / 1000.0);
    distance *= npc::move_speed_mul(&obj);
    if distance < 0.01 {
        on_move(server, sim, npc);
        return;
    }
    let world = npc::world_pos(server, &obj);
    let dist = calculate_distance(world, target, true);
    if dist <= RANGE_TO_CHECK_POINT {
        on_move(server, sim, npc);
        return;
    }
    if crate::systems::skills::controllers::active(&obj)
        .is_some_and(|sc| sc.state() != crate::systems::skills::controllers::ScState::Ended)
    {
        return;
    }
    let local = npc::local_pos(&obj);
    let target_dist = calculate_distance(local, target, true);
    if target_dist <= 0.05 {
        return;
    }
    let travel = target_dist.min(distance);
    let new = add_distance_to_front_between(travel, target_dist, local, target);
    let (zone, can_fly, spawner_z, hold_or_idle, stance, alertness) = {
        let n = npc.data();
        (
            n.unit.transform.zone_id,
            n.can_fly,
            n.spawner_z,
            n.ai_hold_or_idle,
            n.current_game_stance,
            n.current_alertness,
        )
    };
    // GetReferenceHeight(npc.Ai, ...): `null` for an NPC without an AI
    let ai = super::manager::has_ai(server, npc.obj_id).then_some(
        crate::systems::npcs::heightmap::ReferenceHeightAi {
            can_fly,
            hold_or_idle,
            spawner_z,
            world_z: world.z,
        },
    );
    let instance_id = npc.data().unit.transform.instance_id;
    let z = npc::reference_height_in(server, instance_id, ai, new.x, new.y, new.z, zone);
    let new = Vec3::new(new.x, new.y, z);
    let angle = calculate_angle_from(new, target);
    let (vx, vy) = add_distance_to_front(
        4000.0,
        0.0,
        0.0,
        (std::f64::consts::PI / 180.0 * angle) as f32,
    );
    let yaw = std::f32::consts::PI / 180.0 * (angle as f32 - 90.0);
    obj.with_transform(|t| {
        t.set_xyz(new.x, new.y, new.z);
        t.rot_x = 0.0;
        t.rot_y = 0.0;
        t.rot_z = yaw;
    });
    let (rx, ry, rz) = roll_pitch_yaw_sbytes_movement(0.0, 0.0, yaw);
    let running = lock_sim(sim).running_mode;
    let um = UnitMove {
        x: new.x,
        y: new.y,
        z: new.z,
        vel_x: vx as i16,
        vel_y: vy as i16,
        rotation_x: rx,
        rotation_y: ry,
        rotation_z: rz,
        actor_flags: MoveTypeActorFlags(if running { 4 } else { 5 }),
        delta_movement: [0, if running { 127 } else { 63 }, 0],
        stance,
        alertness,
        ..UnitMove::default()
    };
    let mut mt = MoveType::new(MoveKind::Unit(um));
    mt.flags = MoveTypeFlags::NONE;
    mt.time = npc::ms_of_day();
    npc::check_moved_position(server, &obj);
    crate::systems::skills::broadcast(
        server,
        &obj,
        &SCOneUnitMovementPacket {
            obj_id: npc.obj_id,
            move_type: mt,
        },
        true,
    );
    repeat_move(server, sim, npc, target, 100.0);
}

/// What `OnMove` does after reaching (or not yet reaching) a checkpoint.
#[derive(Debug, PartialEq)]
enum Next {
    Repeat(Vec3, f64),
    Stop,
    Cycle(Vec3),
    Second,
}

impl Simulation {
    /// The checkpoint part of `Simulation.OnMove`: moves `move_step_index` along `path` (back and
    /// forth) and says what to do next. `Err` = the index is outside the selected route (the C#
    /// throws there); the caller stops the move. Only `path`, the parsed points of the route
    /// actually selected, is consulted for lengths and targets.
    fn advance(&mut self, pos: Vec3) -> Result<Next, String> {
        let out_of_range =
            |idx: i64, len: usize| format!("path index {idx} out of range (route length {len})");
        let len = self.path.len();
        let idx = self.move_step_index.max(0) as usize;
        let Some(t) = self.path.get(idx).copied() else {
            return Err(out_of_range(i64::from(self.move_step_index), len));
        };
        self.target_position = t;
        if calculate_distance(pos, t, true) > 3.0 {
            return Ok(Next::Repeat(t, 100.0));
        }
        if self.move_to_forward {
            if idx + 1 >= len {
                // turn back
                let back = idx.saturating_sub(1);
                let t = self.path[back];
                self.move_to_forward = false;
                self.move_step_index = back as i32;
                self.target_position = t;
                Ok(if self.cycle {
                    Next::Cycle(t)
                } else if !self.move_file_name2.is_empty() {
                    Next::Second
                } else {
                    Next::Stop
                })
            } else {
                let t = self.path[idx + 1];
                self.move_step_index = (idx + 1) as i32;
                self.target_position = t;
                Ok(Next::Repeat(t, 100.0))
            }
        } else if idx > 0 {
            let t = self.path[idx - 1];
            self.move_step_index = (idx - 1) as i32;
            self.target_position = t;
            Ok(Next::Repeat(t, 100.0))
        } else {
            // turn back; a one-point route has no second point to go to
            let Some(t) = self.path.get(1).copied() else {
                return Err(out_of_range(1, len));
            };
            self.move_to_forward = true;
            self.move_step_index = 1;
            self.target_position = t;
            Ok(if self.cycle {
                Next::Cycle(t)
            } else {
                Next::Stop
            })
        }
    }
}

/// `Simulation.OnMove(npc)`: next checkpoint (back and forth along the path).
pub fn on_move(server: &GameServer, sim: &SimRef, npc: &NpcRef) {
    let (enabled, empty) = {
        let s = lock_sim(sim);
        (s.move_to_path_enabled, s.path.is_empty())
    };
    if !enabled || empty {
        stop_move(server, sim, npc);
        return;
    }
    let pos = npc::world_pos(server, &WorldObject::from(npc));
    let next = {
        let mut s = lock_sim(sim);
        match s.advance(pos) {
            Ok(next) => next,
            Err(e) => {
                tracing::warn!(
                    "Simulation: npc {} route {:?}: {e} (step index {}); stopping the move",
                    npc.obj_id,
                    s.move_file_name,
                    s.move_step_index
                );
                drop(s);
                stop_move(server, sim, npc);
                return;
            }
        }
    };
    match next {
        Next::Repeat(t, ms) => repeat_move(server, sim, npc, t, ms),
        Next::Stop => stop_move(server, sim, npc),
        Next::Cycle(t) => {
            // let's pause, use skill
            pause_move(server, npc);
            let mut time = 100.0;
            let (skill_id, timeout) = {
                let s = lock_sim(sim);
                (s.skill_id, s.timeout)
            };
            if skill_id > 0 {
                let me = WorldObject::from(npc);
                crate::systems::skills::skill::unit_use_skill(server, &me, skill_id, &me);
                if timeout > 0 {
                    time = f64::from(timeout) * 1000.0;
                } else if let Some(t) = server.game_data.systems.skills.skill(skill_id) {
                    if t.casting_time != 0 && t.cooldown_time != 0 {
                        time = f64::from(t.casting_time + t.cooldown_time);
                    }
                }
                lock_sim(sim).skill_id = 0;
            }
            repeat_move(server, sim, npc, t, time);
        }
        Next::Second => {
            pause_move(server, npc);
            let time = {
                let mut s = lock_sim(sim);
                s.move_file_name = s.move_file_name2.clone(); // the way back
                s.timeout.wrapping_mul(1000)
            };
            go_to_path2(server, sim, npc, true, 0, time);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_values() {
        assert_eq!(extract_value("|15629.0|14989.02|141.2055|", 1), Ok(15629.0));
        assert_eq!(
            extract_value("|15629.0|14989.02|141.2055|", 3),
            Ok(141.2055)
        );
        assert_eq!(extract_value("None|1|2|3|", 2), Ok(2.0));
        assert!(extract_value("|a|", 1).is_err());
    }

    #[test]
    fn missing_path_is_reported_once_per_file_name() {
        let ai = crate::systems::ai::System::default();
        assert!(ai.first_missing_path("public_mate2"));
        assert!(!ai.first_missing_path("public_mate2"));
        assert!(ai.first_missing_path("tuto_move"));
        assert!(!ai.first_missing_path("tuto_move"));
        assert!(!ai.first_missing_path("public_mate2"));
    }

    #[test]
    fn min_check_point_skips_the_last() {
        let mut s = Simulation::new(None);
        let pts = [
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(10.0, 0.0, 0.0),
            Vec3::new(20.0, 0.0, 0.0),
        ];
        assert_eq!(s.get_min_check_point(Vec3::new(19.0, 0.0, 0.0), &pts), 1);
        assert_eq!(
            s.get_min_check_point(Vec3::new(0.0, 0.0, 0.0), &pts),
            1,
            "zero distance is skipped"
        );
        assert_eq!(s.get_min_check_point(Vec3::new(0.0, 0.0, 0.0), &[]), -1);
    }

    #[test]
    fn routes_are_cached_by_the_resolved_file() {
        let dir = std::env::temp_dir().join(format!("aaemu_sim_paths_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("farm@1.path"), "|10|20|30|\n|11|20|30|\n").unwrap();
        std::fs::write(dir.join("farm@2.path"), "|500|600|30|\n|501|600|30|\n").unwrap();
        let mut s = Simulation::new(Some(dir.clone()));
        s.move_file_name = "farm".to_string();
        let a = s.read_path("npc", Some(dir.join("farm@2.path"))).unwrap();
        assert_eq!(a, "farm@2");
        assert_eq!(s.get_paths(&a)[0], Vec3::new(500.0, 600.0, 30.0));
        // Another variant of the same MoveFileName does not reuse the cached route.
        let b = s.read_path("npc", Some(dir.join("farm@1.path"))).unwrap();
        assert_eq!(b, "farm@1");
        assert_eq!(s.get_paths(&b)[0], Vec3::new(10.0, 20.0, 30.0));
        assert_eq!(s.get_paths(&a)[0], Vec3::new(500.0, 600.0, 30.0));
        // Without a resolved file the plain MoveFileName is used (missing: an empty route).
        assert_eq!(s.read_path("npc", None).unwrap(), "farm");
        assert!(s.get_paths("farm").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Selects `key` as the route like `start_path` does and walks it with `advance`, standing on
    /// each checkpoint, until the move stops. Returns the visited step indexes.
    fn walk(s: &mut Simulation, key: &str, start: i32) -> Vec<i32> {
        s.path = s.get_paths(key);
        s.move_step_index = start;
        s.move_to_forward = true;
        let mut visited = vec![start];
        for _ in 0..100 {
            let here = s.path[s.move_step_index as usize];
            match s.advance(here).expect("route index must stay in range") {
                Next::Repeat(..) => visited.push(s.move_step_index),
                Next::Stop => return visited,
                other => panic!("unexpected {other:?}"),
            }
        }
        panic!("route never ended: {visited:?}");
    }

    #[test]
    fn cached_route_is_walked_by_its_own_length() {
        // A (5 points) -> B (3 points) -> cached A: `move_path` holds B's 3 lines while `path` is A's
        // 5 points. The end of the route must come from `path`, not from the stale line count.
        let dir = std::env::temp_dir().join(format!("aaemu_sim_stale_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let line = |x: i32| {
            format!(
                "|{x}|0|0|
"
            )
        };
        std::fs::write(
            dir.join("a.path"),
            (0..5).map(|i| line(i * 10)).collect::<String>(),
        )
        .unwrap();
        std::fs::write(
            dir.join("b.path"),
            (0..3).map(|i| line(i * 10)).collect::<String>(),
        )
        .unwrap();
        let mut s = Simulation::new(Some(dir.clone()));
        s.move_file_name = "a".to_string();
        let a = s.read_path("npc", None).unwrap();
        s.move_file_name = "b".to_string();
        let b = s.read_path("npc", None).unwrap();
        s.move_file_name = "a".to_string();
        let a2 = s.read_path("npc", None).unwrap();
        assert_eq!((a.as_str(), b.as_str(), a2.as_str()), ("a", "b", "a"));
        assert_eq!(
            s.move_path.len(),
            3,
            "the stale lines of the last file read"
        );
        assert_eq!(walk(&mut s, "a", 0), vec![0, 1, 2, 3, 4]);
        // the crashing direction: B cached while the last file read was the longer A, so
        // `move_path` (5 lines) is longer than B's 3 points (the old code indexed `path[3]`)
        s.move_file_name = "a".to_string();
        s.paths.remove("a");
        s.read_path("npc", None).unwrap();
        s.move_file_name = "b".to_string();
        s.read_path("npc", None).unwrap();
        assert_eq!(s.move_path.len(), 5);
        assert_eq!(walk(&mut s, "b", 0), vec![0, 1, 2]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn advance_turns_back_and_stops_on_a_short_route() {
        let mut s = Simulation::new(None);
        s.path = vec![Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 0.0, 0.0)];
        s.move_to_forward = true;
        s.move_step_index = 1;
        let here = s.path[1];
        assert_eq!(s.advance(here), Ok(Next::Stop));
        assert!(!s.move_to_forward);
        assert_eq!(s.move_step_index, 0);
        // back at the start: turn forward again (non-cyclic: stop)
        let here = s.path[0];
        assert_eq!(s.advance(here), Ok(Next::Stop));
        assert!(s.move_to_forward);
        assert_eq!(s.move_step_index, 1);
    }

    #[test]
    fn one_point_route_stops_instead_of_panicking() {
        let mut s = Simulation::new(None);
        s.path = vec![Vec3::new(5.0, 5.0, 0.0)];
        s.move_to_forward = true;
        s.move_step_index = 0;
        let here = s.path[0];
        // forward at the only point: turn back, nowhere to go
        assert_eq!(s.advance(here), Ok(Next::Stop));
        // facing backward at index 0 needs a second point
        assert!(s.advance(here).is_err());
    }

    #[test]
    fn out_of_range_index_is_an_error_not_a_panic() {
        let mut s = Simulation::new(None);
        s.path = vec![Vec3::new(0.0, 0.0, 0.0); 3];
        s.move_step_index = 5;
        assert!(s.advance(Vec3::new(0.0, 0.0, 0.0)).is_err());
        s.path.clear();
        s.move_step_index = 0;
        assert!(s.advance(Vec3::new(0.0, 0.0, 0.0)).is_err());
    }

    #[test]
    fn poisoned_simulation_lock_is_recovered() {
        let sim: SimRef = Arc::new(Mutex::new(Simulation::new(None)));
        let c = sim.clone();
        let _ = std::thread::spawn(move || {
            let _g = c.lock().unwrap();
            panic!("poison the simulation");
        })
        .join();
        assert!(sim.is_poisoned());
        lock_sim(&sim).move_step_index = 7;
        assert_eq!(lock_sim(&sim).move_step_index, 7);
        assert!(!sim.is_poisoned());
    }
}
