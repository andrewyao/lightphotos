// SPDX-License-Identifier: MIT OR Apache-2.0

//! "Score photos": the App side of `crate::jobs::score`. It queues the selection,
//! keeps the scoring pool fed one photo per free worker, and stores each
//! score in the photo's sidecar as it lands.

use super::*;

use crate::jobs::score::{ScoreJob, ScoreRequest};

impl App {
    /// Score the selection, every photo of a selected group included, or in
    /// the Loupe the photo on screen. A run already going takes these on too.
    pub(crate) fn score_selection(&mut self) {
        let paths = if !self.group_picks().is_empty() {
            self.action_paths()
        } else if self.mode == ViewMode::Loupe {
            self.selected_path().into_iter().collect()
        } else {
            self.selected_member_paths()
        };
        self.score_paths(paths);
    }

    /// Score every photo in the grid, group members included.
    pub(crate) fn score_all(&mut self) {
        self.score_paths(self.all_member_paths());
    }

    fn score_paths(&mut self, paths: Vec<PathBuf>) {
        if self.score_pool.is_none() {
            return;
        }
        if paths.is_empty() {
            return;
        }
        // The worker reads the edits from the mirrors, and staleness is judged
        // against the catalog, so the two must agree before it starts.
        self.save_edit();
        // Each new score would reorder the Grid under the pointer.
        if self.grid_sort == GridSort::Quality {
            self.grid_sort = GridSort::Name;
            self.recompute_visible();
        }
        self.score_job
            .get_or_insert_with(ScoreJob::default)
            .add(paths);
        self.pump_scoring();
        self.request_redraw();
    }

    /// Store finished scores and refill the pool. Returns true while scoring
    /// work is outstanding, including late results of a cancelled run.
    pub(crate) fn pump_scoring(&mut self) -> bool {
        let Some(pool) = self.score_pool.as_mut() else {
            return false;
        };
        let outcomes = pool.poll();
        let landed = !outcomes.is_empty();
        for o in outcomes {
            let ours = self
                .score_job
                .as_mut()
                .is_some_and(|job| job.finish(&o.path, o.result.is_ok()));
            // A trashed photo's sidecar must not come back for its score.
            if !ours || !o.path.exists() {
                continue;
            }
            if let Ok(score) = o.result {
                self.catalog.set_score(&o.path, score, o.edits);
            }
        }
        if let Some(job) = self.score_job.as_mut() {
            let free = self.score_pool.as_ref().map_or(0, |p| p.capacity());
            for path in job.take(free) {
                let adj = self.edits.get(&path).copied().unwrap_or_default();
                let touchups = self.touchups.get(&path).cloned().unwrap_or_default();
                let rot = self.rotations.get(&path).copied().unwrap_or(0);
                let edits = crate::develop::edit_signature_with_touchups(&adj, &touchups, rot);
                if let Some(pool) = self.score_pool.as_mut() {
                    pool.submit(ScoreRequest {
                        path,
                        adj,
                        touchups,
                        rot,
                        edits,
                    });
                }
            }
        }

        if self.score_job.as_ref().is_some_and(ScoreJob::is_finished) {
            if let Some(job) = self.score_job.take() {
                let (done, total) = job.progress();
                let t = crate::i18n::t();
                let (kind, msg) = match job.failed() {
                    0 => (StatusKind::Success, (t.scored_n)(total)),
                    failed => (StatusKind::Error, (t.scored_partial)(done - failed, total)),
                };
                self.set_status(kind, msg);
            }
        }
        if landed {
            self.request_redraw();
        }
        self.score_pool.as_ref().is_some_and(|p| p.busy()) || self.score_job.is_some()
    }

    /// Stop the run. Photos already on a worker finish there, and their
    /// results are dropped.
    pub(crate) fn cancel_scoring(&mut self) {
        let Some(job) = self.score_job.take() else {
            return;
        };
        let (done, total) = job.progress();
        self.set_status(
            StatusKind::Info,
            (crate::i18n::t().scoring_stopped)(done, total),
        );
        self.request_redraw();
    }

    /// Starts the scoring workers. The window's setup calls it, so a test's
    /// App has none.
    pub(crate) fn start_score_pool(&mut self) {
        self.score_pool = crate::jobs::score::ScorePool::new();
    }

    /// `(done, total)` while a run is going.
    pub(crate) fn score_progress(&self) -> Option<(usize, usize)> {
        self.score_job.as_ref().map(ScoreJob::progress)
    }

    /// Whether scoring, and every score shown, is on offer: only where a
    /// pool started, which is macOS alone.
    pub(crate) fn scoring_available(&self) -> bool {
        self.score_pool.is_some()
    }

    /// The visible cell's stored score and whether it went stale.
    pub(crate) fn score_at(
        &self,
        pos: usize,
    ) -> Option<(&crate::scoring::quality::QualityScore, bool)> {
        if !self.scoring_available() {
            return None;
        }
        let pl = self.playlist.as_ref()?;
        let path = pl.entry(*self.visible.get(pos)?)?;
        self.catalog.score(path).map(|(s, stale)| (&s.score, stale))
    }

    /// The shown photo's stored score and whether it went stale.
    pub(crate) fn shown_score(&self) -> Option<(crate::scoring::quality::QualityScore, bool)> {
        if !self.scoring_available() {
            return None;
        }
        let path = self.selected_path()?;
        self.catalog
            .score(&path)
            .map(|(s, stale)| (s.score.clone(), stale))
    }

    /// The lowest and highest stored score across the selection, and
    /// whether any of them went stale. `None` while none is scored.
    pub(crate) fn selection_score_span(&self) -> Option<(u8, u8, bool)> {
        if !self.scoring_available() {
            return None;
        }
        self.selected_paths()
            .iter()
            .filter_map(|p| self.catalog.score(p))
            .map(|(s, stale)| (s.score.value, stale))
            .fold(None, |span, (v, stale)| match span {
                None => Some((v, v, stale)),
                Some((lo, hi, any)) => Some((v.min(lo), v.max(hi), any || stale)),
            })
    }

    pub(crate) fn grid_sort(&self) -> GridSort {
        self.grid_sort
    }

    /// Whether the Grid may sort by Quality: only where scoring is, and not
    /// while a run is going, since each new score would reorder the Grid.
    pub(crate) fn quality_sort_available(&self) -> bool {
        self.scoring_available() && self.score_job.is_none()
    }

    pub(super) fn set_sort(&mut self, sort: GridSort) {
        if self.mode == ViewMode::Loupe || self.grid_sort == sort {
            return;
        }
        if sort == GridSort::Quality && !self.quality_sort_available() {
            return;
        }
        self.grid_sort = sort;
        self.recompute_visible();
        self.request_redraw();
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use crate::jobs::score::ScorePool;
    use crate::scoring::quality::{Basis, QualityScore};

    fn folder_app(n: usize) -> (App, PathBuf, Vec<PathBuf>) {
        let names: Vec<String> = (0..n).map(|i| format!("p{i:02}.jpg")).collect();
        let (dir, paths) = crate::app::test_support::folder_of("score-test", &names);
        let mut app = App::new(None);
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        app.catalog.open_dir(&dir);
        (app, dir, paths)
    }

    fn slow_fixed(_: &ScoreRequest) -> Result<QualityScore, String> {
        std::thread::sleep(std::time::Duration::from_millis(5));
        Ok(QualityScore {
            value: 61,
            basis: Basis::TechnicalOnly,
            base: None,
            deductions: Vec::new(),
        })
    }

    fn run_until_idle(app: &mut App) {
        let start = std::time::Instant::now();
        while app.pump_scoring() {
            assert!(start.elapsed().as_secs() < 20, "scoring never finished");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn scoring_the_selection_never_queues_past_the_workers_and_stores_every_score() {
        let (mut app, dir, paths) = folder_app(12);
        app.score_pool = ScorePool::with_runner(2, slow_fixed);
        app.select_all();
        app.score_selection();
        let start = std::time::Instant::now();
        while app.pump_scoring() {
            let pool = app.score_pool.as_ref().unwrap();
            assert!(
                pool.outstanding() <= 2,
                "the pool never holds more than its workers"
            );
            assert!(start.elapsed().as_secs() < 20, "scoring never finished");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        for p in &paths {
            let (stored, stale) = app.catalog.score(p).expect("every photo is scored");
            assert_eq!((stored.score.value, stale), (61, false));
        }
        assert_eq!(app.status_text(), Some("Scored 12 photo(s)"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn score_all_scores_every_photo_and_group_member_whatever_is_selected() {
        let (mut app, dir, paths) = folder_app(5);
        crate::app::nav::tests::group_photos(&mut app, &[0, 1, 2], 0);
        app.score_pool = ScorePool::with_runner(2, slow_fixed);
        app.select_single(1);
        app.score_all();
        assert_eq!(app.score_progress(), Some((0, 5)));
        run_until_idle(&mut app);
        assert!(paths.iter().all(|p| app.catalog.score(p).is_some()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sorting by a score still coming in would reshuffle the Grid under the
    /// pointer, so the Quality sort waits for the run to finish.
    #[test]
    fn the_quality_sort_waits_for_a_scoring_run() {
        let (mut app, dir, _) = folder_app(3);
        app.score_pool = ScorePool::with_runner(2, slow_fixed);
        app.score_all();
        assert!(!app.quality_sort_available());
        app.set_sort(GridSort::Quality);
        assert_eq!(app.grid_sort(), GridSort::Name, "no Quality sort mid-run");
        run_until_idle(&mut app);
        assert!(app.quality_sort_available());
        app.set_sort(GridSort::Quality);
        assert_eq!(app.grid_sort(), GridSort::Quality);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn starting_a_scoring_run_leaves_the_quality_sort() {
        let (mut app, dir, _) = folder_app(3);
        app.score_pool = ScorePool::with_runner(2, slow_fixed);
        app.set_sort(GridSort::Quality);
        app.score_all();
        assert_eq!(app.grid_sort(), GridSort::Name);
        run_until_idle(&mut app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_time_sort_puts_the_oldest_first_and_the_untimed_last() {
        let (mut app, dir, paths) = folder_app(4);
        let at = |s| Some(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(s));
        app.set_sort(GridSort::Time);
        app.on_capture_times(vec![
            (paths[0].clone(), None),
            (paths[1].clone(), at(300)),
            (paths[2].clone(), at(100)),
            (paths[3].clone(), at(200)),
        ]);
        assert_eq!(
            visible_names(&app),
            ["p02.jpg", "p03.jpg", "p01.jpg", "p00.jpg"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scoring_a_group_scores_every_photo_in_it() {
        let (mut app, dir, paths) = folder_app(5);
        crate::app::nav::tests::group_photos(&mut app, &[0, 1, 2], 0);
        app.score_pool = ScorePool::with_runner(2, slow_fixed);
        app.select_single(0);
        app.score_selection();
        assert_eq!(app.score_progress(), Some((0, 3)));
        run_until_idle(&mut app);
        for p in &paths[..3] {
            assert!(app.catalog.score(p).is_some(), "{p:?} is scored");
        }
        assert!(paths[3..].iter().all(|p| app.catalog.score(p).is_none()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelling_drains_the_queue_and_stores_nothing_more() {
        let (mut app, dir, paths) = folder_app(8);
        app.score_pool = ScorePool::with_runner(2, slow_fixed);
        app.select_all();
        app.score_selection();
        assert_eq!(app.score_progress(), Some((0, 8)));
        app.cancel_scoring();
        assert_eq!(app.score_progress(), None);
        run_until_idle(&mut app);
        assert!(
            paths.iter().all(|p| app.catalog.score(p).is_none()),
            "the two photos already on workers are dropped too"
        );
        assert_eq!(app.status_text(), Some("Scoring stopped at 0/8"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaving_the_folder_cancels_the_run() {
        let (mut app, dir, _) = folder_app(4);
        app.score_pool = ScorePool::with_runner(2, slow_fixed);
        app.select_all();
        app.score_selection();
        let other = crate::app::test_support::temp_folder("score-test-other");
        app.load_playlist(Playlist::from_dir(&other), other.clone());
        assert_eq!(app.score_progress(), None);
        run_until_idle(&mut app);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    fn scored(value: u8) -> QualityScore {
        QualityScore {
            value,
            basis: Basis::TechnicalOnly,
            base: None,
            deductions: Vec::new(),
        }
    }

    fn visible_names(app: &App) -> Vec<String> {
        let pl = app.playlist.as_ref().unwrap();
        app.visible
            .iter()
            .map(|&i| {
                pl.entry(i)
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn without_a_scoring_pool_no_score_or_quality_sort_shows() {
        let (mut app, _dir, paths) = folder_app(2);
        let unedited = crate::persist::catalog::ImageRecord::default().edit_signature();
        app.catalog.set_score(&paths[0], scored(80), unedited);
        app.select_single(0);
        assert!(app.score_pool.is_none());

        assert!(!app.quality_sort_available());
        app.set_sort(GridSort::Quality);
        assert_eq!(app.grid_sort(), GridSort::Name);
        assert!(app.score_at(0).is_none());
        assert!(app.shown_score().is_none());
        assert!(app.selection_score_span().is_none());
    }

    #[test]
    fn the_quality_sort_puts_the_best_first_and_the_unscored_last_and_keeps_the_selection() {
        let (mut app, dir, paths) = folder_app(4);
        app.score_pool = ScorePool::with_runner(2, slow_fixed);
        let unedited = crate::persist::catalog::ImageRecord::default().edit_signature();
        app.catalog.set_score(&paths[1], scored(30), unedited);
        app.catalog.set_score(&paths[2], scored(80), unedited);
        app.catalog.set_score(&paths[3], scored(30), unedited);
        app.select_single(2);
        assert_eq!(app.selected_path().as_ref(), Some(&paths[2]));

        app.set_sort(GridSort::Quality);
        assert_eq!(
            visible_names(&app),
            ["p02.jpg", "p01.jpg", "p03.jpg", "p00.jpg"]
        );
        assert_eq!(app.sel, Some(0), "the cursor follows its photo");
        assert_eq!(app.selected_path().as_ref(), Some(&paths[2]));
        assert_eq!(
            app.score_at(0).map(|(s, stale)| (s.value, stale)),
            Some((80, false))
        );
        assert!(app.score_at(3).is_none());

        app.set_sort(GridSort::Name);
        assert_eq!(
            visible_names(&app),
            ["p00.jpg", "p01.jpg", "p02.jpg", "p03.jpg"]
        );
        assert_eq!(app.selected_path().as_ref(), Some(&paths[2]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_score_taken_before_an_edit_reads_as_stale() {
        let (mut app, dir, paths) = folder_app(1);
        app.score_pool = ScorePool::with_runner(2, slow_fixed);
        app.select_all();
        app.score_selection();
        run_until_idle(&mut app);
        let adj = Adjustments {
            exposure: 0.5,
            ..Adjustments::default()
        };
        app.catalog.set_adjustments(&paths[0], &adj);
        assert_eq!(
            app.catalog.score(&paths[0]).map(|(_, stale)| stale),
            Some(true)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
