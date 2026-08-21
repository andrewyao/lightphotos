// SPDX-License-Identifier: GPL-3.0-or-later

// Mip-chain generation. Each level is produced by drawing a fullscreen triangle
// into it while sampling the level above with a linear filter: because the
// destination is exactly half the source's size, one bilinear tap at a
// destination texel's center lands on the corner of a source 2x2 block and
// averages it — the same box filter the CPU path used to compute, for none of
// the CPU time.
//
// Kept separate from `shader.wgsl` because that module's vertex stage reads the
// loupe's zoom/pan transform from group 1; this pass has no transform and no
// viewport, it just covers its render target.

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_mip(@builtin(vertex_index) vi: u32) -> VsOut {
    // One oversized triangle rather than two quad triangles: no seam along the
    // diagonal, and it clips to the same covered area.
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
