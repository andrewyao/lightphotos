// SPDX-License-Identifier: GPL-3.0-or-later

//! `decode_probe`: a RAW decode test harness. It writes synthetic DNG
//! fixtures (Linear DNG, Bayer, and DNG with a preview sub-IFD) and checks
//! rawler's decode against the known pixel values it wrote. Extra CLI paths
//! get a decode and brightness report for real camera files.
//!
//! ImageIO cannot decode LinearRaw DNG pixels (see
//! `linear_dng_pixel_decode_is_blocked_on_this_machine`), so the baseline is
//! the analytic gradient the fixture was built from, not ImageIO.
//!
//! There is no lib target, so modules come in by `#[path]` and `crate::`
//! resolves as in the main binary.

#![allow(dead_code)]

#[cfg(target_os = "macos")]
#[path = "../coregraphics.rs"]
mod coregraphics;
#[path = "../hash.rs"]
mod hash;
#[path = "../image_decode.rs"]
mod image_decode;
#[path = "preview.rs"]
mod raw_preview;
// For `denoise_linear_rgb_buffer`, used by `raw_preview`'s `Quality` tier.
#[path = "../develop.rs"]
mod develop;

use std::path::Path;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // One decode, then exit: lets `/usr/bin/time -l` report that decode's
    // own peak RSS instead of the whole fixture suite's.
    match args.first().map(String::as_str) {
        Some("--nonmac-decode") => return run_nonmac_decode_probe(&args[1..]),
        Some("--wasm-quality-decode") => return run_wasm_quality_decode_probe(&args[1..]),
        Some("--compare") => return run_compare_png(&args[1..]),
        _ => {}
    }

    let dir = std::env::temp_dir().join("lightphotos-decode-probe");
    std::fs::create_dir_all(&dir).expect("create probe fixture dir");
    let dng_path = dir.join("linear_test.dng");
    let (width, height) = (256u32, 192u32);
    write_linear_dng(&dng_path, width, height).expect("write fixture DNG");

    println!("--- Synthetic Linear DNG: rawler decode vs analytic gradient ground truth ---");
    match decode_via_rawler(&dng_path) {
        Ok(raw) => match compare_against_gradient_ground_truth(&raw, width, height) {
            Ok(report) => {
                println!(
                    "rawler decoded {}x{} cpp={} bps={}: {report}",
                    raw.width, raw.height, raw.cpp, raw.bps
                );
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

    // Real RAW files from the CLI have no ground truth, so this only reports
    // decode success, shape, and timing.
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

        // Brightness of ImageIO's decode (what the mac app shows) against
        // `raw_preview`'s `Fast` decode (what the browser shows).
        #[cfg(target_os = "macos")]
        match (image_decode::decode(&path, 1600), std::fs::read(&path)) {
            (Ok(imageio), Ok(bytes)) => match raw_preview::decode_raw_fast_from_bytes(&bytes, 1600)
            {
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

        // Brightness of the camera-rendered EXIF thumbnail. If this is dark
        // too, the darkness comes from the camera, not our RAW pipeline.
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

        // Brightness of a reimplementation of the reference app's develop
        // pipeline, using vanilla rawler.
        #[cfg(target_os = "macos")]
        match reference_mimic_avg_luma(&path) {
            Ok((luma, (r, g, b))) => {
                println!("  reference-mimic: avg={luma:.1}/255  R={r:.1} G={g:.1} B={b:.1}")
            }
            Err(e) => println!("  reference-mimic FAILED: {e}"),
        }
    }
}

/// `decode_probe --nonmac-decode <path> <max_dim> [png_out]`: one call to the
/// native non-mac RAW decode path, for peak-RSS and wall-time measurement.
fn run_nonmac_decode_probe(args: &[String]) {
    let path = args
        .first()
        .expect("usage: --nonmac-decode <path> <max_dim> [png_out]");
    let max_dim: u32 = args
        .get(1)
        .expect("max_dim required")
        .parse()
        .expect("max_dim must be a number");

    let bytes = std::fs::read(path).expect("read RAW file");
    let t0 = std::time::Instant::now();
    let img = image_decode::decode_raw_nonmac_from_bytes(&bytes, max_dim)
        .expect("decode_raw_nonmac_from_bytes failed");
    println!(
        "nonmac-decode {}x{} in {:?}",
        img.width,
        img.height,
        t0.elapsed()
    );

    if let Some(out) = args.get(2) {
        save_decoded_png(&img, out);
    }
}

/// `decode_probe --wasm-quality-decode <path> <max_px> [png_out]`: one call
/// to the wasm32-shared `Quality` preview tier, runnable on mac under
/// `raw-probe` since it has no wasm-only dependency.
fn run_wasm_quality_decode_probe(args: &[String]) {
    let path = args
        .first()
        .expect("usage: --wasm-quality-decode <path> <max_px> [png_out]");
    let max_px: u32 = args
        .get(1)
        .expect("max_px required")
        .parse()
        .expect("max_px must be a number");

    let bytes = std::fs::read(path).expect("read RAW file");
    let t0 = std::time::Instant::now();
    let img = raw_preview::decode_raw_quality_from_bytes(&bytes, max_px)
        .expect("decode_raw_quality_from_bytes failed");
    println!(
        "wasm-quality-decode {}x{} in {:?}",
        img.width,
        img.height,
        t0.elapsed()
    );

    if let Some(out) = args.get(2) {
        save_decoded_png(&img, out);
    }
}

/// `decode_probe --compare <a.png> <b.png>`: mean absolute difference and
/// PSNR between two same-size PNGs, for the raw-decode-memory task's
/// before/after quality check.
fn run_compare_png(args: &[String]) {
    let a = image::open(args.first().expect("usage: --compare <a.png> <b.png>"))
        .expect("open a.png")
        .into_rgb8();
    let b = image::open(args.get(1).expect("usage: --compare <a.png> <b.png>"))
        .expect("open b.png")
        .into_rgb8();
    assert_eq!(
        (a.width(), a.height()),
        (b.width(), b.height()),
        "compared images must be the same size"
    );

    let (mut sum_abs, mut sum_sq, mut max_abs, mut n) = (0f64, 0f64, 0f64, 0f64);
    for (pa, pb) in a.pixels().zip(b.pixels()) {
        for c in 0..3 {
            let d = (pa[c] as f64 - pb[c] as f64).abs();
            sum_abs += d;
            sum_sq += d * d;
            max_abs = max_abs.max(d);
            n += 1.0;
        }
    }
    let mae = sum_abs / n;
    let mse = sum_sq / n;
    let psnr = if mse == 0.0 {
        f64::INFINITY
    } else {
        20.0 * 255f64.log10() - 10.0 * mse.log10()
    };
    println!(
        "{}x{}: MAE={mae:.4} MSE={mse:.4} PSNR={psnr:.2}dB max_abs_diff={max_abs}",
        a.width(),
        a.height()
    );
}

/// Writes a `DecodedImage` as PNG, applying the same gamma and display boost
/// as the loupe shader for `LinearF16` so both pixel formats look right.
fn save_decoded_png(img: &image_decode::DecodedImage, out: &str) {
    match img.pixel_format {
        image_decode::PixelFormat::Srgb8 => {
            let buf = image::RgbaImage::from_raw(img.width, img.height, img.rgba.clone())
                .expect("rgba buffer size mismatch");
            buf.save(out).expect("save png");
        }
        image_decode::PixelFormat::LinearF16 => {
            let mut buf = image::RgbImage::new(img.width, img.height);
            let enc = |v: f32| -> u8 {
                let srgb = rawler::imgop::srgb::srgb_apply_gamma(v.clamp(0.0, 1.0));
                (image_decode::apply_raw_preview_boost(srgb) * 255.0)
                    .round()
                    .clamp(0.0, 255.0) as u8
            };
            for (i, px) in img.rgba.chunks_exact(8).enumerate() {
                let r = half::f16::from_le_bytes([px[0], px[1]]).to_f32();
                let g = half::f16::from_le_bytes([px[2], px[3]]).to_f32();
                let b = half::f16::from_le_bytes([px[4], px[5]]).to_f32();
                let (x, y) = (i as u32 % img.width, i as u32 / img.width);
                buf.put_pixel(x, y, image::Rgb([enc(r), enc(g), enc(b)]));
            }
            buf.save(out).expect("save png");
        }
    }
    println!("wrote {out}");
}

/// Copy of `thumbnail.rs`'s `embedded_preview_from_bytes`: decodes the EXIF
/// IFD1 thumbnail JPEG. Copied so this binary avoids `thumbnail.rs`'s cfg
/// split and its `crate::paths` dependency.
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
        image_decode::DecodedImage::new_tracked(image_decode::DecodedImageFields {
            width: nw,
            height: nh,
            rgba,
            pixel_format: image_decode::PixelFormat::Srgb8,
        }),
        orientation,
    ))
}

/// Copy of `thumbnail.rs`'s `rawler_full_image_from_bytes`, for the same
/// reason as `embedded_preview_diag`. Not mac-only because regression tests
/// below use it.
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
        .map(|code| {
            image_decode::exif_code_from_rawler_orientation(rawler::Orientation::from_u16(code))
        })
        .unwrap_or(1);

    let (nw, nh) = image_decode::fit_within(w, h, max_px);
    let rgba = if (nw, nh) == (w, h) {
        img.into_raw()
    } else {
        image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3).into_raw()
    };
    Some(image_decode::apply_exif_orientation(
        image_decode::DecodedImage::new_tracked(image_decode::DecodedImageFields {
            width: nw,
            height: nh,
            rgba,
            pixel_format: image_decode::PixelFormat::Srgb8,
        }),
        orientation,
    ))
}

/// Mean brightness from a reimplementation of the reference app's
/// `develop_internal` on vanilla rawler 0.7.2. `highlight_compression = 4.0`
/// is a guess at its default; it only affects near-clipped highlights.
#[cfg(target_os = "macos")]
fn reference_mimic_avg_luma(path: &Path) -> Result<(f64, (f64, f64, f64)), String> {
    use rawler::imgop::develop::{Intermediate, ProcessingStep, RawDevelop};

    let mut raw = decode_via_rawler(path)?;
    let original_white_level = raw.whitelevel.0.first().copied().unwrap_or(u16::MAX as u32) as f32;
    let original_black_level = raw
        .blacklevel
        .levels
        .first()
        .map(|r| r.as_f32())
        .unwrap_or(0.0);
    for level in raw.whitelevel.0.iter_mut() {
        *level = u32::MAX;
    }

    let mut developer = RawDevelop::default();
    developer.steps.retain(|&step| step != ProcessingStep::SRgb);
    let mut developed = developer
        .develop_intermediate(&raw)
        .map_err(|e| e.to_string())?;

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
        let sr8 =
            image_decode::apply_raw_preview_boost(rawler::imgop::srgb::srgb_apply_gamma(r)) * 255.0;
        let sg8 =
            image_decode::apply_raw_preview_boost(rawler::imgop::srgb::srgb_apply_gamma(g)) * 255.0;
        let sb8 =
            image_decode::apply_raw_preview_boost(rawler::imgop::srgb::srgb_apply_gamma(b)) * 255.0;
        sum += (sr8 + sg8 + sb8) as f64;
        sr += sr8 as f64;
        sg += sg8 as f64;
        sb += sb8 as f64;
        n += 1;
    }
    let n = n.max(1);
    Ok((
        sum / (n as f64 * 3.0),
        (sr / n as f64, sg / n as f64, sb / n as f64),
    ))
}

/// Mean of all R, G, and B values, 0..=255.
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

/// Per-channel mean. Catches color casts that `avg_luma` averages away.
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

/// Decodes with rawler, returning its native `RawImage` so the ground-truth
/// check sees the raw 16-bit samples. For the fixture's uncompressed LinearRaw
/// strips, rawler unpacks samples directly with no resampling or color math.
fn decode_via_rawler(path: &Path) -> Result<rawler::RawImage, String> {
    image_decode::decode_raw_via_rawler(path)
}

/// Runs `RawDevelop::default().develop_intermediate` and `to_dynamic_image`,
/// the calls `decode_raw_nonmac` makes, on the fixture. Develop changes the
/// pixel values, so this checks only for success, matching dimensions, and a
/// non-uniform result.
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

    // A uniform result means the gradient was not processed.
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

/// Checks decoded samples against the gradient `write_linear_dng` wrote,
/// `R=G=B=(x * 65535 / width) as u16`, with zero tolerance. The decode is a
/// plain byte unpack, so any difference is a real bug such as a wrong stride,
/// byte order, or offset.
fn compare_against_gradient_ground_truth(
    raw: &rawler::RawImage,
    width: u32,
    height: u32,
) -> Result<String, String> {
    if raw.width != width as usize || raw.height != height as usize {
        return Err(format!(
            "dimension mismatch: fixture is {width}x{height}, rawler reports {}x{}",
            raw.width, raw.height
        ));
    }
    if raw.cpp != 3 {
        return Err(format!(
            "expected cpp=3 (RGB, already demosaiced), rawler reports cpp={}",
            raw.cpp
        ));
    }
    let data = match &raw.data {
        rawler::RawImageData::Integer(v) => v,
        rawler::RawImageData::Float(_) => {
            return Err("expected 16-bit integer samples, rawler returned f32 samples".to_string());
        }
    };
    let expected_len = width as usize * height as usize * 3;
    if data.len() != expected_len {
        return Err(format!(
            "sample count mismatch: expected {expected_len} (w*h*cpp), got {}",
            data.len()
        ));
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
        return Err(format!(
            "{report} (first mismatch at pixel ({x},{y}): expected {expected}, got {got})"
        ));
    }

    Ok(report)
}

/// Writes a minimal Linear DNG: one little-endian TIFF IFD with
/// PhotometricInterpretation 34892 (LinearRaw) and `width * height` RGB16
/// samples forming a horizontal gradient.
///
/// Tags, in the ascending order TIFF requires: NewSubfileType = 0 (ImageIO
/// rejects the file without it), ImageWidth, ImageLength, BitsPerSample,
/// Compression = 1, PhotometricInterpretation, StripOffsets, SamplesPerPixel,
/// RowsPerStrip, StripByteCounts, PlanarConfiguration = 1, DNGVersion,
/// DNGBackwardVersion, and an identity ColorMatrix1 (readers need one).
/// No AsShotNeutral, so rawler's `wb_coeffs` are all NaN.
///
/// Layout: 8-byte header, IFD, out-of-line values, pixel data.
fn write_linear_dng(path: &Path, width: u32, height: u32) -> std::io::Result<()> {
    write_linear_dng_with_wb(path, width, height, None)
}

/// [`write_linear_dng`] with an optional `AsShotNeutral` of three
/// `(numerator, denominator)` rationals. rawler turns it into
/// `wb_coeffs = [1/n0, 1/n1, 1/n2, NaN]`.
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
    /// TIFF out-of-line values start on a word boundary.
    fn pad_to_even(buf: &mut Vec<u8>) {
        if buf.len() % 2 != 0 {
            buf.push(0);
        }
    }

    // Every per-image value fits inline, so the layout does not depend on
    // width or height. Compute out-of-line offsets first so the IFD can be
    // written in one pass.
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

    buf.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00]); // "II" + magic 42 (LE)
    buf.extend_from_slice(&IFD_OFFSET.to_le_bytes());

    buf.extend_from_slice(&entry_count.to_le_bytes());
    push_entry(&mut buf, 254, T_LONG, 1, inline_u32(0)); // NewSubfileType = 0 (primary image)
    push_entry(&mut buf, 256, T_LONG, 1, inline_u32(width)); // ImageWidth
    push_entry(&mut buf, 257, T_LONG, 1, inline_u32(height)); // ImageLength
    push_entry(
        &mut buf,
        258,
        T_SHORT,
        3,
        inline_u32(bits_per_sample_offset),
    ); // BitsPerSample
    push_entry(&mut buf, 259, T_SHORT, 1, inline_u16(1)); // Compression = none
    push_entry(&mut buf, 262, T_SHORT, 1, inline_u16(34892)); // PhotometricInterpretation = LinearRaw
    push_entry(&mut buf, 273, T_LONG, 1, inline_u32(pixel_offset)); // StripOffsets
    push_entry(&mut buf, 277, T_SHORT, 1, inline_u16(3)); // SamplesPerPixel
    push_entry(&mut buf, 278, T_LONG, 1, inline_u32(height)); // RowsPerStrip
    push_entry(&mut buf, 279, T_LONG, 1, inline_u32(strip_byte_count)); // StripByteCounts
    push_entry(&mut buf, 284, T_SHORT, 1, inline_u16(1)); // PlanarConfiguration = chunky
    push_entry(&mut buf, 50706, T_BYTE, 4, [1, 4, 0, 0]); // DNGVersion
    push_entry(&mut buf, 50707, T_BYTE, 4, [1, 1, 0, 0]); // DNGBackwardVersion
    push_entry(
        &mut buf,
        50721,
        T_SRATIONAL,
        9,
        inline_u32(color_matrix_offset),
    ); // ColorMatrix1
    if as_shot_neutral.is_some() {
        push_entry(
            &mut buf,
            50728,
            T_RATIONAL,
            3,
            inline_u32(as_shot_neutral_offset),
        ); // AsShotNeutral
    }
    buf.extend_from_slice(&0u32.to_le_bytes()); // next IFD offset = none

    debug_assert_eq!(
        buf.len(),
        after_ifd,
        "IFD size drifted from the computed layout"
    );

    // Out-of-line: BitsPerSample = [16, 16, 16].
    for _ in 0..3 {
        buf.extend_from_slice(&16u16.to_le_bytes());
    }
    pad_to_even(&mut buf);
    debug_assert_eq!(buf.len() as u32, color_matrix_offset);

    // Out-of-line: ColorMatrix1, identity 3x3 SRATIONAL (num, denom).
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

    // Out-of-line: AsShotNeutral, 3 RATIONAL (num, denom), optional.
    if let Some(neutral) = as_shot_neutral {
        debug_assert_eq!(buf.len() as u32, as_shot_neutral_offset);
        for (num, den) in neutral {
            buf.extend_from_slice(&num.to_le_bytes());
            buf.extend_from_slice(&den.to_le_bytes());
        }
        pad_to_even(&mut buf);
    }
    debug_assert_eq!(buf.len() as u32, pixel_offset);

    // Pixel data: horizontal gradient test pattern, R=G=B per pixel.
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
    debug_assert_eq!(
        buf.len() as u64,
        pixel_offset as u64 + strip_byte_count as u64
    );

    let mut f = std::fs::File::create(path)?;
    f.write_all(&buf)?;
    Ok(())
}

/// [`write_linear_dng`]'s IFD plus a `SubIFDs` tag pointing at an
/// uncompressed 8-bit RGB preview IFD with `NewSubfileType = 1`. That is the
/// shape rawler's `DngDecoder::full_image()` reads. The preview is a solid
/// color so tests can assert on it easily.
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
    /// Pushes a 12-byte IFD entry and returns the offset of its value field,
    /// so out-of-line offsets can be patched in later.
    fn push_entry(buf: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: [u8; 4]) -> usize {
        let value_pos = buf.len() + 8;
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&typ.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&value);
        value_pos
    }
    /// TIFF out-of-line values start on a word boundary.
    fn pad_to_even(buf: &mut Vec<u8>) {
        if buf.len() % 2 != 0 {
            buf.push(0);
        }
    }

    let mut buf: Vec<u8> = Vec::new();

    buf.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00]); // "II" + magic 42 (LE)
    buf.extend_from_slice(&8u32.to_le_bytes()); // root IFD at offset 8

    // Root IFD: `write_linear_dng`'s 14 entries, plus SubIFDs.
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
    assert!(
        strip_byte_count_u64 <= u32::MAX as u64,
        "fixture too large for a LONG StripByteCounts"
    );
    push_entry(
        &mut buf,
        279,
        T_LONG,
        1,
        inline_u32(strip_byte_count_u64 as u32),
    ); // StripByteCounts
    push_entry(&mut buf, 284, T_SHORT, 1, inline_u16(1)); // PlanarConfiguration = chunky
    push_entry(&mut buf, 50706, T_BYTE, 4, [1, 4, 0, 0]); // DNGVersion
    push_entry(&mut buf, 50707, T_BYTE, 4, [1, 1, 0, 0]); // DNGBackwardVersion
    let sub_ifds_pos = push_entry(&mut buf, 330, T_LONG, 1, inline_u32(0)); // SubIFDs (patched)
    let color_matrix_pos = push_entry(&mut buf, 50721, T_SRATIONAL, 9, inline_u32(0)); // ColorMatrix1 (patched)
    buf.extend_from_slice(&0u32.to_le_bytes()); // next IFD offset = none

    // Root out-of-line: BitsPerSample = [16, 16, 16].
    pad_to_even(&mut buf);
    let bits_per_sample_offset = buf.len() as u32;
    for _ in 0..3 {
        buf.extend_from_slice(&16u16.to_le_bytes());
    }

    // Root out-of-line: ColorMatrix1, identity 3x3 SRATIONAL (num, denom).
    pad_to_even(&mut buf);
    let color_matrix_offset = buf.len() as u32;
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

    // Preview sub-IFD (NewSubfileType=1), referenced via root's SubIFDs.
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
    push_entry(
        &mut buf,
        279,
        T_LONG,
        1,
        inline_u32(preview_strip_byte_count),
    ); // StripByteCounts
    buf.extend_from_slice(&0u32.to_le_bytes()); // next IFD offset = none

    // Preview out-of-line: BitsPerSample = [8, 8, 8].
    pad_to_even(&mut buf);
    let preview_bits_offset = buf.len() as u32;
    for _ in 0..3 {
        buf.extend_from_slice(&8u16.to_le_bytes());
    }

    // Preview pixel data: solid color, 1 byte/sample, chunky RGB.
    pad_to_even(&mut buf);
    let preview_pixel_offset = buf.len() as u32;
    for _ in 0..(preview_width * preview_height) {
        buf.extend_from_slice(&preview_rgb);
    }

    // Root pixel data: horizontal gradient test pattern, R=G=B, 16-bit.
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

    // Patch back every out-of-line/sub-IFD offset now that all of them are known.
    buf[bits_per_sample_pos..bits_per_sample_pos + 4]
        .copy_from_slice(&bits_per_sample_offset.to_le_bytes());
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

/// Writes a minimal RGGB Bayer DNG with 16-bit samples, black and white
/// levels, and a neutral `AsShotNeutral`. Pixels follow
/// `v = 100 + ((x * 53 + y * 197) % 800)` so demosaic has real variation.
/// `width` and `height` must be even.
///
/// rawler reads only `CFAPattern` for the CFA, so `CFARepeatPatternDim` is
/// omitted. Color codes are RED=0, GREEN=1, BLUE=2, so `[0,1,1,2]` is RGGB.
fn write_bayer_dng(path: &Path, width: u32, height: u32) -> std::io::Result<()> {
    write_bayer_dng_with_cfa(path, width, height, [0, 1, 1, 2])
}

/// [`write_bayer_dng`] with an explicit `CFAPattern`, for fixtures outside
/// the four RGGB-family patterns.
fn write_bayer_dng_with_cfa(
    path: &Path,
    width: u32,
    height: u32,
    cfa_pattern: [u8; 4],
) -> std::io::Result<()> {
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

    assert!(
        width % 2 == 0 && height % 2 == 0,
        "Bayer fixture needs even dimensions"
    );

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
    push_entry(
        &mut buf,
        50721,
        T_SRATIONAL,
        9,
        inline_u32(colormatrix_offset),
    ); // ColorMatrix1
    push_entry(
        &mut buf,
        50728,
        T_RATIONAL,
        3,
        inline_u32(asshotneutral_offset),
    ); // AsShotNeutral
    buf.extend_from_slice(&0u32.to_le_bytes()); // next IFD = none

    debug_assert_eq!(buf.len(), after_ifd);

    // BlackLevels = [0, 0, 0, 0]
    for _ in 0..4 {
        buf.extend_from_slice(&0u16.to_le_bytes());
    }
    pad_to_even(&mut buf);
    debug_assert_eq!(buf.len() as u32, colormatrix_offset);

    // ColorMatrix1 = identity 3x3 SRATIONAL
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
    debug_assert_eq!(
        buf.len() as u64,
        pixel_offset as u64 + strip_byte_count as u64
    );

    let mut f = std::fs::File::create(path)?;
    f.write_all(&buf)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ImageIO can open the fixture as a `CGImageSource`, which checks the
    /// TIFF/DNG container without decoding pixels. mac-only because
    /// `open_image_source` exists only there.
    #[test]
    #[cfg(target_os = "macos")]
    fn linear_dng_fixture_writes_and_reopens_as_image_source() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_linear_dng_fixture_test_{}.dng",
            std::process::id()
        ));

        write_linear_dng(&path, 32, 24).expect("write_linear_dng failed");

        let source = image_decode::open_image_source(&path);
        let _ = std::fs::remove_file(&path);
        source.expect("ImageIO could not even open the fixture as an image source");
    }

    /// Documents a macOS limit, not a fixture bug. The fixture matches the
    /// DNG spec, but `CGImageSourceCreateImageAtIndex` fails on any DNG with
    /// PhotometricInterpretation 34892 (LinearRaw). Toggling one tag at a
    /// time showed LinearRaw alone causes it; in a two-IFD file the LinearRaw
    /// IFD is not even counted. Decoding it needs `CIRAWFilter`.
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

    /// rawler decodes the Linear DNG fixture to exactly the gradient written,
    /// which ImageIO cannot do (see the test above).
    #[test]
    fn rawler_decodes_linear_dng_matching_gradient_ground_truth() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_linear_dng_rawler_test_{}.dng",
            std::process::id()
        ));
        let (width, height) = (64u32, 48u32);

        write_linear_dng(&path, width, height).expect("write_linear_dng failed");
        let decode_result = decode_via_rawler(&path);
        let _ = std::fs::remove_file(&path);

        let raw = decode_result.expect("rawler failed to decode the synthetic Linear DNG fixture");
        let report = compare_against_gradient_ground_truth(&raw, width, height);
        report.expect("rawler's decoded pixels diverged from the analytic gradient ground truth");
    }

    /// See `develop_smoke_check`.
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

    /// The full bytes pipeline (develop, boost, denoise) keeps the fixture's
    /// left-to-right gradient.
    #[test]
    fn decode_raw_nonmac_from_bytes_develops_gradient_fixture() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_raw_bytes_core_{}.dng",
            std::process::id()
        ));
        let (w, h) = (48u32, 16u32);
        write_linear_dng(&path, w, h).expect("write_linear_dng failed");
        let bytes = std::fs::read(&path).expect("read fixture");
        let _ = std::fs::remove_file(&path);

        let img = image_decode::decode_raw_nonmac_from_bytes(&bytes, u32::MAX)
            .expect("bytes-core RAW decode should succeed");

        assert_eq!((img.width, img.height), (w, h));
        let row_mid = (h / 2) as usize;
        let at = |x: usize| img.rgba[(row_mid * w as usize + x) * 4] as i32;
        assert!(
            at((w - 2) as usize) - at(1) > 20,
            "gradient should brighten left->right, got {} -> {}",
            at(1),
            at((w - 2) as usize)
        );
    }

    /// Path and bytes entry points give byte-identical output, so native and
    /// wasm export can share one pipeline.
    #[test]
    fn decode_raw_nonmac_bytes_and_path_agree() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_raw_bytes_parity_{}.dng",
            std::process::id()
        ));
        let (w, h) = (40u32, 30u32);
        write_linear_dng(&path, w, h).expect("write_linear_dng failed");
        let bytes = std::fs::read(&path).expect("read fixture");

        let via_path = image_decode::decode_raw_nonmac(&path, u32::MAX).expect("path decode");
        let via_bytes =
            image_decode::decode_raw_nonmac_from_bytes(&bytes, u32::MAX).expect("bytes decode");
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            (via_path.width, via_path.height),
            (via_bytes.width, via_bytes.height)
        );
        assert_eq!(
            via_path.rgba, via_bytes.rgba,
            "bytes-core and path decode must be byte-identical"
        );
    }

    /// A non-RAW file returns `Err` instead of panicking.
    #[test]
    fn rawler_reports_error_on_non_raw_file() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_not_a_raw_file_{}.bin",
            std::process::id()
        ));
        std::fs::write(&path, b"this is not a TIFF or any known RAW format")
            .expect("write dummy file");

        let result = decode_via_rawler(&path);
        let _ = std::fs::remove_file(&path);

        assert!(
            result.is_err(),
            "expected rawler to reject a non-RAW file, got Ok"
        );
    }

    /// Golden hash of the `Fast` tier on the Bayer fixture. `hash::Fnv1a` is
    /// stable across runs, unlike `DefaultHasher`. The expected value was
    /// captured from a run; update it only for intentional output changes.
    #[test]
    fn raw_preview_bayer_fast_tier_matches_golden_hash() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_bayer_dng_test_{}.dng",
            std::process::id()
        ));
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

    /// Golden hash for the cpp == 3 path (`decimate_linear_rgb`). The fixture
    /// has a non-neutral `AsShotNeutral` (`wb_coeffs = [2.0, 1.0, 0.5]`) so
    /// white balance, matrix, rolloff, and gamma all affect the hash, and a
    /// channel-order mistake changes it.
    #[test]
    fn raw_preview_linear_fast_tier_matches_golden_hash() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_linear_wb_dng_test_{}.dng",
            std::process::id()
        ));
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
        // An all-black image would make the hash check only dimensions.
        assert!(
            decoded
                .rgba
                .chunks_exact(4)
                .any(|p| p[0] != 0 || p[1] != 0 || p[2] != 0),
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

    /// `Quality` (PPG) demosaic runs without panicking and yields finite,
    /// non-black f16 output at 8 bytes per pixel.
    #[test]
    fn raw_preview_bayer_quality_tier_runs_without_panicking() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_bayer_quality_dng_test_{}.dng",
            std::process::id()
        ));
        let (width, height) = (8u32, 6u32);
        write_bayer_dng(&path, width, height).expect("write_bayer_dng failed");
        let bytes = std::fs::read(&path).expect("read fixture bytes");
        let _ = std::fs::remove_file(&path);

        // Call `demosaic_cfa` directly to force `Quality` mode.
        let source = rawler::rawsource::RawSource::new_from_slice(&bytes);
        let params = rawler::decoders::RawDecodeParams::default();
        let mut raw = rawler::decode(&source, &params).expect("rawler::decode failed on fixture");
        raw.apply_scaling().expect("apply_scaling failed");

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            raw_preview::demosaic_cfa(&mut raw, raw_preview::DemosaicMode::Quality, u32::MAX)
        }));
        let (w, h, rgba) = result
            .expect("PPGDemosaic panicked")
            .expect("demosaic_cfa returned None");

        assert!(w > 0 && h > 0, "degenerate output dimensions");
        assert_eq!(
            rgba.len(),
            (w * h * 8) as usize,
            "Quality tier must be 8 bytes/pixel (half::f16 linear RGBA)"
        );
        // Check color channels only. Alpha is always 1, so checking any byte
        // would pass on a black image.
        let mut any_nonzero = false;
        for px in rgba.chunks_exact(8) {
            let r = half::f16::from_le_bytes([px[0], px[1]]).to_f32();
            let g = half::f16::from_le_bytes([px[2], px[3]]).to_f32();
            let b = half::f16::from_le_bytes([px[4], px[5]]).to_f32();
            assert!(
                r.is_finite() && g.is_finite() && b.is_finite(),
                "non-finite linear sample"
            );
            any_nonzero |= r != 0.0 || g != 0.0 || b != 0.0;
        }
        assert!(any_nonzero, "output looks all-zero/degenerate");
    }

    /// The `Quality` entry point returns `LinearF16`, covering the `mode`
    /// plumbing the test above skips.
    #[test]
    fn raw_preview_quality_entry_point_produces_linear_f16() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_bayer_quality_entry_dng_test_{}.dng",
            std::process::id()
        ));
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

    /// `Quality` output respects `max_px`. The Loupe reuses its zoom across
    /// tiers of the same photo, so an oversized `Quality` image shows zoomed in.
    #[test]
    fn raw_preview_quality_tier_respects_max_px() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_bayer_quality_bound_dng_test_{}.dng",
            std::process::id()
        ));
        let (width, height) = (8u32, 6u32);
        write_bayer_dng(&path, width, height).expect("write_bayer_dng failed");
        let bytes = std::fs::read(&path).expect("read fixture bytes");
        let _ = std::fs::remove_file(&path);

        // The fixture must be larger than MAX_PX or the resize never runs.
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
        // The resize must not zero the image.
        let mut any_nonzero = false;
        for px in bounded.rgba.chunks_exact(8) {
            let r = half::f16::from_le_bytes([px[0], px[1]]).to_f32();
            let g = half::f16::from_le_bytes([px[2], px[3]]).to_f32();
            let b = half::f16::from_le_bytes([px[4], px[5]]).to_f32();
            assert!(
                r.is_finite() && g.is_finite() && b.is_finite(),
                "non-finite resized sample"
            );
            any_nonzero |= r != 0.0 || g != 0.0 || b != 0.0;
        }
        assert!(any_nonzero, "resized output looks all-zero/degenerate");
    }

    /// An unsupported CFA returns an error instead of panicking, which would
    /// abort the wasm32 worker. `CFAPattern = [0,1,2,1]` ("RGBG") passes
    /// `is_rgb()` but is not an RGGB-family name, so it reaches the same
    /// `unreachable!()` in `Superpixel3Channel` that X-Trans does.
    #[test]
    fn raw_preview_rejects_unsupported_cfa_pattern_without_panicking() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_bayer_rgbg_dng_test_{}.dng",
            std::process::id()
        ));
        let (width, height) = (8u32, 6u32);
        write_bayer_dng_with_cfa(&path, width, height, [0, 1, 2, 1])
            .expect("write_bayer_dng_with_cfa failed");
        let bytes = std::fs::read(&path).expect("read fixture bytes");
        let _ = std::fs::remove_file(&path);

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            raw_preview::decode_raw_fast_from_bytes(&bytes, u32::MAX)
        }))
        .expect(
            "decode_raw_fast_from_bytes panicked on an unsupported CFA pattern (fatal on wasm32)",
        );

        // `DecodedImage` is not `Debug`, so `expect_err` is unavailable.
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

    /// The `full_image()` path only runs for RAF and CR3. Other formats (ARW,
    /// NEF, DNG, ...) also implement `full_image()` and would swap in the
    /// camera JPEG for the real RAW decode. A DNG with a valid preview must
    /// still return `None`.
    #[test]
    fn rawler_full_image_ignores_dng_despite_valid_preview_subifd() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos_dng_preview_subifd_test_{}.dng",
            std::process::id()
        ));
        write_dng_with_preview_subifd(&path, 8, 6, 4, 3, [200, 100, 50])
            .expect("write_dng_with_preview_subifd failed");
        let bytes = std::fs::read(&path).expect("read fixture bytes");
        let _ = std::fs::remove_file(&path);

        assert!(
            rawler_full_image_diag(&bytes, u32::MAX).is_none(),
            "DNG must be excluded by the RAF/CR3 format gate even with a valid preview sub-IFD present"
        );
    }

    /// A DNG with no preview sub-IFD returns `None`.
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

    /// Manual diagnostic: compares mean linear brightness of native
    /// `RawDevelop` (every default step but `SRgb`) and the wasm `Quality`
    /// decode on a real camera file, plus the embedded JPEG if any. Uses a
    /// hardcoded local path.
    #[test]
    #[ignore = "hardcoded path to a real camera file on the developer's machine, not portable"]
    fn diag_wasm_vs_native_linear_brightness() {
        use rawler::imgop::develop::{ProcessingStep, RawDevelop};

        let path = std::path::Path::new("/Users/andyyao/Desktop/07-26 Jackie/DSC02468.ARW");
        let bytes = std::fs::read(path).expect("read real ARW file");

        // Native: `RawDevelop` with every default step but `SRgb`.
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
        let native_linear = native_dev
            .develop_intermediate(&raw)
            .expect("native develop_intermediate failed");
        let native_pixels: Vec<[f32; 3]> = match native_linear {
            rawler::imgop::develop::Intermediate::ThreeColor(pixels) => pixels.pixels().to_vec(),
            _ => panic!("expected ThreeColor intermediate for a Bayer ARW"),
        };
        let native_mean = mean_rgb(&native_pixels);

        // wasm: the `Quality` tier.
        let wasm_decoded = raw_preview::decode_raw_quality_from_bytes(&bytes, u32::MAX)
            .expect("decode_raw_quality_from_bytes failed");
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

        // Apply the shared gamma + boost to both, to compare display brightness.
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
            [
                (sum[0] / n) as f32,
                (sum[1] / n) as f32,
                (sum[2] / n) as f32,
            ]
        }
        let native_boosted = mean_boosted_srgb(&native_pixels);
        let wasm_boosted = mean_boosted_srgb(&wasm_pixels);
        println!("native-equivalent mean boosted sRGB: {native_boosted:?}");
        println!("wasm mean boosted sRGB:               {wasm_boosted:?}");

        // Compare against the camera's embedded JPEG, when rawler has one.
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
            let embedded_mean = [
                (sum[0] / n) as f32,
                (sum[1] / n) as f32,
                (sum[2] / n) as f32,
            ];
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
        [
            (sum[0] / n) as f32,
            (sum[1] / n) as f32,
            (sum[2] / n) as f32,
        ]
    }

    /// Manual diagnostic for tuning `AUTO_RAW_DENOISE_STRENGTH`: writes the
    /// `Quality` output, with gamma and boost applied, to a PNG. Uses a
    /// hardcoded local path.
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
            (image_decode::apply_raw_preview_boost(srgb) * 255.0)
                .round()
                .clamp(0.0, 255.0) as u8
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
