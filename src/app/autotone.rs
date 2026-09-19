//! Auto Tone for one photo or the whole selection. The analysis lives in
//! `crate::autotone`; this file finds pixels for it and saves the result.
//! The shown photo reuses its histogram sample. Other photos use their cached
//! thumbnail, and missing thumbnails are requested and toned as they arrive.

use super::*;
use std::path::Path;

use crate::autotone;
use crate::image_ops;
use crate::thumbnail::THUMB_PX;

/// Longest-side sample count for thumbnail analysis. Matches
/// `build_hist_sample`, so the Loupe and a batch see the same statistics.
const ANALYSIS_TARGET: usize = 256;

#[derive(Clone, Copy)]
enum DeferredAutoToneMode {
    Replace,
    Append,
}

impl App {
    /// Auto Tone the photo on screen from its histogram sample. Applies this
    /// frame, or is deferred until the catalog finishes loading.
    pub(super) fn auto_tone_shown(&mut self) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        if self.catalog_load_pending.is_some() {
            self.auto_tone_batch(vec![path], DeferredAutoToneMode::Append);
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

    /// Auto Tone the selected photo (Cmd+U). Target the selection, not
    /// `shown`: in the Grid, `shown` is still the photo last opened in the
    /// Loupe, not the one under the cursor.
    pub(super) fn auto_tone_one(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };
        if self.catalog_load_pending.is_some() {
            self.auto_tone_batch(vec![path], DeferredAutoToneMode::Append);
            return;
        }
        if Some(path.as_path()) == self.shown.path() && !self.hist_sample.is_empty() {
            self.auto_tone_shown();
            return;
        }
        self.enqueue_auto_tone(vec![path]);
    }

    /// Auto Tone every selected photo. Photos without a cached thumbnail finish
    /// in `poll_auto_tone` as thumbnails arrive.
    pub(super) fn auto_tone_selection(&mut self) {
        self.auto_tone_batch(self.selected_paths(), DeferredAutoToneMode::Replace);
    }

    /// Start a batch over `paths`, or defer it while the catalog loads. When
    /// deferred, `Append` adds to the waiting list and `Replace` swaps it out.
    fn auto_tone_batch(&mut self, paths: Vec<PathBuf>, mode: DeferredAutoToneMode) {
        if paths.is_empty() {
            return;
        }
        if self.catalog_load_pending.is_some() {
            match mode {
                DeferredAutoToneMode::Replace => {
                    let mut seen = std::collections::HashSet::with_capacity(paths.len());
                    let paths = paths
                        .into_iter()
                        .filter(|path| seen.insert(path.clone()))
                        .collect();
                    self.autotone_deferred = Some(paths);
                }
                DeferredAutoToneMode::Append => {
                    let deferred = self.autotone_deferred.get_or_insert_with(Vec::new);
                    let mut seen: std::collections::HashSet<PathBuf> =
                        deferred.iter().cloned().collect();
                    for path in paths {
                        if seen.insert(path.clone()) {
                            deferred.push(path);
                        }
                    }
                }
            }
            self.set_status("Auto Tone will start after the catalog loads…".into());
            self.request_redraw();
            return;
        }
        self.autotone_pending.clear();
        self.autotone_base.clear();
        self.autotone_done = 0;
        self.enqueue_auto_tone(paths);
    }

    /// Add targets without abandoning outstanding work or counting duplicates.
    pub(super) fn enqueue_auto_tone(&mut self, paths: Vec<PathBuf>) {
        let mut wanted: Vec<PathBuf> = Vec::new();
        for path in paths {
            if self.autotone_pending.contains(&path) {
                continue;
            }
            // The shown photo's histogram sample needs no decode.
            if self.shown.path() == Some(path.as_path()) && !self.hist_sample.is_empty() {
                let auto = autotone::analyze(&self.hist_sample, self.hist_pixel_format);
                self.tone_one(&path, &auto);
                continue;
            }
            match self.analyze_thumb(&path) {
                Some(auto) => self.tone_one(&path, &auto),
                None => {
                    // Record the edits now, so a hand edit made while the
                    // thumbnail loads can be detected later.
                    self.autotone_base
                        .insert(path.clone(), self.edits.get(&path).copied().unwrap_or_default());
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

    /// Tone thumbnails that just arrived and drop ones that failed to decode.
    /// Runs every frame, not only on arrivals, so a batch whose last
    /// thumbnails all fail still finishes.
    pub(crate) fn poll_auto_tone(&mut self, arrivals: &[(PathBuf, u32)]) {
        if self.autotone_pending.is_empty() {
            return;
        }
        // Collect failures before arrivals. A late worker can still deliver a
        // thumbnail the loader already marked failed, and that path must not
        // be both dropped and toned.
        let mut dropped: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        if let Some(loader) = &self.loader {
            for path in &self.autotone_pending {
                if loader.thumb_failed(path, THUMB_PX) {
                    dropped.insert(path.clone());
                }
            }
        }
        let mut toned: Vec<(PathBuf, crate::develop::Adjustments)> = Vec::new();
        for (path, px) in arrivals {
            if *px != THUMB_PX || !self.autotone_pending.contains(path) || dropped.contains(path)
            {
                continue;
            }
            match self.analyze_thumb(path) {
                Some(auto) => toned.push((path.clone(), auto)),
                // A degenerate buffer. Waiting on it again would never finish.
                None => {
                    dropped.insert(path.clone());
                }
            }
        }
        for path in &dropped {
            self.autotone_pending.remove(path);
            self.autotone_base.remove(path);
        }
        for (path, auto) in toned {
            self.autotone_pending.remove(&path);
            let base = self.autotone_base.remove(&path);
            // If the user edited this photo while its thumbnail loaded, skip
            // it rather than overwrite their edit.
            let current = self.edits.get(&path).copied().unwrap_or_default();
            if base.is_some_and(|base| base != current) {
                continue;
            }
            self.tone_one(&path, &auto);
        }
        self.report_auto_tone_progress();
        self.request_redraw();
    }

    /// Drop a batch's waiting photos. Call on every folder change: `tone_one`
    /// writes to the active catalog, which keys records by filename only, so a
    /// late photo from the old folder would corrupt a same-named record.
    pub(super) fn cancel_auto_tone(&mut self) {
        if self.autotone_pending.is_empty() && self.autotone_deferred.is_none() {
            return;
        }
        let (done, total) = (self.autotone_done, self.autotone_total());
        self.autotone_pending.clear();
        self.autotone_base.clear();
        self.autotone_deferred = None;
        self.autotone_done = 0;
        self.set_status(format!("Auto Tone stopped at {done}/{total}"));
    }

    /// Batch size: photos toned plus photos waiting. Dropped or skipped photos
    /// count toward neither, so the total shrinks.
    fn autotone_total(&self) -> usize {
        self.autotone_done + self.autotone_pending.len()
    }

    /// Analyze `path`'s cached thumbnail. `None` when it is not in memory.
    fn analyze_thumb(&self, path: &Path) -> Option<crate::develop::Adjustments> {
        let img = self.loader.as_ref()?.get_thumb(path, THUMB_PX)?;
        let (grid, _, _) = image_ops::downsample_linear(&img, ANALYSIS_TARGET);
        (!grid.is_empty()).then(|| autotone::analyze(&grid, img.pixel_format))
    }

    /// Save one photo's auto adjustments, keeping its crop, white balance,
    /// saturation and denoise. Thumbnails rebuild on their own because their
    /// cache key includes the edits.
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
        if self.shown.path() == Some(path) {
            self.push_adjustments();
            self.hist_dirty = true;
        }
    }

    fn report_auto_tone_progress(&mut self) {
        let done = self.autotone_done;
        let total = self.autotone_total();
        if total == 0 {
            return;
        }
        if self.autotone_pending.is_empty() {
            // Cmd+U lands here for one photo when its thumbnail had to load.
            self.set_status(if done == 1 {
                "Auto Tone applied".to_string()
            } else {
                format!("Auto Tone applied to {done} photos")
            });
            self.autotone_done = 0;
        } else {
            self.set_status(format!("Auto Tone {done}/{total}\u{2026}"));
        }
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;

    /// An app showing a real folder of two empty photos in the Grid. Nothing
    /// decodes them, so neither has a thumbnail in memory.
    fn grid_with_two_photos(tag: &str) -> (App, PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.jpg"), dir.join("b.jpg"));
        std::fs::write(&a, []).unwrap();
        std::fs::write(&b, []).unwrap();

        let mut app = App::new(None);
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        // Auto Tone waits for the async catalog load, so let it finish first.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.poll_catalog_load() {
            assert!(
                std::time::Instant::now() < deadline,
                "catalog load timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        app.mode = ViewMode::Grid;
        (app, dir, a, b)
    }

    /// In the Grid, `shown` is still the photo last opened in the Loupe. Cmd+U
    /// must tone the photo under the cursor instead.
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
        assert_eq!(app.autotone_total(), 2);
        assert_eq!(app.autotone_done, 0);

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
                assert_eq!(app.autotone_total(), 2);
            }
        }
        assert!(app.autotone_pending.is_empty());
        assert_eq!(app.autotone_done, 0);
        assert_eq!(app.autotone_total(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn poisoned_thumbnail_queue_terminates_auto_tone_batch() {
        let (mut app, dir, a, b) = grid_with_two_photos("autotone-poison");
        let mut loader = crate::loader::Loader::new(16384);
        loader.poison_thumb_queue_for_test(a.clone(), THUMB_PX);
        app.loader = Some(loader);
        app.auto_tone_batch(vec![a.clone()], DeferredAutoToneMode::Replace);
        assert!(app.autotone_pending.contains(&a));
        let loader = app.loader.as_mut().unwrap();
        loader.poll_all();
        assert!(loader.thumb_failed(&a, THUMB_PX));
        app.auto_tone_batch(vec![b.clone()], DeferredAutoToneMode::Replace);

        let loader = app.loader.as_mut().unwrap();
        let (_, arrivals, _, _) = loader.poll_all();
        assert!(loader.thumb_failed(&a, THUMB_PX));
        assert!(loader.thumb_failed(&b, THUMB_PX));
        app.poll_auto_tone(&arrivals);
        assert!(app.autotone_pending.is_empty());
        assert_eq!(app.autotone_total(), 0);
        assert!(!app.edits.contains_key(&a));
        assert!(!app.edits.contains_key(&b));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// In the Loupe, Cmd+U tones from the histogram sample with no decode.
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

    /// A thumbnail from folder A arriving after a switch to folder B must not
    /// be toned against B's catalog. See `cancel_auto_tone`.
    #[test]
    fn switching_folders_cancels_a_pending_auto_tone_batch() {
        let dir = std::env::temp_dir().join(format!("lp-autotone-switch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(None);
        let stale = PathBuf::from("/folder-a/IMG_0001.jpg");
        app.autotone_pending.insert(stale.clone());
        app.autotone_done = 1;

        app.load_playlist(Playlist::from_dir(&dir), dir.clone());

        assert!(
            app.autotone_pending.is_empty(),
            "folder B must not inherit folder A's outstanding Auto Tone work"
        );
        assert_eq!(app.autotone_done, 0);
        assert_eq!(app.autotone_total(), 0);

        app.poll_auto_tone(&[(stale.clone(), THUMB_PX)]);
        assert!(
            !app.edits.contains_key(&stale),
            "a late arrival from folder A must not be toned against folder B's catalog"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shown_photo_auto_tone_waits_for_catalog_load() {
        let (mut app, dir, a, _b) = grid_with_two_photos("autotone-catalog-race");
        app.shown = Shown::Preview(a.clone(), 1024, 1024);
        app.hist_sample = vec![[0.02f32; 3]; 64];
        app.catalog_load_pending = Some((dir.clone(), 1));

        app.auto_tone_shown();

        assert_eq!(app.autotone_deferred, Some(vec![a.clone()]));
        assert!(!app.edits.contains_key(&a));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cmd_u_in_the_grid_waits_for_catalog_load() {
        let (mut app, dir, a, b) = grid_with_two_photos("autotone-grid-catalog-race");
        app.shown = Shown::Preview(a, 1024, 1024);
        app.hist_sample = vec![[0.02f32; 3]; 64];
        app.sel = app
            .visible
            .iter()
            .position(|&i| app.playlist.as_ref().and_then(|pl| pl.entry(i)) == Some(b.as_path()));
        assert!(app.sel.is_some());
        app.catalog_load_pending = Some((dir.clone(), 1));

        app.auto_tone_one();

        assert_eq!(app.autotone_deferred, Some(vec![b.clone()]));
        assert!(!app.edits.contains_key(&b));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn successive_cmd_u_requests_during_catalog_load_are_preserved_once() {
        let (mut app, dir, a, b) = grid_with_two_photos("autotone-successive-catalog-race");
        app.catalog_load_pending = Some((dir.clone(), 1));

        for path in [&a, &b, &a] {
            app.sel = app.visible.iter().position(|&i| {
                app.playlist.as_ref().and_then(|pl| pl.entry(i)) == Some(path.as_path())
            });
            assert!(app.sel.is_some());
            app.auto_tone_one();
        }

        assert_eq!(app.autotone_deferred, Some(vec![a.clone(), b.clone()]));
        assert!(app.autotone_pending.is_empty());
        assert!(!app.edits.contains_key(&a));
        assert!(!app.edits.contains_key(&b));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn selection_request_replaces_deferred_single_photo_requests() {
        let (mut app, dir, a, b) = grid_with_two_photos("autotone-selection-catalog-race");
        let c = dir.join("c.jpg");
        app.catalog_load_pending = Some((dir.clone(), 1));

        app.auto_tone_batch(vec![a.clone()], DeferredAutoToneMode::Append);
        app.auto_tone_batch(
            vec![b.clone(), b.clone(), c.clone()],
            DeferredAutoToneMode::Replace,
        );

        assert_eq!(app.autotone_deferred, Some(vec![b, c]));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cmd_u_with_empty_histogram_waits_for_catalog_load() {
        let (mut app, dir, a, _b) = grid_with_two_photos("autotone-empty-hist-catalog-race");
        app.mode = ViewMode::Loupe;
        app.shown = Shown::Preview(a.clone(), 1024, 1024);
        app.sel = None;
        app.want = Some(a.clone());
        app.hist_sample.clear();
        app.catalog_load_pending = Some((dir.clone(), 1));

        app.auto_tone_one();

        assert_eq!(app.autotone_deferred, Some(vec![a.clone()]));
        assert!(!app.edits.contains_key(&a));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hand edit made while the photo's thumbnail loads must survive the batch.
    #[test]
    fn manual_edit_made_while_pending_survives_the_batch() {
        let (mut app, dir, a, _b) = grid_with_two_photos("autotone-manual-edit-race");
        app.auto_tone_batch(vec![a.clone()], DeferredAutoToneMode::Replace);
        assert!(app.autotone_pending.contains(&a), "thumbnail not resident yet");

        let manual = crate::develop::Adjustments {
            exposure: 1.23,
            ..Default::default()
        };
        app.edits.insert(a.clone(), manual);

        app.loader = Some(crate::loader::Loader::new(16384));
        app.loader.as_mut().unwrap().insert_thumb_external(
            a.clone(),
            THUMB_PX,
            std::sync::Arc::new(crate::image_decode::DecodedImage {
                width: 8,
                height: 8,
                rgba: [32, 32, 32, 255].repeat(64),
                pixel_format: crate::image_decode::PixelFormat::Srgb8,
            }),
        );
        app.poll_auto_tone(&[(a.clone(), THUMB_PX)]);

        assert_eq!(
            app.edits.get(&a).copied(),
            Some(manual),
            "the manual edit must survive untouched, not be merged with a stale auto result"
        );
        assert!(
            app.autotone_pending.is_empty(),
            "the photo must still be dropped from the batch, not left waiting forever"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
