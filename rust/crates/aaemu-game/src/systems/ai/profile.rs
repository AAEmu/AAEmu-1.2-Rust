//! Opt-in fine-grained wall-time profile of the AI tick.
//!
//! Enabled with `AAEMU_AI_PROFILE=1` (read once). Off by default: every scope then costs one
//! branch and no clock read. The counters are cumulative process-wide nanosecond totals, grouped
//! by stage and by behavior kind; the AI manager logs the deltas of a window (see
//! `manager::log_profile_window`). The scopes nest (an aggression scan contains `Around` and
//! `CanSee`), so the step totals overlap; they are meant to attribute cost, not to sum to the
//! tick time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// One timed stage of the tick. `Count` is the number of stages, not a stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// `behavior.Tick(delta)` (excludes the aggro-table cleanup).
    Behavior,
    /// The stale-aggro cleanup after the behavior tick.
    AggroCleanup,
    /// `Behavior.CheckAggression` (includes the `Around` scan it makes).
    Aggression,
    /// `Behavior.CheckAlert` (includes the `Around` scan it makes).
    Alert,
    /// `CheckForTalk` of Idle / HoldPosition.
    Talk,
    /// Every `npc::around` (`WorldManager.GetAround`) call.
    Around,
    /// `units_by_distance` (distance of every unit + sort).
    TargetSearch,
    /// `BaseUnit.CanSeeTarget` (visibility + stealth detection).
    CanSee,
    /// `group::follower_tick`.
    Follower,
    /// The manager's `refresh_woken` after a handle ticked.
    WokenRefresh,
    Count,
}

pub const STEPS: usize = Step::Count as usize;

impl Step {
    /// Every stage, in declaration order.
    pub const ALL: [Self; STEPS] = [
        Self::Behavior,
        Self::AggroCleanup,
        Self::Aggression,
        Self::Alert,
        Self::Talk,
        Self::Around,
        Self::TargetSearch,
        Self::CanSee,
        Self::Follower,
        Self::WokenRefresh,
    ];
}

/// `BehaviorKind::COUNT` (kept here so the arrays are sized without importing the framework).
pub const BEHAVIORS: usize = super::framework::BehaviorKind::COUNT;

static STEP_NS: [AtomicU64; STEPS] = [const { AtomicU64::new(0) }; STEPS];
static STEP_CALLS: [AtomicU64; STEPS] = [const { AtomicU64::new(0) }; STEPS];
static BEHAVIOR_NS: [AtomicU64; BEHAVIORS] = [const { AtomicU64::new(0) }; BEHAVIORS];
static BEHAVIOR_CALLS: [AtomicU64; BEHAVIORS] = [const { AtomicU64::new(0) }; BEHAVIORS];

/// Whether the profile is enabled (`AAEMU_AI_PROFILE`, not `0` / empty).
pub fn enabled() -> bool {
    *ENABLED
        .get_or_init(|| std::env::var("AAEMU_AI_PROFILE").is_ok_and(|v| !v.is_empty() && v != "0"))
}

static ENABLED: OnceLock<bool> = OnceLock::new();

/// A timed scope; does nothing (and reads no clock) while the profile is disabled.
pub struct Scope {
    step: usize,
    started: Option<Instant>,
}

/// Starts timing `step`.
pub fn step(step: Step) -> Scope {
    if !enabled() {
        return Scope {
            step: step as usize,
            started: None,
        };
    }
    Scope {
        step: step as usize,
        started: Some(Instant::now()),
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            STEP_NS[self.step].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
            STEP_CALLS[self.step].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Records one behavior tick by kind (the caller already timed it).
pub fn behavior_tick(kind: usize, ns: u64) {
    if !enabled() || kind >= BEHAVIORS {
        return;
    }
    BEHAVIOR_NS[kind].fetch_add(ns, Ordering::Relaxed);
    BEHAVIOR_CALLS[kind].fetch_add(1, Ordering::Relaxed);
}

/// One window of counters.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub step_calls: [u64; STEPS],
    pub step_ns: [u64; STEPS],
    pub behavior_calls: [u64; BEHAVIORS],
    pub behavior_ns: [u64; BEHAVIORS],
}

/// The process-wide counters, for the deltas of a profile window.
pub fn snapshot() -> Snapshot {
    Snapshot {
        step_calls: std::array::from_fn(|i| STEP_CALLS[i].load(Ordering::Relaxed)),
        step_ns: std::array::from_fn(|i| STEP_NS[i].load(Ordering::Relaxed)),
        behavior_calls: std::array::from_fn(|i| BEHAVIOR_CALLS[i].load(Ordering::Relaxed)),
        behavior_ns: std::array::from_fn(|i| BEHAVIOR_NS[i].load(Ordering::Relaxed)),
    }
}

/// Subtracts an earlier snapshot from a later one (saturating per counter).
pub fn delta(later: &Snapshot, earlier: &Snapshot) -> Snapshot {
    Snapshot {
        step_calls: std::array::from_fn(|i| {
            later.step_calls[i].saturating_sub(earlier.step_calls[i])
        }),
        step_ns: std::array::from_fn(|i| later.step_ns[i].saturating_sub(earlier.step_ns[i])),
        behavior_calls: std::array::from_fn(|i| {
            later.behavior_calls[i].saturating_sub(earlier.behavior_calls[i])
        }),
        behavior_ns: std::array::from_fn(|i| {
            later.behavior_ns[i].saturating_sub(earlier.behavior_ns[i])
        }),
    }
}

/// Nominal name of a stage (the log line).
pub fn step_name(step: Step) -> &'static str {
    match step {
        Step::Behavior => "behavior",
        Step::AggroCleanup => "aggro-cleanup",
        Step::Aggression => "aggression",
        Step::Alert => "alert",
        Step::Talk => "talk",
        Step::Around => "around",
        Step::TargetSearch => "target-search",
        Step::CanSee => "can-see",
        Step::Follower => "follower",
        Step::WokenRefresh => "woken-refresh",
        Step::Count => "?",
    }
}

/// The NPC ticks slower than this are kept for the window's "slowest NPCs" line, µs.
const SLOW_NPC_US: u64 = 2_000;
/// Upper bound of the retained slow NPC ticks per window.
const MAX_SLOW_NPCS: usize = 64;

/// One recorded slow NPC tick: `(µs, obj_id, behavior)`.
pub type SlowNpc = (u64, u32, Option<usize>);

static SLOW_NPCS: Mutex<Vec<SlowNpc>> = Mutex::new(Vec::new());

/// Keeps a per-NPC tick that cost more than [`SLOW_NPC_US`] for the window's report.
pub fn record_slow_npc(us: u64, obj_id: u32, behavior: Option<usize>) {
    if !enabled() || us < SLOW_NPC_US {
        return;
    }
    let mut slow = SLOW_NPCS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slow.len() >= MAX_SLOW_NPCS {
        return;
    }
    slow.push((us, obj_id, behavior));
}

/// Takes the window's slow NPC ticks, sorted slowest first (at most `top`).
pub fn take_slow_npcs(top: usize) -> Vec<SlowNpc> {
    let mut slow = SLOW_NPCS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut taken: Vec<SlowNpc> = slow.drain(..).collect();
    taken.sort_unstable_by_key(|s| std::cmp::Reverse(s.0));
    taken.truncate(top);
    taken
}

#[cfg(test)]
mod tests {
    use crate::systems::ai::framework::BehaviorKind;

    use super::*;

    #[test]
    fn behavior_indices_fit_the_profile_arrays() {
        assert_eq!(BehaviorKind::ALL.len(), BehaviorKind::COUNT);
        assert_eq!(BehaviorKind::COUNT, BEHAVIORS);
        for (i, kind) in BehaviorKind::ALL.iter().enumerate() {
            assert_eq!(kind.index(), i, "{kind:?}");
        }
    }
}
