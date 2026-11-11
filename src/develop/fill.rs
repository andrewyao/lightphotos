// SPDX-License-Identifier: MIT OR Apache-2.0

//! Generative fill: an area of the photo replaced by pixels a generator
//! (ComfyUI) painted. A [`Fill`] is the sidecar's record of one: the source
//! UV rect it covers and the patch files it may show. A [`Patch`] is one
//! such file's pixels, RGBA8, whose alpha is the feathered fill mask. Every
//! variant of one fill shares that alpha.
//!
//! Patches are stored as coverage is: content-addressed PNGs under
//! `.lightphotos/fills/`, write-once, checked against their id on every
//! read. A fill composites at the Touch Up stage, on source pixels, so the
//! develop sliders grade it with the rest of the photo. [`to_comfy`] and
//! [`from_comfy`] convert between those source pixels and the display sRGB
//! the generator works in.

#[cfg(not(target_arch = "wasm32"))]
use std::ffi::OsStr;
use std::fmt;
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::coverage::RasterId;
use crate::decode::image_decode::PixelFormat;

/// The most fills one photo keeps.
pub const MAX_FILLS: usize = 16;
/// The most variants one fill offers to choose from.
pub const MAX_VARIANTS: usize = 3;
/// The longest prompt kept, in characters.
pub const MAX_PROMPT_CHARS: usize = 1000;
/// The longest side a stored patch may have.
pub const MAX_PATCH_SIDE: u32 = 2048;
/// A patch file larger than this on disk is refused before decoding. An
/// RGBA PNG at the largest side is 16 MiB uncompressed, so this holds one
/// that doesn't compress at all.
pub const MAX_PATCH_FILE_BYTES: u64 = 20 << 20;
/// The subdirectory of `.lightphotos/` that holds patch files.
pub const FILLS_DIR: &str = "fills";

/// One generative fill, as the sidecar records it. Private fields, so a
/// fill always has a usable rect and 1..=[`MAX_VARIANTS`] variants, one of
/// them chosen.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(try_from = "FillJson")]
pub struct Fill {
    /// Source UV rect `(u0, v0, u1, v1)` the patches cover, inside 0..1 with
    /// `u0 < u1` and `v0 < v1`.
    bbox: [f32; 4],
    variants: Vec<RasterId>,
    /// One of `variants`. Held as the id rather than an index, so reading
    /// it can't miss; the sidecar stores the index.
    chosen: RasterId,
    /// What the user asked for. Empty fills from the surrounding pixels.
    prompt: String,
}

impl Serialize for Fill {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut out = serializer.serialize_struct("Fill", 4)?;
        out.serialize_field("bbox", &self.bbox)?;
        out.serialize_field("variants", &self.variants)?;
        out.serialize_field("chosen", &self.chosen_index())?;
        if self.prompt.is_empty() {
            out.skip_field("prompt")?;
        } else {
            out.serialize_field("prompt", &self.prompt)?;
        }
        out.end()
    }
}

/// A fill as any sidecar may hold it, before [`Fill::new`] checks it.
#[derive(Deserialize)]
struct FillJson {
    bbox: [f32; 4],
    variants: Vec<RasterId>,
    /// Any JSON, so a negative or fractional index costs only the choice.
    #[serde(default)]
    chosen: serde_json::Value,
    #[serde(default)]
    prompt: String,
}

impl TryFrom<FillJson> for Fill {
    type Error = &'static str;
    fn try_from(json: FillJson) -> Result<Fill, &'static str> {
        let mut fill = Fill::new(json.bbox, json.variants, json.prompt)
            .ok_or("a fill needs a rect inside the photo and a variant")?;
        if let Some(i) = json.chosen.as_u64().and_then(|i| usize::try_from(i).ok()) {
            fill.choose(i);
        }
        Ok(fill)
    }
}

impl Fill {
    /// A fill showing its first variant. `None` for a rect that isn't a
    /// non-empty one inside 0..1, or no variants. Variants past
    /// [`MAX_VARIANTS`] are dropped and the prompt is cut to
    /// [`MAX_PROMPT_CHARS`].
    pub fn new(bbox: [f32; 4], mut variants: Vec<RasterId>, mut prompt: String) -> Option<Fill> {
        let [u0, v0, u1, v1] = bbox;
        let inside = |lo: f32, hi: f32| 0.0 <= lo && lo < hi && hi <= 1.0;
        if !bbox.iter().all(|c| c.is_finite()) || !inside(u0, u1) || !inside(v0, v1) {
            return None;
        }
        variants.truncate(MAX_VARIANTS);
        let chosen = *variants.first()?;
        if let Some((cut, _)) = prompt.char_indices().nth(MAX_PROMPT_CHARS) {
            prompt.truncate(cut);
        }
        Some(Fill {
            bbox,
            variants,
            chosen,
            prompt,
        })
    }

    pub fn bbox(&self) -> [f32; 4] {
        self.bbox
    }

    pub fn variants(&self) -> &[RasterId] {
        &self.variants
    }

    /// The variant the photo shows.
    pub fn chosen(&self) -> RasterId {
        self.chosen
    }

    pub fn chosen_index(&self) -> usize {
        self.variants
            .iter()
            .position(|&v| v == self.chosen)
            .unwrap_or(0)
    }

    /// Show variant `i`; an index past the variants shows the first.
    pub fn choose(&mut self, i: usize) {
        if let Some(&id) = self.variants.get(i).or(self.variants.first()) {
            self.chosen = id;
        }
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// Drop every variant but the chosen one, as leaving the tool does. The
    /// orphan sweep then removes the dropped files.
    pub fn keep_chosen(&mut self) {
        self.variants = vec![self.chosen];
    }
}

/// Why a patch couldn't be made or read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatchError {
    /// No file for this id: the folder was copied without `.lightphotos/fills`.
    Missing,
    /// Unreadable, not an RGBA8 PNG, the wrong size, or its hash isn't its id.
    Corrupt(String),
    Io(String),
}

impl fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatchError::Missing => write!(f, "the fill's image file is missing"),
            PatchError::Corrupt(why) => write!(f, "the fill's image file is unreadable: {why}"),
            PatchError::Io(why) => write!(f, "{why}"),
        }
    }
}

/// A validated patch: `1..=MAX_PATCH_SIDE` on each side, `width * height *
/// 4` bytes of straight-alpha RGBA, and an id that is the hash of those.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patch {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    id: RasterId,
}

impl Patch {
    /// `Err` for a side of 0 or past [`MAX_PATCH_SIDE`], or a buffer that
    /// isn't exactly `width * height * 4` bytes.
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Result<Patch, PatchError> {
        let sides_ok = |s: u32| (1..=MAX_PATCH_SIDE).contains(&s);
        if !sides_ok(width) || !sides_ok(height) {
            return Err(PatchError::Corrupt(format!(
                "{width}x{height} is not a patch size"
            )));
        }
        let len = width as usize * height as usize * 4;
        if rgba.len() != len {
            return Err(PatchError::Corrupt(format!(
                "a {width}x{height} patch needs {len} bytes, got {}",
                rgba.len()
            )));
        }
        let id = RasterId::of(width, height, &rgba);
        Ok(Patch {
            width,
            height,
            rgba,
            id,
        })
    }

    pub fn id(&self) -> RasterId {
        self.id
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// `width * height * 4` bytes, row-major RGBA, straight alpha.
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// The file's bytes: an 8-bit RGBA PNG.
    pub fn encode(&self) -> Result<Vec<u8>, PatchError> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| PatchError::Io(e.to_string()))?;
        writer
            .write_image_data(&self.rgba)
            .map_err(|e| PatchError::Io(e.to_string()))?;
        drop(writer);
        Ok(out)
    }

    /// A file's bytes back to a patch, refused unless it is an 8-bit RGBA
    /// PNG within [`MAX_PATCH_SIDE`] whose hash is `expected`.
    pub fn decode(bytes: &[u8], expected: RasterId) -> Result<Patch, PatchError> {
        let corrupt = |e: png::DecodingError| PatchError::Corrupt(e.to_string());
        let side = MAX_PATCH_SIDE as usize;
        // The frame plus the decoder's own row buffers.
        let limits = png::Limits {
            bytes: side * side * 4 * 2,
        };
        let decoder = png::Decoder::new_with_limits(std::io::Cursor::new(bytes), limits);
        let mut reader = decoder.read_info().map_err(corrupt)?;
        let info = reader.info();
        let (width, height) = (info.width, info.height);
        if width == 0 || height == 0 || width > MAX_PATCH_SIDE || height > MAX_PATCH_SIDE {
            return Err(PatchError::Corrupt(format!(
                "{width}x{height} is not a patch size"
            )));
        }
        if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
            return Err(PatchError::Corrupt(format!(
                "{:?} {:?} is not 8-bit RGBA",
                info.color_type, info.bit_depth
            )));
        }
        let len = width as usize * height as usize * 4;
        if reader.output_buffer_size() != Some(len) {
            return Err(PatchError::Corrupt("unexpected frame size".into()));
        }
        let mut rgba = vec![0u8; len];
        reader.next_frame(&mut rgba).map_err(corrupt)?;
        let patch = Patch::new(width, height, rgba)?;
        if patch.id != expected {
            return Err(PatchError::Corrupt(format!(
                "its content hashes to {}, not {expected}",
                patch.id
            )));
        }
        Ok(patch)
    }
}

/// `<dir>/.lightphotos/fills/<photo>.<id>.png` for the photo at `photo`.
#[cfg(not(target_arch = "wasm32"))]
fn file_path(photo: &Path, id: RasterId) -> Result<std::path::PathBuf, PatchError> {
    super::side_file::path(photo, FILLS_DIR, id).ok_or(PatchError::Missing)
}

/// The patch `id` stored for `photo`.
#[cfg(not(target_arch = "wasm32"))]
pub fn read(photo: &Path, id: RasterId) -> Result<Patch, PatchError> {
    let path = file_path(photo, id)?;
    match crate::paths::read_capped(&path, MAX_PATCH_FILE_BYTES as usize) {
        Ok(Some(bytes)) => Patch::decode(&bytes, id),
        Ok(None) => Err(PatchError::Missing),
        Err(e @ crate::paths::CappedReadError::TooBig(_)) => {
            Err(PatchError::Corrupt(e.to_string()))
        }
        Err(e) => Err(PatchError::Io(format!("{}: {e}", path.display()))),
    }
}

/// Stores `patch` beside `photo`, atomically. A file already there has the
/// same name, so the same bytes: nothing is written.
#[cfg(not(target_arch = "wasm32"))]
pub fn write(photo: &Path, patch: &Patch) -> Result<(), PatchError> {
    let path = file_path(photo, patch.id)?;
    if path.is_file() {
        return Ok(());
    }
    let bytes = patch.encode()?;
    crate::paths::write_atomic_in_dir(&path, &bytes)
        .map_err(|e| PatchError::Io(format!("{}: {e}", path.display())))
}

/// Removes the file for `id`, when a read found it corrupt.
#[cfg(not(target_arch = "wasm32"))]
pub fn remove(photo: &Path, id: RasterId) {
    if let Ok(path) = file_path(photo, id) {
        let _ = crate::paths::remove_if_present(&path);
    }
}

/// Removes patch files in `dir`'s folder that `named` doesn't claim; see
/// [`super::side_file::sweep_orphans`]. `named(photo, id)` is whether any
/// of the photo's fills lists `id` among its variants.
#[cfg(not(target_arch = "wasm32"))]
pub fn sweep_orphans(dir: &Path, named: impl Fn(&OsStr, RasterId) -> bool) {
    super::side_file::sweep_orphans(dir, FILLS_DIR, named);
}

/// A source pixel, as the Touch Up stage sees it (linear light, decoded
/// from the file's encoding), to the display sRGB a generator works in: the
/// photo as the Loupe shows it with no edits. `Srgb8` gives back the file's
/// own bytes; `TonedF16` is sRGB-encoded; `LinearF16` goes through the RAW
/// display curve, which includes the sRGB encode.
pub fn to_comfy(px: [f32; 3], format: PixelFormat) -> [u8; 3] {
    match format {
        PixelFormat::Srgb8 | PixelFormat::TonedF16 => px.map(super::image_ops::linear_to_srgb8),
        PixelFormat::LinearF16 => px.map(|c| {
            let display = super::apply_raw_preview_boost(super::srgb_encode(c));
            (display * 255.0).round().clamp(0.0, 255.0) as u8
        }),
    }
}

/// The inverse of [`to_comfy`]: a generator's display sRGB back to the
/// source pixel that displays as it. For `LinearF16`, display white is
/// where the RAW look curve first reaches 1, so a white pixel comes back
/// as the darkest source value that shows white, never as infinity.
pub fn from_comfy(rgb: [u8; 3], format: PixelFormat) -> [f32; 3] {
    match format {
        PixelFormat::Srgb8 | PixelFormat::TonedF16 => {
            let lut = super::image_ops::srgb8_to_linear_lut();
            rgb.map(|c| lut[c as usize])
        }
        PixelFormat::LinearF16 => {
            rgb.map(|c| super::srgb_decode(raw_look_inverse(c as f32 / 255.0)))
        }
    }
}

/// The input [`super::apply_raw_preview_boost`] maps to `y`. The curve is
/// piecewise linear through evenly spaced knots, rising everywhere but its
/// flat top, so each rising segment inverts in closed form; on the flat top
/// this answers its left end.
fn raw_look_inverse(y: f32) -> f32 {
    let knots = &super::RAW_LOOK_KNOTS;
    let segments = (knots.len() - 1) as f32;
    let y = if y.is_finite() {
        y.clamp(0.0, 1.0)
    } else {
        0.0
    };
    for (i, pair) in knots.windows(2).enumerate() {
        if let [a, b] = *pair {
            if b > a && y <= b {
                return (i as f32 + (y - a).max(0.0) / (b - a)) / segments;
            }
        }
    }
    1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> RasterId {
        format!("{n:016x}").parse().unwrap()
    }

    fn patch(w: u32, h: u32, seed: u8) -> Patch {
        let rgba = (0..w * h * 4)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect();
        Patch::new(w, h, rgba).unwrap()
    }

    fn png_of(w: u32, h: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(color);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(data).unwrap();
        out
    }

    #[test]
    fn a_fill_refuses_a_rect_outside_the_photo_and_caps_what_it_holds() {
        let ok = [0.1, 0.2, 0.3, 0.4];
        for bad in [
            [f32::NAN, 0.2, 0.3, 0.4],
            [0.1, 0.2, f32::INFINITY, 0.4],
            [0.3, 0.2, 0.1, 0.4],
            [0.1, 0.4, 0.3, 0.4],
            [-0.1, 0.2, 0.3, 0.4],
            [0.1, 0.2, 0.3, 1.5],
        ] {
            assert_eq!(Fill::new(bad, vec![id(1)], String::new()), None, "{bad:?}");
        }
        assert_eq!(Fill::new(ok, vec![], String::new()), None);

        let mut f = Fill::new(ok, vec![id(1), id(2), id(3), id(4)], "é".repeat(5000)).unwrap();
        assert_eq!(f.variants(), &[id(1), id(2), id(3)]);
        assert_eq!(f.prompt().chars().count(), MAX_PROMPT_CHARS);
        assert_eq!(f.chosen(), id(1));
        f.choose(2);
        assert_eq!(f.chosen(), id(3));
        f.choose(9);
        assert_eq!(f.chosen(), id(1), "out of range shows the first");
        f.choose(1);
        f.keep_chosen();
        assert_eq!(f.variants(), &[id(2)]);
        assert_eq!(f.chosen(), id(2));
    }

    #[test]
    fn a_fill_round_trips_its_json_and_a_bad_choice_costs_only_the_choice() {
        let mut f = Fill::new([0.1, 0.2, 0.3, 0.4], vec![id(1), id(2)], "a boat".into()).unwrap();
        f.choose(1);
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(serde_json::from_str::<Fill>(&json).unwrap(), f);
        let no_prompt = Fill::new([0.1, 0.2, 0.3, 0.4], vec![id(1)], String::new()).unwrap();
        assert!(!serde_json::to_string(&no_prompt)
            .unwrap()
            .contains("prompt"));

        for chosen in ["9", "-1", "1.5", "\"x\"", "null"] {
            let json = format!(
                r#"{{"bbox":[0.1,0.2,0.3,0.4],"variants":["{}","{}"],"chosen":{chosen}}}"#,
                id(1),
                id(2)
            );
            let f: Fill = serde_json::from_str(&json).unwrap();
            assert_eq!(f.chosen(), id(1), "{chosen}");
        }
        assert!(serde_json::from_str::<Fill>(
            r#"{"bbox":[0.5,0.2,0.3,0.4],"variants":["0000000000000001"]}"#
        )
        .is_err());
        assert!(
            serde_json::from_str::<Fill>(r#"{"bbox":[0.1,0.2,0.3,0.4],"variants":["nope"]}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<Fill>(
            r#"{"bbox":[0.1,0.2,1e40,0.4],"variants":["0000000000000001"]}"#
        )
        .is_err());
    }

    #[test]
    fn a_patch_survives_its_png_and_a_bad_png_is_refused() {
        let p = patch(37, 11, 3);
        let png = p.encode().unwrap();
        assert_eq!(Patch::decode(&png, p.id()).unwrap(), p);

        let truncated = &png[..png.len() * 2 / 3];
        assert!(matches!(
            Patch::decode(truncated, p.id()),
            Err(PatchError::Corrupt(_))
        ));

        let other = patch(37, 11, 4).encode().unwrap();
        assert!(
            matches!(Patch::decode(&other, p.id()), Err(PatchError::Corrupt(why)) if why.contains("hashes"))
        );

        let gray = png_of(2, 2, png::ColorType::Grayscale, &[0; 4]);
        assert!(
            matches!(Patch::decode(&gray, p.id()), Err(PatchError::Corrupt(why)) if why.contains("RGBA"))
        );
        let rgb = png_of(2, 2, png::ColorType::Rgb, &[0; 12]);
        assert!(matches!(
            Patch::decode(&rgb, p.id()),
            Err(PatchError::Corrupt(_))
        ));

        let wide = png_of(
            MAX_PATCH_SIDE + 1,
            1,
            png::ColorType::Rgba,
            &vec![0; (MAX_PATCH_SIDE as usize + 1) * 4],
        );
        assert!(
            matches!(Patch::decode(&wide, p.id()), Err(PatchError::Corrupt(why)) if why.contains("size"))
        );
        assert!(matches!(
            Patch::decode(b"not a png", p.id()),
            Err(PatchError::Corrupt(_))
        ));

        assert!(Patch::new(0, 4, vec![]).is_err());
        assert!(Patch::new(2, 2, vec![0; 15]).is_err());
        assert!(Patch::new(MAX_PATCH_SIDE + 1, 1, vec![0; 4 * 2049]).is_err());
        assert_ne!(
            patch(4, 1, 0).id(),
            Patch::new(1, 4, patch(4, 1, 0).rgba().to_vec())
                .unwrap()
                .id(),
            "the size is part of the id"
        );
    }

    const FORMATS: [PixelFormat; 3] = [
        PixelFormat::Srgb8,
        PixelFormat::TonedF16,
        PixelFormat::LinearF16,
    ];

    #[test]
    fn every_grey_level_survives_a_trip_through_the_source_encoding() {
        for format in FORMATS {
            for c in 0..=255u8 {
                let px = from_comfy([c; 3], format);
                assert!(px.iter().all(|v| v.is_finite()), "{format:?} {c}");
                assert_eq!(to_comfy(px, format), [c; 3], "{format:?} {c}");
            }
        }
    }

    #[test]
    fn a_source_pixel_comes_back_within_one_display_step() {
        // The display value of a linear source pixel, unquantised.
        let display = |v: f32, format: PixelFormat| match format {
            PixelFormat::LinearF16 => {
                crate::develop::apply_raw_preview_boost(crate::develop::srgb_encode(v))
            }
            _ => crate::develop::srgb_encode(v),
        };
        for format in FORMATS {
            for i in 0..=1000 {
                let x = i as f32 / 1000.0;
                let px = [x, (x * 0.7).min(1.0), x * x];
                let back = from_comfy(to_comfy(px, format), format);
                for (a, b) in px.iter().zip(back) {
                    let err = (display(*a, format) - display(b, format)).abs();
                    assert!(err <= 0.5 / 255.0 + 1e-4, "{format:?} {a} -> {b}");
                }
            }
        }
    }

    #[test]
    fn a_raw_pixel_encodes_as_the_loupe_shows_it_and_clipped_white_comes_back_finite() {
        for i in 0..=200 {
            let x = i as f32 / 100.0;
            let shown = crate::develop::image_ops::px_to_srgb8(
                PixelFormat::LinearF16,
                &[half::f16::from_f32(x).to_le_bytes(); 3].concat(),
            );
            let [r, g, b] = to_comfy([half::f16::from_f32(x).to_f32(); 3], PixelFormat::LinearF16);
            assert_eq!([r, g, b, 255], shown, "{x}");
        }
        let white = from_comfy([255; 3], PixelFormat::LinearF16);
        let first_white = crate::develop::srgb_decode(15.0 / 16.0);
        assert!(white.iter().all(|&v| (v - first_white).abs() < 1e-6));
        assert_eq!(
            to_comfy([f32::NAN, f32::INFINITY, -1.0], PixelFormat::LinearF16)[2],
            0
        );
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod file_tests {
    use super::*;

    fn temp_folder(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lightphotos-fill-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn solid(v: u8) -> Patch {
        Patch::new(2, 1, vec![v, v, v, 255, v, v, v, 128]).unwrap()
    }

    #[test]
    fn a_written_patch_reads_back_and_a_damaged_file_is_refused() {
        let dir = temp_folder("round-trip");
        let photo = dir.join("IMG_1.ARW");
        let p = solid(40);
        assert_eq!(read(&photo, p.id()), Err(PatchError::Missing));
        write(&photo, &p).unwrap();
        let path = file_path(&photo, p.id()).unwrap();
        assert!(path.starts_with(dir.join(".lightphotos").join("fills")));
        assert_eq!(read(&photo, p.id()).unwrap(), p);

        std::fs::write(&path, b"\x89PNG garbage").unwrap();
        assert!(matches!(read(&photo, p.id()), Err(PatchError::Corrupt(_))));
        std::fs::write(&path, vec![0u8; MAX_PATCH_FILE_BYTES as usize + 1]).unwrap();
        assert!(
            matches!(read(&photo, p.id()), Err(PatchError::Corrupt(why)) if why.contains("limit"))
        );
        remove(&photo, p.id());
        assert_eq!(read(&photo, p.id()), Err(PatchError::Missing));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_sweep_keeps_every_variant_a_fill_lists_and_removes_old_orphans() {
        let dir = temp_folder("sweep");
        let photo = dir.join("a.jpg");
        let (chosen, unchosen, orphan, fresh) = (solid(1), solid(2), solid(3), solid(4));
        for p in [&chosen, &unchosen, &orphan, &fresh] {
            write(&photo, p).unwrap();
        }
        let old = std::time::SystemTime::now() - super::super::side_file::ORPHAN_REAP_AFTER * 2;
        for p in [&chosen, &unchosen, &orphan] {
            std::fs::File::open(file_path(&photo, p.id()).unwrap())
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        let fill = Fill::new(
            [0.1, 0.1, 0.2, 0.2],
            vec![chosen.id(), unchosen.id()],
            String::new(),
        )
        .unwrap();
        sweep_orphans(&dir, |name, id| {
            name == "a.jpg" && fill.variants().contains(&id)
        });

        let exists = |p: &Patch| file_path(&photo, p.id()).unwrap().is_file();
        assert!(exists(&chosen), "chosen: kept");
        assert!(exists(&unchosen), "listed but not chosen: kept");
        assert!(!exists(&orphan), "old orphan: gone");
        assert!(exists(&fresh), "young orphan: kept");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
