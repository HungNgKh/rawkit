//! The canvas onto a surface — pipeline stage L.
//!
//! # Why this is in the engine and not the shell
//!
//! Because it is a colour decision, not a windowing one. The canvas holds
//! display-referred *linear* values and a screen expects encoded ones; getting
//! that wrong produces an image that is uniformly too dark, or — if the curve is
//! applied twice — washed out. Both look like grading problems and neither
//! announces itself.
//!
//! The shell supplies a texture view and this decides what lands in it, the same
//! way `rawkit-export` decides what lands in a file. Those two are the only
//! places the transfer function is applied, and the number they agree on is
//! tested: linear 0.18 comes out **118**.

use crate::{render::Canvas, EngineError, Gpu};

/// f32 to IEEE 754 binary16, for the lookup table's texels.
///
/// The inverse of `render::half_to_f32`, and written out for the same reason:
/// fifteen lines against a dependency and its licence review. Only the ordinary
/// range matters here — table values are device fractions in [0, 1].
fn half_from_f32(value: f32) -> u16 {
    if value == 0.0 {
        return 0;
    }
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mantissa = bits & 0x7f_ffff;
    if exponent <= 0 {
        return sign;
    }
    if exponent >= 31 {
        return sign | 0x7c00;
    }
    let mut half = sign | ((exponent as u16) << 10) | (mantissa >> 13) as u16;
    let remainder = mantissa & 0x1fff;
    if remainder > 0x1000 || (remainder == 0x1000 && (half & 1) == 1) {
        half += 1;
    }
    half
}

/// Draws a [`Canvas`] onto a surface or texture.
///
/// Built once per target format. The format matters because an `-Srgb` target
/// encodes on write and a plain one does not, and the difference is a whole
/// transfer function.
pub struct Presenter {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// The monitor's correction, or a 1x1x1 identity when there is none.
    ///
    /// A bind group cannot have holes, so the binding always exists; which
    /// fragment entry point runs is what decides whether it is consulted. One
    /// dummy texel is cheaper than a second pipeline layout.
    lut: wgpu::TextureView,
    lut_sampler: wgpu::Sampler,
}

impl Presenter {
    pub fn new(gpu: &Gpu, target: wgpu::TextureFormat) -> Self {
        Self::build(gpu, target, None)
    }

    /// A presenter that corrects for a specific monitor.
    ///
    /// `lut` is `grid` cubed RGBA entries mapping linear sRGB to that monitor's
    /// device values — see `rawkit_export::display`. Because those values are
    /// already encoded, `target` **must not** be an `-Srgb` format; the caller
    /// configures the surface accordingly, and this panics rather than silently
    /// encoding twice.
    ///
    /// # Panics
    ///
    /// If `target` is an sRGB format, or `lut` is not `grid` cubed entries.
    pub fn with_display_lut(
        gpu: &Gpu,
        target: wgpu::TextureFormat,
        lut: &[[f32; 4]],
        grid: usize,
    ) -> Self {
        assert!(
            !target.is_srgb(),
            "a display LUT already encodes; an -Srgb target would encode a second time"
        );
        assert_eq!(lut.len(), grid * grid * grid, "the LUT is not a cube");
        Self::build(gpu, target, Some((lut, grid)))
    }

    fn build(gpu: &Gpu, target: wgpu::TextureFormat, lut: Option<(&[[f32; 4]], usize)>) -> Self {
        let module = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("present"),
                source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/present.wgsl").into()),
            });

        let layout = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("present"),
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
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D3,
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
        let pipeline_layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("present"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });

        // Which entry point runs is the whole of the format handling: let the
        // hardware encode when the target says it will, and do it in the shader
        // when it will not.
        let entry = match (lut.is_some(), target.is_srgb()) {
            (true, _) => "fs_display_lut",
            (false, true) => "fs_hardware_encode",
            (false, false) => "fs_shader_encode",
        };
        let pipeline = gpu
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("present"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });

        // Linear, and this was Nearest until the viewport gained a real scale.
        //
        // The canvas holds *level* pixels, and `Viewport::level` picks
        // `floor(log2(1/scale))`, so a level pixel is between half a screen
        // pixel and one: the canvas is up to twice the surface's size and only
        // exactly equal at powers of two. Nearest sampling at those ratios
        // aliases, and on fine detail — sea spray, foliage — aliasing reads as
        // shimmer while panning.
        //
        // Linear is the cheap correct-enough answer, not the right one. A real
        // downscale filter belongs with output sharpening, which is its own P0
        // item; doing half of it here would mean two places to change.
        let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("present"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // The table, or one identity texel when there is nothing to correct.
        let (entries, grid) = lut.unwrap_or((&[[0.0, 0.0, 0.0, 1.0]], 1));
        let size = wgpu::Extent3d {
            width: grid as u32,
            height: grid as u32,
            depth_or_array_layers: grid as u32,
        };
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("display lut"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D3,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // Half floats: the table's values are device fractions in [0, 1] and
        // eleven bits of significand is four times finer than the framebuffer
        // they end up in.
        let halves: Vec<u16> = entries
            .iter()
            .flatten()
            .map(|v| half_from_f32(*v))
            .collect();
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&halves),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(grid as u32 * 8),
                rows_per_image: Some(grid as u32),
            },
            size,
        );

        // Clamped and linear: values outside the cube are out of gamut and
        // belong at its edge, and interpolation between grid points is the whole
        // reason a coarse grid is enough.
        let lut_sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("display lut"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Self {
            pipeline,
            layout,
            sampler,
            lut: texture.create_view(&wgpu::TextureViewDescriptor::default()),
            lut_sampler,
        }
    }

    /// Draw `canvas` into `target`, filling it.
    ///
    /// Submits and returns; nothing waits. The caller presents the surface,
    /// which is the one place a frame should synchronise.
    pub fn draw(
        &self,
        gpu: &Gpu,
        canvas: &Canvas,
        target: &wgpu::TextureView,
    ) -> Result<(), EngineError> {
        let view = canvas
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("present"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&self.lut),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.lut_sampler),
                },
            ],
        });

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("present"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // The triangle covers every pixel, so clearing first
                        // would be work whose result is entirely overwritten.
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        gpu.queue.submit([encoder.finish()]);
        Ok(())
    }
}
