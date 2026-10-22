// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fits a decoded JPEG, PNG or TIFF within a size and turns it upright, for
//! every decode of one off macOS: the photo itself, its EXIF thumbnail, and
//! the camera JPEG inside a RAF or CR3.
//!
//! A large shrink first averages whole blocks down to two to four times the
//! target, in the image's own layout and depth, then Lanczos3 finishes.
//! Lanczos's kernel widens with the ratio, so going straight from 24 MP to a
//! 512 px thumbnail read every source pixel dozens of times. The RGBA8 copy
//! and the EXIF rotation then happen at the small size.
//!
//! zune-jpeg, the `image` crate's JPEG decoder, has no reduced-size IDCT, so
//! the frame is still decoded once at full size.
//!
//! On wasm32 a decode first takes a `decode_budget` grant sized from the
//! header for exactly these steps ([`Plan::peak_bytes`]).

use image::metadata::Orientation;
use image::{DynamicImage, ImageBuffer, ImageDecoder, RgbaImage};

use crate::decode::image_decode::fit_within;

/// Most a block shrink averages on a side. Past it Lanczos3 starts from a
/// larger image; it also keeps an 8-bit block sum inside a `u32`.
const MAX_FACTOR: u32 = 256;

/// How [`fit_and_orient`] runs for one image, worked out from its size alone,
/// so the budget grant and the pipeline cannot disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    src: (u32, u32),
    /// Block side for the shrink, 1 for none.
    factor: u32,
    /// The size after the shrink. Partial blocks at the right and bottom
    /// edges are cropped, under half an output pixel, so the image is scaled
    /// evenly rather than stretched toward its far edges.
    shrunk: (u32, u32),
    out: (u32, u32),
}

impl Plan {
    pub(crate) fn new(src: (u32, u32), max_dim: u32) -> Plan {
        let out = fit_within(src.0, src.1, max_dim);
        let factor = if out == src {
            1
        } else {
            let per_side = |s: u32, o: u32| s / o.saturating_mul(2).max(1);
            per_side(src.0, out.0)
                .min(per_side(src.1, out.1))
                .min(MAX_FACTOR)
        };
        let factor = if factor >= 2 { factor } else { 1 };
        Plan {
            src,
            factor,
            shrunk: (src.0 / factor, src.1 / factor),
            out,
        }
    }

    /// Most bytes held at once: the largest of the steps' live buffers, for
    /// an image decoded at `native_bpp` bytes a pixel. `turns` when the
    /// orientation copies the output.
    #[cfg_attr(
        not(target_arch = "wasm32"),
        allow(dead_code, reason = "only wasm32 has a decode budget")
    )]
    pub(crate) fn peak_bytes(&self, native_bpp: u64, turns: bool) -> u64 {
        let px = |(w, h): (u32, u32)| u64::from(w) * u64::from(h);
        let native = px(self.src).saturating_mul(native_bpp);
        let shrunk_native = px(self.shrunk).saturating_mul(native_bpp);
        let rgba = px(self.shrunk).saturating_mul(4);
        let out = px(self.out).saturating_mul(4);
        // Decode, then the block shrink beside it.
        let shrink = if self.factor > 1 {
            native.saturating_add(shrunk_native)
        } else {
            native
        };
        // The shrunk (or whole) image and its RGBA8 copy.
        let to_rgba = shrunk_native.saturating_add(rgba);
        // Lanczos3's first pass is an `Rgba32F` image as wide as its input
        // and as tall as the output, beside the input and the output.
        let lanczos = if self.out == self.src {
            0
        } else {
            let pass = px((self.shrunk.0, self.out.1)).saturating_mul(16);
            rgba.saturating_add(pass).saturating_add(out)
        };
        let turn = if turns { out.saturating_mul(2) } else { 0 };
        shrink.max(to_rgba).max(lanczos).max(turn)
    }
}

/// Decodes `reader`'s image fit within `max_dim`, turned upright by
/// `orientation`, or by the image's own EXIF orientation when `None`.
pub fn decode_fit<R: std::io::BufRead + std::io::Seek>(
    reader: image::ImageReader<R>,
    max_dim: u32,
    orientation: Option<Orientation>,
) -> Result<RgbaImage, String> {
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation =
        orientation.unwrap_or_else(|| decoder.orientation().unwrap_or(Orientation::NoTransforms));
    let plan = Plan::new(decoder.dimensions(), max_dim);
    // Six browser decode threads share one heap that never shrinks, so a
    // folder of large JPEGs must wait its turn as RAWs do. Held until this
    // returns; sized from the header before any pixel is allocated.
    #[cfg(target_arch = "wasm32")]
    let _grant = grant(&plan, decoder.color_type(), orientation);
    let img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    fit_and_orient(img, &plan, orientation)
}

/// [`decode_fit`] for an image another decoder has already decoded, such
/// as rawler's embedded camera JPEG. Its grant covers the steps here, not
/// that decode.
pub fn fit_decoded(
    img: DynamicImage,
    max_dim: u32,
    orientation: Orientation,
) -> Result<RgbaImage, String> {
    let plan = Plan::new((img.width(), img.height()), max_dim);
    #[cfg(target_arch = "wasm32")]
    let _grant = grant(&plan, img.color(), orientation);
    fit_and_orient(img, &plan, orientation)
}

#[cfg(target_arch = "wasm32")]
fn grant(
    plan: &Plan,
    color: image::ColorType,
    orientation: Orientation,
) -> crate::decode::decode_budget::Grant {
    let turns = orientation != Orientation::NoTransforms;
    let bytes = plan.peak_bytes(u64::from(color.bytes_per_pixel()), turns);
    crate::decode::decode_budget::acquire(usize::try_from(bytes).unwrap_or(usize::MAX))
}

fn fit_and_orient(
    img: DynamicImage,
    plan: &Plan,
    orientation: Orientation,
) -> Result<RgbaImage, String> {
    if plan.src.0 == 0 || plan.src.1 == 0 {
        return Err("decoded image has zero dimension".into());
    }
    let rgba = if plan.out == plan.src {
        img.into_rgba8()
    } else {
        let img = if plan.factor > 1 {
            box_shrink(img, plan.factor)
        } else {
            img
        };
        // `fit_within` treats both sides alike, so fitting before the turn
        // gives the turned fit's size. Lanczos3 runs faster on RGBA8.
        image::imageops::resize(
            &img.into_rgba8(),
            plan.out.0,
            plan.out.1,
            image::imageops::FilterType::Lanczos3,
        )
    };
    let mut img = DynamicImage::ImageRgba8(rgba);
    img.apply_orientation(orientation);
    Ok(img.into_rgba8())
}

/// `img` shrunk `factor` times on each side by averaging whole
/// `factor` x `factor` blocks, in its own layout and depth. Colors are
/// weighted by alpha, so a transparent pixel's color does not fringe an
/// edge. Should a buffer not match its size, the unshrunk image is
/// returned, which is only slower to resize.
fn box_shrink(img: DynamicImage, factor: u32) -> DynamicImage {
    use DynamicImage as D;
    match img {
        D::ImageLuma8(b) => {
            shrink_buffer(&b, factor, false).map_or(D::ImageLuma8(b), D::ImageLuma8)
        }
        D::ImageLumaA8(b) => {
            shrink_buffer(&b, factor, true).map_or(D::ImageLumaA8(b), D::ImageLumaA8)
        }
        D::ImageRgb8(b) => shrink_buffer(&b, factor, false).map_or(D::ImageRgb8(b), D::ImageRgb8),
        D::ImageRgba8(b) => shrink_buffer(&b, factor, true).map_or(D::ImageRgba8(b), D::ImageRgba8),
        D::ImageLuma16(b) => {
            shrink_buffer(&b, factor, false).map_or(D::ImageLuma16(b), D::ImageLuma16)
        }
        D::ImageLumaA16(b) => {
            shrink_buffer(&b, factor, true).map_or(D::ImageLumaA16(b), D::ImageLumaA16)
        }
        D::ImageRgb16(b) => {
            shrink_buffer(&b, factor, false).map_or(D::ImageRgb16(b), D::ImageRgb16)
        }
        D::ImageRgba16(b) => {
            shrink_buffer(&b, factor, true).map_or(D::ImageRgba16(b), D::ImageRgba16)
        }
        D::ImageRgb32F(b) => {
            shrink_buffer(&b, factor, false).map_or(D::ImageRgb32F(b), D::ImageRgb32F)
        }
        D::ImageRgba32F(b) => {
            shrink_buffer(&b, factor, true).map_or(D::ImageRgba32F(b), D::ImageRgba32F)
        }
        // A layout added to `image` later: Lanczos3 alone still fits it.
        other => other,
    }
}

fn shrink_buffer<P>(
    src: &ImageBuffer<P, Vec<P::Subpixel>>,
    factor: u32,
    alpha: bool,
) -> Option<ImageBuffer<P, Vec<P::Subpixel>>>
where
    P: image::Pixel,
    P::Subpixel: Sample,
{
    let (w, h) = src.dimensions();
    let k = factor.max(1);
    let (dw, dh) = (w / k, h / k);
    let channels = usize::from(P::CHANNEL_COUNT);
    let data = src.as_raw();
    let dims = (w as usize, h as usize, channels, k as usize);
    let out = if alpha {
        shrink_weighted(data, dims)
    } else {
        Sample::shrink_opaque(data, dims)
    }?;
    ImageBuffer::from_raw(dw, dh, out)
}

/// A channel type the block shrink averages.
trait Sample: Copy {
    fn to_f64(self) -> f64;
    fn from_f64(v: f64) -> Self;

    /// Averages whole `k` x `k` blocks of a `w` x `h` buffer of `channels`
    /// channels a pixel, all of them color.
    fn shrink_opaque(data: &[Self], dims: (usize, usize, usize, usize)) -> Option<Vec<Self>> {
        shrink_rows(
            data,
            dims,
            0.0f64,
            |sums, px| {
                for (s, &v) in sums.iter_mut().zip(px) {
                    *s += v.to_f64();
                }
            },
            |sums, out, n| {
                for (o, &s) in out.iter_mut().zip(sums) {
                    *o = Self::from_f64(s / n as f64);
                }
            },
        )
    }
}

impl Sample for u8 {
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    fn from_f64(v: f64) -> Self {
        v.round().clamp(0.0, 255.0) as u8
    }
    /// The JPEG path: `u32` sums, which `MAX_FACTOR` keeps from
    /// overflowing, over whole pixels of a channel count known at compile
    /// time.
    fn shrink_opaque(data: &[u8], dims: (usize, usize, usize, usize)) -> Option<Vec<u8>> {
        match dims.2 {
            1 => shrink_u8::<1>(data, dims),
            3 => shrink_u8::<3>(data, dims),
            4 => shrink_u8::<4>(data, dims),
            _ => shrink_rows(
                data,
                dims,
                0.0f64,
                |sums, px| {
                    for (s, &v) in sums.iter_mut().zip(px) {
                        *s += f64::from(v);
                    }
                },
                |sums, out, n| {
                    for (o, &s) in out.iter_mut().zip(sums) {
                        *o = u8::from_f64(s / n as f64);
                    }
                },
            ),
        }
    }
}

/// [`Sample::shrink_opaque`] for 8-bit pixels of `C` channels. The same
/// checks as [`shrink_rows`] come first, so every row read lies inside
/// `data`.
fn shrink_u8<const C: usize>(
    data: &[u8],
    (w, h, _, k): (usize, usize, usize, usize),
) -> Option<Vec<u8>> {
    let (dw, dh) = (w.checked_div(k)?, h.checked_div(k)?);
    if dw == 0 || dh == 0 || data.len() != w.checked_mul(h)?.checked_mul(C)? {
        return None;
    }
    let n = u32::try_from(k * k).ok()?;
    let mut out = vec![0u8; dw * dh * C];
    let mut sums = vec![[0u32; C]; dw];
    for (dy, out_row) in out.chunks_exact_mut(dw * C).enumerate() {
        sums.fill([0; C]);
        for y in dy * k..(dy + 1) * k {
            let start = y * w * C;
            let (row, _) = data.get(start..start + dw * k * C)?.as_chunks::<C>();
            for (sum, block) in sums.iter_mut().zip(row.chunks_exact(k)) {
                for px in block {
                    for (s, &v) in sum.iter_mut().zip(px) {
                        *s += u32::from(v);
                    }
                }
            }
        }
        let (row, _) = out_row.as_chunks_mut::<C>();
        for (o, sum) in row.iter_mut().zip(&sums) {
            for (o, &s) in o.iter_mut().zip(sum) {
                *o = ((s + n / 2) / n) as u8;
            }
        }
    }
    Some(out)
}

impl Sample for u16 {
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    fn from_f64(v: f64) -> Self {
        v.round().clamp(0.0, 65535.0) as u16
    }
}

impl Sample for f32 {
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    fn from_f64(v: f64) -> Self {
        v as f32
    }
}

/// [`Sample::shrink_opaque`] for pixels whose last channel is alpha: each
/// color is weighted by its alpha, and a block with no alpha at all comes
/// out transparent black.
fn shrink_weighted<S: Sample>(data: &[S], dims: (usize, usize, usize, usize)) -> Option<Vec<S>> {
    let channels = dims.2;
    shrink_rows(
        data,
        dims,
        0.0f64,
        |sums, px| {
            let Some((&a, colors)) = px.split_last() else {
                return;
            };
            let a = a.to_f64();
            for (s, &v) in sums.iter_mut().zip(colors) {
                *s += v.to_f64() * a;
            }
            if let Some(s) = sums.get_mut(channels - 1) {
                *s += a;
            }
        },
        |sums, out, n| {
            let Some((&a, colors)) = sums.split_last() else {
                return;
            };
            for (o, &s) in out.iter_mut().zip(colors) {
                *o = S::from_f64(if a > 0.0 { s / a } else { 0.0 });
            }
            if let Some(o) = out.last_mut() {
                *o = S::from_f64(a / n as f64);
            }
        },
    )
}

/// The block walk the shrinks share. `data` is checked to hold exactly
/// `w * h * channels` samples first, so every block row read below lies
/// inside it: rows stop at `dh * k <= h` and columns at `dw * k <= w`.
/// `add` folds one pixel into its block's sums; `finish` writes a block's
/// output pixel from its sums and its pixel count. Each output row walks the
/// source in `k`-pixel runs, one run a block, so the hot loop has no
/// division.
fn shrink_rows<S: Copy, A: Copy>(
    data: &[S],
    (w, h, channels, k): (usize, usize, usize, usize),
    zero: A,
    add: impl Fn(&mut [A], &[S]),
    finish: impl Fn(&[A], &mut [S], usize),
) -> Option<Vec<S>> {
    let (dw, dh) = (w.checked_div(k)?, h.checked_div(k)?);
    if dw == 0 || dh == 0 || channels == 0 {
        return None;
    }
    if data.len() != w.checked_mul(h)?.checked_mul(channels)? {
        return None;
    }
    let row_len = w * channels;
    let run = k * channels;
    let used = dw * run;
    let n = k * k;
    let first = *data.first()?;
    let mut out = vec![first; dw * dh * channels];
    let mut sums = vec![zero; dw * channels];
    for (dy, out_row) in out.chunks_exact_mut(dw * channels).enumerate() {
        sums.fill(zero);
        for y in dy * k..(dy + 1) * k {
            let start = y * row_len;
            let row = data.get(start..start + used)?;
            for (sum, block) in sums.chunks_exact_mut(channels).zip(row.chunks_exact(run)) {
                for px in block.chunks_exact(channels) {
                    add(sum, px);
                }
            }
        }
        for (sum, px) in sums
            .chunks_exact(channels)
            .zip(out_row.chunks_exact_mut(channels))
        {
            finish(sum, px, n);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_shrink_averages_whole_blocks_and_crops_the_partial_edge() {
        let src = image::GrayImage::from_raw(5, 3, (0..15).map(|v| v * 10).collect()).unwrap();
        let out = shrink_buffer(&src, 2, false).unwrap();
        assert_eq!(out.dimensions(), (2, 1));
        // Rows 0-1 by columns 0-1 and 2-3; column 4 and row 2 are cropped.
        assert_eq!(out.as_raw(), &vec![30, 50]);
    }

    /// A transparent pixel's color must not tint the block: one opaque red
    /// pixel and three transparent blue ones average to red at a quarter
    /// alpha.
    #[test]
    fn a_block_shrink_weights_color_by_alpha() {
        let mut src = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 255, 0]));
        src.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        let out = shrink_buffer(&src, 2, true).unwrap();
        assert_eq!(out.get_pixel(0, 0).0, [255, 0, 0, 64]);
        let clear = image::RgbaImage::from_pixel(2, 2, image::Rgba([9, 9, 9, 0]));
        assert_eq!(
            shrink_buffer(&clear, 2, true).unwrap().get_pixel(0, 0).0,
            [0, 0, 0, 0]
        );
    }

    /// A 16-bit image shrinks in its own depth, so there is no full-size
    /// RGBA8 copy before the shrink.
    #[test]
    fn a_16_bit_image_shrinks_in_its_own_depth() {
        let src = image::ImageBuffer::<image::Rgb<u16>, _>::from_pixel(
            8,
            8,
            image::Rgb([1000, 40000, 65535]),
        );
        let out = box_shrink(DynamicImage::ImageRgb16(src), 4);
        let DynamicImage::ImageRgb16(out) = out else {
            panic!("the shrink changed the layout");
        };
        assert_eq!(out.dimensions(), (2, 2));
        assert_eq!(out.get_pixel(1, 1).0, [1000, 40000, 65535]);
    }

    #[test]
    fn a_buffer_that_does_not_match_its_size_is_left_alone() {
        assert!(shrink_rows(&[0u8; 7], (2, 2, 2, 2), 0u32, |_, _| {}, |_, _, _| {}).is_none());
        assert!(shrink_rows(&[0u8; 8], (2, 2, 2, 0), 0u32, |_, _| {}, |_, _, _| {}).is_none());
        assert!(shrink_rows(&[0u8; 8], (2, 2, 2, 4), 0u32, |_, _| {}, |_, _, _| {}).is_none());
    }

    /// Fitting before turning lands the same size and the same corners as
    /// turning first, and the block shrink stays close to a straight
    /// Lanczos3 of the whole frame.
    #[test]
    fn fitting_then_orienting_matches_orienting_then_fitting() {
        let src = image::RgbImage::from_fn(1200, 600, |x, y| {
            let r = if x < 600 { 230 } else { 20 };
            image::Rgb([r, (y * 255 / 599) as u8, ((x / 5) % 256) as u8])
        });
        let img = DynamicImage::ImageRgb8(src);
        let fast = fit_decoded(img.clone(), 100, Orientation::Rotate90).unwrap();

        let mut slow = img;
        slow.apply_orientation(Orientation::Rotate90);
        let slow = image::imageops::resize(
            &slow.into_rgba8(),
            50,
            100,
            image::imageops::FilterType::Lanczos3,
        );
        assert_eq!(fast.dimensions(), (50, 100));
        let diff: u64 = fast
            .as_raw()
            .iter()
            .zip(slow.as_raw())
            .map(|(&a, &b)| u64::from(a.abs_diff(b)))
            .sum();
        let mean = diff as f64 / fast.as_raw().len() as f64;
        assert!(mean < 3.0, "mean abs diff {mean}");
        // The red left half turns to the top.
        assert!(fast.get_pixel(25, 10)[0] > 200);
        assert!(fast.get_pixel(25, 90)[0] < 50);
    }

    /// The grant follows the pipeline: a thumbnail of a 24 MP JPEG holds the
    /// decode and its block shrink, far less than the old full-size RGBA8
    /// copies and full-width Lanczos pass; a large target holds Lanczos's
    /// full-width pass.
    #[test]
    fn the_peak_follows_the_pipeline() {
        let rgb = 3;
        let src = (6000, 4000);
        let thumb = Plan::new(src, 512);
        assert_eq!(thumb.factor, 5);
        assert_eq!(thumb.shrunk, (1200, 800));
        assert_eq!(thumb.peak_bytes(rgb, false), 72_000_000 + 1200 * 800 * 3);
        let large = Plan::new(src, 2560);
        assert_eq!(large.factor, 1);
        let lanczos = 96_000_000 + 6000 * 1706 * 16 + 2560 * 1706 * 4;
        assert_eq!(large.peak_bytes(rgb, false), lanczos);
        let whole = Plan::new(src, 8192);
        assert_eq!(whole.peak_bytes(rgb, true), 96_000_000 * 2);
        let huge = Plan::new((u32::MAX, u32::MAX), 1);
        assert!(huge.peak_bytes(u64::MAX, true) == u64::MAX);
    }
}
