# src/app — folder store, folders, catalog, bulk delete

## Scope
Every line read:
- `src/app/folder_store.rs` (1723 lines: about 1305 non-test, the rest tests)
- `src/app/folders.rs` (1386 lines: 138 non-test, the rest tests)
- `src/app/catalog.rs` (979 lines: 368 non-test)
- `src/app/web_catalog.rs` (339 lines, wasm32 only)
- `src/app/bulk_delete.rs` (1308 lines: 487 non-test)

Cross-references checked: `persist/catalog/mod.rs` (`Record`, `land_record`, `land_presets`, `record_marks`), `persist/folder_store.rs` (`apply_loaded`, `marks_revision`, `dir_marks`, `forget_photos`), `app/adjust.rs` (`apply_step`), `develop/side_file.rs` and `jobs/thumbnail.rs` (sweeps), `shell/trash.rs`, `main.rs` (poll cadence), `app/keys.rs` and `app/mod.rs` (read-only gates).

## Summary
Overall quality is high. The non-test code follows the no-panic rules: there are no `unwrap`/`expect`, input-derived indexing is guarded, and the lock/read-only/stale-load generation machinery is carefully commented and heavily tested. I found no panics reachable from input.

The real risks are liveness and ordering problems at the edges of background work.
- A native bulk delete can wedge forever if the trash worker thread can't be spawned or dies.
- On the web, a catalog load can land on the wrong catalog when a switch doesn't bump the load generation.

Findings: 0 Critical, 0 High, 2 Medium, 3 Low, 3 Info.

## Findings

### [APP-LIB-01] Bulk delete never finishes if the trash worker fails to spawn or dies — Severity: Medium — Confidence: High (logic), Low-Medium (trigger likelihood)
- Location: `src/app/bulk_delete.rs:138-152` (`admit`), `:194-261` (`start_delete_with`), `:129-131` (`finished`), `:468-487` (`spawn_trash_worker`)
- Evidence:
  - `spawn_trash_worker` returns `None` when `thread::Builder::spawn` fails.
  - `start_delete_with` still installs the `BulkDelete` with `submit: None`.
  - `admit` begins with `let Some(submit) = self.submit.as_ref() else { return; };`, so nothing is ever admitted.
  - `finished()` is `queue.is_empty() && in_flight.is_empty()`, and the queue stays non-empty.
  - The same hang happens if the worker thread dies mid-batch (for example a panic in `trash::delete` or an objc exception). `admit` hands the whole queue to the channel at once, so everything behind the dying call sits in `in_flight` with no result ever coming. `submit.send` only errors for items sent after the receiver is gone.
- Impact / failure scenario:
  - Under thread exhaustion (EAGAIN), the user confirms Delete. The status reads "Deleting 0/N" forever.
  - `bulk_delete_running()` stays true, which has four effects:
    - Every later Delete is refused with "delete in progress".
    - Export is refused (`export.rs:967`).
    - `delete_available()` stays false.
    - `main.rs:644` polls every 16 ms forever.
  - Only an app restart recovers. No photo is lost, but the app is wedged and burns CPU.
- Suggested fix:
  - In `start_delete_with`, if `spawn_worker` returns `None`, set an error status and return without installing the batch. Or fail every queued path through `done_tx` so the batch ends with errors.
  - Make the worker robust: wrap `move_to_trash` in `catch_unwind` and send `Err`, and/or keep the `JoinHandle` and, when `is_finished()` while `in_flight` is non-empty, fail the remaining paths.
  - Add a test with a stand-in worker that returns `None` and a stand-in worker that drops its receiver mid-batch. `start_delete_with` already makes this injectable.

### [APP-LIB-02] A stale web catalog load can land on a different catalog — Severity: Medium — Confidence: Medium
- Location: `src/app/web_catalog.rs:223-240` (`enter_web_catalog`), `:93-100` (`poll_catalog_load`), `:102-172` (`land_catalog`); `src/persist/catalog/mod.rs:539-557` (`land_record`), `:327-332` (`Record::load`)
- Evidence:
  - The stale-load guard is `load.generation == self.web.catalogs.generation`.
  - The generation is bumped only in `start_catalog_load`, and `enter_web_catalog` calls that only `if self.catalog.loading()`.
  - `create_catalog_named` builds a catalog that has already landed its record (`land_record(Ok(None), &[])`), so entering it does not bump the generation.
  - `CatalogLoad` carries no catalog name, so an earlier in-flight load for catalog A still matches the current generation and is applied by `land_catalog` to the new catalog B.
  - `land_record` on a `Record::Loaded` state runs `Record::load`, which replaces B's folders with A's stored list (`merged_onto` ignores `added` once Loaded). `land_presets(&source, ...)` is called with B's name but A's preset bytes. `restore_waiting` is consumed by a load that wasn't meant for it.
  - A later root change on B then calls `save_record()` and overwrites B's stored IndexedDB record with A's folders.
- Impact / failure scenario:
  - On the web, after launch (so `known` is `Some`), the user, a script or MCP does `catalog.switch A` (pending, load in flight) and then `catalog.new B` before the IndexedDB reads return.
  - B silently shows A's folders and presets, and the next add or remove writes that list into B's record.
  - The window is a few to a few hundred ms, but scripted control (`catalog.switch` then `catalog.new`) hits it easily.
- Suggested fix:
  - Bump `generation` unconditionally in `enter_web_catalog`. For example, call a `state.supersede()` helper that does `generation = generation.wrapping_add(1)`, and also reset `restore_waiting`.
  - Better still, put the catalog name in `CatalogLoad` and drop loads whose name differs from `self.catalog.name()`.
  - Add a wasm-agnostic unit test of the generation/name rule, since the module is untestable natively.

### [APP-LIB-03] Undo/redo can touch photos already trashed by a running batch — Severity: Low — Confidence: Medium
- Location: `src/app/bulk_delete.rs:270-346` (trashed photos stay in the playlist until `prune_deleted`); `src/app/adjust.rs:490-499` (`apply_step` filters on playlist membership only)
- Evidence:
  - During a batch, trashed photos sit in `gone` and in the playlist.
  - `apply_step`'s `in_folder` set is built from `playlist.entries()`, so a step naming a trashed photo is still applied.
  - It calls `folder_store.set_adjustments`, `set`, `set_flag` and so on for that photo, after `forget_photos` removed its record and queued `WriteOp::Delete`.
  - Undo is not blocked while `bulk_delete` runs (`key_edits_folder` only gates read-only).
  - I did not trace `FolderStore::set` to confirm it recreates the record, but the cache entry was removed and the key is in the active dir.
- Impact / failure scenario:
  - The user rates or edits photos, deletes them (a multi-thousand batch takes a while), and presses Cmd+Z mid-batch.
  - The sidecar of a file already in the Trash may be rewritten (an orphan sidecar that nothing sweeps), and `edits`/`ratings` get repopulated for a path that is `gone`.
- Suggested fix: Make `apply_step` also skip paths in `self.bulk_delete.gone()`, or refuse undo/redo while `bulk_delete_running()`. Drop history steps for trashed photos in `prune_deleted`.

### [APP-LIB-04] A failed folder-store load thread leaves `pending` set after the folder closes — Severity: Low — Confidence: Medium
- Location: `src/app/folder_store.rs:227-243`; `src/app/folders.rs:116-137` (`close_folder`)
- Evidence:
  - When the `folder-store-load` thread can't be spawned, `pending` is deliberately kept (comment: export must refuse; "switching folders again retries").
  - `close_folder` does not clear or supersede `folder_store_load.pending`.
  - `delete_available`, `export.rs:261`, `autotone.rs`, `bursts.rs` and `commands.rs:365` all read `folder_store_loading()`.
- Impact / failure scenario:
  - After a spawn failure, the user closes the folder (or removes the root, so `close_folder` runs). `folder_store_loading()` stays true on the landing page.
  - Deferred Auto Tone stays deferred, `get_state` reports pending, and `poll_folder_store_load` returns true forever.
  - Only opening another folder clears it.
  - This is a narrow resource-exhaustion path, but the same state is also reachable for an in-flight real load of a closed folder, which clears when the result lands.
- Suggested fix: In `close_folder`, clear `pending` (and bump `token`) when no folder will be shown. Or have the spawn-failure arm leave `pending` tied to the playlist's dir and clear it when that dir goes away.

### [APP-LIB-05] `catalog_ref_for` / `switch_catalog_named` edge cases — Severity: Low — Confidence: Medium
- Location: `src/app/catalog.rs:158-178`, `:301-319`
- Evidence:
  - `bare = path.components().count() == 1 && !path.has_root()` is true for `.` and `..`, so `catalog.new ..` joins onto the catalog folder and appends the extension to a `..` component.
  - Non-bare relative paths (`sub/trip`) resolve against the process cwd, which for a Finder-launched app is `/`.
  - `n.ends_with(".lightphotos")` is case-sensitive, so `Trip.LIGHTPHOTOS` becomes `Trip.LIGHTPHOTOS.lightphotos`.
  - `switch_catalog_named` takes the first recent whose name matches. Two recents with the same name in different folders can't both be reached by name.
  - `remember_catalog` stores `to_string_lossy()`, so a non-UTF-8 path would be remembered wrongly and fail to reopen next launch.
- Impact / failure scenario: Surprising catalog locations from MCP/command input, or a catalog that can't be reselected by name. It is local-only and not a security issue, since the control surface already can open arbitrary paths.
- Suggested fix:
  - Reject paths with `.` or `..` components.
  - Require an absolute path for non-bare input.
  - Compare the extension case-insensitively.
  - Make `catalog.switch` fail on ambiguous names.

### [APP-LIB-06] Cosmetic/robustness notes in `HeldGroups` and `step_group_delete_focus` — Severity: Info — Confidence: High
- Location: `src/app/bulk_delete.rs:100` (`held.pending -= 1`), `:226` (`paths[0]`); `src/app/folder_store.rs:885-892` (`n - 1`, `% n`, `order[next]`)
- Evidence:
  - `held.pending -= 1` is an unchecked `usize` subtraction. It is unreachable today, because the paths are de-duplicated and each result releases once, but it would wrap or panic if two paths with the same file name from different folders were ever batched, since `slot_by_name` is keyed by file name only.
  - `paths[0]` on wasm is guarded by the earlier `is_empty()` return.
  - `step_group_delete_focus` divides and indexes by `order.len()`, which is non-empty by construction in `ui::modals::delete_group_tab_order`.
- Impact: None today. These are the patterns CLAUDE.md asks to avoid, and they depend on invariants held elsewhere.
- Suggested fix: Use `saturating_sub` and `paths.first()`/`order.get(next)`.

### [APP-LIB-07] Ordering comment in `reconcile_folder_store_mirrors` does not match the call order — Severity: Info — Confidence: Medium
- Location: `src/app/folder_store.rs:290-330`
- Evidence:
  - The comment says `save_edit()` writes an in-progress drag "before the folder store overwrites it".
  - `poll_folder_store_load` calls `apply_loaded` first (line 293) and `reconcile_folder_store_mirrors` (which calls `save_edit`) second.
  - Edits are refused while the folder is opening, so a drag on a photo of the newly loading folder can't exist. The only case is an `unsaved_edit` from the previous folder.
- Impact: Probably benign, but the comment documents an ordering the code does not have.
- Suggested fix: Verify that `load_playlist` saves the previous folder's `unsaved_edit` before switching, and fix or delete the comment.

### [APP-LIB-08] Delete toasts and structure — Severity: Info — Confidence: High
- Location: `src/app/bulk_delete.rs:340-344`, `:516-531`; `src/app/folder_store.rs:1256-1290`
- Evidence:
  - `poll_delete` calls `set_status(Progress)` and `request_redraw()` every frame, overwriting any other toast (an error toast raised mid-batch is replaced the next frame).
  - There is no way to cancel a long batch.
  - `the_count_sits_between_the_sort_and_actions` (a toolbar layout test) lives in `bulk_delete.rs`, so it is misfiled.
  - On macOS, `move_to_trash` fails for non-UTF-8 paths (`path.to_str().ok_or(...)`), so those photos can't be trashed, though the error is reported cleanly.
  - `forget_photos` permanently deletes the photo's sidecar when the photo is trashed, so "Put Back" from Trash loses ratings and edits. This is a design choice but worth stating.
- Impact: Minor UX and design.
- Suggested fix: Let errors survive, or show them at the end (already done by `prune_deleted`). Move the layout test to the toolbar module.

## Test coverage assessment
- Strong:
  - Folder-lock/visit state (`folders.rs` tests) covers many ordering cases: A/B/A quick switches, quit, close while opening, read-only gating through keys, UI actions, commands and menus, and parked results.
  - Bulk delete is tested for async trashing, live grid shrink, one playlist edit, folder change mid-batch, same-named files across folders, group hold, partial failure, dedupe, and deferred Auto Tone.
  - The catalog tests cover marks projection, debounce and launch fallback.
- Gaps:
  - No test for a trash worker that fails to spawn or dies (APP-LIB-01). The injectable `start_delete_with` makes it cheap.
  - `web_catalog.rs` has no tests at all, and none are possible natively. The generation/name rule (APP-LIB-02) could be factored into a pure function and tested.
  - No test for undo during a running delete (APP-LIB-03).
  - No test of `close_folder` with a pending load (APP-LIB-04).
  - Several tests rely on wall-clock timing: `a_relaunch_waits_off_the_ui_thread_for_a_long_quit` asserts `ui_wait < 500ms` and sleeps 3 s, which could flake on a loaded CI box. `a_read_only_visit_sweeps_no_derived_files` asserts "kept" after a fixed 300 ms sleep, which can't distinguish "not swept because read-only" from "sweep thread hasn't run yet". Its second half does prove the sweep works.
  - No malformed-input tests for `catalog_ref_for` beyond one bad name (`.`, `..`, empty, upper-case extension).

## Things done well / verified-safe areas
- Stale folder-store loads: the `(dir, token)` pending pair plus `LoadMark` means A/B/A navigation can't clear or apply the wrong load. Results for a left folder are applied to nothing (`photos = &[]`).
- Orphan sweeps (`develop::side_file::sweep_orphans`) are age-guarded (1 h), so the snapshot of fills/detections captured at load can't delete a variant created while the sweep runs. Thumbnail sweeps compare on content keys. Sweeps are skipped unless the visit is decided writable.
- Edits to folders that are read-only or still opening are refused at every entry point (keys, UI actions, commands) before any history entry or mirror changes. `revert_refused_edits` is the backstop.
- Bulk delete keeps playlist-index-keyed structures valid by hiding `gone` photos in `recompute_visible` and editing the playlist once. It keeps group membership intact until the whole group is trashed. Folder switches mid-batch don't touch the new folder's records (name-keyed cache).
- Web delete wording is explicit ("Permanently delete ... cannot be undone"), and the native path goes to the OS Trash.
- Catalog launch/fallback never overwrites the "last catalog" pref on fallback. `Record::{Pending, Loaded, Unreadable}` refuses to write over an unreadable record.
- No `unsafe`, no non-test `fs::write`, and no lock `unwrap`s in these files.

## Open questions for maintainers
1. Is a wedged delete after trash-worker failure acceptable, or should the batch fail fast (APP-LIB-01)? Is there any path that drops `bulk_delete` besides `poll_delete`?
2. On the web, is `catalog.switch` followed immediately by `catalog.new` a supported flow (APP-LIB-02)? Should `CatalogLoad` carry the catalog name?
3. Should `forget_photos` move the sidecar to the Trash with the photo (or keep it) so macOS "Put Back" restores ratings and edits?
4. Catalog `marks/` shards stay on disk after `remove_folder`, because `persist::catalog::remove_folder` only saves the record. Is that intended? They would be stale when the folder is re-added. This is outside my files.
5. `sync_marks` sets `marks_synced = revision` even when `record_marks` ignored the folder because no catalog root contains it. If the folder is then covered by an added root, its marks don't project until the next mark change or reload. Is that acceptable for a disposable copy?
