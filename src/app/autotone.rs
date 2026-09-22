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

/// How many of a batch's thumbnails may be outstanding at once.
///
/// A batch used to request every photo's thumbnail up front, which made the
/// cache hold the whole selection: about 0.4 MB a photo, so a 20k selection
/// asked for roughly 8 GB, and on wasm32 that is a 32-bit address space that
/// never shrinks. Nothing needs that. A thumbnail only has to survive from
/// landing to the next `poll_auto_tone`.
///
/// 32 is sized from the two measured halves. Analysing one photo costs about
/// 8 ms on the UI thread and cannot be parallelised, while a thumbnail decode
/// is 1.2 ms warm, 12 ms cold, and 137 ms at P95 for a cold RAW. A window has
/// to cover the slowest decode divided by one analysis to keep the workers
/// ahead of the UI thread, which is 17 for that RAW case. 32 has margin and
/// still costs about 13 MB whatever the folder holds.
const AUTOTONE_WINDOW: usize = 32;

/// The window is a memory budget, so it has a ceiling as well as a value. At
/// roughly 0.4 MB a cached thumbnail, 64 is about 26 MB, and past that a batch
/// starts to look like the unbounded one this replaced.
const _: () = assert!(AUTOTONE_WINDOW <= 64);

/// How long one frame may spend analysing. Without a cap, a frame that found a
/// full window ready would analyse all 32 and freeze the window for a quarter
/// of a second.
const AUTOTONE_FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(5);

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
            self.set_status(crate::i18n::t().auto_tone_needs_load.into());
            return;
        }
        let auto = autotone::analyze(&self.hist_sample, self.hist_pixel_format);
        let merged = autotone::merge(&self.current_adjustments(), &auto);
        self.apply_adjustments_kind(merged, "auto_tone");
        self.set_status(crate::i18n::t().auto_tone_applied.into());
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
            self.set_status(crate::i18n::t().auto_tone_waits_for_catalog.into());
            self.request_redraw();
            return;
        }
        self.autotone_pending.clear();
        self.autotone_queue.clear();
        self.autotone_window.clear();
        self.autotone_base.clear();
        self.autotone_done = 0;
        self.enqueue_auto_tone(paths);
    }

    /// Add targets without abandoning outstanding work or counting duplicates.
    ///
    /// Photos are only queued here, never analysed. Analysis costs about 8 ms
    /// each, so toning the already-cached ones inline would freeze the window
    /// for as long as the selection is large. `poll_auto_tone` does all of it
    /// under a frame budget instead, one frame later at worst.
    pub(super) fn enqueue_auto_tone(&mut self, paths: Vec<PathBuf>) {
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
            // Record the edits now, so a hand edit made while the photo waits
            // its turn can be detected later.
            self.autotone_base
                .insert(path.clone(), self.edits.get(&path).copied().unwrap_or_default());
            self.autotone_pending.insert(path.clone());
            self.autotone_queue.push_back(path);
        }
        self.pump_auto_tone();
        self.report_auto_tone_progress();
        self.request_redraw();
    }

    /// Refill the window from the queue, requesting each admitted photo's
    /// thumbnail. This is the only place a batch asks the loader for anything,
    /// so `AUTOTONE_WINDOW` is a hard ceiling on what the cache must hold.
    fn pump_auto_tone(&mut self) {
        let Some(loader) = &mut self.loader else {
            return;
        };
        while self.autotone_window.len() < AUTOTONE_WINDOW {
            let Some(path) = self.autotone_queue.pop_front() else {
                return;
            };
            loader.request_thumb(path.clone(), THUMB_PX);
            self.autotone_window.push_back(path);
        }
    }

    /// Tone the window's thumbnails that have landed and drop the ones that
    /// failed. Runs every frame, not only on arrivals, so a batch whose last
    /// thumbnails all fail still finishes.
    ///
    /// Only the window is examined, never the whole batch, so the per-frame
    /// cost is bounded by `AUTOTONE_WINDOW` and not by the selection. Analysis
    /// stops at `AUTOTONE_FRAME_BUDGET` and resumes next frame.
    pub(crate) fn poll_auto_tone(&mut self) {
        if self.autotone_pending.is_empty() {
            return;
        }
        self.pump_auto_tone();

        let deadline = Instant::now() + AUTOTONE_FRAME_BUDGET;
        let mut dropped: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut toned: Vec<(PathBuf, crate::develop::Adjustments)> = Vec::new();
        let mut waiting: VecDeque<PathBuf> = VecDeque::new();
        let mut spent = false;
        while let Some(path) = self.autotone_window.pop_front() {
            let thumb = self.loader.as_ref().and_then(|loader| {
                if loader.thumb_failed(&path, THUMB_PX) {
                    None
                } else {
                    loader.get_thumb(&path, THUMB_PX)
                }
            });
            let Some(img) = thumb else {
                // Failed decodes must leave the batch or it never finishes;
                // everything else is still in the pool.
                match self.loader.as_ref() {
                    Some(loader) if loader.thumb_failed(&path, THUMB_PX) => {
                        dropped.insert(path);
                    }
                    _ => waiting.push_back(path),
                }
                continue;
            };
            // Hold the rest of the window for the next frame once the budget is
            // gone, rather than freezing on a full window of analyses.
            if spent {
                waiting.push_back(path);
                continue;
            }
            let (grid, _, _) = image_ops::downsample_linear(&img, ANALYSIS_TARGET);
            if grid.is_empty() {
                // A degenerate buffer. Waiting on it again would never finish.
                dropped.insert(path);
                continue;
            }
            toned.push((path, autotone::analyze(&grid, img.pixel_format)));
            spent = Instant::now() >= deadline;
        }
        self.autotone_window = waiting;

        for path in &dropped {
            self.autotone_pending.remove(path);
            self.autotone_base.remove(path);
        }
        for (path, auto) in toned {
            let still_wanted = self.autotone_pending.remove(&path);
            let base = self.autotone_base.remove(&path);
            // A photo trashed, or a batch cancelled, while this thumbnail
            // loaded. Toning it now would write a sidecar for a file that is
            // no longer there.
            if !still_wanted {
                continue;
            }
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
        self.autotone_queue.clear();
        self.autotone_window.clear();
        self.autotone_base.clear();
        self.autotone_deferred = None;
        self.autotone_done = 0;
        self.set_status((crate::i18n::t().auto_tone_stopped)(done, total));
    }

    /// Batch size: photos toned plus photos waiting. Dropped or skipped photos
    /// count toward neither, so the total shrinks.
    fn autotone_total(&self) -> usize {
        self.autotone_done + self.autotone_pending.len()
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
            #[cfg(target_arch = "wasm32")]
            if done > 0 {
                crate::analytics::property("develop_edit_applied", "edit_kind", "auto_tone");
            }
            // Cmd+U lands here for one photo when its thumbnail had to load.
            let t = crate::i18n::t();
            self.set_status(if done == 1 {
                t.auto_tone_applied.to_string()
            } else {
                (t.auto_tone_applied_n)(done)
            });
            self.autotone_done = 0;
        } else {
            self.set_status((crate::i18n::t().auto_tone_progress)(done, total));
        }
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use crate::image_decode::{DecodedImage, DecodedImageFields};

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
                std::sync::Arc::new(DecodedImage::new_tracked(DecodedImageFields {
                    width: 8,
                    height: 8,
                    rgba: [32, 32, 32, 255].repeat(64),
                    pixel_format: crate::image_decode::PixelFormat::Srgb8,
                })),
            );
            app.poll_auto_tone();
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

    /// A batch used to request every photo's thumbnail at once, which made the
    /// cache hold the whole selection. The window is what keeps a 20k-photo
    /// selection from asking for 20k decoded thumbnails.
    #[test]
    fn a_large_batch_only_requests_a_window_of_thumbnails_at_a_time() {
        let dir = std::env::temp_dir().join(format!("lp-autotone-window-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let photos: Vec<PathBuf> = (0..AUTOTONE_WINDOW * 3)
            .map(|i| {
                let p = dir.join(format!("p{i:03}.jpg"));
                std::fs::write(&p, []).unwrap();
                p
            })
            .collect();

        let mut app = App::new(None);
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while app.poll_catalog_load() {
            assert!(Instant::now() < deadline, "catalog load timed out");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        app.mode = ViewMode::Grid;
        app.loader = Some(crate::loader::Loader::new(16384));

        app.auto_tone_batch(photos.clone(), DeferredAutoToneMode::Replace);

        let in_flight = app.loader.as_ref().unwrap().thumbs_in_flight();
        assert!(
            in_flight <= AUTOTONE_WINDOW && in_flight < photos.len(),
            "the batch must ask for a window, not all {} of its photos, got {in_flight}",
            photos.len()
        );
        assert_eq!(
            app.autotone_window.len(),
            AUTOTONE_WINDOW,
            "the batch must ask for exactly one window up front"
        );
        assert_eq!(
            app.autotone_queue.len(),
            photos.len() - AUTOTONE_WINDOW,
            "the rest must wait their turn, not be requested"
        );
        assert_eq!(
            app.autotone_total(),
            photos.len(),
            "progress must still count the whole batch"
        );

        // Land one window's thumbnails. The next poll tones what it can inside
        // its frame budget and refills from the queue, so the window stays
        // capped however much of the batch is left.
        for path in app.autotone_window.clone() {
            app.loader.as_mut().unwrap().insert_thumb_external(
                path,
                THUMB_PX,
                std::sync::Arc::new(DecodedImage::new_tracked(DecodedImageFields {
                    width: 8,
                    height: 8,
                    rgba: [32, 32, 32, 255].repeat(64),
                    pixel_format: crate::image_decode::PixelFormat::Srgb8,
                })),
            );
        }
        app.poll_auto_tone();

        assert!(app.autotone_done > 0, "the poll must make progress");
        assert!(
            app.autotone_window.len() <= AUTOTONE_WINDOW,
            "the window must stay capped after a refill, got {}",
            app.autotone_window.len()
        );
        assert!(
            app.loader.as_ref().unwrap().thumbs_in_flight() <= AUTOTONE_WINDOW,
            "and the loader must still be inside it, got {}",
            app.loader.as_ref().unwrap().thumbs_in_flight()
        );
        assert_eq!(
            app.autotone_done + app.autotone_queue.len() + app.autotone_window.len(),
            photos.len(),
            "every photo must be toned, queued or in the window"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Analysis costs about 8 ms a photo on the UI thread, so a poll that found
    /// a full window ready and toned all of it would freeze the window for a
    /// quarter of a second. The budget is what turns that into a progress bar.
    #[test]
    fn one_poll_does_not_analyse_a_whole_window_of_photos() {
        let dir = std::env::temp_dir().join(format!("lp-autotone-budget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let photos: Vec<PathBuf> = (0..AUTOTONE_WINDOW)
            .map(|i| {
                let p = dir.join(format!("p{i:03}.jpg"));
                std::fs::write(&p, []).unwrap();
                p
            })
            .collect();

        let mut app = App::new(None);
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while app.poll_catalog_load() {
            assert!(Instant::now() < deadline, "catalog load timed out");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        app.mode = ViewMode::Grid;
        app.loader = Some(crate::loader::Loader::new(16384));
        app.auto_tone_batch(photos.clone(), DeferredAutoToneMode::Replace);

        // Thumbnail-sized, and a gradient rather than a flat fill, so analysis
        // does the work a real photo costs instead of short-circuiting.
        let side = 512usize;
        let mut rgba = Vec::with_capacity(side * side * 4);
        for y in 0..side {
            for x in 0..side {
                let v = ((x + y) % 256) as u8;
                rgba.extend_from_slice(&[v, v.wrapping_add(64), v.wrapping_add(128), 255]);
            }
        }
        let img = std::sync::Arc::new(DecodedImage::new_tracked(DecodedImageFields {
            width: side as u32,
            height: side as u32,
            rgba,
            pixel_format: crate::image_decode::PixelFormat::Srgb8,
        }));
        for path in app.autotone_window.clone() {
            app.loader
                .as_mut()
                .unwrap()
                .insert_thumb_external(path, THUMB_PX, img.clone());
        }

        app.poll_auto_tone();

        assert!(app.autotone_done > 0, "a poll must make progress");
        assert!(
            app.autotone_done < photos.len(),
            "a poll must stop at its budget, not tone all {} ready photos",
            photos.len()
        );

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
        loader.poll_all();
        assert!(loader.thumb_failed(&a, THUMB_PX));
        assert!(loader.thumb_failed(&b, THUMB_PX));
        app.poll_auto_tone();
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

        app.poll_auto_tone();
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
    /// A photo can leave the batch between its thumbnail being requested and
    /// landing, because a bulk delete trashed it. Toning it then would write a
    /// sidecar for a file that is already in the Trash.
    #[test]
    fn a_photo_dropped_while_its_thumbnail_loaded_is_not_toned() {
        let (mut app, dir, a, b) = grid_with_two_photos("autotone-dropped");
        app.auto_tone_batch(vec![a.clone(), b.clone()], DeferredAutoToneMode::Replace);
        assert!(app.autotone_pending.contains(&a), "thumbnail not resident yet");

        // What `forget_photo` does when the trash call for `a` lands. Its
        // thumbnail request is already in the window.
        app.autotone_pending.remove(&a);
        app.autotone_base.remove(&a);

        app.loader = Some(crate::loader::Loader::new(16384));
        app.loader.as_mut().unwrap().insert_thumb_external(
            a.clone(),
            THUMB_PX,
            std::sync::Arc::new(DecodedImage::new_tracked(DecodedImageFields {
                width: 8,
                height: 8,
                rgba: [32, 32, 32, 255].repeat(64),
                pixel_format: crate::image_decode::PixelFormat::Srgb8,
            })),
        );
        app.poll_auto_tone();

        assert_eq!(
            app.edits.get(&a),
            None,
            "a photo that left the batch must not be toned"
        );
        assert!(app.catalog.adjustments(&a).is_identity());

        let _ = std::fs::remove_dir_all(&dir);
    }

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
            std::sync::Arc::new(DecodedImage::new_tracked(DecodedImageFields {
                width: 8,
                height: 8,
                rgba: [32, 32, 32, 255].repeat(64),
                pixel_format: crate::image_decode::PixelFormat::Srgb8,
            })),
        );
        app.poll_auto_tone();

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
