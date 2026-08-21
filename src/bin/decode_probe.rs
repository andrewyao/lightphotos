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

#[path = "../coregraphics.rs"]
mod coregraphics;
#[path = "../image_decode.rs"]
mod image_decode;

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
    }
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
fn decode_via_rawler(path: &Path) -> Result<rawler::RawImage, String> {
    rawler::decode_file(path).map_err(|e| e.to_string())
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
///
/// Byte layout: 8-byte header, then the IFD, then the two out-of-line value
/// blocks the IFD entries can't inline (`BitsPerSample`'s 3 SHORTs and
/// `ColorMatrix1`'s 9 SRATIONALs), then the pixel data. Every out-of-line
/// offset in this fixed tag set happens to land on an even byte already, but
/// the code still checks/pads defensively rather than assuming that.
fn write_linear_dng(path: &Path, width: u32, height: u32) -> std::io::Result<()> {
    use std::io::Write;

    const T_BYTE: u16 = 1;
    const T_SHORT: u16 = 3;
    const T_LONG: u16 = 4;
    const T_SRATIONAL: u16 = 10;

    const ENTRY_COUNT: u16 = 14;
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
    let ifd_size = 2 + (ENTRY_COUNT as usize) * 12 + 4;
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
    buf.extend_from_slice(&ENTRY_COUNT.to_le_bytes());
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
    #[test]
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
}
