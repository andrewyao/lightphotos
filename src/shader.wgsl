// SPDX-License-Identifier: GPL-3.0-or-later

// The loupe image shader. The vertex shader applies zoom, pan, and rotation to
// the UVs; the fragment shader applies develop adjustments.

struct Transform {
    // How much of the texture is visible; smaller is zoomed in.
    scale: vec2<f32>,
    // Top-left UV of the visible region.
    offset: vec2<f32>,
    // 2x2 display-UV to texture-UV rotation, row-major [m00, m01, m10, m11].
    rot: vec4<f32>,
};

// Must match `OverlayParams` in renderer.rs.
struct Overlay {
    tint: vec4<f32>,
    // 1.0 = highlight the background instead of the subject.
    invert: f32,
    // Peak opacity of the tint, 0..1.
    strength: f32,
    _pad0: f32,
    _pad1: f32,
};

@group(1) @binding(0) var<uniform> xform: Transform;
// The overlay shares group 3 with the touch-ups because WebGPU guarantees only
// four bind groups. No entry point reads both, and each pipeline's layout
// declares only the bindings its entry point reads.
@group(3) @binding(1) var<uniform> overlay: Overlay;
@group(3) @binding(2) var mask_tex: texture_2d<f32>;
@group(3) @binding(3) var mask_samp: sampler;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
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
    // Clip space (-1..1, y up) to UV (0..1, y down).
    let base_uv = vec2<f32>((c.x + 1.0) * 0.5, (1.0 - c.y) * 0.5);
    // Display-space UV, before undoing rotation.
    let d = base_uv * xform.scale + xform.offset;
    // Rotate about the center. `mat2x2` takes columns, so transpose `rot`.
    let m = mat2x2<f32>(xform.rot.x, xform.rot.z, xform.rot.y, xform.rot.w);
    out.uv = m * (d - vec2<f32>(0.5, 0.5)) + vec2<f32>(0.5, 0.5);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Sample before any early return. WebGPU requires `textureSample` under
    // uniform control flow and rejects the shader module otherwise.
    // `textureSampleLevel(0.0)` would also pass but skips mips, which breaks
    // antialiasing when zoomed out.
    //
    // An Rgba8UnormSrgb texture samples as linear light. The math below must
    // match `apply_linear` in develop.rs.
    let texel = textureSample(tex, samp, in.uv);

    // Outside the image or the crop: the neutral background.
    if (in.uv.x < 0.0 || in.uv.x > 1.0 || in.uv.y < 0.0 || in.uv.y > 1.0) {
        return BACKGROUND;
    }
    if (in.uv.x < adj.crop_l || in.uv.x > adj.crop_r || in.uv.y < adj.crop_t || in.uv.y > adj.crop_b) {
        return BACKGROUND;
    }

    var r = texel.r;
    var g = texel.g;
    var b = texel.b;

    // 0. Edge-aware denoise over a 5x5 neighborhood, weighted by distance and
    // color similarity. The taps use `textureSampleLevel` because the loop is
    // not uniform control flow, so denoise loses mip antialiasing when zoomed
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

    // Spot healing. The CPU picks each source patch and color correction; the
    // GPU blends the feathered patches. Keep this after denoise to match the
    // CPU export path. The touch-up count is packed into `adj._pad0`.
    let touch_count = u32(adj._pad0);
    for (var i = 0u; i < touch_count; i = i + 1u) {
        let t = touchups[i];
        let d = (in.uv - t.center_radius_feather.xy) /
            vec2<f32>(adj.texel_w, adj.texel_h);
        // Radii are fractions of the image's shorter side, as on the CPU.
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

    // 1. White balance: temp and tint (-100..100) as per-channel gains.
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

    // 3. Linear to gamma 2.2 working space.
    r = pow(max(r, 0.0), 1.0 / 2.2);
    g = pow(max(g, 0.0), 1.0 / 2.2);
    b = pow(max(b, 0.0), 1.0 / 2.2);

    // 4. Tone ops in gamma space.
    r = tone(r);
    g = tone(g);
    b = tone(b);

    // 4.5. Vibrance and saturation scale chroma around luma, in gamma space.
    // Must match `apply_linear` in develop.rs.
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

    // 5. Back to linear, then sRGB-encode for the surface.
    let lin = clamp(pow(max(vec3<f32>(r, g, b), vec3<f32>(0.0)), vec3<f32>(2.2)), vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(linear_to_srgb(lin), texel.a);
}

// Subject-selection overlay, drawn as a second pass blended over the image.
// It is separate from `fs_main` so it can never change the rendered tone.
@fragment
fn fs_overlay(in: VsOut) -> @location(0) vec4<f32> {
    // Sample before the discards, as in `fs_main`. Coverage is 0 for
    // background, 1 for subject, soft in between.
    var coverage = textureSample(mask_tex, mask_samp, in.uv).r;

    if (in.uv.x < 0.0 || in.uv.x > 1.0 || in.uv.y < 0.0 || in.uv.y > 1.0) {
        discard;
    }
    if (in.uv.x < adj.crop_l || in.uv.x > adj.crop_r || in.uv.y < adj.crop_t || in.uv.y > adj.crop_b) {
        discard;
    }

    if (overlay.invert > 0.5) {
        coverage = 1.0 - coverage;
    }

    return vec4<f32>(linear_to_srgb(overlay.tint.rgb), coverage * overlay.strength);
}
