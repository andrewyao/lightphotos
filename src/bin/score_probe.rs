// SPDX-License-Identifier: GPL-3.0-or-later

//! `score_probe`: does the quality score agree with the user? Given a folder,
//! it scores every photo through `quality.rs` and `judge.rs`, the same
//! measurements and Vision pass the app runs, reads each photo's star rating
//! and edits from its sidecar, and prints each photo's components and the
//! Spearman correlation between score and rating for each basis. Tune the
//! penalty table in `quality.rs` against it.
//!
//! ```sh
//! cargo run --release --bin score_probe -- ~/Pictures/rated-folder
//! ```
//!
//! The app decodes through ImageIO's thumbnail path, which can use a RAW's
//! embedded preview. That module drags in the catalog, so the probe decodes
//! the image itself at the same size instead; RAW renders may differ a little.
//!
//! There is no lib target, so the modules come in through `#[path]`.

#![allow(dead_code)]

#[cfg(target_os = "macos")]
#[path = "../coregraphics.rs"]
mod coregraphics;
#[path = "../develop.rs"]
mod develop;
#[path = "../facequality.rs"]
mod facequality;
#[path = "../hash.rs"]
mod hash;
#[path = "../image_decode.rs"]
mod image_decode;
#[path = "../image_encode.rs"]
mod image_encode;
#[path = "../image_ops.rs"]
mod image_ops;
#[path = "../judge.rs"]
mod judge;
#[path = "../quality.rs"]
mod quality;
#[cfg(target_os = "macos")]
#[path = "../vision.rs"]
mod vision;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use quality::{Basis, EyeState, Technical};

const PHOTO_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "heic", "heif", "tif", "tiff", "arw", "cr2", "cr3", "nef", "dng", "raf",
    "orf", "rw2",
];

fn main() -> ExitCode {
    let Some(dir) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: score_probe <folder>");
        eprintln!("scores every photo and correlates the score with its star rating");
        return ExitCode::FAILURE;
    };
    let photos = match list_photos(&dir) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}: {e}", dir.display());
            return ExitCode::FAILURE;
        }
    };

    println!(
        "{:<24} {:>5} {:>5} {:>5} {:>6} {:>6} {:>6} {:>5} {:>6} {:>6} {:>7} {:>6}  penalties",
        "photo",
        "stars",
        "tech",
        "aes",
        "focus",
        "hi",
        "lo",
        "mean",
        "noise",
        "vision",
        "utility",
        "eyes"
    );
    let mut rows = Vec::new();
    for path in &photos {
        let sidecar = Sidecar::read(path);
        match measure(path, &sidecar) {
            Ok(row) => {
                print_row(path, sidecar.rating, &row);
                rows.push((sidecar.rating, row));
            }
            Err(e) => println!("{:<24} ERROR: {e}", name(path)),
        }
    }

    let rated: Vec<(f64, &Row)> = rows
        .iter()
        .filter_map(|(r, row)| r.map(|r| (r as f64, row)))
        .collect();
    println!("\n{} photos, {} rated", rows.len(), rated.len());
    if rated.len() < 3 {
        println!("too few rated photos to correlate");
        return ExitCode::SUCCESS;
    }
    let stars: Vec<f64> = rated.iter().map(|(s, _)| *s).collect();
    let report = |label: &str, values: Vec<f64>| match spearman(&stars, &values) {
        Some(rho) => println!("  {label:<28} rho = {rho:+.3}"),
        None => println!("  {label:<28} rho undefined (no variation)"),
    };
    println!("Spearman against stars:");
    report(
        "score, technical only",
        rated.iter().map(|(_, r)| r.technical_only as f64).collect(),
    );
    if rated.iter().all(|(_, r)| r.with_aesthetics.is_some()) {
        report(
            "score, with aesthetics",
            rated
                .iter()
                .map(|(_, r)| r.with_aesthetics.unwrap_or(0) as f64)
                .collect(),
        );
    } else {
        println!("  score, with aesthetics       unavailable (needs macOS 15)");
    }
    report(
        "focus",
        rated.iter().map(|(_, r)| r.t.focus as f64).collect(),
    );
    report(
        "noise (negated)",
        rated.iter().map(|(_, r)| -r.t.noise_sigma as f64).collect(),
    );
    ExitCode::SUCCESS
}

struct Row {
    t: Technical,
    aesthetics: Option<quality::Aesthetics>,
    eyes: Option<EyeState>,
    technical_only: u8,
    with_aesthetics: Option<u8>,
    penalties: Vec<quality::Deduction>,
}

fn measure(path: &Path, sidecar: &Sidecar) -> Result<Row, String> {
    let img = image_decode::decode(path, quality::ANALYSIS_PX)?;
    let (w, h, rgba) = image_ops::bake_edited(
        &img,
        &sidecar.adjustments,
        &sidecar.touchups,
        sidecar.rotation,
    );
    let t = quality::technical(&rgba, w, h);
    let vision = judge::vision_signals(&rgba, w, h).unwrap_or_default();
    let technical_only = quality::score(&t, None, vision.eyes);
    let full = quality::score(&t, vision.aesthetics.as_ref(), vision.eyes);
    Ok(Row {
        t,
        aesthetics: vision.aesthetics,
        eyes: vision.eyes,
        technical_only: technical_only.value,
        with_aesthetics: (full.basis == Basis::WithAesthetics).then_some(full.value),
        penalties: full.deductions,
    })
}

fn print_row(path: &Path, rating: Option<u8>, r: &Row) {
    let opt = |v: Option<String>| v.unwrap_or_else(|| "-".into());
    println!(
        "{:<24} {:>5} {:>5} {:>5} {:>6.3} {:>6.3} {:>6.3} {:>5.2} {:>6.2} {:>6} {:>7} {:>6}  {}",
        name(path),
        opt(rating.map(|r| r.to_string())),
        r.technical_only,
        opt(r.with_aesthetics.map(|v| v.to_string())),
        r.t.focus,
        r.t.clip_hi,
        r.t.clip_lo,
        r.t.mean_luma,
        r.t.noise_sigma,
        opt(r.aesthetics.map(|a| format!("{:+.2}", a.overall))),
        opt(r.aesthetics.map(|a| a.utility.to_string())),
        opt(r.eyes.map(|e| format!("{e:?}").to_lowercase())),
        r.penalties
            .iter()
            .map(|d| format!("{:?} -{}", d.penalty, d.points))
            .collect::<Vec<_>>()
            .join(", "),
    );
}

fn name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn list_photos(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut photos: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| PHOTO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        })
        .collect();
    photos.sort();
    Ok(photos)
}

/// The fields of the app's sidecar (`catalog::ImageRecord`) the probe needs.
/// Read loosely: the probe must not pull in the catalog module.
#[derive(serde::Deserialize, Default)]
struct Sidecar {
    #[serde(default)]
    rating: Option<u8>,
    #[serde(default)]
    adjustments: develop::Adjustments,
    #[serde(default)]
    touchups: Vec<develop::TouchUp>,
    #[serde(default)]
    rotation: u8,
}

impl Sidecar {
    fn read(photo: &Path) -> Sidecar {
        let (Some(dir), Some(file)) = (photo.parent(), photo.file_name()) else {
            return Sidecar::default();
        };
        let mut name = file.to_os_string();
        name.push(".xmp");
        std::fs::read(dir.join(".lightphotos").join(name))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
}

/// Spearman's rho: Pearson's correlation of the ranks, ties averaged. `None`
/// when either side has no variation.
fn spearman(a: &[f64], b: &[f64]) -> Option<f64> {
    let (ra, rb) = (ranks(a), ranks(b));
    let n = ra.len() as f64;
    let (ma, mb) = (ra.iter().sum::<f64>() / n, rb.iter().sum::<f64>() / n);
    let cov: f64 = ra.iter().zip(&rb).map(|(x, y)| (x - ma) * (y - mb)).sum();
    let va: f64 = ra.iter().map(|x| (x - ma).powi(2)).sum();
    let vb: f64 = rb.iter().map(|y| (y - mb).powi(2)).sum();
    (va > 0.0 && vb > 0.0).then(|| cov / (va * vb).sqrt())
}

fn ranks(v: &[f64]) -> Vec<f64> {
    let mut order: Vec<usize> = (0..v.len()).collect();
    order.sort_by(|&i, &j| v[i].total_cmp(&v[j]));
    let mut out = vec![0.0; v.len()];
    let mut i = 0;
    while i < order.len() {
        let mut j = i;
        while j + 1 < order.len() && v[order[j + 1]] == v[order[i]] {
            j += 1;
        }
        let mean_rank = (i + j) as f64 / 2.0;
        for &k in &order[i..=j] {
            out[k] = mean_rank;
        }
        i = j + 1;
    }
    out
}
