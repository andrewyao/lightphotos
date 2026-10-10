// SPDX-License-Identifier: MIT OR Apache-2.0

//! Renders a matrix of edits through the Loupe shader and through
//! `bake_edited` on the same synthetic photos, and checks that the two agree.
//! A second matrix draws the base layer through `BaseProbe` on both sides.

use super::Renderer;
use crate::decode::image_decode::{DecodedImage, DecodedImageFields, PixelFormat};
use crate::develop::curve::Curve;
use crate::develop::image_ops::bake_edited_probe;
use crate::develop::{
    Adjustments, BaseProbe, CaScale, Crop, GpuAdjust, GpuTouchUp, Grain, Straighten, TouchUp,
    Vignette, SLIDERS,
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
    });
    out.push(Case {
        name: "clarity+touchup".into(),
        adj: presence(0.0, 100.0, 0.0),
        touchups: vec![touchup],
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
/// and `/dark` hold.
fn tolerance(case: &str) -> u8 {
    match case {
        "ca" | "ca/detail" => 3,
        "straighten/detail" => 13,
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
            ..GpuAdjust::for_image(&case.adj, w as f32, h as f32, case.touchups.len())
        },
        &case.adj.curve,
    );
    let gpu_touchups: Vec<GpuTouchUp> = case.touchups.iter().map(GpuTouchUp::from).collect();
    r.set_touchups(&gpu_touchups);
    assert!(r.render(None, None, None), "{}: render failed", case.name);
    let gpu = r.read_offscreen_blocking().expect("offscreen readback");
    assert_eq!((gpu.width, gpu.height), (w, h));

    let (cw, ch, cpu) = bake_edited_probe(&fixture.img, &case.adj, &case.touchups, 0, probe);
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
        .filter(|d| d.max > tolerance(&d.case))
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
            ..GpuAdjust::for_image(adj, W as f32, H as f32, spots.len())
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
            GpuAdjust::for_image(&adj, W as f32, H as f32, 0),
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
                ..GpuAdjust::for_image(&adj, w as f32, h as f32, 0)
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
            ..GpuAdjust::for_image(&Adjustments::default(), w as f32, h as f32, 0)
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
