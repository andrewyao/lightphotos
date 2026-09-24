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
// 2. No conversion back to linear at the end. The renderer configures a
//    non-sRGB surface on every platform, so this output is already
//    display-ready.

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

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Sample before any early return. WebGPU rejects an implicit-LOD sample
    // after non-uniform control flow. Rgba16Float has no sRGB decode, so this
    // is linear.
    let texel = textureSample(tex, samp, in.uv);

    // Outside the image (UV beyond 0..1): draw the neutral background.
    if (in.uv.x < 0.0 || in.uv.x > 1.0 || in.uv.y < 0.0 || in.uv.y > 1.0) {
        return BACKGROUND;
    }
    // Outside the crop rectangle.
    if (in.uv.x < adj.crop_l || in.uv.x > adj.crop_r || in.uv.y < adj.crop_t || in.uv.y > adj.crop_b) {
        return BACKGROUND;
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
