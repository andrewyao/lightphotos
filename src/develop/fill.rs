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
use std::sync::Arc;

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
    /// The texels with any alpha, as `(x0, y0, x1, y1)` with the far edges
    /// exclusive; the whole patch when none has.
    painted: [u32; 4],
}

/// The bounds of the texels of `rgba` whose alpha isn't 0.
fn alpha_bounds(width: u32, rgba: &[u8]) -> Option<[u32; 4]> {
    let mut bounds: Option<[u32; 4]> = None;
    for (y, row) in (0u32..).zip(rgba.chunks_exact(width as usize * 4)) {
        let (texels, _) = row.as_chunks::<4>();
        let lit = |px: &[u8; 4]| px[3] > 0;
        let Some(first) = texels.iter().position(lit) else {
            continue;
        };
        let last = texels.iter().rposition(lit).unwrap_or(first);
        let (first, last) = (first as u32, last as u32 + 1);
        bounds = Some(match bounds {
            Some([x0, y0, x1, _]) => [x0.min(first), y0, x1.max(last), y + 1],
            None => [first, y, last, y + 1],
        });
    }
    bounds
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
        let painted = alpha_bounds(width, &rgba).unwrap_or([0, 0, width, height]);
        Ok(Patch {
            width,
            height,
            rgba,
            id,
            painted,
        })
    }

    pub fn id(&self) -> RasterId {
        self.id
    }

    /// The middle of the painted area, the texels with any alpha, inside
    /// `bbox`, the source UV rect the patch covers. A fill's rect is padded
    /// with context and cut at the photo's edges, so its own middle can sit
    /// off the paint.
    pub fn painted_centre(&self, bbox: [f32; 4]) -> [f32; 2] {
        let [x0, y0, x1, y1] = self.painted;
        let [u0, v0, u1, v1] = bbox;
        let mid = |a: u32, b: u32, side: u32| (a + b) as f32 * 0.5 / side as f32;
        [
            u0 + (u1 - u0) * mid(x0, x1, self.width),
            v0 + (v1 - v0) * mid(y0, y1, self.height),
        ]
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

/// One photo's fills as they draw: each rect with the patch it shows, in
/// sidecar order, later on top. A fill whose patch isn't in hand is left
/// out, so the photo shows through there.
#[derive(Clone, Debug, Default)]
pub struct FillSet {
    layers: Vec<([f32; 4], Arc<Patch>)>,
}

impl FillSet {
    pub const EMPTY: FillSet = FillSet { layers: Vec::new() };

    /// The first [`MAX_FILLS`] of `fills`, each with the chosen patch
    /// `patch` hands back for its id.
    pub fn new(fills: &[Fill], mut patch: impl FnMut(RasterId) -> Option<Arc<Patch>>) -> FillSet {
        let layers = fills
            .iter()
            .take(MAX_FILLS)
            .filter_map(|f| Some((f.bbox, patch(f.chosen)?)))
            .collect();
        FillSet { layers }
    }

    /// `fills` with their chosen patches read from beside `photo`. A patch
    /// that is missing or unreadable is logged and its fill left out: the
    /// bake still succeeds, showing the photo there.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn read(photo: &Path, fills: &[Fill]) -> FillSet {
        FillSet::new(fills, |id| match read(photo, id) {
            Ok(patch) => Some(Arc::new(patch)),
            Err(e) => {
                eprintln!("[fill] {}: {e}", photo.display());
                None
            }
        })
    }

    pub fn len(&self) -> usize {
        self.layers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Identity of what draws: each rect and patch id, in order. 0 for none.
    pub fn key(&self) -> u64 {
        if self.layers.is_empty() {
            return 0;
        }
        let mut h = crate::hash::Fnv1a::new();
        for (bbox, patch) in &self.layers {
            for c in bbox {
                h.write(&c.to_bits().to_le_bytes());
            }
            h.write(&patch.id.to_le_bytes());
        }
        h.finish()
    }

    /// The fills ready to composite onto pixels decoded as `format`.
    pub fn on(&self, format: PixelFormat) -> Compositor<'_> {
        Compositor {
            layers: &self.layers,
            lut: from_comfy_lut(format),
        }
    }

    /// Every patch in one RGBA8 image no wider or taller than `max_side`,
    /// for the Loupe's texture. Patches sit on shelves in layer order; if
    /// they don't fit, all are halved until they do.
    pub fn atlas(&self, max_side: u32) -> Atlas {
        // Sixteen patches of 2048 fit 8192 square, four to a shelf, so a
        // larger texture is never needed.
        let max_side = max_side.clamp(1, MAX_PATCH_SIDE * 4);
        let packed = (0..=MAX_PATCH_SIDE.ilog2()).find_map(|shift| {
            let sizes: Vec<(u32, u32)> = self
                .layers
                .iter()
                .map(|(_, p)| ((p.width >> shift).max(1), (p.height >> shift).max(1)))
                .collect();
            shelf_pack(&sizes, max_side)
        });
        let Some((width, height, rects)) = packed else {
            return Atlas::empty(self.layers.len());
        };
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        for ((_, patch), &[x, y, w, h]) in self.layers.iter().zip(&rects) {
            let scaled;
            let pixels = if (w, h) == (patch.width, patch.height) {
                &patch.rgba
            } else {
                scaled = box_shrink(patch, w, h);
                &scaled
            };
            let row_len = w as usize * 4;
            for (row, src) in pixels.chunks_exact(row_len).enumerate() {
                let at = ((y as usize + row) * width as usize + x as usize) * 4;
                if let Some(dst) = rgba.get_mut(at..at + row_len) {
                    dst.copy_from_slice(src);
                }
            }
        }
        Atlas {
            width,
            height,
            rgba,
            rects,
        }
    }

    /// One [`GpuFill`] per layer, pointing into `atlas`.
    pub fn gpu_fills(&self, atlas: &Atlas) -> Vec<GpuFill> {
        self.layers
            .iter()
            .zip(&atlas.rects)
            .map(|((bbox, _), rect)| GpuFill {
                bbox: *bbox,
                atlas_rect: rect.map(|c| c as f32),
            })
            .collect()
    }
}

/// [`FillSet::atlas`]'s image and where each layer's patch sits in it.
pub struct Atlas {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, as [`Patch::rgba`].
    pub rgba: Vec<u8>,
    /// `(x, y, width, height)` in atlas texels, one per layer. A size of 0
    /// is a patch that didn't fit; it draws nothing.
    pub rects: Vec<[u32; 4]>,
}

impl Atlas {
    /// One clear texel, since a texture can't be empty, and no room for
    /// any of `layers` patches.
    fn empty(layers: usize) -> Atlas {
        Atlas {
            width: 1,
            height: 1,
            rgba: vec![0; 4],
            rects: vec![[0; 4]; layers],
        }
    }
}

/// `sizes` placed left to right on shelves inside `max_side` square: the
/// used `(width, height)` and each rect, or `None` if they don't fit.
fn shelf_pack(sizes: &[(u32, u32)], max_side: u32) -> Option<(u32, u32, Vec<[u32; 4]>)> {
    let (mut x, mut y, mut shelf, mut used) = (0u32, 0u32, 0u32, 0u32);
    let mut rects = Vec::with_capacity(sizes.len());
    for &(w, h) in sizes {
        if w > max_side {
            return None;
        }
        if x.checked_add(w)? > max_side {
            y = y.checked_add(shelf)?;
            (x, shelf) = (0, 0);
        }
        rects.push([x, y, w, h]);
        x = x.checked_add(w)?;
        shelf = shelf.max(h);
        used = used.max(x);
    }
    let height = y.checked_add(shelf)?;
    (height <= max_side).then_some((used.max(1), height.max(1), rects))
}

/// `patch` shrunk to `w x h` (each at least 1 and no larger than the
/// patch), every output texel the average of the source box it covers.
fn box_shrink(patch: &Patch, w: u32, h: u32) -> Vec<u8> {
    let (sw, sh) = (patch.width as usize, patch.height as usize);
    let (w, h) = (w as usize, h as usize);
    let span = |o: usize, d: usize, s: usize| {
        let start = o * s / d;
        (start, ((o + 1) * s).div_ceil(d).clamp(start + 1, s))
    };
    let mut out = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let (y0, y1) = span(y, h, sh);
        for x in 0..w {
            let (x0, x1) = span(x, w, sw);
            let mut sum = [0u32; 4];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let px = patch.rgba.get((sy * sw + sx) * 4..).unwrap_or(&[]);
                    for (s, &c) in sum.iter_mut().zip(px) {
                        *s += c as u32;
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            out.extend(sum.map(|s| ((s + n / 2) / n) as u8));
        }
    }
    out
}

/// One fill as laid out in the GPU storage buffer. Must match `Fill` in
/// loupe_common.wgsl.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GpuFill {
    /// Source UV `(u0, v0, u1, v1)`.
    pub bbox: [f32; 4],
    /// The patch's `(x, y, width, height)` in atlas texels.
    pub atlas_rect: [f32; 4],
}

/// [`from_comfy`] for every byte value: the curve is per channel, so one
/// table serves all three. A bake builds it once, and the Loupe uploads it.
pub fn from_comfy_lut(format: PixelFormat) -> [f32; 256] {
    std::array::from_fn(|c| from_comfy([c as u8; 3], format)[0])
}

/// A [`FillSet`] with the table that decodes its patches for one pixel
/// format.
pub struct Compositor<'a> {
    layers: &'a [([f32; 4], Arc<Patch>)],
    lut: [f32; 256],
}

impl Compositor<'_> {
    /// Draw each fill over `rgb`, the linear source pixel at UV `(u, v)`.
    /// Inside a fill's rect its patch is read bilinearly, the four texels
    /// around `t * size - 0.5` clamped at the edge, each decoded to linear
    /// light before blending, then mixed over `rgb` by its alpha. Outside
    /// every rect `rgb` comes back as it was. Must match `applyFills` in
    /// loupe_common.wgsl.
    pub fn apply(&self, u: f32, v: f32, rgb: [f32; 3]) -> [f32; 3] {
        let mut out = rgb;
        for (bbox, patch) in self.layers {
            let [u0, v0, u1, v1] = *bbox;
            // Also false for a NaN.
            if !(u0 <= u && u <= u1 && v0 <= v && v <= v1) {
                continue;
            }
            let w = patch.width as usize;
            let texel = |x: i64, y: i64| {
                let i = (y as usize * w + x as usize) * 4;
                match patch.rgba.get(i..i + 4) {
                    Some(&[r, g, b, a]) => [
                        self.lut[r as usize],
                        self.lut[g as usize],
                        self.lut[b as usize],
                        a as f32 / 255.0,
                    ],
                    _ => [0.0; 4],
                }
            };
            let [r, g, b, a] = super::base::bilinear(
                patch.width,
                patch.height,
                (u - u0) / (u1 - u0),
                (v - v0) / (v1 - v0),
                texel,
            );
            for (o, s) in out.iter_mut().zip([r, g, b]) {
                *o = *o * (1.0 - a) + s * a;
            }
        }
        out
    }
}

/// What a bake job carries, as [`super::MaskInput`] carries masks: native
/// jobs read the patches on their worker, so a queued export holds no
/// pixels and the UI thread decodes no PNG.
#[derive(Clone)]
pub enum FillInput {
    Ready(FillSet),
    #[cfg(not(target_arch = "wasm32"))]
    OnDisk(Vec<Fill>),
}

impl FillInput {
    pub fn none() -> FillInput {
        FillInput::Ready(FillSet::default())
    }

    /// The fills with their patches, read from beside `photo` if needed.
    /// Never fails: see [`FillSet::read`].
    pub fn resolve(self, photo: &std::path::Path) -> FillSet {
        // Only the native variant reads beside the photo.
        let _ = photo;
        match self {
            FillInput::Ready(set) => set,
            #[cfg(not(target_arch = "wasm32"))]
            FillInput::OnDisk(fills) => FillSet::read(photo, &fills),
        }
    }
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
    fn a_patch_knows_the_middle_of_its_paint() {
        // 4x2, alpha only in the left column's top texel.
        let mut rgba = vec![0u8; 4 * 2 * 4];
        rgba[3] = 255;
        let patch = Patch::new(4, 2, rgba).unwrap();
        assert_eq!(patch.painted_centre([0.0, 0.0, 1.0, 1.0]), [0.125, 0.25]);
        assert_eq!(patch.painted_centre([0.2, 0.4, 0.6, 0.8]), [0.25, 0.5]);
        // No alpha at all: the rect's own middle.
        let clear = Patch::new(4, 2, vec![0u8; 32]).unwrap();
        assert_eq!(clear.painted_centre([0.0, 0.0, 1.0, 1.0]), [0.5, 0.5]);
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
mod set_tests {
    use super::*;

    /// An opaque `w x h` patch of gray `v`.
    fn flat(w: u32, h: u32, v: u8) -> Arc<Patch> {
        Arc::new(Patch::new(w, h, [v, v, v, 255].repeat((w * h) as usize)).unwrap())
    }

    fn set_of(patches: &[Arc<Patch>]) -> FillSet {
        let fills: Vec<Fill> = patches
            .iter()
            .map(|p| Fill::new([0.1, 0.1, 0.9, 0.9], vec![p.id()], String::new()).unwrap())
            .collect();
        FillSet::new(&fills, |id| patches.iter().find(|p| p.id() == id).cloned())
    }

    #[test]
    fn a_set_leaves_out_a_fill_without_its_patch_and_keys_on_what_draws() {
        let (a, b) = (flat(2, 2, 10), flat(2, 2, 20));
        let fills: Vec<Fill> = [&a, &b]
            .iter()
            .map(|p| Fill::new([0.1, 0.1, 0.9, 0.9], vec![p.id()], String::new()).unwrap())
            .collect();
        let both = set_of(&[a.clone(), b.clone()]);
        let only_b = FillSet::new(&fills, |id| (id == b.id()).then(|| b.clone()));
        assert_eq!((both.len(), only_b.len()), (2, 1));
        assert_eq!(FillSet::default().key(), 0);
        assert_ne!(both.key(), only_b.key());
        assert_ne!(
            both.key(),
            set_of(&[b, a]).key(),
            "order is what draws on top"
        );
        // Where `only_b` draws, it is patch b alone.
        let px = only_b.on(PixelFormat::Srgb8).apply(0.5, 0.5, [0.0; 3]);
        assert_eq!(px, from_comfy([20; 3], PixelFormat::Srgb8));
        assert_eq!(
            only_b
                .on(PixelFormat::Srgb8)
                .apply(f32::NAN, 0.5, [0.25; 3]),
            [0.25; 3],
            "a NaN coordinate is outside every rect"
        );
    }

    /// Each rect's texels in the atlas, by their red byte.
    fn texels(atlas: &Atlas, [x, y, w, h]: [u32; 4]) -> Vec<u8> {
        let mut out = Vec::new();
        for row in y..y + h {
            for col in x..x + w {
                out.push(atlas.rgba[((row * atlas.width + col) * 4) as usize]);
            }
        }
        out
    }

    #[test]
    fn the_atlas_holds_every_patch_whole_and_apart() {
        let patches: Vec<Arc<Patch>> = [(5, 3), (4, 7), (6, 2), (3, 3)]
            .iter()
            .enumerate()
            .map(|(i, &(w, h))| flat(w, h, 10 * (i as u8 + 1)))
            .collect();
        let set = set_of(&patches);
        // 10 wide: two patches to a shelf.
        let atlas = set.atlas(10);
        assert!(atlas.width <= 10 && atlas.height <= 10);
        assert_eq!(atlas.rgba.len(), (atlas.width * atlas.height * 4) as usize);
        assert_eq!(
            atlas.rects,
            [[0, 0, 5, 3], [5, 0, 4, 7], [0, 7, 6, 2], [6, 7, 3, 3]]
        );
        for (i, (rect, p)) in atlas.rects.iter().zip(&patches).enumerate() {
            assert_eq!((rect[2], rect[3]), (p.width(), p.height()));
            let want = vec![10 * (i as u8 + 1); (rect[2] * rect[3]) as usize];
            assert_eq!(texels(&atlas, *rect), want, "patch {i}");
        }
        let gpu = set.gpu_fills(&atlas);
        assert_eq!(gpu.len(), 4);
        assert_eq!(gpu[1].atlas_rect, [5.0, 0.0, 4.0, 7.0]);
        assert_eq!(gpu[1].bbox, [0.1, 0.1, 0.9, 0.9]);
    }

    #[test]
    fn patches_too_large_for_the_texture_are_halved_until_they_fit() {
        let set = set_of(&[flat(64, 64, 200), flat(64, 16, 100)]);
        let atlas = set.atlas(40);
        assert_eq!(atlas.rects, [[0, 0, 32, 32], [0, 32, 32, 8]]);
        assert!(texels(&atlas, atlas.rects[0]).iter().all(|&v| v == 200));
        assert!(texels(&atlas, atlas.rects[1]).iter().all(|&v| v == 100));
        // A texture of one texel holds one patch at 1x1, and not two.
        assert_eq!(set_of(&[flat(64, 64, 200)]).atlas(1).rects, [[0, 0, 1, 1]]);
        let none = set.atlas(1);
        assert_eq!((none.width, none.height), (1, 1));
        assert_eq!(none.rects, [[0; 4]; 2], "size 0 draws nothing");
        let empty = FillSet::default().atlas(8192);
        assert_eq!((empty.width, empty.height, empty.rgba.len()), (1, 1, 4));
    }

    #[test]
    fn sixteen_of_the_largest_patches_fit_the_smallest_webgpu_texture() {
        let sizes = [(MAX_PATCH_SIDE, MAX_PATCH_SIDE); MAX_FILLS];
        let (w, h, rects) = shelf_pack(&sizes, 8192).unwrap();
        assert_eq!((w, h), (8192, 8192));
        assert_eq!(rects[15], [6144, 6144, 2048, 2048]);
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
