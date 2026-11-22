# src/app: core state, session, history, menus

## Scope
All lines of these files were read:

| File | Lines |
|---|---|
| `src/app/mod.rs` | 2033 |
| `src/app/state.rs` | 309 |
| `src/app/accessors.rs` | 624 |
| `src/app/session.rs` | 754 |
| `src/app/history.rs` | 482 |
| `src/app/menu.rs` | 184 |
| `src/app/update.rs` | 40 |
| `src/app/test_support.rs` | 261 |
| `src/app/slideshow.rs` | 239 |
| `src/app/tour.rs` | 154 |
| `src/app/fonts.rs` | 377 |

Total 5457 lines. For callers and callees I also read parts of `adjust.rs` (`apply_step`, `restore_groups`, `save_edit_unless_dragging`), `folder_store.rs`, `agent.rs`, `folders.rs`, `catalog.rs`, `ui/mod.rs` (`edits_folder`), `persist/prefs.rs` and `paths.rs`. Those other files are not reviewed here.

## Summary
The unit is in good shape. Non-test code has no `unwrap`, `expect`, `panic!` or unchecked indexing on input-derived data, except the small items below. Session and prefs parsing is defensive and well tested (malformed JSON, hostile `secs`, legacy migration). History is a compact, well-reasoned design with solid unit tests.

The real risks are state-lifecycle gaps. `App::open(<file>)` switches folders without clearing the undo history. "Reset all edits" and the folder-scoped undo bookkeeping skip some per-photo state. A few UI-thread costs also exist: a session write per selection change, blocking `fc-match` and font reads, and synchronous `read_dir` calls.

Findings by severity: 0 Critical, 0 High, 1 Medium, 8 Low, 4 Info.

## Findings

### [APP-CORE-01] Opening a file in another folder keeps the previous folder's undo history and group steps — Severity: Medium — Confidence: Medium
- Location: `src/app/mod.rs:1196-1232` (the `else` branch of `App::open`), compared with `load_playlist` at `src/app/mod.rs:1426-1450`. Also `src/app/history.rs:283` and `src/app/adjust.rs:485-567, 572-606`.
- Evidence: `load_playlist` calls `self.history.clear()`, `self.coverage.clear()` and `self.burst_scan = None`, and resets `grid_range`. The single-file branch of `open` never does any of these. It runs `seed_mirrors`, replaces `self.playlist` and loads the file. `history.clear()` has only two other callers, `load_playlist` and `close_folder` (`folders.rs:128`). `touchup_undo` is never cleared on any folder change.
- Impact: The user edits or groups photos in folder A, then opens a file in folder B (Finder "Open With", the CLI, the control `open`, or a file dropped on the window). `can_undo()` stays true and Undo consumes A's steps.
  - Edits, ratings and marks are filtered through `apply_step`'s `in_folder` set, so those steps become silent no-ops. The user presses Undo and nothing happens.
  - `GroupBefore` steps are filtered only by file name (`restore_groups`, `adjust.rs:581-587`). Camera folders routinely share names (`DSC_0001.NEF`, `IMG_0001.JPG`). A group step from A is therefore written into B's `folder_store`, creating or replacing a group in B's sidecars.
- Suggested fix: Factor the folder-leaving reset (`history.clear()`, `coverage.clear()`, `burst_scan = None`, `touchup_undo.clear()`, and `patches.clear()` on wasm) into one helper. Call it from `load_playlist`, from the file branch of `open` when `playlist.dir()` changes, and from `close_folder`. Better, do it inside `seed_mirrors` next to the existing `playlist.dir()` comparison at `mod.rs:1343`, so every path that changes folder gets it. Add a test: group in folder A, open a file in folder B with the same file names, Undo, assert B's groups are unchanged.

### [APP-CORE-02] "Reset all edits" leaves generative fills and the Touch Up undo stack — Severity: Low — Confidence: Medium
- Location: `src/app/mod.rs:1930-1960` (`UiAction::ResetAllEdits`). It is offered by the Loupe photo menu (`ui/photo_menu.rs:200`) as "Reset adjustments".
- Evidence: The handler resets adjustments, touchups (spots) and masks, but not fills. Fills live in a separate store (`folder_store.set_fills`, `fill.rs` / `generative.rs`) and are not part of `Before`/`EditStep`. It also leaves `self.touchup_undo[path]` untouched and does not clear the Touch Up selection stack.
- Impact: After Reset, generative-fill patches still render and still count as edits. Pressing Undo while the Touch Up tool is armed (`adjust.rs:854`) steps the stale `touchup_undo` stack instead of the history step Reset just recorded. Fills are never undoable at all. The reset is destructive for spots and masks but silently incomplete for fills.
- Suggested fix: Decide whether Reset should include fills. If so, clear fills (through `edit_fills` or `store_fills`) and fold them into history, or at least `touchup_undo.remove(&path)`. If not, say so in the menu label and the comment.

### [APP-CORE-03] The session file is written synchronously on every selection change, and `current_session()` allocates every event-loop turn — Severity: Low — Confidence: Medium
- Location: `src/app/session.rs:188-219`, called from `src/main.rs:507` (`pump`).
- Evidence: `save_session_if_changed` builds a full `Session` each turn, with `dir`, `photo` and `anchor` clones plus `selected_path()`. If it differs from the saved one, it calls `prefs::save`. That is `write_atomic_in_dir`: a symlink check, `create_dir`, creating a temp file and a rename (`paths.rs:211-217`). It runs on the UI thread.
- Impact: Holding an arrow key in the Grid or Loupe changes `photo` at key-repeat rate, about 30 times a second. That is up to 30 tiny file create-and-rename cycles per second on the UI thread, which can stutter on slow or networked home directories. The marks sync already uses a debounce, so there is precedent.
- Suggested fix: Debounce (for example, save once the session has been stable for 500 ms, and on exit or page-hide). Compare cheap fields first (`mode`, `sel`, `folder_sel`) before cloning paths.

### [APP-CORE-04] Agent-facing `State` still carries toast text, the selection and counts when the folder is out of the agent's reach — Severity: Low — Confidence: Low
- Location: `src/app/state.rs:183-203` (`State::for_agent`).
- Evidence: `for_agent` blanks `folder`, `photo` and the photo's marks, but leaves `status`, `read_only`, `sel`, `selected`, `visible`, `stacks`, `grid_range`, `failed_thumbs`, `scoring`, `zoom`, `mode` and `catalog` as `State::of` read them. `status` is the raw toast text. `edits_save_failed(&cause)` and export or catalog errors can carry absolute paths, because `prefs`/sidecar errors format `path.display()`. `read_only` names the PID of the process holding the lock.
- Impact: The anchor's stated purpose is "nothing when the open folder is outside its anchor". A toast about an unrelated folder (an export destination, a sidecar path, `open_folder_failed`) can still show an out-of-anchor path to an MCP caller. The counts leak the folder's size.
- Suggested fix: When `!folder_held`, also clear `status`, `read_only`, `sel`, `selected`, `stacks`, `visible`, `failed_thumbs` and `scoring`. Add a test that asserts the redacted JSON equals the empty-folder JSON apart from `mode`, `focus` and `catalog`.

### [APP-CORE-05] Font stack: a blocking `fc-match` subprocess and large synchronous system-font reads in `App::new`, which every test also pays — Severity: Low — Confidence: Medium
- Location: `src/app/fonts.rs:167-183` (Linux and other unix), `137-152` and `192-195` (macOS), and `src/app/mod.rs:913` (`fonts::configure` in `App::new`).
- Evidence:
  - Unix: `std::process::Command::new("fc-match").output()` is resolved via `PATH`, with no timeout. fontconfig can rebuild its cache for many seconds on a first run.
  - macOS: `read_face` reads `SFNS.ttf`, `Hiragino Sans GB.ttc` and `AppleSDGothicNeo.ttc` whole into `Vec<u8>`. They are probably tens of MB in total, and are parsed once for metrics and again by egui.
  - Both run on the main thread before the window is shown.
  - `App::new` is also what every test calls, so each of the hundreds of tests spawns `fc-match` or re-reads three system fonts. That also makes the tests depend on the developer's installed fonts.
- Impact: Slow or hung first paint on a cold fontconfig, and slow, host-dependent tests. A hung `fc-match` hangs app launch.
- Suggested fix:
  - Skip system faces under `cfg!(test)`.
  - Run the `fc-match` lookup off-thread with a deadline, or install the egui defaults first and add the CJK face when it arrives (the wasm path already does this).
  - Mask the fontconfig index: `index & 0xFFFF` (fontconfig packs the named instance of variable fonts in the high 16 bits, which `skrifa::FontRef::from_index` rejects). Today a variable CJK font is silently dropped by `draws_chinese`, which falls back to the bundled subset.
  - Use `FontData::from_static` for the `include_bytes!` subsets instead of `.to_vec()` (`fonts.rs:44, 49`), which copies them to the heap.

### [APP-CORE-06] `thumb_texture_for_path` allocates a `PathBuf` per cell per frame — Severity: Low — Confidence: Medium
- Location: `src/app/accessors.rs:508-511`, reached from `thumb_texture_for` (`:487-491`) for every visible Grid cell, plus the filmstrip.
- Evidence: `self.thumb_tex.get(&(path.to_path_buf(), THUMB_PX))` clones the path to build the tuple key for every lookup.
- Impact: Hundreds of small allocations per frame in a large Grid, with no functional effect. Thumbnail-heavy views already budget carefully elsewhere.
- Suggested fix: Key the map by a borrowed-friendly type (nested `HashMap<PathBuf, ..>` per size, or a `Borrow`-able key struct) so lookups don't allocate.

### [APP-CORE-07] Synchronous directory I/O on the UI thread: `open` metadata, 64 `read_dir` per frame — Severity: Low — Confidence: Low
- Location: `src/app/mod.rs:1145` (`std::fs::metadata` in `open`), `1299-1333` (`prime_visible_subdirs`, `PER_FRAME = 64` and `relist_subdirs`), `session.rs:257-262` (`is_dir`, `ensure_subdirs`), and `1261-1275`.
- Evidence: Each tree row without a fresh listing triggers a blocking `list_subdirs`, up to 64 per frame. `restore_session` opens the catalog root first (a full `load_folder` + sidecar load), then loads the saved subfolder, so the root is read and discarded.
- Impact: On an SMB/NFS mount or a spun-down external drive, an expanded tree can stall a frame for seconds. A restore of a deep subfolder does the root's work for nothing.
- Suggested fix: Lower the per-frame budget for non-local volumes, or move listings to a worker as the web build effectively does. In `restore_session`, load the target directly (open the root only to register it).

### [APP-CORE-08] History `begin`/`end` is not RAII, and `EditStep` invariants rely on convention — Severity: Low — Confidence: Medium
- Location: `src/app/history.rs:115-121`, `186-198`, and the pub(crate) fields at `:37-43`.
- Evidence:
  - `depth` is a bare counter. Every current call site pairs `begin()`/`end()` with straight-line code in between (`adjust.rs:354-357`, `bursts.rs:141-160`, `loupe.rs:252-258`, `commands.rs:22-24`).
  - `EditStep.before` and `.groups` are `pub(crate)`, but `index` and `group_index` are private caches over them. `entry()` then does `&mut step.before[i]` (`:197`).
- Impact: A future early `return`, `?` or panic between `begin` and `end` leaves `depth > 0`. Every later edit then merges into one ever-growing undo step, because `seal` never closes it. A future caller that edits `step.before` directly would make the cached index stale and `entry()` would index out of bounds, against the "no panics on `[i]`" rule.
- Suggested fix: Return a guard from `begin()` that calls `end()` on drop, or add `fn grouped<R>(&mut self, f)`. Make `before` and `groups` private with read accessors (callers in `adjust.rs` iterate them), or use `get_mut` in `entry()`.

### [APP-CORE-09] Session is global, not per catalog, so a catalog switch forgets where each catalog was — Severity: Low — Confidence: Medium
- Location: `src/app/session.rs:11-12` (a single `SESSION_PREF`), `224-244`, `catalog.rs:255-267` (`switch_catalog` calls `restore_last_session`).
- Evidence: `to_restore(self.session, ...)` accepts the saved session only if its `dir` is under one of the new catalog's folders. Otherwise it opens the new catalog's first existing folder at its top in the Grid. After the switch, the saved session is overwritten.
- Impact: Switching A to B to A always lands A on its first folder, not the last place there. This is a product limitation rather than a bug, but the doc comment ("open the last session where it left off") reads as if it did.
- Suggested fix: Key the session by catalog, or store the last place in the catalog record. If this is intended, note it in the doc comment.

### [APP-CORE-10] `preview_target_px` can overflow `u32` on absurd input — Severity: Info — Confidence: High
- Location: `src/app/accessors.rs:517-521`.
- Evidence: `longest_physical.max(1.0) as u32` saturates for `inf` or very large floats, then `longest.div_ceil(PREVIEW_QUANTUM) * PREVIEW_QUANTUM` overflows. That is a debug panic, and in release it wraps to 0 and the result clamps to `PREVIEW_MIN` instead of `PREVIEW_MAX`.
- Impact: Not reachable with real window sizes. It violates the "checked or saturating math on externally derived numbers" rule, and the existing test stops at 100000.
- Suggested fix: `longest_physical.clamp(1.0, PREVIEW_MAX as f32) as u32` first, then quantize.

### [APP-CORE-11] Slideshow RNG seed has low entropy — Severity: Info — Confidence: High
- Location: `src/app/slideshow.rs:99-105`, `122-129`.
- Evidence: `clock_seed()` is `subsec_nanos()` only. On the web, `Date.now()` has ms resolution, so only about 1000 distinct seeds exist (always multiples of 1e6). On macOS the clock is in microseconds. Seeded xorshift64 with a small seed also gives weak early outputs, which Fisher-Yates uses for the largest `i` first.
- Impact: Shuffle orders repeat across launches and are mildly biased. Cosmetic only.
- Suggested fix: Mix seconds into the seed and run a SplitMix64 step before using it (a few lines). Or use `getrandom`, if it is already a dependency.

### [APP-CORE-12] The font baseline correction is stored per face but computed against the Proportional family — Severity: Info — Confidence: Medium
- Location: `src/app/fonts.rs:60-82`, `102-133`.
- Evidence: `y_offset_factor` is set on each shared `FontData` relative to the proportional primary face (SF or Ubuntu-Light). The same faces (Hack and the builtins) are also members of the Monospace family, where Hack leads, so Hack itself ends up with a non-zero offset (about 0.03 em) in monospace text. I grepped `src/` for monospace usage and found none.
- Impact: No visible effect today. It becomes a hidden bug if `ui.code()` or `.monospace()` is ever used.
- Suggested fix: A one-line comment, or install separate `FontData` entries (distinct names) for the Monospace family.

### [APP-CORE-13] Test support: shared fixture names and unhermetic prefs reads — Severity: Info — Confidence: Medium
- Location: `src/app/test_support.rs:251-261` (`one_photo` dir `lp-adjust-{tag}-{pid}`), `src/app/mod.rs:913-915`.
- Evidence: `one_photo` builds its directory from `tag` and the pid only, unlike `temp_folder`, which adds a counter. Two tests that pass the same tag would race on `remove_dir_all` and `create_dir_all`. `App::new` reads `font_size::init` and `theme::init` from the developer's real prefs, though it deliberately avoids prefs for sort, slideshow, presets and catalog.
- Impact: Latent flakiness. UI tests that use positions or text sizes depend on the developer's saved font size and theme.
- Suggested fix: Use the `COUNTER` in `one_photo`. Guard `font_size::init` and `theme::init` with `cfg!(test)`, as is done for the other prefs.

## Test coverage assessment
- **session.rs:** very good. It covers legacy migration, the cleared-anchor round trip, missing folder, subfolder and photo fallbacks, and the landing page. It lacks nested catalog roots (`place` picks the first matching prefix), per-catalog behaviour, and a write-failure path.
- **history.rs:** good unit tests for first-before-wins, `begin` and `hold` spanning seals, the 100-step cap, redo invalidation and groups. Missing: the unbalanced `begin` case (APP-CORE-08), and whether memory is bounded with huge steps.
- **mod.rs:** only two inline tests (`range_set`, folder-change cache forgetting). The `open(<file>)` branch has no test for folder switches (APP-CORE-01). `redraw`, `apply_ui_actions` (including the read-only refusal and `ResetAllEdits`) are covered only indirectly by `commands.rs` tests. `ResetAllEdits` is covered only for touch-ups, not for fills.
- **menu.rs:** four tests. The `menu_enabled` matrix (about 30 arms) is mostly untested, including read-only gating, `typing`, and Settings-open states.
- **state.rs:** list ordering, cap and fresh-state JSON are tested. `for_agent` redaction is not tested here (it may be in `agent.rs`).
- **fonts.rs:** tests cover arrow glyphs and the baseline math only. The system-face discovery paths and index handling are untested.
- **slideshow.rs, tour.rs:** good. The Deck tests check no immediate repeat, each photo once per round, and grid shrinkage. The tour has real-UI click tests.
- **accessors.rs:** `selection_summary`, `strip_layout` shift and `remember_thumb_shape` tolerance have no tests in this file. `strip_layout`, `selection_summary`, `remember_thumb_shape` and `stack_at` are covered at most indirectly from `nav.rs`.
- **update.rs:** no tests (a trivial delegation).

## Things done well / verified-safe areas
- `SlideshowSettings::load` and `slide()` clamp hostile saved values, and there is a test for `secs: 4000000000`.
- Session parsing never panics. It tolerates unknown or missing fields, `null` anchors versus absent anchors (`present`), and the legacy `root`/`folders` shapes. A parse failure falls back to `None` without clobbering until the next save.
- `touchup_radius_min()` is bounded (`.min(TOUCHUP_MAX_RADIUS)`), so `clamp(min, max)` in the callers cannot panic.
- Stale-result protection in `poll_folder_store_load` (token plus dir comparison) and the `seed_mirrors` ordering (flush marks, finish jobs, then swap the playlist) are sound.
- `apply_step` filters steps through the current playlist, so cross-folder edits can't be written (only group steps leak; see APP-CORE-01).
- `redraw` handles viewport math safely: `fs > 0.0` guard, `.max(0.0) as u32` for NaN/negative, `w >= 2` for Compare halves.
- `StripLayout` invalidation relies on `recompute_visible` and the delete path nulling `strip_layout` (`nav.rs:123, 268`, `bulk_delete.rs:430`), which I checked.
- No `unsafe`, no `std::fs::write` in non-test code in these files, and the session and prefs writes go through `write_atomic_in_dir`.
- `redraw` takes `egui_state` out of `self` and puts it back only on the success path. This is harmless today because `window` and `egui_state` are always set together (`main.rs:86-92`) and the UI thread has no `catch_unwind`.

## Open questions for maintainers
1. Is Reset meant to remove generative fills as well (APP-CORE-02)? If so, should fills join the undo history?
2. Should history clearing live in `seed_mirrors` (any playlist-dir change) rather than at individual call sites, so the next new entry point cannot forget it?
3. Is a global (not per-catalog) session intentional (APP-CORE-09)?
4. Is agent exposure of `status` text a deliberate trade-off for debugging, or should it follow the anchor like `folder` and `photo`?
