# src/app — keys, commands, control

## Scope

| File | Lines |
|---|---|
| `src/app/keys.rs` | 2879 (about 450 of code, the rest `#[cfg(test)]`) |
| `src/app/commands.rs` | 1836 (about 705 of code, the rest tests) |
| `src/app/control.rs` | 661 (about 123 of code, the rest tests) |

Every line was read. Cross-references checked in `src/app/nav.rs`, `folder_store.rs`, `group_compare.rs`, `adjust.rs`, `src/persist/folder_store.rs`, `src/shell/control/{mod,tcp}.rs`, `src/main.rs` and `docs/design/folder-lock.md`.

## Summary

The code is in good shape for the project rules. There is no panic, `unsafe`, unchecked arithmetic on input-derived numbers, or non-atomic file write in the non-test code of these three files. `resolve`, `rotate_path` (u8 wrap) and `Crop` (validate all, then write) are written defensively, and the tests cover them. The control channel is also careful: expired requests are dropped unrun, screenshots re-check the anchor at capture time, and the `Wait` timeout is clamped by the transport.

The real risks are structural. The "which modal owns the input" rule is encoded in three places that are kept in step by hand: `handle_key`'s guard chain, `input_blocker`, and `key_edits_folder`. They have already drifted (Escape-to-Grid, the GPS dialog, the Settings ordering, and `check()` versus the keys on a folder that is still opening).

Findings: 0 Critical, 0 High, 3 Medium, 4 Low, 2 Info.

## Findings

### [APP-INPUT-01] Esc from the Loupe sets `mode = Grid` directly and skips `enter_grid`  — Severity: Medium — Confidence: Medium
- Location: `src/app/keys.rs:404-407` (versus `src/app/nav.rs:849-863`, `enter_grid`)
- Evidence: the Escape arm for `Region::Detail` does `self.mode = ViewMode::Grid; self.update_window_title(); self.normalize_focus();`. Every other route to the Grid, such as `G`, `view.mode`, the slideshow and fullscreen exit, goes through `enter_grid()`. `enter_grid` documents that "Crop, Masks and Compare stay in the Loupe, so leaving it ends them and Develop comes back on Sliders". It resets `develop_tab` to Sliders and, if the pane is in Compare, calls `set_group_view(Edit)`.
- Impact: Esc out of a Loupe that has the Masks tab open or the Compare pane open lands in the Grid with `develop_tab == Masks` and `group_view() == Compare`.
  - `develop_visible()` is `develop_open && group_view()==Edit`, so the Grid's Develop panel is hidden.
  - `rail_lit()` lights the Compare icon in the Grid. `the_grid_rail_edits_the_selection_without_the_loupe_pages` asserts the opposite contract: `rail_lit()` is `None` in the Grid.
  - `set_group_view(Edit)` also restores the selection saved for an ad-hoc compare (`group_compare.adhoc`) and clears picks. The Esc path never does, so the pre-compare multi-selection is lost.
  - `group_picks()` is gated on `mode == Loupe`, so I found no leak of picks into Grid bulk actions.
- Suggested fix: replace the three lines with `self.enter_grid()`, which already does the title and focus normalisation and the redraw. Add a test: open Compare or Masks in the Loupe, press Esc through to the Grid, then assert `group_view() == Edit` and `develop_tab() == Sliders`.

### [APP-INPUT-02] `check()` does not refuse photo-writing commands while the folder is still opening; the keys do  — Severity: Medium — Confidence: Medium
- Location: `src/app/commands.rs:49-53` (versus `src/app/keys.rs:95`, `src/app/folder_store.rs:373-375, 415-424`)
- Evidence: the key gate uses `self.read_only()`, which is `read_only().is_some() || opening()`. `check()` uses only `self.folder_read_only_text()`, which is `read_only()` alone and ignores `opening()`. Design row 18 of `docs/design/folder-lock.md` says "gates at key / command / UI action", and row 50 says edits are refused up front while the sidecars load.
- Impact: `run()` is the path for the menu bar, `--drive`, the control channel and MCP, for example `develop.set` or `photo.rate` right after `open_folder`.
  - `apply_command` calls `edit_each`/`put_rating`/`put_flag`/`put_label`. These change the app mirror and call `history.note_*` before the folder store refuses the write (`FolderStore::refuse`, which only pushes to `refused`).
  - `revert_refused_edits` later puts the mirror back, but `refuse_if_read_only()` says nothing for a merely-opening folder.
  - The caller therefore gets `Ok(Outcome{photos: n})` for an edit that was dropped. A stale undo step probably remains, though I did not run it to confirm.
  - On disk nothing is corrupted, because the store refuses the write.
- Suggested fix: in `check()`, for `cmd.writes_photos()`, also return `CommandError::Failed` with `t().folder_opening` when `self.folder_store.opening()`. Exclude the job-style commands the lock design says should defer instead (`AutoTone`, `Score`; see design row 47). Add a test that runs `develop.set` on an app whose store is still opening and asserts the error and an empty history.

### [APP-INPUT-03] The GPS import dialog holds commands but not keys  — Severity: Medium — Confidence: Low
- Location: `src/app/commands.rs:126-127` (`Blocker::GeotagImport`); `src/app/keys.rs` has no `import_dialog_open()` branch; `src/ui/modals.rs:197` (`gps_import_modal`)
- Evidence: `input_blocker` says the GPS dialog holds the input. `handle_key` handles the tour, confirm, copy dialog, crop, WB picker, Touch Up, preset name, wasm catalog prompt, export form and Settings, but never the GPS import dialog. The modal is an `egui::Modal` with no key handling of its own. egui_winit reports a key as consumed only when a text field is focused, so with no field focused `key_input` forwards the key to `handle_key`.
- Impact: with the dialog open and no text field focused, digits, P/X/U, `V`, Delete (which opens a confirm for the photos), `[`/`]` (with Cmd), and Esc (which backs the Grid out to Folders and clears the selection) all act on the photos behind the dialog. The rule stated for the other dialogs ("every other key waits") does not hold here. `every_modal_that_holds_the_keys_holds_their_commands` lists 7 modals and omits this one, the tour, Straighten and Crop, so the drift is not caught.
- Suggested fix: add an `import_dialog_open()` branch beside the copy dialog (Esc cancels, Enter commits if ready, others wait). Add the missing modals to the parity test. If `egui::Modal` does consume Esc and Enter in practice, keep the branch anyway for the other keys. I could not run the app to confirm which keys egui consumes.

### [APP-INPUT-04] Tool and mask key branches run before the Settings gate  — Severity: Low — Confidence: Medium
- Location: `src/app/keys.rs:135-169` (Touch Up), `175-212` (mask section) against `248-259` (Settings), `218-225` (preset name)
- Evidence: the comments say Settings makes "every other key wait", but the order is Touch Up, then mask Delete/Esc/brackets, then the name prompt, then Settings. `input_blocker` orders the modals Tour, Confirm, CopySettings, Straighten, Crop, WbPicker, TouchUp, PresetName, GeotagImport, Settings, so the two disagree on which modal wins. There is no `Blocker` for the mask section at all.
- Impact:
  - Settings opened from the toolbar while a mask is selected: Delete deletes the mask behind it, and Esc deselects the mask instead of closing Settings.
  - Settings opened with Touch Up armed: Enter calls `generate()` (a generative-fill job) and Delete removes a spot.
  - Cmd+, is swallowed by the Touch Up and crop branches, so the shortcut cannot open Settings from those tools.
  - Delete with a mask tool armed and no mask selected is silently swallowed (`return` at line 208) instead of reaching the photo delete. This is probably intended.
- Suggested fix: move the Settings (and Help, preset-name, export-form) gates ahead of the tool branches. Have one function, such as `input_blocker`, drive both the keys and the commands so they cannot drift.

### [APP-INPUT-05] `photo_size` reads file metadata synchronously on the UI thread, once per photo, inside `crop.set`  — Severity: Low — Confidence: Medium
- Location: `src/app/commands.rs:491-507`, called from the `Command::Crop` arm at `:303-309`
- Evidence: when `exif_cache` has no entry and the photo is not the shown one, it calls `image_decode::read_metadata(path)` (a file open and header parse).
- Impact: an agent or script running `crop.set` with `straighten` over a few hundred photos from a slow volume (network share, SD card, iCloud placeholder) blocks the frame loop for the whole batch. The results are not cached into `exif_cache`, so the next command repeats the reads.
- Suggested fix: cache the result in `exif_cache`, or return `Busy` and fill the cache via the existing metadata job when more than a few photos are missing.

### [APP-INPUT-06] `resolve_one` is a linear scan per name, and a named batch is resolved twice or three times  — Severity: Low — Confidence: High
- Location: `src/app/commands.rs:92-94` (`check`), `:148-157` and `:167-182` (`resolve`/`resolve_one`), `:532-546` (`select_photos`)
- Evidence: each name scans the whole playlist (`entries().iter().find`). `check()` resolves, `apply_command` resolves again, and `select_photos` adds a third pass with `position(..)` per path.
- Impact: O(photos x names) with a constant of 2-3 on the main thread. This is harmless at hundreds of photos. A 20k-photo folder with an agent naming thousands of photos is a visible stall. The names come from an untrusted agent.
- Suggested fix: build a `HashMap<&OsStr, usize>` (and a path map) once per call. Pass the resolved list from `check` to `apply_command`, or cap `Named` length at the command layer.

### [APP-INPUT-07] The `control.rs` module doc says neither `wait` nor `screenshot` blocks the main thread, but a screenshot does a blocking GPU read-back  — Severity: Low — Confidence: Medium
- Location: `src/app/control.rs:1-3` and `:105-122` (`renderer.read_offscreen_blocking()`)
- Evidence: the held request is deferred to the loop so the transport thread does not block, but the answer itself renders offscreen, redraws, and blocks the UI thread on the read-back (with the Metal occlusion hazard the `settled_across_a_frame` comment describes). With N queued shots the loop does N back-to-back read-backs in one turn.
- Impact: a latency spike only. The comment misleads a maintainer into thinking the path is non-blocking. A screenshot also does not wait for the app to settle, so a caller who skips `wait` can capture a half-decoded frame. This is probably by design.
- Suggested fix: fix the wording. Optionally answer at most one shot per `poll_control` turn.

### [APP-INPUT-08] Some control and command tests assert too little or could not fail  — Severity: Info — Confidence: High
- Location: `src/app/control.rs:252-254` (`refused`), `:362-384`; `:185-189` (`Op::Screenshot` is `cfg(not(wasm32))` but the test is not gated); `src/app/commands.rs:1157-1219`
- Evidence:
  - `refused()` is `e.contains("an agent")`. The positive checks (`!refused(&out)`) pass for any error that lacks that phrase, such as `NoSuchPhoto` or an export failure, so `an_agent_exports_only_inside_the_anchor...` does not prove the export ran.
  - `a_request_whose_connection_gave_up...` names `Op::Screenshot` without a cfg, so it would not compile for a wasm test build. CI only clippies the wasm bin without `--all-targets`.
  - Temp directories are removed only on the success path.
  - The modal-parity test omits the tour, Straighten, Crop, GPS import and the wasm catalog prompt.
- Suggested fix: assert `out.is_ok()` or a specific error variant. Gate the test or the variant. Use a drop-guard for temp dirs.

### [APP-INPUT-09] About 1,600 lines of slideshow, fullscreen, Develop-cursor and group tests live in `keys.rs`  — Severity: Info — Confidence: High
- Location: `src/app/keys.rs:889-1652` and `:2665-2878`
- Evidence: they exercise `tick_slideshow`, `slide_view` and the loader tiers through `press(...)`.
- Impact: maintainability only. The tests are hard to find from the code they cover, and a failure in `keys.rs` points at the wrong module.
- Suggested fix: move them next to `fullscreen.rs`/`slideshow` and `group_compare.rs`.

## Test coverage assessment

- Good:
  - Undo and redo, one step per command, batch rate, flag, label, rotate and monochrome.
  - Hostile saved rotation wrap.
  - A named photo given twice acts once.
  - Atomic `crop.set` and `develop.set` (all validated before any write).
  - Toggle parity with headless mode.
  - Anchor and symlink containment for agents (a large suite in `control.rs`).
  - Expired requests, and the occluded settle check.
  - Many key behaviours: modals swallowing keys, Cmd/Alt not doubling bare-letter keys, zoom keys.
- Missing:
  - Esc from the Loupe to the Grid with Masks or Compare open (APP-INPUT-01).
  - A command run while the folder is `Opening` (APP-INPUT-02).
  - The GPS dialog, tour, Straighten and Crop in the modal-parity test.
  - A test that a mask is not deleted while Settings is open.
  - `key_edits_folder` against `handle_key` (the "keep in step" comment is enforced only by the read-only tests in `folders.rs`).
  - `select` with a very large `Named` list.
  - `Command::Export` with the form closed.
  - The wasm-only branches of `handle_key` (catalog prompt) and `apply_command`.

## Things done well / verified-safe areas

- No `unwrap`, `expect`, `[i]` indexing or `as usize` casts on input in non-test code. Slices are only used in `match &self.action_paths()[..]` patterns.
- `rotate_path` uses `now % 4 + ...` with u8, so a hostile sidecar value cannot overflow.
- `Crop` and `Mask` batches compute all results before writing any.
- `run()` brackets every command in `history.begin/end`, and undo and redo skip it. `check()` gates undo in a read-only folder before the history pop.
- `answer_control` drops expired requests unanswered (the transport sees a closed channel), clamps `Wait` to `expires`, and screenshots re-check the anchor when the picture is taken, not when requested. The TCP layer bounds `timeout_ms` at 60 s, so `now + timeout` cannot overflow `Instant`. The wasm path uses the fixed `DEFAULT_WAIT`.
- `poll_control` never holds a reply across more than the deadline.
- `press_undo` restores the modifiers it swapped.

## Open questions for maintainers

1. Is Delete-only (no Backspace) for Move to Trash intentional? On a Mac laptop the "delete" key is Backspace, so users must press fn+Delete. `shell/menu.rs:104` and `i18n.rs:1595` both say plain `Delete`.
2. Should the Help overlay (`?`) hold the keys like Settings? Today digits and flags act behind it, and only Esc and `?` are intercepted.
3. Is it deliberate that Cmd+, does nothing while Touch Up or crop is active (APP-INPUT-04)?
4. Should `Navigate` report `photos: 1` even when it did not move (a Grid with one photo, or at the end of a row)?
