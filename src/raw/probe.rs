// SPDX-License-Identifier: GPL-3.0-or-later

//! `decode_probe` — RAW decode validation harness (see plan
//! `native-linux-windows-port-feasibility.md`, Task 6/7/8).
//!
//! No real camera RAW file exists on this machine or in this repo, so this
//! binary also carries a synthetic **Linear DNG** fixture builder
//! (`write_linear_dng`): a hand-written, valid TIFF/DNG container holding
//! already-demosaiced 16-bit RGB samples (not a Bayer mosaic). That's
//! realistic to hand-construct, unlike real sensor RAW data, and it's enough
//! to exercise DNG container/tag parsing end-to-end — it does *not* validate
//! real-camera Bayer-CFA demosaic fidelity across makes (CR2/NEF/ARW); that
//! gap stays open (see the plan's Task 14).
//!
//! The modules are pulled in by `#[path]` rather than through the crate,
//! because lightphotos has no lib target — `src/main.rs` is the crate root, so
//! there is nothing for a second binary to `use`. Re-declaring them here makes
//! `crate::` resolve the same way it does in the main binary (same pattern as
//! `face_probe.rs`/`seg_probe.rs`).
//!
//! **Task 8 note on the comparison baseline**: the original plan for this
//! binary compared `rawler`'s decode against `image_decode::decode()` (the
//! ImageIO pixel-decode path) as a baseline. Task 7 discovered that
//! `CGImageSourceCreateImageAtIndex` cannot decode ANY DNG with
//! `PhotometricInterpretation = 34892` (LinearRaw) on this machine — see
//! `linear_dng_pixel_decode_is_blocked_on_this_machine` below — so there is no
//! ImageIO baseline available for this fixture. Per controller ruling, the
//! comparison baseline here is instead the **known analytic ground truth**
//! used to construct the fixture (the horizontal gradient
//! `R=G=B=(x * 65535 / width) as u16` written by `write_linear_dng` below),
//! which `rawler`'s own decode is checked against directly. This is a
//! stronger check than two-decoders-agree (ground truth vs. one decoder) and
//! sidesteps the ImageIO gap entirely. `image_decode`/`coregraphics` are still
//! pulled in for the container-level open check
//! (`linear_dng_fixture_writes_and_reopens_as_image_source`).

#![allow(dead_code)]

#[cfg(target_os = "macos")]
#[path = "../coregraphics.rs"]
mod coregraphics;
#[path = "../image_decode.rs"]
mod image_decode;
#[path = "preview.rs"]
mod raw_preview;
#[path = "../hash.rs"]
mod hash;
// Pulled in for `denoise_linear_rgb_buffer`, which `raw_preview`'s
// dual-gated (`raw-probe`) `Quality`-tier code now calls.
#[path = "../develop.rs"]
mod develop;

use std::path::Path;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = std::env::temp_dir().join("lightphotos-decode-probe");
    std::fs::create_dir_all(&dir).expect("create probe fixture dir");
    let dng_path = dir.join("linear_test.dng");
    let (width, height) = (256u32, 192u32);
    write_linear_dng(&dng_path, width, height).expect("write fixture DNG");

    println!("--- Synthetic Linear DNG: rawler decode vs analytic gradient ground truth ---");
    match decode_via_rawler(&dng_path) {
        Ok(raw) => match compare_against_gradient_ground_truth(&raw, width, height) {
            Ok(report) => {
                println!("rawler decoded {}x{} cpp={} bps={}: {report}", raw.width, raw.height, raw.cpp, raw.bps);
                println!("PASS: rawler's Linear DNG decode matches the analytic gradient ground truth exactly.");
            }
            Err(e) => {
                eprintln!("FAIL: rawler decoded the fixture but its pixels diverge from ground truth: {e}");
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("FAIL: rawler could not decode the synthetic Linear DNG fixture: {e}");
            std::process::exit(1);
        }
    }

    println!("--- Synthetic Linear DNG: rawler's RawDevelop pipeline (Task 10's decode_raw_nonmac path) ---");
    match develop_smoke_check(&dng_path, width, height) {
        Ok(()) => println!(
            "PASS: RawDevelop::develop_intermediate -> to_dynamic_image ran end-to-end and \
             produced a {width}x{height} image, same shape decode_raw_nonmac's non-mac RAW \
             path builds on."
        ),
        Err(e) => {
            eprintln!("FAIL: RawDevelop pipeline did not complete on the synthetic fixture: {e}");
            std::process::exit(1);
        }
    }

    // Extra files passed on the CLI: decode each with rawler and report
    // shape/timing. These are the user's own real RAW files — there's no
    // analytic ground truth to compare against, so this is a decode-succeeds
    // + timing smoke check only, no assertion (mirrors loader.rs's existing
    // report_decode/timing_enabled instrumentation pattern in spirit).
    for path in args.into_iter().map(PathBuf::from) {
        let t0 = std::time::Instant::now();
        match decode_via_rawler(&path) {
            Ok(img) => println!(
                "{}: {}x{} cpp={} bps={} in {:?}",
                path.display(),
                img.width,
                img.height,
                img.cpp,
                img.bps,
                t0.elapsed()
            ),
            Err(e) => println!("{}: FAILED: {e}", path.display()),
        }

        // Brightness comparison: ImageIO's own decode (what the mac app
        // actually shows for this file) vs. `raw_preview`'s decode
        // (what the wasm/non-mac path shows for the same file) — real
        // evidence for the "still way too dark" report, not another guess
        // at a formula. `raw-probe`'s dual cfg gate on both `image_decode`'s
        // `decode_raw_via_rawler`/mac `decode` and `raw_preview`'s
        // `decode_raw_fast_from_bytes` is exactly what makes this
        // side-by-side possible from one mac dev binary.
        #[cfg(target_os = "macos")]
        match (image_decode::decode(&path, 1600), std::fs::read(&path)) {
            (Ok(imageio), Ok(bytes)) => match raw_preview::decode_raw_fast_from_bytes(&bytes, 1600) {
                Ok(fast) => {
                    let a = avg_luma(&imageio);
                    let b = avg_luma(&fast);
                    let (ar, ag, ab) = avg_rgb(&imageio);
                    let (br, bg, bb) = avg_rgb(&fast);
                    println!(
                        "  brightness: ImageIO avg={a:.1}/255  raw_preview avg={b:.1}/255  ratio={:.2}x",
                        a / b.max(0.01)
                    );
                    println!(
                        "  per-channel: ImageIO R={ar:.1} G={ag:.1} B={ab:.1}  raw_preview R={br:.1} G={bg:.1} B={bb:.1}"
                    );
                }
                Err(e) => println!("  raw_preview FAILED: {e}"),
            },
            (Err(e), _) => println!("  ImageIO baseline FAILED: {e}"),
            (_, Err(e)) => println!("  read FAILED: {e}"),
        }

        // Fourth comparison point: the tiny EXIF thumbnail (IFD1
        // JPEGInterchangeFormat) `thumbnail::embedded_preview_from_bytes`
        // extracts — a *camera-rendered* JPEG, no rawler/gain/gamma math of
        // ours involved at all. This is what the wasm Loupe placeholder and
        // Grid thumbnails actually show at default thumb sizes (~160-320px
        // is common for this tag; `raw_preview`'s own quarter-res
        // decode only kicks in above that). If *this* is dark, it's the
        // camera's own embedded thumbnail rendering, not anything in this
        // codebase's RAW pipeline.
        #[cfg(target_os = "macos")]
        match (image_decode::decode(&path, 1600), std::fs::read(&path)) {
            (Ok(imageio), Ok(bytes)) => match embedded_preview_diag(&bytes, 1600) {
                Some(embedded) => {
                    let a = avg_luma(&imageio);
                    let b = avg_luma(&embedded);
                    println!(
                        "  embedded-preview: {}x{}  ImageIO avg={a:.1}/255  embedded avg={b:.1}/255  ratio={:.2}x",
                        embedded.width,
                        embedded.height,
                        a / b.max(0.01)
                    );
                }
                None => println!("  embedded-preview: none found in this file"),
            },
            (Err(e), _) => println!("  ImageIO baseline FAILED (embedded-preview check): {e}"),
            (_, Err(e)) => println!("  read FAILED (embedded-preview check): {e}"),
        }

        // Third comparison point: does the reference implementation's actual
        // algorithm (verified against its real source, not inferred) look
        // dark on this same file? Not a guess — this replicates its exact
        // published logic (inflate whitelevel to u32::MAX before
        // develop_intermediate so nothing internally clips, rescale by the
        // real black/white levels after, real compress-toward-min-channel
        // highlight compression, same real sRGB gamma + brightness/contrast
        // boost this repo already ported) using vanilla `rawler` — none of
        // this needs their forked demosaic-mode enum.
        #[cfg(target_os = "macos")]
        match reference_mimic_avg_luma(&path) {
            Ok((luma, (r, g, b))) => println!(
                "  reference-mimic: avg={luma:.1}/255  R={r:.1} G={g:.1} B={b:.1}"
            ),
            Err(e) => println!("  reference-mimic FAILED: {e}"),
        }
    }
}

/// Standalone diagnostic copy of `thumbnail.rs`'s (non-mac-gated)
/// `embedded_preview_from_bytes` — same logic verbatim, duplicated here
/// rather than pulled in via `#[path]` so this probe doesn't drag
/// `thumbnail.rs`'s mac-vs-non-mac cfg split and its `crate::paths`
/// dependency into a mac dev binary just to answer one brightness question.
/// Extracts the file's tiny EXIF IFD1 thumbnail (`JPEGInterchangeFormat`) —
/// a camera-rendered JPEG, no rawler/gain/gamma math of ours involved.
#[cfg(target_os = "macos")]
fn embedded_preview_diag(bytes: &[u8], max_px: u32) -> Option<image_decode::DecodedImage> {
    let mut reader = std::io::Cursor::new(bytes);
    let source = exif::Reader::new().read_from_container(&mut reader).ok()?;

    let offset = source
        .get_field(exif::Tag::JPEGInterchangeFormat, exif::In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let length = source
        .get_field(exif::Tag::JPEGInterchangeFormatLength, exif::In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    if length == 0 {
        return None;
    }

    let buf = source.buf();
    let end = offset.checked_add(length)?;
    if end > buf.len() {
        return None;
    }
    let jpeg_bytes = &buf[offset..end];

    let img = image::load_from_memory_with_format(jpeg_bytes, image::ImageFormat::Jpeg)
        .ok()?
        .into_rgba8();
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }

    let orientation = source
        .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .unwrap_or(1) as u8;

    let (nw, nh) = image_decode::fit_within(w, h, max_px);
    let rgba = if (nw, nh) == (w, h) {
        img.into_raw()
    } else {
        image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3).into_raw()
    };
    Some(image_decode::apply_exif_orientation(
        image_decode::DecodedImage {
            width: nw,
            height: nh,
            rgba,
            pixel_format: image_decode::PixelFormat::Srgb8,
        },
        orientation,
    ))
}

/// Standalone diagnostic copy of `thumbnail.rs`'s (non-mac-gated)
/// `rawler_full_image_from_bytes` — same logic verbatim, duplicated here for
/// the same reason as `embedded_preview_diag` above (avoids pulling
/// `thumbnail.rs`'s mac-vs-non-mac cfg split into this binary), but not
/// mac-only: unlike `embedded_preview_diag` (a one-off diagnostic question),
/// this backs a real regression test (`rawler_full_image_returns_preview_subifd`
/// below) that needs to run on every `decode_probe` target, mac dev machines
/// included.
fn rawler_full_image_diag(bytes: &[u8], max_px: u32) -> Option<image_decode::DecodedImage> {
    let source = rawler::rawsource::RawSource::new_from_slice(bytes);
    let params = rawler::decoders::RawDecodeParams::default();
    let decoder = rawler::get_decoder(&source).ok()?;
    if !matches!(
        decoder.format_hint(),
        rawler::decoders::FormatHint::RAF | rawler::decoders::FormatHint::CR3
    ) {
        return None;
    }

    let dynamic = decoder.full_image(&source, &params).ok().flatten()?;
    let img = dynamic.into_rgba8();
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }

    let orientation = decoder
        .raw_metadata(&source, &params)
        .ok()
        .and_then(|meta| meta.exif.orientation)
        .map(|code| image_decode::exif_code_from_rawler_orientation(rawler::Orientation::from_u16(code)))
        .unwrap_or(1);

    let (nw, nh) = image_decode::fit_within(w, h, max_px);
    let rgba = if (nw, nh) == (w, h) {
        img.into_raw()
    } else {
        image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3).into_raw()
    };
    Some(image_decode::apply_exif_orientation(
        image_decode::DecodedImage {
            width: nw,
            height: nh,
            rgba,
            pixel_format: image_decode::PixelFormat::Srgb8,
        },
        orientation,
    ))
}

/// Replicates the reference implementation's actual `develop_internal`
/// (read in full from its source, not guessed) against vanilla `rawler`
/// 0.7.2. `highlight_compression = 4.0` is a reasonable default guess for
/// its own UI slider default (not confirmed from source — this only
/// affects near-clipped highlights, not the overall/midtone brightness
/// comparison this function exists to answer).
#[cfg(target_os = "macos")]
fn reference_mimic_avg_luma(path: &Path) -> Result<(f64, (f64, f64, f64)), String> {
    use rawler::imgop::develop::{Intermediate, ProcessingStep, RawDevelop};

    let mut raw = decode_via_rawler(path)?;
    let original_white_level = raw.whitelevel.0.first().copied().unwrap_or(u16::MAX as u32) as f32;
    let original_black_level = raw.blacklevel.levels.first().map(|r| r.as_f32()).unwrap_or(0.0);
    for level in raw.whitelevel.0.iter_mut() {
        *level = u32::MAX;
    }

    let mut developer = RawDevelop::default();
    developer.steps.retain(|&step| step != ProcessingStep::SRgb);
    let mut developed = developer.develop_intermediate(&raw).map_err(|e| e.to_string())?;

    let denominator = (original_white_level - original_black_level).max(1.0);
    let rescale_factor = (u32::MAX as f32 - original_black_level) / denominator;
    let highlight_compression: f32 = 4.0;
    let clamp_limit = highlight_compression.max(1.01);

    let Intermediate::ThreeColor(pixels) = &mut developed else {
        return Err("expected ThreeColor intermediate".to_string());
    };

    let (mut sum, mut sr, mut sg, mut sb, mut n) = (0f64, 0f64, 0f64, 0f64, 0u64);
    for p in pixels.data.iter_mut() {
        let (mut r, mut g, mut b) = (
            (p[0] * rescale_factor).max(0.0),
            (p[1] * rescale_factor).max(0.0),
            (p[2] * rescale_factor).max(0.0),
        );
        let max_c = r.max(g).max(b);
        if max_c > 1.0 {
            let min_c = r.min(g).min(b);
            let compression_factor = (1.0 - (max_c - 1.0) / (clamp_limit - 1.0)).clamp(0.0, 1.0);
            let (cr, cg, cb) = (
                min_c + (r - min_c) * compression_factor,
                min_c + (g - min_c) * compression_factor,
                min_c + (b - min_c) * compression_factor,
            );
            let compressed_max = cr.max(cg).max(cb);
            if compressed_max > 1e-6 {
                let rescale = max_c / compressed_max;
                r = cr * rescale;
                g = cg * rescale;
                b = cb * rescale;
            } else {
                r = max_c;
                g = max_c;
                b = max_c;
            }
        }
        let (r, g, b) = (r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0));
        let sr8 = image_decode::apply_raw_preview_boost(rawler::imgop::srgb::srgb_apply_gamma(r)) * 255.0;
        let sg8 = image_decode::apply_raw_preview_boost(rawler::imgop::srgb::srgb_apply_gamma(g)) * 255.0;
        let sb8 = image_decode::apply_raw_preview_boost(rawler::imgop::srgb::srgb_apply_gamma(b)) * 255.0;
        sum += (sr8 + sg8 + sb8) as f64;
        sr += sr8 as f64;
        sg += sg8 as f64;
        sb += sb8 as f64;
        n += 1;
    }
    let n = n.max(1);
    Ok((sum / (n as f64 * 3.0), (sr / n as f64, sg / n as f64, sb / n as f64)))
}

/// Mean of R+G+B (not alpha) across every pixel, 0..=255 — a single scalar
/// "how bright overall" number, coarse but enough to quantify "way too
/// dark" against the ImageIO baseline.
#[cfg(target_os = "macos")]
fn avg_luma(img: &image_decode::DecodedImage) -> f64 {
    let mut sum: u64 = 0;
    let mut n: u64 = 0;
    for px in img.rgba.chunks_exact(4) {
        sum += px[0] as u64 + px[1] as u64 + px[2] as u64;
        n += 3;
    }
    sum as f64 / n.max(1) as f64
}

/// Per-channel average — `avg_luma` alone is blind to a color cast (a
/// boosted red / crushed blue can average out to a similar overall number
/// as neutral).
#[cfg(target_os = "macos")]
fn avg_rgb(img: &image_decode::DecodedImage) -> (f64, f64, f64) {
    let (mut r, mut g, mut b) = (0u64, 0u64, 0u64);
    let mut n: u64 = 0;
    for px in img.rgba.chunks_exact(4) {
        r += px[0] as u64;
        g += px[1] as u64;
        b += px[2] as u64;
        n += 1;
    }
    let n = n.max(1) as f64;
    (r as f64 / n, g as f64 / n, b as f64 / n)
}

/// Decode a RAW/DNG file via `rawler`'s top-level convenience entry point.
///
/// Confirmed against the real `rawler` 0.7.2 source (not guessed): reading
/// `~/.cargo/registry/src/index.crates.io-*/rawler-0.7.2/src/lib.rs` shows
/// `pub fn decode_file<P: AsRef<Path>>(path: P) -> Result<RawImage>`, which
/// delegates to `RawLoader::decode_file` -> `RawLoader::decode` ->
/// `RawLoader::get_decoder`. `get_decoder` (`src/decoders/mod.rs`) sniffs the
/// TIFF and routes to `DngDecoder` whenever the file carries a `DNGVersion`
/// tag (0xC612) — which `write_linear_dng` writes — regardless of Make/Model
/// being present (both default to an empty string when absent, per
/// `DngDecoder::make_camera`, not an error).
///
/// `DngDecoder::raw_image` (`src/decoders/dng.rs`) explicitly branches on
/// `PhotometricInterpretation`, with `34892 => RawPhotometricInterpretation::
/// LinearRaw` as a first-class case (not merely tolerated) — confirming
/// `rawler` *does* support already-demosaiced Linear DNG, not just
/// Bayer-mosaiced camera RAW. It then reads pixels via
/// `plain_image_from_ifd`, which for our fixture's `Compression = 1`
/// (uncompressed) + strip-based layout takes the
/// `decode_strips::<u16>(.., PackedDecompressor::new(bits, endian))` path — a
/// direct unpack of the stored 16-bit little-endian samples, with no
/// resampling or color-matrix math applied before the samples land in
/// `RawImage.data`.
///
/// Returns rawler's native `RawImage` rather than adapting it into
/// `image_decode::DecodedImage`: the ground-truth comparison below works
/// directly against rawler's raw 16-bit interleaved samples, which is a more
/// direct (and more exacting) check than routing through an 8-bit RGBA
/// intermediate would be.
///
/// Task 10: delegates to `image_decode::decode_raw_via_rawler` (the exact
/// same `rawler::decode_file` call site `decode_raw_nonmac` uses) instead of
/// calling `rawler::decode_file` a second time here — see that function's doc
/// comment for why it's gated to also compile under `feature = "raw-probe"`
/// on mac, which is what makes this delegation possible in this dev build.
fn decode_via_rawler(path: &Path) -> Result<rawler::RawImage, String> {
    image_decode::decode_raw_via_rawler(path)
}

/// Runs the exact rawler API calls `image_decode::decode_raw_nonmac` (Task
/// 10) uses after `decode_raw_via_rawler` — `RawDevelop::default()
/// .develop_intermediate(&raw)` then `.to_dynamic_image()` — against the
/// synthetic Linear DNG fixture and checks the result isn't degenerate.
///
/// This is deliberately a *different*, weaker check than
/// `compare_against_gradient_ground_truth` above: the develop pipeline
/// rescales, calibrates against `ColorMatrix1`, and applies sRGB gamma, so the
/// output pixel values no longer equal the analytic gradient formula
/// (correctly — that transform is the point of developing a RAW file). What
/// this *does* prove, for real, on this machine: the develop call chain
/// `decode_raw_nonmac` depends on does not panic or error on real
/// rawler-decoded `RawImage` data, and produces an image of the expected
/// dimensions. `decode_raw_nonmac` itself is `#[cfg(not(target_os =
/// "macos"))]` and so cannot be called directly from this mac binary — this
/// is the closest real exercise of its logic available here (see the Task 10
/// report for why: it also applies EXIF-orientation swapping and a resize
/// that this fixture's identity orientation and small size don't exercise).
fn develop_smoke_check(path: &Path, width: u32, height: u32) -> Result<(), String> {
    let raw = decode_via_rawler(path)?;
    let developed = rawler::imgop::develop::RawDevelop::default()
        .develop_intermediate(&raw)
        .map_err(|e| e.to_string())?;
    let dynamic = developed
        .to_dynamic_image()
        .ok_or("RawDevelop produced an empty image")?;

    if dynamic.width() != width || dynamic.height() != height {
        return Err(format!(
            "dimension mismatch: fixture is {width}x{height}, developed image is {}x{}",
            dynamic.width(),
            dynamic.height()
        ));
    }

    // Sanity: a real image, not a degenerate all-zero/uniform buffer (which
    // would indicate the pipeline silently produced garbage rather than
    // actually processing the gradient).
    let rgba = dynamic.into_rgba8();
    let first = rgba.get_pixel(0, 0);
    let last = rgba.get_pixel(width - 1, 0);
    if first == last {
        return Err(format!(
            "developed image looks uniform (first pixel {:?} == last pixel {:?}) \
             for a fixture that is a left-to-right gradient",
            first, last
        ));
    }
    Ok(())
}

/// Compares a `rawler`-decoded `RawImage` against the exact analytic formula
/// `write_linear_dng` used to generate its pixel data: a horizontal gradient,
/// `R=G=B=(x * 65535 / width) as u16` per pixel, RGB16 interleaved (mirrored
/// exactly from that function's pixel-writing loop above, not a
/// restatement — see `write_linear_dng`'s "Pixel data" block).
///
/// Tolerance is **zero** — not a "close enough" threshold. This fixture's
/// pixel data is uncompressed 16-bit little-endian samples, and the decode
/// path rawler takes for that case (`plain_image_from_ifd` ->
/// `decode_strips` with a `PackedDecompressor`, verified above) is a direct
/// byte unpack with no resampling, color conversion, or rounding applied
/// before `RawImage::new_with_data` stores the samples (also verified by
/// reading that constructor: it only computes geometry/black-area metadata,
/// never touches sample values). So unlike an ImageIO RGBA8 round-trip —
/// which would need a nonzero "close enough" tolerance to absorb an 8-bit
/// quantization + alpha-premultiply step — any deviation here reflects an
/// actual decode bug (wrong stride, byte order, or sample offset), not
/// floating-point or interpolation noise. A single mismatched sample fails
/// the check.
fn compare_against_gradient_ground_truth(raw: &rawler::RawImage, width: u32, height: u32) -> Result<String, String> {
    if raw.width != width as usize || raw.height != height as usize {
        return Err(format!(
            "dimension mismatch: fixture is {width}x{height}, rawler reports {}x{}",
            raw.width, raw.height
        ));
    }
    if raw.cpp != 3 {
        return Err(format!("expected cpp=3 (RGB, already demosaiced), rawler reports cpp={}", raw.cpp));
    }
    let data = match &raw.data {
        rawler::RawImageData::Integer(v) => v,
        rawler::RawImageData::Float(_) => {
            return Err("expected 16-bit integer samples, rawler returned f32 samples".to_string());
        }
    };
    let expected_len = width as usize * height as usize * 3;
    if data.len() != expected_len {
        return Err(format!("sample count mismatch: expected {expected_len} (w*h*cpp), got {}", data.len()));
    }

    let w = width.max(1) as u64;
    let mut mismatches = 0u64;
    let mut max_abs_diff: i32 = 0;
    let mut sum_abs_diff: u64 = 0;
    let mut first_mismatch: Option<(u32, u32, u16, u16)> = None;

    for y in 0..height {
        for x in 0..width {
            let expected = ((x as u64 * 65535) / w) as u16;
            let idx = (y as usize * width as usize + x as usize) * 3;
            for &got in &data[idx..idx + 3] {
                let diff = (got as i32 - expected as i32).abs();
                sum_abs_diff += diff as u64;
                if diff > max_abs_diff {
                    max_abs_diff = diff;
                }
                if got != expected {
                    mismatches += 1;
                    if first_mismatch.is_none() {
                        first_mismatch = Some((x, y, expected, got));
                    }
                }
            }
        }
    }

    let report = format!(
        "{expected_len} samples compared, {mismatches} mismatched, max abs diff {max_abs_diff}, mean abs diff {:.6}",
        sum_abs_diff as f64 / expected_len as f64
    );

    if mismatches > 0 {
        let (x, y, expected, got) = first_mismatch.unwrap();
        return Err(format!("{report} (first mismatch at pixel ({x},{y}): expected {expected}, got {got})"));
    }

    Ok(report)
}

/// Writes a minimal, valid Linear DNG: a little-endian TIFF with one IFD
/// carrying the DNG tags a reader needs to treat this as linear (non-mosaiced)
/// raw data, plus `width * height` RGB16 samples (test pattern: a horizontal
/// gradient, so a fidelity comparison has something non-uniform to diff).
///
/// Tags written (ascending tag-ID order, as TIFF requires for the IFD):
///   0x00FE NewSubfileType = 0 (marks this IFD as the primary full-res image;
///     ImageIO's RAW/DNG reader refused to decode without it — see the
///     fixture test's TDD notes)
///   0x0100 ImageWidth, 0x0101 ImageLength
///   0x0102 BitsPerSample = [16,16,16]
///   0x0103 Compression = 1 (none)
///   0x0106 PhotometricInterpretation = 34892 (DNG LinearRaw)
///   0x0111 StripOffsets
///   0x0115 SamplesPerPixel = 3
///   0x0116 RowsPerStrip = height (single strip)
///   0x0117 StripByteCounts = width * height * 3 * 2
///   0x011C PlanarConfiguration = 1 (chunky/interleaved)
///   0xC612 DNGVersion = [1,4,0,0]
///   0xC613 DNGBackwardVersion = [1,1,0,0]
///   0xC621 ColorMatrix1 = identity 3x3 (SRATIONAL) — readers need *a* matrix
///     present even though this fixture doesn't care about color accuracy.
///   0xC628 AsShotNeutral = 3 RATIONALs — *only* via
///     `write_linear_dng_with_wb` below. Omitted by this entry point so the
///     long-standing fixture stays byte-identical for the tests built around
///     it; supplied by the `Fast`-tier linear golden-hash test, which needs
///     real (non-NaN) `wb_coeffs` to exercise any pixel math at all.
///
/// Byte layout: 8-byte header, then the IFD, then the out-of-line value
/// blocks the IFD entries can't inline (`BitsPerSample`'s 3 SHORTs,
/// `ColorMatrix1`'s 9 SRATIONALs, and `AsShotNeutral`'s 3 RATIONALs when
/// present), then the pixel data. Every out-of-line offset in this fixed tag
/// set happens to land on an even byte already, but the code still
/// checks/pads defensively rather than assuming that.
fn write_linear_dng(path: &Path, width: u32, height: u32) -> std::io::Result<()> {
    write_linear_dng_with_wb(path, width, height, None)
}

/// `write_linear_dng`, plus an optional `AsShotNeutral` tag written as 3
/// RATIONALs (`(numerator, denominator)` per channel, the same encoding
/// `write_bayer_dng` uses). rawler's `DngDecoder::get_wb` turns that into
/// `wb_coeffs = [1/n0, 1/n1, 1/n2, NaN]`; with the tag absent it returns
/// `[NaN; 4]` instead.
fn write_linear_dng_with_wb(
    path: &Path,
    width: u32,
    height: u32,
    as_shot_neutral: Option<[(u32, u32); 3]>,
) -> std::io::Result<()> {
    use std::io::Write;

    const T_BYTE: u16 = 1;
    const T_SHORT: u16 = 3;
    const T_LONG: u16 = 4;
    const T_RATIONAL: u16 = 5;
    const T_SRATIONAL: u16 = 10;

    let entry_count: u16 = if as_shot_neutral.is_some() { 15 } else { 14 };
    const IFD_OFFSET: u32 = 8;

    fn inline_u16(v: u16) -> [u8; 4] {
        let b = v.to_le_bytes();
        [b[0], b[1], 0, 0]
    }
    fn inline_u32(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }
    fn push_entry(buf: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: [u8; 4]) {
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&typ.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&value);
    }
    /// Pad `buf` to an even length (TIFF requires out-of-line values to start
    /// on a word boundary).
    fn pad_to_even(buf: &mut Vec<u8>) {
        if buf.len() % 2 != 0 {
            buf.push(0);
        }
    }

    // Layout is fixed given this exact tag set, independent of width/height
    // (every per-image value — ImageWidth, StripByteCounts, etc. — fits
    // inline in its own 12-byte IFD entry). Compute the out-of-line offsets
    // up front so the IFD entries that reference them can be written in one
    // pass, in ascending tag-ID order.
    let ifd_size = 2 + (entry_count as usize) * 12 + 4;
    let after_ifd = IFD_OFFSET as usize + ifd_size;

    let bits_per_sample_offset = after_ifd as u32; // 3 x SHORT = 6 bytes
    let mut off = after_ifd + 6;
    if off % 2 != 0 {
        off += 1;
    }
    let color_matrix_offset = off as u32; // 9 x SRATIONAL = 72 bytes
    off += 72;
    if off % 2 != 0 {
        off += 1;
    }
    let as_shot_neutral_offset = off as u32; // 3 x RATIONAL = 24 bytes
    if as_shot_neutral.is_some() {
        off += 24;
        if off % 2 != 0 {
            off += 1;
        }
    }
    let pixel_offset = off as u32;

    let strip_byte_count_u64 = (width as u64) * (height as u64) * 3 * 2;
    assert!(
        strip_byte_count_u64 <= u32::MAX as u64,
        "fixture too large for a LONG StripByteCounts"
    );
    let strip_byte_count = strip_byte_count_u64 as u32;

    let mut buf: Vec<u8> = Vec::with_capacity(pixel_offset as usize + strip_byte_count as usize);

    // --- 8-byte header ---
    buf.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00]); // "II" + magic 42 (LE)
    buf.extend_from_slice(&IFD_OFFSET.to_le_bytes());

    // --- IFD ---
    buf.extend_from_slice(&entry_count.to_le_bytes());
    push_entry(&mut buf, 254, T_LONG, 1, inline_u32(0)); // NewSubfileType = 0 (primary image)
    push_entry(&mut buf, 256, T_LONG, 1, inline_u32(width)); // ImageWidth
    push_entry(&mut buf, 257, T_LONG, 1, inline_u32(height)); // ImageLength
    push_entry(&mut buf, 258, T_SHORT, 3, inline_u32(bits_per_sample_offset)); // BitsPerSample
    push_entry(&mut buf, 259, T_SHORT, 1, inline_u16(1)); // Compression = none
    push_entry(&mut buf, 262, T_SHORT, 1, inline_u16(34892)); // PhotometricInterpretation = LinearRaw
    push_entry(&mut buf, 273, T_LONG, 1, inline_u32(pixel_offset)); // StripOffsets
    push_entry(&mut buf, 277, T_SHORT, 1, inline_u16(3)); // SamplesPerPixel
    push_entry(&mut buf, 278, T_LONG, 1, inline_u32(height)); // RowsPerStrip
    push_entry(&mut buf, 279, T_LONG, 1, inline_u32(strip_byte_count)); // StripByteCounts
    push_entry(&mut buf, 284, T_SHORT, 1, inline_u16(1)); // PlanarConfiguration = chunky
    push_entry(&mut buf, 50706, T_BYTE, 4, [1, 4, 0, 0]); // DNGVersion
    push_entry(&mut buf, 50707, T_BYTE, 4, [1, 1, 0, 0]); // DNGBackwardVersion
    push_entry(&mut buf, 50721, T_SRATIONAL, 9, inline_u32(color_matrix_offset)); // ColorMatrix1
    if as_shot_neutral.is_some() {
        push_entry(&mut buf, 50728, T_RATIONAL, 3, inline_u32(as_shot_neutral_offset)); // AsShotNeutral
    }
    buf.extend_from_slice(&0u32.to_le_bytes()); // next IFD offset = none

    debug_assert_eq!(buf.len(), after_ifd, "IFD size drifted from the computed layout");

    // --- Out-of-line: BitsPerSample = [16, 16, 16] ---
    for _ in 0..3 {
        buf.extend_from_slice(&16u16.to_le_bytes());
    }
    pad_to_even(&mut buf);
    debug_assert_eq!(buf.len() as u32, color_matrix_offset);

    // --- Out-of-line: ColorMatrix1, identity 3x3 SRATIONAL (num, denom) ---
    const IDENTITY_3X3: [(i32, i32); 9] = [
        (1, 1),
        (0, 1),
        (0, 1),
        (0, 1),
        (1, 1),
        (0, 1),
        (0, 1),
        (0, 1),
        (1, 1),
    ];
    for (num, den) in IDENTITY_3X3 {
        buf.extend_from_slice(&num.to_le_bytes());
        buf.extend_from_slice(&den.to_le_bytes());
    }
    pad_to_even(&mut buf);

    // --- Out-of-line: AsShotNeutral, 3 RATIONAL (num, denom) — optional ---
    if let Some(neutral) = as_shot_neutral {
        debug_assert_eq!(buf.len() as u32, as_shot_neutral_offset);
        for (num, den) in neutral {
            buf.extend_from_slice(&num.to_le_bytes());
            buf.extend_from_slice(&den.to_le_bytes());
        }
        pad_to_even(&mut buf);
    }
    debug_assert_eq!(buf.len() as u32, pixel_offset);

    // --- Pixel data: horizontal gradient test pattern, R=G=B per pixel ---
    let w = width.max(1);
    for _y in 0..height {
        for x in 0..width {
            let v = ((x as u64 * 65535) / w as u64) as u16;
            let sample = v.to_le_bytes();
            buf.extend_from_slice(&sample); // R
            buf.extend_from_slice(&sample); // G
            buf.extend_from_slice(&sample); // B
        }
    }
    debug_assert_eq!(buf.len() as u64, pixel_offset as u64 + strip_byte_count as u64);

    let mut f = std::fs::File::create(path)?;
    f.write_all(&buf)?;
    Ok(())
}

/// `write_linear_dng`'s root IFD (same 14-entry shape, no `AsShotNeutral`)
/// plus one additional `NewSubfileType=1` preview sub-IFD referenced via the
/// root's `SubIFDs` tag (330) — an uncompressed 8-bit RGB strip, the shape
/// rawler's `DngDecoder::full_image()`/`dynamic_image_from_ifd`
/// (`vendor/rawler-0.7.2/src/decoders/{dng,mod}.rs`) reads: it finds the
/// first `SubIFDs`-referenced sub-IFD with `NewSubfileType == 1` and decodes
/// it as a plain TIFF strip image, independent of the root/primary image.
/// Confirmed `SubIFDs` is auto-descended by rawler's TIFF reader by default
/// (`formats/tiff/{ifd,reader}.rs`'s `wellknown_sub_ifd_tags`/`new_root`), so
/// no extra `sub_tags` plumbing is needed for this fixture to work.
///
/// Solid-color preview pixel data (deterministic, trivial to assert on)
/// rather than a gradient — this fixture exists to prove the *container
/// plumbing* (`SubIFDs` -> preview IFD -> strip decode) works, not to
/// exercise pixel math the way `write_bayer_dng`'s gradient does.
fn write_dng_with_preview_subifd(
    path: &Path,
    width: u32,
    height: u32,
    preview_width: u32,
    preview_height: u32,
    preview_rgb: [u8; 3],
) -> std::io::Result<()> {
    use std::io::Write;

    const T_BYTE: u16 = 1;
    const T_SHORT: u16 = 3;
    const T_LONG: u16 = 4;
    const T_SRATIONAL: u16 = 10;

    fn inline_u16(v: u16) -> [u8; 4] {
        let b = v.to_le_bytes();
        [b[0], b[1], 0, 0]
    }
    fn inline_u32(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }
    /// Pushes a 12-byte IFD entry, returning the file offset of its 4-byte
    /// value field so out-of-line offsets (computed only after everything
    /// that follows is laid out) can be patched back in at the end.
    fn push_entry(buf: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: [u8; 4]) -> usize {
        let value_pos = buf.len() + 8;
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&typ.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&value);
        value_pos
    }
    /// Pad `buf` to an even length (TIFF requires out-of-line values to start
    /// on a word boundary).
    fn pad_to_even(buf: &mut Vec<u8>) {
        if buf.len() % 2 != 0 {
            buf.push(0);
        }
    }

    let mut buf: Vec<u8> = Vec::new();

    // --- 8-byte header ---
    buf.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00]); // "II" + magic 42 (LE)
    buf.extend_from_slice(&8u32.to_le_bytes()); // root IFD at offset 8

    // --- Root IFD: `write_linear_dng`'s 14 entries, plus SubIFDs ---
    const ROOT_ENTRY_COUNT: u16 = 15;
    buf.extend_from_slice(&ROOT_ENTRY_COUNT.to_le_bytes());
    push_entry(&mut buf, 254, T_LONG, 1, inline_u32(0)); // NewSubfileType = 0 (primary)
    push_entry(&mut buf, 256, T_LONG, 1, inline_u32(width)); // ImageWidth
    push_entry(&mut buf, 257, T_LONG, 1, inline_u32(height)); // ImageLength
    let bits_per_sample_pos = push_entry(&mut buf, 258, T_SHORT, 3, inline_u32(0)); // BitsPerSample (patched)
    push_entry(&mut buf, 259, T_SHORT, 1, inline_u16(1)); // Compression = none
    push_entry(&mut buf, 262, T_SHORT, 1, inline_u16(34892)); // PhotometricInterpretation = LinearRaw
    let strip_offsets_pos = push_entry(&mut buf, 273, T_LONG, 1, inline_u32(0)); // StripOffsets (patched)
    push_entry(&mut buf, 277, T_SHORT, 1, inline_u16(3)); // SamplesPerPixel
    push_entry(&mut buf, 278, T_LONG, 1, inline_u32(height)); // RowsPerStrip
    let strip_byte_count_u64 = (width as u64) * (height as u64) * 3 * 2;
    assert!(strip_byte_count_u64 <= u32::MAX as u64, "fixture too large for a LONG StripByteCounts");
    push_entry(&mut buf, 279, T_LONG, 1, inline_u32(strip_byte_count_u64 as u32)); // StripByteCounts
    push_entry(&mut buf, 284, T_SHORT, 1, inline_u16(1)); // PlanarConfiguration = chunky
    push_entry(&mut buf, 50706, T_BYTE, 4, [1, 4, 0, 0]); // DNGVersion
    push_entry(&mut buf, 50707, T_BYTE, 4, [1, 1, 0, 0]); // DNGBackwardVersion
    let sub_ifds_pos = push_entry(&mut buf, 330, T_LONG, 1, inline_u32(0)); // SubIFDs (patched)
    let color_matrix_pos = push_entry(&mut buf, 50721, T_SRATIONAL, 9, inline_u32(0)); // ColorMatrix1 (patched)
    buf.extend_from_slice(&0u32.to_le_bytes()); // next IFD offset = none

    // --- Root out-of-line: BitsPerSample = [16, 16, 16] ---
    pad_to_even(&mut buf);
    let bits_per_sample_offset = buf.len() as u32;
    for _ in 0..3 {
        buf.extend_from_slice(&16u16.to_le_bytes());
    }

    // --- Root out-of-line: ColorMatrix1, identity 3x3 SRATIONAL (num, denom) ---
    pad_to_even(&mut buf);
    let color_matrix_offset = buf.len() as u32;
    const IDENTITY_3X3: [(i32, i32); 9] =
        [(1, 1), (0, 1), (0, 1), (0, 1), (1, 1), (0, 1), (0, 1), (0, 1), (1, 1)];
    for (num, den) in IDENTITY_3X3 {
        buf.extend_from_slice(&num.to_le_bytes());
        buf.extend_from_slice(&den.to_le_bytes());
    }

    // --- Preview sub-IFD (NewSubfileType=1), referenced via root's SubIFDs ---
    pad_to_even(&mut buf);
    let preview_ifd_offset = buf.len() as u32;
    const PREVIEW_ENTRY_COUNT: u16 = 9;
    buf.extend_from_slice(&PREVIEW_ENTRY_COUNT.to_le_bytes());
    push_entry(&mut buf, 254, T_LONG, 1, inline_u32(1)); // NewSubfileType = 1 (preview)
    push_entry(&mut buf, 256, T_LONG, 1, inline_u32(preview_width)); // ImageWidth
    push_entry(&mut buf, 257, T_LONG, 1, inline_u32(preview_height)); // ImageLength
    let preview_bits_pos = push_entry(&mut buf, 258, T_SHORT, 3, inline_u32(0)); // BitsPerSample (patched)
    push_entry(&mut buf, 259, T_SHORT, 1, inline_u16(1)); // Compression = none
    let preview_strip_offsets_pos = push_entry(&mut buf, 273, T_LONG, 1, inline_u32(0)); // StripOffsets (patched)
    push_entry(&mut buf, 277, T_SHORT, 1, inline_u16(3)); // SamplesPerPixel
    push_entry(&mut buf, 278, T_LONG, 1, inline_u32(preview_height)); // RowsPerStrip
    let preview_strip_byte_count = preview_width * preview_height * 3;
    push_entry(&mut buf, 279, T_LONG, 1, inline_u32(preview_strip_byte_count)); // StripByteCounts
    buf.extend_from_slice(&0u32.to_le_bytes()); // next IFD offset = none

    // --- Preview out-of-line: BitsPerSample = [8, 8, 8] ---
    pad_to_even(&mut buf);
    let preview_bits_offset = buf.len() as u32;
    for _ in 0..3 {
        buf.extend_from_slice(&8u16.to_le_bytes());
    }

    // --- Preview pixel data: solid color, 1 byte/sample, chunky RGB ---
    pad_to_even(&mut buf);
    let preview_pixel_offset = buf.len() as u32;
    for _ in 0..(preview_width * preview_height) {
        buf.extend_from_slice(&preview_rgb);
    }

    // --- Root pixel data: horizontal gradient test pattern, R=G=B, 16-bit ---
    pad_to_even(&mut buf);
    let pixel_offset = buf.len() as u32;
    let w = width.max(1);
    for _y in 0..height {
        for x in 0..width {
            let v = ((x as u64 * 65535) / w as u64) as u16;
            let sample = v.to_le_bytes();
            buf.extend_from_slice(&sample); // R
            buf.extend_from_slice(&sample); // G
            buf.extend_from_slice(&sample); // B
        }
    }

    // --- Patch back every out-of-line/sub-IFD offset now that all of them are known ---
    buf[bits_per_sample_pos..bits_per_sample_pos + 4].copy_from_slice(&bits_per_sample_offset.to_le_bytes());
    buf[strip_offsets_pos..strip_offsets_pos + 4].copy_from_slice(&pixel_offset.to_le_bytes());
    buf[sub_ifds_pos..sub_ifds_pos + 4].copy_from_slice(&preview_ifd_offset.to_le_bytes());
    buf[color_matrix_pos..color_matrix_pos + 4].copy_from_slice(&color_matrix_offset.to_le_bytes());
    buf[preview_bits_pos..preview_bits_pos + 4].copy_from_slice(&preview_bits_offset.to_le_bytes());
    buf[preview_strip_offsets_pos..preview_strip_offsets_pos + 4]
        .copy_from_slice(&preview_pixel_offset.to_le_bytes());

    let mut f = std::fs::File::create(path)?;
    f.write_all(&buf)?;
    Ok(())
}

/// Writes a minimal, valid Bayer-CFA DNG: an RGGB mosaic, single 16-bit
/// sample per pixel, with the DNG tags a real Bayer decode path reads
/// (`CFAPattern`, black/white levels, `AsShotNeutral` for white balance).
/// Test pattern: `v = 100 + ((x * 53 + y * 197) % 800)`, deterministic and
/// non-uniform (unlike a flat value, gives 2x2-bin/PPG demosaic something
/// real to interpolate/average). `width`/`height` must be even (Bayer 2x2
/// tiling).
///
/// Confirmed against the real `rawler` 0.7.2 source: `DngDecoder::get_cfa`
/// (`src/decoders/dng.rs`) reads only `TiffCommonTag::CFAPattern`
/// (0x828E) — `CFARepeatPatternDim` isn't consulted for the `CFA` object,
/// so it's omitted here. `CFAColor` numeric codes (`src/cfa.rs`) are
/// RED=0, GREEN=1, BLUE=2 — `CFAPattern = [0,1,1,2]` is RGGB.
fn write_bayer_dng(path: &Path, width: u32, height: u32) -> std::io::Result<()> {
    write_bayer_dng_with_cfa(path, width, height, [0, 1, 1, 2])
}

/// `write_bayer_dng` with an explicit 4-byte `CFAPattern` (color codes, see
/// that function's doc comment), so a test can build a fixture whose CFA is
/// *not* one of the four RGGB-family patterns rawler's `Superpixel3Channel`
/// can demosaic.
fn write_bayer_dng_with_cfa(path: &Path, width: u32, height: u32, cfa_pattern: [u8; 4]) -> std::io::Result<()> {
    use std::io::Write;

    const T_BYTE: u16 = 1;
    const T_SHORT: u16 = 3;
    const T_LONG: u16 = 4;
    const T_RATIONAL: u16 = 5;
    const T_SRATIONAL: u16 = 10;

    const ENTRY_COUNT: u16 = 19;
    const IFD_OFFSET: u32 = 8;

    fn inline_u16(v: u16) -> [u8; 4] {
        let b = v.to_le_bytes();
        [b[0], b[1], 0, 0]
    }
    fn inline_u32(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }
    fn push_entry(buf: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: [u8; 4]) {
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&typ.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&value);
    }
    fn pad_to_even(buf: &mut Vec<u8>) {
        if buf.len() % 2 != 0 {
            buf.push(0);
        }
    }

    assert!(width % 2 == 0 && height % 2 == 0, "Bayer fixture needs even dimensions");

    let ifd_size = 2 + (ENTRY_COUNT as usize) * 12 + 4;
    let after_ifd = IFD_OFFSET as usize + ifd_size;

    // Out-of-line blocks, in ascending-tag order: BlackLevels (4x SHORT),
    // ColorMatrix1 (9x SRATIONAL), AsShotNeutral (3x RATIONAL).
    let blacklevels_offset = after_ifd as u32; // 4 x SHORT = 8 bytes
    let mut off = after_ifd + 8;
    if off % 2 != 0 {
        off += 1;
    }
    let colormatrix_offset = off as u32; // 9 x SRATIONAL = 72 bytes
    off += 72;
    if off % 2 != 0 {
        off += 1;
    }
    let asshotneutral_offset = off as u32; // 3 x RATIONAL = 24 bytes
    off += 24;
    if off % 2 != 0 {
        off += 1;
    }
    let pixel_offset = off as u32;

    let strip_byte_count = width * height * 2; // 1 sample/pixel, 16-bit

    let mut buf: Vec<u8> = Vec::with_capacity(pixel_offset as usize + strip_byte_count as usize);

    buf.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00]);
    buf.extend_from_slice(&IFD_OFFSET.to_le_bytes());

    buf.extend_from_slice(&ENTRY_COUNT.to_le_bytes());
    push_entry(&mut buf, 254, T_LONG, 1, inline_u32(0)); // NewSubfileType
    push_entry(&mut buf, 256, T_LONG, 1, inline_u32(width)); // ImageWidth
    push_entry(&mut buf, 257, T_LONG, 1, inline_u32(height)); // ImageLength
    push_entry(&mut buf, 258, T_SHORT, 1, inline_u16(16)); // BitsPerSample
    push_entry(&mut buf, 259, T_SHORT, 1, inline_u16(1)); // Compression = none
    push_entry(&mut buf, 262, T_SHORT, 1, inline_u16(32803)); // PhotometricInterpretation = CFA
    push_entry(&mut buf, 273, T_LONG, 1, inline_u32(pixel_offset)); // StripOffsets
    push_entry(&mut buf, 277, T_SHORT, 1, inline_u16(1)); // SamplesPerPixel
    push_entry(&mut buf, 278, T_LONG, 1, inline_u32(height)); // RowsPerStrip
    push_entry(&mut buf, 279, T_LONG, 1, inline_u32(strip_byte_count)); // StripByteCounts
    push_entry(&mut buf, 284, T_SHORT, 1, inline_u16(1)); // PlanarConfiguration
    push_entry(&mut buf, 33422, T_BYTE, 4, cfa_pattern); // CFAPattern (default [0,1,1,2] = RGGB)
    push_entry(&mut buf, 50706, T_BYTE, 4, [1, 4, 0, 0]); // DNGVersion
    push_entry(&mut buf, 50707, T_BYTE, 4, [1, 1, 0, 0]); // DNGBackwardVersion
    push_entry(&mut buf, 50713, T_SHORT, 2, inline_u32(0x0002_0002)); // BlackLevelRepeatDim = [2,2]
    push_entry(&mut buf, 50714, T_SHORT, 4, inline_u32(blacklevels_offset)); // BlackLevels
    push_entry(&mut buf, 50717, T_LONG, 1, inline_u32(1024)); // WhiteLevel
    push_entry(&mut buf, 50721, T_SRATIONAL, 9, inline_u32(colormatrix_offset)); // ColorMatrix1
    push_entry(&mut buf, 50728, T_RATIONAL, 3, inline_u32(asshotneutral_offset)); // AsShotNeutral
    buf.extend_from_slice(&0u32.to_le_bytes()); // next IFD = none

    debug_assert_eq!(buf.len(), after_ifd);

    // BlackLevels = [0, 0, 0, 0]
    for _ in 0..4 {
        buf.extend_from_slice(&0u16.to_le_bytes());
    }
    pad_to_even(&mut buf);
    debug_assert_eq!(buf.len() as u32, colormatrix_offset);

    // ColorMatrix1 = identity 3x3 SRATIONAL
    const IDENTITY_3X3: [(i32, i32); 9] = [(1, 1), (0, 1), (0, 1), (0, 1), (1, 1), (0, 1), (0, 1), (0, 1), (1, 1)];
    for (num, den) in IDENTITY_3X3 {
        buf.extend_from_slice(&num.to_le_bytes());
        buf.extend_from_slice(&den.to_le_bytes());
    }
    pad_to_even(&mut buf);
    debug_assert_eq!(buf.len() as u32, asshotneutral_offset);

    // AsShotNeutral = [1/1, 1/1, 1/1] (neutral -> wb_coeffs = [1,1,1,NaN])
    for _ in 0..3 {
        buf.extend_from_slice(&1i32.to_le_bytes());
        buf.extend_from_slice(&1i32.to_le_bytes());
    }
    pad_to_even(&mut buf);
    debug_assert_eq!(buf.len() as u32, pixel_offset);

    // Pixel data: deterministic non-uniform single-channel mosaic.
    for y in 0..height {
        for x in 0..width {
            let v = 100u16 + (((x * 53 + y * 197) % 800) as u16);
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    debug_assert_eq!(buf.len() as u64, pixel_offset as u64 + strip_byte_count as u64);

    let mut f = std::fs::File::create(path)?;
    f.write_all(&buf)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes the 32x24 fixture and confirms its *byte layout* is internally
    /// self-consistent: every out-of-line offset the writer computed actually
    /// lands where the IFD entries say it does, sizes match, and the file
    /// re-opens as a `CGImageSource` at all. This is deliberately not a full
    /// `image_decode::decode()` pixel round-trip — see
    /// `linear_dng_pixel_decode_is_blocked_on_this_machine` below for why
    /// that specific check doesn't currently pass on macOS, and why that's a
    /// platform-API finding rather than a fixture bug.
    ///
    /// mac-only: `image_decode::open_image_source` (Task 9's cfg-split) only
    /// exists under `#[cfg(target_os = "macos")]` — non-mac has no
    /// `CGImageSource` equivalent to open a container without decoding it, so
    /// this specific check has no non-mac counterpart to gate it into instead
    /// (deferred fix from Task 9's review, folded into Task 10).
    #[test]
    #[cfg(target_os = "macos")]
    fn linear_dng_fixture_writes_and_reopens_as_image_source() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_linear_dng_fixture_test_{}.dng",
            std::process::id()
        ));

        write_linear_dng(&path, 32, 24).expect("write_linear_dng failed");

        // `open_image_source` only parses the container (CFURL -> CGImageSource);
        // it doesn't attempt to decode pixels, so this exercises the TIFF/DNG
        // header + IFD parsing this fixture exists to validate, independent of
        // the pixel-decode gap documented below.
        let source = image_decode::open_image_source(&path);
        let _ = std::fs::remove_file(&path);
        source.expect("ImageIO could not even open the fixture as an image source");
    }

    /// TDD record, not a bug report against `write_linear_dng`: this fixture's
    /// byte layout is correct per the DNG 1.7 / TIFF 6.0 spec (verified by hand
    /// against a hex dump — header, IFD entry order/offsets, out-of-line
    /// blocks, and pixel data all line up exactly where the writer computes
    /// them), yet `image_decode::decode()` (which calls
    /// `CGImageSource::image_at_index`) fails on it with "ImageIO could not
    /// decode image".
    ///
    /// What ruled out a fixture bug: systematically toggling one variable at a
    /// time (10 variants total) shows the failure tracks
    /// `PhotometricInterpretation == 34892` (LinearRaw) alone, independent of
    /// every other tag:
    ///   - plain TIFF, PhotometricInterpretation=2 (RGB)              -> decodes fine
    ///   - + DNGVersion/DNGBackwardVersion/ColorMatrix1 tags present  -> still decodes fine
    ///   - swap PhotometricInterpretation to 34892 (LinearRaw)        -> fails
    ///   - + Make/Model/UniqueCameraModel (including a real, ImageIO-
    ///     recognized Apple ProRAW camera string)                    -> still fails
    ///   - two-IFD file (IFD0 = normal 4x3 RGB preview, IFD1 = the
    ///     32x24 LinearRaw data, chained via next-IFD-offset)         -> IFD0 (the
    ///     preview) decodes fine at index 0, but `CGImageSourceGetCount`
    ///     reports only 1 image — the LinearRaw IFD isn't enumerable via this
    ///     API at all, not merely rejected.
    ///
    /// Conclusion: on this machine, `CGImageSourceCreateImageAtIndex` (what
    /// `image_decode::decode` calls) does not expose DNG raw-IFD pixel data
    /// through the generic multi-image API, regardless of tag completeness or
    /// camera recognition. Real DNG raw pixel access on macOS goes through a
    /// different Apple API (`CIRAWFilter`/Core Image), which nothing in this
    /// codebase currently uses. This is a macOS ImageIO API-surface gap, not
    /// something a differently-shaped DNG byte layout can route around — so
    /// this test is `#[ignore]`d with this explanation rather than deleted or
    /// forced green. See Task 7's report for the full experiment log.
    #[test]
    #[ignore = "CGImageSourceCreateImageAtIndex does not expose DNG raw-IFD \
                pixel data on this machine regardless of tag completeness — \
                see this test's doc comment and the Task 7 report for the \
                10-variant elimination log; needs CIRAWFilter, not CGImageSource, \
                to actually decode DNG raw pixels"]
    fn linear_dng_pixel_decode_is_blocked_on_this_machine() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_linear_dng_pixel_test_{}.dng",
            std::process::id()
        ));

        write_linear_dng(&path, 32, 24).expect("write_linear_dng failed");
        let result = image_decode::decode(&path, u32::MAX);
        let _ = std::fs::remove_file(&path);

        let decoded = result.expect("ImageIO failed to decode synthetic Linear DNG fixture");
        assert_eq!(decoded.width, 32, "unexpected decoded width");
        assert_eq!(decoded.height, 24, "unexpected decoded height");
    }

    /// The Task 8 check this binary exists to perform: `rawler` decodes the
    /// synthetic Linear DNG fixture and its pixel samples match the exact
    /// analytic gradient formula `write_linear_dng` wrote, with zero
    /// tolerance (see `compare_against_gradient_ground_truth`'s doc comment
    /// for why zero, not "close enough", is the right bar here). This is the
    /// positive counterpart to `linear_dng_pixel_decode_is_blocked_on_this_machine`
    /// above: where ImageIO can't even expose this DNG's raw-IFD pixels,
    /// rawler decodes them and matches ground truth exactly.
    #[test]
    fn rawler_decodes_linear_dng_matching_gradient_ground_truth() {
        let path = std::env::temp_dir().join(format!("lightphotos_linear_dng_rawler_test_{}.dng", std::process::id()));
        let (width, height) = (64u32, 48u32);

        write_linear_dng(&path, width, height).expect("write_linear_dng failed");
        let decode_result = decode_via_rawler(&path);
        let _ = std::fs::remove_file(&path);

        let raw = decode_result.expect("rawler failed to decode the synthetic Linear DNG fixture");
        let report = compare_against_gradient_ground_truth(&raw, width, height);
        report.expect("rawler's decoded pixels diverged from the analytic gradient ground truth");
    }

    /// Task 10: `RawDevelop::default().develop_intermediate(&raw)
    /// .to_dynamic_image()` — the exact call chain `image_decode::
    /// decode_raw_nonmac` runs after `decode_raw_via_rawler` — completes
    /// without error/panic on the synthetic fixture and produces a
    /// same-dimensions, non-degenerate image. See `develop_smoke_check`'s doc
    /// comment for what this does and doesn't prove.
    #[test]
    fn develop_pipeline_runs_end_to_end_on_linear_dng() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_linear_dng_develop_test_{}.dng",
            std::process::id()
        ));
        let (width, height) = (48u32, 32u32);

        write_linear_dng(&path, width, height).expect("write_linear_dng failed");
        let result = develop_smoke_check(&path, width, height);
        let _ = std::fs::remove_file(&path);

        result.expect("RawDevelop pipeline failed on the synthetic Linear DNG fixture");
    }

    /// A malformed/unsupported input (not a DNG at all) should come back as
    /// an `Err`, not panic — sanity check on `decode_via_rawler`'s error
    /// mapping.
    #[test]
    fn rawler_reports_error_on_non_raw_file() {
        let path = std::env::temp_dir().join(format!("lightphotos_not_a_raw_file_{}.bin", std::process::id()));
        std::fs::write(&path, b"this is not a TIFF or any known RAW format").expect("write dummy file");

        let result = decode_via_rawler(&path);
        let _ = std::fs::remove_file(&path);

        assert!(result.is_err(), "expected rawler to reject a non-RAW file, got Ok");
    }

    /// Locks in `raw_preview::decode_raw_fast_from_bytes`'s current
    /// output on a synthetic Bayer fixture, via `hash::Fnv1a` over
    /// width+height+rgba bytes (see `src/hash.rs` — chosen because it's
    /// stable/deterministic across process runs, unlike `DefaultHasher`).
    /// Any future change to `demosaic_cfa` must keep this passing —
    /// the `Fast` tier's output is meant to stay stable.
    ///
    /// The captured hash below was observed by running this test once with a
    /// dummy value and reading the actual value off the `println!` output —
    /// standard golden-snapshot practice, not hand-computed (a multi-stage
    /// float pipeline's output isn't something to derive by hand).
    #[test]
    fn raw_preview_bayer_fast_tier_matches_golden_hash() {
        let path = std::env::temp_dir().join(format!("lightphotos_bayer_dng_test_{}.dng", std::process::id()));
        let (width, height) = (8u32, 6u32);
        write_bayer_dng(&path, width, height).expect("write_bayer_dng failed");
        let bytes = std::fs::read(&path).expect("read fixture bytes");
        let _ = std::fs::remove_file(&path);

        let decoded = raw_preview::decode_raw_fast_from_bytes(&bytes, u32::MAX)
            .expect("decode_raw_fast_from_bytes failed on synthetic Bayer DNG fixture");

        let mut hasher = hash::Fnv1a::new();
        hasher.write(&decoded.width.to_le_bytes());
        hasher.write(&decoded.height.to_le_bytes());
        hasher.write(&decoded.rgba);
        let golden = hasher.finish();

        println!(
            "bayer golden hash: {golden:#x} ({}x{}, {} rgba bytes)",
            decoded.width,
            decoded.height,
            decoded.rgba.len()
        );
        assert_eq!(
            golden, 0x43ad_e440_61b1_f4b5,
            "Fast-tier Bayer decode output changed from the captured golden hash \
             (see this test's println! output above for the actual value) - if this \
             change is intentional, update the literal; if not, a task's supposedly \
             structure-only refactor changed real output"
        );
    }

    /// Same idea, for the already-linear (`cpp == 3`, `decimate_linear_rgb`)
    /// path.
    ///
    /// Uses `write_linear_dng_with_wb` with a deliberately *non*-neutral
    /// `AsShotNeutral` (`[1/2, 1/1, 2/1]` → `wb_coeffs = [2.0, 1.0, 0.5]`)
    /// rather than the plain `write_linear_dng` fixture. That plain fixture
    /// writes no `AsShotNeutral` at all, so `DngDecoder::get_wb` returns
    /// `[NaN; 4]`, every sample gets multiplied to NaN, `to_srgb_u8`
    /// saturates it to 0, and the resulting "golden" image is uniformly
    /// black — a hash that discriminates output *dimensions* and nothing
    /// else, on the exact function this plan rewrote. With real coefficients
    /// this exercises the per-channel WB multiply, the color matrix, the
    /// highlight rolloff and the gamma LUT on real gradient values, and the
    /// asymmetric coefficients mean a channel-order mistake changes the hash.
    #[test]
    fn raw_preview_linear_fast_tier_matches_golden_hash() {
        let path = std::env::temp_dir().join(format!("lightphotos_linear_wb_dng_test_{}.dng", std::process::id()));
        let (width, height) = (16u32, 12u32);
        write_linear_dng_with_wb(&path, width, height, Some([(1, 2), (1, 1), (2, 1)]))
            .expect("write_linear_dng_with_wb failed");
        let bytes = std::fs::read(&path).expect("read fixture bytes");
        let _ = std::fs::remove_file(&path);

        let decoded = raw_preview::decode_raw_fast_from_bytes(&bytes, u32::MAX)
            .expect("decode_raw_fast_from_bytes failed on the white-balanced Linear DNG fixture");

        let mut hasher = hash::Fnv1a::new();
        hasher.write(&decoded.width.to_le_bytes());
        hasher.write(&decoded.height.to_le_bytes());
        hasher.write(&decoded.rgba);
        let golden = hasher.finish();

        println!(
            "linear golden hash: {golden:#x} ({}x{}, {} rgba bytes)",
            decoded.width,
            decoded.height,
            decoded.rgba.len()
        );
        // Guards the property the fixture change above exists to establish:
        // if this ever goes all-black again, the hash below stops testing any
        // pixel math and silently degrades into a dimensions check.
        assert!(
            decoded.rgba.chunks_exact(4).any(|p| p[0] != 0 || p[1] != 0 || p[2] != 0),
            "decoded image has no non-zero color channel anywhere - the golden hash \
             below would then discriminate nothing but the output dimensions"
        );
        assert_eq!(
            golden, 0x3c3d_4904_3a8d_0cd3,
            "Fast-tier Linear decode output changed from the captured golden hash \
             (see this test's println! output above for the actual value)"
        );
    }

    #[test]
    fn raw_preview_without_white_balance_metadata_is_not_black() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_linear_no_wb_dng_test_{}.dng",
            std::process::id()
        ));
        write_linear_dng(&path, 16, 12).expect("write_linear_dng failed");
        let bytes = std::fs::read(&path).expect("read fixture bytes");
        let _ = std::fs::remove_file(&path);

        let decoded = raw_preview::decode_raw_fast_from_bytes(&bytes, u32::MAX)
            .expect("decode_raw_fast_from_bytes failed without AsShotNeutral");
        assert!(
            decoded
                .rgba
                .chunks_exact(4)
                .any(|p| p[0] != 0 || p[1] != 0 || p[2] != 0),
            "missing white balance metadata must use neutral coefficients"
        );
    }

    /// `DemosaicMode::Quality` (`PPGDemosaic`) has no golden hash to match —
    /// this confirms it decodes without panicking and produces a plausible,
    /// non-degenerate image on the same fixture Task 1 uses. Output is
    /// `half::f16` linear RGBA (8 bytes/pixel), not u8 sRGB (4 bytes/pixel) —
    /// see `DemosaicMode::bytes_per_pixel`.
    #[test]
    fn raw_preview_bayer_quality_tier_runs_without_panicking() {
        let path = std::env::temp_dir().join(format!("lightphotos_bayer_quality_dng_test_{}.dng", std::process::id()));
        let (width, height) = (8u32, 6u32);
        write_bayer_dng(&path, width, height).expect("write_bayer_dng failed");
        let bytes = std::fs::read(&path).expect("read fixture bytes");
        let _ = std::fs::remove_file(&path);

        // decode_raw_fast_from_bytes always uses DemosaicMode::Fast internally
        // (Step 3) - exercise Quality directly via rawler::decode + the same
        // apply_scaling/demosaic_cfa call chain that function makes.
        let source = rawler::rawsource::RawSource::new_from_slice(&bytes);
        let params = rawler::decoders::RawDecodeParams::default();
        let mut raw = rawler::decode(&source, &params).expect("rawler::decode failed on fixture");
        raw.apply_scaling().expect("apply_scaling failed");

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            raw_preview::demosaic_cfa(&mut raw, raw_preview::DemosaicMode::Quality, u32::MAX)
        }));
        let (w, h, rgba) = result.expect("PPGDemosaic panicked").expect("demosaic_cfa returned None");

        assert!(w > 0 && h > 0, "degenerate output dimensions");
        assert_eq!(rgba.len(), (w * h * 8) as usize, "Quality tier must be 8 bytes/pixel (half::f16 linear RGBA)");
        // Color channels only, decoded as f16: alpha is hardcoded opaque by
        // `render_rgb_sample_linear_bytes`, so a plain "any byte nonzero"
        // check would be satisfied by the alpha bytes alone and would pass
        // on a fully-black image — the exact degenerate case this is meant
        // to rule out. Also requires every sample to be finite (no NaN/inf
        // leaking out of the color-matrix/highlight-rolloff math).
        let mut any_nonzero = false;
        for px in rgba.chunks_exact(8) {
            let r = half::f16::from_le_bytes([px[0], px[1]]).to_f32();
            let g = half::f16::from_le_bytes([px[2], px[3]]).to_f32();
            let b = half::f16::from_le_bytes([px[4], px[5]]).to_f32();
            assert!(r.is_finite() && g.is_finite() && b.is_finite(), "non-finite linear sample");
            any_nonzero |= r != 0.0 || g != 0.0 || b != 0.0;
        }
        assert!(any_nonzero, "output looks all-zero/degenerate");
    }

    /// Smoke test for the actual entry point (`decode_raw_quality_from_bytes`)
    /// rather than `demosaic_cfa` directly — the test above never
    /// touches `demosaic_preview`/`decode_raw_preview_from_bytes`'s own `mode`
    /// threading, only the inner demosaic call.
    #[test]
    fn raw_preview_quality_entry_point_produces_linear_f16() {
        let path = std::env::temp_dir().join(format!("lightphotos_bayer_quality_entry_dng_test_{}.dng", std::process::id()));
        let (width, height) = (8u32, 6u32);
        write_bayer_dng(&path, width, height).expect("write_bayer_dng failed");
        let bytes = std::fs::read(&path).expect("read fixture bytes");
        let _ = std::fs::remove_file(&path);

        let decoded = raw_preview::decode_raw_quality_from_bytes(&bytes, u32::MAX)
            .expect("decode_raw_quality_from_bytes failed on synthetic Bayer DNG fixture");
        assert!(decoded.width > 0 && decoded.height > 0);
        assert_eq!(decoded.pixel_format, image_decode::PixelFormat::LinearF16);
        assert_eq!(
            decoded.rgba.len(),
            (decoded.width * decoded.height * 8) as usize,
            "LinearF16 must be 8 bytes/pixel"
        );
    }

    /// Regression test for a real bug: `decode_raw_quality_from_bytes` once
    /// skipped resizing entirely (uploaded the full demosaiced resolution
    /// unconditionally), which broke the Loupe's zoom transform the instant
    /// `Quality` replaced a same-photo `Fast` upload — `zoom` is
    /// screen-px-per-image-px and is reused (not recomputed) across a
    /// same-photo sharper-tier swap, so a wild resolution jump between tiers
    /// made the reused value show a wrongly zoomed-in crop. `Quality`'s
    /// output must respect `max_px` exactly like every other decode tier.
    #[test]
    fn raw_preview_quality_tier_respects_max_px() {
        let path = std::env::temp_dir().join(format!("lightphotos_bayer_quality_bound_dng_test_{}.dng", std::process::id()));
        let (width, height) = (8u32, 6u32);
        write_bayer_dng(&path, width, height).expect("write_bayer_dng failed");
        let bytes = std::fs::read(&path).expect("read fixture bytes");
        let _ = std::fs::remove_file(&path);

        // Full-max_px decode first, to confirm this fixture's natural
        // (unbounded) demosaiced size is actually bigger than the small
        // max_px below — otherwise this test would pass trivially without
        // ever exercising the resize path.
        let unbounded = raw_preview::decode_raw_quality_from_bytes(&bytes, u32::MAX)
            .expect("unbounded decode_raw_quality_from_bytes failed");
        let natural_longest = unbounded.width.max(unbounded.height);

        const MAX_PX: u32 = 4;
        assert!(
            natural_longest > MAX_PX,
            "fixture's natural size ({natural_longest}) must exceed MAX_PX ({MAX_PX}) for this test to mean anything"
        );

        let bounded = raw_preview::decode_raw_quality_from_bytes(&bytes, MAX_PX)
            .expect("bounded decode_raw_quality_from_bytes failed");
        assert!(
            bounded.width.max(bounded.height) <= MAX_PX,
            "Quality tier must respect max_px: got {}x{}, longest side must be <= {MAX_PX}",
            bounded.width,
            bounded.height
        );
        assert_eq!(bounded.pixel_format, image_decode::PixelFormat::LinearF16);
        assert_eq!(
            bounded.rgba.len(),
            (bounded.width * bounded.height * 8) as usize,
            "LinearF16 must be 8 bytes/pixel"
        );
        // Non-degenerate: the box-filter resize didn't just zero everything.
        let mut any_nonzero = false;
        for px in bounded.rgba.chunks_exact(8) {
            let r = half::f16::from_le_bytes([px[0], px[1]]).to_f32();
            let g = half::f16::from_le_bytes([px[2], px[3]]).to_f32();
            let b = half::f16::from_le_bytes([px[4], px[5]]).to_f32();
            assert!(r.is_finite() && g.is_finite() && b.is_finite(), "non-finite resized sample");
            any_nonzero |= r != 0.0 || g != 0.0 || b != 0.0;
        }
        assert!(any_nonzero, "resized output looks all-zero/degenerate");
    }

    /// The panic guard: rawler's `Superpixel3Channel::demosaic` matches the
    /// (ROI-shifted) CFA name against exactly `RGGB`/`BGGR`/`GBRG`/`GRBG` and
    /// falls through to `_ => unreachable!()` for anything else that still
    /// clears its `is_rgb()` check — Fuji X-Trans being the real-world case
    /// (its 36-char name is all R/G/B). `wasm32-unknown-unknown`, the only
    /// production target for this code, is `panic=abort`, so that would take
    /// down the whole decode worker rather than surfacing an error.
    ///
    /// `CFAPattern = [0,1,2,1]` ("RGBG") is the cheapest fixture that
    /// reproduces it: a 2x2 pattern, so rawler decodes it happily, `is_rgb()`
    /// is true (only R/G/B characters, one of each present), and the name is
    /// none of the four — the same `unreachable!()` an X-Trans file reaches,
    /// without hand-building a 6x6 X-Trans DNG. The `catch_unwind` here is
    /// what makes the pre-fix failure legible as a test failure on native
    /// rather than aborting the test binary.
    #[test]
    fn raw_preview_rejects_unsupported_cfa_pattern_without_panicking() {
        let path = std::env::temp_dir().join(format!("lightphotos_bayer_rgbg_dng_test_{}.dng", std::process::id()));
        let (width, height) = (8u32, 6u32);
        write_bayer_dng_with_cfa(&path, width, height, [0, 1, 2, 1]).expect("write_bayer_dng_with_cfa failed");
        let bytes = std::fs::read(&path).expect("read fixture bytes");
        let _ = std::fs::remove_file(&path);

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            raw_preview::decode_raw_fast_from_bytes(&bytes, u32::MAX)
        }))
        .expect("decode_raw_fast_from_bytes panicked on an unsupported CFA pattern (fatal on wasm32)");

        // `DecodedImage` isn't `Debug`, so unwrap the error by hand rather
        // than via `expect_err`.
        let err = match outcome {
            Ok(decoded) => panic!(
                "expected an Err for a CFA pattern rawler's demosaic can't handle, got a {}x{} image",
                decoded.width, decoded.height
            ),
            Err(e) => e,
        };
        assert!(
            err.contains("unsupported RAW layout"),
            "expected an 'unsupported RAW layout' error, got: {err}"
        );
    }

    /// `rawler_full_image_diag` (this file's standalone copy of
    /// `thumbnail.rs`'s `rawler_full_image_from_bytes`) is gated to
    /// `FormatHint::RAF`/`CR3` only — confirmed necessary the hard way (a
    /// real Sony ARW: an earlier, ungated version of this function silently
    /// substituted the camera's own embedded JPEG for the Loupe's real
    /// linear-RAW demosaic on every ARW/CR2/NEF/DNG/RW2/PEF file, since all
    /// of those *also* override `full_image()`, just with containers
    /// `embedded_preview_from_bytes` already opens fine). This fixture has a
    /// perfectly valid preview sub-IFD (`write_dng_with_preview_subifd`,
    /// still exercising the real `SubIFDs` -> `NewSubfileType=1` -> strip
    /// decode plumbing internally before the gate check runs) — the point of
    /// this test is that `format_hint()` for a DNG is `FormatHint::DNG`, not
    /// `RAF`/`CR3`, so the gate must still return `None` regardless of a
    /// present, well-formed preview.
    #[test]
    fn rawler_full_image_ignores_dng_despite_valid_preview_subifd() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_dng_preview_subifd_test_{}.dng",
            std::process::id()
        ));
        write_dng_with_preview_subifd(&path, 8, 6, 4, 3, [200, 100, 50]).expect("write_dng_with_preview_subifd failed");
        let bytes = std::fs::read(&path).expect("read fixture bytes");
        let _ = std::fs::remove_file(&path);

        assert!(
            rawler_full_image_diag(&bytes, u32::MAX).is_none(),
            "DNG must be excluded by the RAF/CR3 format gate even with a valid preview sub-IFD present"
        );
    }

    /// Negative case for the same function: `write_linear_dng`'s fixture has
    /// no `SubIFDs`/preview at all *and* isn't RAF/CR3 — `full_image()` must
    /// return `None` for either reason, proving the fallback doesn't crash
    /// or misbehave on a format/file with no embedded image.
    #[test]
    fn rawler_full_image_returns_none_without_preview_subifd() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_dng_no_preview_subifd_test_{}.dng",
            std::process::id()
        ));
        write_linear_dng(&path, 8, 6).expect("write_linear_dng failed");
        let bytes = std::fs::read(&path).expect("read fixture bytes");
        let _ = std::fs::remove_file(&path);

        assert!(
            rawler_full_image_diag(&bytes, u32::MAX).is_none(),
            "expected None for a DNG fixture with no preview sub-IFD"
        );
    }

    /// THROWAWAY DIAGNOSTIC — investigating "wasm32 Loupe RAW renders too
    /// dark vs Linux" (see plans/... in-flight session). Compares the actual
    /// *linear* (pre-gamma/pre-boost) pixel values the native path and the
    /// wasm32 path each produce for the same real camera file, using real
    /// production code on both sides (not a reimplementation), so a
    /// systematic brightness gap here — rather than in the already-confirmed
    /// -identical gamma/boost formulas — pins the bug to the
    /// demosaic/WB/matrix/scaling stage instead of the tonemap stage.
    ///
    /// - "Native-equivalent": `RawDevelop` with every default step except
    ///   `SRgb` (mirrors what `decode_raw_nonmac` computes right before its
    ///   own gamma+boost) — real rawler code, not preview.rs's.
    /// - "wasm": `raw_preview::decode_raw_quality_from_bytes`'s real
    ///   `LinearF16` output, unpacked back to f32.
    ///
    /// Deliberately a global mean/percentile comparison, not a pixel-exact
    /// diff: `CropDefault`/`crop_area` is confirmed missing on the wasm side
    /// (separate, already-known gap), so the two outputs have different
    /// dimensions and can't be aligned pixel-for-pixel here anyway — but a
    /// systematic *brightness* gap should show up in the aggregate
    /// regardless of the crop difference.
    ///
    /// Path is hardcoded to a real file the user provided for this specific
    /// investigation — not a portable regression test. Remove once the bug
    /// is found and fixed, or gate/relocate if kept.
    #[test]
    #[ignore = "hardcoded path to a real camera file on the developer's machine, not portable"]
    fn diag_wasm_vs_native_linear_brightness() {
        use rawler::imgop::develop::{ProcessingStep, RawDevelop};

        let path = std::path::Path::new("/Users/andyyao/Desktop/07-26 Jackie/DSC02468.ARW");
        let bytes = std::fs::read(path).expect("read real ARW file");

        // --- Native-equivalent: real RawDevelop, every default step but SRgb ---
        let raw = decode_via_rawler(path).expect("decode_via_rawler failed on real ARW");
        let native_dev = RawDevelop {
            steps: vec![
                ProcessingStep::Rescale,
                ProcessingStep::Demosaic,
                ProcessingStep::CropActiveArea,
                ProcessingStep::WhiteBalance,
                ProcessingStep::Calibrate,
                ProcessingStep::CropDefault,
            ],
        };
        let native_linear = native_dev.develop_intermediate(&raw).expect("native develop_intermediate failed");
        let native_pixels: Vec<[f32; 3]> = match native_linear {
            rawler::imgop::develop::Intermediate::ThreeColor(pixels) => pixels.pixels().to_vec(),
            _ => panic!("expected ThreeColor intermediate for a Bayer ARW"),
        };
        let native_mean = mean_rgb(&native_pixels);

        // --- wasm: real raw_preview::decode_raw_quality_from_bytes ---
        let wasm_decoded =
            raw_preview::decode_raw_quality_from_bytes(&bytes, u32::MAX).expect("decode_raw_quality_from_bytes failed");
        assert_eq!(
            wasm_decoded.pixel_format,
            image_decode::PixelFormat::LinearF16,
            "expected LinearF16 output from the Quality tier"
        );
        let wasm_pixels: Vec<[f32; 3]> = wasm_decoded
            .rgba
            .chunks_exact(8)
            .map(|px| {
                [
                    half::f16::from_le_bytes([px[0], px[1]]).to_f32(),
                    half::f16::from_le_bytes([px[2], px[3]]).to_f32(),
                    half::f16::from_le_bytes([px[4], px[5]]).to_f32(),
                ]
            })
            .collect();
        let wasm_mean = mean_rgb(&wasm_pixels);

        println!(
            "native-equivalent mean linear RGB: {:?} ({} px, {}x{})",
            native_mean,
            native_pixels.len(),
            raw.width,
            raw.height
        );
        println!(
            "wasm mean linear RGB:              {:?} ({} px, {}x{})",
            wasm_mean,
            wasm_pixels.len(),
            wasm_decoded.width,
            wasm_decoded.height
        );
        let native_luma = 0.299 * native_mean[0] + 0.587 * native_mean[1] + 0.114 * native_mean[2];
        let wasm_luma = 0.299 * wasm_mean[0] + 0.587 * wasm_mean[1] + 0.114 * wasm_mean[2];
        println!(
            "native-equivalent mean luma: {native_luma:.6}   wasm mean luma: {wasm_luma:.6}   ratio (wasm/native): {:.4}",
            wasm_luma / native_luma
        );

        // Apply the exact gamma+boost formula both platforms claim to share
        // (rawler::imgop::srgb::srgb_apply_gamma then
        // image_decode::apply_raw_preview_boost) to both linear datasets, to
        // see whether the *formula*, correctly applied, actually produces
        // matching brightness — isolating "the formula is wrong" from "the
        // formula isn't what's actually running in the browser."
        fn mean_boosted_srgb(pixels: &[[f32; 3]]) -> [f32; 3] {
            let mut sum = [0f64; 3];
            let mut n = 0u64;
            for p in pixels {
                if p[0].is_finite() && p[1].is_finite() && p[2].is_finite() {
                    for c in 0..3 {
                        let srgb = rawler::imgop::srgb::srgb_apply_gamma(p[c].clamp(0.0, 1.0));
                        sum[c] += image_decode::apply_raw_preview_boost(srgb) as f64;
                    }
                    n += 1;
                }
            }
            let n = n.max(1) as f64;
            [(sum[0] / n) as f32, (sum[1] / n) as f32, (sum[2] / n) as f32]
        }
        let native_boosted = mean_boosted_srgb(&native_pixels);
        let wasm_boosted = mean_boosted_srgb(&wasm_pixels);
        println!("native-equivalent mean boosted sRGB: {native_boosted:?}");
        println!("wasm mean boosted sRGB:               {wasm_boosted:?}");

        // NEW HYPOTHESIS: does the live wasm `decode()` (wasm_worker.rs)
        // actually reach `decode_raw_quality_from_bytes` at all for this
        // file, or does `rawler_full_image_from_bytes` (this session's
        // earlier RAF/embedded-preview work — generic, applies to any
        // format overriding `full_image()`, ARW included) win first and
        // return the camera's own embedded JPEG instead? That's a
        // completely different image (in-camera JPEG rendering, not a raw
        // demosaic) and would explain a real visible difference with
        // matching-formula math being a red herring.
        if let Some(embedded) = rawler_full_image_diag(&bytes, u32::MAX) {
            println!(
                "embedded full_image() JPEG: {}x{}, format {:?}",
                embedded.width, embedded.height, embedded.pixel_format
            );
            let mut sum = [0f64; 3];
            let mut n = 0u64;
            for px in embedded.rgba.chunks_exact(4) {
                sum[0] += px[0] as f64 / 255.0;
                sum[1] += px[1] as f64 / 255.0;
                sum[2] += px[2] as f64 / 255.0;
                n += 1;
            }
            let n = n.max(1) as f64;
            let embedded_mean = [(sum[0] / n) as f32, (sum[1] / n) as f32, (sum[2] / n) as f32];
            println!("embedded JPEG mean sRGB (0..1):       {embedded_mean:?}");
            println!(
                "  vs native-equivalent boosted sRGB:  {native_boosted:?}  <- compare these two"
            );
        } else {
            println!("rawler_full_image_diag returned None for this file — full_image() fallback does NOT apply here");
        }
    }

    fn mean_rgb(pixels: &[[f32; 3]]) -> [f32; 3] {
        let mut sum = [0f64; 3];
        let mut n = 0u64;
        for p in pixels {
            if p[0].is_finite() && p[1].is_finite() && p[2].is_finite() {
                sum[0] += p[0] as f64;
                sum[1] += p[1] as f64;
                sum[2] += p[2] as f64;
                n += 1;
            }
        }
        let n = n.max(1) as f64;
        [(sum[0] / n) as f32, (sum[1] / n) as f32, (sum[2] / n) as f32]
    }

    /// Throwaway visual diagnostic for tuning `AUTO_RAW_DENOISE_STRENGTH`
    /// against the real photo from the "wasm denoise still way worse than
    /// ImageIO" report — writes a PNG so the actual `Quality`-tier output
    /// (real decode, real denoise, real gamma+boost, same as what
    /// `raw_shader.wgsl` renders) can be looked at directly instead of
    /// guessing at the constant again. Not a real regression test; remove
    /// once the constant is settled.
    #[test]
    #[ignore = "hardcoded path to a real camera file on the developer's machine, not portable; writes a PNG for manual visual inspection"]
    fn diag_quality_tier_denoise_png() {
        let path = std::path::Path::new("/Users/andyyao/Desktop/07-26 Jackie/DSC02468.ARW");
        let bytes = std::fs::read(path).expect("read real ARW file");

        let decoded = raw_preview::decode_raw_quality_from_bytes(&bytes, 1600)
            .expect("decode_raw_quality_from_bytes failed");
        assert_eq!(decoded.pixel_format, image_decode::PixelFormat::LinearF16);

        let (w, h) = (decoded.width, decoded.height);
        let mut img = image::RgbImage::new(w, h);
        let enc = |v: f32| -> u8 {
            let srgb = rawler::imgop::srgb::srgb_apply_gamma(v.clamp(0.0, 1.0));
            (image_decode::apply_raw_preview_boost(srgb) * 255.0).round().clamp(0.0, 255.0) as u8
        };
        for (i, px) in decoded.rgba.chunks_exact(8).enumerate() {
            let r = half::f16::from_le_bytes([px[0], px[1]]).to_f32();
            let g = half::f16::from_le_bytes([px[2], px[3]]).to_f32();
            let b = half::f16::from_le_bytes([px[4], px[5]]).to_f32();
            let (x, y) = (i as u32 % w, i as u32 / w);
            img.put_pixel(x, y, image::Rgb([enc(r), enc(g), enc(b)]));
        }
        let out = std::env::temp_dir().join("diag_quality_denoise.png");
        img.save(&out).expect("save png");
        println!("wrote {} ({w}x{h})", out.display());
    }
}
