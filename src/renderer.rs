// SPDX-License-Identifier: GPL-3.0-or-later

//! wgpu rendering. The decoded image is uploaded once as a texture, and its mip
//! chain (for smooth zoom-out) is generated on the GPU by `mipgen.wgsl` rather
//! than on the CPU — `set_image` runs on the UI thread at the exact moment a
//! photo should appear, so it must not touch bulk pixels. Zoom/pan are applied
//! purely through a small transform uniform — no per-frame re-upload.

use std::sync::Arc;
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::develop::{GpuAdjust, GpuTouchUp};
use crate::image_decode::DecodedImage;

/// Texture format the decoded photo is uploaded as. Named because the mip-gen
/// render pipeline's color target has to match it exactly — it renders into the
/// image's own mip levels, not into the surface.
const IMAGE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Subject-selection overlay uniform. Field order MUST match `Overlay` in
/// `shader.wgsl`.
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
            // Green: the one hue that reads as "this region is chosen" without
            // colliding with the red of the touch-up markers.
            tint: [0.25, 1.0, 0.45, 1.0],
            invert: 0.0,
            // Strong enough to read at a glance, light enough to still see the
            // photo underneath — the point is judging where the edge falls.
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

/// Everything egui needs to paint a frame, produced by the app each redraw.
/// Coordinates are in physical pixels via `screen_descriptor`.
pub struct EguiPaint {
    pub textures_delta: egui::TexturesDelta,
    pub paint_jobs: Vec<egui::ClippedPrimitive>,
    pub screen_descriptor: egui_wgpu::ScreenDescriptor,
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,

    sampler: wgpu::Sampler,
    tex_bind_layout: wgpu::BindGroupLayout,

    xform_buf: wgpu::Buffer,
    xform_bind: wgpu::BindGroup,

    adj_buf: wgpu::Buffer,
    adj_bind: wgpu::BindGroup,

    /// Second adjustments uniform, used only for the "after" half of the
    /// before/after compare view (the primary `adj_*` holds the "before").
    adj_buf_b: wgpu::Buffer,
    adj_bind_b: wgpu::BindGroup,
    touch_buf: wgpu::Buffer,
    touch_bind: wgpu::BindGroup,

    /// Second pipeline drawing the subject-selection tint over the image. Kept
    /// entirely separate from the main pipeline so the selection can never
    /// affect rendered tone — it is a thing you look at, not an edit.
    overlay_pipeline: wgpu::RenderPipeline,
    overlay_bind_layout: wgpu::BindGroupLayout,
    overlay_buf: wgpu::Buffer,
    /// Bind group holding the overlay uniform *and* the current mask texture.
    /// `None` whenever there is no mask, which is also how the render pass
    /// knows to skip the overlay draw entirely.
    overlay_bind: Option<wgpu::BindGroup>,

    /// Bind group for the current image texture (None until first image loads).
    image_bind: Option<wgpu::BindGroup>,
    /// Current image dimensions in pixels.
    pub image_size: (u32, u32),

    pub max_dim: u32,

    /// Pipeline + sampler that build the image's mip chain on the GPU.
    mip_pipeline: wgpu::RenderPipeline,
    mip_sampler: wgpu::Sampler,

    /// egui paint backend; shares this Renderer's device/queue + surface format.
    egui_renderer: egui_wgpu::Renderer,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window).expect("create surface");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .expect("no adapter");

        // Request the adapter's real limits so large images aren't capped at 8192.
        let limits = adapter.limits();
        let max_dim = limits.max_texture_dimension_2d;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("device"),
            required_features: wgpu::Features::empty(),
            required_limits: limits,
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .expect("request device");

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

        // Overlay resources sit in group 3 alongside (never overlapping) the
        // touch-up storage buffer at binding 0 — see the note in shader.wgsl.
        let overlay_bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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

        // Same vertex shader as the image, so the tint lands on exactly the
        // same quad under the same zoom/pan/rotation. Group 0 (the image
        // texture) goes unused: the overlay reads the mask, not the photo.
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

        // Mip-chain generation runs on the GPU (see mipgen.wgsl). It reuses
        // `tex_bind_layout`'s shape — texture at 0, sampler at 1 — so it needs no
        // layout of its own.
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

        // Deliberately *not* the main `sampler`: this one must never follow the
        // mip chain it is in the middle of building, so its mipmap filter is
        // nearest and every view it reads is pinned to a single level.
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

        // egui's wgpu paint backend, built against the same device + surface
        // format so its textures/buffers interoperate with ours. No depth
        // buffer (we render none), single-sampled, one frame in flight.
        let egui_renderer =
            egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());

        Self {
            surface,
            device,
            queue,
            config,
            pipeline,
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
            max_dim,
            mip_pipeline,
            mip_sampler,
            egui_renderer,
        }
    }

    /// Surface format egui must target. The egui paint backend is built from
    /// this format inside `new`, so the app doesn't need it for T4.
    // TODO: T6 — may be needed if egui textures are registered app-side.
    #[allow(dead_code)]
    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.config.format
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
    /// This runs on the UI thread at the exact moment a newly-decoded photo
    /// should appear, so it must not do bulk pixel work: only level 0 is
    /// uploaded, and the rest of the chain is generated by the GPU. The previous
    /// CPU version cloned the whole RGBA buffer (~180 MB for a 45 MP file) and
    /// box-filtered every level in a scalar loop, which froze the window for
    /// roughly as long as the decode itself had taken.
    pub fn set_image(&mut self, img: &DecodedImage) {
        // web_time::Instant, not std::time::Instant — see loader.rs's
        // launched_at() doc comment for why (no OS clock on bare wasm32/64).
        let t0 = web_time::Instant::now();
        let (w, h) = (img.width, img.height);
        let mip_count = (32 - (w.max(h)).leading_zeros()).max(1); // floor(log2(max))+1

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
            format: IMAGE_FORMAT,
            // RENDER_ATTACHMENT so the mip levels below can be rendered into.
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });

        // Level 0 is the decoded pixels, uploaded straight from the caller's
        // buffer — no intermediate copy.
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &img.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * w),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );

        // Levels 1..n: each is drawn by sampling the level above at half size.
        // One view per level, each pinned to that single level, so a pass can
        // read level n-1 while writing level n without aliasing the resource.
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
                            // The triangle covers every texel, so there is
                            // nothing to preserve underneath.
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
                pass.set_pipeline(&self.mip_pipeline);
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

        // This runs on the UI thread at the moment the photo should appear, so
        // it is the one span the user actually feels as a freeze.
        crate::loader::mark(&format!(
            "set_image {w}x{h} ({mip_count} mips): took {:?}",
            t0.elapsed()
        ));
    }

    /// Update the zoom/pan/rotation transform uniform.
    pub fn set_transform(&mut self, scale: [f32; 2], offset: [f32; 2], rot: [f32; 4]) {
        self.queue.write_buffer(
            &self.xform_buf,
            0,
            bytemuck::bytes_of(&Transform { scale, offset, rot }),
        );
    }

    /// Update the non-destructive adjustments uniform.
    pub fn set_adjustments(&mut self, a: GpuAdjust) {
        self.queue
            .write_buffer(&self.adj_buf, 0, bytemuck::bytes_of(&a));
    }

    /// Update the second ("after") adjustments uniform for the compare view.
    pub fn set_adjustments_b(&mut self, a: GpuAdjust) {
        self.queue
            .write_buffer(&self.adj_buf_b, 0, bytemuck::bytes_of(&a));
    }

    /// Upload a single-channel selection mask, or clear it with `None`.
    ///
    /// `alpha` is `width * height` tightly-packed coverage bytes in the same
    /// texture space as the image (see `segmentation::Mask`), so the shader can
    /// sample it with the image's own UVs — no separate transform to keep in
    /// step. Clearing is what stops the overlay from drawing at all.
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

        // write_texture needs rows aligned to COPY_BYTES_PER_ROW_ALIGNMENT. At
        // one byte per texel that bites almost every time, unlike the RGBA8
        // image path where a multiple-of-64 width is enough — so pad here.
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

    /// Render one frame: the image pass (optionally confined to `image_viewport`)
    /// followed by the egui pass (if `egui` is `Some`), all in one submission.
    ///
    /// `image_viewport` is `(x, y, w, h)` in **physical pixels** with the origin
    /// at the surface's top-left. When `None`, the image draws across the whole
    /// surface as before. The image quad is clipped to this rect via
    /// `set_scissor_rect` so the loupe image can sit above a future filmstrip.
    /// Render one frame. Returns `true` if a frame was presented, `false` if the
    /// surface wasn't presentable this call (occluded/timeout/outdated) so the
    /// caller can schedule a retry — otherwise a window that opens occluded would
    /// stay blank forever (we'd skip every frame and never draw once revealed).
    pub fn render(
        &mut self,
        image_viewport: Option<(u32, u32, u32, u32)>,
        compare_viewport: Option<(u32, u32, u32, u32)>,
        egui: Option<EguiPaint>,
    ) -> bool {
        use wgpu::CurrentSurfaceTexture as C;

        // Apply egui texture uploads FIRST, before testing surface presentability.
        // `update_texture` only needs the device/queue (not the surface frame), and
        // egui's Context emits each allocation delta exactly once. If we dropped it
        // on an occluded/timeout frame (common while the window is appearing), the
        // font atlas would never be allocated, and the next frame's incremental
        // partial update would panic ("texture not allocated yet"). Keeping egui's
        // texture state in sync every frame — even non-presented ones — avoids that.
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

        // egui vertex/index buffers for this frame's paint jobs (textures already
        // uploaded above).
        if let Some(paint) = &egui {
            self.egui_renderer.update_buffers(
                &self.device,
                &self.queue,
                &mut encoder,
                &paint.paint_jobs,
                &paint.screen_descriptor,
            );
        }

        // Pass 1: the image (clear the surface, draw the quad scissored to the rect).
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("image_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.07,
                            g: 0.07,
                            b: 0.08,
                            a: 1.0,
                        }),
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
                let pipeline = &self.pipeline;
                let xform_bind = &self.xform_bind;
                // Draw the quad into a viewport rect (clamped to the surface) with
                // the given adjustments bind group. Shared by the single-image and
                // both compare halves.
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

                    // Selection tint, alpha-blended straight over the pixels
                    // just drawn — same quad, same viewport, same transform, so
                    // it tracks zoom and pan for free.
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
                        // Left half (or full image): the primary adjustments.
                        draw_into(&mut pass, vp, &self.adj_bind);
                        // Right half in compare mode: the "after" adjustments.
                        if let Some(vp2) = compare_viewport {
                            draw_into(&mut pass, vp2, &self.adj_bind_b);
                        }
                    }
                    None => {
                        // Full-surface draw (no scissor), primary adjustments.
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

        // Pass 2: egui, loaded (not cleared) on top of the image.
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
        if crate::loader::timing_enabled() {
            // Only the first few frames matter here: the question is how long
            // after launch the user sees *anything*, and then how long until
            // that anything is the sharp photo rather than a placeholder.
            use std::sync::atomic::{AtomicU32, Ordering};
            static FRAMES: AtomicU32 = AtomicU32::new(0);
            let n = FRAMES.fetch_add(1, Ordering::Relaxed);
            if n < 3 {
                crate::loader::mark(&format!("frame {n} presented"));
            }
        }

        // Free egui textures dropped this frame (after submit, per egui docs).
        self.free_egui_textures(&egui);
        true
    }

    /// Free any egui textures dropped this frame. Called on both the presented
    /// path (after submit) and the early-return paths, so egui's texture state
    /// stays in sync even when the surface wasn't presentable.
    fn free_egui_textures(&mut self, egui: &Option<EguiPaint>) {
        if let Some(paint) = egui {
            for id in &paint.textures_delta.free {
                self.egui_renderer.free_texture(id);
            }
        }
    }
}

