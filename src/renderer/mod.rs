// SPDX-License-Identifier: MIT OR Apache-2.0

//! wgpu rendering of the loupe image, plus the egui pass on top. Each decoded
//! image is uploaded once as a texture with a GPU-built mip chain. Zoom and pan
//! only update a small transform uniform. Every platform's decoder reaches
//! `set_image` through `App::upload_shown`. `shader.wgsl` draws every image
//! except a scene-linear RAW (`PixelFormat::LinearF16`), which uses
//! `raw_shader.wgsl`. Half-float images upload as `Rgba16Float`.

use std::collections::HashMap;
use std::sync::Arc;
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::decode::image_decode::{DecodedImage, PixelFormat};
use crate::develop::curve::ToneCurve;
use crate::develop::{BaseProbe, GpuAdjust, GpuCurve, GpuMask, GpuTouchUp, Grid, PhotoMasks};

/// A storage binding can't be empty, so the coverage buffer starts at one
/// word and grows with the first detected mask.
const MIN_COVERAGE_BUF: u64 = 4;

fn coverage_buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("coverage"),
        size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Group 3 for the image passes: touch-ups at 0, masks at 4, coverage at 5.
fn touch_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    touch_buf: &wgpu::Buffer,
    mask_buf: &wgpu::Buffer,
    coverage_buf: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("touch_bg"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: touch_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: mask_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: coverage_buf.as_entire_binding(),
            },
        ],
    })
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod match_test;
mod raw_render;

/// Texture format for a `PixelFormat::Srgb8` photo. The mip-gen pipeline
/// renders into the image's mip levels, so its target must match this.
const IMAGE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// The base layer's grid textures. Core WebGPU renders to it but can't
/// filter it, so every read is a `textureLoad`.
const BASE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;

/// The base-layer passes' uniform. Must match `BasePass` in base.wgsl.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct BasePass {
    grid: [u32; 2],
    src: [u32; 2],
}

/// What the base and fine layers are built from besides the image: a layer
/// rebuilds only when one of these changes, so a slider drag never does.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
struct BaseInputs {
    ca_red: f32,
    ca_blue: f32,
    touch_count: u32,
    /// Bumped by `set_touchups` when the spots change.
    touch_gen: u64,
    /// The inputs came from the compare view's edited half (`adj_bind_b`),
    /// which the passes then bind. The layer follows the edited photo;
    /// the "before" half never reads it.
    from_b: bool,
    /// Which layers something reads, base then fine: an `Adjustments`
    /// field that needs one, or the match test's probe. Nothing builds a
    /// layer otherwise.
    wanted: [bool; 2],
}

impl BaseInputs {
    fn of(a: &GpuAdjust, from_b: bool, touch_gen: u64) -> Self {
        let probe = BaseProbe::from_gpu(a.base_probe);
        BaseInputs {
            ca_red: a.ca_red,
            ca_blue: a.ca_blue,
            touch_count: a._pad0 as u32,
            touch_gen,
            from_b,
            wanted: [
                a.base_on != 0.0 || probe.reads_base(),
                a.fine_on != 0.0 || probe == BaseProbe::Fine,
            ],
        }
    }

    /// What a layer is built from, without which layers are wanted.
    fn source(self) -> Self {
        BaseInputs {
            wanted: [false; 2],
            ..self
        }
    }
}

/// One of the current image's layer grids on the GPU, the base or the fine
/// one. `fs_base_reduce` writes `a`,
/// `fs_base_blur_h` reads `a` into `b`, and `fs_base_blur_v` reads `b` back
/// into `a`, which the Loupe binds.
struct BaseGpu {
    view_a: wgpu::TextureView,
    view_b: wgpu::TextureView,
    /// The image and the main sampler, without the base texture, so `a` is
    /// never bound while it is a render target.
    image_bind: wgpu::BindGroup,
    /// `grid_in` = `a`, for the horizontal blur.
    read_a: wgpu::BindGroup,
    /// `grid_in` = `b`, for the vertical blur. The reduce binds it too and
    /// ignores it.
    read_b: wgpu::BindGroup,
}

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

#[cfg(not(target_arch = "wasm32"))]
pub struct RgbFrame {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

#[cfg(not(target_arch = "wasm32"))]
impl RgbFrame {
    pub fn to_png(&self) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, self.width, self.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer
            .write_image_data(&self.rgb)
            .map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
        Ok(bytes)
    }
}

/// Everything egui needs to paint one frame.
pub struct EguiPaint {
    pub textures_delta: egui::TexturesDelta,
    pub paint_jobs: Vec<egui::ClippedPrimitive>,
    pub screen_descriptor: egui_wgpu::ScreenDescriptor,
}

pub struct Renderer {
    /// The loupe backdrop, sRGB-encoded since the surface does no encoding of
    /// its own. The theme sets it.
    clear_color: wgpu::Color,
    /// `None` for a headless renderer, which draws only into `offscreen`.
    surface: Option<wgpu::Surface<'static>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    /// Drawn instead of `pipeline` for `PixelFormat::LinearF16` images.
    raw_pipeline: wgpu::RenderPipeline,

    sampler: wgpu::Sampler,
    tex_bind_layout: wgpu::BindGroupLayout,
    /// `tex_bind_layout` plus the base layer at binding 2 and the fine layer
    /// at 3, for the Loupe.
    image_bind_layout: wgpu::BindGroupLayout,

    xform_buf: wgpu::Buffer,
    xform_bind: wgpu::BindGroup,

    adj_buf: wgpu::Buffer,
    curve_buf: wgpu::Buffer,
    adj_bind: wgpu::BindGroup,

    /// Adjustments for the "after" half of the compare view.
    adj_buf_b: wgpu::Buffer,
    curve_buf_b: wgpu::Buffer,
    adj_bind_b: wgpu::BindGroup,
    touch_buf: wgpu::Buffer,
    /// The masks, beside the touch-ups in group 3. See `set_masks`.
    mask_buf: wgpu::Buffer,
    /// Detected and brush masks' coverage, packed by
    /// `PhotoMasks::pack_coverage`. Grows by powers of two up to
    /// `MAX_COVERAGE_BYTES`; `coverage_key` is the
    /// `PhotoMasks::coverage_key` of the last upload, so a slider tick that
    /// changes no coverage uploads none.
    coverage_buf: wgpu::Buffer,
    coverage_key: u64,
    touch_bind_layout: wgpu::BindGroupLayout,
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

    base_reduce: wgpu::RenderPipeline,
    base_blur_h: wgpu::RenderPipeline,
    base_blur_v: wgpu::RenderPipeline,
    base_grid_layout: wgpu::BindGroupLayout,
    /// The base layer. `None` until the first image loads.
    base: Option<BaseGpu>,
    /// The fine layer, made the first time an edit reads it: about 28 MB on
    /// a 3:2 photo, which most photos never need. Until then binding 3
    /// holds `fine_placeholder`.
    fine: Option<BaseGpu>,
    /// The current image's level-0 view and fine grid, to make `fine`.
    image_view: Option<(wgpu::TextureView, Grid)>,
    fine_placeholder: wgpu::TextureView,
    /// The inputs the last `set_adjustments` and `set_touchups` asked for.
    base_wanted: BaseInputs,
    /// The `source()` each layer, base then fine, was last built from;
    /// `None` forces a build.
    layers_built_for: [Option<BaseInputs>; 2],
    /// The spots `set_touchups` last uploaded, to tell a change from a
    /// repeat.
    touchups: Vec<GpuTouchUp>,
    /// Layer builds encoded so far, base and fine.
    base_builds: u64,

    egui_renderer: egui_wgpu::Renderer,

    offscreen: Option<wgpu::Texture>,

    /// Grid thumbnails handed to egui as user textures. `register_native_texture`
    /// stores only a bind group, so the texture has to be kept alive here until
    /// `free_thumb` drops both.
    thumb_textures: HashMap<egui::TextureId, wgpu::Texture>,

    /// The photo as the last frame drew it, which egui fades out over the
    /// next one in a slide show, and the viewports that frame drew into.
    fade_from: Option<(egui::TextureId, wgpu::Texture)>,
    last_viewports: [Option<(u32, u32, u32, u32)>; 2],
}

impl Renderer {
    /// `async` because the browser main thread can't block. Native callers
    /// wrap it in `pollster::block_on`; wasm awaits it in `spawn_local`.
    ///
    /// The caller passes `size` because on wasm32 winit's `inner_size()` is
    /// `(0, 0)` until the browser's `ResizeObserver` first fires. A 1x1 surface
    /// makes every render pass fail WebGPU's scissor-rect validation.
    ///
    /// Errs when the platform has no usable GPU surface, adapter or device.
    pub async fn new(
        window: Arc<Window>,
        size: winit::dpi::PhysicalSize<u32>,
    ) -> Result<Self, String> {
        let instance = wgpu::Instance::default();
        // A browser with no WebGPU at all fails here, before `request_adapter`
        // is ever reached, so this is the report that covers "can't run".
        let surface = instance.create_surface(window).map_err(|e| {
            #[cfg(target_arch = "wasm32")]
            crate::web::analytics::property("webgpu_unsupported", "reason", "surface_unavailable");
            format!("create surface: {e}")
        })?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .map_err(|e| {
                #[cfg(target_arch = "wasm32")]
                crate::web::analytics::property(
                    "webgpu_unsupported",
                    "reason",
                    "adapter_unavailable",
                );
                format!("no adapter: {e}")
            })?;

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
            .map_err(|e| {
                #[cfg(target_arch = "wasm32")]
                crate::web::analytics::property(
                    "webgpu_unsupported",
                    "reason",
                    "device_unavailable",
                );
                format!("request device: {e}")
            })?;

        // A plain, non-sRGB surface on every platform, because WebGPU canvases
        // never offer an sRGB one. Every shader therefore writes sRGB-encoded
        // values itself, and egui switches to its gamma-space path.
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .or(caps.formats.first().copied())
            .ok_or("the surface supports no texture format")?;
        let alpha_mode = caps
            .alpha_modes
            .first()
            .copied()
            .ok_or("the surface supports no alpha mode")?;

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.min(max_dim).max(1),
            height: size.height.min(max_dim).max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        Ok(Self::build(device, queue, Some(surface), config, max_dim))
    }

    /// A renderer with no window that draws into an `Rgba8Unorm` offscreen
    /// texture, for tests that read the Loupe's pixels back.
    #[cfg(test)]
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new_headless(width: u32, height: u32) -> Result<Self, String> {
        pollster::block_on(async {
            let instance = wgpu::Instance::default();
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: None,
                    force_fallback_adapter: false,
                })
                .await
                .map_err(|e| format!("no adapter: {e}"))?;
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
                .map_err(|e| format!("request device: {e}"))?;
            let config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: wgpu::TextureFormat::Rgba8Unorm,
                width: width.min(max_dim).max(1),
                height: height.min(max_dim).max(1),
                present_mode: wgpu::PresentMode::AutoVsync,
                alpha_mode: wgpu::CompositeAlphaMode::Opaque,
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            };
            let mut r = Self::build(device, queue, None, config, max_dim);
            r.render_offscreen();
            Ok(r)
        })
    }

    /// Everything past the device and the surface format: shaders,
    /// pipelines, buffers and the egui renderer.
    fn build(
        device: wgpu::Device,
        queue: wgpu::Queue,
        surface: Option<wgpu::Surface<'static>>,
        config: wgpu::SurfaceConfiguration,
        max_dim: u32,
    ) -> Self {
        let format = config.format;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shader"),
            source: wgpu::ShaderSource::Wgsl(
                concat!(
                    include_str!("../shaders/loupe_common.wgsl"),
                    include_str!("../shaders/shader.wgsl")
                )
                .into(),
            ),
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

        let base_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let image_bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image_bgl"),
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
                base_entry(2),
                base_entry(3),
            ],
        });
        let base_grid_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("base_grid_bgl"),
            entries: &[
                base_entry(0),
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
            entries: &[0, 1].map(|binding| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }),
        });

        let touch_bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("touch_bgl"),
            // Masks at 4 and their coverage at 5, clear of the overlay's 1
            // to 3 in the same group.
            entries: &[0, 4, 5].map(|binding| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }),
        });

        // Overlay bindings start at 1 so they never overlap the touch-up
        // buffer at group 3 binding 0. See loupe_common.wgsl.
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
                Some(&image_bind_layout),
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
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/mipgen.wgsl").into()),
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

        let base_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("base_shader"),
            source: wgpu::ShaderSource::Wgsl(
                concat!(
                    include_str!("../shaders/loupe_common.wgsl"),
                    include_str!("../shaders/base.wgsl")
                )
                .into(),
            ),
        });
        let base_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("base_pl"),
            bind_group_layouts: &[
                Some(&tex_bind_layout),
                Some(&base_grid_layout),
                Some(&adj_bind_layout),
                Some(&touch_bind_layout),
            ],
            immediate_size: 0,
        });
        let base_pipeline = |entry_point: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry_point),
                layout: Some(&base_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &base_shader,
                    entry_point: Some("vs_base"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &base_shader,
                    entry_point: Some(entry_point),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: BASE_FORMAT,
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
        };
        let base_reduce = base_pipeline("fs_base_reduce");
        let base_blur_h = base_pipeline("fs_base_blur_h");
        let base_blur_v = base_pipeline("fs_base_blur_v");
        let fine_placeholder = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("fine_placeholder"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: BASE_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default());

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
        let curve_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("curve"),
            contents: bytemuck::bytes_of(&GpuCurve::from(&ToneCurve::default())),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let adj_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("adj_bg"),
            layout: &adj_bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: adj_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: curve_buf.as_entire_binding(),
                },
            ],
        });

        let adj_buf_b = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("adj_b"),
            contents: bytemuck::bytes_of(&GpuAdjust::default()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let curve_buf_b = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("curve_b"),
            contents: bytemuck::bytes_of(&GpuCurve::from(&ToneCurve::default())),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let adj_bind_b = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("adj_bg_b"),
            layout: &adj_bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: adj_buf_b.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: curve_buf_b.as_entire_binding(),
                },
            ],
        });

        let touch_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("touchups"),
            size: (crate::develop::MAX_TOUCHUPS * std::mem::size_of::<GpuTouchUp>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mask_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("masks"),
            size: (crate::develop::MAX_MASKS * std::mem::size_of::<GpuMask>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let coverage_buf = coverage_buffer(&device, MIN_COVERAGE_BUF);
        let touch_bind = touch_bind_group(
            &device,
            &touch_bind_layout,
            &touch_buf,
            &mask_buf,
            &coverage_buf,
        );

        let overlay_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("overlay"),
            contents: bytemuck::bytes_of(&OverlayParams::default()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let egui_renderer =
            egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());

        Self {
            clear_color: wgpu::Color::BLACK,
            surface,
            device,
            queue,
            config,
            pipeline,
            raw_pipeline,
            sampler,
            tex_bind_layout,
            image_bind_layout,
            xform_buf,
            xform_bind,
            adj_buf,
            curve_buf,
            adj_bind,
            adj_buf_b,
            curve_buf_b,
            adj_bind_b,
            touch_buf,
            mask_buf,
            coverage_buf,
            coverage_key: 0,
            touch_bind_layout,
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
            base_reduce,
            base_blur_h,
            base_blur_v,
            base_grid_layout,
            base: None,
            fine: None,
            image_view: None,
            fine_placeholder,
            base_wanted: BaseInputs::default(),
            layers_built_for: [None; 2],
            touchups: Vec::new(),
            base_builds: 0,
            egui_renderer,
            offscreen: None,
            thumb_textures: HashMap::new(),
            fade_from: None,
            last_viewports: [None; 2],
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn render_offscreen(&mut self) {
        self.offscreen = Some(self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size: wgpu::Extent3d {
                width: self.config.width,
                height: self.config.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        }));
    }

    /// Present to the window again after `render_offscreen`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn end_offscreen(&mut self) {
        self.offscreen = None;
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn read_offscreen_blocking(&self) -> Option<RgbFrame> {
        let texture = self.offscreen.as_ref()?;
        let swap_rb = match self.config.format {
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => true,
            wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba8UnormSrgb => false,
            _ => return None,
        };
        let (w, h) = (self.config.width, self.config.height);
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_row = (w * 4).div_ceil(align) * align;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded_row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("readback_encoder"),
            });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
        let mapped = slice.get_mapped_range();
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h as usize {
            let row = &mapped[y * padded_row as usize..][..(w * 4) as usize];
            for px in row.as_chunks::<4>().0 {
                let (r, b) = if swap_rb {
                    (px[2], px[0])
                } else {
                    (px[0], px[2])
                };
                rgb.extend_from_slice(&[r, px[1], b]);
            }
        }
        Some(RgbFrame {
            width: w,
            height: h,
            rgb,
        })
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
    pub fn upload_thumb(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Option<egui::TextureId> {
        let expected = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
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
        // wgpu panics on a surface past the texture limit. A window that
        // large (or a `--drive` `size` step) gets a surface at the limit,
        // stretched to fill.
        self.config.width = w.min(self.max_dim);
        self.config.height = h.min(self.max_dim);
        if let Some(surface) = &self.surface {
            surface.configure(&self.device, &self.config);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if self.offscreen.is_some() {
            self.render_offscreen();
        }
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
        self.fine = None;
        self.image_view = None;
        self.layers_built_for = [None; 2];
        let Some((grid, fine_grid)) = Grid::for_image(w, h).zip(Grid::fine(w, h)) else {
            eprintln!("set_image: nothing to draw in an empty {w}x{h} image");
            self.image_bind = None;
            self.base = None;
            self.image_size = (0, 0);
            return;
        };
        let mip_count = (32 - (w.max(h)).leading_zeros()).max(1); // floor(log2(max))+1

        let (format, bytes_per_pixel, mip_pipeline): (
            wgpu::TextureFormat,
            u32,
            &wgpu::RenderPipeline,
        ) = match img.pixel_format {
            PixelFormat::Srgb8 => (IMAGE_FORMAT, 4, &self.mip_pipeline),
            PixelFormat::LinearF16 | PixelFormat::TonedF16 => (
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

        // Unlike a buffer-to-texture copy, `write_texture` takes tightly
        // packed rows: wgpu aligns them itself as it fills its staging buffer.
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
                bytes_per_row: Some(bytes_per_pixel * w),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );

        self.build_mips(&texture, mip_count, mip_pipeline);

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let base = self.create_base(&view, grid, (w, h));
        self.image_bind = Some(self.bind_image(&view, &base.view_a, &self.fine_placeholder));
        self.base = Some(base);
        self.image_view = Some((view, fine_grid));
        self.image_size = (w, h);

        // The user feels this UI-thread span as a freeze.
        crate::jobs::loader::mark(&format!(
            "set_image {w}x{h} ({mip_count} mips): took {:?}",
            t0.elapsed()
        ));
    }

    /// The Loupe's group 0: the image, its sampler, and the base and fine
    /// layers.
    fn bind_image(
        &self,
        image: &wgpu::TextureView,
        base: &wgpu::TextureView,
        fine: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image_bg"),
            layout: &self.image_bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(image),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(base),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(fine),
                },
            ],
        })
    }

    /// Make the fine layer for the current image and bind it, unless it
    /// exists.
    fn ensure_fine(&mut self) {
        if self.fine.is_some() {
            return;
        }
        let (Some((view, grid)), Some(base)) = (&self.image_view, &self.base) else {
            return;
        };
        let fine = self.create_base(view, *grid, self.image_size);
        self.image_bind = Some(self.bind_image(view, &base.view_a, &fine.view_a));
        self.fine = Some(fine);
    }

    /// The grid textures and bind groups for an image whose level-0 view is
    /// `image`. The passes run in `render()`.
    fn create_base(&self, image: &wgpu::TextureView, grid: Grid, src: (u32, u32)) -> BaseGpu {
        let grid_texture = |label| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: grid.width(),
                    height: grid.height(),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: BASE_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
        };
        let view_a = grid_texture("base_a").create_view(&wgpu::TextureViewDescriptor::default());
        let view_b = grid_texture("base_b").create_view(&wgpu::TextureViewDescriptor::default());
        let pass = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("base_pass"),
                contents: bytemuck::bytes_of(&BasePass {
                    grid: [grid.width(), grid.height()],
                    src: [src.0, src.1],
                }),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let read = |view: &wgpu::TextureView| {
            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("base_grid_bg"),
                layout: &self.base_grid_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: pass.as_entire_binding(),
                    },
                ],
            })
        };
        let image_bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("base_image_bg"),
            layout: &self.tex_bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(image),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        BaseGpu {
            read_a: read(&view_a),
            read_b: read(&view_b),
            view_a,
            view_b,
            image_bind,
        }
    }

    /// Encode the base layer's three passes for the current adjustments and
    /// touch-ups. They read the same uniforms as the Loupe's image pass.
    fn encode_base(&self, encoder: &mut wgpu::CommandEncoder, base: &BaseGpu) {
        let passes = [
            (&self.base_reduce, &base.read_b, &base.view_a),
            (&self.base_blur_h, &base.read_a, &base.view_b),
            (&self.base_blur_v, &base.read_b, &base.view_a),
        ];
        for (pipeline, grid_in, target) in passes {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("base_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
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
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &base.image_bind, &[]);
            pass.set_bind_group(1, grid_in, &[]);
            let adj = if self.base_wanted.from_b {
                &self.adj_bind_b
            } else {
                &self.adj_bind
            };
            pass.set_bind_group(2, adj, &[]);
            pass.set_bind_group(3, &self.touch_bind, &[]);
            pass.draw(0..3, 0..1);
        }
    }

    /// Render each mip level from the one above with `mip_pipeline`.
    fn build_mips(
        &self,
        texture: &wgpu::Texture,
        mip_count: u32,
        mip_pipeline: &wgpu::RenderPipeline,
    ) {
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
    }

    /// Upload an image as an egui user texture. `mipped` builds the mip
    /// chain through `mipgen.wgsl` and registers a sampler that reads it;
    /// otherwise level 0 only with the plain linear sampler. A half-float
    /// RAW is encoded to sRGB8 first.
    pub fn upload_egui_image(
        &mut self,
        img: &DecodedImage,
        mipped: bool,
    ) -> Option<egui::TextureId> {
        let rgba = crate::develop::image_ops::to_srgb8(img);
        let (w, h) = (img.width, img.height);
        if rgba.len() < (4 * w * h) as usize {
            return None;
        }
        let mip_count = if mipped {
            (32 - (w.max(h)).leading_zeros()).max(1)
        } else {
            1
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("egui_image"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: mip_count,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: IMAGE_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            // egui samples raw bytes, so it gets a non-sRGB view of the same texels.
            view_formats: &[wgpu::TextureFormat::Rgba8Unorm],
        });
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let row = 4 * w;
        let padded_row = row.div_ceil(align) * align;
        let level0: std::borrow::Cow<[u8]> = if padded_row == row {
            rgba
        } else {
            let mut padded = vec![0u8; (padded_row * h) as usize];
            for y in 0..h as usize {
                let src = y * row as usize;
                let dst = y * padded_row as usize;
                padded[dst..dst + row as usize].copy_from_slice(&rgba[src..src + row as usize]);
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
        let t0 = web_time::Instant::now();
        self.build_mips(&texture, mip_count, &self.mip_pipeline);
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(wgpu::TextureFormat::Rgba8Unorm),
            ..Default::default()
        });
        let id = self
            .egui_renderer
            .register_native_texture_with_sampler_options(
                &self.device,
                &view,
                wgpu::SamplerDescriptor {
                    label: Some("egui_image_sampler"),
                    mag_filter: wgpu::FilterMode::Linear,
                    min_filter: wgpu::FilterMode::Linear,
                    mipmap_filter: if mipped {
                        wgpu::MipmapFilterMode::Linear
                    } else {
                        wgpu::MipmapFilterMode::Nearest
                    },
                    ..Default::default()
                },
            );
        eprintln!(
            "compare: upload {w}x{h} mipped={mipped} mips={mip_count} took {:?}",
            t0.elapsed()
        );
        self.thumb_textures.insert(id, texture);
        Some(id)
    }

    pub fn set_transform(&mut self, scale: [f32; 2], offset: [f32; 2], rot: [f32; 4]) {
        self.queue.write_buffer(
            &self.xform_buf,
            0,
            bytemuck::bytes_of(&Transform { scale, offset, rot }),
        );
    }

    /// Uploads `a`, and `curve`'s table when `a.curve_on` says the shader
    /// reads it.
    pub fn set_adjustments(&mut self, a: GpuAdjust, curve: &ToneCurve) {
        Self::write_adjustments(&self.queue, &self.adj_buf, &self.curve_buf, a, curve);
        self.base_wanted = BaseInputs::of(&a, false, self.base_wanted.touch_gen);
    }

    /// Adjustments for the "after" half of the compare view. The base layer
    /// follows this half while compare is on.
    pub fn set_adjustments_b(&mut self, a: GpuAdjust, curve: &ToneCurve) {
        Self::write_adjustments(&self.queue, &self.adj_buf_b, &self.curve_buf_b, a, curve);
        self.base_wanted = BaseInputs::of(&a, true, self.base_wanted.touch_gen);
    }

    fn write_adjustments(
        queue: &wgpu::Queue,
        adj_buf: &wgpu::Buffer,
        curve_buf: &wgpu::Buffer,
        a: GpuAdjust,
        curve: &ToneCurve,
    ) {
        queue.write_buffer(adj_buf, 0, bytemuck::bytes_of(&a));
        if a.curve_on != 0.0 {
            queue.write_buffer(curve_buf, 0, bytemuck::bytes_of(&GpuCurve::from(curve)));
        }
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

    /// How the overlay tints: `tint` is linear RGB, and `inverted` tints
    /// the background instead of the mask.
    pub fn set_selection_style(&mut self, inverted: bool, tint: [f32; 3]) {
        self.queue.write_buffer(
            &self.overlay_buf,
            0,
            bytemuck::bytes_of(&OverlayParams {
                tint: [tint[0], tint[1], tint[2], 1.0],
                invert: if inverted { 1.0 } else { 0.0 },
                ..OverlayParams::default()
            }),
        );
    }

    /// Uploads the spots. The app calls this on every slider change, so the
    /// base layer rebuilds only when the spots differ from the last upload.
    pub fn set_touchups(&mut self, touchups: &[GpuTouchUp]) {
        let bytes: &[u8] = bytemuck::cast_slice(touchups);
        if bytes == bytemuck::cast_slice::<GpuTouchUp, u8>(&self.touchups) {
            return;
        }
        if !touchups.is_empty() {
            self.queue.write_buffer(&self.touch_buf, 0, bytes);
        }
        self.touchups = touchups.to_vec();
        self.base_wanted.touch_gen += 1;
    }

    /// Uploads the masks and their coverage. Masks change no layer, so
    /// nothing rebuilds; the uniform's `mask_count` says how many the shader
    /// reads. The masks go up on every call, since sliders change them; the
    /// coverage only when its key changed, and the buffer grows (and the
    /// bind group is rebuilt) only when it must.
    pub fn set_masks(&mut self, masks: &PhotoMasks) {
        let gpu = masks.gpu_masks();
        let gpu = gpu.get(..crate::develop::MAX_MASKS).unwrap_or(&gpu);
        if !gpu.is_empty() {
            self.queue
                .write_buffer(&self.mask_buf, 0, bytemuck::cast_slice(gpu));
        }
        let key = masks.coverage_key();
        if key == self.coverage_key {
            return;
        }
        let packed = masks.pack_coverage();
        let bytes: &[u8] = bytemuck::cast_slice(&packed);
        if bytes.is_empty() {
            self.coverage_key = key;
            return;
        }
        if bytes.len() as u64 > self.coverage_buf.size() {
            let size = bytes
                .len()
                .next_power_of_two()
                .min(crate::develop::coverage::MAX_COVERAGE_BYTES) as u64;
            self.coverage_buf = coverage_buffer(&self.device, size);
            self.touch_bind = touch_bind_group(
                &self.device,
                &self.touch_bind_layout,
                &self.touch_buf,
                &self.mask_buf,
                &self.coverage_buf,
            );
        }
        let fits = bytes
            .get(..self.coverage_buf.size() as usize)
            .unwrap_or(bytes);
        self.queue.write_buffer(&self.coverage_buf, 0, fits);
        self.coverage_key = key;
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
    pub fn set_clear_color(&mut self, c: egui::Color32) {
        let v = |x: u8| x as f64 / 255.0;
        self.clear_color = wgpu::Color {
            r: v(c.r()),
            g: v(c.g()),
            b: v(c.b()),
            a: 1.0,
        };
    }

    /// Draws the photo as the last frame showed it into a texture egui can
    /// paint, for a slide show to fade out over the next photo. Replaces the
    /// last one. `None` with no photo.
    pub fn snapshot_photo(&mut self) -> Option<egui::TextureId> {
        self.free_snapshot();
        self.image_bind.as_ref()?;
        let format = self.config.format;
        // egui reads sRGB-encoded bytes, which a plain view of an sRGB
        // texture gives back as they are stored.
        let plain = format.remove_srgb_suffix();
        let view_formats = [plain];
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("fade_from"),
            size: wgpu::Extent3d {
                width: self.config.width,
                height: self.config.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            // COPY_SRC lets a test read it back.
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: if plain == format { &[] } else { &view_formats },
        });
        let target = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("fade_from_enc"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fade_from_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let [image, compare] = self.last_viewports;
            self.draw_photo(&mut pass, image, compare);
        }
        self.queue.submit(Some(encoder.finish()));
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(plain),
            ..Default::default()
        });
        let id = self.egui_renderer.register_native_texture(
            &self.device,
            &view,
            wgpu::FilterMode::Linear,
        );
        self.fade_from = Some((id, texture));
        Some(id)
    }

    pub fn free_snapshot(&mut self) {
        if let Some((id, _)) = self.fade_from.take() {
            self.egui_renderer.free_texture(&id);
        }
    }

    /// The photo, and the compare photo beside it, into a pass whose target
    /// is the surface's size and format.
    fn draw_photo(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        image_viewport: Option<(u32, u32, u32, u32)>,
        compare_viewport: Option<(u32, u32, u32, u32)>,
    ) {
        if let Some(image_bind) = &self.image_bind {
            let (sw, sh) = (self.config.width, self.config.height);
            // Both pipelines share `pipeline_layout`, so the bind groups
            // below are the same for either.
            let pipeline = match self.image_pixel_format {
                PixelFormat::Srgb8 | PixelFormat::TonedF16 => &self.pipeline,
                PixelFormat::LinearF16 => &self.raw_pipeline,
            };
            let xform_bind = &self.xform_bind;
            let overlay_pipeline = &self.overlay_pipeline;
            let overlay_bind = self.overlay_bind.as_ref();
            let draw_into =
                |pass: &mut wgpu::RenderPass, vp: (u32, u32, u32, u32), adj: &wgpu::BindGroup| {
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
                    crate::web::analytics::photo_drawn();

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
                    draw_into(pass, vp, &self.adj_bind);
                    if let Some(vp2) = compare_viewport {
                        draw_into(pass, vp2, &self.adj_bind_b);
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

        let frame = match (&self.offscreen, &self.surface) {
            (Some(_), _) | (None, None) => None,
            (None, Some(surface)) => match surface.get_current_texture() {
                C::Success(f) | C::Suboptimal(f) => Some(f),
                C::Outdated | C::Lost => {
                    surface.configure(&self.device, &self.config);
                    self.free_egui_textures(&egui);
                    return false;
                }
                C::Timeout | C::Occluded | C::Validation => {
                    self.free_egui_textures(&egui);
                    return false;
                }
            },
        };
        let Some(target) = frame
            .as_ref()
            .map(|f| &f.texture)
            .or(self.offscreen.as_ref())
        else {
            self.free_egui_textures(&egui);
            return false;
        };
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("enc") });

        let source = self.base_wanted.source();
        let stale =
            |i: usize| self.base_wanted.wanted[i] && self.layers_built_for[i] != Some(source);
        let (base_stale, fine_stale) = (stale(0), stale(1));
        if fine_stale {
            self.ensure_fine();
        }
        for (i, layer) in [(0, &self.base), (1, &self.fine)] {
            if let Some(layer) = layer.as_ref().filter(|_| [base_stale, fine_stale][i]) {
                self.encode_base(&mut encoder, layer);
                self.layers_built_for[i] = Some(source);
                self.base_builds += 1;
                if crate::jobs::loader::timing_enabled() {
                    crate::jobs::loader::mark(&format!("layer build {}", self.base_builds));
                }
            }
        }

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
                        load: wgpu::LoadOp::Clear(self.clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.draw_photo(&mut pass, image_viewport, compare_viewport);
        }
        self.last_viewports = [image_viewport, compare_viewport];

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
        if let Some(frame) = frame {
            frame.present();
        }
        #[cfg(target_arch = "wasm32")]
        crate::web::analytics::presented();
        if crate::jobs::loader::timing_enabled() {
            // Only the first frames matter: time to first pixels, then time
            // to the sharp photo.
            use std::sync::atomic::{AtomicU32, Ordering};
            static FRAMES: AtomicU32 = AtomicU32::new(0);
            let n = FRAMES.fetch_add(1, Ordering::Relaxed);
            if n < 3 {
                crate::jobs::loader::mark(&format!("frame {n} presented"));
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
