// SPDX-License-Identifier: GPL-3.0-or-later

//! Caps how much memory the wasm32 RAW decodes use at once. Every decode
//! thread shares one heap of at most 4 GB that never shrinks, and a 45 MP
//! RAW needs most of a gigabyte of scratch at sensor size. Six threads each
//! decoding a big RAW ran the heap out, and an allocation failure aborts its
//! thread with everything it held still allocated, so the heap never got that
//! memory back and the next decodes failed too.
//!
//! A decode asks for its estimate before it allocates and waits while the
//! other decodes hold too much. One decode bigger than the whole budget still
//! runs, alone.
//!
//! Compiled on every target so the tests run natively. Only wasm32 calls it.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::cell::Cell;
use std::sync::{Condvar, Mutex};

/// Bytes the decodes may hold between them. Leaves the rest of the 4 GB heap
/// for the loader's caches, the files' bytes and the UI.
pub(crate) const BUDGET: usize = 1536 << 20;

pub(crate) struct DecodeBudget {
    limit: usize,
    used: Mutex<usize>,
    freed: Condvar,
}

thread_local! {
    /// What this thread holds, so a thread that dies mid-decode can give it
    /// back from the panic hook. See [`release_held_by_this_thread`].
    static HELD: Cell<usize> = const { Cell::new(0) };
}

static GLOBAL: DecodeBudget = DecodeBudget::new(BUDGET);

impl DecodeBudget {
    pub(crate) const fn new(limit: usize) -> Self {
        Self {
            limit,
            used: Mutex::new(0),
            freed: Condvar::new(),
        }
    }

    /// Blocks until `bytes` fits beside what the other decodes hold. A request
    /// over the whole limit waits for the rest to finish, then runs alone.
    pub(crate) fn acquire(&'static self, bytes: usize) -> Grant {
        let bytes = bytes.min(self.limit);
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        while *used > 0 && *used + bytes > self.limit {
            used = self.freed.wait(used).unwrap_or_else(|e| e.into_inner());
        }
        *used += bytes;
        HELD.with(|held| held.set(held.get() + bytes));
        Grant {
            budget: self,
            bytes,
        }
    }

    fn release(&self, bytes: usize) {
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        *used = used.saturating_sub(bytes);
        drop(used);
        self.freed.notify_all();
    }
}

/// Memory held against a [`DecodeBudget`], given back on drop.
pub(crate) struct Grant {
    budget: &'static DecodeBudget,
    bytes: usize,
}

impl Grant {
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        HELD.with(|held| held.set(held.get().saturating_sub(self.bytes)));
        self.budget.release(self.bytes);
    }
}

/// Waits for `bytes` of the process-wide budget.
pub(crate) fn acquire(bytes: usize) -> Grant {
    GLOBAL.acquire(bytes)
}

/// Gives back whatever this thread holds. wasm32 aborts on a panic or a
/// failed allocation, so a `Grant` on the dying thread never drops; the panic
/// hook calls this instead, or every later decode would wait forever.
pub(crate) fn release_held_by_this_thread() {
    let bytes = HELD.with(|held| held.replace(0));
    if bytes > 0 {
        GLOBAL.release(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn a_decode_that_does_not_fit_waits_for_the_one_holding_the_budget() {
        static BUDGET: DecodeBudget = DecodeBudget::new(100);
        let first = BUDGET.acquire(70);
        let started = Arc::new(AtomicBool::new(false));
        let waiter = {
            let started = Arc::clone(&started);
            std::thread::spawn(move || {
                let _second = BUDGET.acquire(70);
                started.store(true, Ordering::SeqCst);
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !started.load(Ordering::SeqCst),
            "second decode ran over the budget"
        );
        drop(first);
        waiter.join().unwrap();
        assert!(started.load(Ordering::SeqCst));
    }

    #[test]
    fn a_decode_bigger_than_the_whole_budget_still_runs_alone() {
        static BUDGET: DecodeBudget = DecodeBudget::new(100);
        let grant = BUDGET.acquire(1_000);
        assert_eq!(grant.bytes, 100);
    }

    #[test]
    fn decodes_that_fit_together_run_together() {
        static BUDGET: DecodeBudget = DecodeBudget::new(100);
        let _a = BUDGET.acquire(40);
        let _b = BUDGET.acquire(40);
        assert_eq!(*BUDGET.used.lock().unwrap(), 80);
    }

    #[test]
    fn a_dead_threads_share_is_given_back() {
        let worker = std::thread::spawn(|| {
            let grant = acquire(BUDGET / 2);
            // A wasm32 abort never runs the destructor.
            std::mem::forget(grant);
            release_held_by_this_thread();
        });
        worker.join().unwrap();
        let _all = acquire(BUDGET);
    }
}
