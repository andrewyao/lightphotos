# src/app: loupe, fullscreen, histogram, thumbs, faces, score

## Scope

Every line of these files was read, plus the callers and callees needed to check contracts (`nav.rs` `load_selected`, `main.rs` frame loop, `jobs/score.rs`, `jobs/loader.rs` thumb and bake queues, `develop/brush.rs` coverage cache, `develop::Crop::sanitized`, `image_ops::downsample_linear`).

| File | Lines |
|---|---|
| `src/app/loupe.rs` | 1416 |
| `src/app/fullscreen.rs` | 1389 |
| `src/app/histogram.rs` | 216 |
| `src/app/thumbs.rs` | 894 |
| `src/app/faces.rs` | 366 |
| `src/app/score.rs` | 605 |

Total 4886 lines. Tests inside these files were read too, but about 40% of the lines are tests.

## Summary

This unit is in good shape against the CLAUDE.md rubric. I found no non-test `unwrap`, `expect` or `panic`, no unsafe, no raw file writes, and no unbounded recursion. Hostile numbers are handled well: `slide_view` and `face_view` reject NaN and non-positive sizes, `StripLayout` clamps bad aspects, `pixel_at` and `grid_working_range` use clamps and saturating math, and `Crop::sanitized` runs at deserialization. The real defects are state-machine and scheduling problems, not crashes.

- One real correctness bug: the Show Selection overlay is never computed for the photo you land on if you navigate while a segmentation is still running (APP-VIEW-01).
- One robustness gap: a panicking segmentation thread disables the feature for the session (APP-VIEW-02).
- A few performance and behaviour risks: the filmstrip working set when jumping far (APP-VIEW-03), and per-frame allocation churn in the thumbnail sync (APP-VIEW-04).
- The rest are Low or Info.

Counts by severity: Critical 0, High 0, Medium 1, Low 5, Info 3.

## Findings

### [APP-VIEW-01] Show Selection never computes the mask for the photo you navigated to while the previous photo's segmentation was running — Severity: Medium — Confidence: High
- Location: `src/app/loupe.rs:675-700` (`request_selection_mask`), `src/app/loupe.rs:703-719` (`poll_selection_mask`), caller `src/app/nav.rs:625-626`
- Evidence: `request_selection_mask` returns early on `self.selection_pending.is_some()`. `poll_selection_mask` clears `selection_pending` when the old photo's result lands, then does `if self.want.as_ref() != Some(&path) { continue; }`. It never calls `request_selection_mask` again. The only other callers are `load_selected` (once per navigation) and `toggle_selection`.
- Impact / failure scenario: The user turns on Show Selection (O) on photo A, and Vision takes about 100-500 ms. They press Right to photo B before it finishes. `load_selected` calls `request_selection_mask`, which is rejected because A is pending. A's result arrives and is discarded as stale, and pending is cleared, but nothing asks for B. The overlay stays empty and `selection_pending()` is false, so the UI reports "no subject" for B though it was never analysed. It recovers only on the next navigation or by toggling O twice. Rapid arrow-key culling with the overlay on hits this constantly.
- Suggested fix: at the end of the `while` loop in `poll_selection_mask`, call `self.request_selection_mask()` after clearing pending, so the current `want` is requested once the worker is free. A test should drive `selection_tx` directly: set `want` to B, send A's result, poll, and assert `selection_pending == Some(B)`.

### [APP-VIEW-02] A panicking segmentation thread wedges Show Selection for the rest of the session — Severity: Low — Confidence: Medium
- Location: `src/app/loupe.rs:690-699`
- Evidence: the thread closure calls `segmentation::segment(&want)` and then `tx.send(...)`. There is no `catch_unwind`, and no drop guard sends a failure.
- Impact / failure scenario: A malformed image that panics inside the Vision/CoreImage wrapper or the mask code kills the thread without sending. `selection_pending` stays `Some(path)` forever. Every later `request_selection_mask` returns early, so the feature is dead until restart. This is the same "a panic must still send an outcome" invariant that `WorkerPool` and `FacePool` document, but this one-off thread skips it.
- Suggested fix: wrap the call in `std::panic::catch_unwind` and send `Err("segmentation crashed")`. Alternatively, send from a drop guard.

### [APP-VIEW-03] Filmstrip working set unions the selection with a stale strip range, so a far jump asks for thousands of thumbnails — Severity: Low — Confidence: Medium
- Location: `src/app/thumbs.rs:586-599` (`strip_working_range`), consumed by `working_positions` (`:302`), `request_working_thumbs` (`:360`) and `working_thumb_keys` (`:342`)
- Evidence: `start = start.min(s); end = end.max((s + 1).min(len))`. The test `a_selection_outside_the_strip_range_still_comes_first_and_only_once` (`:792`) encodes a 12..51 range for a jump of 30 photos.
- Impact / failure scenario: In a 50k-photo folder, End or Cmd+Down moves `sel` to the last photo while `strip_range` still describes the top. The working set becomes the whole span, about 50,000 positions. `strip_range` follows the viewport during the 0.15 s scroll animation (`ui/loupe.rs:60-96`), so the oversized set persists for several frames. Each frame then:
  - clones 50k `PathBuf`s three or more times (`working_thumb_keys`, `request_working_thumbs`, `wanted_sig`);
  - hashes 50k edit signatures;
  - raises `thumb_capacity` to 50k via `set_thumb_working_set_size(reserved)` (`loader.rs:2104`);
  - rebuilds the loader's viewport queue with 50k keys (`set_viewport_thumbs`).
  This is a multi-frame hitch and a transient memory ceiling. The ordering via `load_order` keeps the actual decodes sensible.
- Suggested fix: widen only to a bounded window around the selection, `(s.saturating_sub(margin))..(s + margin + 1)`, as a second range rather than a union. Or cap the union at a few hundred positions. Add a test that a 50k folder with `sel` far from the range yields a bounded `working.len()`.

### [APP-VIEW-04] Per-frame allocation and clone churn in the thumbnail sync, including inputs built for bakes already in flight — Severity: Low — Confidence: High
- Location: `src/app/thumbs.rs:360-402` (`request_working_thumbs`), `:456-559` (`sync_thumb_textures`), esp. `:511-521`
- Evidence: every frame, even with nothing changed:
  - `request_working_thumbs` rebuilds `paths: Vec<PathBuf>` and compares it with the previous `Vec`.
  - `sync_thumb_textures` builds `wanted` and `wanted_sig` (two more `PathBuf` clones per cell), plus a `key` clone per cell.
  - For each edited cell whose texture is stale, it calls `mask_input_of(...)`, which clones `masks` and `detections`. It also calls `fill_input_of(...)`, which on native does `folder_store.fills(path).to_vec()` for `OnDisk`. Only then does `loader.request_bake` return early on `bake_inflight == Some(sig)` or `bake_failed`.
- Impact / failure scenario: A big window shows several hundred cells, giving thousands of small allocations per frame at idle. On a folder where many photos are edited and baking, every pending cell re-clones mask, detection and fill vectors each frame until its bake lands. A bake that failed permanently (`bake_failed`) keeps costing this every frame forever.
- Suggested fix: expose `Loader::bake_pending(path, px, sig)` and check it before building the inputs, so the inputs are built only when a request will really be queued. Key `thumb_tex` and `wanted_sig` by a cheap id or an `Arc<Path>`. Skip the whole sync when `loader.thumb_epoch()`, the `working_thumbs` key and an edit-generation counter are unchanged.

### [APP-VIEW-05] A scoring failure's reason is dropped; the user gets only a count — Severity: Low — Confidence: High
- Location: `src/app/score.rs:59-77`, `:113-116`
- Evidence: `o.result` is an `Err(String)` (for example "the edited preview is empty", or the masks resolve error). `job.finish(&o.path, o.result.is_ok())` records only a boolean, and `if let Ok(score) = o.result` discards the `Err`. The status line says "scored n of m" via `scored_partial`.
- Impact / failure scenario: A photo that can't be decoded, or whose mask coverage file is missing, is silently skipped with no way to see why. The same applies to the file-existence guard at `:65`: a photo that vanished mid-run is neither counted nor reported (it is counted as done and then skipped).
- Suggested fix: keep the first error string per run on `ScoreJob` and surface it in the Error status, or at least `eprintln!` the path and reason.

### [APP-VIEW-06] A late result from a cancelled run is accepted by the next run, and the genuine result is dropped — Severity: Low — Confidence: Medium
- Location: `src/app/score.rs:60-67`, `:128-138`; `src/jobs/score.rs` `ScoreJob::finish`
- Evidence: `cancel_scoring` takes `score_job`, but the pool still holds the cancelled photos on workers. A new run that includes the same path adds it to `in_flight` and submits it again. When the old worker's result arrives, `finish(path)` returns true because it only checks `in_flight.remove(path)`. It is stored and counted, and the new run's own result is later rejected (`false`).
- Impact / failure scenario: Cancel, edit the photo, then re-run on the same photo. The score stored is computed from the old edits with the old `edits` signature. It is shown as stale although the run reports done, and the correct result is thrown away. It is rare, since it needs the cancel and restart inside one decode's duration.
- Suggested fix: tag each `ScoreRequest` with a run id. `ScoreOutcome` would carry it back, and `finish` would reject mismatches. Alternatively, compare `o.edits` with `self.edit_sig_for(&o.path)` and re-queue on mismatch.

### [APP-VIEW-07] Histogram bins can be poisoned by a non-finite sample — Severity: Low — Confidence: Low
- Location: `src/app/histogram.rs:182-194`
- Evidence: `out[ch].clamp(0.0, 1.0)` leaves NaN as NaN. `pos = NaN`, `lo as usize` becomes 0 and `frac` is NaN. `bins[ch][0] += 1.0 - frac` and `bins[ch][1] += frac` then make those bins NaN. NaN is not an `Err`, and everything downstream (blur, max-normalise in `develop_panel`) inherits it.
- Impact / failure scenario: It needs `display_at` or `local::apply` to emit NaN or inf, for example from out-of-range slider values that bypass sanitizing, or inf in a RAW linear sample after a large exposure. Then the whole histogram goes blank or flickers, with no crash. I did not find a concrete NaN source in sanitized edits, so this is defensive.
- Suggested fix: `let v = if out[ch].is_finite() { out[ch].clamp(0.0, 1.0) } else { 0.0 };`

### [APP-VIEW-08] `zoom_at` divides by the current zoom with no zero guard — Severity: Info — Confidence: Medium
- Location: `src/app/loupe.rs:311-338` (compare `set_zoom` at `:349-354`, which does guard `cur > 0.0`)
- Evidence: `ipx = (cx - self.pan.0) / cur_zoom`. `fit_scale` is `ww / iw`, and `loupe_area()` falls back to `self.win_size`, which is not floored at 1 when `loupe_viewport` is `None` (the `Some` arm uses `.max(1)`).
- Impact / failure scenario: In a 0x0 window (minimised on Windows) with no viewport, `cur_zoom` is 0 and `pan` becomes NaN. `fitted` is then set true, and the next `fit_to_window` heals it, so there is no crash. A wheel event on a minimised window is unlikely.
- Suggested fix: floor `loupe_area()`'s fallback at 1x1 like the viewport arm, or return early from `zoom_at` when `cur_zoom <= 0.0`.

### [APP-VIEW-09] A negative result is not cached for Show Selection, and only one photo's mask is kept — Severity: Info — Confidence: Medium
- Location: `src/app/loupe.rs:661-719`
- Evidence: `current_selection` holds one `(PathBuf, Mask)`. An `Err("no subject")` sets it to `None`, and `invalidate_selection` drops it on every photo change.
- Impact / failure scenario: Stepping A, B, A with Show Selection on re-runs Vision for A each time, and a no-subject photo re-runs on every toggle. That is wasted Vision work, not a bug. It also means [APP-VIEW-01]'s fix will fire a request per navigation.
- Suggested fix: optionally keep a small LRU of masks keyed by path, or remember "no subject" per path for the session.

### [APP-VIEW-10] Structural notes: `tick_slideshow` is a 145-line state machine with many interacting timers — Severity: Info — Confidence: High
- Location: `src/app/fullscreen.rs:737-880`
- Evidence: the clock, decode wait, face wait, next-photo wait, dialog hold, step and fade are in one function. `show` (a `&mut` into `self.fullscreen`) stays live across `self.slide_view()` and `self.show_in_loupe(...)`, which works only because borrows end at last use.
- Impact / failure scenario: No bug found. The behaviour is well covered by about 40 integration tests in `keys.rs`. A change here is easy to get subtly wrong (for example the `held` or `began` resets).
- Suggested fix: split into small pure `decide_*` helpers, as `slide_view` and `face_view` already are, to allow table-driven tests.

## Test coverage assessment

- **loupe.rs** has good coverage of the pure math (`fit_scale_of`, `bounded_zoom`, `loupe_xform`, `zoom_outruns_preview`), the preview-to-full tier logic with a real `Loader`, and crop and rotation fit. It tests a NaN crop falling back. Gaps:
  - Nothing exercises the segmentation request and poll state machine (see APP-VIEW-01 and -02).
  - `cycle_zoom`, `toggle_compare` pan maths and `cursor_in_loupe` for compare halves are untested.
  - Brush and mask overlay sync (`sync_mask_overlay`) is untested in this file.
- **fullscreen.rs**: the pan, face-box, ease and fade maths are thoroughly tested, including NaN and inside-out boxes and a "box stays on screen" sweep. The state machine is tested in `keys.rs`, outside this unit. The `exit_fullscreen` and `restore_panels` interaction with Os states (`Asked/Already`) has only light coverage.
- **thumbs.rs**: working-range and load-order functions are well tested, including stale viewports. `StripLayout` is tested with bad aspects. `sync_thumb_textures` has no direct test: stale-sig handling, a baked result for an unwanted key, and free-on-prune are all untested, and it needs a renderer. `strip_working_range` has no upper bound test (APP-VIEW-03).
- **histogram.rs** is tested in `adjust.rs` for selection and crop behaviour. Nothing covers the straighten path, texture or clarity layers, or a non-finite sample.
- **faces.rs**: candidate selection, seeding rules and answer semantics are tested. `poll_face_quality`, the failed-set behaviour and the wasm give-up path are untested. `grouped_photos_are_scanned_once_per_rebuild` asserts that the `unscanned` flag is consumed even when there is no pool. That is implementation detail and would hide a regression where the flag is lost before the pool starts.
- **score.rs** has solid tests with an injectable runner: bounded queue depth, cancel, a folder change mid-run, stale scores and sort behaviour. Gaps: a failing runner (`Err`), a photo deleted mid-run, and a cancel-then-restart on the same path.

## Things done well / verified-safe areas

- Slide-show geometry never panics: `slide_view` and `face_view` guard non-finite and non-positive inputs, clamp `t`, and fall back to a plain pan.
- `ease_in_out` and `fade_opacity` are NaN-safe. `FACE_PATIENCE` and `LOAD_PATIENCE` bound every wait for a decode or analysis that never reports.
- Views stay anchored across a preview-to-full swap through `zoom_rel`. `image_size()` reads the shown photo's metadata when `want` has moved on, and that is tested.
- `upload_shown` re-fits only a still-fitted view for the same photo, so a sharper tier never yanks a manual zoom.
- `set_apple_tone_curve` resets `shown` to a thumbnail so a re-decode with different pixel semantics is not ignored by `try_show`'s `is_full_of` guard.
- Scoring never queues past the worker count (`capacity`), signatures are captured at submit time, so a score taken before an edit reads as stale, and results land in the photo's own folder after a folder switch.
- `histogram.rs` indexing is safe because `downsample_linear` guarantees `sample.len() == dw * dh`, `pixel_at` clamps, and the straighten path checks `0..1` before indexing.
- `faces.rs`: the wasm path gives up on old asks, `seed` refuses cache entries that lack frame or bounds, and a macOS-only cache write keeps SeetaFace answers out of Vision's cache.

## Open questions for maintainers

1. In `strip_working_range`, is the union with `sel` intended to cover the whole gap on a far jump, or only to keep the selected photo's own thumbnail loading? A bounded window would satisfy the doc comment ("so its thumbnail loads before the strip reports a range").
2. Is leaving `faces.failed` and `faces.quality` uncleared across folder changes deliberate? A photo that failed once (a transient read error) is never retried this session.
3. Should the scoring failure reason be shown to the user? Today a partial run says only how many succeeded.
4. `selected_mask_coverage` rehashes all brush strokes every frame on the Masks page through `brush::coverage` (a cache lookup keyed by an FNV hash). Is that acceptable for the 100k-point cap?
