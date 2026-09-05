//! wasm32-only: File System Access folder picking. `ui::draw`'s landing page
//! (shown while `self.playlist.is_none()`) is the only caller of
//! `request_folder_pick`; `main.rs`'s `about_to_wait` polls
//! `poll_folder_pick` every frame, same one-shot-background-job shape as
//! `request_catalog_load`/`poll_catalog_load` (`app/catalog.rs`) — except
//! this one's "background job" is a browser Promise chain
//! (`wasm_bindgen_futures::spawn_local`), not a real OS thread, since a
//! directory picker and its listing are inherently async browser APIs, not
//! filesystem calls that merely happen to be slow.

use super::*;
use crate::navigation::Playlist;
use crate::web_fs;
use crate::web_worker_pool::JobKind;

/// Cap on concurrent `FileSystemFileHandle::get_file()` reads against the
/// picked folder — see `App::web_read_inflight`'s doc comment for why this
/// exists at all (a real Chrome `NotReadableError` hit once decode got fast
/// enough, via M4, to fire a whole grid page's worth of reads in one frame).
/// Picked to stay comfortably under whatever that limit actually is while
/// still keeping several reads genuinely concurrent; not derived from
/// `web_worker_pool::worker_count()` — the two caps address different
/// resources (open file reads vs. decode workers) and don't need to match.
/// Confirmed working (whole folder loads clean) at 2, but that was measured
/// *before* `wasm_worker.rs` started preferring RAW files' embedded JPEG
/// preview over the full `rawler` Bayer-demosaic decode for grid
/// thumbnails — each read+decode round trip is now much shorter, so the
/// window in which concurrent reads actually contend is smaller too, which
/// is the reasoning for trying a higher value again rather than assuming
/// the old ceiling still holds. Matches `web_worker_pool::worker_count()`'s
/// own cap for a tidy correspondence, not because the two caps need to be
/// equal. Drop back toward 2 if a full-folder load gets stuck again — the
/// jittered retry/backoff safety net stays either way, so a wrong guess
/// here costs speed, not correctness.
const MAX_CONCURRENT_READS: u32 = 4;

/// How many consecutive read/decode failures a key tolerates before
/// `poll_web_thumbs`/`poll_web_preview` give up on it permanently — see
/// `App::web_thumb_retries`'s doc comment for why a failure isn't treated as
/// permanent on the first attempt at all. Confirmed empirically that a
/// short linear backoff (5 retries, ~6s total window) still wasn't enough
/// headroom for every case — a different, still-small set of files failed
/// on each fresh reload of the same folder, meaning genuinely transient
/// resource pressure that just sometimes takes longer than a few seconds to
/// clear, not a specific "always these files" issue.
const MAX_READ_RETRIES: u8 = 10;

/// Backoff before retry number `attempt` (1-indexed) — "full jitter"
/// (AWS's own term for this exact pattern): a duration sampled *uniformly
/// at random* from `[0, cap]`, where `cap` grows exponentially per attempt
/// (500ms, 1s, 2s, 4s, 8s, 10s, 10s, ... capped at 10s). Confirmed via a
/// real console log: several keys that failed in the same frame (same
/// `MAX_CONCURRENT_READS` batch) retried at *identical* attempt counts and
/// visibly collided again on every subsequent retry — a livelock, not just
/// a resource limit. A non-jittered backoff (even a real, several-seconds-
/// long one — tried first, still got stuck) can't fix that: same input
/// (attempt number) always produces the same delay, so a synchronized batch
/// stays synchronized forever. Sampling the *whole* window uniformly (not
/// "base plus a little jitter") is what actually breaks the lockstep.
fn retry_backoff(attempt: u8) -> std::time::Duration {
    let cap_ms = 500u64
        .saturating_mul(1u64 << attempt.saturating_sub(1).min(20))
        .min(10_000);
    let jittered_ms = (js_sys::Math::random() * cap_ms as f64).max(50.0);
    std::time::Duration::from_millis(jittered_ms as u64)
}

impl App {
    /// Fire the browser's folder picker, unless one's already in flight.
    /// Called from the landing page's "Choose Folder" button
    /// (`UiAction::PickFolder`).
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

    /// Drain a finished pick. On success, wires the result into the same
    /// path `load_folder` uses natively (`load_playlist`) so everything
    /// downstream — ratings mirrors, working-set thumbnails, the Grid itself
    /// — behaves identically regardless of how the `Playlist` was built.
    /// Returns whether a pick is still outstanding, same convention as
    /// `request_working_thumbs`/`poll_catalog_load`.
    pub(crate) fn poll_folder_pick(&mut self) -> bool {
        if let Ok(result) = self.web_folder_rx.try_recv() {
            self.web_folder_pending = false;
            match result {
                Ok(picked) => {
                    self.web_file_handles = picked.handles;
                    self.web_dir_handles = picked.dir_handles;
                    self.subdirs.clear();

                    let root = picked.dir.clone();
                    // First level of the tree, derived from the seeded dir
                    // handles (their keys whose parent is the root).
                    let mut first_level: Vec<PathBuf> = self
                        .web_dir_handles
                        .keys()
                        .filter(|p| p.parent() == Some(root.as_path()))
                        .cloned()
                        .collect();
                    crate::navigation::sort_by_name(&mut first_level);
                    self.subdirs.insert(root.clone(), first_level);

                    // Before `load_playlist` triggers the catalog scan (its
                    // wasm arm needs this handle to read `.lightphotos/*.xmp`).
                    if let Some(h) = self.web_dir_handles.get(&root) {
                        self.catalog.set_wasm_dir_handle(h.clone());
                    }
                    let playlist = Playlist::from_entries(root.clone(), picked.entries);
                    self.load_playlist(playlist, root.clone());
                    // Expose the tree only after the handle-backed playlist
                    // is installed. Folder-row actions are wasm-routed below
                    // and must never fall through to native `load_folder`.
                    self.folder_root = Some(root.clone());
                    self.expanded = std::collections::HashSet::from([root]);
                    self.mode = ViewMode::Grid;
                }
                Err(e) => {
                    // A cancelled picker is the common case here, not a real
                    // error — still surfaced via the same status-toast path
                    // as everything else so a genuine permission/listing
                    // failure isn't silent.
                    self.set_status(format!("Couldn't open folder: {e}"));
                }
            }
            self.request_redraw();
        }
        self.web_folder_pending
    }

    /// The wasm32 counterpart of `request_working_thumbs` (`app/thumbs.rs`):
    /// same working-set computation (`working_thumb_keys`), but reads each
    /// missing thumbnail's bytes on the main thread (unavoidable — only the
    /// main thread holds the `FileSystemFileHandle`) and hands them to the
    /// Web Worker pool (`web_worker_pool.rs`, wasm port plan's M4) instead of
    /// decoding inline via `spawn_local` — multiple decodes now genuinely run
    /// in parallel, off the main thread, across the pool's workers.
    pub(crate) fn request_web_thumbs(&mut self) -> bool {
        let px = self.thumb_px;
        let keys: Vec<PathBuf> = self
            .working_thumb_keys()
            .into_iter()
            .map(|(p, _, _)| p)
            .collect();

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
            // Backing off after a prior failure (see `retry_backoff`'s doc
            // comment) — not yet time to try again.
            if self
                .web_thumb_retries
                .get(&key)
                .is_some_and(|(_, retry_at)| Instant::now() < *retry_at)
            {
                continue;
            }
            // Something is genuinely missing (counted above) even if the
            // read budget is exhausted this frame — leaving the key off
            // `web_thumb_inflight` means the same `working_thumb_keys()`
            // recomputation next frame naturally retries it once a slot
            // frees up (see `MAX_CONCURRENT_READS`'s doc comment).
            if self.web_read_inflight.get() >= MAX_CONCURRENT_READS {
                continue;
            }
            let Some(handle) = self.web_file_handles.get(&path).cloned() else {
                continue; // shouldn't happen — every playlist entry came from a handle
            };
            self.web_thumb_inflight.insert(key);
            self.web_read_inflight.set(self.web_read_inflight.get() + 1);
            let read_inflight = self.web_read_inflight.clone();
            let pool = self.web_worker_pool.handle();
            wasm_bindgen_futures::spawn_local(async move {
                let is_raw = crate::image_decode::is_raw_extension(&path);
                let result = web_fs::read_array_buffer(&handle).await;
                read_inflight.set(read_inflight.get().saturating_sub(1));
                match result {
                    Ok(bytes) => pool.submit(path, px, bytes, is_raw, JobKind::Thumb),
                    Err(e) => {
                        web_sys::console::error_1(
                            &format!("[web] reading bytes failed for {}: {e}", path.display())
                                .into(),
                        );
                        // No worker job was submitted, so nothing will ever
                        // land in `poll_web_thumbs` for this key — that
                        // would leave it "in flight" forever, permanently
                        // masking the thumbnail as pending. Route straight
                        // to the same negative-cache path a decode failure
                        // uses.
                        pool.fail(path, px, JobKind::Thumb, e);
                    }
                }
            });
        }
        any_missing
    }

    /// Drain finished thumbnail decodes (from the shared Worker pool result
    /// channel — see `web_preview_pending`'s doc comment for why preview-tier
    /// results are set aside here rather than processed) into `loader.rs`'s
    /// cache (via `insert_thumb_external`/`mark_thumb_failed_external`), same
    /// convention as every other one-shot poll in this codebase. Returns the
    /// arrived `(path, max_px)` keys — `main.rs` folds them into the same
    /// `score_arrived_thumbs`/redraw handling native's thumbnail arrivals
    /// already get, so burst/duplicate scoring works identically regardless
    /// of which path decoded the thumbnail.
    pub(crate) fn poll_web_thumbs(&mut self) -> Vec<(PathBuf, u32)> {
        let mut arrived = Vec::new();
        for r in self.web_worker_pool.poll() {
            if r.kind == JobKind::Full {
                self.web_full_pending.push(r);
                continue;
            }
            if r.kind != JobKind::Thumb {
                self.web_preview_pending.push(r);
                continue;
            }
            let crate::web_worker_pool::PoolResult {
                path,
                target,
                result,
                ..
            } = r;
            let key = (path.clone(), target);
            self.web_thumb_inflight.remove(&key);
            match result {
                Ok(img) => {
                    self.web_thumb_retries.remove(&key);
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
                    // eprintln! goes nowhere on bare wasm32 — no console is
                    // attached to Rust's stdio there by default, only real
                    // panics get surfaced (via console_error_panic_hook).
                    // web_sys::console::error_1/warn_1 is the actual way to
                    // reach DevTools.
                    let entry = self
                        .web_thumb_retries
                        .entry(key)
                        .or_insert((0, Instant::now()));
                    entry.0 += 1;
                    let retries = entry.0; // u8: Copy, avoids borrowing `entry` across the `entry.1 = ...` below
                    if retries <= MAX_READ_RETRIES {
                        // Left off `web_thumb_inflight` (already removed
                        // above) — `request_web_thumbs` will try it again
                        // once `retry_backoff`'s deadline (just set below)
                        // passes. Most read/decode failures seen in practice
                        // are a transient Chrome `NotReadableError`, not a
                        // genuinely bad file — see `web_thumb_retries`'s doc
                        // comment on why real backoff (not just a frame's
                        // worth of delay) turned out to matter.
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

    /// The wasm32 counterpart of `try_show`'s `loader.request_preview(...)`
    /// call: `try_show` already re-requests every frame until something
    /// lands (see `want`'s doc comment — no change needed there), but that
    /// request goes into the same unserviced worker queue thumbnails did
    /// before `request_web_thumbs` existed. This is that same fix for the
    /// Loupe tier — decode at `preview_px()` instead of `thumb_px`, feed
    /// into `insert_preview_external` instead of `insert_thumb_external`.
    ///
    /// For RAW files, also fires a `Speed` request alongside the real
    /// `Preview` (quality) one — same target, same file bytes (one read,
    /// `ArrayBuffer::slice(0)`'d for the second `submit` since a transferred
    /// buffer can't be sent twice), decoded via `decode()`'s fast branch.
    /// Whichever lands first paints; the other supersedes it through the
    /// same `upload_shown` (see its own doc comment for why that's now safe
    /// across a same-photo tier swap — this reinstates the two-pass split
    /// that was reverted before that fix existed). `speed_needed` stops
    /// firing once *something* is already shown for this photo — no point
    /// racing a fast pass behind whatever's already on screen.
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
        // Same shared read-concurrency budget `request_web_thumbs` respects
        // (see `MAX_CONCURRENT_READS`'s doc comment) — the Loupe's own
        // preview read competes with the Grid's thumbnail reads against the
        // same folder, so they share one counter, not independent caps.
        // `try_show` already re-requests every frame, so returning `false`
        // (nothing started) here just defers to a later frame, same as a
        // throttled thumbnail key.
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
                        // `.slice(0)` is a real byte copy, taken before
                        // either `submit` transfers the original out via
                        // `postMessage` — a transferred `ArrayBuffer` is
                        // detached, so the same object can't be handed to
                        // two workers.
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
                        pool.fail(path.clone(), target, JobKind::Preview, e.clone());
                    }
                    if speed_needed {
                        pool.fail(path, target, JobKind::Speed, e);
                    }
                }
            }
        });
        true
    }

    /// Process this frame's preview-tier results, already set aside by
    /// `poll_web_thumbs` (see `web_preview_pending`'s doc comment — every
    /// non-`Thumb` kind shares one Worker pool result channel, so this
    /// drains `Preview` *and* `Speed` results). Negative-caches a failure
    /// locally (`web_preview_failed`/`web_speed_failed`) rather than via
    /// `loader.rs` (whose failure tracking is thumbnail-specific) so
    /// `try_show`'s every-frame re-request doesn't retry a doomed RAW decode
    /// forever.
    ///
    /// `Speed` results bypass `loader.rs`'s cache entirely (see
    /// `App::web_speed_inflight`'s doc comment for why) and instead go
    /// straight to `upload_shown` — but only if nothing has been shown for
    /// this photo yet: a `Speed` result can land *after* the real `Preview`
    /// already painted (worker scheduling isn't ordered), and applying it
    /// then would downgrade a good frame back to a worse one.
    pub(crate) fn poll_web_preview(&mut self) -> bool {
        let mut landed = false;
        let pending = std::mem::take(&mut self.web_preview_pending);
        for crate::web_worker_pool::PoolResult {
            kind,
            path,
            target,
            result,
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
                                self.set_tier_debug(super::thumbs::TIER_DEBUG_GRAY_18, "SPEED"); // TEMPORARY DEBUG
                                landed = true;
                            }
                        }
                        Err(e) => {
                            // Same bounded-retry-with-backoff treatment as
                            // the `Preview`/`Thumb` arms below — a `Speed`
                            // failure isn't fatal (the real `Preview`
                            // request is still independently in flight), so
                            // this is silent (no `set_status`) beyond a
                            // console warning.
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
                                self.web_speed_failed.insert(key);
                            }
                        }
                    }
                    continue;
                }
                // `poll_web_thumbs` routes `Full` results straight to
                // `web_full_pending` before they ever reach here (see its
                // own routing) — `poll_web_full` is what drains those.
                JobKind::Full => continue,
                // Export results never reach the decode channel — they come
                // back on `poll_exports` (JPEG bytes, not a `DecodedImage`).
                JobKind::Export => continue,
                JobKind::Preview | JobKind::Thumb => {}
            }
            self.web_preview_inflight.remove(&key);
            match result {
                Ok(img) => {
                    // Same re-fit `on_exif_info` (app/thumbs.rs) does when a
                    // real EXIF metadata read lands: "any fit computed before
                    // this used the uploaded texture's size as a stand-in."
                    // EXIF reads never land on wasm32 (same unserviced
                    // loader.rs queue) — but the decoded image itself already
                    // carries the real dimensions, no separate metadata read
                    // needed to know them here.
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
                    // Same bounded-retry-with-backoff treatment as
                    // `poll_web_thumbs` — see `App::web_thumb_retries`'s doc
                    // comment for why a failure here isn't immediately
                    // permanent, and why a real delay (not just "next
                    // frame") between attempts turned out to matter.
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
                        self.web_preview_failed.insert(key);
                        self.set_status(format!(
                            "Unable to load Loupe preview for {}",
                            path.display()
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

    /// wasm32 counterpart of `app/loupe.rs::ensure_full_for_zoom`'s
    /// `loader.request_full(path)` call — that enqueues into `loader.rs`'s
    /// own worker queue, which has no live workers on wasm32, so it's a
    /// silent no-op there. Same shape as `request_web_preview`, at
    /// `renderer.max_dim` (the GPU's max texture size — "don't downscale",
    /// matching native's own `full_target`) instead of `preview_px()`, and
    /// always requesting quality (`JobKind::Full`, real PPG demosaic) since
    /// the whole point of this tier is "the user zoomed in far enough that
    /// the screen-fit preview isn't enough detail anymore."
    pub(crate) fn request_web_full(&mut self) -> bool {
        // `main.rs` polls this every frame (for retry/backoff), so it must
        // carry the same zoom gate `ensure_full_for_zoom` applies — without it,
        // opening any photo fitted schedules a full demosaic that normal
        // browsing never needs. See `full_wanted_for_zoom`.
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
                    pool.fail(path, target, JobKind::Full, e);
                }
            }
        });
        true
    }

    /// Drains `Full`-tier results `poll_web_thumbs` set aside. Lands via
    /// `loader.insert_full_external`, so `try_show`'s existing `get_full`
    /// branch (already the first thing it checks) picks it up unchanged —
    /// `upload_shown`'s `fitted` gate then does the right thing on its own:
    /// the user just zoomed to trigger this (`fitted == false`), so it's
    /// applied at the current zoom/pan, not re-fit.
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
                    // `poll_web_preview` set `source_size` from the *preview*
                    // decode, which is bounded to `preview_px()` — so for any
                    // photo larger than that, zoom math and "100%" have been
                    // running on preview pixels, not source pixels. The full
                    // tier decodes at the GPU's `max_texture_dimension_2d`
                    // (>= 8192 per the WebGPU spec) and — for RAW — ignores
                    // that cap entirely (full sensor res, see
                    // `raw/preview.rs`), so its own dimensions are the
                    // true source dimensions for every image the renderer can
                    // actually hold at full resolution. Correct `source_size`
                    // from them now. (An image longer than the max texture
                    // still can't be shown at a true 100% regardless.)
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

    /// Whether any wasm32 Web Worker decode job (Grid thumbnail, or Loupe
    /// Speed/Preview/Full) is still outstanding.
    ///
    /// `main.rs`'s `about_to_wait` uses this to decide whether to keep the
    /// winit event loop polling (`ControlFlow::WaitUntil`) or let it go
    /// idle (`ControlFlow::Wait`) — mirroring `catalog_load_pending`/
    /// `web_folder_pending`, which exist for the exact same reason: async
    /// wasm work whose completion doesn't wake winit on its own.
    ///
    /// Without this, `image_pending` only reads `loader.rs`'s own
    /// `has_pending_image()`, whose in-flight sets are never populated on
    /// wasm32 (`request_preview`/`request_full` are native-only — see
    /// `ensure_full_for_zoom`'s wasm32 branch, which calls
    /// `request_web_full` instead). The loop would then go idle the instant
    /// a zoom submits a `request_web_full` job and nothing else happens to
    /// be in flight, and the finished decode — posted back via
    /// `web_worker_pool.rs`'s `postMessage` handler, which never nudges
    /// winit — would sit undrained until an unrelated event (mouse move,
    /// resize) happened to wake the loop again.
    pub(crate) fn web_decode_pending(&self) -> bool {
        !self.web_thumb_inflight.is_empty()
            || !self.web_preview_inflight.is_empty()
            || !self.web_speed_inflight.is_empty()
            || !self.web_full_inflight.is_empty()
    }

    /// Whether the wanted RAW has no usable preview at the current target.
    /// This remains true during throttling/backoff and after permanent
    /// failure, preventing the previous photo from reappearing underneath.
    pub(crate) fn loupe_is_loading(&self) -> bool {
        let Some(path) = self.want.clone() else {
            return false;
        };
        if !crate::image_decode::is_raw_extension(&path) {
            return false;
        }
        let target = self.preview_px();
        let key = (path.clone(), target);
        // `self.shown.path() == Some(&path)` covers a landed `Speed` result:
        // it bypasses `loader.rs`'s cache entirely (see
        // `App::web_speed_inflight`'s doc comment), so `get_preview` alone
        // wouldn't see it.
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

    /// Kick off an async `web_fs::list_dir` for `dir` (a relative path)
    /// unless its listing is already cached in `self.subdirs` or a scan is
    /// already in flight. The result lands on `web_dirlist_rx`, drained by
    /// `poll_dir_listing`. A missing directory handle is logged and treated
    /// as an empty (leaf) listing — should not happen, since a folder only
    /// becomes reachable after its parent's listing produced its handle.
    pub(crate) fn request_dir_listing(&mut self, dir: &Path) {
        if self.subdirs.contains_key(dir) || self.web_dirlist_inflight.contains(dir) {
            return;
        }
        let Some(handle) = self.web_dir_handles.get(dir).cloned() else {
            web_sys::console::error_1(
                &format!("[web] no directory handle for {}", dir.display()).into(),
            );
            self.subdirs.insert(dir.to_path_buf(), Vec::new());
            self.set_status(format!(
                "Couldn't open {} — folder handle missing",
                dir.display()
            ));
            // No channel send happens on this arm, so poll_dir_listing would
            // never dispatch a nav that a caller stashed for this dir. Drop
            // the whole pending generation rather than leave any older nav
            // eligible for a later listing completion.
            self.supersede_web_pending_nav();
            return;
        };
        self.web_dirlist_inflight.insert(dir.to_path_buf());
        let base = dir.to_path_buf();
        let tx = self.web_dirlist_tx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = web_fs::list_dir(&base, &handle).await;
            let _ = tx.send((base, result));
        });
        self.request_redraw();
    }

    /// Drain finished subfolder listings. For each: merge image handles into
    /// `web_file_handles`, merge subdir handles into `web_dir_handles`, set
    /// `self.subdirs[dir]` to the subdir relative paths, clear the in-flight
    /// mark, and — if a deferred navigation was waiting on this exact
    /// directory — run its apply step. Returns whether any listing is still
    /// outstanding (feeds the poll-cadence calc in `main.rs`).
    pub(crate) fn poll_dir_listing(&mut self) -> bool {
        while let Ok((dir, result)) = self.web_dirlist_rx.try_recv() {
            self.web_dirlist_inflight.remove(&dir);
            let listing_succeeded = result.is_ok();
            match result {
                Ok(listing) => {
                    let mut subdir_paths = Vec::with_capacity(listing.subdirs.len());
                    for (path, handle) in listing.subdirs {
                        subdir_paths.push(path.clone());
                        self.web_dir_handles.insert(path, handle);
                    }
                    for (path, handle) in listing.images {
                        self.web_file_handles.insert(path, handle);
                    }
                    self.subdirs.insert(dir.clone(), subdir_paths);
                }
                Err(e) => {
                    self.set_status(format!("Couldn't list {}: {e}", dir.display()));
                    // Treat as a leaf so the tree stops retrying every frame.
                    self.subdirs.insert(dir.clone(), Vec::new());
                }
            }

            // Complete a navigation that was blocked on this listing.
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

    /// wasm counterpart of the tail of native `open_folder`: toggle the
    /// folder's expansion, and if it is a pure container (subdirs but no
    /// images of its own) skip straight to its first child. Assumes
    /// `dir`'s own listing is cached in `self.subdirs` (the caller in
    /// `open_folder` guarantees it; `poll_dir_listing` calls this only
    /// after inserting `dir`'s listing).
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

        // The pure-container target is a different folder; its own listing
        // may not be loaded yet.
        if target != dir && !self.subdirs.contains_key(&target) {
            // The original open already toggled `dir`. Once the child is
            // listed, only load it; applying `Open` would toggle the child
            // and could recursively skip another pure-container level.
            self.defer_web_nav(WebPendingNav::LoadAfterOpen(target.clone()));
            self.request_dir_listing(&target);
            self.request_redraw();
            return;
        }

        self.apply_web_load_folder(target);
    }

    /// wasm counterpart of native `load_folder`: rebuild the playlist from
    /// `dir`'s cached image handles and switch the grid to it, without
    /// touching expansion state. Deferred here by `nav_to_folder` /
    /// `apply_web_open_folder` (or re-entered by `poll_dir_listing`) once
    /// `dir`'s listing is cached — the guard below still re-checks and
    /// re-defers if it somehow isn't.
    pub(crate) fn apply_web_load_folder(&mut self, dir: PathBuf) {
        if !self.subdirs.contains_key(&dir) {
            self.defer_web_nav(WebPendingNav::Load(dir.clone()));
            self.request_dir_listing(&dir);
            self.request_redraw();
            return;
        }
        // Point sidecar I/O at THIS folder's .lightphotos/ before
        // load_playlist kicks off the catalog scan (request_catalog_load's
        // wasm arm reads catalog.wasm_dir_handle()). Matches native's
        // per-folder catalog switch.
        if let Some(h) = self.web_dir_handles.get(&dir) {
            self.catalog.set_wasm_dir_handle(h.clone());
        }
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
