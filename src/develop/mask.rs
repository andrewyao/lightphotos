// SPDX-License-Identifier: MIT OR Apache-2.0

//! Local adjustments: gradient masks that each carry a few tone and colour
//! sliders. A mask's weight at a pixel scales its [`LocalAmounts`], and the
//! weighted amounts of every mask add to the global sliders of the same name
//! before the tone pipeline reads them. Shapes live in 0..1 source UV, before
//! crop and straighten, so neither moves a mask, the same rule [`TouchUp`]
//! follows.
//!
//! [`TouchUp`]: super::TouchUp

use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[cfg(not(target_arch = "wasm32"))]
use super::coverage::Detections;
use super::coverage::{Coverage, CoverageError, RasterId};
use super::{Adjustments, SliderId, EXPOSURE_RANGE, TONE_RANGE};

/// The most masks one photo can hold: the GPU buffer's size.
pub const MAX_MASKS: usize = 16;

/// The smallest and largest radial radius, as a fraction of the photo's
/// shorter side.
pub const RADIUS_RANGE: std::ops::RangeInclusive<f32> = 1e-4..=4.0;

/// How far off the photo a mask's points may sit, in UV. A gradient may start
/// or end past an edge; anything further is no different on screen, and the
/// bound keeps squared distances finite.
pub(super) const COORD_RANGE: std::ops::RangeInclusive<f32> = -10.0..=11.0;

/// The most strokes one brush mask holds.
pub const MAX_STROKES_PER_MASK: usize = 512;
/// The most stroke points one photo's brush masks hold between them, each
/// stroke counting [`STROKE_POINTS`] more for its settings. The sidecar is
/// pretty-printed JSON, where a point takes about 85 bytes and a stroke's
/// settings about 250, so a photo at the cap writes under 700 KiB, inside
/// `sidecar::MAX_SIDECAR_BYTES` with room for the rest of its record. With
/// points a quarter radius apart that is still some 150 strokes across the
/// photo.
pub const MAX_BRUSH_POINTS: usize = 8_000;
/// What one stroke's own settings weigh against [`MAX_BRUSH_POINTS`].
pub const STROKE_POINTS: usize = 3;
/// The smallest and largest brush radius, as a fraction of the photo's
/// shorter side.
pub const BRUSH_RADIUS_RANGE: std::ops::RangeInclusive<f32> = 1e-3..=1.0;

/// One local adjustment.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Mask {
    pub shape: MaskShape,
    /// Swaps where the mask applies and where it doesn't.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub invert: bool,
    #[serde(default, skip_serializing_if = "LocalAmounts::is_zero")]
    pub amounts: LocalAmounts,
}

/// Where a mask applies. Points are 0..1 source UV; lengths are fractions of
/// the photo's shorter side, so a circle stays round on any aspect.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MaskShape {
    /// Full strength on the `start` side, fading to nothing at `end`, as a
    /// Lightroom linear gradient drags.
    Linear { start: [f32; 2], end: [f32; 2] },
    /// Full strength inside an ellipse of `radii` turned by `angle` radians,
    /// fading out over the outer `feather` share of each radius.
    Radial {
        center: [f32; 2],
        radii: [f32; 2],
        #[serde(default)]
        angle: f32,
        #[serde(default)]
        feather: f32,
    },
    /// Weight is the photo's detected coverage for `target`, read from the
    /// photo's [`Detections`]. No geometry: nothing to drag, nothing to
    /// sanitize. A photo without that detection yet renders it as nothing.
    Detected { target: DetectTarget },
    /// Painted strokes, applied in order: each adds to or erases what the
    /// ones before it painted. `develop::brush` rasterizes them into a
    /// coverage that renders like a detected mask's. An `Arc`, so undo
    /// snapshots and every render's copy share one list.
    Brush { strokes: Arc<[Stroke]> },
}

impl MaskShape {
    /// The `kind` tags this build reads. A sidecar entry with any other tag
    /// is a later build's mask, which `sidecar::MaskList` keeps verbatim.
    pub const KINDS: &[&str] = &["linear", "radial", "detected", "brush"];
}

/// One press-to-release drag of the brush. Every dab along `points` has the
/// same size, feather and flow.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Stroke {
    /// Dab radius, as a fraction of the photo's shorter side.
    pub radius: f32,
    /// The outer share of the radius that fades out, 0..1.
    pub feather: f32,
    /// How much one dab adds, 0..1. Overlapping dabs build toward 1.
    pub flow: f32,
    /// Removes coverage instead of adding it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub erase: bool,
    /// The path in source UV, thinned to about a quarter radius apart.
    pub points: Vec<[f32; 2]>,
}

/// What a detected mask selects. One variant per detector; a photo holds at
/// most one detection per target, which every mask of that target shares.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "lowercase")]
pub enum DetectTarget {
    Subject,
    Sky,
}

impl DetectTarget {
    pub const ALL: [DetectTarget; 2] = [DetectTarget::Subject, DetectTarget::Sky];
    /// The `target` names this build reads, in `ALL`'s order. A detected
    /// mask with any other name is a later build's, which
    /// `sidecar::MaskList` keeps verbatim.
    pub const NAMES: [&str; 2] = ["subject", "sky"];

    /// Position in [`ALL`](Self::ALL), for per-target tables.
    pub fn index(self) -> usize {
        match self {
            DetectTarget::Subject => 0,
            DetectTarget::Sky => 1,
        }
    }
}

/// The sliders a mask carries, in the global sliders' units and ranges.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(default)]
pub struct LocalAmounts {
    #[serde(skip_serializing_if = "is_zero")]
    pub temp: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub tint: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub exposure: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub contrast: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub highlights: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub shadows: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub whites: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub blacks: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub saturation: f32,
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

/// One mask slider: the panel draws [`LOCAL_SLIDERS`] in order, and loading,
/// clamping and the GPU copy walk the same table.
pub struct LocalSlider {
    pub id: SliderId,
    pub field: fn(&mut LocalAmounts) -> &mut f32,
    pub range: std::ops::RangeInclusive<f32>,
    pub decimals: usize,
}

const fn local(id: SliderId, field: fn(&mut LocalAmounts) -> &mut f32) -> LocalSlider {
    LocalSlider {
        id,
        field,
        range: TONE_RANGE,
        decimals: 0,
    }
}

pub const LOCAL_SLIDERS: [LocalSlider; 9] = [
    local(SliderId::Temp, |a| &mut a.temp),
    local(SliderId::Tint, |a| &mut a.tint),
    LocalSlider {
        range: EXPOSURE_RANGE,
        decimals: 2,
        ..local(SliderId::Exposure, |a| &mut a.exposure)
    },
    local(SliderId::Contrast, |a| &mut a.contrast),
    local(SliderId::Highlights, |a| &mut a.highlights),
    local(SliderId::Shadows, |a| &mut a.shadows),
    local(SliderId::Whites, |a| &mut a.whites),
    local(SliderId::Blacks, |a| &mut a.blacks),
    local(SliderId::Saturation, |a| &mut a.saturation),
];

impl LocalSlider {
    pub fn get(&self, amounts: &LocalAmounts) -> f32 {
        let mut a = *amounts;
        *(self.field)(&mut a)
    }
}

impl LocalAmounts {
    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }

    /// The global sliders a mask's amounts add to.
    pub fn of(adj: &Adjustments) -> Self {
        Self {
            temp: adj.temp,
            tint: adj.tint,
            exposure: adj.exposure,
            contrast: adj.contrast,
            highlights: adj.highlights,
            shadows: adj.shadows,
            whites: adj.whites,
            blacks: adj.blacks,
            saturation: adj.saturation,
        }
    }

    /// `self + d · weight` for every slider.
    fn add_weighted(&mut self, d: &LocalAmounts, weight: f32) {
        for s in &LOCAL_SLIDERS {
            *(s.field)(self) += s.get(d) * weight;
        }
    }

    /// Each slider held to its range. Must match the clamps in `localTone`
    /// in loupe_common.wgsl.
    pub fn clamped(mut self) -> Self {
        for s in &LOCAL_SLIDERS {
            let v = (s.field)(&mut self);
            *v = v.clamp(*s.range.start(), *s.range.end());
        }
        self
    }

    fn all_finite(&self) -> bool {
        LOCAL_SLIDERS.iter().all(|s| s.get(self).is_finite())
    }
}

impl Mask {
    /// A new linear mask with no amounts yet.
    pub fn linear(start: [f32; 2], end: [f32; 2]) -> Self {
        Mask {
            shape: MaskShape::Linear { start, end },
            invert: false,
            amounts: LocalAmounts::default(),
        }
    }

    /// A new radial mask with no amounts yet and Lightroom's default feather.
    pub fn radial(center: [f32; 2], radii: [f32; 2]) -> Self {
        Mask {
            shape: MaskShape::Radial {
                center,
                radii,
                angle: 0.0,
                feather: 0.5,
            },
            invert: false,
            amounts: LocalAmounts::default(),
        }
    }

    /// A mask over the photo's detected `target`, with no amounts yet.
    pub fn detected(target: DetectTarget) -> Self {
        Mask {
            shape: MaskShape::Detected { target },
            invert: false,
            amounts: LocalAmounts::default(),
        }
    }

    /// A brush mask with nothing painted yet and no amounts.
    pub fn brush() -> Self {
        Mask {
            shape: MaskShape::Brush {
                strokes: Arc::from([]),
            },
            invert: false,
            amounts: LocalAmounts::default(),
        }
    }

    /// The detection this mask reads, if it is a detected one.
    pub fn target(&self) -> Option<DetectTarget> {
        match self.shape {
            MaskShape::Detected { target } => Some(target),
            MaskShape::Linear { .. } | MaskShape::Radial { .. } | MaskShape::Brush { .. } => None,
        }
    }

    /// The painted strokes, if this is a brush mask.
    pub fn strokes(&self) -> Option<&Arc<[Stroke]>> {
        match &self.shape {
            MaskShape::Brush { strokes } => Some(strokes),
            MaskShape::Linear { .. } | MaskShape::Radial { .. } | MaskShape::Detected { .. } => {
                None
            }
        }
    }

    /// What this mask's strokes weigh toward [`MAX_BRUSH_POINTS`]: their
    /// points, and [`STROKE_POINTS`] for each.
    pub fn brush_points(&self) -> usize {
        self.strokes().map_or(0, |s| {
            s.iter().map(|s| s.points.len() + STROKE_POINTS).sum()
        })
    }

    /// This mask with every number in range, or `None` if one isn't a
    /// number at all. Masks come from sidecars and drive scripts.
    pub fn sanitized(self) -> Option<Mask> {
        self.sanitized_within(&mut { MAX_BRUSH_POINTS })
    }

    /// [`sanitized`](Self::sanitized), keeping at most `points_left` of
    /// [`brush_points`](Self::brush_points) and taking those kept from it,
    /// so a photo's masks share one [`MAX_BRUSH_POINTS`]. A stroke whose
    /// settings aren't numbers is dropped, as is a point that isn't; strokes
    /// and points past a cap are cut.
    pub fn sanitized_within(self, points_left: &mut usize) -> Option<Mask> {
        let point = |p: [f32; 2]| {
            p.iter()
                .all(|v| v.is_finite())
                .then(|| p.map(|v| v.clamp(*COORD_RANGE.start(), *COORD_RANGE.end())))
        };
        if !self.amounts.all_finite() {
            return None;
        }
        let shape = match self.shape {
            MaskShape::Brush { strokes } => {
                let kept: Arc<[Stroke]> = strokes
                    .iter()
                    .take(MAX_STROKES_PER_MASK)
                    .filter_map(|s| {
                        let room = points_left.checked_sub(STROKE_POINTS)?;
                        let stroke = s.sanitized(room)?;
                        *points_left = room.saturating_sub(stroke.points.len());
                        Some(stroke)
                    })
                    .collect();
                // A painted mask that lost every stroke would cover nothing,
                // or everything when inverted: neither is what was painted.
                if kept.is_empty() && !strokes.is_empty() {
                    return None;
                }
                MaskShape::Brush { strokes: kept }
            }
            MaskShape::Linear { start, end } => MaskShape::Linear {
                start: point(start)?,
                end: point(end)?,
            },
            MaskShape::Radial {
                center,
                radii,
                angle,
                feather,
            } => {
                if !radii.iter().all(|r| r.is_finite())
                    || !angle.is_finite()
                    || !feather.is_finite()
                {
                    return None;
                }
                MaskShape::Radial {
                    center: point(center)?,
                    radii: radii.map(|r| r.clamp(*RADIUS_RANGE.start(), *RADIUS_RANGE.end())),
                    angle: angle.rem_euclid(std::f32::consts::TAU),
                    feather: feather.clamp(0.0, 1.0),
                }
            }
            MaskShape::Detected { target } => MaskShape::Detected { target },
        };
        Some(Mask {
            shape,
            invert: self.invert,
            amounts: self.amounts.clamped(),
        })
    }
}

impl Stroke {
    /// This stroke with its numbers in range and at most `max_points`
    /// points, or `None` if a setting isn't a number or no point is left.
    fn sanitized(&self, max_points: usize) -> Option<Stroke> {
        if !(self.radius.is_finite() && self.feather.is_finite() && self.flow.is_finite()) {
            return None;
        }
        let points: Vec<[f32; 2]> = self
            .points
            .iter()
            .filter(|p| p.iter().all(|v| v.is_finite()))
            .take(max_points)
            .map(|p| p.map(|v| v.clamp(*COORD_RANGE.start(), *COORD_RANGE.end())))
            .collect();
        if points.is_empty() {
            return None;
        }
        Some(Stroke {
            radius: self
                .radius
                .clamp(*BRUSH_RADIUS_RANGE.start(), *BRUSH_RADIUS_RANGE.end()),
            feather: self.feather.clamp(0.0, 1.0),
            flow: self.flow.clamp(0.0, 1.0),
            erase: self.erase,
            points,
        })
    }
}

/// How much of `mask` applies at source UV `uv`, 0..1, on a photo whose
/// width over height is `aspect`. `coverage` is the photo's coverage for a
/// detected mask's target, `None` when it has none yet (pending, unsupported,
/// missing): such a mask covers nothing, inverted or not, like a zero-length
/// gradient. A brush mask's `coverage` is its strokes' raster. Must match
/// `maskWeight` in loupe_common.wgsl.
pub fn mask_weight(mask: &Mask, coverage: Option<&Coverage>, uv: [f32; 2], aspect: f32) -> f32 {
    // UV to units of the shorter side.
    let scale = if aspect >= 1.0 {
        [aspect, 1.0]
    } else {
        [1.0, 1.0 / aspect]
    };
    let w = match mask.shape {
        MaskShape::Linear { start, end } => {
            let dir = [
                (end[0] - start[0]) * scale[0],
                (end[1] - start[1]) * scale[1],
            ];
            let len2 = dir[0] * dir[0] + dir[1] * dir[1];
            // A gradient with no length has no direction, so it covers nothing,
            // inverted or not.
            if len2 < 1e-12 {
                return 0.0;
            }
            let p = [(uv[0] - start[0]) * scale[0], (uv[1] - start[1]) * scale[1]];
            let t = ((p[0] * dir[0] + p[1] * dir[1]) / len2).clamp(0.0, 1.0);
            1.0 - t * t * (3.0 - 2.0 * t)
        }
        MaskShape::Radial {
            center,
            radii,
            angle,
            feather,
        } => {
            let q = [
                (uv[0] - center[0]) * scale[0],
                (uv[1] - center[1]) * scale[1],
            ];
            let (s, c) = angle.sin_cos();
            let x = (c * q[0] + s * q[1]) / radii[0];
            let y = (c * q[1] - s * q[0]) / radii[1];
            falloff(x.hypot(y), feather)
        }
        MaskShape::Detected { .. } | MaskShape::Brush { .. } => match coverage {
            Some(c) => c.at_uv(uv),
            None => return 0.0,
        },
    };
    if mask.invert {
        1.0 - w
    } else {
        w
    }
}

/// 1 inside `1 - feather` of the radius, 0 at and past the radius, and a
/// smoothstep between. A zero feather is a hard edge, not a division by zero.
/// Must match `maskFalloff` in loupe_common.wgsl.
pub(super) fn falloff(d: f32, feather: f32) -> f32 {
    if d >= 1.0 {
        return 0.0;
    }
    if d <= 1.0 - feather {
        return 1.0;
    }
    let t = (1.0 - d) / feather;
    t * t * (3.0 - 2.0 * t)
}

/// A photo's masks with the coverage each detected target resolved to:
/// what a bake job carries and every renderer takes. A brush mask's raster
/// depends on the photo's shape, so [`on_photo`](Self::on_photo) adds it
/// where the renderer knows that. Private, so a layer can't carry another
/// target's coverage.
#[derive(Clone, Default)]
pub struct MaskSet {
    layers: Vec<(Mask, Option<Arc<Coverage>>)>,
}

impl MaskSet {
    /// `coverage(target)` is asked once per distinct target the masks use.
    pub fn new(
        masks: &[Mask],
        mut coverage: impl FnMut(DetectTarget) -> Option<Arc<Coverage>>,
    ) -> MaskSet {
        let mut asked: [Option<Option<Arc<Coverage>>>; 2] = [None, None];
        let layers = masks
            .iter()
            .map(|m| {
                let c = m
                    .target()
                    .and_then(|t| asked[t.index()].get_or_insert_with(|| coverage(t)).clone());
                (m.clone(), c)
            })
            .collect();
        MaskSet { layers }
    }

    /// Reads each used target's coverage from `.lightphotos/masks`. A target
    /// with no detection yet resolves to `None` (renders nothing; the edit
    /// signature says so, so the bake re-runs when it lands). A recorded id
    /// whose file is missing or corrupt is an `Err`: the caller must not
    /// cache a bake that silently lacks a mask the signature claims.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn read(
        photo: &std::path::Path,
        masks: &[Mask],
        detections: &Detections,
    ) -> Result<MaskSet, CoverageError> {
        let mut failed = None;
        let set = MaskSet::new(masks, |t| {
            let id = detections.get(t)?;
            match super::coverage::read(photo, id) {
                Ok(c) => Some(Arc::new(c)),
                Err(e) => {
                    failed.get_or_insert(e);
                    None
                }
            }
        });
        match failed {
            Some(e) => Err(e),
            None => Ok(set),
        }
    }

    /// The masks placed on a photo whose width over height is `aspect`, each
    /// brush mask with its strokes' raster. What the Loupe uploads and every
    /// bake and histogram samples, so none can render a brush mask without
    /// its strokes.
    pub fn on_photo(&self, aspect: f32) -> PhotoMasks {
        let layers = self
            .layers
            .iter()
            .map(|(m, c)| {
                let c = match m.strokes() {
                    Some(strokes) => Some(super::brush::coverage(strokes, aspect)),
                    None => c.clone(),
                };
                (m.clone(), c)
            })
            .collect();
        PhotoMasks { layers, aspect }
    }
}

/// A [`MaskSet`] on a photo of a known shape: every mask with the coverage
/// it renders with, detected or painted.
#[derive(Clone, Default)]
pub struct PhotoMasks {
    layers: Vec<(Mask, Option<Arc<Coverage>>)>,
    /// The photo's width over height, which lengths in a mask are scaled by.
    aspect: f32,
}

impl PhotoMasks {
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// The sliders at source UV `uv`: `base` plus every mask's weighted
    /// amounts, each held to its range. Must match `localTone` in
    /// loupe_common.wgsl.
    pub fn local_tone(&self, base: LocalAmounts, uv: [f32; 2]) -> LocalAmounts {
        let mut out = base;
        for (m, c) in &self.layers {
            let w = mask_weight(m, c.as_deref(), uv, self.aspect);
            if w > 0.0 {
                out.add_weighted(&m.amounts, w);
            }
        }
        out.clamped()
    }

    /// The distinct coverages in first-use order, which `pack_coverage`
    /// lays out and `gpu_masks` points into.
    fn distinct(&self) -> Vec<&Arc<Coverage>> {
        let mut out: Vec<&Arc<Coverage>> = Vec::new();
        for c in self.layers.iter().filter_map(|(_, c)| c.as_ref()) {
            if !out.iter().any(|d| d.id() == c.id()) {
                out.push(c);
            }
        }
        out
    }

    /// GPU records, coverage ones pointing into `pack_coverage`'s layout.
    /// Two masks of one raster share one copy of its bytes.
    pub fn gpu_masks(&self) -> Vec<GpuMask> {
        let mut offsets: Vec<(RasterId, usize)> = Vec::new();
        let mut next = 0usize;
        for c in self.distinct() {
            offsets.push((c.id(), next));
            next += c.alpha().len();
        }
        self.layers
            .iter()
            .map(|(m, c)| {
                let mut g = GpuMask::from(m);
                if let Some(c) = c {
                    let off = offsets
                        .iter()
                        .find(|(id, _)| *id == c.id())
                        .map_or(0, |(_, off)| *off);
                    // Below `MAX_COVERAGE_BYTES`, so it fits.
                    let off = u32::try_from(off).unwrap_or(u32::MAX);
                    g.coverage = [off, c.width(), c.height(), 0];
                }
                g
            })
            .collect()
    }

    /// Identity of the packed coverage: the ordered distinct ids. The
    /// renderer re-uploads the buffer only when this changes, not on every
    /// slider tick. 0 when no layer has coverage.
    pub fn coverage_key(&self) -> u64 {
        let distinct = self.distinct();
        if distinct.is_empty() {
            return 0;
        }
        let mut h = crate::hash::Fnv1a::new();
        for c in distinct {
            h.write(&c.id().to_le_bytes());
        }
        h.finish()
    }

    /// Distinct coverages, bytes packed four per u32, little-endian, rows
    /// unpadded, in first-use order. Must match `coverageTexel` in
    /// loupe_common.wgsl.
    pub fn pack_coverage(&self) -> Vec<u32> {
        let mut bytes: Vec<u8> = Vec::new();
        for c in self.distinct() {
            bytes.extend_from_slice(c.alpha());
        }
        bytes
            .chunks(4)
            .map(|w| {
                let mut word = [0u8; 4];
                word[..w.len()].copy_from_slice(w);
                u32::from_le_bytes(word)
            })
            .collect()
    }
}

/// What a bake job carries. Native jobs read coverage on their worker, as
/// they read the photo, so an export of 500 photos holds no bitmaps while
/// queued. The browser's jobs can't reach the folder (File System Access is
/// main-thread only), so the app resolves first, as it already ships source
/// bytes instead of a path.
#[derive(Clone)]
pub enum MaskInput {
    Ready(MaskSet),
    #[cfg(not(target_arch = "wasm32"))]
    OnDisk {
        masks: Vec<Mask>,
        detections: Detections,
    },
}

impl MaskInput {
    pub fn none() -> MaskInput {
        MaskInput::Ready(MaskSet::default())
    }

    /// The masks with their coverage, read from beside `photo` if needed.
    pub fn resolve(self, photo: &std::path::Path) -> Result<MaskSet, CoverageError> {
        // Only the native variant reads beside the photo.
        let _ = photo;
        match self {
            MaskInput::Ready(set) => Ok(set),
            #[cfg(not(target_arch = "wasm32"))]
            MaskInput::OnDisk { masks, detections } => MaskSet::read(photo, &masks, &detections),
        }
    }
}

/// [`Mask`] as laid out in the GPU storage buffer. Must match `Mask` in
/// loupe_common.wgsl.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GpuMask {
    /// Linear: start, end. Radial: center, radii.
    pub geom: [f32; 4],
    /// Kind (0 linear, 1 radial, 2 coverage: detected or brush), invert (0
    /// or 1), angle, feather.
    pub params: [f32; 4],
    /// Exposure, contrast, highlights, shadows.
    pub a0: [f32; 4],
    /// Whites, blacks, temp, tint.
    pub a1: [f32; 4],
    /// Saturation, then padding.
    pub a2: [f32; 4],
    /// A coverage mask's byte offset into the coverage buffer, width,
    /// height, 0; all 0 while it has none. Integers, since offsets pass
    /// 2^24, past where an f32 counts exactly. `From<&Mask>` writes zeros;
    /// only `PhotoMasks::gpu_masks` knows offsets.
    pub coverage: [u32; 4],
}

impl From<&Mask> for GpuMask {
    fn from(m: &Mask) -> Self {
        let invert = if m.invert { 1.0 } else { 0.0 };
        let (geom, params) = match m.shape {
            MaskShape::Linear { start, end } => (
                [start[0], start[1], end[0], end[1]],
                [0.0, invert, 0.0, 0.0],
            ),
            MaskShape::Radial {
                center,
                radii,
                angle,
                feather,
            } => (
                [center[0], center[1], radii[0], radii[1]],
                [1.0, invert, angle, feather],
            ),
            MaskShape::Detected { .. } | MaskShape::Brush { .. } => {
                ([0.0; 4], [2.0, invert, 0.0, 0.0])
            }
        };
        let a = &m.amounts;
        GpuMask {
            geom,
            params,
            a0: [a.exposure, a.contrast, a.highlights, a.shadows],
            a1: [a.whites, a.blacks, a.temp, a.tint],
            a2: [a.saturation, 0.0, 0.0, 0.0],
            coverage: [0; 4],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The coverage layer `i` of a [`MaskSet`] or [`PhotoMasks`] renders
    /// with, if it has one.
    fn coverage(layers: &[(Mask, Option<Arc<Coverage>>)], i: usize) -> Option<&Arc<Coverage>> {
        layers.get(i).and_then(|(_, c)| c.as_ref())
    }

    fn radial(feather: f32) -> Mask {
        Mask::radial([0.5, 0.5], [0.2, 0.1]).with_feather(feather)
    }

    impl Mask {
        fn with_feather(mut self, f: f32) -> Self {
            if let MaskShape::Radial { feather, .. } = &mut self.shape {
                *feather = f;
            }
            self
        }
        fn inverted(&self) -> Self {
            Mask {
                invert: true,
                ..self.clone()
            }
        }
    }

    #[test]
    fn a_linear_mask_is_full_before_start_and_gone_past_end() {
        let m = Mask::linear([0.2, 0.5], [0.6, 0.5]);
        assert_eq!(mask_weight(&m, None, [0.1, 0.3], 1.5), 1.0);
        assert_eq!(mask_weight(&m, None, [0.9, 0.7], 1.5), 0.0);
        assert!((mask_weight(&m, None, [0.4, 0.5], 1.5) - 0.5).abs() < 1e-6);
        let inv = m.inverted();
        assert_eq!(mask_weight(&inv, None, [0.1, 0.3], 1.5), 0.0);
        assert_eq!(mask_weight(&inv, None, [0.9, 0.7], 1.5), 1.0);
    }

    #[test]
    fn a_linear_mask_with_no_length_covers_nothing_even_inverted() {
        let m = Mask::linear([0.4, 0.4], [0.4, 0.4]);
        assert_eq!(mask_weight(&m, None, [0.4, 0.4], 1.0), 0.0);
        assert_eq!(mask_weight(&m.inverted(), None, [0.1, 0.9], 1.0), 0.0);
    }

    #[test]
    fn a_radial_mask_is_full_at_its_center_and_gone_past_its_edge() {
        for feather in [0.0, 0.5, 1.0] {
            let m = radial(feather);
            assert_eq!(
                mask_weight(&m, None, [0.5, 0.5], 1.5),
                1.0,
                "feather {feather}"
            );
            // 0.2 of the shorter side, along x, is 0.2 / 1.5 in UV on a 3:2 photo.
            let edge = 0.5 + 0.2 / 1.5;
            assert_eq!(mask_weight(&m, None, [edge + 1e-4, 0.5], 1.5), 0.0);
            assert_eq!(
                mask_weight(&m.inverted(), None, [edge + 1e-4, 0.5], 1.5),
                1.0
            );
            assert_eq!(mask_weight(&m.inverted(), None, [0.5, 0.5], 1.5), 0.0);
        }
        let hard = radial(0.0);
        assert_eq!(mask_weight(&hard, None, [0.5 + 0.19 / 1.5, 0.5], 1.5), 1.0);
        let soft = radial(0.5);
        let mid = mask_weight(&soft, None, [0.5 + 0.15 / 1.5, 0.5], 1.5);
        assert!((mid - 0.5).abs() < 1e-5, "{mid}");
    }

    #[test]
    fn a_radial_mask_turns_with_its_angle_and_measures_in_pixels() {
        let flat = radial(0.0);
        // 0.15 down is past the 0.1 radius until the ellipse turns upright.
        assert_eq!(mask_weight(&flat, None, [0.5, 0.65], 1.0), 0.0);
        let mut upright = flat;
        if let MaskShape::Radial { angle, .. } = &mut upright.shape {
            *angle = std::f32::consts::FRAC_PI_2;
        }
        assert_eq!(mask_weight(&upright, None, [0.5, 0.65], 1.0), 1.0);
        // On a tall photo the shorter side is the width, so a UV step in v
        // covers more of it.
        let circle = Mask::radial([0.5, 0.5], [0.2, 0.2]).with_feather(0.0);
        assert_eq!(mask_weight(&circle, None, [0.5, 0.62], 0.5), 0.0);
        assert_eq!(mask_weight(&circle, None, [0.5, 0.58], 0.5), 1.0);
        assert_eq!(mask_weight(&circle, None, [0.68, 0.5], 0.5), 1.0);
    }

    #[test]
    fn local_tone_adds_weighted_amounts_and_clamps() {
        let mut m = Mask::radial([0.5, 0.5], [0.3, 0.3]).with_feather(0.0);
        m.amounts.exposure = 4.0;
        m.amounts.temp = -40.0;
        let base = LocalAmounts {
            exposure: 3.0,
            temp: 10.0,
            ..LocalAmounts::default()
        };
        let plain = |masks: &[Mask]| MaskSet::new(masks, |_| None).on_photo(1.0);
        let inside = plain(&[m.clone(), m.clone()]).local_tone(base, [0.5, 0.5]);
        assert_eq!(inside.exposure, 5.0, "held to the slider's range");
        assert_eq!(inside.temp, -70.0);
        let outside = plain(&[m]).local_tone(base, [0.0, 0.0]);
        assert_eq!(outside, base);
    }

    fn half_left() -> Arc<Coverage> {
        let alpha: Vec<u8> = (0..8).map(|i| if i % 4 < 2 { 255 } else { 0 }).collect();
        Arc::new(Coverage::fit(4, 2, &alpha).unwrap())
    }

    #[test]
    fn a_detected_mask_weighs_its_coverage_and_nothing_without_one() {
        let m = Mask::detected(DetectTarget::Subject);
        let c = half_left();
        assert_eq!(mask_weight(&m, Some(&c), [0.2, 0.5], 1.0), 1.0);
        assert_eq!(mask_weight(&m, Some(&c), [0.8, 0.5], 1.0), 0.0);
        assert_eq!(mask_weight(&m.inverted(), Some(&c), [0.2, 0.5], 1.0), 0.0);
        assert_eq!(mask_weight(&m.inverted(), Some(&c), [0.8, 0.5], 1.0), 1.0);
        assert_eq!(mask_weight(&m, None, [0.2, 0.5], 1.0), 0.0);
        assert_eq!(
            mask_weight(&m.inverted(), None, [0.8, 0.5], 1.0),
            0.0,
            "no coverage covers nothing, inverted or not"
        );
        assert_eq!(m.clone().sanitized(), Some(m.clone()), "nothing to clamp");
        assert_eq!(m.target(), Some(DetectTarget::Subject));
        assert_eq!(Mask::linear([0.0; 2], [1.0; 2]).target(), None);
    }

    #[test]
    fn a_set_asks_each_target_once_and_two_masks_of_one_target_pack_one_copy() {
        let mut m = Mask::detected(DetectTarget::Subject);
        m.amounts.exposure = 1.0;
        let sky = Mask::detected(DetectTarget::Sky);
        let c = half_left();
        let mut asked = Vec::new();
        let set = MaskSet::new(&[m.clone(), sky.clone(), m.inverted(), radial(0.5)], |t| {
            asked.push(t);
            (t == DetectTarget::Subject).then(|| Arc::clone(&c))
        });
        assert_eq!(asked, [DetectTarget::Subject, DetectTarget::Sky]);
        assert_eq!(set.layers.len(), 4);
        assert!(coverage(&set.layers, 0).is_some() && coverage(&set.layers, 1).is_none());
        let set = set.on_photo(2.0);

        let packed = set.pack_coverage();
        assert_eq!(packed.len(), 2, "one 8-byte coverage, four bytes a word");
        assert_eq!(packed[0], u32::from_le_bytes([255, 255, 0, 0]));
        let gpu = set.gpu_masks();
        assert_eq!(gpu[0].params[0], 2.0);
        assert_eq!(gpu[0].coverage, [0, 4, 2, 0]);
        assert_eq!(
            gpu[2].coverage, gpu[0].coverage,
            "the second subject mask shares the bytes"
        );
        assert_eq!(gpu[2].params[1], 1.0, "but keeps its own invert");
        assert_eq!(gpu[1].coverage, [0; 4], "a pending target has size 0");
        assert_eq!(gpu[3].params[0], 1.0);
        assert_ne!(set.coverage_key(), 0);
        assert_eq!(
            MaskSet::new(&[sky], |_| None).on_photo(2.0).coverage_key(),
            0
        );

        let base = LocalAmounts::default();
        assert_eq!(set.local_tone(base, [0.2, 0.5]).exposure, 1.0);
        assert_eq!(
            set.local_tone(base, [0.8, 0.5]).exposure,
            1.0,
            "the inverted one"
        );
        assert_eq!(
            set.local_tone(base, [0.5, 0.5]).exposure,
            1.0,
            "half of each"
        );
    }

    #[test]
    fn a_detected_mask_round_trips_through_its_tagged_json() {
        let m = Mask::detected(DetectTarget::Sky);
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, r#"{"shape":{"kind":"detected","target":"sky"}}"#);
        assert_eq!(serde_json::from_str::<Mask>(&json).unwrap(), m);
    }

    #[test]
    fn sanitizing_drops_non_numbers_and_clamps_the_rest() {
        let mut nan = radial(0.5);
        nan.amounts.exposure = f32::NAN;
        assert_eq!(nan.sanitized(), None);
        let inf = Mask::linear([f32::INFINITY, 0.0], [0.5, 0.5]);
        assert_eq!(inf.sanitized(), None);

        let wild = Mask {
            shape: MaskShape::Radial {
                center: [1e30, -0.5],
                radii: [-1.0, 99.0],
                angle: -1.0,
                feather: 7.0,
            },
            invert: true,
            amounts: LocalAmounts {
                exposure: 50.0,
                saturation: -500.0,
                ..LocalAmounts::default()
            },
        };
        let Some(Mask {
            shape:
                MaskShape::Radial {
                    center,
                    radii,
                    angle,
                    feather,
                },
            invert,
            amounts,
        }) = wild.sanitized()
        else {
            panic!("a finite mask survives");
        };
        assert_eq!(center, [11.0, -0.5]);
        assert_eq!(radii, [1e-4, 4.0]);
        assert!((0.0..std::f32::consts::TAU).contains(&angle));
        assert_eq!(feather, 1.0);
        assert!(invert);
        assert_eq!((amounts.exposure, amounts.saturation), (5.0, -100.0));
    }

    #[test]
    fn every_target_serializes_with_a_name_this_build_lists() {
        for (t, name) in DetectTarget::ALL.iter().zip(DetectTarget::NAMES) {
            assert_eq!(serde_json::to_value(t).unwrap(), name);
        }
    }

    #[test]
    fn every_shape_serializes_with_a_kind_this_build_lists() {
        for m in [
            Mask::linear([0.0; 2], [1.0; 2]),
            Mask::radial([0.5; 2], [0.1; 2]),
            Mask::detected(DetectTarget::Subject),
            Mask::brush(),
        ] {
            let v = serde_json::to_value(m.shape).unwrap();
            let kind = v["kind"].as_str().unwrap();
            assert!(
                MaskShape::KINDS.contains(&kind),
                "{kind} missing from KINDS"
            );
        }
    }

    #[test]
    fn a_mask_round_trips_through_its_tagged_json() {
        let mut m = radial(0.3);
        m.amounts.shadows = 20.0;
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains(r#""kind":"radial""#), "{json}");
        assert!(
            !json.contains("invert"),
            "a default field stays out: {json}"
        );
        let back: Mask = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }

    fn brush_of(strokes: Vec<Stroke>) -> Mask {
        Mask {
            shape: MaskShape::Brush {
                strokes: strokes.into(),
            },
            ..Mask::brush()
        }
    }

    fn stroke(points: &[[f32; 2]]) -> Stroke {
        Stroke {
            radius: 0.1,
            feather: 0.5,
            flow: 1.0,
            erase: false,
            points: points.to_vec(),
        }
    }

    #[test]
    fn a_brush_mask_round_trips_through_its_tagged_json() {
        let mut erase = stroke(&[[0.5, 0.5]]);
        erase.erase = true;
        let m = brush_of(vec![stroke(&[[0.1, 0.2], [0.3, 0.4]]), erase]);
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(
            json,
            r#"{"shape":{"kind":"brush","strokes":[{"radius":0.1,"feather":0.5,"flow":1.0,"points":[[0.1,0.2],[0.3,0.4]]},{"radius":0.1,"feather":0.5,"flow":1.0,"erase":true,"points":[[0.5,0.5]]}]}}"#
        );
        assert_eq!(serde_json::from_str::<Mask>(&json).unwrap(), m);
        assert_eq!(m.brush_points(), 3 + 2 * STROKE_POINTS);
        assert_eq!(m.target(), None);
    }

    #[test]
    fn sanitizing_a_brush_drops_bad_strokes_and_points_and_holds_the_caps() {
        let mut nan = stroke(&[[0.5, 0.5]]);
        nan.radius = f32::NAN;
        let mut wild = stroke(&[[f32::NAN, 0.5], [1e9, -1e9], [0.5, f32::INFINITY]]);
        wild.radius = 50.0;
        wild.flow = -2.0;
        wild.feather = 9.0;
        let empty = stroke(&[]);
        let only_nan = stroke(&[[f32::NAN, f32::NAN]]);
        let m = brush_of(vec![nan, wild, empty, only_nan])
            .sanitized()
            .unwrap();
        let strokes = m.strokes().unwrap();
        assert_eq!(strokes.len(), 1, "{strokes:?}");
        assert_eq!(strokes[0].points, vec![[11.0, -10.0]]);
        assert_eq!(
            (strokes[0].radius, strokes[0].flow, strokes[0].feather),
            (1.0, 0.0, 1.0)
        );

        let many = brush_of(vec![stroke(&[[0.5, 0.5]]); MAX_STROKES_PER_MASK + 9]);
        assert_eq!(
            many.sanitized().unwrap().strokes().unwrap().len(),
            MAX_STROKES_PER_MASK
        );
        let mut left = 2 * STROKE_POINTS + 5;
        let long = brush_of(vec![stroke(&[[0.5, 0.5]; 4]); 3]);
        let cut = long.sanitized_within(&mut left).unwrap();
        assert_eq!(
            cut.brush_points(),
            2 * STROKE_POINTS + 5,
            "points past the budget are cut"
        );
        assert_eq!(cut.strokes().unwrap()[1].points.len(), 1);
        assert_eq!(left, 0);
    }

    #[test]
    fn a_brush_mask_renders_its_raster_and_an_empty_one_inverted_covers_all() {
        let mut m = brush_of(vec![stroke(&[[0.2, 0.5], [0.4, 0.5]])]);
        m.amounts.exposure = 1.0;
        let mut empty = Mask::brush();
        empty.invert = true;
        empty.amounts.temp = 10.0;
        let set = MaskSet::new(&[m.clone(), empty], |_| None);
        assert!(
            coverage(&set.layers, 0).is_none(),
            "no raster until it has a shape"
        );
        let placed = set.on_photo(1.5);
        let base = LocalAmounts::default();
        assert_eq!(placed.local_tone(base, [0.3, 0.5]).exposure, 1.0);
        assert_eq!(placed.local_tone(base, [0.8, 0.5]).exposure, 0.0);
        assert_eq!(placed.local_tone(base, [0.8, 0.9]).temp, 10.0);
        let gpu = placed.gpu_masks();
        let raster = coverage(&placed.layers, 0).unwrap();
        assert_eq!(gpu[0].params[0], 2.0);
        assert_eq!(gpu[0].coverage, [0, raster.width(), raster.height(), 0]);
        let after = raster.alpha().len() as u32;
        assert_eq!(gpu[1].coverage, [after, 1, 1, 0], "packed after the first");
        assert_eq!(
            placed.pack_coverage().len(),
            (raster.alpha().len() + 1).div_ceil(4)
        );
    }
}
