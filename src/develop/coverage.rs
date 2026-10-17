// SPDX-License-Identifier: MIT OR Apache-2.0

//! A detected mask's pixels: one byte of coverage per texel, in source UV
//! (decoded, EXIF-upright; before straighten, crop and quarter turns), the
//! same space as every other mask, so one bitmap serves the Loupe and every
//! bake at any resolution.
//!
//! A coverage is content-addressed. Its [`RasterId`] names its file under
//! `.lightphotos/masks/`, enters the edit signature, and is checked on every
//! read, so a renamed, truncated or edited file is refused rather than
//! misapplied. Files are write-once: two writers of the same detection
//! converge on the same bytes, and nothing ever updates one in place.

use std::ffi::{OsStr, OsString};
use std::fmt;
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::mask::DetectTarget;
use crate::hash::Fnv1a;

/// The longest side a stored coverage may have. A detector's output is
/// resampled down to it; a file claiming more is refused. Vision's Accurate
/// person mask on a 3000x3810 photo measures 1512x2016, which fits whole.
pub const MAX_COVERAGE_SIDE: u32 = 2048;
/// A coverage file larger than this on disk is refused before decoding. A
/// gray PNG at the largest side is 4 MiB uncompressed, so this holds one
/// that compresses badly and refuses anything that isn't a coverage.
pub const MAX_COVERAGE_FILE_BYTES: u64 = 8 << 20;
/// The most bytes the Loupe's packed coverage buffer can hold. A photo has
/// one detection per target, which every mask of that target shares, at up
/// to the largest side; each other mask may be a brush, whose raster is at
/// most `brush::MAX_BRUSH_TEXELS`. The worst case, both targets and 14
/// brushes, is 22 MiB, well inside WebGPU's 128 MiB storage binding
/// default. Offsets reach the GPU as u32s, so passing 2^24 is fine.
pub const MAX_COVERAGE_BYTES: usize =
    DetectTarget::ALL.len() * (MAX_COVERAGE_SIDE as usize) * (MAX_COVERAGE_SIDE as usize)
        + (super::MAX_MASKS - DetectTarget::ALL.len()) * super::brush::MAX_BRUSH_TEXELS;
/// The subdirectory of `.lightphotos/` that holds coverage files. Its own
/// directory, so neither the thumbnail cache's name parser nor the sidecar
/// scan ever sees one.
pub const COVERAGE_DIR: &str = "masks";
const FILE_EXT: &str = "png";
/// An orphaned coverage file younger than this is kept: a detection that has
/// written its file but not yet its sidecar looks like one.
#[cfg(not(target_arch = "wasm32"))]
const ORPHAN_REAP_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);

/// Content hash of one coverage: FNV-1a over its width, height and bytes.
/// Serialized as 16 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RasterId(u64);

impl RasterId {
    fn of(width: u32, height: u32, alpha: &[u8]) -> RasterId {
        let mut h = Fnv1a::new();
        h.write(&width.to_le_bytes());
        h.write(&height.to_le_bytes());
        h.write(alpha);
        RasterId(h.finish())
    }

    /// The id as the edit signature hashes it.
    pub fn to_le_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }
}

impl fmt::Display for RasterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl FromStr for RasterId {
    type Err = ();
    fn from_str(s: &str) -> Result<RasterId, ()> {
        if s.len() != 16 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(());
        }
        u64::from_str_radix(s, 16).map(RasterId).map_err(|_| ())
    }
}

impl Serialize for RasterId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RasterId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<RasterId, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse()
            .map_err(|()| serde::de::Error::custom("a raster id is 16 hex digits"))
    }
}

/// The detections a photo has, one per target: a per-photo fact like its
/// score, not an edit. Masks reference it by target, so a pasted or undone
/// mask finds the photo's own detection, and undo never rewrites it. A value
/// that isn't 16 hex digits loads as `None` (re-detected), never failing the
/// record.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Detections {
    pub subject: Option<RasterId>,
    pub sky: Option<RasterId>,
    /// Targets a later build added, with their ids, kept through every
    /// write of this one so its masks stay whole, and so the orphan sweep
    /// keeps their files. Nothing here reads them.
    extra: std::collections::BTreeMap<String, RasterId>,
}

impl Serialize for Detections {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let known = DetectTarget::ALL
            .iter()
            .zip(DetectTarget::NAMES)
            .filter_map(|(&t, name)| self.get(t).map(|id| (name, id)));
        let extra = self.extra.iter().map(|(name, &id)| (name.as_str(), id));
        let mut map = serializer.serialize_map(None)?;
        for (name, id) in known.chain(extra) {
            map.serialize_entry(name, &id)?;
        }
        map.end()
    }
}

/// Whatever JSON the sidecar holds under `detections`, read for the valid
/// ids in it: a number, a string, a list or `null` in place of the object,
/// or in place of an id, costs that id and nothing else. A sidecar comes
/// from whoever supplied the folder, and a bad detection must never cost
/// the photo its rating. A key this build doesn't know keeps its id.
impl<'de> Deserialize<'de> for Detections {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Detections, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let mut out = Detections::default();
        let Some(object) = value.as_object() else {
            return Ok(out);
        };
        for (key, v) in object {
            let Some(id) = v.as_str().and_then(|s| s.parse::<RasterId>().ok()) else {
                continue;
            };
            match DetectTarget::NAMES.iter().position(|n| n == key) {
                Some(i) => out.set(DetectTarget::ALL[i], Some(id)),
                None => {
                    out.extra.insert(key.clone(), id);
                }
            }
        }
        Ok(out)
    }
}

impl Detections {
    pub fn get(&self, target: DetectTarget) -> Option<RasterId> {
        match target {
            DetectTarget::Subject => self.subject,
            DetectTarget::Sky => self.sky,
        }
    }

    pub fn set(&mut self, target: DetectTarget, id: Option<RasterId>) {
        match target {
            DetectTarget::Subject => self.subject = id,
            DetectTarget::Sky => self.sky = id,
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Detections::default()
    }

    /// Whether any target records `id`, a later build's included, for the
    /// orphan sweep.
    pub fn names(&self, id: RasterId) -> bool {
        DetectTarget::ALL.iter().any(|&t| self.get(t) == Some(id))
            || self.extra.values().any(|&e| e == id)
    }
}

/// Why a coverage couldn't be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoverageError {
    /// No file for this id: the folder was copied without `.lightphotos/masks`.
    Missing,
    /// Unreadable, not an 8-bit gray PNG, too large, or its hash isn't its id.
    Corrupt(String),
    Io(String),
}

impl fmt::Display for CoverageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoverageError::Missing => write!(f, "the mask's coverage file is missing"),
            CoverageError::Corrupt(why) => {
                write!(f, "the mask's coverage file is unreadable: {why}")
            }
            CoverageError::Io(why) => write!(f, "{why}"),
        }
    }
}

/// A validated coverage: `1..=MAX_COVERAGE_SIDE` on each side, `width *
/// height` bytes, and an id that is the hash of those. Private fields, so no
/// other shape can exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coverage {
    width: u32,
    height: u32,
    alpha: Vec<u8>,
    id: RasterId,
}

impl Coverage {
    /// A detector's plane at any size, resampled to fit [`MAX_COVERAGE_SIDE`].
    /// `Err` for an empty or short plane.
    pub fn fit(width: u32, height: u32, alpha: &[u8]) -> Result<Coverage, CoverageError> {
        let len = width as usize * height as usize;
        if width == 0 || height == 0 || alpha.len() < len {
            return Err(CoverageError::Corrupt(format!(
                "a {width}x{height} plane needs {len} bytes, got {}",
                alpha.len()
            )));
        }
        let long = width.max(height);
        if long <= MAX_COVERAGE_SIDE {
            return Ok(Coverage::new(width, height, alpha[..len].to_vec()));
        }
        let scale = MAX_COVERAGE_SIDE as f64 / long as f64;
        let w = ((width as f64 * scale).round() as u32).clamp(1, MAX_COVERAGE_SIDE);
        let h = ((height as f64 * scale).round() as u32).clamp(1, MAX_COVERAGE_SIDE);
        let alpha = super::image_ops::resample_bilinear_u8(alpha, width, height, w, h);
        Ok(Coverage::new(w, h, alpha))
    }

    /// A 1x1 zero coverage: what "nothing found" stores, so the answer is
    /// remembered and never re-detected.
    pub fn nothing() -> Coverage {
        Coverage::new(1, 1, vec![0])
    }

    fn new(width: u32, height: u32, alpha: Vec<u8>) -> Coverage {
        let id = RasterId::of(width, height, &alpha);
        Coverage {
            width,
            height,
            alpha,
            id,
        }
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

    /// `width * height` bytes, row-major, no padding.
    pub fn alpha(&self) -> &[u8] {
        &self.alpha
    }

    pub fn is_nothing(&self) -> bool {
        self.alpha.iter().all(|&a| a == 0)
    }

    /// The plane resampled to `width x height`, for a detector that reads
    /// another target's coverage at its own preview size.
    pub fn resampled(&self, width: u32, height: u32) -> Vec<u8> {
        super::image_ops::resample_bilinear_u8(&self.alpha, self.width, self.height, width, height)
    }

    /// Coverage at source `uv`, 0..1: bilinear between the four texels around
    /// `uv * size - 0.5`, clamped at the edge, through the arithmetic
    /// `fieldAt` already matches. Must match `coverageAt` in
    /// loupe_common.wgsl.
    pub fn at_uv(&self, uv: [f32; 2]) -> f32 {
        if !uv[0].is_finite() || !uv[1].is_finite() {
            return 0.0;
        }
        let w = self.width as usize;
        let texel = |x: i64, y: i64| {
            let a = self
                .alpha
                .get(y as usize * w + x as usize)
                .copied()
                .unwrap_or(0);
            [a as f32 / 255.0, 0.0, 0.0]
        };
        super::base::bilinear(self.width, self.height, uv[0], uv[1], texel)[0]
    }

    /// The file's bytes: an 8-bit grayscale PNG. It compresses a mask about
    /// 20x and opens in any image viewer.
    pub fn encode(&self) -> Result<Vec<u8>, CoverageError> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, self.width, self.height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| CoverageError::Io(e.to_string()))?;
        writer
            .write_image_data(&self.alpha)
            .map_err(|e| CoverageError::Io(e.to_string()))?;
        drop(writer);
        Ok(out)
    }

    /// A file's bytes back to a coverage, refused unless it is an 8-bit gray
    /// PNG within [`MAX_COVERAGE_SIDE`] whose hash is `expected`.
    pub fn decode(bytes: &[u8], expected: RasterId) -> Result<Coverage, CoverageError> {
        let corrupt = |e: png::DecodingError| CoverageError::Corrupt(e.to_string());
        let side = MAX_COVERAGE_SIDE as usize;
        // The frame plus the decoder's own row buffers.
        let limits = png::Limits {
            bytes: side * side * 2,
        };
        let decoder = png::Decoder::new_with_limits(std::io::Cursor::new(bytes), limits);
        let mut reader = decoder.read_info().map_err(corrupt)?;
        let info = reader.info();
        let (width, height) = (info.width, info.height);
        if width == 0 || height == 0 || width > MAX_COVERAGE_SIDE || height > MAX_COVERAGE_SIDE {
            return Err(CoverageError::Corrupt(format!(
                "{width}x{height} is not a coverage size"
            )));
        }
        if info.color_type != png::ColorType::Grayscale || info.bit_depth != png::BitDepth::Eight {
            return Err(CoverageError::Corrupt(format!(
                "{:?} {:?} is not 8-bit grayscale",
                info.color_type, info.bit_depth
            )));
        }
        let len = width as usize * height as usize;
        if reader.output_buffer_size() != Some(len) {
            return Err(CoverageError::Corrupt("unexpected frame size".into()));
        }
        let mut alpha = vec![0u8; len];
        reader.next_frame(&mut alpha).map_err(corrupt)?;
        let coverage = Coverage::new(width, height, alpha);
        if coverage.id != expected {
            return Err(CoverageError::Corrupt(format!(
                "its content hashes to {}, not {expected}",
                coverage.id
            )));
        }
        Ok(coverage)
    }
}

/// The coverage file's name inside `.lightphotos/masks/`:
/// `<photo file name>.<id>.png`, such as `IMG_0042.CR3.5be1c0a39d2f7e14.png`.
pub fn file_name(photo_name: &OsStr, id: RasterId) -> OsString {
    let mut name = photo_name.to_os_string();
    name.push(format!(".{id}.{FILE_EXT}"));
    name
}

/// The reverse of [`file_name`]. `None` for anything else, so a sweep leaves
/// other files alone.
pub fn parse_file_name(name: &OsStr) -> Option<(OsString, RasterId)> {
    let rest = name.to_str()?.strip_suffix(&format!(".{FILE_EXT}"))?;
    let (photo, hex) = rest.rsplit_once('.')?;
    if photo.is_empty() {
        return None;
    }
    Some((OsString::from(photo), hex.parse().ok()?))
}

/// `<dir>/.lightphotos/masks/<name>` for the photo at `photo`.
#[cfg(not(target_arch = "wasm32"))]
fn file_path(photo: &Path, id: RasterId) -> Result<std::path::PathBuf, CoverageError> {
    let dir = photo.parent().ok_or(CoverageError::Missing)?;
    let name = photo.file_name().ok_or(CoverageError::Missing)?;
    Ok(dir
        .join(crate::sidecar::SIDECAR_DIR)
        .join(COVERAGE_DIR)
        .join(file_name(name, id)))
}

/// The coverage `id` stored for `photo`.
#[cfg(not(target_arch = "wasm32"))]
pub fn read(photo: &Path, id: RasterId) -> Result<Coverage, CoverageError> {
    let path = file_path(photo, id)?;
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(CoverageError::Missing),
        Err(e) => return Err(CoverageError::Io(format!("{}: {e}", path.display()))),
    };
    let len = file
        .metadata()
        .map_err(|e| CoverageError::Io(e.to_string()))?
        .len();
    if len > MAX_COVERAGE_FILE_BYTES {
        return Err(CoverageError::Corrupt(format!(
            "{len} bytes is over the {MAX_COVERAGE_FILE_BYTES}-byte limit"
        )));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    use std::io::Read;
    (&file)
        .take(MAX_COVERAGE_FILE_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| CoverageError::Io(e.to_string()))?;
    Coverage::decode(&bytes, id)
}

/// Stores `coverage` beside `photo`, atomically. A file already there has
/// the same name, so the same bytes: nothing is written.
#[cfg(not(target_arch = "wasm32"))]
pub fn write(photo: &Path, coverage: &Coverage) -> Result<(), CoverageError> {
    let path = file_path(photo, coverage.id)?;
    if path.is_file() {
        return Ok(());
    }
    let bytes = coverage.encode()?;
    crate::paths::write_atomic_in_dir(&path, &bytes)
        .map_err(|e| CoverageError::Io(format!("{}: {e}", path.display())))
}

/// Removes the file for `id`, when a read found it corrupt: content-addressed
/// writes skip an existing name, so a re-detect could not replace it.
#[cfg(not(target_arch = "wasm32"))]
pub fn remove(photo: &Path, id: RasterId) {
    if let Ok(path) = file_path(photo, id) {
        let _ = crate::paths::remove_if_present(&path);
    }
}

/// Removes coverage files in `dir`'s folder that `named` doesn't claim, and
/// stale write temporaries, each older than an hour, so a detection that has
/// written its file but not yet its sidecar keeps it. `named(photo, id)`
/// says whether the photo's sidecar records `id`.
#[cfg(not(target_arch = "wasm32"))]
pub fn sweep_orphans(dir: &Path, named: impl Fn(&OsStr, RasterId) -> bool) {
    let Ok(entries) = std::fs::read_dir(dir.join(crate::sidecar::SIDECAR_DIR).join(COVERAGE_DIR))
    else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let orphan = match name.strip_suffix(".tmp") {
            Some(stem) => {
                parse_file_name(OsStr::new(crate::paths::atomic_tmp_target(stem))).is_some()
            }
            None => match parse_file_name(OsStr::new(name)) {
                Some((photo, id)) => !named(&photo, id),
                None => false,
            },
        };
        if orphan && is_older_than(&entry, ORPHAN_REAP_AFTER) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `false` when the mtime is unreadable or in the future, so unknown files
/// are kept.
#[cfg(not(target_arch = "wasm32"))]
fn is_older_than(entry: &std::fs::DirEntry, age: std::time::Duration) -> bool {
    entry
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
        .is_some_and(|elapsed| elapsed > age)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32) -> Coverage {
        let alpha: Vec<u8> = (0..w * h)
            .map(|i| ((i % w) * 255 / (w - 1).max(1)) as u8)
            .collect();
        Coverage::fit(w, h, &alpha).unwrap()
    }

    #[test]
    fn the_id_is_the_hash_of_size_and_bytes_and_round_trips_as_hex() {
        let a = Coverage::fit(2, 2, &[0, 255, 255, 0]).unwrap();
        let same = Coverage::fit(2, 2, &[0, 255, 255, 0, 9, 9]).unwrap();
        assert_eq!(a.id(), same.id(), "extra bytes past the plane are ignored");
        let transposed = Coverage::fit(4, 1, &[0, 255, 255, 0]).unwrap();
        assert_ne!(a.id(), transposed.id());
        assert_ne!(a.id(), Coverage::fit(2, 2, &[0, 255, 255, 1]).unwrap().id());

        let hex = a.id().to_string();
        assert_eq!(hex.len(), 16);
        assert_eq!(hex.parse::<RasterId>(), Ok(a.id()));
        let json = serde_json::to_string(&a.id()).unwrap();
        assert_eq!(json, format!("\"{hex}\""));
        assert_eq!(serde_json::from_str::<RasterId>(&json).unwrap(), a.id());
        assert!("zz".parse::<RasterId>().is_err());
        assert!(serde_json::from_str::<RasterId>("\"0123456789abcdef0\"").is_err());
    }

    #[test]
    fn detections_load_a_bad_id_as_none_and_write_only_what_is_set() {
        let d: Detections =
            serde_json::from_str(r#"{"subject":"0123456789abcdef","sky":"nope"}"#).unwrap();
        assert_eq!(d.subject, Some(RasterId(0x0123_4567_89ab_cdef)));
        assert_eq!(d.sky, None);
        assert_eq!(
            serde_json::to_string(&d).unwrap(),
            r#"{"subject":"0123456789abcdef"}"#
        );
        assert!(d.names(RasterId(0x0123_4567_89ab_cdef)));
        assert!(!d.names(RasterId(1)));
        assert!(!d.is_empty() && Detections::default().is_empty());
        let mut e = d.clone();
        e.set(DetectTarget::Subject, None);
        assert!(e.is_empty());

        // A later build's target keeps its id through this build's write,
        // and the sweep sees its file as named; a bad extra is dropped.
        let later: Detections = serde_json::from_str(
            r#"{"person":"fedcba9876543210","sky":"0123456789abcdef","water":"nope"}"#,
        )
        .unwrap();
        assert_eq!(
            later.get(DetectTarget::Sky),
            Some(RasterId(0x0123_4567_89ab_cdef))
        );
        assert!(later.names(RasterId(0xfedc_ba98_7654_3210)));
        assert!(!later.is_empty());
        assert_eq!(
            serde_json::to_string(&later).unwrap(),
            r#"{"sky":"0123456789abcdef","person":"fedcba9876543210"}"#
        );
        let mut only_extra = later.clone();
        only_extra.set(DetectTarget::Sky, None);
        assert!(!only_extra.is_empty(), "still persists");
        assert_eq!(
            serde_json::from_str::<Detections>(&serde_json::to_string(&only_extra).unwrap())
                .unwrap(),
            only_extra
        );
        assert_eq!(MAX_COVERAGE_BYTES, 22 << 20);
        const {
            assert!(
                MAX_COVERAGE_BYTES <= u32::MAX as usize,
                "offsets must fit a u32"
            );
        }
    }

    #[test]
    fn fit_refuses_a_short_plane_and_shrinks_a_large_one() {
        assert!(Coverage::fit(0, 4, &[]).is_err());
        assert!(Coverage::fit(4, 4, &[0; 15]).is_err());
        let big = Coverage::fit(
            MAX_COVERAGE_SIDE * 2,
            10,
            &vec![200u8; MAX_COVERAGE_SIDE as usize * 20],
        )
        .unwrap();
        assert_eq!((big.width(), big.height()), (MAX_COVERAGE_SIDE, 5));
        assert!(big.alpha().iter().all(|&a| a == 200));
        assert!(Coverage::nothing().is_nothing());
        assert!(!gradient(8, 2).is_nothing());
    }

    #[test]
    fn at_uv_blends_the_four_texels_around_the_sample_and_clamps_at_the_edge() {
        let c = Coverage::fit(2, 1, &[0, 255]).unwrap();
        assert_eq!(c.at_uv([0.25, 0.5]), 0.0, "texel 0's center");
        assert_eq!(c.at_uv([0.75, 0.5]), 1.0, "texel 1's center");
        assert!((c.at_uv([0.5, 0.5]) - 0.5).abs() < 1e-6, "halfway between");
        assert_eq!(
            c.at_uv([-3.0, 7.0]),
            0.0,
            "off the photo clamps to the edge texel"
        );
        assert_eq!(c.at_uv([2.0, 0.5]), 1.0);
        assert_eq!(
            c.at_uv([f32::NAN, 0.5]),
            0.0,
            "a NaN lands on an edge, not a panic"
        );
        let square = Coverage::fit(2, 2, &[0, 255, 255, 0]).unwrap();
        assert!((square.at_uv([0.5, 0.5]) - 0.5).abs() < 1e-6);
        assert_eq!(square.at_uv([0.0, 0.0]), 0.0);
        assert_eq!(square.at_uv([1.0, 0.0]), 1.0);
    }

    #[test]
    fn a_coverage_survives_its_png_and_a_tampered_png_is_refused() {
        let c = gradient(37, 11);
        let png = c.encode().unwrap();
        assert!(
            png.len() < c.alpha().len(),
            "compresses: {} bytes",
            png.len()
        );
        assert_eq!(Coverage::decode(&png, c.id()).unwrap(), c);

        let truncated = &png[..png.len() * 2 / 3];
        assert!(matches!(
            Coverage::decode(truncated, c.id()),
            Err(CoverageError::Corrupt(_))
        ));

        let other = gradient(37, 11);
        let mut edited = other.alpha().to_vec();
        edited[5] ^= 0x80;
        let renamed = Coverage::fit(37, 11, &edited).unwrap().encode().unwrap();
        assert!(
            matches!(Coverage::decode(&renamed, c.id()), Err(CoverageError::Corrupt(why)) if why.contains("hashes")),
            "the hash guards against a renamed or edited file"
        );

        let mut colour = Vec::new();
        let mut enc = png::Encoder::new(&mut colour, 2, 2);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .unwrap()
            .write_image_data(&[0; 12])
            .unwrap();
        assert!(matches!(
            Coverage::decode(&colour, c.id()),
            Err(CoverageError::Corrupt(_))
        ));

        let mut sixteen = Vec::new();
        let mut enc = png::Encoder::new(&mut sixteen, 2, 1);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Sixteen);
        enc.write_header()
            .unwrap()
            .write_image_data(&[0; 4])
            .unwrap();
        assert!(matches!(
            Coverage::decode(&sixteen, c.id()),
            Err(CoverageError::Corrupt(_))
        ));

        // A header claiming a side past the cap is refused before any frame
        // is allocated, so the file can stay tiny.
        let mut wide = Vec::new();
        let mut enc = png::Encoder::new(&mut wide, MAX_COVERAGE_SIDE + 1, 1);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .unwrap()
            .write_image_data(&vec![0; MAX_COVERAGE_SIDE as usize + 1])
            .unwrap();
        assert!(
            matches!(Coverage::decode(&wide, c.id()), Err(CoverageError::Corrupt(why)) if why.contains("size"))
        );
        assert!(matches!(
            Coverage::decode(b"not a png", c.id()),
            Err(CoverageError::Corrupt(_))
        ));
    }

    #[test]
    fn file_names_round_trip_and_reject_other_files() {
        let id = "5be1c0a39d2f7e14".parse::<RasterId>().unwrap();
        let name = file_name(OsStr::new("IMG_0042.CR3"), id);
        assert_eq!(name.to_str().unwrap(), "IMG_0042.CR3.5be1c0a39d2f7e14.png");
        assert_eq!(
            parse_file_name(&name),
            Some((OsString::from("IMG_0042.CR3"), id))
        );
        for other in [
            "IMG_0042.CR3.xmp",
            "IMG_0042.CR3.png",
            ".5be1c0a39d2f7e14.png",
            "IMG_0042.CR3.5be1c0a39d2f7e1.png",
            "IMG_0042.CR3.5be1c0a39d2f7e14.thumb.jpg",
        ] {
            assert_eq!(parse_file_name(OsStr::new(other)), None, "{other}");
        }
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
            "lightphotos-coverage-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_written_coverage_reads_back_once_and_a_second_write_is_a_no_op() {
        let dir = temp_folder("round-trip");
        let photo = dir.join("IMG_1.ARW");
        let c = Coverage::fit(3, 2, &[0, 128, 255, 255, 128, 0]).unwrap();
        assert_eq!(read(&photo, c.id()), Err(CoverageError::Missing));
        write(&photo, &c).unwrap();
        let path = file_path(&photo, c.id()).unwrap();
        assert!(path.starts_with(dir.join(".lightphotos").join("masks")));
        let first = std::fs::metadata(&path).unwrap().modified().unwrap();
        write(&photo, &c).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), first);
        assert_eq!(read(&photo, c.id()).unwrap(), c);

        std::fs::write(&path, b"\x89PNG garbage").unwrap();
        assert!(matches!(
            read(&photo, c.id()),
            Err(CoverageError::Corrupt(_))
        ));
        std::fs::write(&path, vec![0u8; MAX_COVERAGE_FILE_BYTES as usize + 1]).unwrap();
        assert!(
            matches!(read(&photo, c.id()), Err(CoverageError::Corrupt(why)) if why.contains("limit"))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_sweep_removes_old_unnamed_files_and_keeps_the_rest() {
        let dir = temp_folder("sweep");
        let photo = dir.join("a.jpg");
        let kept = Coverage::fit(1, 1, &[7]).unwrap();
        let orphan = Coverage::fit(1, 1, &[8]).unwrap();
        let fresh = Coverage::fit(1, 1, &[9]).unwrap();
        let later = Coverage::fit(1, 1, &[10]).unwrap();
        for c in [&kept, &orphan, &fresh, &later] {
            write(&photo, c).unwrap();
        }
        let masks = dir.join(".lightphotos").join("masks");
        let stale_tmp = masks.join(format!("a.jpg.{}.png.123-4.tmp", orphan.id()));
        let foreign = masks.join("notes.txt");
        std::fs::write(&stale_tmp, b"").unwrap();
        std::fs::write(&foreign, b"").unwrap();
        let old = std::time::SystemTime::now() - ORPHAN_REAP_AFTER * 2;
        for p in [
            file_path(&photo, kept.id()).unwrap(),
            file_path(&photo, orphan.id()).unwrap(),
            file_path(&photo, later.id()).unwrap(),
            stale_tmp.clone(),
            foreign.clone(),
        ] {
            std::fs::File::open(&p).unwrap().set_modified(old).unwrap();
        }

        // Named the way the app names files: by any id the record holds,
        // one of this build's targets or a later build's.
        let record: Detections = serde_json::from_str(&format!(
            r#"{{"subject":"{}","person":"{}"}}"#,
            kept.id(),
            later.id()
        ))
        .unwrap();
        sweep_orphans(&dir, |name, id| name == "a.jpg" && record.names(id));

        assert!(
            file_path(&photo, kept.id()).unwrap().is_file(),
            "named: kept"
        );
        assert!(
            file_path(&photo, later.id()).unwrap().is_file(),
            "named by a later build's target: kept"
        );
        assert!(
            !file_path(&photo, orphan.id()).unwrap().exists(),
            "old orphan: gone"
        );
        assert!(
            file_path(&photo, fresh.id()).unwrap().is_file(),
            "young orphan: kept"
        );
        assert!(!stale_tmp.exists(), "old temp: gone");
        assert!(foreign.is_file(), "not a coverage file: untouched");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
