# src/app — adjust, crop, masks, optics, autotone

## Scope

Every line of these files was read:

| File | Lines |
|---|---|
| `src/app/adjust.rs` | 1881 |
| `src/app/crop.rs` | 1498 |
| `src/app/masks.rs` | 1421 |
| `src/app/optics.rs` | 589 |
| `src/app/autotone.rs` | 1157 |

Cross-references were checked in `develop/mod.rs` (`Crop::sanitized`, `Straighten`), `develop/brush.rs`, `app/history.rs`, `app/histogram.rs`, `app/coverage.rs`, `app/folder_store.rs`, `app/nav.rs`, `jobs/loader.rs`, and `app/mod.rs` (`apply_ui_actions` read-only gating).

## Summary

Overall quality is high. Non-test code is free of `unwrap`, `expect` and unchecked indexing. The crop code is defensive: it uses the `Crop::sanitized` contract and has regression tests for `clamp` panics. The undo and batch design (open step held across frames, `here` and `undoable` flags, window-capped thumbnail requests) is careful and well tested. I found no Critical or High issues.

The biggest risk is `optics.rs`: the background measurement thread has no panic guard, so one bad decode wedges Remove CA for the session. The rest are Medium and Low state-consistency and UX hazards:

- The Touch Up undo stack drifts from the main history.
- Undo cancels a running batch even when it has nothing to take back.
- Reset does not cancel queued CA measurements.
- A near-full crop is silently discarded.
- `poll_coverage` copies the whole selection on every frame.

Findings: 0 Critical, 0 High, 3 Medium, 7 Low, 3 Info.

## Findings

### [APP-DEV-01] Remove CA measurement thread has no panic guard, so a panic wedges the feature for the session — Severity: Medium — Confidence: High
- Location: `src/app/optics.rs:229-244` (spawn), `src/app/optics.rs:315-337` (`poll_remove_ca`), `src/app/optics.rs:65-67` and `:121-123`.
- Evidence: the `chroma-measure` thread runs `decode_at_size(...)` then `chroma::measure(&img)` and only afterwards does `tx.send((path, scale))`. There is no `catch_unwind`. If the decode or measure panics, the thread unwinds without sending. `optics.pending` stays `Some(..)` forever, because it is only cleared when a result arrives for that path or on cancel.
- Impact / failure scenario: CLAUDE.md names a "malformed JPEG, raw" as something that must never take the app down, and relies on the `catch_unwind` around decode jobs (`jobs/loader`, `worker_pool`). This thread bypasses that net. On a malformed file, the panic leaves `pending` stuck.
  - `is_running()` stays true, so the frame loop polls forever.
  - `remove_ca_pending_here()` stays true, which holds the Remove CA control.
  - `start_next_measure` never advances: the `while pending.is_none()` loop is never entered, so every later request queues behind the dead entry.
  - `leave_folder` only clears `here`, not `pending`.
  - Only an undo (`cancel_remove_ca`), or turning it off for that exact path (if the control is not held), clears it.
- Suggested fix: wrap the closure body in `catch_unwind(AssertUnwindSafe(..))` and send `Err("decode panicked")`. Alternatively send from a drop guard. Add a test with an injected panicking measure, as `worker_pool` has.

### [APP-DEV-02] Touch Up's private undo stack drifts from the main history — Severity: Medium — Confidence: Medium
- Location: `src/app/adjust.rs:827-841` (`edit_touchups` pushes `TouchStep::Spots`), `:683-707` (`apply_touchups` does not call `history.note_touchups`), `:712-734` (`put_touchups` does), `:511-519` (`apply_step` restores spots), `:865-875` (`undo_touchup`).
- Evidence: spot adds and deletes record only into `touchup_undo[path]`. Paste (`put_touchups`) and global Cmd+Z (`apply_step`) change `self.touchups[path]` without touching `touchup_undo`.
- Impact: add spot A, then paste settings that replace the spots with B, C. Cmd+Z restores the pre-paste spots (empty). With the tool armed, `undo_touchup` then pops `Spots([])` and applies it, which wipes the spots without a redo. Conversely, after a global undo of a paste, the next `undo_touchup` replays a pre-undo snapshot. `touchup_selected` is also left pointing at an index in a list that has been replaced by `apply_step`, so Delete can hit the wrong spot.
- Suggested fix: drop `touchup_undo[path]` (or truncate it) whenever `put_touchups` or `apply_step` replaces a photo's spots, and clear `touchup_selected` in `apply_step`. Better, route spot add and delete through `history.note_touchups` too so there is one stack.

### [APP-DEV-03] Cmd+Z cancels a running batch even when the open step has nothing from it, and also undoes an unrelated older step — Severity: Medium — Confidence: Medium
- Location: `src/app/adjust.rs:458-469` (`undo_edit`), `src/app/autotone.rs:414-431`, `src/app/optics.rs:183-189`.
- Evidence: `undo_edit` calls `cancel_auto_tone()` and `cancel_remove_ca()` unconditionally, before `pop_undo()`. The rationale is "undo takes back the running batch's step". That holds only after the batch has landed at least one result. Before the first result lands (large selection, slow decode, 8 ms of analysis per photo), the open step is empty, and `close()` discards empty steps.
- Impact: the user starts Auto Tone on 500 photos, then presses Cmd+Z to take back their previous slider tweak. The batch is cancelled and the previous tweak is also undone, so two actions happen for one keypress. The status text only says "stopped".
- Suggested fix: cancel only if `history` holds a non-empty open step that the batch owns. Otherwise undo the step and leave the batch running. Alternatively make Cmd+Z during a batch cancel only the batch, and say so.

### [APP-DEV-04] Reset does not cancel queued Remove CA measurements, so CA reappears after Reset — Severity: Low — Confidence: Medium
- Location: `src/app/adjust.rs:326-347` (`reset_adjustments`) versus `src/app/optics.rs:133-153`.
- Evidence: Reset builds `Adjustments { crop, straighten, ..default() }`, which clears `chromatic_aberration`. It does not drop the photo's `Measure` from `optics.queue` or `pending`. `set_ca_for(false)` and the paste path (`pasting_optics_off_cancels_queued_measurements`) do cancel.
- Impact: during a Remove CA run, Reset clears CA on the selection, then the queued or in-flight result lands via `store_measure` and silently switches CA back on.
- Suggested fix: have Reset call `set_ca_for(&paths, false)` or retain-filter the optics queue. Put the cancel next to a shared "clear CA for paths" helper so Reset, paste and toggle-off cannot diverge.

### [APP-DEV-05] A cancelled measurement's thread is abandoned, so measurements can overlap despite "one at a time" — Severity: Low — Confidence: Medium
- Location: `src/app/optics.rs:137-152`, `:183-189`, `:193-209`.
- Evidence: cancel sets `pending = None` but the OS thread keeps decoding at `MEASURE_PX` 2560. `start_next_measure` immediately spawns the next one. The struct comment says one is measured at a time "since each decode can take a large share of memory".
- Impact: toggling Remove CA on and off, or undoing repeatedly on large RAWs, stacks several concurrent decodes. A stale result for path P can also be adopted by a new pending entry for the same P (`take_if(|m| m.path == path)`). That is harmless today because the content is identical, but it is accidental.
- Suggested fix: add an epoch or generation token to the outcome tuple and compare it on receipt. Keep a "thread outstanding" flag so the next spawn waits for the abandoned one to report.

### [APP-DEV-06] A near-full crop is silently saved as "no crop" — Severity: Low — Confidence: High
- Location: `src/app/crop.rs:380-391` (`draft_adjustments`).
- Evidence: `is_full` is true when `straighten == 0.0 && left <= MIN_CROP && top <= MIN_CROP && right >= 1-MIN_CROP && bottom >= 1-MIN_CROP`, with `MIN_CROP = 0.02`. The edge drag clamps allow any value in `[0, right - MIN_CROP]`.
- Impact: the user drags all four edges in by 1.5% (about 90 px on a 6000 px photo) and presses Enter. The crop is stored as `None` and the trim is lost. The straighten case has a test (`a_small_turn_keeps_its_box`); the untilted case has none. The comment says only "full-frame crops store as None".
- Suggested fix: treat as full only when every edge is within a tiny epsilon (e.g. 1e-4) of the frame. Otherwise keep the user's box.

### [APP-DEV-07] `poll_coverage` copies the whole selection on every frame — Severity: Low — Confidence: High
- Location: `src/app/masks.rs:346-348`; the call site is `src/main.rs:618`.
- Evidence: `let selection = self.action_paths();` clones every selected `PathBuf` each pass of the event loop, then `coverage.kick` iterates them. `action_path_refs()` exists precisely for readers that run every frame (`nav.rs:343`).
- Impact: with Select All on a large folder (tens of thousands of photos), this is tens of thousands of allocations per loop turn, on the UI thread, even when no masks exist anywhere.
- Suggested fix: change `kick` to take an `impl Iterator<Item=&Path>` and pass `action_path_refs()`. Better, skip it entirely when `self.masks` is empty.

### [APP-DEV-08] Brush stroke does O(stroke data) plus a full raster conversion for every pointer move — Severity: Low — Confidence: Medium
- Location: `src/app/masks.rs:99-117` (`Painting::stamp_from`), `:609-630` (`extend_stroke`).
- Evidence: per new point it clones all prior strokes (`before.iter().chain(..).cloned().collect()`, up to 8000 points of `Vec` data) and runs `raster.coverage()`, a byte conversion of up to `MAX_BRUSH_TEXELS = 1<<20` plus `Coverage::fit`. It then hashes every point in `brush::remember` and calls `push_adjustments`, which re-uploads the mask set.
- Impact: roughly 1 ms or more of UI-thread work per mouse-move event, scaling with how much is already painted. Likely fine today. It may stutter on high-polling-rate mice or large masks, and it is wasted work when several moves arrive in one frame.
- Suggested fix: coalesce to one `stamp_from` and one upload per frame (mark the painting dirty and flush it in the frame loop), and avoid re-cloning the unchanged prefix, for example by keeping `before` and the live stroke separate until the release.

### [APP-DEV-09] Batch photos that fail to decode are dropped with no message — Severity: Low — Confidence: Medium
- Location: `src/app/autotone.rs:357-366` (`dropped`), `:386-389`, `:468-491` (`report_auto_tone_progress`).
- Evidence: a photo whose thumbnail failed, or whose grid is empty, is removed from `pending` and never counted. If every remaining photo is dropped, `total == 0` and `report_auto_tone_progress` returns without setting any status. The toast expires with no notice that photos were skipped. Photos skipped because the user hand-edited them (`base != current`) are also silent.
- Impact: "Auto Tone 40 of 500" disappears and the user believes the batch finished. A Success toast reports only the toned count.
- Suggested fix: count skipped and failed photos, and show "n photos skipped" in the final status. Use `Info` or `Error` as appropriate.

### [APP-DEV-10] Auto Tone's single `undoable` flag misattributes carried-over photos — Severity: Low — Confidence: Medium
- Location: `src/app/autotone.rs:107-110`, `:285-286`, `:444-452`.
- Evidence: `undoable` is a batch-global bool. `leave_folder` clears it so carried-over results skip history. But `enqueue_auto_tone` sets it back to true for any new request, including one in the new folder while the old folder's photos are still queued. Those old-folder results then go through `put_edit`, which notes them in the new folder's open step. `apply_step` filters them out by `in_folder` on undo. Meanwhile `holds_undo_step` stays on until they drain. Optics got this right with a per-item `here`.
- Impact: undo of a new-folder batch cannot take back old-folder results (correct), but the step is held open longer and the old photos' base snapshots inflate the step. It is also not obvious from the code.
- Suggested fix: mirror `Measure.here` with a per-path flag, or clear `queue` and `window` entries for the old folder on `leave_folder`.

### [APP-DEV-11] Auto Tone window can stall if a requested thumbnail is evicted before analysis — Severity: Low — Confidence: Low
- Location: `src/app/autotone.rs:349-366`, `:318-329`.
- Evidence: a window path with no thumbnail and not `thumb_failed` is pushed back to `waiting` indefinitely. Nothing re-requests it, because `request_thumb` is only called when the path is admitted to the window. `Loader::request_thumb` also silently does nothing on `Enqueued::NoWorkers`.
- Impact: if the LRU thumbnail cache evicts a landed-but-not-yet-analysed thumbnail (heavy Grid scrolling during a 5 ms-budgeted batch), or the loader has no workers, that photo stays in the window for the rest of the session. `pending` never drains, `batch_running()` stays true, and the status toast is kept alive forever.
- Suggested fix: when a window path's thumbnail is absent and the loader is not loading it, call `request_thumb` again. The call is idempotent. Alternatively time out photos that have been in the window for N frames.

### [APP-DEV-12] `AutoTone::forget` leaves the photo in `queue` and `window` — Severity: Info — Confidence: High
- Location: `src/app/autotone.rs:130-133`.
- Evidence: only `pending` and `base` are cleared. A trashed photo still in `queue` is later requested from the loader (a decode of a missing file that fails), and one in `window` is analysed (about 8 ms) before `still_wanted == false` discards it.
- Impact: wasted work only. The result is correctly discarded.
- Suggested fix: `queue.retain` and `window.retain` in `forget`.

### [APP-DEV-13] Developer-facing state handling nits — Severity: Info — Confidence: Medium
- `set_touchup_feather`, `set_touchup_opacity` and `set_touchup_radius` (`adjust.rs:254-274`) clamp but do not map NaN to a default, unlike `BrushSettings::clamped` (`masks.rs:54-64`). `f32::clamp(NaN)` returns NaN and does not panic. The callers are UI-only today, but a NaN would flow into a saved `TouchUp`.
- `auto_tone_batch` reports the "waits for the folder store" notice as `StatusKind::Error` (`autotone.rs:260-263`). It is informational, so `Info` fits better.
- `apply_step` does not clear or revalidate `mask_selected` after restoring masks (`adjust.rs:520-523`). `selected_mask()` guards the length, but the index can silently point at a different mask.

## Test coverage assessment

Strong:
- `crop.rs` tests cover reshaping, edge drags with a fixed ratio, Shift lock, straightening, hostile saved crops, hair-under-`MIN_CROP` boxes, rotation round-trips and ratio inference. They are behavioural and drive real code.
- `masks.rs` tests cover stroke caps, point caps, mid-stroke delete, undo and photo change, undo steps, detection landing outside undo, and paste.
- `autotone.rs` tests cover the window cap, frame budget, folder switching, undo of a batch, and a hand edit mid-batch.
- `adjust.rs` tests cover sync, undo granularity, and the write-on-release behaviour.

Gaps:
- No test that a panicking or failing measure thread recovers (APP-DEV-01). The only failure tested is an empty file that returns `Err`.
- No test for undo while a batch has landed nothing yet (APP-DEV-03).
- No test for the Reset-during-CA-run interaction (APP-DEV-04).
- No test mixing `touchup_undo` with `put_touchups` or global undo (APP-DEV-02).
- No test that a small untilted crop persists (APP-DEV-06).
- Straighten combined with a quarter-turn rotation is untested. `straighten_preview` and `level_angle` use source-pixel deltas while the user draws in canvas UV. I believe they are rotation-invariant, but nothing asserts it.
- `apply_step` is exercised only through full flows. No test covers restoring touchups or masks for a photo that is not shown.
- Several `autotone` tests insert `app.edits` directly instead of going through the edit path, which weakens the proof of `put_edit` bookkeeping.

## Things done well / verified-safe areas

- Crop clamps are all panic-safe: `Move` uses `0.0..1.0-cw` (safe because `Crop::sanitized` is applied at deserialize), edge clamps use `.max(0.0)` and `.min(1.0)` guards, and `reshaped` uses `min/max` instead of `clamp`. `locked_spans` clamps `MIN_CROP/k..1/k` with `k` finite and positive for all reachable inputs.
- Brush point and stroke accounting: `points_left` saturates and `last = len - 1` is only taken after a successful push. The `kept_raster` reuse is guarded by `Arc::ptr_eq` and aspect equality. The global raster cache is a bounded LRU, so intermediate rasters cannot leak without bound.
- `put_masks` and `store_masks` handle undo or edits mid-stroke correctly: the stroke is saved first on edit, or dropped on undo.
- Autotone memory is bounded by the 32-photo window and a compile-time assertion. Analysis has a per-frame budget with guaranteed progress of at least one photo. Hand-edit detection uses a base snapshot.
- Results for photos in other folders go through `save_patch`, which writes a field-level patch, so the sidecar keeps the rest of its record (rating and other fields).
- `pixel_at` and `neutralize_gray` are safe for NaN and zero-size grids. `image_size()` falls back to `(1,1)`, so UV-radius divisions cannot divide by zero.
- Read-only gating is centralised in `apply_ui_actions` and `key_edits_folder`. The direct `enqueue_auto_tone` call in `auto_tone_selected` is covered by that gate.
- No `unsafe` and no `std::fs::write` in these files. Sidecar writes go through the folder store.

## Open questions for maintainers

1. In the Grid, `current_adjustments()` follows `shown`, while `action_paths()` follows the selection. `grid_develop_needs_load` bridges the gap. Between a selection change and the new preview landing, can a slider drag still edit the previous `shown` photo? (`apply_adjustments` targets `shown`; `step_slider` and `edit_each` target the selection.)
2. Is dropping the crop draft in `teardown_loupe_state` (navigation abandons the draft rather than committing it) intentional? Lightroom commits on photo change.
3. `enter_crop` shrinks a saved box that the saved straighten no longer covers (`fit_inside`). Committing without touching anything then stores the shrunk box. Is that silent rewrite intended?
4. Should Touch Up add and delete go through `history.note_touchups`, so the separate `touchup_undo` stack can go away? Generative fills share that stack (`generative.rs`), so the answer affects fill undo too.
