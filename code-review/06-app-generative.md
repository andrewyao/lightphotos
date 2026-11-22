# src/app — generative fill, comfy, agent, coverage

## Scope

Every line of these files was read:

| File | Lines |
|---|---|
| `src/app/generative.rs` | 1118 |
| `src/app/fill.rs` | 783 |
| `src/app/comfy.rs` | 912 |
| `src/app/agent.rs` | 804 |
| `src/app/coverage.rs` | 1179 |

I also read the callers and callees needed to check contracts. These were `comfy/client.rs` (`wait`, `cancel`, `normalize_url`), `persist/folder_store.rs` (`set_fills`, `update`, `refuse`, `cache_key`), `paths::resolve`, `paths::export_target`, `app/control.rs` tests, `app/export.rs` (blocker), `app/masks.rs` (`kick`, `mask_input_of`), `app/keys.rs` and `app/adjust.rs` (Touch Up undo).

## Summary

The code is careful and defensive. I found no `unwrap`, `expect`, indexing or slicing panics, and no non-test `std::fs::write` in non-test code. Strings are cut at char boundaries, and the stroke point count is capped. The agent anchor policy is thorough and well tested in `app/control.rs`: symlink escapes, `..`, export folder confinement and the album/copyright pin all have tests.

The real risks are state-lifecycle issues around the long-running generation job and the coverage store's export gate:
- A generation that finishes after a folder switch is silently dropped with a misleading error.
- Export can stay blocked forever in a read-only folder that has recorded detected masks.
- A few smaller lifecycle leaks: a stale picked index, cancel latency, and queued ComfyUI prompts that are not cancelled after a timeout.

Findings: 0 Critical, 0 High, 2 Medium, 7 Low, 5 Info.

## Findings

### [APP-GEN-01] A generation that lands after the folder changed is dropped, with a misleading error — Severity: Medium — Confidence: Medium
- Location: `src/app/generative.rs:684-756` (`land_job`), `:769-825` (`poll_generative`). Callee: `src/persist/folder_store.rs:452-458, 874-878`.
- Evidence: Nothing cancels `Generative::job` when the folder changes. `grep` shows `cancel_generation` is called only from Esc in `keys.rs`. When the job lands, `land_job` calls `edit_fills` → `store_fills` → `folder_store.set_fills(&job.path, ..)`. For a photo outside the open folder, `FolderStore::update` skips `refuse` (`!in_open_folder`) and `apply_change` takes the `cache_key == None` branch. That branch calls `note_persist_error("<path> is not in the open folder")` and returns.
- Impact: A user starts a generation (up to 20 min of GPU), opens another folder, and the fill lands. The patches were already written and `hand_in`'d, but the fill record is never saved. The user gets no "your fill was discarded" notice, only a persist-error side channel with a developer-ish message. `edit_fills` also pushes a `TouchStep::Fills` onto `touchup_undo` for the stale path, and `unpruned` gains an entry for it. The patch files are left as orphans until the sweep.
- Suggested fix: On folder change, set `job.stop` and drop the job (or at least drop the result). Or, in `land_job`, check `self.folder_store.takes_result(&job.path)` first and show an explicit status such as "the folder changed while generating; fill discarded". Skip the undo and `unpruned` side effects in that case.

### [APP-GEN-02] Export can stay blocked forever in a read-only folder with a recorded detected-mask coverage — Severity: Medium — Confidence: Medium
- Location: `src/app/coverage.rs:409-417` (`is_ready`), `:424-439` (`kick`). Consumers: `src/app/export.rs:265-272`, `src/app/masks.rs:348, 405`.
- Evidence:
  - `is_ready` calls `need(..., in_memory = self.read_only_photo(photo))`. For a read-only photo with a recorded id and no slot, `need` returns `Need::Load(id)`, so `is_ready` is `false`.
  - `export_blocker_for` returns "masks detecting" while `pending_among > 0`.
  - The doc on `is_ready` says "Starts nothing; the frame loop's `kick` ... starts what this reports pending, so pending never stands." But `kick` never loads: "Never loads, since the selection can outgrow `ready_budget`". Only `readiness` (a bake or thumbnail already running) starts a load.
- Impact: A folder opened read-only (a locked volume or a mount) whose sidecar already records Sky/Subject detections gives `Need::Load` for each such photo not in the store. Export of a selection containing them stays blocked until some other path (a thumbnail bake for a visible photo, or showing the photo) loads each one. Photos that are selected but never scrolled into view never load, so Export hangs with a message that says "detecting". The regression test `a_selection_past_the_budget_loads_nothing...` covers only the browser flag (`store.in_memory = true`), where `is_ready` ignores `in_memory`. No test covers read-only plus recorded.
- Suggested fix: Make `kick` start loads for read-only photos that the export blocker waits on, with a bound such as the selection size capped to what fits in `ready_budget`. Alternatively, have `is_ready` treat a read-only recorded coverage as `Ready` (as it does natively), and have the export's own bake loading cover it (it already waits via `mask_input_of` returning `None`). Add a read-only plus recorded plus `pending_among` test.

### [APP-GEN-03] Queued ComfyUI prompts are cancelled only on a user stop, not after a timeout, LOST or error — Severity: Low — Confidence: High
- Location: `src/app/generative.rs:185-219` (`run`).
- Evidence: All `MAX_VARIANTS` prompts are queued up front. The loop calls `server.cancel` for outstanding ids only when `patch` is `Err(STOPPED)`. A `wait` timeout ("took too long"), `LOST`, or a fetch/decode error moves on or ends the loop without cancelling.
- Impact: After the 20 min deadline, or if variant 1 fails for a non-stop reason, the remaining prompts stay in the ComfyUI queue and burn GPU and disk (output images) after the app has already reported a failure. A second Generate then waits behind them.
- Suggested fix: After the loop, cancel (best effort) every queued id whose result was not consumed, for any early exit. A small `Drop` guard on the queued set would do it.

### [APP-GEN-04] Cancel doesn't free the UI until the job thread finishes its cancel round-trips — Severity: Low — Confidence: Medium
- Location: `src/app/generative.rs:585-591` (`cancel_generation`), `:194-217` (`run`), `:812-824` (`poll_generative`).
- Evidence: Esc only sets `stop`. The `Job` stays `Some` until the thread returns. The thread notices `stop` on its next poll, then does up to 3 × 2 blocking POSTs (`cancel`), each with `timeout_recv_response` of 60 s.
- Impact: With a hung or slow server, "Generating" and the disabled brush persist for minutes after Esc. `begin_fill_stroke` is also refused because `job.is_some()`.
- Suggested fix: On cancel, detach the job immediately: drop `Job` (keeping the thread to finish by itself) and restore the Kept stroke. Or use short timeouts for `cancel`.

### [APP-GEN-05] `picked` can go stale across Touch Up close/reopen — Severity: Low — Confidence: Medium
- Location: `src/app/generative.rs:740-742` (`land_job`), `:771-781` (`poll_generative`), `:574-576` (`fill_picked`).
- Evidence: `picked` is cleared only on the open→close edge, in `set_generative`, and on stroke start. `land_job` sets `picked = Some(index)` whenever the job's photo is shown, including after Touch Up closed (the `!open` branch at `:816-819` clears `painting` and prunes, but not `picked`). Nothing resets `picked` on photo change or on reopen.
- Impact: Land a job while Touch Up is closed, navigate to another photo, then reopen Touch Up. `picked` is an index into a different photo's fills, so a different fill appears selected, and Delete (`fill_picked`) deletes it. In the tests the same scenario is fine only because it never navigates.
- Suggested fix: Key `picked` by path (`Option<(PathBuf, usize)>`), or clear it on the open edge and in the `!open` landing branch.

### [APP-GEN-06] A download that completes after the models folder changed marks the file installed in the new folder — Severity: Low — Confidence: Medium
- Location: `src/app/comfy.rs:334-340` (`set_models_dir`), `:644-667` (`poll`).
- Evidence: `set_models_dir` sets the cancel flag and starts a scan of the new folder, but `running` stays until the thread returns. If the download finished before it saw the flag, `Ok(())` runs `self.installed.insert(file)` without checking `cancelled`.
- Impact: The gate can say the model is installed for the new folder when it isn't. The probe against the ComfyUI server should catch it ("missing model"), but the Settings row shows "Installed" until the next scan.
- Suggested fix: Insert into `installed` only if `!cancelled`, or call `scan_models()` instead.

### [APP-GEN-07] The web PatchStore's `clear` can leave `busy()` true forever — Severity: Low — Confidence: Medium
- Location: `src/app/fill.rs:153-158` and `:162-176`.
- Evidence: `ask` does `in_flight += 1` and pushes onto `wanted`. `clear()` drops `wanted` without decrementing `in_flight`. Only keys already handed to the app (`take_wanted`) later send a `Landing` that decrements.
- Impact: If a folder switch lands between a frame's `ask` and the next `poll_fills`, `in_flight` stays above 0 and `fills_loading()` keeps the frame loop polling (battery burn on wasm).
- Suggested fix: In `clear`, `self.in_flight = self.in_flight.saturating_sub(self.wanted.len())` before clearing. Add a test.

### [APP-GEN-08] Read-only folders keep every generated variant in memory for the session — Severity: Low — Confidence: Medium
- Location: `src/app/fill.rs:330-354` (`hand_in`), `src/app/generative.rs:666-679` (`prune_fill_variants`).
- Evidence: `Unsaved::patches` only grows. Pruning to the chosen variant trims the fill record, not `unsaved.patches`. `Fill` patches can be up to 16 MiB each.
- Impact: Each Generate in a read-only folder retains 3 × up to 16 MiB for the session, and `MAX_FILLS` per photo, across all photos.
- Suggested fix: When `keep_fills` is called, drop `patches` entries not named by any variant in the new fills. Prune at the same moment as `prune_fill_variants`.

### [APP-GEN-09] The native PatchStore queues loads with no cancellation — Severity: Low — Confidence: Low
- Location: `src/app/fill.rs:377-409`, `:420-445`.
- Evidence: One worker, loads are submitted for every chosen patch of every photo asked. Switching photos clears `slots` but not the worker queue. A flick through 50 photos with fills decodes up to 16 MiB PNGs for photos no longer shown, and `drain` only drops them afterwards.
- Impact: Latency before the current photo's fill appears, plus wasted decode, when navigating quickly.
- Suggested fix: Generation-tag the jobs and have the worker skip stale ones, or only ask for the shown photo (which it already does) while letting the pool drop queued jobs on a photo change.

### [APP-GEN-10] A saved ComfyUI server URL can be any http(s) host, and the photo crop is uploaded to it — Severity: Info — Confidence: High
- Location: `src/app/comfy.rs:321-332` (`set_server`), `src/comfy/client.rs:96-112` (`normalize_url`); the upload is at `src/app/generative.rs:181-182`.
- Evidence: Only the scheme is validated. Docs and module comments say "a ComfyUI server running on the same computer". Nothing enforces loopback, and `https` or plain `http` to a LAN or remote host is accepted.
- Impact: The user typed the URL themselves, so this is consent-by-config. But a crop of their photo goes out in plaintext to whichever host is saved, and a prefs-file edit can redirect it.
- Suggested fix: Decide on a policy: either document that remote servers are allowed, or warn once when the host isn't loopback, or restrict to loopback plus private ranges.

### [APP-GEN-11] The agent photo guard fails open when it cannot resolve photos or has no playlist — Severity: Info — Confidence: Medium
- Location: `src/app/agent.rs:231-247` (`photos_outside_anchor`).
- Evidence: `self.resolve(cmd.photos()?).ok()?` and `self.playlist.as_ref()?` both return `None`, meaning "no objection". Today `run` re-resolves and errors in the same way, so no escape exists.
- Impact: The invariant "the guard and the executor resolve the same way" is implicit. A future command that takes paths itself could pass unchecked.
- Suggested fix: Return `Some(reason)` when a command names photos but they can't be resolved. Or document the invariant and test it.

### [APP-GEN-12] An impossible match arm is left in `run_from_agent` — Severity: Info — Confidence: High
- Location: `src/app/agent.rs:409-414`.
- Evidence: The `Command::NewCatalog(_) | ... => {}` arm is "refused above" and unreachable. CLAUDE.md says to restructure rather than leave an impossible arm.
- Suggested fix: Make `agent_refusal` return an enum or the command class, so the match is total without a dead arm.

### [APP-GEN-13] The link table is a snapshot, so a photo swapped for a symlink after listing is not noticed — Severity: Info — Confidence: Low
- Location: `src/app/agent.rs:238-240, 299-310` (using `Playlist::links`).
- Evidence: `links` is computed from the listing only. `holds_photo` canonicalizes at check time, but only for photos already marked as links.
- Impact: A local actor who swaps a regular file for a symlink after the folder is listed bypasses the link check until the folder reloads. A TOCTOU needing local write access to the user's own photo folder; low value.

### [APP-GEN-14] Detached background results and leftover state at shutdown and disable — Severity: Info — Confidence: Low
- Location: `src/app/comfy.rs:478-514`, `:623-642`.
- Evidence: `scan_models` is not cancelled by `set_enabled(false)`. A detected-folder scan that finishes later still calls `save_pref(MODELS_PREF, ..)` and sets `models_dir`, even though "nothing here runs until ComfyUI is enabled". `models_dir` read from prefs (`load`, `:252`) is not required to be absolute, unlike `detect_models_dir_in`.
- Impact: A pref written after the user turned the feature off; a relative path in prefs would download into the process's cwd.
- Suggested fix: Drop `self.scan` in `set_enabled(false)`, and reject non-absolute `MODELS_PREF` values on load.

### [APP-GEN-15] Minor lifecycle and clarity points — Severity: Info — Confidence: Medium
- `src/app/generative.rs:710-734`: if the job target is refused after `hand_in` (`MAX_FILLS`, or the replaced fill is gone), the patches were already handed in and written. They remain in memory and on disk as orphans until the sweep. This is harmless, but easy to avoid by checking the target first.
- `src/app/generative.rs:530, 706`: `Generate More` uses the prompt currently typed in the panel, not the picked fill's stored prompt, and then replaces the stored prompt. Check that this is intended.
- `src/app/generative.rs:786-790`: `settings_was_open` is assigned twice; the second assignment is redundant.
- `src/app/coverage.rs:80-83`: a doc comment is split by a `#[cfg]` attribute. `src/app/coverage.rs:936-941`: the doc for `readiness_starts_a_detection...` is stuck on the read-only test.
- `src/app/coverage.rs` (wasm): `CoverageStore::clear` leaves `wanted_detects`. Entries for the old folder are never run, since `can_run` is false for them, and they leak until the app exits.
- `src/app/comfy.rs:123-139`: `save_pref`/`load_pref` short-circuit on `cfg!(test)` inside non-test code, so persistence of these prefs is untestable.

## Test coverage assessment

- **Generative** (`generative.rs`) has good behavior tests: variant settling and dedup, fill landing and pin, partial failure notice, failed job keeps the stroke, undo through fills, Generate More replacement and deleted-while-generating, pruning on Touch Up close, and a long drag.
  - Not covered:
    - the folder-switch-during-job case (APP-GEN-01)
    - cancel latency
    - cancelling queued prompts on timeout (the `run` loop is untested because there is no server stand-in at this layer)
    - `picked` across photos
    - `begin_fill_stroke` refusals (the MAX_FILLS path, aspect NaN)
- **Fill store** (`fill.rs`) is well tested for native load, drop of stale loads, corrupt file removal except read-only, and the web store's want-once, budget and eviction. It lacks a `clear` plus busy test and a read-only memory-growth test.
- **Comfy** (`comfy.rs`) has a thorough gate-ordering test against a stub server, and detection of the models folder with hostile settings. It lacks tests for a download finishing after a folder change, for scan/disable races, and for pref validation.
- **Agent** (`agent.rs` plus `control.rs`) is the best covered: anchor reach, links, `..`, one-photo anchor, export confinement, album/copyright/keep pinning. The only gap is the fail-open behavior (APP-GEN-11).
- **Coverage** (`coverage.rs`) tests are behavior-based: stale load dropped, retry, I/O vs corrupt, read-only per folder, selection over budget. The read-only plus recorded plus export-blocker combination (APP-GEN-02) is not covered.

## Things done well / verified-safe areas

- No panics found: the prompt cap cuts at `char_indices`; `cycle_fill_variant` uses `n.max(1)` and saturating subtraction; aspect is checked finite and positive; `set_ready` eviction can't spin because it is guarded by `ready_order.len() > 1`; `Disconnected` channel arms are handled, so a panicked worker thread becomes an error rather than a hang.
- The agent policy resolves symlinks and `..` before comparing (`paths::resolve` rejects relative paths and `..`). The export is confined by resolved path, the album/copyright/keep options are pinned to the form, and the naming template is sanitised. Exports can't clobber existing files because `export_target` picks a free name.
- Corrupt coverage and patch files are only deleted from folders that are not read-only.
- ComfyUI client calls are capped, with no redirects, and the downloader is one-at-a-time with cancel.
- Pruning of fill variants also rewrites the Undo stack, so no undo brings back a variant without its file.

## Open questions for maintainers

1. Should a running generation survive a folder switch, or be cancelled? If it should survive, where should the result go (APP-GEN-01)?
2. In read-only folders, is it intended that the export blocker waits for a recorded coverage to be loaded, and who is meant to load it (APP-GEN-02)?
3. Is a remote (non-loopback) ComfyUI server a supported configuration? If so, should it be https only or warn (APP-GEN-10)?
4. Should `Generate More` reuse the fill's own prompt (APP-GEN-15)?
