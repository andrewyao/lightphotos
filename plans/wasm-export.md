# Web (wasm32) JPEG export

## Context

Export ("bake the develop/crop/rotation edits into a fresh JPEG under
`<current folder>/Exports/`") works on native today:

- `app/export.rs::start_export` (native arm) gathers each photo's
  `Adjustments` / `Vec<TouchUp>` / rotation from the in-memory mirrors,
  resolves a collision-free `Exports/<stem>.jpg` target
  (`paths::jpg_export_target`), and submits one self-contained `ExportJob`
  per photo to `export::Exporter`.
- `export::Exporter` is a `std::thread` worker pool. Each worker runs
  `do_export`: `image_decode::decode(src, u32::MAX)` (full-res) →
  `image_ops::bake_edited(&img, &adj, &touchups, rot)` →
  `image_encode::encode_jpeg(tmp, w, h, &rgba)` → atomic `rename` onto the
  final path.
- `App::on_export_outcomes` (already platform-neutral) folds finished jobs
  into the progress toast; `main.rs`'s frame loop drains
  `Exporter::poll()` and keeps a lazy redraw alive while
  `export_progress.is_some()`.

On wasm the whole thing is a stub:

```rust
#[cfg(target_arch = "wasm32")]
pub(super) fn start_export(&mut self, _paths: Vec<PathBuf>) {
    self.set_status("Export isn't supported in the browser yet".into());
    self.request_redraw();
}
```

`export::Exporter`'s thread pool degrades to zero workers in the browser
(thread spawn always fails), `image_encode::encode_jpeg`'s non-mac arm ends
in `std::fs::write`, and a picked folder has no real OS path — output has to
go through the folder's `FileSystemDirectoryHandle` (File System Access API),
the same transport `web_catalog_fs::write_sidecar` already uses for sidecars.

wasm decode already runs off the main thread: `web_worker_pool.rs` (main
thread) + `wasm_worker.rs` (a separate `[[bin]]`, N `web_sys::Worker`
instances, each its own wasm linear memory, no `SharedArrayBuffer`). Loupe
and Grid decode both dispatch there today.

## Decisions already made

- **One shared compute path for wasm32 and native non-mac.** Decode + bake +
  encode is platform-neutral Rust already; only the byte IO differs. A
  single `export::bake_jpeg(src_bytes, is_raw, adj, touchups, rot)
  -> Result<Vec<u8>, String>` is called verbatim by native worker threads
  and by the wasm Web Worker. macOS keeps its own ImageIO decode/encode
  path (out of scope here — `bake_jpeg` is `#[cfg(not(target_os =
  "macos"))]`). The native and wasm implementations of `Exporter` are
  separate cfg-gated implementations: macOS retains its existing exporter
  and `App` field/constructor types, while only non-mac native targets use
  the generic `Exporter<F: ExportFs>` described below.
- **RAW export uses the native non-mac RAW pipeline, not a preview tier.**
  `raw/nonmac_decode.rs::decode_raw_nonmac` (rawler `RawDevelop`: PPG
  demosaic → white balance → colour matrix → sRGB gamma, then the shared
  brightness/contrast boost LUT and the fixed auto-denoise post-pass) is
  refactored to a bytes core so wasm can call it. Output is full-resolution
  **premul sRGB8** — exactly the shape `bake_edited` expects and
  byte-identical to what native export bakes. No `LinearF16` bridge, no
  `DemosaicMode::Quality` reuse (that tier exists for the Loupe's GPU
  tonemap and outputs `LinearF16`).
- **Filesystem seam is a trait, `ExportFs`, implemented on non-mac native
  and wasm.**
  `NativeFs` (std::fs + tmp/rename) and `WebFs` (FSA dir handle +
  `create_writable` + `close`). Generic over `F: ExportFs` — no `dyn`, no
  `async-trait` crate (native impl bodies contain no `.await`; the worker
  threads `pollster::block_on` the ready futures for free).
- **Execution model stays per-platform.** Native `std::thread` pool
  (`export::Exporter`); wasm the existing `web_worker_pool` with a new
  `JobKind::Export`. Unifying the thread-vs-Worker lifecycle is a separate,
  much larger lift and is not a decode/encode concern. `ExportJob` /
  `ExportOutcome` / target resolution / `on_export_outcomes` / the progress
  toast are shared.
- **Collision resolution is FS-aware.** `paths::jpg_export_target` calls
  `Path::exists()`, which is always false in the browser and would clobber
  an `Exports/foo.jpg` left by a previous session. `ExportFs` owns an
  `existing_targets(dir) -> HashSet<String>` scan; a pure name resolver
  seeds `taken` from it. Enumeration errors are fatal: only a positively
  identified missing `Exports/` directory is represented as an empty set.
- **Exports have dedicated capacity.** Bulk export work uses a dedicated
  export worker pool (or an equivalent reserved export queue/concurrency
  limit), separate from the interactive thumbnail, preview, and Loupe
  decode pool. Export jobs may not consume all interactive decode slots.

## Design

### 1. `image_encode.rs` — split the encoder

Extract the pure half:

```rust
#[cfg(not(target_os = "macos"))]
pub fn encode_jpeg_to_vec(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    // zero-size / buffer-length validation (moved verbatim from encode_jpeg)
    // mozjpeg_rs::Encoder ... .quality(90).encode_rgba(rgba, width, height)
}
```

The non-mac `encode_jpeg(out, w, h, rgba)` becomes `encode_jpeg_to_vec(..)?`
followed by `std::fs::write(out, bytes)`. The macOS arm (ImageIO,
file-URL-based) is untouched — wasm never reaches it.

### 2. `raw/nonmac_decode.rs` — bytes core

- `decode_raw_via_rawler(path)` remains the native path-based rawler entry
  point. The bytes path constructs `RawSource::new_from_slice(bytes)` (already
  used on wasm by `raw/preview.rs`), and both paths share the post-parse
  `RawImage` handling through `decode_raw_nonmac_from_source`.
- `decode_raw_nonmac_from_bytes(&[u8], max_dim)` is the wasm entry point, while
  `decode_raw_nonmac(path, max_dim)` remains the native path entry point. The
  orientation lookup currently does a second `RawSource::new(path)`; the bytes
  version reuses the already-parsed metadata (or re-parses from the same
  slice). Everything after the raw read — `RawDevelop::develop_intermediate`,
  the boost LUT, the
  `AUTO_RAW_DENOISE_STRENGTH` post-pass, `fit_within` + `Lanczos3` resize,
  `apply_exif_orientation` — is unchanged and already CPU-only.
- Native `decode_raw_nonmac(path, ..)` keeps its signature and mmap-backed
  `RawSource::new(path)` path for lower peak heap use; the bytes entry point is
  used by wasm export. `nonmac_decode::decode` and `decode_probe.rs` remain
  unaffected.
- **Build risk:** `raw/preview.rs` calls lower-level rawler pieces, not
  `RawDevelop` / `DynamicImage::into_rgba8` / `image::imageops::resize`
  directly. If any of those three does not link for
  `wasm32-unknown-unknown`, fall back to a `LinearF16`→premul-sRGB8
  converter on `decode_raw_quality_from_bytes`'s output (reusing
  `rawler::imgop::srgb::srgb_apply_gamma` + `apply_raw_preview_boost`, the
  same curves `raw_shader.wgsl` applies on the GPU). Verify with a
  `trunk build --release` before wiring the rest.

### 3. `export.rs` — shared compute + FS trait

```rust
#[cfg(not(target_os = "macos"))]
pub fn bake_jpeg(
    src_bytes: &[u8],
    is_raw: bool,
    adj: &Adjustments,
    touchups: &[TouchUp],
    rot: u8,
) -> Result<Vec<u8>, String> {
    let img = if is_raw {
        crate::image_decode::decode_raw_nonmac_from_bytes(src_bytes, u32::MAX)?
    } else {
        crate::image_decode::decode_nonraw_from_bytes(src_bytes, u32::MAX)?
    };
    let (w, h, rgba) = crate::image_ops::bake_edited(&img, adj, touchups, rot);
    crate::image_encode::encode_jpeg_to_vec(w, h, &rgba)
}
```

```rust
#[cfg(not(target_os = "macos"))]
pub trait ExportFs {
    async fn read_source(&self, src: &Path) -> Result<Vec<u8>, String>;
    async fn existing_targets(&self, dest_dir: &Path) -> Result<HashSet<String>, ExportFsError>;
    async fn write_atomic(&self, dest_dir: &Path, filename: &str, bytes: &[u8]) -> Result<(), String>;
}
```

`ExportFsError` has a distinct `MissingDirectory` variant plus an error
variant for permission, listing, and other failures. Only the former is
converted to an empty set by target resolution.

- `NativeFs` — unit struct. `read_source` = `std::fs::read`.
  `existing_targets` = `std::fs::read_dir` filenames (or `HashSet::new()`
  and let `jpg_export_target`'s own `.exists()` stand — either works
  natively). `write_atomic` = write `<dest>.jpg.tmp` then `rename`.
  Bodies have no `.await`.
- `WebFs` — holds the current folder's `FileSystemDirectoryHandle` plus a
  clone of the `web_file_handles` map (`PathBuf` → `FileSystemFileHandle`).
  `read_source` = map lookup + `web_fs::read_bytes`. `existing_targets` =
  `values()` scan of the `Exports/` subdir handle. It returns an explicit
  `MissingDirectory` outcome only when the handle lookup confirms that
  `Exports/` does not exist; permission, enumeration, and other failures
  remain errors. `write_atomic` = get/create `Exports/` under the
  folder handle, `get_file_handle_with_options(create)`, `create_writable`,
  `write_with_js_u8_array`, `close` (atomic swap) — lifted from
  `web_catalog_fs::write_sidecar`. Lives in `src/web/web_export_fs.rs`.
- Pure name resolver shared by both platforms:
  `resolve_targets(srcs: &[PathBuf], dest_dir: &Path, existing: &HashSet<String>)
  -> Vec<PathBuf>` — the `jpg_export_target` loop with the `.exists()` call
  replaced by an `existing.contains(name)` check. `jpg_export_target` itself
  can stay for native or be reworked in terms of this.

`ExportJob` / `ExportOutcome` move to the shared (non-cfg) part of the file.
`ExportJob` carries `src`, `dest_dir`, `filename`, `is_raw`, `adj`,
`touchups`, `rot`. Native reads the source bytes *inside* the worker
(`fs.read_source` on the thread — keeps IO parallel). wasm reads them on the
main thread *before* dispatch, because the bytes cross to the Web Worker as a
transferred `ArrayBuffer` (the established `request_web_preview` pattern);
`dest_dir` + `filename` ride along on the job and come back on
`ExportPoolResult` so the main thread can write without a side table.
`submit_export` receives both destination fields and stores them in pending
metadata; every success or failure result carries them, including read,
worker, `postMessage`, and write failures.

### 4. Native — `export::Exporter` onto the shared path

Each worker thread:

```rust
let bytes  = pollster::block_on(fs.read_source(&job.src))?;
let jpeg   = bake_jpeg(&bytes, job.is_raw, &job.adj, &job.touchups, job.rot)?;
pollster::block_on(fs.write_atomic(&job.dest_dir, &job.filename, &jpeg))?;
```

`Exporter` becomes `Exporter<F: ExportFs>` (or holds an `Arc<F>`) only on
non-mac native targets; macOS keeps its current concrete `Exporter` and
constructor unchanged. The non-mac construction site is cfg-gated and
constructs `Exporter::new(NativeFs)`. `do_export`'s old
decode/bake/encode/rename body collapses into the three lines above. The
`std::fs::create_dir_all(Exports/)` up-front check stays in `start_export`'s
native arm (or moves behind `NativeFs`).
Concretely, the existing macOS `App` exporter field and `main.rs`
construction remain under the macOS cfg; the non-mac `App` field is
`Exporter<NativeFs>` and its construction is under the complementary cfg.
No generic `ExportFs` type may appear in the macOS-only module or
constructor.

### 5. wasm — `web_worker_pool` `JobKind::Export`

`web_worker_pool.rs`:

- `enum JobKind { Thumb, Speed, Preview, Full, Export }`.
- `WorkerPoolHandle::submit_export(id_key: PathBuf, dest_dir: PathBuf,
  filename: String, bytes: ArrayBuffer, is_raw: bool, adj_json: String,
  touchups_json: String, rot: u8)` — stores the destination fields in the
  pending entry, then posts
  `{ id, export: true, bytes (transferred), isRaw, adjustments, touchups,
  rot }`.
- Second result channel: `struct ExportPoolResult { path: PathBuf,
  dest_dir: PathBuf, filename: String, result: Result<Vec<u8>, String> }`,
  `Inner.export_tx`, `WorkerPool.export_rx`,
  `WorkerPool::poll_exports() -> Vec<ExportPoolResult>`.
- `handle_worker_message`: when the pending job's kind is `Export`, read the
  `jpeg` `ArrayBuffer` field (not `rgba`/`width`/`height`) and route to
  `export_tx`, copying `dest_dir` and `filename` from pending metadata.
- Install both `Worker::onerror` and `Worker::onmessageerror` handlers, and
  treat a failed `postMessage` as an immediate job failure. A worker trap,
  termination, or message error fails every export assigned to that worker, removes
  the dead worker slot, and replaces it (or marks the dedicated export slot
  unavailable). This must produce an `ExportOutcome` so
  `export_progress` cannot remain active forever.

`wasm_worker.rs`:

- Add `#[path = "../image_ops.rs"] mod image_ops;`, `#[path =
  "../image_encode.rs"] mod image_encode;`, `#[path = "../export.rs"] mod
  export;` (the `Exporter`/`NativeFs`/`ExportFs` items are all
  `#[cfg(not(target_arch = "wasm32"))]` or native-only and drop out; only
  `bake_jpeg` + types compile into the worker bin). `raw/nonmac_decode.rs`
  is pulled in transitively or via its own `#[path]`.
- `onmessage`: if `get_bool(&data, "export")`, deserialize
  `adjustments`/`touchups` (`serde_json::from_str`) + `rot`, call
  `export::bake_jpeg(&bytes, is_raw, &adj, &touchups, rot)`, post `{ id, ok,
  jpeg }` with the buffer transferred; on `Err`, post `{ id, ok: false,
  error }`. Existing decode branch unchanged.

### 6. wasm — `app/export.rs` real `start_export`

```rust
#[cfg(target_arch = "wasm32")]
pub(super) fn start_export(&mut self, paths: Vec<PathBuf>) {
    // guards: empty, export_progress.is_some(), catalog_load_pending
    // (mirror the native arm's three rejections)
    let folder   = self.folder_sel.clone().unwrap_or_else(|| root_path());
    let dir_h    = self.web_dir_handles.get(&folder)...;   // fallback: root handle
    let fs       = WebFs::new(dir_h.clone(), self.web_file_handles.clone());
    let dest_dir = folder.join("Exports");

    // per-photo edits from the mirrors, same as native:
    //   adj  = self.edits.get(&src).copied().unwrap_or_default()
    //   tu   = self.touchups.get(&src).cloned().unwrap_or_default()
    //   rot  = self.rotations.get(&src).copied().unwrap_or(0)

    let total = paths.len();
    self.export_progress = Some(ExportProgress { done: 0, total, errors: 0, last_err: None });

    let pool = self.web_worker_pool.handle();
    let tx   = self.web_export_tx.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let existing = match fs.existing_targets(&dest_dir).await {
            Ok(names) => names,
            Err(ExportFsError::MissingDirectory) => HashSet::new(),
            Err(e) => {
                // Abort the batch. A listing or permission error is not an
                // empty directory and must never permit an overwrite.
                for src in &paths {
                    let _ = tx.send(ExportOutcome {
                        src: src.clone(), dest_dir: dest_dir.clone(),
                        filename: planned_filename(src),
                        result: Err(format!("cannot enumerate export directory: {e}")),
                    });
                }
                return;
            }
        };
        let dests = resolve_targets(&paths, &dest_dir, &existing);
        for (src, dest) in paths.iter().zip(&dests) {
            let filename = dest.file_name().unwrap().to_string_lossy().into_owned();
            match fs.read_source(src).await {
                Ok(bytes) => pool.submit_export(
                    src.clone(), dest_dir.clone(), filename, bytes_to_arraybuffer(bytes),
                    is_raw_extension(src), adj_json, tu_json, rot,
                ),
                Err(e) => { let _ = tx.send(ExportOutcome {
                    src: src.clone(), dest_dir: dest_dir.clone(), filename, result: Err(e)
                }); }
            }
        }
    });
}
```

(Exact byte-handle plumbing follows `request_web_preview`'s
`read_array_buffer` + transfer pattern; `submit_export` may take the
`ArrayBuffer` directly to avoid a copy into main-thread wasm memory.)

### 7. wasm — result → write → outcome

- New `App` fields (wasm only): `web_export_tx: Sender<ExportOutcome>`,
  `web_export_rx: Receiver<ExportOutcome>` (mpsc, like `web_dirlist_tx/rx`).
- `ExportOutcome` and `ExportPoolResult` both carry `dest_dir` + `filename`.
  These are copied from the pending job by `handle_worker_message`, so the
  write side needs no side table; read, worker, message, and write errors
  use the same fields.
- `main.rs` frame loop, wasm arm:

```rust
for ExportPoolResult { path, dest_dir, filename, result } in self.web_worker_pool.poll_exports() {
    match result {
        Ok(jpeg) => {
            let fs = /* WebFs for the current export dir */;
            let tx = self.web_export_tx.clone();
            spawn_local(async move {
                let r = fs.write_atomic(&dest_dir, &filename, &jpeg)
                    .await.map(|_| dest_dir.join(&filename));
                let _ = tx.send(ExportOutcome {
                    src: path, dest_dir, filename, result: r
                });
            });
        }
        Err(e) => { let _ = self.web_export_tx.send(ExportOutcome {
            src: path, dest_dir, filename, result: Err(e)
        }); }
    }
}
let outcomes: Vec<_> = std::iter::from_fn(|| self.web_export_rx.try_recv().ok()).collect();
if !outcomes.is_empty() { self.on_export_outcomes(outcomes); }
```

`dest_dir` + `filename` are carried on `ExportJob` and copied onto
`ExportPoolResult` by `handle_worker_message`, so the write side needs no
`App`-side side table.

- Native `main.rs` keeps `self.exporter.poll()` → `on_export_outcomes`
  unchanged. The keep-awake redraw (`export_progress.is_some()`) already
  covers both.

## Status

Tasks 1–7 implemented and committed (`7d2ed42`..`HEAD`). Native + wasm
(`trunk build --release`) both build clean; 193 native tests pass (incl. the
new `encode_jpeg_to_vec`, RAW bytes-core parity, `bake_jpeg`, and
`jpg_export_name` tests). The Task 2 link risk is resolved — `RawDevelop` /
`to_dynamic_image` / `image::imageops::resize` all link for wasm32, so RAW
export uses the real native pipeline with no `LinearF16` fallback. **Task 8
(browser acceptance) is outstanding** — needs a human at a browser.

Implementation notes / deviations from the sketch above:
- `ExportFs` is `read_source` + `write_atomic` only; the collision scan is
  `WebFs::existing_export_names` (inherent, wasm-only), not a trait method —
  native keeps `jpg_export_target`'s `Path::exists()`.
- Task 2 uses `decode_raw_nonmac_from_bytes` plus a shared
  `decode_raw_nonmac_from_source` tail; there is no separate
  `decode_raw_via_rawler_bytes`. The native path-based decoder intentionally
  retains rawler's mmap-backed `RawSource::new(path)` instead of becoming an
  `std::fs::read` wrapper.
- `submit_export` takes `Vec<u8>`, not an `ArrayBuffer` — export isn't
  latency-critical, so one copy into a fresh JS buffer is fine (the decode
  path's zero-copy transfer is kept only where it matters).
- `main.rs`'s existing `export_progress.is_some()` → 100 ms poll cadence
  already keeps the wasm loop draining `poll_exports` — no new keep-awake
  wiring.

## Tasks

- [x] **Task 1 — `encode_jpeg_to_vec`.** Split `image_encode.rs`. Unit test
  writes the returned bytes to a temporary JPEG and uses
  `image_decode::decode` to round-trip dims + colour. `cargo test`.
- [x] **Task 2 — RAW bytes core.** Add the bytes-backed RAW source path and
  `decode_raw_nonmac_from_bytes` in `raw/nonmac_decode.rs`; share the parsed
  `RawImage` processing tail with native `decode_raw_nonmac`, which retains its
  mmap-backed path-based decoder for lower peak memory use. `cargo test`
  (`decode_probe` golden hashes must not move). **Then `RUSTFLAGS="--cfg=
  web_sys_unstable_apis" trunk build --release` to confirm `RawDevelop` /
  `to_dynamic_image` / `image::imageops::resize` link for wasm32** — if
  not, take the `LinearF16` fallback noted in §2 before continuing.
- [x] **Task 3 — `bake_jpeg` + `ExportFs` + `NativeFs`.** Shared items in
  `export.rs`; `resolve_targets`. Rewrite `Exporter`/`do_export` onto
  `bake_jpeg` + `NativeFs` + `pollster::block_on`. `cargo test`; run a
  native export by hand (RAW + JPEG, with crop/rotate/develop) and eyeball
  the output against the Loupe.
- [x] **Task 4 — `web_worker_pool` `JobKind::Export`.** `submit_export`,
  `ExportPoolResult`, `export_rx`, `poll_exports`, message routing.
- [x] **Task 5 — `wasm_worker.rs` export branch.** Module includes,
  `export: true` handling, `bake_jpeg` call, `{id, ok, jpeg}` reply. `trunk
  build --release`.
- [x] **Task 6 — `WebFs` + `web/web_export_fs.rs`.** `read_source` /
  `existing_targets` / `write_atomic` against FSA handles.
- [x] **Task 7 — wasm `start_export` + result plumbing.** `App`
  `web_export_tx/rx`; the `app/export.rs` wasm arm; the `main.rs` wasm
  frame-loop drain. `trunk build --release`.
- [ ] **Task 8 — browser acceptance (human).** `deploy-web.sh` or `trunk
  serve`, pick a folder with a JPEG and a RAW:
  - Export one JPEG, no edits → `Exports/<stem>.jpg` appears, opens, looks
    right.
  - Export one RAW → same; matches the Loupe render (develop + auto-denoise
    baked).
  - Export with crop + rotation + exposure/tone edits → all baked.
  - Export the same photo twice → `<stem>.jpg` then `<stem>-1.jpg`; a
    pre-existing `Exports/<stem>.jpg` from a prior session is not clobbered.
  - Multi-select export → progress toast counts up, final summary correct,
    per-file errors surfaced.
  - `Exports/` shows up in the folder tree (subfolder-browsing work) and can
    be opened to view results.

Implementation follow-ups for Tasks 4–7: use the dedicated export capacity
specified above; preserve destination metadata on every `ExportOutcome`; and
cover enumeration failure, worker loss/message failure, and failed writes in
the progress/error tests. These are required correctness conditions, not
optional browser-only behavior.

## Out of scope

- macOS export path (unchanged — ImageIO decode + encode).
- Unifying the native thread pool and the wasm Web Worker pool behind one
  executor interface.
- Export format options (always JPEG q90, matching native).
- Passing FSA handles into the Web Worker to make it do its own write —
  main-thread write via `WebFs` is the chosen model.
