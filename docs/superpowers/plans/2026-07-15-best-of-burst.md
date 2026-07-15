# Best-of-Burst Culling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a non-destructive `Bursts` toggle that groups the current folder into bursts by capture-time proximity, scores each frame's sharpness, and in the grid badges the sharpest frame of each burst while dimming its siblings.

**Architecture:** A new pure `burst` module derives per-entry marks from capture times + sharpness scores. Capture times are read in the background via a new `Loader` `Meta` job; sharpness is computed from decoded thumbnails. The `App` caches both keyed by path, rebuilds a `burst_marks` vector when either changes, and exposes a per-cell accessor the grid renderer consumes. The toggle is mutually exclusive with the star filter.

**Tech Stack:** Rust, egui 0.34, winit, objc2 (macOS ImageIO via existing `image_decode`). No new dependencies.

## Global Constraints

- Every new source file starts with the exact header line: `// SPDX-License-Identifier: MIT OR Apache-2.0`
- No new crate dependencies — reuse `navigation::group_by_time`, `sharpness::sharpness`, `image_decode::capture_time`, and the existing `Loader`.
- Burst gap is fixed: `Duration::from_secs(2)`. No UI knob.
- Invariant: `bursts_on == true` implies `filter.is_none()`. Enforced in both directions.
- Non-destructive: never write ratings, never hide/remove entries.
- Follow the codebase convention of `#[allow(dead_code)]` on API added before its consumer (see `loader.rs`), removed when wired. Intermediate `dead_code` warnings between tasks are expected; `cargo build`/`cargo test` still pass (warnings are not errors).
- Spec: `docs/superpowers/specs/2026-07-15-best-of-burst-design.md`

---

### Task 1: Pure `burst` module

**Files:**
- Create: `src/burst.rs`
- Modify: `src/main.rs` (add `mod burst;`)

**Interfaces:**
- Consumes: `navigation::group_by_time(times: &[Option<SystemTime>], gap: Duration) -> Vec<u32>` (existing).
- Produces:
  - `pub const BURST_GAP: Duration` (= 2s)
  - `pub enum BurstMark { Best, Sibling }` (derives `Copy, Clone, Debug, PartialEq, Eq`)
  - `pub fn compute_marks(group_ids: &[u32], scores: &[Option<f64>]) -> Vec<Option<BurstMark>>`
  - `pub fn marks_for(times: &[Option<SystemTime>], scores: &[Option<f64>], gap: Duration) -> Vec<Option<BurstMark>>`

- [ ] **Step 1: Create `src/burst.rs` with the implementation**

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Best-of-burst derivation. Given a per-entry burst grouping and per-entry
//! sharpness scores, decide which frame of each burst is the "best" (sharpest)
//! and which are its siblings. Pure and total so it can be unit-tested without
//! any UI, filesystem, or decode state.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use crate::navigation::group_by_time;

/// Max gap between consecutive shots for them to count as one burst. Fixed at
/// the typical camera burst-mode cadence; no UI knob (YAGNI).
pub const BURST_GAP: Duration = Duration::from_secs(2);

/// How a single frame relates to its burst. Absent (`None` in the output)
/// means the frame is a singleton (its group has size 1) — never badged.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BurstMark {
    /// The sharpest known frame of a burst of 2+ frames.
    Best,
    /// A non-best member of a burst of 2+ frames.
    Sibling,
}

/// Strict "a is a better score than b": a real score beats no score; two real
/// scores compare numerically. Used so ties and unscored frames keep the
/// earliest member as the provisional winner.
fn score_gt(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x > y,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Mark each entry given its burst `group_ids` and optional sharpness `scores`
/// (parallel to `group_ids`). Groups of size 1 → `None`. In a group of 2+, the
/// member with the strictly-highest known score is `Best`, the rest `Sibling`;
/// with all scores tied or unknown, the earliest member is the provisional
/// `Best`. Output is 1:1 with `group_ids`.
pub fn compute_marks(group_ids: &[u32], scores: &[Option<f64>]) -> Vec<Option<BurstMark>> {
    let score_at = |i: usize| scores.get(i).copied().flatten();

    // Group sizes.
    let mut sizes: HashMap<u32, usize> = HashMap::new();
    for &g in group_ids {
        *sizes.entry(g).or_insert(0) += 1;
    }

    // Winner index per group: first member seen, replaced only on a strictly
    // greater score (so ties/None keep the earliest).
    let mut best: HashMap<u32, usize> = HashMap::new();
    for (i, &g) in group_ids.iter().enumerate() {
        match best.get(&g).copied() {
            None => {
                best.insert(g, i);
            }
            Some(bi) => {
                if score_gt(score_at(i), score_at(bi)) {
                    best.insert(g, i);
                }
            }
        }
    }

    group_ids
        .iter()
        .enumerate()
        .map(|(i, &g)| {
            if sizes.get(&g).copied().unwrap_or(0) < 2 {
                None
            } else if best.get(&g) == Some(&i) {
                Some(BurstMark::Best)
            } else {
                Some(BurstMark::Sibling)
            }
        })
        .collect()
}

/// Convenience: group `times` by `gap` (via `group_by_time`) then mark. This is
/// the entry point the app uses each time capture times or scores change.
pub fn marks_for(
    times: &[Option<SystemTime>],
    scores: &[Option<f64>],
    gap: Duration,
) -> Vec<Option<BurstMark>> {
    let groups = group_by_time(times, gap);
    compute_marks(&groups, scores)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singleton_group_is_unmarked() {
        assert_eq!(compute_marks(&[0], &[None]), vec![None]);
        assert_eq!(
            compute_marks(&[0, 1, 2], &[Some(1.0), Some(2.0), Some(3.0)]),
            vec![None, None, None]
        );
    }

    #[test]
    fn two_member_burst_picks_higher_score() {
        assert_eq!(
            compute_marks(&[0, 0], &[Some(1.0), Some(2.0)]),
            vec![Some(BurstMark::Sibling), Some(BurstMark::Best)]
        );
    }

    #[test]
    fn all_unscored_burst_defaults_to_first() {
        assert_eq!(
            compute_marks(&[0, 0, 0], &[None, None, None]),
            vec![
                Some(BurstMark::Best),
                Some(BurstMark::Sibling),
                Some(BurstMark::Sibling)
            ]
        );
    }

    #[test]
    fn tie_keeps_earliest_as_best() {
        assert_eq!(
            compute_marks(&[0, 0], &[Some(5.0), Some(5.0)]),
            vec![Some(BurstMark::Best), Some(BurstMark::Sibling)]
        );
    }

    #[test]
    fn scored_member_beats_unscored_even_if_later() {
        assert_eq!(
            compute_marks(&[0, 0], &[None, Some(1.0)]),
            vec![Some(BurstMark::Sibling), Some(BurstMark::Best)]
        );
    }

    #[test]
    fn mixed_groups_are_independent() {
        // group 0: idx 0,1 (best = 1); group 1: idx 2 (singleton);
        // group 2: idx 3,4 (best = 4, since idx3 is unscored).
        let groups = [0u32, 0, 1, 2, 2];
        let scores = [Some(1.0), Some(9.0), Some(3.0), None, Some(4.0)];
        assert_eq!(
            compute_marks(&groups, &scores),
            vec![
                Some(BurstMark::Sibling),
                Some(BurstMark::Best),
                None,
                Some(BurstMark::Sibling),
                Some(BurstMark::Best),
            ]
        );
    }

    #[test]
    fn marks_for_groups_by_gap_then_marks() {
        let t = |s: u64| Some(SystemTime::UNIX_EPOCH + Duration::from_secs(s));
        // 0s,1s = burst A; 10s,11s = burst B (9s jump splits).
        let times = [t(0), t(1), t(10), t(11)];
        // In A, idx1 sharper; in B, idx2 sharper.
        let scores = [Some(1.0), Some(2.0), Some(8.0), Some(3.0)];
        assert_eq!(
            marks_for(&times, &scores, Duration::from_secs(3)),
            vec![
                Some(BurstMark::Sibling),
                Some(BurstMark::Best),
                Some(BurstMark::Best),
                Some(BurstMark::Sibling),
            ]
        );
    }
}
```

- [ ] **Step 2: Register the module in `src/main.rs`**

Insert `mod burst;` immediately after `mod app;` (line 26), keeping the alphabetical order (before `mod catalog;`).

```rust
mod app;
mod burst;
mod catalog;
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test burst::`
Expected: PASS — 7 tests in `burst::tests` pass.

- [ ] **Step 4: Commit**

```bash
git add src/burst.rs src/main.rs
git commit -m "Add pure burst-mark derivation module"
```

---

### Task 2: Loader `Meta` job for background capture-time reads

**Files:**
- Modify: `src/loader.rs`
- Modify: `src/main.rs:247-256` (poll_all drain — destructure the new third bucket)

**Interfaces:**
- Consumes: `image_decode::capture_time(&Path) -> Option<SystemTime>` (existing).
- Produces:
  - `Loader::request_meta(&mut self, path: PathBuf)` — enqueue a capture-time read.
  - `Loader::poll_all(&mut self) -> (Vec<PathBuf>, Vec<(PathBuf, u32)>, Vec<(PathBuf, Option<SystemTime>)>)` — the third element is `(path, capture_time)` arrivals.

- [ ] **Step 1: Add the `SystemTime` import to `src/loader.rs`**

At the top of `src/loader.rs`, add to the `std` imports (near line 18-22):

```rust
use std::time::SystemTime;
```

- [ ] **Step 2: Add the `Meta` variants to `Job` and `JobResult`**

In `src/loader.rs`, extend the `Job` enum (currently lines 28-35):

```rust
enum Job {
    /// Full-resolution decode at `max_dim` for the loupe.
    Full(PathBuf),
    /// Thumbnail of `path` whose longest side is at most `max_px`.
    // Consumed by the grid/filmstrip in a later wave (T5/T6).
    #[allow(dead_code)]
    Thumb(PathBuf, u32),
    /// Capture-time (EXIF/mtime) read for burst grouping. Lowest priority.
    Meta(PathBuf),
}
```

And extend `JobResult` (currently lines 38-41):

```rust
enum JobResult {
    Full(PathBuf, Result<DecodedImage, String>),
    Thumb(PathBuf, u32, Result<Arc<DecodedImage>, String>),
    Meta(PathBuf, Option<SystemTime>),
}
```

- [ ] **Step 3: Add a `meta` queue and process `Meta` jobs in the worker**

In `struct Queue` (lines 48-54), add a `meta` queue:

```rust
#[derive(Default)]
struct Queue {
    full: VecDeque<Job>,
    thumbs: VecDeque<Job>,
    /// Capture-time reads — served after full-image and thumbnail work, since
    /// burst badges are not latency-critical.
    meta: VecDeque<Job>,
    /// Set when the `Loader` is dropped so idle workers wake and exit.
    shutdown: bool,
}
```

In the worker loop, update the job-pop line (currently line 125) to drain `meta` last:

```rust
                            if let Some(j) = q
                                .full
                                .pop_front()
                                .or_else(|| q.thumbs.pop_front())
                                .or_else(|| q.meta.pop_front())
                            {
                                break j;
                            }
```

And add a `Job::Meta` arm to the worker's `match job` (after the `Job::Thumb` arm, before the closing `}` at line 158):

```rust
                        Job::Meta(path) => {
                            // capture_time never panics by contract, but the FFI
                            // boundary is caught for parity with the other arms.
                            let t = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                image_decode::capture_time(&path)
                            }))
                            .unwrap_or(None);
                            JobResult::Meta(path, t)
                        }
```

- [ ] **Step 4: Add the `meta_inflight` set to `Loader` and initialize it**

In `struct Loader` (lines 68-87), add after `thumb_capacity`:

```rust
    /// Paths with a capture-time read in flight, to avoid enqueuing duplicates.
    meta_inflight: HashSet<PathBuf>,
```

In `Loader::new`'s returned `Self { .. }` (lines 168-180), add:

```rust
            meta_inflight: HashSet::new(),
```

- [ ] **Step 5: Add `request_meta` and route `Meta` results in `drain`**

Add this method to `impl Loader` (e.g. right after `request_thumb`, ~line 241):

```rust
    /// Ask a worker to read `path`'s capture time unless already in flight.
    /// Results arrive in the third bucket of [`poll_all`](Self::poll_all).
    pub fn request_meta(&mut self, path: PathBuf) {
        if self.meta_inflight.contains(&path) {
            return;
        }
        if let Ok(mut q) = self.shared.queue.lock() {
            q.meta.push_back(Job::Meta(path.clone()));
            self.meta_inflight.insert(path);
            self.shared.ready.notify_one();
        }
    }
```

Change `poll_all`'s signature and body (lines 278-281) to a 3-tuple:

```rust
    pub fn poll_all(
        &mut self,
    ) -> (
        Vec<PathBuf>,
        Vec<(PathBuf, u32)>,
        Vec<(PathBuf, Option<SystemTime>)>,
    ) {
        self.drain()
    }
```

Change `drain`'s signature/body (lines 285-317) to collect and return metas:

```rust
    fn drain(
        &mut self,
    ) -> (
        Vec<PathBuf>,
        Vec<(PathBuf, u32)>,
        Vec<(PathBuf, Option<SystemTime>)>,
    ) {
        let mut full = vec![];
        let mut thumbs = vec![];
        let mut metas = vec![];
        while let Ok(result) = self.res_rx.try_recv() {
            match result {
                JobResult::Full(path, r) => {
                    self.inflight.remove(&path);
                    match r {
                        Ok(img) => {
                            self.insert(path.clone(), Arc::new(img));
                            full.push(path);
                        }
                        Err(e) => eprintln!("decode failed for {}: {e}", path.display()),
                    }
                }
                JobResult::Thumb(path, max_px, r) => {
                    let key = (path.clone(), max_px);
                    self.thumb_inflight.remove(&key);
                    match r {
                        Ok(img) => {
                            self.insert_thumb(key.clone(), img);
                            thumbs.push(key);
                        }
                        Err(e) => {
                            eprintln!("thumbnail failed for {} @ {max_px}: {e}", path.display());
                            self.thumb_failed.insert(key);
                        }
                    }
                }
                JobResult::Meta(path, t) => {
                    self.meta_inflight.remove(&path);
                    metas.push((path, t));
                }
            }
        }
        (full, thumbs, metas)
    }
```

- [ ] **Step 6: Fix the two thin-wrapper callers of the tuple**

`poll` (line 212) and `poll_thumbs` (line 266) index the tuple — they still compile because `.0` / `.1` are unchanged, but confirm they read:

```rust
    pub fn poll(&mut self) -> Vec<PathBuf> {
        self.poll_all().0
    }
```

```rust
    pub fn poll_thumbs(&mut self) -> Vec<(PathBuf, u32)> {
        self.poll_all().1
    }
```

No change needed if they already look like this. (They do — this step is a verification, not an edit.)

- [ ] **Step 7: Update the frame-loop drain in `src/main.rs`**

In `about_to_wait` (lines 247-256), destructure the third bucket (ignored for now — consumed in Task 5):

```rust
        // Drain all loader tiers once per frame.
        if let Some(loader) = &mut self.loader {
            let (full, thumbs, _metas) = loader.poll_all();
            // Any arrival may be the wanted image (full) or its placeholder
            // (thumb), so try to (re)show on either; redraw to paint new thumbs.
            let any = !full.is_empty() || !thumbs.is_empty();
            if any {
                self.try_show();
                self.request_redraw();
            }
        }
```

- [ ] **Step 8: Build and run existing tests**

Run: `cargo build 2>&1 | tail -5`
Expected: `Finished` (dead-code warnings for `request_meta` are acceptable until Task 4/5 call it).

Run: `cargo test`
Expected: PASS — all existing tests plus Task 1's `burst::` tests pass.

- [ ] **Step 9: Commit**

```bash
git add src/loader.rs src/main.rs
git commit -m "Add Loader Meta job for background capture-time reads"
```

---

### Task 3: App burst state, derivation, and accessor

**Files:**
- Modify: `src/app.rs` (struct fields, `new()`, new methods, folder-load reset)

**Interfaces:**
- Consumes: `burst::{BurstMark, BURST_GAP, marks_for}` (Task 1); `image_decode::capture_time` cache fed later.
- Produces (all on `App`):
  - fields `bursts_on: bool`, `capture_times: HashMap<PathBuf, Option<SystemTime>>`, `sharpness: HashMap<PathBuf, f64>`, `burst_marks: Vec<Option<BurstMark>>`
  - `fn recompute_burst_marks(&mut self)` — rebuild `burst_marks` from caches over `entries()`
  - `pub(crate) fn burst_mark_at(&self, pos: usize) -> Option<BurstMark>`
  - `pub(crate) fn bursts_on(&self) -> bool`
  - `fn reset_burst_state(&mut self)`

- [ ] **Step 1: Add imports to `src/app.rs`**

Near the other `use crate::...` lines at the top of `src/app.rs`, add:

```rust
use crate::burst::{self, BurstMark};
use crate::sharpness;
```

Ensure `std::time::SystemTime` is in scope. If the file has a `use std::time::{Instant};` line, change it to `use std::time::{Instant, SystemTime};`; otherwise add `use std::time::SystemTime;`.

- [ ] **Step 2: Add the four fields to `struct App`**

In `pub(crate) struct App` (near the browser-state block, after `thumb_tex` at line 221), add:

```rust
    // ---- Best-of-burst state ----
    /// Whether burst detection is active (badges + dimming). Mutually exclusive
    /// with the star filter; `bursts_on` implies `filter.is_none()`.
    bursts_on: bool,
    /// Cached capture times per path (EXIF/mtime). `Some(None)` records a read
    /// that yielded no time, so we don't re-request it. Survives toggling off.
    capture_times: HashMap<PathBuf, Option<SystemTime>>,
    /// Cached sharpness scores per path. Survives toggling off.
    sharpness: HashMap<PathBuf, f64>,
    /// Derived burst marks, indexed by playlist entry index (not visible pos).
    /// Empty when bursts are off. Rebuilt when caches or the toggle change.
    burst_marks: Vec<Option<BurstMark>>,
```

- [ ] **Step 3: Initialize the fields in `App::new`**

In the `Self { .. }` returned by `new` (starting line 303), add (anywhere among the field initializers):

```rust
            bursts_on: false,
            capture_times: HashMap::new(),
            sharpness: HashMap::new(),
            burst_marks: Vec::new(),
```

- [ ] **Step 4: Add the derivation, accessor, getter, and reset methods**

Add these to an `impl App` block (e.g. right after `rating_at`, ~line 2401). `burst_mark_at` is marked `#[allow(dead_code)]` until Task 6 wires it into the grid.

```rust
    /// Rebuild `burst_marks` from the cached capture times + sharpness over the
    /// current playlist entries. Clears the marks when bursts are off or there
    /// is no playlist. Cheap: O(entries).
    fn recompute_burst_marks(&mut self) {
        let Some(pl) = &self.playlist else {
            self.burst_marks.clear();
            return;
        };
        if !self.bursts_on {
            self.burst_marks.clear();
            return;
        }
        let entries = pl.entries();
        let times: Vec<Option<SystemTime>> = entries
            .iter()
            .map(|p| self.capture_times.get(p).copied().flatten())
            .collect();
        let scores: Vec<Option<f64>> = entries.iter().map(|p| self.sharpness.get(p).copied()).collect();
        self.burst_marks = burst::marks_for(&times, &scores, burst::BURST_GAP);
    }

    /// Burst mark for the visible cell at `pos`. `None` when bursts are off, the
    /// cell is a singleton, or `pos` is out of range. `burst_marks` is indexed by
    /// playlist entry index, so we map the visible position through `visible`.
    #[allow(dead_code)]
    pub(crate) fn burst_mark_at(&self, pos: usize) -> Option<BurstMark> {
        let idx = *self.visible.get(pos)?;
        self.burst_marks.get(idx).copied().flatten()
    }

    /// Whether burst mode is currently on (for the toolbar toggle state).
    pub(crate) fn bursts_on(&self) -> bool {
        self.bursts_on
    }

    /// Reset transient burst view state on a folder change. Keeps the path-keyed
    /// caches (harmless across folders; helps on revisit) but drops the toggle
    /// and derived marks so a new folder starts plain.
    fn reset_burst_state(&mut self) {
        self.bursts_on = false;
        self.burst_marks.clear();
    }
```

- [ ] **Step 5: Reset burst state on both playlist assignments**

After `self.playlist = Some(playlist);` at **line 400** (the loupe `open` path) add:

```rust
            self.reset_burst_state();
```

After `self.playlist = Some(playlist);` at **line 444** (`load_folder`) add:

```rust
        self.reset_burst_state();
```

- [ ] **Step 6: Build and test**

Run: `cargo build 2>&1 | tail -5`
Expected: `Finished`. Dead-code warnings for `recompute_burst_marks` / `burst_mark_at` / `bursts_on` are acceptable until Tasks 4–6 consume them.

Run: `cargo test`
Expected: PASS (no behavior change yet).

- [ ] **Step 7: Commit**

```bash
git add src/app.rs
git commit -m "Add App burst state, derivation, and per-cell accessor"
```

---

### Task 4: Toggle, invocation, and filter mutual-exclusion

**Files:**
- Modify: `src/ui.rs` (`UiAction`, `global_toolbar`, help overlay)
- Modify: `src/app.rs` (`toggle_bursts`, `request_capture_times`, action dispatch, `B` key, `set_filter` auto-off)

**Interfaces:**
- Consumes: `Loader::request_meta` (Task 2), `App::recompute_burst_marks` / `bursts_on` (Task 3).
- Produces: `UiAction::ToggleBursts`; `App::toggle_bursts(&mut self)`; `App::request_capture_times(&mut self)`.

- [ ] **Step 1: Add the `ToggleBursts` action variant**

In `src/ui.rs`, add to the `UiAction` enum (after `SetRating(u8)`, line 71):

```rust
    /// Toggle best-of-burst detection (badges + dimming). Ignored while a star
    /// filter is active.
    ToggleBursts,
```

- [ ] **Step 2: Add `toggle_bursts` and `request_capture_times` to `App`**

Add to `impl App` in `src/app.rs` (e.g. near `set_filter`, ~line 1197):

```rust
    /// Flip best-of-burst mode. Ignored while a star filter is active (bursts run
    /// only over the unfiltered folder). Turning on kicks off the background
    /// capture-time scan and rebuilds marks; turning off clears the badges but
    /// keeps the caches so re-enabling is instant.
    fn toggle_bursts(&mut self) {
        if self.filter.is_some() {
            return; // mutually exclusive with the filter
        }
        self.bursts_on = !self.bursts_on;
        if self.bursts_on {
            self.request_capture_times();
            self.recompute_burst_marks();
        } else {
            self.burst_marks.clear();
        }
        self.request_redraw();
    }

    /// Enqueue background capture-time reads for every entry not yet cached.
    fn request_capture_times(&mut self) {
        let Some(pl) = &self.playlist else { return };
        let paths: Vec<PathBuf> = pl
            .entries()
            .iter()
            .filter(|p| !self.capture_times.contains_key(*p))
            .cloned()
            .collect();
        if let Some(loader) = &mut self.loader {
            for p in paths {
                loader.request_meta(p);
            }
        }
    }
```

- [ ] **Step 3: Dispatch the action**

In the `UiAction` match in `src/app.rs` (near `SetRating`, ~line 2249), add:

```rust
                ui::UiAction::ToggleBursts => self.toggle_bursts(),
```

- [ ] **Step 4: Bind the `B` key**

In `handle_key`'s `match code` (near `KeyCode::KeyG`, line 2514), add:

```rust
            // `B` toggles best-of-burst badges (grid). No-op while a filter is
            // active — `toggle_bursts` guards it.
            KeyCode::KeyB if !cmd && !alt => self.toggle_bursts(),
```

- [ ] **Step 5: Enforce the invariant in `set_filter`**

At the top of `set_filter` (line 1197, before `let want_idx = ...`), add:

```rust
        // Bursts run only over the unfiltered folder; applying a filter ends
        // burst mode. Clearing the filter (`None`) leaves bursts off — the user
        // re-enables with the toggle / `B`.
        if filter.is_some() && self.bursts_on {
            self.bursts_on = false;
            self.burst_marks.clear();
        }
```

- [ ] **Step 6: Add the toolbar toggle**

In `global_toolbar` (`src/ui.rs`), after the `Unrated` block (ends line 238), add:

```rust
            // Best-of-burst toggle. Disabled while a filter is active (bursts
            // need the whole, unfiltered folder to be meaningful).
            ui.separator();
            let filter_active = app.filter().is_some();
            let resp = ui.add_enabled(
                !filter_active,
                egui::SelectableLabel::new(app.bursts_on(), "Bursts"),
            );
            if resp.clicked() {
                out.actions.push(UiAction::ToggleBursts);
            }
            resp.on_hover_text(if filter_active {
                "Clear the filter to use Bursts"
            } else {
                "Group bursts and badge the sharpest frame (B)"
            });
```

- [ ] **Step 7: Add the `B` shortcut to the help overlay**

In `help_modal`'s `SHORTCUTS` array (`src/ui.rs`, ~line 447, near the `"C"` entry), add:

```rust
        ("B", "Best-of-burst badges (grid)"),
```

- [ ] **Step 8: Build and manually verify the toggle plumbing**

Run: `cargo build 2>&1 | tail -5`
Expected: `Finished`.

Run: `cargo run` — open a folder in the grid. Confirm:
- A `Bursts` label appears in the toolbar; clicking it or pressing `B` highlights it (on) / unhighlights (off).
- Applying a star filter (click a star) greys out `Bursts` and un-highlights it; pressing `B` does nothing while filtered.
- Clicking `All` re-enables the `Bursts` label.

(No badges yet — scoring/drain is Task 5.)

- [ ] **Step 9: Commit**

```bash
git add src/app.rs src/ui.rs
git commit -m "Add Bursts toggle, B shortcut, and filter mutual-exclusion"
```

---

### Task 5: Background compute wiring (capture times → grouping → sharpness)

**Files:**
- Modify: `src/main.rs:247-256` (route meta + thumb arrivals into the app)
- Modify: `src/app.rs` (`on_capture_times`, `score_arrived_thumbs`, `request_burst_thumbs`; extend `toggle_bursts`)

**Interfaces:**
- Consumes: `Loader::poll_all` 3-tuple (Task 2); `sharpness::sharpness` (existing); `App::recompute_burst_marks` (Task 3).
- Produces (on `App`): `pub(crate) fn on_capture_times(&mut self, times: Vec<(PathBuf, Option<SystemTime>)>)`; `pub(crate) fn score_arrived_thumbs(&mut self, arrivals: &[(PathBuf, u32)])`; `fn request_burst_thumbs(&mut self)`.

- [ ] **Step 1: Add `request_burst_thumbs` to `App`**

Add to `impl App` in `src/app.rs` (near `request_working_thumbs`, ~line 2001):

```rust
    /// For burst members (size >= 2) not yet scored: score any whose thumbnail is
    /// already decoded, and request the rest. Bounds decode work to burst members
    /// (never singletons, never the whole folder) so each burst's winner is
    /// chosen from the full burst — not just the frames scrolled past. Once a path
    /// is scored it's never requested again (the score cache is the guard).
    fn request_burst_thumbs(&mut self) {
        if !self.bursts_on {
            return;
        }
        let px = self.thumb_px;
        let members: Vec<PathBuf> = {
            let Some(pl) = &self.playlist else { return };
            pl.entries()
                .iter()
                .enumerate()
                .filter(|(idx, p)| {
                    matches!(self.burst_marks.get(*idx), Some(Some(_)))
                        && !self.sharpness.contains_key(*p)
                })
                .map(|(_, p)| p.clone())
                .collect()
        };
        if members.is_empty() {
            return;
        }
        // Score already-decoded members now; request the rest.
        let mut newly: Vec<(PathBuf, f64)> = Vec::new();
        if let Some(loader) = &mut self.loader {
            for p in &members {
                if let Some(img) = loader.get_thumb(p, px) {
                    // Thumbnails are premultiplied RGBA8; photos are opaque so
                    // premultiplied == straight for the luma-based metric.
                    newly.push((p.clone(), sharpness::sharpness(&img.rgba, img.width, img.height)));
                } else if !loader.thumb_failed(p, px) {
                    loader.request_thumb(p.clone(), px);
                }
            }
        }
        if !newly.is_empty() {
            for (p, s) in newly {
                self.sharpness.insert(p, s);
            }
            self.recompute_burst_marks();
        }
    }
```

- [ ] **Step 2: Add `on_capture_times` and `score_arrived_thumbs` to `App`**

Add to `impl App` (near `request_burst_thumbs`):

```rust
    /// Fold background capture-time reads into the cache, then refresh grouping +
    /// request thumbnails for the newly-identified burst members.
    pub(crate) fn on_capture_times(&mut self, times: Vec<(PathBuf, Option<SystemTime>)>) {
        for (path, t) in times {
            self.capture_times.insert(path, t);
        }
        if self.bursts_on {
            self.recompute_burst_marks();
            self.request_burst_thumbs();
        }
    }

    /// When thumbnails arrive and bursts are on, compute + cache sharpness for any
    /// unscored burst member among them, then refresh the winners and redraw.
    pub(crate) fn score_arrived_thumbs(&mut self, arrivals: &[(PathBuf, u32)]) {
        if !self.bursts_on {
            return;
        }
        let px = self.thumb_px;
        let mut newly: Vec<(PathBuf, f64)> = Vec::new();
        if let Some(loader) = &self.loader {
            for (path, mpx) in arrivals {
                if *mpx != px || self.sharpness.contains_key(path) {
                    continue;
                }
                // Only score burst members (their entry index maps to a mark).
                let is_member = self
                    .playlist
                    .as_ref()
                    .and_then(|pl| pl.entries().iter().position(|e| e == path))
                    .map(|idx| matches!(self.burst_marks.get(idx), Some(Some(_))))
                    .unwrap_or(false);
                if !is_member {
                    continue;
                }
                if let Some(img) = loader.get_thumb(path, px) {
                    newly.push((path.clone(), sharpness::sharpness(&img.rgba, img.width, img.height)));
                }
            }
        }
        if !newly.is_empty() {
            for (p, s) in newly {
                self.sharpness.insert(p, s);
            }
            self.recompute_burst_marks();
            self.request_redraw();
        }
    }
```

- [ ] **Step 3: Kick scoring on toggle-on (for cached-capture-time revisits)**

In `toggle_bursts` (added Task 4), add `self.request_burst_thumbs();` after `self.recompute_burst_marks();` in the `if self.bursts_on` branch, so a re-toggle with capture times already cached still scores + requests member thumbs:

```rust
        if self.bursts_on {
            self.request_capture_times();
            self.recompute_burst_marks();
            self.request_burst_thumbs();
        } else {
```

- [ ] **Step 4: Route meta + thumb arrivals in the frame loop**

Replace the drain block in `src/main.rs` (`about_to_wait`, lines 247-256) with:

```rust
        // Drain all loader tiers once per frame.
        if let Some(loader) = &mut self.loader {
            let (full, thumbs, metas) = loader.poll_all();
            let any = !full.is_empty() || !thumbs.is_empty() || !metas.is_empty();
            // Note: `loader`'s borrow ends at `poll_all` above (NLL), so these
            // `&mut self` calls are allowed even though `loader` is still in scope.
            if !metas.is_empty() {
                self.on_capture_times(metas);
            }
            if !thumbs.is_empty() {
                self.score_arrived_thumbs(&thumbs);
            }
            if any {
                self.try_show();
                self.request_redraw();
            }
        }
```

- [ ] **Step 5: Build and test**

Run: `cargo build 2>&1 | tail -5`
Expected: `Finished`. (No badges render yet — Task 6 draws them — but scoring runs; `burst_mark_at` remains the only `dead_code` item.)

Run: `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/app.rs src/main.rs
git commit -m "Wire background capture-time grouping and sharpness scoring"
```

---

### Task 6: Grid rendering — best badge + dimmed siblings

**Files:**
- Modify: `src/ui.rs` (`theme`, `thumbnail_cell`, `use`)
- Modify: `src/app.rs` (remove the `#[allow(dead_code)]` on `burst_mark_at`)

**Interfaces:**
- Consumes: `App::burst_mark_at(pos) -> Option<BurstMark>` (Task 3); `burst::BurstMark`.
- Produces: visible badge on the best frame + translucent dim on siblings, in both the grid and the Loupe filmstrip (shared cell).

- [ ] **Step 1: Import `BurstMark` and add a badge color to `theme`**

In `src/ui.rs`, add to the imports near line 17:

```rust
use crate::burst::BurstMark;
```

In `mod theme` (lines 22-33), add:

```rust
    /// Best-of-burst winner badge (mint green = "the keeper"), distinct from the
    /// gold rating stars so the two overlays never read as the same mark.
    pub const BURST_BADGE: Color32 = Color32::from_rgb(120, 230, 160);
```

- [ ] **Step 2: Paint the dim + badge in `thumbnail_cell`**

In `thumbnail_cell` (`src/ui.rs`), insert this block **after** the image/placeholder block (after line 675, the closing `}` of the `} else if style.show_placeholder { .. }`) and **before** the `let stars = app.rating_at(pos);` line (677). Drawing here means the rating stars and the selection outline paint on top of the dim, staying readable.

```rust
    // Best-of-burst overlay. Bursts imply no active filter, so every burst is
    // whole in the grid: the sharpest frame gets a badge, the rest are dimmed.
    match app.burst_mark_at(pos) {
        Some(BurstMark::Sibling) => {
            ui.painter()
                .rect_filled(rect, style.corner, egui::Color32::from_black_alpha(140));
        }
        Some(BurstMark::Best) => {
            // Top-left corner — rating stars live bottom-left, so no clash.
            let c = rect.left_top() + egui::vec2(style.corner + 9.0, style.corner + 9.0);
            ui.painter()
                .circle_filled(c, 9.0, egui::Color32::from_black_alpha(170));
            ui.painter().text(
                c,
                egui::Align2::CENTER_CENTER,
                "\u{2605}",
                egui::FontId::proportional(13.0),
                theme::BURST_BADGE,
            );
        }
        None => {}
    }
```

- [ ] **Step 3: Remove the `dead_code` allow on `burst_mark_at`**

In `src/app.rs`, delete the `#[allow(dead_code)]` line directly above `pub(crate) fn burst_mark_at` (added in Task 3, Step 4) — it now has a consumer.

- [ ] **Step 4: Build and confirm no burst-related warnings remain**

Run: `cargo build 2>&1 | tail -8`
Expected: `Finished`. The original 12 dead-code warnings (`capture_time`, `group_by_time`, `sharpness`, etc.) are now gone — every primitive is wired.

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Manual end-to-end verification**

Run: `cargo run` and open a folder that contains at least one burst (2+ photos shot within ~2s; any camera burst or quick re-shoots). Confirm:
1. Press `B` (or click `Bursts`). After a moment (background scan + scoring), each burst shows one **badged** frame (mint ★ top-left) and its other frames **dimmed**.
2. Scroll through the folder — the badged winner within each burst stays stable (doesn't flip as siblings scroll in/out).
3. A photo with no near-in-time neighbor is neither badged nor dimmed.
4. Press `B` again — badges/dimming vanish; the grid is plain. Press `B` once more — badges reappear instantly (caches retained).
5. With bursts on, click a star filter — the `Bursts` label greys out and badges disappear (mutual exclusion). Click `All`, press `B` — badges return.

- [ ] **Step 6: Commit**

```bash
git add src/app.rs src/ui.rs
git commit -m "Render best-of-burst badge and dim siblings in the grid"
```

---

## Self-Review

**Spec coverage:**
- Toggle (on-demand), off by default → Task 4 (`toggle_bursts`, toolbar, `B`).
- Fixed 2s gap → Task 1 (`BURST_GAP`).
- Best badge + dim siblings → Task 6.
- Filter mutual-exclusion (disabled toggle, auto-off, invariant) → Task 4 (Steps 5, 6) + Task 3 (marks only when on).
- Background capture times via `Job::Meta` → Task 2 + Task 5.
- Sharpness from decoded thumbs + winner-stability (request burst members) → Task 5 (`request_burst_thumbs`, `score_arrived_thumbs`).
- Grouping via `group_by_time` over `entries()` → Task 3 (`recompute_burst_marks`) + Task 1 (`marks_for`).
- Caches not persisted to catalog → satisfied (in-memory `HashMap`s only).
- Reset on folder change → Task 3 (Step 5).
- Unit tests (singleton, 2-member, all-unscored, tie, mixed) → Task 1 tests.
- Manual verification → Task 6 Step 5.

**Placeholder scan:** none — every code step contains full code; no TBD/TODO.

**Type consistency:** `poll_all` 3-tuple `(Vec<PathBuf>, Vec<(PathBuf,u32)>, Vec<(PathBuf, Option<SystemTime>)>)` used identically in Task 2 (loader) and Task 5 (main.rs). `BurstMark` / `compute_marks` / `marks_for` / `BURST_GAP` names match across Tasks 1, 3, 6. `burst_mark_at` signature identical in Task 3 (def) and Task 6 (consume). `on_capture_times` / `score_arrived_thumbs` signatures match between Task 5 def and main.rs call.

**Known accepted trade-offs:** `score_arrived_thumbs` does an O(n) `position` lookup per arrival (O(n·arrivals) worst case) — acceptable for typical folders; a path→index map is deferred (YAGNI). Loader background behavior is verified via build + manual run rather than a thread-timing integration test, matching the codebase's existing (untested) loader.
