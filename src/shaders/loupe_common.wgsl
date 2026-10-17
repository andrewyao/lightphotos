// SPDX-License-Identifier: MIT OR Apache-2.0

// Declarations shared by the two loupe fragment shaders. Rust prepends this
// file to `shader.wgsl` and `raw_shader.wgsl`, so each is one module.

// Field order must match `GpuAdjust` in develop/mod.rs.
struct Adjust {
    exposure: f32,
    contrast: f32,
    highlights: f32,
    shadows: f32,
    whites: f32,
    blacks: f32,
    temp: f32,
    tint: f32,
    crop_l: f32,
    crop_t: f32,
    crop_r: f32,
    crop_b: f32,
    denoise: f32,
    vibrance: f32,
    saturation: f32,
    texel_w: f32,
    texel_h: f32,
    _pad0: f32,
    // Radians. See `straightenUv`.
    straighten: f32,
    // Chromatic aberration scales. See `caUv`.
    ca_red: f32,
    ca_blue: f32,
    // 1 when `curve_lut` holds a bent point curve. See `toneCurve`.
    curve_on: f32,
    // 0, or the base-layer view `baseProbe` draws instead of the photo.
    base_probe: f32,
    // 1 when an edit reads the base layer; the renderer builds it only then.
    base_on: f32,
    // -100..100 each. See `localAdjust`.
    texture: f32,
    clarity: f32,
    dehaze: f32,
    // 1 when Texture reads the fine layer; the renderer builds it only then.
    fine_on: f32,
    // Sharpening, derived on the CPU by `develop::Sharpen`. See
    // `sharpenAdjust`.
    sharp_gain: f32,
    sharp_step: f32,
    sharp_limit: f32,
    sharp_mask: f32,
    // Vignette and Grain, derived on the CPU by `develop::Effects`. See
    // `effects`.
    vig_amount: f32,
    vig_exponent: f32,
    vig_lo: f32,
    vig_hi: f32,
    vig_axis: vec2<f32>,
    vig_center: vec2<f32>,
    grain_amount: f32,
    grain_rough: f32,
    grain_scale: vec2<f32>,
    // The Color Mixer and the B&W mix, derived on the CPU by
    // `develop::ColorOps`. Each array is 8 floats, one per band. See
    // `colorMix`.
    color_on: f32,
    _cpad0: f32,
    _cpad1: f32,
    _cpad2: f32,
    band_hue: array<vec4<f32>, 2>,
    mix_hue: array<vec4<f32>, 2>,
    mix_sat: array<vec4<f32>, 2>,
    mix_lum: array<vec4<f32>, 2>,
    gray: array<vec4<f32>, 2>,
    // Color Grading, derived on the CPU by `develop::GradeOps`. Each wheel
    // is (a, b, lightness, pad), in shadows, midtones, highlights, global
    // order. See `colorGrade`.
    grade_on: f32,
    grade_mid: f32,
    grade_width: f32,
    _gpad: f32,
    grade_wheels: array<vec4<f32>, 4>,
    // How many entries of `masks` apply; 0 for none. See `localTone`.
    mask_count: f32,
    _mpad0: f32,
    _mpad1: f32,
    _mpad2: f32,
};

struct TouchUp {
    center_radius_feather: vec4<f32>,
    source: vec2<f32>,
    opacity: f32,
    _pad: f32,
    delta: vec4<f32>,
};

// Must match `GpuMask` in develop/mask.rs.
struct Mask {
    // Linear: start, end. Radial: center, radii.
    geom: vec4<f32>,
    // Kind (0 linear, 1 radial, 2 coverage), invert, angle, feather.
    params: vec4<f32>,
    // Exposure, contrast, highlights, shadows.
    a0: vec4<f32>,
    // Whites, blacks, temp, tint.
    a1: vec4<f32>,
    // Saturation, then padding.
    a2: vec4<f32>,
    // A coverage mask's byte offset into `coverage`, width, height, 0.
    cov: vec4<u32>,
};

// The sliders a mask can move, at one pixel.
struct LocalTone {
    exposure: f32,
    contrast: f32,
    highlights: f32,
    shadows: f32,
    whites: f32,
    blacks: f32,
    temp: f32,
    tint: f32,
    saturation: f32,
};

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
// The base layer: one texel per grid cell, Rgba32Float, (mid, wide, dark).
// base.wgsl builds it. Read only through `baseAt`.
@group(0) @binding(2) var base_tex: texture_2d<f32>;
// The fine layer: the same build on a finer grid. Texture reads its `mid`.
@group(0) @binding(3) var fine_tex: texture_2d<f32>;
@group(2) @binding(0) var<uniform> adj: Adjust;
// The baked point curve: entry i is the red, green and blue output for input
// i / 255. Written from `GpuCurve` in develop/mod.rs.
@group(2) @binding(1) var<uniform> curve_lut: array<vec4<f32>, 256>;
@group(3) @binding(0) var<storage, read> touchups: array<TouchUp>;
// Bindings 4 and 5, past the overlay's 1 to 3 in shader.wgsl, which share
// group 3.
@group(3) @binding(4) var<storage, read> masks: array<Mask>;
// Detected and brush masks' coverage, packed by `PhotoMasks::pack_coverage`: one byte per
// texel, four per word, little-endian, rows unpadded. A storage buffer, not
// a texture array: coverages differ in size, and the manual bilinear below
// is what CPU parity needs anyway.
@group(3) @binding(5) var<storage, read> coverage: array<u32>;

// A straightened-canvas UV to the source texture's UV: the canvas offset from
// the center, turned in pixel space. The crop is in canvas UV; sampling,
// touch-ups and masks are in source UV. Must match `develop::Straighten`.
fn straightenUv(uv: vec2<f32>) -> vec2<f32> {
    if (adj.straighten == 0.0) {
        return uv;
    }
    let px = vec2<f32>(1.0 / adj.texel_w, 1.0 / adj.texel_h);
    let d = (uv - vec2<f32>(0.5, 0.5)) * px;
    let c = cos(adj.straighten);
    let s = sin(adj.straighten);
    let r = vec2<f32>(c * d.x - s * d.y, s * d.x + c * d.y);
    return r / px + vec2<f32>(0.5, 0.5);
}

// Where channel scale `k` reads for source `uv`: a radial magnification about
// the center. Must match `develop::CaScale::source_uv`.
fn caUv(uv: vec2<f32>, k: f32) -> vec2<f32> {
    return (uv - vec2<f32>(0.5, 0.5)) * (1.0 + k) + vec2<f32>(0.5, 0.5);
}

// The source texel at `uv` with chromatic aberration removed: red and blue
// read from their own scaled UVs. `ddx` and `ddy` are the screen derivatives
// of the canvas UV, before the turn: a turn keeps the footprint's size, and
// the hardware's own derivative of a turned UV picks a level of detail above
// 0 at 1:1, which blends in mip 1 and softens the photo.
fn sampleSrc(uv: vec2<f32>, ddx: vec2<f32>, ddy: vec2<f32>) -> vec4<f32> {
    let g = textureSampleGrad(tex, samp, uv, ddx, ddy);
    let r = textureSampleGrad(tex, samp, caUv(uv, adj.ca_red), ddx, ddy).r;
    let b = textureSampleGrad(tex, samp, caUv(uv, adj.ca_blue), ddx, ddy).b;
    return vec4<f32>(r, g.g, b, g.a);
}

// `sampleSrc` at mip 0, for taps in non-uniform control flow.
fn sampleSrcLevel(uv: vec2<f32>) -> vec4<f32> {
    let g = textureSampleLevel(tex, samp, uv, 0.0);
    if (adj.ca_red == 0.0 && adj.ca_blue == 0.0) {
        return g;
    }
    let r = textureSampleLevel(tex, samp, caUv(uv, adj.ca_red), 0.0).r;
    let b = textureSampleLevel(tex, samp, caUv(uv, adj.ca_blue), 0.0).b;
    return vec4<f32>(r, g.g, b, g.a);
}

// Blend each spot-heal patch over `rgb`, the pixel at source `uv`. The CPU
// picks each source patch and color correction; the GPU blends the feathered
// patches. The touch-up count is packed into `adj._pad0`. Must match
// `apply_touchups` in develop/image_ops.rs.
fn applyTouchups(uv: vec2<f32>, rgb: vec3<f32>) -> vec3<f32> {
    var r = rgb.r;
    var g = rgb.g;
    var b = rgb.b;
    let touch_count = u32(adj._pad0);
    for (var i = 0u; i < touch_count; i = i + 1u) {
        let t = touchups[i];
        let d = (uv - t.center_radius_feather.xy) /
            vec2<f32>(adj.texel_w, adj.texel_h);
        // Radii are fractions of the image's shorter side, as on the CPU.
        let radius_px = t.center_radius_feather.z / max(adj.texel_w, adj.texel_h);
        let distance_px = length(d);
        if (distance_px < radius_px) {
            let feather_px = max(radius_px * clamp(t.center_radius_feather.w, 0.02, 1.0), 1.0);
            var mask = clamp((radius_px - distance_px) / feather_px, 0.0, 1.0);
            mask = mask * mask * (3.0 - 2.0 * mask) * t.opacity;
            let source_uv = t.source + (uv - t.center_radius_feather.xy);
            let source = sampleSrcLevel(source_uv).rgb + t.delta.xyz;
            r = r * (1.0 - mask) + clamp(source.r, 0.0, 1.0) * mask;
            g = g * (1.0 - mask) + clamp(source.g, 0.0, 1.0) * mask;
            b = b * (1.0 - mask) + clamp(source.b, 0.0, 1.0) * mask;
        }
    }
    return vec3<f32>(r, g, b);
}

// Gamma-2.2 Rec.709 luma of a linear pixel. Must match
// `develop::base::luma_gamma`.
fn lumaGamma(rgb: vec3<f32>) -> f32 {
    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    return pow(max(luma, 0.0), 1.0 / 2.2);
}

// log2 of a pixel's luma over its local base, both gamma 2.2. Must match
// `develop::base::log2_detail`.
fn log2Detail(pre: vec3<f32>, base: f32) -> f32 {
    let eps = 1.0 / 255.0;
    return log2((lumaGamma(pre) + eps) / (base + eps));
}

struct BaseTexel {
    mid: f32,
    wide: f32,
    dark: f32,
};

// A layer grid `t` at source `uv`: the four cells around `uv * grid - 0.5`,
// clamped at the edge, blended in f32. `textureLoad` throughout, because
// Rgba32Float doesn't filter and the CPU can't model hardware weights. Must
// match `BaseLayer::at_uv` in develop/base.rs.
fn fieldAt(t: texture_2d<f32>, uv: vec2<f32>) -> vec3<f32> {
    let size = vec2<i32>(textureDimensions(t));
    let p = uv * vec2<f32>(size) - vec2<f32>(0.5);
    let f = floor(p);
    let w = p - f;
    let hi = size - vec2<i32>(1);
    let i0 = clamp(vec2<i32>(f), vec2<i32>(0), hi);
    let i1 = clamp(vec2<i32>(f) + vec2<i32>(1), vec2<i32>(0), hi);
    let a = textureLoad(t, vec2<i32>(i0.x, i0.y), 0).rgb;
    let b = textureLoad(t, vec2<i32>(i1.x, i0.y), 0).rgb;
    let c = textureLoad(t, vec2<i32>(i0.x, i1.y), 0).rgb;
    let d = textureLoad(t, vec2<i32>(i1.x, i1.y), 0).rgb;
    let top = a * (1.0 - w.x) + b * w.x;
    let bottom = c * (1.0 - w.x) + d * w.x;
    return top * (1.0 - w.y) + bottom * w.y;
}

fn baseAt(uv: vec2<f32>) -> BaseTexel {
    let v = fieldAt(base_tex, uv);
    return BaseTexel(v.r, v.g, v.b);
}

// The base layer as a gray pixel in place of the photo, for the match test:
// 1 `mid`, 2 `wide`, 3 `dark` (through gamma 2.2 like the others), 4 the
// detail `0.5 + 8 * (lumaGamma(pre) - mid)`, whose gain makes a half-cell
// shift or a wrong weight show, 5 the fine layer's `mid`. The rest of the
// pipeline treats the value as a linear pixel. Must match `BaseProbe::apply`
// in develop/base.rs.
fn baseProbe(uv: vec2<f32>, pre: vec3<f32>, mode: f32) -> vec3<f32> {
    let base = baseAt(uv);
    let m = u32(mode);
    if (m == 1u) {
        return vec3<f32>(base.mid);
    }
    if (m == 2u) {
        return vec3<f32>(base.wide);
    }
    if (m == 3u) {
        return vec3<f32>(pow(max(base.dark, 0.0), 1.0 / 2.2));
    }
    if (m == 5u) {
        return vec3<f32>(fieldAt(fine_tex, uv).r);
    }
    return vec3<f32>(0.5 + 8.0 * (lumaGamma(pre) - base.mid));
}

// Texture, Clarity and Dehaze on `pre`, the linear pre-tone pixel at source
// `uv`. Must match `local::apply` in develop/local.rs.
fn localAdjust(uv: vec2<f32>, pre: vec3<f32>) -> vec3<f32> {
    let texture = adj.texture / 100.0;
    let clarity = adj.clarity / 100.0;
    let dehaze = adj.dehaze / 100.0;
    var out = pre;
    var gain = 0.0;
    if (clarity != 0.0 || dehaze != 0.0) {
        let base = baseAt(uv);
        if (clarity != 0.0) {
            let d = clamp(log2Detail(pre, base.mid), -1.5, 1.5);
            let m = clamp(base.mid, 0.0, 1.0);
            let weight = 1.0 - (2.0 * m - 1.0) * (2.0 * m - 1.0);
            gain = gain + clarity * d * weight;
        }
        if (dehaze > 0.0) {
            let darkest = max(min(out.r, min(out.g, out.b)), 0.0);
            let veil = min(0.8 * dehaze * clamp(base.dark, 0.0, 1.0), darkest);
            let t = max(1.0 - veil, 0.2);
            out = (out - vec3<f32>(veil)) / t;
        } else if (dehaze < 0.0) {
            let k = -dehaze * 0.5;
            out = out + (vec3<f32>(0.7) - out) * k;
        }
    }
    if (texture != 0.0) {
        let fine = fieldAt(fine_tex, uv).r;
        gain = gain + texture * clamp(log2Detail(pre, fine), -1.5, 1.5);
    }
    if (gain != 0.0) {
        out = out * exp2(2.2 * gain);
    }
    return out;
}

// Gaussian weights for Sharpening's 5x5 blur, by squared grid distance.
// Must match `tap_weight` in develop/sharpen.rs.
fn sharpenWeight(d2: i32) -> f32 {
    if (d2 == 0) { return 1.0; }
    if (d2 == 1) { return 0.7548403; }
    if (d2 == 2) { return 0.5697828; }
    if (d2 == 4) { return 0.3246525; }
    if (d2 == 5) { return 0.2450614; }
    if (d2 == 8) { return 0.1053992; }
    return 0.0;
}

// Sharpening on `out`, the pixel at source `uv` after the other local edits,
// against a blur of the pre-tone source around it: taps `sharp_step` texels
// apart with CA removed and the touch-ups blended. Must match
// `Sharpen::apply` and `sharpen::blur` as `bake_edited` feeds them.
fn sharpenAdjust(uv: vec2<f32>, pre: vec3<f32>, out: vec3<f32>) -> vec3<f32> {
    if (adj.sharp_gain == 0.0) {
        return out;
    }
    // The pixel's luma from mip 0, like the taps: zoomed out, `pre` is a mip
    // average, and comparing it with a mip-0 blur would subtract detail
    // rather than add it. Denoise already read mip 0.
    var center = pre;
    if (adj.denoise == 0.0) {
        center = applyTouchups(uv, sampleSrcLevel(uv).rgb);
    }
    var sum = vec3<f32>(0.0);
    var wsum = 0.0;
    let step = adj.sharp_step * vec2<f32>(adj.texel_w, adj.texel_h);
    for (var dy = -2; dy <= 2; dy = dy + 1) {
        for (var dx = -2; dx <= 2; dx = dx + 1) {
            let tapUv = uv + vec2<f32>(f32(dx), f32(dy)) * step;
            let tap = applyTouchups(tapUv, sampleSrcLevel(tapUv).rgb);
            let w = sharpenWeight(dx * dx + dy * dy);
            sum = sum + w * tap;
            wsum = wsum + w;
        }
    }
    let l = lumaGamma(center);
    let diff = l - lumaGamma(sum / wsum);
    var d = clamp(diff, -adj.sharp_limit, adj.sharp_limit);
    if (adj.sharp_mask > 0.0) {
        let t = clamp((abs(diff) - adj.sharp_mask * 0.5) / (adj.sharp_mask * 0.5), 0.0, 1.0);
        d = d * t * t * (3.0 - 2.0 * t);
    }
    let goal = max(l + adj.sharp_gain * d, 0.0);
    let eps = 1.0 / 255.0;
    return out * pow((goal + eps) / (l + eps), 2.2);
}

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// The Exposure slider in linear light. Darkening is a plain gain, which
// recovers above-white RAW highlights. Brightening maps luma through the
// standard rational tone curve x*g / (1 + x*(g - 1)), which keeps black at
// black and white at white, and colors keep 1 / (1 + x*(g - 1)) of their
// chroma relative to luma. Must match `exposure_curve` in develop/mod.rs.
fn exposureCurve(rgb: vec3<f32>, stops: f32) -> vec3<f32> {
    let gain = exp2(stops);
    if (stops <= 0.0) {
        return rgb * gain;
    }
    // Rec.709 luma weights for linear light. The gamma-space vibrance block
    // uses different weights.
    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    let d = 1.0 + max(luma, 0.0) * (gain - 1.0);
    let newLuma = luma * gain / d;
    let chroma = gain / (d * d);
    return vec3<f32>(newLuma) + (rgb - vec3<f32>(luma)) * chroma;
}

// 1 inside `1 - feather` of the radius, 0 at and past it, smoothstep
// between. Must match `falloff` in develop/mask.rs.
fn maskFalloff(d: f32, feather: f32) -> f32 {
    if (d >= 1.0) {
        return 0.0;
    }
    if (d <= 1.0 - feather) {
        return 1.0;
    }
    let t = (1.0 - d) / feather;
    return t * t * (3.0 - 2.0 * t);
}

// Texel (x, y) of the coverage that starts at byte `off` and is `w` wide.
fn coverageTexel(off: u32, w: u32, x: u32, y: u32) -> f32 {
    let i = off + y * w + x;
    let word = coverage[i >> 2u];
    return f32((word >> ((i & 3u) * 8u)) & 0xffu) / 255.0;
}

// Bilinear coverage of coverage mask `m` at source `uv`: the four texels
// around `uv * size - 0.5`, clamped at the edge, blended as `fieldAt`
// blends. Must match `Coverage::at_uv` in develop/coverage.rs.
fn coverageAt(m: Mask, uv: vec2<f32>) -> f32 {
    let off = m.cov.x;
    let size = vec2<i32>(i32(m.cov.y), i32(m.cov.z));
    let p = uv * vec2<f32>(size) - vec2<f32>(0.5);
    let f = floor(p);
    let t = p - f;
    let hi = size - vec2<i32>(1);
    let i0 = vec2<u32>(clamp(vec2<i32>(f), vec2<i32>(0), hi));
    let i1 = vec2<u32>(clamp(vec2<i32>(f) + vec2<i32>(1), vec2<i32>(0), hi));
    let w = u32(size.x);
    let a = coverageTexel(off, w, i0.x, i0.y);
    let b = coverageTexel(off, w, i1.x, i0.y);
    let c = coverageTexel(off, w, i0.x, i1.y);
    let d = coverageTexel(off, w, i1.x, i1.y);
    let top = a * (1.0 - t.x) + b * t.x;
    let bottom = c * (1.0 - t.x) + d * t.x;
    return top * (1.0 - t.y) + bottom * t.y;
}

// How much of mask `m` applies at source `uv`. Lengths are in units of the
// shorter side. Must match `mask_weight` in develop/mask.rs.
fn maskWeight(m: Mask, uv: vec2<f32>) -> f32 {
    let aspect = adj.texel_h / adj.texel_w;
    var scale = vec2<f32>(1.0, 1.0 / aspect);
    if (aspect >= 1.0) {
        scale = vec2<f32>(aspect, 1.0);
    }
    var w = 0.0;
    let kind = u32(m.params.x);
    if (kind == 2u) {
        // A detected mask with no coverage yet arrives with size 0: it
        // covers nothing, inverted or not, like a zero-length gradient.
        if (m.cov.y == 0u || m.cov.z == 0u) {
            return 0.0;
        }
        w = coverageAt(m, uv);
    } else if (kind == 0u) {
        let dir = (m.geom.zw - m.geom.xy) * scale;
        let len2 = dot(dir, dir);
        // A gradient with no length covers nothing, inverted or not.
        if (len2 < 1e-12) {
            return 0.0;
        }
        let t = clamp(dot((uv - m.geom.xy) * scale, dir) / len2, 0.0, 1.0);
        w = 1.0 - t * t * (3.0 - 2.0 * t);
    } else {
        let q = (uv - m.geom.xy) * scale;
        let s = sin(m.params.z);
        let c = cos(m.params.z);
        let x = (c * q.x + s * q.y) / m.geom.z;
        let y = (c * q.y - s * q.x) / m.geom.w;
        w = maskFalloff(length(vec2<f32>(x, y)), m.params.w);
    }
    if (m.params.y != 0.0) {
        return 1.0 - w;
    }
    return w;
}

// The global sliders plus every mask's weighted amounts at source `uv`, each
// held to its range. Must match `mask::local_tone` and `LocalAmounts::of` in
// develop.
fn localTone(uv: vec2<f32>) -> LocalTone {
    var l = LocalTone(adj.exposure, adj.contrast, adj.highlights, adj.shadows,
        adj.whites, adj.blacks, adj.temp, adj.tint, adj.saturation);
    let n = u32(adj.mask_count);
    if (n == 0u) {
        return l;
    }
    var a0 = vec4<f32>(l.exposure, l.contrast, l.highlights, l.shadows);
    var a1 = vec4<f32>(l.whites, l.blacks, l.temp, l.tint);
    var sat = l.saturation;
    for (var i = 0u; i < n; i = i + 1u) {
        let m = masks[i];
        let w = maskWeight(m, uv);
        if (w > 0.0) {
            a0 = a0 + m.a0 * w;
            a1 = a1 + m.a1 * w;
            sat = sat + m.a2.x * w;
        }
    }
    a0 = clamp(a0, vec4<f32>(-5.0, -100.0, -100.0, -100.0), vec4<f32>(5.0, 100.0, 100.0, 100.0));
    a1 = clamp(a1, vec4<f32>(-100.0), vec4<f32>(100.0));
    return LocalTone(a0.x, a0.y, a0.z, a0.w, a1.x, a1.y, a1.z, a1.w, clamp(sat, -100.0, 100.0));
}

// Gamma-space tone ops, per channel, with `l`'s sliders. Must match the
// `tone` closure in `apply_linear` in develop/mod.rs.
fn tone(v: f32, l: LocalTone) -> f32 {
    var x = v;

    // Blacks and whites move the endpoints by up to 0.2.
    let blacks = l.blacks / 100.0 * 0.2;
    let whites = l.whites / 100.0 * 0.2;
    // Positive whites brighten and clip the top end, as in Lightroom.
    x = (x + blacks) / ((1.0 - whites) + blacks);

    // Contrast: an S-curve around mid-gray, strength up to 0.5.
    let c = l.contrast / 100.0 * 0.5;
    if (c != 0.0) {
        let d = x - 0.5;
        x = 0.5 + d + c * d * (1.0 - 4.0 * d * d);
    }

    // Shadows: masked to act near black.
    let s = l.shadows / 100.0 * 0.3;
    if (s != 0.0) {
        let mask = pow(1.0 - clamp(x, 0.0, 1.0), 2.0);
        x = x + s * mask;
    }

    // Highlights: masked to act near white.
    let h = l.highlights / 100.0 * 0.3;
    if (h != 0.0) {
        let mask = pow(clamp(x, 0.0, 1.0), 2.0);
        x = x + h * mask;
    }

    return x;
}

// The point curve on one gamma-space pixel, interpolating between table
// entries. Must match `curve::apply` in curve.rs.
fn toneCurve(c: vec3<f32>) -> vec3<f32> {
    if (adj.curve_on == 0.0) {
        return c;
    }
    let x = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)) * 255.0;
    let i = min(vec3<u32>(x), vec3<u32>(254u));
    let t = x - vec3<f32>(i);
    return vec3<f32>(
        mix(curve_lut[i.r].r, curve_lut[i.r + 1u].r, t.r),
        mix(curve_lut[i.g].g, curve_lut[i.g + 1u].g, t.g),
        mix(curve_lut[i.b].b, curve_lut[i.b + 1u].b, t.b),
    );
}

// A cube root through `pow` on the magnitude. Must match `color::cbrt`.
fn cbrtSigned(v: f32) -> f32 {
    return sign(v) * pow(abs(v), 1.0 / 3.0);
}

// Linear sRGB to OkLab. Must match `color::oklab` in develop/color.rs.
fn oklab(c: vec3<f32>) -> vec3<f32> {
    let l = cbrtSigned(0.41222147 * c.r + 0.53633255 * c.g + 0.051445995 * c.b);
    let m = cbrtSigned(0.2119035 * c.r + 0.6806995 * c.g + 0.10739696 * c.b);
    let s = cbrtSigned(0.08830246 * c.r + 0.28171885 * c.g + 0.6299787 * c.b);
    return vec3<f32>(
        0.21045426 * l + 0.7936178 * m - 0.004072047 * s,
        1.9779985 * l - 2.4285922 * m + 0.4505937 * s,
        0.025904037 * l + 0.78277177 * m - 0.80867577 * s,
    );
}

// OkLab to linear sRGB. Must match `color::oklab_to_linear`.
fn oklabToLinear(lab: vec3<f32>) -> vec3<f32> {
    let l0 = lab.x + 0.39633778 * lab.y + 0.21580376 * lab.z;
    let m0 = lab.x - 0.105561346 * lab.y - 0.06385417 * lab.z;
    let s0 = lab.x - 0.08948418 * lab.y - 1.2914855 * lab.z;
    let l = l0 * l0 * l0;
    let m = m0 * m0 * m0;
    let s = s0 * s0 * s0;
    return vec3<f32>(
        4.0767417 * l - 3.3077116 * m + 0.23096994 * s,
        -1.268438 * l + 2.6097574 * m - 0.34131938 * s,
        -0.0041960863 * l - 0.7034186 * m + 1.7076147 * s,
    );
}

// `x` into 0..TAU. Must match `color::wrap_tau`.
fn wrapTau(x: f32) -> f32 {
    let tau = 6.2831855;
    return x - tau * floor(x / tau);
}

// Band `i` of one of the Color Mixer's per-band arrays.
fn bandHue(i: u32) -> f32 {
    return adj.band_hue[i / 4u][i % 4u];
}

fn mixHue(i: u32) -> f32 {
    return adj.mix_hue[i / 4u][i % 4u];
}

fn mixSat(i: u32) -> f32 {
    return adj.mix_sat[i / 4u][i % 4u];
}

fn mixLum(i: u32) -> f32 {
    return adj.mix_lum[i / 4u][i % 4u];
}

fn grayMix(i: u32) -> f32 {
    return adj.gray[i / 4u][i % 4u];
}

// The Color Mixer, or the B&W mix, on gamma-space `rgb`, in OkLCh: each
// pixel takes a raised-cosine blend of the two bands its hue sits between.
// Must match `ColorOps::apply` in develop/color.rs.
fn colorMix(rgb: vec3<f32>) -> vec3<f32> {
    if (adj.color_on == 0.0) {
        return rgb;
    }
    let lab = oklab(pow(max(rgb, vec3<f32>(0.0)), vec3<f32>(2.2)));
    let c = sqrt(lab.y * lab.y + lab.z * lab.z);
    // atan2(0, 0) is undefined on some backends. Must match the CPU's
    // early return.
    if (c < 1e-6) {
        return rgb;
    }
    let h = atan2(lab.z, lab.y);
    var i = 0u;
    var s = 0.0;
    for (var k = 0u; k < 8u; k = k + 1u) {
        let a = bandHue(k);
        let span = wrapTau(bandHue((k + 1u) % 8u) - a);
        let d = wrapTau(h - a);
        if (d < span) {
            i = k;
            s = 0.5 - 0.5 * cos(3.1415927 * (d / span));
            break;
        }
    }
    let j = (i + 1u) % 8u;
    let cw = min(c / 0.12, 1.0);
    let h2 = h + (mixHue(i) + (mixHue(j) - mixHue(i)) * s) * cw;
    let c2 = c * max(1.0 + (mixSat(i) + (mixSat(j) - mixSat(i)) * s), 0.0);
    let lum = (mixLum(i) + (mixLum(j) - mixLum(i)) * s) * sqrt(max(lab.x, 0.05));
    let gray = grayMix(i) + (grayMix(j) - grayMix(i)) * s;
    let l2 = lab.x + (lum + gray) * cw;
    let lin = oklabToLinear(vec3<f32>(l2, c2 * cos(h2), c2 * sin(h2)));
    return pow(max(lin, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.2));
}

// Must match `grade::smooth` in develop/grade.rs.
fn smoothRamp(e0: f32, e1: f32, x: f32) -> f32 {
    let t = clamp((x - e0) / (e1 - e0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

// Color Grading on gamma-space `rgb`, in OkLab: each wheel adds its tint
// and lightness, weighted by the pixel's lightness for the three ranges and
// in full for the global wheel. Must match `GradeOps::apply` in
// develop/grade.rs.
fn colorGrade(rgb: vec3<f32>) -> vec3<f32> {
    if (adj.grade_on == 0.0) {
        return rgb;
    }
    var lab = oklab(pow(max(rgb, vec3<f32>(0.0)), vec3<f32>(2.2)));
    let m = adj.grade_mid;
    let w = adj.grade_width;
    let ws = 1.0 - smoothRamp(m - w, m + w * 0.25, lab.x);
    let wh = smoothRamp(m - w * 0.25, m + w, lab.x);
    let wm = max(1.0 - ws - wh, 0.0);
    let weights = vec4<f32>(ws, wm, wh, 1.0);
    for (var k = 0u; k < 4u; k = k + 1u) {
        let wheel = adj.grade_wheels[k];
        lab = vec3<f32>(lab.x + wheel.z * weights[k], lab.y + wheel.x * weights[k], lab.z + wheel.y * weights[k]);
    }
    return pow(max(oklabToLinear(lab), vec3<f32>(0.0)), vec3<f32>(1.0 / 2.2));
}

// 0 inside the vignette, 1 past its falloff, at canvas `uv`. Must match
// `Effects::vignette_mask` in develop/effects.rs.
fn vignetteMask(uv: vec2<f32>) -> f32 {
    let n = adj.vig_exponent;
    let p = max(abs((uv - adj.vig_center) / adj.vig_axis), vec2<f32>(1e-6));
    var d = sqrt(p.x * p.x + p.y * p.y);
    if (n != 2.0) {
        d = pow(pow(p.x, n) + pow(p.y, n), 1.0 / n);
    }
    let t = clamp((d - adj.vig_lo) / (adj.vig_hi - adj.vig_lo), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

// PCG's output hash. Must match `pcg_hash` in develop/effects.rs.
fn pcgHash(v: u32) -> u32 {
    let state = v * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

// A value in -1..1 for lattice point `i` of the octave salted `salt`. Must
// match `lattice` in develop/effects.rs.
fn lattice(i: vec2<i32>, salt: u32) -> f32 {
    let h = pcgHash(bitcast<u32>(i.x) ^ pcgHash(bitcast<u32>(i.y) ^ salt));
    return f32(h >> 8u) * (2.0 / 16777215.0) - 1.0;
}

// Smooth value noise at lattice coordinates `p`. Must match `value_noise`
// in develop/effects.rs.
fn valueNoise(p: vec2<f32>, salt: u32) -> f32 {
    let f = floor(p);
    let i = vec2<i32>(f);
    let t = p - f;
    let s = t * t * (3.0 - 2.0 * t);
    let a = lattice(i, salt);
    let b = lattice(i + vec2<i32>(1, 0), salt);
    let c = lattice(i + vec2<i32>(0, 1), salt);
    let d = lattice(i + vec2<i32>(1, 1), salt);
    let top = a + (b - a) * s.x;
    let bottom = c + (d - c) * s.x;
    return top + (bottom - top) * s.y;
}

// Two octaves of grain at canvas `uv`, each faded by `fp`, the cells one
// screen pixel covers. Must match `Effects::grain_noise`.
fn grainNoise(uv: vec2<f32>, fp: f32) -> f32 {
    let p = uv * adj.grain_scale;
    let octave = 2.31;
    let n1 = valueNoise(p, 0u) / max(fp, 1.0);
    let r = adj.grain_rough;
    if (r == 0.0) {
        return n1;
    }
    // The salt must match `OCTAVE_SALT`.
    let n2 = valueNoise(p * octave, 0x9e3779b9u) / max(fp * octave, 1.0);
    return (n1 + r * n2) / sqrt(1.0 + r * r);
}

// Vignette, then Grain, on gamma-space `c` at canvas `uv`. `footprint` is one
// screen pixel's size in canvas UV along x and y. Must match
// `Effects::apply` in develop/effects.rs.
fn effects(rgb: vec3<f32>, uv: vec2<f32>, footprint: vec2<f32>) -> vec3<f32> {
    var c = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    if (adj.vig_amount != 0.0) {
        let m = vignetteMask(uv);
        let dark = 1.0 + min(adj.vig_amount, 0.0) * m;
        let light = max(adj.vig_amount, 0.0) * m;
        c = c * dark + (vec3<f32>(1.0) - c) * light;
    }
    if (adj.grain_amount != 0.0) {
        let cells = footprint * adj.grain_scale;
        let n = grainNoise(uv, max(cells.x, cells.y));
        let luma = clamp(0.299 * c.r + 0.587 * c.g + 0.114 * c.b, 0.0, 1.0);
        let weight = 0.25 + 3.0 * luma * (1.0 - luma);
        c = c + vec3<f32>(adj.grain_amount * n * weight);
    }
    return c;
}

// One screen pixel's size in canvas UV along x and y, from the canvas UV's
// screen derivatives. Any turn of the view keeps it right.
fn footprintOf(ddx: vec2<f32>, ddy: vec2<f32>) -> vec2<f32> {
    return max(abs(ddx), abs(ddy));
}

// Gaussian (sigma 1.0) weights for the 5x5 denoise kernel, by squared tap
// distance. Must match `spatial_weight` in develop/mod.rs.
fn spatialWeight(d2: i32) -> f32 {
    if (d2 == 0) { return 1.0; }
    if (d2 == 1) { return 0.606531; }
    if (d2 == 2) { return 0.367879; }
    if (d2 == 4) { return 0.135335; }
    if (d2 == 5) { return 0.082085; }
    if (d2 == 8) { return 0.018316; }
    return 0.0;
}

// Linear to sRGB-encoded, for the non-sRGB surface. Must match
// `rawler::imgop::srgb::srgb_apply_gamma`, used by the CPU LUT paths.
fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let cutoff = vec3<f32>(0.0031308);
    let a = vec3<f32>(0.055);
    let higher = (1.0 + a) * pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - a;
    let lower = c * 12.92;
    return select(higher, lower, c <= cutoff);
}

// The neutral gray outside the image or the crop, sRGB-encoded.
const BACKGROUND: vec4<f32> = vec4<f32>(0.3811, 0.3811, 0.3959, 1.0);
