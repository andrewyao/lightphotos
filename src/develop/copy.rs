// SPDX-License-Identifier: MIT OR Apache-2.0

//! Which parts of an edit Copy and Paste carry, by Lightroom's sections.
//! [`merge`] is the one place a copied edit or a preset lands on a photo,
//! so the two can't disagree about what a section holds.

use super::{Adjustments, Section, SLIDERS};

/// One part of an edit that Copy can carry or leave.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CopySection {
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
    /// Remove Chromatic Aberration. The scales are measured per photo, so
    /// pasting it measures each photo that lacks it.
    Optics,
    /// The crop and the straighten angle.
    Crop,
    /// The local adjustment masks, which live beside the edit, so
    /// [`merge`] leaves them to the caller. Off unless chosen: they are
    /// drawn for one photo's framing.
    Masks,
}

impl CopySection {
    pub const ALL: [CopySection; 11] = [
        CopySection::WhiteBalance,
        CopySection::Tone,
        CopySection::Presence,
        CopySection::Curve,
        CopySection::Color,
        CopySection::ColorGrading,
        CopySection::Detail,
        CopySection::Effects,
        CopySection::Optics,
        CopySection::Crop,
        CopySection::Masks,
    ];

    /// The section a Develop slider belongs to.
    fn of(section: Section) -> CopySection {
        match section {
            Section::WhiteBalance => CopySection::WhiteBalance,
            Section::Tone => CopySection::Tone,
            Section::Presence => CopySection::Presence,
            Section::Curve => CopySection::Curve,
            Section::ColorMixer | Section::GrayMix => CopySection::Color,
            Section::ColorGrading => CopySection::ColorGrading,
            Section::Detail => CopySection::Detail,
            Section::Optics => CopySection::Optics,
            Section::Vignette | Section::Grain => CopySection::Effects,
        }
    }

    const fn bit(self) -> u16 {
        1 << self as u16
    }

    /// The name the preference stores. Never translated.
    fn key(self) -> &'static str {
        match self {
            CopySection::WhiteBalance => "white_balance",
            CopySection::Tone => "tone",
            CopySection::Presence => "presence",
            CopySection::Curve => "curve",
            CopySection::Color => "color",
            CopySection::ColorGrading => "color_grading",
            CopySection::Detail => "detail",
            CopySection::Effects => "effects",
            CopySection::Optics => "optics",
            CopySection::Crop => "crop",
            CopySection::Masks => "masks",
        }
    }
}

/// A set of [`CopySection`]s.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct CopySections(u16);

impl CopySections {
    pub const NONE: CopySections = CopySections(0);

    /// A look: everything but the crop and the measured optics, which belong
    /// to one photo's framing and lens. What a preset applies, and the copy
    /// choice before the user makes one.
    pub const LOOK: CopySections = CopySections(
        CopySection::WhiteBalance.bit()
            | CopySection::Tone.bit()
            | CopySection::Presence.bit()
            | CopySection::Curve.bit()
            | CopySection::Color.bit()
            | CopySection::ColorGrading.bit()
            | CopySection::Detail.bit()
            | CopySection::Effects.bit(),
    );

    pub fn all() -> CopySections {
        CopySection::ALL
            .into_iter()
            .fold(Self::NONE, |s, c| s.with(c, true))
    }

    pub fn has(self, section: CopySection) -> bool {
        self.0 & section.bit() != 0
    }

    pub fn with(self, section: CopySection, on: bool) -> CopySections {
        if on {
            CopySections(self.0 | section.bit())
        } else {
            CopySections(self.0 & !section.bit())
        }
    }

    /// The stored preference: a format mark, then every section's key,
    /// comma-separated, with `-` before the ones not chosen, so a section
    /// added later can tell "not chosen" from "not known when this was
    /// saved".
    pub fn to_pref(self) -> String {
        std::iter::once(PREF_FORMAT.to_string())
            .chain(CopySection::ALL.into_iter().map(|c| {
                let key = c.key();
                if self.has(c) {
                    key.to_string()
                } else {
                    format!("-{key}")
                }
            }))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Reads [`CopySections::to_pref`]. An unknown key is skipped, so a
    /// newer app's choice still loads. A section the text doesn't name takes
    /// its [`CopySections::LOOK`] default, so one added since the choice was
    /// saved still copies. The first format, with no mark, listed only the
    /// chosen keys and predates the colour sections, so there an unnamed key
    /// is off, except those. Text naming no section at all is
    /// [`Self::NONE`].
    pub fn from_pref(text: &str) -> CopySections {
        let items: Vec<&str> = text.split(',').map(str::trim).collect();
        let named = |c: CopySection| {
            items
                .iter()
                .find(|item| item.strip_prefix('-').unwrap_or(item) == c.key())
                .map(|item| !item.starts_with('-'))
        };
        if CopySection::ALL.into_iter().all(|c| named(c).is_none()) {
            return Self::NONE;
        }
        let first_format = !items.contains(&PREF_FORMAT);
        CopySection::ALL.into_iter().fold(Self::NONE, |s, c| {
            let on = match named(c) {
                Some(on) => on,
                None if first_format
                    && !matches!(c, CopySection::Color | CopySection::ColorGrading) =>
                {
                    false
                }
                None => Self::LOOK.has(c),
            };
            s.with(c, on)
        })
    }
}

/// The mark that opens a [`CopySections::to_pref`] text.
const PREF_FORMAT: &str = "v2";

/// `target` with the `sections` of `source` laid over it. Optics carries
/// only whether the correction is on: a photo that has its own scales keeps
/// them, and one that lacks them keeps `None` for the caller to measure.
pub fn merge(target: Adjustments, source: &Adjustments, sections: CopySections) -> Adjustments {
    let mut out = target;
    let mut from = *source;
    for slider in &SLIDERS {
        if sections.has(CopySection::of(slider.section)) {
            *(slider.field)(&mut out) = *(slider.field)(&mut from);
        }
    }
    if sections.has(CopySection::Curve) {
        out.curve = source.curve;
    }
    if sections.has(CopySection::Optics) && source.chromatic_aberration.is_none() {
        out.chromatic_aberration = None;
    }
    if sections.has(CopySection::Crop) {
        out.crop = source.crop;
        out.straighten = source.straighten;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::develop::{CaScale, Crop, Grain, Vignette};

    fn source() -> Adjustments {
        let mut a = Adjustments {
            temp: 20.0,
            exposure: 1.0,
            clarity: 30.0,
            mixer: crate::develop::ColorMixer {
                blue: crate::develop::BandShift {
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
            exposure: -0.5,
            saturation: 10.0,
            straighten: -1.0,
            ..Default::default()
        }
    }

    #[test]
    fn nothing_chosen_changes_nothing_and_everything_copies_all_but_the_scales() {
        assert_eq!(merge(target(), &source(), CopySections::NONE), target());
        let all = merge(target(), &source(), CopySections::all());
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
    fn each_section_carries_only_its_own_fields() {
        let only = |c| merge(target(), &source(), CopySections::NONE.with(c, true));
        let wb = only(CopySection::WhiteBalance);
        assert_eq!((wb.temp, wb.exposure), (20.0, -0.5));
        let tone = only(CopySection::Tone);
        assert_eq!((tone.temp, tone.exposure), (-5.0, 1.0));
        let presence = only(CopySection::Presence);
        assert_eq!((presence.clarity, presence.saturation), (30.0, 0.0));
        assert_eq!(only(CopySection::Detail).denoise, 40.0);
        assert_eq!(only(CopySection::Curve).curve, source().curve);
        let color = only(CopySection::Color);
        assert_eq!((color.mixer, color.clarity), (source().mixer, 0.0));
        assert!(presence.mixer.is_neutral());
        let grading = only(CopySection::ColorGrading);
        assert_eq!(
            (grading.grade, grading.mixer),
            (source().grade, target().mixer)
        );
        let effects = only(CopySection::Effects);
        assert_eq!(
            (effects.vignette, effects.grain),
            (source().vignette, source().grain)
        );
        assert_eq!(effects.temp, -5.0);
        let crop = only(CopySection::Crop);
        assert_eq!((crop.crop, crop.straighten), (source().crop, 3.0));
        assert_eq!(crop.exposure, -0.5);
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
        let optics = CopySections::NONE.with(CopySection::Optics, true);
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
            merge(measured, &off, CopySections::LOOK).chromatic_aberration,
            Some(own),
            "a look leaves the optics alone"
        );
    }

    #[test]
    fn the_preference_round_trips_and_skips_unknown_keys() {
        for sections in [CopySections::NONE, CopySections::LOOK, CopySections::all()] {
            assert_eq!(CopySections::from_pref(&sections.to_pref()), sections);
        }
        assert_eq!(
            CopySections::from_pref("v2,tone,-crop,hdr"),
            CopySections::LOOK.with(CopySection::Crop, false),
            "unnamed sections take the look's default"
        );
        assert_eq!(
            CopySections::from_pref(
                "v2,white_balance,tone,presence,curve,color,detail,effects,optics,crop"
            ),
            CopySections::all().with(CopySection::Masks, false),
            "a choice of everything keeps a later section's default"
        );
        for junk in ["", "hdr,,x", "v2"] {
            assert_eq!(
                CopySections::from_pref(junk),
                CopySections::NONE,
                "{junk:?}"
            );
        }
    }

    /// Catches a choice saved before the Color Mixer existed dropping it
    /// from every copy.
    #[test]
    fn a_first_format_choice_keeps_its_sections_and_gains_color() {
        assert_eq!(
            CopySections::from_pref("tone, crop,hdr"),
            CopySections::NONE
                .with(CopySection::Tone, true)
                .with(CopySection::Crop, true)
                .with(CopySection::Color, true)
                .with(CopySection::ColorGrading, true)
        );
    }
}
