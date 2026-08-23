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
            let Some(handle) = self.web_file_handles.get(&path).cloned() else {
                continue; // shouldn't happen — every playlist entry came from a handle
            };
            self.web_thumb_inflight.insert(key);
            any_missing = true;
            let pool = self.web_worker_pool.handle();
            wasm_bindgen_futures::spawn_local(async move {
                let is_raw = crate::image_decode::is_raw_extension(&path);
                match web_fs::read_bytes(&handle).await {
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
            self.web_thumb_inflight.remove(&(path.clone(), target));
            if let Some(loader) = &mut self.loader {
                match result {
                    Ok(img) => loader.insert_thumb_external(path.clone(), target, std::sync::Arc::new(img)),
                    Err(e) => {
                        // eprintln! goes nowhere on bare wasm32 — no console
                        // is attached to Rust's stdio there by default, only
                        // real panics get surfaced (via
                        // console_error_panic_hook). web_sys::console::error_1
                        // is the actual way to reach DevTools.
                        web_sys::console::error_1(
                            &format!("[web] thumbnail decode failed for {}: {e}", path.display())
                                .into(),
                        );
                        loader.mark_thumb_failed_external(path.clone(), target);
                    }
                }
            }
            arrived.push((path, target));
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
        let Some(handle) = self.web_file_handles.get(&path).cloned() else {
            return false;
        };
        self.web_preview_inflight.insert(key);
        let pool = self.web_worker_pool.handle();
        wasm_bindgen_futures::spawn_local(async move {
            let is_raw = crate::image_decode::is_raw_extension(&path);
            match web_fs::read_bytes(&handle).await {
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
                    landed = true;
                }
                Err(e) => {
                    web_sys::console::error_1(
                        &format!("[web] preview decode failed for {}: {e}", path.display()).into(),
                    );
                    self.web_preview_failed.insert(key);
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

