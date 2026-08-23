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
    let cap_ms = 500u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(20)).min(10_000);
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
                    // Before `load_playlist` (below) triggers
                    // `seed_mirrors`/`request_catalog_load` — its wasm32 arm
                    // needs this handle to actually read `.lightphotos/*.xmp`
                    // back.
                    self.catalog.set_wasm_dir_handle(picked.dir_handle);
                    let playlist = Playlist::from_entries(picked.dir.clone(), picked.entries);
                    self.load_playlist(playlist, picked.dir);
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
            if r.kind != JobKind::Thumb {
                self.web_preview_pending.push(r);
                continue;
            }
            let crate::web_worker_pool::PoolResult { path, target, result, .. } = r;
            let key = (path.clone(), target);
            self.web_thumb_inflight.remove(&key);
            match result {
                Ok(img) => {
                    self.web_thumb_retries.remove(&key);
                    if let Some(loader) = &mut self.loader {
                        loader.insert_thumb_external(path.clone(), target, std::sync::Arc::new(img));
                    }
                    arrived.push((path, target));
                }
                Err(e) => {
                    // eprintln! goes nowhere on bare wasm32 — no console is
                    // attached to Rust's stdio there by default, only real
                    // panics get surfaced (via console_error_panic_hook).
                    // web_sys::console::error_1/warn_1 is the actual way to
                    // reach DevTools.
                    let entry = self.web_thumb_retries.entry(key).or_insert((0, Instant::now()));
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
    pub(crate) fn request_web_preview(&mut self) -> bool {
        let Some(path) = self.want.clone() else {
            return false;
        };
        let target = self.preview_px();
        let key = (path.clone(), target);
        let already_have = self.loader.as_ref().is_some_and(|l| {
            l.get_full(&path).is_some() || l.get_preview(&path, target).is_some()
        });
        if already_have
            || self.web_preview_inflight.contains(&key)
            || self.web_preview_failed.contains(&key)
        {
            return false;
        }
        // Backing off after a prior failure — see `retry_backoff`'s doc
        // comment.
        if self
            .web_preview_retries
            .get(&key)
            .is_some_and(|(_, retry_at)| Instant::now() < *retry_at)
        {
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
        self.web_preview_inflight.insert(key);
        self.web_read_inflight.set(self.web_read_inflight.get() + 1);
        let read_inflight = self.web_read_inflight.clone();
        let pool = self.web_worker_pool.handle();
        wasm_bindgen_futures::spawn_local(async move {
            let is_raw = crate::image_decode::is_raw_extension(&path);
            let result = web_fs::read_array_buffer(&handle).await;
            read_inflight.set(read_inflight.get().saturating_sub(1));
            match result {
                Ok(bytes) => pool.submit(path, target, bytes, is_raw, JobKind::Preview),
                Err(e) => {
                    web_sys::console::error_1(
                        &format!("[web] reading bytes failed for {}: {e}", path.display()).into(),
                    );
                    pool.fail(path, target, JobKind::Preview, e);
                }
            }
        });
        true
    }

    /// Process this frame's preview-tier results, already set aside by
    /// `poll_web_thumbs` (see `web_preview_pending`'s doc comment — both
    /// tiers share one Worker pool result channel). Negative-caches a
    /// failure locally (`web_preview_failed`) rather than via `loader.rs`
    /// (whose failure tracking is thumbnail-specific) so `try_show`'s
    /// every-frame re-request doesn't retry a doomed RAW decode forever.
    pub(crate) fn poll_web_preview(&mut self) -> bool {
        let mut landed = false;
        let pending = std::mem::take(&mut self.web_preview_pending);
        for crate::web_worker_pool::PoolResult { path, target, result, .. } in pending {
            let key = (path.clone(), target);
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
                    if self.want.as_deref() == Some(path.as_path()) && self.source_size != real_size {
                        self.source_size = real_size;
                        if self.fitted {
                            self.fit_to_window();
                        }
                    }
                    if let Some(loader) = &mut self.loader {
                        loader.insert_preview_external(path.clone(), target, std::sync::Arc::new(img));
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
}

