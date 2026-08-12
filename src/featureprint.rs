// SPDX-License-Identifier: MIT OR Apache-2.0

//! Feature-print (learned image-similarity) refinement for duplicate
//! grouping, via Apple's Vision framework (`VNGenerateImageFeaturePrintRequest`).
//!
//! Unlike `phash.rs`'s dHash (cheap pixel-gradient hashing), this is a real
//! learned embedding — meaningfully better at telling a true duplicate (same
//! framing, same moment) apart from a creative variation (different
//! pose/expression, similar framing). Only run on the small subset of photos
//! dHash already flagged as candidates, so its cost stays bounded.
//!
//! Vision decodes the file itself (its own ImageIO-backed path), independent
//! of lightphotos' own decode/thumbnail pipeline — so this module needs
//! nothing from `image_decode.rs` beyond a file path. The handler setup that
//! gets it there lives in `vision.rs`, shared with the other Vision-backed
//! features.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use objc2::rc::Retained;
use objc2::ClassType;
use objc2_vision::VNGenerateImageFeaturePrintRequest;

use crate::vision;

/// A computed feature print for one photo. Opaque; compare two with
/// [`feature_distance`].
pub struct FeaturePrint(Retained<objc2_vision::VNFeaturePrintObservation>);

/// Compute the feature print of the image at `path`. Vision decodes the file
/// itself, so this doesn't touch lightphotos' own decode/thumbnail cache.
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

/// Vision-native distance between two feature prints (lower = more similar;
/// Vision doesn't document a fixed scale, so this is only meaningful as a
/// relative ordering / threshold, not an absolute similarity percentage).
pub fn feature_distance(a: &FeaturePrint, b: &FeaturePrint) -> Result<f32, String> {
    let mut distance: f32 = 0.0;
    unsafe {
        a.0.computeDistance_toFeaturePrintObservation_error((&mut distance).into(), &b.0)
            .map_err(|e| e.localizedDescription().to_string())?;
    }
    Ok(distance)
}

/// One feature-print comparison job: compute the feature prints of `member`
/// and its dHash group's `anchor`, then the distance between them.
pub struct DistanceJob {
    pub member: PathBuf,
    pub anchor: PathBuf,
}

/// A finished comparison, carrying both paths back so the caller can reject
/// the result if the member's anchor changed while it was in flight.
pub struct DistanceOutcome {
    pub anchor: PathBuf,
    pub member: PathBuf,
    pub result: Result<f32, String>,
}

/// Background worker pool for feature-print comparisons. `VNFeaturePrintObservation`
/// isn't `Send` (Vision's Rust bindings make no thread-safety claim about it),
/// so — unlike `export.rs`'s pool, which ships a `Retained` `CGImage` result
/// back to the main thread — each job computes *both* feature prints and the
/// distance between them on the same worker thread, and only the resulting
/// `f32` (plus the paths) crosses the channel back. Mirrors `export::Exporter`
/// otherwise: small pool, self-contained jobs, drained once per frame.
pub struct DistancePool {
    job_tx: Sender<DistanceJob>,
    res_rx: Receiver<DistanceOutcome>,
}

impl DistancePool {
    pub fn new() -> Self {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<DistanceJob>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<DistanceOutcome>();
        let job_rx = Arc::new(Mutex::new(job_rx));

        // Bounded candidate subset (dHash-flagged only), not a bulk pass like
        // export — a couple of workers is plenty and keeps Vision/ANE
        // contention low.
        let cores = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let workers = cores.saturating_sub(2).clamp(1, 2);

        for i in 0..workers {
            let job_rx = Arc::clone(&job_rx);
            let res_tx = res_tx.clone();
            thread::Builder::new()
                .name(format!("featureprint-worker-{i}"))
                .spawn(move || loop {
                    let job = {
                        let rx = match job_rx.lock() {
                            Ok(rx) => rx,
                            Err(_) => return,
                        };
                        match rx.recv() {
                            Ok(job) => job,
                            Err(_) => return, // all senders dropped → shut down
                        }
                    };
                    let result = (|| {
                        let anchor_fp = compute(&job.anchor)?;
                        let member_fp = compute(&job.member)?;
                        feature_distance(&anchor_fp, &member_fp)
                    })();
                    let outcome = DistanceOutcome {
                        anchor: job.anchor,
                        member: job.member,
                        result,
                    };
                    if res_tx.send(outcome).is_err() {
                        break; // UI side gone
                    }
                })
                .expect("spawn featureprint worker");
        }

        Self { job_tx, res_rx }
    }

    /// Queue a comparison. Ignored if the workers are gone (shutdown).
    pub fn submit(&self, job: DistanceJob) {
        let _ = self.job_tx.send(job);
    }

    /// Drain all finished comparisons (non-blocking).
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

    fn write_jpeg(name: &str, w: u32, h: u32, rgba: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
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

    // Real Vision-framework round trip (like image_encode's own ImageIO round
    // trip test): confirms the FFI plumbing (VNImageRequestHandler ->
    // VNGenerateImageFeaturePrintRequest -> VNFeaturePrintObservation ->
    // computeDistance) actually runs end to end, and that its distance
    // ordering is sane — identical images score near zero, clearly different
    // ones score higher.
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
