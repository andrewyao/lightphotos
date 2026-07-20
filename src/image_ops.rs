// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure pixel operations shared by export, thumbnail baking, and the histogram.
//!
//! These have no `App` dependency, so the exporter's worker pool and the
//! thumbnail-upload path can both call them without reaching back into the UI
//! state module. Keeping the crop/tone/rotate math in one place is also what
//! guarantees the exported JPEG and the on-screen edited thumbnail agree.

use crate::develop::{self, Adjustments};
use crate::image_decode::DecodedImage;

/// Un-premultiply a premultiplied-sRGB8 RGBA pixel and convert it to
/// linear-light RGB with the 2.2 gamma that `develop::apply_linear` assumes.
///
/// The decode path produces premultiplied alpha; both the histogram sampler and
/// the bake pipeline consume pixels through this one helper so their input
/// domains can't drift apart.
pub(crate) fn unpremul_to_linear(px: [u8; 4]) -> [f32; 3] {
    let [r, g, b, a] = px;
    let (r, g, b) = if a == 0 {
        (0.0, 0.0, 0.0)
    } else if a == 255 {
        (r as f32, g as f32, b as f32)
    } else {
        let inv = 255.0 / a as f32;
        (
            (r as f32 * inv).min(255.0),
            (g as f32 * inv).min(255.0),
            (b as f32 * inv).min(255.0),
        )
    };
    let srgb_to_linear = |c: f32| (c / 255.0).powf(2.2);
    [srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b)]
}

/// Bake `adj` (crop + tone) and `rot` (90° CW steps) into a fresh, straight
/// (opaque) sRGB8 RGBA buffer. Order: crop in texture space → apply the tone
/// pipeline per pixel → rotate. Returns `(w, h, rgba)`. Used both for JPEG export
/// (full-res) and to render edited grid/filmstrip thumbnails (on the cached raw
/// thumbnail RGBA), so the two always agree.
///
/// The decode is premultiplied sRGB8; the un-premultiply + sRGB→linear (via
/// [`unpremul_to_linear`]) matches the histogram sampler, and
/// `develop::apply_linear` is the same tone pipeline the shader runs, so the
/// result matches what's on screen.
pub(crate) fn bake_edited(img: &DecodedImage, adj: &Adjustments, rot: u8) -> (u32, u32, Vec<u8>) {
    let (w, h) = (img.width, img.height);
    if w == 0 || h == 0 || img.rgba.len() < (w * h * 4) as usize {
        return (0, 0, Vec::new());
    }

    // Crop rectangle → integer pixel bounds in texture space.
    let (cl, ct, cr, cb) = match adj.crop {
        Some(c) => (c.left, c.top, c.right, c.bottom),
        None => (0.0, 0.0, 1.0, 1.0),
    };
    let x0 = ((cl * w as f32).round() as i64).clamp(0, w as i64 - 1) as u32;
    let y0 = ((ct * h as f32).round() as i64).clamp(0, h as i64 - 1) as u32;
    let x1 = ((cr * w as f32).round() as i64).clamp(x0 as i64 + 1, w as i64) as u32;
    let y1 = ((cb * h as f32).round() as i64).clamp(y0 as i64 + 1, h as i64) as u32;
    let (cw, ch) = (x1 - x0, y1 - y0);

    let encode = |v: f32| (v.max(0.0).powf(1.0 / 2.2) * 255.0).round().clamp(0.0, 255.0) as u8;

    // When denoise is active, precompute the whole source image's linear-light
    // buffer once so the 25-tap neighborhood lookup (`denoise_sample`) is a
    // cheap indexed read instead of re-running unpremul_to_linear per tap.
    // Taps read from the full source image (not just the crop), clamped to its
    // bounds — matching the shader's clamp-to-edge sampling against the full
    // uploaded texture — so pixels near the crop edge still see real
    // neighbors instead of the crop boundary. When denoise == 0.0 this is
    // skipped entirely, leaving the original single-conversion-per-pixel path
    // (and its cost) unchanged.
    let full_linear: Option<Vec<[f32; 3]>> = (adj.denoise > 0.0).then(|| {
        (0..(w * h) as usize)
            .map(|i| {
                let si = i * 4;
                unpremul_to_linear([img.rgba[si], img.rgba[si + 1], img.rgba[si + 2], img.rgba[si + 3]])
            })
            .collect()
    });

    // Cropped + tone-applied buffer, still in texture orientation.
    let mut cropped = vec![0u8; (cw * ch * 4) as usize];
    for y in 0..ch {
        for x in 0..cw {
            let lin = match &full_linear {
                Some(buf) => develop::denoise_sample(adj, |dx, dy| {
                    let sx = (x0 as i64 + x as i64 + dx as i64).clamp(0, w as i64 - 1) as u32;
                    let sy = (y0 as i64 + y as i64 + dy as i64).clamp(0, h as i64 - 1) as u32;
                    buf[(sy * w + sx) as usize]
                }),
                None => {
                    let si = (((y0 + y) * w + (x0 + x)) * 4) as usize;
                    unpremul_to_linear([
                        img.rgba[si],
                        img.rgba[si + 1],
                        img.rgba[si + 2],
                        img.rgba[si + 3],
                    ])
                }
            };
            let out = develop::apply_linear(adj, lin);
            let di = ((y * cw + x) * 4) as usize;
            cropped[di] = encode(out[0]);
            cropped[di + 1] = encode(out[1]);
            cropped[di + 2] = encode(out[2]);
            cropped[di + 3] = 255;
        }
    }

    rotate_rgba(&cropped, cw, ch, rot)
}

/// Rotate a tightly-packed RGBA8 buffer by `steps` × 90° clockwise. Returns the
/// (possibly swapped) `(width, height, rgba)`.
pub(crate) fn rotate_rgba(src: &[u8], w: u32, h: u32, steps: u8) -> (u32, u32, Vec<u8>) {
    let steps = steps % 4;
    if steps == 0 {
        return (w, h, src.to_vec());
    }
    let (nw, nh) = if steps == 2 { (w, h) } else { (h, w) };
    let mut dst = vec![0u8; (nw * nh * 4) as usize];
    let px = |x: u32, y: u32| ((y * w + x) * 4) as usize;
    for yo in 0..nh {
        for xo in 0..nw {
            // Source pixel that lands at output (xo, yo).
            let (xs, ys) = match steps {
                1 => (yo, h - 1 - xo),         // 90° CW
                2 => (w - 1 - xo, h - 1 - yo), // 180°
                _ => (w - 1 - yo, xo),         // 270° CW (90° CCW)
            };
            let s = px(xs, ys);
            let d = ((yo * nw + xo) * 4) as usize;
            dst[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    (nw, nh, dst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::develop::Crop;

    fn px(v: u8) -> [u8; 4] {
        [v, v, v, 255]
    }

    #[test]
    fn rotate_90cw_swaps_dims_and_moves_pixels() {
        // Two horizontal pixels A,B (w=2,h=1). 90° CW → a 1×2 column A over B.
        let src = [px(10), px(20)].concat();
        let (w, h, out) = rotate_rgba(&src, 2, 1, 1);
        assert_eq!((w, h), (1, 2));
        assert_eq!(&out[0..4], &px(10)); // top
        assert_eq!(&out[4..8], &px(20)); // bottom
    }

    #[test]
    fn rotate_360_is_identity() {
        let src = [px(1), px(2), px(3), px(4)].concat(); // 2×2
        let (w, h, out) = rotate_rgba(&src, 2, 2, 4);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, src);
    }

    #[test]
    fn identity_bake_preserves_opaque_pixels() {
        // No crop, no rotation, identity adjustments → pixels survive the
        // premultiply/sRGB↔linear round-trip unchanged (alpha becomes opaque).
        let src = [px(0), px(64), px(128), px(255)].concat(); // 2×2
        let img = DecodedImage { width: 2, height: 2, rgba: src.clone() };
        let (w, h, out) = bake_edited(&img, &Adjustments::default(), 0);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, src);
    }

    #[test]
    fn bake_crop_slices_to_the_crop_rect() {
        // 4×1 image; crop the right half → 2×1 keeping the last two pixels.
        let src = [px(1), px(2), px(3), px(4)].concat();
        let img = DecodedImage { width: 4, height: 1, rgba: src };
        let mut adj = Adjustments::default();
        adj.crop = Some(Crop { left: 0.5, top: 0.0, right: 1.0, bottom: 1.0 });
        let (w, h, out) = bake_edited(&img, &adj, 0);
        assert_eq!((w, h), (2, 1));
        assert_eq!(&out[0..4], &px(3));
        assert_eq!(&out[4..8], &px(4));
    }

    #[test]
    fn bake_denoise_zero_matches_identity_bake() {
        // Same fixture/assertion as identity_bake_preserves_opaque_pixels,
        // just with an explicit denoise: 0.0 to confirm the new field doesn't
        // change the fast path at all.
        let src = [px(0), px(64), px(128), px(255)].concat();
        let img = DecodedImage { width: 2, height: 2, rgba: src.clone() };
        let adj = Adjustments { denoise: 0.0, ..Default::default() };
        let (w, h, out) = bake_edited(&img, &adj, 0);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, src);
    }

    #[test]
    fn bake_denoise_changes_output() {
        // A noisy 3x3 image: a bright outlier pixel surrounded by dark ones.
        // Denoising should visibly pull the center pixel away from raw white.
        let mut src = vec![0u8; 3 * 3 * 4];
        for i in 0..9 {
            let v = if i == 4 { 255 } else { 0 };
            src[i * 4..i * 4 + 4].copy_from_slice(&px(v));
        }
        let img = DecodedImage { width: 3, height: 3, rgba: src };
        let (_, _, out0) = bake_edited(&img, &Adjustments::default(), 0);
        let denoised = Adjustments { denoise: 100.0, ..Default::default() };
        let (_, _, out100) = bake_edited(&img, &denoised, 0);
        let center = 4 * 4; // pixel index 4, byte offset
        assert_eq!(out0[center], 255);
        assert!(out100[center] < 255, "expected denoise to darken the outlier center pixel");
    }

    #[test]
    fn bake_denoise_clamps_at_edges() {
        // Small 3x3 image; denoise must not panic or read out of bounds when
        // taps for a corner pixel fall outside the image.
        let src = [px(10), px(20), px(30), px(40), px(50), px(60), px(70), px(80), px(90)].concat();
        let img = DecodedImage { width: 3, height: 3, rgba: src };
        let adj = Adjustments { denoise: 50.0, ..Default::default() };
        let (w, h, out) = bake_edited(&img, &adj, 0);
        assert_eq!((w, h), (3, 3));
        assert_eq!(out.len(), 3 * 3 * 4);
    }

    #[test]
    fn unpremul_opaque_is_linear_of_srgb() {
        // Opaque pixel: un-premultiply is a no-op, output is sRGB→linear.
        let out = unpremul_to_linear([255, 0, 128, 255]);
        assert!((out[0] - 1.0).abs() < 1e-6);
        assert!((out[1] - 0.0).abs() < 1e-6);
        assert!((out[2] - (128.0f32 / 255.0).powf(2.2)).abs() < 1e-6);
    }

    #[test]
    fn unpremul_zero_alpha_is_black() {
        assert_eq!(unpremul_to_linear([200, 100, 50, 0]), [0.0, 0.0, 0.0]);
    }
}
