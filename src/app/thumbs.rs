//! Tier selection for the Loupe (`try_show`), thumbnail texture sync for the
//! Grid/filmstrip (`sync_thumb_textures`), and the background scoring hooks
//! (burst/duplicate/face-quality) that ride along on decoded thumbnails.
//!
//! ## Pipeline position
//! - `try_show` runs every frame and is Pipeline 1's "what does the user see
//!   right now" decision: it reads whatever `loader.rs`'s caches
//!   (`get_full`/`get_preview`/`get_thumb`) already have and calls
//!   `upload_shown` for the best tier available, falling coarsest-first.
//! - `upload_shown` is the actual hand-off into Pipeline 1's shared final
//!   stage: it calls `Renderer::set_image` (`renderer.rs`).
//! - `request_working_thumbs`/`sync_thumb_textures` are Pipeline 2's
//!   UI-thread half: the first asks `loader.rs` to decode thumbnails for the
//!   visible range, the second uploads whatever has landed as egui
//!   textures — baking edits in via `image_ops::bake_edited` first when the
//!   photo has any.
//! - `request_burst_thumbs`/`request_dup_thumbs`/`request_face_quality` and
//!   their `poll_*`/`score_*` counterparts aren't part of the three
//!   decode/render pipelines — they're background analysis that consumes
//!   thumbnails Pipeline 2 already decoded, rather than driving decode
//!   itself.

use super::*;
use std::path::{Path, PathBuf};
use std::time::SystemTime;


use crate::develop::{self};
use crate::featureprint;
use crate::phash;
use crate::sharpness;
use crate::{image_decode, image_ops};

// TEMPORARY DEBUG colors — see `Renderer::tier_debug_color`'s doc comment.
// Remove alongside `set_tier_debug_color` once the Loupe zoom-refit fix is
// verified.
pub(super) const TIER_DEBUG_WHITE: wgpu::Color = wgpu::Color { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
// Only referenced from `app/web.rs` (the `Speed` tier is wasm32-only).
#[cfg(target_arch = "wasm32")]
pub(super) const TIER_DEBUG_GRAY_18: wgpu::Color = wgpu::Color { r: 0.18, g: 0.18, b: 0.18, a: 1.0 };
pub(super) const TIER_DEBUG_BLACK: wgpu::Color = wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };

impl App {

    pub(crate) fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Show the wanted image at the best tier available, in descending order:
    /// full resolution (only fetched once the user zooms in), then the screen-fit
    /// preview (the normal case), then the cached thumbnail as an instant
    /// placeholder. Each tier that lands replaces the coarser one below it.
    pub(crate) fn try_show(&mut self) {
        let Some(want) = self.want.clone() else {
            return;
        };
        let target = self.preview_px();

        // Full resolution ready → show it, unless it's already what's shown.
        if let Some(img) = self.loader.as_ref().and_then(|l| l.get_full(&want)) {
            if !self.shown.is_full_of(&want) {
                self.upload_shown(&want, &img, Shown::Full(want.clone()));
                self.set_tier_debug(TIER_DEBUG_BLACK, "FULL"); // TEMPORARY DEBUG
            }
            return;
        }

        // Preview ready → show it, unless that exact image is already up. The
        // size check is what lets the forced decode replace the quick pass
        // behind it. Never downgrade a full-resolution image already on screen.
        if let Some(img) = self
            .loader
            .as_ref()
            .and_then(|l| l.get_preview(&want, target))
        {
            let actual = img.width.max(img.height);
            if !self.shown.is_preview_of(&want, target, actual) && !self.shown.is_full_of(&want) {
                self.upload_shown(&want, &img, Shown::Preview(want.clone(), target, actual));
                // Reached only via `loader.rs`'s preview cache — the
                // wasm32 `Speed` tier bypasses this entirely (see
                // `poll_web_preview`), so anything landing here is always
                // the real quality decode (native's forced `Preview`, or
                // wasm32's `JobKind::Preview`). TEMPORARY DEBUG.
                self.set_tier_debug(TIER_DEBUG_BLACK, "QUALITY");
            }
            return;
        }

        // No preview at this target. Re-request it rather than assuming
        // `load_selected` already did: the target moves with the window size, so
        // a resize past a quantum boundary invalidates the one in flight, and an
        // LRU eviction can drop one that did land. `request_preview` de-dupes,
        // so this is free in the common case where it's simply still decoding.
        //
        // Native only: `loader.rs`'s own worker queue has zero live workers on
        // wasm32 (thread spawn always fails there), so nothing ever calls
        // `Loader::drain()` to clear the `preview_inflight`/`quick_inflight`
        // entry this call would create — `has_pending_image()` would then
        // read as permanently true the moment any photo is opened in the
        // Loupe, locking `main.rs`'s frame loop into an indefinite busy-poll
        // even after wasm32's own (separate, working) preview pipeline
        // — `request_web_preview`/`poll_web_preview`, app/web.rs — has
        // already landed the image. That pipeline is what actually services
        // the Loupe on wasm32, so this call is both redundant and the real
        // source of the stuck state there.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(loader) = &mut self.loader {
            loader.request_preview(want.clone(), target);
        }

        // Nothing decoded yet: show the thumbnail placeholder if we aren't
        // already showing this image in some form.
        //
        // wasm32 RAW used to skip this (its thumb-tier cache entry — the
        // file's own tiny embedded EXIF preview, or our quarter-res `Fast`
        // demosaic — reads as visibly wrong blown up to fill the Loupe, not
        // just coarse) and wait for the real `Quality` decode instead. That
        // was a workaround for `upload_shown`'s missing `fitted` gate (a
        // same-photo tier swap always landed mis-zoomed, so fewer tiers
        // meant fewer chances to hit it) rather than an actual quality
        // concern with showing this stage — `upload_shown` re-fits
        // correctly now, and the `Speed` tier (`request_web_preview`)
        // replaces this placeholder within moments anyway, so it's now just
        // the instant "something's happening" first frame it already is on
        // native.
        if self.shown.path() != Some(want.as_path()) {
            if let Some(thumb) = self
                .loader
                .as_ref()
                .and_then(|l| l.get_thumb(&want, self.thumb_px))
            {
                self.upload_shown(&want, &thumb, Shown::Thumb(want.clone()));
                self.set_tier_debug(TIER_DEBUG_WHITE, "THUMB"); // TEMPORARY DEBUG
            }
        }
    }

    // TEMPORARY DEBUG — see `Renderer::tier_debug_color`'s and
    // `App::debug_tier_label`'s doc comments. Remove both call sites and
    // this method once the Loupe zoom-refit fix is verified.
    pub(super) fn set_tier_debug(&mut self, color: wgpu::Color, label: &'static str) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.tier_debug_color = color;
        }
        self.debug_tier_label = label;
        self.update_window_title();
        // Window/tab title can be invisible depending on how the page is
        // displayed (installed/app-mode window, no tab strip, ...) — the
        // DevTools console always exists regardless, so log there too.
        #[cfg(target_arch = "wasm32")]
        web_sys::console::log_1(
            &format!(
                "[debug] tier -> {label} ({}x{})",
                self.renderer.as_ref().map(|r| r.image_size.0).unwrap_or(0),
                self.renderer.as_ref().map(|r| r.image_size.1).unwrap_or(0),
            )
            .into(),
        );
    }

    /// Upload an image to the renderer as the currently-shown image and re-fit.
    /// `tier` records which of the three resolutions this pixel data came from.
    pub(super) fn upload_shown(
        &mut self,
        path: &Path,
        img: &image_decode::DecodedImage,
        tier: Shown,
    ) {
        // Whether this upload swaps in a sharper tier of the picture already on
        // screen, rather than moving to a different picture. Must be read before
        // `self.shown` is reassigned below.
        let same_photo = self.shown.path() == Some(path);

        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        renderer.set_image(img);
        self.shown = tier;
        // Rebuild the histogram sample from the newly-shown image, then mark the
        // histogram dirty so it's recomputed before the next draw.
        self.build_hist_sample(img);
        // Load this image's stored edits into the shader (or identity if none).
        self.push_adjustments();
        // A new picture always starts fitted. A sharper tier of the *same*
        // picture must not disturb a view the user has already zoomed away
        // from fit — the full-resolution decode is triggered precisely by
        // zooming in, so re-fitting here would yank the user back out to fit
        // the instant their zoom paid off. But while `self.fitted` is still
        // true (no manual zoom yet — the common case for a tier swap that
        // lands moments after opening), the view is supposed to be tracking
        // the image's own size, and `image_size()` falls back to the
        // just-uploaded texture's raw pixel dimensions until real
        // `source_size` metadata lands — so a same-photo tier swap in that
        // window must re-fit too, or the old zoom/pan (computed against the
        // previous, differently-sized tier) gets reapplied against the new
        // one's dimensions: shrunk scale, near-zero offset, stuck zoomed
        // into the top-left corner. This is what `on_exif_info` already
        // does when `source_size` itself changes; this covers the same
        // hazard for every other tier landing, not just that one.
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
                    // TEMPORARY DEBUG prefix — see `App::debug_tier_label`.
                    let tag =
                        if self.debug_tier_label.is_empty() { String::new() } else { format!("[{}] ", self.debug_tier_label) };
                    w.set_title(&format!("{tag}{}  ({}/{})", name, pos, self.visible.len()));
                }
            }
            ViewMode::Grid => {
                w.set_title(&format!("Grid  ({} photos)", self.visible.len()));
            }
            ViewMode::Survey => {
                w.set_title(&format!("Survey  ({} photos)", self.survey_members.len()));
            }
        }
    }

    /// The image's size for all view math, in source pixels — *not* the size of
    /// the uploaded texture, which may be a thumbnail or a downscaled preview
    /// (see `App::source_size`). Falls back to the texture's own size until the
    /// metadata read lands; every tier shares the source's aspect ratio, so a
    /// fit computed from the fallback is already correct, and only absolute
    /// scales (100% zoom, touch-up radii in pixels) need the real value.
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
            // Survey's thumbnails are an arbitrary scattered set of paths
            // (not a contiguous visible-position range), so they're handled
            // separately in `working_thumb_keys` instead of through this
            // range-based path.
            ViewMode::Survey => 0..0,
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
        let mut keys: Vec<(PathBuf, u32, u64)> = self
            .working_positions()
            .filter_map(|pos| self.visible.get(pos).copied())
            .filter_map(|i| pl.entry(i))
            .map(|p| {
                let sig = self.edit_sig_for(p);
                (p.to_path_buf(), px, sig)
            })
            .collect();
        // Survey's members are an arbitrary scattered set, not covered by the
        // position-range walk above — add them explicitly so their thumbnails
        // stay loaded/uploaded while the screen is open.
        if self.mode == ViewMode::Survey {
            for p in &self.survey_members {
                let sig = self.edit_sig_for(p);
                keys.push((p.clone(), px, sig));
            }
        }
        keys
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

    /// For every playlist entry not yet hashed: hash it if its thumbnail is
    /// already decoded, else request it. Unlike `request_burst_thumbs`, this
    /// runs over the *whole* folder (not a pre-identified member set) since
    /// dHash grouping needs every entry's hash to find candidates in the first
    /// place — that's why it's gated behind `dupes_on` as an opt-in cost.
    /// Returns whether any hash is still outstanding, so the caller keeps
    /// polling until the pass converges.
    pub(crate) fn request_dup_thumbs(&mut self) -> bool {
        if !self.dupes_on {
            return false;
        }
        let px = self.thumb_px;
        let pending: Vec<PathBuf> = {
            let Some(pl) = &self.playlist else {
                return false;
            };
            pl.entries()
                .iter()
                .filter(|p| !self.phashes.contains_key(*p) || !self.sharpness.contains_key(*p))
                .cloned()
                .collect()
        };

        let mut newly: Vec<(PathBuf, u64, f64)> = Vec::new();
        let mut still_unhashed = false;
        if let Some(loader) = &mut self.loader {
            for p in &pending {
                if let Some(img) = loader.get_thumb(p, px) {
                    newly.push((
                        p.clone(),
                        phash::dhash(&img.rgba, img.width, img.height),
                        sharpness::sharpness(&img.rgba, img.width, img.height),
                    ));
                } else if loader.thumb_failed(p, px) {
                    // Permanently failed — will never hash; not counted as pending.
                } else {
                    loader.request_thumb(p.clone(), px);
                    still_unhashed = true;
                }
            }
        }
        if !newly.is_empty() {
            for (p, h, s) in newly {
                self.phashes.insert(p.clone(), h);
                self.sharpness.insert(p, s);
            }
            self.recompute_dup_marks();
        }
        still_unhashed
    }

    /// When thumbnails arrive and dupes are on, hash any unhashed entry among
    /// them, then refresh the groups and redraw. Unlike
    /// `score_arrived_thumbs`'s burst-membership filter, every arrival is a
    /// candidate here since there's no prior grouping to gate against.
    pub(crate) fn score_arrived_dup_thumbs(&mut self, arrivals: &[(PathBuf, u32)]) {
        if !self.dupes_on {
            return;
        }
        let px = self.thumb_px;
        let mut newly: Vec<(PathBuf, u64, f64)> = Vec::new();
        if let Some(loader) = &self.loader {
            for (path, mpx) in arrivals {
                if *mpx != px
                    || (self.phashes.contains_key(path) && self.sharpness.contains_key(path))
                {
                    continue;
                }
                if let Some(img) = loader.get_thumb(path, px) {
                    newly.push((
                        path.clone(),
                        phash::dhash(&img.rgba, img.width, img.height),
                        sharpness::sharpness(&img.rgba, img.width, img.height),
                    ));
                }
            }
        }
        if !newly.is_empty() {
            for (p, h, s) in newly {
                self.phashes.insert(p.clone(), h);
                self.sharpness.insert(p, s);
            }
            self.recompute_dup_marks();
            self.request_redraw();
        }
    }

    /// For every non-anchor member of a current dHash group (size 2+) not yet
    /// compared and not already in flight, submit a feature-print comparison
    /// job against its group's anchor. Returns whether any comparison is still
    /// outstanding, so the caller keeps polling until the pass converges.
    pub(crate) fn request_feature_prints(&mut self) -> bool {
        if !self.dupes_on {
            return false;
        }
        let Some(pl) = &self.playlist else {
            return false;
        };
        let entries = pl.entries();
        // `dup_groups` is rebuilt by `recompute_dup_marks` whenever the
        // playlist changes; if it hasn't run yet for this playlist, there's
        // nothing valid to submit against yet.
        if entries.len() != self.dup_groups.len() {
            return !self.feature_pending.is_empty();
        }

        let mut sizes: HashMap<u32, usize> = HashMap::new();
        for &g in &self.dup_groups {
            *sizes.entry(g).or_insert(0) += 1;
        }
        let mut anchor_of: HashMap<u32, PathBuf> = HashMap::new();
        let mut to_submit: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (i, &g) in self.dup_groups.iter().enumerate() {
            if sizes.get(&g).copied().unwrap_or(0) < 2 {
                continue; // singleton, no anchor to compare against
            }
            let anchor = anchor_of
                .entry(g)
                .or_insert_with(|| entries[i].clone())
                .clone();
            let member = &entries[i];
            if *member == anchor {
                continue; // this member is the anchor itself
            }
            let key = (anchor.clone(), member.clone());
            if self.feature_distances.contains_key(&key)
                || self.feature_failed.contains(&key)
                || self.feature_pending.contains(&key)
            {
                continue;
            }
            to_submit.push((member.clone(), anchor));
        }

        if let Some(pool) = &self.feature_pool {
            for (member, anchor) in to_submit {
                self.feature_pending.insert((anchor.clone(), member.clone()));
                pool.submit(featureprint::DistanceJob { member, anchor });
            }
        }
        !self.feature_pending.is_empty()
    }

    /// Fold finished feature-print comparisons into the cache, then refresh
    /// duplicate marks (the refinement pass can now split off any newly-
    /// confirmed false positive) and redraw.
    pub(crate) fn poll_feature_prints(&mut self) {
        let outcomes = self
            .feature_pool
            .as_ref()
            .map(|p| p.poll())
            .unwrap_or_default();
        if outcomes.is_empty() {
            return;
        }
        // Only accept a result for the exact anchor/member pairing currently
        // represented by the raw dHash groups. A member can acquire a new
        // anchor while Vision is still processing the old job.
        let current_pairs: HashSet<(PathBuf, PathBuf)> = self
            .playlist
            .as_ref()
            .map(|pl| {
                let entries = pl.entries();
                let mut anchors: HashMap<u32, PathBuf> = HashMap::new();
                self.dup_groups
                    .iter()
                    .enumerate()
                    .filter_map(|(i, &group)| {
                        let anchor = anchors
                            .entry(group)
                            .or_insert_with(|| entries[i].clone())
                            .clone();
                        let member = entries[i].clone();
                        (member != anchor).then_some((anchor, member))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut changed = false;
        for o in outcomes {
            let key = (o.anchor.clone(), o.member.clone());
            self.feature_pending.remove(&key);
            if !current_pairs.contains(&key) {
                continue;
            }
            match o.result {
                Ok(d) => {
                    self.feature_distances.insert(key, d);
                    changed = true;
                }
                Err(_) => {
                    self.feature_failed.insert(key);
                }
            }
        }
        if changed {
            self.recompute_dup_marks();
            self.request_redraw();
        }
    }

    /// Submit face analysis for every burst member and every multi-photo
    /// duplicate group member not already analyzed, in flight, or failed.
    /// Returns whether any analysis is still outstanding, so the caller keeps
    /// polling until the pass converges.
    ///
    /// Deliberately gated to grouped photos rather than the whole folder: a
    /// blink only matters when there's a sibling frame to prefer instead, and
    /// Vision decodes the file at full resolution to find faces — much heavier
    /// than the thumbnail-based blur and dHash passes that do run folder-wide.
    pub(crate) fn request_face_quality(&mut self) -> bool {
        if !self.bursts_on && !self.dupes_on {
            return false;
        }
        let Some(pl) = &self.playlist else {
            return false;
        };
        let entries = pl.entries();

        // Group sizes over the refined duplicate grouping, so singletons (which
        // have no sibling to be preferred over) don't get analyzed. Both mark
        // vectors are indexed by playlist entry index and are only valid when
        // they've been rebuilt for the current playlist.
        let dups_valid = self.dupes_on && self.dup_refined.len() == entries.len();
        let mut sizes: HashMap<u32, usize> = HashMap::new();
        if dups_valid {
            for &g in &self.dup_refined {
                *sizes.entry(g).or_insert(0) += 1;
            }
        }

        let mut to_submit: Vec<PathBuf> = Vec::new();
        for (i, p) in entries.iter().enumerate() {
            let in_burst = self.bursts_on && matches!(self.burst_marks.get(i), Some(Some(_)));
            let in_dup_group = dups_valid
                && sizes
                    .get(&self.dup_refined[i])
                    .copied()
                    .unwrap_or(0)
                    >= 2;
            if !(in_burst || in_dup_group) {
                continue;
            }
            if self.face_quality.contains_key(p)
                || self.face_pending.contains(p)
                || self.face_failed.contains(p)
            {
                continue;
            }
            to_submit.push(p.clone());
        }

        if let Some(pool) = &self.face_pool {
            for p in to_submit {
                self.face_pending.insert(p.clone());
                pool.submit(p);
            }
        }
        !self.face_pending.is_empty()
    }

    /// Fold finished face analyses into the cache, then refresh whichever marks
    /// are live (a newly-known blink can change which frame is Best) and redraw.
    pub(crate) fn poll_face_quality(&mut self) {
        let outcomes = self
            .face_pool
            .as_ref()
            .map(|p| p.poll())
            .unwrap_or_default();
        if outcomes.is_empty() {
            return;
        }
        let mut changed = false;
        for o in outcomes {
            self.face_pending.remove(&o.path);
            match o.result {
                Ok(q) => {
                    self.face_quality.insert(o.path, q);
                    changed = true;
                }
                Err(_) => {
                    self.face_failed.insert(o.path);
                }
            }
        }
        if changed {
            if self.bursts_on {
                self.recompute_burst_marks();
            }
            if self.dupes_on {
                self.recompute_dup_marks();
            }
            // Newly-found blinks change what the filter should be showing.
            if self.eyes_filter_on() {
                self.recompute_visible();
            }
            self.request_redraw();
        }
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
            // The metadata read doubles as how the loupe learns the original's
            // true resolution while it is still showing a downscaled tier.
            // Re-fit once it lands: any fit computed before this used the
            // uploaded texture's size as a stand-in.
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
