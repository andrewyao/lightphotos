// SPDX-License-Identifier: MIT OR Apache-2.0

//! A Lightroom `.xmp` develop-preset reader. It fills the [`Adjustments`]
//! fields a preset maps onto and names the `crs:` fields it had to leave
//! behind, so an import can say what it does not carry.
//!
//! This is a scanner over a known key list, not an XML parser. Camera Raw
//! writes every field either as an attribute or as a child element of one
//! `rdf:Description`, and both spellings are read. A dozen keys do not earn an
//! XML crate, least of all one that has to build for wasm32.

use crate::develop::curve::{Channel, Curve};
use crate::develop::{Adjustments, ColorAxis, SliderId, SLIDERS};

/// What a preset file yielded.
pub struct Imported {
    pub name: Option<String>,
    pub adj: Adjustments,
    /// `crs:` fields Lightroom set that have no field here.
    pub dropped: Vec<&'static str>,
    pub monochrome: bool,
}

/// One `crs:` field as found in the file. Present-but-unreadable is its own
/// case because it is reported, while absent is silent.
enum Field {
    Absent,
    Unparseable,
    Value(f32),
}

/// What a family of unsupported fields looks like when Lightroom did not
/// touch it. Only a field away from its default is reported, since a note
/// that fires on every export would train the user to ignore it.
enum Untouched {
    Zero,
    /// A field Camera Raw writes with a nonzero default, so only a value away
    /// from that default means the user moved it.
    Default(f32),
}

/// The point-curve keys, one per [`Channel`].
const CURVE_KEYS: [(&str, Channel); 4] = [
    ("ToneCurvePV2012", Channel::Rgb),
    ("ToneCurvePV2012Red", Channel::Red),
    ("ToneCurvePV2012Green", Channel::Green),
    ("ToneCurvePV2012Blue", Channel::Blue),
];

/// The `crs:` keys that feed one slider.
fn keys(id: SliderId) -> &'static [&'static str] {
    match id {
        SliderId::Temp => &["IncrementalTemperature"],
        SliderId::Tint => &["IncrementalTint"],
        SliderId::Exposure => &["Exposure2012"],
        SliderId::Contrast => &["Contrast2012"],
        SliderId::Highlights => &["Highlights2012"],
        SliderId::Shadows => &["Shadows2012"],
        SliderId::Whites => &["Whites2012"],
        SliderId::Blacks => &["Blacks2012"],
        SliderId::Texture => &["Texture"],
        SliderId::Clarity => &["Clarity2012"],
        SliderId::Dehaze => &["Dehaze"],
        SliderId::Vibrance => &["Vibrance"],
        SliderId::Saturation => &["Saturation"],
        // `LuminanceSmoothing` alone. Camera Raw defaults `ColorNoiseReduction`
        // to 25, so reading it would put denoise on every import that carries
        // the Detail panel at all, which nobody asked for.
        SliderId::Denoise => &["LuminanceSmoothing"],
        SliderId::SharpenAmount => &["Sharpness"],
        SliderId::SharpenRadius => &["SharpenRadius"],
        SliderId::SharpenDetail => &["SharpenDetail"],
        SliderId::SharpenMasking => &["SharpenEdgeMasking"],
        SliderId::VignetteAmount => &["PostCropVignetteAmount"],
        SliderId::VignetteMidpoint => &["PostCropVignetteMidpoint"],
        SliderId::VignetteRoundness => &["PostCropVignetteRoundness"],
        SliderId::VignetteFeather => &["PostCropVignetteFeather"],
        SliderId::GrainAmount => &["GrainAmount"],
        SliderId::GrainSize => &["GrainSize"],
        SliderId::GrainRoughness => &["GrainFrequency"],
        SliderId::Mix(axis, band) => {
            let table = match axis {
                ColorAxis::Hue => &HUE_KEYS,
                ColorAxis::Saturation => &SATURATION_KEYS,
                ColorAxis::Luminance => &LUMINANCE_KEYS,
            };
            std::slice::from_ref(&table[band as usize])
        }
        SliderId::Gray(band) => std::slice::from_ref(&GRAY_KEYS[band as usize]),
        SliderId::Grade(zone, axis) => {
            std::slice::from_ref(&GRADE_KEYS[zone as usize][axis as usize])
        }
        SliderId::GradeBlending => &["ColorGradeBlending"],
        SliderId::GradeBalance => &["SplitToningBalance"],
    }
}

/// The Color Mixer keys, one per band in [`crate::develop::Band::ALL`] order.
const HUE_KEYS: [&str; 8] = [
    "HueAdjustmentRed",
    "HueAdjustmentOrange",
    "HueAdjustmentYellow",
    "HueAdjustmentGreen",
    "HueAdjustmentAqua",
    "HueAdjustmentBlue",
    "HueAdjustmentPurple",
    "HueAdjustmentMagenta",
];
const SATURATION_KEYS: [&str; 8] = [
    "SaturationAdjustmentRed",
    "SaturationAdjustmentOrange",
    "SaturationAdjustmentYellow",
    "SaturationAdjustmentGreen",
    "SaturationAdjustmentAqua",
    "SaturationAdjustmentBlue",
    "SaturationAdjustmentPurple",
    "SaturationAdjustmentMagenta",
];
/// The Color Grading keys per wheel in [`crate::develop::Zone::ALL`] order,
/// as hue, saturation, luminance. Camera Raw keeps the shadow and highlight
/// hue and saturation under their Split Toning names.
const GRADE_KEYS: [[&str; 3]; 4] = [
    [
        "SplitToningShadowHue",
        "SplitToningShadowSaturation",
        "ColorGradeShadowLum",
    ],
    [
        "ColorGradeMidtoneHue",
        "ColorGradeMidtoneSat",
        "ColorGradeMidtoneLum",
    ],
    [
        "SplitToningHighlightHue",
        "SplitToningHighlightSaturation",
        "ColorGradeHighlightLum",
    ],
    [
        "ColorGradeGlobalHue",
        "ColorGradeGlobalSat",
        "ColorGradeGlobalLum",
    ],
];
const GRAY_KEYS: [&str; 8] = [
    "GrayMixerRed",
    "GrayMixerOrange",
    "GrayMixerYellow",
    "GrayMixerGreen",
    "GrayMixerAqua",
    "GrayMixerBlue",
    "GrayMixerPurple",
    "GrayMixerMagenta",
];
const LUMINANCE_KEYS: [&str; 8] = [
    "LuminanceAdjustmentRed",
    "LuminanceAdjustmentOrange",
    "LuminanceAdjustmentYellow",
    "LuminanceAdjustmentGreen",
    "LuminanceAdjustmentAqua",
    "LuminanceAdjustmentBlue",
    "LuminanceAdjustmentPurple",
    "LuminanceAdjustmentMagenta",
];

/// Fields with no counterpart here, under the label the note names. The
/// labels are Adobe's identifiers because no `i18n` string wraps them.
///
/// Absolute `Temperature` and `Tint` head the list on purpose. They are
/// Kelvin against one camera's as-shot reference, while `temp` here is a
/// relative nudge, so any conversion would mis-white-balance every photo
/// while looking like it worked.
const UNSUPPORTED: &[(&str, &[&str], Untouched)] = &[
    ("Temperature", &["Temperature"], Untouched::Zero),
    ("Tint", &["Tint"], Untouched::Zero),
    (
        "ColorNoiseReduction",
        &["ColorNoiseReduction"],
        Untouched::Default(25.0),
    ),
    ("CropAngle", &["CropAngle"], Untouched::Zero),
];

/// Parse a Lightroom `.xmp` develop preset. `Err` for a file that is not one.
pub fn parse(xmp: &str) -> Result<Imported, String> {
    let body = description(xmp).ok_or("no rdf:Description")?;
    if !body.contains("crs:") {
        return Err("no crs: fields".into());
    }

    let mut adj = Adjustments::default();
    let mut dropped = Vec::new();
    for slider in &SLIDERS {
        let mut found: Option<f32> = None;
        for key in keys(slider.id) {
            match number(body, key) {
                Field::Value(v) => found = Some(found.map_or(v, |b| b.max(v))),
                Field::Unparseable => dropped.push(*key),
                Field::Absent => {}
            }
        }
        if let Some(v) = found {
            *(slider.field)(&mut adj) = v.clamp(*slider.range.start(), *slider.range.end());
        }
    }

    // Camera Raw writes its raw default sharpening into every preset that
    // saves the Detail panel, chosen or not. Only a change from it counts.
    if adj.sharpening
        == (crate::develop::Sharpening {
            amount: 40.0,
            ..Default::default()
        })
    {
        adj.sharpening = Default::default();
    }

    // A split-toning preset from before Color Grading has no Blending.
    // Camera Raw reads it as Blending 100, which tints across the whole
    // range as split toning did.
    let split_toning = GRADE_KEYS
        .iter()
        .flat_map(|w| w.iter())
        .chain(&["SplitToningBalance"])
        .any(|k| k.starts_with("SplitToning") && !matches!(number(body, k), Field::Absent));
    if split_toning && matches!(number(body, "ColorGradeBlending"), Field::Absent) {
        adj.grade.blending = 100.0;
    }

    // Our closest monochrome. Lightroom's B&W mode ignores its own
    // Saturation, so this overrides a mapped one.
    let monochrome =
        value(body, "ConvertToGrayscale").is_some_and(|v| v.eq_ignore_ascii_case("true"));
    if monochrome {
        adj.saturation = -100.0;
    }

    for (key, ch) in CURVE_KEYS {
        if let Some(seq) = raw_value(body, key) {
            match curve(seq) {
                Some(c) => *adj.curve.get_mut(ch) = c,
                None => dropped.push(key),
            }
        }
    }

    for (label, keys, default) in UNSUPPORTED {
        let set = keys.iter().any(|key| match default {
            Untouched::Zero => is_set(number(body, key)),
            Untouched::Default(d) => is_away_from(number(body, key), *d),
        });
        if set {
            dropped.push(label);
        }
    }

    let name = raw_value(body, "Name")
        .and_then(|alt| li_texts(alt).next())
        .map(|li| decode(li).trim().to_string())
        .filter(|n| !n.is_empty());

    Ok(Imported {
        name,
        adj,
        dropped,
        monochrome,
    })
}

/// The first `rdf:Description` block. A second block is cut off so it cannot
/// contribute values.
fn description(xmp: &str) -> Option<&str> {
    const TAG: &str = "<rdf:Description";
    let body = &xmp[xmp.find(TAG)?..];
    let end = body[TAG.len()..]
        .find(TAG)
        .map_or(body.len(), |i| i + TAG.len());
    Some(&body[..end])
}

fn number(body: &str, key: &str) -> Field {
    match value(body, key) {
        None => Field::Absent,
        Some(text) => match text.parse::<f32>() {
            Ok(v) if v.is_finite() => Field::Value(v),
            _ => Field::Unparseable,
        },
    }
}

/// Whether a field is worth a note. An unreadable value is, since it cannot
/// be shown harmless.
fn is_set(field: Field) -> bool {
    is_away_from(field, 0.0)
}

/// Whether Lightroom moved this field off `default`. Unparseable counts as
/// moved, so a value we could not read is still reported rather than silently
/// treated as the default.
fn is_away_from(field: Field, default: f32) -> bool {
    match field {
        Field::Absent => false,
        Field::Unparseable => true,
        Field::Value(v) => v != default,
    }
}

/// A point curve from its `rdf:Seq` of `x, y` items. Lightroom writes a
/// diagonal into every sidecar, and that reads as [`Curve::LINEAR`]. `None`
/// when a point does not read as two numbers.
fn curve(seq: &str) -> Option<Curve> {
    let level = |n: &str| Some(n.trim().parse::<f32>().ok()?.round().clamp(0.0, 255.0) as u8);
    let points = li_texts(seq)
        .map(|point| {
            let (x, y) = point.split_once(',')?;
            Some([level(x)?, level(y)?])
        })
        .collect::<Option<Vec<_>>>()?;
    if points.iter().all(|[x, y]| x == y) {
        return Some(Curve::LINEAR);
    }
    Some(Curve::from(points))
}

/// The raw text of each `<rdf:li>` in an `rdf:Seq` or `rdf:Alt`.
fn li_texts(block: &str) -> impl Iterator<Item = &str> {
    block.match_indices("<rdf:li").filter_map(move |(i, _)| {
        let rest = &block[i..];
        let open = rest.find('>')?;
        if rest[..open].ends_with('/') {
            return Some("");
        }
        let text = &rest[open + 1..];
        Some(&text[..text.find("</rdf:li")?])
    })
}

/// A field's decoded, trimmed value.
fn value(body: &str, key: &str) -> Option<String> {
    raw_value(body, key).map(|raw| decode(raw).trim().to_string())
}

/// A field's text as written, attribute spelling first. The inner text of an
/// element can itself hold XML (the tone curve and `crs:Name` do), which the
/// caller takes apart before decoding entities.
fn raw_value<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    attribute(body, key).or_else(|| element(body, key))
}

/// `crs:Key="value"`. Anchored on both sides. Without the trailing anchor
/// `crs:Exposure2012` matches inside `crs:Exposure2012Reference`, and without
/// the leading one a differently-prefixed attribute matches.
fn attribute<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("crs:{key}");
    let bytes = body.as_bytes();
    body.match_indices(&needle).find_map(|(i, _)| {
        let led = i > 0 && (bytes[i - 1].is_ascii_whitespace() || bytes[i - 1] == b'<');
        if !led {
            return None;
        }
        let rest = body[i + needle.len()..].trim_start_matches(|c: char| c.is_ascii_whitespace());
        let rest = rest
            .strip_prefix('=')?
            .trim_start_matches(|c: char| c.is_ascii_whitespace());
        let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''))?;
        let inner = &rest[1..];
        Some(&inner[..inner.find(quote)?])
    })
}

/// `<crs:Key>value</crs:Key>`. A self-closing `<crs:Key/>` is an empty value.
fn element<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let open = format!("<crs:{key}");
    let close = format!("</crs:{key}");
    body.match_indices(&open).find_map(|(i, _)| {
        let rest = &body[i + open.len()..];
        let next = *rest.as_bytes().first()?;
        if !(next == b'>' || next == b'/' || next.is_ascii_whitespace()) {
            return None;
        }
        let tag_end = rest.find('>')?;
        if rest[..tag_end].ends_with('/') {
            return Some("");
        }
        let inner = &rest[tag_end + 1..];
        Some(&inner[..inner.find(&close)?])
    })
}

/// The five XML entities, in one left-to-right pass so `&amp;lt;` yields
/// `&lt;` and not `<`.
fn decode(text: &str) -> String {
    const ENTITIES: [(&str, char); 5] = [
        ("&lt;", '<'),
        ("&gt;", '>'),
        ("&amp;", '&'),
        ("&quot;", '"'),
        ("&apos;", '\''),
    ];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        match ENTITIES.iter().find(|(e, _)| rest.starts_with(e)) {
            Some((e, c)) => {
                out.push(*c);
                rest = &rest[e.len()..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::develop::{EXPOSURE_RANGE, TONE_RANGE};

    /// A packet with one `rdf:Description` carrying `attrs` on the tag and
    /// `children` inside it.
    fn packet(attrs: &str, children: &str) -> String {
        format!(
            "<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\
             <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
             <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
             <rdf:Description rdf:about=\"\" \
             xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\" {attrs}>\
             {children}</rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end=\"w\"?>"
        )
    }

    const ATTRIBUTE_PRESET: &str = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
        <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
        <rdf:Description rdf:about=\"\"\n\
        \x20 xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\"\n\
        \x20 crs:Version=\"15.0\"\n\
        \x20 crs:IncrementalTemperature=\"+12\"\n\
        \x20 crs:IncrementalTint=\"-4\"\n\
        \x20 crs:Exposure2012=\"+0.35\"\n\
        \x20 crs:Contrast2012=\"+15\"\n\
        \x20 crs:Highlights2012=\"-40\"\n\
        \x20 crs:Shadows2012=\"+25\"\n\
        \x20 crs:Whites2012=\"+10\"\n\
        \x20 crs:Blacks2012=\"-8\"\n\
        \x20 crs:Vibrance=\"+20\"\n\
        \x20 crs:Saturation=\"-5\"\n\
        \x20 crs:LuminanceSmoothing=\"30\"\n\
        \x20 crs:HasSettings=\"True\">\n\
        <crs:Name><rdf:Alt><rdf:li xml:lang=\"x-default\">Warm Film</rdf:li></rdf:Alt></crs:Name>\
        </rdf:Description></rdf:RDF></x:xmpmeta>";

    const ELEMENT_PRESET: &str = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
        <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
        <rdf:Description rdf:about=\"\" \
        xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\">\n\
        <crs:Version>15.0</crs:Version>\n\
        <crs:IncrementalTemperature>+12</crs:IncrementalTemperature>\n\
        <crs:IncrementalTint>-4</crs:IncrementalTint>\n\
        <crs:Exposure2012>+0.35</crs:Exposure2012>\n\
        <crs:Contrast2012>+15</crs:Contrast2012>\n\
        <crs:Highlights2012>-40</crs:Highlights2012>\n\
        <crs:Shadows2012>+25</crs:Shadows2012>\n\
        <crs:Whites2012>+10</crs:Whites2012>\n\
        <crs:Blacks2012>-8</crs:Blacks2012>\n\
        <crs:Vibrance>+20</crs:Vibrance>\n\
        <crs:Saturation>-5</crs:Saturation>\n\
        <crs:LuminanceSmoothing>30</crs:LuminanceSmoothing>\n\
        <crs:HasSettings>True</crs:HasSettings>\n\
        <crs:Name><rdf:Alt><rdf:li xml:lang=\"x-default\">Warm Film</rdf:li></rdf:Alt></crs:Name>\
        </rdf:Description></rdf:RDF></x:xmpmeta>";

    fn expected_preset() -> Adjustments {
        Adjustments {
            temp: 12.0,
            tint: -4.0,
            exposure: 0.35,
            contrast: 15.0,
            highlights: -40.0,
            shadows: 25.0,
            whites: 10.0,
            blacks: -8.0,
            vibrance: 20.0,
            saturation: -5.0,
            mixer: Default::default(),
            gray_mix: Default::default(),
            grade: Default::default(),
            denoise: 30.0,
            sharpening: Default::default(),
            crop: None,
            texture: 0.0,
            clarity: 0.0,
            dehaze: 0.0,
            straighten: 0.0,
            chromatic_aberration: None,
            curve: Default::default(),
            vignette: Default::default(),
            grain: Default::default(),
        }
    }

    /// Camera Raw's raw default sharpening, saved into every Detail panel,
    /// is not taken as a choice; anything else is.
    #[test]
    fn default_sharpening_is_not_imported_and_a_change_is() {
        let at = |amount: &str| {
            parse(&packet(
                &format!(
                    "crs:Sharpness=\"{amount}\" crs:SharpenRadius=\"+1.0\" \
                     crs:SharpenDetail=\"25\" crs:SharpenEdgeMasking=\"0\""
                ),
                "",
            ))
            .unwrap()
            .adj
            .sharpening
        };
        assert_eq!(at("40"), crate::develop::Sharpening::default());
        assert_eq!(at("60").amount, 60.0);
    }

    /// Catches a Color Mixer key read into the wrong band or axis, or still
    /// reported as dropped.
    #[test]
    fn color_mixer_keys_fill_their_band_and_axis() {
        let got = parse(&packet(
            "crs:HueAdjustmentRed=\"+10\" crs:SaturationAdjustmentAqua=\"-20\" \
             crs:LuminanceAdjustmentMagenta=\"+30\" crs:SaturationAdjustmentBlue=\"-250\"",
            "",
        ))
        .unwrap();
        assert!(got.dropped.is_empty(), "{:?}", got.dropped);
        let m = got.adj.mixer;
        assert_eq!(m.red.hue, 10.0);
        assert_eq!(m.aqua.saturation, -20.0);
        assert_eq!(m.magenta.luminance, 30.0);
        assert_eq!(m.blue.saturation, -100.0, "out of range clamps");
        assert_eq!(
            crate::develop::ColorMixer {
                red: Default::default(),
                aqua: Default::default(),
                magenta: Default::default(),
                blue: Default::default(),
                ..m
            },
            Default::default(),
            "no other band moved"
        );
    }

    /// Catches a Color Grading key read into the wrong wheel or slider,
    /// including the shadow and highlight tints Camera Raw keeps under
    /// their Split Toning names.
    #[test]
    fn color_grade_and_split_toning_keys_fill_the_wheels() {
        let got = parse(&packet(
            "crs:SplitToningShadowHue=\"220\" crs:SplitToningShadowSaturation=\"25\" \
             crs:SplitToningHighlightHue=\"45\" crs:SplitToningHighlightSaturation=\"30\" \
             crs:SplitToningBalance=\"-20\" crs:ColorGradeMidtoneHue=\"100\" \
             crs:ColorGradeMidtoneSat=\"10\" crs:ColorGradeShadowLum=\"-5\" \
             crs:ColorGradeHighlightLum=\"8\" crs:ColorGradeMidtoneLum=\"3\" \
             crs:ColorGradeGlobalHue=\"300\" crs:ColorGradeGlobalSat=\"12\" \
             crs:ColorGradeGlobalLum=\"-4\" crs:ColorGradeBlending=\"70\"",
            "",
        ))
        .unwrap();
        assert!(got.dropped.is_empty(), "{:?}", got.dropped);
        let w = |hue, saturation, luminance| crate::develop::Wheel {
            hue,
            saturation,
            luminance,
        };
        assert_eq!(
            got.adj.grade,
            crate::develop::ColorGrade {
                shadows: w(220.0, 25.0, -5.0),
                midtones: w(100.0, 10.0, 3.0),
                highlights: w(45.0, 30.0, 8.0),
                global: w(300.0, 12.0, -4.0),
                blending: 70.0,
                balance: -20.0,
            }
        );
    }

    /// Catches an old split-toning preset losing its full-range tint.
    #[test]
    fn a_split_toning_preset_without_blending_blends_fully() {
        let old = parse(&packet(
            "crs:SplitToningShadowHue=\"220\" crs:SplitToningShadowSaturation=\"25\"",
            "",
        ))
        .unwrap();
        assert_eq!(old.adj.grade.blending, 100.0);
        let new = parse(&packet(
            "crs:SplitToningShadowHue=\"220\" crs:ColorGradeBlending=\"40\"",
            "",
        ))
        .unwrap();
        assert_eq!(new.adj.grade.blending, 40.0);
        let none = parse(&packet("crs:Exposure2012=\"+1\"", "")).unwrap();
        assert_eq!(none.adj.grade.blending, 50.0);
    }

    /// Catches a B&W mix key read into the wrong band.
    #[test]
    fn gray_mixer_keys_fill_the_bw_mix() {
        let got = parse(&packet(
            "crs:ConvertToGrayscale=\"True\" crs:GrayMixerRed=\"+25\" \
             crs:GrayMixerBlue=\"-40\" crs:GrayMixerMagenta=\"7\"",
            "",
        ))
        .unwrap();
        assert!(got.dropped.is_empty(), "{:?}", got.dropped);
        assert!(got.adj.is_monochrome());
        assert_eq!(
            got.adj.gray_mix,
            crate::develop::GrayMix {
                red: 25.0,
                blue: -40.0,
                magenta: 7.0,
                ..Default::default()
            }
        );
    }

    /// Catches Texture, Clarity or Dehaze still reported as dropped.
    #[test]
    fn presence_keys_fill_texture_clarity_and_dehaze() {
        let got = parse(&packet(
            "crs:Texture=\"+20\" crs:Clarity2012=\"-15\" crs:Dehaze=\"+30\"",
            "",
        ))
        .unwrap();
        assert_eq!(
            (got.adj.texture, got.adj.clarity, got.adj.dehaze),
            (20.0, -15.0, 30.0)
        );
        assert!(got.dropped.is_empty(), "{:?}", got.dropped);
    }

    #[test]
    fn effects_keys_fill_vignette_and_grain() {
        let xmp = "<x:xmpmeta><rdf:RDF><rdf:Description \
            crs:PostCropVignetteAmount=\"-35\" crs:PostCropVignetteMidpoint=\"40\" \
            crs:PostCropVignetteRoundness=\"+20\" crs:PostCropVignetteFeather=\"70\" \
            crs:GrainAmount=\"25\" crs:GrainSize=\"30\" crs:GrainFrequency=\"60\"/>\
            </rdf:RDF></x:xmpmeta>";
        let got = parse(xmp).unwrap();
        assert_eq!(
            got.adj.vignette,
            crate::develop::Vignette {
                amount: -35.0,
                midpoint: 40.0,
                roundness: 20.0,
                feather: 70.0,
            }
        );
        assert_eq!(
            got.adj.grain,
            crate::develop::Grain {
                amount: 25.0,
                size: 30.0,
                roughness: 60.0,
            }
        );
        assert!(got.dropped.is_empty(), "{:?}", got.dropped);
    }

    /// Catches a scanner that reads only the spelling its author looked at.
    #[test]
    fn attribute_and_element_spellings_read_the_same_adjustments() {
        let attr = parse(ATTRIBUTE_PRESET).unwrap();
        let elem = parse(ELEMENT_PRESET).unwrap();
        assert_eq!(attr.adj, expected_preset());
        assert_eq!(elem.adj, expected_preset());
        assert_eq!(attr.name.as_deref(), Some("Warm Film"));
        assert_eq!(elem.name.as_deref(), Some("Warm Film"));
        assert!(attr.dropped.is_empty(), "{:?}", attr.dropped);
        assert!(elem.dropped.is_empty(), "{:?}", elem.dropped);
        assert!(!attr.monochrome);
    }

    /// Catches an unanchored match, where a key matches inside a longer key
    /// or a differently-prefixed attribute.
    #[test]
    fn a_key_does_not_match_inside_a_longer_or_differently_prefixed_one() {
        let attr = parse(&packet(
            "crs:Exposure2012Reference=\"+2.0\" crs:SaturationAdjustmentRed=\"0\" \
             xcrs:Vibrance=\"50\"",
            "",
        ))
        .unwrap();
        assert_eq!(attr.adj.exposure, 0.0);
        assert_eq!(attr.adj.saturation, 0.0);
        assert_eq!(attr.adj.vibrance, 0.0);

        let elem = parse(&packet(
            "",
            "<crs:Exposure2012Reference>+2.0</crs:Exposure2012Reference>\
             <crs:SaturationAdjustmentRed>0</crs:SaturationAdjustmentRed>",
        ))
        .unwrap();
        assert_eq!(elem.adj.exposure, 0.0);
        assert_eq!(elem.adj.saturation, 0.0);
    }

    /// Catches an invented Kelvin conversion.
    #[test]
    fn absolute_temperature_and_tint_are_dropped_not_converted() {
        let got = parse(&packet("crs:Temperature=\"5500\" crs:Tint=\"+10\"", "")).unwrap();
        assert_eq!(got.adj.temp, 0.0);
        assert_eq!(got.adj.tint, 0.0);
        assert_eq!(got.dropped, ["Temperature", "Tint"]);
    }

    /// Catches a number parser that rejects Lightroom's `+`, and an
    /// out-of-range value reaching `Adjustments` unclamped.
    #[test]
    fn a_leading_plus_parses_and_out_of_range_values_clamp() {
        let got = parse(&packet(
            "crs:Exposure2012=\"+7.5\" crs:Contrast2012=\"-250\" crs:Shadows2012=\"+3\"",
            "",
        ))
        .unwrap();
        assert_eq!(got.adj.shadows, 3.0);
        assert_eq!(got.adj.exposure, *EXPOSURE_RANGE.end());
        assert_eq!(got.adj.contrast, *TONE_RANGE.start());
        assert!(got.dropped.is_empty(), "clamping is not a drop");
    }

    /// Catches a scanner that hands entity text through, or one that does not
    /// trim before parsing a number.
    #[test]
    fn xml_entities_decode_and_values_are_trimmed() {
        let got = parse(&packet(
            "",
            "<crs:Name><rdf:Alt><rdf:li xml:lang=\"x-default\">\
             &lt;Tom &amp; Jerry&gt; &quot;Cool&quot; &apos;90s&apos;\
             </rdf:li></rdf:Alt></crs:Name>\
             <crs:Exposure2012>\n  +0.5\n</crs:Exposure2012>\
             <crs:ConvertToGrayscale> True </crs:ConvertToGrayscale>",
        ))
        .unwrap();
        assert_eq!(got.name.as_deref(), Some("<Tom & Jerry> \"Cool\" '90s'"));
        assert_eq!(got.adj.exposure, 0.5);
        assert!(got.monochrome);
    }

    /// Catches an import that leaves a black-and-white preset in colour.
    #[test]
    fn convert_to_grayscale_becomes_full_desaturation() {
        let got = parse(&packet(
            "crs:ConvertToGrayscale=\"True\" crs:Saturation=\"+20\"",
            "",
        ))
        .unwrap();
        assert_eq!(got.adj.saturation, -100.0);
        assert!(got.monochrome);

        let colour = parse(&packet("crs:ConvertToGrayscale=\"False\"", "")).unwrap();
        assert_eq!(colour.adj.saturation, 0.0);
        assert!(!colour.monochrome);
    }

    /// Pins the noise conversion so it cannot drift. Only `LuminanceSmoothing`
    /// reaches denoise, and Camera Raw's default `ColorNoiseReduction` of 25 is
    /// silent, so an import cannot arrive denoised when nobody asked.
    #[test]
    fn only_luminance_smoothing_reaches_denoise() {
        let lum = parse(&packet(
            "crs:LuminanceSmoothing=\"60\" crs:ColorNoiseReduction=\"25\"",
            "",
        ))
        .unwrap();
        assert_eq!(lum.adj.denoise, 60.0);
        assert!(
            !lum.dropped.contains(&"ColorNoiseReduction"),
            "25 is Camera Raw's default, so it is not worth reporting: {:?}",
            lum.dropped
        );

        let untouched = parse(&packet("crs:ColorNoiseReduction=\"25\"", "")).unwrap();
        assert_eq!(
            untouched.adj.denoise, 0.0,
            "a preset that never touched noise must not arrive denoised"
        );

        let moved = parse(&packet("crs:ColorNoiseReduction=\"45\"", "")).unwrap();
        assert_eq!(
            moved.adj.denoise, 0.0,
            "colour noise reduction has no field here"
        );
        assert_eq!(
            moved.dropped,
            ["ColorNoiseReduction"],
            "but moving it off the default is reported"
        );
    }

    /// Catches a report that fires on Lightroom's zero defaults.
    #[test]
    fn a_zero_unsupported_field_is_not_reported_while_a_nonzero_one_is() {
        let zero = parse(&packet(
            "crs:Temperature=\"0\" crs:Tint=\"+0\" crs:CropAngle=\"0\"",
            "",
        ))
        .unwrap();
        assert!(zero.dropped.is_empty(), "{:?}", zero.dropped);

        let set = parse(&packet(
            "crs:CropAngle=\"+15\" crs:Temperature=\"0\" crs:Tint=\"-3\"",
            "",
        ))
        .unwrap();
        assert_eq!(set.dropped, ["Tint", "CropAngle"]);
    }

    /// Catches a parser that turns a stray file into an empty preset.
    #[test]
    fn a_file_that_is_not_a_preset_is_an_error() {
        assert!(parse("just some notes\n").is_err());
        assert!(parse("").is_err());
        assert!(
            parse(&packet("xmp:Rating=\"3\"", "")).is_err(),
            "an XMP packet with no crs: fields is not a preset"
        );
    }

    /// Catches a scanner that reads past the first description block.
    #[test]
    fn a_second_description_block_cannot_contribute_a_value() {
        let two = format!(
            "{}{}",
            packet("crs:Exposure2012=\"+0.2\"", ""),
            packet(
                "crs:Contrast2012=\"+50\"",
                "<crs:Vibrance>40</crs:Vibrance>"
            ),
        );
        let got = parse(&two).unwrap();
        assert_eq!(got.adj.exposure, 0.2);
        assert_eq!(got.adj.contrast, 0.0);
        assert_eq!(got.adj.vibrance, 0.0);
    }

    /// Catches an unreadable value that is silently zero, and a report whose
    /// order depends on scan luck.
    #[test]
    fn an_unparseable_mapped_value_is_dropped_and_left_at_zero() {
        let got = parse(&packet(
            "crs:Saturation=\"lots\" crs:Exposure2012=\"\" crs:Temperature=\"5000\" \
             crs:CropAngle=\"+5\" crs:LuminanceSmoothing=\"NaN\"",
            "<crs:Vibrance/>",
        ))
        .unwrap();
        assert_eq!(got.adj.exposure, 0.0);
        assert_eq!(got.adj.saturation, 0.0);
        assert_eq!(got.adj.vibrance, 0.0);
        assert_eq!(got.adj.denoise, 0.0);
        assert_eq!(
            got.dropped,
            [
                "Exposure2012",
                "Vibrance",
                "Saturation",
                "LuminanceSmoothing",
                "Temperature",
                "CropAngle",
            ],
            "mapped keys in slider order, then the unsupported table in its order"
        );
    }

    /// Catches a name read from the wrong place or an empty name passed on.
    #[test]
    fn the_name_is_the_first_alt_item_or_none() {
        let named = parse(&packet(
            "crs:Exposure2012=\"0\"",
            "<crs:Name><rdf:Alt>\
             <rdf:li xml:lang=\"x-default\">  Golden Hour  </rdf:li>\
             <rdf:li xml:lang=\"fr\">Heure dor\u{e9}e</rdf:li>\
             </rdf:Alt></crs:Name>",
        ))
        .unwrap();
        assert_eq!(named.name.as_deref(), Some("Golden Hour"));

        let unnamed = parse(&packet("crs:Exposure2012=\"0\"", "")).unwrap();
        assert_eq!(unnamed.name, None);

        let blank = parse(&packet(
            "crs:Exposure2012=\"0\"",
            "<crs:Name><rdf:Alt><rdf:li xml:lang=\"x-default\"> </rdf:li></rdf:Alt></crs:Name>",
        ))
        .unwrap();
        assert_eq!(blank.name, None, "a blank name falls back to the caller");
    }

    /// Catches an import that bends the curve for the diagonal Lightroom
    /// writes into every sidecar, and one that loses a bent curve.
    #[test]
    fn point_curves_import_and_a_linear_one_stays_linear() {
        fn curve(name: &str, points: &[&str]) -> String {
            let lis: String = points
                .iter()
                .map(|p| format!("<rdf:li>{p}</rdf:li>"))
                .collect();
            format!("<crs:{name}><rdf:Seq>{lis}</rdf:Seq></crs:{name}>")
        }
        let linear = parse(&packet(
            "crs:Exposure2012=\"0\"",
            &format!(
                "{}{}",
                curve("ToneCurvePV2012", &["0, 0", "128, 128", "255, 255"]),
                curve("ToneCurvePV2012Red", &["0, 0", "255, 255"]),
            ),
        ))
        .unwrap();
        assert!(linear.dropped.is_empty(), "{:?}", linear.dropped);
        assert!(linear.adj.curve.is_linear());

        let bent = parse(&packet(
            "crs:Exposure2012=\"0\"",
            &format!(
                "{}{}{}",
                curve("ToneCurvePV2012", &["0, 0", "255, 255"]),
                curve("ToneCurvePV2012Blue", &["0, 0", "64, 80", "255, 255"]),
                curve("ToneCurvePV2012Green", &["0, 0", "oops", "255, 255"]),
            ),
        ))
        .unwrap();
        assert_eq!(bent.dropped, ["ToneCurvePV2012Green"]);
        assert!(bent.adj.curve.rgb.is_linear());
        assert!(bent.adj.curve.green.is_linear());
        assert_eq!(
            bent.adj.curve.blue.points(),
            &[[0, 0], [64, 80], [255, 255]]
        );
    }
}
