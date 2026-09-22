// SPDX-License-Identifier: GPL-3.0-or-later

//! wgpu rendering of the loupe image, plus the egui pass on top. Each decoded
//! image is uploaded once as a texture with a GPU-built mip chain. Zoom and pan
//! only update a small transform uniform. Every platform's decoder reaches
//! `set_image` through `App::upload_shown`. `shader.wgsl` draws every image
//! except `PixelFormat::LinearF16`, which uses `raw_shader.wgsl`.

use std::collections::HashMap;
use std::sync::Arc;
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::develop::{GpuAdjust, GpuTouchUp};
use crate::image_decode::{DecodedImage, PixelFormat};

#[path = "raw/render.rs"]
mod raw_render;

/// Texture format for a `PixelFormat::Srgb8` photo. The mip-gen pipeline
/// renders into the image's mip levels, so its target must match this.
const IMAGE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Subject-selection overlay uniform. Must match `Overlay` in `shader.wgsl`.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct OverlayParams {
    tint: [f32; 4],
    invert: f32,
    strength: f32,
    _pad0: f32,
    _pad1: f32,
}

impl Default for OverlayParams {
    fn default() -> Self {
        Self {
            tint: [0.25, 1.0, 0.45, 1.0],
            invert: 0.0,
            // Light enough to see the photo and judge where the edge falls.
            strength: 0.45,
            _pad0: 0.0,
            _pad1: 0.0,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct Transform {
    scale: [f32; 2],
    offset: [f32; 2],
    /// 2x2 display-UV -> texture-UV rotation, row-major [m00, m01, m10, m11].
    rot: [f32; 4],
}

/// Everything egui needs to paint one frame.
pub struct EguiPaint {
    pub textures_delta: egui::TexturesDelta,
    pub paint_jobs: Vec<egui::ClippedPrimitive>,
    pub screen_descriptor: egui_wgpu::ScreenDescriptor,
}

const CLEAR_COLOR: wgpu::Color = wgpu::Color {
    r: 0.07,
    g: 0.07,
    b: 0.08,
    a: 1.0,
};

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    /// Drawn instead of `pipeline` for `PixelFormat::LinearF16` images.
    raw_pipeline: wgpu::RenderPipeline,

    sampler: wgpu::Sampler,
    tex_bind_layout: wgpu::BindGroupLayout,

    xform_buf: wgpu::Buffer,
    xform_bind: wgpu::BindGroup,

    adj_buf: wgpu::Buffer,
    adj_bind: wgpu::BindGroup,

    /// Adjustments for the "after" half of the compare view.
    adj_buf_b: wgpu::Buffer,
    adj_bind_b: wgpu::BindGroup,
    touch_buf: wgpu::Buffer,
    touch_bind: wgpu::BindGroup,

    /// Draws the subject-selection tint over the image. It is a separate
    /// pipeline so the selection can never change the rendered tone.
    overlay_pipeline: wgpu::RenderPipeline,
    overlay_bind_layout: wgpu::BindGroupLayout,
    overlay_buf: wgpu::Buffer,
    /// The overlay uniform and mask texture. `None` means no mask, and the
    /// overlay draw is skipped.
    overlay_bind: Option<wgpu::BindGroup>,

    /// `None` until the first image loads.
    image_bind: Option<wgpu::BindGroup>,
    pub image_size: (u32, u32),
    /// Picks `pipeline` or `raw_pipeline` in `render()`.
    image_pixel_format: PixelFormat,

    pub max_dim: u32,

    mip_pipeline: wgpu::RenderPipeline,
    /// The same mip-gen shader, targeting `LINEAR_IMAGE_FORMAT`.
    mip_pipeline_linear: wgpu::RenderPipeline,
    mip_sampler: wgpu::Sampler,

    egui_renderer: egui_wgpu::Renderer,

    /// Grid thumbnails handed to egui as user textures. `register_native_texture`
    /// stores only a bind group, so the texture has to be kept alive here until
    /// `free_thumb` drops both.
    thumb_textures: HashMap<egui::TextureId, wgpu::Texture>,
}

impl Renderer {
    /// `async` because the browser main thread can't block. Native callers
    /// wrap it in `pollster::block_on`; wasm awaits it in `spawn_local`.
    ///
    /// The caller passes `size` because on wasm32 winit's `inner_size()` is
    /// `(0, 0)` until the browser's `ResizeObserver` first fires. A 1x1 surface
    /// makes every render pass fail WebGPU's scissor-rect validation.
    pub async fn new(window: Arc<Window>, size: winit::dpi::PhysicalSize<u32>) -> Self {
        let instance = wgpu::Instance::default();
        // A browser with no WebGPU at all fails here, before `request_adapter`
        // is ever reached, so this is the report that covers "can't run".
        let surface = instance.create_surface(window).unwrap_or_else(|e| {
            #[cfg(target_arch = "wasm32")]
            crate::analytics::property("webgpu_unsupported", "reason", "surface_unavailable");
            panic!("create surface: {e}");
        });

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .unwrap_or_else(|e| {
                #[cfg(target_arch = "wasm32")]
                crate::analytics::property("webgpu_unsupported", "reason", "adapter_unavailable");
                panic!("no adapter: {e}");
            });

        // Request the adapter's real limits; the defaults cap textures at 8192.
        let limits = adapter.limits();
        let max_dim = limits.max_texture_dimension_2d;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("device"),
                required_features: wgpu::Features::empty(),
                required_limits: limits,
                experimental_features: wgpu::ExperimentalFeatures::default(),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
            })
            .await
            .unwrap_or_else(|e| {
                #[cfg(target_arch = "wasm32")]
                crate::analytics::property("webgpu_unsupported", "reason", "device_unavailable");
                panic!("request device: {e}");
            });

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let tex_bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tex_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let xform_bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("xform_bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let adj_bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("adj_bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let touch_bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("touch_bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        // Overlay bindings start at 1 so they never overlap the touch-up
        // buffer at group 3 binding 0. See shader.wgsl.
        let overlay_bind_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("overlay_bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pl"),
            bind_group_layouts: &[
                Some(&tex_bind_layout),
                Some(&xform_bind_layout),
                Some(&adj_bind_layout),
                Some(&touch_bind_layout),
            ],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
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
        });

        let raw_pipeline =
            raw_render::create_raw_pipeline(&device, &pipeline_layout, &shader, format);

        // Same vertex shader as the image, so the tint follows zoom, pan, and
        // rotation. Group 0 (the image) is unused: the overlay reads the mask.
        let overlay_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("overlay_pl"),
                bind_group_layouts: &[
                    None,
                    Some(&xform_bind_layout),
                    Some(&adj_bind_layout),
                    Some(&overlay_bind_layout),
                ],
                immediate_size: 0,
            });

        let overlay_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("overlay_pipeline"),
            layout: Some(&overlay_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_overlay"),
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
        });

        // Mip generation reuses `tex_bind_layout` (texture at 0, sampler at 1).
        let mip_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mipgen_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("mipgen.wgsl").into()),
        });
        let mip_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("mip_pl"),
            bind_group_layouts: &[Some(&tex_bind_layout)],
            immediate_size: 0,
        });
        let mip_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("mip_pipeline"),
            layout: Some(&mip_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &mip_shader,
                entry_point: Some("vs_mip"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &mip_shader,
                entry_point: Some("fs_mip"),
                // Must match the image texture's format, not the surface's.
                targets: &[Some(wgpu::ColorTargetState {
                    format: IMAGE_FORMAT,
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
        });

        let mip_pipeline_linear =
            raw_render::create_mip_pipeline_linear(&device, &mip_pipeline_layout, &mip_shader);

        // Not the main `sampler`: it must not read the mip chain it is still
        // building, so its mipmap filter is nearest and each view it reads is
        // pinned to one level.
        let mip_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("mip_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        let xform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("xform"),
            contents: bytemuck::bytes_of(&Transform {
                scale: [1.0, 1.0],
                offset: [0.0, 0.0],
                rot: [1.0, 0.0, 0.0, 1.0],
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let xform_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("xform_bg"),
            layout: &xform_bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: xform_buf.as_entire_binding(),
            }],
        });

        let adj_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("adj"),
            contents: bytemuck::bytes_of(&GpuAdjust::default()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let adj_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("adj_bg"),
            layout: &adj_bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: adj_buf.as_entire_binding(),
            }],
        });

        let adj_buf_b = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("adj_b"),
            contents: bytemuck::bytes_of(&GpuAdjust::default()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let adj_bind_b = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("adj_bg_b"),
            layout: &adj_bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: adj_buf_b.as_entire_binding(),
            }],
        });

        const MAX_TOUCHUPS: u64 = 64;
        let touch_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("touchups"),
            size: MAX_TOUCHUPS * std::mem::size_of::<GpuTouchUp>() as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let touch_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("touch_bg"),
            layout: &touch_bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: touch_buf.as_entire_binding(),
            }],
        });

        let overlay_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("overlay"),
            contents: bytemuck::bytes_of(&OverlayParams::default()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let egui_renderer =
            egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());

        Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            raw_pipeline,
            sampler,
            tex_bind_layout,
            xform_buf,
            xform_bind,
            adj_buf,
            adj_bind,
            adj_buf_b,
            adj_bind_b,
            touch_buf,
            touch_bind,
            overlay_pipeline,
            overlay_bind_layout,
            overlay_buf,
            overlay_bind: None,
            image_bind: None,
            image_size: (0, 0),
            image_pixel_format: PixelFormat::Srgb8,
            max_dim,
            mip_pipeline,
            mip_pipeline_linear,
            mip_sampler,
            egui_renderer,
            thumb_textures: HashMap::new(),
        }
    }

    /// Upload one grid thumbnail and return the id egui draws it by.
    ///
    /// This exists so the grid never pays for a second copy of the pixels.
    /// `Context::load_texture` takes an `egui::ColorImage`, which can only be
    /// built by copying `rgba` into a fresh `Vec<Color32>`; the thumbnail is
    /// already owned by the loader's cache, so that copy was pure duplication
    /// (measured at 47% of a session's allocation). Writing the bytes straight
    /// into a texture skips it, and on wasm32 it keeps them out of a linear
    /// memory that never shrinks.
    ///
    /// `Rgba8Unorm` is what `register_native_texture` requires and what egui
    /// stores its own images in, so these bytes reach the shader exactly as
    /// `load_texture` would have delivered them. `rgba` must be tightly packed
    /// RGBA8, premultiplied, holding the same sRGB-encoded bytes egui expects.
    pub fn upload_thumb(&mut self, width: u32, height: u32, rgba: &[u8]) -> Option<egui::TextureId> {
        let expected = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
        if width == 0 || height == 0 || rgba.len() != expected {
            return None;
        }

        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("thumb"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            texture.as_image_copy(),
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            size,
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        // LINEAR to match the `TextureOptions::LINEAR` the grid asked for when
        // these went through `load_texture`.
        let id = self.egui_renderer.register_native_texture(
            &self.device,
            &view,
            wgpu::FilterMode::Linear,
        );
        self.thumb_textures.insert(id, texture);
        Some(id)
    }

    /// Release a thumbnail texture. An id that was already freed is ignored.
    pub fn free_thumb(&mut self, id: egui::TextureId) {
        if self.thumb_textures.remove(&id).is_some() {
            self.egui_renderer.free_texture(&id);
        }
    }

    #[allow(dead_code)]
    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// The size the surface is configured at, which is what every pass
    /// draws into. On wasm this can differ by a pixel from
    /// `window.inner_size()`, which rounds the canvas size its own way.
    pub fn surface_size(&self) -> [u32; 2] {
        [self.config.width, self.config.height]
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
    }

    /// Upload a decoded image as a mipmapped texture and bind it.
    ///
    /// Runs on the UI thread when the photo should appear, so it avoids bulk CPU
    /// pixel work: it uploads level 0 and the GPU builds the other levels.
    #[hotpath::measure]
    pub fn set_image(&mut self, img: &DecodedImage) {
        // `std::time::Instant::now()` panics on wasm32.
        let t0 = web_time::Instant::now();
        let (w, h) = (img.width, img.height);
        let mip_count = (32 - (w.max(h)).leading_zeros()).max(1); // floor(log2(max))+1

        let (format, bytes_per_pixel, mip_pipeline): (
            wgpu::TextureFormat,
            u32,
            &wgpu::RenderPipeline,
        ) = match img.pixel_format {
            PixelFormat::Srgb8 => (IMAGE_FORMAT, 4, &self.mip_pipeline),
            PixelFormat::LinearF16 => (
                raw_render::LINEAR_IMAGE_FORMAT,
                8,
                &self.mip_pipeline_linear,
            ),
        };
        self.image_pixel_format = img.pixel_format;

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("image"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: mip_count,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            // RENDER_ATTACHMENT lets the GPU render into the mip levels.
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });

        // `write_texture` needs rows aligned to `COPY_BYTES_PER_ROW_ALIGNMENT`
        // (256 bytes). Upload the caller's buffer directly when rows already
        // align, and copy into a padded buffer otherwise.
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let row = bytes_per_pixel * w;
        let padded_row = row.div_ceil(align) * align;
        let level0: std::borrow::Cow<[u8]> = if padded_row == row {
            std::borrow::Cow::Borrowed(&img.rgba)
        } else {
            let mut padded = vec![0u8; (padded_row * h) as usize];
            for y in 0..h as usize {
                let src = y * row as usize;
                let dst = y * padded_row as usize;
                padded[dst..dst + row as usize].copy_from_slice(&img.rgba[src..src + row as usize]);
            }
            std::borrow::Cow::Owned(padded)
        };
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &level0,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );

        // Each level n is drawn by sampling level n-1. One view per level lets
        // a pass read n-1 while writing n without aliasing.
        if mip_count > 1 {
            let levels: Vec<wgpu::TextureView> = (0..mip_count)
                .map(|level| {
                    texture.create_view(&wgpu::TextureViewDescriptor {
                        label: Some("image_mip"),
                        base_mip_level: level,
                        mip_level_count: Some(1),
                        ..Default::default()
                    })
                })
                .collect();

            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("mipgen_encoder"),
                });
            for level in 1..mip_count as usize {
                let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("mip_bg"),
                    layout: &self.tex_bind_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&levels[level - 1]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&self.mip_sampler),
                        },
                    ],
                });
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("mipgen_pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &levels[level],
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(mip_pipeline);
                pass.set_bind_group(0, &bind, &[]);
                pass.draw(0..3, 0..1);
            }
            self.queue.submit(std::iter::once(encoder.finish()));
        }

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image_bg"),
            layout: &self.tex_bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.image_bind = Some(bind);
        self.image_size = (w, h);

        // The user feels this UI-thread span as a freeze.
        crate::loader::mark(&format!(
            "set_image {w}x{h} ({mip_count} mips): took {:?}",
            t0.elapsed()
        ));
    }

    pub fn set_transform(&mut self, scale: [f32; 2], offset: [f32; 2], rot: [f32; 4]) {
        self.queue.write_buffer(
            &self.xform_buf,
            0,
            bytemuck::bytes_of(&Transform { scale, offset, rot }),
        );
    }

    pub fn set_adjustments(&mut self, a: GpuAdjust) {
        self.queue
            .write_buffer(&self.adj_buf, 0, bytemuck::bytes_of(&a));
    }

    /// Adjustments for the "after" half of the compare view.
    pub fn set_adjustments_b(&mut self, a: GpuAdjust) {
        self.queue
            .write_buffer(&self.adj_buf_b, 0, bytemuck::bytes_of(&a));
    }

    /// Upload a selection mask, or clear it with `None` to stop drawing the
    /// overlay. `alpha` is `width * height` coverage bytes, packed with no row
    /// padding, in the image's texture space so the shader samples it with the
    /// image's UVs.
    pub fn set_selection_mask(&mut self, mask: Option<(&[u8], u32, u32)>) {
        let Some((alpha, w, h)) = mask else {
            self.overlay_bind = None;
            return;
        };
        if w == 0 || h == 0 || alpha.len() < (w as usize * h as usize) {
            self.overlay_bind = None;
            return;
        }

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("selection_mask"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        // At one byte per texel, rows almost never meet the 256-byte
        // alignment `write_texture` needs, so always pad.
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
        let row = w as usize;
        let padded_row = row.div_ceil(align) * align;
        let mut padded = vec![0u8; padded_row * h as usize];
        for y in 0..h as usize {
            let src = y * row;
            padded[y * padded_row..y * padded_row + row].copy_from_slice(&alpha[src..src + row]);
        }

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &padded,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row as u32),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.overlay_bind = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("overlay_bg"),
            layout: &self.overlay_bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.overlay_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        }));
    }

    /// Flip the overlay between tinting the subject and tinting the background.
    pub fn set_selection_inverted(&mut self, inverted: bool) {
        self.queue.write_buffer(
            &self.overlay_buf,
            0,
            bytemuck::bytes_of(&OverlayParams {
                invert: if inverted { 1.0 } else { 0.0 },
                ..OverlayParams::default()
            }),
        );
    }

    pub fn set_touchups(&mut self, touchups: &[GpuTouchUp]) {
        if !touchups.is_empty() {
            self.queue
                .write_buffer(&self.touch_buf, 0, bytemuck::cast_slice(touchups));
        }
    }

    /// Render the image pass, then the egui pass, in one submission.
    ///
    /// Viewports are `(x, y, w, h)` in physical pixels from the surface's
    /// top-left. `image_viewport` of `None` draws across the whole surface.
    /// `compare_viewport` draws the "after" half.
    ///
    /// Returns `false` when the surface wasn't presentable (occluded, timeout,
    /// outdated). The caller must retry, or a window that opens occluded stays
    /// blank.
    pub fn render(
        &mut self,
        image_viewport: Option<(u32, u32, u32, u32)>,
        compare_viewport: Option<(u32, u32, u32, u32)>,
        egui: Option<EguiPaint>,
    ) -> bool {
        use wgpu::CurrentSurfaceTexture as C;

        // Apply egui texture uploads even when the frame can't be presented.
        // egui sends each texture allocation only once. Dropping one leaves the
        // font atlas unallocated, and the next partial update panics.
        if let Some(paint) = &egui {
            for (id, delta) in &paint.textures_delta.set {
                self.egui_renderer
                    .update_texture(&self.device, &self.queue, *id, delta);
            }
        }

        let frame = match self.surface.get_current_texture() {
            C::Success(f) | C::Suboptimal(f) => f,
            C::Outdated | C::Lost => {
                self.surface.configure(&self.device, &self.config);
                self.free_egui_textures(&egui);
                return false;
            }
            C::Timeout | C::Occluded | C::Validation => {
                self.free_egui_textures(&egui);
                return false;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("enc") });

        if let Some(paint) = &egui {
            self.egui_renderer.update_buffers(
                &self.device,
                &self.queue,
                &mut encoder,
                &paint.paint_jobs,
                &paint.screen_descriptor,
            );
        }

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("image_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(CLEAR_COLOR),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(image_bind) = &self.image_bind {
                let (sw, sh) = (self.config.width, self.config.height);
                // Both pipelines share `pipeline_layout`, so the bind groups
                // below are the same for either.
                let pipeline = match self.image_pixel_format {
                    PixelFormat::Srgb8 => &self.pipeline,
                    PixelFormat::LinearF16 => &self.raw_pipeline,
                };
                let xform_bind = &self.xform_bind;
                let overlay_pipeline = &self.overlay_pipeline;
                let overlay_bind = self.overlay_bind.as_ref();
                let draw_into = |pass: &mut wgpu::RenderPass,
                                 vp: (u32, u32, u32, u32),
                                 adj: &wgpu::BindGroup| {
                    let (x, y, w, h) = vp;
                    let x = x.min(sw);
                    let y = y.min(sh);
                    let w = w.min(sw - x);
                    let h = h.min(sh - y);
                    if w == 0 || h == 0 {
                        return;
                    }
                    pass.set_scissor_rect(x, y, w, h);
                    pass.set_viewport(x as f32, y as f32, w as f32, h as f32, 0.0, 1.0);
                    pass.set_pipeline(pipeline);
                    pass.set_bind_group(0, image_bind, &[]);
                    pass.set_bind_group(1, xform_bind, &[]);
                    pass.set_bind_group(2, adj, &[]);
                    pass.set_bind_group(3, &self.touch_bind, &[]);
                    pass.draw(0..6, 0..1);
                    #[cfg(target_arch = "wasm32")]
                    crate::analytics::photo_drawn();

                    if let Some(overlay) = overlay_bind {
                        pass.set_pipeline(overlay_pipeline);
                        pass.set_bind_group(1, xform_bind, &[]);
                        pass.set_bind_group(2, adj, &[]);
                        pass.set_bind_group(3, overlay, &[]);
                        pass.draw(0..6, 0..1);
                    }
                };
                match image_viewport {
                    Some(vp) => {
                        draw_into(&mut pass, vp, &self.adj_bind);
                        if let Some(vp2) = compare_viewport {
                            draw_into(&mut pass, vp2, &self.adj_bind_b);
                        }
                    }
                    None => {
                        pass.set_pipeline(pipeline);
                        pass.set_bind_group(0, image_bind, &[]);
                        pass.set_bind_group(1, xform_bind, &[]);
                        pass.set_bind_group(2, &self.adj_bind, &[]);
                        pass.set_bind_group(3, &self.touch_bind, &[]);
                        pass.draw(0..6, 0..1);
                        if let Some(overlay) = overlay_bind {
                            pass.set_pipeline(overlay_pipeline);
                            pass.set_bind_group(1, xform_bind, &[]);
                            pass.set_bind_group(2, &self.adj_bind, &[]);
                            pass.set_bind_group(3, overlay, &[]);
                            pass.draw(0..6, 0..1);
                        }
                    }
                }
            }
        }

        if let Some(paint) = &egui {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            // egui_wgpu needs a 'static-lifetime pass for its render call.
            let mut pass = pass.forget_lifetime();
            self.egui_renderer
                .render(&mut pass, &paint.paint_jobs, &paint.screen_descriptor);
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();
        #[cfg(target_arch = "wasm32")]
        crate::analytics::presented();
        if crate::loader::timing_enabled() {
            // Only the first frames matter: time to first pixels, then time
            // to the sharp photo.
            use std::sync::atomic::{AtomicU32, Ordering};
            static FRAMES: AtomicU32 = AtomicU32::new(0);
            let n = FRAMES.fetch_add(1, Ordering::Relaxed);
            if n < 3 {
                crate::loader::mark(&format!("frame {n} presented"));
            }
        }

        // egui requires freeing textures after submit.
        self.free_egui_textures(&egui);
        true
    }

    /// Also called when the frame isn't presented, to keep egui's texture
    /// state in sync.
    fn free_egui_textures(&mut self, egui: &Option<EguiPaint>) {
        if let Some(paint) = egui {
            for id in &paint.textures_delta.free {
                self.egui_renderer.free_texture(id);
            }
        }
    }
}

