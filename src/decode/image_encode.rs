// SPDX-License-Identifier: MIT OR Apache-2.0

//! Encode baked pixels. JPEG serves export and the on-disk thumbnail cache,
//! each at its own [`JpegQuality`]; macOS writes it and PNG through ImageIO,
//! other targets through `mozjpeg-rs` and the `image` crate. Exports also
//! write 16-bit TIFF through `kamadak-exif`'s TIFF writer, lossless WebP
//! through `image-webp` and AVIF through `ravif`, each with an EXIF block.

#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;

#[cfg(target_os = "macos")]
use std::ffi::c_void;

#[cfg(target_os = "macos")]
use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionary, CFMutableData,
    CFNumber, CFNumberType, CFRetained, CFString,
};
#[cfg(target_os = "macos")]
use objc2_image_io::{kCGImageDestinationLossyCompressionQuality, CGImageDestination};

#[cfg(target_os = "macos")]
use crate::decode::coregraphics;

/// What a JPEG is for, which sets its quality. The cache holds one entry per
/// photo for as long as the photo exists, so it trades quality for disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JpegQuality {
    Export,
    Thumbnail,
}

/// Encode `rgba` (tightly packed RGBA8, row-major, sRGB, alpha opaque or
/// premultiplied) to a JPEG at `out`.
#[cfg(target_os = "macos")]
#[hotpath::measure]
pub fn encode_jpeg(
    out: &Path,
    width: u32,
    height: u32,
    rgba: &[u8],
    quality: JpegQuality,
) -> Result<(), String> {
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
    // SAFETY: url and type are valid.
    let dest = unsafe { CGImageDestination::with_url(&url, &jpeg_uti, 1, None) }
        .ok_or("could not create image destination (unwritable path?)")?;

    // Export passes no properties, so ImageIO uses its default quality and
    // export output stays what it has always been.
    let properties = match quality {
        JpegQuality::Export => None,
        JpegQuality::Thumbnail => Some(lossy_quality_properties(0.6)?),
    };
    // SAFETY: dest/image are valid; properties, if any, map a documented
    // destination key to a CFNumber.
    unsafe { CGImageDestination::add_image(&dest, &image, properties.as_deref()) };
    // SAFETY: dest is valid; returns false if the file could not be written.
    let ok = unsafe { CGImageDestination::finalize(&dest) };
    if !ok {
        return Err("CGImageDestinationFinalize failed (could not write file)".into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn lossy_quality_properties(quality: f64) -> Result<CFRetained<CFDictionary>, String> {
    // SAFETY: value_ptr points at a valid f64 matching Float64Type.
    let number = unsafe {
        CFNumber::new(
            None,
            CFNumberType::Float64Type,
            &quality as *const f64 as *const c_void,
        )
    }
    .ok_or("could not create CFNumber for JPEG quality")?;
    // SAFETY: the static is a valid CFString key at runtime.
    let mut keys: [*const c_void; 1] = [
        unsafe { kCGImageDestinationLossyCompressionQuality } as *const CFString as *const c_void
    ];
    let mut values: [*const c_void; 1] = [&*number as *const CFNumber as *const c_void];
    // SAFETY: keys and values hold 1 valid CFType pointer each. The CFType
    // callbacks retain entries, so `number` may drop afterwards.
    unsafe {
        CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            keys.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }
    .ok_or_else(|| "could not create JPEG properties dictionary".into())
}

/// Encode `rgba` (same pixel contract as [`encode_jpeg`]) to JPEG bytes with
/// `mozjpeg-rs`. The wasm32 export path uses this because it has no `std::fs`.
///
/// Export is quality 90, not mozjpeg's default 75, to stay close to ImageIO's
/// default on macOS.
#[cfg(not(target_os = "macos"))]
#[hotpath::measure]
pub fn encode_jpeg_to_vec(
    width: u32,
    height: u32,
    rgba: &[u8],
    quality: JpegQuality,
) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err("cannot encode a zero-sized image".into());
    }
    let bytes_per_row = width as usize * 4;
    if rgba.len() < bytes_per_row * height as usize {
        return Err("pixel buffer too small for the given dimensions".into());
    }

    let quality = match quality {
        JpegQuality::Export => EXPORT_QUALITY,
        JpegQuality::Thumbnail => 60,
    };
    mozjpeg_rs::Encoder::new(mozjpeg_rs::Preset::default())
        .quality(quality)
        .encode_rgba(rgba, width, height)
        .map_err(|e| e.to_string())
}

/// Encode `rgba` to a JPEG file at `out` with `mozjpeg-rs`.
#[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
#[hotpath::measure]
pub fn encode_jpeg(
    out: &Path,
    width: u32,
    height: u32,
    rgba: &[u8],
    quality: JpegQuality,
) -> Result<(), String> {
    let jpeg_data = encode_jpeg_to_vec(width, height, rgba, quality)?;
    crate::paths::write_atomic(out, &jpeg_data, None).map_err(|e| e.to_string())
}

/// Baked pixels to encode: tightly packed sRGB RGBA, row-major, opaque.
#[derive(Clone, Copy)]
pub enum Pixels<'a> {
    Rgba8(&'a [u8]),
    Rgba16(&'a [u16]),
}

impl Pixels<'_> {
    fn check(self, width: u32, height: u32) -> Result<(), String> {
        if width == 0 || height == 0 {
            return Err("cannot encode a zero-sized image".into());
        }
        let need = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or("image too large to encode")?;
        let have = match self {
            Pixels::Rgba8(p) => p.len(),
            Pixels::Rgba16(p) => p.len(),
        };
        if have < need {
            return Err("pixel buffer too small for the given dimensions".into());
        }
        Ok(())
    }

    /// RGBA at 8 bits per channel; a 16-bit sample keeps its high byte.
    #[cfg(target_os = "macos")]
    fn rgbx8(self, width: u32, height: u32) -> Vec<u8> {
        let n = width as usize * height as usize * 4;
        match self {
            Pixels::Rgba8(p) => p.iter().take(n).copied().collect(),
            Pixels::Rgba16(p) => p.iter().take(n).map(|c| (c >> 8) as u8).collect(),
        }
    }

    /// RGB without the alpha byte, 8 bits per channel; a 16-bit sample keeps
    /// its high byte.
    fn rgb8(self, width: u32, height: u32) -> Vec<u8> {
        let n = width as usize * height as usize;
        match self {
            Pixels::Rgba8(p) => p
                .as_chunks::<4>()
                .0
                .iter()
                .take(n)
                .flat_map(|&[r, g, b, _]| [r, g, b])
                .collect(),
            Pixels::Rgba16(p) => p
                .as_chunks::<4>()
                .0
                .iter()
                .take(n)
                .flat_map(|&[r, g, b, _]| [r, g, b].map(|c| (c >> 8) as u8))
                .collect(),
        }
    }
}

/// How hard mozjpeg works on an export JPEG, fastest first. macOS writes
/// JPEG through ImageIO, which offers no such choice and ignores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum JpegPreset {
    /// Baseline, no trellis or optimized Huffman tables: 4-10x faster than
    /// the others, files 10-20% larger.
    Fastest,
    /// Baseline with trellis quantization and optimized Huffman tables.
    Baseline,
    /// `Baseline`'s optimizations in a progressive file.
    #[default]
    Progressive,
    /// `Progressive` plus a search over scan layouts: about twice its time
    /// for about 1% smaller files.
    Smallest,
}

impl JpegPreset {
    pub const ALL: [JpegPreset; 4] = [
        JpegPreset::Fastest,
        JpegPreset::Baseline,
        JpegPreset::Progressive,
        JpegPreset::Smallest,
    ];

    #[cfg(not(target_os = "macos"))]
    fn mozjpeg(self) -> mozjpeg_rs::Preset {
        match self {
            JpegPreset::Fastest => mozjpeg_rs::Preset::BaselineFastest,
            JpegPreset::Baseline => mozjpeg_rs::Preset::BaselineBalanced,
            JpegPreset::Progressive => mozjpeg_rs::Preset::ProgressiveBalanced,
            JpegPreset::Smallest => mozjpeg_rs::Preset::ProgressiveSmallest,
        }
    }
}

/// An export JPEG's settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct JpegOptions {
    /// Quality 1 to 100, or `None` for the export default.
    pub quality: Option<u8>,
    pub preset: JpegPreset,
}

/// A file format to encode an export as. Which ones a build can write is
/// [`can_encode`]'s answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Jpeg(JpegOptions),
    Png,
    /// 16 bits per channel when given [`Pixels::Rgba16`].
    Tiff,
    /// Lossless.
    Webp,
    Avif,
}

/// Whether this build, on this machine, can write `enc`. macOS asks
/// ImageIO, which writes AVIF only on recent systems.
pub fn can_encode(enc: Encoding) -> bool {
    match enc {
        Encoding::Jpeg(_) | Encoding::Png | Encoding::Tiff | Encoding::Webp => true,
        #[cfg(target_os = "macos")]
        Encoding::Avif => {
            static AVIF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *AVIF.get_or_init(|| imageio_writes("public.avif"))
        }
        #[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
        Encoding::Avif => true,
        // rav1e on a browser's threads takes minutes per frame.
        #[cfg(target_arch = "wasm32")]
        Encoding::Avif => false,
    }
}

#[cfg(target_os = "macos")]
fn imageio_writes(uti: &str) -> bool {
    // SAFETY: returns a +1 CFArray of CFStrings, owned by the CFRetained.
    let types = unsafe { CGImageDestination::type_identifiers() };
    // SAFETY: CGImageDestinationCopyTypeIdentifiers returns an array of
    // CFStrings, so viewing it as one is sound.
    let types = unsafe { types.cast_unchecked::<CFString>() };
    (0..types.len()).any(|i| types.get(i).is_some_and(|t| t.to_string() == uti))
}

/// sRGB IEC 61966-2.1 as a 456-byte ICC v2 profile (CC0, from
/// saucecontrol/Compact-ICC-Profiles), for formats whose readers assume
/// nothing without one: TIFF and WebP.
const SRGB_ICC: &[u8] = include_bytes!("../../assets/icc/sRGB-v2-micro.icc");

/// Metadata to put in an encoded file, each part already serialized: a
/// TIFF-structured EXIF block (`export::metadata::block`) and an XMP packet
/// (`export::metadata::xmp`).
#[derive(Clone, Copy, Default)]
pub struct Embedded<'a> {
    pub exif: Option<&'a [u8]>,
    pub xmp: Option<&'a [u8]>,
}

impl Embedded<'_> {
    pub const NONE: Embedded<'static> = Embedded {
        exif: None,
        xmp: None,
    };
}

const EXIF_HEAD: &[u8] = b"Exif\0\0";
const XMP_HEAD: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";

/// The largest EXIF block a JPEG's APP1 segment holds, after its `Exif\0\0`.
pub const MAX_JPEG_EXIF: usize = 65535 - 2 - 6;

/// Pixels checked and ready to encode, more than once if need be: a JPEG
/// held to a size limit is encoded at several qualities, and on macOS the
/// `CGImage` behind each try is built only once.
pub struct Prepared<'a> {
    width: u32,
    height: u32,
    pixels: Pixels<'a>,
    #[cfg(target_os = "macos")]
    image: std::cell::OnceCell<CFRetained<objc2_core_graphics::CGImage>>,
}

impl<'a> Prepared<'a> {
    pub fn new(width: u32, height: u32, pixels: Pixels<'a>) -> Result<Prepared<'a>, String> {
        pixels.check(width, height)?;
        Ok(Prepared {
            width,
            height,
            pixels,
            #[cfg(target_os = "macos")]
            image: std::cell::OnceCell::new(),
        })
    }

    /// Encode as `enc`, carrying `meta` where the format keeps it. Any EXIF
    /// or XMP the encoder wrote itself is replaced, or dropped when `meta`
    /// has none.
    #[hotpath::measure]
    pub fn encode(&self, enc: Encoding, meta: Embedded) -> Result<Vec<u8>, String> {
        let Embedded { exif, xmp } = meta;
        if !can_encode(enc) {
            return Err(format!("this build cannot write {enc:?}"));
        }
        let (width, height, pixels) = (self.width, self.height, self.pixels);
        match enc {
            Encoding::Jpeg(options) => {
                let jpeg = self.jpeg(options)?;
                let segment = |head: &[u8], body: &[u8]| {
                    let len = u16::try_from(2 + head.len() + body.len()).ok()?;
                    let mut app1 = vec![0xFF, 0xE1];
                    app1.extend_from_slice(&len.to_be_bytes());
                    app1.extend_from_slice(head);
                    app1.extend_from_slice(body);
                    Some(app1)
                };
                let mut app1 = exif.and_then(|e| segment(EXIF_HEAD, e)).unwrap_or_default();
                app1.extend(xmp.and_then(|x| segment(XMP_HEAD, x)).unwrap_or_default());
                Ok(splice_app1(&jpeg, &app1))
            }
            Encoding::Png => Ok(with_png_meta(&self.png()?, exif, xmp)),
            Encoding::Webp => {
                let mut out = Vec::new();
                let mut encoder = image_webp::WebPEncoder::new(&mut out);
                encoder.set_icc_profile(SRGB_ICC.to_vec());
                if let Some(e) = exif {
                    encoder.set_exif_metadata(e.to_vec());
                }
                if let Some(x) = xmp {
                    encoder.set_xmp_metadata(x.to_vec());
                }
                encoder
                    .encode(
                        &pixels.rgb8(width, height),
                        width,
                        height,
                        image_webp::ColorType::Rgb8,
                    )
                    .map_err(|e| e.to_string())?;
                Ok(out)
            }
            Encoding::Tiff => encode_tiff(width, height, pixels, exif, xmp),
            #[cfg(target_os = "macos")]
            Encoding::Avif => self.encode_imageio("public.avif", None, meta),
            #[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
            Encoding::Avif => encode_avif(width, height, &pixels.rgb8(width, height), exif),
            #[cfg(target_arch = "wasm32")]
            Encoding::Avif => Err(format!("this build cannot write {enc:?}")),
        }
    }

    #[cfg(target_os = "macos")]
    fn jpeg(&self, options: JpegOptions) -> Result<Vec<u8>, String> {
        self.encode_imageio("public.jpeg", options.quality, Embedded::NONE)
    }

    #[cfg(target_os = "macos")]
    fn png(&self) -> Result<Vec<u8>, String> {
        self.encode_imageio("public.png", None, Embedded::NONE)
    }

    /// Borrows 8-bit pixels as they are, so a size-limited export's
    /// quality tries copy nothing.
    #[cfg(not(target_os = "macos"))]
    fn jpeg(&self, options: JpegOptions) -> Result<Vec<u8>, String> {
        let rgba = match self.pixels {
            Pixels::Rgba8(p) => std::borrow::Cow::Borrowed(p),
            Pixels::Rgba16(p) => {
                std::borrow::Cow::Owned(p.iter().map(|c| (c >> 8) as u8).collect())
            }
        };
        mozjpeg_rs::Encoder::new(options.preset.mozjpeg())
            .quality(options.quality.unwrap_or(EXPORT_QUALITY))
            .encode_rgba(&rgba, self.width, self.height)
            .map_err(|e| e.to_string())
    }

    #[cfg(not(target_os = "macos"))]
    fn png(&self) -> Result<Vec<u8>, String> {
        let img = image::RgbImage::from_raw(
            self.width,
            self.height,
            self.pixels.rgb8(self.width, self.height),
        )
        .ok_or("pixel buffer does not fit the image")?;
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        Ok(out.into_inner())
    }

    /// Encode through ImageIO into memory. `quality` is 1 to 100, for a
    /// lossy format; `None` keeps ImageIO's default. `exif` goes in as the
    /// properties ImageIO reads from it, for a format with no EXIF segment
    /// of its own to splice.
    #[cfg(target_os = "macos")]
    fn encode_imageio(
        &self,
        uti: &str,
        quality: Option<u8>,
        meta: Embedded,
    ) -> Result<Vec<u8>, String> {
        let image = match self.image.get() {
            Some(image) => image,
            None => {
                let _ = self.image.set(self.cg_image()?);
                self.image.get().ok_or("could not keep the CGImage")?
            }
        };
        let data = CFMutableData::new(None, 0).ok_or("could not allocate CFData")?;
        // SAFETY: data and type are valid; one image follows.
        let dest =
            unsafe { CGImageDestination::with_data(&data, &CFString::from_str(uti), 1, None) }
                .ok_or_else(|| format!("ImageIO cannot write {uti}"))?;
        let properties = match quality {
            Some(q) => Some(lossy_quality_properties(
                f64::from(q.clamp(1, 100)) / 100.0,
            )?),
            None if meta.exif.is_some() || meta.xmp.is_some() => Some(meta_properties(meta)?),
            None => None,
        };
        // SAFETY: dest/image are valid; properties, if any, map a documented
        // destination key to a CFNumber.
        unsafe { CGImageDestination::add_image(&dest, image, properties.as_deref()) };
        // SAFETY: dest is valid; returns false if encoding failed.
        if !unsafe { CGImageDestination::finalize(&dest) } {
            return Err(format!("ImageIO could not encode {uti}"));
        }
        Ok(data.to_vec())
    }

    /// The pixels as an opaque 8-bit sRGB `CGImage`.
    #[cfg(target_os = "macos")]
    fn cg_image(&self) -> Result<CFRetained<objc2_core_graphics::CGImage>, String> {
        let mut buffer = self.pixels.rgbx8(self.width, self.height);
        // SAFETY: buffer holds width*height RGBX bytes, checked by
        // `Pixels::check`, and outlives `ctx`.
        let ctx = unsafe {
            coregraphics::srgb_opaque_context(
                buffer.as_mut_ptr() as *mut c_void,
                self.width,
                self.height,
                self.width as usize * 4,
            )?
        };
        coregraphics::bitmap_context_image(&ctx)
    }
}

/// What ImageIO reads from `meta`: the dictionaries of a one-pixel JPEG that
/// carries it, handed to a destination as the image's properties.
#[cfg(target_os = "macos")]
fn meta_properties(meta: Embedded) -> Result<CFRetained<CFDictionary>, String> {
    if meta.exif.is_some_and(|e| e.len() > MAX_JPEG_EXIF) {
        return Err("EXIF block too large".into());
    }
    let carrier = Prepared::new(1, 1, Pixels::Rgba8(&[0, 0, 0, 255]))?
        .encode(Encoding::Jpeg(JpegOptions::default()), meta)?;
    let data = objc2_core_foundation::CFData::from_bytes(&carrier);
    // SAFETY: data is a valid CFData and no options are passed.
    let source = unsafe { objc2_image_io::CGImageSource::with_data(&data, None) }
        .ok_or("ImageIO could not read the EXIF block")?;
    // SAFETY: index 0 exists in a one-image JPEG and no options are passed.
    unsafe { source.properties_at_index(0, None) }
        .ok_or_else(|| "ImageIO found no properties".into())
}

/// The quality of an export JPEG with no size limit. macOS passes none and
/// keeps ImageIO's default.
#[cfg(not(target_os = "macos"))]
const EXPORT_QUALITY: u8 = 90;

/// A TIFF of `pixels`, RGB at 8 or 16 bits per channel, little-endian,
/// uncompressed, with `exif`'s fields in its own directories and `xmp` as
/// its XMLPacket.
fn encode_tiff(
    width: u32,
    height: u32,
    pixels: Pixels,
    exif: Option<&[u8]>,
    xmp: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    use exif::{Field, In, Tag, Value};
    let n = width as usize * height as usize;
    let (samples, bits): (Vec<u8>, u16) = match pixels {
        Pixels::Rgba8(_) => (pixels.rgb8(width, height), 8),
        Pixels::Rgba16(p) => (
            p.as_chunks::<4>()
                .0
                .iter()
                .take(n)
                .flat_map(|&[r, g, b, _]| [r, g, b])
                .flat_map(u16::to_le_bytes)
                .collect(),
            16,
        ),
    };
    let row_bytes = width as usize * 3 * usize::from(bits / 8);
    // About 1 MB per strip, as most writers do.
    let rows = (1 << 20) / row_bytes.max(1);
    let rows = rows.clamp(1, height as usize);
    let strips: Vec<&[u8]> = samples.chunks(rows * row_bytes).collect();

    let source: Vec<Field> = match exif {
        Some(block) => exif::Reader::new()
            .read_raw(block.to_vec())
            .map_err(|e| format!("EXIF: {e}"))?
            .fields()
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    let field = |tag, value| Field {
        tag,
        ifd_num: In::PRIMARY,
        value,
    };
    let own = [
        field(Tag::ImageWidth, Value::Long(vec![width])),
        field(Tag::ImageLength, Value::Long(vec![height])),
        field(Tag::BitsPerSample, Value::Short(vec![bits; 3])),
        field(Tag::Compression, Value::Short(vec![1])),
        field(Tag::PhotometricInterpretation, Value::Short(vec![2])),
        field(Tag::SamplesPerPixel, Value::Short(vec![3])),
        field(Tag::RowsPerStrip, Value::Long(vec![rows as u32])),
        field(Tag::PlanarConfiguration, Value::Short(vec![1])),
        // InterColorProfile, which `exif` has no name for.
        field(
            Tag(exif::Context::Tiff, 0x8773),
            Value::Undefined(SRGB_ICC.to_vec(), 0),
        ),
    ];
    // XMLPacket, which `exif` has no name for either.
    let packet = xmp.map(|x| field(Tag(exif::Context::Tiff, 0x02BC), Value::Byte(x.to_vec())));
    let mut writer = exif::experimental::Writer::new();
    for f in own.iter().chain(&packet).chain(
        source
            .iter()
            .filter(|f| !own.iter().any(|o| o.tag == f.tag)),
    ) {
        writer.push_field(f);
    }
    writer.set_strips(&strips, In::PRIMARY);
    let mut out = std::io::Cursor::new(Vec::new());
    writer
        .write(&mut out, true)
        .map_err(|e| format!("TIFF: {e}"))?;
    Ok(out.into_inner())
}

/// AVIF through rav1e at quality 80 and speed 6, the middle of its range.
#[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
fn encode_avif(
    width: u32,
    height: u32,
    rgb: &[u8],
    exif: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let px: Vec<ravif::RGB8> = rgb
        .as_chunks::<3>()
        .0
        .iter()
        .map(|&[r, g, b]| ravif::RGB8 { r, g, b })
        .collect();
    let mut encoder = ravif::Encoder::new().with_quality(80.0).with_speed(6);
    if let Some(e) = exif {
        encoder = encoder.with_exif(e);
    }
    encoder
        .encode_rgb(ravif::Img::new(
            px.as_slice(),
            width as usize,
            height as usize,
        ))
        .map(|encoded| encoded.avif_file)
        .map_err(|e| e.to_string())
}

/// `png` with its eXIf chunks and XMP iTXt replaced by `exif` and `xmp`,
/// placed after IHDR, or dropped when `None`, and an `sRGB` chunk after IHDR
/// when the encoder named no colour space. Returns `png` unchanged if its
/// chunks don't parse.
fn with_png_meta(png: &[u8], exif: Option<&[u8]>, xmp: Option<&[u8]>) -> Vec<u8> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if !png.starts_with(SIGNATURE) {
        return png.to_vec();
    }
    let tagged = png_names_colour_space(png);
    let mut out = SIGNATURE.to_vec();
    let mut i = SIGNATURE.len();
    while let Some(head) = png.get(i..i + 8) {
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let Some(chunk) = len
            .checked_add(12)
            .and_then(|total| png.get(i..i.checked_add(total)?))
        else {
            return png.to_vec();
        };
        let kind = &head[4..8];
        let xmp_text =
            kind == b"iTXt" && chunk.get(8..).is_some_and(|b| b.starts_with(PNG_XMP_KEY));
        if kind != b"eXIf" && !xmp_text {
            out.extend_from_slice(chunk);
        }
        if kind == b"IHDR" {
            let mut chunk = |kind: &[u8; 4], body: &[u8]| {
                let Ok(n) = u32::try_from(body.len()) else {
                    return;
                };
                out.extend_from_slice(&n.to_be_bytes());
                let start = out.len();
                out.extend_from_slice(kind);
                out.extend_from_slice(body);
                let crc = crc32(&out[start..]);
                out.extend_from_slice(&crc.to_be_bytes());
            };
            if !tagged {
                // Rendering intent 0, perceptual.
                chunk(b"sRGB", &[0]);
            }
            if let Some(e) = exif {
                chunk(b"eXIf", e);
            }
            if let Some(x) = xmp {
                // Keyword, then no compression, no language, no translation.
                let mut text = PNG_XMP_KEY.to_vec();
                text.extend_from_slice(&[0, 0, 0, 0]);
                text.extend_from_slice(x);
                chunk(b"iTXt", &text);
            }
        }
        i += chunk.len();
    }
    out
}

/// Whether `png` has an `sRGB` or `iCCP` chunk. Pixel data can spell either name.
fn png_names_colour_space(png: &[u8]) -> bool {
    let mut i = 8usize;
    while let Some(head) = i.checked_add(8).and_then(|end| png.get(i..end)) {
        if matches!(&head[4..8], b"sRGB" | b"iCCP") {
            return true;
        }
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let Some(next) = len.checked_add(12).and_then(|n| i.checked_add(n)) else {
            return false;
        };
        i = next;
    }
    false
}

/// The iTXt keyword XMP goes under, with its terminating NUL.
const PNG_XMP_KEY: &[u8] = b"XML:com.adobe.xmp\0";

/// CRC-32 as PNG chunks use it (ISO 3309, reflected, poly 0xEDB88320).
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// Put `app1` after SOI (and after a JFIF APP0, which by convention comes
/// first), dropping any EXIF or XMP APP1 already there. Returns `jpeg` unchanged if
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
        // A length under 2 is malformed; it would leave no room for itself.
        let (Some(segment), Some(payload)) =
            (jpeg.get(i..i + 2 + len), jpeg.get(i + 4..i + 2 + len))
        else {
            return jpeg.to_vec();
        };
        let is_jfif = jpeg[i + 1] == 0xE0 && payload.starts_with(b"JFIF\0");
        let is_exif = jpeg[i + 1] == 0xE1
            && (payload.starts_with(EXIF_HEAD) || payload.starts_with(XMP_HEAD));
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
    use crate::decode::image_decode;

    /// Encode solid red, decode the bytes back, and check that the size and
    /// the approximate color survive.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn encode_jpeg_to_vec_round_trips() {
        let (w, h) = (8u32, 6u32);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            rgba.extend_from_slice(&[220, 30, 30, 255]);
        }

        let jpeg =
            encode_jpeg_to_vec(w, h, &rgba, JpegQuality::Export).expect("encode should succeed");
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

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn encode_jpeg_to_vec_rejects_zero_size() {
        assert!(encode_jpeg_to_vec(0, 4, &[], JpegQuality::Export).is_err());
        assert!(encode_jpeg_to_vec(4, 0, &[], JpegQuality::Export).is_err());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn encode_jpeg_to_vec_rejects_short_buffer() {
        assert!(encode_jpeg_to_vec(4, 4, &[0u8; 16], JpegQuality::Export).is_err());
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
        encode_jpeg(&out, w, h, &rgba, JpegQuality::Export).expect("encode should succeed");
        assert!(out.exists(), "jpeg file should have been written");

        let decoded = image_decode::decode(&out, 16384).expect("re-decode should succeed");
        assert_eq!((decoded.width, decoded.height), (w, h));
        // JPEG is lossy, so only check that the result is mostly red.
        let (r, g, b) = (decoded.rgba[0], decoded.rgba[1], decoded.rgba[2]);
        assert!(r > 150, "red channel should be high, got {r}");
        assert!(g < 100 && b < 100, "green/blue should be low, got {g},{b}");

        std::fs::remove_file(&out).ok();
    }

    /// Encode noise through `write` at both qualities and compare file sizes.
    /// Noise, so the gap comes from quantization and not from flat areas
    /// every quality compresses alike.
    fn assert_thumbnail_is_smaller(
        encoder: &str,
        write: impl Fn(&Path, u32, u32, &[u8], JpegQuality),
    ) {
        let (w, h) = (256u32, 256u32);
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let [r, g, b, ..] = seed.to_le_bytes();
            rgba.extend_from_slice(&[r, g, b, 255]);
        }

        let size = |quality: JpegQuality| {
            let path = std::env::temp_dir().join(format!(
                "lightphotos-quality-{encoder}-{quality:?}-{}.jpg",
                std::process::id()
            ));
            write(&path, w, h, &rgba, quality);
            let img = image_decode::decode(&path, u32::MAX).expect("re-decode should succeed");
            assert_eq!((img.width, img.height), (w, h));
            let len = std::fs::metadata(&path).unwrap().len();
            std::fs::remove_file(&path).ok();
            len
        };

        let (export, thumbnail) = (size(JpegQuality::Export), size(JpegQuality::Thumbnail));
        assert!(
            thumbnail * 10 < export * 8,
            "{encoder}: thumbnail {thumbnail} bytes should be under 80% of export {export} bytes"
        );
    }

    #[test]
    fn a_thumbnail_jpeg_is_smaller_than_an_export_of_the_same_pixels() {
        assert_thumbnail_is_smaller("platform", |path, w, h, rgba, quality| {
            encode_jpeg(path, w, h, rgba, quality).expect("encode should succeed")
        });
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_mozjpeg_thumbnail_is_smaller_than_an_export_of_the_same_pixels() {
        assert_thumbnail_is_smaller("mozjpeg", |path, w, h, rgba, quality| {
            let jpeg = encode_jpeg_to_vec(w, h, rgba, quality).expect("encode should succeed");
            std::fs::write(path, jpeg).unwrap()
        });
    }

    fn exif_blocks(jpeg: &[u8]) -> usize {
        jpeg.windows(6).filter(|w| w == b"Exif\0\0").count()
    }

    /// A `w` x `h` grey JPEG whose EXIF holds `stamp`, as an export of a
    /// photo taken then would.
    pub(crate) fn dated_jpeg(w: u32, h: u32, stamp: Option<image_decode::CaptureStamp>) -> Vec<u8> {
        use crate::export::metadata::{block, fields, Keep, SourceMeta};
        let source = SourceMeta::from_parts(None, stamp);
        let exif = block(&fields(&source, Keep::default(), "", w, h)).unwrap();
        let rgba = vec![90u8; (w * h * 4) as usize];
        Prepared::new(w, h, Pixels::Rgba8(&rgba))
            .unwrap()
            .encode(
                Encoding::Jpeg(JpegOptions::default()),
                Embedded {
                    exif: exif.as_deref(),
                    xmp: None,
                },
            )
            .unwrap()
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
        for stamp in [
            image_decode::CaptureStamp::new("2024:06:01 18:04:05", Some("-07:00")).unwrap(),
            image_decode::CaptureStamp::new("2023:12:31 23:59:59", None).unwrap(),
        ] {
            let tagged = dated_jpeg(12, 8, Some(stamp.clone()));
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

        let rgba = [90u8; 12 * 8 * 4];
        let bare = Prepared::new(12, 8, Pixels::Rgba8(&rgba))
            .unwrap()
            .encode(Encoding::Jpeg(JpegOptions::default()), Embedded::NONE)
            .unwrap();
        assert_eq!(exif_blocks(&bare), 0, "no metadata means no EXIF at all");
        std::fs::write(&path, bare).unwrap();
        assert_eq!(image_decode::capture_stamp(&path), None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn exif_goes_after_jfif_and_bytes_that_are_not_a_jpeg_pass_through() {
        let jfif = [0xFF, 0xE0, 0x00, 0x07, b'J', b'F', b'I', b'F', 0x00];
        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend_from_slice(&jfif);
        jpeg.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x02, 0xFF, 0xD9]);
        let app1 = [0xFF, 0xE1, 0x00, 0x08, b'E', b'x', b'i', b'f', 0, 0];
        let out = splice_app1(&jpeg, &app1);
        assert_eq!(&out[2..11], &jfif, "JFIF stays first");
        assert_eq!(&out[11..13], &[0xFF, 0xE1], "EXIF follows it");
        assert!(out.ends_with(&[0xFF, 0xDB, 0x00, 0x02, 0xFF, 0xD9]));

        assert_eq!(splice_app1(b"not a jpeg", &app1), b"not a jpeg");
    }

    #[test]
    fn an_app_segment_too_short_to_hold_its_length_passes_through() {
        for len in [0u8, 1] {
            let jpeg = [0xFF, 0xD8, 0xFF, 0xE1, 0x00, len, 0xFF, 0xD9];
            assert_eq!(splice_app1(&jpeg, &[]), jpeg);
        }
    }

    /// PNG chunks that run past the end, or a file that is not a PNG, pass
    /// through untouched rather than panic.
    #[test]
    fn a_malformed_png_passes_through_the_exif_splice() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xF0, b'I', b'H', b'D', b'R', 1, 2]);
        assert_eq!(with_png_meta(&png, Some(b"II*\0"), None), png);
        assert_eq!(with_png_meta(b"nope", Some(b"II*\0"), None), b"nope");
        assert_eq!(crc32(b"IEND"), 0xAE42_6082, "PNG's own IEND CRC");
    }

    fn png_with_idat(idat: &[u8], tag: Option<&[u8; 4]>) -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut chunk = |kind: &[u8; 4], body: &[u8]| {
            png.extend_from_slice(&u32::try_from(body.len()).unwrap().to_be_bytes());
            png.extend_from_slice(kind);
            png.extend_from_slice(body);
            png.extend_from_slice(&[0; 4]);
        };
        chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        if let Some(tag) = tag {
            chunk(tag, &[0]);
        }
        chunk(b"IDAT", idat);
        chunk(b"IEND", &[]);
        png
    }

    fn png_chunk_kinds(png: &[u8]) -> Vec<[u8; 4]> {
        let mut kinds = Vec::new();
        let mut i = 8;
        while let Some(head) = png.get(i..i + 8) {
            kinds.push([head[4], head[5], head[6], head[7]]);
            i += 12 + u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        }
        kinds
    }

    #[test]
    fn pixel_bytes_that_spell_a_colour_chunk_name_do_not_count_as_tagged() {
        for name in [b"sRGB", b"iCCP"] {
            let png = png_with_idat(name, None);
            let kinds = png_chunk_kinds(&with_png_meta(&png, None, None));
            assert_eq!(
                kinds.iter().filter(|k| *k == b"sRGB").count(),
                1,
                "an untagged PNG gets its sRGB chunk"
            );
        }
        let tagged = png_with_idat(b"pixels", Some(b"sRGB"));
        let kinds = png_chunk_kinds(&with_png_meta(&tagged, None, None));
        assert_eq!(
            kinds.iter().filter(|k| *k == b"sRGB").count(),
            1,
            "a PNG that names its colour space keeps just that chunk"
        );
    }

    /// The browser's export path: its worker holds only the source bytes, so
    /// the date it writes has to come from them.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_baked_export_carries_the_capture_date_of_its_source_bytes() {
        let stamp = image_decode::CaptureStamp::new("2024:06:01 18:04:05", Some("+09:00")).unwrap();
        let tagged = dated_jpeg(16, 8, Some(stamp.clone()));
        let plain = encode_jpeg_to_vec(16, 8, &[120u8; 16 * 8 * 4], JpegQuality::Export).unwrap();
        let bake = |src: Vec<u8>| {
            crate::export::bake_export_from_shared_vec(
                std::sync::Arc::new(src),
                false,
                &Default::default(),
                &[],
                &crate::develop::MaskSet::default(),
                0,
                crate::export::Output {
                    max_px: 8,
                    ..crate::export::Output::FULL
                },
            )
            .unwrap()
        };

        let out = bake(tagged);
        assert_eq!(image_decode::capture_stamp_from_bytes(&out), Some(stamp));
        assert_eq!(exif_blocks(&out), 1);

        let out = bake(plain);
        assert_eq!(
            image_decode::capture_stamp_from_bytes(&out),
            None,
            "no date is invented"
        );
    }
}
