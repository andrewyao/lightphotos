# Fix: saved adjustments not visible until Y (compare toggle)

## Context

Opening an image that has saved develop adjustments doesn't visually apply them
in the Loupe. The image (both the thumbnail placeholder and the full-res
decode) renders as if `Adjustments::default()` (identity) — until the user
presses `Y` (before/after compare toggle), at which point the correct look
appears and stays correct from then on. This happens on every entry path:
opening a single file, opening a folder then entering Loupe, and stepping
prev/next with arrows.

Confirmed via the user's own testing (not guessed):
- The Develop panel sliders show the *correct* saved values immediately —
  only the rendered image looks unadjusted.
- Both the thumbnail placeholder and the full decode are affected, not just one.
- It's fixed not only by pressing `Y`, but equally by nudging any develop
  slider by a tiny amount. This is an important data point: it rules out
  anything specific to `toggle_compare`/`push_compare`/`push_transform` as the
  fix mechanism. A slider nudge goes through a completely different path
  (`apply_adjustments` → `push_adjustments`, `app.rs:1801-1814`) that never
  touches `self.compare` or the transform uniform at all. The one thing every
  fixing action has in common is simply: **a second call to
  `push_adjustments()`/`renderer.set_adjustments()` after the one that ran at
  image-load time.** The first call's write doesn't visibly take effect; a
  second one (from any source) does, and it sticks permanently after that.

That last point matters: sliders and the GPU push both read the exact same
function, `App::current_adjustments()` (`src/app.rs:1763`), which is a pure
read of `self.edits` keyed by `self.shown.path()`. If the sliders are right,
`self.edits` and `self.shown` are already correct at paint time — so the bug
is not in catalog loading, path-key matching, or `is_identity()` filtering.
It has to be in how/when the *GPU-side* uniform (`adj_buf` / `adj_bind`) gets
written relative to the frame(s) that actually present, or in something the
render path does differently on the very first push vs. later ones.

## What's already been ruled out (static trace, two independent passes + manual read of every call site)

- `seed_mirrors` (`app.rs:575`) populates `self.edits` from the catalog
  *before* `load_selected()`/`try_show()` ever runs, for every entry-path
  (fresh file open, folder→Loupe, and folder-wide seeding covers prev/next
  stepping too since the whole playlist is seeded up front).
- `upload_shown` (`app.rs:1707`) unconditionally calls `push_adjustments()`
  right after `renderer.set_image(...)` and `self.shown = ...`, for both the
  thumbnail and full-image paths — this is the sole path that ever displays
  an image.
- `push_adjustments()` (`app.rs:1818`) writes `GpuAdjust` via
  `renderer.set_adjustments()` → `queue.write_buffer(&adj_buf, ...)`
  (`renderer.rs:418`) — same buffer the primary (non-compare) render pass
  binds (`renderer.rs:553`, `adj_bind`).
- `resize()` (`renderer.rs:328`) only reconfigures the surface — it never
  touches `adj_buf`/`adj_bind`, and `Renderer::new` (`main.rs:71`) is
  constructed exactly once per app lifetime (guarded by
  `if self.window.is_some() { return; }` in `resumed()`), so there's no
  buffer-recreation-wipes-the-write theory either.
- `push_transform()` (`app.rs:2601`) only touches `xform_buf`, never `adj_buf`
  — so it can't be silently clobbering adjustments before/after `fit_to_window()`.
  runs `push_transform()` after `push_adjustments()` in `upload_shown`, but
  that's a different buffer.
- The per-frame render call (`app.rs:3010-3061`) only takes the
  `compare_vp = Some(...)` branch (drawing a second time with `adj_bind_b`)
  when `self.compare && self.mode == ViewMode::Loupe` — properly guarded, so
  it isn't unconditionally painting over the correct image with the
  (identity-default) `adj_bind_b`.
- `GpuAdjust`'s `#[repr(C)]` field layout (`develop.rs:219`) matches the WGSL
  `Adjust` struct (`shader.wgsl:17`) field-for-field, 80 bytes, 16-byte
  aligned — no layout/alignment mismatch.
- The shader (`shader.wgsl:130`) applies `adj.*` unconditionally; there's no
  hidden "enabled" flag gating tone/crop application.

In short: every place that *should* make this work, does, by static reading.
That means the remaining explanation is almost certainly a **timing/ordering
issue between when `push_adjustments()` runs at image-load time and when a
frame that actually reads `adj_buf` gets submitted/presented** — something
that only a live repro with instrumentation will pin down. Given the slider-
nudge data point above, the bug isn't in any particular caller
(`toggle_compare` isn't special) — it's specifically that the *first*
`push_adjustments()` call after `upload_shown` doesn't result in a presented
frame with the new uniform value, while literally any *second* call to it
does. That points at the redraw/present scheduling right after image load
(`upload_shown` → `request_redraw()` → the next `WindowEvent::RedrawRequested`
→ `App::redraw()` → `renderer.render()`), not at the adjustments data or the
shader.

## Plan

1. **Instrument, don't guess further.** Add temporary `eprintln!` (or a debug
   log) in three places, run the app against a real image with saved
   adjustments, and capture the sequence:
   - `push_adjustments()` (`app.rs:1818`): log the call site context (e.g. via
     a `&str` tag threaded in, or just distinguish by which caller you add the
     print to) and the `GpuAdjust.exposure`/`contrast` values being pushed.
   - `Renderer::set_adjustments()` (`renderer.rs:418`): log the values
     actually written to `adj_buf`.
   - The per-frame render call (`app.rs:3061`, right before
     `renderer.render(...)`): log `primary_vp`/`compare_vp` and whether this
     is the frame where the image texture just became available
     (`self.shown`).
   This will show definitively whether (a) `push_adjustments()` is ever
   called with identity when it shouldn't be, (b) it's called correctly but
   a *later* call before the first real paint re-clobbers it with identity
   from somewhere not yet found, or (c) the correct data is written but the
   frame that presents it is somehow the *previous* (pre-write) swapchain
   image — a genuine wgpu frame-latency/double-buffering issue, which would
   point at needing an extra forced redraw after `upload_shown` rather than a
   data-layer fix.

2. **Apply the fix based on what's actually observed.** Don't pre-commit to a
   diff before that evidence exists — the leading candidates, in order of
   likelihood given everything ruled out above:
   - A one-frame-late uniform write relative to presentation: fix by forcing
     a second `request_redraw()` / render pass after `upload_shown` (mirroring
     what `toggle_compare`'s two-call sequence does), or by moving
     `push_adjustments()` to run once more on the *next* frame after image
     load.
   - Some call path that runs after `upload_shown`'s `push_adjustments()` but
     before first present, writing identity to `adj_buf` — once the
     instrumentation shows *where*, remove/reorder that call.

3. **Verify end-to-end**: open a file with saved non-identity adjustments
   directly from Finder/CLI, from folder→Loupe, and via prev/next stepping —
   confirm the correct look renders on the very first frame, with no `Y`
   press needed. Remove the temporary debug logging before committing.

## Files involved

- `src/app.rs` — `upload_shown`, `push_adjustments`, `toggle_compare`,
  `push_compare`, the per-frame render call (~line 2990-3061).
- `src/renderer.rs` — `set_adjustments`, `render`.
- No changes expected in `src/develop.rs` or `src/shader.wgsl` (layout and
  application logic there are already correct).
