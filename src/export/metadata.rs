// SPDX-License-Identifier: MIT OR Apache-2.0

//! What an export says about itself: an EXIF block built from the source's
//! own. The capture date, camera and lens model, exposure settings, size and
//! colour space always go in. Location and identifying camera data go in
//! only when the user turns them on ([`Keep`]), and a copyright line set
//! once replaces the photo's own. One block serves every format;
//! `image_encode` puts it where each format keeps EXIF.

use exif::{Context, Field, In, Tag, Value};

use crate::decode::image_decode::CaptureStamp;

/// The sensitive parts of a photo's metadata an export keeps. Both off by
/// default, so sharing a photo shares neither where it was taken nor whose
/// camera took it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Keep {
    /// The GPS block.
    #[serde(default)]
    pub location: bool,
    /// [`IDENTIFYING_TAGS`]: serial numbers and the people named.
    #[serde(default)]
    pub identity: bool,
}

impl Keep {
    /// Neither: the default.
    pub const NONE: Keep = Keep {
        location: false,
        identity: false,
    };
}

/// Tags that name a person or one physical camera or lens. The last two
/// are EXIF 3.0's Photographer and ImageEditor, which the `exif` crate has
/// no names for.
pub const IDENTIFYING_TAGS: [Tag; 7] = [
    Tag::Artist,
    Tag(Context::Exif, 0xA437),
    Tag(Context::Exif, 0xA438),
    Tag::CameraOwnerName,
    Tag::BodySerialNumber,
    Tag::LensSerialNumber,
    Tag::ImageUniqueID,
];

/// The longest copyright line written, in characters. EXIF has room for
/// more, but a JPEG's whole EXIF segment is 64 KB.
pub const MAX_COPYRIGHT_CHARS: usize = 200;

/// What a source file says about itself, read once per export.
#[derive(Default)]
pub struct SourceMeta {
    exif: Option<exif::Exif>,
    /// The capture time, read by the platform's own reader, for sources
    /// whose EXIF the `exif` crate cannot open (CR3, RAF on macOS).
    stamp: Option<CaptureStamp>,
}

impl SourceMeta {
    /// Read `path`'s metadata. A file with none, or with EXIF too broken to
    /// read, gives an empty one: the export goes on without it.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn read_path(path: &std::path::Path) -> SourceMeta {
        let exif = std::fs::File::open(path)
            .ok()
            .and_then(|file| read_exif(&mut std::io::BufReader::new(file)));
        let stamp = match exif.as_ref().is_some_and(has_date) {
            true => None,
            false => crate::decode::image_decode::capture_stamp(path),
        };
        SourceMeta { exif, stamp }
    }

    /// [`SourceMeta::read_path`] for a source already in memory.
    #[cfg(not(target_os = "macos"))]
    pub fn read_bytes(bytes: &[u8]) -> SourceMeta {
        let exif = read_exif(&mut std::io::Cursor::new(bytes));
        let stamp = match exif.as_ref().is_some_and(has_date) {
            true => None,
            false => crate::decode::image_decode::capture_stamp_from_bytes(bytes),
        };
        SourceMeta { exif, stamp }
    }

    /// When the photo was taken, by the camera's clock.
    pub fn stamp(&self) -> Option<CaptureStamp> {
        let text = |tag| {
            let field = self.exif.as_ref()?.get_field(tag, In::PRIMARY)?;
            match &field.value {
                Value::Ascii(v) => Some(String::from_utf8_lossy(v.first()?).into_owned()),
                _ => None,
            }
        };
        text(Tag::DateTimeOriginal)
            .and_then(|t| CaptureStamp::new(&t, text(Tag::OffsetTimeOriginal).as_deref()))
            .or_else(|| self.stamp.clone())
    }

    /// A source known by its parts, such as a test's.
    pub fn from_parts(exif: Option<exif::Exif>, stamp: Option<CaptureStamp>) -> SourceMeta {
        SourceMeta { exif, stamp }
    }
}

/// The EXIF in a container, keeping every field that parsed when some
/// entry is broken, as camera files often have one. `None` when there is no
/// EXIF or nothing in it parsed.
fn read_exif<R: std::io::BufRead + std::io::Seek>(reader: &mut R) -> Option<exif::Exif> {
    match exif::Reader::new()
        .continue_on_error(true)
        .read_from_container(reader)
    {
        Ok(exif) => Some(exif),
        Err(e) => e.distill_partial_result(|_| {}).ok(),
    }
}

fn has_date(exif: &exif::Exif) -> bool {
    exif.get_field(Tag::DateTimeOriginal, In::PRIMARY).is_some()
}

/// The source's tags every export keeps: what took the photo, when, and
/// how it was exposed. Anything else, such as a caption, a comment, a maker
/// note or a tag describing the source's own pixels, stays behind.
const ALWAYS_TAGS: [Tag; 49] = [
    Tag::Make,
    Tag::Model,
    Tag::Software,
    Tag::DateTime,
    Tag::Copyright,
    Tag::XResolution,
    Tag::YResolution,
    Tag::ResolutionUnit,
    Tag::DateTimeOriginal,
    Tag::DateTimeDigitized,
    Tag::OffsetTime,
    Tag::OffsetTimeOriginal,
    Tag::OffsetTimeDigitized,
    Tag::SubSecTime,
    Tag::SubSecTimeOriginal,
    Tag::SubSecTimeDigitized,
    Tag::ExposureTime,
    Tag::FNumber,
    Tag::ExposureProgram,
    Tag::PhotographicSensitivity,
    Tag::SensitivityType,
    Tag::RecommendedExposureIndex,
    Tag::ISOSpeed,
    Tag::ExposureIndex,
    Tag::ShutterSpeedValue,
    Tag::ApertureValue,
    Tag::BrightnessValue,
    Tag::ExposureBiasValue,
    Tag::MaxApertureValue,
    Tag::SubjectDistance,
    Tag::MeteringMode,
    Tag::LightSource,
    Tag::Flash,
    Tag::FocalLength,
    Tag::FocalLengthIn35mmFilm,
    Tag::ExposureMode,
    Tag::WhiteBalance,
    Tag::DigitalZoomRatio,
    Tag::SceneCaptureType,
    Tag::GainControl,
    Tag::Contrast,
    Tag::Saturation,
    Tag::Sharpness,
    Tag::SensingMethod,
    Tag::SceneType,
    Tag::CustomRendered,
    Tag::LensSpecification,
    Tag::LensMake,
    Tag::LensModel,
];

/// The EXIF fields an export of `src` at `width` x `height` carries.
/// `copyright`, when not blank, replaces the source's own.
pub fn fields(
    src: &SourceMeta,
    keep: Keep,
    copyright: &str,
    width: u32,
    height: u32,
) -> Vec<Field> {
    let field = |tag, value| Field {
        tag,
        ifd_num: In::PRIMARY,
        value,
    };
    let own_copyright = copyright_line(copyright).map(|c| field(Tag::Copyright, c));

    let mut out: Vec<Field> = src
        .exif
        .iter()
        .flat_map(|e| e.fields())
        .filter(|f| f.ifd_num == In::PRIMARY)
        .filter(|f| !matches!(f.value, Value::Unknown(..)))
        .filter(|f| !(own_copyright.is_some() && f.tag == Tag::Copyright))
        .filter(|f| {
            ALWAYS_TAGS.contains(&f.tag)
                || (keep.identity && IDENTIFYING_TAGS.contains(&f.tag))
                || (keep.location && f.tag.context() == Context::Gps)
        })
        .cloned()
        .filter_map(|mut f| {
            if f.tag == Tag::Copyright {
                f.value = copyright_text(&f.value).and_then(|t| copyright_line(&t))?;
            }
            Some(f)
        })
        .collect();
    out.extend(own_copyright);

    let has = |out: &[Field], tag| out.iter().any(|f: &Field| f.tag == tag);
    if !has(&out, Tag::DateTimeOriginal) {
        out.retain(|f| f.tag != Tag::OffsetTimeOriginal);
        out.extend(capture_date(src));
    }
    out.extend(technical(width, height));
    out
}

/// What every export with metadata says about its own pixels: EXIF 2.31,
/// sRGB, and its size.
fn technical(width: u32, height: u32) -> [Field; 4] {
    let field = |tag, value| Field {
        tag,
        ifd_num: In::PRIMARY,
        value,
    };
    [
        field(Tag::ExifVersion, Value::Undefined(b"0231".to_vec(), 0)),
        field(Tag::ColorSpace, Value::Short(vec![1])),
        field(Tag::PixelXDimension, Value::Long(vec![width])),
        field(Tag::PixelYDimension, Value::Long(vec![height])),
    ]
}

/// The fields a file keeps when the rest will not fit, as in a JPEG's
/// 64 KB EXIF segment: everything but location and identifying data, all
/// of which [`ALWAYS_TAGS`] and [`technical`] keep small.
pub fn essential(fields: &[Field]) -> Vec<Field> {
    fields
        .iter()
        .filter(|f| {
            ALWAYS_TAGS.contains(&f.tag)
                || matches!(
                    f.tag,
                    Tag::ExifVersion
                        | Tag::ColorSpace
                        | Tag::PixelXDimension
                        | Tag::PixelYDimension
                )
        })
        .cloned()
        .collect()
}

/// `text` as a copyright value: control characters out, trimmed, then cut
/// to [`MAX_COPYRIGHT_CHARS`]. `None` when nothing is left.
fn copyright_line(text: &str) -> Option<Value> {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    let line: String = clean.trim().chars().take(MAX_COPYRIGHT_CHARS).collect();
    (!line.is_empty()).then(|| Value::Ascii(vec![line.into_bytes()]))
}

fn copyright_text(value: &Value) -> Option<String> {
    match value {
        Value::Ascii(v) => Some(String::from_utf8_lossy(v.first()?).into_owned()),
        _ => None,
    }
}

/// When the photo was taken, as EXIF fields: the source's own
/// `DateTimeOriginal` and its offset, else the platform reader's stamp.
fn capture_date(src: &SourceMeta) -> Vec<Field> {
    let own: Vec<Field> = src
        .exif
        .iter()
        .flat_map(|e| e.fields())
        .filter(|f| f.ifd_num == In::PRIMARY)
        .filter(|f| matches!(f.tag, Tag::DateTimeOriginal | Tag::OffsetTimeOriginal))
        .filter(|f| matches!(f.value, Value::Ascii(_)))
        .cloned()
        .collect();
    if own.iter().any(|f| f.tag == Tag::DateTimeOriginal) {
        return own;
    }
    let ascii = |s: &str| Value::Ascii(vec![s.as_bytes().to_vec()]);
    let field = |tag, value| Field {
        tag,
        ifd_num: In::PRIMARY,
        value,
    };
    src.stamp
        .iter()
        .flat_map(|stamp| {
            std::iter::once(field(Tag::DateTimeOriginal, ascii(&stamp.local))).chain(
                stamp
                    .offset
                    .as_deref()
                    .map(|o| field(Tag::OffsetTimeOriginal, ascii(o))),
            )
        })
        .collect()
}

/// An XMP packet holding the copyright in `fields` as `dc:rights`, or
/// `None` without one. EXIF's Copyright is an ASCII tag; readers that
/// decode it as Latin-1, as Windows does, garble a "©" or a Chinese name,
/// while every XMP reader takes UTF-8.
pub fn xmp(fields: &[Field]) -> Option<Vec<u8>> {
    let rights = fields
        .iter()
        .find(|f| f.tag == Tag::Copyright)
        .and_then(|f| copyright_text(&f.value))?;
    let escaped = rights
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    Some(
        format!(
            "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\
             <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
             <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
             <rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\
             <dc:rights><rdf:Alt><rdf:li xml:lang=\"x-default\">{escaped}</rdf:li></rdf:Alt></dc:rights>\
             </rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end=\"r\"?>"
        )
        .into_bytes(),
    )
}

/// `fields` as a little-endian TIFF-structured EXIF block, the body of a
/// JPEG's APP1 after `Exif\0\0`, a PNG's eXIf chunk or a WebP's EXIF chunk.
/// `None` for no fields.
pub fn block(fields: &[Field]) -> Result<Option<Vec<u8>>, String> {
    if fields.is_empty() {
        return Ok(None);
    }
    let mut writer = exif::experimental::Writer::new();
    for f in fields {
        writer.push_field(f);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    writer
        .write(&mut out, true)
        .map_err(|e| format!("EXIF: {e}"))?;
    Ok(Some(out.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> SourceMeta {
        let f = |tag, value| Field {
            tag,
            ifd_num: In::PRIMARY,
            value,
        };
        let ascii = |s: &str| Value::Ascii(vec![s.as_bytes().to_vec()]);
        let fields = vec![
            f(Tag::Make, ascii("TestCam")),
            f(Tag::Orientation, Value::Short(vec![6])),
            f(Tag::Copyright, ascii("Source Copyright")),
            f(Tag::DateTimeOriginal, ascii("2024:06:01 18:04:05")),
            f(Tag::MakerNote, Value::Undefined(vec![1, 2, 3, 4], 0)),
            f(Tag::GPSLatitudeRef, ascii("N")),
            f(Tag::PixelXDimension, Value::Long(vec![6000])),
            f(Tag::BodySerialNumber, ascii("123456")),
            f(Tag::Artist, ascii("Ada")),
            f(Tag::LensModel, ascii("35mm")),
            f(Tag::ImageDescription, ascii("Home, kids' room")),
            // EXIF 3.0's Photographer, which the exif crate cannot name.
            f(Tag(Context::Exif, 0xA437), ascii("Ada")),
            f(
                Tag::UserComment,
                Value::Undefined(b"ASCII\0\0\0at home".to_vec(), 0),
            ),
            f(Tag(Context::Exif, 0xBEEF), ascii("unknown")),
        ];
        let blob = block(&fields).unwrap().unwrap();
        SourceMeta::from_parts(Some(exif::Reader::new().read_raw(blob).unwrap()), None)
    }

    fn tags(fields: &[Field]) -> Vec<Tag> {
        fields.iter().map(|f| f.tag).collect()
    }

    fn text(fields: &[Field], tag: Tag) -> Option<String> {
        fields
            .iter()
            .find(|f| f.tag == tag)
            .map(|f| match &f.value {
                Value::Ascii(v) => String::from_utf8_lossy(&v[0]).into_owned(),
                other => format!("{other:?}"),
            })
    }

    #[test]
    fn location_and_identity_go_in_only_when_kept() {
        let src = source();
        let plain = fields(&src, Keep::default(), "", 300, 200);
        for tag in [
            Tag::Make,
            Tag::LensModel,
            Tag::DateTimeOriginal,
            Tag::Copyright,
        ] {
            assert!(tags(&plain).contains(&tag), "{tag} always goes in");
        }
        assert!(!plain.iter().any(|f| f.tag.context() == Context::Gps));
        assert!(!tags(&plain).contains(&Tag::BodySerialNumber));
        assert!(!tags(&plain).contains(&Tag::Artist));
        assert!(
            !tags(&plain).contains(&Tag::ImageDescription),
            "captions never go"
        );
        assert!(
            !tags(&plain).contains(&Tag(Context::Exif, 0xA437)),
            "Photographer is identifying"
        );
        assert!(
            !tags(&plain).contains(&Tag::MakerNote),
            "maker notes cannot move"
        );
        assert!(
            !tags(&plain).contains(&Tag::Orientation),
            "pixels are upright"
        );
        assert_eq!(
            text(&plain, Tag::PixelXDimension).as_deref(),
            Some("Long([300])")
        );
        assert_eq!(
            plain
                .iter()
                .filter(|f| f.tag == Tag::PixelXDimension)
                .count(),
            1
        );
        assert_eq!(Keep::default(), Keep::NONE);

        let located = fields(
            &src,
            Keep {
                location: true,
                identity: false,
            },
            "",
            1,
            1,
        );
        assert!(tags(&located).contains(&Tag::GPSLatitudeRef));
        assert!(!tags(&located).contains(&Tag::BodySerialNumber));

        let known = fields(
            &src,
            Keep {
                location: false,
                identity: true,
            },
            "",
            1,
            1,
        );
        assert!(tags(&known).contains(&Tag::BodySerialNumber));
        assert!(tags(&known).contains(&Tag::Artist));
        assert!(
            tags(&known).contains(&Tag(Context::Exif, 0xA437)),
            "EXIF 3.0 Photographer"
        );
        assert!(
            !tags(&known).contains(&Tag::ImageDescription),
            "captions never go"
        );
        assert!(!known.iter().any(|f| f.tag.context() == Context::Gps));
        assert_eq!(block(&[]).unwrap(), None);
    }

    #[test]
    fn a_line_set_once_replaces_the_source_copyright() {
        let src = source();
        let theirs = fields(&src, Keep::default(), "", 1, 1);
        assert_eq!(
            text(&theirs, Tag::Copyright).as_deref(),
            Some("Source Copyright")
        );
        let mine = fields(&src, Keep::default(), " © Ada\u{0} ", 1, 1);
        assert_eq!(text(&mine, Tag::Copyright).as_deref(), Some("© Ada"));
        assert_eq!(mine.iter().filter(|f| f.tag == Tag::Copyright).count(), 1);
        let padded = fields(
            &src,
            Keep::default(),
            &format!("   {}", "y".repeat(300)),
            1,
            1,
        );
        assert_eq!(
            text(&padded, Tag::Copyright).map(|t| t.len()),
            Some(MAX_COPYRIGHT_CHARS)
        );
        let theirs_long = {
            let f = Field {
                tag: Tag::Copyright,
                ifd_num: In::PRIMARY,
                value: Value::Ascii(vec![vec![b'z'; 70_000]]),
            };
            let blob = block(&[f]).unwrap().unwrap();
            SourceMeta::from_parts(exif::Reader::new().read_raw(blob).ok(), None)
        };
        let capped = fields(&theirs_long, Keep::default(), "", 1, 1);
        assert_eq!(
            text(&capped, Tag::Copyright).map(|t| t.len()),
            Some(MAX_COPYRIGHT_CHARS)
        );
        let long = fields(&src, Keep::default(), &"x".repeat(5000), 1, 1);
        assert_eq!(
            text(&long, Tag::Copyright).map(|t| t.len()),
            Some(MAX_COPYRIGHT_CHARS)
        );
    }

    /// A source with no readable EXIF still gets its capture time, from the
    /// platform reader, and the export's own size and colour space.
    #[test]
    fn a_source_without_exif_gets_its_date_and_size() {
        let stamp = CaptureStamp::new("2023:12:31 23:59:59", Some("+09:00"));
        let out = fields(
            &SourceMeta::from_parts(None, stamp),
            Keep::default(),
            "",
            40,
            30,
        );
        assert_eq!(
            text(&out, Tag::DateTimeOriginal).as_deref(),
            Some("2023:12:31 23:59:59")
        );
        assert_eq!(
            text(&out, Tag::OffsetTimeOriginal).as_deref(),
            Some("+09:00")
        );
        assert_eq!(text(&out, Tag::ColorSpace).as_deref(), Some("Short([1])"));
        let blob = block(&out).unwrap().unwrap();
        let back = exif::Reader::new().read_raw(blob).unwrap();
        assert!(back.get_field(Tag::PixelYDimension, In::PRIMARY).is_some());
    }

    /// EXIF that is cut off or garbage reads as no EXIF, never a panic.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_malformed_source_reads_as_no_metadata() {
        let blob = block(&fields(&source(), Keep::default(), "", 1, 1))
            .unwrap()
            .unwrap();
        for bad in [
            &blob[..blob.len() / 2],
            &blob[..9],
            b"II*\0\xff\xff\xff\xff".as_slice(),
        ] {
            let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
            jpeg.extend(((bad.len() + 8) as u16).to_be_bytes());
            jpeg.extend(b"Exif\0\0");
            jpeg.extend(bad);
            jpeg.extend([0xFF, 0xD9]);
            let meta = SourceMeta::read_bytes(&jpeg);
            let out = fields(&meta, Keep::default(), "", 1, 1);
            assert!(!tags(&out).contains(&Tag::Make));
        }
    }
}
