//! `Models/Game/AI/v2/Controls/**` (`AiPathHandler`, `AiPathPoint`, `AiPathPointAction`) and
//! `Core/Managers/AiPathsManager` (`Data/Path/*.path`, cached by file name).
//!
//! Deviation from the C#: a route name that is used in several zones (`public_farm`,
//! `tuto_move`, ...) is shipped as `<name>@<zone key>.path` variants instead of one file; see
//! [`AiPathsManager::resolve_path_file`].

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::systems::skills::controllers::{calculate_distance, Vec3};

/// `AiPathPointAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiPathPointAction {
    /// No additional actions for this point.
    None,
    /// Disables movement path loop (default behavior).
    DisableLoop,
    /// Enables movement path loop.
    EnableLoop,
    /// Changes movement speed to be used when following the path.
    Speed,
    /// Stance to be used when following the path.
    ActorFlags,
    /// Inserted by RunCommandSet to mark that it needs to return there.
    ReturnToCommandSet,
    /// An undefined numeric value (`Enum.TryParse` accepts it; `RunCurrentPath` throws).
    Other(i64),
}

impl AiPathPointAction {
    /// `Enum.TryParse<AiPathPointAction>(text, ignoreCase: true)`: names or numeric values.
    pub fn parse(text: &str) -> Option<Self> {
        let all = [
            Self::None,
            Self::DisableLoop,
            Self::EnableLoop,
            Self::Speed,
            Self::ActorFlags,
            Self::ReturnToCommandSet,
        ];
        let t = text.trim();
        if let Ok(n) = t.parse::<i64>() {
            // Enum.TryParse accepts any integer (undefined values parse too).
            return Some(
                usize::try_from(n)
                    .ok()
                    .and_then(|i| all.get(i).copied())
                    .unwrap_or(Self::Other(n)),
            );
        }
        all.into_iter()
            .find(|a| format!("{a:?}").eq_ignore_ascii_case(t))
    }
}

/// `AiPathPoint`.
#[derive(Debug, Clone, PartialEq)]
pub struct AiPathPoint {
    pub position: Vec3,
    pub action: AiPathPointAction,
    pub param: String,
}

/// How long a target may make no progress before it is dropped. Straight-line steps always
/// reduce the distance to a reachable target; a blocked/exiled one (a target above the ground
/// the walker follows, a body pushed away every tick, ...) would otherwise be chased forever.
pub const TARGET_STALL_TIMEOUT_MS: i64 = 5_000;

/// `AiPathHandler`.
#[derive(Debug, Clone)]
pub struct AiPathHandler {
    /// Loaded AI path points.
    pub ai_path_points: Vec<AiPathPoint>,
    /// Queue of locations to go to next.
    pub ai_path_points_remaining: VecDeque<AiPathPoint>,
    /// Needs to be set to true to trigger initial loading into the queue.
    pub ai_path_looping: bool,
    /// Speed multiplier when moving on the path.
    pub ai_path_speed: f32,
    /// Stance to use when moving on the path; 5-walk, 4-run, 3-stand still.
    pub ai_path_actor_flags: u8,
    /// Currently targeted position for path movement (`Vector3.Zero` = none).
    pub target_position: Vec3,
    /// Route file stem of the loaded points (diagnostics, e.g. the stall warning).
    pub path_name: String,
    /// Closest distance to [`Self::target_position`] seen since it was set.
    target_best_distance: f32,
    /// When the target last got closer (`now_ms`).
    target_progress_time: i64,
}

impl Default for AiPathHandler {
    fn default() -> Self {
        Self {
            ai_path_points: Vec::new(),
            ai_path_points_remaining: VecDeque::new(),
            ai_path_looping: true,
            ai_path_speed: 1.0,
            ai_path_actor_flags: 4,
            target_position: Vec3::default(),
            path_name: String::new(),
            target_best_distance: f32::INFINITY,
            target_progress_time: 0,
        }
    }
}

/// What one `RunCurrentPath` step asks its owner to do.
#[derive(Debug, Clone, PartialEq)]
pub enum PathStep {
    /// `Owner.GoToRunCommandSet(); return true;`
    ReturnToCommandSet,
    /// Move towards `target` with `distance` (already speed / delta scaled) and the path's
    /// actor flags, then update the idle position; the bool is the return value.
    Move {
        target: Vec3,
        speed: f32,
        actor_flags: u8,
        result: bool,
    },
    /// Nothing to move; the bool is the return value (`HasUnhandledPathMovementData()`).
    Idle(bool),
    /// The current target made no progress for [`TARGET_STALL_TIMEOUT_MS`] and was dropped;
    /// the caller logs it and moves on. The bool is `HasUnhandledPathMovementData()`.
    Stalled {
        target: Vec3,
        path: String,
        result: bool,
    },
    /// `throw new NotSupportedException(...)` for an action that cannot be handled.
    Unsupported(String),
}

fn is_zero(v: Vec3) -> bool {
    v.x == 0.0 && v.y == 0.0 && v.z == 0.0
}

impl AiPathHandler {
    /// The bookkeeping half of `RunCurrentPath(delta)`: refills / dequeues and tells the caller
    /// what to do. `owner_pos` = `Owner.Owner.Transform.World.Position`, `template_scale` =
    /// `Owner.Owner.Template.Scale`, `include_z` = the owner moves in 3D (`CanFly`).
    pub fn run_current_path(
        &mut self,
        owner_pos: Vec3,
        template_scale: f32,
        include_z: bool,
    ) -> PathStep {
        self.run_current_path_at(owner_pos, template_scale, include_z, crate::world::now_ms())
    }

    /// [`Self::run_current_path`] with the clock injected (unit tests).
    pub fn run_current_path_at(
        &mut self,
        owner_pos: Vec3,
        template_scale: f32,
        include_z: bool,
        now_ms: i64,
    ) -> PathStep {
        // Queue empty? refill!
        if self.ai_path_points_remaining.is_empty()
            && !self.ai_path_points.is_empty()
            && self.ai_path_looping
        {
            self.ai_path_looping = false;
            self.ai_path_points_remaining
                .extend(self.ai_path_points.iter().cloned());
        }
        // Are we there yet? Ground walkers follow the terrain, so the authored Z of a path
        // point is not where their feet end up: only the horizontal distance says whether they
        // reached it. The C# used the 3D distance and chased points above the ground forever
        // (every tick emitted a tiny in-place step). The radius is at least the 1 m movement
        // tolerance of `Npc.MoveTowards`, so a target the movement stopped in front of still
        // counts as reached (a template scale below 1 m never did in the C#).
        let radius = template_scale.max(1.0);
        if !is_zero(self.target_position) {
            let distance = calculate_distance(self.target_position, owner_pos, include_z);
            if distance <= radius {
                self.target_position = Vec3::default();
                self.target_best_distance = f32::INFINITY;
            } else if distance < self.target_best_distance - 1e-3 {
                // Progress; the stall fallback below must not fire while moving.
                self.target_best_distance = distance;
                self.target_progress_time = now_ms;
            } else if now_ms - self.target_progress_time >= TARGET_STALL_TIMEOUT_MS {
                // Blocked or unreachable: drop it so the route can continue. The caller logs
                // the skipped point (this type has no NPC id).
                let target = self.target_position;
                self.target_position = Vec3::default();
                self.target_best_distance = f32::INFINITY;
                return PathStep::Stalled {
                    target,
                    path: self.path_name.clone(),
                    result: self.has_unhandled_path_movement_data(),
                };
            }
        }
        // No current target? Set it!
        if is_zero(self.target_position) {
            if let Some(next) = self.ai_path_points_remaining.pop_front() {
                match next.action {
                    AiPathPointAction::None => {}
                    AiPathPointAction::DisableLoop => self.ai_path_looping = false,
                    AiPathPointAction::EnableLoop => self.ai_path_looping = true,
                    AiPathPointAction::Speed => {
                        if let Some(s) = parse_invariant_f32(&next.param) {
                            self.ai_path_speed = s;
                        }
                    }
                    AiPathPointAction::ActorFlags => {
                        if let Ok(f) = next.param.trim().parse::<u8>() {
                            self.ai_path_actor_flags = f;
                        }
                    }
                    AiPathPointAction::ReturnToCommandSet => return PathStep::ReturnToCommandSet,
                    AiPathPointAction::Other(v) => {
                        return PathStep::Unsupported(format!(
                            "Not supported nextPos.Action value: {v}"
                        ))
                    }
                }
                // Set next move point if it's not zero
                if !is_zero(next.position) {
                    self.target_position = next.position;
                    self.target_best_distance =
                        calculate_distance(next.position, owner_pos, include_z);
                    self.target_progress_time = now_ms;
                }
            }
        }
        if !is_zero(self.target_position) {
            return PathStep::Move {
                target: self.target_position,
                speed: self.ai_path_speed,
                actor_flags: self.ai_path_actor_flags,
                result: self.has_unhandled_path_movement_data(),
            };
        }
        PathStep::Idle(self.has_unhandled_path_movement_data())
    }

    /// Restart the stall clock for the current target. Re-entering FollowPath after an
    /// interruption (combat, alert) must not count the interruption as "no progress".
    pub fn reset_target_progress(&mut self, owner_pos: Vec3, include_z: bool, now_ms: i64) {
        if is_zero(self.target_position) {
            return;
        }
        self.target_best_distance = calculate_distance(self.target_position, owner_pos, include_z);
        self.target_progress_time = now_ms;
    }

    /// `HasUnhandledPathMovementData()`.
    pub fn has_unhandled_path_movement_data(&self) -> bool {
        !self.ai_path_points_remaining.is_empty()
            || self.ai_path_looping
            || !is_zero(self.target_position)
    }

    /// `HasPathMovementData()`.
    pub fn has_path_movement_data(&self) -> bool {
        !self.ai_path_points_remaining.is_empty()
            || (self.ai_path_looping && !self.ai_path_points.is_empty())
    }
}

/// `float.TryParse(text, CultureInfo.InvariantCulture)`.
fn parse_invariant_f32(text: &str) -> Option<f32> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    t.parse::<f32>().ok()
}

/// `float.TryParse(text)` with the server's (invariant) culture; failures give 0.
fn parse_coord(text: &str) -> f32 {
    parse_invariant_f32(text).unwrap_or(0.0)
}

/// `AiPathsManager`: path files cached by name.
#[derive(Debug, Default)]
pub struct AiPathsManager {
    folder: Mutex<Option<PathBuf>>,
    cache: Mutex<HashMap<String, Arc<Vec<AiPathPoint>>>>,
    /// Route name to its `<name>@<zone key>` file stems (lowest zone key first), read from the
    /// folder on first use.
    variants: Mutex<Option<HashMap<String, Vec<String>>>>,
}

/// Horizontal distance from `p` to the nearest segment (or point) of a route.
fn distance_to_route(p: Vec3, route: &[AiPathPoint]) -> f32 {
    let seg = |a: Vec3, b: Vec3| {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let len2 = dx * dx + dy * dy;
        let t = if len2 == 0.0 {
            0.0
        } else {
            (((p.x - a.x) * dx + (p.y - a.y) * dy) / len2).clamp(0.0, 1.0)
        };
        (p.x - a.x - t * dx).hypot(p.y - a.y - t * dy)
    };
    match route {
        [] => f32::INFINITY,
        [a] => seg(a.position, a.position),
        _ => route
            .windows(2)
            .map(|w| seg(w[0].position, w[1].position))
            .fold(f32::INFINITY, f32::min),
    }
}

/// One `.path` file (`File.ReadAllLines` + the per-line parse).
pub fn parse_path_file(text: &str) -> Vec<AiPathPoint> {
    let mut res = Vec::new();
    for line in text.lines() {
        let columns: Vec<&str> = line.split('|').collect();
        if columns.len() != 5 {
            continue;
        }
        let action = AiPathPointAction::parse(columns[0]).unwrap_or(AiPathPointAction::None);
        res.push(AiPathPoint {
            position: Vec3::new(
                parse_coord(columns[1]),
                parse_coord(columns[2]),
                parse_coord(columns[3]),
            ),
            action,
            param: columns[4].to_string(),
        });
    }
    res
}

impl AiPathsManager {
    /// Sets `Data/Path` (the C# `Path.Combine("Data", "Path")` relative to the working dir).
    pub fn set_folder(&self, folder: impl AsRef<Path>) {
        *self.folder.lock().expect("paths poisoned") = Some(folder.as_ref().to_path_buf());
        *self.variants.lock().expect("paths poisoned") = None;
    }

    /// The `<name>@<zone key>.path` stems of route `name`, lowest zone key first.
    fn variants_of(&self, name: &str) -> Vec<String> {
        let mut guard = self.variants.lock().expect("paths poisoned");
        if guard.is_none() {
            let mut map: HashMap<String, Vec<(u32, String)>> = HashMap::new();
            let folder = self.folder.lock().expect("paths poisoned").clone();
            if let Some(entries) = folder.and_then(|f| std::fs::read_dir(f).ok()) {
                for e in entries.filter_map(Result::ok) {
                    let p = e.path();
                    let is_path = p
                        .extension()
                        .is_some_and(|x| x.eq_ignore_ascii_case("path"));
                    let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    if let (true, Some((base, zone))) = (is_path, stem.rsplit_once('@')) {
                        let zone = zone.parse::<u32>().unwrap_or(u32::MAX);
                        map.entry(base.to_string())
                            .or_default()
                            .push((zone, stem.to_string()));
                    }
                }
            }
            *guard = Some(
                map.into_iter()
                    .map(|(base, mut v)| {
                        v.sort();
                        (base, v.into_iter().map(|(_, stem)| stem).collect())
                    })
                    .collect(),
            );
        }
        guard
            .as_ref()
            .and_then(|m| m.get(name))
            .cloned()
            .unwrap_or_default()
    }

    /// The file stem that serves route `name` for an NPC at `near`: `<name>` itself when that
    /// route exists, otherwise the `<name>@<zone key>` variant whose route passes closest to
    /// the NPC (horizontally; ties go to the lowest zone key). With several variants and no
    /// position there is no answer.
    pub fn resolve_stem(&self, name: &str, near: Option<Vec3>) -> Option<String> {
        let plain_cached = self
            .cache
            .lock()
            .expect("paths poisoned")
            .contains_key(name);
        if plain_cached {
            return Some(name.to_string());
        }
        let folder = self.folder.lock().expect("paths poisoned").clone();
        if folder.is_some_and(|f| f.join(format!("{name}.path")).is_file()) {
            return Some(name.to_string());
        }
        let variants = self.variants_of(name);
        match (variants.as_slice(), near) {
            ([], _) => None,
            ([only], _) => Some(only.clone()),
            (_, None) => None,
            (all, Some(p)) => {
                let mut best: Option<(f32, &String)> = None;
                for stem in all {
                    let d = distance_to_route(p, &self.load_ai_path_points(stem));
                    if best.is_none_or(|(bd, _)| d < bd) {
                        best = Some((d, stem));
                    }
                }
                best.map(|(_, stem)| stem.clone())
            }
        }
    }

    /// The route file for `name` (see [`Self::resolve_stem`]).
    pub fn resolve_path_file(&self, name: &str, near: Option<Vec3>) -> Option<PathBuf> {
        let stem = self.resolve_stem(name, near)?;
        let folder = self.folder.lock().expect("paths poisoned").clone()?;
        Some(folder.join(format!("{stem}.path")))
    }

    /// `LoadAiPathPoints(aiPathFileName)` for an NPC at `near`: the points of the file that
    /// [`Self::resolve_stem`] picks, or nothing.
    pub fn load_ai_path_points_near(
        &self,
        name: &str,
        near: Option<Vec3>,
    ) -> Arc<Vec<AiPathPoint>> {
        match self.resolve_stem(name, near) {
            Some(stem) => self.load_ai_path_points(&stem),
            None => Arc::new(Vec::new()),
        }
    }

    /// `LoadAiPathPoints(aiPathFileName)`: the cached points, or the file's (cached when the
    /// file exists), or nothing.
    pub fn load_ai_path_points(&self, name: &str) -> Arc<Vec<AiPathPoint>> {
        let mut cache = self.cache.lock().expect("paths poisoned");
        if let Some(p) = cache.get(name) {
            return p.clone();
        }
        let Some(folder) = self.folder.lock().expect("paths poisoned").clone() else {
            return Arc::new(Vec::new());
        };
        let file = folder.join(format!("{name}.path"));
        let Ok(bytes) = std::fs::read(&file) else {
            return Arc::new(Vec::new());
        };
        let text = String::from_utf8_lossy(&bytes);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let points = Arc::new(parse_path_file(text));
        cache.insert(name.to_string(), points.clone());
        points
    }

    /// Installs a path directly (tests).
    pub fn insert(&self, name: &str, points: Vec<AiPathPoint>) {
        self.cache
            .lock()
            .expect("paths poisoned")
            .insert(name.to_string(), Arc::new(points));
    }

    /// `Load()`: caches every `*.path` file of the folder (sub folders excluded). Returns the
    /// number of non-empty files.
    pub fn load(&self) -> usize {
        let Some(folder) = self.folder.lock().expect("paths poisoned").clone() else {
            return 0;
        };
        let Ok(entries) = std::fs::read_dir(&folder) else {
            tracing::error!("Could not read path folder {}", folder.display());
            return 0;
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| {
                let p = e.path();
                let is_path = p
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("path"));
                is_path.then(|| p.file_stem()?.to_str().map(str::to_string))?
            })
            .collect();
        names.sort();
        let loaded = names
            .iter()
            .filter(|n| !self.load_ai_path_points(n).is_empty())
            .count();
        tracing::info!("Loaded {loaded} path files from {}", folder.display());
        loaded
    }

    /// `ClearCache()`.
    pub fn clear_cache(&self) {
        self.cache.lock().expect("paths poisoned").clear();
        *self.variants.lock().expect("paths poisoned") = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_file_lines() {
        let p = parse_path_file(
            "None|100.5|200|30|\nSpeed|0|0|0|3\nbogus line\nACTORFLAGS|1|2|3|5\nEnableLoop|x|1|2|\n7|1|1|1|\n",
        );
        assert_eq!(p.len(), 5);
        assert_eq!(p[0].position, Vec3::new(100.5, 200.0, 30.0));
        assert_eq!(p[1].action, AiPathPointAction::Speed);
        assert_eq!(p[1].param, "3");
        assert_eq!(p[2].action, AiPathPointAction::ActorFlags);
        assert_eq!(p[3].action, AiPathPointAction::EnableLoop);
        assert_eq!(p[3].position, Vec3::new(0.0, 1.0, 2.0));
        // an undefined numeric action parses (and throws when reached)
        assert_eq!(p[4].action, AiPathPointAction::Other(7));
    }

    #[test]
    fn run_current_path_walks_and_loops() {
        let mut h = AiPathHandler {
            ai_path_points: vec![
                AiPathPoint {
                    position: Vec3::new(10.0, 0.0, 0.0),
                    action: AiPathPointAction::Speed,
                    param: "2.5".into(),
                },
                AiPathPoint {
                    position: Vec3::new(20.0, 0.0, 0.0),
                    action: AiPathPointAction::DisableLoop,
                    param: String::new(),
                },
            ],
            ..AiPathHandler::default()
        };
        // first call refills the queue and targets the first point
        let s = h.run_current_path(Vec3::new(0.0, 0.0, 0.0), 1.0, false);
        assert_eq!(
            s,
            PathStep::Move {
                target: Vec3::new(10.0, 0.0, 0.0),
                speed: 2.5,
                actor_flags: 4,
                result: true
            }
        );
        assert!(!h.ai_path_looping);
        // reaching the point (closer than Template.Scale) moves on
        let s = h.run_current_path(Vec3::new(9.5, 0.0, 0.0), 1.0, false);
        assert!(matches!(s, PathStep::Move { target, .. } if target == Vec3::new(20.0, 0.0, 0.0)));
        // last point reached: nothing left, no loop
        let s = h.run_current_path(Vec3::new(20.0, 0.0, 0.0), 1.0, false);
        assert_eq!(s, PathStep::Idle(false));
        assert!(!h.has_path_movement_data());
    }

    fn two_points(z: f32) -> AiPathHandler {
        AiPathHandler {
            ai_path_points: vec![
                AiPathPoint {
                    position: Vec3::new(10.0, 0.0, z),
                    action: AiPathPointAction::None,
                    param: String::new(),
                },
                AiPathPoint {
                    position: Vec3::new(20.0, 0.0, 0.0),
                    action: AiPathPointAction::DisableLoop,
                    param: String::new(),
                },
            ],
            path_name: "public_farm".into(),
            ..AiPathHandler::default()
        }
    }

    #[test]
    fn a_ground_walker_arrives_at_a_point_authored_above_the_ground() {
        // The route's Z can be above the terrain the walker follows (public_farm's second point
        // is 1.4 m high). The 3D distance there never drops below 1.4 m, so the C# never
        // arrived and emitted an in-place step every tick.
        let mut h = two_points(1.4);
        let s = h.run_current_path_at(Vec3::new(0.0, 0.0, 0.0), 1.0, false, 0);
        assert!(matches!(s, PathStep::Move { target, .. } if target == Vec3::new(10.0, 0.0, 1.4)));
        // Horizontally on the point, 1.4 m below it: reached (3D distance would be 1.4 m).
        let s = h.run_current_path_at(Vec3::new(10.0, 0.0, 0.0), 1.0, false, 100);
        assert!(matches!(s, PathStep::Move { target, .. } if target == Vec3::new(20.0, 0.0, 0.0)));
    }

    #[test]
    fn a_flying_npc_still_has_to_reach_the_authored_height() {
        let mut h = two_points(1.4);
        let s = h.run_current_path_at(Vec3::new(0.0, 0.0, 0.0), 1.0, true, 0);
        assert!(matches!(s, PathStep::Move { target, .. } if target == Vec3::new(10.0, 0.0, 1.4)));
        // Below the point at the same XY: not reached in 3D.
        let s = h.run_current_path_at(Vec3::new(10.0, 0.0, 0.0), 1.0, true, 100);
        assert!(matches!(s, PathStep::Move { target, .. } if target == Vec3::new(10.0, 0.0, 1.4)));
        let s = h.run_current_path_at(Vec3::new(10.0, 0.0, 1.4), 1.0, true, 200);
        assert!(matches!(s, PathStep::Move { target, .. } if target == Vec3::new(20.0, 0.0, 0.0)));
    }

    #[test]
    fn the_arrival_radius_covers_the_movement_tolerance() {
        // `Npc.MoveTowards` stops within 1 m, so a smaller Template.Scale still has to count
        // that stop as an arrival (otherwise the NPC stays there forever).
        let mut h = two_points(0.0);
        h.run_current_path_at(Vec3::new(0.0, 0.0, 0.0), 0.1, false, 0);
        let s = h.run_current_path_at(Vec3::new(9.4, 0.0, 0.0), 0.1, false, 100);
        assert!(matches!(s, PathStep::Move { target, .. } if target == Vec3::new(20.0, 0.0, 0.0)));
    }

    #[test]
    fn a_target_without_progress_is_skipped_after_the_timeout() {
        let mut h = two_points(0.0);
        let s = h.run_current_path_at(Vec3::new(0.0, 0.0, 0.0), 1.0, false, 0);
        assert!(matches!(s, PathStep::Move { target, .. } if target == Vec3::new(10.0, 0.0, 0.0)));
        // Still in front of it just before the timeout.
        let s = h.run_current_path_at(
            Vec3::new(0.0, 0.0, 0.0),
            1.0,
            false,
            TARGET_STALL_TIMEOUT_MS - 1,
        );
        assert!(matches!(s, PathStep::Move { .. }));
        // No progress for the whole timeout: dropped, exactly one `Stalled` report.
        let s = h.run_current_path_at(
            Vec3::new(0.0, 0.0, 0.0),
            1.0,
            false,
            TARGET_STALL_TIMEOUT_MS,
        );
        assert_eq!(
            s,
            PathStep::Stalled {
                target: Vec3::new(10.0, 0.0, 0.0),
                path: "public_farm".into(),
                result: true,
            }
        );
        // The next call moves on to the next point instead of re-targeting the skipped one.
        let s = h.run_current_path_at(Vec3::new(0.0, 0.0, 0.0), 1.0, false, 5_100);
        assert!(matches!(s, PathStep::Move { target, .. } if target == Vec3::new(20.0, 0.0, 0.0)));
    }

    #[test]
    fn re_entering_follow_path_restarts_the_stall_clock() {
        let mut h = two_points(0.0);
        h.run_current_path_at(Vec3::new(0.0, 0.0, 0.0), 1.0, false, 0);
        // A combat interruption lasts longer than the timeout; resuming restarts the clock.
        h.reset_target_progress(Vec3::new(0.0, 0.0, 0.0), false, TARGET_STALL_TIMEOUT_MS + 1);
        let s = h.run_current_path_at(
            Vec3::new(0.0, 0.0, 0.0),
            1.0,
            false,
            TARGET_STALL_TIMEOUT_MS + 2_000,
        );
        assert!(matches!(s, PathStep::Move { .. }));
    }

    #[test]
    fn slow_steady_progress_is_never_skipped() {
        let mut h = two_points(0.0);
        h.run_current_path_at(Vec3::new(0.0, 0.0, 0.0), 1.0, false, 0);
        // 0.05 m per 100 ms (0.5 m/s): ~15 s to the point, far beyond the stall timeout.
        for i in 1..=150 {
            let x = i as f32 * 0.05;
            let s = h.run_current_path_at(Vec3::new(x, 0.0, 0.0), 1.0, false, i * 100);
            assert!(
                matches!(s, PathStep::Move { .. }),
                "skipped while progressing at step {i}"
            );
        }
        let s = h.run_current_path_at(Vec3::new(10.0, 0.0, 0.0), 1.0, false, 15_200);
        assert!(matches!(s, PathStep::Move { target, .. } if target == Vec3::new(20.0, 0.0, 0.0)));
    }

    fn route(points: &[(f32, f32)]) -> Vec<AiPathPoint> {
        points
            .iter()
            .map(|(x, y)| AiPathPoint {
                position: Vec3::new(*x, *y, 0.0),
                action: AiPathPointAction::None,
                param: String::new(),
            })
            .collect()
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("aaemu_paths_{tag}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn plain_file_wins_over_variants() {
        let dir = TempDir::new("plain");
        std::fs::write(dir.0.join("walk.path"), "|1|2|3|\n|4|5|6|\n").unwrap();
        std::fs::write(dir.0.join("walk@7.path"), "|100|100|0|\n|101|100|0|\n").unwrap();
        let m = AiPathsManager::default();
        m.set_folder(&dir.0);
        let points = m.load_ai_path_points_near("walk", Some(Vec3::new(100.0, 100.0, 0.0)));
        assert_eq!(points[0].position, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(
            m.resolve_path_file("walk", None),
            Some(dir.0.join("walk.path"))
        );
    }

    #[test]
    fn variants_resolve_to_the_route_nearest_the_npc() {
        let dir = TempDir::new("variants");
        std::fs::write(
            dir.0.join("farm@156.path"),
            "|5000|5000|0|\n|5010|5000|0|\n",
        )
        .unwrap();
        std::fs::write(dir.0.join("farm@142.path"), "|100|100|0|\n|140|100|0|\n").unwrap();
        std::fs::write(dir.0.join("solo@9.path"), "|1|1|0|\n|2|1|0|\n").unwrap();
        let m = AiPathsManager::default();
        m.set_folder(&dir.0);
        // Nearest segment, not just the nearest point: 120,104 is beside the middle of farm@142.
        let near = Some(Vec3::new(120.0, 104.0, 9.0));
        assert_eq!(m.resolve_stem("farm", near).as_deref(), Some("farm@142"));
        let far = Some(Vec3::new(5004.0, 4990.0, 0.0));
        assert_eq!(m.resolve_stem("farm", far).as_deref(), Some("farm@156"));
        assert_eq!(
            m.load_ai_path_points_near("farm", far)[0].position,
            Vec3::new(5000.0, 5000.0, 0.0)
        );
        // Several variants and no position: no guess. A single variant needs no position.
        assert_eq!(m.resolve_stem("farm", None), None);
        assert_eq!(m.resolve_stem("solo", None).as_deref(), Some("solo@9"));
        assert_eq!(m.resolve_stem("nothing", near), None);
        assert!(m.load_ai_path_points_near("nothing", near).is_empty());
    }

    #[test]
    fn inserted_routes_resolve_without_a_folder() {
        let m = AiPathsManager::default();
        m.insert("test_path", route(&[(0.0, 0.0), (1.0, 0.0)]));
        assert_eq!(
            m.resolve_stem("test_path", None).as_deref(),
            Some("test_path")
        );
        assert_eq!(m.resolve_path_file("test_path", None), None);
    }
}
