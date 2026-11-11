// SPDX-License-Identifier: MIT OR Apache-2.0

//! The pixels a generative fill sends and gets back. A painted area becomes
//! a padded rect of the photo, cropped and encoded as display sRGB with the
//! area cut out of its alpha, which is what a generator's image loader reads
//! as "fill here". Its reply, at whatever size, becomes a [`Patch`] at the
//! crop's own size whose alpha is the painted area's feathered coverage.
//!
//! No network here: the app's worker carries the bytes.

use std::sync::Arc;

use super::coverage::Coverage;
use super::fill::{to_comfy, Patch, MAX_PATCH_SIDE};
use super::image_ops;
use crate::decode::image_decode::DecodedImage;

/// The longest side of the image a generator is sent. SDXL's working size.
pub const UPLOAD_SIDE: u32 = 1024;
/// A generator's latent is an eighth of the image, so each side sent is a
/// multiple of this.
const UPLOAD_STEP: u32 = 8;
/// The largest side accepted of a generator's reply, before it is scaled to
/// the crop.
const MAX_RESULT_SIDE: u32 = 4096;
/// The rect sent reaches this share of the painted area's longer side past
/// it on every edge, so the generator sees what surrounds the hole.
const CONTEXT: f32 = 0.5;
/// The least context on each edge, as a share of the photo's shorter side.
const MIN_CONTEXT: f32 = 0.04;

/// A painted area: a coverage plane and the source UV rect it spans. A
/// brush stroke's raster spans the photo; a finished patch's alpha spans
/// its fill's rect.
#[derive(Clone)]
pub struct Painted {
    coverage: Arc<Coverage>,
    rect: [f32; 4],
}

impl Painted {
    /// A raster over the whole photo, as the brush makes.
    pub fn whole(coverage: Arc<Coverage>) -> Painted {
        Painted {
            coverage,
            rect: [0.0, 0.0, 1.0, 1.0],
        }
    }

    /// The area `patch` covers inside `bbox`, so a fill can be made again
    /// from its own patch. `None` for a patch whose alpha can't be read.
    pub fn of_patch(patch: &Patch, bbox: [f32; 4]) -> Option<Painted> {
        let alpha: Vec<u8> = patch.rgba().iter().skip(3).step_by(4).copied().collect();
        let coverage = Coverage::fit(patch.width(), patch.height(), &alpha).ok()?;
        Some(Painted {
            coverage: Arc::new(coverage),
            rect: bbox,
        })
    }

    pub fn coverage(&self) -> &Arc<Coverage> {
        &self.coverage
    }

    /// Coverage at source `(u, v)`, 0 outside the rect.
    pub fn at(&self, u: f32, v: f32) -> f32 {
        let [u0, v0, u1, v1] = self.rect;
        let (w, h) = (u1 - u0, v1 - v0);
        if !(w > 0.0 && h > 0.0) || u < u0 || u > u1 || v < v0 || v > v1 {
            return 0.0;
        }
        self.coverage.at_uv([(u - u0) / w, (v - v0) / h])
    }

    /// The source UV rect to send: the painted texels' bounds with context
    /// around them, inside the photo. `None` when nothing is painted.
    pub fn padded_bbox(&self) -> Option<[f32; 4]> {
        let (w, h) = (
            self.coverage.width() as usize,
            self.coverage.height() as usize,
        );
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0usize, 0usize);
        for (y, row) in self.coverage.alpha().chunks_exact(w).enumerate() {
            let Some(first) = row.iter().position(|&a| a > 0) else {
                continue;
            };
            let last = row.iter().rposition(|&a| a > 0).unwrap_or(first);
            x0 = x0.min(first);
            x1 = x1.max(last + 1);
            y0 = y0.min(y);
            y1 = y1.max(y + 1);
        }
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        let [r0, s0, r1, s1] = self.rect;
        let (rw, rh) = (r1 - r0, s1 - s0);
        let u = |x: usize| r0 + rw * x as f32 / w as f32;
        let v = |y: usize| s0 + rh * y as f32 / h as f32;
        let (u0, v0, u1, v1) = (u(x0), v(y0), u(x1), v(y1));
        // A whole-photo raster's texels are square in the photo, so its
        // sides give the photo's shape and the pad is the same length along
        // both axes.
        let (photo_w, photo_h) = (
            w as f32 / rw.max(f32::EPSILON),
            h as f32 / rh.max(f32::EPSILON),
        );
        let short = photo_w.min(photo_h);
        let long_px = ((u1 - u0) * photo_w).max((v1 - v0) * photo_h);
        let pad_px = (long_px * CONTEXT).max(short * MIN_CONTEXT);
        let (pad_u, pad_v) = (pad_px / photo_w, pad_px / photo_h);
        let bbox = [
            (u0 - pad_u).max(0.0),
            (v0 - pad_v).max(0.0),
            (u1 + pad_u).min(1.0),
            (v1 + pad_v).min(1.0),
        ];
        (bbox.iter().all(|c| c.is_finite()) && bbox[0] < bbox[2] && bbox[1] < bbox[3])
            .then_some(bbox)
    }
}

/// One generation's input, and what turns its reply into a patch.
pub struct Request {
    bbox: [f32; 4],
    /// The patch's size: the crop in the decode's own pixels.
    width: u32,
    height: u32,
    /// The patch's alpha, `width * height`.
    alpha: Vec<u8>,
    upload: Vec<u8>,
}

/// `(w, h)` scaled to fit `max` on its longer side, never up, each side at
/// least 1.
fn fit(w: f32, h: f32, max: u32) -> (u32, u32) {
    let scale = (max as f32 / w.max(h)).min(1.0);
    let side = |s: f32| ((s * scale).round() as u32).clamp(1, max);
    (side(w), side(h))
}

impl Request {
    /// Crops `img` to `bbox` for a generator to fill where `painted` covers.
    /// `Err` for an image with no pixels or a rect outside the photo.
    pub fn new(
        img: &DecodedImage,
        ca: super::CaScale,
        painted: &Painted,
        bbox: [f32; 4],
    ) -> Result<Request, String> {
        let [u0, v0, u1, v1] = bbox;
        let inside = |lo: f32, hi: f32| 0.0 <= lo && lo < hi && hi <= 1.0;
        if !bbox.iter().all(|c| c.is_finite()) || !inside(u0, u1) || !inside(v0, v1) {
            return Err("the painted area is outside the photo".into());
        }
        let sample = image_ops::bilinear_sampler(img, ca)
            .ok_or("the photo has not finished loading; try again in a moment")?;
        let crop_w = (u1 - u0) * img.width as f32;
        let crop_h = (v1 - v0) * img.height as f32;
        let (width, height) = fit(crop_w, crop_h, MAX_PATCH_SIDE);
        let uv = |x: u32, y: u32, w: u32, h: u32| {
            (
                u0 + (u1 - u0) * (x as f32 + 0.5) / w as f32,
                v0 + (v1 - v0) * (y as f32 + 0.5) / h as f32,
            )
        };

        let mut alpha = Vec::with_capacity(width as usize * height as usize);
        for y in 0..height {
            for x in 0..width {
                let (u, v) = uv(x, y, width, height);
                alpha.push((painted.at(u, v) * 255.0).round().clamp(0.0, 255.0) as u8);
            }
        }
        if alpha.iter().all(|&a| a == 0) {
            return Err("nothing is painted to fill".into());
        }

        let (fit_w, fit_h) = fit(crop_w, crop_h, UPLOAD_SIDE);
        let step = |s: u32| (s.div_ceil(UPLOAD_STEP) * UPLOAD_STEP).clamp(UPLOAD_STEP, UPLOAD_SIDE);
        let (up_w, up_h) = (step(fit_w), step(fit_h));
        let mut rgba = Vec::with_capacity(up_w as usize * up_h as usize * 4);
        for y in 0..up_h {
            for x in 0..up_w {
                let (u, v) = uv(x, y, up_w, up_h);
                let [r, g, b] = to_comfy(sample(u, v), img.pixel_format);
                // Any coverage at all is filled, so every pixel the patch
                // blends in is one the generator made.
                let a = if painted.at(u, v) > 0.0 { 0 } else { 255 };
                rgba.extend_from_slice(&[r, g, b, a]);
            }
        }
        let upload = encode_rgba(up_w, up_h, &rgba)?;
        Ok(Request {
            bbox,
            width,
            height,
            alpha,
            upload,
        })
    }

    pub fn bbox(&self) -> [f32; 4] {
        self.bbox
    }

    /// The PNG to send: RGBA, transparent where the generator should fill.
    pub fn upload(&self) -> &[u8] {
        &self.upload
    }

    /// A generator's reply, a PNG of any size, as this request's patch:
    /// its colours at the crop's size under the painted area's coverage.
    pub fn patch(&self, reply: &[u8]) -> Result<Patch, String> {
        let (w, h, rgb) = decode_rgb(reply)?;
        let plane = |c: usize| -> Vec<u8> {
            let source: Vec<u8> = rgb.iter().skip(c).step_by(3).copied().collect();
            image_ops::resample_bilinear_u8(&source, w, h, self.width, self.height)
        };
        let (r, g, b) = (plane(0), plane(1), plane(2));
        let n = self.alpha.len();
        if r.len() != n || g.len() != n || b.len() != n {
            return Err("ComfyUI's image could not be scaled to the painted area".into());
        }
        let mut rgba = Vec::with_capacity(n * 4);
        for (((r, g), b), a) in r.iter().zip(&g).zip(&b).zip(&self.alpha) {
            rgba.extend_from_slice(&[*r, *g, *b, *a]);
        }
        Patch::new(self.width, self.height, rgba).map_err(|e| e.to_string())
    }
}

fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    // The server is on this machine, so the bytes cost nothing to send and
    // a fast encode keeps the wait short.
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("could not encode the image to send: {e}"))?;
    writer
        .write_image_data(rgba)
        .map_err(|e| format!("could not encode the image to send: {e}"))?;
    drop(writer);
    Ok(out)
}

/// Any PNG a generator saves, as 8-bit RGB: `(width, height, pixels)`.
fn decode_rgb(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let bad = |e: png::DecodingError| format!("ComfyUI's image could not be read: {e}");
    let side = MAX_RESULT_SIDE as usize;
    let limits = png::Limits {
        bytes: side * side * 4 * 2,
    };
    let mut decoder = png::Decoder::new_with_limits(std::io::Cursor::new(bytes), limits);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(bad)?;
    let (width, height) = (reader.info().width, reader.info().height);
    if width == 0 || height == 0 || width > MAX_RESULT_SIDE || height > MAX_RESULT_SIDE {
        return Err(format!(
            "ComfyUI's image is {width}x{height}, too large to use"
        ));
    }
    let size = reader
        .output_buffer_size()
        .filter(|&s| s <= side * side * 4)
        .ok_or("ComfyUI's image is too large to use")?;
    let mut buf = vec![0u8; size];
    let frame = reader.next_frame(&mut buf).map_err(bad)?;
    let channels = match frame.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return Err("ComfyUI's image has an unexpected format".into()),
    };
    let n = width as usize * height as usize;
    if frame.bit_depth != png::BitDepth::Eight || buf.len() < n * channels {
        return Err("ComfyUI's image has an unexpected format".into());
    }
    let mut rgb = Vec::with_capacity(n * 3);
    for px in buf.chunks_exact(channels).take(n) {
        match *px {
            [l] | [l, _] => rgb.extend_from_slice(&[l, l, l]),
            [r, g, b] | [r, g, b, _] => rgb.extend_from_slice(&[r, g, b]),
            _ => {}
        }
    }
    Ok((width, height, rgb))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::image_decode::{DecodedImageFields, PixelFormat};

    fn photo(width: u32, height: u32, value: u8) -> DecodedImage {
        DecodedImage::new_tracked(DecodedImageFields {
            width,
            height,
            rgba: [value, value, value, 255].repeat(width as usize * height as usize),
            pixel_format: PixelFormat::Srgb8,
        })
    }

    /// A 100x50 raster painted solid over x 40..60, y 20..30.
    fn painted() -> Painted {
        let (w, h) = (100usize, 50usize);
        let mut alpha = vec![0u8; w * h];
        for y in 20..30 {
            for x in 40..60 {
                alpha[y * w + x] = 255;
            }
        }
        Painted::whole(Arc::new(Coverage::fit(w as u32, h as u32, &alpha).unwrap()))
    }

    fn decode_rgba(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut reader = png::Decoder::new(std::io::Cursor::new(png_bytes))
            .read_info()
            .unwrap();
        let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!(info.color_type, png::ColorType::Rgba);
        (info.width, info.height, buf)
    }

    fn solid_png(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&rgb.repeat(width as usize * height as usize))
            .unwrap();
        drop(writer);
        out
    }

    #[test]
    fn the_rect_sent_pads_the_painted_area_evenly_and_stays_in_the_photo() {
        let bbox = painted().padded_bbox().unwrap();
        // 20 texels wide, so 10 of context on each edge, in both axes.
        let want = [0.30, 0.20, 0.70, 0.80];
        for (got, want) in bbox.iter().zip(want) {
            assert!((got - want).abs() < 1e-4, "{bbox:?}");
        }

        let mut corner = vec![0u8; 100 * 50];
        corner[0] = 255;
        let corner = Painted::whole(Arc::new(Coverage::fit(100, 50, &corner).unwrap()));
        let [u0, v0, u1, v1] = corner.padded_bbox().unwrap();
        assert_eq!((u0, v0), (0.0, 0.0));
        assert!(u1 > 0.01 && u1 < 0.1 && v1 > 0.02 && v1 < 0.2);

        let empty = Painted::whole(Arc::new(Coverage::nothing()));
        assert_eq!(empty.padded_bbox(), None);
    }

    #[test]
    fn the_upload_is_the_crop_with_the_painted_area_cut_from_its_alpha() {
        let img = photo(400, 200, 90);
        let painted = painted();
        let bbox = painted.padded_bbox().unwrap();
        let request = Request::new(&img, Default::default(), &painted, bbox).unwrap();
        let (w, h, rgba) = decode_rgba(request.upload());
        // 160x120 source pixels, already a multiple of 8.
        assert_eq!((w, h), (160, 120));
        let at = |x: u32, y: u32| {
            let i = (y * w + x) as usize * 4;
            [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
        };
        assert_eq!(at(2, 2), [90, 90, 90, 255], "context keeps the photo");
        assert_eq!(at(80, 60), [90, 90, 90, 0], "the painted middle is cut out");
    }

    #[test]
    fn a_large_crop_is_sent_at_the_working_size_and_comes_back_at_its_own() {
        let img = photo(4000, 2000, 90);
        let painted = Painted::whole(Arc::new(Coverage::fit(2, 1, &[255, 255]).unwrap()));
        let request =
            Request::new(&img, Default::default(), &painted, [0.0, 0.0, 1.0, 1.0]).unwrap();
        let (w, h, _) = decode_rgba(request.upload());
        assert_eq!((w, h), (1024, 512));

        let patch = request.patch(&solid_png(1024, 512, [10, 200, 30])).unwrap();
        assert_eq!((patch.width(), patch.height()), (2048, 1024), "capped");
        assert_eq!(patch.rgba().get(..4), Some(&[10, 200, 30, 255][..]));
    }

    #[test]
    fn the_patch_takes_the_replys_colour_under_the_feathered_coverage() {
        let img = photo(400, 200, 90);
        let painted = painted();
        let bbox = painted.padded_bbox().unwrap();
        let request = Request::new(&img, Default::default(), &painted, bbox).unwrap();
        let patch = request.patch(&solid_png(64, 64, [200, 10, 20])).unwrap();
        assert_eq!((patch.width(), patch.height()), (160, 120));
        let at = |x: usize, y: usize| {
            let i = (y * 160 + x) * 4;
            patch.rgba()[i..i + 4].to_vec()
        };
        assert_eq!(at(80, 60), [200, 10, 20, 255]);
        assert_eq!(
            at(2, 2),
            [200, 10, 20, 0],
            "outside the paint shows the photo"
        );

        // Made again from its own patch, the fill covers the same area.
        let again = Painted::of_patch(&patch, bbox).unwrap();
        assert!(again.at(0.5, 0.5) > 0.99);
        assert_eq!(again.at(0.32, 0.25), 0.0);
        assert_eq!(again.at(0.1, 0.1), 0.0, "outside its rect");
    }

    #[test]
    fn hostile_input_is_an_error_not_a_panic() {
        let img = photo(400, 200, 90);
        let painted = painted();
        let bbox = painted.padded_bbox().unwrap();
        let request = Request::new(&img, Default::default(), &painted, bbox).unwrap();
        let good = solid_png(64, 64, [1, 2, 3]);
        assert!(request.patch(&good[..good.len() / 2]).is_err(), "truncated");
        assert!(request.patch(b"not a png").is_err());
        assert!(request.patch(&[]).is_err());

        let mut short = photo(400, 200, 0);
        short.rgba.truncate(16);
        assert!(Request::new(&short, Default::default(), &painted, bbox).is_err());
        for bad in [
            [0.5, 0.5, 0.5, 0.9],
            [-0.1, 0.0, 0.5, 0.5],
            [0.0, 0.0, f32::NAN, 1.0],
            [0.0, 0.0, 1.5, 1.0],
        ] {
            assert!(Request::new(&img, Default::default(), &painted, bad).is_err());
        }
        // A rect the paint doesn't touch has nothing to fill.
        assert!(Request::new(&img, Default::default(), &painted, [0.0, 0.0, 0.2, 0.2]).is_err());
    }
}
