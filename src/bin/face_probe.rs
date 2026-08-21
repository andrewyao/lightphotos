// SPDX-License-Identifier: MIT OR Apache-2.0

//! `face_probe` — the validation harness for `facequality.rs`.
//!
//! The eye-openness heuristic can't be trusted until it's been pointed at real
//! photographs: synthetic fixtures have no faces, so the unit tests can only
//! prove the plumbing works, never that the numbers mean anything. This binary
//! is how a human closes that gap — run it over a folder holding known
//! open-eyed and known blinking frames and read the printed openness scores.
//!
//! ```sh
//! cargo run --bin face_probe -- ~/Pictures/burst/*.jpg
//! ```
//!
//! The modules are pulled in by `#[path]` rather than through the crate,
//! because lightphotos has no lib target — `src/main.rs` is the crate root, so
//! there is nothing for a second binary to `use`. Re-declaring them here makes
//! `crate::` resolve the same way it does in the main binary; the list is
//! `facequality.rs` plus its transitive dependencies, and it needs extending
//! whenever those grow.

// Those modules carry plenty the probe itself never calls (the whole decode
// path, for one) — that's expected of a re-include, not a code smell.
#![allow(dead_code)]

#[path = "../coregraphics.rs"]
mod coregraphics;
#[path = "../facequality.rs"]
mod facequality;
#[path = "../image_decode.rs"]
mod image_decode;
#[path = "../vision.rs"]
mod vision;

use std::path::PathBuf;
use std::process::ExitCode;

fn main() {
    #[cfg(target_os = "macos")]
    {
        real_main();
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("face_probe is macOS-only (uses Apple Vision).");
    }
}

#[cfg(target_os = "macos")]
fn real_main() -> ExitCode {
    let paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if paths.is_empty() {
        eprintln!("usage: face_probe <image> [image ...]");
        eprintln!("prints Vision's face landmarks and the derived eye-openness score");
        return ExitCode::FAILURE;
    }

    let mut failures = 0;
    for path in &paths {
        println!("\n=== {} ===", path.display());
        match facequality::detect_faces(path) {
            Err(e) => {
                failures += 1;
                println!("  ERROR: {e}");
            }
            Ok(faces) if faces.is_empty() => println!("  no faces detected"),
            Ok(faces) => {
                let aspect_wh = match image_decode::pixel_size(path) {
                    Some((w, h)) => {
                        println!("  {w}x{h} stored pixels");
                        w as f32 / h as f32
                    }
                    None => {
                        println!("  WARNING: could not read pixel size, assuming square");
                        1.0
                    }
                };
                for (i, f) in faces.iter().enumerate() {
                    let (x, y, w, h) = f.bounding_box;
                    println!(
                        "  face {i}: confidence {:.3}  box [{x:.3} {y:.3} {w:.3} {h:.3}]",
                        f.confidence
                    );
                    print_region("left eye ", &f.left_eye, aspect_wh);
                    print_region("right eye", &f.right_eye, aspect_wh);
                }
                let q = facequality::face_quality(&faces, aspect_wh);
                println!(
                    "  => faces {}, worst eye {}, verdict {:?} (threshold {})",
                    q.faces,
                    q.min_eye_openness
                        .map_or("n/a".to_string(), |o| format!("{o:.4}")),
                    q.eye_state(),
                    facequality::CLOSED_EYE_RATIO,
                );
            }
        }
    }

    if failures > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn print_region(label: &str, points: &facequality::Points, aspect_wh: f32) {
    if points.is_empty() {
        println!("    {label}: (not resolved)");
        return;
    }
    let openness = facequality::eye_openness(points, aspect_wh)
        .map_or("n/a".to_string(), |o| format!("{o:.4}"));
    println!(
        "    {label}: {} points, openness {openness}",
        points.len()
    );
    // Full point dump — the whole reason this harness exists is to eyeball
    // whether the contour is plausible, so truncating would defeat it.
    for (i, (x, y)) in points.iter().enumerate() {
        println!("      [{i:2}] {x:.5}, {y:.5}");
    }
}
