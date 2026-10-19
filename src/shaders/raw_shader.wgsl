// SPDX-License-Identifier: MIT OR Apache-2.0

// Fragment shader for scene-linear RAW images (`PixelFormat::LinearF16`:
// rawler's RAW develops off macOS, and macOS's with the Apple tone curve
// off). The decode gives linear RGB and this shader does the rest: sRGB
// gamma, the RAW display boost, and the Develop sliders. `apply_raw_display`
// in `develop/mod.rs` is its CPU twin, which export, the histogram and
// thumbnails use.
//
// Order: denoise -> touch-ups -> local edits -> WB -> exposure -> sRGB
// gamma + boost -> tone -> curve -> color mixer or B&W mix ->
// vibrance/saturation -> color grading -> vignette and grain -> clamp.
//
// Two differences from `shader.wgsl`:
// 1. The working gamma is the real sRGB curve plus the display boost, not
//    `pow(x, 1/2.2)`. With every slider at default the output equals the
//    plain gamma + boost result.
// 2. No conversion back to linear at the end. The renderer configures a
//    non-sRGB surface on every platform, so this output is already
//    display-ready.

// RAW display look curve, fitted to Apple ImageIO's render. Knots must match
// `RAW_LOOK_KNOTS` in develop/mod.rs.
const RAW_LOOK_KNOTS = array<f32, 17>(
    0.000, 0.027, 0.122, 0.220, 0.337, 0.463, 0.576, 0.678, 0.757, 0.824, 0.878, 0.918, 0.949,
    0.976, 0.992, 1.000, 1.000,
);

fn apply_raw_preview_boost(v: f32) -> f32 {
    let x = clamp(v, 0.0, 1.0) * 16.0;
    let i = min(u32(x), 15u);
    let t = x - f32(i);
    var knots = RAW_LOOK_KNOTS;
    return mix(knots[i], knots[i + 1u], t);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // `in.uv` is the straightened canvas, which the crop is in.
    let uv = straightenUv(in.uv);
    // Derivatives before any early return: WebGPU rejects `dpdx` after
    // non-uniform control flow. Rgba16Float has no sRGB decode, so this is
    // linear.
    let ddx = dpdx(in.uv);
    let ddy = dpdy(in.uv);
    let texel = sampleSrc(uv, ddx, ddy);
    if (outsidePhoto(in.uv, uv)) {
        return vec4<f32>(0.0);
    }

    // The sliders here, with every mask's share added.
    let l = localTone(uv);
    let lin = developLinear(uv, texel.rgb, l);
    // Linear to working gamma (difference 1 in the header).
    let srgb = linear_to_srgb(lin);
    let working = vec3<f32>(
        apply_raw_preview_boost(srgb.r),
        apply_raw_preview_boost(srgb.g),
        apply_raw_preview_boost(srgb.b),
    );
    let graded = developGamma(working, l, in.uv, footprintOf(ddx, ddy));

    // No return to linear (difference 2 in the header).
    return vec4<f32>(clamp(graded, vec3<f32>(0.0), vec3<f32>(1.0)), texel.a);
}
