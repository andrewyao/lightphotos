// SPDX-License-Identifier: GPL-3.0-or-later

//! The non-mac (Linux/Windows/wasm32) half of `image_decode.rs`'s decode and
//! metadata API, plus the RAW-preview tonemap it shares with
//! `raw/preview.rs`. This gets pulled into `image_decode.rs` via
//! `#[path]` plus a glob `pub(crate) use`, so every existing
//! `image_decode::decode` / `image_decode::pixel_size` / etc. call site keeps
//! working unchanged — see that file's own `mod nonmac_decode` declaration
//! for the exact cfg gate this whole file lives under.
//!
//! ## Pipeline position
//! - `decode()` is Linux/Windows's counterpart to `image_decode.rs`'s mac
//!   `decode()` — called from `loader.rs`'s `Job::Preview`/`Job::Full`
//!   (Pipeline 1) and `export.rs`'s `do_export` (Pipeline 3).
//! - `decode_nonraw_from_bytes` is reused, bytes-in instead of
//!   path-in, by wasm32's own non-RAW decode path (`wasm_worker.rs`) — one
//!   implementation instead of two that could drift apart.
//! - `apply_raw_preview_boost`/`exif_code_from_rawler_orientation` are also
//!   reused by `raw/preview.rs` (wasm32's RAW tiers) and `thumbnail.rs`
//!   (the embedded-preview fallback) — see each function's doc comment.
//! - See `ARCHITECTURE.md`.

use std::path::Path;

// Only the `not(macos)` items below actually use these. The dual-gated
// (`raw-probe`) items never touch `DecodedImage`/`PixelFormat`, so on a
// mac+raw-probe build — where only that dual-gated half compiles — this
// import would otherwise sit unused.
#[cfg(not(target_os = "macos"))]
use crate::image_decode::{apply_exif_orientation, fit_within, DecodedImage, PixelFormat};

/// File extensions we treat as camera RAW on the non-mac decode path — these
/// get routed to `decode_raw_nonmac` instead of the `image` crate, which
/// can't parse RAW containers at all. Mirrors the RAW subset of
/// `navigation.rs`'s `IMAGE_EXTS`. This is `pub(crate)` rather than private
/// because `app/web.rs`'s wasm32 thumbnail decode reuses the same list to
/// skip RAW files for now — that milestone's scope is JPEG only (RAW gets
/// its own decode path later); `image::load_from_memory` can't read RAW
/// sensor data at all, since it's not a baseline-TIFF image despite the
/// TIFF-based container.
#[cfg(not(target_os = "macos"))]
pub(crate) fn is_raw_extension(path: &Path) -> bool {
    const RAW_EXTS: &[&str] = &[
        "cr2", "cr3", "nef", "arw", "dng", "raf", "rw2", "orf", "pef", "srw",
    ];
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| RAW_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Decode a RAW/DNG file into rawler's native `RawImage`: undeveloped sensor
/// samples (still mosaiced for a typical Bayer/X-Trans camera file, cpp=1;
/// already demosaiced for Linear DNG, cpp=3/4) — no white balance, color
/// matrix, or gamma applied yet.
///
/// Gated on `feature = "raw-probe"` as well as `not(target_os = "macos")` so
/// `decode_probe.rs`'s ground-truth harness can call this same function too —
/// including from a mac dev build, via `cargo run --bin decode_probe
/// --features raw-probe`, where `rawler` is available as the optional
/// top-level dependency — instead of duplicating the `rawler::decode_file`
/// call site. That harness compares these raw, undeveloped samples directly
/// against an analytic fixture, so it must NOT be routed through
/// [`decode_raw_nonmac`]'s develop/demosaic pipeline below.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
// On a mac+raw-probe build this compiles into the *main* `lightphotos`
// binary too (the feature has no way to scope itself to just
// `decode_probe.rs`'s build), where nothing actually calls it — only
// `decode_probe.rs`'s own copy of this module does. It's genuinely used on
// non-mac (by `decode_raw_nonmac` below) and via `cargo run/test --bin
// decode_probe --features raw-probe`.
#[allow(dead_code)]
pub(crate) fn decode_raw_via_rawler(path: &Path) -> Result<rawler::RawImage, String> {
    rawler::decode_file(path).map_err(|e| e.to_string())
}

/// Maps rawler's own `Orientation` enum (read from the file's EXIF/TIFF
/// orientation tag during decode) to the raw EXIF orientation code (`1..=8`)
/// that [`apply_exif_orientation`] expects. It's a direct rename, not a
/// reinterpretation — rawler's variants are the same 8 EXIF cases in the same
/// order (see `rawler::Orientation::from_u16`).
///
/// `pub(crate)` (re-exported into `image_decode::*` via this module's glob
/// `use`) so `thumbnail.rs`'s `rawler_full_image_from_bytes` can reuse the
/// exact same `meta.exif.orientation` (`Option<u16>`) -> EXIF-code conversion
/// this file's own `decode_raw_nonmac` uses, instead of a second copy. Dual
/// `cfg` (not just non-mac), matching `apply_raw_preview_boost`/
/// `decode_raw_via_rawler` above, so `probe.rs`'s own diagnostic copy can
/// reach it from a mac dev build under `--features raw-probe` too.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
pub(crate) fn exif_code_from_rawler_orientation(o: rawler::Orientation) -> u8 {
    use rawler::Orientation::*;
    match o {
        Normal => 1,
        HorizontalFlip => 2,
        Rotate180 => 3,
        VerticalFlip => 4,
        Transpose => 5,
        Rotate90 => 6,
        Transverse => 7,
        Rotate270 => 8,
        Unknown => 1,
    }
}

/// Brightness+contrast boost applied unconditionally to every RAW photo's
/// display rendering — ported verbatim, constants included, from the
/// reference display shader this pipeline matches. Both callers here
/// (`decode_raw_nonmac` below, `raw/preview.rs`'s `to_srgb_u8`) are
/// RAW-only by construction, so there's no separate flag needed to scope it.
///
/// A naive linear-matrix -> sRGB-gamma RAW conversion comes out flatter and
/// darker than a camera's own JPEG or a tool like Lightroom, by design —
/// those apply an additional, deliberately-tuned rendering transform on top
/// of the "correct" linear conversion. This is that transform, not a fix to
/// the conversion itself.
///
/// Takes an already sRGB-gamma-encoded value, not linear — gamma is applied
/// first, and this boost runs after.
///
/// Has a WGSL twin, `raw_shader.wgsl`'s `apply_raw_preview_boost` — same
/// constants, same formula — for `raw/preview.rs`'s `Quality` tier
/// (wasm32 Loupe), which stops at linear camera-RGB and lets the GPU do this
/// instead of baking it into a CPU LUT the way every caller of *this*
/// function does.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
const RAW_PREVIEW_BRIGHTNESS_GAMMA: f32 = 1.1;
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
const RAW_PREVIEW_CONTRAST_MIX: f32 = 0.75;

#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
pub(crate) fn apply_raw_preview_boost(srgb: f32) -> f32 {
    let brightened = srgb.powf(1.0 / RAW_PREVIEW_BRIGHTNESS_GAMMA);
    let contrast_curve = brightened * brightened * (3.0 - 2.0 * brightened);
    (brightened + (contrast_curve - brightened) * RAW_PREVIEW_CONTRAST_MIX).clamp(0.0, 1.0)
}

/// Fixed strength (same 0..=100 scale as the user-facing denoise slider,
/// `develop::denoise_linear_rgb_buffer`'s `strength` parameter) for the
/// automatic, always-on RAW decode-time denoise pass — the non-mac/wasm32
/// counterpart of whatever proprietary noise reduction ImageIO's own RAW
/// decode applies internally on macOS. Not exposed to the user and never
/// read from `Adjustments`; a tuned constant, same pattern as
/// `RAW_PREVIEW_BRIGHTNESS_GAMMA`/`RAW_PREVIEW_CONTRAST_MIX` above.
/// Deliberately not ISO-scaled: real ISO-adaptive strength needs an EXIF ISO
/// read, which non-mac's `read_metadata` below doesn't have wired up yet.
/// Applied by `decode_raw_nonmac` below and by `raw/preview.rs`'s
/// `Quality` tier only — `Fast`'s quarter-resolution 2x2 bin already gets
/// free noise reduction from averaging, so denoising it again would just
/// soften an already-small preview further.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
pub(crate) const AUTO_RAW_DENOISE_STRENGTH: f32 = 25.0;

/// Decodes a camera RAW file on non-mac platforms via `rawler`: decode the
/// raw sensor samples ([`decode_raw_via_rawler`]), then run rawler's own
/// `RawDevelop` pipeline (rescale -> demosaic -> active-area crop -> white
/// balance -> color-matrix calibration -> default crop -> sRGB gamma) to turn
/// them into a viewable image — a raw sensor mosaic isn't displayable pixel
/// data on its own. Finally applies the file's EXIF/TIFF orientation the same
/// way the mac arm and the non-mac JPEG/PNG/TIFF arm do, so callers never see
/// a sideways/mirrored RAW regardless of platform.
///
/// Known gap: this pipeline has only been exercised end-to-end against the
/// synthetic Linear DNG fixture (cpp=3, so the demosaic branch below is never
/// taken) via `decode_probe.rs` — no real-camera Bayer-CFA RAW (CR2/NEF/ARW)
/// has been run through it yet. Also worth knowing: rawler's own demosaic
/// dispatch (`RawDevelop::develop_intermediate`) panics via `todo!()` for a
/// couple of CFA layouts it doesn't recognize. Ordinary Bayer/X-Trans cameras
/// don't hit those arms, but it's a real, narrow panic surface inherited from
/// the dependency, not something this function can guard against from the
/// outside.
///
/// Stage vocabulary note: this delegates black/white-normalize, white
/// balance, and demosaic to `RawDevelop`'s own `ProcessingStep`s — this is
/// literally the same `PPGDemosaic`/`apply_scaling`-equivalent machinery that
/// `raw/preview.rs`'s `Fast`/`Quality` `DemosaicMode` now also calls
/// directly. Neither path applies any per-camera profile — both use
/// `raw.color_matrix` (DNG-embedded calibration) directly.
#[cfg(not(target_os = "macos"))]
fn decode_raw_nonmac(path: &Path, max_dim: u32) -> Result<DecodedImage, String> {
    let raw = decode_raw_via_rawler(path)?;
    // `RawSource::new` (memory-mapped), not `std::fs::read` +
    // `new_from_slice`: the latter reads the whole file into a `Vec` and then
    // `new_from_slice` clones it again into its own `Arc<Vec<u8>>` — two
    // full-file heap copies alive at once, on top of `raw`'s already-decoded
    // sensor buffer above. A large RAW file can add hundreds of MB of
    // transient heap for a value (orientation) that's one byte deep in the
    // file's metadata. The mmap is file-backed and reclaimable, not a heap
    // duplicate.
    let orientation = rawler::rawsource::RawSource::new(path)
        .ok()
        .and_then(|source| {
            let params = rawler::decoders::RawDecodeParams::default();
            rawler::get_decoder(&source)
                .ok()
                .and_then(|decoder| decoder.raw_metadata(&source, &params).ok())
                .and_then(|meta| meta.exif.orientation)
                .map(|orientation| {
                    exif_code_from_rawler_orientation(rawler::Orientation::from_u16(orientation))
                })
        })
        .unwrap_or_else(|| exif_code_from_rawler_orientation(raw.orientation));

    let developed = rawler::imgop::develop::RawDevelop::default()
        .develop_intermediate(&raw)
        .map_err(|e| e.to_string())?;
    let dynamic = developed
        .to_dynamic_image()
        .ok_or("rawler produced an empty developed image")?;
    let mut img = dynamic.into_rgba8();

    // `RawDevelop::default()`'s `SRgb` step already gamma-encoded these
    // bytes, so now apply the same brightness/contrast boost
    // `raw/preview.rs`'s LUT uses, via a small local u8->u8 lookup table
    // (256 entries, built once per call — this path isn't hot-looped the way
    // the wasm decode path is, so a static/`OnceLock` would be overkill).
    let boost_lut: [u8; 256] =
        std::array::from_fn(|i| (apply_raw_preview_boost(i as f32 / 255.0) * 255.0).round() as u8);
    for px in img.pixels_mut() {
        px[0] = boost_lut[px[0] as usize];
        px[1] = boost_lut[px[1] as usize];
        px[2] = boost_lut[px[2] as usize];
    }

    // Automatic decode-time denoise — see `AUTO_RAW_DENOISE_STRENGTH`'s own
    // doc comment for why this exists and why it's a fixed constant, not a
    // slider. `RawDevelop`/the boost LUT above are a vendored black box with
    // no mid-pipeline hook, so this runs as a post-pass on the finished
    // sRGB8 image rather than pre-gamma the way `raw/preview.rs`'s
    // `Quality` tier can (see that file's own comment) — round-tripping
    // through this codebase's usual simple 2.2 gamma approximation
    // (`develop.rs`/`image_ops.rs`), not the real piecewise sRGB curve
    // `apply_raw_preview_boost` uses, since that's the domain
    // `denoise_linear_rgb_buffer` expects.
    let (dw, dh) = (img.width() as usize, img.height() as usize);
    let linear: Vec<[f32; 3]> = img
        .pixels()
        .map(|p| {
            [
                (p[0] as f32 / 255.0).powf(2.2),
                (p[1] as f32 / 255.0).powf(2.2),
                (p[2] as f32 / 255.0).powf(2.2),
            ]
        })
        .collect();
    let denoised = crate::develop::denoise_linear_rgb_buffer(AUTO_RAW_DENOISE_STRENGTH, dw, dh, &linear);
    let encode = |v: f32| (v.max(0.0).powf(1.0 / 2.2) * 255.0).round().clamp(0.0, 255.0) as u8;
    for (px, lin) in img.pixels_mut().zip(denoised) {
        px[0] = encode(lin[0]);
        px[1] = encode(lin[1]);
        px[2] = encode(lin[2]);
    }

    let (src_w, src_h) = (img.width(), img.height());
    if src_w == 0 || src_h == 0 {
        return Err("decoded RAW image has zero dimension".into());
    }
    let (w, h) = fit_within(src_w, src_h, max_dim);
    let rgba = if (w, h) == (src_w, src_h) {
        img.into_raw()
    } else {
        image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3).into_raw()
    };
    Ok(apply_exif_orientation(
        DecodedImage {
            width: w,
            height: h,
            rgba,
            pixel_format: PixelFormat::Srgb8,
        },
        orientation,
    ))
}

/// Decodes `path`, optionally downscaling so neither side exceeds `max_dim`.
/// JPEG/PNG/TIFF go through the `image` crate; RAW extensions are routed to
/// `decode_raw_nonmac`. Applies EXIF orientation via the decoder's own
/// `orientation()` (JPEG/TIFF support it; PNG has none and defaults to
/// identity), matching the mac arm's behavior so callers never see a
/// sideways/mirrored image regardless of platform.
#[cfg(not(target_os = "macos"))]
pub fn decode(path: &Path, max_dim: u32) -> Result<DecodedImage, String> {
    if is_raw_extension(path) {
        return decode_raw_nonmac(path, max_dim);
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    decode_nonraw_from_bytes(&bytes, max_dim)
}

/// The non-RAW half of [`decode`] above, minus the file read — kept
/// bytes-based so wasm32's own decode path (`app/web.rs`, reading via
/// `FileSystemFileHandle` instead of `std::fs::read`) can share this exact
/// logic (full decode + correct EXIF orientation + `Lanczos3` resize)
/// instead of a second implementation that could quietly drift out of sync.
/// That's not hypothetical — it already happened once: an earlier wasm32
/// version used `DynamicImage::thumbnail()` (a fast, low-quality filter, not
/// `Lanczos3`) and applied no orientation at all.
#[cfg(not(target_os = "macos"))]
pub(crate) fn decode_nonraw_from_bytes(
    bytes: &[u8],
    max_dim: u32,
) -> Result<DecodedImage, String> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;

    use image::ImageDecoder;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);

    let mut img = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    let img = img.into_rgba8();

    let (src_w, src_h) = (img.width(), img.height());
    if src_w == 0 || src_h == 0 {
        return Err("decoded image has zero dimension".into());
    }
    let (w, h) = fit_within(src_w, src_h, max_dim);
    let rgba = if (w, h) == (src_w, src_h) {
        img.into_raw()
    } else {
        image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3).into_raw()
    };
    Ok(DecodedImage {
        width: w,
        height: h,
        rgba,
        pixel_format: PixelFormat::Srgb8,
    })
}

/// Capture time for `path`. Non-mac has no EXIF reader wired up yet, so this
/// always falls back to file mtime — the same fallback the mac arm takes for
/// any file whose EXIF is absent or unparseable.
#[cfg(not(target_os = "macos"))]
pub fn capture_time(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// Reads camera/lens/exposure metadata plus capture date for `path`. Non-mac
/// has no EXIF reader wired up yet, so every field is `None` except
/// `source_size`, which comes from [`pixel_size`] (the stored, pre-orientation
/// dimensions), swapped into display orientation for the quarter-turn EXIF
/// orientations via [`orientation_of`] — mirroring the mac arm's logic exactly
/// so the result lines up with what [`decode`] actually produces.
#[cfg(not(target_os = "macos"))]
pub fn read_metadata(path: &Path) -> crate::image_decode::ImageMetadata {
    let source_size = pixel_size(path).map(|(w, h)| {
        if matches!(orientation_of(path), 5..=8) {
            (h, w)
        } else {
            (w, h)
        }
    });
    crate::image_decode::ImageMetadata {
        source_size,
        ..crate::image_decode::ImageMetadata::default()
    }
}

/// The image's stored pixel dimensions, before EXIF orientation is applied —
/// same semantics as the mac arm (see above). Reads just the header via the
/// `image` crate's decoder, no full decode.
#[cfg(not(target_os = "macos"))]
pub fn pixel_size(path: &Path) -> Option<(u32, u32)> {
    image::image_dimensions(path).ok()
}

/// The EXIF orientation of the image at `path`. Non-mac: reads it via the
/// `image` crate's own decoder-level orientation support (the same mechanism
/// [`decode`]'s non-mac arm already uses), converted back to the raw EXIF
/// code (`1..=8`) via [`image::metadata::Orientation::to_exif`]. Returns the
/// identity orientation (`1`) for RAW files (no `image`-crate decoder), PNGs
/// (no orientation tag), or any unreadable/unrecognized file.
#[cfg(not(target_os = "macos"))]
pub fn orientation_of(path: &Path) -> u8 {
    let Ok(reader) = image::ImageReader::open(path) else {
        return 1;
    };
    let Ok(reader) = reader.with_guessed_format() else {
        return 1;
    };
    let Ok(mut decoder) = reader.into_decoder() else {
        return 1;
    };
    use image::ImageDecoder;
    decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms)
        .to_exif()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checks the two properties that matter for "why this fixes darkness":
    /// identity at both ends of the range (0 stays 0, 1 stays 1 — this is a
    /// display boost, not a levels shift that clips or crushes), and genuine
    /// brightening in between (a mid-gray input comes out brighter, not
    /// darker or unchanged). Confirmed by hand: `0.5f32.powf(1.0/1.1)` ≈
    /// 0.533, and the contrast curve/mix only pull further in that same
    /// direction for a value already above its own midpoint.
    #[test]
    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    fn raw_preview_boost_is_identity_at_endpoints_and_brightens_midtones() {
        assert_eq!(apply_raw_preview_boost(0.0), 0.0);
        assert!((apply_raw_preview_boost(1.0) - 1.0).abs() < 1e-6);

        let mid = apply_raw_preview_boost(0.5);
        assert!(mid > 0.5, "expected midtone brightening, got {mid}");
    }
}
