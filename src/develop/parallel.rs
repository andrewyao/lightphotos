// SPDX-License-Identifier: MIT OR Apache-2.0

//! Spreads a per-pixel pass over threads, a band of whole rows at a time. A
//! 24 MP export bake took 1.9 s on one native thread and 6 s in the browser.

use std::sync::{Mutex, PoisonError};

/// Rows per band. Small enough that a slow band, such as the one a crop or
/// a spot lands in, doesn't hold the others up; large enough that taking one
/// costs nothing next to its pixels.
const BAND_ROWS: usize = 16;

/// Call `row(y, out_row)` for every row of `out`, `row_len` elements each,
/// for a pass whose every pixel is costly (the bake, a RAW demosaic), so it
/// takes helper threads at any size: at 1024 px with four bakes at once,
/// running it on the calling thread alone doubled their wall time. Rows run
/// in any order and on any thread, so `row` must depend only on `y`. The
/// calling thread works too, so every row runs even when no other thread
/// can start.
pub fn for_each_row<T: Send>(out: &mut [T], row_len: usize, row: impl Fn(usize, &mut [T]) + Sync) {
    run_chunks(
        out,
        BAND_ROWS.saturating_mul(row_len),
        true,
        || (),
        |(), band, chunk| {
            for (i, out_row) in chunk.chunks_mut(row_len).enumerate() {
                row(band * BAND_ROWS + i, out_row);
            }
        },
    );
}

/// Below this many pixels a cheap pass runs on the calling thread, in order.
/// The thumbnail and scoring bakes (512 and 1024 px) run 2-4 at once on
/// their own workers, so helpers for their layer and copy passes only add
/// thread starts; an export's full-size bake is well past it.
pub const THREADS_FROM_PX: usize = 2_000_000;

/// Call `chunk(scratch, i, out_chunk)` for each `chunk_len`-element chunk of
/// `out`, the last one possibly shorter, for a cheap pass over `pixels`
/// pixels that sizes its own bands: helper threads from
/// [`THREADS_FROM_PX`] up, else every chunk here, in order. Otherwise the
/// rules of [`for_each_row`]. Each thread makes one `scratch` with `init`
/// and hands it to every chunk it runs.
pub fn for_each_chunk<T: Send, S>(
    out: &mut [T],
    chunk_len: usize,
    pixels: usize,
    init: impl Fn() -> S + Sync,
    chunk: impl Fn(&mut S, usize, &mut [T]) + Sync,
) {
    run_chunks(out, chunk_len, pixels >= THREADS_FROM_PX, init, chunk);
}

/// [`for_each_row`] for a cheap pass over `pixels` pixels, threaded only
/// from [`THREADS_FROM_PX`] up.
pub fn for_each_light_row<T: Send>(
    out: &mut [T],
    row_len: usize,
    pixels: usize,
    row: impl Fn(usize, &mut [T]) + Sync,
) {
    for_each_chunk(
        out,
        BAND_ROWS.saturating_mul(row_len),
        pixels,
        || (),
        |(), band, chunk| {
            for (i, out_row) in chunk.chunks_mut(row_len).enumerate() {
                row(band * BAND_ROWS + i, out_row);
            }
        },
    );
}

fn run_chunks<T: Send, S>(
    out: &mut [T],
    chunk_len: usize,
    threads: bool,
    init: impl Fn() -> S + Sync,
    chunk: impl Fn(&mut S, usize, &mut [T]) + Sync,
) {
    if chunk_len == 0 || out.is_empty() {
        return;
    }
    if !threads {
        let mut scratch = init();
        for (i, out_chunk) in out.chunks_mut(chunk_len).enumerate() {
            chunk(&mut scratch, i, out_chunk);
        }
        return;
    }
    let chunks = out.len().div_ceil(chunk_len);
    let next = Mutex::new(out.chunks_mut(chunk_len).enumerate());
    let take = || next.lock().unwrap_or_else(PoisonError::into_inner).next();
    let work = || {
        let Some((i, out_chunk)) = take() else {
            return;
        };
        let mut scratch = init();
        chunk(&mut scratch, i, out_chunk);
        while let Some((i, out_chunk)) = take() {
            chunk(&mut scratch, i, out_chunk);
        }
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
    fn every_light_row_runs_once_with_its_own_index() {
        for pixels in [0, THREADS_FROM_PX] {
            let mut out = vec![usize::MAX; 1000 * 5];
            for_each_light_row(&mut out, 5, pixels, |y, out_row| {
                for (x, v) in out_row.iter_mut().enumerate() {
                    assert_eq!(*v, usize::MAX, "row {y} written twice");
                    *v = y * 5 + x;
                }
            });
            assert!(out.iter().enumerate().all(|(i, &v)| i == v));
        }
    }

    #[test]
    fn every_chunk_runs_once_with_its_own_index_and_the_last_is_short() {
        for pixels in [0, THREADS_FROM_PX] {
            let mut out = vec![usize::MAX; 10];
            for_each_chunk(
                &mut out,
                4,
                pixels,
                || (),
                |(), i, chunk| {
                    assert_eq!(chunk.len(), if i == 2 { 2 } else { 4 });
                    for (k, v) in chunk.iter_mut().enumerate() {
                        assert_eq!(*v, usize::MAX, "chunk {i} written twice");
                        *v = i * 4 + k;
                    }
                },
            );
            assert!(out.iter().enumerate().all(|(i, &v)| i == v));
        }
    }

    #[test]
    fn a_small_pass_runs_here_in_order_with_one_scratch() {
        let here = std::thread::current().id();
        let (seen, made) = (Mutex::new(Vec::new()), Mutex::new(0));
        let mut out = vec![0u8; 100];
        let init = || *made.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        for_each_chunk(&mut out, 3, THREADS_FROM_PX - 1, init, |(), i, _| {
            assert_eq!(std::thread::current().id(), here);
            seen.lock().unwrap_or_else(PoisonError::into_inner).push(i);
        });
        let seen = seen.into_inner().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(seen, (0..34).collect::<Vec<_>>());
        assert_eq!(made.into_inner().unwrap_or_else(PoisonError::into_inner), 1);
    }

    #[test]
    fn a_threaded_pass_makes_a_scratch_per_thread_not_per_chunk() {
        let made = Mutex::new(0usize);
        let mut out = vec![0u8; 10_000];
        let init = || *made.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        for_each_chunk(&mut out, 1, THREADS_FROM_PX, init, |(), _, c| c[0] = 1);
        assert!(out.iter().all(|&v| v == 1));
        let made = made.into_inner().unwrap_or_else(PoisonError::into_inner);
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        assert!((1..=cores).contains(&made), "{made} scratches");
    }

    #[test]
    fn a_zero_row_length_writes_nothing() {
        let mut out = vec![7u8; 4];
        for_each_row(&mut out, 0, |_, _| panic!("no rows to run"));
        assert_eq!(out, [7; 4]);
    }
}
