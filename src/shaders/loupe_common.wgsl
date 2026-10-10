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
};

struct TouchUp {
    center_radius_feather: vec4<f32>,
    source: vec2<f32>,
    opacity: f32,
    _pad: f32,
    delta: vec4<f32>,
};

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
// The base layer: one texel per grid cell, Rgba32Float, (mid, wide, dark).
// base.wgsl builds it. Read only through `baseAt`.
@group(0) @binding(2) var base_tex: texture_2d<f32>;
@group(2) @binding(0) var<uniform> adj: Adjust;
// The baked point curve: entry i is the red, green and blue output for input
// i / 255. Written from `GpuCurve` in develop/mod.rs.
@group(2) @binding(1) var<uniform> curve_lut: array<vec4<f32>, 256>;
@group(3) @binding(0) var<storage, read> touchups: array<TouchUp>;

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

// The base layer at source `uv`: the four cells around `uv * grid - 0.5`,
// clamped at the edge, blended in f32. `textureLoad` throughout, because
// Rgba32Float doesn't filter and the CPU can't model hardware weights. Must
// match `BaseLayer::at_uv` in develop/base.rs.
fn baseAt(uv: vec2<f32>) -> BaseTexel {
    let size = vec2<i32>(textureDimensions(base_tex));
    let p = uv * vec2<f32>(size) - vec2<f32>(0.5);
    let f = floor(p);
    let t = p - f;
    let hi = size - vec2<i32>(1);
    let i0 = clamp(vec2<i32>(f), vec2<i32>(0), hi);
    let i1 = clamp(vec2<i32>(f) + vec2<i32>(1), vec2<i32>(0), hi);
    let a = textureLoad(base_tex, vec2<i32>(i0.x, i0.y), 0).rgb;
    let b = textureLoad(base_tex, vec2<i32>(i1.x, i0.y), 0).rgb;
    let c = textureLoad(base_tex, vec2<i32>(i0.x, i1.y), 0).rgb;
    let d = textureLoad(base_tex, vec2<i32>(i1.x, i1.y), 0).rgb;
    let top = a * (1.0 - t.x) + b * t.x;
    let bottom = c * (1.0 - t.x) + d * t.x;
    let v = top * (1.0 - t.y) + bottom * t.y;
    return BaseTexel(v.r, v.g, v.b);
}

// The base layer as a gray pixel in place of the photo, for the match test:
// 1 `mid`, 2 `wide`, 3 `dark` (through gamma 2.2 like the others), 4 the
// detail `0.5 + 8 * (lumaGamma(pre) - mid)`, whose gain makes a half-cell
// shift or a wrong weight show. The rest of the pipeline treats the value as
// a linear pixel. Must match `base_probe` in develop/image_ops.rs.
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
    return vec3<f32>(0.5 + 8.0 * (lumaGamma(pre) - base.mid));
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

// Gamma-space tone ops, per channel. Must match the `tone` closure in
// `apply_linear` in develop/mod.rs.
fn tone(v: f32) -> f32 {
    var x = v;

    // Blacks and whites move the endpoints by up to 0.2.
    let blacks = adj.blacks / 100.0 * 0.2;
    let whites = adj.whites / 100.0 * 0.2;
    // Positive whites brighten and clip the top end, as in Lightroom.
    x = (x + blacks) / ((1.0 - whites) + blacks);

    // Contrast: an S-curve around mid-gray, strength up to 0.5.
    let c = adj.contrast / 100.0 * 0.5;
    if (c != 0.0) {
        let d = x - 0.5;
        x = 0.5 + d + c * d * (1.0 - 4.0 * d * d);
    }

    // Shadows: masked to act near black.
    let s = adj.shadows / 100.0 * 0.3;
    if (s != 0.0) {
        let mask = pow(1.0 - clamp(x, 0.0, 1.0), 2.0);
        x = x + s * mask;
    }

    // Highlights: masked to act near white.
    let h = adj.highlights / 100.0 * 0.3;
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
