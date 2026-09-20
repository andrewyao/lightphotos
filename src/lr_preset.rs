// SPDX-License-Identifier: GPL-3.0-or-later

//! A Lightroom `.xmp` develop-preset reader. It fills the [`Adjustments`]
//! fields a preset maps onto and names the `crs:` fields it had to leave
//! behind, so an import can say what it does not carry.
//!
//! This is a scanner over a known key list, not an XML parser. Camera Raw
//! writes every field either as an attribute or as a child element of one
//! `rdf:Description`, and both spellings are read. A dozen keys do not earn an
//! XML crate, least of all one that has to build for wasm32.

use crate::develop::{Adjustments, SliderId, SLIDERS};

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
    /// A tone curve whose every point sits on the diagonal. Lightroom writes
    /// one into every sidecar.
    Linear,
}

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
        SliderId::Vibrance => &["Vibrance"],
        SliderId::Saturation => &["Saturation"],
        // `LuminanceSmoothing` alone. Camera Raw defaults `ColorNoiseReduction`
        // to 25, so reading it would put denoise on every import that carries
        // the Detail panel at all, which nobody asked for.
        SliderId::Denoise => &["LuminanceSmoothing"],
    }
}

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
    ("Clarity2012", &["Clarity2012"], Untouched::Zero),
    ("Dehaze", &["Dehaze"], Untouched::Zero),
    ("Sharpness", &["Sharpness"], Untouched::Zero),
    ("CropAngle", &["CropAngle"], Untouched::Zero),
    (
        "HueAdjustment*",
        &[
            "HueAdjustmentRed",
            "HueAdjustmentOrange",
            "HueAdjustmentYellow",
            "HueAdjustmentGreen",
            "HueAdjustmentAqua",
            "HueAdjustmentBlue",
            "HueAdjustmentPurple",
            "HueAdjustmentMagenta",
        ],
        Untouched::Zero,
    ),
    (
        "SaturationAdjustment*",
        &[
            "SaturationAdjustmentRed",
            "SaturationAdjustmentOrange",
            "SaturationAdjustmentYellow",
            "SaturationAdjustmentGreen",
            "SaturationAdjustmentAqua",
            "SaturationAdjustmentBlue",
            "SaturationAdjustmentPurple",
            "SaturationAdjustmentMagenta",
        ],
        Untouched::Zero,
    ),
    (
        "LuminanceAdjustment*",
        &[
            "LuminanceAdjustmentRed",
            "LuminanceAdjustmentOrange",
            "LuminanceAdjustmentYellow",
            "LuminanceAdjustmentGreen",
            "LuminanceAdjustmentAqua",
            "LuminanceAdjustmentBlue",
            "LuminanceAdjustmentPurple",
            "LuminanceAdjustmentMagenta",
        ],
        Untouched::Zero,
    ),
    (
        "ToneCurvePV2012*",
        &[
            "ToneCurvePV2012",
            "ToneCurvePV2012Red",
            "ToneCurvePV2012Green",
            "ToneCurvePV2012Blue",
        ],
        Untouched::Linear,
    ),
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

    // Our closest monochrome. Lightroom's B&W mode ignores its own
    // Saturation, so this overrides a mapped one.
    let monochrome =
        value(body, "ConvertToGrayscale").is_some_and(|v| v.eq_ignore_ascii_case("true"));
    if monochrome {
        adj.saturation = -100.0;
    }

    for (label, keys, default) in UNSUPPORTED {
        let set = keys.iter().any(|key| match default {
            Untouched::Zero => is_set(number(body, key)),
            Untouched::Default(d) => is_away_from(number(body, key), *d),
            Untouched::Linear => raw_value(body, key).is_some_and(curve_is_bent),
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

/// A tone curve with some point off the diagonal. A point that does not read
/// as `x, y` counts as bent, for the same reason an unreadable number counts
/// as set.
fn curve_is_bent(seq: &str) -> bool {
    li_texts(seq).any(|point| {
        let mut xy = point.split(',').map(|n| n.trim().parse::<f32>().ok());
        match (xy.next().flatten(), xy.next().flatten()) {
            (Some(x), Some(y)) => x != y,
            _ => true,
        }
    })
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
            denoise: 30.0,
            crop: None,
        }
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
            "crs:Clarity2012=\"0\" crs:Dehaze=\"+0\" crs:HueAdjustmentRed=\"0\" \
             crs:CropAngle=\"0\"",
            "",
        ))
        .unwrap();
        assert!(zero.dropped.is_empty(), "{:?}", zero.dropped);

        let set = parse(&packet(
            "crs:Clarity2012=\"+15\" crs:HueAdjustmentBlue=\"0\" crs:HueAdjustmentAqua=\"-3\"",
            "",
        ))
        .unwrap();
        assert_eq!(set.dropped, ["Clarity2012", "HueAdjustment*"]);
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
             crs:Clarity2012=\"+5\" crs:LuminanceSmoothing=\"NaN\"",
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
                "Clarity2012",
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

    /// Catches a report that fires on the linear curve Lightroom writes into
    /// every sidecar, and one that misses a bent curve.
    #[test]
    fn a_linear_tone_curve_is_not_reported_but_a_bent_one_is() {
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

        let bent = parse(&packet(
            "crs:Exposure2012=\"0\"",
            &format!(
                "{}{}",
                curve("ToneCurvePV2012", &["0, 0", "255, 255"]),
                curve("ToneCurvePV2012Blue", &["0, 0", "64, 80", "255, 255"]),
            ),
        ))
        .unwrap();
        assert_eq!(bent.dropped, ["ToneCurvePV2012*"]);
    }
}
