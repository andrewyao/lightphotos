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

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@group(1) @binding(0) var<uniform> xform: Transform;

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

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Outside the image (UV beyond 0..1): draw the neutral background.
    if (in.uv.x < 0.0 || in.uv.x > 1.0 || in.uv.y < 0.0 || in.uv.y > 1.0) {
        return vec4<f32>(0.12, 0.12, 0.13, 1.0);
    }
    return textureSample(tex, samp, in.uv);
}
