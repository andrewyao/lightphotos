// SPDX-License-Identifier: GPL-3.0-or-later

// Fragment shader for linear-light RAW images (`PixelFormat::LinearF16`, the
// wasm32 Loupe path). The CPU demosaics to linear camera RGB and this shader
// does the rest: sRGB gamma, the RAW display boost, and the Develop sliders.
// `apply_raw_preview_boost` in `raw/nonmac_decode.rs` is the CPU twin that
// every other RAW path bakes into u8 sRGB at decode time.
//
// Order: denoise -> touch-ups -> WB -> exposure -> sRGB gamma + boost ->
// tone -> vibrance/saturation -> clamp.
//
// Two differences from `shader.wgsl`:
// 1. The working gamma is the real sRGB curve plus the display boost, not
//    `pow(x, 1/2.2)`. With every slider at default the output equals the
//    plain gamma + boost result.
// 2. No conversion back to linear at the end. WebGPU canvases never offer an
//    sRGB surface format, so nothing re-encodes on store and this output is
//    already display-ready.

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Field order must match `GpuAdjust` in develop.rs and `Adjust` in shader.wgsl.
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
// Group 1 (pan/zoom) is bound but only read by the shared `vs_main`.

// Linear to sRGB-encoded. Must match `rawler::imgop::srgb::srgb_apply_gamma`
// (used by the CPU LUT paths) and `linear_to_srgb` in shader.wgsl.
fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let cutoff = vec3<f32>(0.0031308);
    let a = vec3<f32>(0.055);
    let higher = (1.0 + a) * pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - a;
    let lower = c * 12.92;
    return select(higher, lower, c <= cutoff);
}

// RAW display boost: brightens and adds contrast on top of the sRGB curve so
// RAW files look less flat. Formula and constants must match
// `apply_raw_preview_boost` in raw/nonmac_decode.rs.
const RAW_PREVIEW_BRIGHTNESS_GAMMA: f32 = 1.1;
const RAW_PREVIEW_CONTRAST_MIX: f32 = 0.75;

fn apply_raw_preview_boost(v: f32) -> f32 {
    let brightened = pow(clamp(v, 0.0, 1.0), 1.0 / RAW_PREVIEW_BRIGHTNESS_GAMMA);
    let contrast_curve = brightened * brightened * (3.0 - 2.0 * brightened);
    return clamp(brightened + (contrast_curve - brightened) * RAW_PREVIEW_CONTRAST_MIX, 0.0, 1.0);
}

// Must match `filmic_exposure` in develop.rs. MIX is the share of the change
// routed through the curve, MIDTONE is how hard it bends per stop, and ANCHOR
// is the curve's fixed point, just above white so a 1.0 pixel still moves.
const FILMIC_MIX: f32 = 0.95;
const FILMIC_MIDTONE: f32 = 1.2;
const FILMIC_ANCHOR: f32 = 1.06;

// Exposure in linear light. Luma goes through a rational curve so brightening
// rolls into white instead of clipping, and chroma grows slower so colors go
// pale. Must match `filmic_exposure` in develop.rs and shader.wgsl.
fn filmicExposure(rgb: vec3<f32>, stops: f32) -> vec3<f32> {
    if (stops == 0.0) {
        return rgb;
    }

    // Scale the entire RAW range to recover above-white highlights.
    if (stops < 0.0) {
        return rgb * exp2(stops);
    }

    // Rec.709 luma weights, because this runs in linear light.
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

// Per-channel tone ops in gamma space. Must match the `tone` closure in
// develop.rs `apply_linear` and `tone` in shader.wgsl.
fn tone(v: f32) -> f32 {
    var x = v;

    // Blacks/whites: shift the endpoints. ±100 → ±0.2 endpoint move.
    let blacks = adj.blacks / 100.0 * 0.2;
    let whites = adj.whites / 100.0 * 0.2;
    // Positive whites brightens the top end, as in Lightroom.
    x = (x + blacks) / ((1.0 - whites) + blacks);

    // Contrast: S-curve pivoting at mid-gray. ±100 → ±0.5 strength.
    let c = adj.contrast / 100.0 * 0.5;
    if (c != 0.0) {
        let d = x - 0.5;
        x = 0.5 + d + c * d * (1.0 - 4.0 * d * d);
    }

    // Shadows: luminance-masked lift/compress near black.
    let s = adj.shadows / 100.0 * 0.3;
    if (s != 0.0) {
        let mask = pow(1.0 - clamp(x, 0.0, 1.0), 2.0);
        x = x + s * mask;
    }

    // Highlights: luminance-masked lift/compress near white.
    let h = adj.highlights / 100.0 * 0.3;
    if (h != 0.0) {
        let mask = pow(clamp(x, 0.0, 1.0), 2.0);
        x = x + h * mask;
    }

    return x;
}

// σ=1.0 Gaussian weight for the 5×5 denoise kernel, keyed by squared tap
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

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Sample before any early return. WebGPU rejects an implicit-LOD sample
    // after non-uniform control flow. Rgba16Float has no sRGB decode, so this
    // is linear.
    let texel = textureSample(tex, samp, in.uv);

    // Outside the image (UV beyond 0..1): draw the neutral background.
    if (in.uv.x < 0.0 || in.uv.x > 1.0 || in.uv.y < 0.0 || in.uv.y > 1.0) {
        return vec4<f32>(0.12, 0.12, 0.13, 1.0);
    }
    // Outside the crop rectangle.
    if (in.uv.x < adj.crop_l || in.uv.x > adj.crop_r || in.uv.y < adj.crop_t || in.uv.y > adj.crop_b) {
        return vec4<f32>(0.12, 0.12, 0.13, 1.0);
    }

    var r = texel.r;
    var g = texel.g;
    var b = texel.b;

    // 0. Edge-aware denoise over a 5x5 neighborhood, weighted by distance and
    // color similarity. The loop is not uniform control flow, so taps use
    // textureSampleLevel at LOD 0; this skips mip antialiasing when zoomed
    // out. Must match `denoise_sample` in develop.rs.
    if (adj.denoise > 0.0) {
        let center = textureSampleLevel(tex, samp, in.uv, 0.0).rgb;
        let sigmaR = 0.02 + adj.denoise / 100.0 * 0.30;
        let sigmaR2 = sigmaR * sigmaR;
        var sum = vec3<f32>(0.0, 0.0, 0.0);
        var wsum = 0.0;
        for (var dy = -2; dy <= 2; dy = dy + 1) {
            for (var dx = -2; dx <= 2; dx = dx + 1) {
                let tapUv = in.uv + vec2<f32>(f32(dx), f32(dy)) * vec2<f32>(adj.texel_w, adj.texel_h);
                let tap = textureSampleLevel(tex, samp, tapUv, 0.0).rgb;
                let diff = tap - center;
                let diff2 = dot(diff, diff);
                // wsum can't be 0: (dx,dy)=(0,0) always contributes weight 1.0.
                let w = spatialWeight(dx * dx + dy * dy) / (1.0 + diff2 / sigmaR2);
                sum = sum + w * tap;
                wsum = wsum + w;
            }
        }
        let denoised = sum / wsum;
        r = denoised.x;
        g = denoised.y;
        b = denoised.z;
    }

    // Spot healing. The CPU picks source spots and color deltas; the GPU
    // blends the feathered patches. Runs after denoise to match export.
    // The touch-up count rides in `_pad0`.
    let touch_count = u32(adj._pad0);
    for (var i = 0u; i < touch_count; i = i + 1u) {
        let t = touchups[i];
        let d = (in.uv - t.center_radius_feather.xy) /
            vec2<f32>(adj.texel_w, adj.texel_h);
            // Radii are normalized to the image's shorter side.
        let radius_px = t.center_radius_feather.z / max(adj.texel_w, adj.texel_h);
        let distance_px = length(d);
        if (distance_px < radius_px) {
            let feather_px = max(radius_px * clamp(t.center_radius_feather.w, 0.02, 1.0), 1.0);
            var mask = clamp((radius_px - distance_px) / feather_px, 0.0, 1.0);
            mask = mask * mask * (3.0 - 2.0 * mask);
            let source_uv = t.source + (in.uv - t.center_radius_feather.xy);
            let source = textureSampleLevel(tex, samp, source_uv, 0.0).rgb + t.delta.xyz;
            r = r * (1.0 - mask) + clamp(source.r, 0.0, 1.0) * mask;
            g = g * (1.0 - mask) + clamp(source.g, 0.0, 1.0) * mask;
            b = b * (1.0 - mask) + clamp(source.b, 0.0, 1.0) * mask;
        }
    }

    // 1. White balance: temp/tint (−100..100) → gentle per-channel gains.
    let t = adj.temp / 100.0;
    let ti = adj.tint / 100.0;
    r = r * (1.0 + t * 0.3);
    g = g * (1.0 - ti * 0.15);
    b = b * (1.0 - t * 0.3);

    // 2. Exposure.
    let exposed = filmicExposure(vec3<f32>(r, g, b), adj.exposure);
    r = exposed.r;
    g = exposed.g;
    b = exposed.b;

    // 3. Linear to working gamma (difference 1 in the header).
    let srgb = linear_to_srgb(vec3<f32>(r, g, b));
    r = apply_raw_preview_boost(srgb.r);
    g = apply_raw_preview_boost(srgb.g);
    b = apply_raw_preview_boost(srgb.b);

    // 4. Perceptual tone ops, in the working (boosted) space.
    r = tone(r);
    g = tone(g);
    b = tone(b);

    // 4.5. Vibrance/saturation: scale chroma around luma in gamma space.
    // Must match develop.rs `apply_linear` and shader.wgsl.
    let luma = 0.299 * r + 0.587 * g + 0.114 * b;
    let satTotal = 1.0 + adj.saturation / 100.0;
    let cmax = max(r, max(g, b));
    let cmin = min(r, min(g, b));
    var curSat = 0.0;
    if (cmax > 0.0) {
        curSat = (cmax - cmin) / cmax;
    }
    let vibFactor = 1.0 + adj.vibrance / 100.0 * (1.0 - curSat);
    let total = satTotal * vibFactor;
    r = luma + (r - luma) * total;
    g = luma + (g - luma) * total;
    b = luma + (b - luma) * total;

    // 5. Clamp and keep alpha. No return to linear (difference 2 in the header).
    return vec4<f32>(clamp(r, 0.0, 1.0), clamp(g, 0.0, 1.0), clamp(b, 0.0, 1.0), texel.a);
}
