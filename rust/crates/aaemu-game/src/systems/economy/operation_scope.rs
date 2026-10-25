//! Synchronous operation boundary around callback-bearing world/item changes. The inner
//! economy ops mutex stays non-reentrant; only this outer scope permits same-thread entry.
//!
//! Acquire this scope before callback-bearing manager/publication locks. Financial capture
//! keeps its existing auction -> housing -> authority order inside the scope, followed by
//! ops -> object/container leaf locks (at most one object data guard). Mutation callbacks
//! run after ops/object guards drop and may then enter ItemTxn again on the same thread.
//! Snapshot-only manager locks which never wait for an item operation need no scope.
//! ItemTxn::unlocked remains guardless: callers under a character lock must not acquire
//! this scope to compensate.
//! Callback-bearing spawn chains enter scope -> spawn lock -> spawner/instance leaf locks
//! before item callbacks. The bare spawn lock stays available for pure bookkeeping; adding
//! an implicit scope there would invert callers which already hold character/object locks.

use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{Condvar, Mutex};
use std::thread::{self, ThreadId};

#[derive(Debug, Default)]
struct Owner {
    thread: Option<ThreadId>,
    depth: usize,
    #[cfg(test)]
    waiters: usize,
}

#[derive(Debug, Default)]
pub(super) struct OperationScope {
    owner: Mutex<Owner>,
    available: Condvar,
    #[cfg(test)]
    waiting: Condvar,
}

/// The Rc marker binds acquisition/release to one thread, without unsafe Send/Sync impls.
pub(crate) struct OperationScopeGuard<'a> {
    scope: &'a OperationScope,
    _thread_bound: PhantomData<Rc<()>>,
}

impl OperationScope {
    pub(super) fn enter(&self) -> OperationScopeGuard<'_> {
        let current = thread::current().id();
        let mut owner = self.owner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if owner.thread.is_none() || owner.thread == Some(current) {
                owner.thread = Some(current);
                owner.depth += 1;
                return OperationScopeGuard {
                    scope: self,
                    _thread_bound: PhantomData,
                };
            }
            #[cfg(test)]
            {
                owner.waiters += 1;
                self.waiting.notify_all();
            }
            owner = self
                .available
                .wait(owner)
                .unwrap_or_else(|e| e.into_inner());
            #[cfg(test)]
            {
                owner.waiters -= 1;
            }
        }
    }

    #[cfg(test)]
    pub(super) fn wait_until_blocked(&self) {
        let owner = self.owner.lock().unwrap_or_else(|e| e.into_inner());
        let (owner, timeout) = self
            .waiting
            .wait_timeout_while(owner, std::time::Duration::from_secs(5), |owner| {
                owner.waiters == 0
            })
            .unwrap_or_else(|e| e.into_inner());
        assert!(
            !timeout.timed_out() && owner.waiters != 0,
            "ordinary operation must reach the outer scope"
        );
    }
}

impl Drop for OperationScopeGuard<'_> {
    fn drop(&mut self) {
        let mut owner = self.scope.owner.lock().unwrap_or_else(|e| e.into_inner());
        debug_assert_eq!(owner.thread, Some(thread::current().id()));
        owner.depth -= 1;
        if owner.depth == 0 {
            owner.thread = None;
            self.scope.available.notify_one();
        }
    }
}
