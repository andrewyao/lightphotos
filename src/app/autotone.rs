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

#[derive(Clone, Copy)]
enum DeferredAutoToneMode {
    Replace,
    Append,
}

impl App {
    /// Auto Tone the photo on screen, from the histogram sample already in
    /// memory. No decode, no thread, no progress — it lands this frame once
    /// the catalog has finished loading.
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
        if self.catalog_load_pending.is_some() {
            self.auto_tone_batch(vec![path], DeferredAutoToneMode::Append);
            return;
        }
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
        self.auto_tone_batch(self.selected_paths(), DeferredAutoToneMode::Replace);
    }

    /// Run one batch over `paths`, or defer it while the catalog is still
    /// loading. Single-photo requests append; a confirmed selection-wide
    /// request replaces the deferred batch.
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
                    // Snapshot the base *now*, before the thumbnail arrives —
                    // see `autotone_base`'s doc comment.
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

    /// Advance a running batch: tone whatever thumbnails just arrived, and drop
    /// any whose decode has permanently failed. Called every frame off the
    /// loader's drain (no-op unless a batch is actually running), because a
    /// batch whose last few thumbnails all fail would otherwise never see
    /// another arrival to wake it up, and would sit at "n/total" forever.
    pub(crate) fn poll_auto_tone(&mut self, arrivals: &[(PathBuf, u32)]) {
        if self.autotone_pending.is_empty() {
            return;
        }
        // Photos the loader has given up on entirely — computed *before* the
        // arrivals below, and consulted while building `toned`, so a path
        // the loader flagged failed in an earlier poll (its negative cache
        // is permanent, see `loader.rs`'s poison-sweep) is never also toned
        // here just because a straggling worker — mid-decode when the poison
        // hit, so untouched by the sweep — reports its result late. Without
        // this ordering the same path could land in both lists in one call.
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
                // The thumbnail landed but could not be analyzed (a degenerate
                // or short buffer). Waiting on it again would never finish.
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
            // The base snapshot was taken when this photo was queued. If the
            // edits on record now don't match it, the user touched this
            // photo by hand while its thumbnail was in flight — applying
            // `auto` (computed before that edit) would silently overwrite
            // whatever they just set. Stand down instead; the photo simply
            // doesn't get auto-toned by this batch.
            let current = self.edits.get(&path).copied().unwrap_or_default();
            if base.is_some_and(|base| base != current) {
                continue;
            }
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

    /// Batch size so far: everything actually toned plus everything still
    /// waiting. A photo dropped (failed thumbnail) or stood down (edited by
    /// hand mid-batch) leaves both counts without ever being added to
    /// either, shrinking the total rather than leaving it stale. Derived
    /// rather than tracked, so it can never drift out of sync the way a
    /// hand-maintained counter did — see the poison-race double-count this
    /// replaced.
    fn autotone_total(&self) -> usize {
        self.autotone_done + self.autotone_pending.len()
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
        let done = self.autotone_done;
        let total = self.autotone_total();
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
        // Auto Tone intentionally waits for the asynchronous catalog seed;
        // make this fixture start after that state has settled.
        for _ in 0..1000 {
            if !app.poll_catalog_load() {
                break;
            }
            std::thread::yield_now();
        }
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
        assert_eq!(app.autotone_total(), 2);
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

        app.load_playlist(Playlist::from_dir(&dir), dir.clone());

        assert!(
            app.autotone_pending.is_empty(),
            "folder B must not inherit folder A's outstanding Auto Tone work"
        );
        assert_eq!(app.autotone_done, 0);
        assert_eq!(app.autotone_total(), 0);

        // A thumbnail that lands after the switch must now be inert.
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

    /// The bug this exists for: `tone_one` used to re-read `self.edits` fresh
    /// at merge time with no staleness check, so a manual edit made to a
    /// photo while its thumbnail was still in flight got silently
    /// overwritten the moment the batch got around to it. The fix stands
    /// that photo down instead of applying a now-stale auto result.
    #[test]
    fn manual_edit_made_while_pending_survives_the_batch() {
        let (mut app, dir, a, _b) = grid_with_two_photos("autotone-manual-edit-race");
        app.auto_tone_batch(vec![a.clone()], DeferredAutoToneMode::Replace);
        assert!(app.autotone_pending.contains(&a), "thumbnail not resident yet");

        // The user manually edits the photo while it's still waiting.
        let manual = crate::develop::Adjustments {
            exposure: 1.23,
            ..Default::default()
        };
        app.edits.insert(a.clone(), manual);

        // Its thumbnail now lands, and the batch gets around to it.
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
