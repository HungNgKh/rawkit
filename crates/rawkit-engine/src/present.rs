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

/// Draws a [`Canvas`] onto a surface or texture.
///
/// Built once per target format. The format matters because an `-Srgb` target
/// encodes on write and a plain one does not, and the difference is a whole
/// transfer function.
pub struct Presenter {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

impl Presenter {
    pub fn new(gpu: &Gpu, target: wgpu::TextureFormat) -> Self {
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
        let entry = if target.is_srgb() {
            "fs_hardware_encode"
        } else {
            "fs_shader_encode"
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

        // Nearest, deliberately. The canvas is rendered at the size it is shown
        // at, so there is nothing to interpolate and filtering would only soften
        // a 1:1 image. When the canvas is stale mid-resize the honest artifact is
        // a blocky frame for one vsync, not a blurry one.
        let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("present"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        Self {
            pipeline,
            layout,
            sampler,
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
