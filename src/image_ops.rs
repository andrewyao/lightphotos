// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure pixel operations shared by export, thumbnail baking, and the histogram.
//!
//! These have no `App` dependency, so the exporter's worker pool and the
//! thumbnail-upload path can both call them without reaching back into the UI
//! state module. Keeping the crop/tone/rotate math in one place is also what
//! guarantees the exported JPEG and the on-screen edited thumbnail agree.

use crate::develop::{self, Adjustments, Crop, TouchUp};
use crate::image_decode::DecodedImage;

/// A crop rectangle (normalized 0..1, or `None` for the full frame) → integer
/// pixel bounds `(x0, y0, x1, y1)` in texture space, clamped so the region is
/// always at least 1×1. Used by `bake_edited` (crop + tone + rotate).
fn crop_bounds(crop: Option<Crop>, w: u32, h: u32) -> (u32, u32, u32, u32) {
    let (cl, ct, cr, cb) = match crop {
        Some(c) => (c.left, c.top, c.right, c.bottom),
        None => (0.0, 0.0, 1.0, 1.0),
    };
    let x0 = ((cl * w as f32).round() as i64).clamp(0, w as i64 - 1) as u32;
    let y0 = ((ct * h as f32).round() as i64).clamp(0, h as i64 - 1) as u32;
    let x1 = ((cr * w as f32).round() as i64).clamp(x0 as i64 + 1, w as i64) as u32;
    let y1 = ((cb * h as f32).round() as i64).clamp(y0 as i64 + 1, h as i64) as u32;
    (x0, y0, x1, y1)
}

/// Reduce RGBA8 to grayscale (Rec.601 luma, 0..255), box-averaged down to an
/// exact `out_w`×`out_h` grid (`out_w`/`out_h` ≤ input dims). Shared by the
/// sharpness metric (long-side-capped downscale) and dHash (fixed 9×8 grid for
/// gradient hashing) so both agree on how pixels become grayscale samples.
pub(crate) fn resize_luma(rgba: &[u8], width: u32, height: u32, out_w: usize, out_h: usize) -> Vec<f32> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0f32; out_w * out_h];
    for oy in 0..out_h {
        let y0 = oy * h / out_h;
        let y1 = ((oy + 1) * h / out_h).max(y0 + 1).min(h);
        for ox in 0..out_w {
            let x0 = ox * w / out_w;
            let x1 = ((ox + 1) * w / out_w).max(x0 + 1).min(w);
            let mut sum = 0f32;
            let mut n = 0u32;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * w + x) * 4;
                    let (r, g, b) = (rgba[i] as f32, rgba[i + 1] as f32, rgba[i + 2] as f32);
                    sum += 0.299 * r + 0.587 * g + 0.114 * b;
                    n += 1;
                }
            }
            out[oy * out_w + ox] = if n > 0 { sum / n as f32 } else { 0.0 };
        }
    }
    out
}

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
pub(crate) fn bake_edited(
    img: &DecodedImage,
    adj: &Adjustments,
    touchups: &[TouchUp],
    rot: u8,
) -> (u32, u32, Vec<u8>) {
    let (w, h) = (img.width, img.height);
    if w == 0 || h == 0 || img.rgba.len() < (w * h * 4) as usize {
        return (0, 0, Vec::new());
    }

    // Crop rectangle → integer pixel bounds in texture space.
    let (x0, y0, x1, y1) = crop_bounds(adj.crop, w, h);
    let (cw, ch) = (x1 - x0, y1 - y0);

    let encode = |v: f32| {
        (v.max(0.0).powf(1.0 / 2.2) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8
    };

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
                unpremul_to_linear([
                    img.rgba[si],
                    img.rgba[si + 1],
                    img.rgba[si + 2],
                    img.rgba[si + 3],
                ])
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
            let retouched = apply_touchups(img, touchups, x0 + x, y0 + y, lin);
            let out = develop::apply_linear(adj, retouched);
            let di = ((y * cw + x) * 4) as usize;
            cropped[di] = encode(out[0]);
            cropped[di + 1] = encode(out[1]);
            cropped[di + 2] = encode(out[2]);
            cropped[di + 3] = 255;
        }
    }

    rotate_rgba(&cropped, cw, ch, rot)
}

fn sample_linear(img: &DecodedImage, u: f32, v: f32) -> [f32; 3] {
    let x = (u.clamp(0.0, 1.0) * (img.width.saturating_sub(1)) as f32).round() as u32;
    let y = (v.clamp(0.0, 1.0) * (img.height.saturating_sub(1)) as f32).round() as u32;
    let i = ((y * img.width + x) * 4) as usize;
    unpremul_to_linear([
        img.rgba[i],
        img.rgba[i + 1],
        img.rgba[i + 2],
        img.rgba[i + 3],
    ])
}

fn apply_touchups(
    img: &DecodedImage,
    touchups: &[TouchUp],
    x: u32,
    y: u32,
    base: [f32; 3],
) -> [f32; 3] {
    if touchups.is_empty() {
        return base;
    }
    let u = x as f32 / img.width.max(1) as f32;
    let v = y as f32 / img.height.max(1) as f32;
    let mut out = base;
    for t in touchups {
        let dx = (u - t.center[0]) * img.width as f32;
        let dy = (v - t.center[1]) * img.height as f32;
        let distance = (dx * dx + dy * dy).sqrt();
        let radius = (t.radius * img.width.min(img.height) as f32).max(1.0);
        if distance >= radius {
            continue;
        }
        let feather = (radius * t.feather.clamp(0.02, 1.0)).max(1.0);
        let mask = ((radius - distance) / feather).clamp(0.0, 1.0);
        let mask = mask * mask * (3.0 - 2.0 * mask);
        let source_u = t.source[0] + (u - t.center[0]);
        let source_v = t.source[1] + (v - t.center[1]);
        let mut src = sample_linear(img, source_u, source_v);
        for c in 0..3 {
            src[c] = (src[c] + t.delta[c]).clamp(0.0, 1.0);
        }
        for c in 0..3 {
            out[c] = out[c] * (1.0 - mask) + src[c] * mask;
        }
    }
    out
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

/// Bilinearly resample a tightly-packed single-channel buffer to `dw × dh`.
///
/// Written for Vision's segmentation masks, which come back at whatever
/// resolution the model chose (typically much smaller than the photo) and have
/// to be stretched to display size before they can be overlaid. Nearest-
/// neighbour would turn the model's soft matte edge into visible stair-steps,
/// which is exactly the part of the mask worth looking at.
///
/// Uses pixel-*center* mapping (the `+ 0.5 … - 0.5` shuffle) rather than naive
/// `x * sw / dw`, so the resampled image stays centered instead of drifting
/// half a source pixel toward the origin. Edge samples clamp rather than wrap.
pub(crate) fn resample_bilinear_u8(
    src: &[u8],
    sw: u32,
    sh: u32,
    dw: u32,
    dh: u32,
) -> Vec<u8> {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || src.len() < (sw * sh) as usize {
        return Vec::new();
    }
    if (sw, sh) == (dw, dh) {
        return src[..(sw * sh) as usize].to_vec();
    }

    let at = |x: u32, y: u32| src[(y * sw + x) as usize] as f32;
    let (x_scale, y_scale) = (sw as f32 / dw as f32, sh as f32 / dh as f32);
    let mut out = Vec::with_capacity((dw * dh) as usize);

    for yo in 0..dh {
        let fy = ((yo as f32 + 0.5) * y_scale - 0.5).clamp(0.0, (sh - 1) as f32);
        let y0 = fy.floor() as u32;
        let y1 = (y0 + 1).min(sh - 1);
        let ty = fy - y0 as f32;

        for xo in 0..dw {
            let fx = ((xo as f32 + 0.5) * x_scale - 0.5).clamp(0.0, (sw - 1) as f32);
            let x0 = fx.floor() as u32;
            let x1 = (x0 + 1).min(sw - 1);
            let tx = fx - x0 as f32;

            let top = at(x0, y0) + (at(x1, y0) - at(x0, y0)) * tx;
            let bottom = at(x0, y1) + (at(x1, y1) - at(x0, y1)) * tx;
            out.push((top + (bottom - top) * ty + 0.5) as u8);
        }
    }
    out
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
        let img = DecodedImage {
            width: 2,
            height: 2,
            rgba: src.clone(),
        };
        let (w, h, out) = bake_edited(&img, &Adjustments::default(), &[], 0);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, src);
    }

    #[test]
    fn bake_crop_slices_to_the_crop_rect() {
        // 4×1 image; crop the right half → 2×1 keeping the last two pixels.
        let src = [px(1), px(2), px(3), px(4)].concat();
        let img = DecodedImage {
            width: 4,
            height: 1,
            rgba: src,
        };
        let mut adj = Adjustments::default();
        adj.crop = Some(Crop {
            left: 0.5,
            top: 0.0,
            right: 1.0,
            bottom: 1.0,
        });
        let (w, h, out) = bake_edited(&img, &adj, &[], 0);
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
        let img = DecodedImage {
            width: 2,
            height: 2,
            rgba: src.clone(),
        };
        let adj = Adjustments {
            denoise: 0.0,
            ..Default::default()
        };
        let (w, h, out) = bake_edited(&img, &adj, &[], 0);
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
        let img = DecodedImage {
            width: 3,
            height: 3,
            rgba: src,
        };
        let (_, _, out0) = bake_edited(&img, &Adjustments::default(), &[], 0);
        let denoised = Adjustments {
            denoise: 100.0,
            ..Default::default()
        };
        let (_, _, out100) = bake_edited(&img, &denoised, &[], 0);
        let center = 4 * 4; // pixel index 4, byte offset
        assert_eq!(out0[center], 255);
        assert!(
            out100[center] < 255,
            "expected denoise to darken the outlier center pixel"
        );
    }

    #[test]
    fn bake_touchup_replaces_a_soft_spot_from_source_region() {
        let mut src = vec![0u8; 5 * 1 * 4];
        for x in 0..5 {
            let value = if x == 2 { 255 } else { 0 };
            src[x * 4..x * 4 + 4].copy_from_slice(&px(value));
        }
        let img = DecodedImage {
            width: 5,
            height: 1,
            rgba: src,
        };
        let touchup = TouchUp {
            center: [0.4, 0.0],
            radius: 0.4,
            source: [0.0, 0.0],
            feather: 0.5,
            delta: [0.0; 3],
        };
        let (_, _, out) = bake_edited(&img, &Adjustments::default(), &[touchup], 0);
        assert!(out[2 * 4] < 255, "touch-up should reduce the bright spot");
    }

    #[test]
    fn bake_denoise_clamps_at_edges() {
        // Small 3x3 image; denoise must not panic or read out of bounds when
        // taps for a corner pixel fall outside the image.
        let src = [
            px(10),
            px(20),
            px(30),
            px(40),
            px(50),
            px(60),
            px(70),
            px(80),
            px(90),
        ]
        .concat();
        let img = DecodedImage {
            width: 3,
            height: 3,
            rgba: src,
        };
        let adj = Adjustments {
            denoise: 50.0,
            ..Default::default()
        };
        let (w, h, out) = bake_edited(&img, &adj, &[], 0);
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

    #[test]
    fn resize_luma_identity_is_per_pixel_luma() {
        // out dims == input dims: no averaging, exact Rec.601 luma per pixel.
        let src = [px(0), px(64), px(128), px(255)].concat(); // 2x2
        let out = resize_luma(&src, 2, 2, 2, 2);
        assert_eq!(out, vec![0.0, 64.0, 128.0, 255.0]);
    }

    #[test]
    fn resize_luma_downscale_averages_blocks() {
        // 4x4 image, four uniform 2x2 quadrants: 0, 100, 200, 300(clamped later).
        let mut src = vec![0u8; 4 * 4 * 4];
        for y in 0..4u32 {
            for x in 0..4u32 {
                let v = if x < 2 && y < 2 {
                    0u8
                } else if x >= 2 && y < 2 {
                    100u8
                } else if x < 2 && y >= 2 {
                    200u8
                } else {
                    255u8
                };
                let i = ((y * 4 + x) * 4) as usize;
                src[i..i + 4].copy_from_slice(&px(v));
            }
        }
        let out = resize_luma(&src, 4, 4, 2, 2);
        assert_eq!(out, vec![0.0, 100.0, 200.0, 255.0]);
    }

    #[test]
    fn resampling_to_the_same_size_is_the_identity() {
        let src = vec![0u8, 40, 90, 255];
        assert_eq!(resample_bilinear_u8(&src, 2, 2, 2, 2), src);
    }

    #[test]
    fn resampling_a_ramp_interpolates_between_the_two_ends() {
        // 2 source pixels stretched to 4. Pixel-center mapping puts the two
        // outer destination samples outside the source centers, so they clamp
        // to the endpoints, and the two inner ones land a quarter and three
        // quarters of the way along.
        let out = resample_bilinear_u8(&[0, 255], 2, 1, 4, 1);
        assert_eq!(out, vec![0, 64, 191, 255]);
    }

    #[test]
    fn resampling_stays_centered_rather_than_drifting_to_the_origin() {
        // A symmetric source must resample to a symmetric result — the check
        // that catches a naive `x * sw / dw` mapping, which shifts everything
        // half a source pixel toward the origin.
        let out = resample_bilinear_u8(&[0, 255, 255, 0], 4, 1, 8, 1);
        let reversed: Vec<u8> = out.iter().rev().copied().collect();
        assert_eq!(out, reversed, "resampled {out:?} is not symmetric");
    }

    #[test]
    fn resampling_a_single_pixel_fills_the_whole_output() {
        assert_eq!(resample_bilinear_u8(&[200], 1, 1, 3, 2), vec![200; 6]);
    }

    #[test]
    fn resampling_a_2x2_block_gives_a_smooth_bilinear_field() {
        // Corners keep their values; the middle of the upscaled field averages
        // all four, both of which fail under nearest-neighbour.
        let out = resample_bilinear_u8(&[0, 100, 200, 255], 2, 2, 4, 4);
        assert_eq!(out.len(), 16);
        assert_eq!(out[0], 0, "top-left corner");
        assert_eq!(out[3], 100, "top-right corner");
        assert_eq!(out[12], 200, "bottom-left corner");
        assert_eq!(out[15], 255, "bottom-right corner");
        let center = (out[5] as u16 + out[6] as u16 + out[9] as u16 + out[10] as u16) / 4;
        assert!(
            (center as i32 - 139).abs() <= 1,
            "center of the field should sit near the mean of the four corners, got {center}"
        );
    }

    #[test]
    fn degenerate_resample_requests_produce_nothing() {
        assert!(resample_bilinear_u8(&[1, 2, 3, 4], 2, 2, 0, 4).is_empty());
        assert!(resample_bilinear_u8(&[1, 2, 3, 4], 0, 2, 4, 4).is_empty());
        // Source buffer smaller than its declared dimensions: refuse rather
        // than index out of bounds.
        assert!(resample_bilinear_u8(&[1, 2], 4, 4, 8, 8).is_empty());
    }
}
