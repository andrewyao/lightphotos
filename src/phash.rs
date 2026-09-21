// SPDX-License-Identifier: GPL-3.0-or-later

//! Difference hash (dHash) for finding duplicate photos. The image shrinks to a
//! 9x8 gray grid, and each bit records whether a pixel is darker than its right
//! neighbor. Similar images get hashes a few bits apart, regardless of size or
//! uniform brightness changes.

const HASH_W: usize = 9;
const HASH_H: usize = 8;

/// 64-bit difference hash of row-major RGBA8 pixels. `0` for empty input or a
/// short buffer.
#[hotpath::measure]
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

/// Number of differing bits.
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
        assert_eq!(dhash(&[0, 0, 0, 255], 10, 10), 0);
    }

    #[test]
    fn identical_images_hash_identically() {
        let (w, h) = (32, 32);
        let img = horizontal_gradient(w, h);
        assert_eq!(hamming(dhash(&img, w, h), dhash(&img, w, h)), 0);
    }

    #[test]
    fn reversed_gradient_is_far_from_original() {
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
        // Known limitation: dHash only sees neighbor differences, so every flat
        // image hashes to 0.
        let (w, h) = (16, 16);
        let black = solid(w, h, 0);
        let white = solid(w, h, 255);
        assert_eq!(dhash(&black, w, h), dhash(&white, w, h));
    }
}
