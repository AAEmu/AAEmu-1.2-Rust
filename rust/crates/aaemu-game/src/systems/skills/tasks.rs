//! Timers (`TaskManager.Schedule`) and the `SkillTlIdManager`.
//!
//! Tasks run on Tokio with an `Arc<GameServer>` obtained from the weak reference the skills
//! subsystem stores at start ([`super::System::bind_server`]); without one (unit tests that never
//! bind a server) scheduling is a logged no-op. [`schedule`] reports whether the task was
//! actually spawned so callers can fall back instead of leaving a skill task slot (and its
//! timeline id) set forever.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::server::GameServer;

/// Runs `f` after `delay_ms` (`TaskManager.Schedule(task, delay)`). Negative delays run
/// immediately (the next scheduler turn). Returns `false` when no task was spawned (no bound
/// server or no Tokio runtime); the caller then owns the cleanup.
pub fn schedule(
    server: &GameServer,
    delay_ms: i64,
    f: impl FnOnce(Arc<GameServer>) + Send + 'static,
) -> bool {
    let Some(srv) = server.systems.skills.server() else {
        tracing::debug!("skills: no bound server, task not scheduled");
        return false;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::debug!("skills: no Tokio runtime, task not scheduled");
        return false;
    };
    let delay = Duration::from_millis(delay_ms.max(0) as u64);
    handle.spawn(async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        } else {
            tokio::task::yield_now().await;
        }
        f(srv);
    });
    true
}

/// An id held longer than this is treated as leaked (no 1.2 skill keeps a timeline beyond a
/// channel of a few minutes).
pub const LEAK_AGE_MS: u64 = 10 * 60 * 1000;

/// A snapshot of the manager's counters (logging / metrics / tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlIdStats {
    /// `LAST_ID - FIRST_ID + 1` (the usable ids).
    pub capacity: u32,
    /// Currently assigned ids (never returns to 0 when something leaks).
    pub in_use: u32,
    pub peak_in_use: u32,
    pub total_gets: u64,
    pub total_releases: u64,
    /// Releases of an id that was not assigned (double release).
    pub unused_releases: u64,
    /// `GetNextId` calls that found no free id (the manager's fallback path).
    pub exhaustions: u64,
    /// Timeline ids recovered because a reused skill still held them (a leak path was taken).
    pub replaced: u64,
}

/// `SkillTlIdManager` (ids `0x000A..=0xFFFE`, round robin, released explicitly).
#[derive(Debug)]
pub struct TlIdManager {
    inner: Mutex<TlIdInner>,
}

#[derive(Debug)]
struct TlIdInner {
    assigned: Vec<bool>,
    /// `AssignedUnit`: (caster obj id, skill id) that last took each id.
    owners: Vec<(u32, u32)>,
    /// Monotonic ms since the manager was created, per assigned id (leak diagnostics).
    assigned_at_ms: Vec<u64>,
    start: Instant,
    next: u16,
    in_use: u32,
    peak_in_use: u32,
    total_gets: u64,
    total_releases: u64,
    unused_releases: u64,
    exhaustions: u64,
    replaced: u64,
}

const FIRST_ID: u16 = 0x000A;
const LAST_ID: u16 = 0xFFFE;
/// The C# `MaxEntries` is `LastId - FirstId` (one less than the usable count) and its status
/// line is only cosmetic; the loop bounds keep the real number of usable ids.
const CAPACITY: u32 = (LAST_ID - FIRST_ID) as u32 + 1;

impl Default for TlIdManager {
    fn default() -> Self {
        Self {
            inner: Mutex::new(TlIdInner {
                assigned: vec![false; u16::MAX as usize],
                owners: vec![(0, 0); u16::MAX as usize],
                assigned_at_ms: vec![0; u16::MAX as usize],
                start: Instant::now(),
                next: FIRST_ID,
                in_use: 0,
                peak_in_use: 0,
                total_gets: 0,
                total_releases: 0,
                unused_releases: 0,
                exhaustions: 0,
                replaced: 0,
            }),
        }
    }
}

impl TlIdInner {
    fn elapsed_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn stats(&self) -> TlIdStats {
        TlIdStats {
            capacity: CAPACITY,
            in_use: self.in_use,
            peak_in_use: self.peak_in_use,
            total_gets: self.total_gets,
            total_releases: self.total_releases,
            unused_releases: self.unused_releases,
            exhaustions: self.exhaustions,
            replaced: self.replaced,
        }
    }

    /// Assigned ids older than `age_ms` (the leak metric: a timeline id of a finished skill
    /// should be back in the pool within seconds).
    fn leaked_over(&self, age_ms: u64) -> u32 {
        if self.in_use == 0 {
            return 0;
        }
        let now = self.elapsed_ms();
        (FIRST_ID..=LAST_ID)
            .filter(|&id| {
                self.assigned[id as usize]
                    && now.saturating_sub(self.assigned_at_ms[id as usize]) > age_ms
            })
            .count() as u32
    }

    fn status_line(&self) -> String {
        let stats = self.stats();
        format!(
            "{}/{} free TlId slots, in use {}, peak {}, Gets {}, Releases {}, unused releases {}, \
             exhaustions {}, replaced {}, leaked(>{LEAK_AGE_MS}ms) {}",
            CAPACITY - stats.in_use,
            CAPACITY,
            stats.in_use,
            stats.peak_in_use,
            stats.total_gets,
            stats.total_releases,
            stats.unused_releases,
            stats.exhaustions,
            stats.replaced,
            self.leaked_over(LEAK_AGE_MS),
        )
    }

    /// Up to 5 assigned owners, for the exhaustion / leak warnings.
    fn oldest_holders(&self) -> String {
        let mut held: Vec<(u64, u16, (u32, u32))> = (FIRST_ID..=LAST_ID)
            .filter(|&id| self.assigned[id as usize])
            .map(|id| {
                (
                    self.assigned_at_ms[id as usize],
                    id,
                    self.owners[id as usize],
                )
            })
            .collect();
        held.sort_unstable();
        held.truncate(5);
        held.iter()
            .map(|(at, id, (owner, skill))| {
                format!(
                    "id {id} held {}ms by obj {owner} skill {skill}",
                    self.elapsed_ms().saturating_sub(*at)
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

impl TlIdManager {
    /// `GetNextId(referenceUnit)` (0 when exhausted). `owner` (the caster obj id) and `skill_id`
    /// are remembered for the unused-release warning.
    ///
    /// Exhaustion never blocks or panics: it returns 0 with a throttled warning and the holder
    /// diagnostics, and the cast continues without a timeline id (callers treat 0 as "no
    /// timeline"). Leaks are fixed at their source (see the skills module); this counter makes a
    /// remaining one visible.
    pub fn next_id(&self, owner: u32, skill_id: u32) -> u16 {
        let mut s = self.inner.lock().expect("tl ids poisoned");
        s.total_gets += 1;
        let mut res = 0u16;
        for _ in 0..CAPACITY {
            let id = s.next;
            if !s.assigned[id as usize] {
                s.assigned[id as usize] = true;
                s.owners[id as usize] = (owner, skill_id);
                s.assigned_at_ms[id as usize] = s.elapsed_ms();
                s.in_use += 1;
                s.peak_in_use = s.peak_in_use.max(s.in_use);
                res = id;
            }
            s.next = s.next.wrapping_add(1);
            if s.next > LAST_ID {
                s.next = FIRST_ID;
            }
            if res != 0 {
                break;
            }
        }
        if res == 0 {
            s.exhaustions += 1;
            // The diagnostics scan all ids under the global lock, so they are only built when
            // the throttled warning is actually logged; the other failures cost two counters.
            if s.exhaustions == 1 || s.exhaustions.is_multiple_of(100) {
                tracing::warn!(
                    "No more TlIds left to use: {} (caster obj {owner}, skill {skill_id}); \
                     holders: {}",
                    s.status_line(),
                    s.oldest_holders()
                );
            } else {
                tracing::debug!(
                    "No more TlIds left to use: in use {}, exhaustions {} (caster obj {owner}, \
                     skill {skill_id})",
                    s.in_use,
                    s.exhaustions
                );
            }
        } else if s.total_gets.is_multiple_of(500) {
            // The C# printed `ReportStatus()` every 500 gets (debug).
            tracing::debug!("TlIdManager: {}", s.status_line());
        }
        res
    }

    /// `ReleaseId(tlId)`.
    pub fn release(&self, tl: u16) {
        if tl == 0 {
            return;
        }
        let mut s = self.inner.lock().expect("tl ids poisoned");
        if tl as usize >= s.assigned.len() {
            tracing::warn!("Tried to release out-of-range TlId {tl} (ignored)");
            return;
        }
        s.total_releases += 1;
        if !s.assigned[tl as usize] {
            s.unused_releases += 1;
            let (owner, skill_id) = s.owners[tl as usize];
            tracing::warn!(
                "Tried to release unused Id {tl}, last owner obj id {owner}, skill id {skill_id}"
            );
        } else {
            s.in_use = s.in_use.saturating_sub(1);
        }
        s.assigned[tl as usize] = false;
    }

    /// Releases a still-held id that a new use of the same `SkillRun` replaced. This is the
    /// safety net for the "reuse a skill that never released its timeline" leak class; the
    /// replacement count is part of the metrics (it must stay 0 in ordinary play).
    pub fn release_replaced(&self, tl: u16) {
        if tl == 0 {
            return;
        }
        let mut s = self.inner.lock().expect("tl ids poisoned");
        if tl as usize >= s.assigned.len() {
            tracing::warn!("Tried to replace out-of-range TlId {tl} (ignored)");
            return;
        }
        s.replaced += 1;
        let (owner, skill_id) = s.owners[tl as usize];
        let age = s.elapsed_ms().saturating_sub(s.assigned_at_ms[tl as usize]);
        tracing::warn!(
            "TlId {tl} was still held by obj {owner}, skill {skill_id} after {age}ms when the \
             skill was used again; released it"
        );
        s.total_releases += 1;
        if s.assigned[tl as usize] {
            s.in_use = s.in_use.saturating_sub(1);
        }
        s.assigned[tl as usize] = false;
    }

    /// Releases of an id that was not assigned (the warning's count).
    pub fn unused_releases(&self) -> u64 {
        self.inner.lock().expect("tl ids poisoned").unused_releases
    }

    /// Currently assigned ids (0 = the pool is fully released).
    pub fn in_use(&self) -> u32 {
        self.inner.lock().expect("tl ids poisoned").in_use
    }

    pub const fn capacity() -> u32 {
        CAPACITY
    }

    pub fn stats(&self) -> TlIdStats {
        self.inner.lock().expect("tl ids poisoned").stats()
    }

    /// Assigned ids older than `age_ms` (see [`LEAK_AGE_MS`]).
    pub fn leaked_over(&self, age_ms: u64) -> u32 {
        self.inner
            .lock()
            .expect("tl ids poisoned")
            .leaked_over(age_ms)
    }

    pub fn is_assigned(&self, tl: u16) -> bool {
        let s = self.inner.lock().expect("tl ids poisoned");
        (tl as usize) < s.assigned.len() && s.assigned[tl as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tl_ids_round_robin() {
        let m = TlIdManager::default();
        assert_eq!(m.next_id(1, 2), 0x0A);
        assert_eq!(m.next_id(1, 2), 0x0B);
        m.release(0x0A);
        assert!(!m.is_assigned(0x0A));
        // continues after the last handed out id, not at the released one
        assert_eq!(m.next_id(1, 2), 0x0C);
    }

    #[test]
    fn every_id_can_be_handed_out_and_returned() {
        let m = TlIdManager::default();
        let mut ids = Vec::with_capacity(TlIdManager::capacity() as usize);
        for _ in 0..TlIdManager::capacity() {
            let id = m.next_id(7, 9);
            assert_ne!(id, 0);
            ids.push(id);
        }
        assert_eq!(m.in_use(), TlIdManager::capacity());
        assert_eq!(m.stats().peak_in_use, TlIdManager::capacity());
        // The whole pool is handed out: the next get falls back to 0 instead of blocking.
        assert_eq!(m.next_id(7, 9), 0);
        assert_eq!(m.stats().exhaustions, 1);
        // A release puts an id back immediately, in round-robin order.
        let first = ids[0];
        m.release(first);
        assert_eq!(m.next_id(7, 9), first);
        for id in ids.iter().skip(1) {
            m.release(*id);
        }
        m.release(first);
        m.release(0); // a no-op that must not count as an unused release
        assert_eq!(m.in_use(), 0);
        assert_eq!(m.stats().unused_releases, 0);
        assert_eq!(m.leaked_over(0), 0);
    }

    #[test]
    fn out_of_range_release_does_not_panic() {
        let m = TlIdManager::default();
        m.release(0xFFFF);
        m.release_replaced(0xFFFF);
        assert!(!m.is_assigned(0xFFFF));
        assert_eq!(m.unused_releases(), 0);
    }

    #[test]
    fn long_held_ids_are_reported_as_leaks() {
        let m = TlIdManager::default();
        let id = m.next_id(11, 22);
        assert_eq!(m.leaked_over(10_000), 0, "freshly assigned is not a leak");
        assert_eq!(m.leaked_over(LEAK_AGE_MS), 0);
        // The metric only needs a short age in the test; the production threshold is 10 min.
        std::thread::sleep(Duration::from_millis(15));
        assert_eq!(m.leaked_over(10), 1);
        assert_eq!(m.leaked_over(60_000), 0);
        m.release(id);
        assert_eq!(m.leaked_over(10), 0);
        assert_eq!(m.in_use(), 0);
    }

    #[test]
    fn replaced_releases_are_counted_separately() {
        let m = TlIdManager::default();
        let id = m.next_id(1, 2);
        m.release_replaced(id);
        assert_eq!(m.stats().replaced, 1);
        assert_eq!(m.stats().unused_releases, 0);
        assert_eq!(m.in_use(), 0);
    }
}
