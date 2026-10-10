// SPDX-License-Identifier: MIT OR Apache-2.0

// The base layer's build: three passes over a grid whose short side is 256
// cells at every image size (`develop::base::Grid`). Rust prepends
// loupe_common.wgsl. `fs_base_reduce` takes each cell's exact area average
// of the pre-tone pixel, then `fs_base_blur_h` and `fs_base_blur_v` blur it.
// Every read is a `textureLoad` and every value f32, so `BaseLayer::build`
// in develop/base.rs does the same arithmetic in the same order.

// The grid and the source, in cells and texels. Must match `BasePass` in
// renderer/mod.rs.
struct BasePass {
    grid: vec2<u32>,
    src: vec2<u32>,
};

// The previous pass's grid; unused by `fs_base_reduce`.
@group(1) @binding(0) var grid_in: texture_2d<f32>;
@group(1) @binding(1) var<uniform> pass_info: BasePass;

// Gaussian half-kernels, center first. Must match `GAUSS_MID` and
// `GAUSS_WIDE` in develop/base.rs; a test there compares the text.
const GAUSS_MID = array<f32, 7>(
    0.19967563, 0.17621312, 0.12110939, 0.064825185, 0.027023157, 0.008773135, 0.002218196,
);
const GAUSS_WIDE = array<f32, 25>(
    0.049976755, 0.04958783, 0.04843913, 0.046583455, 0.04410433, 0.041109756, 0.03772443,
    0.034081183, 0.030312434, 0.026542453, 0.022881025, 0.019418873, 0.016225077, 0.013346383,
    0.010808231, 0.008617073, 0.006763618, 0.005226519, 0.0039761257, 0.0029779808, 0.0021958253,
    0.0015939983, 0.0011391791, 0.0008015126, 0.0005551916,
);

struct BaseVsOut {
    @builtin(position) pos: vec4<f32>,
};

// One oversized triangle, clipped to the grid, as in mipgen.wgsl.
@vertex
fn vs_base(@builtin(vertex_index) vi: u32) -> BaseVsOut {
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    var out: BaseVsOut;
    out.pos = vec4<f32>(corners[vi], 0.0, 1.0);
    return out;
}

// The texels `[x, y)` that cell `i` of `cells` covers over `size` texels.
// Must match `cell_span` in develop/base.rs.
fn cellSpan(i: u32, cells: u32, size: u32) -> vec2<u32> {
    return vec2<u32>(i * size / cells, ((i + 1u) * size + cells - 1u) / cells);
}

// Texel `x`'s overlap with cell `i`, in units of 1 / `cells` texel. Must
// match `overlap` in develop/base.rs.
fn overlap(x: u32, i: u32, cells: u32, size: u32) -> u32 {
    return min((x + 1u) * cells, (i + 1u) * size) - max(x * cells, i * size);
}

// The pre-tone pixel at texel `(x, y)`'s center: CA removed, touch-ups
// blended, no denoise.
fn preAt(x: u32, y: u32) -> vec3<f32> {
    let uv = (vec2<f32>(f32(x), f32(y)) + vec2<f32>(0.5)) / vec2<f32>(pass_info.src);
    return applyTouchups(uv, sampleSrcLevel(uv).rgb);
}

// (luma, luma, darkest channel) of the cell's mean: the blur inputs.
@fragment
fn fs_base_reduce(in: BaseVsOut) -> @location(0) vec4<f32> {
    let cell = vec2<u32>(in.pos.xy);
    let grid = pass_info.grid;
    let src = pass_info.src;
    let xs = cellSpan(cell.x, grid.x, src.x);
    let ys = cellSpan(cell.y, grid.y, src.y);
    var sum = vec3<f32>(0.0);
    for (var y = ys.x; y < ys.y; y = y + 1u) {
        var row = vec3<f32>(0.0);
        for (var x = xs.x; x < xs.y; x = x + 1u) {
            row = row + f32(overlap(x, cell.x, grid.x, src.x)) * preAt(x, y);
        }
        sum = sum + f32(overlap(y, cell.y, grid.y, src.y)) * row;
    }
    let mean = sum / (f32(src.x) * f32(src.y));
    let luma = lumaGamma(mean);
    return vec4<f32>(luma, luma, min(mean.r, min(mean.g, mean.b)), 1.0);
}

// One separable pass along `step`: red and blue at sigma 2 cells, green at
// sigma 8, taps clamped at the edge and summed from the most negative up.
// Must match `blur_axis` in develop/base.rs.
fn blur(pos: vec4<f32>, step: vec2<i32>) -> vec4<f32> {
    var mid = GAUSS_MID;
    var wide = GAUSS_WIDE;
    let cell = vec2<i32>(pos.xy);
    let hi = vec2<i32>(pass_info.grid) - vec2<i32>(1);
    var acc = vec3<f32>(0.0);
    for (var k = -24; k <= 24; k = k + 1) {
        let v = textureLoad(grid_in, clamp(cell + k * step, vec2<i32>(0), hi), 0).rgb;
        let a = u32(abs(k));
        acc.g = acc.g + wide[a] * v.g;
        if (a <= 6u) {
            acc.r = acc.r + mid[a] * v.r;
            acc.b = acc.b + mid[a] * v.b;
        }
    }
    return vec4<f32>(acc, 1.0);
}

@fragment
fn fs_base_blur_h(in: BaseVsOut) -> @location(0) vec4<f32> {
    return blur(in.pos, vec2<i32>(1, 0));
}

@fragment
fn fs_base_blur_v(in: BaseVsOut) -> @location(0) vec4<f32> {
    return blur(in.pos, vec2<i32>(0, 1));
}
