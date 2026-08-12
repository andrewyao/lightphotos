// SPDX-License-Identifier: MIT OR Apache-2.0

// Full-screen quad. The vertex shader maps each corner to a UV that is
// scaled and offset by the zoom/pan transform, so panning and zooming are
// pure GPU transforms — the texture is never re-uploaded.

struct Transform {
    // scale.xy: how much of the texture is visible (smaller = zoomed in)
    scale: vec2<f32>,
    // offset.xy: top-left UV of the visible region
    offset: vec2<f32>,
    // rot: 2x2 display-UV -> texture-UV rotation, row-major [m00, m01, m10, m11].
    rot: vec4<f32>,
};

// Packed tone/crop uniform. Field order MUST match GpuAdjust in develop.rs.
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

// Subject-selection overlay parameters. Visualization only — nothing here
// feeds the tone pipeline.
struct Overlay {
    // Colour laid over the selected region.
    tint: vec4<f32>,
    // 1.0 = highlight the background instead of the subject.
    invert: f32,
    // Peak opacity of the tint, 0..1.
    strength: f32,
    _pad0: f32,
    _pad1: f32,
};

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@group(1) @binding(0) var<uniform> xform: Transform;
@group(2) @binding(0) var<uniform> adj: Adjust;
@group(3) @binding(0) var<storage, read> touchups: array<TouchUp>;
// The overlay's own resources share group 3 with the touch-ups because wgpu
// guarantees only four bind groups. They never collide: each pipeline declares
// a layout covering exactly the bindings its own entry point reads, and no
// entry point reads both.
@group(3) @binding(1) var<uniform> overlay: Overlay;
@group(3) @binding(2) var mask_tex: texture_2d<f32>;
@group(3) @binding(3) var mask_samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // Two triangles covering clip space [-1, 1].
    var corners = array<vec2<f32>, 4>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 1.0,  1.0),
    );
    var idx = array<u32, 6>(0u, 1u, 2u, 2u, 1u, 3u);
    let c = corners[idx[vi]];

    var out: VsOut;
    out.pos = vec4<f32>(c, 0.0, 1.0);
    // Map clip-space corner (-1..1) to base UV (0..1), y flipped.
    let base_uv = vec2<f32>((c.x + 1.0) * 0.5, (1.0 - c.y) * 0.5);
    // Display-space UV (0..1 over the on-screen, possibly-rotated footprint).
    let d = base_uv * xform.scale + xform.offset;
    // Rotate about the center to get the texture-space UV. rot is row-major
    // [m00, m01, m10, m11]; mat2x2 takes columns, so feed it transposed.
    let m = mat2x2<f32>(xform.rot.x, xform.rot.z, xform.rot.y, xform.rot.w);
    out.uv = m * (d - vec2<f32>(0.5, 0.5)) + vec2<f32>(0.5, 0.5);
    return out;
}

// One gamma-space tone op, applied per channel. MUST stay in sync with the
// `tone` closure in apply_linear in develop.rs.
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
// in develop.rs.
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
    // Outside the image (UV beyond 0..1): draw the neutral background.
    if (in.uv.x < 0.0 || in.uv.x > 1.0 || in.uv.y < 0.0 || in.uv.y > 1.0) {
        return vec4<f32>(0.12, 0.12, 0.13, 1.0);
    }
    // Outside the crop rectangle: same neutral background (identity crop
    // 0,0,1,1 never triggers this).
    if (in.uv.x < adj.crop_l || in.uv.x > adj.crop_r || in.uv.y < adj.crop_t || in.uv.y > adj.crop_b) {
        return vec4<f32>(0.12, 0.12, 0.13, 1.0);
    }

    // Sampling an Rgba8UnormSrgb texture returns LINEAR-light RGB.
    // MUST stay in sync with apply_linear in develop.rs.
    let texel = textureSample(tex, samp, in.uv);
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

    // 3. Linear → working gamma (clamp negatives first).
    r = pow(max(r, 0.0), 1.0 / 2.2);
    g = pow(max(g, 0.0), 1.0 / 2.2);
    b = pow(max(b, 0.0), 1.0 / 2.2);

    // 4. Perceptual tone ops in gamma space.
    r = tone(r);
    g = tone(g);
    b = tone(b);

    // 4.5. Vibrance/saturation: a cross-channel chroma scale about luma, in
    // gamma space. MUST stay in sync with the equivalent block in
    // apply_linear in develop.rs.
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

    // 5. Working → linear.
    r = pow(max(r, 0.0), 2.2);
    g = pow(max(g, 0.0), 2.2);
    b = pow(max(b, 0.0), 2.2);

    // 6. Clamp final to 0..1, preserve sampled alpha.
    return vec4<f32>(clamp(r, 0.0, 1.0), clamp(g, 0.0, 1.0), clamp(b, 0.0, 1.0), texel.a);
}

// Subject-selection overlay: a second draw of the same quad, alpha-blended over
// the already-rendered image. Deliberately separate from `fs_main` — the
// selection is a thing you look at, never a thing that changes the picture, so
// it cannot touch the tone pipeline even by accident.
@fragment
fn fs_overlay(in: VsOut) -> @location(0) vec4<f32> {
    // Outside the image, or outside the crop: draw nothing, so the overlay
    // never spills onto the neutral surround.
    if (in.uv.x < 0.0 || in.uv.x > 1.0 || in.uv.y < 0.0 || in.uv.y > 1.0) {
        discard;
    }
    if (in.uv.x < adj.crop_l || in.uv.x > adj.crop_r || in.uv.y < adj.crop_t || in.uv.y > adj.crop_b) {
        discard;
    }

    // The mask is single-channel coverage: 0 background, 1 subject, soft rim
    // between. Inverting is a read of the same mask from the other side.
    var coverage = textureSample(mask_tex, mask_samp, in.uv).r;
    if (overlay.invert > 0.5) {
        coverage = 1.0 - coverage;
    }

    return vec4<f32>(overlay.tint.rgb, coverage * overlay.strength);
}
