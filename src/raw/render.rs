// SPDX-License-Identifier: GPL-3.0-or-later

//! Render pipelines for linear-light RAW images (`PixelFormat::LinearF16`).
//! Only the wasm32 Loupe RAW path produces those; every other path bakes the
//! tonemap on the CPU in `apply_raw_preview_boost`. `Renderer::new` builds
//! these once at startup.

/// Texture format for a `PixelFormat::LinearF16` photo. It holds linear light
/// with no hardware sRGB decode. `Rgba16Float` is filterable and renderable in
/// core WebGPU, while `Rgba32Float` needs extra features and twice the memory.
pub(crate) const LINEAR_IMAGE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// The RAW tonemap pipeline, drawn instead of `pipeline` for `LinearF16`
/// images. It shares the full 4-group `pipeline_layout` so `render()` binds
/// groups 0-3 the same way for both pipelines. The vertex stage is
/// `shader.wgsl`'s `vs_main`, so pan/zoom (group 1) works unchanged.
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

/// The mip-gen pipeline for `LINEAR_IMAGE_FORMAT` textures. `mipgen.wgsl` is
/// format-agnostic, so only the color target differs from `mip_pipeline`.
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
