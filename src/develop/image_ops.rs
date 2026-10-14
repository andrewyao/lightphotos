// SPDX-License-Identifier: MIT OR Apache-2.0

//! CPU pixel operations with no UI dependency. [`bake_edited`] is the one
//! place edits are burned into pixels, for both export and edited thumbnails,
//! so the two always match. The Loupe applies edits on the GPU instead.

use crate::decode::image_decode::{DecodedImage, PixelFormat};
use crate::develop::base::Fields;
use crate::develop::{self, local, mask, Adjustments, BaseLayer, BaseProbe, Crop, Mask, TouchUp};

/// Normalized crop (`None` = full frame) to pixel bounds `(x0, y0, x1, y1)`,
/// at least 1x1.
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

use develop::{srgb_decode, srgb_encode};

/// Linear light to an sRGB8 channel, rounded as the Loupe's 8-bit surface
/// stores `linear_to_srgb`'s output.
fn encode_srgb8_curve(v: f32) -> u8 {
    (srgb_encode(v) * 255.0).round().clamp(0.0, 255.0) as u8
}

/// [`srgb_decode`] for every 8-bit `c`: the same floats, computed once. A
/// 512px edited thumbnail makes half a million of these.
pub(crate) fn srgb8_to_linear_lut() -> &'static [f32; 256] {
    static LUT: std::sync::OnceLock<[f32; 256]> = std::sync::OnceLock::new();
    LUT.get_or_init(|| std::array::from_fn(|c| srgb_decode(c as f32 / 255.0)))
}

/// Linear light to an sRGB8 channel, rounded. Bit for bit what
/// [`encode_srgb8_curve`] gives, found by a search over the 255 inputs where
/// that output steps up rather than a `powf` per channel.
pub(crate) fn linear_to_srgb8(v: f32) -> u8 {
    static STEPS: std::sync::OnceLock<[f32; 255]> = std::sync::OnceLock::new();
    let steps = STEPS.get_or_init(|| {
        // Positive floats order the same as their bit patterns, so each step
        // is a bisection over bits for the smallest input reaching `k`.
        std::array::from_fn(|i| {
            let k = i as u8 + 1;
            let (mut lo, mut hi) = (0u32, 1.0f32.to_bits());
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if encode_srgb8_curve(f32::from_bits(mid)) >= k {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            f32::from_bits(lo)
        })
    });
    steps.partition_point(|&step| step <= v) as u8
}

/// One sRGB-encoded channel of a baked image: 8 bits for JPEG, the Loupe
/// and thumbnails, 16 for a TIFF export, which keeps the edit math's
/// in-between levels instead of rounding them to 256.
pub trait Sample: Copy + Default + Send + Sync {
    /// The value of full white.
    const MAX: f32;
    /// Linear light to this sample, rounded.
    fn encode(linear: f32) -> Self;
    /// Every sample's linear light, indexed by [`Sample::index`].
    fn linear_lut() -> &'static [f32];
    fn index(self) -> usize;
    /// `self` scaled by `g`, rounded and clamped to the sample's range.
    fn scaled(self, g: f32) -> Self;
    fn level(self) -> f32;
}

impl Sample for u8 {
    const MAX: f32 = 255.0;
    fn encode(linear: f32) -> u8 {
        linear_to_srgb8(linear)
    }
    fn linear_lut() -> &'static [f32] {
        srgb8_to_linear_lut()
    }
    fn index(self) -> usize {
        self as usize
    }
    fn scaled(self, g: f32) -> u8 {
        (self as f32 * g).round().clamp(0.0, 255.0) as u8
    }
    fn level(self) -> f32 {
        self as f32
    }
}

impl Sample for u16 {
    const MAX: f32 = 65535.0;
    fn encode(linear: f32) -> u16 {
        (srgb_encode(linear) * 65535.0).round().clamp(0.0, 65535.0) as u16
    }
    fn linear_lut() -> &'static [f32] {
        static LUT: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
        LUT.get_or_init(|| {
            (0..=65535u32)
                .map(|c| srgb_decode(c as f32 / 65535.0))
                .collect()
        })
    }
    fn index(self) -> usize {
        self as usize
    }
    fn scaled(self, g: f32) -> u16 {
        (self as f32 * g).round().clamp(0.0, 65535.0) as u16
    }
    fn level(self) -> f32 {
        self as f32
    }
}

/// Premultiplied sRGB8 pixel (as decoded) to linear RGB, through the sRGB
/// curve the Loupe's texture sampler uses. Every CPU reader of decoded pixels
/// goes through this so they all see the same values.
fn unpremul_to_linear(px: [u8; 4]) -> [f32; 3] {
    let [r, g, b, a] = px;
    if a == 255 {
        let lut = srgb8_to_linear_lut();
        return [lut[r as usize], lut[g as usize], lut[b as usize]];
    }
    if a == 0 {
        return [0.0; 3];
    }
    let inv = 255.0 / a as f32;
    [r, g, b].map(|c| srgb_decode((c as f32 * inv).min(255.0) / 255.0))
}

/// The photo as the shader samples it: linear light, bilinear, clamped at the
/// edge, red and blue read through the chromatic-aberration scales.
struct Source<'a> {
    img: &'a DecodedImage,
    ca: develop::CaScale,
    /// Every texel in linear light, for denoise, which reads each one 25
    /// times.
    linear: Option<Vec<[f32; 3]>>,
}

impl<'a> Source<'a> {
    /// `None` for an empty image or one short of `width * height` pixels in
    /// its format, so every read below stays inside the buffer.
    fn new(img: &'a DecodedImage, ca: develop::CaScale, cache: bool) -> Option<Self> {
        let (w, h) = (img.width as usize, img.height as usize);
        let n = w.checked_mul(h)?;
        if n == 0 || img.rgba.len() < n.checked_mul(img.pixel_format.bytes_per_pixel())? {
            return None;
        }
        let linear = cache.then(|| {
            (0..n)
                .map(|i| pixel_linear(img, (i % w) as u32, (i / w) as u32))
                .collect()
        });
        Some(Self { img, ca, linear })
    }

    /// Texel `(x, y)` clamped to the image, for the exact path.
    fn at_px(&self, x: i64, y: i64) -> [f32; 3] {
        let (w, h) = (self.img.width as i64, self.img.height as i64);
        let (x, y) = (x.clamp(0, w - 1), y.clamp(0, h - 1));
        match &self.linear {
            Some(buf) => buf[(y * w + x) as usize],
            None => pixel_linear(self.img, x as u32, y as u32),
        }
    }

    /// All three channels at source UV, as `textureSample` filters them:
    /// the four texels around `uv * size - 0.5`, clamped at the edge.
    fn bilinear(&self, u: f32, v: f32) -> [f32; 3] {
        develop::base::bilinear(self.img.width, self.img.height, u, v, |x, y| {
            self.at_px(x, y)
        })
    }

    /// The source at UV with chromatic aberration removed. Must match
    /// `sampleSrc` and `sampleSrcLevel` in loupe_common.wgsl.
    fn at_uv(&self, u: f32, v: f32) -> [f32; 3] {
        let g = self.bilinear(u, v);
        if self.ca.is_zero() {
            return g;
        }
        let channel = |k: f32, c: usize| {
            let (u, v) = develop::CaScale::source_uv(k, u, v);
            self.bilinear(u, v)[c]
        };
        [channel(self.ca.red, 0), g[1], channel(self.ca.blue, 2)]
    }
}

/// A bilinear read of `img` at UV in linear light through the aberration
/// scales `ca`, the way the shader and the bake read a touch-up's source
/// patch. `None` for a short image.
pub fn bilinear_sampler(
    img: &DecodedImage,
    ca: develop::CaScale,
) -> Option<impl Fn(f32, f32) -> [f32; 3] + '_> {
    let src = Source::new(img, ca, false)?;
    Some(move |u, v| src.at_uv(u, v))
}

/// Burn edits into an opaque sRGB8 RGBA buffer: crop, then touch-ups and tone
/// per pixel, then rotate by `rot` 90-degree clockwise steps. Returns
/// `(w, h, rgba)`. Each pixel reads the source the way `fs_main` in
/// shader.wgsl does, and tone uses `develop::apply_linear`, which matches the
/// shader. Where the turned photo leaves the frame the shader draws
/// background but this clamps to the edge, so an export never shows grey.
#[hotpath::measure]
pub fn bake_edited(
    img: &DecodedImage,
    adj: &Adjustments,
    touchups: &[TouchUp],
    masks: &[Mask],
    rot: u8,
) -> (u32, u32, Vec<u8>) {
    bake_edited_probe(img, adj, touchups, masks, rot, BaseProbe::Off)
}

/// [`bake_edited`] at any sample depth. At 16 bits the decode is still
/// 8-bit sRGB, but every edit runs in floats, so the extra bits hold the
/// levels a curve or exposure push lands between two 8-bit steps.
#[hotpath::measure]
pub fn bake_edited_as<S: Sample>(
    img: &DecodedImage,
    adj: &Adjustments,
    touchups: &[TouchUp],
    masks: &[Mask],
    rot: u8,
) -> (u32, u32, Vec<S>) {
    bake_samples(img, adj, touchups, masks, rot, BaseProbe::Off)
}

/// [`bake_edited`] with a layer drawn in place of the photo as `probe`
/// says, as the Loupe does for `GpuAdjust::base_probe`. A layer is built
/// only for an edit or probe that reads it.
pub fn bake_edited_probe(
    img: &DecodedImage,
    adj: &Adjustments,
    touchups: &[TouchUp],
    masks: &[Mask],
    rot: u8,
    probe: BaseProbe,
) -> (u32, u32, Vec<u8>) {
    bake_samples(img, adj, touchups, masks, rot, probe)
}

fn bake_samples<S: Sample>(
    img: &DecodedImage,
    adj: &Adjustments,
    touchups: &[TouchUp],
    masks: &[Mask],
    rot: u8,
    probe: BaseProbe,
) -> (u32, u32, Vec<S>) {
    let (w, h) = (img.width, img.height);
    let ca = adj.chromatic_aberration.unwrap_or_default();
    let sharpen = develop::Sharpen::new(&adj.sharpening, w as f32, h as f32);
    let Some(src) = Source::new(img, ca, adj.denoise > 0.0) else {
        return (0, 0, Vec::new());
    };
    let turn = (adj.straighten != 0.0)
        .then(|| develop::Straighten::new(adj.straighten, w as f32, h as f32));
    // Upright with no aberration, every read lands on a texel center, so
    // read texels directly: bit-exact, and the fast path for export.
    let exact = turn.is_none() && ca.is_zero();
    let (texel_w, texel_h) = (1.0 / w as f32, 1.0 / h as f32);

    let (x0, y0, x1, y1) = crop_bounds(adj.crop, w, h);
    let (cw, ch) = (x1 - x0, y1 - y0);
    let dev = develop::Develop::framed(adj, w, h);
    let global = develop::LocalAmounts::of(adj);
    let aspect = w as f32 / h as f32;
    let pre = |x: u32, y: u32| pre_at(&src, touchups, x, y);
    // Each layer only for an edit or probe that reads it.
    let base = if probe.reads_base() {
        BaseLayer::build(w, h, Fields::All, pre)
    } else if adj.needs_base() {
        BaseLayer::build(w, h, Fields::MidDark, pre)
    } else {
        None
    };
    let fine = (probe == BaseProbe::Fine || adj.texture != 0.0)
        .then(|| BaseLayer::build_fine(w, h, pre))
        .flatten();
    let mut cropped = vec![S::default(); (cw * ch * 4) as usize];
    let opaque = S::encode(1.0);
    develop::parallel::for_each_row(&mut cropped, (cw * 4) as usize, |y, out_row| {
        let y = y as u32;
        for (x, out_px) in (0..cw).zip(out_row.as_chunks_mut::<4>().0) {
            let (px, py) = (x0 + x, y0 + y);
            let canvas = ((px as f32 + 0.5) / w as f32, (py as f32 + 0.5) / h as f32);
            let (u, v) = turn.map_or(canvas, |t| t.to_source(canvas.0, canvas.1));
            let lin = develop::denoise_sample(adj, |dx, dy| {
                if exact {
                    src.at_px(px as i64 + dx as i64, py as i64 + dy as i64)
                } else {
                    src.at_uv(u + dx as f32 * texel_w, v + dy as f32 * texel_h)
                }
            });
            let pre = apply_touchups(&src, touchups, u, v, lin);
            let base_here = base.as_ref().map(|b| b.at_uv(u, v));
            let fine_here = fine.as_ref().map(|f| f.at_uv(u, v));
            // The probe shows a layer itself, so nothing reads it after.
            let pre = if probe == BaseProbe::Off {
                let out = local::apply(adj, pre, base_here, fine_here.map(|f| f.mid));
                if sharpen.is_off() {
                    out
                } else {
                    let step = (sharpen.step * texel_w, sharpen.step * texel_h);
                    // A pixel no spot reaches needs no touch-up on its taps.
                    let spots = if near_a_spot(&src, touchups, u, v, 3.0 * sharpen.step) {
                        touchups
                    } else {
                        &[]
                    };
                    let blur = develop::sharpen::blur(|dx, dy| {
                        let (tu, tv) = (u + dx as f32 * step.0, v + dy as f32 * step.1);
                        apply_touchups(&src, spots, tu, tv, src.at_uv(tu, tv))
                    });
                    sharpen.apply(out, pre, blur)
                }
            } else {
                probe.apply(
                    base_here.unwrap_or_default(),
                    fine_here.unwrap_or_default(),
                    pre,
                )
            };
            let out = if masks.is_empty() {
                dev.linear_at(pre, canvas.0, canvas.1)
            } else {
                let tone = mask::local_tone(global, masks, [u, v], aspect);
                dev.linear_at_local(pre, canvas.0, canvas.1, &tone)
            };
            out_px[0] = S::encode(out[0]);
            out_px[1] = S::encode(out[1]);
            out_px[2] = S::encode(out[2]);
            out_px[3] = opaque;
        }
    });

    rotate_rgba(&cropped, cw, ch, rot)
}

/// The pre-tone pixel the base and fine layers average: texel `(x, y)` at
/// its center with CA removed and the touch-ups blended, without denoise.
/// Must match `preAt` in base.wgsl.
fn pre_at(src: &Source, touchups: &[TouchUp], x: u32, y: u32) -> [f32; 3] {
    let (w, h) = (src.img.width, src.img.height);
    let (u, v) = ((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
    // A texel center reads that texel exactly, so skip the bilinear.
    let px = if src.ca.is_zero() {
        src.at_px(x as i64, y as i64)
    } else {
        src.at_uv(u, v)
    };
    apply_touchups(src, touchups, u, v, px)
}

/// A `LinearF16` image as opaque sRGB8 RGBA, with no edits, as
/// `raw_shader.wgsl` would display it. Empty for a short buffer.
pub fn linear_f16_to_srgb8(img: &DecodedImage) -> Vec<u8> {
    let n = img.width as usize * img.height as usize;
    if img.rgba.len() < n * 8 {
        return Vec::new();
    }
    let adj = Adjustments::default();
    let f = |b: &[u8]| half::f16::from_le_bytes([b[0], b[1]]).to_f32();
    let mut out = Vec::with_capacity(n * 4);
    for px in img.rgba.as_chunks::<8>().0.iter().take(n) {
        let d = develop::apply_raw_display(&adj, [f(&px[0..]), f(&px[2..]), f(&px[4..])]);
        out.extend(d.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8));
        out.push(255);
    }
    out
}

/// Shrink opaque sRGB8 RGBA so its long side is at most `max_px`, averaging
/// each output pixel's whole source box in linear light. Averaging the whole
/// box rather than sampling it is what keeps fine detail from aliasing at the
/// 3-6x reductions an export sees, and averaging in linear keeps fine
/// light-dark texture from darkening. Returns the input untouched when it
/// already fits. Never upscales.
pub(crate) fn fit_long_edge<S: Sample>(
    w: u32,
    h: u32,
    rgba: Vec<S>,
    max_px: u32,
) -> (u32, u32, Vec<S>) {
    if w.max(h) <= max_px || max_px == 0 || rgba.len() < (w as usize * h as usize * 4) {
        return (w, h, rgba);
    }
    let scale = max_px as f64 / w.max(h) as f64;
    let dw = ((w as f64 * scale).round() as u32).clamp(1, max_px);
    let dh = ((h as f64 * scale).round() as u32).clamp(1, max_px);

    // Source span `[start, end)` covered by output index `o` of `d` over `s`.
    let span = |o: u32, d: u32, s: u32| {
        let start = (o as u64 * s as u64 / d as u64) as usize;
        let end = ((o as u64 + 1) * s as u64).div_ceil(d as u64) as usize;
        (start, end.max(start + 1).min(s as usize))
    };
    let xspans: Vec<(usize, usize)> = (0..dw).map(|x| span(x, dw, w)).collect();

    let mut out = vec![S::default(); dw as usize * dh as usize * 4];
    let opaque = S::encode(1.0);
    let to_linear = S::linear_lut();
    for yo in 0..dh {
        let (y0, y1) = span(yo, dh, h);
        for (xo, &(x0, x1)) in xspans.iter().enumerate() {
            let mut sum = [0f32; 3];
            for y in y0..y1 {
                let row = y * w as usize * 4;
                for x in x0..x1 {
                    let i = row + x * 4;
                    sum[0] += to_linear[rgba[i].index()];
                    sum[1] += to_linear[rgba[i + 1].index()];
                    sum[2] += to_linear[rgba[i + 2].index()];
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as f32;
            let d = (yo as usize * dw as usize + xo) * 4;
            out[d] = S::encode(sum[0] / n);
            out[d + 1] = S::encode(sum[1] / n);
            out[d + 2] = S::encode(sum[2] / n);
            out[d + 3] = opaque;
        }
    }
    (dw, dh, out)
}

/// Strided downsample of `img` to linear RGB with about `target` samples on
/// the long side. Returns `(grid, w, h)`, or an empty grid for a bad image.
/// The histogram and Auto Tone both use this so they see the same pixels.
#[hotpath::measure]
pub fn downsample_linear(img: &DecodedImage, target: usize) -> (Vec<[f32; 3]>, usize, usize) {
    let (w, h) = (img.width as usize, img.height as usize);
    if w == 0 || h == 0 || img.rgba.len() < w * h * img.pixel_format.bytes_per_pixel() {
        return (Vec::new(), 0, 0);
    }
    let step = (w.max(h) / target.max(1)).max(1);
    let (dw, dh) = (w.div_ceil(step), h.div_ceil(step));
    let mut grid = Vec::with_capacity(dw * dh);
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            grid.push(sample_linear(
                img,
                x as f32 / w.saturating_sub(1).max(1) as f32,
                y as f32 / h.saturating_sub(1).max(1) as f32,
            ));
            x += step;
        }
        y += step;
    }
    (grid, dw, dh)
}

/// Nearest-pixel sample at UV coordinates, as linear RGB. Handles both pixel
/// formats; the browser RAW path decodes to linear RGBA16F.
pub fn sample_linear(img: &DecodedImage, u: f32, v: f32) -> [f32; 3] {
    let x = (u.clamp(0.0, 1.0) * (img.width.saturating_sub(1)) as f32).round() as u32;
    let y = (v.clamp(0.0, 1.0) * (img.height.saturating_sub(1)) as f32).round() as u32;
    pixel_linear(img, x, y)
}

/// Pixel `(x, y)` as linear RGB, in either pixel format. The caller keeps
/// `x` and `y` inside the image.
pub(crate) fn pixel_linear(img: &DecodedImage, x: u32, y: u32) -> [f32; 3] {
    let i = (y as usize * img.width as usize + x as usize) * 4;
    match img.pixel_format {
        PixelFormat::Srgb8 => unpremul_to_linear([
            img.rgba[i],
            img.rgba[i + 1],
            img.rgba[i + 2],
            img.rgba[i + 3],
        ]),
        PixelFormat::LinearF16 => [
            half::f16::from_le_bytes([img.rgba[i * 2], img.rgba[i * 2 + 1]]).to_f32(),
            half::f16::from_le_bytes([img.rgba[i * 2 + 2], img.rgba[i * 2 + 3]]).to_f32(),
            half::f16::from_le_bytes([img.rgba[i * 2 + 4], img.rgba[i * 2 + 5]]).to_f32(),
        ],
    }
}

/// Whether any spot's circle comes within `reach` texels of source UV
/// `(u, v)`, as `apply_touchups` measures it.
fn near_a_spot(src: &Source, touchups: &[TouchUp], u: f32, v: f32, reach: f32) -> bool {
    let (w, h) = (src.img.width as f32, src.img.height as f32);
    touchups.iter().any(|t| {
        let distance = ((u - t.center[0]) * w).hypot((v - t.center[1]) * h);
        distance < t.radius * w.min(h) + reach + 1.0
    })
}

/// Blend each spot over `base`, the pixel at source UV `(u, v)`. Must match
/// the `touch_count` loop in shader.wgsl.
fn apply_touchups(src: &Source, touchups: &[TouchUp], u: f32, v: f32, base: [f32; 3]) -> [f32; 3] {
    let (w, h) = (src.img.width as f32, src.img.height as f32);
    let mut out = base;
    for t in touchups {
        let distance = ((u - t.center[0]) * w).hypot((v - t.center[1]) * h);
        let radius = t.radius * w.min(h);
        if distance >= radius {
            continue;
        }
        let feather = (radius * t.feather.clamp(0.02, 1.0)).max(1.0);
        let mask = ((radius - distance) / feather).clamp(0.0, 1.0);
        let mask = mask * mask * (3.0 - 2.0 * mask) * t.opacity;
        let patch = src.at_uv(
            t.source[0] + (u - t.center[0]),
            t.source[1] + (v - t.center[1]),
        );
        for ((o, s), d) in out.iter_mut().zip(patch).zip(t.delta) {
            let s = (s + d).clamp(0.0, 1.0);
            *o = *o * (1.0 - mask) + s * mask;
        }
    }
    out
}

/// Rotate packed RGBA8 by `steps` 90-degree clockwise turns. Returns the new
/// `(width, height, rgba)`.
#[hotpath::measure]
fn rotate_rgba<S: Sample>(src: &[S], w: u32, h: u32, steps: u8) -> (u32, u32, Vec<S>) {
    let steps = steps % 4;
    if steps == 0 {
        return (w, h, src.to_vec());
    }
    let (nw, nh) = if steps == 2 { (w, h) } else { (h, w) };
    let mut dst = vec![S::default(); (nw * nh * 4) as usize];
    let px = |x: u32, y: u32| ((y * w + x) * 4) as usize;
    for yo in 0..nh {
        for xo in 0..nw {
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

/// Bilinearly resample a packed one-channel buffer (a segmentation mask) to
/// `dw x dh`. Bilinear keeps the mask's soft edges. The `+ 0.5 ... - 0.5`
/// maps pixel centers, so the result doesn't shift half a pixel toward the
/// origin. Edges clamp.
// Only `seg_probe` uses this. The Loupe stretches masks on the GPU.
#[allow(dead_code)]
#[hotpath::measure]
pub(crate) fn resample_bilinear_u8(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
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

/// Apply an EXIF orientation (`1..=8`) to a packed one-channel mask. Returns
/// `(width, height, buffer)`. Same mapping as `image_decode`'s
/// `apply_exif_orientation`.
#[cfg(any(target_os = "macos", test))]
#[hotpath::measure]
pub(crate) fn orient_mask(src: &[u8], w: u32, h: u32, orientation: u8) -> (u32, u32, Vec<u8>) {
    if orientation <= 1 || src.len() < (w * h) as usize {
        return (w, h, src.to_vec());
    }
    let swaps = matches!(orientation, 5..=8);
    let (nw, nh) = if swaps { (h, w) } else { (w, h) };
    let mut dst = vec![0u8; (nw * nh) as usize];
    for yo in 0..nh {
        for xo in 0..nw {
            let (xs, ys) = match orientation {
                2 => (w - 1 - xo, yo),
                3 => (w - 1 - xo, h - 1 - yo),
                4 => (xo, h - 1 - yo),
                5 => (yo, xo),
                6 => (yo, h - 1 - xo),
                7 => (w - 1 - yo, h - 1 - xo),
                _ => (w - 1 - yo, xo),
            };
            dst[(yo * nw + xo) as usize] = src[(ys * w + xs) as usize];
        }
    }
    (nw, nh, dst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::image_decode::DecodedImageFields;
    use crate::develop::Crop;

    /// The largest red-green and blue-green gap, in sRGB8 steps, over the
    /// outer part of an opaque `w x h` gray chart.
    fn worst_fringe(rgba: &[u8], w: u32, h: u32) -> u8 {
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let mut worst = 0;
        for y in 4..h - 4 {
            for x in 4..w - 4 {
                if (x as f32 - cx).hypot(y as f32 - cy) < 0.6 * cx.hypot(cy) {
                    continue;
                }
                let i = ((y * w + x) * 4) as usize;
                worst = worst
                    .max(rgba[i].abs_diff(rgba[i + 1]))
                    .max(rgba[i + 2].abs_diff(rgba[i + 1]));
            }
        }
        worst
    }

    #[test]
    fn baking_the_measured_scales_lines_up_the_channels() {
        let truth = develop::CaScale {
            red: 0.002,
            blue: -0.0015,
        };
        let img = crate::develop::chroma::fixture::photo(1200, 800, truth);
        let before = worst_fringe(&img.rgba, 1200, 800);
        let adj = Adjustments {
            chromatic_aberration: Some(crate::develop::chroma::measure(&img)),
            ..Adjustments::default()
        };
        let (w, h, out) = bake_edited(&img, &adj, &[], &[], 0);
        assert_eq!((w, h), (1200, 800));
        let after = worst_fringe(&out, w, h);
        assert!(before > 20, "the fixture should fringe: {before}");
        assert!(after <= 3, "fringe {before} -> {after}");
    }

    /// The scales come from the sidecar, and serde reads `1e39` as an f32
    /// infinity. The neighbor index must not overflow on either.
    #[test]
    fn a_sidecar_with_runaway_scales_bakes_without_overflow() {
        let img = crate::develop::chroma::fixture::photo(9, 7, develop::CaScale::default());
        for json in [
            r#"{"chromatic_aberration": {"red": 1e30, "blue": -1e30}}"#,
            r#"{"chromatic_aberration": {"red": 1e39, "blue": -1e39}}"#,
        ] {
            let adj: Adjustments = serde_json::from_str(json).unwrap();
            let (w, h, out) = bake_edited(&img, &adj, &[], &[], 0);
            assert_eq!((w, h, out.len()), (9, 7, 9 * 7 * 4));
        }
    }

    #[test]
    fn a_mask_brightens_only_where_it_reaches_and_follows_the_source_under_a_crop() {
        let img = crate::develop::chroma::fixture::photo(60, 40, develop::CaScale::default());
        let mut left = Mask::linear([0.2, 0.5], [0.4, 0.5]);
        left.amounts.exposure = 1.0;
        let plain = bake_edited(&img, &Adjustments::default(), &[], &[], 0).2;
        let masked = bake_edited(&img, &Adjustments::default(), &[], &[left], 0).2;
        let px = |buf: &[u8], x: usize, y: usize| buf[(y * 60 + x) * 4 + 1];
        for y in [5, 20, 35] {
            assert!(
                px(&masked, 3, y) > px(&plain, 3, y),
                "left of start, row {y}"
            );
            assert_eq!(px(&masked, 50, y), px(&plain, 50, y), "past end, row {y}");
        }
        // A crop of the right half leaves the mask on the source's left.
        let cropped = Adjustments {
            crop: Some(Crop {
                left: 0.5,
                top: 0.0,
                right: 1.0,
                bottom: 1.0,
            }),
            ..Adjustments::default()
        };
        assert_eq!(
            bake_edited(&img, &cropped, &[], &[left], 0),
            bake_edited(&img, &cropped, &[], &[], 0)
        );
    }

    #[test]
    fn zero_scales_bake_byte_identical_to_no_correction() {
        let img = crate::develop::chroma::fixture::photo(60, 40, develop::CaScale::default());
        let off = bake_edited(&img, &Adjustments::default(), &[], &[], 0);
        let zero = Adjustments {
            chromatic_aberration: Some(develop::CaScale::default()),
            ..Adjustments::default()
        };
        assert_eq!(bake_edited(&img, &zero, &[], &[], 0), off);
    }

    #[test]
    fn fit_long_edge_scales_the_long_side_and_keeps_the_aspect() {
        let (w, h) = (400, 300);
        let (dw, dh, out) = fit_long_edge(w, h, vec![128u8; (w * h * 4) as usize], 205);
        assert_eq!((dw, dh), (205, 154));
        assert_eq!(out.len(), (dw * dh * 4) as usize);
        assert!(
            out.chunks(4).all(|p| p == [128, 128, 128, 255]),
            "flat gray stays flat"
        );
    }

    #[test]
    fn fit_long_edge_never_upscales() {
        let rgba = vec![7u8; 30 * 20 * 4];
        let (w, h, out) = fit_long_edge(30, 20, rgba.clone(), 2048);
        assert_eq!((w, h), (30, 20));
        assert_eq!(out, rgba);
    }

    /// A one-pixel black/white checkerboard halved is 50% light, which is
    /// about 188 in sRGB, not the 128 a gamma-space average would give.
    #[test]
    fn fit_long_edge_averages_in_linear_light() {
        let (w, h) = (64u32, 64u32);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let v: u8 = if (x + y) % 2 == 0 { 255 } else { 0 };
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let (_, _, out) = fit_long_edge(w, h, rgba, 32);
        assert!(
            out.chunks(4).all(|p| (184..=188).contains(&p[0])),
            "got {}",
            out[0]
        );
    }

    fn px(v: u8) -> [u8; 4] {
        [v, v, v, 255]
    }

    /// A decode that claims more pixels than its buffer holds bakes to
    /// nothing and samples as nothing, instead of reading past the end.
    #[test]
    fn a_truncated_image_bakes_to_nothing() {
        for (format, bytes) in [(PixelFormat::Srgb8, 4), (PixelFormat::LinearF16, 8)] {
            let img = DecodedImage::new_tracked(DecodedImageFields {
                width: 4,
                height: 3,
                rgba: vec![0; 4 * 3 * bytes - 1],
                pixel_format: format,
            });
            assert_eq!(
                bake_edited(&img, &Adjustments::default(), &[], &[], 0),
                (0, 0, vec![])
            );
            assert!(bilinear_sampler(&img, develop::CaScale::default()).is_none());
        }
    }

    /// Linear half-float pixels bake through the same sRGB encode as the
    /// Loupe's `linear_to_srgb`.
    #[test]
    fn a_linear_f16_image_bakes_to_its_srgb8_value() {
        let v = half::f16::from_f32(0.18).to_le_bytes();
        let one = half::f16::ONE.to_le_bytes();
        let px: Vec<u8> = [v, v, v, one].concat();
        let img = DecodedImage::new_tracked(DecodedImageFields {
            width: 2,
            height: 2,
            rgba: px.repeat(4),
            pixel_format: PixelFormat::LinearF16,
        });
        let (w, h, out) = bake_edited(&img, &Adjustments::default(), &[], &[], 0);
        assert_eq!((w, h), (2, 2));
        let want = linear_to_srgb8(half::f16::from_f32(0.18).to_f32());
        assert_eq!(out, [want, want, want, 255].repeat(4));
    }

    #[test]
    fn rotate_90cw_swaps_dims_and_moves_pixels() {
        let src = [px(10), px(20)].concat();
        let (w, h, out) = rotate_rgba(&src, 2, 1, 1);
        assert_eq!((w, h), (1, 2));
        assert_eq!(&out[0..4], &px(10));
        assert_eq!(&out[4..8], &px(20));
    }

    #[test]
    fn rotate_360_is_identity() {
        let src = [px(1), px(2), px(3), px(4)].concat();
        let (w, h, out) = rotate_rgba(&src, 2, 2, 4);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, src);
    }

    #[test]
    fn identity_bake_preserves_opaque_pixels() {
        // Opaque pixels survive the sRGB to linear round trip exactly.
        let src = [px(0), px(64), px(128), px(255)].concat();
        let img = DecodedImage::new_tracked(DecodedImageFields {
            width: 2,
            height: 2,
            rgba: src.clone(),
            pixel_format: PixelFormat::Srgb8,
        });
        let (w, h, out) = bake_edited(&img, &Adjustments::default(), &[], &[], 0);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, src);
    }

    #[test]
    fn bake_crop_slices_to_the_crop_rect() {
        let src = [px(1), px(2), px(3), px(4)].concat();
        let img = DecodedImage::new_tracked(DecodedImageFields {
            width: 4,
            height: 1,
            rgba: src,
            pixel_format: PixelFormat::Srgb8,
        });
        let adj = Adjustments {
            crop: Some(Crop {
                left: 0.5,
                top: 0.0,
                right: 1.0,
                bottom: 1.0,
            }),
            ..Default::default()
        };
        let (w, h, out) = bake_edited(&img, &adj, &[], &[], 0);
        assert_eq!((w, h), (2, 1));
        assert_eq!(&out[0..4], &px(3));
        assert_eq!(&out[4..8], &px(4));
    }

    #[test]
    fn a_quarter_straighten_bakes_as_a_quarter_turn() {
        let src: Vec<u8> = (0..9u8).flat_map(|i| px(i * 25)).collect();
        let img = DecodedImage::new_tracked(DecodedImageFields {
            width: 3,
            height: 3,
            rgba: src,
            pixel_format: PixelFormat::Srgb8,
        });
        let adj = Adjustments {
            straighten: 90.0,
            ..Default::default()
        };
        // A positive angle shows the photo turned anti-clockwise.
        assert_eq!(
            bake_edited(&img, &adj, &[], &[], 0),
            bake_edited(&img, &Adjustments::default(), &[], &[], 3)
        );
    }

    #[test]
    fn bake_denoise_zero_matches_identity_bake() {
        let src = [px(0), px(64), px(128), px(255)].concat();
        let img = DecodedImage::new_tracked(DecodedImageFields {
            width: 2,
            height: 2,
            rgba: src.clone(),
            pixel_format: PixelFormat::Srgb8,
        });
        let adj = Adjustments {
            denoise: 0.0,
            ..Default::default()
        };
        let (w, h, out) = bake_edited(&img, &adj, &[], &[], 0);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, src);
    }

    #[test]
    fn bake_denoise_changes_output() {
        let mut src = vec![0u8; 3 * 3 * 4];
        for i in 0..9 {
            let v = if i == 4 { 255 } else { 0 };
            src[i * 4..i * 4 + 4].copy_from_slice(&px(v));
        }
        let img = DecodedImage::new_tracked(DecodedImageFields {
            width: 3,
            height: 3,
            rgba: src,
            pixel_format: PixelFormat::Srgb8,
        });
        let (_, _, out0) = bake_edited(&img, &Adjustments::default(), &[], &[], 0);
        let denoised = Adjustments {
            denoise: 100.0,
            ..Default::default()
        };
        let (_, _, out100) = bake_edited(&img, &denoised, &[], &[], 0);
        let center = 4 * 4;
        assert_eq!(out0[center], 255);
        assert!(
            out100[center] < 255,
            "expected denoise to darken the outlier center pixel"
        );
    }

    #[test]
    fn bake_touchup_replaces_a_soft_spot_from_source_region() {
        let mut src = vec![0u8; 5 * 4];
        for x in 0..5 {
            let value = if x == 2 { 255 } else { 0 };
            src[x * 4..x * 4 + 4].copy_from_slice(&px(value));
        }
        let img = DecodedImage::new_tracked(DecodedImageFields {
            width: 5,
            height: 1,
            rgba: src,
            pixel_format: PixelFormat::Srgb8,
        });
        // Centered on the bright pixel, one pixel in radius, copying the
        // first pixel's black.
        let touchup = TouchUp {
            center: [0.5, 0.5],
            radius: 1.0,
            source: [0.1, 0.5],
            feather: 0.5,
            delta: [0.0; 3],
            opacity: 1.0,
        };
        let (_, _, out) = bake_edited(&img, &Adjustments::default(), &[touchup], &[], 0);
        assert!(out[2 * 4] < 255, "touch-up should reduce the bright spot");

        let at = |opacity: f32| {
            let t = TouchUp { opacity, ..touchup };
            bake_edited(&img, &Adjustments::default(), &[t], &[], 0).2[2 * 4]
        };
        assert_eq!(at(0.0), 255, "a transparent spot leaves the photo alone");
        assert!(
            at(0.5) > out[2 * 4] && at(0.5) < 255,
            "half opacity lands between the photo and the full fix"
        );
    }

    #[test]
    fn a_spot_saved_without_opacity_reads_as_opaque_and_a_partial_one_hashes_apart() {
        let json =
            r#"{"center":[0.5,0.5],"radius":0.1,"source":[0.2,0.2],"feather":0.5,"delta":[0,0,0]}"#;
        let t: TouchUp = serde_json::from_str(json).unwrap();
        assert_eq!(t.opacity, 1.0);
        assert!(
            !serde_json::to_string(&t).unwrap().contains("opacity"),
            "an opaque spot writes no opacity"
        );
        let adj = Adjustments::default();
        let sig = |t: TouchUp| crate::develop::edit_signature_with_touchups(&adj, &[t], &[], 0);
        assert_ne!(sig(t), sig(TouchUp { opacity: 0.5, ..t }));
    }

    #[test]
    fn bake_denoise_clamps_at_edges() {
        // Corner pixels read neighbors outside the image; that must not panic.
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
        let img = DecodedImage::new_tracked(DecodedImageFields {
            width: 3,
            height: 3,
            rgba: src,
            pixel_format: PixelFormat::Srgb8,
        });
        let adj = Adjustments {
            denoise: 50.0,
            ..Default::default()
        };
        let (w, h, out) = bake_edited(&img, &adj, &[], &[], 0);
        assert_eq!((w, h), (3, 3));
        assert_eq!(out.len(), 3 * 3 * 4);
    }

    #[test]
    fn unpremul_opaque_is_linear_of_srgb() {
        let out = unpremul_to_linear([255, 0, 128, 255]);
        assert!((out[0] - 1.0).abs() < 1e-6);
        assert!((out[1] - 0.0).abs() < 1e-6);
        assert!((out[2] - srgb_decode(128.0 / 255.0)).abs() < 1e-6);
    }

    #[test]
    fn the_srgb8_lookups_match_the_srgb_curve_bit_for_bit() {
        for c in 0..=255u8 {
            let lin = unpremul_to_linear([c, c, c, 255])[0];
            assert_eq!(
                lin.to_bits(),
                srgb_decode(c as f32 / 255.0).to_bits(),
                "decode {c}"
            );
        }
        // Every float from just below zero to past one, stepped finely enough
        // to land on both sides of every rounding boundary, plus each
        // boundary's neighbours.
        let mut probes: Vec<f32> = (-1000..=1_100_000)
            .map(|i| i as f32 / 1_000_000.0)
            .collect();
        for k in 0..=255u32 {
            let edge = srgb_decode(((k as f32 - 0.5) / 255.0).max(0.0));
            let bits = edge.to_bits();
            probes.extend((bits.saturating_sub(4)..bits + 4).map(f32::from_bits));
        }
        probes.extend([f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -0.0]);
        for v in probes {
            assert_eq!(linear_to_srgb8(v), encode_srgb8_curve(v), "encode {v:e}");
        }
    }

    #[test]
    fn unpremul_zero_alpha_is_black() {
        assert_eq!(unpremul_to_linear([200, 100, 50, 0]), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn resampling_to_the_same_size_is_the_identity() {
        let src = vec![0u8, 40, 90, 255];
        assert_eq!(resample_bilinear_u8(&src, 2, 2, 2, 2), src);
    }

    #[test]
    fn resampling_a_ramp_interpolates_between_the_two_ends() {
        // Outer samples clamp to the ends; inner ones land at 1/4 and 3/4.
        let out = resample_bilinear_u8(&[0, 255], 2, 1, 4, 1);
        assert_eq!(out, vec![0, 64, 191, 255]);
    }

    #[test]
    fn resampling_stays_centered_rather_than_drifting_to_the_origin() {
        // A naive `x * sw / dw` mapping shifts the result and breaks symmetry.
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
    fn orienting_a_mask_matches_the_exif_table() {
        let src = vec![1, 2, 3, 4, 5, 6];

        assert_eq!(orient_mask(&src, 2, 3, 1), (2, 3, src.clone()));
        assert_eq!(
            orient_mask(&src, 2, 3, 3),
            (2, 3, vec![6, 5, 4, 3, 2, 1]),
            "180° rotation reverses the buffer"
        );
        assert_eq!(
            orient_mask(&src, 2, 3, 2),
            (2, 3, vec![2, 1, 4, 3, 6, 5]),
            "horizontal mirror reverses each row"
        );

        let (w, h, rot) = orient_mask(&src, 2, 3, 6);
        assert_eq!((w, h), (3, 2));
        assert_eq!(rot, vec![5, 3, 1, 6, 4, 2]);
    }

    #[test]
    fn orienting_a_mask_is_reversible_through_its_inverse() {
        let src: Vec<u8> = (0..12).collect();
        // Orientations 6 and 8 undo each other.
        let (w, h, once) = orient_mask(&src, 4, 3, 6);
        let (w2, h2, back) = orient_mask(&once, w, h, 8);
        assert_eq!((w2, h2), (4, 3));
        assert_eq!(back, src);
    }

    #[test]
    fn degenerate_resample_requests_produce_nothing() {
        assert!(resample_bilinear_u8(&[1, 2, 3, 4], 2, 2, 0, 4).is_empty());
        assert!(resample_bilinear_u8(&[1, 2, 3, 4], 0, 2, 4, 4).is_empty());
        // Buffer shorter than its declared size.
        assert!(resample_bilinear_u8(&[1, 2], 4, 4, 8, 8).is_empty());
    }
}
