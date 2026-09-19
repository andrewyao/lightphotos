//! Which decode tier the Loupe shows (`try_show`), thumbnail textures for the
//! Grid and filmstrip (`sync_thumb_textures`), and the burst, duplicate, and
//! face scoring that runs on decoded thumbnails.

use super::*;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::develop::{self};
use crate::featureprint;
use crate::phash;
use crate::sharpness;
use crate::thumbnail::THUMB_PX;
use crate::{image_decode, image_ops};

// TEMPORARY DEBUG colors, removed together with `set_tier_debug`.
pub(super) const TIER_DEBUG_WHITE: wgpu::Color = wgpu::Color {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 1.0,
};
// The wasm32 `Speed` tier's color.
#[cfg(target_arch = "wasm32")]
pub(super) const TIER_DEBUG_GRAY_18: wgpu::Color = wgpu::Color {
    r: 0.18,
    g: 0.18,
    b: 0.18,
    a: 1.0,
};
pub(super) const TIER_DEBUG_BLACK: wgpu::Color = wgpu::Color {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 1.0,
};

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
                self.set_tier_debug(TIER_DEBUG_BLACK, "FULL"); // TEMPORARY DEBUG
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
                // TEMPORARY DEBUG. On native this may be the Speed pass too.
                self.set_tier_debug(TIER_DEBUG_BLACK, "QUALITY");
            }
            return;
        }

        // Re-request every frame: a resize can change the target and the LRU
        // can evict a landed preview. `request_preview` dedupes.
        //
        // Native only. `loader.rs` has no workers on wasm32, so the in-flight
        // marker would never clear and the frame loop would poll forever.
        // `app/web.rs` decodes previews there.
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
                self.set_tier_debug(TIER_DEBUG_WHITE, "THUMB"); // TEMPORARY DEBUG
            }
        }
    }

    // TEMPORARY DEBUG: tints the clear color and titles the window with the
    // decode tier on screen. Remove with its call sites and
    // `Renderer::tier_debug_color`.
    pub(super) fn set_tier_debug(&mut self, color: wgpu::Color, label: &'static str) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.tier_debug_color = color;
        }
        self.debug_tier_label = label;
        self.update_window_title();
        // An installed web app may have no visible title, so log it too.
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
                    // TEMPORARY DEBUG prefix.
                    let tag = if self.debug_tier_label.is_empty() {
                        String::new()
                    } else {
                        format!("[{}] ", self.debug_tier_label)
                    };
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
                // Include the selection, so its thumbnail loads before the
                // strip reports a range.
                let margin = 8;
                let mut start = self.strip_range.0.saturating_sub(margin);
                let mut end = (self.strip_range.1 + margin).min(len);
                if let Some(s) = self.sel {
                    start = start.min(s);
                    end = end.max((s + 1).min(len));
                }
                start..end.max(start)
            }
            // Survey members aren't a contiguous range; `working_thumb_keys`
            // adds them.
            ViewMode::Survey => 0..0,
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
            rot,
        )
    }

    /// Texture keys for the working set.
    pub(super) fn working_thumb_keys(&self) -> Vec<(PathBuf, u32, u64)> {
        let px = THUMB_PX;
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
        if self.mode == ViewMode::Survey {
            for p in &self.survey_members {
                let sig = self.edit_sig_for(p);
                keys.push((p.clone(), px, sig));
            }
        }
        keys
    }

    /// Request thumbnails for the working set. Returns true while any is
    /// missing, so the caller keeps redrawing.
    pub(crate) fn request_working_thumbs(&mut self) -> bool {
        let px = THUMB_PX;
        let paths: Vec<PathBuf> = self
            .working_thumb_keys()
            .into_iter()
            .map(|(p, _, _)| p)
            .collect();

        let mut any_missing = false;
        if let Some(loader) = &mut self.loader {
            loader.set_thumb_working_set_size(paths.len());
            for p in &paths {
                // Skip failed decodes, or the redraw loop would spin on them.
                if loader.get_thumb(p, px).is_none() && !loader.thumb_failed(p, px) {
                    loader.request_thumb(p.clone(), px);
                    any_missing = true;
                }
            }
        }
        any_missing
    }

    /// Score sharpness for every burst member, requesting thumbnails as needed,
    /// so each burst's winner is chosen from all its frames, not just those on
    /// screen. Returns true while the capture-time scan or any scoring is
    /// unfinished. The event loop polls this each frame, since worker
    /// completions don't wake it.
    pub(crate) fn request_burst_thumbs(&mut self) -> bool {
        if !self.bursts_on {
            return false;
        }
        let px = THUMB_PX;

        let scan_pending = {
            let Some(pl) = &self.playlist else {
                return false;
            };
            pl.entries()
                .iter()
                .any(|p| !self.capture_times.contains_key(p))
        };

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

        // Thumbnails are premultiplied RGBA8. Photos are opaque, so that equals
        // straight alpha for the luma metric.
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
                    // Failed for good; not pending.
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

    /// Hash every photo in the folder, requesting thumbnails as needed. Grouping
    /// needs every hash to find candidates, so this runs folder-wide. Returns
    /// true while any hash is outstanding.
    pub(crate) fn request_dup_thumbs(&mut self) -> bool {
        if !self.dupes_on {
            return false;
        }
        let px = THUMB_PX;
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
                    // Failed for good; not pending.
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

    /// Hash newly arrived thumbnails and refresh the duplicate groups.
    pub(crate) fn score_arrived_dup_thumbs(&mut self, arrivals: &[(PathBuf, u32)]) {
        if !self.dupes_on {
            return;
        }
        let px = THUMB_PX;
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

    /// Submit a feature-print comparison of each dHash group member against
    /// its group's anchor, skipping pairs already done or in flight. Returns
    /// true while any comparison is outstanding.
    pub(crate) fn request_feature_prints(&mut self) -> bool {
        if !self.dupes_on {
            return false;
        }
        let Some(pl) = &self.playlist else {
            return false;
        };
        let entries = pl.entries();
        // `dup_groups` hasn't been rebuilt for this playlist yet.
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
                continue;
            }
            let anchor = anchor_of
                .entry(g)
                .or_insert_with(|| entries[i].clone())
                .clone();
            let member = &entries[i];
            if *member == anchor {
                continue;
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
                self.feature_pending
                    .insert((anchor.clone(), member.clone()));
                pool.submit(featureprint::DistanceJob { member, anchor });
            }
        }
        !self.feature_pending.is_empty()
    }

    /// Cache finished feature-print comparisons and refresh the duplicate marks.
    pub(crate) fn poll_feature_prints(&mut self) {
        let outcomes = self
            .feature_pool
            .as_ref()
            .map(|p| p.poll())
            .unwrap_or_default();
        if outcomes.is_empty() {
            return;
        }
        // Accept only pairs that match the current groups. A member's anchor
        // can change while Vision is still running the old job.
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

    /// Submit face analysis for burst and duplicate group members only: a
    /// blink matters only when a sibling frame can replace it, and Vision
    /// decodes at full resolution. Returns true while any analysis is outstanding.
    pub(crate) fn request_face_quality(&mut self) -> bool {
        if !self.bursts_on && !self.dupes_on {
            return false;
        }
        let Some(pl) = &self.playlist else {
            return false;
        };
        let entries = pl.entries();

        // Group sizes, to skip singletons. The mark vectors are valid only once
        // rebuilt for the current playlist.
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
            let in_dup_group =
                dups_valid && sizes.get(&self.dup_refined[i]).copied().unwrap_or(0) >= 2;
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

    /// Cache finished face analyses. A new blink can change which frame is
    /// Best, so the live marks are recomputed.
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
            if self.eyes_filter_on() {
                self.recompute_visible();
            }
            self.request_redraw();
        }
    }

    /// Cache capture times, then regroup bursts and request their thumbnails.
    pub(crate) fn on_capture_times(&mut self, times: Vec<(PathBuf, Option<SystemTime>)>) {
        for (path, t) in times {
            self.capture_times.insert(path, t);
        }
        if self.bursts_on {
            self.recompute_burst_marks();
            self.request_burst_thumbs();
        }
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

    /// Score newly arrived burst-member thumbnails and refresh the winners.
    pub(crate) fn score_arrived_thumbs(&mut self, arrivals: &[(PathBuf, u32)]) {
        if !self.bursts_on {
            return;
        }
        let px = THUMB_PX;
        let mut newly: Vec<(PathBuf, f64)> = Vec::new();
        if let Some(loader) = &self.loader {
            for (path, mpx) in arrivals {
                if *mpx != px || self.sharpness.contains_key(path) {
                    continue;
                }
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

    /// Upload decoded working-set thumbnails as egui textures and drop the rest.
    pub(super) fn sync_thumb_textures(&mut self) {
        if self.playlist.is_none() {
            return;
        }
        let wanted = self.working_thumb_keys();

        // Bake edits into the texture so the grid matches the loupe. The
        // cached thumbnail stays unedited, since the loupe applies edits to its
        // placeholder in the shader.
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
                egui::ColorImage::from_rgba_premultiplied(
                    [img.width as usize, img.height as usize],
                    &img.rgba,
                )
            } else {
                let (w, h, rgba) = image_ops::bake_edited(&img, &adj, touchups, rot);
                // bake_edited output is opaque, so premultiplied equals straight.
                egui::ColorImage::from_rgba_premultiplied([w as usize, h as usize], &rgba)
            };
            let name = format!("thumb:{}:{}:{:016x}", key.0.display(), key.1, key.2);
            let handle = self
                .egui_ctx
                .load_texture(name, color, egui::TextureOptions::LINEAR);
            self.thumb_tex.insert(key.clone(), handle);
        }

        // Also evicts textures with a stale edit signature.
        let keep: std::collections::HashSet<(PathBuf, u32, u64)> = wanted.into_iter().collect();
        self.thumb_tex.retain(|k, _| keep.contains(k));
    }
}
