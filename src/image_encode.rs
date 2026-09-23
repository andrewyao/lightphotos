// SPDX-License-Identifier: GPL-3.0-or-later

//! Encode RGBA8 pixels to JPEG. macOS uses ImageIO through a CoreGraphics
//! bitmap context. Other targets use the pure-Rust `mozjpeg-rs` encoder.
//! Export and the on-disk thumbnail cache both write through here.

use std::path::Path;

#[cfg(target_os = "macos")]
use std::ffi::c_void;

#[cfg(target_os = "macos")]
use objc2_core_foundation::CFString;
#[cfg(target_os = "macos")]
use objc2_image_io::CGImageDestination;

#[cfg(target_os = "macos")]
use crate::coregraphics;

/// Encode `rgba` (tightly packed RGBA8, row-major, sRGB, alpha opaque or
/// premultiplied) to a JPEG at `out`.
#[cfg(target_os = "macos")]
#[hotpath::measure]
pub fn encode_jpeg(out: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("cannot encode a zero-sized image".into());
    }
    let bytes_per_row = width as usize * 4;
    if rgba.len() < bytes_per_row * height as usize {
        return Err("pixel buffer too small for the given dimensions".into());
    }

    // `bitmap_context_image` copies the pixels, so `buffer` may drop after it.
    let mut buffer = rgba.to_vec();
    // SAFETY: buffer is width*height*4 bytes and outlives `ctx`.
    let ctx = unsafe {
        coregraphics::srgb_bitmap_context(
            buffer.as_mut_ptr() as *mut c_void,
            width,
            height,
            bytes_per_row,
        )?
    };

    let image = coregraphics::bitmap_context_image(&ctx)?;

    let url = coregraphics::file_url(out)?;

    let jpeg_uti = CFString::from_str("public.jpeg");
    // SAFETY: url and type are valid. No options, so ImageIO uses its default
    // JPEG quality.
    let dest = unsafe { CGImageDestination::with_url(&url, &jpeg_uti, 1, None) }
        .ok_or("could not create image destination (unwritable path?)")?;

    // SAFETY: dest/image are valid; no per-image properties.
    unsafe { CGImageDestination::add_image(&dest, &image, None) };
    // SAFETY: dest is valid; returns false if the file could not be written.
    let ok = unsafe { CGImageDestination::finalize(&dest) };
    if !ok {
        return Err("CGImageDestinationFinalize failed (could not write file)".into());
    }
    Ok(())
}

/// Encode `rgba` (same pixel contract as [`encode_jpeg`]) to JPEG bytes with
/// `mozjpeg-rs`. The wasm32 export path uses this because it has no `std::fs`.
///
/// Quality is 90, not mozjpeg's default 75, to stay close to ImageIO's
/// default on macOS. Built on macOS only under `raw-probe`, for its tests.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[allow(dead_code)]
#[hotpath::measure]
pub fn encode_jpeg_to_vec(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err("cannot encode a zero-sized image".into());
    }
    let bytes_per_row = width as usize * 4;
    if rgba.len() < bytes_per_row * height as usize {
        return Err("pixel buffer too small for the given dimensions".into());
    }

    mozjpeg_rs::Encoder::new(mozjpeg_rs::Preset::default())
        .quality(90)
        .encode_rgba(rgba, width, height)
        .map_err(|e| e.to_string())
}

/// Encode `rgba` to a JPEG file at `out` with `mozjpeg-rs`. The wasm32 build
/// has no caller, hence `dead_code`.
#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
#[hotpath::measure]
pub fn encode_jpeg(out: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let jpeg_data = encode_jpeg_to_vec(width, height, rgba)?;
    std::fs::write(out, jpeg_data).map_err(|e| e.to_string())
}

/// `jpeg` with its EXIF block replaced by one this app writes: sRGB, the pixel
/// size, and the capture date when `taken` has one. The pixels are already
/// upright, so there is no orientation tag, and nothing else from the source
/// carries over. Without the date an export would lose it, and Immich would
/// file an upload under the day it was exported.
pub fn with_exif(
    jpeg: &[u8],
    width: u32,
    height: u32,
    taken: Option<&crate::image_decode::CaptureStamp>,
) -> Vec<u8> {
    const ASCII: u16 = 2;
    const SHORT: u16 = 3;
    const LONG: u16 = 4;
    const UNDEFINED: u16 = 7;
    let ascii = |s: &str| {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    };
    // Sorted by tag, as TIFF requires. The offset tags are EXIF 2.31.
    let mut exif_ifd: Vec<(u16, u16, Vec<u8>)> = vec![(0x9000, UNDEFINED, b"0231".to_vec())];
    if let Some(stamp) = taken {
        exif_ifd.push((0x9003, ASCII, ascii(&stamp.local)));
        if let Some(offset) = &stamp.offset {
            exif_ifd.push((0x9011, ASCII, ascii(offset)));
        }
    }
    exif_ifd.push((0xA001, SHORT, 1u16.to_be_bytes().to_vec()));
    exif_ifd.push((0xA002, LONG, width.to_be_bytes().to_vec()));
    exif_ifd.push((0xA003, LONG, height.to_be_bytes().to_vec()));

    // Big-endian TIFF: header, IFD0 holding only the pointer to the EXIF IFD,
    // then the EXIF IFD and the values too long to sit in an entry.
    const EXIF_IFD_AT: u32 = 8 + 2 + 12 + 4;
    let mut tiff = b"MM\0\x2a\0\0\0\x08\0\x01".to_vec();
    tiff.extend_from_slice(&0x8769u16.to_be_bytes());
    tiff.extend_from_slice(&LONG.to_be_bytes());
    tiff.extend_from_slice(&1u32.to_be_bytes());
    tiff.extend_from_slice(&EXIF_IFD_AT.to_be_bytes());
    tiff.extend_from_slice(&0u32.to_be_bytes());

    let mut data_at = EXIF_IFD_AT + 2 + 12 * exif_ifd.len() as u32 + 4;
    let mut data = Vec::new();
    tiff.extend_from_slice(&(exif_ifd.len() as u16).to_be_bytes());
    for (tag, kind, value) in &exif_ifd {
        let unit = match *kind {
            SHORT => 2,
            LONG => 4,
            _ => 1,
        };
        tiff.extend_from_slice(&tag.to_be_bytes());
        tiff.extend_from_slice(&kind.to_be_bytes());
        tiff.extend_from_slice(&((value.len() / unit) as u32).to_be_bytes());
        if value.len() <= 4 {
            let mut inline = value.clone();
            inline.resize(4, 0);
            tiff.extend_from_slice(&inline);
        } else {
            tiff.extend_from_slice(&data_at.to_be_bytes());
            data.extend_from_slice(value);
            if value.len() % 2 == 1 {
                data.push(0);
            }
            data_at = EXIF_IFD_AT + 2 + 12 * exif_ifd.len() as u32 + 4 + data.len() as u32;
        }
    }
    tiff.extend_from_slice(&0u32.to_be_bytes());
    tiff.extend_from_slice(&data);

    let mut app1 = vec![0xFF, 0xE1];
    app1.extend_from_slice(&((2 + 6 + tiff.len()) as u16).to_be_bytes());
    app1.extend_from_slice(b"Exif\0\0");
    app1.extend_from_slice(&tiff);
    splice_app1(jpeg, &app1)
}

/// Put `app1` after SOI (and after a JFIF APP0, which by convention comes
/// first), dropping any EXIF APP1 already there. Returns `jpeg` unchanged if
/// its header doesn't parse.
fn splice_app1(jpeg: &[u8], app1: &[u8]) -> Vec<u8> {
    if !jpeg.starts_with(&[0xFF, 0xD8]) {
        return jpeg.to_vec();
    }
    let mut head = vec![0xFF, 0xD8];
    let mut placed = false;
    let mut i = 2;
    // Walk the APPn segments; the first other marker ends the header.
    while i + 4 <= jpeg.len() && jpeg[i] == 0xFF && (0xE0..=0xEF).contains(&jpeg[i + 1]) {
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        let Some(segment) = jpeg.get(i..i + 2 + len) else {
            return jpeg.to_vec();
        };
        let is_jfif = jpeg[i + 1] == 0xE0 && segment[4..].starts_with(b"JFIF\0");
        let is_exif = jpeg[i + 1] == 0xE1 && segment[4..].starts_with(b"Exif\0\0");
        if !is_jfif && !placed {
            head.extend_from_slice(app1);
            placed = true;
        }
        if !is_exif {
            head.extend_from_slice(segment);
        }
        i += 2 + len;
    }
    if !placed {
        head.extend_from_slice(app1);
    }
    head.extend_from_slice(&jpeg[i..]);
    head
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_decode;

    /// Encode solid red, decode the bytes back, and check that the size and
    /// the approximate color survive.
    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    #[test]
    fn encode_jpeg_to_vec_round_trips() {
        let (w, h) = (8u32, 6u32);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            rgba.extend_from_slice(&[220, 30, 30, 255]);
        }

        let jpeg = encode_jpeg_to_vec(w, h, &rgba).expect("encode should succeed");
        assert!(!jpeg.is_empty(), "should have produced JPEG bytes");

        let path = std::env::temp_dir().join(format!(
            "lightphotos-encode-vec-test-{}.jpg",
            std::process::id()
        ));
        std::fs::write(&path, jpeg).expect("write encoded JPEG should succeed");
        let decoded = image_decode::decode(&path, u32::MAX).expect("re-decode should succeed");
        assert_eq!((decoded.width, decoded.height), (w, h));
        let px = &decoded.rgba[..4];
        assert!(px[0] > 150, "red channel should be high, got {}", px[0]);
        assert!(
            px[1] < 100 && px[2] < 100,
            "green/blue should be low, got {},{}",
            px[1],
            px[2]
        );
        std::fs::remove_file(path).ok();
    }

    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    #[test]
    fn encode_jpeg_to_vec_rejects_zero_size() {
        assert!(encode_jpeg_to_vec(0, 4, &[]).is_err());
        assert!(encode_jpeg_to_vec(4, 0, &[]).is_err());
    }

    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    #[test]
    fn encode_jpeg_to_vec_rejects_short_buffer() {
        assert!(encode_jpeg_to_vec(4, 4, &[0u8; 16]).is_err());
    }

    /// Write solid red through the platform encoder, decode it back, and check
    /// the size and the approximate color.
    #[test]
    fn encode_then_decode_round_trips() {
        let (w, h) = (8u32, 6u32);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            rgba.extend_from_slice(&[220, 30, 30, 255]);
        }

        let out = std::env::temp_dir().join(format!("iv-encode-test-{}.jpg", std::process::id()));
        encode_jpeg(&out, w, h, &rgba).expect("encode should succeed");
        assert!(out.exists(), "jpeg file should have been written");

        let decoded = image_decode::decode(&out, 16384).expect("re-decode should succeed");
        assert_eq!((decoded.width, decoded.height), (w, h));
        // JPEG is lossy, so only check that the result is mostly red.
        let (r, g, b) = (decoded.rgba[0], decoded.rgba[1], decoded.rgba[2]);
        assert!(r > 150, "red channel should be high, got {r}");
        assert!(g < 100 && b < 100, "green/blue should be low, got {g},{b}");

        std::fs::remove_file(&out).ok();
    }

    fn exif_blocks(jpeg: &[u8]) -> usize {
        jpeg.windows(6).filter(|w| w == b"Exif\0\0").count()
    }

    /// Read back through the app's own EXIF reader, which is ImageIO on macOS
    /// and kamadak-exif elsewhere, not through code in this file.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn an_export_carries_its_capture_date_in_exif() {
        let path = std::env::temp_dir().join(format!(
            "lightphotos-with-exif-test-{}.jpg",
            std::process::id()
        ));
        encode_jpeg(&path, 12, 8, &[90u8; 12 * 8 * 4]).unwrap();
        let plain = std::fs::read(&path).unwrap();

        for stamp in [
            image_decode::CaptureStamp::new("2024:06:01 18:04:05", Some("-07:00")).unwrap(),
            image_decode::CaptureStamp::new("2023:12:31 23:59:59", None).unwrap(),
        ] {
            let tagged = with_exif(&plain, 12, 8, Some(&stamp));
            assert_eq!(
                exif_blocks(&tagged),
                1,
                "the encoder's EXIF is replaced, not doubled"
            );
            std::fs::write(&path, &tagged).unwrap();
            assert_eq!(image_decode::capture_stamp(&path), Some(stamp));
            let img = image_decode::decode(&path, u32::MAX).expect("still a valid JPEG");
            assert_eq!((img.width, img.height), (12, 8));
        }

        std::fs::write(&path, with_exif(&plain, 12, 8, None)).unwrap();
        assert_eq!(image_decode::capture_stamp(&path), None);
        std::fs::remove_file(&path).ok();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn exif_goes_after_jfif_and_bytes_that_are_not_a_jpeg_pass_through() {
        let jfif = [0xFF, 0xE0, 0x00, 0x07, b'J', b'F', b'I', b'F', 0x00];
        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend_from_slice(&jfif);
        jpeg.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x02, 0xFF, 0xD9]);
        let out = with_exif(&jpeg, 1, 1, None);
        assert_eq!(&out[2..11], &jfif, "JFIF stays first");
        assert_eq!(&out[11..13], &[0xFF, 0xE1], "EXIF follows it");
        assert!(out.ends_with(&[0xFF, 0xDB, 0x00, 0x02, 0xFF, 0xD9]));

        assert_eq!(with_exif(b"not a jpeg", 1, 1, None), b"not a jpeg");
    }

    /// The browser's export path: its worker holds only the source bytes, so
    /// the date it writes has to come from them.
    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    #[test]
    fn a_baked_export_carries_the_capture_date_of_its_source_bytes() {
        let stamp = image_decode::CaptureStamp::new("2024:06:01 18:04:05", Some("+09:00")).unwrap();
        let plain = encode_jpeg_to_vec(16, 8, &[120u8; 16 * 8 * 4]).unwrap();
        let tagged = with_exif(&plain, 16, 8, Some(&stamp));
        let bake = |src: Vec<u8>| {
            crate::export::bake_jpeg_from_shared_vec(
                std::sync::Arc::new(src),
                false,
                &Default::default(),
                &[],
                0,
                8,
            )
            .unwrap()
        };

        let out = bake(tagged);
        assert_eq!(image_decode::capture_stamp_from_bytes(&out), Some(stamp));
        assert_eq!(exif_blocks(&out), 1);

        let out = bake(plain);
        assert_eq!(image_decode::capture_stamp_from_bytes(&out), None, "no date is invented");
    }
}
