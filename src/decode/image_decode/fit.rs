// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fits a decoded image within a size and turns it upright, for every decode
//! of one off macOS: a JPEG, PNG or TIFF photo, its EXIF thumbnail, the
//! camera JPEG inside a RAF or CR3, and the RAW `Fast` tier's binned image.
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
//! On wasm32 a decode first takes one `decode_budget` grant for all of it,
//! sized from the header ([`Plan::peak_bytes`]).

use image::metadata::Orientation;
use image::{DynamicImage, ImageBuffer, ImageDecoder, ImageFormat, RgbaImage};

use crate::decode::image_decode::{check_decode_size, fit_within};

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
    /// an image decoded at `native_bpp` bytes a pixel with `scratch` more
    /// bytes of decoder working memory. `turns` when the orientation copies
    /// the output.
    #[cfg_attr(
        not(target_arch = "wasm32"),
        allow(dead_code, reason = "only wasm32 has a decode budget")
    )]
    pub(crate) fn peak_bytes(&self, native_bpp: u64, scratch: u64, turns: bool) -> u64 {
        let px = |(w, h): (u32, u32)| u64::from(w) * u64::from(h);
        let native = px(self.src).saturating_mul(native_bpp);
        let shrunk_native = px(self.shrunk).saturating_mul(native_bpp);
        let rgba = px(self.shrunk).saturating_mul(4);
        let out = px(self.out).saturating_mul(4);
        // The decode: its output and its own working memory.
        let decode = native.saturating_add(scratch);
        // The block shrink beside the decode's output.
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
        decode.max(shrink).max(to_rgba).max(lanczos).max(turn)
    }
}

/// Decodes `bytes`, of `format` or guessed when `None`, fit within
/// `max_dim` and turned upright by `orientation`, or by the image's own EXIF
/// orientation when `None`. The header's size is checked against
/// `check_decode_size` before anything is reserved or allocated.
pub fn decode_fit(
    bytes: &[u8],
    format: Option<ImageFormat>,
    max_dim: u32,
    orientation: Option<Orientation>,
) -> Result<RgbaImage, String> {
    let reader = std::io::Cursor::new(bytes);
    let reader = match format {
        Some(format) => image::ImageReader::with_format(reader, format),
        None => image::ImageReader::new(reader)
            .with_guessed_format()
            .map_err(|e| e.to_string())?,
    };
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation =
        orientation.unwrap_or_else(|| decoder.orientation().unwrap_or(Orientation::NoTransforms));
    let src = decoder.dimensions();
    check_decode_size(src.0, src.1)?;
    let plan = Plan::new(src, max_dim);
    // Six browser decode threads share one heap that never shrinks, so a
    // folder of large JPEGs must wait its turn as RAWs do. Held until this
    // returns; sized from the header before any pixel is allocated.
    #[cfg(target_arch = "wasm32")]
    let _grant = {
        let bpp = u64::from(decoder.color_type().bytes_per_pixel());
        let scratch = decoder_scratch(bytes, src, decoder.color_type());
        crate::decode::decode_budget::acquire(clamp_usize(plan.peak_bytes(
            bpp,
            scratch,
            orientation != Orientation::NoTransforms,
        )))
    };
    let img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    fit_and_orient(img, &plan, orientation)
}

/// [`decode_fit`]'s steps after the decode, for an image another decoder
/// produced: rawler's embedded camera JPEG, or the RAW `Fast` tier's bins.
/// It takes no grant; on wasm32 the caller holds one for the whole decode
/// (see [`decoded_grant_bytes`]), since a second grant asked for on the same
/// thread could wait on the first forever.
pub fn fit_decoded(
    img: DynamicImage,
    max_dim: u32,
    orientation: Orientation,
) -> Result<RgbaImage, String> {
    let plan = Plan::new((img.width(), img.height()), max_dim);
    fit_and_orient(img, &plan, orientation)
}

/// What to reserve for decoding a `src`-sized 8-bit RGB image elsewhere and
/// then [`fit_decoded`]: the caller's grant, taken before that decode.
#[cfg_attr(
    not(target_arch = "wasm32"),
    allow(dead_code, reason = "only wasm32 has a decode budget")
)]
pub fn decoded_grant_bytes(src: (u32, u32), max_dim: u32) -> usize {
    // A camera JPEG decodes to RGB8, with up to as much again for the
    // decoder's own working memory; the turn may copy the output.
    let native = 3;
    let px = u64::from(src.0) * u64::from(src.1);
    clamp_usize(Plan::new(src, max_dim).peak_bytes(native, px.saturating_mul(native), true))
}

#[cfg_attr(
    not(target_arch = "wasm32"),
    allow(dead_code, reason = "only wasm32 has a decode budget")
)]
fn clamp_usize(bytes: u64) -> usize {
    usize::try_from(bytes).unwrap_or(usize::MAX)
}

/// The decoder's working memory beyond its output: a progressive JPEG keeps
/// every DCT coefficient, 2 bytes a sample, until its last scan. Baseline
/// JPEGs, PNG and TIFF decode in rows.
#[cfg_attr(
    not(target_arch = "wasm32"),
    allow(dead_code, reason = "only wasm32 has a decode budget")
)]
fn decoder_scratch(bytes: &[u8], (w, h): (u32, u32), color: image::ColorType) -> u64 {
    if !jpeg_is_progressive(bytes) {
        return 0;
    }
    let samples = u64::from(color.channel_count());
    (u64::from(w) * u64::from(h))
        .saturating_mul(samples)
        .saturating_mul(2)
}

/// Whether `bytes` is a JPEG whose frame header is progressive (SOF2, SOF6,
/// SOF10 or SOF14). Walks the marker segments up to the first frame header;
/// anything malformed reads as not progressive.
fn jpeg_is_progressive(bytes: &[u8]) -> bool {
    if bytes.get(..2) != Some(&[0xFF, 0xD8]) {
        return false;
    }
    let mut at = 2usize;
    loop {
        let (Some(&0xFF), Some(&marker)) = (bytes.get(at), bytes.get(at + 1)) else {
            return false;
        };
        match marker {
            // Fill bytes before a marker.
            0xFF => at += 1,
            0xC2 | 0xC6 | 0xCA | 0xCE => return true,
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => return false,
            // Start of scan or end of image before any frame header.
            0xDA | 0xD9 => return false,
            _ => {
                let len = match (bytes.get(at + 2), bytes.get(at + 3)) {
                    (Some(&hi), Some(&lo)) => usize::from(u16::from_be_bytes([hi, lo])),
                    _ => return false,
                };
                if len < 2 {
                    return false;
                }
                at = match at.checked_add(2 + len) {
                    Some(next) => next,
                    None => return false,
                };
            }
        }
    }
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
    let dims = (w as usize, h as usize, k as usize);
    let data = src.as_raw();
    let out = match (P::CHANNEL_COUNT, alpha) {
        (1, false) => shrink::<_, 1>(data, dims, false),
        (2, true) => shrink::<_, 2>(data, dims, true),
        (3, false) => shrink::<_, 3>(data, dims, false),
        (4, true) => shrink::<_, 4>(data, dims, true),
        _ => None,
    }?;
    ImageBuffer::from_raw(w / k, h / k, out)
}

/// A channel type the block shrink averages, with the sums it averages in:
/// for 8-bit, `u32` when opaque (`MAX_FACTOR` keeps them from overflowing)
/// and `u64` when weighted by alpha; `f64` for 16-bit and float.
trait Sample: Copy {
    type Sum: Copy + Default + std::ops::AddAssign;
    type Weighted: Copy + Default + std::ops::AddAssign + PartialEq;
    fn sum(self) -> Self::Sum;
    fn weighted(self, alpha: Self) -> Self::Weighted;
    fn alpha(self) -> Self::Weighted;
    /// The block's mean: `sum / n`, rounded, back in this type.
    fn mean(sum: Self::Sum, n: usize) -> Self;
    /// `sum / weight`, rounded, back in this type. `weight` is never 0.
    fn mean_weighted(sum: Self::Weighted, weight: Self::Weighted) -> Self;
    /// The block's mean alpha.
    fn mean_alpha(sum: Self::Weighted, n: usize) -> Self;
    fn zero() -> Self;
}

impl Sample for u8 {
    type Sum = u32;
    type Weighted = u64;
    fn sum(self) -> u32 {
        u32::from(self)
    }
    fn weighted(self, alpha: u8) -> u64 {
        u64::from(self) * u64::from(alpha)
    }
    fn alpha(self) -> u64 {
        u64::from(self)
    }
    fn mean(sum: u32, n: usize) -> u8 {
        let n = n as u32;
        ((sum + n / 2) / n).min(255) as u8
    }
    fn mean_weighted(sum: u64, weight: u64) -> u8 {
        ((sum + weight / 2) / weight).min(255) as u8
    }
    fn mean_alpha(sum: u64, n: usize) -> u8 {
        let n = n as u64;
        ((sum + n / 2) / n).min(255) as u8
    }
    fn zero() -> u8 {
        0
    }
}

/// `Sample` for a channel averaged in `f64`, rounding and clamping back
/// with `back`.
macro_rules! float_sample {
    ($t:ty, $back:expr) => {
        impl Sample for $t {
            type Sum = f64;
            type Weighted = f64;
            fn sum(self) -> f64 {
                f64::from(self)
            }
            fn weighted(self, alpha: $t) -> f64 {
                f64::from(self) * f64::from(alpha)
            }
            fn alpha(self) -> f64 {
                f64::from(self)
            }
            fn mean(sum: f64, n: usize) -> $t {
                $back(sum / n as f64)
            }
            fn mean_weighted(sum: f64, weight: f64) -> $t {
                $back(sum / weight)
            }
            fn mean_alpha(sum: f64, n: usize) -> $t {
                $back(sum / n as f64)
            }
            fn zero() -> $t {
                <$t>::default()
            }
        }
    };
}

float_sample!(u16, |v: f64| v.round().clamp(0.0, 65535.0) as u16);
float_sample!(f32, |v: f64| v as f32);

/// Averages whole `k` x `k` blocks of a `w` x `h` buffer of `C`-channel
/// pixels. With `alpha`, the last channel is alpha: colors are weighted by
/// it, and a block with none comes out transparent black.
fn shrink<S: Sample, const C: usize>(
    data: &[S],
    dims: (usize, usize, usize),
    alpha: bool,
) -> Option<Vec<S>> {
    let n = dims.2.checked_mul(dims.2)?;
    if !alpha {
        return walk::<S, S::Sum, C>(
            data,
            dims,
            |sum, px| {
                for (s, &v) in sum.iter_mut().zip(px) {
                    *s += v.sum();
                }
            },
            |sum, out| {
                for (o, &s) in out.iter_mut().zip(sum) {
                    *o = S::mean(s, n);
                }
            },
        );
    }
    walk::<S, S::Weighted, C>(
        data,
        dims,
        |sum, px| {
            let a = px[C - 1];
            for (s, &v) in sum.iter_mut().zip(&px[..C - 1]) {
                *s += v.weighted(a);
            }
            sum[C - 1] += a.alpha();
        },
        |sum, out| {
            let a = sum[C - 1];
            let none = a == S::Weighted::default();
            for (o, &s) in out.iter_mut().zip(&sum[..C - 1]) {
                *o = if none {
                    S::zero()
                } else {
                    S::mean_weighted(s, a)
                };
            }
            out[C - 1] = S::mean_alpha(a, n);
        },
    )
}

/// The block walk both shrinks share. `add` folds one pixel into its
/// block's sums; `finish` writes a block's output pixel.
///
/// `data` is checked to hold exactly `w * h * C` samples first, so every
/// row read below lies inside it: rows stop at `dh * k <= h` and columns at
/// `dw * k <= w`. Each output row walks the source in `k`-pixel runs, one
/// run a block, so the hot loop has no division.
fn walk<S: Copy, A: Copy + Default, const C: usize>(
    data: &[S],
    (w, h, k): (usize, usize, usize),
    add: impl Fn(&mut [A; C], &[S; C]),
    finish: impl Fn(&[A; C], &mut [S; C]),
) -> Option<Vec<S>> {
    let (dw, dh) = (w.checked_div(k)?, h.checked_div(k)?);
    if dw == 0 || dh == 0 || C == 0 || data.len() != w.checked_mul(h)?.checked_mul(C)? {
        return None;
    }
    let mut out = vec![*data.first()?; dw * dh * C];
    let mut sums = vec![[A::default(); C]; dw];
    for (dy, out_row) in out.chunks_exact_mut(dw * C).enumerate() {
        sums.fill([A::default(); C]);
        for y in dy * k..(dy + 1) * k {
            let start = y * w * C;
            let (row, _) = data.get(start..start + dw * k * C)?.as_chunks::<C>();
            for (sum, block) in sums.iter_mut().zip(row.chunks_exact(k)) {
                for px in block {
                    add(sum, px);
                }
            }
        }
        let (row, _) = out_row.as_chunks_mut::<C>();
        for (o, sum) in row.iter_mut().zip(&sums) {
            finish(sum, o);
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
    /// alpha, in 8 and 16 bits.
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
        let mut deep =
            ImageBuffer::<image::Rgba<u16>, _>::from_pixel(2, 2, image::Rgba([0, 0, 65535, 0]));
        deep.put_pixel(0, 0, image::Rgba([65535, 0, 0, 65535]));
        let out = shrink_buffer(&deep, 2, true).unwrap();
        assert_eq!(out.get_pixel(0, 0).0, [65535, 0, 0, 16384]);
    }

    /// A 16-bit image shrinks in its own depth, so there is no full-size
    /// RGBA8 copy before the shrink.
    #[test]
    fn a_16_bit_image_shrinks_in_its_own_depth() {
        let src =
            ImageBuffer::<image::Rgb<u16>, _>::from_pixel(8, 8, image::Rgb([1000, 40000, 65535]));
        let out = box_shrink(DynamicImage::ImageRgb16(src), 4);
        let DynamicImage::ImageRgb16(out) = out else {
            panic!("the shrink changed the layout");
        };
        assert_eq!(out.dimensions(), (2, 2));
        assert_eq!(out.get_pixel(1, 1).0, [1000, 40000, 65535]);
    }

    #[test]
    fn a_buffer_that_does_not_match_its_size_is_left_alone() {
        assert!(shrink::<u8, 2>(&[0; 7], (2, 2, 2), false).is_none());
        assert!(shrink::<u8, 2>(&[0; 8], (2, 2, 0), false).is_none());
        assert!(shrink::<u8, 2>(&[0; 8], (2, 2, 4), false).is_none());
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
    /// full-width pass; a progressive decode's coefficients count.
    #[test]
    fn the_peak_follows_the_pipeline() {
        let rgb = 3;
        let src = (6000, 4000);
        let thumb = Plan::new(src, 512);
        assert_eq!(thumb.factor, 5);
        assert_eq!(thumb.shrunk, (1200, 800));
        assert_eq!(thumb.peak_bytes(rgb, 0, false), 72_000_000 + 1200 * 800 * 3);
        assert_eq!(thumb.peak_bytes(rgb, 144_000_000, false), 216_000_000);
        let large = Plan::new(src, 2560);
        assert_eq!(large.factor, 1);
        let lanczos = 96_000_000 + 6000 * 1706 * 16 + 2560 * 1706 * 4;
        assert_eq!(large.peak_bytes(rgb, 0, false), lanczos);
        let whole = Plan::new(src, 8192);
        assert_eq!(whole.peak_bytes(rgb, 0, true), 96_000_000 * 2);
        let huge = Plan::new((u32::MAX, u32::MAX), 1);
        assert_eq!(huge.peak_bytes(u64::MAX, 0, true), u64::MAX);
    }

    #[test]
    fn a_progressive_jpeg_is_told_from_a_baseline_one() {
        let jpeg = |sof: u8| {
            let mut b = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0, 0, 0xFF, 0xFF];
            b.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x02, 0xFF, sof, 0x00, 0x02]);
            b
        };
        assert!(jpeg_is_progressive(&jpeg(0xC2)));
        assert!(!jpeg_is_progressive(&jpeg(0xC0)));
        assert!(!jpeg_is_progressive(b"\x89PNG"));
        // Truncated or a length that runs off the end: not progressive.
        assert!(!jpeg_is_progressive(&[0xFF, 0xD8, 0xFF, 0xE0, 0xFF]));
        assert!(!jpeg_is_progressive(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x00]));
    }

    #[test]
    fn a_huge_header_is_refused_before_anything_is_reserved() {
        // A 16 x 16 PNG header claiming 65535 x 65535.
        let mut png = Vec::new();
        image::RgbImage::new(16, 16)
            .write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        png[16..20].copy_from_slice(&65535u32.to_be_bytes());
        png[20..24].copy_from_slice(&65535u32.to_be_bytes());
        assert!(decode_fit(&png, None, 512, None).is_err());
    }
}
