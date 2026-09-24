// SPDX-License-Identifier: GPL-3.0-or-later

// Declarations shared by the two loupe fragment shaders. Rust prepends this
// file to `shader.wgsl` and `raw/raw_shader.wgsl`, so each is one module.

// Field order must match `GpuAdjust` in develop.rs.
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
    _pad1: f32,
    _pad2: f32,
};

struct TouchUp {
    center_radius_feather: vec4<f32>,
    source: vec2<f32>,
    _pad: vec2<f32>,
    delta: vec4<f32>,
};

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@group(2) @binding(0) var<uniform> adj: Adjust;
@group(3) @binding(0) var<storage, read> touchups: array<TouchUp>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Must match `filmic_exposure` in develop.rs. MIX is the share of the
// adjustment that goes through the curve, MIDTONE is how hard it bends per
// stop, and ANCHOR is the curve's fixed point. ANCHOR sits just above white so
// a 1.0 pixel still moves instead of being pinned.
const FILMIC_MIX: f32 = 0.95;
const FILMIC_MIDTONE: f32 = 1.2;
const FILMIC_ANCHOR: f32 = 1.06;

// Exposure in linear light. Positive stops bend luma through a curve that
// rolls into white instead of clipping, and colors fade toward white as they
// brighten. Must match `filmic_exposure` in develop.rs.
fn filmicExposure(rgb: vec3<f32>, stops: f32) -> vec3<f32> {
    if (stops == 0.0) {
        return rgb;
    }

    // Negative stops are a plain gain, which recovers above-white RAW
    // highlights.
    if (stops < 0.0) {
        return rgb * exp2(stops);
    }

    // Rec.709 luma weights for linear light. The gamma-space vibrance block
    // uses different weights.
    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    if (abs(luma) < 1e-5) {
        return rgb;
    }

    let scale = exp2(stops * (1.0 - FILMIC_MIX));
    // Curve strength: k == 1 is the identity, k < 1 lifts, k > 1 drops.
    let k = exp2(-stops * FILMIC_MIX * FILMIC_MIDTONE);

    // Past the anchor the curve tiles, so RAW values above white keep being
    // shaped instead of saturating. Srgb8 input never gets there.
    let la = abs(luma);
    let base = floor(la / FILMIC_ANCHOR) * FILMIC_ANCHOR;
    let norm = (la - base) / FILMIC_ANCHOR;
    let shaped = norm / (norm + (1.0 - norm) * k);
    let newLuma = sign(luma) * (base + shaped * FILMIC_ANCHOR) * scale;

    // Chroma grows more slowly than luma, and slower still near white.
    let lumaScale = max(newLuma / luma, 0.0);
    let w = clamp(newLuma, 0.0, 2.0) * 0.5;
    let dynExp = mix(0.95, 0.65, w);
    // Fade in highlight desaturation continuously from the identity at zero.
    let rolloff = 1.0 / (1.0 + max(newLuma - 0.9, 0.0) * 2.0 * min(stops, 1.0));
    let chromaScale = pow(lumaScale, dynExp) * rolloff;

    return vec3<f32>(newLuma) + (rgb - vec3<f32>(luma)) * chromaScale;
}

// Gamma-space tone ops, per channel. Must match the `tone` closure in
// `apply_linear` in develop.rs.
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

// Gaussian (sigma 1.0) weights for the 5x5 denoise kernel, by squared tap
// distance. Must match `spatial_weight` in develop.rs.
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
