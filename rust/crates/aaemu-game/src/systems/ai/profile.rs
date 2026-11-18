//! Opt-in fine-grained wall-time profile of the AI tick.
//!
//! Enabled with `AAEMU_AI_PROFILE=1` (read once). Off by default: every scope then costs one
//! branch and no clock read. The counters are cumulative process-wide nanosecond totals, grouped
//! by stage and by behavior kind; the AI manager logs the deltas of a window (see
//! `manager::log_profile_window`). The scopes nest (an aggression scan contains `Around` and
//! `CanSee`), so the step totals overlap; they are meant to attribute cost, not to sum to the
//! tick time.

#[cfg(not(test))]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
#[cfg(not(test))]
use std::sync::OnceLock;
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

#[cfg(not(test))]
static STEP_NS: [AtomicU64; STEPS] = [const { AtomicU64::new(0) }; STEPS];
#[cfg(not(test))]
static STEP_CALLS: [AtomicU64; STEPS] = [const { AtomicU64::new(0) }; STEPS];
#[cfg(not(test))]
static BEHAVIOR_NS: [AtomicU64; BEHAVIORS] = [const { AtomicU64::new(0) }; BEHAVIORS];
#[cfg(not(test))]
static BEHAVIOR_CALLS: [AtomicU64; BEHAVIORS] = [const { AtomicU64::new(0) }; BEHAVIORS];

/// Whether the profile is enabled (`AAEMU_AI_PROFILE`, not `0` / empty).
#[cfg(not(test))]
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var("AAEMU_AI_PROFILE").is_ok_and(|v| !v.is_empty() && v != "0"))
}

// Tests opt in per thread and see their own counters.
#[cfg(test)]
thread_local! {
    static TEST_COUNTERS: std::cell::RefCell<Option<Snapshot>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub fn enabled() -> bool {
    TEST_COUNTERS.with(|counters| counters.borrow().is_some())
}

#[cfg(test)]
pub(crate) fn force_enabled(on: bool) {
    TEST_COUNTERS.with(|counters| *counters.borrow_mut() = on.then(Snapshot::default));
}

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
            let ns = started.elapsed().as_nanos() as u64;
            #[cfg(not(test))]
            {
                STEP_NS[self.step].fetch_add(ns, Ordering::Relaxed);
                STEP_CALLS[self.step].fetch_add(1, Ordering::Relaxed);
            }
            #[cfg(test)]
            TEST_COUNTERS.with(|counters| {
                if let Some(snapshot) = counters.borrow_mut().as_mut() {
                    snapshot.step_ns[self.step] += ns;
                    snapshot.step_calls[self.step] += 1;
                }
            });
        }
    }
}

/// Records one behavior tick by kind (the caller already timed it).
pub fn behavior_tick(kind: usize, ns: u64) {
    if !enabled() || kind >= BEHAVIORS {
        return;
    }
    #[cfg(not(test))]
    {
        BEHAVIOR_NS[kind].fetch_add(ns, Ordering::Relaxed);
        BEHAVIOR_CALLS[kind].fetch_add(1, Ordering::Relaxed);
    }
    #[cfg(test)]
    TEST_COUNTERS.with(|counters| {
        if let Some(snapshot) = counters.borrow_mut().as_mut() {
            snapshot.behavior_ns[kind] += ns;
            snapshot.behavior_calls[kind] += 1;
        }
    });
}

/// One window of counters.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub step_calls: [u64; STEPS],
    pub step_ns: [u64; STEPS],
    pub behavior_calls: [u64; BEHAVIORS],
    pub behavior_ns: [u64; BEHAVIORS],
}

/// The counters used to calculate deltas across a profile window.
#[cfg(not(test))]
pub fn snapshot() -> Snapshot {
    Snapshot {
        step_calls: std::array::from_fn(|i| STEP_CALLS[i].load(Ordering::Relaxed)),
        step_ns: std::array::from_fn(|i| STEP_NS[i].load(Ordering::Relaxed)),
        behavior_calls: std::array::from_fn(|i| BEHAVIOR_CALLS[i].load(Ordering::Relaxed)),
        behavior_ns: std::array::from_fn(|i| BEHAVIOR_NS[i].load(Ordering::Relaxed)),
    }
}

#[cfg(test)]
pub fn snapshot() -> Snapshot {
    TEST_COUNTERS.with(|counters| counters.borrow().clone().unwrap_or_default())
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

/// Keeps a per-NPC tick that cost more than [`SLOW_NPC_US`] for the window's report, retaining
/// the slowest [`MAX_SLOW_NPCS`]: once full, a slower sample replaces the fastest kept sample.
fn keep_slowest(slow: &mut Vec<SlowNpc>, sample: SlowNpc) {
    if slow.len() < MAX_SLOW_NPCS {
        slow.push(sample);
        return;
    }
    let min = slow
        .iter()
        .enumerate()
        .min_by_key(|(_, kept)| kept.0)
        .map(|(index, _)| index);
    if let Some(index) = min {
        if sample.0 > slow[index].0 {
            slow[index] = sample;
        }
    }
}

/// Keeps a per-NPC tick that cost more than [`SLOW_NPC_US`] for the window's report.
pub fn record_slow_npc(us: u64, obj_id: u32, behavior: Option<usize>) {
    if !enabled() || us < SLOW_NPC_US {
        return;
    }
    let mut slow = SLOW_NPCS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    keep_slowest(&mut slow, (us, obj_id, behavior));
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

    /// Once the window holds [`MAX_SLOW_NPCS`] samples, a slower tick must still make it in:
    /// the first 64 qualifying ticks are not the slowest 64.
    #[test]
    fn slow_npc_samples_keep_the_largest_when_full() {
        let mut kept = Vec::new();
        for i in 0..MAX_SLOW_NPCS {
            keep_slowest(&mut kept, (SLOW_NPC_US + i as u64, i as u32, None));
        }
        keep_slowest(
            &mut kept,
            (SLOW_NPC_US + 10_000, MAX_SLOW_NPCS as u32, Some(1)),
        );
        assert_eq!(
            kept.len(),
            MAX_SLOW_NPCS,
            "the retained window stays capped"
        );
        assert!(
            kept.contains(&(SLOW_NPC_US + 10_000, MAX_SLOW_NPCS as u32, Some(1))),
            "a slower tick replaces the fastest kept sample"
        );
        assert!(
            !kept.contains(&(SLOW_NPC_US, 0, None)),
            "the smallest sample made room"
        );
    }
}
