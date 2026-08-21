// SPDX-License-Identifier: GPL-3.0-or-later

//! Focus/blur metric via the variance of the Laplacian.
//!
//! A sharp image has strong high-frequency detail, so its Laplacian (a
//! second-derivative edge operator) has a wide spread of responses — high
//! variance. A blurred image smears edges, shrinking that spread. The score is
//! relative (bigger = sharper), used to pick the sharpest frame within a burst.
//!
//! Pixels are first reduced to a grayscale image whose long side is at most
//! [`TARGET_LONG`], so scores are comparable across images of differing
//! resolution within a group.

/// Long-side cap for the analysis image. Downscaling normalizes the metric
/// across resolutions and cheapens the convolution.
const TARGET_LONG: u32 = 1024;

/// Relative sharpness of RGBA8 pixels (`width`×`height`, row-major). Higher is
/// sharper. Returns `0.0` for empty/degenerate input or a short buffer.
pub fn sharpness(rgba: &[u8], width: u32, height: u32) -> f64 {
    if width == 0 || height == 0 || rgba.len() < (width as usize * height as usize * 4) {
        return 0.0;
    }
    let block = ((width.max(height) as f32 / TARGET_LONG as f32).ceil() as usize).max(1);
    let ow = (width as usize).div_ceil(block);
    let oh = (height as usize).div_ceil(block);
    let gray = crate::image_ops::resize_luma(rgba, width, height, ow, oh);
    variance_of_laplacian(&gray, ow, oh)
}

/// Variance of the 3×3 Laplacian response over a grayscale image. `0.0` when the
/// image is smaller than the 3×3 kernel.
fn variance_of_laplacian(gray: &[f32], w: usize, h: usize) -> f64 {
    if w < 3 || h < 3 {
        return 0.0;
    }
    let mut responses = Vec::with_capacity((w - 2) * (h - 2));
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let c = gray[y * w + x];
            // 4-neighbour Laplacian: ∑neighbours − 4·center.
            let lap = gray[(y - 1) * w + x]
                + gray[(y + 1) * w + x]
                + gray[y * w + x - 1]
                + gray[y * w + x + 1]
                - 4.0 * c;
            responses.push(lap as f64);
        }
    }
    let n = responses.len() as f64;
    if n == 0.0 {
        return 0.0;
    }
    let mean = responses.iter().sum::<f64>() / n;
    responses
        .iter()
        .map(|v| (v - mean) * (v - mean))
        .sum::<f64>()
        / n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_image_has_zero_variance() {
        let gray = vec![128.0f32; 6 * 6];
        assert_eq!(variance_of_laplacian(&gray, 6, 6), 0.0);
    }

    #[test]
    fn linear_gradient_is_near_zero() {
        // Laplacian of a linear ramp is ~0 everywhere → tiny variance.
        let (w, h) = (8usize, 8usize);
        let gray: Vec<f32> = (0..w * h).map(|i| (i % w) as f32 * 10.0).collect();
        assert!(variance_of_laplacian(&gray, w, h) < 1.0);
    }

    #[test]
    fn checkerboard_beats_gradient() {
        let (w, h) = (8usize, 8usize);
        let checker: Vec<f32> = (0..w * h)
            .map(|i| {
                if ((i % w) + (i / w)) % 2 == 0 {
                    0.0
                } else {
                    255.0
                }
            })
            .collect();
        let gradient: Vec<f32> = (0..w * h).map(|i| (i % w) as f32 * 10.0).collect();
        assert!(
            variance_of_laplacian(&checker, w, h) > variance_of_laplacian(&gradient, w, h),
            "sharp checkerboard should out-score a smooth gradient"
        );
    }

    #[test]
    fn sharp_rgba_beats_blurred() {
        // 32x32 checkerboard vs its 3x3 box-blurred version.
        let (w, h) = (32u32, 32u32);
        let mut sharp = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let v = if (x / 2 + y / 2) % 2 == 0 { 0 } else { 255 };
                let i = ((y * w + x) * 4) as usize;
                sharp[i] = v;
                sharp[i + 1] = v;
                sharp[i + 2] = v;
                sharp[i + 3] = 255;
            }
        }
        // Box-blur luma into a new RGBA buffer.
        let mut blur = sharp.clone();
        for y in 1..h - 1 {
            for x in 1..w - 1 {
                let mut acc = 0u32;
                for dy in -1i32..=1 {
                    for dx in -1i32..=1 {
                        let xx = (x as i32 + dx) as u32;
                        let yy = (y as i32 + dy) as u32;
                        acc += sharp[((yy * w + xx) * 4) as usize] as u32;
                    }
                }
                let avg = (acc / 9) as u8;
                let i = ((y * w + x) * 4) as usize;
                blur[i] = avg;
                blur[i + 1] = avg;
                blur[i + 2] = avg;
            }
        }
        assert!(
            sharpness(&sharp, w, h) > sharpness(&blur, w, h),
            "the crisp checkerboard should score higher than its blurred copy"
        );
    }

    #[test]
    fn degenerate_input_is_zero() {
        assert_eq!(sharpness(&[], 0, 0), 0.0);
        assert_eq!(sharpness(&[0, 0, 0, 255], 10, 10), 0.0); // buffer too short
    }
}
