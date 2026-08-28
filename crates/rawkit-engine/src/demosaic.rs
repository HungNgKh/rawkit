//! RCD demosaic on the GPU — pipeline stage B.
//!
//! The kernel is `shaders/demosaic_rcd.wgsl`, ported from vkdt (BSD-2-Clause);
//! that file carries the provenance and the two assumptions the port makes.
//! This module is the plumbing: buffers in, five dispatches, pixels out.
//!
//! # Tiling
//!
//! Every render is tiled, including one that would fit in a single tile. Buffers
//! are sized by the tile and never by the image, which is what lets the engine
//! run on WebGPU's *default* limits — a 24 MP frame as RGBA f32 is 388 MB and
//! the portable floor is a 256 MB buffer and a 128 MB storage binding. An
//! untiled path would work on a large desktop GPU and fail on a smaller one,
//! which is the divergence the engine exists to prevent.
//!
//! It is also the same structure the interactive canvas needs, where only the
//! visible tiles get rendered. Getting it now rather than later is the whole
//! point: a pipeline written against whole frames does not become tiled by
//! refactoring, it becomes tiled by rewriting.
//!
//! Buffers are allocated once per `run` and reused across tiles. A render must
//! not allocate; the remaining per-call allocation is the next thing to lift out
//! when there is a canvas to keep state for.

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

/// How far outside a tile the kernel reaches, in pixels.
///
/// Derived by following the dependency chain rather than guessed, because the
/// cost of guessing low is a faint seam that only shows on some images:
///
/// | Stage | Reads | Cumulative reach |
/// |---|---|---|
/// | `conv` | CFA ±3 | 3 |
/// | `green_at_rb` | CFA ±4, `lp` ±2 (itself CFA ±3) | 5 |
/// | `rb_at_br` | `pq` ±1, green ±3 (itself reach 5) | 8 |
/// | `rb_at_g` | chroma ±3 (itself reach 8) | 11 |
///
/// Rounded up to 12 to keep it **even**, which is not cosmetic: an odd halo
/// shifts the CFA phase inside the tile, and every pixel would come out the
/// wrong colour.
pub const HALO: u32 = 12;

/// Tile edge in pixels, excluding halo. 512 keeps every buffer far inside
/// WebGPU's default limits while leaving the halo a small fraction of the work
/// (a 512 tile carries 536² samples, so ~9% overhead).
pub const DEFAULT_TILE: u32 = 512;

/// Compiled RCD pipelines. Build once, reuse — shader compilation is far too
/// slow to sit on a render path.
pub struct Demosaic {
    layout: wgpu::BindGroupLayout,
    pipelines: Vec<wgpu::ComputePipeline>,
    tile: u32,
}

impl Demosaic {
    pub fn new(gpu: &Gpu) -> Self {
        Self::with_tile_size(gpu, DEFAULT_TILE)
    }

    /// Mostly for tests: a small tile forces many seams over a small image,
    /// which is how the halo gets proven.
    ///
    /// # Panics
    ///
    /// If `tile` is odd. Tile origins must land on the same CFA phase as the
    /// image, and an odd tile size guarantees they do not.
    pub fn with_tile_size(gpu: &Gpu, tile: u32) -> Self {
        assert!(tile > 0 && tile % 2 == 0, "tile size must be even");
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

        Self {
            layout,
            pipelines,
            tile,
        }
    }

    /// Demosaic one image, returning interleaved RGBA in the caller's units.
    ///
    /// Always tiled, including when the image fits in a single tile. One code
    /// path means the tiled result cannot drift from an untiled one, because
    /// there is no untiled one — the same rule that keeps preview and export
    /// sharing kernels.
    ///
    /// The outermost few pixels of the *image* are still wrong by construction:
    /// RCD reaches four pixels out and the kernel clamps at the edge rather than
    /// mirroring, which would flip CFA parity. Crop before showing the result.
    /// Tile seams are not in that category — they are exact, and a test asserts
    /// it.
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

        // Every tile is processed at the same padded size, so the shader's view
        // of the world — dimensions, packed stride, CFA phase — is identical for
        // all of them and the uniform never changes. Edge tiles are padded with
        // clamped pixels rather than shrunk, which costs a little work at the
        // border and removes a whole class of index bug.
        let padded = self.tile + 2 * HALO;
        let (dx, dy) = image.phase.offset();
        // Green normalised to 1.0. The kernel estimates green at red and blue
        // sites from unscaled CFA values, so any other normalisation would mix
        // scaled and unscaled greens in the same subtraction.
        let g = image.wb[1];
        let params = Params {
            width: padded,
            height: padded,
            packed_width: padded.div_ceil(2),
            cfa_x_offset: dx,
            cfa_y_offset: dy,
            _pad: [0; 3],
            wb: [image.wb[0] / g, 1.0, image.wb[2] / g, 1.0],
        };

        let device = &gpu.device;
        let px = padded as usize * padded as usize;
        let params_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("rcd params"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });

        let plane = |label: &str, len: usize, extra: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (len * std::mem::size_of::<f32>()) as u64,
                usage: wgpu::BufferUsages::STORAGE | extra,
                mapped_at_creation: false,
            })
        };
        // Allocated once and reused for every tile. This is the shape the
        // interactive path needs: a render must not allocate.
        let cfa_buf = plane("cfa", px, wgpu::BufferUsages::COPY_DST);
        let vh = plane("vh", px, wgpu::BufferUsages::empty());
        let packed = params.packed_width as usize * padded as usize;
        let pq = plane("pq", packed, wgpu::BufferUsages::empty());
        let lp = plane("lp", packed, wgpu::BufferUsages::empty());
        let ch_r = plane("r", px, wgpu::BufferUsages::empty());
        let ch_g = plane("g", px, wgpu::BufferUsages::empty());
        let ch_b = plane("b", px, wgpu::BufferUsages::empty());
        let out = plane("rgba", px * 4, wgpu::BufferUsages::COPY_SRC);

        let out_size = (px * 4 * std::mem::size_of::<f32>()) as u64;
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

        let mut result = vec![0.0f32; w * h * 4];
        let mut scratch = vec![0.0f32; px];

        for oy in (0..image.height).step_by(self.tile as usize) {
            for ox in (0..image.width).step_by(self.tile as usize) {
                gather_padded(image, ox, oy, padded, &mut scratch);
                gpu.queue
                    .write_buffer(&cfa_buf, 0, bytemuck::cast_slice(&scratch));

                let mut encoder = device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("rcd") });
                {
                    // One pass per stage. The stage boundaries are vkdt's
                    // barriers: each reads what the previous one wrote, so they
                    // cannot be merged without reintroducing the
                    // workgroup-memory version.
                    for (pipeline, stage) in self.pipelines.iter().zip(STAGES) {
                        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                            label: Some(stage),
                            timestamp_writes: None,
                        });
                        pass.set_pipeline(pipeline);
                        pass.set_bind_group(0, &bind_group, &[]);
                        pass.dispatch_workgroups(padded.div_ceil(8), padded.div_ceil(8), 1);
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

                {
                    let view = staging.slice(..).get_mapped_range();
                    let tile_px: &[f32] = bytemuck::cast_slice(&view);
                    // Copy the interior only. The halo exists to make the
                    // interior correct and is discarded.
                    let valid_w = self.tile.min(image.width - ox) as usize;
                    let valid_h = self.tile.min(image.height - oy) as usize;
                    for y in 0..valid_h {
                        let src = ((y + HALO as usize) * padded as usize + HALO as usize) * 4;
                        let dst = ((oy as usize + y) * w + ox as usize) * 4;
                        result[dst..dst + valid_w * 4]
                            .copy_from_slice(&tile_px[src..src + valid_w * 4]);
                    }
                }
                staging.unmap();
            }
        }

        Ok(result)
    }
}

/// Fill `out` with the tile at `(ox, oy)` plus its halo, clamping at the image
/// edge.
///
/// Clamping here has to match what the shader does when it reads out of bounds,
/// or the tiled result would differ from an untiled one along the image border.
/// Both clamp to the nearest edge pixel.
fn gather_padded(image: &Mosaic<'_>, ox: u32, oy: u32, padded: u32, out: &mut [f32]) {
    let w = image.width as i64;
    let h = image.height as i64;
    for py in 0..padded as i64 {
        let gy = (oy as i64 - HALO as i64 + py).clamp(0, h - 1);
        let row = (gy * w) as usize;
        let dst = (py * padded as i64) as usize;
        for px in 0..padded as i64 {
            let gx = (ox as i64 - HALO as i64 + px).clamp(0, w - 1);
            out[dst + px as usize] = image.data[row + gx as usize];
        }
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
