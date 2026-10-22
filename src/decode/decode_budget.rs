// SPDX-License-Identifier: MIT OR Apache-2.0

//! Caps how much memory the wasm32 decodes use at once. Every decode
//! thread shares one heap of at most 4 GB that never shrinks, and a 45 MP
//! RAW needs most of a gigabyte of scratch at sensor size. Six threads each
//! decoding a big RAW ran the heap out, and an allocation failure aborts its
//! thread with everything it held still allocated, so the heap never got that
//! memory back and the next decodes failed too.
//!
//! A decode asks for its estimate before it allocates and waits while the
//! other decodes hold too much. One decode bigger than the whole budget still
//! runs, alone. Requests are served in order, so a stream of small thumbnail
//! grants cannot keep a large Loupe decode waiting for an empty budget
//! forever.
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
    state: Mutex<State>,
    freed: Condvar,
}

/// Bytes granted, and a ticket line: each request takes the next ticket and
/// is admitted only when its ticket is `serving` and its bytes fit.
struct State {
    used: usize,
    next_ticket: u64,
    serving: u64,
    /// A held grant is waiting to grow (see `Grant::grow`). New requests
    /// wait behind it, since it already holds memory and is mid-decode.
    growing: bool,
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
            state: Mutex::new(State {
                used: 0,
                next_ticket: 0,
                serving: 0,
                growing: false,
            }),
            freed: Condvar::new(),
        }
    }

    /// Blocks until every earlier request has been granted and `bytes` fits
    /// beside what the other decodes hold. A request over the whole limit
    /// waits for the rest to finish, then runs alone, and the requests after
    /// it wait behind it rather than slipping past.
    pub(crate) fn acquire(&'static self, bytes: usize) -> Grant {
        let bytes = bytes.min(self.limit);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        while state.serving != ticket
            || state.growing
            || (state.used > 0 && state.used.saturating_add(bytes) > self.limit)
        {
            state = self.freed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.used += bytes;
        state.serving = state.serving.wrapping_add(1);
        drop(state);
        // The next ticket may fit beside this one.
        self.freed.notify_all();
        HELD.with(|held| held.set(held.get() + bytes));
        Grant {
            budget: self,
            bytes,
        }
    }

    /// `grant` grown to `bytes` (at most the limit). It keeps what it
    /// holds and waits only for the difference, ahead of every new request,
    /// until it fits or it is all that is held. One grant grows at a time:
    /// two that each waited for the other's memory would never wake, so a
    /// second gives its bytes back and queues for the whole amount instead.
    fn grow(&'static self, mut grant: Grant, bytes: usize) -> Grant {
        let target = bytes.min(self.limit);
        if target <= grant.bytes {
            return grant;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.growing {
            drop(state);
            drop(grant);
            return self.acquire(target);
        }
        state.growing = true;
        let more = target - grant.bytes;
        while state.used > grant.bytes && state.used.saturating_add(more) > self.limit {
            state = self.freed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.used += more;
        state.growing = false;
        drop(state);
        self.freed.notify_all();
        HELD.with(|held| held.set(held.get() + more));
        grant.bytes = target;
        grant
    }

    fn release(&self, bytes: usize) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.used = state.used.saturating_sub(bytes);
        drop(state);
        self.freed.notify_all();
    }
}

/// Memory held against a [`DecodeBudget`], given back on drop.
pub(crate) struct Grant {
    budget: &'static DecodeBudget,
    bytes: usize,
}

impl Grant {
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// This grant grown to `bytes`, keeping what it holds; see
    /// `DecodeBudget::grow`.
    pub(crate) fn grow(self, bytes: usize) -> Grant {
        let budget = self.budget;
        budget.grow(self, bytes)
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
pub fn release_held_by_this_thread() {
    let bytes = HELD.with(|held| held.replace(0));
    if bytes > 0 {
        GLOBAL.release(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
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
        assert_eq!(BUDGET.state.lock().unwrap().used, 80);
    }

    /// A request that is waiting for the whole budget holds back the ones
    /// after it, even those that would fit now, so streaming small grants
    /// cannot starve it.
    #[test]
    fn a_waiting_large_request_is_not_overtaken() {
        static BUDGET: DecodeBudget = DecodeBudget::new(100);
        let first = BUDGET.acquire(60);
        let order = Arc::new(Mutex::new(Vec::new()));
        let spawn = |name: &'static str, bytes: usize| {
            let order = Arc::clone(&order);
            std::thread::spawn(move || {
                let grant = BUDGET.acquire(bytes);
                order.lock().unwrap().push(name);
                std::thread::sleep(Duration::from_millis(20));
                drop(grant);
            })
        };
        let large = spawn("large", 1_000);
        while BUDGET.state.lock().unwrap().next_ticket < 2 {
            std::thread::yield_now();
        }
        let small = spawn("small", 10);
        std::thread::sleep(Duration::from_millis(50));
        assert!(order.lock().unwrap().is_empty(), "small slipped past");
        drop(first);
        large.join().unwrap();
        small.join().unwrap();
        assert_eq!(*order.lock().unwrap(), ["large", "small"]);
    }

    /// A decode that learns it needs more grows its grant ahead of a
    /// request queued after it, instead of giving its bytes back and
    /// queueing at the back of the line. The queued request doesn't fit
    /// beside the grown grant, so it can only be admitted once the grower
    /// lets go, and the order is fixed when the grow goes first.
    #[test]
    fn a_held_grant_grows_ahead_of_new_requests() {
        static BUDGET: DecodeBudget = DecodeBudget::new(100);
        let other = BUDGET.acquire(50);
        let first = BUDGET.acquire(30);
        let order = Arc::new(Mutex::new(Vec::new()));
        let queued = {
            let order = Arc::clone(&order);
            std::thread::spawn(move || {
                let _g = BUDGET.acquire(50);
                order.lock().unwrap().push("queued");
            })
        };
        while BUDGET.state.lock().unwrap().next_ticket < 3 {
            std::thread::yield_now();
        }
        let grower = {
            let order = Arc::clone(&order);
            std::thread::spawn(move || {
                let grown = first.grow(60);
                order.lock().unwrap().push("grown");
                assert_eq!(grown.bytes(), 60);
            })
        };
        // Free `other` only once the grower is waiting, or the queued
        // request would fit first and rightly go ahead.
        while !BUDGET.state.lock().unwrap().growing {
            std::thread::yield_now();
        }
        drop(other);
        grower.join().unwrap();
        queued.join().unwrap();
        assert_eq!(*order.lock().unwrap(), ["grown", "queued"]);
    }

    /// Two grants growing at once, each needing memory the other holds:
    /// the second gives its bytes back, so neither waits forever.
    #[test]
    fn two_growing_grants_do_not_wait_on_each_other() {
        static BUDGET: DecodeBudget = DecodeBudget::new(100);
        let a = BUDGET.acquire(60);
        let b = BUDGET.acquire(30);
        let ta = std::thread::spawn(move || drop(a.grow(80)));
        while !BUDGET.state.lock().unwrap().growing {
            std::thread::yield_now();
        }
        let tb = std::thread::spawn(move || drop(b.grow(80)));
        ta.join().unwrap();
        tb.join().unwrap();
        assert_eq!(BUDGET.state.lock().unwrap().used, 0);
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
