// SPDX-License-Identifier: MIT OR Apache-2.0

//! Renders a matrix of edits through the Loupe shader and through
//! `bake_edited` on the same synthetic photos, and checks that the two agree.
//! A second matrix draws the base layer through `BaseProbe` on both sides.

use std::sync::Arc;

use super::Renderer;
use crate::decode::image_decode::{DecodedImage, DecodedImageFields, PixelFormat};
use crate::develop::color::hue_rgb;
use crate::develop::curve::Curve;
use crate::develop::image_ops::bake_edited_probe;
use crate::develop::{
    Adjustments, BandShift, BaseProbe, CaScale, ColorAxis, ColorGrade, ColorMixer, Coverage, Crop,
    DetectTarget, GpuAdjust, GpuTouchUp, Grain, GrayMix, Mask, MaskSet, MaskShape, Section,
    Sharpening, SliderId, Straighten, Stroke, TouchUp, Vignette, Wheel, SLIDERS,
};

const W: u32 = 96;
const H: u32 = 64;

/// One edit in the matrix. Later items add a row here.
struct Case {
    name: String,
    adj: Adjustments,
    touchups: Vec<TouchUp>,
    masks: Vec<Mask>,
}

/// A synthetic photo and what it stresses.
struct Fixture {
    name: &'static str,
    img: DecodedImage,
}

/// The measured disagreement of one (fixture, case) pair.
struct Diff {
    fixture: &'static str,
    case: String,
    max: u8,
    /// Pixels with any channel off by more than 2.
    over_2: usize,
    pixels: usize,
}

fn encode(v: f32) -> u8 {
    (v.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0).round() as u8
}

fn photo(px: impl Fn(u32, u32) -> [u8; 3]) -> DecodedImage {
    let mut rgba = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            let [r, g, b] = px(x, y);
            rgba.extend([r, g, b, 255]);
        }
    }
    DecodedImage::new_tracked(DecodedImageFields {
        width: W,
        height: H,
        rgba,
        pixel_format: PixelFormat::Srgb8,
    })
}

fn ramp(x: u32, y: u32) -> [f32; 3] {
    let (xf, yf) = (x as f32, y as f32);
    let tau = std::f32::consts::TAU;
    [
        xf / (W - 1) as f32,
        yf / (H - 1) as f32,
        0.5 + 0.5 * (tau * xf / 23.0).sin() * (tau * yf / 17.0).cos(),
    ]
}

fn fixtures() -> Vec<Fixture> {
    let mut seed: u32 = 1;
    let mut noise = Vec::with_capacity((W * H * 3) as usize);
    for _ in 0..W * H * 3 {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        noise.push(60 + ((seed >> 16) % 141) as u8);
    }
    vec![
        Fixture {
            name: "ramps",
            img: photo(|x, y| ramp(x, y).map(encode)),
        },
        Fixture {
            name: "shadows",
            img: photo(|x, y| ramp(x, y).map(|v| encode(v * 0.12))),
        },
        Fixture {
            name: "edges",
            img: photo(|x, y| {
                if x < 2 || y < 2 || x >= W - 2 || y >= H - 2 {
                    [255, 255, 255]
                } else if (x / 8 + y / 8) % 2 == 0 {
                    [235, 40, 30]
                } else {
                    [25, 60, 230]
                }
            }),
        },
        // Every hue across, dull to vivid down, for the Color Mixer's bands
        // and the seams between them.
        Fixture {
            name: "hues",
            img: photo(|x, y| {
                let deg = 360.0 * x as f32 / W as f32;
                let s = 0.15 + 0.85 * y as f32 / (H - 1) as f32;
                hue_rgb(deg).map(|c| (0.85 * (1.0 - s + s * c) * 255.0).round() as u8)
            }),
        },
        Fixture {
            name: "noise",
            img: photo(|x, y| {
                let i = ((y * W + x) * 3) as usize;
                let lift = (20.0 * (x + y) as f32 / (W + H - 2) as f32) as u8;
                [0, 1, 2].map(|c| noise[i + c].saturating_add(lift))
            }),
        },
    ]
}

/// The coverage every detected mask in the matrix reads: a soft diagonal
/// blob at 13x9, so no bake or Loupe pixel lands on a texel centre.
fn test_coverage() -> Arc<Coverage> {
    let (w, h) = (13u32, 9u32);
    let alpha: Vec<u8> = (0..w * h)
        .map(|i| {
            let (x, y) = (
                (i % w) as f32 / (w - 1) as f32,
                (i / w) as f32 / (h - 1) as f32,
            );
            let d = ((x - 0.6) * 1.4).hypot(y - 0.45);
            ((1.0 - (d / 0.5).clamp(0.0, 1.0)) * 255.0).round() as u8
        })
        .collect();
    Arc::new(Coverage::fit(w, h, &alpha).expect("a whole plane"))
}

fn cases() -> Vec<Case> {
    let case = |name: String, adj: Adjustments| Case {
        name,
        adj,
        touchups: Vec::new(),
        masks: Vec::new(),
    };
    let touchup = TouchUp {
        center: [0.5, 0.5],
        radius: 0.15,
        source: [0.2, 0.3],
        feather: 0.5,
        delta: [0.02, -0.01, 0.0],
        opacity: 0.6,
    };
    let straighten_crop = Some(Straighten::new(7.0, W as f32, H as f32).largest_crop(1.5));
    let crop = Some(Crop {
        left: 0.25,
        top: 0.25,
        right: 0.75,
        bottom: 0.75,
    });
    let ca = Some(CaScale {
        red: 0.004,
        blue: -0.003,
    });

    let wheel = |hue, saturation, luminance| Wheel {
        hue,
        saturation,
        luminance,
    };
    let split = ColorGrade {
        shadows: wheel(220.0, 60.0, 0.0),
        highlights: wheel(40.0, 60.0, 0.0),
        ..Default::default()
    };
    let mut out = vec![case("identity".into(), Adjustments::default())];
    for s in &SLIDERS {
        for end in [*s.range.start(), *s.range.end()] {
            let mut adj = Adjustments::default();
            // The B&W mix acts only on a monochrome photo.
            if s.section == Section::GrayMix {
                adj.saturation = -100.0;
            }
            // A wheel's hue tints only with saturation, and Blending and
            // Balance only move tints that are there.
            match s.id {
                SliderId::Grade(zone, ColorAxis::Hue) => {
                    let mut g = ColorGrade::default();
                    *g.wheel_mut(zone) = wheel(0.0, 60.0, 0.0);
                    adj.grade = g;
                }
                SliderId::GradeBlending | SliderId::GradeBalance => adj.grade = split,
                _ => {}
            }
            *(s.field)(&mut adj) = end;
            out.push(case(format!("{:?}={end}", s.id).to_lowercase(), adj));
        }
    }
    let mut adj = Adjustments::default();
    adj.curve.rgb = Curve::STRONG_CONTRAST;
    out.push(case("curve_rgb".into(), adj));
    let mut adj = Adjustments::default();
    adj.curve.red = Curve::STRONG_CONTRAST;
    out.push(case("curve_red".into(), adj));
    out.push(case(
        "crop".into(),
        Adjustments {
            crop,
            ..Default::default()
        },
    ));
    out.push(case(
        "straighten".into(),
        Adjustments {
            straighten: 7.0,
            crop: straighten_crop,
            ..Default::default()
        },
    ));
    out.push(case(
        "ca".into(),
        Adjustments {
            chromatic_aberration: ca,
            ..Default::default()
        },
    ));
    out.push(Case {
        name: "touchup".into(),
        adj: Adjustments::default(),
        touchups: vec![touchup],
        masks: Vec::new(),
    });
    out.push(Case {
        name: "touchup_opaque".into(),
        adj: Adjustments::default(),
        touchups: vec![TouchUp {
            opacity: 1.0,
            delta: [0.0; 3],
            ..touchup
        }],
        masks: Vec::new(),
    });
    let mut sky = Mask::linear([0.5, 0.1], [0.4, 0.7]);
    sky.amounts = crate::develop::LocalAmounts {
        exposure: 1.2,
        temp: -40.0,
        highlights: -60.0,
        saturation: 30.0,
        ..Default::default()
    };
    let mut spot = Mask {
        shape: MaskShape::Radial {
            center: [0.55, 0.45],
            radii: [0.35, 0.2],
            angle: 0.6,
            feather: 0.7,
        },
        invert: true,
        amounts: crate::develop::LocalAmounts {
            exposure: -0.8,
            contrast: 40.0,
            shadows: 50.0,
            whites: -30.0,
            blacks: 20.0,
            tint: 25.0,
            ..Default::default()
        },
    };
    out.push(Case {
        name: "mask_linear".into(),
        adj: Adjustments::default(),
        touchups: Vec::new(),
        masks: vec![sky.clone()],
    });
    out.push(Case {
        name: "mask_radial_inverted".into(),
        adj: Adjustments {
            exposure: 0.4,
            saturation: -20.0,
            ..Default::default()
        },
        touchups: Vec::new(),
        masks: vec![spot.clone()],
    });
    spot.invert = false;
    out.push(Case {
        name: "masks+crop+straighten".into(),
        adj: Adjustments {
            straighten: 7.0,
            crop: straighten_crop,
            ..Default::default()
        },
        touchups: vec![touchup],
        masks: vec![sky.clone(), spot.clone()],
    });
    // Every detected mask resolves to `test_coverage`, whose odd size puts
    // every read between texels.
    let mut subject = Mask::detected(DetectTarget::Subject);
    subject.amounts = crate::develop::LocalAmounts {
        exposure: -1.5,
        contrast: 30.0,
        temp: 35.0,
        saturation: -40.0,
        ..Default::default()
    };
    out.push(Case {
        name: "mask_raster".into(),
        adj: Adjustments::default(),
        touchups: Vec::new(),
        masks: vec![subject.clone()],
    });
    subject.invert = true;
    out.push(Case {
        name: "mask_raster_inverted".into(),
        adj: Adjustments {
            exposure: 0.3,
            ..Default::default()
        },
        touchups: Vec::new(),
        masks: vec![subject.clone()],
    });
    subject.invert = false;
    // A turned canvas reads UVs past 0..1 at the corners, which exercises
    // the edge clamp on both sides.
    out.push(Case {
        name: "mask_raster+crop+straighten".into(),
        adj: Adjustments {
            straighten: 7.0,
            crop: straighten_crop,
            ..Default::default()
        },
        touchups: Vec::new(),
        masks: vec![sky.clone(), subject, spot.clone()],
    });
    // A brush mask renders its strokes' raster through the same coverage
    // path, so these hold the raster's size and layout to the CPU's.
    let paint = |radius: f32, flow: f32, erase: bool, points: &[[f32; 2]]| Stroke {
        radius,
        feather: 0.6,
        flow,
        erase,
        points: points.to_vec(),
    };
    let band = paint(0.12, 0.8, false, &[[0.05, 0.3], [0.4, 0.55], [0.95, 0.6]]);
    let mut brush = Mask {
        shape: MaskShape::Brush {
            strokes: vec![band.clone()].into(),
        },
        invert: false,
        amounts: crate::develop::LocalAmounts {
            exposure: 1.1,
            shadows: 40.0,
            temp: -30.0,
            saturation: 25.0,
            ..Default::default()
        },
    };
    out.push(Case {
        name: "mask_brush".into(),
        adj: Adjustments::default(),
        touchups: Vec::new(),
        masks: vec![brush.clone()],
    });
    brush.invert = true;
    out.push(Case {
        name: "mask_brush_inverted".into(),
        adj: Adjustments {
            exposure: -0.2,
            ..Default::default()
        },
        touchups: Vec::new(),
        masks: vec![brush.clone()],
    });
    brush.invert = false;
    out.push(Case {
        name: "mask_brush+crop+straighten".into(),
        adj: Adjustments {
            straighten: 7.0,
            crop: straighten_crop,
            ..Default::default()
        },
        touchups: Vec::new(),
        masks: vec![spot, brush.clone()],
    });
    brush.shape = MaskShape::Brush {
        strokes: vec![
            band,
            paint(0.05, 1.0, true, &[[0.5, 0.1], [0.45, 0.9]]),
            paint(0.08, 0.3, false, &[[0.2, 0.8], [0.3, 0.75]]),
        ]
        .into(),
    };
    out.push(Case {
        name: "brush_erase".into(),
        adj: Adjustments::default(),
        touchups: Vec::new(),
        masks: vec![brush],
    });
    out.push(case(
        "denoise+straighten".into(),
        Adjustments {
            denoise: 60.0,
            straighten: 7.0,
            crop: straighten_crop,
            ..Default::default()
        },
    ));
    let vignette = |amount, midpoint, roundness, feather| Vignette {
        amount,
        midpoint,
        roundness,
        feather,
    };
    for (name, v) in [
        ("vignette_round", vignette(-100.0, 20.0, 100.0, 0.0)),
        ("vignette_square", vignette(70.0, 80.0, -100.0, 100.0)),
    ] {
        out.push(case(
            name.into(),
            Adjustments {
                vignette: v,
                ..Default::default()
            },
        ));
    }
    out.push(case(
        "vignette+crop".into(),
        Adjustments {
            vignette: vignette(-80.0, 30.0, 0.0, 30.0),
            crop,
            ..Default::default()
        },
    ));
    out.push(case(
        "vignette+straighten".into(),
        Adjustments {
            vignette: vignette(-80.0, 30.0, 0.0, 30.0),
            straighten: 7.0,
            crop: straighten_crop,
            ..Default::default()
        },
    ));
    for (name, size, roughness) in [("grain_coarse", 100.0, 0.0), ("grain_rough", 100.0, 100.0)] {
        out.push(case(
            name.into(),
            Adjustments {
                grain: Grain {
                    amount: 100.0,
                    size,
                    roughness,
                },
                ..Default::default()
            },
        ));
    }
    let presence = |texture, clarity, dehaze| Adjustments {
        texture,
        clarity,
        dehaze,
        ..Default::default()
    };
    out.push(Case {
        name: "texture+touchup".into(),
        adj: presence(100.0, 0.0, 0.0),
        touchups: vec![touchup],
        masks: Vec::new(),
    });
    out.push(Case {
        name: "clarity+touchup".into(),
        adj: presence(0.0, 100.0, 0.0),
        touchups: vec![touchup],
        masks: Vec::new(),
    });
    out.push(case(
        "presence+straighten".into(),
        Adjustments {
            straighten: 7.0,
            crop: straighten_crop,
            ..presence(-60.0, 60.0, 50.0)
        },
    ));
    out.push(case(
        "presence+ca".into(),
        Adjustments {
            chromatic_aberration: ca,
            ..presence(70.0, -70.0, -50.0)
        },
    ));
    out.push(case(
        "presence+denoise".into(),
        Adjustments {
            denoise: 50.0,
            ..presence(80.0, 40.0, 0.0)
        },
    ));
    let sharp = |amount, radius, detail, masking| Sharpening {
        amount,
        radius,
        detail,
        masking,
    };
    for (name, s) in [
        ("sharpen_fine", sharp(150.0, 0.5, 100.0, 0.0)),
        ("sharpen_wide", sharp(150.0, 3.0, 25.0, 0.0)),
        ("sharpen_masked", sharp(100.0, 1.5, 50.0, 80.0)),
    ] {
        out.push(case(
            name.into(),
            Adjustments {
                sharpening: s,
                ..Default::default()
            },
        ));
    }
    out.push(Case {
        name: "sharpen+touchup".into(),
        adj: Adjustments {
            sharpening: sharp(120.0, 2.0, 60.0, 0.0),
            ..Default::default()
        },
        touchups: vec![touchup],
        masks: Vec::new(),
    });
    out.push(case(
        "sharpen+straighten".into(),
        Adjustments {
            sharpening: sharp(120.0, 2.0, 60.0, 0.0),
            straighten: 7.0,
            crop: straighten_crop,
            ..Default::default()
        },
    ));
    let shift = |hue, saturation, luminance| BandShift {
        hue,
        saturation,
        luminance,
    };
    let mixer = ColorMixer {
        red: shift(40.0, 30.0, -20.0),
        orange: shift(-30.0, -50.0, 40.0),
        yellow: shift(100.0, 0.0, 0.0),
        green: shift(-80.0, 80.0, -60.0),
        aqua: shift(0.0, -100.0, 100.0),
        blue: shift(60.0, 100.0, -100.0),
        purple: shift(-100.0, -30.0, 20.0),
        magenta: shift(20.0, 60.0, 60.0),
    };
    out.push(case(
        "mixer_all".into(),
        Adjustments {
            mixer,
            ..Default::default()
        },
    ));
    let mut mixer_curve = Adjustments {
        mixer,
        contrast: 40.0,
        ..Default::default()
    };
    mixer_curve.curve.rgb = Curve::STRONG_CONTRAST;
    out.push(case("mixer+curve".into(), mixer_curve));
    out.push(case(
        "mixer+monochrome".into(),
        Adjustments {
            mixer,
            saturation: -100.0,
            ..Default::default()
        },
    ));
    let gray_mix = GrayMix {
        red: 80.0,
        orange: -40.0,
        yellow: 100.0,
        green: -100.0,
        aqua: 30.0,
        blue: -70.0,
        purple: 60.0,
        magenta: -20.0,
    };
    out.push(case(
        "gray_all".into(),
        Adjustments {
            gray_mix,
            saturation: -100.0,
            ..Default::default()
        },
    ));
    out.push(case(
        "gray+color".into(),
        Adjustments {
            gray_mix,
            mixer,
            ..Default::default()
        },
    ));
    let grade = ColorGrade {
        shadows: wheel(210.0, 70.0, -30.0),
        midtones: wheel(120.0, 40.0, 20.0),
        highlights: wheel(45.0, 80.0, 40.0),
        global: wheel(300.0, 30.0, -10.0),
        blending: 80.0,
        balance: -40.0,
    };
    out.push(case(
        "grade_all".into(),
        Adjustments {
            grade,
            ..Default::default()
        },
    ));
    out.push(case(
        "grade+mono".into(),
        Adjustments {
            grade: split,
            saturation: -100.0,
            ..Default::default()
        },
    ));
    out.push(case(
        "mixer+ca".into(),
        Adjustments {
            mixer,
            chromatic_aberration: ca,
            ..Default::default()
        },
    ));
    let mut everything = Adjustments {
        temp: 30.0,
        tint: -20.0,
        exposure: 0.7,
        contrast: 25.0,
        highlights: -40.0,
        shadows: 30.0,
        whites: 10.0,
        blacks: -10.0,
        texture: 30.0,
        clarity: 40.0,
        dehaze: 25.0,
        sharpening: sharp(80.0, 1.0, 40.0, 20.0),
        vibrance: 20.0,
        saturation: 15.0,
        denoise: 30.0,
        chromatic_aberration: ca,
        crop,
        vignette: vignette(-40.0, 50.0, 0.0, 50.0),
        grain: Grain {
            amount: 40.0,
            ..Default::default()
        },
        ..Default::default()
    };
    everything.curve.rgb = Curve::STRONG_CONTRAST;
    out.push(Case {
        name: "everything".into(),
        adj: everything,
        touchups: vec![touchup],
        masks: Vec::new(),
    });
    // The colour tools on top of everything else, but without CA, which
    // `mixer+ca` covers with its own bound.
    out.push(Case {
        name: "everything_color".into(),
        adj: Adjustments {
            mixer,
            grade,
            chromatic_aberration: None,
            ..everything
        },
        touchups: vec![touchup],
        masks: Vec::new(),
    });
    out.push(Case {
        name: "everything_mono".into(),
        adj: Adjustments {
            mixer,
            grade,
            gray_mix,
            saturation: -100.0,
            chromatic_aberration: None,
            ..everything
        },
        touchups: vec![touchup],
        masks: Vec::new(),
    });
    out
}

/// The most any channel of a case may differ, in 8-bit steps. Measured on an
/// Apple M-series GPU (Metal) on 2026-10-10. The entries above 2 are the
/// GPU's own: its bilinear weights have 8 bits, which shows on two pixels of
/// the hard-edged chart when red and blue read between texels. The detail
/// view multiplies the pixel's own error by 8, so a turned photo, whose
/// every read falls between texels, shows it on the noise chart (13 on 16
/// pixels). The layer itself holds at 2 there: `straighten/mid`, `/wide`
/// and `/dark` hold. The hue chart has a channel at 0 over a third of its
/// width, where the dark view's gamma step is steepest, so the same CA
/// weights show as 4 on 30 of its pixels, and the Color Mixer, which can
/// double a band's chroma, takes `ca`'s two edge pixels to 4 (both
/// measured 2026-10-10).
fn tolerance(fixture: &str, case: &str) -> u8 {
    match (fixture, case) {
        (_, "ca" | "ca/detail") => 3,
        ("hues", "ca/dark") | (_, "mixer+ca") => 4,
        (_, "straighten/detail") => 13,
        _ => 2,
    }
}

fn measure(r: &mut Renderer, fixture: &Fixture, case: &Case, probe: BaseProbe) -> Diff {
    let (w, h) = (fixture.img.width, fixture.img.height);
    r.resize(w, h);
    r.set_image(&fixture.img);
    r.set_transform([1.0, 1.0], [0.0, 0.0], [1.0, 0.0, 0.0, 1.0]);
    r.set_adjustments(
        GpuAdjust {
            base_probe: probe.gpu(),
            ..GpuAdjust::for_image(
                &case.adj,
                w as f32,
                h as f32,
                case.touchups.len(),
                case.masks.len(),
            )
        },
        &case.adj.curve,
    );
    let gpu_touchups: Vec<GpuTouchUp> = case.touchups.iter().map(GpuTouchUp::from).collect();
    r.set_touchups(&gpu_touchups);
    let masks = MaskSet::new(&case.masks, |_| Some(test_coverage()));
    r.set_masks(&masks.on_photo(w as f32 / h as f32));
    assert!(r.render(None, None, None), "{}: render failed", case.name);
    let gpu = r.read_offscreen_blocking().expect("offscreen readback");
    assert_eq!((gpu.width, gpu.height), (w, h));

    let (cw, ch, cpu) =
        bake_edited_probe(&fixture.img, &case.adj, &case.touchups, &masks, 0, probe);
    let crop = case.adj.crop.unwrap_or(Crop {
        left: 0.0,
        top: 0.0,
        right: 1.0,
        bottom: 1.0,
    });
    let x0 = (crop.left * w as f32).round() as u32;
    let y0 = (crop.top * h as f32).round() as u32;
    assert_eq!(
        cw,
        (crop.right * w as f32).round() as u32 - x0,
        "{}",
        case.name
    );
    assert_eq!(
        ch,
        (crop.bottom * h as f32).round() as u32 - y0,
        "{}",
        case.name
    );

    let (mut max, mut over_2) = (0u8, 0usize);
    for y in 0..ch {
        for x in 0..cw {
            let c = ((y * cw + x) * 4) as usize;
            let g = (((y0 + y) * w + x0 + x) * 3) as usize;
            let d = (0..3)
                .map(|i| cpu[c + i].abs_diff(gpu.rgb[g + i]))
                .max()
                .unwrap_or(0);
            max = max.max(d);
            over_2 += usize::from(d > 2);
        }
    }
    Diff {
        fixture: fixture.name,
        case: match probe {
            BaseProbe::Off => case.name.clone(),
            _ => format!("{}/{probe:?}", case.name).to_lowercase(),
        },
        max,
        over_2,
        pixels: (cw * ch) as usize,
    }
}

fn headless(w: u32, h: u32) -> Option<Renderer> {
    match Renderer::new_headless(w, h) {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!("match_test: skipped, no GPU adapter: {e}");
            None
        }
    }
}

/// Print every pair, worst first, and fail on any over its tolerance.
fn report(mut diffs: Vec<Diff>) {
    diffs.sort_by_key(|d| std::cmp::Reverse(d.max));
    for d in &diffs {
        eprintln!(
            "{:8} {:20} {:3} {:5}/{}",
            d.fixture, d.case, d.max, d.over_2, d.pixels
        );
    }
    if let Some(d) = diffs.first() {
        eprintln!("worst: {} {} max {}", d.fixture, d.case, d.max);
    }
    let failing: Vec<String> = diffs
        .iter()
        .filter(|d| d.max > tolerance(d.fixture, &d.case))
        .map(|d| format!("{} {} max {}", d.fixture, d.case, d.max))
        .collect();
    assert!(failing.is_empty(), "over tolerance: {failing:#?}");
}

#[test]
fn loupe_shader_matches_cpu_bake() {
    let Some(mut r) = headless(W, H) else {
        return;
    };
    let cases = cases();
    let diffs = fixtures()
        .iter()
        .flat_map(|f| cases.iter().map(move |c| (f, c)))
        .map(|(f, c)| measure(&mut r, f, c, BaseProbe::Off))
        .collect();
    report(diffs);
}

/// The GPU base layer, read through `baseAt`, against `BaseLayer`, read
/// through `at_uv`, for every field and the 8x detail view.
#[test]
fn loupe_base_layer_matches_cpu_base() {
    let Some(mut r) = headless(W, H) else {
        return;
    };
    let cases: Vec<Case> = cases()
        .into_iter()
        .filter(|c| ["identity", "ca", "touchup", "straighten", "everything"].contains(&&*c.name))
        .collect();
    assert_eq!(cases.len(), 5);
    let probes = [
        BaseProbe::Mid,
        BaseProbe::Wide,
        BaseProbe::Dark,
        BaseProbe::Detail,
        BaseProbe::Fine,
    ];
    let mut diffs = Vec::new();
    for f in &fixtures() {
        for c in &cases {
            for p in probes {
                diffs.push(measure(&mut r, f, c, p));
            }
        }
    }
    report(diffs);
}

/// Grain at full strength: on the small charts every pixel covers several
/// grain cells, which fades it, so this frame is large enough for the
/// coarse grain to land at about one cell per pixel or more.
#[test]
fn loupe_grain_matches_cpu_bake_at_full_strength() {
    let (w, h) = (768, 512);
    let Some(mut r) = headless(w, h) else {
        return;
    };
    let fixture = Fixture {
        name: "gray",
        img: DecodedImage::new_tracked(DecodedImageFields {
            width: w,
            height: h,
            rgba: [118, 118, 118, 255].repeat((w * h) as usize),
            pixel_format: PixelFormat::Srgb8,
        }),
    };
    let grain = |amount, size, roughness| Case {
        name: format!("grain {amount}/{size}/{roughness}"),
        adj: Adjustments {
            grain: Grain {
                amount,
                size,
                roughness,
            },
            ..Default::default()
        },
        touchups: Vec::new(),
        masks: Vec::new(),
    };
    let cases = [
        grain(100.0, 100.0, 0.0),
        grain(100.0, 100.0, 100.0),
        grain(100.0, 25.0, 50.0),
    ];
    let diffs = cases
        .iter()
        .map(|c| measure(&mut r, &fixture, c, BaseProbe::Off))
        .collect();
    report(diffs);
}

/// Texture, Clarity and Dehaze on a frame large enough that the fine grid
/// is its full 768 cells, coarser than a pixel's neighbours, as on a real
/// photo.
#[test]
fn loupe_presence_matches_cpu_bake_on_a_large_frame() {
    let (w, h) = (1152, 768);
    let Some(mut r) = headless(w, h) else {
        return;
    };
    let mut seed: u32 = 7;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = ((seed >> 16) % 61) as f32 - 30.0;
            let (u, v) = (x as f32 / w as f32, y as f32 / h as f32);
            let base = [40.0 + 170.0 * u, 60.0 + 120.0 * v, 200.0 - 150.0 * u * v];
            rgba.extend(base.map(|c| (c + n).clamp(0.0, 255.0) as u8));
            rgba.push(255);
        }
    }
    let fixture = Fixture {
        name: "large",
        img: DecodedImage::new_tracked(DecodedImageFields {
            width: w,
            height: h,
            rgba,
            pixel_format: PixelFormat::Srgb8,
        }),
    };
    let presence = |name: &str, texture, clarity, dehaze| Case {
        name: name.into(),
        adj: Adjustments {
            texture,
            clarity,
            dehaze,
            ..Default::default()
        },
        touchups: Vec::new(),
        masks: Vec::new(),
    };
    let cases = [
        presence("texture=100", 100.0, 0.0, 0.0),
        presence("texture=-100", -100.0, 0.0, 0.0),
        presence("clarity=100", 0.0, 100.0, 0.0),
        presence("dehaze=100", 0.0, 0.0, 100.0),
        presence("presence", 60.0, -40.0, -50.0),
    ];
    let diffs = cases
        .iter()
        .map(|c| measure(&mut r, &fixture, c, BaseProbe::Off))
        .collect();
    report(diffs);
}

/// Sharpening at the frame size its Radius is measured on, a 4000 px
/// square, where the taps step whole texels apart. A crop keeps the CPU
/// side small.
#[test]
fn loupe_sharpening_matches_cpu_bake_at_full_size() {
    let (w, h) = (4000, 4000);
    let Some(mut r) = headless(w, h) else {
        return;
    };
    let mut seed: u32 = 11;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = ((seed >> 16) % 41) as u8;
            let checker = if (x / 3 + y / 5) % 2 == 0 { 70 } else { 170 };
            rgba.extend([checker + n, checker, 200 - n, 255]);
        }
    }
    let fixture = Fixture {
        name: "4000px",
        img: DecodedImage::new_tracked(DecodedImageFields {
            width: w,
            height: h,
            rgba,
            pixel_format: PixelFormat::Srgb8,
        }),
    };
    let crop = Some(Crop {
        left: 0.45,
        top: 0.45,
        right: 0.55,
        bottom: 0.55,
    });
    let sharp = |name: &str, amount, radius, detail, masking| Case {
        name: name.into(),
        adj: Adjustments {
            sharpening: Sharpening {
                amount,
                radius,
                detail,
                masking,
            },
            crop,
            ..Default::default()
        },
        touchups: Vec::new(),
        masks: Vec::new(),
    };
    let spot = Case {
        touchups: vec![TouchUp {
            center: [0.5, 0.5],
            radius: 0.01,
            source: [0.47, 0.48],
            feather: 0.3,
            delta: [0.0; 3],
            opacity: 1.0,
        }],
        ..sharp("sharpen+touchup", 150.0, 2.0, 80.0, 0.0)
    };
    let cases = [
        sharp("sharpen r=0.5", 150.0, 0.5, 100.0, 0.0),
        sharp("sharpen r=1.4", 150.0, 1.4, 50.0, 0.0),
        sharp("sharpen r=3", 150.0, 3.0, 25.0, 0.0),
        sharp("sharpen masked", 100.0, 1.0, 60.0, 70.0),
        spot,
    ];
    let diffs = cases
        .iter()
        .map(|c| measure(&mut r, &fixture, c, BaseProbe::Off))
        .collect();
    report(diffs);
}

/// A slider drag rewrites only the adjustments, and the app re-sends the
/// same spots with each, so no frame of it may rebuild the base layer.
/// Changing the spots or the CA scales must.
#[test]
fn only_the_spots_and_the_ca_scales_rebuild_the_base_layer() {
    let Some(mut r) = headless(W, H) else {
        return;
    };
    let fixtures = fixtures();
    let img = &fixtures[0].img;
    let spot = GpuTouchUp::from(&TouchUp {
        center: [0.5, 0.5],
        radius: 0.1,
        source: [0.2, 0.2],
        feather: 0.5,
        delta: [0.0; 3],
        opacity: 1.0,
    });
    // A reader of the layer, as item 1.2's sliders will be: nothing builds
    // it otherwise.
    let push = |r: &mut Renderer, adj: &Adjustments, spots: &[GpuTouchUp]| {
        let gpu = GpuAdjust {
            base_on: 1.0,
            ..GpuAdjust::for_image(adj, W as f32, H as f32, spots.len(), 0)
        };
        r.set_adjustments(gpu, &adj.curve);
        r.set_touchups(spots);
        assert!(r.render(None, None, None));
    };
    r.set_image(img);
    push(&mut r, &Adjustments::default(), &[spot]);
    let built = r.base_builds;
    for i in 0..30 {
        let adj = Adjustments {
            exposure: i as f32 * 0.1,
            temp: -(i as f32),
            ..Default::default()
        };
        push(&mut r, &adj, &[spot]);
    }
    assert_eq!(r.base_builds, built, "a slider drag rebuilt the base layer");

    let moved = GpuTouchUp {
        source: [0.3, 0.2],
        ..spot
    };
    push(&mut r, &Adjustments::default(), &[moved]);
    assert_eq!(r.base_builds, built + 1, "a moved spot");
    let ca = Adjustments {
        chromatic_aberration: Some(CaScale {
            red: 0.002,
            blue: 0.0,
        }),
        ..Default::default()
    };
    push(&mut r, &ca, &[moved]);
    assert_eq!(r.base_builds, built + 2, "a new CA scale");
    r.set_image(img);
    push(&mut r, &ca, &[moved]);
    assert_eq!(r.base_builds, built + 3, "a new image");
}

/// Each layer builds only for an edit that reads it, and turning one on
/// leaves the other alone.
#[test]
fn each_layer_builds_only_for_its_reader() {
    let Some(mut r) = headless(W, H) else {
        return;
    };
    let fixtures = fixtures();
    r.set_image(&fixtures[0].img);
    let mut push = |adj: Adjustments| {
        r.set_adjustments(
            GpuAdjust::for_image(&adj, W as f32, H as f32, 0, 0),
            &adj.curve,
        );
        r.set_touchups(&[]);
        assert!(r.render(None, None, None));
        (r.base_builds, r.fine.is_some())
    };
    assert_eq!(push(Adjustments::default()), (0, false), "no reader");
    let texture = Adjustments {
        texture: 50.0,
        ..Default::default()
    };
    assert_eq!(push(texture), (1, true), "Texture builds the fine layer");
    let both = Adjustments {
        clarity: 30.0,
        ..texture
    };
    assert_eq!(push(both), (2, true), "Clarity adds the base layer only");
    assert_eq!(push(both), (2, true), "nothing changed");
}

/// Frame times on a 24 MP photo, with the base layer cached (a slider
/// drag) and rebuilt every frame. Prints; asserts nothing. Run it with
/// `cargo test --release frame_time -- --ignored --nocapture`.
#[test]
#[ignore = "a timing report, not a check"]
fn frame_time_with_and_without_a_base_rebuild() {
    let (sw, sh) = (1600, 1067);
    let Some(mut r) = headless(sw, sh) else {
        return;
    };
    let (w, h) = (6000u32, 4000u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            rgba.extend([
                (x * 255 / w) as u8,
                (y * 255 / h) as u8,
                ((x ^ y) & 255) as u8,
                255,
            ]);
        }
    }
    let img = DecodedImage::new_tracked(DecodedImageFields {
        width: w,
        height: h,
        rgba,
        pixel_format: PixelFormat::Srgb8,
    });
    let wait = |r: &Renderer| {
        r.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
    };
    r.set_image(&img);
    r.set_transform([1.0, 1.0], [0.0, 0.0], [1.0, 0.0, 0.0, 1.0]);
    let frames = |r: &mut Renderer, rebuild: bool| {
        let t0 = std::time::Instant::now();
        let built = r.base_builds;
        for i in 0..60 {
            let adj = Adjustments {
                exposure: (i % 20) as f32 * 0.05,
                ..Default::default()
            };
            let gpu = GpuAdjust {
                base_on: 1.0,
                fine_on: 1.0,
                ..GpuAdjust::for_image(&adj, w as f32, h as f32, 0, 0)
            };
            r.set_adjustments(gpu, &adj.curve);
            r.set_touchups(&[]);
            if rebuild {
                r.layers_built_for = [None; 2];
            }
            assert!(r.render(None, None, None));
            wait(r);
        }
        (t0.elapsed() / 60, r.base_builds - built)
    };
    // The first frame builds the layer; time the ones after it.
    r.set_adjustments(
        GpuAdjust {
            base_on: 1.0,
            fine_on: 1.0,
            ..GpuAdjust::for_image(&Adjustments::default(), w as f32, h as f32, 0, 0)
        },
        &Adjustments::default().curve,
    );
    assert!(r.render(None, None, None));
    wait(&r);
    let (cached, cached_builds) = frames(&mut r, false);
    let (rebuilt, rebuilds) = frames(&mut r, true);
    eprintln!("frame_time: {w}x{h} source, {sw}x{sh} target, 60 frames each");
    eprintln!("frame_time: cached base   {cached:?}/frame, {cached_builds} base builds");
    eprintln!("frame_time: rebuilt base  {rebuilt:?}/frame, {rebuilds} base builds");
}
