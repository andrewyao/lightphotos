// SPDX-License-Identifier: GPL-3.0-or-later

// Mip-chain generation. Each level is drawn by sampling the level above with
// a linear filter. The destination is half the source size, so one bilinear
// tap at a destination texel center averages a 2x2 source block.
//
// Separate from `shader.wgsl`, whose vertex stage needs the zoom/pan transform.

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_mip(@builtin(vertex_index) vi: u32) -> VsOut {
    // One oversized triangle, clipped to the target, avoids a diagonal seam.
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let xy = corners[vi];
    var out: VsOut;
    out.pos = vec4<f32>(xy, 0.0, 1.0);
    // NDC (y up) to UV (y down).
    out.uv = vec2<f32>((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
    return out;
}

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_samp: sampler;

@fragment
fn fs_mip(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(src_tex, src_samp, in.uv);
}
