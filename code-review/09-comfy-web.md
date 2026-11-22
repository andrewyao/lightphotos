# src/comfy and src/web (ComfyUI client and wasm browser glue)

## Scope
Every line of these files was read.

| File | Lines |
|---|---|
| src/comfy/mod.rs | 14 |
| src/comfy/client.rs | 869 |
| src/comfy/download.rs | 443 |
| src/comfy/features.rs | 127 |
| src/comfy/workflow.rs | 317 |
| src/comfy/test_server.rs | 112 |
| src/web/mod.rs | 27 |
| src/web/analytics.rs | 201 |
| src/web/heap_stats.rs | 30 |
| src/web/web_canvas.rs | 55 |
| src/web/web_catalog_idb.rs | 198 |
| src/web/web_decode.rs | 287 |
| src/web/web_export_fs.rs | 144 |
| src/web/web_exports.rs | 148 |
| src/web/web_folder_store_fs.rs | 452 |
| src/web/web_fs.rs | 536 |
| src/web/web_mcp.rs | 499 |
| src/web/web_thumb_cache.rs | 258 |

I also read the callers in `src/app/comfy.rs`, `src/app/generative.rs`, `src/app/web.rs`, `src/app/export.rs`, `src/persist/folder_store/writeback.rs` and `src/jobs/loader.rs` to check contracts.

## Summary
The code is careful and follows the project rules. I found no non-test `unwrap`, `expect`, `panic!`, `unsafe` or unchecked indexing in either module.
- **comfy**: Reply sizes are capped. The downloader checks size and SHA-256, resumes with Range requests, and guards against path traversal. Malformed workflow, history and queue JSON are tested not to panic.
- **web**: Atomic writes go through writable streams. Sidecar and listing code is defensive.

The main risks are robustness gaps, not crashes:
- **Job loss**: a single failed poll kills a long ComfyUI job and leaves the prompt running on the GPU.
- **Streams**: the browser write streams are never aborted on failure, and the same code is copy-pasted three times.
- **Silent errors**: several browser paths swallow errors, so partial reads or writes look like success.
- **Unbounded reads**: some file reads into wasm memory have no size cap, and an allocation failure there aborts the tab.

No Critical or High findings. Counts: Medium 1, Low 9, Info 6.

## Findings

### [COMFY-WEB-01] One failed poll kills a long generation and leaves the prompt running — Severity: Medium — Confidence: Medium
- Location: `src/comfy/client.rs:221-246` (`wait`); caller `src/app/generative.rs:196-216`
- Evidence: every `/history` poll does `.call().map_err(|e| lost(..))?` and `checked(..)?`. Any transport error or timeout (`timeout_recv_response` is 60 s) returns `LOST` at once. The caller cancels queued prompts only when the error is `STOPPED`. On `LOST` or "took too long" it just records the variant error and moves on to the next `wait`.
- Impact / failure scenario: a generation can run for up to 15 minutes (`JOB_TIMEOUT`). One dropped connection, a sleeping laptop, or a ComfyUI server too busy to answer one poll within 60 s fails the variant. The prompt keeps running and keeps its slot in ComfyUI's queue. The user retries and queues a second job behind it. The deadline path has the same leak: a variant that times out is never cancelled, and neither are the variants queued behind it.
- Suggested fix:
  - Tolerate N consecutive poll failures, for example 3 to 5 with backoff, before returning `LOST`.
  - On any non-`STOPPED` error that ends a variant, make a best-effort `server.cancel(id)` for that prompt and the unstarted ones.

### [COMFY-WEB-02] Browser write streams are never aborted on failure, and the write code is triplicated — Severity: Low — Confidence: Medium
- Location: `src/web/web_export_fs.rs:112-129`, `src/web/web_folder_store_fs.rs:181-198` (`write_file`), `src/web/web_thumb_cache.rs:78-94` (`store`)
- Evidence: all three do `create_writable()`, then `write(...).await.map_err(..)?`, then `close().await?`. A failed `write` or `close` returns without `writable.abort()`.
- Impact / failure scenario: on a quota or disk-full error, or when permission is revoked mid-write, the stream stays open. Chrome keeps a `.crswap` swap file in the user's photo folder or `.lightphotos` until the stream is garbage collected. Further writers to the same name may fail with `NoModificationAllowedError`. The thumbnail cache can make many such leftovers. The copies have already diverged slightly: only `web_export_fs` sizes the filename check, and only `web_folder_store_fs` creates directories.
- Suggested fix: add one `write_via_stream(dir, name, bytes)` helper in `web_fs` that calls `writable.abort()` on any error and returns the real error. Use it from all three places.

### [COMFY-WEB-03] `write_coverage` trusts any existing file of the right name — Severity: Low — Confidence: Medium
- Location: `src/web/web_folder_store_fs.rs:438`
- Evidence: `if JsFuture::from(dir.get_file_handle(&name)).await.is_ok() { return Ok(()); }`. The comment says a file already there "holds the same bytes".
- Impact / failure scenario: existence is the only check. A zero-length, truncated or foreign file with that name, for example one left by an interrupted copy outside the app, is never repaired. The mask then loads as corrupt or missing although the edit refers to it. Errors other than `NotFound` do fall through to the write, which then reports its own error, so that part is fine.
- Suggested fix: compare the existing file's size to `bytes.len()` before skipping the write, and rewrite on mismatch.

### [COMFY-WEB-04] Directory iteration errors in `files()` become a silent partial listing — Severity: Low — Confidence: High
- Location: `src/web/web_folder_store_fs.rs:69-98`; same pattern in `src/web/web_thumb_cache.rs:158-161` and `:205-208`
- Evidence: `Err(_) => break` on both `iter.next()` and the `JsFuture`. `load_sidecars` then returns a `SidecarLoad` whose `skipped` count is unchanged.
- Impact / failure scenario: a mid-iteration failure (permission revoked, folder moved) means the grid shows ratings, flags and edits as missing for some photos with no warning and no `skipped` count. By contrast `web_fs::list_dir` and `children` do propagate iteration errors.
- Suggested fix: make `files()` return `Result` and count a failure as skipped, or surface it to the caller. This is best-effort for the thumbnail cache but not for sidecars.

### [COMFY-WEB-05] `update_sidecar` reads the whole sidecar before any size check — Severity: Low — Confidence: High
- Location: `src/web/web_folder_store_fs.rs:244-249` (compare `read_record`, `:214`)
- Evidence: `read_record` rejects `file.size() > MAX_SIDECAR_BYTES` before reading. `update_sidecar` calls `stat` and then `file_bytes(&file)` unconditionally. The 1 MiB check is only in `parse_record`, after the bytes are in the wasm heap. `keep_aside` then writes a full second copy.
- Impact / failure scenario: a sidecar-named file of hundreds of MB or GB makes the next edit of that photo read it into wasm memory twice. An allocation failure aborts the tab. The doc comment says "too large" files are copied aside, but they are fully loaded first.
- Suggested fix: check `file.size()` as `read_record` does. If the file is over the limit, use the same aside path with a streamed or size-limited copy, or refuse the edit with an error.

### [COMFY-WEB-06] Unbounded reads into wasm linear memory — Severity: Low — Confidence: Medium
- Location: `src/web/web_fs.rs:487-498` (`read_bytes` / `file_bytes`), `src/web/web_thumb_cache.rs:53`, `src/web/web_exports.rs:91`
- Evidence: `Uint8Array::new(&buf).to_vec()` with no cap. `WebExports::submit` copies the whole source file out of the `ArrayBuffer` into a `Vec`, on the main thread.
- Impact / failure scenario: exporting a very large image (a large TIFF or panorama of many hundreds of MB) copies it onto the 4 GB shared heap on the UI thread. That also blocks a frame. A failed allocation in wasm aborts the app. The thumbnail `load` has the same shape on a cache file, though cache files are app-written.
- Suggested fix: check `File.size()` against a decode-budget limit before `array_buffer()`. Pass the `ArrayBuffer` to the worker as a transfer, as `read_array_buffer` already does for decode, instead of `to_vec()` on main.

### [COMFY-WEB-07] `cancel` is all-or-nothing, and its fallback can interrupt someone else's job — Severity: Low — Confidence: Medium
- Location: `src/comfy/client.rs:270-282`, used at `src/app/generative.rs:212-215`
- Evidence: `send("/queue", ..)?;` then `send("/interrupt", ..)`. The first error returns early and the interrupt is never sent. On an older ComfyUI the code's own doc says `/interrupt` "ignores the id and stops whatever is running".
- Impact / failure scenario:
  - A non-2xx from `/queue` leaves a running prompt running even though the user pressed Cancel.
  - The caller loops `cancel` over every remaining id, so on old servers each call interrupts whatever is running. That can be another application's prompt or the next variant. The caller ignores the result.
- Suggested fix: attempt both calls and combine their errors. Only call `/interrupt` once, and only if the prompt is the running one (from `/queue`'s `queue_running`), or if the server is new enough to honour `prompt_id`.

### [COMFY-WEB-08] Model URLs pin `main`, not a revision — Severity: Low — Confidence: Medium
- Location: `src/comfy/features.rs:37-44`
- Evidence: `.../stable-diffusion-xl-1.0-inpainting-0.1/resolve/main` + path, with fixed size and SHA-256.
- Impact / failure scenario: integrity is protected by the hash, but availability is not. If upstream re-uploads or reformats a file on `main`, every user's download ends with "downloaded with the wrong checksum and was deleted" and the feature is permanently broken until an app update.
- Suggested fix: use `resolve/<commit sha>` in the macro.

### [COMFY-WEB-09] `installed()` is size-only, and resume hashing shows no progress — Severity: Low — Confidence: Medium
- Location: `src/comfy/download.rs:50-55`, `:102-120`
- Evidence: `m.is_file() && m.len() == model.bytes`. `resume_point` reads and hashes the whole existing `.part` (up to 5 GB) before any `progress` call.
- Impact / failure scenario:
  - A same-size but corrupt or tampered file in `models/` is reported "installed". The doc comment accepts this for speed, but a preallocated or zero-filled file from an interrupted external download would pass.
  - On resume the UI shows 0% for as long as the hash takes (seconds on a spinning disk), with no cancel check inside `resume_point`.
- Suggested fix: add a cheap extra check, such as the file's mtime or a header sniff (safetensors header length). Report `progress(have, total)` and honour `cancel` while hashing.

### [COMFY-WEB-10] `decode_failed` can suppress a stage's first report — Severity: Low — Confidence: Medium
- Location: `src/web/analytics.rs:78-88`
- Evidence: `s.failed.insert(path) && s.reasons.insert(reason)`. If photo P2 fails at an already-reported stage "thumbnail", P2 enters `failed` and nothing is sent. P2's later failure at "full" then finds P2 in `failed` and is dropped, although "full" was never reported this folder. This contradicts the comment "then one per stage per folder".
- Impact / failure scenario: telemetry undercounts later stages. No user-visible effect.
- Suggested fix: insert into both sets separately and send if the reason is new for the folder, and the path is new or the reason is new.

### [COMFY-WEB-11] Preset folder scan hides unreadable directories — Severity: Low — Confidence: Medium
- Location: `src/web/web_fs.rs:330-332`
- Evidence: `let Ok(children) = children(&dir).await else { continue; };` does not increment `out.skipped`, though unreadable files do.
- Impact / failure scenario: a subdirectory whose listing fails silently drops all its presets with no indication to the user.
- Suggested fix: increment `out.skipped` (or add a `failed_dirs` count) on that branch.

### [COMFY-WEB-12] `JSON::stringify` result converted with `String::from` — Severity: Low — Confidence: Low
- Location: `src/web/web_mcp.rs:386-389`
- Evidence: `JSON::stringify(args)` yields `undefined` for values such as a function or symbol, or an object whose `toJSON` returns `undefined`. It is typed as `JsString` and passed straight to `String::from`, which is `unwrap_throw` in js-sys. That would trap the wasm instance.
- Impact / failure scenario: an agent cannot normally send those values, since arguments arrive as decoded JSON. This is theoretical, but it is the only panic-shaped path I found in the browser glue.
- Suggested fix: use `text.as_string().ok_or_else(..)`.

### [COMFY-WEB-13] `WebFs::write_atomic` ignores the destination's directory and overwrites silently — Severity: Info — Confidence: High
- Location: `src/web/web_export_fs.rs:88-110`; caller `src/app/web.rs:332-338`
- Evidence: only `dest.file_name()` is used, always inside `Exports/` of `self.folder`. The caller builds `dest = dest_dir.join(&filename)` and reports `ExportLanding::File(dest)`. `get_file_handle(create: true)` followed by `create_writable()` replaces an existing file.
- Impact / failure scenario: it is correct today because `dest_dir` is `folder/Exports` and `self.folder` is the same folder's handle. A caller passing a different `dest_dir` would be told the file landed somewhere it did not. The name collision check (`existing_export_names`) is a snapshot taken before the batch. Another tab, or a second batch that started before the first finished its writes, could produce the same name and silently replace the file.
- Suggested fix: take the file name only (or assert `dest.parent()` equals `Exports`). Optionally re-check existence immediately before creating.

### [COMFY-WEB-14] Photos are uploaded to ComfyUI and never deleted there; URL is not restricted to loopback — Severity: Info — Confidence: High
- Location: `src/comfy/client.rs:185-204`; `src/app/generative.rs:182`
- Evidence: a new `lightphotos_fill_<hex>.png` per run, `overwrite=true`, `type=input`. There is no cleanup call. `normalize_url` accepts any `http`/`https` host.
- Impact / failure scenario: every fill leaves a copy of the user's photo crop in ComfyUI's `input/` forever. If the user points Settings at a LAN or remote host over plain `http`, the crop travels unencrypted. Both are probably intended but undocumented.
- Suggested fix: document it. Optionally warn when the URL's host is not loopback.

### [COMFY-WEB-15] IndexedDB helpers: cross-DB check-then-act and a non-string key crash — Severity: Info — Confidence: Medium
- Location: `src/web/web_catalog_idb.rs:97-110`, `:111-125`
- Evidence: the comment says the check and the forget run "together, so nothing on the Rust side can come between them". The check reads `lightphotos-catalogs` and the delete runs in `lightphotos-folders` in a separate transaction. Another tab can add a catalog that lists the folder in between and lose its handle. `!k.includes("\0")` assumes string keys, while `forgetPresetFolderIfUnlisted` guards with `typeof k === "string"`.
- Impact / failure scenario: low. Cross-tab race, and a hand-edited key would reject the promise (it is logged).
- Suggested fix: add the same `typeof` guard. Re-check after the delete, or accept the race and say so in the comment.

### [COMFY-WEB-16] Small robustness items — Severity: Info — Confidence: Medium
- `src/comfy/workflow.rs:42-46`: `loader_classes` uses `Vec::dedup`, which removes only adjacent duplicates. It works because the two `DualCLIPLoader` rows are adjacent in `LOADERS`. Reordering the table would query a class twice. A `HashSet` or `BTreeSet` would be sturdier.
- `src/comfy/client.rs:96-112`: `normalize_url("http://")` returns `Ok("http://")`. `ComfyServer::new` is infallible apart from this, so it fails later with a `lost` message. Reject an empty host.
- `src/web/web_fs.rs:454-460`: `sort_pairs_by_name` allocates two lowercase `String`s per comparison. Use `sort_by_cached_key` for large folders. Ties (`A.jpg` and `a.jpg`) keep browser iteration order, which differs across browsers.
- `src/web/web_mcp.rs:124-127`: the comment says "a command can export, delete to the Trash". `Command` has no export or delete command today (it has `folder.open`, `preset.add_folder`, `geotag.import`, and edits). The `destructiveHint` is still justified by overwriting edits, but the comment is stale.
- `src/web/web_mcp.rs:251-268`: tools are registered once at startup. A page that loads the polyfill after the wasm starts gets no tools.
- `src/web/analytics.rs:1`: missing the `SPDX-License-Identifier` header every other file has.

## Test coverage assessment
- **comfy, covered well**: URL normalisation, probe, lost-server behaviour for every call, multipart upload, queue and history parsing including malformed JSON, the combo schemas, download resume, ignored Range, hash mismatch, cancel, hostile `Content-Length`, an over-long streaming body, and path-traversal names. These test behaviour through a real local HTTP server.
- **comfy, gaps**:
  - No test for a bad `Content-Range` (the "resumed at the wrong place" branch) or a 416 on resume.
  - No test for a poll that fails once then succeeds, or for `cancel` when `/queue` errors (COMFY-WEB-01, -07).
  - No test for a disk-full write.
  - The `live_fill` test is `#[ignore]` and needs a live server.
- **web, covered**: only `analytics` (host-side shim) and the pure parts of `web_mcp` (`Tool::op`, definitions, `result` shapes) are tested. Neither covers the `decode_failed` stage-suppression case (COMFY-WEB-10).
- **web, not covered**: everything that touches File System Access or IndexedDB (`web_fs`, `web_folder_store_fs`, `web_thumb_cache`, `web_catalog_idb`, `web_export_fs`, `web_exports`, `web_canvas`, `web_decode`). These are wasm-only and have no wasm-bindgen-test harness. The pure logic inside them (`strip_xmp`, `xmp_name`, the cleanup index in `web_thumb_cache`, `sort_pairs_by_name`, `uniqueName`) could be extracted and tested on the host. At minimum, `Cleanup`'s bookkeeping is pure and untested.

## Things done well / verified-safe areas
- `src/comfy/client.rs`: every reply body is size-capped (`read_capped` with `BodyExceedsLimit`). Redirects are off. `clip` slices only at char boundaries. All JSON access uses `get`/`and_then`, so no indexing.
- `src/comfy/download.rs`:
  - Streams to a `.part`, verifies size and SHA-256, and renames. A hostile `Content-Length` is rejected before any bytes are written.
  - `final_path` rejects traversal names.
  - On a cancel, a dropped connection or a write error it keeps the bytes. Wrong or untrustworthy bytes are deleted.
  - No `std::fs::write`. Streaming to `.part` instead of `write_atomic` is documented and justified.
- `src/comfy/workflow.rs`: validation errors name what to fix. Seeds are replaced only when they are literal `u64`s, so links are left alone. Numeric node ids are ordered numerically.
- `src/web/web_folder_store_fs.rs`: an unreadable sidecar is never overwritten. It is copied aside under the first free `.unreadable-N` name before the new record is written. This is a good data-integrity design.
- `src/web/web_folder_store_fs.rs`, `src/web/web_fs.rs`: `is_not_found` is used to separate "missing" from real errors for the sidecar paths. Writeback serialises writes per path, so `update_sidecar`'s read-modify-write cannot interleave within a tab.
- `src/web/web_mcp.rs`: a reply settles exactly once through the shared `Option<Settle>`, whether the app answers, the call is dropped or the signal aborts. Results are marked as untrusted content. Argument validation is tested.
- `src/web/web_exports.rs`: a job that never reaches a thread is reported through `failed`, and the loader answers an error if it cannot enqueue. No leaked destination entries.

## Open questions for maintainers
1. Should a lost poll during `wait` be retried, and should the caller cancel queued prompts on `LOST` and deadline expiry? Today only an explicit user cancel does (COMFY-WEB-01).
2. Does native `cache_key(mtime_ms, len)` truncate `SystemTime` to milliseconds exactly as Chrome's `File.lastModified` does? The sharing of one thumbnail cache between native and web depends on bit-for-bit equal keys. If they differ, each build's `sweep_orphans` would delete the other's entries. I could not verify this from these files.
3. Is it intentional that the web sidecar and thumbnail paths key photos by filename only (not relative path)? Two photos with the same filename in different subfolders resolve to the same `<name>.xmp` and cache entry if both use the same `.lightphotos` handle. I did not confirm which directory handle each caller passes.
4. Is `__lpTestRoot` (`web_fs.rs:44`) meant to ship in the production wasm? Same-origin script can already do anything, so it is not a privilege issue, but a build flag would keep it out.
