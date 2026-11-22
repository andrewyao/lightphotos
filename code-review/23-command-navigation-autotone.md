# Top-level command.rs, navigation.rs, autotone.rs

## Scope
Every line of these files was read:

| File | Lines |
|---|---|
| `src/command.rs` | 2226 |
| `src/navigation.rs` | 1028 |
| `src/autotone.rs` | 859 |

Cross-references read to verify contracts: `src/app/commands.rs` (dispatch and `check`), `src/app/agent.rs` (agent policy), `src/app/geotag.rs`, `src/geotag/mod.rs`, `src/develop/mod.rs` (`Adjustments`, `SLIDERS`, `Develop::new`), `src/sidecar.rs` (`ImageRecord::edit`), and `src/app/nav.rs`, `src/app/bulk_delete.rs` and `src/app/autotone.rs` (callers).

## Summary
The code is careful and mostly safe. Non-test code has no `unwrap`/`expect`/`panic`, and input-derived numbers are validated before use: `DevelopParams::validate`, `CropParams::validate`, `Stars`, and mask caps. The command enum is exhaustive. Every `match` on `Command` (`id`, `writes_photos`, `photos`, `about`, `apply_to_record`, and the app's `apply_command` and agent policy) lists all variants with no `_` arm. The `apply_to_records` headless contract is pinned by a test.

Biggest risks:
- `Playlist::from_file` mishandles a bare relative filename.
- `geotag.import` passes an unvalidated clock offset into code that does `i64::abs()`.
- Autotone builds a `Develop` per histogram bin and per sample, which looks like heavy UI-thread work.
- The "malformed requests" test cannot fail for most of its inputs.

Findings: 0 Critical, 0 High, 3 Medium, 6 Low, 3 Info.

## Findings

### [CORE-01] Bare relative filename gives a one-photo playlist with an empty directory  — Severity: Medium — Confidence: High
- Location: `src/navigation.rs:476` (`Playlist::from_file`); caller `src/app/mod.rs:1207`; `src/main.rs:730-735` passes the CLI argument through unmodified.
- Evidence: `let dir = current.parent().unwrap_or_else(|| Path::new("."));`. For `Path::new("photo.jpg")`, `parent()` returns `Some("")`, not `None`. I verified this with a scratch `rustc` program: `Some("")`, and `read_dir("")` fails with ENOENT. The `"."` fallback is therefore dead for the common case.
- Impact: `lightphotos IMG_0001.jpg`, run from the photo's folder, gives these results:
  - `sorted_images_in("")` returns nothing, so the playlist is just that one file.
  - `dir` is `""`, so the folder store and sidecars resolve relative to the cwd.
  - `add_root("")` and `set_anchor("")` are called with an empty path.
  - The user gets no siblings to cull and a bogus tree root. `./IMG.jpg` and absolute paths work.
- Suggested fix: Treat an empty parent as `.`, or better, make the path absolute in `main` or `App::open`. In `from_file`, use `current.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."))`. Add a test that opens a bare name after `set_current_dir`, or a pure unit test of the parent logic.

### [CORE-02] Autotone constructs a full `Develop` per histogram bin and per sample  — Severity: Medium — Confidence: Medium
- Location: `src/autotone.rs:77-80` (`display`), `82-84` (`respond`), `122` (`Histogram::build`), `165` (`percentile`), `95-101` (`fit_to_level`).
- Evidence: `display` calls `develop::Develop::new(adj)` on every call. `Develop::new` runs `adj.clamped()` (a loop over 71 `SLIDERS`), `ColorOps::new` and `GradeOps::new`, and maybe a curve LUT. It is called:
  - once per sample in `build`. The grid is up to about 65k samples (`downsample_linear(.., 256)`), and the identity `Adjustments` never changes;
  - 256 times per `percentile` call;
  - 17 times per bin in `fit_to_level`.

  `analyze` makes roughly 150 `measure` calls per pass over 4 passes. That is about 600 `percentile` calls, or about 150k `Develop::new` calls, plus 40-65k in `build`.
- Impact: Roughly 200k `Develop` constructions per photo. `poll_auto_tone` runs `analyze` on the UI thread and checks its frame budget only between photos (`src/app/autotone.rs:374-381`), so one analysis can overrun a frame by a large factor. This is likely why a profile hook exists in `shell/profile.rs`. I did not measure it.
- Suggested fix: Hoist `Develop::new(adj)` out of the loops. In `percentile`, build it once per call. In `build`, build the identity `Develop` once. Pass `&Develop` into `respond` and `fit_to_level`. This cuts the work by about 256x with no behaviour change.

### [CORE-03] "malformed requests are errors" test passes for any parse that succeeds  — Severity: Medium — Confidence: High
- Location: `src/command.rs:1724-1753` (`malformed_requests_are_errors_not_panics`).
- Evidence: `Ok(_) => Err(CommandError::NoPhotos)` then `assert!(checked.is_err())`. Any input that parses to a non-`Develop` command is converted into an error, so the assertion holds regardless.
- Impact: These cases would still pass if validation regressed, for example if `stars: 6` or `-1` were accepted, if an unknown field or `extra` key were tolerated, or if `{"photos":"some"}` were accepted. The `deny_unknown_fields` and `Stars` guards the test claims to cover are effectively unverified here. Some are covered elsewhere, such as `stars: 9` in `an_unknown_id_and_bad_params_are_told_apart`.
- Suggested fix: Assert `parsed.is_err()` for the inputs that must fail at parse time. Keep a separate table for inputs that parse and are refused by `validate()`. If the intent is only "no panic", rename the test.

### [CORE-04] `geotag.import` `camera_minus_utc_s` is not range-checked at the command layer  — Severity: Low — Confidence: Medium
- Location: `src/command.rs:933-934` (`GeotagParams`), `src/app/commands.rs:465`, `src/app/geotag.rs:410-419`, `src/geotag/mod.rs:203-210`.
- Evidence: The GPX path validates offsets with `offset()` against `MAX_CLOCK_ERROR_S`. The command takes any `i64`. `ClockModel::select` then does `.filter(|s| s.abs() <= MAX_CLOCK_ERROR_S)` and silently drops an out-of-range value.
- Impact:
  - A bad offset such as 999999999 makes `geotag.import` report success while locating no photo with that offset, and the caller gets no error.
  - `i64::MIN.abs()` overflows. That panics in debug and tests, which have overflow checks on. In release it wraps to `i64::MIN`, which passes the filter and is then skipped by `checked_sub`. This is reachable from an MCP caller sending `-9223372036854775808`.
- Suggested fix: Validate `camera_minus_utc_s` in the command layer (a `GeotagParams::validate` called from `App::check`), and return `OutOfRange` or `BadParams`. Also change `select` to use `unsigned_abs()` or `checked_abs`.

### [CORE-05] `from_file` silently opens the first sibling when the requested file isn't in the listing  — Severity: Low — Confidence: Medium
- Location: `src/navigation.rs:483-492`.
- Evidence: `.unwrap_or(0)` after both the name match and the normalized-path match fail. This happens when the requested file is non-empty-listing but not listed (extension not recognised, hidden by `keep_name`, or removed between the `metadata` check and the listing).
- Impact: Opening `dir/foo.xyz` next to `a.jpg` positions the Loupe on `a.jpg` with no message. Only the empty-listing case appends `current`.
- Suggested fix: When no entry matches, insert `current` into the sorted list (or return an error and surface it). At minimum, log the miss.

### [CORE-06] Explicit `develop.monochrome {on:false}` resets Saturation on photos that were not B&W  — Severity: Low — Confidence: High
- Location: `src/command.rs:550-554` (`apply_to_record`), mirrored in `src/app/commands.rs:346-354`.
- Evidence: `saturation = if on { -100.0 } else { 0.0 }; Adjustments { saturation, ..adj }` is applied to every named photo regardless of its current state.
- Impact: `{"on": false}` over a set that includes colour photos with Saturation +30 silently wipes that to 0 (undoable in the app, but the headless sidecar path has no undo). The toggle (`on` absent) is safe, because it only turns off when every photo is mono.
- Suggested fix: For `on == false`, change only photos where `is_monochrome()`. For `on == true`, skip photos that are already mono. Add a test.

### [CORE-07] `resolve_one` scans linearly per name, three times per command, though `Playlist::index_of` exists  — Severity: Low — Confidence: Medium
- Location: `src/app/commands.rs:166-180` (consumer); `src/navigation.rs:534` (`index_of`, unused here); `src/command.rs:104` (`Photos::Named(Vec<PathBuf>)`, unbounded).
- Evidence: `entries.iter().find(..)` per named photo. `resolve` runs in `check`, again in `apply_command`, and a third time in the agent's `photos_outside_anchor`.
- Impact: 10k names against a 10k-photo folder is about 3x10^8 path comparisons on the UI thread for one MCP call.
- Suggested fix: Use `index_of(file_name)` and confirm the full path for absolute names. Consider capping `Named` length.

### [CORE-08] Directory read errors are indistinguishable from an empty folder  — Severity: Low — Confidence: Medium
- Location: `src/navigation.rs:283-286`, `286` (`filter_map(Result::ok)`), `271` (doc: "Empty when `dir` can't be read").
- Evidence: A failed `read_dir` returns `Vec::new()`. Per-entry I/O errors are dropped.
- Impact: A permission-denied or flaky network folder shows as having no photos, and a partially failing listing silently omits some photos. A user could conclude photos are missing.
- Suggested fix: Return `Result` (or an error flag) from `Playlist::from_dir` and `list_subdirs` and surface it in the status toast. Count and report skipped entries.

### [CORE-09] `grid_cells` indexes a `HashMap` with `[]`  — Severity: Low — Confidence: High
- Location: `src/navigation.rs:180` (`stack_passes[stack]`).
- Evidence: The key is always present because the first loop inserts every collapsed `Cover` and `Member`, so no panic today. The proof is non-local and breaks silently if the `Slot` match arms are edited. It also violates the no-indexing rule in CLAUDE.md.
- Impact: Latent panic on a future edit. The code is already correct today.
- Suggested fix: `stack_passes.get(stack).copied().unwrap_or(false)`.

### [CORE-10] `shift_index` can underflow if `removed` is not strictly ascending  — Severity: Info — Confidence: Medium
- Location: `src/navigation.rs:564-569`.
- Evidence: `Err(before) => Some(i - before)`. With duplicate or unsorted `removed`, `before` can exceed `i`. The only caller passes `remove_matching`'s strictly ascending output, so this is safe today.
- Suggested fix: `i.checked_sub(before)`, or `saturating_sub`.

### [CORE-11] `remove_matching` calls `remove` twice per entry and leaves stale `links`  — Severity: Info — Confidence: Medium
- Location: `src/navigation.rs:550-559`.
- Evidence: The `filter` collecting indices and `retain` each call `remove`. If the predicate were stateful or could change between passes, the returned indices would disagree with the retained set. The only caller is a pure `HashSet::contains`. Removed entries also stay in `self.links`, which the agent anchor check consults.
- Suggested fix: Do one pass, building `removed` while compacting. Drop removed paths from `links`.

### [CORE-12] Tie order among case-insensitive twins depends on `read_dir` order  — Severity: Info — Confidence: Medium
- Location: `src/navigation.rs:318-324` (`sort_by_name`, `name_key`).
- Evidence: `a.jpg` and `A.jpg` (possible on Linux) get equal keys. `sort_by_cached_key` is stable, so the order is whatever the OS returned. `index_of` handles the tie correctly, but the order is non-deterministic across runs and OSes.
- Suggested fix: Add the raw name as a secondary key in both the sort and `cmp_name_key`.

## Test coverage assessment
- **Good:**
  - Command round-trip, example-per-command, unique ids and headless-flag parity (`apply_to_records_runs_exactly_the_commands_listed_as_headless`).
  - Mask caps, atomic refusal across records (`a_command_refused_on_one_record_changes_none`), and hostile rotation wrap.
  - Autotone percentile and bin-stand-in fidelity, RAW monotonicity of the solve measures, and slider range bounds.
  - Navigation: `index_of` on case ties, `cmp_name_key` vs `name_key`, stack and filter combinations.
- **Weak or never-failing:**
  - `malformed_requests_are_errors_not_panics` (CORE-03).
  - `lists_sorted_siblings_positioned_on_opened_file` and `from_dir_enumerates_images_at_index_zero` (`src/navigation.rs:959`, `1017`) silently `return` when `/tmp/iv-test` is absent, so they never run in CI.
  - `every_command_has_an_example` scrapes serde's error text (`split('`')`), which is fragile to serde upgrades. It does assert a minimum tag count, which partly guards this.
- **Missing:**
  - Parse tests for `select`, `navigate`, `view.mode`, `view.zoom`, `catalog.*`, `geotag.import` and `export`.
  - A headless `Monochrome` explicit-off test and a mixed-set toggle test.
  - Autotone on flat black, flat white, a single sample and NaN/inf pixels. NaN lands in bin 0 with a black stand-in, which I reasoned is harmless but is untested.
  - Convergence and idempotence: `analyze` ends on the shadows solve, which shifts the median after the exposure solve. Only one test checks the final median, within 0.05.
  - `Playlist::from_file` with a bare relative name, a name not in the listing, and a missing directory.
  - `grid_move` with extreme `dx`/`dy`.

## Things done well / verified-safe areas
- No non-test panics anywhere in the three files. `out.remove(i)`, `get_mut`, `self.entries[start..]` and `self.count[first..]` are bounds-guarded. `output[len]` in `percentile` is at most `BINS`.
- `DevelopParams::validate` rejects non-finite and out-of-range values before any photo is touched. `By` deltas go through `stepped`, which clamps, or wraps a hue with `rem_euclid`. A huge `by` yields infinity that is clamped, not NaN, because steps are at least 0.05.
- `CropParams::apply` guards a non-positive or non-finite size, and a `turn.covers` failure returns a corrective example crop.
- The per-record refusal semantic ("a refused command on any photo changes none") is implemented by editing a clone.
- Autotone bisection domains are monotone (tested for RAW). Highlights is held to 0 or below and Shadows to 0 or above. Exposure is clamped, `Histogram::build` rejects an empty sample, and `fraction_above` / `percentile` guard `total` and array bounds.
- `grid_move` has no division and handles `len == 0` and `cols == 0`.
- `flatten_visible_tree` recursion is bounded by the finite expanded set plus cached `subdirs`. I found no reachable cycle, since distinct symlink-loop paths each require a user expansion.
- The agent policy (`run_from_agent`) is exhaustive over `Command`, so a new variant forces a decision.

## Open questions for maintainers
1. `Command::photos()` returns `None` for `Select`, even though `SelectParams` has a `photos` field. The agent policy handles it separately via `link_outside`. Is excluding it from `photos()` deliberate?
2. `command::reset` and `App::reset_adjustments` (`src/app/adjust.rs:326-346`) duplicate the "keep crop and straighten" logic. Should the app call `command::reset`, as the Command path already does, to keep them in step?
3. `analyze` always measures against the identity render, ignoring the user's existing WB, mixer, curve and B&W state, while `merge` keeps those. Is that intended, since the "Auto" median target will be off for an edited photo?
4. For `develop.monochrome`, is restoring Saturation to 0 (not to the pre-B&W value) the intended off state?
