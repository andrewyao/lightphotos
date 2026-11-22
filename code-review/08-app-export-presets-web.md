# src/app — export, presets, web, geotag

## Scope

Every line of these files was read:

| File | Lines |
|---|---|
| `src/app/export.rs` | 1666 |
| `src/app/presets.rs` | 1607 |
| `src/app/web.rs` | 1735 |
| `src/app/geotag.rs` | 916 |
| `src/app/geotag/cameras.rs` | 171 |
| `src/app/geotag/source.rs` | 319 |
| `src/app/geotag/walk.rs` | 56 |
| `src/app/geotag/tests.rs` | 942 |
| Total | 7412 |

Cross-references read to confirm contracts (not reviewed in full): `src/export/mod.rs` (`Exporter`, `stem`), `src/persist/presets.rs` (`rename`, `unique_name`, `writable`), `src/persist/preset_folders.rs` (`scan`, `LIMITS`), `src/app/agent.rs` (`confine_agent_export`, `ExportConfine`, `agent_refusal`), `src/app/folder_store.rs` (`save_patch`), `src/jobs/loader.rs` (`forget_loupe_decodes`), `src/navigation.rs` (`list_subdirs`, `index_of`), `src/paths.rs` (`drop_unlisted_children`), and `src/geotag/gpx.rs` (size cap).

## Summary

The code is in good shape. Non-test code has no panics: nothing indexes unchecked, input-derived sizes are capped (GPX bytes, file count, depth, entries, preset file bytes, camera count), and recursion is bounded. Agent export is confined carefully: the destination is checked before and after the folder is made, and Immich is allowed only when the form already uploads there. Stale async results are dropped by generation counters in most of the web code. The geotag worker threads are well built: bounded walks, `catch_unwind` around the photo decoders, symlinked folders not followed, and sidecar writes that don't depend on the open folder.

The main risks are small state-machine gaps, not crashes:
- The browser Loupe's failure bookkeeping is never reset.
- A few "batch finished" paths depend on invariants held only by callers.
- A dead preset-scan thread leaves a folder "Scanning" forever.

Findings: 0 Critical, 0 High, 1 Medium, 8 Low, 4 Info.

## Findings

### [APP-OUT-01] Browser Loupe failure and retry state is never cleared  — Severity: Medium — Confidence: Medium
- Location: `src/app/web.rs:1210`, `:1272`, `:1444` (inserts); the sets are declared at `:174-188`. `invalidate_web_thumb_handles` (`:301-306`) clears only the thumbnail maps.
- Evidence: `preview_failed`, `speed_failed`, `full_failed`, `preview_retries`, `speed_retries` and `full_retries` have no `.clear()` or `.retain()` anywhere in `web.rs` (only `:303-305` and `:508-511` clear or retain anything, and none of those are these sets). The native loader clears its equivalents (`view_failed`, `full_failed`) in `Loader::forget_loupe_decodes` (`src/jobs/loader.rs:1572`). `App::forget_loupe_decodes` (`src/app/mod.rs:1359`) has no web counterpart, and neither does a re-pick of the same folder (`poll_folder_pick`, `:517`).
- Impact: After `MAX_READ_RETRIES` transient Chrome `NotReadableError`s, or one decode failure, `(path, preview_px)` is failed for the rest of the session. `web_loupe_reads` never asks for it again. `loupe_is_loading` (`:1470`) then reports "loading" for a RAW indefinitely. Re-picking the folder, changing a decode setting or leaving the Loupe doesn't help, because the keys depend only on path and size and the same paths come back. The three failed sets and three retry maps also grow without bound over a long session.
- Suggested fix: Clear all failed, retry and in-flight-key state in the same place the loader forgets Loupe decodes, via a `Web::forget_loupe(&mut self)` called from `App::forget_loupe_decodes` and `forget_folder_decodes`. Also clear the failed and retry maps for a root in `drop_web_root_handles`.

### [APP-OUT-02] `export_with` deliberately lets an empty batch through, and an empty batch never finishes  — Severity: Low — Confidence: High (latent; no caller reaches it today)
- Location: `src/app/export.rs:323-329` (the `.filter(|&why| why != nothing_selected)`), `:760-771`, `:791-792`; `src/main.rs:556-559`.
- Evidence: `export_with` drops the "nothing selected" blocker, then `start_export` sets `progress = Some(ExportProgress { total: 0, .. })`. `on_export_outcomes` runs only when a frame drains at least one outcome (`main.rs` `if !outcomes.is_empty()`, and `web.rs:354`). `prog.done >= prog.total` is therefore never evaluated for `total == 0`.
- Impact: Today `Photos::Selected` and `Named` go through `resolve` (`commands.rs:160`), which returns `NoPhotos` when empty, and the agent passes `vec![photo]`. A future caller with an empty `Vec` would leave `export_busy()` true forever. The form stays disabled, every later export is blocked with "export in progress", and `batch_running` pins the status toast. Only a restart clears it.
- Suggested fix: Return `Err(nothing_selected)` for an empty batch, or make `start_export` early-return when `paths.is_empty()`. If the filter exists for a reason, add a comment saying why.

### [APP-OUT-03] A preset scan thread that dies leaves folders "Scanning" forever  — Severity: Low — Confidence: Medium
- Location: `src/app/presets.rs:376-405` (`poll_preset_scans`, the `Disconnected => break false` arm at `:389`), `:487-505` (`spawn_scan`).
- Evidence: The scan thread sends one result per folder. If it panics, the receiver sees `Disconnected`, `preset_scan` is set to `None`, and every folder not yet reported keeps `FolderState::Scanning`. Nothing converts those leftovers. `scan()` parses untrusted `.xmp` files and has no `catch_unwind`. `find_preset` also reads `preset_scan.is_some()` to say "still being read".
- Impact: One malformed preset file that trips a parser bug leaves the folder shown as scanning indefinitely, with no error and no retry, until the next rescan (which would hit the same file).
- Suggested fix: When the channel disconnects, mark every folder still `Scanning` as `Unavailable("the scan stopped")`. Optionally wrap each `scan()` call in `catch_unwind`, as `read_photo` in `geotag.rs` already does.

### [APP-OUT-04] The finished album add rewrites the form's `New` album choice even if the batch used a different album  — Severity: Low — Confidence: Medium
- Location: `src/app/export.rs:941-949` (`poll_album_add`); `AlbumAdd` is defined at `:120-124`.
- Evidence: The batch's album is captured in `ExportProgress.album` and passed to `start_album_add`, but `AlbumAdd` doesn't keep it. `poll_album_add` checks `matches!(self.exports.settings.album, AlbumChoice::New(_))` against the form's current settings.
- Impact: `Command::Export` with `settings` (the drive/control path, `commands.rs:456`) can use an album that differs from the form's. If the form holds `New("Trip")` and the command uploads into `Existing(X)`, the form is silently switched to `Existing(X)`. The reverse also applies: a different `New` is turned into `Existing` of the wrong album. The agent path pins `album` to the form's, so it is unaffected.
- Suggested fix: Store the batch's `AlbumChoice` in `AlbumAdd`. Update the form only when that choice was `New` and the form still holds that same `New(name)`.

### [APP-OUT-05] An album add that fails to spawn is shown with the export's Success kind  — Severity: Low — Confidence: High
- Location: `src/app/export.rs:849-854`, `:899-901`.
- Evidence: `start_album_add` returns `Err((t.album_failed)(&summary, &e))` when the thread can't spawn, and the caller maps `Err(summary) => (kind, summary)`. The "nothing to add" `Err(summary)` shares that arm, so a failure message gets whatever `kind` the upload had (usually `Success`).
- Impact: A failure to start the album add is shown in the green style.
- Suggested fix: Return a three-way result (no-op, started, failed), or return `(StatusKind, String)` from the failure case.

### [APP-OUT-06] An unavailable saved export folder resets the setting, and the next save makes it permanent  — Severity: Low — Confidence: Medium
- Location: `src/app/export.rs:34-46`.
- Evidence: A `Custom(dir)` target that `!dir.is_dir()` at launch is replaced with the default target. Any later settings change saves the replaced value (`set_export_settings`, `save_export_edits`).
- Impact: Launching with an external or network drive unmounted loses the user's chosen export folder the first time they touch any export setting. The existing comment calls this intended. `is_dir()` on a dead network mount also runs on the main thread in `App::new`.
- Suggested fix: Keep the saved value on disk and apply the fallback only for the session (for example `effective_target()`), or show the missing folder in the form instead of replacing it.

### [APP-OUT-07] Web thumbnail results do a linear scan of the playlist each  — Severity: Low — Confidence: High
- Location: `src/app/web.rs:815-822`.
- Evidence: `playlist.entries().contains(&path)` runs on the UI thread for every landed thumbnail. `Playlist::index_of(name)` (binary search, `src/navigation.rs:534`) exists.
- Impact: O(n²) path compares over the life of a folder load: about 10k comparisons per result for 10k photos, so roughly 10^8 over a full folder. It is a cost on the main thread of a wasm build that is already memory and CPU constrained, but not a hang.
- Suggested fix: Use `index_of(file_name)` and compare the parent, or keep a `HashSet<PathBuf>` built at `load_playlist`.

### [APP-OUT-08] Two folder presets with the same group and name can't be applied by command  — Severity: Low — Confidence: High
- Location: `src/app/presets.rs:214-247` (the `many =>` arm at `:239`).
- Evidence: The error says "N folder presets are named `x`: ...; give its `group`". Duplicates are possible even with the group, for example two preset folders that both have `Film/Warm` or the same `crs:Group`. Their `label()` strings are then identical and `group` can't disambiguate.
- Impact: A `preset.apply` agent or drive command has no way to pick either. The hint is wrong for this case.
- Suggested fix: When the labels are identical, say so ("same group and name in two folders"), or accept the folder or file path as a disambiguator.

### [APP-OUT-09] The geotag dialog drops a chosen camera clock when later batches change how cameras fold  — Severity: Low — Confidence: Low
- Location: `src/app/geotag.rs:355-363` (`set_camera_clock`), `:486-514` (`pump_dialog`), `:707-733` (`camera_rows`).
- Evidence: `chosen` is keyed by the folded `CameraKey` at the time of the click. The dialog lets rows be edited while `reading` is still true; only commit waits. If the first batches hold only serial-less JPEGs of a model, the user edits `(Z 8, None)`. When a later batch brings the numbered RAW, `fold_serials` maps the rows to `(Z 8, Some("3001"))`, `chosen.get(&camera)` misses, and the row reverts to its default.
- Impact: A silently lost edit, visible in the row but easy to miss before pressing Import. Narrow: it needs a race against the reader.
- Suggested fix: Disable the row controls while `reading`, or store the choice by model and re-key it after folding.

### [APP-OUT-10] A failed unrelated folder listing cancels the web session restore  — Severity: Low — Confidence: Low
- Location: `src/app/web.rs:1639-1643`.
- Evidence: On any failed `poll_dir_listing` result (including `prime_visible_subdirs` or `refresh_dir_listing` for another directory), `session_restore = None`, regardless of whether that listing was the chain the restore waits on.
- Impact: The saved folder and photo may not be reselected after a stray listing error at startup.
- Suggested fix: Cancel the restore only when the failed `dir` is in `session.folder_chain(..)`.

### [APP-OUT-11] Geotag worker results do filesystem checks and queue writes on the UI thread  — Severity: Low — Confidence: Medium
- Location: `src/app/geotag.rs:554-559` (`path.exists()` per located photo) and `:562-572`; `GpxFile` loading in `src/app/geotag/source.rs:287-302`.
- Evidence: `pump_events` drains every queued `Matched` batch (up to 256 photos each) in one frame and calls `exists()` for each. In a slow or offline network share each call can stall. `source::load` reads up to `MAX_FILES` (1000) GPX files of up to 64 MiB each, keeping all parsed tracks before merging, with no aggregate byte cap.
- Impact: A very large run over a network volume can hitch the UI. A hostile or oversized GPX set can need many GiB (1000 × 64 MiB of XML before the DOM). Per-file limits hold, but the sum isn't bounded.
- Suggested fix: Move the existence check into the worker (it already walks the folder), or check once per batch directory. Add a total-bytes cap in `load`.

### [APP-OUT-12] The geotag tests use `std::os::unix` without a `cfg(unix)` gate  — Severity: Info — Confidence: High
- Location: `src/app/geotag/tests.rs:757`, `:809`.
- Evidence: `std::os::unix::fs::symlink` appears in two tests, and the module has no `cfg(unix)` gate. `geotag` is compiled on Windows (`#[cfg(not(target_arch = "wasm32"))]` only). CI's Windows job only runs `cargo clippy --bin lightphotos` (not `--all-targets` and not `cargo test`), so this isn't caught there.
- Impact: `cargo test` on Windows would fail to compile.
- Suggested fix: Gate those two tests with `#[cfg(unix)]`, as `presets.rs:769` already does for its symlink test.

### [APP-OUT-13] Misleading and stale doc comments  — Severity: Info — Confidence: High
- Location: `src/app/export.rs:665-671`.
- Evidence: Two `///` blocks are split by a `#[cfg]`. The first ("Queue `paths` ... Returns at once; ... The caller has checked `export_blocker`") predates the `Result` return and `confine`, and does not match the checks `export_with` now does.
- Impact: It misleads the next reader about the contract.
- Suggested fix: Merge into one doc block describing the `confine` and `Result` behavior.

### [APP-OUT-14] Preset lookup is case-sensitive while the library enforces case-insensitive uniqueness  — Severity: Info — Confidence: High
- Location: `src/app/presets.rs:210`; contrast with `unique_name`/`taken` in `src/persist/presets.rs:226-251`.
- Evidence: `find_preset` matches `p.name == name`. The library refuses two names that differ only in case.
- Impact: `preset.apply "golden"` fails with a list that contains "Golden". The error lists the names, so it is recoverable.
- Suggested fix: Compare with `eq_ignore_ascii_case` as a second pass when there is no exact match.

### [APP-OUT-15] The Keychain lookup runs on the UI thread  — Severity: Info — Confidence: Medium
- Location: `src/app/export.rs:396-413` (`resume_immich` -> `persist::secret::load_api_key`), `:472-477` (`save_api_key`).
- Evidence: Both are called synchronously from `toggle_export_form`, `set_export_settings` and `poll_immich_connect`.
- Impact: A Keychain prompt or a slow secret service blocks a frame. The deferral to first use is a good mitigation.
- Suggested fix: Move the lookup into the same worker thread that does the connect.

## Test coverage assessment

Covered well:
- Presets: apply with one-undo semantics, name prompts, rename, import of good and bad files, an oversized file, a cancelled picker, the folder cap, and a stale scan being cancelled by a rescan. The UI tests (`click`, `settled`) drive real clicks through the page.
- Export form and album state: album state transitions, the blocker ordering, the form staying up through a batch, and typing only saving on blur or Enter.
- Geotag: the end-to-end run and clear, the depth and entry caps, iCloud placeholders, download timeouts, the dialog's per-camera memory, paired and unpaired clocks, hidden and linked folders, and batching. Hostile prefs are tested for `CameraClocks::parse`.

Gaps:
- `web.rs` has no tests. It is wasm-only and depends on handles, so most of it can't be unit tested. The pure policies (retry/backoff, the failed-set lifecycle behind APP-OUT-01, the dir-listing generation logic) could be extracted behind a small struct and tested natively.
- No test where a preset scan thread dies (APP-OUT-03).
- No test for `export_with` with an empty path list (APP-OUT-02).
- No test for `start_export`'s confine, create-then-recheck path. This is the security-relevant code in `export.rs`. `agent.rs` tests `confine_agent_export` only.
- No test for `poll_immich_connect` success or failure (needs a fake server). The Immich round trip is `#[ignore]` and needs a real server.
- Geotag: no test that a located photo trashed mid-run is skipped (`path.exists()`), no folder-switch-mid-run test (the claim in the module doc), and no panic test for `read_photo`'s `catch_unwind`.
- Several tests would pass even if the behavior regressed. For example `an_album_add_keeps_or_raises_the_export_error` only checks the status kind, and `a_cancelled_picker_says_nothing` is correct but passes by early return.

## Things done well / verified-safe areas

- No `unwrap`/`expect`/indexing on input in non-test code across all eight files. `get()`, `saturating_*` and `checked_*` are used for input-derived math (`retry_backoff`, `summary`, `suggested_preset_name`).
- Agent export confinement (`start_export` folder branch) checks the folder both before and after `create_dir_all`, and Immich is allowed only when the form already targets it. `Command::ImportGpsTracks`, `AddPresetFolder` and catalog commands are user-only.
- Geotag source walk is bounded in depth (8), files (1000), entries (20k), and per-file size; symlinked directories aren't followed in either `source::Walk` or `PhotoWalk`; hidden folders and `.lightphotos` catalogs are skipped; a decoder panic costs one photo.
- Geotag writes go through `save_patch` and never touch the photo files; the job slot is shared so a run and a clear can't overlap.
- The preset library write path goes through `PresetStore`, whose `corrupt` state refuses to overwrite a bad file (`writable()`), and import reads files with a size cap.
- Web: async results carry generation tags (`handle_generation`, `loupe_generation`, `nav_generation`); read concurrency is budgeted with a shared counter; Chrome's transient-read failure is retried with jittered backoff.
- Immich connection is on a worker with connect/send/recv timeouts; failed connect hands the typed fields back; an HTTP URL warns in the form.
- Status toasts: the `keep_status_alive` rule that revives only a live toast is documented and tested.

## Open questions for maintainers

1. Is the `nothing_selected` filter in `export_with` (`export.rs:325`) relied on by anything? If not, removing it closes APP-OUT-02.
2. Should the browser's `preview_failed` and `full_failed` survive a re-pick of the same folder? The native loader clears its equivalents on a folder change; the browser looks like an oversight rather than a decision (APP-OUT-01).
3. Is it intended that `geotag.clear` (allowed for agents) walks every subfolder of the open folder and drops GPX locations without confirmation? It matches the module doc but is the one destructive agent command in this unit.
4. Should a re-run of the geotag import overwrite an existing GPX location in the sidecar? Today `has_gps` reflects the photo file only, so a second run with a different clock replaces earlier GPX locations. That is probably intended ("fix the clock and rerun"), but it isn't stated in the dialog or tally.
5. On Windows, is `cargo test` expected to build? `tests.rs` currently wouldn't (APP-OUT-12).
