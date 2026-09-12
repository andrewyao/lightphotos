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

    /// Auto Tone one photo: whichever one the user is pointing at. Bound to
    /// Cmd+U, and unconfirmed, unlike the selection-wide `Cmd+Shift+U`.
    ///
    /// The target is the *selection*, not `shown`. Those agree in the Loupe,
    /// but `shown` holds whatever the Loupe last uploaded and grid arrow
    /// navigation never reloads it, so keying off it toned the photo the user
    /// last looked at rather than the one under the cursor.
    pub(super) fn auto_tone_one(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };
        // The photo on screen already has a full-resolution histogram sample in
        // memory, so prefer that to its thumbnail whenever the target happens
        // to be it — in the Loupe always, in the Grid when the cursor sits on
        // the photo last opened. `auto_tone_batch` makes the same choice, but
        // going straight through `auto_tone_shown` keeps the Loupe's inline
        // "no decode, no thread, no progress" path and its own status line.
        if Some(path.as_path()) == self.shown.path() && !self.hist_sample.is_empty() {
            self.auto_tone_shown();
            return;
        }
        self.enqueue_auto_tone(vec![path]);
    }

    /// Auto Tone every selected photo. Photos whose thumbnail is already
    /// resident are done immediately; the rest are queued and finished by
    /// `poll_auto_tone` as their thumbnails arrive.
    pub(super) fn auto_tone_selection(&mut self) {
        self.auto_tone_batch(self.selected_paths());
    }

    /// Run one batch over `paths`, replacing any batch already in flight.
    fn auto_tone_batch(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.autotone_pending.clear();
        self.autotone_done = 0;
        self.autotone_total = 0;
        self.enqueue_auto_tone(paths);
    }

    /// Add targets without abandoning outstanding work or counting duplicates.
    fn enqueue_auto_tone(&mut self, paths: Vec<PathBuf>) {
        let mut wanted: Vec<PathBuf> = Vec::new();
        for path in paths {
            if self.autotone_pending.contains(&path) {
                continue;
            }
            self.autotone_total += 1;
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

    /// Abandon a running batch's outstanding work. Called on every folder
    /// change, because `tone_one` writes through whichever catalog is active
    /// *now* and `Catalog` keys its records by filename alone: a folder-A
    /// thumbnail landing after the user has moved to folder B would take B's
    /// record for that filename, merge A's auto adjustments into it, and write
    /// the result back out to A's sidecar — losing A's rating, rotation and
    /// touch-ups and leaving a phantom record in B. Photos the batch already
    /// toned keep their edits; only the ones still waiting are dropped.
    pub(super) fn cancel_auto_tone(&mut self) {
        if self.autotone_pending.is_empty() {
            return;
        }
        let (done, total) = (self.autotone_done, self.autotone_total);
        self.autotone_pending.clear();
        self.autotone_done = 0;
        self.autotone_total = 0;
        self.set_status(format!("Auto Tone stopped at {done}/{total}"));
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
            // Cmd+U routes a single photo through here whenever its thumbnail
            // has to be decoded first, so the one-photo wording matters.
            self.set_status(if done == 1 {
                "Auto Tone applied".to_string()
            } else {
                format!("Auto Tone applied to {done} photos")
            });
            self.autotone_done = 0;
            self.autotone_total = 0;
        } else {
            self.set_status(format!("Auto Tone {done}/{total}\u{2026}"));
        }
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;

    /// Two photos in a real folder, plus the app that has them loaded in the
    /// Grid. Files are empty: nothing here decodes them, and a photo with no
    /// resident thumbnail is exactly the case these tests want.
    fn grid_with_two_photos(tag: &str) -> (App, PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.jpg"), dir.join("b.jpg"));
        std::fs::write(&a, []).unwrap();
        std::fs::write(&b, []).unwrap();

        let mut app = App::new(None);
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        app.mode = ViewMode::Grid;
        (app, dir, a, b)
    }

    /// The bug this exists for: `shown` holds whatever the Loupe last had
    /// uploaded, and grid arrow navigation never reloads it, so Cmd+U in the
    /// Grid used to tone the photo the user last *looked at* rather than the
    /// one under the cursor — and reported success for it.
    #[test]
    fn cmd_u_in_the_grid_tones_the_cursor_photo_not_the_last_loupe_photo() {
        let (mut app, dir, a, b) = grid_with_two_photos("autotone-cursor");
        // Photo A was open in the Loupe, and still has its histogram sample.
        app.shown = Shown::Preview(a.clone(), 1024, 1024);
        app.hist_sample = vec![[0.02f32; 3]; 64];
        // The grid cursor is on photo B.
        app.sel = app
            .visible
            .iter()
            .position(|&i| app.playlist.as_ref().and_then(|pl| pl.entry(i)) == Some(b.as_path()));
        assert!(
            app.sel.is_some(),
            "test setup: B must be in the visible grid"
        );

        app.auto_tone_one();

        assert!(
            !app.edits.contains_key(&a),
            "the photo left over in the Loupe must not be touched"
        );
        assert!(
            app.autotone_pending.contains(&b),
            "the cursor photo must be the one queued for toning"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn successive_single_photo_requests_preserve_pending_work() {
        let (mut app, dir, a, b) = grid_with_two_photos("autotone-successive");
        for path in [&a, &b, &a] {
            app.sel = app.visible.iter().position(|&i| {
                app.playlist.as_ref().and_then(|pl| pl.entry(i)) == Some(path.as_path())
            });
            assert!(app.sel.is_some());
            app.auto_tone_one();
        }
        assert_eq!(app.autotone_pending.len(), 2);
        assert_eq!(app.autotone_total, 2);
        assert_eq!(app.autotone_done, 0);

        // Deliver thumbnails deterministically, without background decoding.
        app.loader = Some(crate::loader::Loader::new(16384));
        for (index, path) in [&a, &b].into_iter().enumerate() {
            app.loader.as_mut().unwrap().insert_thumb_external(
                path.clone(),
                THUMB_PX,
                std::sync::Arc::new(crate::image_decode::DecodedImage {
                    width: 8,
                    height: 8,
                    rgba: [32, 32, 32, 255].repeat(64),
                    pixel_format: crate::image_decode::PixelFormat::Srgb8,
                }),
            );
            app.poll_auto_tone(&[(path.clone(), THUMB_PX)]);
            assert!(app.edits.contains_key(path), "each request must be toned");
            if index == 0 {
                assert!(app.autotone_pending.contains(&b));
                assert_eq!(app.autotone_done, 1);
                assert_eq!(app.autotone_total, 2);
            }
        }
        assert!(app.autotone_pending.is_empty());
        assert_eq!(app.autotone_done, 0);
        assert_eq!(app.autotone_total, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn poisoned_thumbnail_queue_terminates_auto_tone_batch() {
        let (mut app, dir, a, b) = grid_with_two_photos("autotone-poison");
        let mut loader = crate::loader::Loader::new(16384);
        loader.poison_thumb_queue_for_test(a.clone(), THUMB_PX);
        app.loader = Some(loader);
        app.auto_tone_batch(vec![a.clone()]);
        assert!(app.autotone_pending.contains(&a));
        let loader = app.loader.as_mut().unwrap();
        loader.poll_all();
        assert!(loader.thumb_failed(&a, THUMB_PX));
        app.auto_tone_batch(vec![b.clone()]);

        let loader = app.loader.as_mut().unwrap();
        let (_, arrivals, _, _) = loader.poll_all();
        assert!(loader.thumb_failed(&a, THUMB_PX));
        assert!(loader.thumb_failed(&b, THUMB_PX));
        app.poll_auto_tone(&arrivals);
        assert!(app.autotone_pending.is_empty());
        assert_eq!(app.autotone_total, 0);
        assert!(!app.edits.contains_key(&a));
        assert!(!app.edits.contains_key(&b));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Loupe case is unchanged, and must stay on the in-memory histogram
    /// sample rather than falling back to the thumbnail: it is a better sample
    /// and needs no decode.
    #[test]
    fn cmd_u_in_the_loupe_tones_the_open_photo_from_its_histogram_sample() {
        let (mut app, dir, a, _b) = grid_with_two_photos("autotone-loupe");
        app.mode = ViewMode::Loupe;
        app.shown = Shown::Preview(a.clone(), 1024, 1024);
        app.hist_sample = vec![[0.02f32; 3]; 64];
        app.sel = None;
        app.want = Some(a.clone());

        app.auto_tone_one();

        assert!(
            app.edits.contains_key(&a),
            "the open photo must be toned inline, with no decode queued"
        );
        assert!(app.autotone_pending.is_empty(), "nothing should be pending");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bug this exists for: `tone_one` writes through whichever catalog is
    /// active *now*, and `Catalog` keys its records by filename alone. A batch
    /// left waiting on folder A's thumbnails while the user moves to folder B
    /// would therefore fold A's auto adjustments into B's record of the same
    /// filename, and write that record back out to A's sidecar — losing A's
    /// rating, rotation and touch-ups. Switching folders has to drop the
    /// outstanding work instead.
    #[test]
    fn switching_folders_cancels_a_pending_auto_tone_batch() {
        let dir = std::env::temp_dir().join(format!("lp-autotone-switch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(None);
        let stale = PathBuf::from("/folder-a/IMG_0001.jpg");
        app.autotone_pending.insert(stale.clone());
        app.autotone_done = 1;
        app.autotone_total = 3;

        app.load_playlist(Playlist::from_dir(&dir), dir.clone());

        assert!(
            app.autotone_pending.is_empty(),
            "folder B must not inherit folder A's outstanding Auto Tone work"
        );
        assert_eq!(app.autotone_done, 0);
        assert_eq!(app.autotone_total, 0);

        // A thumbnail that lands after the switch must now be inert.
        app.poll_auto_tone(&[(stale.clone(), THUMB_PX)]);
        assert!(
            !app.edits.contains_key(&stale),
            "a late arrival from folder A must not be toned against folder B's catalog"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
