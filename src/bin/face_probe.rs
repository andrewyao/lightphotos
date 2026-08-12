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
//! `facequality.rs` is pulled in by path rather than through the crate,
//! because lightphotos has no lib target — `src/main.rs` is the crate root.
//! That's also why this module has to stay free of other crate modules.

#[path = "../facequality.rs"]
mod facequality;

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
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
                for (i, f) in faces.iter().enumerate() {
                    let (x, y, w, h) = f.bounding_box;
                    println!(
                        "  face {i}: confidence {:.3}  box [{x:.3} {y:.3} {w:.3} {h:.3}]",
                        f.confidence
                    );
                    print_region("left eye ", &f.left_eye);
                    print_region("right eye", &f.right_eye);
                }
            }
        }
    }

    if failures > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn print_region(label: &str, points: &facequality::Points) {
    if points.is_empty() {
        println!("    {label}: (not resolved)");
        return;
    }
    println!("    {label}: {} points", points.len());
    // Full point dump — the whole reason this harness exists is to eyeball
    // whether the contour is plausible, so truncating would defeat it.
    for (i, (x, y)) in points.iter().enumerate() {
        println!("      [{i:2}] {x:.5}, {y:.5}");
    }
}
