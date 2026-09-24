//! wasm32-only file access and decoding. Picks and lists folders through the
//! File System Access API, reads photo bytes on the main thread (only it holds
//! the file handles), and sends them to the Web Worker pool. Results land in
//! `loader.rs`'s caches through its `*_external` methods.

use super::*;
use crate::navigation::Playlist;
use crate::thumbnail::THUMB_PX;
use crate::web_fs;
use crate::web_worker_pool::JobKind;

/// Cap on concurrent file reads from the picked folder, shared by thumbnails,
/// the loupe, and the cache sweep. Chrome throws `NotReadableError` when too
/// many reads run at once. 2 is the highest value observed never to trip it;
/// 4 is used anyway, because the retry backoff below recovers a read that does
/// fail and the extra concurrency is worth more than those retries cost. If
/// folder loads stall, lower it.
const MAX_CONCURRENT_READS: u32 = 4;

/// Failures a key tolerates before it is marked failed for good. Chrome's
/// read failures are transient and can last longer than a few seconds.
const MAX_READ_RETRIES: u8 = 10;

/// Delay before retry `attempt` (1-based), drawn uniformly from `[0, cap]`
/// where `cap` doubles from 500 ms up to 10 s ("full jitter"). Keys that fail
/// together would retry together and collide again with a fixed delay. The
/// random spread breaks them apart.
fn retry_backoff(attempt: u8) -> std::time::Duration {
    let cap_ms = 500u64
        .saturating_mul(1u64 << attempt.saturating_sub(1).min(20))
        .min(10_000);
    let jittered_ms = (js_sys::Math::random() * cap_ms as f64).max(50.0);
    std::time::Duration::from_millis(jittered_ms as u64)
}

impl App {
    /// Sizes the thumbnail cache for the visible grid plus pending Auto Tone
    /// photos, so an Auto Tone thumbnail is not evicted before
    /// `poll_auto_tone` reads it. Call before draining worker results.
    pub(crate) fn prepare_web_thumb_cache(&mut self) {
        let visible = self.working_thumb_keys().len();
        // The window, not the whole batch. Sizing this to `autotone_pending`
        // told the cache to hold the entire selection, which on a 32-bit heap
        // that never shrinks is how a large folder ran the tab out of memory.
        let capacity = visible + self.autotone_window.len();
        if let Some(loader) = &mut self.loader {
            loader.set_thumb_working_set_size(capacity);
        }
    }

    /// Opens the browser's folder picker unless one is already open.
    pub(crate) fn request_folder_pick(&mut self) {
        if self.web_folder_pending {
            return;
        }
        self.web_folder_pending = true;
        let tx = self.web_folder_tx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = web_fs::pick_and_list_folder().await;
            let _ = tx.send(result);
        });
        self.request_redraw();
    }

    /// Loads a finished pick through `load_playlist`, like native
    /// `load_folder`. Returns true while a pick is still open.
    pub(crate) fn poll_folder_pick(&mut self) -> bool {
        if let Ok(result) = self.web_folder_rx.try_recv() {
            self.web_folder_pending = false;
            match result {
                Ok(picked) => {
                    // A new folder invalidates pending listings, deferred
                    // navigation, and thumbnail decodes using the old handles.
                    self.supersede_web_pending_nav();
                    self.invalidate_web_thumb_handles();
                    self.fetch_cjk_font_for(picked.handles.keys().chain(picked.dir_handles.keys()));
                    self.web_file_handles = picked.handles;
                    self.web_dir_handles = picked.dir_handles;
                    self.web_thumb_cleanup.clear();
                    self.subdirs.clear();

                    let root = picked.dir.clone();
                    let mut first_level: Vec<PathBuf> = self
                        .web_dir_handles
                        .keys()
                        .filter(|p| p.parent() == Some(root.as_path()))
                        .cloned()
                        .collect();
                    crate::navigation::sort_by_name(&mut first_level);
                    self.subdirs.insert(root.clone(), first_level);

                    // Must precede `load_playlist`, whose catalog load reads
                    // sidecars through this handle.
                    let root_handle = self.web_dir_handles.get(&root).cloned();
                    self.catalog.set_wasm_dir_handle(root_handle);
                    let playlist = Playlist::from_entries(root.clone(), picked.entries);
                    self.load_playlist(playlist, root.clone());
                    // Show the folder tree only once the playlist is loaded.
                    self.folder_root = Some(root.clone());
                    self.expanded = std::collections::HashSet::from([root]);
                    self.mode = ViewMode::Grid;
                }
                Err(e) => {
                    // Usually a cancelled picker, but a permission or listing
                    // failure lands here too, so always show it.
                    self.set_status((crate::i18n::t().open_folder_failed)(&e.to_string()));
                }
            }
            self.request_redraw();
        }
        self.web_folder_pending
    }

    /// The wasm32 version of `request_working_thumbs`. Reads each missing
    /// thumbnail's bytes (or its cached JPEG) and sends them to the Web Worker
    /// pool. Returns true if any thumbnail is still missing.
    pub(crate) fn request_web_thumbs(&mut self) -> bool {
        let px = THUMB_PX;
        let mut keys: Vec<PathBuf> = self
            .working_thumb_keys()
            .into_iter()
            .map(|(p, _, _)| p)
            .collect();
        // Auto Tone needs thumbnails for its photos even off screen, and
        // `loader.rs`'s queue has no workers on wasm32. They go after the
        // visible grid so the grid reads first.
        keys.extend(self.autotone_pending.iter().cloned());
        self.prepare_web_thumb_cache();

        let mut any_missing = false;
        for path in keys {
            let key = (path.clone(), px);
            let already_have = self
                .loader
                .as_ref()
                .is_some_and(|l| l.get_thumb(&path, px).is_some() || l.thumb_failed(&path, px));
            if already_have || self.web_thumb_inflight.contains(&key) {
                continue;
            }
            any_missing = true;
            if self
                .web_thumb_retries
                .get(&key)
                .is_some_and(|(_, retry_at)| Instant::now() < *retry_at)
            {
                continue;
            }
            // Out of read slots. The key stays off `web_thumb_inflight`, so
            // next frame tries it again.
            if self.web_read_inflight.get() >= MAX_CONCURRENT_READS {
                continue;
            }
            let Some(handle) = self.web_file_handles.get(&path).cloned() else {
                // An Auto Tone photo deleted mid-batch has no handle. Mark it
                // failed so `poll_auto_tone` drops it instead of waiting.
                if let Some(loader) = &mut self.loader {
                    loader.mark_thumb_failed_external(path.clone(), px);
                }
                continue;
            };
            // The folder holding this photo's `.lightphotos/` cache. If it is
            // missing, the photo decodes without the cache.
            let cache_dir = path
                .parent()
                .and_then(|d| self.web_dir_handles.get(d))
                .cloned();
            let generation = self.web_handle_generation;
            self.web_thumb_inflight.insert(key);
            self.web_read_inflight.set(self.web_read_inflight.get() + 1);
            let read_inflight = self.web_read_inflight.clone();
            let pool = self.web_worker_pool.handle();
            wasm_bindgen_futures::spawn_local(async move {
                let is_raw = crate::image_decode::is_raw_extension(&path);

                // The cache entry is named by size and mtime, which `stat`
                // reads without reading the file's bytes.
                let entry = match (web_fs::stat(&handle).await, path.file_name()) {
                    (Ok(file), Some(name)) => Some(crate::web_thumb_cache::entry_name(
                        name,
                        crate::web_thumb_cache::key_for(&file),
                    )),
                    _ => None,
                };

                // A cache hit decodes a ~45 KB JPEG and never reads the
                // multi-megabyte source.
                if let (Some(name), Some(root)) = (&entry, &cache_dir) {
                    if let Some(cached) = crate::web_thumb_cache::load(root, name).await {
                        read_inflight.set(read_inflight.get().saturating_sub(1));
                        let buf = js_sys::Uint8Array::from(cached.as_slice()).buffer();
                        pool.submit_thumb(path, px, buf, false, entry, generation, true);
                        return;
                    }
                }

                let result = web_fs::read_array_buffer(&handle).await;
                read_inflight.set(read_inflight.get().saturating_sub(1));
                match result {
                    // On a miss the worker also encodes a JPEG, and
                    // `poll_web_thumbs` stores it.
                    Ok(bytes) => {
                        pool.submit_thumb(path, px, bytes, is_raw, entry, generation, false)
                    }
                    Err(e) => {
                        web_sys::console::error_1(
                            &format!("[web] reading bytes failed for {}: {e}", path.display())
                                .into(),
                        );
                        // Report through the pool so `poll_web_thumbs` clears
                        // the in-flight key like any decode failure.
                        pool.fail(path, px, JobKind::Thumb, Some(generation), e);
                    }
                }
            });
        }
        any_missing
    }

    /// Writes a worker-encoded thumbnail JPEG into the photo's `.lightphotos/`
    /// folder in the background. Failures are logged, not toasted: they only
    /// cost a re-decode next session, and a full disk would toast once per
    /// photo.
    fn store_web_thumb(&mut self, path: &Path, name: String, bytes: Vec<u8>) {
        let Some(root) = path
            .parent()
            .and_then(|d| self.web_dir_handles.get(d))
            .cloned()
        else {
            return;
        };
        let cleanup = self
            .web_thumb_cleanup
            .entry(path.parent().unwrap().to_path_buf())
            .or_default()
            .clone();
        let display = path.to_path_buf();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = crate::web_thumb_cache::store(&root, &name, &bytes, &cleanup).await {
                web_sys::console::warn_1(
                    &format!(
                        "[web] could not cache thumbnail for {}: {e}",
                        display.display()
                    )
                    .into(),
                );
            }
        });
    }

    /// Drains the Worker pool's results. Thumbnails go into `loader.rs`'s
    /// cache; loupe results are set aside for `poll_web_preview` and
    /// `poll_web_full`. Returns the `(path, max_px)` keys that arrived, for
    /// burst and duplicate scoring.
    pub(crate) fn poll_web_thumbs(&mut self) -> Vec<(PathBuf, u32)> {
        let mut arrived = Vec::new();
        let pending = std::mem::take(&mut self.web_thumb_recovery_pending);
        for r in pending.into_iter().chain(self.web_worker_pool.poll()) {
            if r.kind == JobKind::Full {
                self.web_full_pending.push(r);
                continue;
            }
            if r.kind != JobKind::Thumb {
                self.web_preview_pending.push(r);
                continue;
            }
            // Browser paths start at the picked folder's name, so two picks of
            // same-named folders share paths. Drop results from before the
            // last pick so they cannot resolve the new folder's handles.
            if r.generation != Some(self.web_handle_generation) {
                continue;
            }
            let recover_source = r.needs_source_decode();
            // A corrupt cache entry needs a source read. With no read slot
            // free, park the result for next frame without using a retry.
            if recover_source && self.web_read_inflight.get() >= MAX_CONCURRENT_READS {
                self.web_thumb_recovery_pending.push(r);
                continue;
            }
            let crate::web_worker_pool::PoolResult {
                path,
                target,
                result,
                jpeg,
                cache_name,
                generation,
                ..
            } = r;
            let key = (path.clone(), target);
            // Re-decode from the source, keeping the key in flight and not
            // using a retry. With no file handle this falls through to the
            // failure arm, whose retry limit eventually marks it failed.
            if recover_source {
                if let Some(handle) = self.web_file_handles.get(&path).cloned() {
                    let pool = self.web_worker_pool.handle();
                    let read_inflight = self.web_read_inflight.clone();
                    read_inflight.set(read_inflight.get() + 1);
                    wasm_bindgen_futures::spawn_local(async move {
                        let source = web_fs::read_array_buffer(&handle).await;
                        read_inflight.set(read_inflight.get().saturating_sub(1));
                        match source {
                            Ok(bytes) => {
                                let is_raw = crate::image_decode::is_raw_extension(&path);
                                pool.submit_thumb(
                                    path,
                                    target,
                                    bytes,
                                    is_raw,
                                    cache_name,
                                    generation.expect("validated thumbnail generation"),
                                    false,
                                );
                            }
                            Err(e) => pool.fail(path, target, JobKind::Thumb, generation, e),
                        }
                    });
                    continue;
                }
            }
            self.web_thumb_inflight.remove(&key);
            // Ignore results for files deleted while they decoded.
            if !self
                .playlist
                .as_ref()
                .is_some_and(|playlist| playlist.entries().contains(&path))
                || !self.web_file_handles.contains_key(&path)
            {
                continue;
            }
            match result {
                Ok(img) => {
                    self.web_thumb_retries.remove(&key);
                    // `jpeg` is `Some` only on a cache miss.
                    if let (Some(bytes), Some(name)) = (jpeg, cache_name) {
                        self.store_web_thumb(&path, name, bytes);
                    }
                    if let Some(loader) = &mut self.loader {
                        loader.insert_thumb_external(
                            path.clone(),
                            target,
                            std::sync::Arc::new(img),
                        );
                    }
                    arrived.push((path, target));
                }
                Err(e) => {
                    // Log with `web_sys::console`: `eprintln!` goes nowhere on
                    // wasm32.
                    let entry = self
                        .web_thumb_retries
                        .entry(key)
                        .or_insert((0, Instant::now()));
                    entry.0 += 1;
                    let retries = entry.0;
                    if retries <= MAX_READ_RETRIES {
                        // `request_web_thumbs` retries once this deadline
                        // passes.
                        entry.1 = Instant::now() + retry_backoff(retries);
                        web_sys::console::warn_1(
                            &format!(
                                "[web] thumbnail decode failed for {} (retry {}/{MAX_READ_RETRIES}): {e}",
                                path.display(),
                                retries
                            )
                            .into(),
                        );
                    } else {
                        web_sys::console::error_1(
                            &format!(
                                "[web] thumbnail decode failed permanently for {}: {e}",
                                path.display()
                            )
                            .into(),
                        );
                        self.web_thumb_retries.remove(&(path.clone(), target));
                        crate::analytics::decode_failed(&path, "thumbnail");
                        if let Some(loader) = &mut self.loader {
                            loader.mark_thumb_failed_external(path.clone(), target);
                        }
                        arrived.push((path, target));
                    }
                }
            }
        }
        if !arrived.is_empty() {
            self.request_redraw();
        }
        arrived
    }

    /// The wasm32 version of `loader.request_preview`: reads the wanted photo
    /// and asks the Worker pool for a `Preview` decode at `preview_px()`.
    ///
    /// For RAW it also sends a fast `Speed` decode of the same bytes, unless
    /// something is already shown for this photo. Whichever lands first
    /// paints, and the `Preview` replaces a `Speed` result. Returns true if a
    /// read started.
    pub(crate) fn request_web_preview(&mut self) -> bool {
        let Some(path) = self.want.clone() else {
            return false;
        };
        let target = self.preview_px();
        let key = (path.clone(), target);
        let is_raw = crate::image_decode::is_raw_extension(&path);

        let already_have = self
            .loader
            .as_ref()
            .is_some_and(|l| l.get_full(&path).is_some() || l.get_preview(&path, target).is_some());
        let quality_needed = !already_have
            && !self.web_preview_inflight.contains(&key)
            && !self.web_preview_failed.contains(&key)
            && !self
                .web_preview_retries
                .get(&key)
                .is_some_and(|(_, retry_at)| Instant::now() < *retry_at);

        let speed_needed = is_raw
            && self.shown.path() != Some(path.as_path())
            && !self.web_speed_inflight.contains(&key)
            && !self.web_speed_failed.contains(&key)
            && !self
                .web_speed_retries
                .get(&key)
                .is_some_and(|(_, retry_at)| Instant::now() < *retry_at);

        if !quality_needed && !speed_needed {
            return false;
        }
        // Shares the read budget with thumbnails. Callers retry every frame.
        if self.web_read_inflight.get() >= MAX_CONCURRENT_READS {
            return false;
        }
        let Some(handle) = self.web_file_handles.get(&path).cloned() else {
            return false;
        };
        if quality_needed {
            self.web_preview_inflight.insert(key.clone());
        }
        if speed_needed {
            self.web_speed_inflight.insert(key.clone());
        }
        self.web_read_inflight.set(self.web_read_inflight.get() + 1);
        let read_inflight = self.web_read_inflight.clone();
        let pool = self.web_worker_pool.handle();
        wasm_bindgen_futures::spawn_local(async move {
            let result = web_fs::read_array_buffer(&handle).await;
            read_inflight.set(read_inflight.get().saturating_sub(1));
            match result {
                Ok(bytes) => {
                    if quality_needed && speed_needed {
                        // `submit` transfers the buffer, which detaches it, so
                        // the second worker needs its own copy.
                        let speed_bytes = bytes.slice(0);
                        pool.submit(path.clone(), target, speed_bytes, is_raw, JobKind::Speed);
                        pool.submit(path, target, bytes, is_raw, JobKind::Preview);
                    } else if quality_needed {
                        pool.submit(path, target, bytes, is_raw, JobKind::Preview);
                    } else {
                        pool.submit(path, target, bytes, is_raw, JobKind::Speed);
                    }
                }
                Err(e) => {
                    web_sys::console::error_1(
                        &format!("[web] reading bytes failed for {}: {e}", path.display()).into(),
                    );
                    if quality_needed {
                        pool.fail(path.clone(), target, JobKind::Preview, None, e.clone());
                    }
                    if speed_needed {
                        pool.fail(path, target, JobKind::Speed, None, e);
                    }
                }
            }
        });
        true
    }

    /// Handles the `Preview` and `Speed` results `poll_web_thumbs` set aside.
    /// `Preview` goes into `loader.rs`'s preview cache. `Speed` skips the
    /// cache and is uploaded only if nothing is shown for the photo yet,
    /// because it can land after the sharper `Preview`. Keys that run out of
    /// retries go in `web_preview_failed` or `web_speed_failed`.
    pub(crate) fn poll_web_preview(&mut self) -> bool {
        let mut landed = false;
        let pending = std::mem::take(&mut self.web_preview_pending);
        for crate::web_worker_pool::PoolResult {
            kind,
            path,
            target,
            result,
            ..
        } in pending
        {
            let key = (path.clone(), target);
            match kind {
                JobKind::Speed => {
                    self.web_speed_inflight.remove(&key);
                    match result {
                        Ok(img) => {
                            self.web_speed_retries.remove(&key);
                            if self.want.as_deref() == Some(path.as_path())
                                && self.shown.path() != Some(path.as_path())
                            {
                                self.upload_shown(
                                    &path,
                                    &img,
                                    Shown::Preview(path.clone(), target, img.width.max(img.height)),
                                );
                                landed = true;
                            }
                        }
                        Err(e) => {
                            // No status toast: the `Preview` decode is still
                            // running independently.
                            let entry = self
                                .web_speed_retries
                                .entry(key.clone())
                                .or_insert((0, Instant::now()));
                            entry.0 += 1;
                            let retries = entry.0;
                            if retries <= MAX_READ_RETRIES {
                                entry.1 = Instant::now() + retry_backoff(retries);
                                web_sys::console::warn_1(
                                    &format!(
                                        "[web] speed decode failed for {} (retry {}/{MAX_READ_RETRIES}): {e}",
                                        path.display(),
                                        retries
                                    )
                                    .into(),
                                );
                            } else {
                                web_sys::console::warn_1(
                                    &format!(
                                        "[web] speed decode failed permanently for {}: {e}",
                                        path.display()
                                    )
                                    .into(),
                                );
                                self.web_speed_retries.remove(&key);
                                if self.want.as_deref() == Some(path.as_path()) {
                                    crate::analytics::decode_failed(&path, "speed");
                                }
                                self.web_speed_failed.insert(key);
                            }
                        }
                    }
                    continue;
                }
                // `Full` goes to `poll_web_full`, and exports come back on
                // `poll_exports`, so neither reaches here.
                JobKind::Full => continue,
                JobKind::Export => continue,
                JobKind::Preview | JobKind::Thumb => {}
            }
            self.web_preview_inflight.remove(&key);
            match result {
                Ok(img) => {
                    // EXIF reads never complete on wasm32, so take the source
                    // size from the decode and re-fit, as `on_exif_info` does
                    // natively.
                    let real_size = Some((img.width, img.height));
                    if self.want.as_deref() == Some(path.as_path()) && self.source_size != real_size
                    {
                        self.source_size = real_size;
                        if self.fitted {
                            self.fit_to_window();
                        }
                    }
                    if let Some(loader) = &mut self.loader {
                        loader.insert_preview_external(
                            path.clone(),
                            target,
                            std::sync::Arc::new(img),
                        );
                    }
                    self.web_preview_retries.remove(&key);
                    landed = true;
                }
                Err(e) => {
                    let entry = self
                        .web_preview_retries
                        .entry(key.clone())
                        .or_insert((0, Instant::now()));
                    entry.0 += 1;
                    let retries = entry.0;
                    if retries <= MAX_READ_RETRIES {
                        entry.1 = Instant::now() + retry_backoff(retries);
                        web_sys::console::warn_1(
                            &format!(
                                "[web] preview decode failed for {} (retry {}/{MAX_READ_RETRIES}): {e}",
                                path.display(),
                                retries
                            )
                            .into(),
                        );
                    } else {
                        web_sys::console::error_1(
                            &format!(
                                "[web] preview decode failed permanently for {}: {e}",
                                path.display()
                            )
                            .into(),
                        );
                        self.web_preview_retries.remove(&key);
                        if self.want.as_deref() == Some(path.as_path()) {
                            crate::analytics::decode_failed(&path, "preview");
                        }
                        self.web_preview_failed.insert(key);
                        self.set_status((crate::i18n::t().preview_failed)(
                            &path.display().to_string(),
                        ));
                    }
                }
            }
        }
        if landed {
            self.try_show();
            self.request_redraw();
        }
        landed
    }

    /// The wasm32 version of `loader.request_full`: reads the wanted photo and
    /// asks the Worker pool for a full decode at the GPU's max texture size.
    /// Returns true if a read started.
    pub(crate) fn request_web_full(&mut self) -> bool {
        // Called every frame, so it needs the same zoom check as
        // `ensure_full_for_zoom`, or every opened photo gets a full decode.
        if !self.full_wanted_for_zoom() {
            return false;
        }
        let Some(path) = self.want.clone() else {
            return false;
        };
        let Some(target) = self.renderer.as_ref().map(|r| r.max_dim) else {
            return false;
        };
        let key = (path.clone(), target);
        let already_have = self
            .loader
            .as_ref()
            .is_some_and(|l| l.get_full(&path).is_some());
        if already_have
            || self.web_full_inflight.contains(&key)
            || self.web_full_failed.contains(&key)
            || self
                .web_full_retries
                .get(&key)
                .is_some_and(|(_, retry_at)| Instant::now() < *retry_at)
        {
            return false;
        }
        if self.web_read_inflight.get() >= MAX_CONCURRENT_READS {
            return false;
        }
        let Some(handle) = self.web_file_handles.get(&path).cloned() else {
            return false;
        };
        self.web_full_inflight.insert(key);
        self.web_read_inflight.set(self.web_read_inflight.get() + 1);
        let read_inflight = self.web_read_inflight.clone();
        let pool = self.web_worker_pool.handle();
        wasm_bindgen_futures::spawn_local(async move {
            let is_raw = crate::image_decode::is_raw_extension(&path);
            let result = web_fs::read_array_buffer(&handle).await;
            read_inflight.set(read_inflight.get().saturating_sub(1));
            match result {
                Ok(bytes) => pool.submit(path, target, bytes, is_raw, JobKind::Full),
                Err(e) => {
                    web_sys::console::error_1(
                        &format!("[web] reading bytes failed for {}: {e}", path.display()).into(),
                    );
                    pool.fail(path, target, JobKind::Full, None, e);
                }
            }
        });
        true
    }

    /// Moves the `Full` results `poll_web_thumbs` set aside into `loader.rs`'s
    /// full-resolution cache, where `try_show` picks them up.
    pub(crate) fn poll_web_full(&mut self) -> bool {
        let mut landed = false;
        let pending = std::mem::take(&mut self.web_full_pending);
        for crate::web_worker_pool::PoolResult {
            path,
            target,
            result,
            ..
        } in pending
        {
            let key = (path.clone(), target);
            self.web_full_inflight.remove(&key);
            match result {
                Ok(img) => {
                    self.web_full_retries.remove(&key);
                    // `source_size` came from the preview, which is capped at
                    // `preview_px()`, so zoom and "100%" were using preview
                    // pixels. The full decode has the true size for any image
                    // that fits in a texture.
                    let real_size = Some((img.width, img.height));
                    if self.want.as_deref() == Some(path.as_path()) && self.source_size != real_size
                    {
                        self.source_size = real_size;
                        if self.fitted {
                            self.fit_to_window();
                        }
                    }
                    if let Some(loader) = &mut self.loader {
                        loader.insert_full_external(path, std::sync::Arc::new(img));
                    }
                    landed = true;
                }
                Err(e) => {
                    let entry = self
                        .web_full_retries
                        .entry(key.clone())
                        .or_insert((0, Instant::now()));
                    entry.0 += 1;
                    let retries = entry.0;
                    if retries <= MAX_READ_RETRIES {
                        entry.1 = Instant::now() + retry_backoff(retries);
                        web_sys::console::warn_1(
                            &format!(
                                "[web] full-resolution decode failed for {} (retry {}/{MAX_READ_RETRIES}): {e}",
                                path.display(),
                                retries
                            )
                            .into(),
                        );
                    } else {
                        web_sys::console::error_1(
                            &format!(
                                "[web] full-resolution decode failed permanently for {}: {e}",
                                path.display()
                            )
                            .into(),
                        );
                        self.web_full_retries.remove(&key);
                        if self.want.as_deref() == Some(path.as_path()) {
                            crate::analytics::decode_failed(&path, "full");
                        }
                        self.web_full_failed.insert(key);
                    }
                }
            }
        }
        if landed {
            self.try_show();
            self.request_redraw();
        }
        landed
    }

    /// True while any Web Worker decode is outstanding. The event loop keeps
    /// polling while this is true, because a worker's `postMessage` reply
    /// does not wake winit. `loader.has_pending_image` is always false on
    /// wasm32.
    pub(crate) fn web_decode_pending(&self) -> bool {
        !self.web_thumb_inflight.is_empty()
            || !self.web_preview_inflight.is_empty()
            || !self.web_speed_inflight.is_empty()
            || !self.web_full_inflight.is_empty()
    }

    /// True while the wanted RAW has nothing to show yet. Stays true during
    /// backoff and after permanent failure, so the previous photo does not
    /// show through.
    pub(crate) fn loupe_is_loading(&self) -> bool {
        let Some(path) = self.want.clone() else {
            return false;
        };
        if !crate::image_decode::is_raw_extension(&path) {
            return false;
        }
        let target = self.preview_px();
        let key = (path.clone(), target);
        // A landed `Speed` result is only visible through `shown`, because it
        // skips `loader.rs`'s cache.
        let has_preview = self.shown.path() == Some(path.as_path())
            || self.loader.as_ref().is_some_and(|loader| {
                loader.get_full(&path).is_some() || loader.get_preview(&path, target).is_some()
            });
        !has_preview
            && (self.web_preview_inflight.contains(&key)
                || self.web_preview_retries.contains_key(&key)
                || self.web_preview_failed.contains(&key)
                || self.web_file_handles.contains_key(&path))
    }

    /// Starts listing `dir` unless it is cached in `subdirs` or already in
    /// flight. `poll_dir_listing` receives the result. A missing handle is
    /// treated as an empty folder.
    pub(crate) fn request_dir_listing(&mut self, dir: &Path) {
        let generation = self.web_nav_generation;
        let dir = dir.to_path_buf();
        if self.subdirs.contains_key(&dir)
            || self
                .web_dirlist_inflight
                .contains(&(dir.clone(), generation))
        {
            return;
        }
        let Some(handle) = self.web_dir_handles.get(&dir).cloned() else {
            web_sys::console::error_1(
                &format!("[web] no directory handle for {}", dir.display()).into(),
            );
            self.subdirs.insert(dir.clone(), Vec::new());
            self.set_status((crate::i18n::t().folder_handle_missing)(
                &dir.display().to_string(),
            ));
            // No result will arrive, so a navigation waiting on this listing
            // would never run. Cancel pending navigation.
            self.supersede_web_pending_nav();
            return;
        };
        self.web_dirlist_inflight.insert((dir.clone(), generation));
        let base = dir;
        let tx = self.web_dirlist_tx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = web_fs::list_dir(&base, &handle).await;
            let _ = tx.send((generation, base, result));
        });
        self.request_redraw();
    }

    /// Merges finished folder listings into the handle maps and `subdirs`, then
    /// runs any navigation that was waiting on that folder. Returns true while
    /// a listing is still in flight.
    pub(crate) fn poll_dir_listing(&mut self) -> bool {
        while let Ok((generation, dir, result)) = self.web_dirlist_rx.try_recv() {
            self.web_dirlist_inflight.remove(&(dir.clone(), generation));

            // Superseded by a newer navigation.
            if generation != self.web_nav_generation {
                continue;
            }

            let listing_succeeded = result.is_ok();
            match result {
                Ok(listing) => {
                    let mut subdir_paths = Vec::with_capacity(listing.subdirs.len());
                    for (path, handle) in listing.subdirs {
                        subdir_paths.push(path.clone());
                        self.web_dir_handles.insert(path, handle);
                    }
                    self.fetch_cjk_font_for(
                        listing.images.iter().map(|(p, _)| p).chain(&subdir_paths),
                    );
                    for (path, handle) in listing.images {
                        self.web_file_handles.insert(path, handle);
                    }
                    self.subdirs.insert(dir.clone(), subdir_paths);
                }
                Err(e) => {
                    self.set_status((crate::i18n::t().list_folder_failed)(
                        &dir.display().to_string(),
                        &e.to_string(),
                    ));
                    // Not cached, so navigating here again retries.
                }
            }

            if self.web_pending_nav_generation != self.web_nav_generation {
                self.web_pending_nav = None;
            }
            match self.web_pending_nav.clone() {
                Some(WebPendingNav::Open(p)) if p == dir => {
                    self.web_pending_nav = None;
                    if listing_succeeded {
                        self.apply_web_open_folder(p);
                    }
                }
                Some(WebPendingNav::Load(p)) if p == dir => {
                    self.web_pending_nav = None;
                    if listing_succeeded {
                        self.apply_web_load_folder(p);
                    }
                }
                Some(WebPendingNav::LoadAfterOpen(p)) if p == dir => {
                    self.web_pending_nav = None;
                    if listing_succeeded {
                        self.apply_web_load_folder(p);
                        self.mode = ViewMode::Grid;
                        self.update_window_title();
                        self.normalize_focus();
                    }
                }
                _ => {}
            }
            self.request_redraw();
        }
        !self.web_dirlist_inflight.is_empty()
    }

    /// Fetch the full Chinese font the first time a listed name has Chinese
    /// characters the loaded fonts can't draw. Every name the UI shows reaches
    /// it through a listing first.
    fn fetch_cjk_font_for<'a>(&mut self, paths: impl IntoIterator<Item = &'a PathBuf>) {
        if self.web_full_cjk_requested {
            return;
        }
        let font = egui::FontId::proportional(14.0);
        let missing = self.egui_ctx.fonts_mut(|fonts| {
            paths.into_iter().any(|path| {
                path.to_string_lossy()
                    .chars()
                    .any(|c| super::fonts::is_cjk(c) && !fonts.has_glyph(&font, c))
            })
        });
        if missing {
            self.web_full_cjk_requested = true;
            super::fonts::fetch_full_cjk(self.egui_ctx.clone(), self.window.clone());
        }
    }

    /// The wasm32 end of `open_folder`: toggles the folder's expansion, and if
    /// it has subfolders but no photos, opens its first subfolder instead.
    /// Expects `dir`'s listing to be in `subdirs`.
    pub(crate) fn apply_web_open_folder(&mut self, dir: PathBuf) {
        let subdirs = self.subdirs.get(&dir).cloned().unwrap_or_default();
        let has_own_images = self
            .web_file_handles
            .keys()
            .any(|p| p.parent() == Some(dir.as_path()));
        let pure_container = !subdirs.is_empty() && !has_own_images;

        if !subdirs.is_empty() {
            if self.expanded.contains(&dir) {
                if !pure_container {
                    self.expanded.remove(&dir);
                }
            } else {
                self.expanded.insert(dir.clone());
            }
        }

        let target = if pure_container {
            subdirs.into_iter().next().unwrap_or_else(|| dir.clone())
        } else {
            dir.clone()
        };

        if target != dir && !self.subdirs.contains_key(&target) {
            // Once the child is listed, only load it. `Open` would also toggle
            // the child and could skip down another level.
            self.defer_web_nav(WebPendingNav::LoadAfterOpen(target.clone()));
            self.request_dir_listing(&target);
            self.request_redraw();
            return;
        }

        self.apply_web_load_folder(target);
        self.mode = ViewMode::Grid;
        self.update_window_title();
        self.normalize_focus();
    }

    /// The wasm32 version of `load_folder`: shows `dir`'s photos in the grid
    /// without changing expansion. Lists `dir` first if needed.
    pub(crate) fn apply_web_load_folder(&mut self, dir: PathBuf) {
        if !self.subdirs.contains_key(&dir) {
            self.defer_web_nav(WebPendingNav::Load(dir.clone()));
            self.request_dir_listing(&dir);
            self.request_redraw();
            return;
        }
        // Must precede `load_playlist`. Set even when `None`, so sidecar
        // writes never go to the previous folder.
        let dir_handle = self.web_dir_handles.get(&dir).cloned();
        self.catalog.set_wasm_dir_handle(dir_handle);
        let mut entries: Vec<PathBuf> = self
            .web_file_handles
            .keys()
            .filter(|p| p.parent() == Some(dir.as_path()))
            .cloned()
            .collect();
        crate::navigation::sort_by_name(&mut entries);
        let playlist = crate::navigation::Playlist::from_entries(dir.clone(), entries);
        self.load_playlist(playlist, dir);
        self.request_redraw();
    }
}
