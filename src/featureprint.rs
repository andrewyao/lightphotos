// SPDX-License-Identifier: GPL-3.0-or-later

//! Vision feature prints: a learned image embedding that tells true duplicates
//! from similar shots better than dHash. It only runs on photos dHash already
//! flagged, which keeps the cost bounded. macOS only.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

#[cfg(target_os = "macos")]
use objc2::rc::Retained;
#[cfg(target_os = "macos")]
use objc2::ClassType;
#[cfg(target_os = "macos")]
use objc2_vision::VNGenerateImageFeaturePrintRequest;

#[cfg(target_os = "macos")]
use crate::vision;

/// A computed feature print for one photo. Opaque; compare two with
/// [`feature_distance`].
#[cfg(target_os = "macos")]
pub struct FeaturePrint(Retained<objc2_vision::VNFeaturePrintObservation>);

/// A computed feature print for one photo. Opaque; compare two with
/// [`feature_distance`].
#[cfg(not(target_os = "macos"))]
pub struct FeaturePrint;

#[cfg(target_os = "macos")]
#[hotpath::measure]
pub fn compute(path: &Path) -> Result<FeaturePrint, String> {
    unsafe {
        let request = VNGenerateImageFeaturePrintRequest::new();
        vision::perform_request(path, request.as_super().as_super())?;
        let results = request
            .results()
            .ok_or("Vision returned no feature-print results")?;
        let first = results
            .firstObject()
            .ok_or("Vision returned an empty feature-print result list")?;
        Ok(FeaturePrint(first))
    }
}

#[cfg(not(target_os = "macos"))]
pub fn compute(_path: &Path) -> Result<FeaturePrint, String> {
    Err("feature-print computation is unsupported on this platform".into())
}

/// Lower is more similar. Vision documents no fixed scale, so only compare
/// distances with each other or a threshold.
#[cfg(target_os = "macos")]
#[hotpath::measure]
pub fn feature_distance(a: &FeaturePrint, b: &FeaturePrint) -> Result<f32, String> {
    let mut distance: f32 = 0.0;
    unsafe {
        a.0.computeDistance_toFeaturePrintObservation_error((&mut distance).into(), &b.0)
            .map_err(|e| e.localizedDescription().to_string())?;
    }
    Ok(distance)
}

#[cfg(not(target_os = "macos"))]
pub fn feature_distance(_a: &FeaturePrint, _b: &FeaturePrint) -> Result<f32, String> {
    Err("feature-print distance is unsupported on this platform".into())
}

/// One worker's memory of the last anchor print it computed. Jobs arrive
/// grouped by anchor, so a single slot captures nearly all the reuse, and a
/// group of `n` costs about `n` computations instead of `2(n - 1)`. It is per
/// worker because `FeaturePrint` is not `Send` and cannot be shared.
struct AnchorCache {
    entry: Option<(PathBuf, FeaturePrint)>,
}

impl AnchorCache {
    fn new() -> Self {
        Self { entry: None }
    }

    /// The print for `path`, computing it only when the slot holds another
    /// photo. A failed computation is not cached, so the next job retries.
    fn get_or_compute<F>(&mut self, path: &Path, compute: F) -> Result<&FeaturePrint, String>
    where
        F: FnOnce(&Path) -> Result<FeaturePrint, String>,
    {
        if self.entry.as_ref().is_none_or(|(p, _)| p != path) {
            self.entry = Some((path.to_path_buf(), compute(path)?));
        }
        Ok(&self.entry.as_ref().expect("just populated").1)
    }
}

/// Compare `member` against its dHash group's `anchor`.
pub struct DistanceJob {
    pub member: PathBuf,
    pub anchor: PathBuf,
}

/// Carries both paths so the caller can drop the result if the member's
/// anchor changed while the job ran.
pub struct DistanceOutcome {
    pub anchor: PathBuf,
    pub member: PathBuf,
    pub result: Result<f32, String>,
}

/// Background workers for feature-print comparisons. `VNFeaturePrintObservation`
/// isn't `Send`, so each job computes both prints and their distance on one
/// thread and sends back only the `f32`.
pub struct DistancePool {
    job_tx: Sender<DistanceJob>,
    res_rx: Receiver<DistanceOutcome>,
}

impl DistancePool {
    /// `None` when no worker could start, which is the whole story on targets
    /// that can't spawn threads. The caller must then hold no pool at all. A
    /// pool with no workers accepts jobs that never run, and the callers that
    /// mark those jobs pending would spin the frame loop waiting for them.
    pub fn new() -> Option<Self> {
        // Only dHash candidates reach this pool, so two workers are enough and
        // keep contention for Vision and the Neural Engine low.
        let cores = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self::with_workers(cores.saturating_sub(2).clamp(1, 2))
    }

    fn with_workers(workers: usize) -> Option<Self> {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<DistanceJob>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<DistanceOutcome>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let mut running = 0usize;

        for i in 0..workers {
            let job_rx = Arc::clone(&job_rx);
            let res_tx = res_tx.clone();
            let spawned = thread::Builder::new()
                .name(format!("featureprint-worker-{i}"))
                .spawn(move || {
                    let mut anchors = AnchorCache::new();
                    loop {
                        let job = {
                            let rx = match job_rx.lock() {
                                Ok(rx) => rx,
                                Err(_) => return,
                            };
                            match rx.recv() {
                                Ok(job) => job,
                                Err(_) => return,
                            }
                        };
                        let result = (|| {
                            let member_fp = compute(&job.member)?;
                            let anchor_fp = anchors.get_or_compute(&job.anchor, compute)?;
                            feature_distance(anchor_fp, &member_fp)
                        })();
                        let outcome = DistanceOutcome {
                            anchor: job.anchor,
                            member: job.member,
                            result,
                        };
                        if res_tx.send(outcome).is_err() {
                            break;
                        }
                    }
                });
            match spawned {
                Ok(_) => running += 1,
                // Some targets (wasm32) can't spawn threads. Log instead of
                // crashing at startup.
                Err(e) => eprintln!("[featureprint] could not spawn worker {i}: {e}"),
            }
        }

        (running > 0).then_some(Self { job_tx, res_rx })
    }

    /// Queue a comparison. Ignored if the workers are gone (shutdown).
    pub fn submit(&self, job: DistanceJob) {
        let _ = self.job_tx.send(job);
    }

    /// Drain finished comparisons without blocking.
    pub fn poll(&self) -> Vec<DistanceOutcome> {
        let mut out = Vec::new();
        while let Ok(o) = self.res_rx.try_recv() {
            out.push(o);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_encode::encode_jpeg;

    // Per process, because two `cargo test` runs on one machine otherwise
    // share these fixture names and one truncates a file while the other's
    // Vision request is still reading it.
    fn write_jpeg(name: &str, w: u32, h: u32, rgba: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lp-vision-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let path = dir.join(name);
        encode_jpeg(&path, w, h, rgba).expect("encode fixture jpeg");
        path
    }

    fn checkerboard(w: u32, h: u32) -> Vec<u8> {
        let mut out = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let v = if (x / 8 + y / 8) % 2 == 0 {
                    30u8
                } else {
                    220u8
                };
                let i = ((y * w + x) * 4) as usize;
                out[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        out
    }

    fn gradient(w: u32, h: u32) -> Vec<u8> {
        let mut out = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let v = ((x * 255) / w.max(1)) as u8;
                let i = ((y * w + x) * 4) as usize;
                out[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        out
    }

    /// A group of `n` used to cost `2(n - 1)` feature prints because every
    /// pair recomputed the anchor's. At the measured ~95 ms per print that is
    /// most of a second wasted on a group of ten.
    #[test]
    fn one_anchor_is_computed_once_however_many_members_compare_against_it() {
        let (w, h) = (64, 64);
        let anchor = write_jpeg("featureprint_cache_anchor.jpg", w, h, &checkerboard(w, h));
        let other = write_jpeg("featureprint_cache_other.jpg", w, h, &gradient(w, h));

        let mut cache = AnchorCache::new();
        let computed = std::cell::Cell::new(0usize);
        let counted = |path: &Path| {
            computed.set(computed.get() + 1);
            compute(path)
        };

        for _ in 0..5 {
            cache
                .get_or_compute(&anchor, counted)
                .expect("the fixture computes");
        }
        assert_eq!(
            computed.get(),
            1,
            "five members must share one anchor print"
        );

        cache
            .get_or_compute(&other, counted)
            .expect("the fixture computes");
        assert_eq!(computed.get(), 2, "a different anchor must be computed");

        let _ = std::fs::remove_file(&anchor);
        let _ = std::fs::remove_file(&other);
    }

    #[test]
    fn a_failed_anchor_is_retried_rather_than_remembered() {
        let missing = std::env::temp_dir().join("featureprint_cache_missing.jpg");
        let _ = std::fs::remove_file(&missing);

        let mut cache = AnchorCache::new();
        let attempts = std::cell::Cell::new(0usize);
        for _ in 0..2 {
            let failed = cache.get_or_compute(&missing, |path| {
                attempts.set(attempts.get() + 1);
                compute(path)
            });
            assert!(failed.is_err(), "a missing file has no feature print");
        }
        assert_eq!(attempts.get(), 2, "a failure must not poison the slot");
    }

    #[test]
    fn a_pool_that_started_no_workers_is_never_handed_to_a_caller() {
        assert!(
            DistancePool::with_workers(0).is_none(),
            "a pool with no workers would accept jobs nothing runs"
        );
    }

    // The counterpart to the test above: a pool that did start a worker must
    // still carry a job all the way through Vision and back.
    #[test]
    fn a_running_pool_returns_a_distance_for_a_submitted_pair() {
        let (w, h) = (64, 64);
        let anchor = write_jpeg("featureprint_pool_anchor.jpg", w, h, &checkerboard(w, h));
        let member = write_jpeg("featureprint_pool_member.jpg", w, h, &checkerboard(w, h));

        let pool = DistancePool::with_workers(1).expect("one worker should start");
        pool.submit(DistanceJob {
            member: member.clone(),
            anchor: anchor.clone(),
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let outcome = loop {
            if let Some(o) = pool.poll().into_iter().next() {
                break o;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the worker never returned an outcome"
            );
            thread::sleep(std::time::Duration::from_millis(10));
        };

        assert_eq!(outcome.anchor, anchor);
        assert_eq!(outcome.member, member);
        outcome.result.expect("identical fixtures should compare");

        let _ = std::fs::remove_file(&anchor);
        let _ = std::fs::remove_file(&member);
    }

    // Runs real Vision end to end and checks that the distance ordering is sane.
    #[test]
    fn identical_images_are_closer_than_different_ones() {
        let (w, h) = (64, 64);
        let a_path = write_jpeg("featureprint_test_a.jpg", w, h, &checkerboard(w, h));
        let b_path = write_jpeg("featureprint_test_b.jpg", w, h, &checkerboard(w, h));
        let c_path = write_jpeg("featureprint_test_c.jpg", w, h, &gradient(w, h));

        let fp_a = compute(&a_path).expect("compute feature print a");
        let fp_b = compute(&b_path).expect("compute feature print b");
        let fp_c = compute(&c_path).expect("compute feature print c");

        let same = feature_distance(&fp_a, &fp_b).expect("distance a-b");
        let different = feature_distance(&fp_a, &fp_c).expect("distance a-c");

        assert!(
            same < different,
            "expected identical images to score closer than different ones: same={same} different={different}"
        );

        let _ = std::fs::remove_file(&a_path);
        let _ = std::fs::remove_file(&b_path);
        let _ = std::fs::remove_file(&c_path);
    }
}
