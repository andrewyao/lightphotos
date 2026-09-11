//! Auto Tone, wired into the app: one photo from the Develop panel, or every
//! photo in the grid selection.
//!
//! The analysis itself lives in `crate::autotone`. This file only sources
//! pixels for it and writes the result back.
//!
//! Pixels come from whatever is cheapest. The shown photo already has a
//! downscaled linear-light grid in `hist_sample`, built for the Develop
//! histogram, so toning it costs nothing but the analysis. Every other photo is
//! analyzed from its 512px thumbnail, which the loader caches on disk and in
//! memory for the grid anyway — so a batch over a folder the user has already
//! scrolled through does no decoding at all. Thumbnails that are not resident
//! are requested through the normal queue and folded in as they land.

use super::*;
use std::path::Path;

use crate::autotone;
use crate::image_ops;
use crate::thumbnail::THUMB_PX;

/// Longest-side sample count for thumbnail analysis. Matches what
/// `build_hist_sample` uses for the shown photo, so a photo toned in the Loupe
/// and the same photo toned in a batch see the same statistics.
const ANALYSIS_TARGET: usize = 256;

impl App {
    /// Auto Tone the photo on screen, from the histogram sample already in
    /// memory. No decode, no thread, no progress — it lands this frame.
    pub(super) fn auto_tone_shown(&mut self) {
        if self.shown.path().is_none() {
            return;
        }
        if self.hist_sample.is_empty() {
            self.set_status("Auto Tone needs the photo to finish loading".into());
            return;
        }
        let auto = autotone::analyze(&self.hist_sample, self.hist_pixel_format);
        let merged = autotone::merge(&self.current_adjustments(), &auto);
        self.apply_adjustments(merged);
        self.set_status("Auto Tone applied".into());
    }

    /// Auto Tone every selected photo. Photos whose thumbnail is already
    /// resident are done immediately; the rest are queued and finished by
    /// `poll_auto_tone` as their thumbnails arrive.
    pub(super) fn auto_tone_selection(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        self.autotone_pending.clear();
        self.autotone_done = 0;
        self.autotone_total = paths.len();

        let mut wanted: Vec<PathBuf> = Vec::new();
        for path in paths {
            // The photo on screen has a better sample than its thumbnail, and
            // it is the one the user is looking at, so prefer it.
            if self.shown.path() == Some(path.as_path()) && !self.hist_sample.is_empty() {
                let auto = autotone::analyze(&self.hist_sample, self.hist_pixel_format);
                self.tone_one(&path, &auto);
                continue;
            }
            match self.analyze_thumb(&path) {
                Some(auto) => self.tone_one(&path, &auto),
                None => {
                    self.autotone_pending.insert(path.clone());
                    wanted.push(path);
                }
            }
        }
        if let Some(loader) = &mut self.loader {
            for path in wanted {
                loader.request_thumb(path, THUMB_PX);
            }
        }
        self.report_auto_tone_progress();
        self.request_redraw();
    }

    /// Advance a running batch: tone whatever thumbnails just arrived, and drop
    /// any whose decode has permanently failed. Called every frame off the
    /// loader's drain (no-op unless a batch is actually running), because a
    /// batch whose last few thumbnails all fail would otherwise never see
    /// another arrival to wake it up, and would sit at "n/total" forever.
    pub(crate) fn poll_auto_tone(&mut self, arrivals: &[(PathBuf, u32)]) {
        if self.autotone_pending.is_empty() {
            return;
        }
        let mut toned: Vec<(PathBuf, crate::develop::Adjustments)> = Vec::new();
        let mut dropped: Vec<PathBuf> = Vec::new();
        for (path, px) in arrivals {
            if *px != THUMB_PX || !self.autotone_pending.contains(path) {
                continue;
            }
            match self.analyze_thumb(path) {
                Some(auto) => toned.push((path.clone(), auto)),
                // The thumbnail landed but could not be analyzed (a degenerate
                // or short buffer). Waiting on it again would never finish.
                None => dropped.push(path.clone()),
            }
        }
        // Photos the loader has given up on entirely.
        if let Some(loader) = &self.loader {
            for path in &self.autotone_pending {
                if loader.thumb_failed(path, THUMB_PX) {
                    dropped.push(path.clone());
                }
            }
        }
        for path in dropped {
            if self.autotone_pending.remove(&path) {
                self.autotone_total = self.autotone_total.saturating_sub(1);
            }
        }
        for (path, auto) in toned {
            self.autotone_pending.remove(&path);
            self.tone_one(&path, &auto);
        }
        self.report_auto_tone_progress();
        self.request_redraw();
    }

    /// Analyze `path`'s cached thumbnail, or `None` when it is not resident.
    fn analyze_thumb(&self, path: &Path) -> Option<crate::develop::Adjustments> {
        let img = self.loader.as_ref()?.get_thumb(path, THUMB_PX)?;
        let (grid, _, _) = image_ops::downsample_linear(&img, ANALYSIS_TARGET);
        (!grid.is_empty()).then(|| autotone::analyze(&grid, img.pixel_format))
    }

    /// Write one photo's auto adjustments into the edits map and the catalog,
    /// keeping the crop, white balance, saturation and denoise it already had.
    /// Thumbnails re-bake on their own, since their cache key folds in the edit
    /// signature.
    fn tone_one(&mut self, path: &Path, auto: &crate::develop::Adjustments) {
        let base = self.edits.get(path).copied().unwrap_or_default();
        let merged = autotone::merge(&base, auto);
        if merged.is_identity() {
            self.edits.remove(path);
        } else {
            self.edits.insert(path.to_path_buf(), merged);
        }
        self.catalog.set_adjustments(path, &merged);
        self.autotone_done += 1;
        // Keep the loupe and its histogram honest if this is the shown photo.
        if self.shown.path() == Some(path) {
            self.push_adjustments();
            self.hist_dirty = true;
        }
    }

    /// Status line for a running or just-finished batch.
    fn report_auto_tone_progress(&mut self) {
        let (done, total) = (self.autotone_done, self.autotone_total);
        if total == 0 {
            return;
        }
        if self.autotone_pending.is_empty() {
            self.set_status(format!("Auto Tone applied to {done} photo(s)"));
            self.autotone_done = 0;
            self.autotone_total = 0;
        } else {
            self.set_status(format!("Auto Tone {done}/{total}\u{2026}"));
        }
    }
}
