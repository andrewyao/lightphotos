// SPDX-License-Identifier: GPL-3.0-or-later

// GPU half of the RAW display tonemap, ported to WGSL. The CPU side
// (`raw/preview.rs`'s `DemosaicMode::Quality`) does the demosaic and
// stops at linear camera-RGB; this shader finishes the job with real sRGB
// gamma plus a display brightness/contrast boost, applied to every RAW
// photo's rendering, and then the full Develop-slider tone pipeline
// (`develop.rs`'s `Adjustments`/`GpuAdjust`) on top — full parity with
// `shader.wgsl`, the non-RAW pipeline's own fragment shader.
// `image_decode.rs`'s `apply_raw_preview_boost` is the CPU twin of the
// gamma+boost math here — used by every OTHER RAW decode path (native
// non-mac, and this file's own `Fast`/Grid tier), which bakes the transform
// into u8 sRGB bytes at decode time instead. This module exists because
// `decode_raw_quality_from_bytes` (Loupe/`Preview` jobs, wasm32) deliberately
// stops at linear camera-RGB and hands the rest to the GPU — a two-stage
// CPU-decode/GPU-tonemap split, instead of lightphotos's usual CPU-LUT
// approach.
//
// Composition order (deliberately NOT a straight copy of `shader.wgsl`'s
// pipeline — see the two divergences called out inline below):
//   denoise -> touch-ups -> WB -> exposure -> real sRGB gamma + display
//   boost -> tone curve -> vibrance/saturation -> clamp -> return.
//
// Divergence 1: the real sRGB curve + display boost (`linear_to_srgb` +
// `apply_raw_preview_boost`) stay exactly where they were before this file
// grew develop-slider support, unmodified and undecomposed — they ARE this
// pipeline's "linear -> working gamma" step, `shader.wgsl`'s real-curve
// analog of its own `pow(x, 1/2.2)`. Keeping them untouched, in this exact
// position, is what makes the identity case (every slider at its default)
// render byte-identical to this file's pre-develop-slider output: `tone()`
// and the vibrance/saturation factor are both no-ops at default slider
// values (confirmed against `develop.rs`), so nothing downstream of the
// boost disturbs identity either.
//
// Divergence 2: this pipeline does NOT convert back to linear before
// returning (`shader.wgsl`'s final step, needed because ITS target is an
// sRGB-format swapchain view that auto-encodes linear values on store). This
// pipeline's target never gets that hardware encode: `wgpu`'s WebGPU canvas
// backend (`wgpu-29.0.3/src/backend/webgpu.rs`'s `WebSurface::get_capabilities`)
// only ever reports `Rgba8Unorm`/`Bgra8Unorm`/`Rgba16Float` for a canvas
// surface — never an sRGB variant, a real WebGPU spec restriction on canvas
// `configure()` formats — so `Renderer::new`'s `format.is_srgb()` search
// (`renderer.rs`) is always false here, and the boosted+toned value already
// IS the final display-ready output. Converting it back to linear here would
// be wrong for this target even though `shader.wgsl` requires exactly that
// for its own.

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Packed tone/crop uniform. Field order MUST match GpuAdjust in develop.rs,
// and MUST stay identical to shader.wgsl's own `Adjust` struct.
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
// Group 1 (xform) stays bound per the shared 4-group `pipeline_layout` (see
// `raw/render.rs::create_raw_pipeline`) but isn't declared here — only
// `shader.wgsl`'s own `vs_main` (the vertex stage this pipeline reuses)
// reads it, not this file's `fs_main`.

// Standard sRGB EOTF^-1 (linear -> sRGB-gamma-encoded). MUST stay in sync
// with `rawler::imgop::srgb::srgb_apply_gamma` (what the CPU LUT paths use)
// and shader.wgsl's own `linear_to_srgb` — same piecewise curve everywhere.
fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let cutoff = vec3<f32>(0.0031308);
    let a = vec3<f32>(0.055);
    let higher = (1.0 + a) * pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - a;
    let lower = c * 12.92;
    return select(higher, lower, c <= cutoff);
}

// RAW-only display boost, ported verbatim (constants included) from the
// reference implementation this pipeline matches — flattens/darkens less
// than a naive linear-matrix -> sRGB-gamma conversion, by design. MUST stay
// in sync with `image_decode.rs`'s `RAW_PREVIEW_BRIGHTNESS_GAMMA`/
// `RAW_PREVIEW_CONTRAST_MIX`/`apply_raw_preview_boost` — same formula, same
// constants, this module's WGSL twin of that CPU LUT.
const RAW_PREVIEW_BRIGHTNESS_GAMMA: f32 = 1.1;
const RAW_PREVIEW_CONTRAST_MIX: f32 = 0.75;

fn apply_raw_preview_boost(v: f32) -> f32 {
    let brightened = pow(clamp(v, 0.0, 1.0), 1.0 / RAW_PREVIEW_BRIGHTNESS_GAMMA);
    let contrast_curve = brightened * brightened * (3.0 - 2.0 * brightened);
    return clamp(brightened + (contrast_curve - brightened) * RAW_PREVIEW_CONTRAST_MIX, 0.0, 1.0);
}

// One gamma-space tone op, applied per channel. MUST stay in sync with the
// `tone` closure in apply_linear in develop.rs, and with shader.wgsl's own
// `tone` function — identical body, just fed by this file's real-sRGB+boost
// working space instead of shader.wgsl's `pow(1/2.2)` one.
fn tone(v: f32) -> f32 {
    var x = v;

    // Blacks/whites: shift the endpoints. ±100 → ±0.2 endpoint move.
    let blacks = adj.blacks / 100.0 * 0.2;
    let whites = adj.whites / 100.0 * 0.2;
    x = (x - (-blacks)) / ((1.0 + whites) - (-blacks));

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

// Hand-baked σ=1.0 Gaussian spatial weight for the 5×5 denoise kernel,
// indexed by squared tap distance. MUST stay in sync with `spatial_weight`
// in develop.rs and shader.wgsl's own copy.
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
    // Sampled unconditionally, before any branching — same WebGPU uniformity
    // requirement shader.wgsl's fs_main documents at its own textureSample
    // call: an implicit-LOD sample reachable only after a per-fragment
    // discard/return fails WebGPU's validator even though every fragment
    // that reaches it took the same path.
    //
    // Rgba16Float is not an sRGB-variant format, so this returns the stored
    // values AS-IS — genuinely linear, no hardware gamma decode (unlike
    // shader.wgsl's Rgba8UnormSrgb sample).
    let texel = textureSample(tex, samp, in.uv);

    // Outside the image (UV beyond 0..1): draw the neutral background.
    if (in.uv.x < 0.0 || in.uv.x > 1.0 || in.uv.y < 0.0 || in.uv.y > 1.0) {
        return vec4<f32>(0.12, 0.12, 0.13, 1.0);
    }
    // Outside the crop rectangle: same neutral background (identity crop
    // 0,0,1,1 never triggers this).
    if (in.uv.x < adj.crop_l || in.uv.x > adj.crop_r || in.uv.y < adj.crop_t || in.uv.y > adj.crop_b) {
        return vec4<f32>(0.12, 0.12, 0.13, 1.0);
    }

    var r = texel.r;
    var g = texel.g;
    var b = texel.b;

    // 0. Edge-aware (bilateral-style) denoise: a fixed 5x5 neighborhood, blended
    // by spatial + color-similarity weight. Only taken when denoise is active —
    // the identity path above (implicit-LOD textureSample) is left completely
    // untouched, so denoise == 0 renders byte-identical to before this branch
    // existed. Explicit textureSampleLevel (not textureSample) is used for every
    // tap since the tap loop isn't uniform control flow that implicit-LOD
    // derivatives can rely on; this also means minification antialiasing is
    // bypassed while denoise is active (a known, accepted trade-off when
    // zoomed far out). MUST stay in sync with `denoise_sample` in develop.rs.
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

    // Content-aware spot healing. Source centers and local color corrections
    // are selected on the CPU; the GPU only applies the feathered patches.
    // Keep this after denoise to match the CPU bake/export pipeline.
    let touch_count = u32(adj._pad0);
    for (var i = 0u; i < touch_count; i = i + 1u) {
        let t = touchups[i];
        let d = (in.uv - t.center_radius_feather.xy) /
            vec2<f32>(adj.texel_w, adj.texel_h);
        // Touch-up radii are normalized against the source image's shorter
        // dimension, matching the CPU bake/export path.
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

    // 2. Exposure: a stop is a doubling of linear light.
    let e = exp2(adj.exposure);
    r = r * e;
    g = g * e;
    b = b * e;

    // 3. Linear -> working gamma: real sRGB curve + display
    // boost, both left exactly as they were before this file had any
    // develop-slider math — see this file's module doc, "Divergence 1".
    let srgb = linear_to_srgb(vec3<f32>(r, g, b));
    r = apply_raw_preview_boost(srgb.r);
    g = apply_raw_preview_boost(srgb.g);
    b = apply_raw_preview_boost(srgb.b);

    // 4. Perceptual tone ops, in the working (boosted) space.
    r = tone(r);
    g = tone(g);
    b = tone(b);

    // 4.5. Vibrance/saturation: a cross-channel chroma scale about luma, in
    // gamma space. MUST stay in sync with the equivalent block in
    // apply_linear in develop.rs and shader.wgsl's own copy.
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

    // 5. Clamp final to 0..1, preserve sampled alpha. No linear round-trip
    // here — see this file's module doc, "Divergence 2": this target never
    // auto-encodes on store, so this IS the final display-ready value.
    return vec4<f32>(clamp(r, 0.0, 1.0), clamp(g, 0.0, 1.0), clamp(b, 0.0, 1.0), texel.a);
}
