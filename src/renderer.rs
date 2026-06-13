//! wgpu rendering. The decoded image is uploaded once as a texture (with a
//! CPU-generated mip chain for smooth zoom-out). Zoom/pan are applied purely
//! through a small transform uniform — no per-frame re-upload.

use std::sync::Arc;
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::image_decode::DecodedImage;

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

    /// Bind group for the current image texture (None until first image loads).
    image_bind: Option<wgpu::BindGroup>,
    /// Current image dimensions in pixels.
    pub image_size: (u32, u32),

    pub max_dim: u32,

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

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pl"),
            bind_group_layouts: &[Some(&tex_bind_layout), Some(&xform_bind_layout)],
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
            image_bind: None,
            image_size: (0, 0),
            max_dim,
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
    pub fn set_image(&mut self, img: &DecodedImage) {
        let (w, h) = (img.width, img.height);
        let mip_count = (32 - (w.max(h)).leading_zeros()).max(1); // floor(log2(max))+1

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("image"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: mip_count,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        // Upload mip 0 then CPU-generate each subsequent level by 2x box filter.
        let mut level_pixels = img.rgba.clone();
        let mut lw = w;
        let mut lh = h;
        for level in 0..mip_count {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: level,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &level_pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * lw),
                    rows_per_image: Some(lh),
                },
                wgpu::Extent3d { width: lw, height: lh, depth_or_array_layers: 1 },
            );
            if level + 1 < mip_count {
                let (np, nw, nh) = downsample2x(&level_pixels, lw, lh);
                level_pixels = np;
                lw = nw;
                lh = nh;
            }
        }

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image_bg"),
            layout: &self.tex_bind_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        self.image_bind = Some(bind);
        self.image_size = (w, h);
    }

    /// Update the zoom/pan/rotation transform uniform.
    pub fn set_transform(&mut self, scale: [f32; 2], offset: [f32; 2], rot: [f32; 4]) {
        self.queue.write_buffer(
            &self.xform_buf,
            0,
            bytemuck::bytes_of(&Transform { scale, offset, rot }),
        );
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
        egui: Option<EguiPaint>,
    ) -> bool {
        use wgpu::CurrentSurfaceTexture as C;
        let frame = match self.surface.get_current_texture() {
            C::Success(f) | C::Suboptimal(f) => f,
            C::Outdated | C::Lost => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            C::Timeout | C::Occluded | C::Validation => return false,
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("enc") });

        // Upload any egui texture changes before the passes (must precede use).
        if let Some(paint) = &egui {
            for (id, delta) in &paint.textures_delta.set {
                self.egui_renderer
                    .update_texture(&self.device, &self.queue, *id, delta);
            }
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
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.07, g: 0.07, b: 0.08, a: 1.0 }),
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
                // Confine the image to its viewport rect (clamped to the surface).
                if let Some((x, y, w, h)) = image_viewport {
                    let sw = self.config.width;
                    let sh = self.config.height;
                    let x = x.min(sw);
                    let y = y.min(sh);
                    let w = w.min(sw - x);
                    let h = h.min(sh - y);
                    if w == 0 || h == 0 {
                        // Degenerate rect: draw nothing this frame.
                    } else {
                        pass.set_scissor_rect(x, y, w, h);
                        pass.set_viewport(x as f32, y as f32, w as f32, h as f32, 0.0, 1.0);
                        pass.set_pipeline(&self.pipeline);
                        pass.set_bind_group(0, image_bind, &[]);
                        pass.set_bind_group(1, &self.xform_bind, &[]);
                        pass.draw(0..6, 0..1);
                    }
                } else {
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, image_bind, &[]);
                    pass.set_bind_group(1, &self.xform_bind, &[]);
                    pass.draw(0..6, 0..1);
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

        // Free egui textures dropped this frame (after submit, per egui docs).
        if let Some(paint) = &egui {
            for id in &paint.textures_delta.free {
                self.egui_renderer.free_texture(id);
            }
        }
        true
    }
}

/// Box-filter downsample an RGBA8 buffer to half size (min 1px).
fn downsample2x(src: &[u8], w: u32, h: u32) -> (Vec<u8>, u32, u32) {
    let nw = (w / 2).max(1);
    let nh = (h / 2).max(1);
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        for x in 0..nw {
            for c in 0..4usize {
                let sx0 = (x * 2).min(w - 1);
                let sy0 = (y * 2).min(h - 1);
                let sx1 = (x * 2 + 1).min(w - 1);
                let sy1 = (y * 2 + 1).min(h - 1);
                let idx = |px: u32, py: u32| ((py * w + px) * 4) as usize + c;
                let sum = src[idx(sx0, sy0)] as u32
                    + src[idx(sx1, sy0)] as u32
                    + src[idx(sx0, sy1)] as u32
                    + src[idx(sx1, sy1)] as u32;
                out[((y * nw + x) * 4) as usize + c] = (sum / 4) as u8;
            }
        }
    }
    (out, nw, nh)
}
