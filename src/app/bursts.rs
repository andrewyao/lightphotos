//! Group Bursts: stack each run of photos shot within
//! [`BURST_GAP`](crate::navigation::BURST_GAP) of each other into a group.
//! It reads capture times on the loader's workers first, so it finishes in
//! [`App::on_capture_times`] once the last one lands.

use super::*;
use std::path::PathBuf;

use crate::groups::Group;

impl App {
    /// Group the bursts among the selection, or the whole folder when one
    /// photo or none is selected. Photos already in a group are left alone.
    pub(crate) fn group_bursts(&mut self) {
        let paths = self.burst_candidates();
        self.scan_bursts(paths);
    }

    /// Group the bursts among every single photo in the grid, whatever is
    /// selected.
    pub(crate) fn group_all_bursts(&mut self) {
        let paths = self.single_paths(0..self.visible.len());
        self.scan_bursts(paths);
    }

    /// Asks the loader for every listed photo's capture time not yet known,
    /// for the Time sort. Each landing re-sorts the Grid.
    pub(super) fn request_capture_times(&mut self) {
        let (Some(pl), Some(loader)) = (self.playlist.as_ref(), self.loader.as_mut()) else {
            return;
        };
        for path in pl.entries() {
            if !self.capture_times.contains_key(path) && !loader.request_meta(path.clone()) {
                // No worker will read it, so it sorts with the untimed.
                self.capture_times.insert(path.clone(), None);
            }
        }
    }

    fn scan_bursts(&mut self, paths: Vec<PathBuf>) {
        if !self.group_bursts_available() {
            return;
        }
        let mut waiting = Vec::new();
        for path in &paths {
            if self.capture_times.contains_key(path) {
                continue;
            }
            let queued = self
                .loader
                .as_mut()
                .is_some_and(|l| l.request_meta(path.clone()));
            if queued {
                waiting.push(path.clone());
            } else {
                // No worker will read it, so it joins no burst.
                self.capture_times.insert(path.clone(), None);
            }
        }
        self.burst_scan = Some(paths);
        if waiting.is_empty() {
            self.finish_bursts();
        } else {
            let t = crate::i18n::t();
            self.set_status(StatusKind::Progress, t.bursts_reading.to_string());
            self.request_redraw();
        }
    }

    pub(crate) fn group_bursts_available(&self) -> bool {
        !cfg!(target_arch = "wasm32")
            && self.mode == ViewMode::Grid
            && self.catalog_load_pending.is_none()
            && !self.visible.is_empty()
    }

    /// The selected single photos when two or more cells are selected, else
    /// every single photo in the grid.
    fn burst_candidates(&self) -> Vec<PathBuf> {
        let selected = self.selected_cells();
        if selected.len() >= 2 {
            self.single_paths(selected)
        } else {
            self.single_paths(0..self.visible.len())
        }
    }

    /// The photos at `cells` that are not in a group.
    fn single_paths(&self, cells: impl IntoIterator<Item = usize>) -> Vec<PathBuf> {
        cells
            .into_iter()
            .filter(|&p| self.group_at(p).is_none())
            .filter_map(|p| self.cell_path(p))
            .collect()
    }

    fn cell_path(&self, pos: usize) -> Option<PathBuf> {
        let idx = *self.visible.get(pos)?;
        self.playlist.as_ref()?.entry(idx).map(Path::to_path_buf)
    }

    /// Called as capture times land: groups the bursts once the scan has
    /// every time it asked for.
    pub(super) fn poll_bursts(&mut self) {
        let ready = self
            .burst_scan
            .as_ref()
            .is_some_and(|paths| paths.iter().all(|p| self.capture_times.contains_key(p)));
        if ready {
            self.finish_bursts();
        }
    }

    fn finish_bursts(&mut self) {
        let Some(paths) = self.burst_scan.take() else {
            return;
        };
        // Photos may have left the grid or joined a group while times were
        // read. A selection change since doesn't narrow the scan.
        let still: std::collections::HashSet<PathBuf> = self
            .single_paths(0..self.visible.len())
            .into_iter()
            .collect();
        let timed = paths
            .into_iter()
            .filter(|p| still.contains(p))
            .filter_map(|p| Some((p.clone(), (*self.capture_times.get(&p)?)?)))
            .collect();
        let bursts = crate::navigation::bursts(timed, crate::navigation::BURST_GAP);
        let t = crate::i18n::t();
        let mut grouped = 0;
        let mut photos = 0;
        for burst in bursts {
            let Some(groups) = self.catalog.groups() else {
                self.set_status(StatusKind::Error, t.group_refused_loading.to_string());
                self.request_redraw();
                return;
            };
            let Some(group) = burst_group(&burst, |p| self.member_score(p).map(|(s, _)| s.value))
            else {
                continue;
            };
            let writes = groups.create(group, web_time::SystemTime::now());
            if !self.apply_group_writes(writes) {
                return;
            }
            grouped += 1;
            photos += burst.len();
        }
        let msg = if grouped == 0 {
            t.bursts_none.to_string()
        } else {
            (t.bursts_grouped)(grouped, photos)
        };
        self.set_status(StatusKind::Success, msg);
        self.request_redraw();
    }
}

/// A burst as a group. Its representative is the highest-scored frame, the
/// earliest of a tie, or the first frame when none is scored.
fn burst_group(burst: &[PathBuf], score: impl Fn(&Path) -> Option<u8>) -> Option<Group> {
    let rep = burst
        .iter()
        .filter_map(|p| Some((p, score(p)?)))
        .fold(None, |best: Option<(&PathBuf, u8)>, (p, v)| match best {
            Some((_, b)) if b >= v => best,
            _ => Some((p, v)),
        })
        .map_or(burst.first()?, |(p, _)| p);
    let members = burst
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_os_string()))
        .collect();
    Group::new(members, rep.file_name()?.to_os_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::nav::tests::group_photos;
    use crate::app::presets::tests::folder_app;
    use std::time::{Duration, SystemTime};

    fn at(ms: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_millis(ms))
    }

    /// Each group in the folder as its members' indices into `paths`.
    fn groups_of(app: &App, paths: &[PathBuf]) -> Vec<Vec<usize>> {
        let groups = app.catalog.groups().unwrap();
        let mut out: Vec<Vec<usize>> = groups
            .iter()
            .map(|(_, g)| {
                let mut m: Vec<usize> = g
                    .members()
                    .iter()
                    .filter_map(|n| paths.iter().position(|p| p.file_name() == Some(n)))
                    .collect();
                m.sort();
                m
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn group_bursts_stacks_each_burst_and_leaves_lone_and_grouped_photos_alone() {
        let (mut app, dir, paths) = folder_app("bursts-folder", 7);
        group_photos(&mut app, &[5, 6], 5);
        let times = [
            at(0),
            at(400),
            at(5_000),
            at(9_000),
            at(9_800),
            at(9_900),
            at(10_000),
        ];
        for (p, t) in paths.iter().zip(times) {
            app.capture_times.insert(p.clone(), t);
        }
        app.group_bursts();
        assert_eq!(
            groups_of(&app, &paths),
            vec![vec![0, 1], vec![3, 4], vec![5, 6]]
        );
        app.group_bursts();
        assert_eq!(
            groups_of(&app, &paths),
            vec![vec![0, 1], vec![3, 4], vec![5, 6]],
            "a second run changes nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn group_all_bursts_ignores_the_selection() {
        let (mut app, dir, paths) = folder_app("bursts-all", 4);
        let times = [at(0), at(300), at(5_000), at(9_000)];
        for (p, t) in paths.iter().zip(times) {
            app.capture_times.insert(p.clone(), t);
        }
        app.select_single(2);
        app.selected.extend([2, 3]);
        app.group_all_bursts();
        assert_eq!(groups_of(&app, &paths), vec![vec![0, 1]]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_selection_change_while_times_load_does_not_narrow_a_folder_scan() {
        let (mut app, dir, paths) = folder_app("bursts-late", 4);
        app.burst_scan = Some(paths.clone());
        app.select_single(2);
        app.selected.extend([2, 3]);
        assert_eq!(app.selected_cells(), vec![2, 3]);
        let times = [at(0), at(300), at(5_000), at(9_000)];
        app.on_capture_times(paths.iter().cloned().zip(times).collect());
        assert!(app.burst_scan.is_none(), "the scan finished");
        assert_eq!(groups_of(&app, &paths), vec![vec![0, 1]]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bursts_representative_is_its_best_scored_frame() {
        let burst: Vec<PathBuf> = ["a", "b", "c"]
            .iter()
            .map(|n| PathBuf::from(format!("/f/{n}.jpg")))
            .collect();
        let rep = |score: &dyn Fn(&str) -> Option<u8>| {
            burst_group(&burst, |p| score(p.file_stem()?.to_str()?))
                .unwrap()
                .rep()
                .clone()
        };
        assert_eq!(rep(&|_| None), "a.jpg");
        assert_eq!(rep(&|n| (n == "b").then_some(40)), "b.jpg");
        assert_eq!(
            rep(&|n| Some(if n == "a" { 50 } else { 70 })),
            "b.jpg",
            "the earliest of a tie"
        );
    }
}
