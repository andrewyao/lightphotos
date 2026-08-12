# Plan — Zoom/pan support in compare mode (Y)

Work through these top to bottom (or by worktree assignment, if parallel).
Check a box only after verification passes.

**Status: COMPLETE** — shipped in commit `2cdbf19`. All boxes checked; kept for reference.

- [x] Task 1: `loupe_area()` returns half-width while comparing, with a `w >= 2` guard mirroring the render-split guard so zoom math and the GPU viewport split never disagree (files: src/app.rs)
- [x] Task 2: `cursor_in_loupe()` folds right-half cursor coords into the same `0..half` local space as the left half, so `zoom_at`'s cursor anchoring is correct on either side (requires Task 1) (files: src/app.rs)
- [x] Task 3: `push_compare()` stops re-fitting every frame — uses the live shared `loupe_transform()`, drops the `(half_w, half_h)` params, keeps crop-sharing (requires Tasks 1, 2) (files: src/app.rs)
- [x] Task 4: Update the per-frame call site to drop the now-unused args (requires Task 3) (files: src/app.rs)
- [x] Task 5: Delete `fit_transform_for` — dead once `push_compare` no longer calls it; confirm no other references (requires Task 3) (files: src/app.rs)
- [x] Task 6: `toggle_compare()` refits (if fitted) or recenters at the current zoom (if manually zoomed), since flipping `compare` silently changes what `loupe_area()` returns with no resize event to trigger the usual refit (requires Task 1) (files: src/app.rs)
- [x] Task 7: `cargo build --release` clean with no leftover `fit_transform_for` references, and `cargo test` still passing (files: —)
- [x] Task 8: Manual verification pass — scroll-zoom over each half anchors at the cursor with both halves in lockstep and no jump when crossing sides; drag-pan moves both; exiting compare re-fits sanely; toggling while zoomed recenters at the same zoom with no half-blank pane; window resize keeps both halves in sync (files: —)

<!--
Tips:
- Make each task independently verifiable (a test, a build, a specific output).
- Note "files:" per task if you plan to parallelize across worktrees —
  plan-runner uses this to avoid assigning conflicting tasks to different tracks.
- Keep tasks small; one failure shouldn't cascade into the next.
- Note dependencies explicitly ("requires Task 2") if order matters.
-->

---

## Reference

Line numbers below refer to the pre-refactor single-file `src/app.rs` (this work predates commit `6a917e3`, which split it into `src/app/`).

### Context

Compare mode (Y key) shows the current image twice, side by side: left half = "before" (identity tone, same crop), right half = "after" (current edits). It's not two different photos — same crop, same geometry, only tone differs.

Before this change, compare mode had no working zoom. `push_compare()` recomputed a fresh "contain fit" transform (`fit_transform_for`) every single frame from scratch, ignoring `self.zoom`/`self.pan` entirely. Mouse wheel/pinch/drag handlers did mutate `self.zoom`/`self.pan` even while comparing (they're not gated on `self.compare`), but that got clobbered on the very next frame — so zooming during compare had no lasting visual effect.

Root cause: `self.zoom`/`self.pan`/`loupe_area()`/`cursor_in_loupe()` are all keyed to the *full* loupe rect, but while comparing, each half only occupies half that width. Fix: make the shared geometry state compare-aware so both halves render from one live `zoom`/`pan`, correctly anchored regardless of which half the mouse is over.

All changes were in `src/app.rs`. No changes needed in `src/main.rs` (verified: wheel/pinch/drag handlers already route through `cursor_in_loupe()` / `self.pan` deltas).

### Task 1 — `loupe_area()` (app.rs:2418-2425)

```rust
fn loupe_area(&self) -> (f32, f32) {
    match self.loupe_viewport {
        Some((_, _, w, h)) => {
            let h = h.max(1) as f32;
            if self.compare && self.mode == ViewMode::Loupe && w >= 2 && h > 0.0 {
                ((w / 2).max(1) as f32, h)
            } else {
                (w.max(1) as f32, h)
            }
        }
        None => self.win_size,
    }
}
```
`w / 2` integer division must match the split's `let half = w / 2;` exactly (avoids off-by-one between the two halves).

### Task 2 — `cursor_in_loupe()` (app.rs:2512-2522)

```rust
pub(crate) fn cursor_in_loupe(&self) -> (f32, f32) {
    let (px, py) = (self.cursor.0 as f32, self.cursor.1 as f32);
    match self.loupe_viewport {
        Some((x, y, w, h)) => {
            let mut lx = px - x as f32;
            let ly = py - y as f32;
            if self.compare && self.mode == ViewMode::Loupe && w >= 2 && h > 0 {
                let half = (w / 2) as f32;
                if lx >= half {
                    lx -= half;
                }
            }
            (lx, ly)
        }
        None => (px, py),
    }
}
```
Left half unchanged (`0..half`); right half has `half` subtracted so it lands in the same local space. `zoom_at`, `fit_to_window`, `center`, `loupe_transform` need zero changes — they already consume `loupe_area()`/`cursor_in_loupe()` output generically.

### Task 3 — `push_compare()` (app.rs:2566-2579)

```rust
fn push_compare(&mut self) {
    let after = self.current_adjustments();
    let before = Adjustments {
        crop: after.crop,
        ..Adjustments::default()
    };
    let (scale, offset, rot) = self.loupe_transform();
    let (gpu_before, gpu_after) = (self.gpu_adjust(&before), self.gpu_adjust(&after));
    if let Some(r) = &mut self.renderer {
        r.set_transform(scale, offset, rot);
        r.set_adjustments(gpu_before);
        r.set_adjustments_b(gpu_after);
    }
}
```

### Task 4 — call site (app.rs:3038-3051)

```rust
if self.compare && self.mode == ViewMode::Loupe {
    if let Some((x, y, w, h)) = image_viewport {
        if w >= 2 && h > 0 {
            let half = w / 2;
            self.push_compare();
            primary_vp = Some((x, y, half, h));
            compare_vp = Some((x + half, y, w - half, h));
        }
    }
}
```

### Task 6 — `toggle_compare()` (app.rs:2581-2593)

```rust
fn toggle_compare(&mut self) {
    if self.mode != ViewMode::Loupe {
        return;
    }
    self.compare = !self.compare;
    if self.fitted {
        if self.crop_edit.is_some() {
            self.fit_for_crop();
        } else {
            self.fit_to_window();
        }
    } else {
        self.center();
        self.push_transform();
    }
    if !self.compare {
        self.push_adjustments();
    }
    self.request_redraw();
}
```
Known minor trade-off: if the user was zoomed into a specific detail off-center, toggling Y recenters rather than preserving that exact focal point — zoom level itself is preserved, only pan resets to centered. A focal-point-preserving version can be added later if it bothers the user in practice.
