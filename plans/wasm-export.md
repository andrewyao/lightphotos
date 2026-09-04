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
  "macos"))]`).
- **RAW export uses the native non-mac RAW pipeline, not a preview tier.**
  `raw/nonmac_decode.rs::decode_raw_nonmac` (rawler `RawDevelop`: PPG
  demosaic → white balance → colour matrix → sRGB gamma, then the shared
  brightness/contrast boost LUT and the fixed auto-denoise post-pass) is
  refactored to a bytes core so wasm can call it. Output is full-resolution
  **premul sRGB8** — exactly the shape `bake_edited` expects and
  byte-identical to what native export bakes. No `LinearF16` bridge, no
  `DemosaicMode::Quality` reuse (that tier exists for the Loupe's GPU
  tonemap and outputs `LinearF16`).
- **Filesystem seam is a trait, `ExportFs`, implemented on both platforms.**
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
  seeds `taken` from it.

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

- `decode_raw_via_rawler(path)` → add `decode_raw_via_rawler_bytes(&[u8])`
  using `rawler::rawsource::RawSource::new_from_slice(bytes)` (already used
  on wasm by `raw/preview.rs`). The `path` version becomes a
  `std::fs`-reading wrapper, or keeps its mmap `RawSource::new(path)` and
  the two share only the post-parse `RawImage` handling.
- `decode_raw_nonmac(path, max_dim)` → `decode_raw_nonmac_from_bytes(&[u8],
  max_dim)`. The orientation lookup currently does a second
  `RawSource::new(path)`; the bytes version reuses the already-parsed
  metadata (or re-parses from the same slice). Everything after the raw
  read — `RawDevelop::develop_intermediate`, the boost LUT, the
  `AUTO_RAW_DENOISE_STRENGTH` post-pass, `fit_within` + `Lanczos3` resize,
  `apply_exif_orientation` — is unchanged and already CPU-only.
- Native `decode_raw_nonmac(path, ..)` keeps its signature (thin wrapper:
  `std::fs::read` then delegate) so `nonmac_decode::decode` and
  `decode_probe.rs` are unaffected.
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
        crate::raw::nonmac_decode::decode_raw_nonmac_from_bytes(src_bytes, u32::MAX)?
    } else {
        crate::raw::nonmac_decode::decode_nonraw_from_bytes(src_bytes, u32::MAX)?
    };
    let (w, h, rgba) = crate::image_ops::bake_edited(&img, adj, touchups, rot);
    crate::image_encode::encode_jpeg_to_vec(w, h, &rgba)
}
```

```rust
#[cfg(not(target_os = "macos"))]
pub trait ExportFs {
    async fn read_source(&self, src: &Path) -> Result<Vec<u8>, String>;
    async fn existing_targets(&self, dest_dir: &Path) -> Result<HashSet<String>, String>;
    async fn write_atomic(&self, dest_dir: &Path, filename: &str, bytes: &[u8]) -> Result<(), String>;
}
```

- `NativeFs` — unit struct. `read_source` = `std::fs::read`.
  `existing_targets` = `std::fs::read_dir` filenames (or `HashSet::new()`
  and let `jpg_export_target`'s own `.exists()` stand — either works
  natively). `write_atomic` = write `<dest>.jpg.tmp` then `rename`.
  Bodies have no `.await`.
- `WebFs` — holds the current folder's `FileSystemDirectoryHandle` plus a
  clone of the `web_file_handles` map (`PathBuf` → `FileSystemFileHandle`).
  `read_source` = map lookup + `web_fs::read_bytes`. `existing_targets` =
  `values()` scan of the `Exports/` subdir handle (`Ok(HashSet::new())` if
  it doesn't exist yet). `write_atomic` = get/create `Exports/` under the
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

### 4. Native — `export::Exporter` onto the shared path

Each worker thread:

```rust
let bytes  = pollster::block_on(fs.read_source(&job.src))?;
let jpeg   = bake_jpeg(&bytes, job.is_raw, &job.adj, &job.touchups, job.rot)?;
pollster::block_on(fs.write_atomic(&job.dest_dir, &job.filename, &jpeg))?;
```

`Exporter` becomes `Exporter<F: ExportFs>` (or holds an `Arc<F>`);
`main.rs:115` constructs `Exporter::new(NativeFs)`. `do_export`'s old
decode/bake/encode/rename body collapses into the three lines above. The
`std::fs::create_dir_all(Exports/)` up-front check stays in `start_export`'s
native arm (or moves behind `NativeFs`).

### 5. wasm — `web_worker_pool` `JobKind::Export`

`web_worker_pool.rs`:

- `enum JobKind { Thumb, Speed, Preview, Full, Export }`.
- `WorkerPoolHandle::submit_export(id_key: PathBuf, bytes: ArrayBuffer,
  is_raw: bool, adj_json: String, touchups_json: String, rot: u8)` — posts
  `{ id, export: true, bytes (transferred), isRaw, adjustments, touchups,
  rot }`.
- Second result channel: `struct ExportPoolResult { path: PathBuf, result:
  Result<Vec<u8>, String> }`, `Inner.export_tx`, `WorkerPool.export_rx`,
  `WorkerPool::poll_exports() -> Vec<ExportPoolResult>`.
- `handle_worker_message`: when the pending job's kind is `Export`, read the
  `jpeg` `ArrayBuffer` field (not `rgba`/`width`/`height`) and route to
  `export_tx`.

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
        let existing = fs.existing_targets(&dest_dir).await.unwrap_or_default();
        let dests    = resolve_targets(&paths, &dest_dir, &existing);
        for (src, dest) in paths.iter().zip(&dests) {
            match fs.read_source(src).await {
                Ok(bytes) => pool.submit_export(src.clone(), bytes_to_arraybuffer(bytes),
                                                is_raw_extension(src), adj_json, tu_json, rot),
                Err(e)    => { let _ = tx.send(ExportOutcome { src: src.clone(), result: Err(e) }); }
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
- `ExportPoolResult` carries `dest_dir` + `filename` (copied from the job by
  `handle_worker_message`) so the write side needs no side table.
- `main.rs` frame loop, wasm arm:

```rust
for ExportPoolResult { path, result } in self.web_worker_pool.poll_exports() {
    match result {
        Ok(jpeg) => {
            let fs = /* WebFs for the current export dir */;
            let (dir, name) = /* dest_dir + filename resolved for `path` */;
            let tx = self.web_export_tx.clone();
            spawn_local(async move {
                let r = fs.write_atomic(&dir, &name, &jpeg).await.map(|_| dir.join(&name));
                let _ = tx.send(ExportOutcome { src: path, result: r });
            });
        }
        Err(e) => { let _ = self.web_export_tx.send(ExportOutcome { src: path, result: Err(e) }); }
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

## Tasks

- [ ] **Task 1 — `encode_jpeg_to_vec`.** Split `image_encode.rs`. Unit test:
  `encode_jpeg_to_vec` → `image_decode::decode` round-trips dims + colour
  (mirror the existing `encode_then_decode_round_trips`, minus the file).
  `cargo test`.
- [ ] **Task 2 — RAW bytes core.** `decode_raw_via_rawler_bytes` +
  `decode_raw_nonmac_from_bytes` in `raw/nonmac_decode.rs`; native
  `decode_raw_nonmac` becomes a `std::fs::read` wrapper. `cargo test`
  (`decode_probe` golden hashes must not move). **Then `RUSTFLAGS="--cfg=
  web_sys_unstable_apis" trunk build --release` to confirm `RawDevelop` /
  `to_dynamic_image` / `image::imageops::resize` link for wasm32** — if
  not, take the `LinearF16` fallback noted in §2 before continuing.
- [ ] **Task 3 — `bake_jpeg` + `ExportFs` + `NativeFs`.** Shared items in
  `export.rs`; `resolve_targets`. Rewrite `Exporter`/`do_export` onto
  `bake_jpeg` + `NativeFs` + `pollster::block_on`. `cargo test`; run a
  native export by hand (RAW + JPEG, with crop/rotate/develop) and eyeball
  the output against the Loupe.
- [ ] **Task 4 — `web_worker_pool` `JobKind::Export`.** `submit_export`,
  `ExportPoolResult`, `export_rx`, `poll_exports`, message routing.
- [ ] **Task 5 — `wasm_worker.rs` export branch.** Module includes,
  `export: true` handling, `bake_jpeg` call, `{id, ok, jpeg}` reply. `trunk
  build --release`.
- [ ] **Task 6 — `WebFs` + `web/web_export_fs.rs`.** `read_source` /
  `existing_targets` / `write_atomic` against FSA handles.
- [ ] **Task 7 — wasm `start_export` + result plumbing.** `App`
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

## Out of scope

- macOS export path (unchanged — ImageIO decode + encode).
- Unifying the native thread pool and the wasm Web Worker pool behind one
  executor interface.
- Export format options (always JPEG q90, matching native).
- Passing FSA handles into the Web Worker to make it do its own write —
  main-thread write via `WebFs` is the chosen model.
