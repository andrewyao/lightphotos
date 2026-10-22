//! Which decode tier the Loupe shows (`try_show`), thumbnail textures for the
//! Grid and filmstrip (`sync_thumb_textures`), and the capture-time and face
//! signals the culling features read.

use super::*;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::decode::image_decode;
use crate::develop::{self};
use crate::jobs::thumbnail::{Origin, THUMB_PX};
use crate::persist::signalcache::Signal;

/// How often the Grid sorts again while capture times or sort metadata are
/// still arriving.
const META_SORT_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

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

        #[cfg(not(target_arch = "wasm32"))]
        self.ensure_full_after_preview();

        if let Some(loader) = &mut self.loader {
            loader.touch(&want, target);
            if let Some(img) = loader.get_full(&want) {
                let origin = loader.full_origin(&want);
                if !self.shown.is_full_of(&want) {
                    self.upload_shown(&want, &img, Shown::Full(want.clone()), origin);
                }
                return;
            }
        }

        // The size check lets the full-quality decode replace the Speed pass.
        // Never downgrade a full-resolution image already on screen.
        if let Some(loader) = &self.loader {
            if let Some(img) = loader.get_preview(&want, target) {
                let origin = loader.preview_origin(&want, target);
                let actual = img.width.max(img.height);
                // An embedded JPEG and a decode can share a size, so the
                // origin counts too.
                let same =
                    self.shown.is_preview_of(&want, target, actual) && self.shown_origin == origin;
                if !same && !self.shown.is_full_of(&want) {
                    self.upload_shown(
                        &want,
                        &img,
                        Shown::Preview(want.clone(), target, actual),
                        origin,
                    );
                }
                return;
            }
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
                self.upload_shown(&want, &thumb, Shown::Thumb(want.clone()), Origin::Decoded);
            }
        }
    }

    /// Upload `img` as the loupe image. `tier` records which decode it came
    /// from, and `origin` whether that decode is the file's embedded JPEG.
    pub(super) fn upload_shown(
        &mut self,
        path: &Path,
        img: &image_decode::DecodedImage,
        tier: Shown,
        origin: Origin,
    ) {
        // Read before `self.shown` is reassigned.
        let same_photo = self.shown.path() == Some(path);

        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        renderer.set_image(img);
        self.shown = tier;
        self.shown_origin = origin;
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
        w.set_title(&self.window_title());
    }

    /// "LightPhotos - Folder" in the Grid, "LightPhotos - Folder - File (x/y)"
    /// in the Loupe, "LightPhotos" on the landing page.
    pub(super) fn window_title(&self) -> String {
        let mut title = String::from("LightPhotos");
        let folder = self
            .folder_sel
            .as_deref()
            .or_else(|| self.playlist.as_ref().map(Playlist::dir))
            .or_else(|| self.shown.path().and_then(|p| p.parent()))
            .and_then(|d| d.file_name());
        if let Some(folder) = folder {
            title.push_str(" - ");
            title.push_str(&folder.to_string_lossy());
        }
        let name = self.shown.path().and_then(|p| p.file_name());
        if let (ViewMode::Loupe, Some(_), Some(name)) = (self.mode, &self.playlist, name) {
            let pos = self.sel.unwrap_or(0) + 1;
            title.push_str(&format!(
                " - {} ({}/{})",
                name.to_string_lossy(),
                pos,
                self.visible.len()
            ));
        }
        title
    }

    /// Which decode the Loupe shows for the selected photo, or `None` while
    /// it shows another photo or nothing.
    pub(crate) fn shown_tier(&self) -> Option<ShownTier> {
        if self.shown.path() != self.want.as_deref() {
            return None;
        }
        Some(match (&self.shown, self.shown_origin) {
            (Shown::Nothing, _) => return None,
            (Shown::Thumb(_), _) => ShownTier::Thumb,
            (_, Origin::Embedded) => ShownTier::Embedded,
            // A preview of a small photo already holds every pixel.
            (Shown::Preview(_, _, actual), Origin::Decoded) if self.covers_source(*actual) => {
                ShownTier::Full
            }
            (Shown::Preview(..), Origin::Decoded) => ShownTier::Preview,
            (Shown::Full(_), Origin::Decoded) => ShownTier::Full,
        })
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
    pub(super) fn edit_sig_for(&self, path: &Path) -> u64 {
        let adj = self.edits.get(path).copied().unwrap_or_default();
        let rot = self.rotations.get(path).copied().unwrap_or(0);
        develop::edit_signature_with_touchups(
            &adj,
            self.touchups.get(path).map(Vec::as_slice).unwrap_or(&[]),
            self.masks.get(path).map(Vec::as_slice).unwrap_or(&[]),
            self.detections
                .get(path)
                .unwrap_or(&develop::Detections::default()),
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
        // This runs at least once a frame. With the same request and nothing
        // landed since, asking again would change nothing.
        if let Some(last) = &self.working_thumbs {
            if last.epoch == loader.thumb_epoch()
                && last.reserved == reserved
                && last.paths == paths
            {
                return last.pending;
            }
        }
        loader.set_thumb_working_set_size(reserved);
        loader.set_viewport_thumbs(&paths, px);
        // Failed decodes don't count, or the redraw loop would spin on them.
        let pending = paths
            .iter()
            .any(|p| loader.get_thumb(p, px).is_none() && !loader.thumb_failed(p, px));
        self.working_thumbs = Some(WorkingThumbs {
            epoch: loader.thumb_epoch(),
            reserved,
            paths,
            pending,
        });
        pending
    }

    pub(crate) fn on_sort_meta(
        &mut self,
        metas: Vec<(PathBuf, crate::decode::image_decode::SortMeta)>,
    ) {
        let mut times = Vec::with_capacity(metas.len());
        for (path, meta) in metas {
            times.push((path.clone(), meta.capture));
            self.sort_meta.insert(path, meta);
        }
        self.on_capture_times(times);
    }

    pub(crate) fn on_capture_times(&mut self, times: Vec<(PathBuf, Option<SystemTime>)>) {
        for (path, t) in times {
            self.signals.record(&path, Signal::Capture(t));
            self.capture_times.insert(path, t);
        }
        // Reads land in small batches. Sorting the whole folder for each one
        // reshuffles the Grid a hundred times on a cold folder, so sort at
        // most every `META_SORT_EVERY`, and once more when the last lands.
        if self.sort_order.key.reads_files() {
            let now = Instant::now();
            let due = self
                .last_meta_sort
                .is_none_or(|at| now.duration_since(at) >= META_SORT_EVERY);
            let last = !self.loader.as_ref().is_some_and(|l| l.meta_pending());
            if due || last {
                self.last_meta_sort = Some(now);
                self.recompute_visible();
            }
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

        let mut shapes = Vec::new();

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
            let masks = self.masks.get(path).map(Vec::as_slice).unwrap_or(&[]);
            let rot = self.rotations.get(path).copied().unwrap_or(0);
            if adj.is_identity() && touchups.is_empty() && masks.is_empty() && rot % 4 == 0 {
                if let Some(id) = renderer.upload_thumb(img.width, img.height, &img.rgba) {
                    let tex = ThumbTexture {
                        id,
                        width: img.width,
                        height: img.height,
                        sig: *sig,
                    };
                    shapes.push((path.clone(), img.width, img.height));
                    if let Some(old) = self.thumb_tex.insert(key, tex) {
                        renderer.free_thumb(old.id);
                    }
                }
            } else if let Some(masks) =
                super::masks::mask_input_of(&self.masks, &self.detections, &mut self.coverage, path)
            {
                loader.request_bake(path, *px, *sig, img, adj, touchups, masks, rot);
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
                shapes.push((key.0.clone(), baked.width, baked.height));
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
        for (path, w, h) in shapes {
            self.remember_thumb_shape(&path, w, h);
        }
    }
}

/// One `request_working_thumbs` call: the loader's thumbnail epoch after
/// it, the working-set size and paths it asked for, and its answer.
pub(crate) struct WorkingThumbs {
    epoch: u64,
    reserved: usize,
    paths: Vec<PathBuf>,
    pending: bool,
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

/// Width over height of a filmstrip cell whose thumbnail hasn't loaded yet:
/// 3:2, the shape most cameras shoot.
const DEFAULT_STRIP_ASPECT: f32 = 1.5;

/// The narrowest and widest a filmstrip cell gets, as width over height, so
/// a panorama can't fill the strip.
const STRIP_ASPECT_RANGE: (f32, f32) = (0.4, 3.0);

/// Where each filmstrip cell sits. Every cell is `cell_h` tall and as wide as
/// its thumbnail's aspect ratio needs, so the cells can't be found by
/// multiplying a position by a fixed width.
pub(crate) struct StripLayout {
    cell_h: f32,
    inset: f32,
    spacing: f32,
    /// Left edge of each cell, then the strip's total width. Never empty.
    edges: Vec<f32>,
}

impl StripLayout {
    /// `aspects` gives each cell's thumbnail width over height, `None` where
    /// it hasn't loaded. `inset` is the margin inside a cell around the
    /// thumbnail and `spacing` the gap after each cell.
    pub(crate) fn new(
        aspects: impl Iterator<Item = Option<f32>>,
        cell_h: f32,
        inset: f32,
        spacing: f32,
    ) -> Self {
        let image_h = (cell_h - 2.0 * inset).max(0.0);
        let mut edges = vec![0.0];
        let mut x = 0.0;
        for aspect in aspects {
            let aspect = match aspect {
                Some(a) if a.is_finite() => a.clamp(STRIP_ASPECT_RANGE.0, STRIP_ASPECT_RANGE.1),
                _ => DEFAULT_STRIP_ASPECT,
            };
            x += image_h * aspect + 2.0 * inset + spacing;
            edges.push(x);
        }
        Self {
            cell_h,
            inset,
            spacing,
            edges,
        }
    }

    pub(super) fn built_for(&self, len: usize, cell_h: f32, inset: f32, spacing: f32) -> bool {
        self.edges.len() == len + 1
            && self.cell_h == cell_h
            && self.inset == inset
            && self.spacing == spacing
    }

    fn len(&self) -> usize {
        self.edges.len().saturating_sub(1)
    }

    /// The width of every cell and the gaps between them.
    pub(crate) fn total_width(&self) -> f32 {
        self.edges.last().copied().unwrap_or(0.0)
    }

    /// The cell at `pos` as `(left, size)`, without the gap after it.
    pub(crate) fn cell(&self, pos: usize) -> Option<(f32, egui::Vec2)> {
        let left = *self.edges.get(pos)?;
        let right = *self.edges.get(pos + 1)?;
        let w = (right - left - self.spacing).max(0.0);
        Some((left, egui::vec2(w, self.cell_h)))
    }

    /// The cells `[start, end)` that overlap `left..right`.
    pub(crate) fn range_in(&self, left: f32, right: f32) -> (usize, usize) {
        // The first cell whose right edge (with its gap) is past `left`.
        let start = self
            .edges
            .get(1..)
            .map_or(0, |right_edges| right_edges.partition_point(|&e| e <= left));
        let end = self.edges.partition_point(|&e| e < right);
        let len = self.len();
        (start.min(len), end.clamp(start.min(len), len))
    }
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
    use super::{grid_working_range, load_order, strip_working_range, StripLayout};
    use crate::jobs::loader::Loader;
    use std::path::PathBuf;

    /// The answer kept between frames follows the loader: once one
    /// thumbnail lands and the other fails, in either order, nothing is
    /// loading.
    #[test]
    fn the_working_set_stops_loading_once_its_thumbnails_land() {
        use crate::decode::image_decode::{DecodedImage, DecodedImageFields, PixelFormat};
        use crate::jobs::thumbnail::THUMB_PX;
        let dir = std::env::temp_dir().join(format!("lp-working-thumbs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["a.jpg", "b.jpg"] {
            std::fs::write(dir.join(n), []).unwrap();
        }
        let land = |loader: &mut Loader| {
            let img = DecodedImage::new_tracked(DecodedImageFields {
                width: 1,
                height: 1,
                rgba: vec![0, 0, 0, 255],
                pixel_format: PixelFormat::Srgb8,
            });
            loader.insert_thumb_external(dir.join("a.jpg"), THUMB_PX, std::sync::Arc::new(img));
        };
        let fail =
            |loader: &mut Loader| loader.mark_thumb_failed_external(dir.join("b.jpg"), THUMB_PX);
        for land_first in [true, false] {
            let mut app = super::App::new(None);
            app.load_playlist(crate::navigation::Playlist::from_dir(&dir), dir.clone());
            app.loader = Some(Loader::new(
                16384,
                crate::jobs::cache_limits::CacheLimits::PLATFORM,
            ));
            assert!(app.request_working_thumbs());
            assert!(app.request_working_thumbs(), "asked again, still loading");
            let steps: [&dyn Fn(&mut Loader); 2] = if land_first {
                [&land, &fail]
            } else {
                [&fail, &land]
            };
            steps[0](app.loader.as_mut().unwrap());
            assert!(app.request_working_thumbs(), "one is still loading");
            steps[1](app.loader.as_mut().unwrap());
            assert!(!app.request_working_thumbs(), "land first: {land_first}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// Cells 100 tall with no inset or gap, so each is 100 × its aspect wide.
    fn layout(aspects: &[Option<f32>]) -> StripLayout {
        StripLayout::new(aspects.iter().copied(), 100.0, 0.0, 0.0)
    }

    #[test]
    fn strip_cells_share_a_height_and_take_their_width_from_the_aspect() {
        let strip = StripLayout::new([Some(1.5), Some(0.75)].into_iter(), 110.0, 5.0, 4.0);
        assert_eq!(strip.cell(0), Some((0.0, egui::vec2(160.0, 110.0))));
        assert_eq!(strip.cell(1), Some((164.0, egui::vec2(85.0, 110.0))));
        assert_eq!(strip.cell(2), None);
        assert_eq!(strip.total_width(), 253.0);
    }

    #[test]
    fn a_strip_cell_with_no_thumbnail_or_a_bad_aspect_gets_a_sane_width() {
        let strip = layout(&[
            None,
            Some(f32::NAN),
            Some(0.0),
            Some(f32::INFINITY),
            Some(50.0),
        ]);
        let widths: Vec<f32> = (0..5).map(|p| strip.cell(p).unwrap().1.x).collect();
        assert_eq!(widths, [150.0, 150.0, 40.0, 150.0, 300.0]);
    }

    #[test]
    fn the_strip_range_covers_every_cell_the_viewport_touches() {
        // Edges at 0, 150, 225, 375.
        let strip = layout(&[Some(1.5), Some(0.75), Some(1.5)]);
        assert_eq!(strip.range_in(0.0, 150.0), (0, 1));
        assert_eq!(strip.range_in(149.0, 151.0), (0, 2));
        assert_eq!(strip.range_in(150.0, 225.0), (1, 2));
        assert_eq!(strip.range_in(200.0, 10_000.0), (1, 3));
        assert_eq!(strip.range_in(10_000.0, 20_000.0), (3, 3));
        assert_eq!(layout(&[]).range_in(0.0, 500.0), (0, 0));
    }
}
