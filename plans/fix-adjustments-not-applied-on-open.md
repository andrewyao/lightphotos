# Plan — Fix: saved adjustments not visible until Y (compare toggle)

Work through these top to bottom (or by worktree assignment, if parallel).
Check a box only after verification passes.

- [ ] Task 1: Instrument `push_adjustments()` — log the calling context (a `&str` tag threaded in, or one print per caller) plus the `GpuAdjust.exposure`/`contrast` values being pushed (files: src/app/adjust.rs)
- [ ] Task 2: Instrument `Renderer::set_adjustments()` — log the values actually written to `adj_buf` (files: src/renderer.rs)
- [ ] Task 3: Instrument the per-frame render call — log `primary_vp`/`compare_vp` and whether this is the frame where the image texture just became available (`self.shown`) (files: src/app/mod.rs)
- [ ] Task 4: Run against a real image with saved adjustments and capture the sequence; classify the failure as (a) `push_adjustments()` called with identity when it shouldn't be, (b) a later call re-clobbering it with identity before first paint, or (c) correct data written but the presented frame is the previous (pre-write) swapchain image — a frame-latency issue (requires Tasks 1-3) (files: —)
- [ ] Task 5: Apply the fix the evidence points to — do not pre-commit to a diff before Task 4 produces it (requires Task 4) (files: src/app/, src/renderer.rs)
- [ ] Task 6: Verify end-to-end on all three entry paths — open a file with saved non-identity adjustments directly from Finder/CLI, folder→Loupe, and via prev/next stepping; the correct look must render on the very first frame with no `Y` press (requires Task 5) (files: —)
- [ ] Task 7: Remove all temporary debug logging, then `cargo build --release` and `cargo test` clean before committing (requires Task 6) (files: src/app/, src/renderer.rs)

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

Line numbers below refer to the pre-refactor single-file `src/app.rs`/`src/ui.rs` (this analysis predates commit `6a917e3`, which split them into `src/app/` and `src/ui/`).

### Context

Opening an image that has saved develop adjustments doesn't visually apply them in the Loupe. The image (both the thumbnail placeholder and the full-res decode) renders as if `Adjustments::default()` (identity) — until the user presses `Y` (before/after compare toggle), at which point the correct look appears and stays correct from then on. This happens on every entry path: opening a single file, opening a folder then entering Loupe, and stepping prev/next with arrows.

Confirmed via the user's own testing (not guessed):
- The Develop panel sliders show the *correct* saved values immediately — only the rendered image looks unadjusted.
- Both the thumbnail placeholder and the full decode are affected, not just one.
- It's fixed not only by pressing `Y`, but equally by nudging any develop slider by a tiny amount. This is an important data point: it rules out anything specific to `toggle_compare`/`push_compare`/`push_transform` as the fix mechanism. A slider nudge goes through a completely different path (`apply_adjustments` → `push_adjustments`, `app.rs:1801-1814`) that never touches `self.compare` or the transform uniform at all. The one thing every fixing action has in common is simply: **a second call to `push_adjustments()`/`renderer.set_adjustments()` after the one that ran at image-load time.** The first call's write doesn't visibly take effect; a second one (from any source) does, and it sticks permanently after that.

That last point matters: sliders and the GPU push both read the exact same function, `App::current_adjustments()` (`src/app.rs:1763`), a pure read of `self.edits` keyed by `self.shown.path()`. If the sliders are right, `self.edits` and `self.shown` are already correct at paint time — so the bug is not in catalog loading, path-key matching, or `is_identity()` filtering. It has to be in how/when the *GPU-side* uniform (`adj_buf` / `adj_bind`) gets written relative to the frame(s) that actually present, or in something the render path does differently on the very first push vs. later ones.

### What's already been ruled out (static trace, two independent passes + manual read of every call site)

- `seed_mirrors` (`app.rs:575`) populates `self.edits` from the catalog *before* `load_selected()`/`try_show()` ever runs, for every entry path (fresh file open, folder→Loupe, and folder-wide seeding covers prev/next stepping too since the whole playlist is seeded up front).
- `upload_shown` (`app.rs:1707`) unconditionally calls `push_adjustments()` right after `renderer.set_image(...)` and `self.shown = ...`, for both the thumbnail and full-image paths — this is the sole path that ever displays an image.
- `push_adjustments()` (`app.rs:1818`) writes `GpuAdjust` via `renderer.set_adjustments()` → `queue.write_buffer(&adj_buf, ...)` (`renderer.rs:418`) — same buffer the primary (non-compare) render pass binds (`renderer.rs:553`, `adj_bind`).
- `resize()` (`renderer.rs:328`) only reconfigures the surface — it never touches `adj_buf`/`adj_bind`, and `Renderer::new` (`main.rs:71`) is constructed exactly once per app lifetime (guarded by `if self.window.is_some() { return; }` in `resumed()`), so there's no buffer-recreation-wipes-the-write theory either.
- `push_transform()` (`app.rs:2601`) only touches `xform_buf`, never `adj_buf` — so it can't be silently clobbering adjustments before/after `fit_to_window()`.
- The per-frame render call (`app.rs:3010-3061`) only takes the `compare_vp = Some(...)` branch (drawing a second time with `adj_bind_b`) when `self.compare && self.mode == ViewMode::Loupe` — properly guarded, so it isn't unconditionally painting over the correct image with the (identity-default) `adj_bind_b`.
- `GpuAdjust`'s `#[repr(C)]` field layout (`develop.rs:219`) matches the WGSL `Adjust` struct (`shader.wgsl:17`) field-for-field, 80 bytes, 16-byte aligned — no layout/alignment mismatch.
- The shader (`shader.wgsl:130`) applies `adj.*` unconditionally; there's no hidden "enabled" flag gating tone/crop application.

In short: every place that *should* make this work, does, by static reading. That leaves a **timing/ordering issue between when `push_adjustments()` runs at image-load time and when a frame that actually reads `adj_buf` gets submitted/presented** — something only a live repro with instrumentation will pin down. That points at the redraw/present scheduling right after image load (`upload_shown` → `request_redraw()` → the next `WindowEvent::RedrawRequested` → `App::redraw()` → `renderer.render()`), not at the adjustments data or the shader.

### Leading fix candidates (order of likelihood, pending Task 4's evidence)

- A one-frame-late uniform write relative to presentation: force a second `request_redraw()` / render pass after `upload_shown` (mirroring what `toggle_compare`'s two-call sequence does), or move `push_adjustments()` to run once more on the *next* frame after image load.
- Some call path that runs after `upload_shown`'s `push_adjustments()` but before first present, writing identity to `adj_buf` — once instrumentation shows *where*, remove/reorder that call.

### Files involved

- `src/app/` — `upload_shown`, `push_adjustments`, `toggle_compare`, `push_compare`, the per-frame render call.
- `src/renderer.rs` — `set_adjustments`, `render`.
- No changes expected in `src/develop.rs` or `src/shader.wgsl` (layout and application logic there are already correct).
