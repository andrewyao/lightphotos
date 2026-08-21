// SPDX-License-Identifier: GPL-3.0-or-later

//! Perceptual hash (dHash) for duplicate-photo grouping.
//!
//! Unlike `sharpness.rs`'s variance-of-Laplacian (a focus metric), dHash is a
//! similarity fingerprint: images that look alike hash to nearby bit patterns
//! regardless of resolution or minor recompression. It reduces an image to a
//! 9×8 grayscale grid (via `image_ops::resize_luma`, shared with the
//! sharpness metric) and encodes each row's left-to-right brightness trend
//! (8 comparisons/row × 8 rows = 64 bits) — a difference hash, robust to
//! uniform brightness/contrast shifts since it only looks at relative
//! neighbor comparisons, not absolute pixel values.

const HASH_W: usize = 9;
const HASH_H: usize = 8;

/// 64-bit difference hash of RGBA8 pixels (`width`×`height`, row-major).
/// Returns `0` for empty/degenerate input or a short buffer (matches
/// `sharpness::sharpness`'s degenerate-input convention).
pub fn dhash(rgba: &[u8], width: u32, height: u32) -> u64 {
    if width == 0 || height == 0 || rgba.len() < (width as usize * height as usize * 4) {
        return 0;
    }
    let gray = crate::image_ops::resize_luma(rgba, width, height, HASH_W, HASH_H);
    let mut hash: u64 = 0;
    let mut bit = 0u32;
    for row in 0..HASH_H {
        for col in 0..HASH_W - 1 {
            let left = gray[row * HASH_W + col];
            let right = gray[row * HASH_W + col + 1];
            if left < right {
                hash |= 1u64 << bit;
            }
            bit += 1;
        }
    }
    hash
}

/// Hamming distance (number of differing bits) between two hashes.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(v: u8) -> [u8; 4] {
        [v, v, v, 255]
    }

    fn solid(w: u32, h: u32, v: u8) -> Vec<u8> {
        vec![px(v); (w * h) as usize].concat()
    }

    fn horizontal_gradient(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..h {
            for x in 0..w {
                let v = ((x as u32 * 255) / w.max(1)) as u8;
                out.extend_from_slice(&px(v));
            }
        }
        out
    }

    fn reversed_gradient(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..h {
            for x in 0..w {
                let v = (((w - 1 - x) as u32 * 255) / w.max(1)) as u8;
                out.extend_from_slice(&px(v));
            }
        }
        out
    }

    #[test]
    fn hamming_of_equal_hashes_is_zero() {
        assert_eq!(hamming(0xABCD_1234, 0xABCD_1234), 0);
    }

    #[test]
    fn hamming_counts_differing_bits() {
        assert_eq!(hamming(0b0000, 0b1111), 4);
    }

    #[test]
    fn degenerate_input_is_zero_hash() {
        assert_eq!(dhash(&[], 0, 0), 0);
        assert_eq!(dhash(&[0, 0, 0, 255], 10, 10), 0); // buffer too short
    }

    #[test]
    fn identical_images_hash_identically() {
        let (w, h) = (32, 32);
        let img = horizontal_gradient(w, h);
        assert_eq!(hamming(dhash(&img, w, h), dhash(&img, w, h)), 0);
    }

    #[test]
    fn reversed_gradient_is_far_from_original() {
        // Every left<right comparison in the original flips to left>right
        // in the mirrored version, so nearly every bit differs.
        let (w, h) = (32, 32);
        let fwd = horizontal_gradient(w, h);
        let rev = reversed_gradient(w, h);
        let dist = hamming(dhash(&fwd, w, h), dhash(&rev, w, h));
        assert!(
            dist > 48,
            "expected a mirrored gradient to differ in most of the 64 bits, got {dist}"
        );
    }

    #[test]
    fn solid_images_of_any_brightness_hash_the_same() {
        // dHash only encodes relative neighbor trends, so flat images (no
        // gradient at all) collapse to the same all-zero hash regardless of
        // absolute brightness — an intentional/expected limitation, not a bug.
        let (w, h) = (16, 16);
        let black = solid(w, h, 0);
        let white = solid(w, h, 255);
        assert_eq!(dhash(&black, w, h), dhash(&white, w, h));
    }
}
