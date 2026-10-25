// SPDX-License-Identifier: MIT OR Apache-2.0

//! The edit model. [`Adjustments`] is what the folder store stores, [`GpuAdjust`]
//! is its uniform-buffer copy, and [`Develop`] is the CPU tone pipeline
//! used for export, edited thumbnails, and the histogram. The Loupe runs the
//! same math in `shader.wgsl`; the two must stay in sync.

pub mod base;
pub mod brush;
pub mod chroma;
pub mod color;
pub mod copy;
pub mod coverage;
pub mod curve;
pub mod effects;
pub mod grade;
pub mod image_ops;
pub mod local;
pub mod mask;
pub mod parallel;
pub mod sharpen;

use serde::{Deserialize, Serialize};

pub use base::{BaseLayer, BaseProbe, Grid};
pub use color::{Band, ColorAxis, ColorMixer, ColorOps, GrayMix};
pub use coverage::{Coverage, CoverageError, Detections, RasterId};
pub use effects::{Effects, Grain, Vignette};
pub use grade::{ColorGrade, GradeOps, Wheel, Zone};
pub use mask::{
    DetectTarget, GpuMask, LocalAmounts, Mask, MaskInput, MaskSet, MaskShape, PhotoMasks, Stroke,
    MAX_MASKS,
};
pub use sharpen::{Sharpen, Sharpening};

use crate::decode::image_decode::PixelFormat;
use curve::ToneCurve;

/// Range of every tone slider except exposure and denoise. 0 is no change.
pub const TONE_RANGE: std::ops::RangeInclusive<f32> = -100.0..=100.0;
/// Exposure range in stops.
pub const EXPOSURE_RANGE: std::ops::RangeInclusive<f32> = -5.0..=5.0;
/// Range of a Color Grading wheel's hue, in degrees.
pub const HUE_RANGE: std::ops::RangeInclusive<f32> = 0.0..=360.0;
/// Range of Denoise and the 0..100 Vignette and Grain sliders.
pub const PERCENT_RANGE: std::ops::RangeInclusive<f32> = 0.0..=100.0;

/// One Develop slider. The panel draws `SLIDERS` in order and keyboard focus
/// indexes it, so both always agree.
pub struct Slider {
    pub section: Section,
    pub id: SliderId,
    pub field: fn(&mut Adjustments) -> &mut f32,
    pub range: std::ops::RangeInclusive<f32>,
    /// The value Reset and a double-click put back.
    pub default: f32,
    pub decimals: usize,
    /// Keyboard nudge size.
    pub step: f32,
}

const fn tone(section: Section, id: SliderId, field: fn(&mut Adjustments) -> &mut f32) -> Slider {
    Slider {
        section,
        id,
        field,
        range: TONE_RANGE,
        default: 0.0,
        decimals: 0,
        step: 1.0,
    }
}

/// One Color Mixer slider: `axis` of `band`, -100..100.
const fn mix(axis: ColorAxis, band: Band, field: fn(&mut Adjustments) -> &mut f32) -> Slider {
    tone(Section::ColorMixer, SliderId::Mix(axis, band), field)
}

/// One B&W mix slider: how light `band` turns out, -100..100.
const fn gray(band: Band, field: fn(&mut Adjustments) -> &mut f32) -> Slider {
    tone(Section::GrayMix, SliderId::Gray(band), field)
}

/// One Color Grading slider: `axis` of the `zone` wheel. Hue and
/// Saturation take their own ranges.
const fn grade(zone: Zone, axis: ColorAxis, field: fn(&mut Adjustments) -> &mut f32) -> Slider {
    tone(Section::ColorGrading, SliderId::Grade(zone, axis), field)
}

/// A 0..100 slider that rests at `default`.
const fn percent(
    section: Section,
    id: SliderId,
    field: fn(&mut Adjustments) -> &mut f32,
    default: f32,
) -> Slider {
    Slider {
        range: PERCENT_RANGE,
        default,
        ..tone(section, id, field)
    }
}

/// What the Sliders page shows of the sliders that share rows: the Color
/// Mixer's chosen axis, and whether the photos are monochrome, which swaps
/// the mixer for the B&W mix as Lightroom does.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct SliderView {
    pub mixer_axis: ColorAxis,
    pub monochrome: bool,
    /// The Color Grading wheel whose sliders show.
    pub grade_zone: Zone,
    /// Sections folded shut on the Sliders page.
    pub collapsed: Sections,
}

/// A set of [`Section`]s.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Sections(u16);

impl Sections {
    pub const NONE: Self = Self(0);

    pub fn contains(self, section: Section) -> bool {
        self.0 & (1 << section as u16) != 0
    }

    /// `self` with `section` in it if it was out, or out if it was in.
    pub fn toggled(self, section: Section) -> Self {
        Self(self.0 ^ (1 << section as u16))
    }
}

impl Slider {
    /// `value` moved `steps` keyboard steps: around the circle for a
    /// grading hue, which is an angle, else clamped to the range.
    pub fn stepped(&self, value: f32, steps: f32) -> f32 {
        let v = value + steps * self.step;
        if let SliderId::Grade(_, ColorAxis::Hue) = self.id {
            return v.rem_euclid(360.0);
        }
        v.clamp(*self.range.start(), *self.range.end())
    }

    /// Whether the panel draws this slider under `view`. The keyboard
    /// cursor walks only these.
    pub fn shown(&self, view: SliderView) -> bool {
        if view.collapsed.contains(self.section) {
            return false;
        }
        match self.id {
            SliderId::Mix(axis, _) => !view.monochrome && axis == view.mixer_axis,
            SliderId::Gray(_) => view.monochrome,
            SliderId::Grade(zone, _) => zone == view.grade_zone,
            _ => true,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Section {
    WhiteBalance,
    Tone,
    Presence,
    /// The point curve, not sliders. See [`ToneCurve`].
    Curve,
    /// Hue, saturation and luminance per hue band. See [`ColorMixer`].
    ColorMixer,
    /// The B&W mix, in the mixer's place on a monochrome photo. See
    /// [`GrayMix`].
    GrayMix,
    /// Wheels for the shadows, midtones, highlights and whole photo. See
    /// [`ColorGrade`].
    ColorGrading,
    Detail,
    /// Checkboxes, not sliders. See [`CaScale`].
    Optics,
    /// Post-crop. See [`Vignette`].
    Vignette,
    /// See [`Grain`].
    Grain,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum SliderId {
    Temp,
    Tint,
    Exposure,
    Contrast,
    Highlights,
    Shadows,
    Whites,
    Blacks,
    Texture,
    Clarity,
    Dehaze,
    Vibrance,
    Saturation,
    Mix(ColorAxis, Band),
    Gray(Band),
    Grade(Zone, ColorAxis),
    GradeBlending,
    GradeBalance,
    SharpenAmount,
    SharpenRadius,
    SharpenDetail,
    SharpenMasking,
    Denoise,
    VignetteAmount,
    VignetteMidpoint,
    VignetteRoundness,
    VignetteFeather,
    GrainAmount,
    GrainSize,
    GrainRoughness,
}

pub const SLIDERS: [Slider; 71] = [
    tone(Section::WhiteBalance, SliderId::Temp, |a| &mut a.temp),
    tone(Section::WhiteBalance, SliderId::Tint, |a| &mut a.tint),
    Slider {
        section: Section::Tone,
        id: SliderId::Exposure,
        field: |a| &mut a.exposure,
        range: EXPOSURE_RANGE,
        default: 0.0,
        decimals: 2,
        step: 0.05,
    },
    tone(Section::Tone, SliderId::Contrast, |a| &mut a.contrast),
    tone(Section::Tone, SliderId::Highlights, |a| &mut a.highlights),
    tone(Section::Tone, SliderId::Shadows, |a| &mut a.shadows),
    tone(Section::Tone, SliderId::Whites, |a| &mut a.whites),
    tone(Section::Tone, SliderId::Blacks, |a| &mut a.blacks),
    tone(Section::Presence, SliderId::Texture, |a| &mut a.texture),
    tone(Section::Presence, SliderId::Clarity, |a| &mut a.clarity),
    tone(Section::Presence, SliderId::Dehaze, |a| &mut a.dehaze),
    tone(Section::Presence, SliderId::Vibrance, |a| &mut a.vibrance),
    tone(Section::Presence, SliderId::Saturation, |a| {
        &mut a.saturation
    }),
    mix(ColorAxis::Hue, Band::Red, |a| &mut a.mixer.red.hue),
    mix(ColorAxis::Hue, Band::Orange, |a| &mut a.mixer.orange.hue),
    mix(ColorAxis::Hue, Band::Yellow, |a| &mut a.mixer.yellow.hue),
    mix(ColorAxis::Hue, Band::Green, |a| &mut a.mixer.green.hue),
    mix(ColorAxis::Hue, Band::Aqua, |a| &mut a.mixer.aqua.hue),
    mix(ColorAxis::Hue, Band::Blue, |a| &mut a.mixer.blue.hue),
    mix(ColorAxis::Hue, Band::Purple, |a| &mut a.mixer.purple.hue),
    mix(ColorAxis::Hue, Band::Magenta, |a| &mut a.mixer.magenta.hue),
    mix(ColorAxis::Saturation, Band::Red, |a| {
        &mut a.mixer.red.saturation
    }),
    mix(ColorAxis::Saturation, Band::Orange, |a| {
        &mut a.mixer.orange.saturation
    }),
    mix(ColorAxis::Saturation, Band::Yellow, |a| {
        &mut a.mixer.yellow.saturation
    }),
    mix(ColorAxis::Saturation, Band::Green, |a| {
        &mut a.mixer.green.saturation
    }),
    mix(ColorAxis::Saturation, Band::Aqua, |a| {
        &mut a.mixer.aqua.saturation
    }),
    mix(ColorAxis::Saturation, Band::Blue, |a| {
        &mut a.mixer.blue.saturation
    }),
    mix(ColorAxis::Saturation, Band::Purple, |a| {
        &mut a.mixer.purple.saturation
    }),
    mix(ColorAxis::Saturation, Band::Magenta, |a| {
        &mut a.mixer.magenta.saturation
    }),
    mix(ColorAxis::Luminance, Band::Red, |a| {
        &mut a.mixer.red.luminance
    }),
    mix(ColorAxis::Luminance, Band::Orange, |a| {
        &mut a.mixer.orange.luminance
    }),
    mix(ColorAxis::Luminance, Band::Yellow, |a| {
        &mut a.mixer.yellow.luminance
    }),
    mix(ColorAxis::Luminance, Band::Green, |a| {
        &mut a.mixer.green.luminance
    }),
    mix(ColorAxis::Luminance, Band::Aqua, |a| {
        &mut a.mixer.aqua.luminance
    }),
    mix(ColorAxis::Luminance, Band::Blue, |a| {
        &mut a.mixer.blue.luminance
    }),
    mix(ColorAxis::Luminance, Band::Purple, |a| {
        &mut a.mixer.purple.luminance
    }),
    mix(ColorAxis::Luminance, Band::Magenta, |a| {
        &mut a.mixer.magenta.luminance
    }),
    gray(Band::Red, |a| &mut a.gray_mix.red),
    gray(Band::Orange, |a| &mut a.gray_mix.orange),
    gray(Band::Yellow, |a| &mut a.gray_mix.yellow),
    gray(Band::Green, |a| &mut a.gray_mix.green),
    gray(Band::Aqua, |a| &mut a.gray_mix.aqua),
    gray(Band::Blue, |a| &mut a.gray_mix.blue),
    gray(Band::Purple, |a| &mut a.gray_mix.purple),
    gray(Band::Magenta, |a| &mut a.gray_mix.magenta),
    Slider {
        range: HUE_RANGE,
        ..grade(Zone::Shadows, ColorAxis::Hue, |a| &mut a.grade.shadows.hue)
    },
    Slider {
        range: PERCENT_RANGE,
        ..grade(Zone::Shadows, ColorAxis::Saturation, |a| {
            &mut a.grade.shadows.saturation
        })
    },
    grade(Zone::Shadows, ColorAxis::Luminance, |a| {
        &mut a.grade.shadows.luminance
    }),
    Slider {
        range: HUE_RANGE,
        ..grade(Zone::Midtones, ColorAxis::Hue, |a| {
            &mut a.grade.midtones.hue
        })
    },
    Slider {
        range: PERCENT_RANGE,
        ..grade(Zone::Midtones, ColorAxis::Saturation, |a| {
            &mut a.grade.midtones.saturation
        })
    },
    grade(Zone::Midtones, ColorAxis::Luminance, |a| {
        &mut a.grade.midtones.luminance
    }),
    Slider {
        range: HUE_RANGE,
        ..grade(Zone::Highlights, ColorAxis::Hue, |a| {
            &mut a.grade.highlights.hue
        })
    },
    Slider {
        range: PERCENT_RANGE,
        ..grade(Zone::Highlights, ColorAxis::Saturation, |a| {
            &mut a.grade.highlights.saturation
        })
    },
    grade(Zone::Highlights, ColorAxis::Luminance, |a| {
        &mut a.grade.highlights.luminance
    }),
    Slider {
        range: HUE_RANGE,
        ..grade(Zone::Global, ColorAxis::Hue, |a| &mut a.grade.global.hue)
    },
    Slider {
        range: PERCENT_RANGE,
        ..grade(Zone::Global, ColorAxis::Saturation, |a| {
            &mut a.grade.global.saturation
        })
    },
    grade(Zone::Global, ColorAxis::Luminance, |a| {
        &mut a.grade.global.luminance
    }),
    percent(
        Section::ColorGrading,
        SliderId::GradeBlending,
        |a| &mut a.grade.blending,
        50.0,
    ),
    tone(Section::ColorGrading, SliderId::GradeBalance, |a| {
        &mut a.grade.balance
    }),
    Slider {
        range: sharpen::AMOUNT_RANGE,
        ..percent(
            Section::Detail,
            SliderId::SharpenAmount,
            |a| &mut a.sharpening.amount,
            0.0,
        )
    },
    Slider {
        section: Section::Detail,
        id: SliderId::SharpenRadius,
        field: |a| &mut a.sharpening.radius,
        range: sharpen::RADIUS_RANGE,
        default: 1.0,
        decimals: 1,
        step: 0.1,
    },
    percent(
        Section::Detail,
        SliderId::SharpenDetail,
        |a| &mut a.sharpening.detail,
        25.0,
    ),
    percent(
        Section::Detail,
        SliderId::SharpenMasking,
        |a| &mut a.sharpening.masking,
        0.0,
    ),
    percent(Section::Detail, SliderId::Denoise, |a| &mut a.denoise, 0.0),
    tone(Section::Vignette, SliderId::VignetteAmount, |a| {
        &mut a.vignette.amount
    }),
    percent(
        Section::Vignette,
        SliderId::VignetteMidpoint,
        |a| &mut a.vignette.midpoint,
        50.0,
    ),
    tone(Section::Vignette, SliderId::VignetteRoundness, |a| {
        &mut a.vignette.roundness
    }),
    percent(
        Section::Vignette,
        SliderId::VignetteFeather,
        |a| &mut a.vignette.feather,
        50.0,
    ),
    percent(
        Section::Grain,
        SliderId::GrainAmount,
        |a| &mut a.grain.amount,
        0.0,
    ),
    percent(
        Section::Grain,
        SliderId::GrainSize,
        |a| &mut a.grain.size,
        25.0,
    ),
    percent(
        Section::Grain,
        SliderId::GrainRoughness,
        |a| &mut a.grain.roughness,
        50.0,
    ),
];

/// A slider's stable name, the id the command layer and headless callers
/// use: lower snake parts joined by `.`, such as `exposure`,
/// `mix.blue.saturation` or `grade.shadows.hue`.
impl std::fmt::Display for SliderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use SliderId::*;
        let plain = match *self {
            Mix(axis, band) => return write!(f, "mix.{}.{}", band_name(band), axis_name(axis)),
            Gray(band) => return write!(f, "gray.{}", band_name(band)),
            Grade(zone, axis) => {
                return write!(f, "grade.{}.{}", zone_name(zone), axis_name(axis));
            }
            Temp => "temp",
            Tint => "tint",
            Exposure => "exposure",
            Contrast => "contrast",
            Highlights => "highlights",
            Shadows => "shadows",
            Whites => "whites",
            Blacks => "blacks",
            Texture => "texture",
            Clarity => "clarity",
            Dehaze => "dehaze",
            Vibrance => "vibrance",
            Saturation => "saturation",
            GradeBlending => "grade.blending",
            GradeBalance => "grade.balance",
            SharpenAmount => "sharpen.amount",
            SharpenRadius => "sharpen.radius",
            SharpenDetail => "sharpen.detail",
            SharpenMasking => "sharpen.masking",
            Denoise => "denoise",
            VignetteAmount => "vignette.amount",
            VignetteMidpoint => "vignette.midpoint",
            VignetteRoundness => "vignette.roundness",
            VignetteFeather => "vignette.feather",
            GrainAmount => "grain.amount",
            GrainSize => "grain.size",
            GrainRoughness => "grain.roughness",
        };
        f.write_str(plain)
    }
}

/// Parses a name `Display` wrote. It scans [`SLIDERS`], so a name exists
/// exactly when its slider does.
impl std::str::FromStr for SliderId {
    type Err = UnknownSlider;

    fn from_str(name: &str) -> Result<SliderId, UnknownSlider> {
        SLIDERS
            .iter()
            .map(|s| s.id)
            .find(|id| id.to_string() == name)
            .ok_or_else(|| UnknownSlider(name.to_string()))
    }
}

/// A name no slider has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownSlider(pub String);

impl std::fmt::Display for UnknownSlider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no develop control is named `{}`", self.0)
    }
}

impl std::error::Error for UnknownSlider {}

impl Serialize for SliderId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SliderId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<SliderId, D::Error> {
        let name = std::borrow::Cow::<str>::deserialize(d)?;
        name.parse().map_err(serde::de::Error::custom)
    }
}

impl SliderId {
    /// This id's row in [`SLIDERS`].
    pub fn slider(self) -> Option<&'static Slider> {
        SLIDERS.iter().find(|s| s.id == self)
    }
}

fn band_name(band: Band) -> &'static str {
    match band {
        Band::Red => "red",
        Band::Orange => "orange",
        Band::Yellow => "yellow",
        Band::Green => "green",
        Band::Aqua => "aqua",
        Band::Blue => "blue",
        Band::Purple => "purple",
        Band::Magenta => "magenta",
    }
}

fn axis_name(axis: ColorAxis) -> &'static str {
    match axis {
        ColorAxis::Hue => "hue",
        ColorAxis::Saturation => "saturation",
        ColorAxis::Luminance => "luminance",
    }
}

fn zone_name(zone: Zone) -> &'static str {
    match zone {
        Zone::Shadows => "shadows",
        Zone::Midtones => "midtones",
        Zone::Highlights => "highlights",
        Zone::Global => "global",
    }
}

impl Section {
    /// The section's stable name, for the `commands` listing.
    pub fn name(self) -> &'static str {
        match self {
            Section::WhiteBalance => "white_balance",
            Section::Tone => "tone",
            Section::Presence => "presence",
            Section::Curve => "curve",
            Section::ColorMixer => "color_mixer",
            Section::GrayMix => "gray_mix",
            Section::ColorGrading => "color_grading",
            Section::Detail => "detail",
            Section::Optics => "optics",
            Section::Vignette => "vignette",
            Section::Grain => "grain",
        }
    }
}

/// The most touch-ups one photo can hold: the GPU buffer's size.
pub const MAX_TOUCHUPS: usize = 64;

/// One spot-heal: copy a soft circle from `source` onto `center`. Coordinates
/// are 0..1 of the unrotated image, so the edit works at any size or rotation.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct TouchUp {
    pub center: [f32; 2],
    pub radius: f32,
    pub source: [f32; 2],
    pub feather: f32,
    pub delta: [f32; 3],
    /// How much of the patch shows over the photo, 0..1.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub opacity: f32,
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

fn one() -> f32 {
    1.0
}

fn is_one(v: &f32) -> bool {
    *v == 1.0
}

/// A crop rectangle in 0..1 image coordinates, origin top-left.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct Crop {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// Smallest crop edge separation, in normalized units, so the rect never collapses.
pub const MIN_CROP: f32 = 0.02;

impl Crop {
    /// Whether the box is at least [`MIN_CROP`] wide and tall. The crop tool
    /// builds its smallest box as `edge ± MIN_CROP`, and f32 subtraction can
    /// give back a hair under `MIN_CROP`, so a few ulps of slack count as met.
    pub fn holds_min_size(&self) -> bool {
        const SLACK: f32 = 1e-6;
        self.right - self.left >= MIN_CROP - SLACK && self.bottom - self.top >= MIN_CROP - SLACK
    }

    /// This crop with every edge held to 0..1, or `None` if an edge isn't a
    /// number or the box is narrower or shorter than [`MIN_CROP`]. A crop comes
    /// from sidecars and presets, and the crop tool clamps against these
    /// bounds, which panics on a box that breaks them.
    pub fn sanitized(self) -> Option<Crop> {
        let edges = [self.left, self.top, self.right, self.bottom];
        if !edges.iter().all(|v| v.is_finite()) {
            return None;
        }
        let [left, top, right, bottom] = edges.map(|v| v.clamp(0.0, 1.0));
        let held = Crop {
            left,
            top,
            right,
            bottom,
        };
        held.holds_min_size().then_some(held)
    }
}

fn deserialize_crop<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Crop>, D::Error> {
    Ok(Option::<Crop>::deserialize(d)?.and_then(Crop::sanitized))
}

/// The edit applied to one image. `Default` is no edit. Zero fields are left
/// out when serialized, so an unedited image writes nothing.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub struct Adjustments {
    /// Negative is cooler, positive warmer.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub temp: f32,
    /// Negative is greener, positive more magenta.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tint: f32,
    /// In stops.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub exposure: f32,
    /// S-curve strength around mid-gray.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub contrast: f32,
    /// Negative recovers highlights, positive brightens them.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub highlights: f32,
    /// Negative deepens shadows, positive lifts them.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub shadows: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub whites: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub blacks: f32,
    /// Fine detail against the fine layer; negative smooths it. See
    /// [`local`].
    #[serde(default, skip_serializing_if = "is_zero")]
    pub texture: f32,
    /// Midtone contrast against the base layer's local mean; negative
    /// softens.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub clarity: f32,
    /// Removes a haze veil estimated from the local dark channel; negative
    /// adds haze.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dehaze: f32,
    /// Saturation that acts mostly on dull pixels, sparing vivid colours and skin.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub vibrance: f32,
    /// -100 is grayscale.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub saturation: f32,
    #[serde(default, skip_serializing_if = "ColorMixer::is_neutral")]
    pub mixer: ColorMixer,
    /// Acts only while the photo is monochrome.
    #[serde(default, skip_serializing_if = "GrayMix::is_neutral")]
    pub gray_mix: GrayMix,
    #[serde(default, skip_serializing_if = "ColorGrade::is_default")]
    pub grade: ColorGrade,
    #[serde(default, skip_serializing_if = "Sharpening::is_default")]
    pub sharpening: Sharpening,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub denoise: f32,
    /// `None` means the full frame of the straightened photo.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_crop"
    )]
    pub crop: Option<Crop>,
    /// Degrees the photo turns about its center before the crop, within
    /// [`STRAIGHTEN_RANGE`]. See [`Straighten`].
    #[serde(default, skip_serializing_if = "is_zero")]
    pub straighten: f32,
    /// Remove Chromatic Aberration. `None` is off; `Some` holds the scales
    /// measured from this photo, so every render agrees without re-measuring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chromatic_aberration: Option<CaScale>,
    /// The point curve, run after the tone sliders and before color.
    #[serde(default, skip_serializing_if = "ToneCurve::is_linear")]
    pub curve: ToneCurve,
    /// Post-crop: it follows the crop, not the photo's frame.
    #[serde(default, skip_serializing_if = "Vignette::is_default")]
    pub vignette: Vignette,
    #[serde(default, skip_serializing_if = "Grain::is_default")]
    pub grain: Grain,
}

/// Lateral chromatic aberration as a radial magnification of red and blue
/// relative to green, about the photo's center. A source pixel for channel
/// `c` at UV `p` is `0.5 + (p - 0.5) * (1 + k_c)`, in pixel space so the
/// scale is the same along both axes. Must match `caUv` in loupe_common.wgsl.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub struct CaScale {
    pub red: f32,
    pub blue: f32,
}

impl CaScale {
    /// The source UV that red (`k = self.red`) or blue reads for output `uv`.
    pub fn source_uv(k: f32, u: f32, v: f32) -> (f32, f32) {
        let s = 1.0 + k;
        ((u - 0.5) * s + 0.5, (v - 0.5) * s + 0.5)
    }

    pub fn is_zero(&self) -> bool {
        self.red == 0.0 && self.blue == 0.0
    }
}

/// How far the Straighten tool can turn a photo, in degrees.
pub const STRAIGHTEN_RANGE: std::ops::RangeInclusive<f32> = -45.0..=45.0;

/// The straighten turn as a map between the straightened canvas and the
/// source photo. Both are 0..1 coordinates of the same `w x h` frame, and
/// the turn is in pixel space so it stays a rotation on a non-square photo.
/// A source pixel's offset from the center is the canvas offset turned by
/// the angle, clockwise on screen for a positive angle with y down. Must
/// match `straightenUv` in `loupe_common.wgsl`.
#[derive(Clone, Copy, Debug)]
pub struct Straighten {
    cos: f32,
    sin: f32,
    w: f32,
    h: f32,
}

/// An angle held to `STRAIGHTEN_RANGE`, a NaN to no turn.
fn clamp_straighten(degrees: f32) -> f32 {
    if degrees.is_nan() {
        0.0
    } else {
        degrees.clamp(*STRAIGHTEN_RANGE.start(), *STRAIGHTEN_RANGE.end())
    }
}

impl Straighten {
    /// A sidecar's angle can be anything, so it is held to `STRAIGHTEN_RANGE`.
    pub fn new(degrees: f32, w: f32, h: f32) -> Self {
        let (sin, cos) = clamp_straighten(degrees).to_radians().sin_cos();
        Self { cos, sin, w, h }
    }

    fn turn(&self, u: f32, v: f32, sin: f32) -> (f32, f32) {
        let (x, y) = ((u - 0.5) * self.w, (v - 0.5) * self.h);
        let (rx, ry) = (self.cos * x - sin * y, sin * x + self.cos * y);
        (rx / self.w + 0.5, ry / self.h + 0.5)
    }

    pub fn to_source(self, u: f32, v: f32) -> (f32, f32) {
        self.turn(u, v, self.sin)
    }

    pub fn to_canvas(self, u: f32, v: f32) -> (f32, f32) {
        self.turn(u, v, -self.sin)
    }

    /// True when all of `r` shows the photo, with no corner past its edge.
    pub fn covers(&self, r: Crop) -> bool {
        const EPS: f32 = 1e-4;
        let inside =
            |(u, v): (f32, f32)| (-EPS..=1.0 + EPS).contains(&u) && (-EPS..=1.0 + EPS).contains(&v);
        [
            (r.left, r.top),
            (r.right, r.top),
            (r.left, r.bottom),
            (r.right, r.bottom),
        ]
        .into_iter()
        .all(|(u, v)| inside((u, v)) && inside(self.to_source(u, v)))
    }

    /// The largest centered crop of pixel ratio `w_over_h` that `covers`
    /// accepts. The turned photo is convex and symmetric about the center,
    /// so no off-center box of that ratio is larger.
    pub fn largest_crop(&self, w_over_h: f32) -> Crop {
        let (c, s) = (self.cos.abs(), self.sin.abs());
        // A centered `a*h0 x h0` box fits the turned `w x h` photo when its
        // corners, turned back, land inside: a*h0*c + h0*s <= w and
        // a*h0*s + h0*c <= h. It must also fit the canvas itself.
        let a = w_over_h;
        let h0 = (self.w / (a * c + s))
            .min(self.h / (a * s + c))
            .min(self.w / a)
            .min(self.h);
        let (hw, hh) = (a * h0 / self.w / 2.0, h0 / self.h / 2.0);
        Crop {
            left: 0.5 - hw,
            top: 0.5 - hh,
            right: 0.5 + hw,
            bottom: 0.5 + hh,
        }
    }
}

impl Adjustments {
    /// Every slider and the straighten angle held to its range, a NaN put back
    /// to its default. A sidecar is a file anyone can edit, so a render never
    /// trusts its values.
    pub fn clamped(mut self) -> Self {
        for s in &SLIDERS {
            let v = (s.field)(&mut self);
            *v = if v.is_nan() {
                s.default
            } else {
                v.clamp(*s.range.start(), *s.range.end())
            };
        }
        self.straighten = clamp_straighten(self.straighten);
        self
    }

    /// True for no edit. The folder store skips storing these.
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }

    /// True when Saturation is all the way down, which is the B&W toggle's
    /// "on" state.
    pub fn is_monochrome(&self) -> bool {
        self.saturation <= -100.0
    }

    /// True when a render must build the [`BaseLayer`] first: Clarity and
    /// Dehaze read it. Texture reads the fine layer instead.
    pub fn needs_base(&self) -> bool {
        self.clarity != 0.0 || self.dehaze != 0.0
    }

    /// A copy without the crop, straighten or chromatic aberration scales,
    /// for pasting settings onto other photos. The CA scales were measured
    /// from this photo's lens and framing, so they don't carry over.
    pub fn tone_only(&self) -> Adjustments {
        Adjustments {
            crop: None,
            straighten: 0.0,
            chromatic_aberration: None,
            ..*self
        }
    }
}

/// A hash of everything that changes how an image renders, used in the
/// thumbnail cache key. Floats are rounded so tiny jitter doesn't re-bake, and
/// every unedited photo gets the same value. `detections` enters only
/// through the targets `masks` use, so a detection no mask reads changes
/// nothing, and a photo without detected masks hashes as it always has.
pub fn edit_signature_with_touchups(
    adj: &Adjustments,
    touchups: &[TouchUp],
    masks: &[Mask],
    detections: &Detections,
    rot: u8,
) -> u64 {
    use crate::hash::Fnv1a;

    let mut h = Fnv1a::new();

    // Tone sliders, rounded to 1e-3.
    let tone = [
        adj.temp,
        adj.tint,
        adj.exposure,
        adj.contrast,
        adj.highlights,
        adj.shadows,
        adj.whites,
        adj.blacks,
        adj.vibrance,
        adj.saturation,
        adj.denoise,
    ];
    for v in tone {
        h.write(&((v * 1000.0).round() as i32).to_le_bytes());
    }

    for t in touchups {
        for v in [
            t.center[0],
            t.center[1],
            t.radius,
            t.source[0],
            t.source[1],
            t.feather,
        ] {
            h.write(&((v * 100_000.0).round() as i32).to_le_bytes());
        }
        for v in t.delta {
            h.write(&((v * 100_000.0).round() as i32).to_le_bytes());
        }
        // Only a partial spot hashes its opacity, so spots saved before
        // opacity existed keep their cached renders.
        if t.opacity != 1.0 {
            h.write(&((t.opacity * 100_000.0).round() as i32).to_le_bytes());
        }
    }

    // A full-frame crop hashes the same as no crop. Coordinates round to 1e-5.
    let q = |v: f32| ((v.clamp(0.0, 1.0) * 100_000.0).round()) as u32;
    match adj.crop {
        Some(c) if c.left > 0.0 || c.top > 0.0 || c.right < 1.0 || c.bottom < 1.0 => {
            h.write(&[1]);
            for v in [c.left, c.top, c.right, c.bottom] {
                h.write(&q(v).to_le_bytes());
            }
        }
        _ => h.write(&[0]),
    }
    // Only a nonzero turn writes, so an unedited photo keeps its hash.
    if adj.straighten != 0.0 {
        h.write(&((adj.straighten * 1000.0).round() as i32).to_le_bytes());
    }
    if let Some(ca) = adj.chromatic_aberration {
        h.write(&[2]);
        for v in [ca.red, ca.blue] {
            h.write(&((v * 1e6).round() as i32).to_le_bytes());
        }
    }
    if !adj.curve.is_linear() {
        h.write(&[3]);
        for c in curve::Channel::ALL {
            let pts = adj.curve.get(c).points();
            h.write(&[pts.len() as u8]);
            h.write(pts.as_flattened());
        }
    }

    // Only a set slider writes, so older hashes hold.
    for (tag, v) in [(6u8, adj.texture), (7, adj.clarity), (8, adj.dehaze)] {
        if v != 0.0 {
            h.write(&[tag]);
            h.write(&((v * 1000.0).round() as i32).to_le_bytes());
        }
    }
    // A monochrome photo skips the mixer, so its values can't change a render.
    if !adj.mixer.is_neutral() && !adj.is_monochrome() {
        h.write(&[10]);
        for b in adj.mixer.bands() {
            for x in [b.hue, b.saturation, b.luminance] {
                h.write(&((x * 1000.0).round() as i32).to_le_bytes());
            }
        }
    }
    // And the B&W mix only on one.
    if !adj.gray_mix.is_neutral() && adj.is_monochrome() {
        h.write(&[11]);
        for x in adj.gray_mix.bands() {
            h.write(&((x * 1000.0).round() as i32).to_le_bytes());
        }
    }
    // The derived numbers, so a hue at saturation 0 or an out-of-range
    // value that clamps to the same render doesn't re-bake.
    let grade = GradeOps::new(&adj.grade);
    if grade.on != 0.0 {
        h.write(&[12]);
        for x in [grade.mid, grade.width]
            .into_iter()
            .chain(grade.wheels.into_iter().flatten())
        {
            h.write(&((x * 1e5).round() as i32).to_le_bytes());
        }
    }
    if adj.sharpening.amount != 0.0 {
        h.write(&[9]);
        let s = adj.sharpening;
        for x in [s.amount, s.radius, s.detail, s.masking] {
            h.write(&((x * 1000.0).round() as i32).to_le_bytes());
        }
    }
    // Only a vignette or grain that shows writes, so older hashes hold and
    // a sub-slider moved at Amount 0 doesn't re-bake.
    if adj.vignette.amount != 0.0 {
        h.write(&[4]);
        let v = adj.vignette;
        for x in [v.amount, v.midpoint, v.roundness, v.feather] {
            h.write(&((x * 1000.0).round() as i32).to_le_bytes());
        }
    }
    if adj.grain.amount != 0.0 {
        h.write(&[5]);
        let g = adj.grain;
        for x in [g.amount, g.size, g.roughness] {
            h.write(&((x * 1000.0).round() as i32).to_le_bytes());
        }
    }

    // Only a photo with masks writes, so older hashes hold.
    if !masks.is_empty() {
        h.write(&[13]);
        for m in masks {
            let g = GpuMask::from(m);
            for x in [g.geom, g.params, g.a0, g.a1, g.a2].into_iter().flatten() {
                h.write(&((x * 100_000.0).round() as i32).to_le_bytes());
            }
            // A pending detection hashes as none, so the bake re-runs when
            // it lands.
            if let Some(target) = m.target() {
                h.write(&[target.index() as u8]);
                h.write(&detections.get(target).map_or([0; 8], RasterId::to_le_bytes));
            }
            for s in m.strokes().into_iter().flat_map(|s| s.iter()) {
                h.write(&[u8::from(s.erase)]);
                h.write(&(s.points.len() as u64).to_le_bytes());
                let settings = [s.radius, s.feather, s.flow];
                for v in settings.iter().chain(s.points.iter().flatten()) {
                    h.write(&((v * 100_000.0).round() as i32).to_le_bytes());
                }
            }
        }
    }

    h.write(&[rot % 4]);

    h.finish()
}

/// [`TouchUp`] as laid out in the GPU storage buffer.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GpuTouchUp {
    pub center_radius_feather: [f32; 4],
    pub source: [f32; 2],
    pub opacity: f32,
    pub _pad: f32,
    pub delta: [f32; 4],
}

impl From<&TouchUp> for GpuTouchUp {
    fn from(t: &TouchUp) -> Self {
        Self {
            center_radius_feather: [t.center[0], t.center[1], t.radius, t.feather],
            source: t.source,
            opacity: t.opacity,
            _pad: 0.0,
            delta: [t.delta[0], t.delta[1], t.delta[2], 0.0],
        }
    }
}

/// [`ToneCurve::lut`] as the shader's `curve_lut` uniform.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GpuCurve {
    pub lut: curve::Lut,
}

impl From<&ToneCurve> for GpuCurve {
    fn from(c: &ToneCurve) -> Self {
        GpuCurve { lut: c.lut() }
    }
}

/// [`Adjustments`] as the shader's uniform buffer. Uniforms need 16-byte
/// alignment, so keep the field count a multiple of 4. `texel_w`/`texel_h` are
/// 1/width and 1/height of the shown image, set by `App::gpu_adjust` so
/// denoise can step by whole texels.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GpuAdjust {
    pub exposure: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub temp: f32,
    pub tint: f32,
    pub crop_l: f32,
    pub crop_t: f32,
    pub crop_r: f32,
    pub crop_b: f32,
    pub denoise: f32,
    pub vibrance: f32,
    pub saturation: f32,
    pub texel_w: f32,
    pub texel_h: f32,
    pub _pad0: f32,
    /// [`Adjustments::straighten`] in radians.
    pub straighten: f32,
    /// [`CaScale::red`], 0 when the correction is off.
    pub ca_red: f32,
    /// [`CaScale::blue`], 0 when the correction is off.
    pub ca_blue: f32,
    /// 1 when [`Adjustments::curve`] is bent and the shader should read its
    /// table, 0 to skip it.
    pub curve_on: f32,
    /// A test view of a layer in place of the photo: 0 off, then 1 to 5 for
    /// `mid`, `wide`, `dark`, the detail and the fine layer's `mid`. See
    /// [`BaseProbe::gpu`] and `baseProbe` in loupe_common.wgsl.
    pub base_probe: f32,
    /// 1 when [`Adjustments::needs_base`]: the renderer builds the base
    /// layer only then, or for a probe.
    pub base_on: f32,
    pub texture: f32,
    pub clarity: f32,
    pub dehaze: f32,
    /// 1 when Texture reads the fine layer: the renderer builds it only
    /// then, or for a probe.
    pub fine_on: f32,
    /// Set by [`GpuAdjust::for_image`], which knows the image size.
    pub sharpen: Sharpen,
    /// Set by [`GpuAdjust::for_image`], which knows the image size.
    pub effects: Effects,
    pub color: ColorOps,
    pub grade: GradeOps,
    /// How many masks the shader reads from its mask buffer.
    pub mask_count: f32,
    pub _mpad: [f32; 3],
}

impl Default for GpuAdjust {
    fn default() -> Self {
        Self {
            exposure: 0.0,
            contrast: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            temp: 0.0,
            tint: 0.0,
            crop_l: 0.0,
            crop_t: 0.0,
            crop_r: 1.0,
            crop_b: 1.0,
            denoise: 0.0,
            vibrance: 0.0,
            saturation: 0.0,
            texel_w: 1.0,
            texel_h: 1.0,
            _pad0: 0.0,
            straighten: 0.0,
            ca_red: 0.0,
            ca_blue: 0.0,
            curve_on: 0.0,
            base_probe: 0.0,
            base_on: 0.0,
            texture: 0.0,
            clarity: 0.0,
            dehaze: 0.0,
            fine_on: 0.0,
            sharpen: Sharpen::default(),
            effects: Effects::default(),
            color: ColorOps::default(),
            grade: GradeOps::default(),
            mask_count: 0.0,
            _mpad: [0.0; 3],
        }
    }
}

impl GpuAdjust {
    /// The uniform for `adj` on a `width x height` image with `touchups`
    /// spot-heals and `masks` masks. Denoise steps by whole texels, and the
    /// shader reads the touch-up count from `_pad0`.
    pub fn for_image(
        adj: &Adjustments,
        width: f32,
        height: f32,
        touchups: usize,
        masks: usize,
    ) -> Self {
        Self {
            texel_w: 1.0 / width,
            texel_h: 1.0 / height,
            _pad0: touchups as f32,
            mask_count: masks.min(MAX_MASKS) as f32,
            sharpen: Sharpen::new(&adj.sharpening, width, height),
            effects: Effects::new(&adj.vignette, &adj.grain, adj.crop, width, height),
            ..Self::from(adj)
        }
    }
}

impl From<&Adjustments> for GpuAdjust {
    /// Leaves `texel_w`/`texel_h` at 1.0; [`GpuAdjust::for_image`] sets them.
    fn from(a: &Adjustments) -> Self {
        let a = &a.clamped();
        let (crop_l, crop_t, crop_r, crop_b) = match a.crop {
            Some(c) => (c.left, c.top, c.right, c.bottom),
            None => (0.0, 0.0, 1.0, 1.0),
        };
        let ca = a.chromatic_aberration.unwrap_or_default();
        Self {
            exposure: a.exposure,
            contrast: a.contrast,
            highlights: a.highlights,
            shadows: a.shadows,
            whites: a.whites,
            blacks: a.blacks,
            temp: a.temp,
            tint: a.tint,
            crop_l,
            crop_t,
            crop_r,
            crop_b,
            denoise: a.denoise,
            vibrance: a.vibrance,
            saturation: a.saturation,
            straighten: a.straighten.to_radians(),
            ca_red: ca.red,
            ca_blue: ca.blue,
            curve_on: if a.curve.is_linear() { 0.0 } else { 1.0 },
            base_on: if a.needs_base() { 1.0 } else { 0.0 },
            fine_on: if a.texture != 0.0 { 1.0 } else { 0.0 },
            texture: a.texture.clamp(-100.0, 100.0),
            clarity: a.clarity.clamp(-100.0, 100.0),
            dehaze: a.dehaze.clamp(-100.0, 100.0),
            color: ColorOps::new(&a.mixer, &a.gray_mix, a.is_monochrome()),
            grade: GradeOps::new(&a.grade),
            ..Self::default()
        }
    }
}

/// The Exposure slider on one linear-light pixel. Darkening is a plain
/// `2^stops` gain, which also recovers RAW values above white. Brightening
/// maps luma through `x·g / (1 + x·(g − 1))`, the standard rational tone curve
/// (Reinhard et al., 2002): black stays black and white stays white, shadows
/// gain close to the full `g = 2^stops`, and highlights ease into white
/// instead of clipping. Colors keep `1 / (1 + x·(g − 1))` of their chroma
/// relative to luma, the share of headroom left, so a color reaching white
/// goes pale, not neon.
///
/// Must match `exposureCurve` in loupe_common.wgsl.
fn exposure_curve(rgb: [f32; 3], stops: f32) -> [f32; 3] {
    let gain = stops.exp2();
    if stops <= 0.0 {
        return rgb.map(|v| v * gain);
    }
    // Rec.709 luma, because these values are linear light.
    let luma = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
    let d = 1.0 + luma.max(0.0) * (gain - 1.0);
    let new_luma = luma * gain / d;
    let chroma = gain / (d * d);
    rgb.map(|v| new_luma + (v - luma) * chroma)
}

/// The look curve for every RAW display rendering, so non-mac RAW matches
/// what Apple ImageIO shows on macOS. Without it, a plain linear-to-sRGB
/// conversion renders about 1.45x darker. The input is already sRGB-encoded,
/// not linear.
///
/// The knots are evenly spaced over 0..=1 and were fitted with
/// `decode_probe --fit-look` over 148 Sony ARW files. `raw_shader.wgsl` has a
/// WGSL twin with the same knots, and [`apply_raw_display`] runs this same
/// function, so the histogram and Auto Tone measure what the shader draws.
pub(crate) const RAW_LOOK_KNOTS: [f32; 17] = [
    0.000, 0.027, 0.122, 0.220, 0.337, 0.463, 0.576, 0.678, 0.757, 0.824, 0.878, 0.918, 0.949,
    0.976, 0.992, 1.000, 1.000,
];

/// The sRGB transfer curve, encoded 0..1 to linear light. The Loupe's
/// `Rgba8UnormSrgb` texture decodes with this same curve in hardware, so
/// every CPU reader of sRGB8 pixels must use it too.
pub fn srgb_decode(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear light to the sRGB encoding, 0 for anything at or below black.
/// Must match `linear_to_srgb` in loupe_common.wgsl.
pub fn srgb_encode(v: f32) -> f32 {
    let v = v.max(0.0);
    if v <= 0.0031308 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

pub fn apply_raw_preview_boost(srgb: f32) -> f32 {
    let x = srgb.clamp(0.0, 1.0) * (RAW_LOOK_KNOTS.len() - 1) as f32;
    let i = (x as usize).min(RAW_LOOK_KNOTS.len() - 2);
    let t = x - i as f32;
    RAW_LOOK_KNOTS[i] + (RAW_LOOK_KNOTS[i + 1] - RAW_LOOK_KNOTS[i]) * t
}

/// [`Develop::linear_at`] as raw_shader.wgsl runs it, for an unframed pixel.
/// Bakes the tone curve on every call, so a loop over pixels uses
/// [`Develop::raw_display`] instead. Returns display values, not
/// linear, because the RAW shader's render target is not an sRGB surface.
pub fn apply_raw_display(adj: &Adjustments, rgb: [f32; 3]) -> [f32; 3] {
    Develop::new(adj).raw_display(rgb)
}

/// Every edit, applied to the pixels of one image, with the tone curve
/// baked once instead of per pixel. Without a frame
/// ([`Develop::framed`]) it leaves out Vignette and Grain, which depend on
/// where a pixel sits; Auto Tone measures tone without them.
pub struct Develop {
    adj: Adjustments,
    curve: Option<Box<curve::Lut>>,
    color: ColorOps,
    grade: GradeOps,
    frame: Option<Frame>,
}

/// The canvas a framed [`Develop`] renders: its [`Effects`] and one output
/// pixel's size in canvas UV.
struct Frame {
    effects: Effects,
    texel: [f32; 2],
}

impl Develop {
    pub fn new(adj: &Adjustments) -> Self {
        let adj = adj.clamped();
        let curve = (!adj.curve.is_linear()).then(|| Box::new(adj.curve.lut()));
        Develop {
            color: ColorOps::new(&adj.mixer, &adj.gray_mix, adj.is_monochrome()),
            grade: GradeOps::new(&adj.grade),
            adj,
            curve,
            frame: None,
        }
    }

    /// A render of a `w x h` canvas, one output pixel per canvas pixel, as
    /// the bake does: Vignette and Grain included.
    pub fn framed(adj: &Adjustments, w: u32, h: u32) -> Self {
        let develop = Develop::new(adj);
        let a = &develop.adj;
        let effects = Effects::new(&a.vignette, &a.grain, a.crop, w as f32, h as f32);
        Develop {
            frame: (!effects.is_off()).then_some(Frame {
                effects,
                texel: [1.0 / w as f32, 1.0 / h as f32],
            }),
            ..develop
        }
    }

    pub fn raw_display(&self, rgb: [f32; 3]) -> [f32; 3] {
        apply_linear_impl(self, rgb, None, true, None)
    }

    /// Every edit applied to one linear-light pixel at canvas UV `(u, v)`,
    /// which only a framed render reads. Input may exceed 1.0; output is
    /// linear RGB clamped to 0..1. Must match `fs_main` in shader.wgsl. Keep it
    /// simple so the two stay in sync.
    pub fn linear_at(&self, rgb: [f32; 3], u: f32, v: f32) -> [f32; 3] {
        let at = self.frame.as_ref().map(|f| (f, u, v));
        apply_linear_impl(self, rgb, at, false, None)
    }

    /// [`Develop::linear_at`] with `local` in place of the photo's own
    /// values for the sliders a mask moves; see [`mask::local_tone`].
    pub fn linear_at_local(&self, rgb: [f32; 3], u: f32, v: f32, local: &LocalAmounts) -> [f32; 3] {
        let at = self.frame.as_ref().map(|f| (f, u, v));
        apply_linear_impl(self, rgb, at, false, Some(local))
    }

    /// The pixel at canvas UV `(u, v)` as the Loupe puts it on screen for a
    /// photo decoded as `format`, sRGB-encoded 0..1: through `shader.wgsl`'s
    /// math for `Srgb8` and `TonedF16`, `raw_shader.wgsl`'s for `LinearF16`.
    /// `local` stands in for the photo's own values under a mask.
    pub fn display_at(
        &self,
        format: PixelFormat,
        rgb: [f32; 3],
        u: f32,
        v: f32,
        local: Option<&LocalAmounts>,
    ) -> [f32; 3] {
        let at = self.frame.as_ref().map(|f| (f, u, v));
        match format {
            PixelFormat::Srgb8 | PixelFormat::TonedF16 => {
                apply_linear_impl(self, rgb, at, false, local).map(srgb_encode)
            }
            PixelFormat::LinearF16 => apply_linear_impl(self, rgb, at, true, local),
        }
    }
}

fn apply_linear_impl(
    dev: &Develop,
    rgb: [f32; 3],
    at: Option<(&Frame, f32, f32)>,
    raw_display: bool,
    local: Option<&LocalAmounts>,
) -> [f32; 3] {
    let adj = &dev.adj;
    // The nine sliders a mask can move, here or for the whole photo.
    let l = local.copied().unwrap_or_else(|| LocalAmounts::of(adj));
    let [mut r, mut g, mut b] = rgb;

    // White balance as small per-channel gains, at most ±0.3.
    let t = l.temp / 100.0;
    let ti = l.tint / 100.0;
    let r_gain = 1.0 + t * 0.3;
    let b_gain = 1.0 - t * 0.3;
    let g_gain = 1.0 - ti * 0.15;
    r *= r_gain;
    g *= g_gain;
    b *= b_gain;

    let exposed = exposure_curve([r, g, b], l.exposure);
    r = exposed[0];
    g = exposed[1];
    b = exposed[2];

    // Tone and colour steps below work in gamma space. The RAW path uses
    // raw_shader.wgsl's display curve instead of plain 2.2 gamma.
    let to_gamma = |x: f32| {
        if raw_display {
            apply_raw_preview_boost(srgb_encode(x))
        } else {
            x.max(0.0).powf(1.0 / 2.2)
        }
    };
    let mut rg = to_gamma(r);
    let mut gg = to_gamma(g);
    let mut bg = to_gamma(b);

    let tone = |v: f32| -> f32 {
        let mut x = v;

        let blacks = l.blacks / 100.0 * 0.2;
        let whites = l.whites / 100.0 * 0.2;
        // Map input `-blacks` to 0 and `1 - whites` to 1. Positive Whites
        // brightens the top end, as in Lightroom.
        x = (x + blacks) / ((1.0 - whites) + blacks);

        // S-curve around 0.5.
        let c = l.contrast / 100.0 * 0.5;
        if c != 0.0 {
            let d = x - 0.5;
            x = 0.5 + d + c * d * (1.0 - 4.0 * d * d);
        }

        // Shadows and Highlights are weighted toward black and white respectively.
        let s = l.shadows / 100.0 * 0.3;
        if s != 0.0 {
            let mask = (1.0 - x.clamp(0.0, 1.0)).powf(2.0);
            x += s * mask;
        }

        let h = l.highlights / 100.0 * 0.3;
        if h != 0.0 {
            let mask = x.clamp(0.0, 1.0).powf(2.0);
            x += h * mask;
        }

        x
    };
    rg = tone(rg);
    gg = tone(gg);
    bg = tone(bg);

    if let Some(lut) = dev.curve.as_deref() {
        rg = curve::apply(lut, 0, rg);
        gg = curve::apply(lut, 1, gg);
        bg = curve::apply(lut, 2, bg);
    }

    [rg, gg, bg] = dev.color.apply([rg, gg, bg]);

    // Vibrance and saturation scale chroma around Rec.601 luma. Vibrance is
    // weaker on pixels that are already saturated.
    let luma = 0.299 * rg + 0.587 * gg + 0.114 * bg;
    let sat_total = 1.0 + l.saturation / 100.0;
    let cmax = rg.max(gg).max(bg);
    let cmin = rg.min(gg).min(bg);
    let cur_sat = if cmax > 0.0 {
        (cmax - cmin) / cmax
    } else {
        0.0
    };
    let vib_factor = 1.0 + adj.vibrance / 100.0 * (1.0 - cur_sat);
    let total = sat_total * vib_factor;
    rg = luma + (rg - luma) * total;
    gg = luma + (gg - luma) * total;
    bg = luma + (bg - luma) * total;

    [rg, gg, bg] = dev.grade.apply([rg, gg, bg]);

    if let Some((frame, u, v)) = at {
        [rg, gg, bg] = frame.effects.apply([rg, gg, bg], u, v, frame.texel);
    }

    if raw_display {
        return [rg.clamp(0.0, 1.0), gg.clamp(0.0, 1.0), bg.clamp(0.0, 1.0)];
    }

    let to_linear = |x: f32| x.max(0.0).powf(2.2);
    r = to_linear(rg);
    g = to_linear(gg);
    b = to_linear(bg);

    [r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0)]
}

/// The `(temp, tint)` that turn an unedited linear-light pixel gray, by
/// inverting the white-balance gains in [`Develop::linear_at`]:
///
/// ```text
/// r*(1+0.3t)  = b*(1-0.3t)        =>  t  = (b - r) / (0.3*(r + b))
/// r*(1+0.3t)  = g*(1-0.15*ti)     =>  ti = (1 - r*(1+0.3t)/g) / 0.15
/// ```
///
/// Later steps keep gray pixels gray, so other edits don't matter. `None` when
/// the pixel is too dark to solve.
pub fn neutralize_gray(rgb: [f32; 3]) -> Option<(f32, f32)> {
    let [r, g, b] = rgb;
    const EPS: f32 = 0.02;
    if r + b < EPS || g < EPS {
        return None;
    }
    let t = ((b - r) / (0.3 * (r + b))).clamp(-1.0, 1.0);
    let ti = ((1.0 - r * (1.0 + 0.3 * t) / g) / 0.15).clamp(-1.0, 1.0);
    Some((t * 100.0, ti * 100.0))
}

/// Denoise kernel radius (5x5). Fixed so cost doesn't grow with strength;
/// the slider only changes how much neighbors blend.
pub(crate) const DENOISE_RADIUS: i32 = 2;

/// Gaussian (sigma 1) weight by squared tap distance, precomputed.
/// Must match `spatialWeight` in loupe_common.wgsl.
fn spatial_weight(dx: i32, dy: i32) -> f32 {
    match dx * dx + dy * dy {
        0 => 1.0,
        1 => 0.606531,
        2 => 0.367879,
        4 => 0.135335,
        5 => 0.082085,
        8 => 0.018316,
        _ => 0.0,
    }
}

/// Edge-preserving (bilateral) denoise of one linear-light pixel.
/// `sample(dx, dy)` returns the neighbor at that offset; the caller handles
/// edges. Returns `sample(0, 0)` exactly when denoise is off.
///
/// Must match the denoise branch of `fs_main` in shader.wgsl.
pub fn denoise_sample(adj: &Adjustments, sample: impl Fn(i32, i32) -> [f32; 3]) -> [f32; 3] {
    denoise_sample_with_strength(adj.denoise, sample)
}

/// [`denoise_sample`] with a plain strength, for the RAW decoders' fixed
/// denoise pass.
fn denoise_sample_with_strength(strength: f32, sample: impl Fn(i32, i32) -> [f32; 3]) -> [f32; 3] {
    if strength <= 0.0 {
        return sample(0, 0);
    }
    let center = sample(0, 0);
    let sigma_r = 0.02 + strength / 100.0 * 0.30;
    let sigma_r2 = sigma_r * sigma_r;
    let (mut sum, mut wsum) = ([0f32; 3], 0f32);
    for dy in -DENOISE_RADIUS..=DENOISE_RADIUS {
        for dx in -DENOISE_RADIUS..=DENOISE_RADIUS {
            let tap = sample(dx, dy);
            let d = [tap[0] - center[0], tap[1] - center[1], tap[2] - center[2]];
            let diff2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
            // The center tap has weight 1.0, so `wsum` is never 0.
            let w = spatial_weight(dx, dy) / (1.0 + diff2 / sigma_r2);
            for c in 0..3 {
                sum[c] += w * tap[c];
            }
            wsum += w;
        }
    }
    [sum[0] / wsum, sum[1] / wsum, sum[2] / wsum]
}

/// Denoise one pixel of a packed linear-light buffer, clamping at its edges.
/// The `rawler` RAW decoders (browser, Linux, Windows) call this after
/// demosaic because rawler doesn't denoise; ImageIO on macOS does its own.
/// Callers consume pixels as they go, a band of rows per thread, so there is
/// no second full-size buffer.
#[cfg(not(target_os = "macos"))]
pub(crate) fn denoise_linear_rgb_pixel(
    strength: f32,
    width: usize,
    height: usize,
    buf: &[[f32; 3]],
    x: usize,
    y: usize,
) -> [f32; 3] {
    denoise_sample_with_strength(strength, |dx, dy| {
        let sx = (x as i64 + dx as i64).clamp(0, width as i64 - 1) as usize;
        let sy = (y as i64 + dy as i64).clamp(0, height as i64 - 1) as usize;
        buf[sy * width + sx]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One unframed pixel through the whole pipeline.
    fn apply_linear(adj: &Adjustments, rgb: [f32; 3]) -> [f32; 3] {
        Develop::new(adj).linear_at(rgb, 0.0, 0.0)
    }

    #[test]
    fn every_slider_name_parses_back_to_its_own_slider() {
        for slider in &SLIDERS {
            let name = slider.id.to_string();
            assert_eq!(name.parse::<SliderId>(), Ok(slider.id), "{name}");
            let json = serde_json::to_string(&slider.id).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            assert_eq!(serde_json::from_str::<SliderId>(&json).unwrap(), slider.id);
        }
    }

    #[test]
    fn no_two_sliders_share_a_name() {
        let names: std::collections::BTreeSet<String> =
            SLIDERS.iter().map(|s| s.id.to_string()).collect();
        assert_eq!(names.len(), SLIDERS.len());
    }

    #[test]
    fn slider_names_are_the_documented_wire_names() {
        for (id, name) in [
            (SliderId::Exposure, "exposure"),
            (
                SliderId::Mix(ColorAxis::Saturation, Band::Blue),
                "mix.blue.saturation",
            ),
            (SliderId::Gray(Band::Aqua), "gray.aqua"),
            (
                SliderId::Grade(Zone::Shadows, ColorAxis::Hue),
                "grade.shadows.hue",
            ),
            (SliderId::VignetteAmount, "vignette.amount"),
        ] {
            assert_eq!(id.to_string(), name);
        }
        assert!("Exposure".parse::<SliderId>().is_err(), "names are exact");
        assert!("mix.blue".parse::<SliderId>().is_err());
    }

    fn edit_signature(adj: &Adjustments, rot: u8) -> u64 {
        edit_signature_with_touchups(adj, &[], &[], &Detections::default(), rot)
    }

    /// 0 and 1 stay fixed, so the boost never clips or crushes, and mid-gray
    /// comes out brighter.
    #[test]
    fn raw_preview_boost_is_identity_at_endpoints_and_brightens_midtones() {
        assert_eq!(apply_raw_preview_boost(0.0), 0.0);
        assert!((apply_raw_preview_boost(1.0) - 1.0).abs() < 1e-6);

        let mid = apply_raw_preview_boost(0.5);
        assert!(mid > 0.5, "expected midtone brightening, got {mid}");
    }

    #[test]
    fn raw_shader_look_knots_match_cpu() {
        let wgsl = include_str!("../shaders/raw_shader.wgsl");
        let body = wgsl
            .split("const RAW_LOOK_KNOTS = array<f32, 17>(")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .expect("RAW_LOOK_KNOTS in raw_shader.wgsl");
        let shader: Vec<f32> = body
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.parse().expect("knot literal"))
            .collect();
        assert_eq!(shader, RAW_LOOK_KNOTS);
    }

    /// At the identity edit, the CPU RAW pipeline must land exactly on the
    /// shader's curve: sRGB-encode, then the look curve.
    #[test]
    fn raw_display_at_identity_follows_the_look_curve() {
        let adj = Adjustments::default();
        for linear in [0.0, 0.002, 0.01, 0.05, 0.18, 0.4, 0.7, 1.0] {
            let srgb = if linear <= 0.0031308 {
                linear * 12.92
            } else {
                1.055 * f32::powf(linear, 1.0 / 2.4) - 0.055
            };
            let want = apply_raw_preview_boost(srgb);
            let got = apply_raw_display(&adj, [linear; 3])[0];
            assert!(
                (got - want).abs() < 1e-4,
                "linear {linear}: got {got}, want {want}"
            );
        }
    }

    /// A `TonedF16` photo already carries Apple's curve, so it displays
    /// exactly as `Srgb8` does from the same linear values; only a
    /// scene-linear `LinearF16` one gets the RAW look curve.
    #[test]
    fn display_at_curves_only_a_scene_linear_photo() {
        let adj = Adjustments {
            exposure: 0.3,
            ..Default::default()
        };
        let dev = Develop::new(&adj);
        for linear in [0.0, 0.01, 0.18, 0.5, 1.0] {
            let px = [linear, linear * 0.8, linear * 0.5];
            let srgb = dev.display_at(PixelFormat::Srgb8, px, 0.5, 0.5, None);
            assert_eq!(
                dev.display_at(PixelFormat::TonedF16, px, 0.5, 0.5, None),
                srgb
            );
            assert_eq!(srgb, dev.linear_at(px, 0.0, 0.0).map(srgb_encode));
            assert_eq!(
                dev.display_at(PixelFormat::LinearF16, px, 0.5, 0.5, None),
                dev.raw_display(px)
            );
        }
        let mid = [0.18; 3];
        assert_ne!(
            dev.display_at(PixelFormat::LinearF16, mid, 0.5, 0.5, None),
            dev.display_at(PixelFormat::TonedF16, mid, 0.5, 0.5, None)
        );
    }

    fn crop(l: f32, t: f32, r: f32, b: f32) -> Crop {
        Crop {
            left: l,
            top: t,
            right: r,
            bottom: b,
        }
    }

    #[test]
    fn the_largest_straightened_crop_just_fits() {
        for deg in [-30.0, -7.5, 0.0, 3.0, 45.0] {
            for ratio in [4.0 / 3.0, 9.0 / 16.0, 1.0] {
                let turn = Straighten::new(deg, 4000.0, 3000.0);
                let r = turn.largest_crop(ratio);
                assert!(turn.covers(r), "{deg} {ratio}: {r:?}");
                let px = ((r.right - r.left) * 4000.0) / ((r.bottom - r.top) * 3000.0);
                assert!((px - ratio).abs() < 1e-3, "{deg} {ratio}: {px}");
                let grow = |v: f32| 0.5 + (v - 0.5) * 1.01;
                let bigger = Crop {
                    left: grow(r.left),
                    top: grow(r.top),
                    right: grow(r.right),
                    bottom: grow(r.bottom),
                };
                assert!(!turn.covers(bigger), "{deg} {ratio}: room to grow");
            }
        }
    }

    #[test]
    fn straighten_round_trips() {
        let turn = Straighten::new(12.0, 4000.0, 3000.0);
        let (u, v) = turn.to_source(0.2, 0.7);
        let (u, v) = turn.to_canvas(u, v);
        assert!((u - 0.2).abs() < 1e-5 && (v - 0.7).abs() < 1e-5);
    }

    #[test]
    fn edit_signature_differs_on_straighten() {
        let turned = Adjustments {
            straighten: 2.5,
            ..Default::default()
        };
        assert_ne!(
            edit_signature(&turned, 0),
            edit_signature(&Adjustments::default(), 0)
        );
        assert_eq!(
            turned.tone_only().straighten,
            0.0,
            "a look never carries a turn"
        );
    }

    #[test]
    fn edit_signature_stable() {
        let a = Adjustments {
            exposure: 1.5,
            crop: Some(crop(0.1, 0.2, 0.8, 0.9)),
            ..Default::default()
        };
        assert_eq!(edit_signature(&a, 1), edit_signature(&a, 1));
    }

    #[test]
    fn edit_signature_identity_is_fixed() {
        // A full-frame crop must hash the same as no crop.
        let base = edit_signature(&Adjustments::default(), 0);
        let full = Adjustments {
            crop: Some(crop(0.0, 0.0, 1.0, 1.0)),
            ..Default::default()
        };
        assert_eq!(edit_signature(&full, 0), base);
    }

    #[test]
    fn edit_signature_differs_on_crop() {
        let a = Adjustments::default();
        let b = Adjustments {
            crop: Some(crop(0.1, 0.1, 0.9, 0.9)),
            ..Default::default()
        };
        assert_ne!(edit_signature(&a, 0), edit_signature(&b, 0));
    }

    #[test]
    fn edit_signature_differs_on_tone() {
        let a = Adjustments::default();
        let b = Adjustments {
            exposure: 0.5,
            ..Default::default()
        };
        assert_ne!(edit_signature(&a, 0), edit_signature(&b, 0));
    }

    #[test]
    fn edit_signature_differs_on_rotation() {
        let a = Adjustments::default();
        assert_ne!(edit_signature(&a, 0), edit_signature(&a, 1));
    }

    #[test]
    fn edit_signature_quantizes() {
        let a = Adjustments {
            exposure: 1.0,
            ..Default::default()
        };
        let b = Adjustments {
            exposure: 1.0 + 1e-5,
            ..Default::default()
        };
        assert_eq!(edit_signature(&a, 0), edit_signature(&b, 0));
    }

    fn luma709(px: [f32; 3]) -> f32 {
        0.2126 * px[0] + 0.7152 * px[1] + 0.0722 * px[2]
    }

    #[test]
    fn a_slider_value_out_of_range_renders_as_the_slider_end() {
        let px = [0.6, 0.3, 0.2];
        let with = |exposure, contrast| Adjustments {
            exposure,
            contrast,
            ..Adjustments::default()
        };
        let five = with(5.0, 100.0);
        for bad in [1e30, f32::INFINITY] {
            let adj = with(bad, bad);
            assert_eq!(apply_linear(&adj, px), apply_linear(&five, px), "{bad}");
            let gpu = GpuAdjust::from(&adj);
            assert_eq!((gpu.exposure, gpu.contrast), (5.0, 100.0), "{bad}");
        }
        let nan = with(f32::NAN, f32::NAN);
        assert_eq!(
            apply_linear(&nan, px),
            apply_linear(&Adjustments::default(), px)
        );
        let gpu = GpuAdjust::from(&nan);
        assert_eq!((gpu.exposure, gpu.contrast), (0.0, 0.0));
    }

    #[test]
    fn a_straighten_angle_out_of_range_renders_as_the_nearest_end() {
        let with = |straighten| Adjustments {
            straighten,
            ..Adjustments::default()
        };
        for (bad, end) in [
            (1e30_f32, 45.0_f32),
            (f32::INFINITY, 45.0),
            (f32::NEG_INFINITY, -45.0),
            (f32::NAN, 0.0),
        ] {
            let gpu = GpuAdjust::from(&with(bad));
            assert_eq!(gpu.straighten, end.to_radians(), "{bad}");
        }
    }

    #[test]
    fn a_straighten_turn_from_a_bad_angle_stays_finite_and_in_range() {
        let (w, h) = (4000.0, 3000.0);
        let at = |deg| Straighten::new(deg, w, h).to_source(0.9, 0.1);
        assert_eq!(at(f32::NAN), at(0.0), "a NaN is no turn");
        assert_eq!(at(1e30), at(45.0));
        assert_eq!(at(f32::NEG_INFINITY), at(-45.0));
        let (u, v) = at(f32::INFINITY);
        assert!(u.is_finite() && v.is_finite());
    }

    #[test]
    fn a_crop_that_is_not_a_box_is_dropped_or_held_to_the_frame() {
        let at = |left, top, right, bottom| {
            Crop {
                left,
                top,
                right,
                bottom,
            }
            .sanitized()
        };
        let nan = f32::NAN;
        let edges = |c: Option<Crop>| c.map(|c| [c.left, c.top, c.right, c.bottom]);
        assert_eq!(edges(at(0.1, 0.2, 0.8, 0.9)), Some([0.1, 0.2, 0.8, 0.9]));
        for gone in [
            at(nan, 0.0, 1.0, 1.0),
            at(0.0, 0.0, f32::INFINITY, 1.0),
            at(0.5, 0.0, 0.2, 1.0),
            at(0.0, 0.0, 0.01, 1.0),
            at(2.0, 0.0, 3.0, 1.0),
        ] {
            assert_eq!(gone, None);
        }
        assert_eq!(edges(at(-5.0, -1.0, 5.0, 1.5)), Some([0.0, 0.0, 1.0, 1.0]));
        assert_eq!(edges(at(0.7, 0.0, 5.0, 1.0)), Some([0.7, 0.0, 1.0, 1.0]));
    }

    #[test]
    fn a_crop_in_a_sidecar_is_sanitized_as_it_loads() {
        let load = |crop: &str| {
            serde_json::from_str::<Adjustments>(&format!("{{\"crop\":{crop}}}")).unwrap()
        };
        let good = load(r#"{"left":0.1,"top":0.2,"right":0.8,"bottom":0.9}"#);
        assert_eq!(good.crop.map(|c| c.right), Some(0.8));
        let wide = load(r#"{"left":-5,"top":0,"right":5,"bottom":1}"#);
        assert_eq!(wide.crop.map(|c| (c.left, c.right)), Some((0.0, 1.0)));
        for bad in [
            r#"{"left":0.5,"top":0,"right":0.2,"bottom":1}"#,
            r#"{"left":0,"top":0,"right":0.001,"bottom":1}"#,
        ] {
            assert_eq!(load(bad).crop, None, "{bad}");
        }
    }

    #[test]
    fn a_positive_straighten_reads_the_source_clockwise_of_the_canvas() {
        // The canvas's right-middle reads the source below its right-middle,
        // which shows the photo turned anti-clockwise.
        let (_, v) = Straighten::new(10.0, 4000.0, 3000.0).to_source(1.0, 0.5);
        assert!(v > 0.5, "{v}");
    }

    #[test]
    fn exposure_curve_zero_is_identity() {
        let px = [0.6, 0.3, 0.2];
        assert_eq!(exposure_curve(px, 0.0), px);
        let out = apply_linear(&Adjustments::default(), px);
        for i in 0..3 {
            assert!(
                (out[i] - px[i]).abs() < 1e-5,
                "channel {i}: {} vs {}",
                out[i],
                px[i]
            );
        }
    }

    #[test]
    fn exposure_curve_colored_highlights_are_continuous_at_zero() {
        for px in [[1.0, 1.0, 0.0], [1.5, 1.0, 0.5], [3.0, 2.0, 1.0]] {
            for stops in [-1e-4, -1e-5, 1e-5, 1e-4] {
                let out = exposure_curve(px, stops);
                for i in 0..3 {
                    assert!(
                        (out[i] - px[i]).abs() < 10.0 * stops.abs(),
                        "discontinuity for {px:?} at {stops}: {out:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn negative_exposure_recovers_raw_highlights_in_display_pipeline() {
        for pipeline in [apply_linear, apply_raw_display] {
            // Values just above white and several stops above it.
            for level in [1.06, 2.12, 4.24, 8.0, 16.0] {
                let mut previous = 1.0;
                for step in 0..=50 {
                    let adj = Adjustments {
                        exposure: -(step as f32) / 10.0,
                        ..Default::default()
                    };
                    let out = pipeline(&adj, [level; 3]);
                    assert!(out.iter().all(|v| v.is_finite()));
                    assert!(out[0] <= previous + 1e-6);
                    assert!((out[0] - out[1]).abs() < 1e-6);
                    assert!((out[0] - out[2]).abs() < 1e-6);
                    previous = out[0];
                }
                assert!(previous > 0.0, "highlight {level} went black");
            }
        }
        // Both pipelines share the exposure step, so at -5 stops RAW must
        // show the look curve of the linear pipeline's recovered value.
        let adj = Adjustments {
            exposure: -5.0,
            ..Default::default()
        };
        for level in [1.06, 2.12, 4.24, 8.0, 16.0] {
            let linear = apply_linear(&adj, [level; 3])[0];
            assert!(
                linear < 0.9,
                "highlight {level} failed to recover at -5 stops: {linear}"
            );
            let srgb = 1.055 * linear.powf(1.0 / 2.4) - 0.055;
            let raw = apply_raw_display(&adj, [level; 3])[0];
            assert!((raw - apply_raw_preview_boost(srgb)).abs() < 1e-4);
        }
    }

    #[test]
    fn exposure_curve_is_monotonic_in_the_slider() {
        let px = [0.2, 0.2, 0.2];
        let mut prev = f32::NEG_INFINITY;
        for stops in [-5.0, -3.0, -1.0, 0.0, 1.0, 3.0, 5.0] {
            let out = luma709(exposure_curve(px, stops));
            assert!(
                out > prev,
                "luma fell going to {stops} stops: {out} after {prev}"
            );
            prev = out;
        }
    }

    #[test]
    fn exposure_curve_lifts_shadows_more_than_highlights() {
        // A plain `2^stops` gain would give both the same ratio.
        let shadow = [0.05, 0.05, 0.05];
        let highlight = [0.9, 0.9, 0.9];
        let shadow_gain = luma709(exposure_curve(shadow, 1.0)) / luma709(shadow);
        let highlight_gain = luma709(exposure_curve(highlight, 1.0)) / luma709(highlight);
        assert!(
            shadow_gain > highlight_gain,
            "expected shadows to gain more than highlights: {shadow_gain} vs {highlight_gain}"
        );
    }

    #[test]
    fn exposure_curve_desaturates_as_it_brightens() {
        let px = [0.5, 0.1, 0.1];
        let chroma_ratio = |p: [f32; 3]| {
            let l = luma709(p);
            (p[0] - l).abs() / l
        };
        let before = chroma_ratio(px);
        let after = chroma_ratio(exposure_curve(px, 2.0));
        assert!(
            after < before,
            "expected brightening to desaturate: {after} vs {before}"
        );
    }

    #[test]
    fn exposure_curve_is_finite_at_the_edges() {
        // Black, white, a pure single-channel color, and a RAW value above
        // white.
        for px in [
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [1.0, 0.0, 0.0],
            [2.0, 2.0, 2.0],
        ] {
            for stops in [-5.0, -1.0, 0.0, 1.0, 5.0] {
                let out = exposure_curve(px, stops);
                assert!(
                    out.iter().all(|v| v.is_finite()),
                    "non-finite output for {px:?} at {stops} stops: {out:?}"
                );
            }
        }
    }

    #[test]
    fn whites_brightens_the_top_end() {
        // Lightroom's direction: positive Whites brightens, negative recovers.
        let bright = [0.8, 0.8, 0.8];
        let up = apply_linear(
            &Adjustments {
                whites: 60.0,
                ..Default::default()
            },
            bright,
        );
        let down = apply_linear(
            &Adjustments {
                whites: -60.0,
                ..Default::default()
            },
            bright,
        );
        assert!(up[0] > bright[0], "whites +60 should brighten: {}", up[0]);
        assert!(
            down[0] < bright[0],
            "whites -60 should recover: {}",
            down[0]
        );
    }

    #[test]
    fn blacks_lifts_the_floor() {
        let dark = [0.03, 0.03, 0.03];
        let up = apply_linear(
            &Adjustments {
                blacks: 60.0,
                ..Default::default()
            },
            dark,
        );
        let down = apply_linear(
            &Adjustments {
                blacks: -60.0,
                ..Default::default()
            },
            dark,
        );
        assert!(up[0] > dark[0], "blacks +60 should lift: {}", up[0]);
        assert!(down[0] < dark[0], "blacks -60 should crush: {}", down[0]);
    }

    #[test]
    fn denoise_zero_is_passthrough() {
        let adj = Adjustments::default();
        // Any blending with these neighbors would change the result.
        let out = denoise_sample(&adj, |dx, dy| {
            if (dx, dy) == (0, 0) {
                [0.2, 0.3, 0.4]
            } else {
                [1.0, 0.0, 0.0]
            }
        });
        assert_eq!(out, [0.2, 0.3, 0.4]);
    }

    #[test]
    fn denoise_smooths_flat_noise() {
        let adj = Adjustments {
            denoise: 100.0,
            ..Default::default()
        };
        let out = denoise_sample(&adj, |dx, dy| {
            if (dx, dy) == (0, 0) {
                [1.0, 1.0, 1.0]
            } else {
                [0.0, 0.0, 0.0]
            }
        });
        assert!(
            out[0] < 1.0 && out[0] > 0.0,
            "expected a blend, got {out:?}"
        );
    }

    #[test]
    fn denoise_preserves_hard_edge() {
        // A step edge at dx = 0. The result must stay nearer its own side
        // than a plain average would.
        let adj = Adjustments {
            denoise: 100.0,
            ..Default::default()
        };
        let step = |dx: i32, _dy: i32| -> [f32; 3] {
            let v = if dx < 0 { 0.0 } else { 1.0 };
            [v, v, v]
        };
        let out = denoise_sample(&adj, step);
        // 3 of 5 columns are 1.0.
        let plain_avg = 0.6;
        assert!(
            (out[0] - 1.0).abs() < (out[0] - plain_avg).abs(),
            "expected range weighting to favor the pixel's own side, got {out:?}"
        );
    }

    #[test]
    fn denoise_sample_with_strength_matches_denoise_sample_at_the_same_value() {
        let adj = Adjustments {
            denoise: 60.0,
            ..Default::default()
        };
        let sample = |dx: i32, dy: i32| -> [f32; 3] {
            let v = ((dx + dy) as f32) * 0.05 + 0.5;
            [v, v, v]
        };
        assert_eq!(
            denoise_sample(&adj, sample),
            denoise_sample_with_strength(60.0, sample)
        );
    }

    #[cfg(not(target_os = "macos"))]
    fn denoise_linear_rgb_buffer(
        strength: f32,
        width: usize,
        height: usize,
        buf: &[[f32; 3]],
    ) -> Vec<[f32; 3]> {
        (0..height)
            .flat_map(|y| {
                (0..width)
                    .map(move |x| denoise_linear_rgb_pixel(strength, width, height, buf, x, y))
            })
            .collect()
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn denoise_linear_rgb_buffer_smooths_an_isolated_outlier() {
        let (w, h) = (3, 3);
        let mut buf = vec![[0.0f32; 3]; w * h];
        buf[4] = [1.0, 1.0, 1.0];
        let out = denoise_linear_rgb_buffer(100.0, w, h, &buf);
        assert!(
            out[4][0] < 1.0 && out[4][0] > 0.0,
            "expected the outlier pulled toward its neighbors, got {:?}",
            out[4]
        );
        // Edge preservation keeps dark pixels from jumping toward the outlier.
        assert!(
            out[0][0] < 0.5,
            "expected a far corner to stay close to its own dark value, got {:?}",
            out[0]
        );
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn denoise_linear_rgb_buffer_zero_strength_is_identity() {
        let buf = vec![[0.2, 0.3, 0.4], [1.0, 0.0, 0.0]];
        let out = denoise_linear_rgb_buffer(0.0, 2, 1, &buf);
        assert_eq!(out, buf);
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn denoise_linear_rgb_buffer_clamps_at_edges_without_panicking() {
        let buf = vec![
            [0.1, 0.1, 0.1],
            [0.9, 0.9, 0.9],
            [0.5, 0.5, 0.5],
            [0.3, 0.3, 0.3],
        ];
        let out = denoise_linear_rgb_buffer(50.0, 2, 2, &buf);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn edit_signature_changes_with_denoise() {
        let a = Adjustments::default();
        let b = Adjustments {
            denoise: 40.0,
            ..Default::default()
        };
        assert_ne!(edit_signature(&a, 0), edit_signature(&b, 0));
    }

    #[test]
    fn is_identity_false_when_denoise_set() {
        let a = Adjustments {
            denoise: 1.0,
            ..Default::default()
        };
        assert!(!a.is_identity());
    }

    #[test]
    fn chromatic_aberration_is_a_per_photo_edit() {
        let fixed = Adjustments {
            chromatic_aberration: Some(CaScale {
                red: 0.0012,
                blue: -0.0008,
            }),
            ..Default::default()
        };
        assert!(!fixed.is_identity());
        assert_ne!(
            edit_signature(&fixed, 0),
            edit_signature(&Adjustments::default(), 0)
        );
        assert_eq!(
            fixed.tone_only().chromatic_aberration,
            None,
            "the scales were measured from this photo's lens"
        );
        let gpu = GpuAdjust::from(&fixed);
        assert_eq!((gpu.ca_red, gpu.ca_blue), (0.0012, -0.0008));
    }

    #[test]
    fn chromatic_aberration_round_trips_and_old_sidecars_still_load() {
        let fixed = Adjustments {
            chromatic_aberration: Some(CaScale {
                red: 0.0012,
                blue: 0.0,
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&fixed).unwrap();
        assert_eq!(serde_json::from_str::<Adjustments>(&json).unwrap(), fixed);
        assert_eq!(
            serde_json::to_string(&Adjustments::default()).unwrap(),
            "{}"
        );
        let old: Adjustments = serde_json::from_str(r#"{"exposure":0.5}"#).unwrap();
        assert_eq!(old.chromatic_aberration, None);
    }

    #[test]
    fn tone_only_drops_crop_keeps_sliders() {
        let a = Adjustments {
            exposure: 1.5,
            contrast: 20.0,
            crop: Some(crop(0.1, 0.1, 0.9, 0.9)),
            ..Default::default()
        };
        let t = a.tone_only();
        assert_eq!(t.crop, None);
        assert_eq!(t.exposure, 1.5);
        assert_eq!(t.contrast, 20.0);
    }

    #[test]
    fn vibrance_saturation_zero_is_identity() {
        let adj = Adjustments::default();
        let px = [0.6, 0.3, 0.2];
        let out = apply_linear(&adj, px);
        for i in 0..3 {
            assert!(
                (out[i] - px[i]).abs() < 1e-5,
                "channel {i}: {} vs {}",
                out[i],
                px[i]
            );
        }
    }

    #[test]
    fn saturation_pushes_channels_from_luma() {
        let base = Adjustments::default();
        let saturated = Adjustments {
            saturation: 80.0,
            ..Default::default()
        };
        let px = [0.6, 0.4, 0.4];
        let out_base = apply_linear(&base, px);
        let out_sat = apply_linear(&saturated, px);
        let spread = |o: [f32; 3]| (o[0] - o[1]).abs() + (o[1] - o[2]).abs() + (o[0] - o[2]).abs();
        assert!(
            spread(out_sat) > spread(out_base),
            "expected more saturation to widen channel spread: base={out_base:?} sat={out_sat:?}"
        );
    }

    #[test]
    fn edit_signature_differs_on_vibrance_and_saturation() {
        let a = Adjustments::default();
        let v = Adjustments {
            vibrance: 30.0,
            ..Default::default()
        };
        let s = Adjustments {
            saturation: 30.0,
            ..Default::default()
        };
        assert_ne!(edit_signature(&a, 0), edit_signature(&v, 0));
        assert_ne!(edit_signature(&a, 0), edit_signature(&s, 0));
        assert_ne!(edit_signature(&v, 0), edit_signature(&s, 0));
    }

    #[test]
    fn edit_signature_hashes_the_color_mixer_once_it_moves() {
        let mut a = Adjustments::default();
        let plain = edit_signature(&a, 0);
        a.mixer.green.hue = 20.0;
        let hue = edit_signature(&a, 0);
        a.mixer.green = color::BandShift {
            luminance: 20.0,
            ..Default::default()
        };
        assert_ne!(plain, hue);
        assert_ne!(hue, edit_signature(&a, 0));
        a.mixer = ColorMixer::default();
        assert_eq!(plain, edit_signature(&a, 0));
    }

    /// Catches a B&W mix that acts on a colour photo, changes the plain
    /// B&W look at 0, or lightens the wrong band.
    #[test]
    fn the_gray_mix_sets_how_light_each_band_turns_out_in_black_and_white() {
        let red = [0.6, 0.06, 0.04];
        let blue = [0.04, 0.08, 0.6];
        let mono = Adjustments {
            saturation: -100.0,
            ..Default::default()
        };
        let lit = Adjustments {
            gray_mix: GrayMix {
                red: 100.0,
                ..Default::default()
            },
            ..mono
        };
        let plain = apply_linear(&mono, red);
        let mixed = apply_linear(&lit, red);
        assert!(mixed[0] > plain[0] + 0.05, "{plain:?} -> {mixed:?}");
        assert!((mixed[0] - mixed[2]).abs() < 1e-5, "still gray: {mixed:?}");
        let b = apply_linear(&lit, blue);
        let b0 = apply_linear(&mono, blue);
        assert!((b[0] - b0[0]).abs() < 1e-3, "blue moved: {b0:?} -> {b:?}");
        let colour = Adjustments {
            saturation: 0.0,
            ..lit
        };
        assert_eq!(
            apply_linear(&colour, red),
            apply_linear(&Adjustments::default(), red)
        );
        assert_eq!(
            ColorOps::new(&ColorMixer::default(), &GrayMix::default(), true),
            ColorOps::default(),
            "a plain B&W photo skips the colour step"
        );
    }

    /// Catches a grading hue that sticks at 360 instead of going round.
    #[test]
    fn a_grading_hue_steps_round_the_circle_and_other_sliders_clamp() {
        let at = |id: SliderId| SLIDERS.iter().find(|s| s.id == id).unwrap();
        let hue = at(SliderId::Grade(Zone::Midtones, ColorAxis::Hue));
        assert_eq!(hue.stepped(358.0, 3.0), 1.0);
        assert_eq!(hue.stepped(2.0, -5.0), 357.0);
        let sat = at(SliderId::Grade(Zone::Midtones, ColorAxis::Saturation));
        assert_eq!(sat.stepped(99.0, 5.0), 100.0);
        assert_eq!(at(SliderId::Temp).stepped(-99.0, -5.0), -100.0);
    }

    #[test]
    fn edit_signature_hashes_color_grading_once_it_tints() {
        let mut a = Adjustments::default();
        let plain = edit_signature(&a, 0);
        a.grade.balance = 50.0;
        a.grade.midtones.hue = 90.0;
        assert_eq!(plain, edit_signature(&a, 0), "nothing tints yet");
        a.grade.global.luminance = 10.0;
        let lit = edit_signature(&a, 0);
        a.grade.midtones.hue = 200.0;
        assert_eq!(
            lit,
            edit_signature(&a, 0),
            "a hue at saturation 0 renders the same"
        );
        a.grade.midtones.saturation = 30.0;
        let tinted = edit_signature(&a, 0);
        a.grade.balance = -50.0;
        assert_ne!(plain, tinted);
        assert_ne!(tinted, edit_signature(&a, 0));
    }

    #[test]
    fn is_identity_false_when_vibrance_or_saturation_set() {
        assert!(!Adjustments {
            vibrance: 1.0,
            ..Default::default()
        }
        .is_identity());
        assert!(!Adjustments {
            saturation: 1.0,
            ..Default::default()
        }
        .is_identity());
    }

    #[test]
    fn neutralize_gray_recovers_neutral() {
        let (t, ti) = neutralize_gray([0.5, 0.5, 0.5]).expect("should solve");
        assert!(t.abs() < 1e-3, "expected ~0 temp, got {t}");
        assert!(ti.abs() < 1e-3, "expected ~0 tint, got {ti}");
    }

    #[test]
    fn neutralize_gray_recovers_warm_cast() {
        // Apply the solved gains the way `apply_linear` does and check for gray.
        let px = [0.6, 0.5, 0.4];
        let (t, ti) = neutralize_gray(px).expect("should solve");
        let tt = t / 100.0;
        let tit = ti / 100.0;
        let r = px[0] * (1.0 + tt * 0.3);
        let g = px[1] * (1.0 - tit * 0.15);
        let b = px[2] * (1.0 - tt * 0.3);
        assert!((r - g).abs() < 1e-4, "r={r} g={g}");
        assert!((g - b).abs() < 1e-4, "g={g} b={b}");
    }

    #[test]
    fn neutralize_gray_none_when_too_dark() {
        assert_eq!(neutralize_gray([0.0, 0.0, 0.0]), None);
    }

    #[test]
    fn edit_signature_differs_on_curve_only_when_bent() {
        use crate::develop::curve::Curve;
        let mut adj = Adjustments::default();
        let base = edit_signature(&adj, 0);
        adj.curve.rgb = Curve::STRONG_CONTRAST;
        let strong = edit_signature(&adj, 0);
        assert_ne!(strong, base);
        adj.curve.rgb = Curve::LINEAR;
        adj.curve.blue = Curve::STRONG_CONTRAST;
        assert_ne!(edit_signature(&adj, 0), strong, "the channel counts");
        adj.curve.blue = Curve::LINEAR;
        assert_eq!(edit_signature(&adj, 0), base);
    }

    #[test]
    fn a_contrast_curve_spreads_tones_in_both_pipelines() {
        use crate::develop::curve::Curve;
        let mut adj = Adjustments::default();
        adj.curve.rgb = Curve::STRONG_CONTRAST;
        for pipeline in [apply_linear, apply_raw_display] {
            let plain = |v: f32| pipeline(&Adjustments::default(), [v; 3])[0];
            let curved = |v: f32| pipeline(&adj, [v; 3])[0];
            assert!(curved(0.02) < plain(0.02));
            assert!(curved(0.7) > plain(0.7));
        }
    }

    #[test]
    fn a_red_curve_moves_only_red() {
        use crate::develop::curve::Curve;
        let mut adj = Adjustments::default();
        adj.curve.red = Curve::STRONG_CONTRAST;
        let px = [0.6, 0.6, 0.6];
        let plain = apply_linear(&Adjustments::default(), px);
        let out = Develop::new(&adj).linear_at(px, 0.0, 0.0);
        assert!(out[0] > plain[0]);
        assert!((out[1] - plain[1]).abs() < 1e-5);
        assert!((out[2] - plain[2]).abs() < 1e-5);
    }

    #[test]
    fn the_curve_table_fills_its_uniform() {
        assert_eq!(std::mem::size_of::<GpuCurve>(), 4096);
        let mut adj = Adjustments::default();
        assert_eq!(GpuAdjust::from(&adj).curve_on, 0.0);
        adj.curve.green.insert(100, 140);
        assert_eq!(GpuAdjust::from(&adj).curve_on, 1.0);
    }
}
