//! RCD demosaic on the GPU — pipeline stage B.
//!
//! The kernel is `shaders/demosaic_rcd.wgsl`, ported from vkdt (BSD-2-Clause);
//! that file carries the provenance and the two assumptions the port makes.
//! This module is the plumbing: buffers in, five dispatches, pixels out.
//!
//! Everything here allocates per call. That is correct for a spike and wrong for
//! an editor — the interactive path will keep the buffers alive across renders
//! and reuse them per tile. The stage boundary is what matters now.

use crate::{EngineError, Gpu};
use wgpu::util::DeviceExt;

/// Which colour sits at pixel (0,0). Bayer only: RCD is a Bayer algorithm, and
/// X-Trans needs a different kernel rather than a different phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BayerPhase {
    Rggb,
    Bggr,
    Grbg,
    Gbrg,
}

impl BayerPhase {
    /// The decoder's CFA layout, or `None` for a sensor RCD cannot handle.
    ///
    /// Returning an `Option` rather than defaulting to RGGB is the point: a
    /// wrong phase produces a plausible image with subtly wrong colour, and an
    /// X-Trans file run through a Bayer kernel produces a confident mess.
    pub fn from_cfa(pattern: rawkit_decode::CfaPattern) -> Option<Self> {
        use rawkit_decode::CfaPattern;
        match pattern {
            CfaPattern::Rggb => Some(BayerPhase::Rggb),
            CfaPattern::Bggr => Some(BayerPhase::Bggr),
            CfaPattern::Grbg => Some(BayerPhase::Grbg),
            CfaPattern::Gbrg => Some(BayerPhase::Gbrg),
            CfaPattern::XTrans => None,
        }
    }

    /// The offset that makes the kernel's RGGB reasoning land on this layout.
    /// One kernel, four patterns — a per-pattern kernel would be four places for
    /// the same bug to hide.
    fn offset(self) -> (u32, u32) {
        match self {
            BayerPhase::Rggb => (0, 0),
            BayerPhase::Bggr => (1, 1),
            BayerPhase::Grbg => (1, 0),
            BayerPhase::Gbrg => (0, 1),
        }
    }
}

/// One mosaiced image to demosaic.
///
/// `data` is one sample per photosite, row-major, black-subtracted and scaled
/// however the caller likes — RCD only cares about ratios, so the unit is the
/// caller's business.
pub struct Mosaic<'a> {
    pub data: &'a [f32],
    pub width: u32,
    pub height: u32,
    pub phase: BayerPhase,
    /// Per-channel white balance. Normalised internally so green is 1.0, which
    /// the kernel depends on.
    pub wb: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    width: u32,
    height: u32,
    packed_width: u32,
    cfa_x_offset: u32,
    cfa_y_offset: u32,
    _pad: [u32; 3],
    wb: [f32; 4],
}

const STAGES: [&str; 5] = ["conv", "green_at_rb", "rb_at_br", "rb_at_g", "pack"];

/// Compiled RCD pipelines. Build once, reuse — shader compilation is far too
/// slow to sit on a render path.
pub struct Demosaic {
    layout: wgpu::BindGroupLayout,
    pipelines: Vec<wgpu::ComputePipeline>,
}

impl Demosaic {
    pub fn new(gpu: &Gpu) -> Self {
        let module = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("rcd demosaic"),
                source: wgpu::ShaderSource::Wgsl(
                    include_str!("../shaders/demosaic_rcd.wgsl").into(),
                ),
            });

        // Explicit layout rather than an auto-derived one: the five entry points
        // touch different subsets of the bindings, and a derived layout would
        // differ per stage, so one bind group could not serve them all.
        let entry = |binding: u32, read_only: bool| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let mut entries = vec![wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }];
        entries.push(entry(1, true));
        entries.extend((2..=8).map(|b| entry(b, false)));

        let layout = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("rcd bindings"),
                entries: &entries,
            });
        let pipeline_layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("rcd layout"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });

        let pipelines = STAGES
            .iter()
            .map(|stage| {
                gpu.device
                    .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                        label: Some(stage),
                        layout: Some(&pipeline_layout),
                        module: &module,
                        entry_point: Some(stage),
                        compilation_options: Default::default(),
                        cache: None,
                    })
            })
            .collect();

        Self { layout, pipelines }
    }

    /// Demosaic one image, returning interleaved RGBA in the caller's units.
    ///
    /// The outermost few pixels are wrong by construction — RCD reaches four
    /// pixels out and the kernel clamps at the edge rather than mirroring, which
    /// would flip CFA parity. Crop before showing the result.
    pub fn run(&self, gpu: &Gpu, image: &Mosaic<'_>) -> Result<Vec<f32>, EngineError> {
        let (w, h) = (image.width as usize, image.height as usize);
        if image.data.len() != w * h {
            return Err(EngineError::DeviceRequest(format!(
                "mosaic is {}x{} but carries {} samples",
                w,
                h,
                image.data.len()
            )));
        }

        let packed_width = image.width.div_ceil(2);
        let (dx, dy) = image.phase.offset();
        // Green normalised to 1.0. The kernel estimates green at red and blue
        // sites from unscaled CFA values, so any other normalisation would mix
        // scaled and unscaled greens in the same subtraction.
        let g = image.wb[1];
        let params = Params {
            width: image.width,
            height: image.height,
            packed_width,
            cfa_x_offset: dx,
            cfa_y_offset: dy,
            _pad: [0; 3],
            wb: [image.wb[0] / g, 1.0, image.wb[2] / g, 1.0],
        };

        let device = &gpu.device;
        let params_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("rcd params"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let cfa_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("cfa"),
            contents: bytemuck::cast_slice(image.data),
            usage: wgpu::BufferUsages::STORAGE,
        });

        let plane = |label: &str, len: usize| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (len * std::mem::size_of::<f32>()) as u64,
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            })
        };
        let vh = plane("vh", w * h);
        let pq = plane("pq", packed_width as usize * h);
        let lp = plane("lp", packed_width as usize * h);
        let ch_r = plane("r", w * h);
        let ch_g = plane("g", w * h);
        let ch_b = plane("b", w * h);

        let out_size = (w * h * 4 * std::mem::size_of::<f32>()) as u64;
        let out = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rgba"),
            size: out_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: out_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let buffers = [&cfa_buf, &vh, &pq, &lp, &ch_r, &ch_g, &ch_b, &out];
        let mut bindings = vec![wgpu::BindGroupEntry {
            binding: 0,
            resource: params_buf.as_entire_binding(),
        }];
        bindings.extend(
            buffers
                .iter()
                .enumerate()
                .map(|(i, b)| wgpu::BindGroupEntry {
                    binding: i as u32 + 1,
                    resource: b.as_entire_binding(),
                }),
        );
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rcd"),
            layout: &self.layout,
            entries: &bindings,
        });

        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("rcd") });
        {
            // One pass per stage. The stage boundaries are vkdt's barriers: each
            // reads what the previous one wrote, so they cannot be merged
            // without reintroducing the workgroup-memory version.
            for (pipeline, stage) in self.pipelines.iter().zip(STAGES) {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some(stage),
                    timestamp_writes: None,
                });
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, &bind_group, &[]);
                pass.dispatch_workgroups(image.width.div_ceil(8), image.height.div_ceil(8), 1);
            }
        }
        encoder.copy_buffer_to_buffer(&out, 0, &staging, 0, out_size);
        gpu.queue.submit([encoder.finish()]);

        let (tx, rx) = std::sync::mpsc::channel();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| EngineError::DeviceRequest(e.to_string()))?;
        rx.recv()
            .map_err(|_| EngineError::DeviceRequest("readback never completed".into()))?
            .map_err(|e| EngineError::DeviceRequest(e.to_string()))?;

        let pixels = {
            let view = staging.slice(..).get_mapped_range();
            bytemuck::cast_slice::<u8, f32>(&view).to_vec()
        };
        staging.unmap();
        Ok(pixels)
    }
}

/// Sensor readings to scene-linear [0, 1]: subtract black, divide by headroom.
///
/// This is the first arithmetic in the pipeline and the first place it can go
/// quietly wrong. The two failure modes are worth naming because neither
/// announces itself:
///
/// - Skip the black level and every shadow lifts toward grey with a colour cast,
///   because the offset is per channel.
/// - Use the wrong white and either the highlights clip early or nothing ever
///   reaches 1.0, which the tone map then interprets as an underexposed frame.
///
/// Values above white — sensors do produce them — are kept rather than clipped.
/// Highlight reconstruction needs to know a channel blew past full scale, and
/// clamping here would destroy that information before the stage that wants it.
pub fn normalise(raw: &rawkit_decode::RawImage) -> Vec<f32> {
    let phase = BayerPhase::from_cfa(raw.cfa).unwrap_or(BayerPhase::Rggb);
    let (dx, dy) = phase.offset();
    let white = raw.levels.white as f32;

    raw.data
        .iter()
        .enumerate()
        .map(|(i, &sample)| {
            let x = (i as u32 % raw.width) + dx;
            let y = (i as u32 / raw.width) + dy;
            // Index the per-channel black by the sensor's own channel order:
            // 0 = red, 1 and 3 = the two greens, 2 = blue.
            let channel = if (x + y) % 2 == 1 {
                if y % 2 == 0 {
                    1
                } else {
                    3
                }
            } else if y % 2 == 0 {
                0
            } else {
                2
            };
            let black = raw.levels.black[channel] as f32;
            (sample as f32 - black) / (white - black)
        })
        .collect()
}
