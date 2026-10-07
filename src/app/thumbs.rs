//! Which decode tier the Loupe shows (`try_show`), thumbnail textures for the
//! Grid and filmstrip (`sync_thumb_textures`), and the capture-time and face
//! signals the culling features read.

use super::*;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::develop::{self};
use crate::image_decode;
use crate::signalcache::Signal;
use crate::thumbnail::THUMB_PX;

impl App {
    pub(crate) fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Show `want` at the best tier available: full resolution, then the
    /// preview, then the thumbnail as a placeholder. Runs every frame.
    pub(crate) fn try_show(&mut self) {
        let Some(want) = self.want.clone() else {
            return;
        };
        let target = self.preview_px();

        if let Some(img) = self.loader.as_ref().and_then(|l| l.get_full(&want)) {
            if !self.shown.is_full_of(&want) {
                self.upload_shown(&want, &img, Shown::Full(want.clone()));
            }
            return;
        }

        // The size check lets the full-quality decode replace the Speed pass.
        // Never downgrade a full-resolution image already on screen.
        if let Some(img) = self
            .loader
            .as_ref()
            .and_then(|l| l.get_preview(&want, target))
        {
            let actual = img.width.max(img.height);
            if !self.shown.is_preview_of(&want, target, actual) && !self.shown.is_full_of(&want) {
                self.upload_shown(&want, &img, Shown::Preview(want.clone(), target, actual));
            }
            return;
        }

        // Re-request every frame: a resize can change the target and the LRU
        // can evict a landed preview. `request_preview` dedupes.
        //
        // On wasm32 `app/web.rs` decodes previews instead.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(loader) = &mut self.loader {
            loader.request_preview(want.clone(), target);
        }

        // Nothing sharper yet: show the thumbnail unless this photo is already
        // up in some tier.
        if self.shown.path() != Some(want.as_path()) {
            if let Some(thumb) = self
                .loader
                .as_ref()
                .and_then(|l| l.get_thumb(&want, THUMB_PX))
            {
                self.upload_shown(&want, &thumb, Shown::Thumb(want.clone()));
            }
        }
    }

    /// Upload `img` as the loupe image. `tier` records which decode it came from.
    pub(super) fn upload_shown(
        &mut self,
        path: &Path,
        img: &image_decode::DecodedImage,
        tier: Shown,
    ) {
        // Read before `self.shown` is reassigned.
        let same_photo = self.shown.path() == Some(path);

        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        renderer.set_image(img);
        self.shown = tier;
        self.build_hist_sample(img);
        self.push_adjustments();
        // A new photo starts fitted. A sharper tier of the same photo keeps a
        // manual zoom, since zooming is what fetched the full decode. While
        // still fitted it must re-fit: until `source_size` lands,
        // `image_size()` is the texture's size, and the old fit would land the
        // view zoomed into a corner.
        if same_photo {
            if self.fitted {
                self.fit_to_window();
            } else {
                self.push_transform();
            }
        } else {
            self.fit_to_window();
        }
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
                w.set_title(&(crate::i18n::t().grid_title)(self.visible.len()));
            }
        }
    }

    /// Image size in source pixels for all view math. Until the metadata
    /// lands, falls back to the texture's size. Every tier has the same aspect
    /// ratio, so a fit is already right; only absolute scales (100% zoom,
    /// touch-up radii) need the real size.
    pub(super) fn image_size(&self) -> (f32, f32) {
        self.source_size
            .map(|(w, h)| (w as f32, h as f32))
            .or_else(|| {
                self.renderer
                    .as_ref()
                    .map(|r| (r.image_size.0 as f32, r.image_size.1 as f32))
            })
            .filter(|(w, h)| *w > 0.0 && *h > 0.0)
            .unwrap_or((1.0, 1.0))
    }

    /// Rotation of the shown image, in 90° clockwise steps.
    pub(super) fn current_rotation(&self) -> u8 {
        self.shown
            .path()
            .and_then(|p| self.rotations.get(p))
            .copied()
            .unwrap_or(0)
    }

    /// Visible positions whose thumbnails stay loaded: the grid cells on screen
    /// plus three rows, or the filmstrip range plus a margin. Keeps huge
    /// folders from loading every image.
    fn working_positions(&self) -> std::ops::Range<usize> {
        let len = self.visible.len();
        if len == 0 {
            return 0..0;
        }
        match self.mode {
            ViewMode::Grid => grid_working_range(self.grid_range, self.grid_cols, len),
            ViewMode::Loupe => strip_working_range(self.strip_range, self.sel, len),
        }
    }

    /// `working_positions` in the order their thumbnails should load: the
    /// selection, then what is on screen, then the margin.
    fn working_positions_ordered(&self) -> Vec<usize> {
        let working = self.working_positions();
        match self.mode {
            ViewMode::Grid => load_order(working, self.grid_range, None),
            ViewMode::Loupe => load_order(working, self.strip_range, self.sel),
        }
    }

    /// Hash of `path`'s edits, rotation, and touch-ups. Part of the thumbnail
    /// texture key, so an edit re-bakes the thumbnail.
    fn edit_sig_for(&self, path: &Path) -> u64 {
        let adj = self.edits.get(path).copied().unwrap_or_default();
        let rot = self.rotations.get(path).copied().unwrap_or(0);
        develop::edit_signature_with_touchups(
            &adj,
            self.touchups.get(path).map(Vec::as_slice).unwrap_or(&[]),
            rot,
        )
    }

    /// Texture keys for the working set.
    #[hotpath::measure]
    pub(super) fn working_thumb_keys(&self) -> Vec<(PathBuf, u32, u64)> {
        let px = THUMB_PX;
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

    /// Request thumbnails for the working set, on-screen cells first. Returns
    /// true while any is missing, so the caller keeps polling.
    #[hotpath::measure]
    pub(crate) fn request_working_thumbs(&mut self) -> bool {
        let px = THUMB_PX;
        let Some(pl) = &self.playlist else {
            return false;
        };
        let paths: Vec<PathBuf> = self
            .working_positions_ordered()
            .into_iter()
            .filter_map(|pos| self.visible.get(pos).copied())
            .filter_map(|i| pl.entry(i))
            .map(Path::to_path_buf)
            .collect();

        // Auto Tone's window shares this cache, so it has to be counted in or
        // its thumbnails can be evicted before `poll_auto_tone` reads them.
        let reserved = paths.len() + self.autotone.window_len();
        let Some(loader) = &mut self.loader else {
            return false;
        };
        loader.set_thumb_working_set_size(reserved);
        loader.set_viewport_thumbs(&paths, px);
        // Failed decodes don't count, or the redraw loop would spin on them.
        paths
            .iter()
            .any(|p| loader.get_thumb(p, px).is_none() && !loader.thumb_failed(p, px))
    }

    pub(crate) fn on_capture_times(&mut self, times: Vec<(PathBuf, Option<SystemTime>)>) {
        for (path, t) in times {
            self.signals.record(&path, Signal::Capture(t));
            self.capture_times.insert(path, t);
        }
        if self.grid_sort == GridSort::Time {
            self.recompute_visible();
        }
        self.poll_bursts();
    }

    /// Cache metadata for the info panel. For the photo in the loupe, this is
    /// also where its true resolution arrives.
    pub(crate) fn on_exif_info(&mut self, results: Vec<(PathBuf, image_decode::ImageMetadata)>) {
        for (path, meta) in results {
            // Earlier fits used the texture's size, so re-fit.
            if self.want.as_deref() == Some(path.as_path()) && meta.source_size.is_some() {
                let changed = self.source_size != meta.source_size;
                self.source_size = meta.source_size;
                if changed && self.fitted {
                    self.fit_to_window();
                }
            }
            self.exif_cache.insert(path, meta);
        }
    }

    /// Upload decoded working-set thumbnails as egui textures and drop the rest.
    #[hotpath::measure]
    pub(super) fn sync_thumb_textures(&mut self) {
        if self.playlist.is_none() {
            return;
        }
        let wanted = self.working_thumb_keys();

        // Disjoint field borrows, so the loop can read the loader and the edit
        // mirrors while holding the renderer mutably. Nothing in it may call a
        // `self` method.
        let (Some(renderer), Some(loader)) = (self.renderer.as_mut(), self.loader.as_mut()) else {
            return;
        };
        let wanted_sig: HashMap<(PathBuf, u32), u64> = wanted
            .iter()
            .map(|(p, px, sig)| ((p.clone(), *px), *sig))
            .collect();

        // Bake edits into the texture so the grid matches the loupe. The
        // cached thumbnail stays unedited, since the loupe applies edits to its
        // placeholder in the shader. The bake runs on a decode worker; until it
        // lands, a cell keeps the texture it had, stale edit and all.
        for (path, px, sig) in &wanted {
            let key = (path.clone(), *px);
            if self.thumb_tex.get(&key).is_some_and(|t| t.sig == *sig) {
                continue;
            }
            let Some(img) = loader.get_thumb(path, *px) else {
                continue;
            };
            let adj = self.edits.get(path).copied().unwrap_or_default();
            let touchups = self.touchups.get(path).map(Vec::as_slice).unwrap_or(&[]);
            let rot = self.rotations.get(path).copied().unwrap_or(0);
            if adj.is_identity() && touchups.is_empty() && rot % 4 == 0 {
                if let Some(id) = renderer.upload_thumb(img.width, img.height, &img.rgba) {
                    let tex = ThumbTexture {
                        id,
                        width: img.width,
                        height: img.height,
                        sig: *sig,
                    };
                    if let Some(old) = self.thumb_tex.insert(key, tex) {
                        renderer.free_thumb(old.id);
                    }
                }
            } else {
                loader.request_bake(path, *px, *sig, img, adj, touchups, rot);
            }
        }

        // After the requests, so a bake that ran inline (no workers) shows
        // this frame. One the user has edited again since is dropped.
        for baked in loader.take_baked() {
            let key = (baked.path, baked.px);
            if wanted_sig.get(&key) != Some(&baked.sig) {
                continue;
            }
            // A quarter turn swaps the axes, so the size comes from the bake.
            if let Some(id) = renderer.upload_thumb(baked.width, baked.height, &baked.rgba) {
                let tex = ThumbTexture {
                    id,
                    width: baked.width,
                    height: baked.height,
                    sig: baked.sig,
                };
                if let Some(old) = self.thumb_tex.insert(key, tex) {
                    renderer.free_thumb(old.id);
                }
            }
        }
        loader.retain_bakes(|k| wanted_sig.contains_key(k));

        // The renderer holds the GPU side, so a dropped entry has to be
        // handed back.
        self.thumb_tex.retain(|k, tex| {
            if wanted_sig.contains_key(k) {
                return true;
            }
            renderer.free_thumb(tex.id);
            false
        });
    }
}

/// The grid's working set: `grid_range` plus three rows either side.
pub(crate) fn grid_working_range(
    grid_range: (usize, usize),
    cols: usize,
    len: usize,
) -> std::ops::Range<usize> {
    let margin = cols.saturating_mul(3).max(1);
    let start = grid_range.0.saturating_sub(margin);
    let end = (grid_range.1 + margin).min(len);
    start..end.max(start)
}

/// The filmstrip's working set: `strip_range` plus eight either side, widened
/// to include the selection so its thumbnail loads before the strip reports
/// a range.
pub(crate) fn strip_working_range(
    strip_range: (usize, usize),
    sel: Option<usize>,
    len: usize,
) -> std::ops::Range<usize> {
    let margin = 8;
    let mut start = strip_range.0.saturating_sub(margin);
    let mut end = (strip_range.1 + margin).min(len);
    if let Some(s) = sel {
        start = start.min(s);
        end = end.max((s + 1).min(len));
    }
    start..end.max(start)
}

/// `working` in the order its thumbnails should load: `sel` first, then
/// `visible` top to bottom, then the margin after it, then the margin before
/// it nearest first. Every position of `working` appears exactly once.
pub(crate) fn load_order(
    working: std::ops::Range<usize>,
    visible: (usize, usize),
    sel: Option<usize>,
) -> Vec<usize> {
    let start = visible.0.clamp(working.start, working.end);
    let on_screen = start..visible.1.clamp(start, working.end);
    let sel = sel.filter(|s| working.contains(s));
    let mut order = Vec::with_capacity(working.len());
    order.extend(sel);
    order.extend(on_screen.clone().filter(|p| Some(*p) != sel));
    order.extend((on_screen.end..working.end).filter(|p| Some(*p) != sel));
    order.extend(
        (working.start..on_screen.start)
            .rev()
            .filter(|p| Some(*p) != sel),
    );
    order
}

#[cfg(test)]
mod tests {
    use super::{grid_working_range, load_order, strip_working_range};
    use crate::loader::Loader;
    use std::path::PathBuf;

    fn sorted(mut v: Vec<usize>) -> Vec<usize> {
        v.sort_unstable();
        v
    }

    #[test]
    fn the_grid_loads_the_screen_first_then_the_rows_below_then_the_rows_above() {
        let working = grid_working_range((12, 42), 6, 100);
        assert_eq!(working, 0..60);
        let order = load_order(working.clone(), (12, 42), None);
        let mut expected: Vec<usize> = (12..42).collect();
        expected.extend(42..60);
        expected.extend((0..12).rev());
        assert_eq!(order, expected);
        assert_eq!(sorted(order), working.collect::<Vec<_>>());
    }

    #[test]
    fn the_strip_loads_the_selection_first_then_the_strip_then_the_margin() {
        let working = strip_working_range((20, 29), Some(24), 100);
        assert_eq!(working, 12..37);
        let order = load_order(working.clone(), (20, 29), Some(24));
        let mut expected = vec![24];
        expected.extend((20..29).filter(|p| *p != 24));
        expected.extend(29..37);
        expected.extend((12..20).rev());
        assert_eq!(order, expected);
        assert_eq!(sorted(order), working.collect::<Vec<_>>());
    }

    #[test]
    fn a_selection_outside_the_strip_range_still_comes_first_and_only_once() {
        // Right after a jump, before the strip has reported a range around it.
        let working = strip_working_range((20, 29), Some(50), 100);
        assert_eq!(working, 12..51);
        let order = load_order(working.clone(), (20, 29), Some(50));
        assert_eq!(order[0], 50);
        assert_eq!(order.iter().filter(|p| **p == 50).count(), 1);
        assert_eq!(sorted(order), working.collect::<Vec<_>>());
    }

    #[test]
    fn a_viewport_past_the_end_of_the_folder_is_clamped() {
        let working = grid_working_range((90, 120), 6, 100);
        assert_eq!(working, 72..100);
        assert_eq!(
            load_order(working, (90, 120), None),
            (90..100).chain((72..90).rev()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_stale_viewport_beyond_the_working_set_adds_no_positions_outside_it() {
        // A filter shrank the folder before the grid reported its new range.
        let working = grid_working_range((90, 120), 6, 40);
        assert_eq!(
            sorted(load_order(working.clone(), (90, 120), None)),
            working.collect::<Vec<_>>()
        );
        let working = strip_working_range((90, 99), Some(3), 40);
        assert_eq!(working, 3..40);
        let order = load_order(working.clone(), (90, 99), Some(3));
        assert_eq!(order[0], 3);
        assert_eq!(sorted(order), working.collect::<Vec<_>>());
    }

    #[test]
    fn flicking_the_strip_to_a_far_photo_drops_the_windows_passed_on_the_way() {
        let photos: Vec<PathBuf> = (0..600)
            .map(|i| PathBuf::from(format!("/p/{i}.jpg")))
            .collect();
        let request = |loader: &mut Loader, sel: usize| {
            let strip = (sel.saturating_sub(4), (sel + 5).min(photos.len()));
            let working = strip_working_range(strip, Some(sel), photos.len());
            let paths: Vec<PathBuf> = load_order(working, strip, Some(sel))
                .into_iter()
                .map(|p| photos[p].clone())
                .collect();
            loader.set_viewport_thumbs(&paths, 512);
        };
        let mut loader = Loader::queue_only_for_test();
        for sel in (100..400).step_by(20) {
            request(&mut loader, sel);
        }
        request(&mut loader, 550);
        let queued: Vec<PathBuf> = loader
            .queued_viewport_thumbs()
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(queued[0], photos[550]);
        assert_eq!(queued.len(), 25);
        assert!(queued.iter().all(|p| photos[538..563].contains(p)));
    }
}
