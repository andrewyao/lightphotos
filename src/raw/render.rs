// SPDX-License-Identifier: GPL-3.0-or-later

//! GPU tonemap pipeline for RAW previews — the wgpu counterpart to
//! `raw/nonmac_decode.rs`'s `apply_raw_preview_boost`. Split out of
//! `renderer.rs`'s `Renderer::new()` (which still owns the `Renderer` struct
//! and its fields — this file just builds the two pipeline objects, since
//! Rust won't let a struct's fields live in one file and its impl in
//! another). Compiles on every platform, but the pipelines here only ever
//! get fed a `PixelFormat::LinearF16` image on the wasm32 Loupe RAW path.

// ## Pipeline position
// - Built once, at startup, inside `Renderer::new` — not per-frame.
// - Used every frame `renderer.rs`'s `render()` draws a `LinearF16` image:
//   the last step of Pipeline 1 for wasm32's RAW "Quality" tier only.
// - Never invoked on macOS or native Linux/Windows — they never produce a
//   `LinearF16` image in the first place (see `image_decode.rs`'s
//   `PixelFormat` doc comment).
// - See `ARCHITECTURE.md`.

/// Texture format used for an uploaded `PixelFormat::LinearF16` photo (the
/// wasm32 Loupe RAW path, `DemosaicMode::Quality`) — genuinely linear light,
/// 2 bytes per channel. `raw_shader.wgsl` samples it as-is, with no hardware
/// sRGB decode (unlike `renderer.rs`'s `IMAGE_FORMAT`). We use `Rgba16Float`
/// rather than `Rgba32Float` because both are filterable and usable as a
/// render attachment in core WebGPU (`Rgba32Float` needs extra features for
/// either), and it's half the memory of a 32-bit float texture — real
/// savings given how tight linear memory gets on wasm32 with large RAW
/// files.
pub(crate) const LINEAR_IMAGE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// The RAW-preview tonemap pipeline (see `raw_shader.wgsl`'s module doc
/// comment for what it does) — drawn instead of `renderer.rs`'s `pipeline`
/// whenever the current image is `PixelFormat::LinearF16`. Built against the
/// same 4-group `pipeline_layout` as `pipeline`, not a smaller one: the
/// fragment shader here only touches group 0, but sharing the full layout
/// means `Renderer::render()`'s draw code can set bind groups 0-3 the same
/// way for both pipelines with no branching — a shader is free to ignore
/// part of its pipeline layout. The vertex stage is just `shader.wgsl`'s own
/// `vs_main`, reused via the `shader` module passed in (wgpu lets a
/// pipeline's vertex and fragment stages come from different shader modules
/// as long as their I/O matches up by `@location`), so pan/zoom (group 1)
/// keeps working here for free.
pub(crate) fn create_raw_pipeline(
    device: &wgpu::Device,
    pipeline_layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let raw_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("raw_shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("raw_shader.wgsl").into()),
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("raw_pipeline"),
        layout: Some(pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &raw_shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

/// Same mip-gen shader/layout as `renderer.rs`'s `mip_pipeline`, just
/// targeting `LINEAR_IMAGE_FORMAT` instead — `Renderer::set_image` picks
/// whichever pipeline matches the image just uploaded. `mipgen.wgsl` doesn't
/// care about format (it's just an unconditional `textureSample`), so no
/// shader changes were needed, only a second `ColorTargetState`.
pub(crate) fn create_mip_pipeline_linear(
    device: &wgpu::Device,
    mip_pipeline_layout: &wgpu::PipelineLayout,
    mip_shader: &wgpu::ShaderModule,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("mip_pipeline_linear"),
        layout: Some(mip_pipeline_layout),
        vertex: wgpu::VertexState {
            module: mip_shader,
            entry_point: Some("vs_mip"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: mip_shader,
            entry_point: Some("fs_mip"),
            targets: &[Some(wgpu::ColorTargetState {
                format: LINEAR_IMAGE_FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}
