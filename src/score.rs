// SPDX-License-Identifier: GPL-3.0-or-later

//! Scoring photos on request. The user picks the photos; nothing here sweeps
//! a folder. Each photo is decoded at a small preview size, has its edits
//! baked in so the score describes what the user sees, and is measured.
//!
//! The work runs on its own few low-priority workers, apart from the decode
//! pool, so a scoring job never queues ahead of the thumbnails on screen.
//! [`ScoreJob`] hands the pool one photo per free worker and no more, so the
//! queue never holds more than the workers can start.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::develop::{Adjustments, TouchUp};
use crate::quality::QualityScore;

/// Everything one worker needs to score one photo, owned so it never reads
/// app state.
pub struct ScoreRequest {
    pub path: PathBuf,
    pub adj: Adjustments,
    pub touchups: Vec<TouchUp>,
    pub rot: u8,
    /// The edit signature of `adj`, `touchups` and `rot`, stored with the
    /// score so a later edit marks it stale.
    pub edits: u64,
}

pub struct ScoreOutcome {
    pub path: PathBuf,
    pub edits: u64,
    pub result: Result<QualityScore, String>,
}

/// Decode a preview, bake the edits in, and score the result.
#[hotpath::measure]
pub fn score_photo(req: &ScoreRequest) -> Result<QualityScore, String> {
    let img = preview(&req.path)?;
    let (w, h, rgba) = crate::image_ops::bake_edited(&img, &req.adj, &req.touchups, req.rot);
    if w == 0 || h == 0 {
        return Err("the edited preview is empty".into());
    }
    Ok(crate::judge::judge(&rgba, w, h))
}

/// A preview at the analysis size. The embedded preview is the fast path,
/// but a JPEG's EXIF thumbnail is far too small to judge focus on, so a short
/// result is decoded again from the image itself.
fn preview(path: &Path) -> Result<crate::image_decode::DecodedImage, String> {
    use crate::thumbnail::{decode_at_size, EmbeddedPreview};
    let px = crate::quality::ANALYSIS_PX;
    let img = decode_at_size(path, px, EmbeddedPreview::UseIfPresent)?;
    if img.width.max(img.height) >= px {
        return Ok(img);
    }
    decode_at_size(path, px, EmbeddedPreview::Never)
}

/// The scoring workers. Two to four of them, a quarter of the cores, so a
/// job leaves the rest of the machine to the decode pool and the UI.
pub struct ScorePool {
    job_tx: Sender<ScoreRequest>,
    res_rx: Receiver<ScoreOutcome>,
    workers: usize,
    /// Submitted and not yet polled back.
    outstanding: usize,
}

impl ScorePool {
    /// `None` off macOS, where Vision's face and aesthetics signals are
    /// missing and a technical-only score would mislead, and when no worker
    /// could start. A pool with no workers would hold a job open forever.
    pub fn new() -> Option<Self> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let cores = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self::with_runner((cores / 4).clamp(2, 4), score_photo)
    }

    pub(crate) fn with_runner(
        workers: usize,
        run: fn(&ScoreRequest) -> Result<QualityScore, String>,
    ) -> Option<Self> {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<ScoreRequest>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<ScoreOutcome>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let mut running = 0usize;
        for i in 0..workers {
            let job_rx = Arc::clone(&job_rx);
            let res_tx = res_tx.clone();
            let spawned = thread::Builder::new()
                .name(format!("score-worker-{i}"))
                .spawn(move || {
                    lower_priority();
                    loop {
                        let req = {
                            let Ok(rx) = job_rx.lock() else { return };
                            let Ok(req) = rx.recv() else { return };
                            req
                        };
                        // The job counts the photo in flight until its outcome
                        // arrives, so a panic must still send one.
                        let result = std::panic::catch_unwind(|| run(&req))
                            .unwrap_or_else(|_| Err("scoring panicked".into()));
                        let outcome = ScoreOutcome {
                            path: req.path,
                            edits: req.edits,
                            result,
                        };
                        if res_tx.send(outcome).is_err() {
                            return;
                        }
                    }
                });
            match spawned {
                Ok(_) => running += 1,
                Err(e) => eprintln!("[score] could not spawn worker {i}: {e}"),
            }
        }
        (running > 0).then_some(Self {
            job_tx,
            res_rx,
            workers: running,
            outstanding: 0,
        })
    }

    /// How many more photos the workers can start right now.
    pub fn capacity(&self) -> usize {
        self.workers.saturating_sub(self.outstanding)
    }

    #[cfg(test)]
    pub fn outstanding(&self) -> usize {
        self.outstanding
    }

    pub fn busy(&self) -> bool {
        self.outstanding > 0
    }

    pub fn submit(&mut self, req: ScoreRequest) {
        if self.job_tx.send(req).is_ok() {
            self.outstanding += 1;
        }
    }

    /// Drain finished photos without blocking.
    pub fn poll(&mut self) -> Vec<ScoreOutcome> {
        let out: Vec<ScoreOutcome> = std::iter::from_fn(|| self.res_rx.try_recv().ok()).collect();
        self.outstanding = self.outstanding.saturating_sub(out.len());
        out
    }
}

/// Run the calling worker below the UI and the decode pool.
fn lower_priority() {
    #[cfg(target_os = "macos")]
    {
        // QOS_CLASS_UTILITY from <sys/qos.h>.
        const QOS_CLASS_UTILITY: u32 = 0x11;
        extern "C" {
            fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
        }
        // SAFETY: affects only the calling thread; a failure leaves it at its
        // default priority.
        unsafe {
            pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0);
        }
    }
    #[cfg(target_os = "linux")]
    {
        // On Linux a nice value belongs to the thread, not the process.
        extern "C" {
            fn nice(inc: i32) -> i32;
        }
        // SAFETY: plain libc call on the calling thread.
        unsafe {
            nice(10);
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        // SAFETY: the pseudo-handle names the calling thread.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
    }
}

/// The photos one "Score photos" run still has to do. A second run while
/// one is going adds to it rather than starting over.
#[derive(Default, Debug)]
pub struct ScoreJob {
    pending: VecDeque<PathBuf>,
    in_flight: HashSet<PathBuf>,
    done: usize,
    failed: usize,
    total: usize,
}

impl ScoreJob {
    /// Queue `paths`, skipping any already waiting or in flight.
    pub fn add(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for p in paths {
            if !self.in_flight.contains(&p) && !self.pending.contains(&p) {
                self.pending.push_back(p);
                self.total += 1;
            }
        }
    }

    /// Move up to `n` waiting photos in flight and return them.
    pub fn take(&mut self, n: usize) -> Vec<PathBuf> {
        let take = n.min(self.pending.len());
        let out: Vec<PathBuf> = self.pending.drain(..take).collect();
        self.in_flight.extend(out.iter().cloned());
        out
    }

    /// Record a photo's outcome. `false` when it is not this job's, such as
    /// a late result from a job that was cancelled.
    pub fn finish(&mut self, path: &Path, ok: bool) -> bool {
        if !self.in_flight.remove(path) {
            return false;
        }
        self.done += 1;
        if !ok {
            self.failed += 1;
        }
        true
    }

    pub fn is_finished(&self) -> bool {
        self.pending.is_empty() && self.in_flight.is_empty()
    }

    /// `(done, total)`.
    pub fn progress(&self) -> (usize, usize) {
        (self.done, self.total)
    }

    pub fn failed(&self) -> usize {
        self.failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_job_hands_out_no_more_than_it_is_asked_for() {
        let mut job = ScoreJob::default();
        job.add(paths(&["a", "b", "c", "d", "e"]));
        assert_eq!(job.take(2), paths(&["a", "b"]));
        assert!(job.take(0).is_empty());
        assert!(job.finish(Path::new("a"), true));
        assert_eq!(job.take(1), paths(&["c"]));
        assert_eq!(job.progress(), (1, 5));
    }

    #[test]
    fn a_second_submission_skips_photos_already_queued_or_in_flight() {
        let mut job = ScoreJob::default();
        job.add(paths(&["a", "b", "c"]));
        job.take(1);
        job.add(paths(&["a", "c", "d"]));
        assert_eq!(job.progress(), (0, 4));
        assert_eq!(job.take(9), paths(&["b", "c", "d"]));
    }

    #[test]
    fn a_photo_finished_and_resubmitted_is_scored_again() {
        let mut job = ScoreJob::default();
        job.add(paths(&["a"]));
        job.take(1);
        job.finish(Path::new("a"), true);
        job.add(paths(&["a"]));
        assert_eq!(job.take(1), paths(&["a"]));
        assert_eq!(job.progress(), (1, 2));
    }

    #[test]
    fn a_result_the_job_never_handed_out_is_refused() {
        let mut job = ScoreJob::default();
        job.add(paths(&["a"]));
        assert!(
            !job.finish(Path::new("a"), true),
            "still waiting, not in flight"
        );
        assert!(!job.finish(Path::new("z"), true));
        assert_eq!(job.progress(), (0, 1));
    }

    #[test]
    fn failures_count_as_done_and_are_tallied() {
        let mut job = ScoreJob::default();
        job.add(paths(&["a", "b"]));
        job.take(2);
        job.finish(Path::new("a"), false);
        job.finish(Path::new("b"), true);
        assert!(job.is_finished());
        assert_eq!((job.progress(), job.failed()), ((2, 2), 1));
    }

    fn fixed(_: &ScoreRequest) -> Result<QualityScore, String> {
        Ok(QualityScore {
            value: 42,
            basis: crate::quality::Basis::TechnicalOnly,
            base: None,
            deductions: Vec::new(),
        })
    }

    fn request(name: &str) -> ScoreRequest {
        ScoreRequest {
            path: PathBuf::from(name),
            adj: Adjustments::default(),
            touchups: Vec::new(),
            rot: 0,
            edits: 9,
        }
    }

    #[test]
    fn the_pool_reports_capacity_until_its_outcomes_are_polled() {
        let mut pool = ScorePool::with_runner(2, fixed).expect("threads spawn");
        assert_eq!(pool.capacity(), 2);
        pool.submit(request("a"));
        pool.submit(request("b"));
        assert_eq!(pool.capacity(), 0);
        let mut got = Vec::new();
        let start = std::time::Instant::now();
        while got.len() < 2 && start.elapsed().as_secs() < 10 {
            got.extend(pool.poll());
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(pool.capacity(), 2);
        assert!(!pool.busy());
        assert!(got
            .iter()
            .all(|o| o.edits == 9 && o.result == fixed(&request("x"))));
    }

    #[test]
    fn a_panicking_score_still_reports_and_the_worker_keeps_going() {
        fn panics(req: &ScoreRequest) -> Result<QualityScore, String> {
            if req.path == Path::new("boom") {
                panic!("decoder blew up");
            }
            fixed(req)
        }
        let mut pool = ScorePool::with_runner(1, panics).expect("threads spawn");
        pool.submit(request("boom"));
        pool.submit(request("fine"));
        let mut got = Vec::new();
        let start = std::time::Instant::now();
        while got.len() < 2 && start.elapsed().as_secs() < 10 {
            got.extend(pool.poll());
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(got[0].result.is_err());
        assert!(got[1].result.is_ok());
    }
}
