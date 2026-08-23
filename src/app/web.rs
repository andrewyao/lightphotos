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
use crate::image_decode::DecodedImage;
use crate::navigation::Playlist;
use crate::web_fs;

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
    /// same working-set computation (`working_thumb_keys`), but spawns an
    /// async decode task per missing thumbnail instead of enqueueing to
    /// `loader.rs`'s worker queue, since nothing services that queue here
    /// yet (see `Loader::insert_thumb_external`'s doc comment). Runs
    /// synchronously on the main thread inside each spawned task — a real,
    /// known cost (no Web Worker pool until the wasm port plan's M4), not
    /// hidden: this is a correctness-first pass, not the performance-tuned
    /// path the app's actual goal calls for.
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
            let tx = self.web_thumb_tx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = decode_thumbnail(&path, &handle, px).await;
                let _ = tx.send((path, px, result));
            });
        }
        any_missing
    }

    /// Drain finished thumbnail decodes into `loader.rs`'s cache (via
    /// `insert_thumb_external`/`mark_thumb_failed_external`), same
    /// convention as every other one-shot poll in this codebase. Returns the
    /// arrived `(path, max_px)` keys — `main.rs` folds them into the same
    /// `score_arrived_thumbs`/redraw handling native's thumbnail arrivals
    /// already get, so burst/duplicate scoring works identically regardless
    /// of which path decoded the thumbnail.
    pub(crate) fn poll_web_thumbs(&mut self) -> Vec<(PathBuf, u32)> {
        let mut arrived = Vec::new();
        while let Ok((path, px, result)) = self.web_thumb_rx.try_recv() {
            self.web_thumb_inflight.remove(&(path.clone(), px));
            if let Some(loader) = &mut self.loader {
                match result {
                    Ok(img) => loader.insert_thumb_external(path.clone(), px, std::sync::Arc::new(img)),
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
                        loader.mark_thumb_failed_external(path.clone(), px);
                    }
                }
            }
            arrived.push((path, px));
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
        let tx = self.web_preview_tx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = decode_thumbnail(&path, &handle, target).await;
            let _ = tx.send((path, target, result));
        });
        true
    }

    /// Drain finished Loupe decodes — same shape as `poll_web_thumbs`, into
    /// the preview tier instead of the thumbnail tier. Negative-caches a
    /// failure locally (`web_preview_failed`) rather than via `loader.rs`
    /// (whose failure tracking is thumbnail-specific) so `try_show`'s
    /// every-frame re-request doesn't retry a doomed RAW decode forever.
    pub(crate) fn poll_web_preview(&mut self) -> bool {
        let mut landed = false;
        while let Ok((path, target, result)) = self.web_preview_rx.try_recv() {
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

/// Read `handle`'s bytes and decode a thumbnail at (approximately, longest
/// side) `max_px`. Tries the embedded EXIF preview first
/// (`thumbnail::embedded_preview_from_bytes` — cheap, no full decode), then
/// falls back to a full decode + `Lanczos3` resize
/// (`image_decode::decode_jpeg_png_tiff_from_bytes`) — both are the exact
/// same shared functions native's own non-mac `thumbnail()`/`decode()` use,
/// bytes-based instead of path-based. Deliberately *not* reimplemented here:
/// an earlier version of this function called `image::load_from_memory` +
/// `.thumbnail()` directly, which both skipped the embedded-preview
/// fast path entirely (confirmed slow against a real folder) and used a
/// fast/low-quality resize filter instead of `Lanczos3` (confirmed
/// visibly worse resolution) — two real, separate bugs from not reusing
/// native's already-correct logic.
async fn decode_thumbnail(
    path: &Path,
    handle: &web_sys::FileSystemFileHandle,
    max_px: u32,
) -> Result<DecodedImage, String> {
    let bytes = web_fs::read_bytes(handle).await?;
    // RAW (ARW/CR2/NEF/DNG/...): the fast quarter-res preview path
    // (raw_fast_preview.rs), ported from the earlier wasm decode spike —
    // the wasm port plan's M3 chose this over full PPG demosaic
    // (image_decode::decode's RAW branch) specifically because PPG measured
    // 4-5x *slower* than native, failing the port's whole performance goal;
    // the fast path measured ~6.3x faster than PPG in that same spike.
    if crate::image_decode::is_raw_extension(path) {
        return crate::raw_fast_preview::decode_raw_fast_from_bytes(&bytes, max_px);
    }
    if let Some(preview) = crate::thumbnail::embedded_preview_from_bytes(&bytes, max_px) {
        return Ok(preview);
    }
    crate::image_decode::decode_jpeg_png_tiff_from_bytes(&bytes, max_px)
}
