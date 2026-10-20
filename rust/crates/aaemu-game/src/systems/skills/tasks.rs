//! Timers (`TaskManager.Schedule`) and the `SkillTlIdManager`.
//!
//! Tasks run on Tokio with an `Arc<GameServer>` obtained from the weak reference the skills
//! subsystem stores at start ([`super::System::bind_server`]); without one (unit tests that never
//! bind a server) scheduling is a logged no-op.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::server::GameServer;

/// Runs `f` after `delay_ms` (`TaskManager.Schedule(task, delay)`). Negative delays run
/// immediately (the next scheduler turn).
pub fn schedule(
    server: &GameServer,
    delay_ms: i64,
    f: impl FnOnce(Arc<GameServer>) + Send + 'static,
) {
    let Some(srv) = server.systems.skills.server() else {
        tracing::debug!("skills: no bound server, task not scheduled");
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::debug!("skills: no Tokio runtime, task not scheduled");
        return;
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
    next: u16,
    total_gets: u64,
    total_releases: u64,
    unused_releases: u64,
}

const FIRST_ID: u16 = 0x000A;
const LAST_ID: u16 = 0xFFFE;

impl Default for TlIdManager {
    fn default() -> Self {
        Self {
            inner: Mutex::new(TlIdInner {
                assigned: vec![false; u16::MAX as usize],
                owners: vec![(0, 0); u16::MAX as usize],
                next: FIRST_ID,
                total_gets: 0,
                total_releases: 0,
                unused_releases: 0,
            }),
        }
    }
}

impl TlIdManager {
    /// `GetNextId(referenceUnit)` (0 when exhausted, with a fatal log). `owner` (the caster obj
    /// id) and `skill_id` are remembered for the unused-release warning.
    pub fn next_id(&self, owner: u32, skill_id: u32) -> u16 {
        let mut s = self.inner.lock().expect("tl ids poisoned");
        s.total_gets += 1;
        let mut res = 0u16;
        for _ in 0..=(LAST_ID - FIRST_ID) {
            let id = s.next;
            if !s.assigned[id as usize] {
                s.assigned[id as usize] = true;
                s.owners[id as usize] = (owner, skill_id);
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
            tracing::error!(
                "No more Ids left to use with a total Gets of {} and Release of {} !!!",
                s.total_gets,
                s.total_releases
            );
        }
        res
    }

    /// `ReleaseId(tlId)`.
    pub fn release(&self, tl: u16) {
        if tl == 0 {
            return;
        }
        let mut s = self.inner.lock().expect("tl ids poisoned");
        s.total_releases += 1;
        if !s.assigned[tl as usize] {
            s.unused_releases += 1;
            let (owner, skill_id) = s.owners[tl as usize];
            tracing::warn!(
                "Tried to release unused Id {tl}, last owner obj id {owner}, skill id {skill_id}"
            );
        }
        s.assigned[tl as usize] = false;
    }

    /// Releases of an id that was not assigned (the warning's count).
    pub fn unused_releases(&self) -> u64 {
        self.inner.lock().expect("tl ids poisoned").unused_releases
    }

    pub fn is_assigned(&self, tl: u16) -> bool {
        self.inner.lock().expect("tl ids poisoned").assigned[tl as usize]
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
}
