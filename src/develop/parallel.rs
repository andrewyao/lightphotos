// SPDX-License-Identifier: MIT OR Apache-2.0

//! Spreads a per-pixel pass over threads, a band of whole rows at a time. A
//! 24 MP export bake took 1.9 s on one native thread and 6 s in the browser.

use std::sync::{Mutex, PoisonError};

/// Rows per band. Small enough that a slow band, such as the one a crop or
/// a spot lands in, doesn't hold the others up; large enough that taking one
/// costs nothing next to its pixels.
const BAND_ROWS: usize = 16;

/// Call `row(y, out_row)` for every row of `out`, `row_len` elements each.
/// Rows run in any order and on any thread, so `row` must depend only on
/// `y`. The calling thread works too, so every row runs even when no other
/// thread can start.
pub fn for_each_row<T: Send>(out: &mut [T], row_len: usize, row: impl Fn(usize, &mut [T]) + Sync) {
    for_each_chunk(out, BAND_ROWS.saturating_mul(row_len), |band, chunk| {
        for (i, out_row) in chunk.chunks_mut(row_len).enumerate() {
            row(band * BAND_ROWS + i, out_row);
        }
    });
}

/// Call `chunk(i, out_chunk)` for each `chunk_len`-element chunk of `out`,
/// the last one possibly shorter, for a pass that sizes its own bands. The
/// same rules as [`for_each_row`]: any order, any thread, and the calling
/// thread works too.
pub fn for_each_chunk<T: Send>(
    out: &mut [T],
    chunk_len: usize,
    chunk: impl Fn(usize, &mut [T]) + Sync,
) {
    if chunk_len == 0 {
        return;
    }
    let chunks = out.len().div_ceil(chunk_len);
    let next = Mutex::new(out.chunks_mut(chunk_len).enumerate());
    let work = || loop {
        let taken = next.lock().unwrap_or_else(PoisonError::into_inner).next();
        let Some((i, out_chunk)) = taken else {
            return;
        };
        chunk(i, out_chunk);
    };
    run_with_helpers(chunks.saturating_sub(1), &work);
}

/// Run `work` on this thread and on up to `helpers` others at once.
#[cfg(not(target_arch = "wasm32"))]
fn run_with_helpers(helpers: usize, work: &(impl Fn() + Sync)) {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let helpers = helpers.min(cores.saturating_sub(1));
    std::thread::scope(|s| {
        for _ in 0..helpers {
            // A thread that can't start leaves its bands to the others.
            let _ = std::thread::Builder::new().spawn_scoped(s, work);
        }
        work();
    });
}

/// Run `work` on this thread and on up to `helpers` threads of a pool kept
/// for the purpose. Starting a Web Worker costs far more than a native
/// thread, so the pool's threads live for the session. The browser's main
/// thread may not block, so there `work` runs alone.
///
/// Under `panic = "abort"` a panic on a pool thread ends that thread and
/// this call waits forever, so `work` must not panic, which the crate's
/// clippy lints already demand of non-test code.
#[cfg(target_arch = "wasm32")]
fn run_with_helpers(helpers: usize, work: &(impl Fn() + Sync)) {
    let pool = (web_sys::window().is_none() && helpers > 0)
        .then(pool)
        .flatten();
    let Some(pool) = pool else {
        work();
        return;
    };
    let helpers = helpers.min(pool.current_num_threads());
    pool.in_place_scope(|s| {
        for _ in 0..helpers {
            s.spawn(|_| work());
        }
        work();
    });
}

/// Starts the pool's threads now, from the main thread, so the first export
/// doesn't wait on them and no Web Worker has to start another.
#[cfg(target_arch = "wasm32")]
pub fn warm() {
    let _ = pool();
}

#[cfg(target_arch = "wasm32")]
fn pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let cores = wasm_thread::available_parallelism().map_or(1, |n| n.get());
        rayon::ThreadPoolBuilder::new()
            .num_threads(cores.saturating_sub(2).clamp(1, 8))
            .spawn_handler(|thread| {
                wasm_thread::Builder::new()
                    .name(format!("band-worker-{}", thread.index()))
                    .spawn(move || thread.run())
                    .map(drop)
            })
            .build()
            .ok()
    })
    .as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_runs_once_with_its_own_index() {
        for (rows, row_len) in [(0, 3), (1, 3), (15, 2), (16, 2), (17, 2), (1000, 5)] {
            let mut out = vec![usize::MAX; rows * row_len];
            for_each_row(&mut out, row_len, |y, out_row| {
                assert_eq!(out_row.len(), row_len);
                for (x, v) in out_row.iter_mut().enumerate() {
                    assert_eq!(*v, usize::MAX, "row {y} written twice");
                    *v = y * row_len + x;
                }
            });
            assert!(out.iter().enumerate().all(|(i, &v)| i == v), "{rows} rows");
        }
    }

    #[test]
    fn every_chunk_runs_once_with_its_own_index_and_the_last_is_short() {
        let mut out = vec![usize::MAX; 10];
        for_each_chunk(&mut out, 4, |i, chunk| {
            assert_eq!(chunk.len(), if i == 2 { 2 } else { 4 });
            for (k, v) in chunk.iter_mut().enumerate() {
                assert_eq!(*v, usize::MAX, "chunk {i} written twice");
                *v = i * 4 + k;
            }
        });
        assert!(out.iter().enumerate().all(|(i, &v)| i == v));
    }

    #[test]
    fn a_zero_row_length_writes_nothing() {
        let mut out = vec![7u8; 4];
        for_each_row(&mut out, 0, |_, _| panic!("no rows to run"));
        assert_eq!(out, [7; 4]);
    }
}
