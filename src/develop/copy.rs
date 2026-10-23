// SPDX-License-Identifier: MIT OR Apache-2.0

//! Which parts of an edit Copy and Paste carry, by Lightroom's Copy Settings
//! items. [`merge`] is the one place a copied edit or a preset lands on a
//! photo, so the two can't disagree about what an item holds.

use super::{Adjustments, SliderId, SLIDERS};

/// One part of an edit that Copy can carry or leave. Its wire name is its
/// preference key.
#[derive(Copy, Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyItem {
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
    Curve,
    ColorMixer,
    GrayMix,
    ColorGrading,
    Sharpening,
    NoiseReduction,
    Vignette,
    Grain,
    /// Remove Chromatic Aberration. The scales are measured per photo, so
    /// pasting it measures each photo that lacks it.
    RemoveCa,
    Crop,
    Straighten,
    /// The local adjustment masks, which live beside the edit, so
    /// [`merge`] leaves them to the caller. Off unless chosen: they are
    /// drawn for one photo's framing.
    Masks,
    /// The spot heals, which also live beside the edit.
    TouchUp,
}

/// A heading in Copy Settings, over the items it holds.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum CopyGroup {
    WhiteBalance,
    Tone,
    Presence,
    Curve,
    /// The Color Mixer and the B&W mix.
    Color,
    ColorGrading,
    Detail,
    /// Vignette and Grain.
    Effects,
    Optics,
    /// The crop and the straighten angle.
    Crop,
    Masks,
    TouchUp,
}

/// Whether a group's items are all chosen, none, or some.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum GroupState {
    On,
    Off,
    Mixed,
}

impl CopyGroup {
    pub const ALL: [CopyGroup; 12] = [
        CopyGroup::WhiteBalance,
        CopyGroup::Tone,
        CopyGroup::Presence,
        CopyGroup::Curve,
        CopyGroup::Color,
        CopyGroup::ColorGrading,
        CopyGroup::Detail,
        CopyGroup::Effects,
        CopyGroup::Optics,
        CopyGroup::Crop,
        CopyGroup::Masks,
        CopyGroup::TouchUp,
    ];

    /// The group's items, in the order Copy Settings lists them.
    pub fn items(self) -> &'static [CopyItem] {
        use CopyItem as I;
        match self {
            CopyGroup::WhiteBalance => &[I::Temp, I::Tint],
            CopyGroup::Tone => &[
                I::Exposure,
                I::Contrast,
                I::Highlights,
                I::Shadows,
                I::Whites,
                I::Blacks,
            ],
            CopyGroup::Presence => &[
                I::Texture,
                I::Clarity,
                I::Dehaze,
                I::Vibrance,
                I::Saturation,
            ],
            CopyGroup::Curve => &[I::Curve],
            CopyGroup::Color => &[I::ColorMixer, I::GrayMix],
            CopyGroup::ColorGrading => &[I::ColorGrading],
            CopyGroup::Detail => &[I::Sharpening, I::NoiseReduction],
            CopyGroup::Effects => &[I::Vignette, I::Grain],
            CopyGroup::Optics => &[I::RemoveCa],
            CopyGroup::Crop => &[I::Crop, I::Straighten],
            CopyGroup::Masks => &[I::Masks],
            CopyGroup::TouchUp => &[I::TouchUp],
        }
    }

    /// The name the first two preference formats stored, when a group was
    /// all Copy could choose. Never translated.
    fn key(self) -> &'static str {
        match self {
            CopyGroup::WhiteBalance => "white_balance",
            CopyGroup::Tone => "tone",
            CopyGroup::Presence => "presence",
            CopyGroup::Curve => "curve",
            CopyGroup::Color => "color",
            CopyGroup::ColorGrading => "color_grading",
            CopyGroup::Detail => "detail",
            CopyGroup::Effects => "effects",
            CopyGroup::Optics => "optics",
            CopyGroup::Crop => "crop",
            CopyGroup::Masks => "masks",
            CopyGroup::TouchUp => "touch_up",
        }
    }
}

impl CopyItem {
    /// Every item, group by group.
    pub fn all() -> impl Iterator<Item = CopyItem> {
        CopyGroup::ALL
            .into_iter()
            .flat_map(|g| g.items().iter().copied())
    }

    /// The item a Develop slider belongs to.
    fn of(id: SliderId) -> CopyItem {
        use SliderId as S;
        match id {
            S::Temp => CopyItem::Temp,
            S::Tint => CopyItem::Tint,
            S::Exposure => CopyItem::Exposure,
            S::Contrast => CopyItem::Contrast,
            S::Highlights => CopyItem::Highlights,
            S::Shadows => CopyItem::Shadows,
            S::Whites => CopyItem::Whites,
            S::Blacks => CopyItem::Blacks,
            S::Texture => CopyItem::Texture,
            S::Clarity => CopyItem::Clarity,
            S::Dehaze => CopyItem::Dehaze,
            S::Vibrance => CopyItem::Vibrance,
            S::Saturation => CopyItem::Saturation,
            S::Mix(..) => CopyItem::ColorMixer,
            S::Gray(_) => CopyItem::GrayMix,
            S::Grade(..) | S::GradeBlending | S::GradeBalance => CopyItem::ColorGrading,
            S::SharpenAmount | S::SharpenRadius | S::SharpenDetail | S::SharpenMasking => {
                CopyItem::Sharpening
            }
            S::Denoise => CopyItem::NoiseReduction,
            S::VignetteAmount | S::VignetteMidpoint | S::VignetteRoundness | S::VignetteFeather => {
                CopyItem::Vignette
            }
            S::GrainAmount | S::GrainSize | S::GrainRoughness => CopyItem::Grain,
        }
    }

    const fn bit(self) -> u64 {
        1 << self as u64
    }

    /// The name the preference stores. Never translated.
    fn key(self) -> &'static str {
        match self {
            CopyItem::Temp => "temp",
            CopyItem::Tint => "tint",
            CopyItem::Exposure => "exposure",
            CopyItem::Contrast => "contrast",
            CopyItem::Highlights => "highlights",
            CopyItem::Shadows => "shadows",
            CopyItem::Whites => "whites",
            CopyItem::Blacks => "blacks",
            CopyItem::Texture => "texture",
            CopyItem::Clarity => "clarity",
            CopyItem::Dehaze => "dehaze",
            CopyItem::Vibrance => "vibrance",
            CopyItem::Saturation => "saturation",
            CopyItem::Curve => "curve",
            CopyItem::ColorMixer => "color_mixer",
            CopyItem::GrayMix => "gray_mix",
            CopyItem::ColorGrading => "color_grading",
            CopyItem::Sharpening => "sharpening",
            CopyItem::NoiseReduction => "noise_reduction",
            CopyItem::Vignette => "vignette",
            CopyItem::Grain => "grain",
            CopyItem::RemoveCa => "remove_ca",
            CopyItem::Crop => "crop",
            CopyItem::Straighten => "straighten",
            CopyItem::Masks => "masks",
            CopyItem::TouchUp => "touch_up",
        }
    }
}

/// A set of [`CopyItem`]s.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct CopyItems(u64);

impl CopyItems {
    pub const NONE: CopyItems = CopyItems(0);

    /// A look: every slider, the curve and the colour work, but not the
    /// measured optics, the framing or anything drawn on one photo. What a
    /// preset applies, and the copy choice before the user makes one.
    pub const LOOK: CopyItems = CopyItems(
        CopyItem::Temp.bit()
            | CopyItem::Tint.bit()
            | CopyItem::Exposure.bit()
            | CopyItem::Contrast.bit()
            | CopyItem::Highlights.bit()
            | CopyItem::Shadows.bit()
            | CopyItem::Whites.bit()
            | CopyItem::Blacks.bit()
            | CopyItem::Texture.bit()
            | CopyItem::Clarity.bit()
            | CopyItem::Dehaze.bit()
            | CopyItem::Vibrance.bit()
            | CopyItem::Saturation.bit()
            | CopyItem::Curve.bit()
            | CopyItem::ColorMixer.bit()
            | CopyItem::GrayMix.bit()
            | CopyItem::ColorGrading.bit()
            | CopyItem::Sharpening.bit()
            | CopyItem::NoiseReduction.bit()
            | CopyItem::Vignette.bit()
            | CopyItem::Grain.bit(),
    );

    /// The items chosen afresh for each copy: never remembered, and off
    /// whenever Copy Settings opens, so a crop or a heal laid on one photo's
    /// framing never rides along unasked.
    pub const PER_COPY: CopyItems =
        CopyItems(CopyItem::Crop.bit() | CopyItem::Straighten.bit() | CopyItem::TouchUp.bit());

    pub fn all() -> CopyItems {
        CopyItem::all().fold(Self::NONE, |s, c| s.with(c, true))
    }

    pub fn has(self, item: CopyItem) -> bool {
        self.0 & item.bit() != 0
    }

    pub fn with(self, item: CopyItem, on: bool) -> CopyItems {
        if on {
            CopyItems(self.0 | item.bit())
        } else {
            CopyItems(self.0 & !item.bit())
        }
    }

    /// Every item of `group` turned `on`.
    pub fn with_group(self, group: CopyGroup, on: bool) -> CopyItems {
        group.items().iter().fold(self, |s, &c| s.with(c, on))
    }

    pub fn group_state(self, group: CopyGroup) -> GroupState {
        let items = group.items();
        let on = items.iter().filter(|&&c| self.has(c)).count();
        if on == 0 {
            GroupState::Off
        } else if on == items.len() {
            GroupState::On
        } else {
            GroupState::Mixed
        }
    }

    /// These items less the [`Self::PER_COPY`] ones.
    pub fn without_per_copy(self) -> CopyItems {
        CopyItems(self.0 & !Self::PER_COPY.0)
    }

    /// The stored preference: a format mark, then every remembered item's
    /// key, comma-separated, with `-` before the ones not chosen, so an item
    /// added later can tell "not chosen" from "not known when this was
    /// saved". [`Self::PER_COPY`] items are left out.
    pub fn to_pref(self) -> String {
        std::iter::once(PREF_FORMAT.to_string())
            .chain(
                CopyItem::all()
                    .filter(|&c| !Self::PER_COPY.has(c))
                    .map(|c| {
                        let key = c.key();
                        if self.has(c) {
                            key.to_string()
                        } else {
                            format!("-{key}")
                        }
                    }),
            )
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Reads [`CopyItems::to_pref`], or a choice saved when Copy chose
    /// whole groups. An unknown key is skipped, so a newer app's choice still
    /// loads. An item the text doesn't name takes its [`CopyItems::LOOK`]
    /// default, so one added since the choice was saved still copies.
    ///
    /// The group formats name a group's key for all of its items. Of those,
    /// the first, with no mark, listed only the chosen keys and predates the
    /// colour groups, so there an unnamed group is off, except those. Text
    /// naming nothing known is [`Self::NONE`]. [`Self::PER_COPY`] items
    /// always read as off.
    pub fn from_pref(text: &str) -> CopyItems {
        let items: Vec<&str> = text.split(',').map(str::trim).collect();
        let named = |key: &str| {
            items
                .iter()
                .find(|item| item.strip_prefix('-').unwrap_or(item) == key)
                .map(|item| !item.starts_with('-'))
        };
        let chosen = if items.contains(&PREF_FORMAT) {
            if CopyItem::all().all(|c| named(c.key()).is_none()) {
                return Self::NONE;
            }
            CopyItem::all().fold(Self::NONE, |s, c| {
                s.with(c, named(c.key()).unwrap_or(Self::LOOK.has(c)))
            })
        } else {
            if CopyGroup::ALL.into_iter().all(|g| named(g.key()).is_none()) {
                return Self::NONE;
            }
            let first_format = !items.contains(&GROUP_FORMAT);
            CopyGroup::ALL.into_iter().fold(Self::NONE, |s, g| {
                let on = match named(g.key()) {
                    Some(on) => on,
                    None if first_format
                        && !matches!(g, CopyGroup::Color | CopyGroup::ColorGrading) =>
                    {
                        false
                    }
                    None => Self::LOOK.group_state(g) == GroupState::On,
                };
                s.with_group(g, on)
            })
        };
        chosen.without_per_copy()
    }
}

/// The mark that opens a [`CopyItems::to_pref`] text.
const PREF_FORMAT: &str = "v3";

/// The mark of the second format, which stored groups.
const GROUP_FORMAT: &str = "v2";

/// `target` with the `items` of `source` laid over it. Remove Chromatic
/// Aberration carries only whether the correction is on: a photo that has
/// its own scales keeps them, and one that lacks them keeps `None` for the
/// caller to measure.
pub fn merge(target: Adjustments, source: &Adjustments, items: CopyItems) -> Adjustments {
    let mut out = target;
    let mut from = *source;
    for slider in &SLIDERS {
        if items.has(CopyItem::of(slider.id)) {
            *(slider.field)(&mut out) = *(slider.field)(&mut from);
        }
    }
    if items.has(CopyItem::Curve) {
        out.curve = source.curve;
    }
    if items.has(CopyItem::RemoveCa) && source.chromatic_aberration.is_none() {
        out.chromatic_aberration = None;
    }
    if items.has(CopyItem::Crop) {
        out.crop = source.crop;
    }
    if items.has(CopyItem::Straighten) {
        out.straighten = source.straighten;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::develop::{CaScale, Crop, Grain, Vignette};

    #[test]
    fn an_items_wire_name_is_its_preference_key() {
        for item in CopyItem::all() {
            assert_eq!(serde_json::to_value(item).unwrap(), item.key());
        }
    }

    fn source() -> Adjustments {
        let mut a = Adjustments {
            temp: 20.0,
            tint: 8.0,
            exposure: 1.0,
            clarity: 30.0,
            mixer: crate::develop::ColorMixer {
                blue: crate::develop::color::BandShift {
                    saturation: -40.0,
                    ..Default::default()
                },
                ..Default::default()
            },
            grade: crate::develop::ColorGrade {
                balance: 30.0,
                ..Default::default()
            },
            denoise: 40.0,
            vignette: Vignette {
                amount: -30.0,
                ..Default::default()
            },
            grain: Grain {
                amount: 20.0,
                ..Default::default()
            },
            crop: Some(Crop {
                left: 0.1,
                top: 0.1,
                right: 0.9,
                bottom: 0.9,
            }),
            straighten: 3.0,
            chromatic_aberration: Some(CaScale {
                red: 0.001,
                blue: 0.0,
            }),
            ..Default::default()
        };
        a.curve.rgb = crate::develop::curve::Curve::STRONG_CONTRAST;
        a
    }

    fn target() -> Adjustments {
        Adjustments {
            temp: -5.0,
            tint: -3.0,
            exposure: -0.5,
            saturation: 10.0,
            straighten: -1.0,
            ..Default::default()
        }
    }

    #[test]
    fn nothing_chosen_changes_nothing_and_everything_copies_all_but_the_scales() {
        assert_eq!(merge(target(), &source(), CopyItems::NONE), target());
        let all = merge(target(), &source(), CopyItems::all());
        assert_eq!(
            all,
            Adjustments {
                chromatic_aberration: None,
                ..source()
            },
            "a photo without scales waits to be measured"
        );
    }

    #[test]
    fn each_item_carries_only_its_own_fields() {
        let only = |c| merge(target(), &source(), CopyItems::NONE.with(c, true));
        let temp = only(CopyItem::Temp);
        assert_eq!((temp.temp, temp.tint, temp.exposure), (20.0, -3.0, -0.5));
        let tint = only(CopyItem::Tint);
        assert_eq!((tint.temp, tint.tint), (-5.0, 8.0));
        let exposure = only(CopyItem::Exposure);
        assert_eq!((exposure.temp, exposure.exposure), (-5.0, 1.0));
        let clarity = only(CopyItem::Clarity);
        assert_eq!((clarity.clarity, clarity.saturation), (30.0, 10.0));
        assert_eq!(only(CopyItem::Saturation).saturation, 0.0);
        assert_eq!(only(CopyItem::NoiseReduction).denoise, 40.0);
        assert_eq!(only(CopyItem::Sharpening).denoise, 0.0);
        assert_eq!(only(CopyItem::Curve).curve, source().curve);
        let mixer = only(CopyItem::ColorMixer);
        assert_eq!((mixer.mixer, mixer.clarity), (source().mixer, 0.0));
        assert!(clarity.mixer.is_neutral());
        let grading = only(CopyItem::ColorGrading);
        assert_eq!(
            (grading.grade, grading.mixer),
            (source().grade, target().mixer)
        );
        let vignette = only(CopyItem::Vignette);
        assert_eq!(
            (vignette.vignette, vignette.grain),
            (source().vignette, target().grain)
        );
        assert_eq!(only(CopyItem::Grain).grain, source().grain);
        let crop = only(CopyItem::Crop);
        assert_eq!((crop.crop, crop.straighten), (source().crop, -1.0));
        let straighten = only(CopyItem::Straighten);
        assert_eq!((straighten.crop, straighten.straighten), (None, 3.0));
    }

    #[test]
    fn a_group_turns_all_its_items_and_reports_a_mix() {
        let tone = CopyItems::NONE.with_group(CopyGroup::Tone, true);
        assert_eq!(tone.group_state(CopyGroup::Tone), GroupState::On);
        assert_eq!(tone.group_state(CopyGroup::Presence), GroupState::Off);
        let mixed = CopyItems::LOOK.with(CopyItem::Tint, false);
        assert_eq!(
            mixed.group_state(CopyGroup::WhiteBalance),
            GroupState::Mixed
        );
        assert_eq!(
            mixed.with_group(CopyGroup::WhiteBalance, true),
            CopyItems::LOOK
        );
    }

    #[test]
    fn every_item_sits_in_one_group() {
        let mut seen = CopyItems::NONE;
        for item in CopyItem::all() {
            assert!(!seen.has(item), "{item:?} is listed twice");
            seen = seen.with(item, true);
        }
        assert_eq!(seen, CopyItems::all());
    }

    #[test]
    fn optics_keeps_a_photos_own_scales_and_turns_off_with_the_source() {
        let own = CaScale {
            red: 0.002,
            blue: -0.001,
        };
        let measured = Adjustments {
            chromatic_aberration: Some(own),
            ..target()
        };
        let optics = CopyItems::NONE.with(CopyItem::RemoveCa, true);
        assert_eq!(
            merge(measured, &source(), optics).chromatic_aberration,
            Some(own)
        );
        let off = Adjustments {
            chromatic_aberration: None,
            ..source()
        };
        assert_eq!(merge(measured, &off, optics).chromatic_aberration, None);
        assert_eq!(
            merge(measured, &off, CopyItems::LOOK).chromatic_aberration,
            Some(own),
            "a look leaves the optics alone"
        );
    }

    #[test]
    fn the_preference_round_trips_and_forgets_the_per_copy_items() {
        let no_tint = CopyItems::LOOK.with(CopyItem::Tint, false);
        let with_masks = CopyItems::LOOK.with(CopyItem::Masks, true);
        for items in [CopyItems::NONE, CopyItems::LOOK, no_tint, with_masks] {
            assert_eq!(CopyItems::from_pref(&items.to_pref()), items);
        }
        let pref = CopyItems::all().to_pref();
        for per_copy in ["crop", "straighten", "touch_up"] {
            assert!(
                !pref
                    .split(',')
                    .any(|k| k.trim_start_matches('-') == per_copy),
                "{per_copy} is saved in {pref:?}"
            );
        }
        assert_eq!(
            CopyItems::from_pref(&pref),
            CopyItems::all().without_per_copy()
        );
        assert_eq!(
            CopyItems::from_pref("v3,-tint,crop,hdr"),
            CopyItems::LOOK.with(CopyItem::Tint, false),
            "unnamed items take the look's default, and crop is never read"
        );
        for junk in ["", "hdr,,x", "v3", "v2"] {
            assert_eq!(CopyItems::from_pref(junk), CopyItems::NONE, "{junk:?}");
        }
    }

    /// Catches a choice saved when Copy chose whole groups losing them.
    #[test]
    fn a_group_choice_loads_as_its_items() {
        assert_eq!(
            CopyItems::from_pref("v2,-tone,crop,hdr"),
            CopyItems::LOOK.with_group(CopyGroup::Tone, false),
            "unnamed groups take the look's default, and crop is never read"
        );
        assert_eq!(
            CopyItems::from_pref(
                "v2,white_balance,tone,presence,curve,color,detail,effects,optics,crop"
            ),
            CopyItems::LOOK.with(CopyItem::RemoveCa, true),
        );
    }

    /// Catches a choice saved before the Color Mixer existed dropping it
    /// from every copy.
    #[test]
    fn a_first_format_choice_keeps_its_groups_and_gains_color() {
        assert_eq!(
            CopyItems::from_pref("tone, crop,hdr"),
            CopyItems::NONE
                .with_group(CopyGroup::Tone, true)
                .with_group(CopyGroup::Color, true)
                .with_group(CopyGroup::ColorGrading, true)
        );
    }
}
