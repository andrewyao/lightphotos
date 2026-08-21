// SPDX-License-Identifier: MIT OR Apache-2.0

//! `seg_probe` — the validation harness for `segmentation.rs`.
//!
//! Whether a Vision mask actually traces the subject is not something a unit
//! test can answer: synthetic fixtures have no subject, so the tests can only
//! prove the buffer plumbing. This binary closes that gap by writing out
//! artifacts a human can look at — for each input photo:
//!
//! - `<stem>.mask.jpg`  — the raw mask as grayscale, at Vision's own resolution
//! - `<stem>.overlay.jpg` — the photo with the foreground tinted, i.e. a preview
//!   of what the Loupe's "Show Selection" overlay will look like
//!
//! Both land in the current directory, never beside the originals.
//!
//! ```sh
//! cargo run --bin seg_probe -- ~/Pictures/portrait.jpg
//! ```
//!
//! Like `face_probe`, the modules are re-declared by `#[path]` because the
//! crate has no lib target; the list is `segmentation.rs` plus its transitive
//! dependencies.

// Re-including whole modules pulls in plenty this probe never calls.
#![allow(dead_code)]

#[path = "../coregraphics.rs"]
mod coregraphics;
#[path = "../develop.rs"]
mod develop;
#[path = "../hash.rs"]
mod hash;
#[path = "../image_decode.rs"]
mod image_decode;
#[path = "../image_encode.rs"]
mod image_encode;
#[path = "../image_ops.rs"]
mod image_ops;
#[path = "../segmentation.rs"]
mod segmentation;
#[path = "../vision.rs"]
mod vision;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Longest side the overlay preview is rendered at. Big enough to judge the
/// mask edge, small enough to write quickly.
const PREVIEW_MAX_DIM: u32 = 1600;

fn main() {
    #[cfg(target_os = "macos")]
    {
        real_main();
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("seg_probe is macOS-only (uses Apple Vision).");
    }
}

#[cfg(target_os = "macos")]
fn real_main() -> ExitCode {
    let paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if paths.is_empty() {
        eprintln!("usage: seg_probe <image> [image ...]");
        eprintln!("writes <stem>.mask.jpg and <stem>.overlay.jpg into the current directory");
        return ExitCode::FAILURE;
    }

    let mut failures = 0;
    for path in &paths {
        println!("\n=== {} ===", path.display());
        match probe(path) {
            Ok(()) => {}
            Err(e) => {
                failures += 1;
                println!("  ERROR: {e}");
            }
        }
    }

    if failures > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn probe(path: &Path) -> Result<(), String> {
    let mask = segmentation::segment(path)?;
    // Both numbers, because their *ratio* is the tell: a real subject's matte
    // is nearly binary so they track each other, while a model firing at
    // nothing smears low-confidence coverage everywhere and the solid fraction
    // collapses. See `Mask::solid_coverage`.
    println!(
        "  source {:?}, mask {}x{}, coverage {:.1}% mean / {:.1}% solid",
        mask.source,
        mask.width,
        mask.height,
        mask.coverage() * 100.0,
        mask.solid_coverage() * 100.0
    );

    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "image".to_string());

    let mask_out = PathBuf::from(format!("{stem}.mask.jpg"));
    image_encode::encode_jpeg(
        &mask_out,
        mask.width,
        mask.height,
        &gray_to_rgba(&mask.alpha),
    )?;
    println!("  wrote {}", mask_out.display());

    let img = image_decode::decode(path, PREVIEW_MAX_DIM)?;
    let scaled = mask.resized(img.width, img.height);
    let overlay_out = PathBuf::from(format!("{stem}.overlay.jpg"));
    image_encode::encode_jpeg(
        &overlay_out,
        img.width,
        img.height,
        &tint_foreground(&img.rgba, &scaled.alpha),
    )?;
    println!("  wrote {}", overlay_out.display());

    Ok(())
}

/// Single-channel mask → opaque RGBA, so it can go through the JPEG encoder.
fn gray_to_rgba(alpha: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(alpha.len() * 4);
    for &a in alpha {
        out.extend_from_slice(&[a, a, a, 255]);
    }
    out
}

/// Tint the masked region red, in proportion to its coverage — soft mask edges
/// come out as a soft tint, which is the whole point of looking at this.
fn tint_foreground(rgba: &[u8], alpha: &[u8]) -> Vec<u8> {
    let mut out = rgba.to_vec();
    for (i, chunk) in out.chunks_exact_mut(4).enumerate() {
        let a = alpha.get(i).copied().unwrap_or(0) as f32 / 255.0 * 0.5;
        chunk[0] = (chunk[0] as f32 * (1.0 - a) + 255.0 * a) as u8;
        chunk[1] = (chunk[1] as f32 * (1.0 - a)) as u8;
        chunk[2] = (chunk[2] as f32 * (1.0 - a)) as u8;
    }
    out
}
