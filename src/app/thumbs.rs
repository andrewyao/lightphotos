use super::*;
use std::path::{Path, PathBuf};
use std::time::SystemTime;


use crate::develop::{self};
use crate::sharpness;
use crate::{image_decode, image_ops};

impl App {

    pub(crate) fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Show the wanted image: prefer the full-resolution decode, but fall back
    /// to the cached thumbnail as an instant placeholder while the full image is
    /// still decoding. Swaps thumbnail → full once the full image arrives.
    pub(crate) fn try_show(&mut self) {
        let Some(want) = self.want.clone() else {
            return;
        };

        // Full image ready → show it (unless it's already the shown full image).
        if let Some(img) = self.loader.as_ref().and_then(|l| l.get(&want)) {
            if !self.shown.is_full_of(&want) {
                self.upload_shown(&want, &img, true);
            }
            return;
        }

        // Full not ready: show the thumbnail placeholder if we aren't already
        // showing this image in some form.
        if self.shown.path() != Some(want.as_path()) {
            if let Some(thumb) = self
                .loader
                .as_ref()
                .and_then(|l| l.get_thumb(&want, self.thumb_px))
            {
                self.upload_shown(&want, &thumb, false);
            }
        }
    }

    /// Upload an image to the renderer as the currently-shown image and re-fit.
    pub(super) fn upload_shown(&mut self, path: &Path, img: &image_decode::DecodedImage, is_full: bool) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        renderer.set_image(img);
        self.shown = if is_full {
            Shown::Full(path.to_path_buf())
        } else {
            Shown::Thumb(path.to_path_buf())
        };
        // Rebuild the histogram sample from the newly-shown image, then mark the
        // histogram dirty so it's recomputed before the next draw.
        self.build_hist_sample(img);
        // Load this image's stored edits into the shader (or identity if none).
        self.push_adjustments();
        self.fit_to_window();
        self.update_window_title();
        self.request_redraw();
    }

    pub(super) fn update_window_title(&self) {
        let Some(w) = &self.window else { return };
        match self.mode {
            ViewMode::Loupe => {
                if let (Some(p), Some(_pl)) = (self.shown.path(), &self.playlist) {
                    let name = p
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let pos = self.sel.unwrap_or(0) + 1;
                    w.set_title(&format!("{}  ({}/{})", name, pos, self.visible.len()));
                }
            }
            ViewMode::Grid => {
                w.set_title(&format!("Grid  ({} photos)", self.visible.len()));
            }
        }
    }

    pub(super) fn image_size(&self) -> (f32, f32) {
        self.renderer
            .as_ref()
            .map(|r| (r.image_size.0 as f32, r.image_size.1 as f32))
            .filter(|(w, h)| *w > 0.0 && *h > 0.0)
            .unwrap_or((1.0, 1.0))
    }

    /// Rotation (in 90° CW steps) of the image currently shown.
    pub(super) fn current_rotation(&self) -> u8 {
        self.shown
            .path()
            .and_then(|p| self.rotations.get(p))
            .copied()
            .unwrap_or(0)
    }

    /// The range of visible positions whose thumbnails we keep loaded — the
    /// *working set*. Bounded so huge folders never load every image:
    /// - Grid: only the cells scrolled into view (`grid_range`) plus a few rows
    ///   of prefetch margin.
    /// - Loupe: the filmstrip neighbors around the selection.
    pub(super) fn working_positions(&self) -> std::ops::Range<usize> {
        let len = self.visible.len();
        if len == 0 {
            return 0..0;
        }
        match self.mode {
            ViewMode::Grid => {
                let margin = self.grid_cols.saturating_mul(3).max(1);
                let start = self.grid_range.0.saturating_sub(margin);
                let end = (self.grid_range.1 + margin).min(len);
                start..end.max(start)
            }
            ViewMode::Loupe => {
                // The filmstrip reports its scrolled-into-view range; load that
                // window plus a prefetch margin (the horizontal analogue of the
                // grid). Union with the selection so the current image's own
                // thumbnail always loads even before the strip reports a range.
                let margin = 8;
                let mut start = self.strip_range.0.saturating_sub(margin);
                let mut end = (self.strip_range.1 + margin).min(len);
                if let Some(s) = self.sel {
                    start = start.min(s);
                    end = end.max((s + 1).min(len));
                }
                start..end.max(start)
            }
        }
    }

    /// Paths (with thumb size) for the current working set.
    /// Signature of the persisted edits (tone + crop + manual rotation) for
    /// `path`. Folded into thumbnail-texture keys so an edit change re-bakes the
    /// thumbnail. `edits`/`rotations` are in-memory maps seeded at folder load.
    pub(super) fn edit_sig_for(&self, path: &Path) -> u64 {
        let adj = self.edits.get(path).copied().unwrap_or_default();
        let rot = self.rotations.get(path).copied().unwrap_or(0);
        develop::edit_signature_with_touchups(
            &adj,
            self.touchups.get(path).map(Vec::as_slice).unwrap_or(&[]),
            rot,
        )
    }

    pub(super) fn working_thumb_keys(&self) -> Vec<(PathBuf, u32, u64)> {
        let px = self.thumb_px;
        let Some(pl) = &self.playlist else {
            return Vec::new();
        };
        self.working_positions()
            .filter_map(|pos| self.visible.get(pos).copied())
            .filter_map(|i| pl.entry(i))
            .map(|p| {
                let sig = self.edit_sig_for(p);
                (p.to_path_buf(), px, sig)
            })
            .collect()
    }

    /// Request thumbnails for the working set. Returns true if any requested
    /// thumb is still missing (so the caller can keep redrawing until they
    /// arrive).
    pub(crate) fn request_working_thumbs(&mut self) -> bool {
        let px = self.thumb_px;
        let paths: Vec<PathBuf> = self
            .working_thumb_keys()
            .into_iter()
            .map(|(p, _, _)| p)
            .collect();

        let mut any_missing = false;
        if let Some(loader) = &mut self.loader {
            for p in &paths {
                // Skip thumbs whose decode permanently failed (deleted/corrupt) —
                // otherwise we'd re-request every frame and spin the redraw loop.
                if loader.get_thumb(p, px).is_none() && !loader.thumb_failed(p, px) {
                    loader.request_thumb(p.clone(), px);
                    any_missing = true;
                }
            }
        }
        any_missing
    }

    /// For burst members (size >= 2) not yet scored: score any whose thumbnail is
    /// already decoded, and request the rest. Bounds decode work to burst members
    /// (never singletons, never the whole folder) so each burst's winner is
    /// chosen from the full burst — not just the frames scrolled past. Once a path
    /// is scored it's never requested again (the score cache is the guard).
    /// Score/request thumbnails for unscored burst members, and report whether
    /// any burst background work is still outstanding. Returns true when either
    /// the capture-time scan is unfinished OR some burst member still lacks a
    /// score (excluding permanently-failed thumbs, which will never score). The
    /// event loop calls this each frame to keep polling until burst results
    /// converge — worker-thread completions don't wake the loop on their own.
    pub(crate) fn request_burst_thumbs(&mut self) -> bool {
        if !self.bursts_on {
            return false;
        }
        let px = self.thumb_px;

        // Capture-time scan still running → grouping not final yet; stay awake.
        let scan_pending = {
            let Some(pl) = &self.playlist else {
                return false;
            };
            pl.entries()
                .iter()
                .any(|p| !self.capture_times.contains_key(p))
        };

        // Unscored burst members, identified by the current marks.
        let members: Vec<PathBuf> = {
            let Some(pl) = &self.playlist else {
                return false;
            };
            pl.entries()
                .iter()
                .enumerate()
                .filter(|(idx, p)| {
                    matches!(self.burst_marks.get(*idx), Some(Some(_)))
                        && !self.sharpness.contains_key(*p)
                })
                .map(|(_, p)| p.clone())
                .collect()
        };

        // Score any member whose thumbnail is already decoded; (re-)request the
        // rest. Thumbnails are premultiplied RGBA8; opaque photos make
        // premultiplied == straight for the luma metric.
        let mut newly: Vec<(PathBuf, f64)> = Vec::new();
        let mut still_unscored = false;
        if let Some(loader) = &mut self.loader {
            for p in &members {
                if let Some(img) = loader.get_thumb(p, px) {
                    newly.push((
                        p.clone(),
                        sharpness::sharpness(&img.rgba, img.width, img.height),
                    ));
                } else if loader.thumb_failed(p, px) {
                    // Permanently failed — will never score; not counted as pending.
                } else {
                    loader.request_thumb(p.clone(), px);
                    still_unscored = true;
                }
            }
        }
        if !newly.is_empty() {
            for (p, s) in newly {
                self.sharpness.insert(p, s);
            }
            self.recompute_burst_marks();
        }
        scan_pending || still_unscored
    }

    /// Fold background capture-time reads into the cache, then refresh grouping +
    /// request thumbnails for the newly-identified burst members.
    pub(crate) fn on_capture_times(&mut self, times: Vec<(PathBuf, Option<SystemTime>)>) {
        for (path, t) in times {
            self.capture_times.insert(path, t);
        }
        if self.bursts_on {
            self.recompute_burst_marks();
            self.request_burst_thumbs();
        }
    }

    /// Fold background exif-metadata reads into the cache for the Loupe info panel.
    pub(crate) fn on_exif_info(&mut self, results: Vec<(PathBuf, image_decode::ImageMetadata)>) {
        for (path, meta) in results {
            self.exif_cache.insert(path, meta);
        }
    }

    /// When thumbnails arrive and bursts are on, compute + cache sharpness for any
    /// unscored burst member among them, then refresh the winners and redraw.
    pub(crate) fn score_arrived_thumbs(&mut self, arrivals: &[(PathBuf, u32)]) {
        if !self.bursts_on {
            return;
        }
        let px = self.thumb_px;
        let mut newly: Vec<(PathBuf, f64)> = Vec::new();
        if let Some(loader) = &self.loader {
            for (path, mpx) in arrivals {
                if *mpx != px || self.sharpness.contains_key(path) {
                    continue;
                }
                // Only score burst members (their entry index maps to a mark).
                let is_member = self
                    .playlist
                    .as_ref()
                    .and_then(|pl| pl.entries().iter().position(|e| e == path))
                    .map(|idx| matches!(self.burst_marks.get(idx), Some(Some(_))))
                    .unwrap_or(false);
                if !is_member {
                    continue;
                }
                if let Some(img) = loader.get_thumb(path, px) {
                    newly.push((
                        path.clone(),
                        sharpness::sharpness(&img.rgba, img.width, img.height),
                    ));
                }
            }
        }
        if !newly.is_empty() {
            for (p, s) in newly {
                self.sharpness.insert(p, s);
            }
            self.recompute_burst_marks();
            self.request_redraw();
        }
    }

    /// Sync `thumb_tex` with the loader's available thumbnails for the working
    /// set, uploading new ones as egui textures and dropping stale handles.
    pub(super) fn sync_thumb_textures(&mut self) {
        if self.playlist.is_none() {
            return;
        }
        // Only the working set — bounds GPU textures to what's on screen so a
        // folder with thousands of images can't exhaust memory.
        let wanted = self.working_thumb_keys();

        // Upload any wanted thumbnail that's decoded but not yet a texture, baking
        // the image's edits (crop + tone + rotation) into it first so the grid
        // matches the loupe. The loader/disk thumbnail stays RAW (so the loupe's
        // placeholder isn't double-edited); the bake happens only here.
        for key in &wanted {
            if self.thumb_tex.contains_key(key) {
                continue;
            }
            let Some(img) = self
                .loader
                .as_ref()
                .and_then(|l| l.get_thumb(&key.0, key.1))
            else {
                continue;
            };
            let adj = self.edits.get(&key.0).copied().unwrap_or_default();
            let touchups = self.touchups.get(&key.0).map(Vec::as_slice).unwrap_or(&[]);
            let rot = self.rotations.get(&key.0).copied().unwrap_or(0);
            let color = if adj.is_identity() && touchups.is_empty() && rot % 4 == 0 {
                // Fast path: no edits, so upload the raw thumbnail verbatim (also
                // avoids a needless sRGB round-trip through the tone pipeline).
                egui::ColorImage::from_rgba_premultiplied(
                    [img.width as usize, img.height as usize],
                    &img.rgba,
                )
            } else {
                let (w, h, rgba) = image_ops::bake_edited(&img, &adj, touchups, rot);
                // bake_edited yields opaque (alpha=255) pixels, so premultiplied
                // == straight; from_rgba_premultiplied is correct.
                egui::ColorImage::from_rgba_premultiplied([w as usize, h as usize], &rgba)
            };
            let name = format!("thumb:{}:{}:{:016x}", key.0.display(), key.1, key.2);
            let handle = self
                .egui_ctx
                .load_texture(name, color, egui::TextureOptions::LINEAR);
            self.thumb_tex.insert(key.clone(), handle);
        }

        // Drop handles outside the working set (frees GPU memory; egui-managed).
        // A stale edit signature isn't in `wanted`, so this also evicts the old
        // texture after an edit change.
        let keep: std::collections::HashSet<(PathBuf, u32, u64)> = wanted.into_iter().collect();
        self.thumb_tex.retain(|k, _| keep.contains(k));
    }
}
