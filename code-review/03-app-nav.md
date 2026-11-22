# src/app — navigation, sort, bursts, group compare

## Scope

Every line of these files was read:

| File | Lines |
|---|---|
| `src/app/nav.rs` | 2196 (about 1440 non-test, the rest tests) |
| `src/app/sort.rs` | 429 |
| `src/app/bursts.rs` | 286 |
| `src/app/group_compare.rs` | 2034 (about 1035 non-test, the rest tests) |

I cross-checked callers and callees in `navigation.rs`, `jobs/loader.rs`, `app/thumbs.rs`, `app/folder_store.rs`, `app/adjust.rs`, `app/commands.rs`, `scoring/facequality.rs` and `ui/loupe.rs`. Findings are about the four assigned files.

## Summary

The code is in good shape for its size. Index math on input-derived numbers is mostly guarded (`get()`, `saturating_*`, `rem_euclid`). The cell-remapping logic in `recompute_cells`/`Placer` is careful and well tested, and the Compare delete path asks before it trashes anything. No `unwrap`, `expect` or `unsafe` appears in non-test code.

The biggest risks are state held as Grid cell indices or per-frame work in the Compare pane:

- One real data-integrity bug: the Compare-on-a-selection restore uses stale cell indices.
- One latent panic through a debug env var.
- A GPU texture leak while the Grid is showing.
- A few perf and consistency problems.

Counts: High 0, Medium 2, Low 7, Info 3.

## Findings

### [APP-NAV-01] Closing a selection-Compare restores stale cell indices and can select the wrong photos — Severity: Medium — Confidence: High
- Location: `src/app/group_compare.rs:306-314` (`AdhocSet.selected`), `:690-699` (capture), `:839-843` (restore). The remapping that is missing lives in `src/app/nav.rs:109-300` (`recompute_cells`).
- Evidence: `AdhocSet.selected` is a `BTreeSet<usize>` of Grid cell positions. It is taken with `mem::replace(&mut self.selected, …)` when Compare opens and written back verbatim in `set_group_view(Edit)`: `self.selected = set.selected; self.selected.extend(self.sel);`. `recompute_cells` remaps `self.selected`/`anchor`/`sel` through playlist indices on every re-filter, re-sort or delete. Nothing remaps `group_compare.adhoc` (grep: the only other `adhoc` use outside this file is `forget_adhoc_off_set`).
- Impact / failure scenario:
  1. In the Grid, select cells {1,3,4} and press C.
  2. In the Compare pane, press X, or use a tile's reject, on a loser. The Grid's default flag filter is Not Rejected, so `refilter_after_folder_store_change` drops that photo from `visible` and every later cell shifts down by one.
  3. Rating a member under an active star filter, or finishing a delete, shifts cells the same way.
  4. Close Compare or press Esc. The restored `{1,3,4}` now names different photos, and may include indices at or past `visible.len()`.
  5. The next Delete, Export or Rate from the Grid acts on the wrong photos. The confirm dialog shows a count but not which photos.
  6. Out-of-range indices also make `selection_count()` (which uses `selected.len()`) disagree with `selected_paths()`.
- Suggested fix: store the restored selection as playlist indices or paths in `AdhocSet`, and re-place them through `place_of`/`Placer` at restore time, dropping `Gone` ones. Alternatively remap `adhoc.selected` inside `recompute_cells` next to `selected`/`anchor`. Add a test: Compare on a selection, reject one member, close, assert the restored selection names the other photos.

### [APP-NAV-02] Compare tile textures are never freed while the Grid is showing — Severity: Medium — Confidence: Medium
- Location: `src/app/group_compare.rs:389-400` (`sync_compare_tiles`), `src/app/nav.rs:849-863` (`enter_grid`).
- Evidence: `sync_compare_tiles` begins with `if self.mode != ViewMode::Loupe { return; }`. The only place that frees the tiles on leaving Compare is the next line, `if view == Edit { tiles.take().free(r) }`, which is therefore unreachable once the mode is Grid. `enter_grid` calls `set_group_view(Edit)` and then sets `mode = Grid`, so the next frame never gets to the free. `sync_loupe_tiers` drops the held full decodes and the full-resolution crops are only released by `drop_full_crops` (also inside the same function), but the mipped preview textures and crops stay.
- Impact: after one Compare session, up to `COMPARE_PAGE` (9) mipped preview textures at preview size (about 10-20 MB each at 2-3k px, plus any Full-fidelity crops) stay allocated for the whole Grid session. They free only when the Loupe is entered again. `request_web_preview` on wasm also keeps their paths alive via `compare_tiles()`. On the 4 GB wasm heap and on small GPUs this is a real, avoidable hold.
- Suggested fix: do the `view == Edit` free before the `mode != Loupe` return, or free in `enter_grid`/`set_group_view(Edit)` when a renderer is available.

### [APP-NAV-03] `compare_zoom_uv` can panic through `f32::clamp` — Severity: Low — Confidence: High
- Location: `src/app/group_compare.rs:73-81`, fed by `GroupCompare::new` at `:323-326`.
- Evidence: `center.x.clamp(half.x, 1.0 - half.x)` panics if `half.x > 0.5` or `half.x` is NaN. `half = side * short / w / 2`. `side` comes from `LIGHTPHOTOS_COMPARE_SIDE` parsed with no range check, so `=2` or `=NaN` gives a panic on the first Compare frame. `w == 0` or `h == 0` gives `0.0/0.0`, also NaN. The other writers of `side` are clamped (`resize_compare_square` and `face_frame` use 0.02..1.0), and `Tile`/`sizes` come from uploaded images, so the practical trigger is the env var only.
- Impact: a panic in non-test UI code, against the CLAUDE.md "never panics" rule. It is a debug knob, so severity is low, but `compare_zoom_uv` is `pub(crate)` and also called from `ui/loupe.rs` with `sizes` values.
- Suggested fix: clamp `side` at construction (`.clamp(0.02, 1.0)` with a finite check), and guard `w == 0 || h == 0` in `compare_zoom_uv`. Alternatively use `max(min)`/`min(max)` chains, which cannot panic. Add a test with `side = 2.0` and `w = 0`.

### [APP-NAV-04] `place_of` rebuilds the playlist-to-cell map on every call — Severity: Low — Confidence: High
- Location: `src/app/nav.rs:302-304`; callers `src/app/folder_store.rs:645`, `:706`, `src/app/session.rs:281`, `src/app/commands.rs:540`, `src/app/mod.rs:1214`.
- Evidence: `Placer` memoizes `cells` (and `group_cells`) precisely to avoid repeated O(n) work. `place_of` does `Placer::new(self).place(idx)`, so with any sort other than Name ascending (`visible_in_order == false`) every call builds a fresh `HashMap` of all `visible`.
- Impact: `ungroup_selected` calls it per freed member (`folder_store.rs:706`), so ungrouping many stacks in a 50k-photo folder under a Rating/Time sort is O(members × n), in the hundreds of millions of operations on the UI thread.
- Suggested fix: add a `placer()` accessor, or a `places(&self, impl IntoIterator<usize>)` that shares one `Placer`, and use it in the batch callers.

### [APP-NAV-05] Compare pane does heavy per-frame allocation and sorting — Severity: Low — Confidence: Medium
- Location: `src/app/group_compare.rs:407-422` (`sync_compare_tiles`), `:732-743` (`compare_order`), `:756-774`, `:821-832` (`group_picks`), `src/app/nav.rs:334-357`.
- Evidence: `action_path_refs` and `action_count` are documented as "readers that run every frame", and both call `group_picks()`. That goes `pickable()`, then `pane_members()`, then `compare_set()` (one `PathBuf` join per member) and `compare_order()` (a score sort, plus `selected_path()` allocations in `shown_group` and `shown_adhoc`). It runs only while the pane is open, but then several times per frame. `sync_compare_tiles` also clones `paths` three times per frame (`paths.clone()` for `compare_order`, one inside the `take_if` key, and `page_paths().to_vec()`), and `t.key != (paths.clone(), px)` clones the whole member list just to compare.
- Impact: a burst stack of several hundred photos makes each Compare frame allocate thousands of `PathBuf`s. Bursts are the main way big groups get created.
- Suggested fix: compare the key without cloning (compare `paths` and `px` separately); cache `compare_order` against a (group revision, flag filter, score revision) tuple; make `group_picks` a plain scan of the stored picks when no pick is stale.

### [APP-NAV-06] Compare picks are discarded when the preview size bucket changes — Severity: Low — Confidence: Medium
- Location: `src/app/group_compare.rs:415-422`.
- Evidence: the stale-tiles branch does `self.group_compare.picks = GroupPicks::default()` whenever `t.key != (paths, px)`. `px` is `preview_px()`, derived from the window size (`accessors.rs:259`), so resizing or moving the window across a bucket boundary re-uploads the tiles and also wipes the picks.
- Impact: a user who picked three tiles to delete and drags the window edge silently loses the picks. This is the safe direction, but surprising. The picks are already filtered on read (`group_picks` re-checks `pickable`), so the clear is only needed when the member set changes.
- Suggested fix: clear the picks only when `paths` changed, not `px`.

### [APP-NAV-07] `scroll_filmstrip` loops without a bound — Severity: Low — Confidence: Low
- Location: `src/app/nav.rs:759-770`.
- Evidence: `while accum >= STEP_PX { step_loupe(false); accum -= STEP_PX; }`. With a huge finite delta this performs delta/30 full `step_loupe` calls (each doing `load_selected`, loader requests and `neighbor_paths` allocation). With `+inf` the loop never ends. A NaN delta poisons `filmstrip_scroll_accum` permanently, so the wheel stops working.
- Impact: only an abnormal input device or a synthetic event can trigger it, hence Low. Even a normal fast flick turns into several full photo loads per frame that are immediately cancelled by `retain_previews`.
- Suggested fix: ignore non-finite deltas, clamp the number of steps per frame (for example to 8), and step once by the clamped total instead of calling `load_selected` per step.

### [APP-NAV-08] `neighbor_paths` returns the same photo twice when there are two photos — Severity: Low — Confidence: High
- Location: `src/app/nav.rs:736-753`.
- Evidence: with `n == 2`, `(cur + n - 1) % n == (cur + 1) % n`, so previous and next are the same cell and `paths` has duplicates. `request_neighbors` then requests each twice, and `neighbor_fulls_fit` (`jobs/loader.rs:1648`) counts the duplicate's bytes twice and compares `neighbors.len()` to the tier limit.
- Impact: harmless duplicate requests (the loader de-duplicates) but a pessimistic fit check, which can skip the full-resolution prefetch for a two-photo folder.
- Suggested fix: dedupe `positions` (skip the second when equal to the first, and skip `cur`).

### [APP-NAV-09] Tree arrows jump when the selected folder is not in the visible tree — Severity: Low — Confidence: Medium
- Location: `src/app/nav.rs:1042-1047` (`folder_move`).
- Evidence: `cur = folder_sel.and_then(position).unwrap_or(0)`, then `next = cur + delta`. If `folder_sel` is `None`, or is a folder under a collapsed ancestor (so it is not a row), a Down press moves to row 1 rather than the first row, and Up goes to row 0. The grid's `move_grid` lands on the first cell for the first press, so the two regions behave differently.
- Impact: minor keyboard-navigation surprise, for example after opening a deep folder from the command line and collapsing its parent.
- Suggested fix: when `cur` is not found, land on the nearest visible ancestor of `folder_sel`, else on row 0, without applying `delta` on top.

### [APP-NAV-10] `open_folder` stats the path on the UI thread — Severity: Low — Confidence: Low
- Location: `src/app/nav.rs:1138-1145`.
- Evidence: `path.is_dir()` runs synchronously for catalog roots before opening. For a root on a disconnected SMB/NFS share it can block for the OS timeout.
- Impact: UI hang when clicking a root whose volume is half-gone (a case the code explicitly handles for unplugged drives, which fail fast).
- Suggested fix: check on a worker, or accept the cost and document it.

### [APP-NAV-11] `request_delete_member` overwrites the picks even when no delete can start — Severity: Info — Confidence: High
- Location: `src/app/group_compare.rs:926-935`, `:939-944`.
- Evidence: `request_delete_member` replaces `picks` with the one tile, then `request_delete_picks` silently does nothing if `delete_available()` is false (another delete running).
- Impact: the user's earlier multi-pick is lost and replaced by a single pick with no confirm shown.
- Suggested fix: check `delete_available()` first, or restore the picks on refusal.

### [APP-NAV-12] Merged or stale doc comments and left-in telemetry — Severity: Info — Confidence: High
- Location: `src/app/group_compare.rs:338-345` (the `sync_compare_tiles` doc sits on `compare_fulls_waiting`, and `sync_compare_tiles` at `:389` has none), `:512-515` (broken wrapping), `:910-913` (the "Make the picked member the representative. Nothing unless exactly one member is picked." doc is above `set_member_as_rep`, which takes a path), `src/app/bursts.rs:26-28` (two doc blocks merged on `request_sort_meta`). `group_compare.rs:502-508` prints an `eprintln!` timing line each time a Compare opens.
- Impact: misleading docs for the next reader; the log line is noise in release builds (this crate otherwise logs failures only).
- Suggested fix: move each doc to its function; put the timing line behind a debug env var or `cfg(debug_assertions)`.

### [APP-NAV-13] `nav_arrow` treats any unknown direction as Down; `sort_grid` bypasses the Loupe guard — Severity: Info — Confidence: Medium
- Location: `src/app/nav.rs:1286-1291`; `src/app/sort.rs:150-158` versus `src/app/commands.rs:606-630`.
- Evidence: the `match (dx, dy)` ends with `_ => self.nav_down()`, so `(0,0)` or `(2,0)` moves down. `set_sort` and `reverse_sort` refuse in the Loupe, while the control command `sort_grid` calls `apply_sort` directly (the browser filter on file-reading sorts is also missing there, though harmless since the metadata simply never arrives).
- Impact: none today (callers pass unit vectors); the divergence between the two sort entry points could drift further.
- Suggested fix: route `sort_grid` through the same checks (or document that the command is allowed in the Loupe), and use `(0, 1)` explicitly for Down.

## Test coverage assessment

- `nav.rs` has good scenario tests for groups, stacks, filters and selection remapping (collapse, expand, hidden member in the Loupe, rate-out-of-filter), filmstrip Cmd/Shift selection, F6 cycling, and the develop-slider nudge order.
- Not covered:
  - `scroll_filmstrip`, `folder_move`, `folder_expand`, `folder_collapse`, `toggle_folder`.
  - `neighbor_paths` at n = 1, 2 and 3.
  - `Reorder::Keep` ordering with newly shown cells.
  - `select_toggle` down to an empty selection in the Grid.
  - `extend_grid`.
  - `recompute_cells` under bulk-delete `gone`.
- `sort.rs` covers most behaviour (direction, ties, unsorted-last, stacks, loupe-waits, parse round trip). There is no test for Time, Modified, Size or Shutter, for the "no value goes last" rule on Quality, or for `load()`.
- `bursts.rs` covers grouping, idempotence and the late-selection case. Missing:
  - A scan whose meta jobs never complete, which leaves `burst_scan` set and the "reading" status up until the folder changes.
  - A chain longer than the gap, for example 1,000 photos 0.9 s apart collapsing into one stack.
  - `finish_bursts` with `failed`.
- `group_compare.rs` has thorough pick/rate/flag/delete tests. Missing:
  - Selection-Compare restore after the Grid changes (APP-NAV-01).
  - Freeing tiles on leaving the Loupe (APP-NAV-02).
  - `compare_zoom_uv` with out-of-range side or zero size.
  - `sync_full_crops` (no test at all; its `in_flight` bookkeeping is subtle but looks correct).
- `a_second_run_changes_nothing` and similar tests assert real behaviour. The hard-coded 71-entry order table in `keyboard_nudges_follow_the_develop_panel_order` is brittle: it fails to compile when a slider is added, which is arguably the intent.

## Things done well / verified-safe areas worth noting

- `crop_px`/`crop_image` are properly defensive: `get(..)?` on the buffer, a zero-size guard, and tests with a short buffer. `grid_move`, `range_set` and the `select_range` index math are bounded.
- `pick_group_tile`'s `pickable[a.min(at)..=a.max(at)]` is safe, since both indices come from `position()` on the same vector.
- `sync_full_crops`' `in_flight -= 1` cannot underflow: every decrement matches a tile counted as Waiting.
- `Placer`/`Place::{Cell, Hidden, Gone}` is a clean model. The cursor/selection/anchor remap in `recompute_cells` is well documented and tested. `Reorder::Keep` avoids the photo jumping in the Loupe.
- Shutter sort relies on positive-finite bit ordering, and both decoders filter to `is_finite() && > 0` (`nonmac_decode.rs:145`, `image_decode/mod.rs:616`).
- `SortOrder::parse` is strict and round-trip tested. `drop_unavailable_sort` and the wasm filter keep restored prefs from selecting an impossible sort.
- Delete from Compare asks first, never includes the representative, and re-reads picks at confirm time.
- `bursts::finish_bursts` batches one history step and keeps going after a partial failure only by stopping cleanly.

## Open questions for maintainers

1. Should `enter_loupe` collapse a Grid multi-selection? It leaves `selected` as is, so `selected_paths()` in the Loupe still names every Grid-selected photo while only one is on screen. The filmstrip multi-select tests suggest this is intended. If so, rating or flagging with a key in the Loupe applies to all of them.
2. Is an unbounded burst chain intended? `group_by_time` chains frames within `BURST_GAP` of the previous one, so a 1,000-frame timelapse at 1 s intervals becomes a single stack. Is there a cap or a "split at N" rule, in `Group::new` or elsewhere?
3. `request_sort_meta` runs from every `recompute_cells` call for the file-reading sorts, and it walks the whole playlist each time. Was a single "requested" flag per folder considered? It matters because `recompute_cells` also runs on each rating change.
