# src/jobs plus worker_pool, paths, hash, base64

## Scope

Every line of these files was read.

| File | Lines |
|---|---|
| `src/jobs/mod.rs` | 9 |
| `src/jobs/cache_limits.rs` | 164 |
| `src/jobs/tier.rs` | 173 |
| `src/jobs/score.rs` | 401 |
| `src/jobs/thumbnail.rs` | 918 |
| `src/jobs/loader.rs` | 3220 |
| `src/worker_pool.rs` | 177 |
| `src/paths.rs` | 617 |
| `src/hash.rs` | 74 |
| `src/base64.rs` | 43 |

I cross-checked callers in `src/app/score.rs`, `src/app/thumbs.rs`, `src/app/nav.rs`, `src/app/folder_store.rs`, `src/app/export.rs` and `src/decode/image_encode.rs`.

## Summary

The code is generally careful. It has no non-test panics: slices are guarded and `checked_add` is used where input-derived offsets appear. Poisoned locks are handled. Every unsafe block has a `// SAFETY:` comment. Cancellation of Loupe decodes uses a generation counter that is checked on landing. Eviction is byte-accounted, and the tests for it are good.

The main risks:
- `write_atomic` never calls `fsync`, so a power loss or kernel panic can leave a zero-length sidecar, which contradicts the crash-safety claim.
- The long-lived `Loader` never invalidates a thumbnail or forgets a thumbnail failure for the session.
- The macOS thumbnail cache writes through a fixed `.tmp` name, and the loader documents that it can decode the same photo twice at once.
- A few stale-result and bookkeeping gaps in scoring and the worker lifecycle.

Findings by severity: Critical 0, High 0, Medium 3, Low 8, Info 5.

## Findings

### [JOBS-01] `write_atomic` never fsyncs, so a power loss can leave an empty or partial file under the final name — Severity: Medium — Confidence: High
- Location: `src/paths.rs:283-287` (`write_atomic_then`); the same pattern is in the macOS path at `src/jobs/thumbnail.rs:511-529`.
- Evidence: the sequence is `file.write_all(bytes)`, `drop(file)`, `before()`, `fs::rename(&tmp, dest)`. There is no `file.sync_all()` before the rename and no sync of the parent directory after it.
- Impact: a process crash is safe, because the page cache survives it. An OS crash, power loss or sudden unplug (an external-drive photo folder is the common case here) can persist the rename before the data blocks. On ext4 and XFS with delayed allocation, on exFAT/FAT, and on APFS without `F_FULLFSYNC`, `dest` can come back zero-length or truncated. This is the path used for sidecars (edit data), presets and groups. It conflicts with CLAUDE.md's "a crash, a full disk … never leaves a partial file".
- Suggested fix: call `file.sync_all()` (macOS: `fcntl(F_FULLFSYNC)`) before `rename`, and `File::open(parent).sync_all()` after it on Unix (best effort). Make it a parameter, or a separate `write_durable`, if the cost on bulk thumbnail writes matters. User-authored data (sidecars, presets) should always sync.

### [JOBS-02] The loader never forgets a thumbnail or a thumbnail failure: stale and permanently failed thumbnails for the whole session — Severity: Medium — Confidence: Medium
- Location: `src/jobs/loader.rs:848-859` (`thumb_cache`, `thumb_failed`), `:1276-1297` (`request_thumb`), `:2003-2017` (landing a `Thumb` result), `:1562-1580` (`forget_loupe_decodes`).
- Evidence:
  - There is exactly one `Loader`, created in `main.rs:69`.
  - `thumb_cache` is only trimmed by capacity.
  - `thumb_failed` is only ever inserted into. A grep of `loader.rs` shows no `clear` or `remove` for it, and `forget_loupe_decodes` explicitly keeps thumbnails.
  - `Job::Thumb` carries no generation.
- Impact:
  - A thumbnail that fails once is never retried until restart. Examples: a file still being copied from a card, a network volume that blipped, or a file replaced by a good copy.
  - Re-opening the folder, or replacing the file on disk, keeps showing the old in-memory image while it stays in the 256+ entry cache. The disk cache key (mtime+len) would regenerate it correctly, but the in-memory layer sits in front.
  - `thumb_failed` also grows without bound across folders.
- Suggested fix: add `Loader::forget_thumbs(paths_or_dir)` and call it on folder open or refresh. Alternatively, key `thumb_cache` and `thumb_failed` by `(path, px, mtime/len)`. At minimum, clear `thumb_failed` and `bake_failed` when a folder is (re)opened.

### [JOBS-03] macOS thumbnail cache writes to a fixed `<entry>.tmp` name, which races with the loader's documented duplicate decode — Severity: Medium — Confidence: Low
- Location: `src/jobs/thumbnail.rs:511-529`; the duplicate-decode note is at `src/jobs/loader.rs:852-856`.
- Evidence: `tmp = file + ".tmp"` is the same name for every writer of that entry. `encode_jpeg` (`decode/image_encode.rs:38-89`) lets ImageIO write straight to the URL. The loader says a thumbnail wanted by both the viewport and background lanes "can be decoded twice". `nav.rs:615` requests a background thumb for the selected photo while the filmstrip, which includes the selection, requests it through the viewport lane.
- Impact: on a first open with no disk cache, two workers can run `get_or_make` for the same photo at once. Both write the same tmp path, and the first `rename` moves the file while the second may still be writing or truncating it. The best case is a failed second `rename`, which is ignored. The worst case is a window where a truncated JPEG sits under the valid entry name, which ImageIO may decode "successfully" with a grey bottom, and the next `get_or_make` then returns and in-memory-caches it. The non-mac path uses `write_atomic` with unique pid+counter names and is safe. I did not verify how ImageIO opens an existing destination, so the exact failure mode is unconfirmed.
- Suggested fix: use a unique temp name (`.{pid}-{n}.tmp`, matching `atomic_tmp_target`), or encode to memory with `CGImageDestinationCreateWithData` and use `write_atomic`. Also dedupe in-flight thumbnail decodes per path in the loader.

### [JOBS-04] A late result from a cancelled scoring run is accepted by the new run and shadows the fresh one — Severity: Low — Confidence: Medium
- Location: `src/jobs/score.rs:241-250` (`ScoreJob::finish`), `src/app/score.rs:59-76`.
- Evidence: `finish` matches by path only. `cancel_scoring` drops the job, but the worker keeps scoring photo X. A new "Score photos" run can hand X to a free worker, because `capacity()` counts the old outstanding jobs per worker rather than per run. The old outcome for X arrives first, `finish(X)` succeeds against the new job's `in_flight`, and it is stored with the old `edits` signature. The new outcome then fails `finish` and is dropped.
- Impact: cancel, edit the photo, then re-score quickly. The photo ends up with the old score, flagged stale. The run reports "scored N" although the fresh score was discarded.
- Suggested fix: stamp each `ScoreRequest` and `ScoreOutcome` with a run id (or match on `(path, edits)`) and refuse mismatches in `finish`.

### [JOBS-05] `ScorePool::submit` swallows a dead pool, leaving the job unfinishable — Severity: Low — Confidence: Medium
- Location: `src/jobs/score.rs:139-143`, caller `src/app/score.rs:95-105`.
- Evidence: if `WorkerPool::submit` returns false (workers gone), the request is dropped and `outstanding` is not bumped. `ScoreJob::take` already moved the path into `in_flight`, so `is_finished()` is never true.
- Impact: unlikely today, since workers only exit on a poisoned lock or when the result channel closes. Once it happens, scoring shows "in progress" forever and `quality_sort_available` stays false, because `score_job` stays `Some`.
- Suggested fix: return a bool from `submit`, and make the caller `finish(path, false)` or `defer` on failure.

### [JOBS-06] Native decode workers die silently if anything panics outside the `catch_unwind` — Severity: Low — Confidence: Medium
- Location: `src/jobs/loader.rs:581-596` (`speed_is_whole_photo` runs outside the `catch_unwind` in `run_job`), `:528-572` (`Worker::run`).
- Evidence: on native there is no worker replacement (`panic_recovery` is wasm-only). `speed_is_whole_photo`, which calls `image_decode::pixel_size` (ImageIO FFI on macOS), and `report_decode` run unguarded.
- Impact: a panic there kills the thread, sends no result, and leaves the path in `speed_inflight` forever. `has_pending_image()` then keeps the frame loop spinning, and the pool is one worker smaller.
- Suggested fix: move the `whole` computation inside the `catch_unwind` closure, or wrap the whole `run_job` call in `Worker::run`. Do not let a thread die without a result.

### [JOBS-07] The wasm export-name picker is case-sensitive, unlike the native one — Severity: Low — Confidence: Medium
- Location: `src/paths.rs:25-44` (`export_name`), caller `src/app/export.rs:606-609`.
- Evidence: `export_target` lowercases keys because macOS and Windows treat `IMG_1.jpg` and `img_1.jpg` as one file, and a test covers it (`export_target_treats_names_that_differ_only_in_case_as_taken`). `export_name` compares exact strings, and `existing` and `taken` are raw names.
- Impact: in a browser on macOS or Windows, two sources whose templated stems differ only in case export to the same file, and the second overwrites the first. Duplicate logic that has already diverged.
- Suggested fix: share one implementation, or lowercase in `export_name`'s `free` check and in `taken`/`existing`.

### [JOBS-08] No retry on Windows when the `rename` over `dest` hits a sharing violation — Severity: Low — Confidence: Medium
- Location: `src/paths.rs:286`.
- Evidence: `std::fs::rename` uses `MoveFileExW(REPLACE_EXISTING)`. It fails with `ERROR_ACCESS_DENIED` or `ERROR_SHARING_VIOLATION` if another process (antivirus, OneDrive, Explorer preview, indexer) holds `dest` open without `FILE_SHARE_DELETE`.
- Impact: sidecar saves fail intermittently on Windows with no retry. The temp file is cleaned up correctly.
- Suggested fix: retry a few times with a short backoff on Windows for `PermissionDenied`, before surfacing the error.

### [JOBS-09] Several loader sets grow for the life of the process, and a stale `escalation_wanted` defeats its own contract — Severity: Low — Confidence: Medium
- Location: `src/jobs/loader.rs:846` and `:1043` (`escalation_wanted`), `:878` and `:1784` (`bake_failed`), `:859` (`thumb_failed`).
- Evidence: `escalation_wanted` is inserted on every `request_preview` and removed only in `retain_previews`, and only for jobs still queued. `forget_loupe_decodes` does not clear it. `bake_failed` is keyed with the edit signature and never trimmed.
- Impact: small unbounded memory growth. Behaviourally, a key once viewed stays "wanted", so a later `prefetch_preview` of that photo (after eviction or a settings change) escalates to the expensive `Preview` decode, contrary to the documented "keys from `prefetch_preview` alone are not in this set".
- Suggested fix: clear `escalation_wanted` in `forget_loupe_decodes` and remove the key when its `Preview` lands. Prune `bake_failed` per path on folder change.

### [JOBS-10] Per-frame heap allocation in the loader's lookup API — Severity: Low — Confidence: High
- Location: `src/jobs/loader.rs:1197-1199` (`get_preview`), `:1204-1211`, `:1635-1638` (`touch`), `:1815-1817` (`get_thumb`), `:1821-1823` (`thumb_failed`), `:1250-1259`.
- Evidence: each lookup builds `(path.to_path_buf(), px)` to query a `HashMap`/`Tier<(PathBuf,u32)>`. The grid calls `get_thumb` and `thumb_failed` for every visible cell every frame.
- Impact: hundreds of small allocations and memcpys per frame. Not a bug, but avoidable.
- Suggested fix: key by a `(Arc<Path>, u32)` or use `hashbrown`'s raw-entry / `Equivalent` lookup. Alternatively, nest `HashMap<PathBuf, SmallMap<u32, _>>` so `&Path` can be the lookup key.

### [JOBS-11] `Job::Thumb` carries `max_px` but the worker ignores it — Severity: Low — Confidence: High
- Location: `src/jobs/loader.rs:615-620`, `src/jobs/thumbnail.rs:409-442`.
- Evidence: `get_or_make(&path)` always produces `THUMB_PX` (512), while the result is filed under the requested `max_px`. All current callers pass `THUMB_PX` (plus 512 in `thumbs.rs:839`). Tests use 192.
- Impact: latent. A caller asking for another size would get a 512 px image cached under that size's key, with no error.
- Suggested fix: drop the parameter, or assert or reject `max_px != THUMB_PX`.

### [JOBS-12] The embedded-preview acceptance test checks size only, not aspect ratio — Severity: Low — Confidence: Low
- Location: `src/jobs/thumbnail.rs:290-292` (`preview_is_large_enough`), used in `decode_speed` (`:118-133` and `:143-161`).
- Evidence: the doc says a Sony 4:3 letterboxed EXIF thumbnail "showed the photo at the wrong aspect ratio". The fix is a size floor of `max_px/2`. A letterboxed or cropped thumbnail of 256 px or more (for example 320x240 or 640x480) on a 3:2 photo still passes.
- Impact: the cache entry (valid for the life of the photo) keeps the wrong-aspect thumbnail for the photo, and the Loupe's first tier is also wrong-aspect until the sharper tier lands.
- Suggested fix: when the embedded size passes, also compare its aspect against `pixel_size(path)` (header read, cheap) and fall through to the decode if they differ by more than about 2%.

### [JOBS-13] The thumbnail cache is documented as LRU but evicts in insertion order — Severity: Info — Confidence: High
- Location: `src/jobs/loader.rs:5` (module doc) and `:2109-2125`.
- Evidence: `thumb_order` is a FIFO. `get_thumb` does not touch it and re-inserting an existing key does not refresh it. The Loupe tiers do use the real LRU `Tier`.
- Impact: a thumbnail on screen since early in a long scroll can be evicted before colder ones. Harmless because it is re-requested from the disk cache, but the doc is wrong, and the working-set sizing hides it.
- Suggested fix: fix the doc, or reuse `Tier` for thumbnails.

### [JOBS-14] `ThumbCache::new` deletes macOS-only legacy paths on every platform, and on every construction, including tests — Severity: Info — Confidence: High
- Location: `src/jobs/thumbnail.rs:379-384` and `:587-600`.
- Evidence: it spawns a thread that does `remove_dir_all($HOME/Library/Caches/com.lightphotos/thumbnails)` (and `com.imageviewer`) wherever `HOME` is set. Every `Loader::new` in tests does this to the developer's real home.
- Impact: harmless on Linux. Wasteful, and it surprises a developer running the tests.
- Suggested fix: gate with `#[cfg(target_os = "macos")]`, and drop the migration once old versions have aged out.

### [JOBS-15] Long photo filenames defeat the thumbnail cache and the `write_atomic` temp names — Severity: Info — Confidence: Medium
- Location: `src/jobs/thumbnail.rs:320-324` (`cache_name` adds 27 bytes) and `src/paths.rs:263-266` (adds `.{pid}-{n}.tmp`).
- Evidence: the entry name is `<photo>.<16 hex>.thumb.jpg`, plus `.<pid>-<n>.tmp` for the temp file.
- Impact: filenames of roughly 200 bytes or more exceed `NAME_MAX` (255), so the write fails with `ENAMETOOLONG`. The failure is swallowed (`let _ = write_entry`), and the photo is re-decoded every session. Orphan `*.tmp` files from a crash are only swept for thumbnails, so they linger in `Exports/` and next to photos.
- Suggested fix: hash long names, and optionally sweep `write_atomic` leftovers in the other callers' directories.

### [JOBS-16] Other minor robustness notes — Severity: Info — Confidence: Medium
- `max_px as i32` (`src/jobs/thumbnail.rs:232`) would wrap for values above `i32::MAX`. Current callers are at most 16384, so use `i32::try_from(..).unwrap_or(i32::MAX)`.
- `ScorePool::new` returns `None` off macOS, so the Linux (`nice`) and Windows (`SetThreadPriority`) branches of `lower_priority` are only reachable from tests.
- `normalize` (`src/paths.rs:10`) returns `\\?\`-prefixed verbatim paths on Windows. Mixing it with non-normalized keys would never match.
- `read_capped` (`src/paths.rs:126-154`) checks `metadata` and then opens. A file swapped for a FIFO between the two calls would block `open`. This is a narrow TOCTOU.

## Test coverage assessment

- Good:
  - `Tier` (byte accounting, LRU, kept keys).
  - Loader queue priority and the dedicated worker.
  - Generation-based drop of stale Loupe decodes.
  - `leave_loupe` marker semantics.
  - Poisoned-queue handling.
  - `write_atomic` against planted symlinks and its mode behaviour.
  - Sidecar-dir symlink refusal.
  - Cache-name parse and sweep.
  - Panicking jobs in `WorkerPool` and `ScorePool`.
- Weak or missing:
  - `hash.rs` has no external FNV-1a test vector beyond the empty-input basis. The "stable and deterministic" test compares two identical computations and could never fail. Add `"a" → 0xaf63dc4c8601ec8c`.
  - `write_atomic` has no test for `before()` failing and cleaning up the temp file, and none for the full-disk or error paths.
  - No test of concurrent `ThumbCache::get_or_make` on one photo (JOBS-03).
  - No test of `WorkerPool` shutdown on drop, zero workers, or spawn failure. `workers == 0 → submit false` is only implied.
  - `ScoreJob` has no cancel-then-resubmit-same-path test (JOBS-04), and none for a failed `ScorePool::submit`.
  - `Loader::drain` has no test for a `Thumb` result, thumbnail-failure retry, or thumbnail invalidation, since none exists (JOBS-02).
  - `thumbnail::write_entry` has no test of a truncated cache entry being regenerated.
  - `base64` is encode-only, with RFC vectors. That is adequate, but it does not cover inputs longer than a few bytes.

## Things done well / verified-safe areas

- No `unwrap`, `expect` or unguarded indexing in non-test code in these files. The EXIF thumbnail slice uses `checked_add` plus a bounds check. `unsafe` is confined to ImageIO/CF construction and QoS calls, each with a SAFETY note.
- `write_atomic` uses `create_new`, so a planted symlink at the temp name cannot be followed. It retries name collisions and removes the temp file on write, `before` or rename failure. The mode is applied at create time, so the content is never briefly world-readable, and the rename replaces the old bits.
- `refuse_linked_sidecar_dir` and `create_sidecar_dir` stop writes through a planted `.lightphotos` link, and refuse to recreate a vanished photo folder.
- `read_capped` refuses non-regular files before opening and bounds the read to `max + 1` bytes.
- The loader's lock use is disciplined. The lock is held only to pop or push a job. Poisoning is surfaced as failed markers instead of a hang, and the reserved decode worker plus the priority lanes are tested.
- `Tier` and the loader keep the byte total in step on insert, replace, remove and evict, with saturating math.
- `export_target` is linear for large same-stem batches. `sweep_orphans` is conservative: it only touches names that parse as cache entries, spares fresh temp files, and refuses a linked `.lightphotos`.

## Open questions for maintainers

1. Is there an intended invalidation path for the in-memory thumbnail cache when a file changes on disk or a folder is re-opened (JOBS-02)? I found none in `Loader` or `app`.
2. Is `fsync` deliberately skipped in `write_atomic` for performance (JOBS-01)? If so, should sidecars and presets get a durable variant?
3. On macOS, does ImageIO's `CGImageDestination` replace the destination file or truncate it in place when it already exists? That decides how bad the JOBS-03 race is.
4. Should a thumbnail request for a key already in flight on the other lane be deduplicated, rather than relying on the disk cache to make the second decode cheap?
