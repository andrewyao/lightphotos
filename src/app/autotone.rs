//! Auto Tone for one photo or the whole selection. The analysis lives in
//! `crate::autotone`; this file finds pixels for it and saves the result.
//! The shown photo reuses its histogram sample. Other photos use their cached
//! thumbnail, and missing thumbnails are requested and toned as they arrive.

use super::*;
use std::path::Path;

use crate::autotone;
use crate::develop::image_ops;
use crate::develop::Adjustments;
use crate::jobs::thumbnail::THUMB_PX;

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

/// One Auto Tone batch's bookkeeping. App's other modules see it only
/// through the methods below.
pub(super) struct AutoTone {
    /// Photos in the running Auto Tone batch still waiting on a thumbnail.
    /// Emptied by `cancel_auto_tone` on a folder change.
    ///
    /// The batch is paced across three stages so its memory never tracks the
    /// selection: this set is the whole outstanding batch, for progress and
    /// deduplication, and every photo in it sits in exactly one of
    /// `queue` or `window`.
    pending: HashSet<PathBuf>,
    /// Batch photos whose thumbnail has not been asked for yet, in the order
    /// they will be. Unbounded, but a `PathBuf` each, not a decoded thumbnail.
    queue: VecDeque<PathBuf>,
    /// Batch photos whose thumbnail has been requested, oldest first. Capped at
    /// `AUTOTONE_WINDOW`, and this is the only part of a batch the thumbnail
    /// cache has to hold at once.
    window: VecDeque<PathBuf>,
    /// Each pending photo's edits when it was queued. If they changed by the
    /// time its thumbnail lands, the user edited by hand, and `tone_one` must
    /// not overwrite that.
    base: HashMap<PathBuf, Adjustments>,
    /// Targets waiting for the sidecar scan, so Auto Tone snapshots their
    /// saved edits and not an empty default.
    deferred: Option<Vec<PathBuf>>,
    /// Photos toned so far in the running batch. The total is this plus
    /// `pending.len()`.
    done: usize,
    /// How a normal photo is centered, from Settings.
    centering: autotone::Centering,
}

impl AutoTone {
    pub(super) fn new(centering: autotone::Centering) -> Self {
        Self {
            pending: HashSet::new(),
            queue: VecDeque::new(),
            window: VecDeque::new(),
            base: HashMap::new(),
            deferred: None,
            done: 0,
            centering,
        }
    }

    pub(super) fn centering(&self) -> autotone::Centering {
        self.centering
    }

    pub(super) fn set_centering(&mut self, centering: autotone::Centering) {
        self.centering = centering;
    }

    /// Whether a batch has photos left to tone.
    pub(super) fn is_running(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Batch photos whose thumbnail is requested and not yet toned.
    pub(super) fn window_len(&self) -> usize {
        self.window.len()
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) fn pending(&self) -> impl Iterator<Item = &PathBuf> {
        self.pending.iter()
    }

    /// The batch that waited for the catalog, once it has loaded.
    pub(super) fn take_deferred(&mut self) -> Option<Vec<PathBuf>> {
        self.deferred.take()
    }

    /// Stops toning `path`, which has gone to the Trash.
    pub(super) fn forget(&mut self, path: &Path) {
        self.pending.remove(path);
        self.base.remove(path);
    }

    /// Drops trashed photos from a batch waiting for the catalog, and the
    /// batch with them if none are left.
    pub(super) fn drop_deferred(&mut self, gone: &HashSet<PathBuf>) {
        if let Some(deferred) = self.deferred.as_mut() {
            deferred.retain(|p| !gone.contains(p));
            if deferred.is_empty() {
                self.deferred = None;
            }
        }
    }
}

#[cfg(test)]
impl AutoTone {
    pub(super) fn deferred(&self) -> Option<&[PathBuf]> {
        self.deferred.as_deref()
    }

    pub(super) fn defer(&mut self, paths: Vec<PathBuf>) {
        self.deferred = Some(paths);
    }

    pub(super) fn is_pending(&self, path: &Path) -> bool {
        self.pending.contains(path) || self.base.contains_key(path)
    }
}

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
        if self.catalog_loading() {
            self.auto_tone_batch(vec![path], DeferredAutoToneMode::Append);
            return;
        }
        let Some((sample, format)) = self.hist.sample() else {
            self.set_status(
                StatusKind::Error,
                crate::i18n::t().auto_tone_needs_load.into(),
            );
            return;
        };
        let auto = autotone::analyze(sample, format, self.autotone.centering);
        let merged = autotone::merge(&self.current_adjustments(), &auto);
        self.apply_adjustments_kind(merged, "auto_tone");
        self.set_status(
            StatusKind::Success,
            crate::i18n::t().auto_tone_applied.into(),
        );
    }

    /// Auto Tone the selection (Cmd+U and the Auto Adjust button). Several
    /// photos run as a batch. One photo is the selection, not `shown`: in the
    /// Grid, `shown` is still the photo last opened in the Loupe, not the one
    /// under the cursor.
    pub(super) fn auto_tone_selected(&mut self) {
        if self.action_count() > 1 {
            self.auto_tone_batch(self.action_paths(), DeferredAutoToneMode::Append);
            return;
        }
        let Some(path) = self.selected_path() else {
            return;
        };
        if self.catalog_loading() {
            self.auto_tone_batch(vec![path], DeferredAutoToneMode::Append);
            return;
        }
        if Some(path.as_path()) == self.shown.path() && self.hist.sample().is_some() {
            self.auto_tone_shown();
            return;
        }
        self.enqueue_auto_tone(vec![path]);
    }

    /// Auto Tone every selected photo. Photos without a cached thumbnail finish
    /// in `poll_auto_tone` as thumbnails arrive.
    pub(super) fn auto_tone_selection(&mut self) {
        self.auto_tone_batch(self.action_paths(), DeferredAutoToneMode::Replace);
    }

    /// Auto Tone every photo in the grid, group members included.
    pub(super) fn auto_tone_all(&mut self) {
        self.auto_tone_batch(self.all_member_paths(), DeferredAutoToneMode::Replace);
    }

    /// Start a batch over `paths`, or defer it while the catalog loads. When
    /// deferred, `Append` adds to the waiting list and `Replace` swaps it out.
    fn auto_tone_batch(&mut self, paths: Vec<PathBuf>, mode: DeferredAutoToneMode) {
        if paths.is_empty() {
            return;
        }
        if self.catalog_loading() {
            match mode {
                DeferredAutoToneMode::Replace => {
                    let mut seen = std::collections::HashSet::with_capacity(paths.len());
                    let paths = paths
                        .into_iter()
                        .filter(|path| seen.insert(path.clone()))
                        .collect();
                    self.autotone.deferred = Some(paths);
                }
                DeferredAutoToneMode::Append => {
                    let deferred = self.autotone.deferred.get_or_insert_with(Vec::new);
                    let mut seen: std::collections::HashSet<PathBuf> =
                        deferred.iter().cloned().collect();
                    for path in paths {
                        if seen.insert(path.clone()) {
                            deferred.push(path);
                        }
                    }
                }
            }
            self.set_status(
                StatusKind::Error,
                crate::i18n::t().auto_tone_waits_for_catalog.into(),
            );
            self.request_redraw();
            return;
        }
        self.autotone.pending.clear();
        self.autotone.queue.clear();
        self.autotone.window.clear();
        self.autotone.base.clear();
        self.autotone.done = 0;
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
            if self.autotone.pending.contains(&path) {
                continue;
            }
            // The shown photo's histogram sample needs no decode.
            let shown_sample = self
                .hist
                .sample()
                .filter(|_| self.shown.path() == Some(path.as_path()));
            if let Some((sample, format)) = shown_sample {
                let auto = autotone::analyze(sample, format, self.autotone.centering);
                self.tone_one(&path, &auto);
                continue;
            }
            // Record the edits now, so a hand edit made while the photo waits
            // its turn can be detected later.
            self.autotone.base.insert(
                path.clone(),
                self.edits.get(&path).copied().unwrap_or_default(),
            );
            self.autotone.pending.insert(path.clone());
            self.autotone.queue.push_back(path);
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
        while self.autotone.window.len() < AUTOTONE_WINDOW {
            let Some(path) = self.autotone.queue.pop_front() else {
                return;
            };
            loader.request_thumb(path.clone(), THUMB_PX);
            self.autotone.window.push_back(path);
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
        if self.autotone.pending.is_empty() {
            return;
        }
        self.pump_auto_tone();

        let deadline = Instant::now() + AUTOTONE_FRAME_BUDGET;
        let mut dropped: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut toned: Vec<(PathBuf, crate::develop::Adjustments)> = Vec::new();
        let mut waiting: VecDeque<PathBuf> = VecDeque::new();
        let mut spent = false;
        while let Some(path) = self.autotone.window.pop_front() {
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
            let auto = autotone::analyze(&grid, img.pixel_format, self.autotone.centering);
            toned.push((path, auto));
            spent = Instant::now() >= deadline;
        }
        self.autotone.window = waiting;

        for path in &dropped {
            self.autotone.pending.remove(path);
            self.autotone.base.remove(path);
        }
        for (path, auto) in toned {
            let still_wanted = self.autotone.pending.remove(&path);
            let base = self.autotone.base.remove(&path);
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
        if self.autotone.pending.is_empty() && self.autotone.deferred.is_none() {
            return;
        }
        let (done, total) = (self.autotone.done, self.autotone_total());
        self.autotone.pending.clear();
        self.autotone.queue.clear();
        self.autotone.window.clear();
        self.autotone.base.clear();
        self.autotone.deferred = None;
        self.autotone.done = 0;
        self.set_status(
            StatusKind::Info,
            (crate::i18n::t().auto_tone_stopped)(done, total),
        );
    }

    /// Batch size: photos toned plus photos waiting. Dropped or skipped photos
    /// count toward neither, so the total shrinks.
    fn autotone_total(&self) -> usize {
        self.autotone.done + self.autotone.pending.len()
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
        #[cfg(target_arch = "wasm32")]
        if merged != base {
            crate::web::analytics::property("develop_edit_applied", "edit_kind", "auto_tone");
        }
        self.autotone.done += 1;
        if self.shown.path() == Some(path) {
            self.push_adjustments();
            self.hist.invalidate();
        }
    }

    fn report_auto_tone_progress(&mut self) {
        let done = self.autotone.done;
        let total = self.autotone_total();
        if total == 0 {
            return;
        }
        if self.autotone.pending.is_empty() {
            // Cmd+U lands here for one photo when its thumbnail had to load.
            let t = crate::i18n::t();
            self.set_status(
                StatusKind::Success,
                if done == 1 {
                    t.auto_tone_applied.to_string()
                } else {
                    (t.auto_tone_applied_n)(done)
                },
            );
            self.autotone.done = 0;
        } else {
            self.set_status(
                StatusKind::Progress,
                (crate::i18n::t().auto_tone_progress)(done, total),
            );
        }
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use crate::app::test_support::{folder_of, load_folder, temp_folder};
    use crate::decode::image_decode::{DecodedImage, DecodedImageFields};

    /// An app showing a real folder of two empty photos in the Grid. Nothing
    /// decodes them, so neither has a thumbnail in memory.
    fn grid_with_two_photos(tag: &str) -> (App, PathBuf, PathBuf, PathBuf) {
        let (dir, paths) = folder_of(tag, &["a.jpg".into(), "b.jpg".into()]);
        let mut app = App::new(None);
        // Auto Tone waits for the async catalog load, so let it finish first.
        load_folder(&mut app, &dir);
        app.mode = ViewMode::Grid;
        (app, dir, paths[0].clone(), paths[1].clone())
    }

    /// In the Grid, `shown` is still the photo last opened in the Loupe. Cmd+U
    /// must tone the photo under the cursor instead.
    #[test]
    fn auto_adjust_all_asks_first_then_takes_every_photo_and_group_member() {
        let (mut app, dir, _) = crate::app::test_support::folder_app("tone-all", 5);
        crate::app::nav::tests::group_photos(&mut app, &[0, 1, 2], 0);
        assert_eq!(app.selection_count(), 0);
        app.request_bulk(ui::BulkKind::AutoToneAll);
        let (kind, prompt) = app.pending_bulk_prompt().expect("it asks first");
        assert_eq!(kind, ui::BulkKind::AutoToneAll);
        assert_eq!(prompt, (crate::i18n::t().confirm_auto_tone)(5));
        assert_eq!(app.autotone_total(), 0, "nothing runs before the confirm");
        app.confirm_pending();
        assert_eq!(app.autotone_total(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cmd_u_in_the_grid_tones_the_cursor_photo_not_the_last_loupe_photo() {
        let (mut app, dir, a, b) = grid_with_two_photos("autotone-cursor");
        // Photo A was open in the Loupe, and still has its histogram sample.
        app.shown = Shown::Preview(a.clone(), 1024, 1024);
        app.hist.set_sample(vec![[0.02f32; 3]; 64]);
        // The grid cursor is on photo B.
        app.sel = app
            .visible
            .iter()
            .position(|&i| app.playlist.as_ref().and_then(|pl| pl.entry(i)) == Some(b.as_path()));
        assert!(
            app.sel.is_some(),
            "test setup: B must be in the visible grid"
        );

        app.auto_tone_selected();

        assert!(
            !app.edits.contains_key(&a),
            "the photo left over in the Loupe must not be touched"
        );
        assert!(
            app.autotone.pending.contains(&b),
            "the cursor photo must be the one queued for toning"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cmd_u_with_several_selected_tones_every_one() {
        let (mut app, dir, a, b) = grid_with_two_photos("autotone-several");
        app.selected = (0..2).collect();
        app.sel = Some(0);
        app.auto_tone_selected();
        assert_eq!(app.autotone_total(), 2);
        assert!(app.autotone.pending.contains(&a) && app.autotone.pending.contains(&b));
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
            app.auto_tone_selected();
        }
        assert_eq!(app.autotone.pending.len(), 2);
        assert_eq!(app.autotone_total(), 2);
        assert_eq!(app.autotone.done, 0);

        app.loader = Some(crate::jobs::loader::Loader::new(
            16384,
            crate::jobs::cache_limits::CacheLimits::PLATFORM,
        ));
        for (index, path) in [&a, &b].into_iter().enumerate() {
            app.loader.as_mut().unwrap().insert_thumb_external(
                path.clone(),
                THUMB_PX,
                std::sync::Arc::new(DecodedImage::new_tracked(DecodedImageFields {
                    width: 8,
                    height: 8,
                    rgba: [32, 32, 32, 255].repeat(64),
                    pixel_format: crate::decode::image_decode::PixelFormat::Srgb8,
                })),
            );
            app.poll_auto_tone();
            assert!(app.edits.contains_key(path), "each request must be toned");
            if index == 0 {
                assert!(app.autotone.pending.contains(&b));
                assert_eq!(app.autotone.done, 1);
                assert_eq!(app.autotone_total(), 2);
            }
        }
        assert!(app.autotone.pending.is_empty());
        assert_eq!(app.autotone.done, 0);
        assert_eq!(app.autotone_total(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A batch used to request every photo's thumbnail at once, which made the
    /// cache hold the whole selection. The window is what keeps a 20k-photo
    /// selection from asking for 20k decoded thumbnails.
    #[test]
    fn a_large_batch_only_requests_a_window_of_thumbnails_at_a_time() {
        let names: Vec<String> = (0..AUTOTONE_WINDOW * 3)
            .map(|i| format!("p{i:03}.jpg"))
            .collect();
        let (dir, photos) = folder_of("autotone-window", &names);

        let mut app = App::new(None);
        load_folder(&mut app, &dir);
        app.mode = ViewMode::Grid;
        app.loader = Some(crate::jobs::loader::Loader::new(
            16384,
            crate::jobs::cache_limits::CacheLimits::PLATFORM,
        ));

        app.auto_tone_batch(photos.clone(), DeferredAutoToneMode::Replace);

        let in_flight = app.loader.as_ref().unwrap().thumbs_in_flight();
        assert!(
            in_flight <= AUTOTONE_WINDOW && in_flight < photos.len(),
            "the batch must ask for a window, not all {} of its photos, got {in_flight}",
            photos.len()
        );
        assert_eq!(
            app.autotone.window.len(),
            AUTOTONE_WINDOW,
            "the batch must ask for exactly one window up front"
        );
        assert_eq!(
            app.autotone.queue.len(),
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
        for path in app.autotone.window.clone() {
            app.loader.as_mut().unwrap().insert_thumb_external(
                path,
                THUMB_PX,
                std::sync::Arc::new(DecodedImage::new_tracked(DecodedImageFields {
                    width: 8,
                    height: 8,
                    rgba: [32, 32, 32, 255].repeat(64),
                    pixel_format: crate::decode::image_decode::PixelFormat::Srgb8,
                })),
            );
        }
        app.poll_auto_tone();

        assert!(app.autotone.done > 0, "the poll must make progress");
        assert!(
            app.autotone.window.len() <= AUTOTONE_WINDOW,
            "the window must stay capped after a refill, got {}",
            app.autotone.window.len()
        );
        assert!(
            app.loader.as_ref().unwrap().thumbs_in_flight() <= AUTOTONE_WINDOW,
            "and the loader must still be inside it, got {}",
            app.loader.as_ref().unwrap().thumbs_in_flight()
        );
        assert_eq!(
            app.autotone.done + app.autotone.queue.len() + app.autotone.window.len(),
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
        let names: Vec<String> = (0..AUTOTONE_WINDOW)
            .map(|i| format!("p{i:03}.jpg"))
            .collect();
        let (dir, photos) = folder_of("autotone-budget", &names);

        let mut app = App::new(None);
        load_folder(&mut app, &dir);
        app.mode = ViewMode::Grid;
        app.loader = Some(crate::jobs::loader::Loader::new(
            16384,
            crate::jobs::cache_limits::CacheLimits::PLATFORM,
        ));
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
            pixel_format: crate::decode::image_decode::PixelFormat::Srgb8,
        }));
        for path in app.autotone.window.clone() {
            app.loader
                .as_mut()
                .unwrap()
                .insert_thumb_external(path, THUMB_PX, img.clone());
        }

        app.poll_auto_tone();

        assert!(app.autotone.done > 0, "a poll must make progress");
        assert!(
            app.autotone.done < photos.len(),
            "a poll must stop at its budget, not tone all {} ready photos",
            photos.len()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn poisoned_thumbnail_queue_terminates_auto_tone_batch() {
        let (mut app, dir, a, b) = grid_with_two_photos("autotone-poison");
        let mut loader = crate::jobs::loader::Loader::new(
            16384,
            crate::jobs::cache_limits::CacheLimits::PLATFORM,
        );
        loader.poison_thumb_queue_for_test(a.clone(), THUMB_PX);
        app.loader = Some(loader);
        app.auto_tone_batch(vec![a.clone()], DeferredAutoToneMode::Replace);
        assert!(app.autotone.pending.contains(&a));
        let loader = app.loader.as_mut().unwrap();
        loader.poll_all();
        assert!(loader.thumb_failed(&a, THUMB_PX));
        app.auto_tone_batch(vec![b.clone()], DeferredAutoToneMode::Replace);

        let loader = app.loader.as_mut().unwrap();
        loader.poll_all();
        assert!(loader.thumb_failed(&a, THUMB_PX));
        assert!(loader.thumb_failed(&b, THUMB_PX));
        app.poll_auto_tone();
        assert!(app.autotone.pending.is_empty());
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
        app.hist.set_sample(vec![[0.02f32; 3]; 64]);
        app.sel = None;
        app.want = Some(a.clone());

        app.auto_tone_selected();

        assert!(
            app.edits.contains_key(&a),
            "the open photo must be toned inline, with no decode queued"
        );
        assert!(app.autotone.pending.is_empty(), "nothing should be pending");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A thumbnail from folder A arriving after a switch to folder B must not
    /// be toned against B's catalog. See `cancel_auto_tone`.
    #[test]
    fn switching_folders_cancels_a_pending_auto_tone_batch() {
        let dir = temp_folder("autotone-switch");

        let mut app = App::new(None);
        let stale = PathBuf::from("/folder-a/IMG_0001.jpg");
        app.autotone.pending.insert(stale.clone());
        app.autotone.done = 1;

        app.load_playlist(Playlist::from_dir(&dir), dir.clone());

        assert!(
            app.autotone.pending.is_empty(),
            "folder B must not inherit folder A's outstanding Auto Tone work"
        );
        assert_eq!(app.autotone.done, 0);
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
        app.hist.set_sample(vec![[0.02f32; 3]; 64]);
        app.pretend_catalog_loading(&dir);

        app.auto_tone_shown();

        assert_eq!(app.autotone.deferred, Some(vec![a.clone()]));
        assert!(!app.edits.contains_key(&a));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cmd_u_in_the_grid_waits_for_catalog_load() {
        let (mut app, dir, a, b) = grid_with_two_photos("autotone-grid-catalog-race");
        app.shown = Shown::Preview(a, 1024, 1024);
        app.hist.set_sample(vec![[0.02f32; 3]; 64]);
        app.sel = app
            .visible
            .iter()
            .position(|&i| app.playlist.as_ref().and_then(|pl| pl.entry(i)) == Some(b.as_path()));
        assert!(app.sel.is_some());
        app.pretend_catalog_loading(&dir);

        app.auto_tone_selected();

        assert_eq!(app.autotone.deferred, Some(vec![b.clone()]));
        assert!(!app.edits.contains_key(&b));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn successive_cmd_u_requests_during_catalog_load_are_preserved_once() {
        let (mut app, dir, a, b) = grid_with_two_photos("autotone-successive-catalog-race");
        app.pretend_catalog_loading(&dir);

        for path in [&a, &b, &a] {
            app.sel = app.visible.iter().position(|&i| {
                app.playlist.as_ref().and_then(|pl| pl.entry(i)) == Some(path.as_path())
            });
            assert!(app.sel.is_some());
            app.auto_tone_selected();
        }

        assert_eq!(app.autotone.deferred, Some(vec![a.clone(), b.clone()]));
        assert!(app.autotone.pending.is_empty());
        assert!(!app.edits.contains_key(&a));
        assert!(!app.edits.contains_key(&b));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn selection_request_replaces_deferred_single_photo_requests() {
        let (mut app, dir, a, b) = grid_with_two_photos("autotone-selection-catalog-race");
        let c = dir.join("c.jpg");
        app.pretend_catalog_loading(&dir);

        app.auto_tone_batch(vec![a.clone()], DeferredAutoToneMode::Append);
        app.auto_tone_batch(
            vec![b.clone(), b.clone(), c.clone()],
            DeferredAutoToneMode::Replace,
        );

        assert_eq!(app.autotone.deferred, Some(vec![b, c]));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cmd_u_with_empty_histogram_waits_for_catalog_load() {
        let (mut app, dir, a, _b) = grid_with_two_photos("autotone-empty-hist-catalog-race");
        app.mode = ViewMode::Loupe;
        app.shown = Shown::Preview(a.clone(), 1024, 1024);
        app.sel = None;
        app.want = Some(a.clone());
        app.hist.set_sample(Vec::new());
        app.pretend_catalog_loading(&dir);

        app.auto_tone_selected();

        assert_eq!(app.autotone.deferred, Some(vec![a.clone()]));
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
        assert!(
            app.autotone.pending.contains(&a),
            "thumbnail not resident yet"
        );

        // What `forget_photo` does when the trash call for `a` lands. Its
        // thumbnail request is already in the window.
        app.autotone.pending.remove(&a);
        app.autotone.base.remove(&a);

        app.loader = Some(crate::jobs::loader::Loader::new(
            16384,
            crate::jobs::cache_limits::CacheLimits::PLATFORM,
        ));
        app.loader.as_mut().unwrap().insert_thumb_external(
            a.clone(),
            THUMB_PX,
            std::sync::Arc::new(DecodedImage::new_tracked(DecodedImageFields {
                width: 8,
                height: 8,
                rgba: [32, 32, 32, 255].repeat(64),
                pixel_format: crate::decode::image_decode::PixelFormat::Srgb8,
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
        assert!(
            app.autotone.pending.contains(&a),
            "thumbnail not resident yet"
        );

        let manual = crate::develop::Adjustments {
            exposure: 1.23,
            ..Default::default()
        };
        app.edits.insert(a.clone(), manual);

        app.loader = Some(crate::jobs::loader::Loader::new(
            16384,
            crate::jobs::cache_limits::CacheLimits::PLATFORM,
        ));
        app.loader.as_mut().unwrap().insert_thumb_external(
            a.clone(),
            THUMB_PX,
            std::sync::Arc::new(DecodedImage::new_tracked(DecodedImageFields {
                width: 8,
                height: 8,
                rgba: [32, 32, 32, 255].repeat(64),
                pixel_format: crate::decode::image_decode::PixelFormat::Srgb8,
            })),
        );
        app.poll_auto_tone();

        assert_eq!(
            app.edits.get(&a).copied(),
            Some(manual),
            "the manual edit must survive untouched, not be merged with a stale auto result"
        );
        assert!(
            app.autotone.pending.is_empty(),
            "the photo must still be dropped from the batch, not left waiting forever"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
