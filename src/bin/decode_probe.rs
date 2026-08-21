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

#![allow(dead_code)]

#[path = "../coregraphics.rs"]
mod coregraphics;
#[path = "../image_decode.rs"]
mod image_decode;

use std::path::Path;

fn main() {}

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
}
