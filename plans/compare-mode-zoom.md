# Zoom/pan support in compare mode (Y)

## Context

Compare mode (Y key) shows the current image twice, side by side: left half = "before" (identity tone, same crop), right half = "after" (current edits). It's not two different photos — same crop, same geometry, only tone differs.

Today compare mode has no working zoom. `push_compare()` recomputes a fresh "contain fit" transform (`fit_transform_for`) every single frame from scratch, ignoring `self.zoom`/`self.pan` entirely. Mouse wheel/pinch/drag handlers do mutate `self.zoom`/`self.pan` even while comparing (they're not gated on `self.compare`), but that gets clobbered on the very next frame — so zooming during compare currently has no lasting visual effect.

Root cause: `self.zoom`/`self.pan`/`loupe_area()`/`cursor_in_loupe()` are all keyed to the *full* loupe rect, but while comparing, each half only occupies half that width. Fix: make the shared geometry state compare-aware so both halves render from one live `zoom`/`pan`, correctly anchored regardless of which half the mouse is over — satisfying "always show the same portion of the image" (single shared crop/zoom/pan) and "zoom anchored at whichever half the mouse is on."

All changes are in `src/app.rs`. No changes needed in `src/main.rs` (verified: wheel/pinch/drag handlers already route through `cursor_in_loupe()` / `self.pan` deltas and need no gating changes once the primitives below are fixed).

## Changes

**1. `loupe_area()` (app.rs:2418-2425)** — return half-width when comparing:
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
`w >= 2` guard mirrors the existing render-split guard at the call site (step 5) so zoom math and actual GPU viewport split never disagree. `w / 2` integer division must match the split's `let half = w / 2;` exactly (avoids off-by-one between the two halves).

**2. `cursor_in_loupe()` (app.rs:2512-2522)** — map right-half cursor into the same local space as the left half:
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
Left half: unchanged (`0..half`). Right half: `half` subtracted so it lands in the same `0..half` local space `loupe_area()` now uses. This is what makes `zoom_at`'s cursor-anchored zoom correct no matter which half the mouse is over — `zoom_at`, `fit_to_window`, `center`, `loupe_transform` need zero changes, they already consume `loupe_area()`/`cursor_in_loupe()` output generically.

**3. `push_compare()` (app.rs:2566-2579)** — stop re-fitting every frame, use the live shared transform:
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
Drops the `(half_w, half_h)` params. `loupe_transform()` already reads `self.zoom`/`self.pan`/`loupe_area()` — now correctly half-width via step 1. Crop-sharing (`before.crop = after.crop`) untouched.

**4. Call site (app.rs:3038-3051)** — drop now-unused args:
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

**5. Delete `fit_transform_for` (app.rs:2548-2560)** — dead code after step 3 (only caller was `push_compare`; confirmed no other references).

**6. `toggle_compare()` (app.rs:2581-2593)** — refit/recenter on toggle, since flipping `self.compare` silently changes what `loupe_area()` returns (full width ↔ half width) with no viewport-resize event to trigger the usual per-frame refit path:
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
If the view was "fitted", refit to the new area (mirrors the existing per-frame resize-refit branch). If the user had manually zoomed (not fitted), `center()` at the current zoom level rather than reusing stale `pan` (which was computed against the old area width and would leave the image shoved off-center or half-blank). Known minor trade-off: if the user was zoomed into a specific detail off-center, toggling Y recenters rather than preserving that exact focal point — zoom level itself is preserved, only pan resets to centered. Acceptable for this change; a focal-point-preserving version can be added later if it bothers the user in practice.

## Verification

- `cargo build --release` (or debug) to confirm it compiles clean, no leftover `fit_transform_for` references.
- `cargo test` — no existing unit tests should be affected (this logic isn't in `#[cfg(test)]`-covered pure modules).
- Manual test via `./target/release/lightphotos /path/to/photo.jpg`:
  - Open an image with visible edits (adjust exposure/tone so before/after differ), press Y to enter compare.
  - Scroll-zoom with mouse over the left half — confirm zoom anchors at the cursor and both halves zoom in lockstep (same crop region, before/after tone only differs).
  - Move mouse to the right half, scroll-zoom again — confirm it anchors correctly there too (no jump/offset when crossing from left to right).
  - Drag-pan while comparing — confirm both halves pan together.
  - Press Y again to exit compare — confirm the single loupe view re-fits/recenters sanely, not corrupted pan.
  - Toggle Y while already zoomed in (not fitted) — confirm recentered view at the same zoom level, no half-blank pane.
  - Resize the window while in compare mode — confirm both halves stay in sync and correctly split.
