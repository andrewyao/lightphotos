// SPDX-License-Identifier: GPL-3.0-or-later

//! `face_probe`: prints Vision face landmarks and eye-openness scores so a
//! person can check `facequality.rs` against real open-eyed and blinking
//! photos. Unit tests cannot, because synthetic fixtures have no faces.
//!
//! ```sh
//! cargo run --bin face_probe -- ~/Pictures/burst/*.jpg
//! ```
//!
//! There is no lib target, so `facequality.rs` and its dependencies come in
//! through `#[path]`. Extend the list when its dependencies grow.

#![allow(dead_code)]

#[cfg(target_os = "macos")]
#[path = "../coregraphics.rs"]
mod coregraphics;
#[path = "../facequality.rs"]
mod facequality;
#[path = "../image_decode.rs"]
mod image_decode;
#[cfg(target_os = "macos")]
#[path = "../vision.rs"]
mod vision;

#[cfg(target_os = "macos")]
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(target_os = "macos")]
    {
        real_main()
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("face_probe is macOS-only (uses Apple Vision).");
        ExitCode::FAILURE
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
    println!("    {label}: {} points, openness {openness}", points.len());
    // Print every point so a person can judge whether the contour is plausible.
    for (i, (x, y)) in points.iter().enumerate() {
        println!("      [{i:2}] {x:.5}, {y:.5}");
    }
}
