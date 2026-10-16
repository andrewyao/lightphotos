// SPDX-License-Identifier: MIT OR Apache-2.0
// Non-test code never panics, and every `unsafe` block says why it is sound.
// See CLAUDE.md → Rust rules.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable,
    clippy::undocumented_unsafe_blocks
)]

//! `sky_probe`: writes `scoring/sky.rs`'s sky coverage as images so a person
//! can check it on real photos. Each input is decoded at most 1024 px on its
//! long side, the size the app analyses, and for each it writes into `--out`:
//!
//! - `<stem>.sky.jpg`: the coverage as grayscale.
//! - `<stem>.sky-overlay.jpg`: the photo with the sky tinted red.
//!
//! On macOS the Vision subject mask is passed in, as the app does, unless
//! `--no-subject` is given. It prints
//! the coverage fraction and the best of five timings.
//!
//! ```sh
//! cargo run --release --bin sky_probe -- --out /tmp/sky ~/Pictures/beach.jpg
//! ```

use lightphotos::decode::image_decode;
use lightphotos::decode::image_encode;
use lightphotos::develop::image_ops;
use lightphotos::scoring::sky;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// Longest side the sky detector sees, in pixels.
const ANALYSIS_MAX_DIM: u32 = 1024;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let mut out_dir = None;
    let mut use_subject = true;
    let mut paths = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--no-subject" {
            use_subject = false;
        } else if arg == "--out" {
            out_dir = args.next().map(PathBuf::from);
        } else {
            paths.push(PathBuf::from(arg));
        }
    }
    let (Some(out_dir), false) = (out_dir, paths.is_empty()) else {
        eprintln!("usage: sky_probe [--no-subject] --out <dir> <image> [image ...]");
        return ExitCode::FAILURE;
    };
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("cannot create {}: {e}", out_dir.display());
        return ExitCode::FAILURE;
    }

    let mut failures = 0;
    for path in &paths {
        println!("\n=== {} ===", path.display());
        if let Err(e) = probe(path, &out_dir, use_subject) {
            failures += 1;
            println!("  ERROR: {e}");
        }
    }
    if failures > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn probe(path: &Path, out_dir: &Path, use_subject: bool) -> Result<(), String> {
    let img = image_decode::decode(path, ANALYSIS_MAX_DIM)?;
    let rgba = image_ops::to_srgb8(&img);
    let (w, h) = (img.width, img.height);
    let subject = use_subject.then(|| subject_mask(path, w, h)).flatten();

    let mut best = Duration::MAX;
    let mut coverage = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        coverage = sky::sky_coverage(&rgba, w, h, subject.as_deref());
        best = best.min(start.elapsed());
    }
    let mean = coverage.iter().map(|&c| c as f64).sum::<f64>() / coverage.len().max(1) as f64;
    println!(
        "  {w}x{h}, subject {}, sky {:.1}% of the frame, best of 5: {:.1} ms",
        if subject.is_some() { "yes" } else { "no" },
        mean / 255.0 * 100.0,
        best.as_secs_f64() * 1000.0
    );

    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "image".to_string());
    let gray: Vec<u8> = coverage.iter().flat_map(|&c| [c, c, c, 255]).collect();
    let cov_out = out_dir.join(format!("{stem}.sky.jpg"));
    image_encode::encode_jpeg(&cov_out, w, h, &gray, image_encode::JpegQuality::Export)?;
    println!("  wrote {}", cov_out.display());

    let overlay_out = out_dir.join(format!("{stem}.sky-overlay.jpg"));
    let overlay = tint(&rgba, &coverage);
    image_encode::encode_jpeg(
        &overlay_out,
        w,
        h,
        &overlay,
        image_encode::JpegQuality::Export,
    )?;
    println!("  wrote {}", overlay_out.display());
    Ok(())
}

#[cfg(target_os = "macos")]
fn subject_mask(path: &Path, w: u32, h: u32) -> Option<Vec<u8>> {
    match lightphotos::scoring::segmentation::segment(path) {
        Ok(mask) => Some(mask.resized(w, h).alpha),
        Err(e) => {
            println!("  no subject: {e}");
            None
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn subject_mask(_path: &Path, _w: u32, _h: u32) -> Option<Vec<u8>> {
    None
}

/// Tint the sky red in proportion to coverage, so soft edges show as a soft tint.
fn tint(rgba: &[u8], coverage: &[u8]) -> Vec<u8> {
    let mut out = rgba.to_vec();
    for (px, &c) in out.as_chunks_mut::<4>().0.iter_mut().zip(coverage) {
        let a = c as f32 / 255.0 * 0.6;
        px[0] = (px[0] as f32 * (1.0 - a) + 255.0 * a) as u8;
        px[1] = (px[1] as f32 * (1.0 - a)) as u8;
        px[2] = (px[2] as f32 * (1.0 - a)) as u8;
    }
    out
}
