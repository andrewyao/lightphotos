// SPDX-License-Identifier: MIT OR Apache-2.0

//! Renders a matrix of edits through the Loupe shader and through
//! `bake_edited` on the same synthetic photos, and checks that the two agree.

use super::Renderer;
use crate::decode::image_decode::{DecodedImage, DecodedImageFields, PixelFormat};
use crate::develop::curve::Curve;
use crate::develop::image_ops::bake_edited;
use crate::develop::{
    Adjustments, CaScale, Crop, GpuAdjust, GpuTouchUp, Straighten, TouchUp, SLIDERS,
};

const W: u32 = 96;
const H: u32 = 64;

/// One edit in the matrix. Later items add a row here.
struct Case {
    name: String,
    adj: Adjustments,
    touchups: Vec<TouchUp>,
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

fn cases() -> Vec<Case> {
    let case = |name: String, adj: Adjustments| Case {
        name,
        adj,
        touchups: Vec::new(),
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

    let mut out = vec![case("identity".into(), Adjustments::default())];
    for s in &SLIDERS {
        for end in [*s.range.start(), *s.range.end()] {
            let mut adj = Adjustments::default();
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
    });
    out.push(Case {
        name: "touchup_opaque".into(),
        adj: Adjustments::default(),
        touchups: vec![TouchUp {
            opacity: 1.0,
            delta: [0.0; 3],
            ..touchup
        }],
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
    let mut everything = Adjustments {
        temp: 30.0,
        tint: -20.0,
        exposure: 0.7,
        contrast: 25.0,
        highlights: -40.0,
        shadows: 30.0,
        whites: 10.0,
        blacks: -10.0,
        vibrance: 20.0,
        saturation: 15.0,
        denoise: 30.0,
        chromatic_aberration: ca,
        crop,
        ..Default::default()
    };
    everything.curve.rgb = Curve::STRONG_CONTRAST;
    out.push(Case {
        name: "everything".into(),
        adj: everything,
        touchups: vec![touchup],
    });
    out
}

/// The most any channel of a case may differ, in 8-bit steps. Measured on an
/// Apple M-series GPU (Metal) on 2026-10-10. The one entry above 2 is the
/// GPU's own: its bilinear weights have 8 bits, which shows on two pixels of
/// the hard-edged chart when red and blue read between texels.
fn tolerance(case: &str) -> u8 {
    match case {
        "ca" => 3,
        _ => 2,
    }
}

fn measure(r: &mut Renderer, fixture: &Fixture, case: &Case) -> Diff {
    let (w, h) = (fixture.img.width, fixture.img.height);
    r.resize(w, h);
    r.set_image(&fixture.img);
    r.set_transform([1.0, 1.0], [0.0, 0.0], [1.0, 0.0, 0.0, 1.0]);
    r.set_adjustments(
        GpuAdjust::for_image(&case.adj, w as f32, h as f32, case.touchups.len()),
        &case.adj.curve,
    );
    let gpu_touchups: Vec<GpuTouchUp> = case.touchups.iter().map(GpuTouchUp::from).collect();
    r.set_touchups(&gpu_touchups);
    assert!(r.render(None, None, None), "{}: render failed", case.name);
    let gpu = r.read_offscreen_blocking().expect("offscreen readback");
    assert_eq!((gpu.width, gpu.height), (w, h));

    let (cw, ch, cpu) = bake_edited(&fixture.img, &case.adj, &case.touchups, 0);
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
        case: case.name.clone(),
        max,
        over_2,
        pixels: (cw * ch) as usize,
    }
}

#[test]
fn loupe_shader_matches_cpu_bake() {
    let mut r = match Renderer::new_headless(W, H) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("match_test: skipped, no GPU adapter: {e}");
            return;
        }
    };
    let cases = cases();
    let mut diffs: Vec<Diff> = fixtures()
        .iter()
        .flat_map(|f| cases.iter().map(|c| (f, c)).collect::<Vec<_>>())
        .map(|(f, c)| measure(&mut r, f, c))
        .collect();
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
        .filter(|d| d.max > tolerance(&d.case))
        .map(|d| format!("{} {} max {}", d.fixture, d.case, d.max))
        .collect();
    assert!(failing.is_empty(), "over tolerance: {failing:#?}");
}
