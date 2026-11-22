# src/persist (part A) — catalog, groups, lr_preset, prefs, mod

## Scope

Every line of these files was read:

| File | Lines |
|---|---|
| `src/persist/mod.rs` | 14 |
| `src/persist/catalog/mod.rs` | 946 |
| `src/persist/catalog/native.rs` | 771 |
| `src/persist/catalog/web.rs` | 145 |
| `src/persist/groups.rs` | 938 |
| `src/persist/lr_preset.rs` | 1304 |
| `src/persist/prefs.rs` | 232 |
| Total | 4350 |

Cross-references checked outside the unit: `src/paths.rs` (`write_atomic_in_dir`, `read_capped`), `src/persist/presets.rs` (load path), `src/persist/preset_folders.rs`, `src/app/presets.rs` (LR import), `src/app/folder_store.rs` (`poll_folder_store_load`), `src/app/catalog.rs`, `src/develop/curve.rs`.

A throwaway `rustc` program in the scratchpad confirmed two language facts. `Path::new("/a/b").starts_with("")` is `true`. `"NaN".parse::<f32>()` is `Ok(NaN)`.

## Summary

This is careful, defensive code. Parsers use `get()` and checked math. Corrupt records are never overwritten. All writes go through `write_atomic_in_dir`. The writer queue coalesces correctly. The groups model has a clear crash-ordering story. The LR scanner is linear-time and has good hostile-input tests. I found no panics reachable from input in the non-test code.

The risks are data-integrity and UX issues at the seams, not crashes:
- `prefs::load` cannot tell "unreadable" from "missing". Because of that, an existing but unreadable presets library is replaced by an empty one on the next save.
- The Lightroom importer accepts presets it cannot map at all (no warning). Its "dropped" report covers only 4 hard-coded fields.
- Format version fields are written but never checked.
- A few edge cases around empty or hostile folder paths and minted group ids.

Findings by severity: Critical 0, High 0, Medium 2, Low 8, Info 4.

## Findings

### [PERSIST-A-01] `prefs::load` conflates "unreadable" with "missing", so an existing presets library can be silently replaced — Severity: Medium — Confidence: High
- Location: `src/persist/prefs.rs:94-96` (`load_at`) and `:7-9` (doc). Consumers: `src/persist/presets.rs:87`, `src/persist/secret.rs:34`.
- Evidence: `std::fs::read_to_string(path).ok()` turns every error into `None`. The module doc promises "a caller can tell a value it cannot parse from one that is missing or unreadable". `PresetStore::load()` does `prefs::load(KEY).unwrap_or_default()` and then `from_text(&text, true)`. An empty string is a normal, writable, empty library. Only a parse failure sets `corrupt` and refuses writes.
- Impact: any read failure on an existing `presets` file looks like "no presets". Triggers include invalid UTF-8 (a partial write by an old non-atomic build, a hand edit, an encoding change), EACCES, EIO, or the path being a directory. The first preset save then atomically replaces the user's whole library. This is the exact corruption the `corrupt`/refuse-writes design exists to prevent, but it only guards the parse-failure branch.
- Suggested fix: make the API return `Result<Option<String>, String>`. Return `Ok(None)` only for `NotFound`. Let `PresetStore::load` treat `Err` like the corrupt case: empty list, writes refused, error shown. Use `String::from_utf8` on the bytes so a non-UTF-8 file is an error and not a miss. Keep the wasm variant's semantics (`localStorage` unavailable is an error, absent key is `Ok(None)`).

### [PERSIST-A-02] LR import accepts presets it cannot map, and its "dropped fields" report is only a 4-item hard-coded list — Severity: Medium — Confidence: High
- Location: `src/persist/lr_preset.rs:176-185` (`UNSUPPORTED`), `:188-273` (`parse`), `:23-24` (`Imported::dropped` doc: "`crs:` fields Lightroom set that have no field here"). Consumers: `src/app/presets.rs:431-447`, `src/persist/preset_folders.rs:86`.
- Evidence: `parse` returns `Err` only if there is no `rdf:Description`, no `crs:` text at all, or `PresetType` is not Normal. Everything else yields `Ok`. The only fields reported as left behind are `Temperature`, `Tint`, `ColorNoiseReduction` (away from 25) and `CropAngle`. Nothing checks whether anything was mapped.
- Impact:
  1. A preset that carries only unmapped data imports as a successful, empty look. Examples: a lens-correction-only preset, a crop preset (`CropTop`/`CropLeft`...), a local-adjustment or mask preset (`MaskGroupBasedCorrections`, `GradientBasedCorrections`), a Calibration or `Parametric*` preset, or a legacy PV2010 preset (`crs:Exposure`, `crs:Shadows`, `crs:Brightness` and so on). The status line reports success and the library gains a no-op preset with no note.
  2. A preset that mixes mapped and unmapped data (lens profile, CA, defringe, masks, crop, `ParametricShadows`...) imports with no hint that part was lost. `notes()` says "could not carry X, Y" for only the 4 keys.
  3. The same applies to the folder scanner (`read_preset` -> `parse(..).ok()?`), where an empty look appears in the Presets panel.
- Suggested fix: add a "nothing mapped" check. `Imported.adj == Adjustments::default() && curve linear` (and not monochrome) should return `Err("no develop settings this app can carry")`, or at least a note. Broaden the dropped detection with a data-driven table of known unsupported `crs:` families (crop, lens, local corrections, calibration, parametric tone, legacy PV2010 keys). Alternatively, enumerate every `crs:` attribute and child element on the description and report any key that is not in the mapped table or a known-benign metadata list (`Version`, `UUID`, `HasSettings`, `Name`, `Group`...).

### [PERSIST-A-03] `v` in `catalog.json`, `presets.json` and shards is written but never checked — Severity: Low — Confidence: Medium
- Location: `src/persist/catalog/mod.rs:37` (`VERSION`), `:241-245` (`RecordFile.v`), `:250-255` (`Shard.v`), `:632-645` (`parse_record`), `:616-629` (`parse_shard`).
- Evidence: `RecordFile`/`Shard` deserialize `v` and discard it. There is no `v > VERSION` rejection, no migration hook, and no `#[serde(flatten)]` extras bucket.
- Impact: a catalog written by a newer build (a `v:2` record whose `folders` semantics or entry type changed, or with extra fields) is read as v1. The first folder add then rewrites it as `{v:1, folders}` and drops the unknown fields. The module doc already cites this exact failure for v0.1.26/27 ("would drop a field they don't know") and works around it with a separate preset-folders record, but nothing prevents it from recurring. Today there is nothing to migrate, so the risk is forward only.
- Suggested fix: treat `v > VERSION` as an unreadable record (`Record::Unreadable`, never overwritten, clear message). Add `v == 0 || v == VERSION` handling as the migration seam. Add a test with `{"v":2,...}`.

### [PERSIST-A-04] An empty (or relative) folder path wipes every root; `parse_record` does not validate entries — Severity: Low — Confidence: Medium
- Location: `src/persist/catalog/mod.rs:362-370` (`add_root`), `:632-645` (`parse_record`), `:297-307` (`Record::add`), `:430-432` (`contains`).
- Evidence: `Path::new("/x").starts_with("")` is `true`. In `add_root`, `roots.retain(|r| !r.starts_with(dir))` therefore removes every root when `dir == ""`. In the opposite direction, if `""` is already a root, `dir.starts_with(r)` rejects every add and `contains()` is true for every directory. `parse_record` feeds each stored entry through `add_root` with no check for empty, relative or `..` paths.
- Impact: a hand-edited, truncated or buggy record `{"folders":["/a","/b",""]}` collapses to `[""]`. Then `contains` is true for everything, so `record_marks` writes shards for any open directory, and no real folder can be added. `Catalog::add_folder` has the same hazard if any caller ever passes an empty path.
- Suggested fix: in `add_root`, and when parsing and adding, reject `dir.as_os_str().is_empty()` and non-absolute paths (`!dir.is_absolute()`). Skip bad entries when parsing, or treat the record as unreadable. Add a test.

### [PERSIST-A-05] A planted far-future group id makes new ids stop sorting newest — Severity: Low — Confidence: Medium
- Location: `src/persist/groups.rs:48-78` (`GroupId::mint`, esp. `:57-59`), `:81-82` (`MAX_MINTED_MS`).
- Evidence: `ms = newest.map_or(now, |n| now.max(n.saturating_add(1))).min(MAX_MINTED_MS)`. If the folder holds a sidecar named like `g-ffffffffffff-xxxxxx` (any hand-written or hostile file passes `minted_ms`), then `newest == MAX` and every new id gets the same `ms`. Ordering then falls to the name's 24-bit hash suffix, which is effectively random.
- Impact: the doc'd crash-safety invariant ("the new group's id is the newest, so a crash partway through loads with the photos in the new group") fails about half the time in such a folder. Moderately far-future ids merely pin all future ids to that time, which works but is surprising. The trigger needs a hand-edited or planted id.
- Suggested fix: treat minted ids with `ms > now + slack` as unminted (ordered by name) in `minted_ms`, or have `mint` bump the hash portion's counter, so the new id compares strictly greater than `newest`'s full id. Test `mint` with `newest = Some(MAX_MINTED_MS)`.

### [PERSIST-A-06] Size caps are enforced only on read; writes can exceed them and make the catalog unreadable — Severity: Low — Confidence: Medium
- Location: `src/persist/catalog/mod.rs:40` (`MAX_RECORD_BYTES`), `:45` (`MAX_SHARD_BYTES`), `:572-595` (`record_marks`), `:647-653` (`record_bytes`).
- Evidence: `record_bytes` and `record_marks` serialize without checking length. `parse_record` rejects `> 1 MiB`, and `parse_shard` (test-only today) rejects `> 16 MiB`.
- Impact: a catalog with up to `MAX_FOLDERS = 1000` roots, each a long path (up to PATH_MAX 4096 bytes, more with JSON escapes on Windows), can exceed 1 MiB. The next `open` fails `TooBig`, the file is "never overwritten", and the catalog is permanently unopenable until hand-edited. Unrealistic with ordinary paths but unguarded. A very large marks directory (>~250k marked photos) would write a shard the future cross-folder reader refuses.
- Suggested fix: in `Record::add`, also refuse when the serialized size would exceed the read cap. In `record_marks`, report an error or skip the shard when `bytes.len() > MAX_SHARD_BYTES`.

### [PERSIST-A-07] A failed shard write is never retried, and shards of removed folders are never cleaned up — Severity: Low — Confidence: High
- Location: `src/persist/catalog/mod.rs:589-594` (`record_marks`), `:448-454` (`remove_folder`).
- Evidence: `self.written.insert(dir, sig)` runs before `put_shard`. The writer reports failure later through `pump`, but `written` still holds the signature. Re-recording identical marks is skipped, so the shard stays missing or stale until the marks change. `remove_folder` does not delete the folder's `marks/<hash>.json` (or the web `Marks` entry). `record_marks` returns early for directories outside the catalog.
- Impact: the copy is documented as disposable, so this is low. In practice a transient ENOSPC leaves one directory's shard stale for the session. Orphan shards accumulate, and a future cross-folder reader would have to filter by `shard.dir` against current roots.
- Suggested fix: clear `written[dir]` on a reported failure (the error string needs the file path mapped back to a dir, or have the writer return failed files). Optionally queue a delete of shards for removed roots.

### [PERSIST-A-08] Catalog writes recreate a missing catalog directory and its parents — Severity: Low — Confidence: Medium
- Location: `src/persist/catalog/native.rs:107` (`create_at`), `:156-187` (`save_record`/`save_presets`/`put_shard` via `Writer::perform` -> `write_atomic_in_dir`), `src/paths.rs:186-201` (`create_sidecar_dir`). Also the doc at `native.rs:84-86`.
- Evidence: `write_atomic_in_dir` calls `create_sidecar_dir(parent)`, which only protects `.lightphotos` sidecar directories (the `photos.is_dir()` check). A catalog dir is `<name>.lightphotos`, so it falls through to plain `create_dir_all`. Separately, `create_at`'s doc says it is refused "where anything that is not an empty directory is", but the code only refuses an existing `catalog.json` or a non-directory.
- Impact: if the user deletes or ejects the volume holding a catalog while the app runs, the next folder add or marks sync recreates `<volume>/.../x.lightphotos/catalog.json`. For an unmounted `/Volumes/X/...` that makes a stray directory on the boot disk. The catalog then has only the in-memory roots, which silently replaces the user's real catalog on remount.
- Suggested fix: after the catalog is opened, writes should require the catalog dir to still exist (`create_dir` for `marks/` only, not `create_dir_all` upward). Fix the `create_at` doc, or check the directory is empty.

### [PERSIST-A-09] `Groups::from_loaded` makes any photo absent at load a permanent repair, and one caller passes `&[]` — Severity: Low — Confidence: Low
- Location: `src/persist/groups.rs:206-233`; caller `src/app/folder_store.rs:289-293`; applied at `src/persist/folder_store.rs:522-531` and `:558`.
- Evidence: the caller passes `Some(p) if p.dir() == dir => p.entries(), _ => &[]`. `apply_loaded` builds `names` from those entries, and `from_loaded` drops every member not in `names`. Each change is recorded in `repairs`, and `take_repairs` queues Put/Delete sidecar writes on the next group mutation.
- Impact: if the load for `dir` lands while the playlist is for a different directory or `None` (the `&[]` arm), while `folder_store.dir == dir` (so the groups are kept), every group has zero photos. All of them are dropped in memory and recorded as `Delete` repairs. The next group action in that folder, such as one create, then deletes every group sidecar. The same applies if the listing is ever incomplete (streamed in, filtered, or a photo temporarily missing). I could not construct a concrete ordering that triggers the `&[]` arm; the playlist normally exists before `request_folder_store_load`.
- Suggested fix: when the photo list is not the folder's authoritative full listing, skip reconciliation. Pass an `Option`, and keep the loaded groups unrepaired. Or only record repairs for members that are definitely gone (the file stat'd missing), not merely absent from the list.

### [PERSIST-A-10] Config/default directory derivation accepts empty or relative env values — Severity: Low — Confidence: Medium
- Location: `src/persist/prefs.rs:66-80` (`config_dir_from`), `src/persist/catalog/native.rs:45-51` (`default_dir_from`).
- Evidence: `XDG_CONFIG_HOME=""` gives `PathBuf::from("").join("lightphotos")`, a CWD-relative `lightphotos/`. The XDG spec says to ignore relative values. An empty `HOME` gives a relative `Library/Application Support/LightPhotos` or `Pictures/LightPhotos`. `prefs::save` then writes into the current directory. `load_at` also uses an unbounded `read_to_string`.
- Impact: settings and the default catalog silently land in the CWD for a launcher that sets empty variables. They are lost when CWD changes. Low likelihood.
- Suggested fix: filter `Some(p) if p.is_absolute() && !p.as_os_str().is_empty()` for all four variables. Use `read_capped` for prefs.

### [PERSIST-A-11] Curve point parser turns `NaN`/`inf` into valid points — Severity: Info — Confidence: High
- Location: `src/persist/lr_preset.rs:390-402` (`curve`).
- Evidence: `n.trim().parse::<f32>()` accepts `NaN`, `inf` and `-inf`. `.round().clamp(0.0, 255.0) as u8` maps NaN to 0 and inf to 255. `number()` rejects non-finite values, but `curve` does not.
- Impact: no panic. A corrupt curve such as `NaN, NaN` imports as a `[0,0]` point instead of being reported in `dropped`. Also non-integral or out-of-range values are rounded or clamped without notice.
- Suggested fix: `.filter(|v| v.is_finite())` before rounding.

### [PERSIST-A-12] LR scanner re-scans the whole body for every key (up to ~100 keys x 1 MiB) — Severity: Info — Confidence: Medium
- Location: `src/persist/lr_preset.rs:188-273`, `:463-531` (`raw_value`/`attribute`/`element`).
- Evidence: each `number()` call allocates `format!` strings and runs `match_indices` over the body (plus a second pass for the element spelling). `parse` calls it for about 90 sliders, the grade keys again for the split-toning test, and the unsupported table. Folder scans (`LIMITS.file_bytes = 1 MiB`) run `parse` per `.xmp`.
- Impact: tens to hundreds of milliseconds worst case per hostile 1 MiB file, on a worker thread. The quadratic cases are already covered by `unclosed_tags_parse_in_linear_time`. Not a defect today.
- Suggested fix: optional. Index the attribute/element names once into a `HashMap<&str, &str>` in a single pass.

### [PERSIST-A-13] Invariant-based indexing in `groups.rs` contradicts the "no unchecked `[]`" rule — Severity: Info — Confidence: High
- Location: `src/persist/groups.rs:143` (`self.members[0]`), `:250` (`self.by_id[stack]`), `:281-283` (`group.members[0]`, `self.by_id[id]`), `:335` (`self.by_id[id]`).
- Evidence: all are safe given the private fields. `Group::new`/`with_rep_if_member` enforce at least 2 members. `of` and `by_id` are kept in step by `insert`/`remove`/`apply`. A duplicate `GroupId` in the `files` argument of `from_loaded` would desynchronize them, but no panic would follow.
- Impact: a future refactor that breaks the invariant would panic, where CLAUDE.md asks for `get()`.
- Suggested fix: `get(..)` with a fallback (`Slot::Single` or skip). `slot()` runs once per photo per frame, so a failed lookup should not be costly. Optionally `debug_assert!` the invariant.

### [PERSIST-A-14] Catalog `last_error` and writer-thread edge cases — Severity: Info — Confidence: Medium
- Location: `src/persist/catalog/mod.rs:559-567`; `src/persist/catalog/native.rs:278-314`, `:351-365`.
- Evidence: `note_error` overwrites `last_error`, so several failures in a frame surface only the last. If `perform` ever panicked, the writer thread would die with `in_flight` stuck above 0. `flush_blocking` would then wait the full timeout and `backlog()` would never reach 0, which holds the app open if the UI waits on it (`main.rs:648`). The `Drop` join is unbounded on the UI thread, so a catalog switch blocks while queued writes drain on a slow volume (the comment calls this intentional).
- Impact: no known trigger, since `write_atomic_in_dir` returns errors and does not panic.
- Suggested fix: wrap `perform` in `catch_unwind` so `in_flight` always decrements. Append errors to a bounded list or a count.

## Test coverage assessment

Strong:
- `catalog/mod.rs` and `native.rs` cover the pending/removed merge, unreadable records never being overwritten, round trips, malformed and truncated records and shards, queue coalescing, flush timeout reporting, and write-failure reporting once.
- `groups.rs` covers merge crash-ordering, claim resolution, repairs, replay, clock skew and id safety.
- `lr_preset.rs` has hostile and truncated input tests, an explicit linear-time test, nesting cases, and anchoring tests.
- `prefs.rs` covers every layout's path derivation and the atomic write.

Gaps:
- `web.rs` has no tests at all, including the `write`/`drive` coalescing state machine, which is pure and testable apart from `idb`.
- No test of an unreadable prefs file (non-UTF-8, a directory, EACCES) or of `PresetStore::load` on one (PERSIST-A-01).
- No `v` greater than 1 test (A-03). No test for an empty or relative folder in a record or `add_folder` (A-04). No test for `mint` at `newest = MAX_MINTED_MS` (A-05). No cap-on-write tests (A-06).
- LR import: no test for "preset with only unmapped keys" or a legacy PV2010 preset (A-02), no NaN/inf curve test, and no BOM or UTF-16 input.
- No test that a failed shard write is retried (A-07).
- `a_flush_that_times_out...` bypasses the worker, so a real stuck writer is untested.
- Tests in `mod.rs` use `Catalog::create_at`, so the native-only paths are well exercised, while the wasm `land_record` and `pending` flow is exercised only through `Record` unit tests.

## Things done well / verified-safe areas

- All native catalog and prefs writes use `write_atomic_in_dir` and `write_atomic`. The writer coalesces to the newest bytes per file and orders errors before the "done" count, so `flush_blocking` always collects them.
- Never-overwrite-unreadable is implemented consistently via `Record::{Pending,Loaded,Unreadable}`, and `save_*` only writes `Loaded`. The preset folders have their own file, deliberately, for old-build compatibility.
- `description()` offsets all come from `find` plus `get()`, and `Ahead` keeps open-without-close inputs linear. I traced the nesting depth machine for stray closes, truncated tags and unclosed nesting, and found no panic or out-of-range slice.
- Slider writes are clamped to each slider's range. The `Curve::from` input dedupes by x (u8), so it is bounded to 256 points before `thin`'s O(n^2) loop.
- `GroupId::from_stem` restricts ids to ASCII `[A-Za-z0-9_-]`, so ids cannot escape `groups/`. `sorted_members` rejects non-UTF-8 and duplicate names.
- `Group::new` makes illegal groups unrepresentable. `apply` keeps `of`/`by_id` in step, including the recursive forget.

## Open questions for maintainers

1. Is the `_ => &[]` arm in `poll_folder_store_load` reachable for the same `dir`? If so, `from_loaded` plus `take_repairs` can queue deletes for every group (A-09).
2. Should an LR preset with no mappable fields be refused, or kept with a note (A-02)? Is the 4-key `UNSUPPORTED` list intended to be exhaustive, or a "most common" list?
3. Is there a plan for catalog format versioning before the cross-folder reader ships (A-03, A-06), or is "older builds drop unknown fields" accepted?
4. Is two app instances sharing one catalog supported? Each instance keeps its own `written` map and rewrites the whole `catalog.json` from its in-memory roots, so last writer wins on folder lists.
5. Should `prefs::load` get a real error channel (A-01)? `secret.rs` and `session.rs` also treat unreadable as missing.
