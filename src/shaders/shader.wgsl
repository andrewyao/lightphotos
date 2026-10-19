// SPDX-License-Identifier: MIT OR Apache-2.0

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

// Must match `OverlayParams` in renderer/mod.rs.
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
    // `in.uv` is the straightened canvas, which the crop is in.
    let uv = straightenUv(in.uv);
    // Derivatives before any early return: WebGPU requires `dpdx` under
    // uniform control flow. `textureSampleLevel(0.0)` would avoid them but
    // skips mips, which breaks antialiasing when zoomed out.
    //
    // An Rgba8UnormSrgb texture samples as linear light. The math below must
    // match `apply_linear` in develop/mod.rs.
    let ddx = dpdx(in.uv);
    let ddy = dpdy(in.uv);
    let texel = sampleSrc(uv, ddx, ddy);
    if (outsidePhoto(in.uv, uv)) {
        return vec4<f32>(0.0);
    }

    // The sliders here, with every mask's share added.
    let l = localTone(uv);
    let lin = developLinear(uv, texel.rgb, l);
    let working = pow(max(lin, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.2));
    let graded = developGamma(working, l, in.uv, footprintOf(ddx, ddy));

    // Back to linear, then sRGB-encode for the surface.
    let out = clamp(pow(max(graded, vec3<f32>(0.0)), vec3<f32>(2.2)), vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(linear_to_srgb(out), texel.a);
}

// Subject-selection overlay, drawn as a second pass blended over the image.
// It is separate from `fs_main` so it can never change the rendered tone.
@fragment
fn fs_overlay(in: VsOut) -> @location(0) vec4<f32> {
    // `in.uv` is the straightened canvas, which the crop is in.
    let uv = straightenUv(in.uv);
    // Sample before the discards, as in `fs_main`. Coverage is 0 for
    // background, 1 for subject, soft in between.
    var coverage = textureSample(mask_tex, mask_samp, uv).r;

    if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
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
