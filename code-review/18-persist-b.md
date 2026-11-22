# src/persist (part B): folder_store, preset_folders, presets, secret, signalcache

## Scope

Every line of these files was read. Line counts:

- `src/persist/folder_store.rs` (2860, about 1090 non-test and 1770 test lines)
- `src/persist/folder_store/group_file.rs` (131)
- `src/persist/folder_store/locks.rs` (548)
- `src/persist/folder_store/writeback.rs` (1730)
- `src/persist/preset_folders.rs` (411)
- `src/persist/presets.rs` (497)
- `src/persist/secret.rs` (95)
- `src/persist/signalcache.rs` (813)

Cross-checked, not reviewed:

- `src/paths.rs` (`write_atomic*`, `read_capped`)
- `src/persist/prefs.rs` (`load`/`save`)
- `src/sidecar/lock.rs` (`FolderLock::acquire`)
- `docs/design/folder-lock.md`
- callers in `src/app/folder_store.rs`, `src/app/export.rs` and `src/app/bulk_delete.rs`

## Summary

This is high-quality, defensively written code. The folder-lock and writeback design is coherent:

- Every photo write is a field patch applied to the record read from disk, under a lock.
- The `LoadMark`/overlay machinery stops stale loads from reverting edits.
- Parked writes survive a dead writer.
- Untrusted-input paths are bounded: capped reads, a symlink-loop seen-set, and validated group-id stems.

I found no panics, no `unsafe`, no non-atomic writes of user data, and no lock-ordering or deadlock bugs. I traced the UI/writer handover on `shared.writer`/`held`, and the `parked` mutex is never taken by the UI thread on the normal path.

The risks are mostly at the edges:

- **Presets:** an unreadable presets file is treated as an empty library, and the first edit then overwrites it. This is the most important finding.
- **Folder load:** a failed `read_dir` is reported as "no sidecars".
- **Staleness:** the `A->B->A` double-load can apply an older load over a newer one.
- **Durability:** there is no fsync before the rename (in `paths.rs`).
- **Signal cache:** `forget` is not scoped to the open folder.
- **Preset scan:** the folder scan is bounded by depth and preset-file count but not by directory count.

Findings: 0 Critical, 0 High, 1 Medium, 9 Low, 3 Info.

## Findings

### [PERSIST-B-01] Unreadable presets file is treated as an empty library, then overwritten  -  Severity: Medium  -  Confidence: High
- Location: `src/persist/presets.rs:86-88` (`PresetStore::load`); `src/persist/prefs.rs:94-96` (`load_at` is `read_to_string(path).ok()`)
- Evidence: `PresetStore::from_text(&prefs::load(KEY).unwrap_or_default(), true)`. `prefs::load` returns `None` for every error. That includes `PermissionDenied`, `EIO`, and invalid UTF-8 (a hand-edit saved as Latin-1 or UTF-16, or a BOM'd editor save). `from_text("")` returns a store with `corrupt = false` and `persistent = true`.
- Impact: the module has an explicit "corrupt document, refuse every write" protection (`corrupt`, `writable()`). It only fires when the file reads as UTF-8 but fails to parse. If the file exists but cannot be read as a string, the user sees an empty library. Their first "Save preset" or import then calls `flush()` and atomically replaces their real library with a one-preset document. The `presets.rs` header says a preset library is user-authored data with no second copy anywhere.
- Suggested fix: read the file in a way that separates `NotFound` (empty, writable) from any other error (corrupt-style: empty list, writes refused, error toast naming the file). Add `prefs::load_checked() -> Result<Option<String>, io::Error>` and use it here. Add a test with an invalid-UTF-8 presets file asserting that `add` is refused and the file is untouched.

### [PERSIST-B-02] A failed `read_dir` of `.lightphotos` is "no sidecars", so the visit opens editable with an empty cache  -  Severity: Low  -  Confidence: Medium
- Location: `src/persist/folder_store.rs:1040-1049` (`load_sidecars`); `src/persist/folder_store/group_file.rs:67-70`
- Evidence: `Err(_) => return SidecarLoad { images, groups: Vec::new(), skipped }` with `skipped == 0`. The group loader likewise returns an empty list on any `read_dir` error.
- Impact: a missing `.lightphotos` is the normal first-visit case. `EACCES`, `EIO`, or a flaky network share are not. They show every photo as unrated, with no toast, because `skipped` stays 0. `apply_loaded` then makes the folder `editable()`. New edits are field patches onto the on-disk record, so they mostly survive, but the user is working blind. Group membership is also unknown, so a new group can claim photos already in an unloaded group file.
- Suggested fix: treat `ErrorKind::NotFound` as empty. For any other error, count it as skipped or report it, and consider making the visit read-only, as for an unlockable folder.

### [PERSIST-B-03] `apply_loaded` has no notion of "latest load", so with `A->B->A` an older load can overwrite a newer one  -  Severity: Low  -  Confidence: Medium
- Location: `src/persist/folder_store.rs:504-533`; caller `src/app/folder_store.rs:288-297`
- Evidence: `apply_loaded` accepts any load whose `dir` equals the active dir. The app's own comment says "after A, B, A navigation two loads for A can be in flight". The token is used only to clear `pending`, not to discard the stale payload. Both loads run on separate threads, so the older one can arrive second. It then `images.insert`s its older snapshot over every non-`dirty` name and replaces `groups`.
- Impact: our own writes are protected by the overlay (the older mark replays more). A sidecar changed by another program between the two snapshots is not. The older state wins for the visit, because `dirty` is cleared on `switch_dir` and no further load runs. An older load that lands before the newer one also flips `groups` to `Some`, so `editable()` is true before the latest snapshot exists. This is a narrow race.
- Suggested fix: give `apply_loaded` a "current load" id, for example the `LoadMark` or a token kept in `FolderStore`. Make it ignore a payload whose mark is older than the active visit's mark, or have the app pass `token == latest`.

### [PERSIST-B-04] `write_atomic` renames without fsync, so a power loss can leave a zero-length sidecar, group file or preset library  -  Severity: Low  -  Confidence: Medium
- Location: `src/paths.rs:283-287` (callers `folder_store/writeback.rs:1048-1066`, `presets.rs` via `prefs::save`, `signalcache.rs:466`)
- Evidence: `file.write_all(bytes)`, then `drop(file)`, then `rename`. There is no `sync_all()` on the temp file or on the directory.
- Impact: this protects against a process crash. After a kernel panic or power cut, APFS and ext4 delayed allocation can persist the rename before the data. The result is an empty or short file. For a sidecar this surfaces as "unreadable sidecar" and the next edit moves it aside, so the edit history is lost. For `presets` it makes the library "corrupt" (locked), unrecoverable from within the app.
- Suggested fix: `file.sync_all()` before the rename, at least for sidecars, group files and the presets document. These are small user-authored files. Keep it off for `signals.json`, which is a cache. Per-write fsync costs about 1 ms or more on SSDs, so measure it against the 20 000-photo drain benchmark. A `sync_data` on the sidecar and one `fsync` of the directory per batch is an alternative.

### [PERSIST-B-05] `SignalCache::forget` ignores the folder, so it can drop another folder's same-named entry  -  Severity: Low  -  Confidence: High
- Location: `src/persist/signalcache.rs:293-301`; caller `src/app/bulk_delete.rs:395`
- Evidence: `forget` uses only `path.file_name()`. `record` carefully checks `self.dir == path.parent()` and explains why: camera names repeat across folders. The wasm branch of `bulk_delete` carries an `origin_dir`, showing deletions can finish after the user switched folder.
- Impact: finishing a bulk delete of `/a/IMG_0001.JPG` after switching to `/b` evicts `/b/IMG_0001.JPG`'s capture time and face quality. Only a recompute is lost, but it is the same cross-folder confusion the module guards against everywhere else.
- Suggested fix: `if self.dir.as_deref() != path.parent() { return; }`, as in `record`. Add a test mirroring `a_signal_for_a_photo_in_another_folder_is_ignored`.

### [PERSIST-B-06] Preset-folder scan is bounded by depth and preset count only, not by directories visited or time  -  Severity: Low  -  Confidence: Medium
- Location: `src/persist/preset_folders.rs:31-46` (doc), `157-207` (`scan`)
- Evidence: the `Limits` doc says it keeps "a folder pointed at the wrong place (a home directory, a photo archive) [to] a bounded walk". Only `files` (counted after the `.xmp` filter) and `depth: 8` are limited. Every directory is `canonicalize`d, `read_dir`ed and sorted. Every entry gets a `metadata` stat, whether or not it is an `.xmp`.
- Impact: pointing a preset folder at `~` or an archive means millions of `stat` and `canonicalize` calls across a depth-8 BFS. `node_modules`, caches and `.photoslibrary` internals (only dot-folders and `*.lightphotos` are skipped) make it worse. It runs on a background thread and is cancellable, so there is no UI freeze. But the scan can run for minutes, and `truncated` is never set, because only the preset-file cap sets it.
- Suggested fix: add `Limits::dirs` and `Limits::entries` (or a wall-clock budget), and set `truncated` when they trip. Also return the cancelled-versus-complete distinction: a cancelled walk returns `Ok(partial)`, indistinguishable from a complete scan unless the caller always discards it.

### [PERSIST-B-07] Only the last persist error survives; earlier ones in the same frame are overwritten  -  Severity: Low  -  Confidence: High
- Location: `src/persist/folder_store.rs:133-137` (`pump`), `511-513` (`apply_loaded`), `580-583` (`note_persist_error`)
- Evidence: `last_error: Option<String>` is assigned, not appended. `pump` calls `note_persist_error` once per failed write. `apply_loaded` assigns the "N sidecars could not be read" message without checking for a pending error.
- Impact: distinct failures in one frame (a lock refusal plus an unreadable sidecar, or a skipped-sidecars message plus a write failure) show only one toast. The others reach only stderr. Notices (`last_notice`) are merged properly, so the error path is inconsistent with it.
- Suggested fix: join like `take_write_notices`, or keep a small `Vec` with a count.

### [PERSIST-B-08] The preset library is a single global document with last-writer-wins across app instances and one bad entry locks everything  -  Severity: Low  -  Confidence: Medium
- Location: `src/persist/presets.rs:93-117`, `268-281`, `293-297`
- Evidence: the whole `Document` is rewritten from the in-memory list on every mutation, with no lock and no re-read. `parse` fails the whole document if any `Preset` fails (for example one wrong-typed `adjustments` field).
- Impact: two windows or instances each load the library at launch. Whatever each saves last replaces the other's additions. A single hand-edited bad preset marks the entire library `corrupt` and read-only. The file stays intact, which is good, but the only fix is outside the app.
- Suggested fix: re-read and merge by `id` just before a write, or take a short lock file. Optionally parse presets leniently (`Vec<serde_json::Value>`, keeping unparseable entries verbatim) and write them back untouched.

### [PERSIST-B-09] `PresetStore::to_text` swallows serialization failure into an empty document  -  Severity: Low  -  Confidence: Medium
- Location: `src/persist/presets.rs:284-290`; `flush` at `278`
- Evidence: `serde_json::to_string_pretty(..).unwrap_or_default()`, then `prefs::save(KEY, &self.to_text())`. If serialization returned `Err`, the library file would be replaced with an empty string. The comment says it is infallible "because `Preset` is plain data".
- Impact: effectively unreachable today. But `serde_json` writes a non-finite `f32` as `null`, and `Adjustments` then fails to deserialize `null`. A NaN in any preset's `Adjustments` would therefore produce a document that serializes fine but fails to load, which locks the library on the next launch (see PERSIST-B-08). The LR importer filters non-finite values (`lr_preset.rs:364`), but the save-from-current-edit path (`app/presets.rs:155`) does not obviously do so. This was not verified against the sliders.
- Suggested fix: make `flush` skip the write and report an error when serialization fails. Reject non-finite adjustments in `add`/`insert`.

### [PERSIST-B-10] Signal-cache validity key is mtime (ms) plus length only  -  Severity: Low  -  Confidence: Medium
- Location: `src/persist/signalcache.rs:114-132`
- Evidence: `signal_key(mtime_ms, len)`. A pre-1970 mtime collapses to 0 through `unwrap_or(0)`. `as_millis() as u64` truncates.
- Impact: a same-length in-place metadata rewrite that preserves mtime (`exiftool -P` changing orientation, or `touch -r`) keeps serving the stale capture time or face verdict. The orientation case matters here, because the v2 key bump was needed for exactly this class of bug. Pre-1970 mtimes all share a key per length.
- Suggested fix: acceptable as a documented heuristic. Optionally add ctime on Unix. Add a note in the doc comment.

### [PERSIST-B-11] Signal-cache writer can leave `outstanding` stuck, stalling every `Drop` for the full timeout  -  Severity: Low  -  Confidence: Low
- Location: `src/persist/signalcache.rs:404-413` (`submit`), `388-397`, `415-421`
- Evidence: `submit` increments `outstanding`, then `let _ = tx.send(..)`. If the writer thread has ended (a panic, or a poisoned counter lock), the send fails or the decrement is skipped. The count then never returns to 0.
- Impact: `wait_idle` then waits the whole timeout. `Drop` uses 2 s and runs on the folder switch, on the load thread, or at quit. This needs the writer thread to die, which nothing in `write_snapshot` should cause.
- Suggested fix: on `send` failure, undo the increment. Optionally make `wait_idle` return early when the thread has exited.

### [PERSIST-B-12] An `app-save` holder in another process makes the visit read-only without waiting  -  Severity: Low  -  Confidence: Medium
- Location: `src/persist/folder_store/locks.rs:217-225`
- Evidence: only no-holder, own-pid and `AppQuitting` are `Err` (retried). `Some(_) => Ok(Visit::ReadOnly(held_by(holder)))` covers `Writer::AppSave`, the brief lock another instance's writer thread holds while draining writes for a folder it has left.
- Impact: this is documented (`folder-lock.md` section 6, "A holder another process names is not waited for") and consistent with `mcp`. But `AppSave` is, by construction, held only for milliseconds. Opening folder X in window B while window A is still draining X's last writes gives a read-only visit for the whole visit, labelled "held by another window". The user must reopen the folder. The UI thread cannot sleep, but it could retry for `BRIEF_WAIT`, as it does for unnamed holders.
- Suggested fix: add `Some(Holder { writer: Writer::AppSave, .. }) => Err(holder)` to the retry arm, or update the design table to say why not.

### [PERSIST-B-13] Secret storage: silent save failure and delete ignores both errors and the server  -  Severity: Low  -  Confidence: Medium
- Location: `src/persist/secret.rs:68-73`; caller `src/app/export.rs:475-477`
- Evidence: `delete_api_key(_server)` removes the single key file whatever server it was saved for, and discards the error. `save_api_key` failures are only `eprintln!`ed at the call site.
- Impact: there is one key at a time. If a save fails (full disk, permissions) the user is told they are connected and the key is "remembered", but it is not, and they discover this at the next launch. If the delete fails, "Forget" appears to work but the 0600 file stays. The server mismatch cannot hurt today, because the file always matches the connected server. It is a latent trap if multi-server support arrives.
- Suggested fix: surface `save_api_key`'s `Err` as a toast. Make `delete_api_key` return `Result` and only delete when `key_for(body, server)` matches. A nicer long-term fix is the Secret Service or Windows Credential Manager.

### [PERSIST-B-14] Parked writes are dropped silently when a `Writeback` is dropped  -  Severity: Info  -  Confidence: High
- Location: `src/persist/folder_store/writeback.rs:804-817`, `write_loop` exit at `864-867`
- Evidence: `Drop` closes the channel and joins. The writer does one last `retry_parked`, then returns. Anything still parked behind another program's lock disappears without a notice. `flush_blocking` (called at quit) reports it, but any other `FolderStore` drop path does not.
- Impact: edits were already in memory and the user was told earlier via the "still trying" notice. A `FolderStore` replaced mid-session would still lose them quietly. Today the app has one store for its lifetime, so this is only a hazard if that changes.
- Suggested fix: on drop, push a notice or `eprintln!` the count.

### [PERSIST-B-15] `replace_unreadable` aside names are capped at 1000 and never cleaned  -  Severity: Info  -  Confidence: High
- Location: `src/persist/folder_store/writeback.rs:1119-1126`
- Evidence: `(1..=1000u32)...find(|aside| !aside.exists())`, else "no free name".
- Impact: a photo whose sidecar is repeatedly corrupted (a sync tool or flaky disk) accumulates `.unreadable-N` files. After 1000, every save fails permanently. The bound is sensible, and it is an extreme case.
- Suggested fix: none needed. Optionally prune the oldest.

### [PERSIST-B-16] Unit-level test hygiene  -  Severity: Info  -  Confidence: High
- Location: `src/persist/folder_store.rs:1790-1805`, `1591-1631`, `2146-2183`; `src/persist/signalcache.rs:660-692`
- Evidence and impact:
  - `lightphotos_dir_created_lazily_only_on_first_write` asserts that "opening/reading a directory must not create .lightphotos". It opens a folder with no photos, which takes no lock. For a folder with photos, `FolderLock::acquire` creates `.lightphotos/.lock` at open (the neighbouring test `a_load_without_groups_creates_no_directory` filters it out). The test name and message claim a stronger guarantee than the code gives and than the test checks.
  - `an_unreadable_sidecar_fails_its_own_write_only` relies on a mode-000 file being unreadable. It fails when the tests run as root (some CI containers).
  - `rating_a_whole_folder_does_not_block_on_the_filesystem` (<1 s for 20 000 sets) and `the_periodic_save_leaves_the_listing...` (<50 ms with 300 000 entries) are wall-clock assertions that can flake on a busy debug-build runner.
- Suggested fix: rename or extend the first test to cover a folder with photos. Skip the mode-000 test when `geteuid() == 0`. Consider `#[ignore]`-gating or loosening the timing tests.

## Test coverage assessment

Strong:

- Writeback has many regression tests keyed to past review rounds: overlay ordering, mark semantics, parked writes, a dead writer, a panicking write, history trimming, flush reporting, and key counting.
- `folder_store.rs` has end-to-end tests for round trips, unreadable sidecars (moved aside, left alone, per-photo isolation), stale loads, group replay order, and off-folder writes.
- `signalcache.rs` covers invalidation, corrupt and oversized files, and cross-folder records.
- `group_file.rs` and `preset_folders.rs` have sound unit tests for the safe-stem rule, the size cap, the depth limit, the file limit and the symlink loop.

Gaps:

- `locks.rs`: nothing for the `Opening` to `ReadOnly` timeout (`BRIEF_WAIT` and `QUITTING_WAIT` expiry), `Unsupported` to `Lockless`, `Refused` and `Unavailable` mapping, `settle`/`release_locks`/`release_writer`, `mark_quitting`, or `forget_undecided`/`quiet_added` bookkeeping.
- `FolderStore::pump` and `visit_changed` are untested, as are `editable()` and `opening()` for `Lockless` and `Unlocked`.
- Double-load staleness (PERSIST-B-03) and a failing `read_dir` on `.lightphotos` (PERSIST-B-02) are untested.
- `group_file::parse` has no direct malformed tests (wrong types, duplicate members, empty members). Only integration-level bad-JSON and wrong-version cases exist.
- `presets.rs`: the persistent path (`load` then `add` then `load` through `prefs`) is never exercised, so PERSIST-B-01 has no test. There is no test for NaN or non-finite adjustments or for duplicate ids in a stored document.
- `secret.rs`: only `key_for` is tested. There is no test that the file is created 0600, that a save/load round trip works, or that `delete` is scoped.
- `preset_folders.rs`: no test for an unreadable subdirectory, a symlinked `.xmp` file, or a very wide tree.
- `signalcache.rs`: no test for `forget` across folders, the `absorb` stale-key branch, or same-mtime-and-length edits.

## Things done well

- The "write is a field patch applied to the on-disk record under the lock" design removes a whole class of lost-update bugs. The `fallback` (cache copy) is used only when the disk record is unreadable, and the bad file is moved aside atomically (`write_atomic_then`), never deleted.
- Lock handover is race-free. The shared mutex is never held across I/O. Lock files are dropped outside the mutex. `try_lock` is used where the UI thread must keep its deadline (`flush_blocking`). Per-path ordering is preserved across parking (`behind_parked`).
- `retire` cannot underflow, because `pending` entries are removed at zero.
- Writer panics are contained (`guarded`), and a dead writer's queue is drained inline (`reap_dead_writer`).
- `GroupId::from_stem` validates group filenames, so there is no traversal through `groups/<id>.json`. Group files are size-capped. Groups are never written during load, and repairs are deferred to the first real mutation.
- `signalcache` keys by name, but `record` rejects foreign-folder paths. Its writer snapshots an `Arc`, so no big clone happens on the frame. `read_capped` bounds the file, which is good against a symlink to a huge file.
- `presets.rs` has a `corrupt` latch, never reuses ids, handles `u64::MAX`, resolves collisions case-insensitively, and writes a batch import in one flush.
- `secret.rs` uses the macOS Keychain, and the fallback file is created 0600 from the start through the temp-file mode. There is no key in logs or error strings, and the key is bound to the server it was saved for.
- `preset_folders::scan` uses a canonical-path seen-set against symlink loops, a per-file byte cap, a cancel flag, and skips dot-folders.

## Open questions for maintainers

1. Is fsync-before-rename deliberately omitted (the 20 000-sidecar drain budget), or just never considered? If deliberate, should the presets document and group files be exempt?
2. Should a sidecar written by a newer LightPhotos (valid JSON but unknown version or shape) be classed "unreadable" and moved aside by an older build, as the code does today for any `ReadError::Unreadable`? That would lose the newer app's data from the live sidecar (it is kept aside). Confirm `read_record_checked` tolerates unknown keys and versions.
3. Is a visit with a failed `read_dir` of `.lightphotos` (PERSIST-B-02) meant to be writable?
4. Is a single-slot `last_error` intentional? A merged line, like `last_notice`, would match the notices.
5. Should `app-save` from another process be a retry case (PERSIST-B-12)?
6. Should presets use the folder-lock machinery or a simple lock, now that multiple windows are supported?
