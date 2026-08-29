//! Drawing a cached preview into the canvas.
//!
//! # Why the engine and not the shell
//!
//! Same reason as `present.rs`: it is a colour decision. A preview is *encoded*
//! sRGB and the canvas is *linear*, and getting the conversion wrong produces an
//! image that is uniformly too bright — the washed-out failure, which looks like
//! a grading problem and announces itself as nothing.
//!
//! The mechanism is a declaration rather than arithmetic. The texture is
//! `Rgba8UnormSrgb`, so the hardware decodes on every sample and there is no code
//! path that could forget to. It costs nothing and cannot drift.
//!
//! # What this is for
//!
//! Moving through a shoot. Decoding a RAW and building its pyramid is a fifth of
//! a second; a preview is already rendered, so showing it is an upload and a
//! triangle. The renderer still exists and still runs — this only covers the
//! case where the preview has at least as many pixels as the view can show, which
//! during a cull is nearly always.

use crate::{render::Canvas, EngineError, Gpu, CANVAS_FORMAT};

/// One cell's worth of uniform, padded to the alignment a uniform binding needs.
///
/// The stride rather than the size, because a grid frame writes every cell into
/// one buffer and binds a slice of it per draw. 256 is the alignment every
/// backend accepts.
const REGION_STRIDE: u64 = 256;
/// origin(2) + span(2) + tint(4) + edge(4) + inner(4), in floats.
const REGION_FLOATS: usize = 16;

/// One thumbnail to draw, and how it should look.
pub struct Cell<'a> {
    pub image: &'a PreviewImage,
    /// Where it goes in the canvas, in canvas pixels: x, y, width, height.
    /// May extend past the edges; the part that is off-canvas is cropped rather
    /// than squashed.
    pub dest: [i32; 4],
    /// Multiplied into the sample, in linear light. `[1.0; 3]` leaves it alone.
    pub tint: [f32; 3],
    /// Colour and thickness in canvas pixels. Thickness zero draws no edge.
    pub edge: ([f32; 3], f32),
    /// A second band just inside the first — a colour label, which a frame can
    /// carry at the same time as a flag.
    pub inner: ([f32; 3], f32),
}

/// A preview uploaded to the GPU, ready to be drawn at any zoom.
///
/// Held across frames: panning and zooming re-draw from the same texture, and
/// only moving to another photograph replaces it.
pub struct PreviewImage {
    view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
}

/// Draws a [`PreviewImage`] into a [`Canvas`].
pub struct PreviewBlit {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    region: wgpu::Buffer,
}

impl PreviewBlit {
    pub fn new(gpu: &Gpu) -> Self {
        let module = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("preview"),
                source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/preview.wgsl").into()),
            });

        let layout = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("preview"),
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
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
        let pipeline_layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("preview"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
        let pipeline = gpu
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("preview"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        // The canvas: linear half floats, which is what makes the
                        // sRGB texture format above the whole of the conversion.
                        format: CANVAS_FORMAT,
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

        let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("preview"),
            // Clamped so a sample exactly on the edge does not wrap to the far
            // side. The shader rejects anything genuinely outside the image, so
            // this only matters for the half-texel at the border.
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let region = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("preview region"),
            size: REGION_STRIDE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            layout,
            sampler,
            region,
        }
    }

    /// Put a decoded preview on the GPU.
    ///
    /// `rgba` is eight-bit **sRGB**, four bytes per pixel — exactly what
    /// `rawkit_export::decode` hands back. It is not converted here; the texture
    /// format says what it is and the sampler does the rest.
    pub fn upload(
        &self,
        gpu: &Gpu,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<PreviewImage, EngineError> {
        let expected = width as usize * height as usize * 4;
        if width == 0 || height == 0 || rgba.len() != expected {
            return Err(EngineError::WrongSize(format!(
                "a {width}x{height} preview needs {expected} bytes, got {}",
                rgba.len()
            )));
        }
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("preview"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // The declaration that does the colour management.
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            size,
        );
        Ok(PreviewImage {
            view: texture.create_view(&wgpu::TextureViewDescriptor::default()),
            width,
            height,
        })
    }

    /// Draw a grid of thumbnails into `canvas`, clearing it first.
    ///
    /// One render pass and one draw per cell. The cells share a uniform buffer,
    /// each binding its own slice of it, so a frame costs one buffer write and
    /// no allocation beyond the bind groups — which are needed regardless
    /// because every cell has a different texture.
    ///
    /// A cell hanging off the top or bottom of the canvas has its **rectangle**
    /// clamped and its **sampling window** cropped to match, so a partly visible
    /// row shows part of a photograph rather than a squashed whole one. That is
    /// the failure worth avoiding here: a squashed edge row looks like a layout
    /// choice rather than a bug.
    /// Lay out cells over the whole canvas, clearing what was there.
    ///
    /// What a grid wants: every cell is redrawn each frame, and the gaps between
    /// them are background rather than whatever the last view left.
    pub fn draw_grid(&self, gpu: &Gpu, canvas: &Canvas, cells: &[Cell<'_>]) {
        self.render(gpu, canvas, cells, true);
    }

    /// Draw cells *onto* what is already on the canvas.
    ///
    /// What an overlay wants — a crop rectangle drawn over the photograph. The
    /// only difference from [`draw_grid`](Self::draw_grid) is the load: clearing
    /// first would leave four white lines on an empty canvas, which is what
    /// happened before this existed.
    pub fn draw_over(&self, gpu: &Gpu, canvas: &Canvas, cells: &[Cell<'_>]) {
        self.render(gpu, canvas, cells, false);
    }

    fn render(&self, gpu: &Gpu, canvas: &Canvas, cells: &[Cell<'_>], clear: bool) {
        let [canvas_w, canvas_h] = canvas.size();
        let mut regions = vec![0.0f32; cells.len() * (REGION_STRIDE as usize / 4)];
        let mut placed: Vec<([u32; 4], usize)> = Vec::with_capacity(cells.len());

        for (index, cell) in cells.iter().enumerate() {
            let [x, y, w, h] = cell.dest;
            if w <= 0 || h <= 0 {
                continue;
            }
            let (left, top) = (x.max(0), y.max(0));
            let right = (x + w).min(canvas_w as i32);
            let bottom = (y + h).min(canvas_h as i32);
            if right <= left || bottom <= top {
                continue;
            }
            // What fraction of the cell survived the clamp, and where it starts.
            let origin = [(left - x) as f32 / w as f32, (top - y) as f32 / h as f32];
            let span = [
                (right - left) as f32 / w as f32,
                (bottom - top) as f32 / h as f32,
            ];

            let at = index * (REGION_STRIDE as usize / 4);
            regions[at..at + 4].copy_from_slice(&[origin[0], origin[1], span[0], span[1]]);
            regions[at + 4..at + 8].copy_from_slice(&[
                cell.tint[0],
                cell.tint[1],
                cell.tint[2],
                1.0,
            ]);
            // Thickness as a fraction of the *drawn* rectangle, so an edge stays
            // the same number of pixels wide on a cell that is half off-screen.
            let thickness = if cell.edge.1 > 0.0 {
                cell.edge.1 / (right - left).min(bottom - top).max(1) as f32
            } else {
                0.0
            };
            regions[at + 8..at + 12].copy_from_slice(&[
                cell.edge.0[0],
                cell.edge.0[1],
                cell.edge.0[2],
                thickness,
            ]);
            let inner = if cell.inner.1 > 0.0 {
                cell.inner.1 / (right - left).min(bottom - top).max(1) as f32
            } else {
                0.0
            };
            regions[at + 12..at + 16].copy_from_slice(&[
                cell.inner.0[0],
                cell.inner.0[1],
                cell.inner.0[2],
                inner,
            ]);
            placed.push((
                [
                    left as u32,
                    top as u32,
                    (right - left) as u32,
                    (bottom - top) as u32,
                ],
                index,
            ));
        }

        let needed = (cells.len().max(1) as u64) * REGION_STRIDE;
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("preview grid regions"),
            size: needed,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue
            .write_buffer(&buffer, 0, bytemuck::cast_slice(&regions));

        let groups: Vec<wgpu::BindGroup> = placed
            .iter()
            .map(|(_, index)| {
                gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("preview cell"),
                    layout: &self.layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&cells[*index].image.view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&self.sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: &buffer,
                                offset: *index as u64 * REGION_STRIDE,
                                size: std::num::NonZeroU64::new((REGION_FLOATS * 4) as u64),
                            }),
                        },
                    ],
                })
            })
            .collect();

        let target = canvas
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("preview grid"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("preview grid"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // The background between cells. Dark rather than black so
                        // a black photograph still has an edge.
                        load: if clear {
                            wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.012,
                                g: 0.012,
                                b: 0.014,
                                a: 1.0,
                            })
                        } else {
                            wgpu::LoadOp::Load
                        },
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            for (group, ([x, y, w, h], _)) in groups.iter().zip(&placed) {
                // The viewport maps the triangle onto the cell; the scissor stops
                // the oversized part of it reaching anything else.
                pass.set_viewport(*x as f32, *y as f32, *w as f32, *h as f32, 0.0, 1.0);
                pass.set_scissor_rect(*x, *y, *w, *h);
                pass.set_bind_group(0, group, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        gpu.queue.submit(Some(encoder.finish()));
    }

    /// Fill `canvas` with the part of `image` the view is looking at.
    ///
    /// `origin` and `span` are in preview coordinates, 0 to 1. A span above 1
    /// means the view is wider than the photograph, and the surplus comes out
    /// black rather than as a stretched edge.
    ///
    /// Submits and returns, like every other draw on this path.
    pub fn draw(
        &self,
        gpu: &Gpu,
        image: &PreviewImage,
        canvas: &Canvas,
        origin: [f32; 2],
        span: [f32; 2],
    ) {
        let mut region = [0.0f32; REGION_FLOATS];
        region[..4].copy_from_slice(&[origin[0], origin[1], span[0], span[1]]);
        // No tint and no edge: the loupe shows the photograph and nothing else.
        region[4..8].copy_from_slice(&[1.0, 1.0, 1.0, 1.0]);
        gpu.queue
            .write_buffer(&self.region, 0, bytemuck::cast_slice(&region));

        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("preview"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&image.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.region.as_entire_binding(),
                },
            ],
        });

        let target = canvas
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("preview"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("preview"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
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
        gpu.queue.submit(Some(encoder.finish()));
    }
}
