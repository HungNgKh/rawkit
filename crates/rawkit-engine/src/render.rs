//! The render: a decoded frame plus an `EditState`, in; pixels, out.
//!
//! Covers pipeline stages B through H — demosaic, white balance, camera
//! profile, exposure, tone map. The kernels are in
//! `shaders/demosaic_rcd.wgsl`; the RCD half is ported from vkdt (BSD-2-Clause)
//! and that file carries the provenance. This module is the plumbing: buffers
//! in, six dispatches per tile, pixels out.
//!
//! The signature is the architecture's central claim written as a function.
//! Everything the camera contributes is in [`Frame`]; everything the user
//! contributes is in `EditState`; nothing else influences the result. That is
//! what makes "same RAW + same EditState -> same pixels" checkable rather than
//! aspirational.
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
//! Buffers live in [`TileBuffers`], allocated once by the caller and reused for
//! every tile of every frame. A render must not allocate, so the interactive
//! path holds one set for as long as an image is open and `run` builds a
//! throwaway set for the one-shot case.
//!
//! # Two ways out
//!
//! [`Renderer::render_tile`] ends in a `map_async` and a blocking device poll:
//! the caller wanted the pixels, so it pays a full CPU/GPU sync to get them.
//! That is right for export and fatal for a canvas — a dozen visible tiles is a
//! dozen stalls a frame, which no amount of tiling or level selection recovers
//! from.
//!
//! [`Renderer::draw_tile`] writes into a [`Canvas`] texture and returns. Nothing
//! synchronises; the frame becomes visible when the surface is presented, which
//! is the one place a frame should wait. The two paths share every kernel, and
//! a test measures their disagreement at 2^-11 — the spacing of half floats,
//! and nothing else.
//!
//! # Resolution levels
//!
//! [`Pyramid`] reduces the *mosaic*, not the image, keeping the CFA pattern
//! intact so a reduced level demosaics through the same kernels. That is what
//! makes zooming out cheap: a tile at level *n* covers `tile << n` image pixels
//! and costs the same as one at level 0, so fit-to-screen on a 24 MP frame is a
//! handful of tiles rather than ninety-two.

use crate::profile::{CameraProfile, Matrix3};
use crate::{EngineError, Gpu};
use rawkit_editstate::EditState;

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

/// One decoded frame, ready to render: the mosaic plus what the sensor knows
/// about its own colour.
///
/// `data` is one sample per photosite, row-major, black-subtracted and scaled
/// to roughly [0, 1]. Values above 1 are expected and preserved, because a
/// clipped channel is information the highlight stage will need.
///
/// This is deliberately *not* an `EditState`. The frame is what the camera
/// recorded and cannot be edited; the `EditState` is what the user decided.
/// Keeping them apart is what makes "same RAW + same EditState -> same pixels"
/// a statement with two independent halves.
pub struct Frame<'a> {
    pub data: &'a [f32],
    pub width: u32,
    pub height: u32,
    pub phase: BayerPhase,
    /// As-shot white balance as per-channel multipliers, green-referenced.
    pub as_shot_wb: [f32; 3],
    /// The value in `data` at which the sensor saturated.
    ///
    /// Normally 1.0, because [`normalise`] scales the white level there. It is
    /// a field rather than a constant because some bodies record usable signal
    /// above the level the decoder reports, and because a caller feeding
    /// synthetic data may have no clipping at all — passing `f32::INFINITY`
    /// turns highlight reconstruction off, which is what a test measuring
    /// something else wants.
    pub clip_level: f32,
    /// How this sensor sees colour. Carried rather than a bare matrix so that
    /// the renderer can ask it for the transform *at the chosen temperature* —
    /// which is the whole reason a profile is a profile and not a constant.
    pub profile: CameraProfile,
}

impl Frame<'_> {
    /// The white balance and colour transform this frame renders with, given an
    /// edit.
    ///
    /// The two come back together because they are one decision. The matrix has
    /// to be the one for *this* illuminant, and the multipliers have to be the
    /// ones that neutralise it; computing them apart is how an image ends up
    /// with a cast that looks like a white-balance error and is not one.
    fn colour(&self, state: &EditState) -> Result<Colour, EngineError> {
        let (multipliers, temperature) = match state.white_balance.temperature_k {
            Some(temperature) => (
                self.profile
                    .multipliers_for(temperature, state.white_balance.tint),
                temperature,
            ),
            None => {
                let g = self.as_shot_wb[1];
                if g <= 0.0 {
                    return Err(EngineError::DeviceRequest(
                        "as-shot white balance has no green multiplier".into(),
                    ));
                }
                let as_shot = [self.as_shot_wb[0] / g, 1.0, self.as_shot_wb[2] / g];
                // As-shot still needs a temperature, because the matrix depends
                // on one. Recovering it from the multipliers is exactly the
                // conversion a UI needs to display "As Shot 5200 K", so the same
                // path serves both and cannot disagree with itself.
                let (temperature, _) = self.profile.temperature_from_multipliers(as_shot);
                (as_shot, temperature)
            }
        };
        // With a hue/saturation table the render goes camera -> working space
        // -> table -> display; without one it goes straight to display and the
        // second matrix is the identity, so the kernel has a single path either
        // way.
        // Ask for the transform and the table together, so there is no way to
        // end up with one and not the other.
        let with_table = self
            .profile
            .camera_to_working(temperature)
            .zip(self.profile.hue_sat_map(temperature));

        Ok(match with_table {
            Some(((to_working, to_display), map)) => Colour {
                multipliers,
                cam_to_display: to_working,
                working_to_display: to_display,
                hue_sat: Some(map),
            },
            None => Colour {
                multipliers,
                cam_to_display: self.profile.camera_to_display(temperature),
                working_to_display: crate::profile::IDENTITY,
                hue_sat: None,
            },
        })
    }

    /// The temperature and tint this frame was shot at, for a UI to display.
    pub fn as_shot_temperature(&self) -> (f32, f32) {
        let g = self.as_shot_wb[1].max(1e-6);
        self.profile.temperature_from_multipliers([
            self.as_shot_wb[0] / g,
            1.0,
            self.as_shot_wb[2] / g,
        ])
    }
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
    cam_to_display: [[f32; 4]; 3],
    working_to_display: [[f32; 4]; 3],
    develop: [f32; 4],
    hsm_dims: [u32; 4],
    /// `[dest_x, dest_y, tile, halo]`. Rewritten per tile; everything above it
    /// moves only when the edit does, which is why this sits last and is
    /// patched in place rather than re-uploading the whole uniform.
    ///
    /// Signed: once the image can be panned, a tile beginning left of or above
    /// the viewport is ordinary rather than exceptional.
    present: [i32; 4],
    /// How much of the tile is inside the image, in pixels. See `present`.
    extent: [i32; 4],
}

/// Byte offset of `Params::present`, for the per-tile partial write.
const PRESENT_OFFSET: u64 = 176;

/// What the caller wants back.
///
/// Not two code paths: both run the same kernels in the same order, and
/// `Display` simply runs one stage more. The distinction is real product
/// behaviour rather than test scaffolding — a 16-bit linear export for a
/// specialist denoiser wants the scene-linear result, and so does the golden
/// harness, which should not be measuring the tone map when it means to be
/// measuring the demosaic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// Camera-native, demosaiced, scene-linear. No white balance, no profile,
    /// no tone map.
    SceneLinear,
    /// White balanced, profiled, exposed and tone mapped. Display-referred
    /// **linear** — the transfer function belongs to the output transform.
    Display,
}

const STAGES: [&str; 6] = [
    "conv",
    "green_at_rb",
    "rb_at_br",
    "rb_at_g",
    "pack",
    "develop",
];

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
pub struct Renderer {
    layout: wgpu::BindGroupLayout,
    canvas_layout: wgpu::BindGroupLayout,
    pipelines: Vec<wgpu::ComputePipeline>,
    present: wgpu::ComputePipeline,
    tile: u32,
}

impl Renderer {
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
        entries.extend((2..=7).map(|b| entry(b, false)));
        entries.push(entry(8, true));

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

        // The canvas lives in its own group so a window resize rebuilds one
        // small bind group rather than the per-image buffers beside it.
        let canvas_layout = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("canvas"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: CANVAS_FORMAT,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                }],
            });
        let present_layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("present layout"),
                bind_group_layouts: &[Some(&layout), Some(&canvas_layout)],
                immediate_size: 0,
            });
        let present = gpu
            .device
            .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("present"),
                layout: Some(&present_layout),
                module: &module,
                entry_point: Some("present"),
                compilation_options: Default::default(),
                cache: None,
            });

        Self {
            layout,
            canvas_layout,
            pipelines,
            present,
            tile,
        }
    }

    /// Buffers for one tile, allocated once and reused for every render.
    ///
    /// Sized by the tile and by this frame's profile — never by the image. The
    /// caller holds them so an interactive canvas can render frame after frame
    /// without allocating, which is the difference between a steady 60fps and
    /// one that stutters whenever the allocator is unlucky.
    pub fn allocate(&self, gpu: &Gpu, image: &Frame<'_>) -> TileBuffers {
        let device = &gpu.device;
        let padded = self.tile + 2 * HALO;
        let packed_width = padded.div_ceil(2);
        let px = padded as usize * padded as usize;

        let plane = |label: &str, len: usize, extra: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (len * std::mem::size_of::<f32>()) as u64,
                usage: wgpu::BufferUsages::STORAGE | extra,
                mapped_at_creation: false,
            })
        };

        // The hue/saturation table is sized from the profile rather than from
        // the edit, because its dimensions are a property of the file and only
        // its contents move with temperature. A bind group cannot have holes, so
        // a profile without a table still gets one identity cell.
        let table_cells = image
            .profile
            .hue_sat_map(5000.0)
            .map(|m| (m.hue_divisions * m.sat_divisions * m.value_divisions) as usize)
            .unwrap_or(1)
            .max(1);

        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rcd params"),
            size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let cfa = plane("cfa", px, wgpu::BufferUsages::COPY_DST);
        let vh = plane("vh", px, wgpu::BufferUsages::empty());
        // `pq` and `lp` share one buffer: WebGPU guarantees only eight storage
        // buffers per shader stage, and the develop stage needs one for the
        // profile's hue/saturation table. Raising the limit instead would make
        // the engine work here and fail on a conforming device, which is the
        // divergence the default-limits decision exists to prevent.
        let helpers = plane(
            "pq+lp",
            2 * packed_width as usize * padded as usize,
            wgpu::BufferUsages::empty(),
        );
        let ch_r = plane("r", px, wgpu::BufferUsages::empty());
        let ch_g = plane("g", px, wgpu::BufferUsages::empty());
        let ch_b = plane("b", px, wgpu::BufferUsages::empty());
        let out = plane("rgba", px * 4, wgpu::BufferUsages::COPY_SRC);
        let hue_sat = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("hue/sat map"),
            size: (table_cells * 4 * std::mem::size_of::<f32>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let out_size = (px * 4 * std::mem::size_of::<f32>()) as u64;
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: out_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let storage = [&cfa, &vh, &helpers, &ch_r, &ch_g, &ch_b, &out, &hue_sat];
        let mut bindings = vec![wgpu::BindGroupEntry {
            binding: 0,
            resource: params.as_entire_binding(),
        }];
        bindings.extend(
            storage
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

        TileBuffers {
            params,
            cfa,
            out,
            hue_sat,
            staging,
            bind_group,
            padded,
            out_size,
            table_cells,
            scratch: std::cell::RefCell::new(vec![0.0; px]),
            _held: vec![vh, helpers, ch_r, ch_g, ch_b],
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
    pub fn run(
        &self,
        gpu: &Gpu,
        image: &Frame<'_>,
        state: &EditState,
        intent: Output,
    ) -> Result<Vec<f32>, EngineError> {
        state.validate()?;
        let (w, h) = (image.width as usize, image.height as usize);
        if image.data.len() != w * h {
            return Err(EngineError::DeviceRequest(format!(
                "mosaic is {}x{} but carries {} samples",
                w,
                h,
                image.data.len()
            )));
        }

        let buffers = self.allocate(gpu, image);
        self.upload_params(gpu, &buffers, image, state)?;

        let mut result = vec![0.0f32; w * h * 4];
        for oy in (0..image.height).step_by(self.tile as usize) {
            for ox in (0..image.width).step_by(self.tile as usize) {
                let tile_px = self.render_at(
                    gpu,
                    &buffers,
                    image.data,
                    image.width,
                    image.height,
                    intent,
                    ox,
                    oy,
                )?;
                // Copy the interior only. The halo exists to make the interior
                // correct and is discarded.
                let valid_w = self.tile.min(image.width - ox) as usize;
                let valid_h = self.tile.min(image.height - oy) as usize;
                for y in 0..valid_h {
                    let src = ((y + HALO as usize) * buffers.padded as usize + HALO as usize) * 4;
                    let dst = ((oy as usize + y) * w + ox as usize) * 4;
                    result[dst..dst + valid_w * 4]
                        .copy_from_slice(&tile_px[src..src + valid_w * 4]);
                }
            }
        }

        Ok(result)
    }

    /// Render one tile of a [`Pyramid`], at that tile's resolution level.
    ///
    /// Returns `tile x tile` interleaved RGBA with the halo already trimmed —
    /// the canvas wants a rectangle it can present, not a padded one it has to
    /// know the geometry of.
    ///
    /// This is the entry point the interactive canvas uses, and the reason the
    /// pyramid exists: a tile at level *n* covers `tile << n` image pixels but
    /// costs exactly as much to render as one at level 0. Fit-to-screen is
    /// therefore a handful of tiles rather than the whole frame.
    ///
    /// Takes no [`Frame`], and that is the point: everything the camera and the
    /// edit contribute was resolved once by [`Renderer::set_edit`]. Drawing a
    /// tile is pure geometry, which is what makes a pan cost nothing but the
    /// tiles it exposes.
    #[allow(clippy::too_many_arguments)]
    pub fn render_tile(
        &self,
        gpu: &Gpu,
        buffers: &TileBuffers,
        pyramid: &Pyramid<'_>,
        level: u8,
        tx: u32,
        ty: u32,
        intent: Output,
    ) -> Result<Vec<f32>, EngineError> {
        let (data, lw, lh) = pyramid.level(level).ok_or_else(|| {
            EngineError::DeviceRequest(format!(
                "level {level} does not exist; the pyramid has {}",
                pyramid.levels()
            ))
        })?;
        let (ox, oy) = (tx * self.tile, ty * self.tile);
        if ox >= lw || oy >= lh {
            return Err(EngineError::DeviceRequest(format!(
                "tile ({tx}, {ty}) at level {level} starts outside a {lw}x{lh} mosaic"
            )));
        }

        // A reduced mosaic is a mosaic: same phase, same profile, same clip
        // level, fewer pixels. That is what makes a level render the ordinary
        // render rather than a second code path — and why nothing about the
        // frame's colour needs restating here. It went to the GPU with the edit.
        let padded = self.render_at(gpu, buffers, data, lw, lh, intent, ox, oy)?;

        let t = self.tile as usize;
        let stride = buffers.padded as usize;
        let mut out = vec![0.0f32; t * t * 4];
        for y in 0..t {
            let src = ((y + HALO as usize) * stride + HALO as usize) * 4;
            out[y * t * 4..(y + 1) * t * 4].copy_from_slice(&padded[src..src + t * 4]);
        }
        Ok(out)
    }

    /// One tile, halo included, straight off the GPU.
    #[allow(clippy::too_many_arguments)]
    fn render_at(
        &self,
        gpu: &Gpu,
        buffers: &TileBuffers,
        mosaic: &[f32],
        width: u32,
        height: u32,
        intent: Output,
        ox: u32,
        oy: u32,
    ) -> Result<Vec<f32>, EngineError> {
        {
            let mut scratch = buffers.scratch.borrow_mut();
            gather_padded(mosaic, width, height, ox, oy, buffers.padded, &mut scratch);
            gpu.queue
                .write_buffer(&buffers.cfa, 0, bytemuck::cast_slice(&scratch));
        }

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("rcd") });
        self.dispatch_stages(&mut encoder, buffers, intent);
        encoder.copy_buffer_to_buffer(&buffers.out, 0, &buffers.staging, 0, buffers.out_size);
        gpu.queue.submit([encoder.finish()]);

        let (tx, rx) = std::sync::mpsc::channel();
        buffers
            .staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r);
            });
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| EngineError::DeviceRequest(e.to_string()))?;
        rx.recv()
            .map_err(|_| EngineError::DeviceRequest("readback never completed".into()))?
            .map_err(|e| EngineError::DeviceRequest(e.to_string()))?;

        let pixels = {
            let view = buffers.staging.slice(..).get_mapped_range();
            bytemuck::cast_slice::<u8, f32>(&view).to_vec()
        };
        buffers.staging.unmap();
        Ok(pixels)
    }

    /// One pass per stage. The stage boundaries are vkdt's barriers: each reads
    /// what the previous one wrote, so they cannot be merged without
    /// reintroducing the workgroup-memory version.
    fn dispatch_stages(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        buffers: &TileBuffers,
        intent: Output,
    ) {
        let stages = match intent {
            Output::SceneLinear => &self.pipelines[..STAGES.len() - 1],
            Output::Display => &self.pipelines[..],
        };
        let groups = buffers.padded.div_ceil(8);
        for (pipeline, stage) in stages.iter().zip(STAGES) {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some(stage),
                timestamp_writes: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &buffers.bind_group, &[]);
            pass.dispatch_workgroups(groups, groups, 1);
        }
    }

    /// A canvas to draw tiles into. Sized in screen pixels and rebuilt on
    /// resize; the per-image buffers beside it are untouched by that.
    pub fn create_canvas(&self, gpu: &Gpu, width: u32, height: u32) -> Canvas {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("canvas"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: CANVAS_FORMAT,
            // STORAGE to be written by `present`, TEXTURE_BINDING so a surface
            // blit can sample it, COPY_SRC so a test can check what landed.
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("canvas"),
            layout: &self.canvas_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            }],
        });
        Canvas {
            texture,
            bind_group,
            size: [width.max(1), height.max(1)],
        }
    }

    /// Draw one tile into `canvas` at `dest`, and **do not wait for it**.
    ///
    /// This is the interactive path, and the whole difference from
    /// [`Renderer::render_tile`] is the absence of a readback. That call ends in
    /// `map_async` and a blocking device poll — a full CPU/GPU sync per tile,
    /// which at a dozen visible tiles is a dozen stalls a frame. Nothing about
    /// tiling or resolution levels reaches 60fps through those.
    ///
    /// Work is submitted and the function returns. The result becomes visible
    /// when the caller presents the surface, which is the only place a frame
    /// should synchronise.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_tile(
        &self,
        gpu: &Gpu,
        buffers: &TileBuffers,
        canvas: &Canvas,
        pyramid: &Pyramid<'_>,
        level: u8,
        tx: u32,
        ty: u32,
        dest: [i32; 2],
        intent: Output,
    ) -> Result<(), EngineError> {
        let (data, lw, lh) = pyramid.level(level).ok_or_else(|| {
            EngineError::DeviceRequest(format!(
                "level {level} does not exist; the pyramid has {}",
                pyramid.levels()
            ))
        })?;
        let (ox, oy) = (tx * self.tile, ty * self.tile);
        if ox >= lw || oy >= lh {
            return Err(EngineError::DeviceRequest(format!(
                "tile ({tx}, {ty}) at level {level} starts outside a {lw}x{lh} mosaic"
            )));
        }

        {
            let mut scratch = buffers.scratch.borrow_mut();
            gather_padded(data, lw, lh, ox, oy, buffers.padded, &mut scratch);
            gpu.queue
                .write_buffer(&buffers.cfa, 0, bytemuck::cast_slice(&scratch));
        }
        // Patch only the tail of the uniform. The colour half was resolved once
        // by `set_edit` and does not move because a different tile is being
        // drawn — writes and submits are ordered on the queue, so this lands
        // before the dispatch that reads it.
        // Everything the present pass needs, written as one patch at the tail of
        // the uniform: where the tile goes, and how much of it is real.
        let tail: [i32; 8] = [
            dest[0],
            dest[1],
            self.tile as i32,
            HALO as i32,
            (lw - ox).min(self.tile) as i32,
            (lh - oy).min(self.tile) as i32,
            0,
            0,
        ];
        gpu.queue
            .write_buffer(&buffers.params, PRESENT_OFFSET, bytemuck::cast_slice(&tail));

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("draw tile"),
            });
        self.dispatch_stages(&mut encoder, buffers, intent);
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("present"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.present);
            pass.set_bind_group(0, &buffers.bind_group, &[]);
            pass.set_bind_group(1, &canvas.bind_group, &[]);
            let groups = self.tile.div_ceil(8);
            pass.dispatch_workgroups(groups, groups, 1);
        }
        gpu.queue.submit([encoder.finish()]);
        Ok(())
    }

    /// Resolve the edit against the frame's profile and upload the result.
    ///
    /// Separate from rendering because it depends on the *edit* and not on which
    /// tile is being drawn: a slider move rewrites this and nothing else, while
    /// a pan rewrites nothing at all.
    fn upload_params(
        &self,
        gpu: &Gpu,
        buffers: &TileBuffers,
        image: &Frame<'_>,
        state: &EditState,
    ) -> Result<(), EngineError> {
        let padded = buffers.padded;
        let (dx, dy) = image.phase.offset();
        // Green normalised to 1.0. The demosaic kernel estimates green at red
        // and blue sites from unscaled CFA values, so any other normalisation
        // would mix scaled and unscaled greens in the same subtraction.
        let colour = image.colour(state)?;
        let (wb, m) = (colour.multipliers, colour.cam_to_display);
        let working = colour.working_to_display;
        let hsm = colour.hue_sat.as_ref();
        let params = Params {
            width: padded,
            height: padded,
            packed_width: padded.div_ceil(2),
            cfa_x_offset: dx,
            cfa_y_offset: dy,
            _pad: [0; 3],
            wb: [wb[0], wb[1], wb[2], 1.0],
            cam_to_display: [
                [m[0][0], m[0][1], m[0][2], 0.0],
                [m[1][0], m[1][1], m[1][2], 0.0],
                [m[2][0], m[2][1], m[2][2], 0.0],
            ],
            working_to_display: [
                [working[0][0], working[0][1], working[0][2], 0.0],
                [working[1][0], working[1][1], working[1][2], 0.0],
                [working[2][0], working[2][1], working[2][2], 0.0],
            ],
            develop: [
                crate::exposure_multiplier(state),
                if hsm.is_some() { 1.0 } else { 0.0 },
                image.clip_level,
                0.0,
            ],
            hsm_dims: match hsm {
                Some(m) => [m.hue_divisions, m.sat_divisions, m.value_divisions, 0],
                None => [1, 1, 1, 0],
            },
            present: [0, 0, self.tile as i32, HALO as i32],
            extent: [self.tile as i32, self.tile as i32, 0, 0],
        };
        gpu.queue
            .write_buffer(&buffers.params, 0, bytemuck::bytes_of(&params));

        let table: Vec<[f32; 4]> = match hsm {
            Some(m) => m.deltas.iter().map(|d| [d[0], d[1], d[2], 0.0]).collect(),
            None => vec![[0.0, 1.0, 1.0, 0.0]],
        };
        if table.len() != buffers.table_cells {
            return Err(EngineError::DeviceRequest(format!(
                "hue/sat table is {} cells but buffers were allocated for {}; \
                 the buffers belong to a different profile",
                table.len(),
                buffers.table_cells
            )));
        }
        gpu.queue
            .write_buffer(&buffers.hue_sat, 0, bytemuck::cast_slice(&table));
        Ok(())
    }

    /// Upload an edit without rendering, for a canvas that changes the slider
    /// and then draws many tiles with it.
    pub fn set_edit(
        &self,
        gpu: &Gpu,
        buffers: &TileBuffers,
        image: &Frame<'_>,
        state: &EditState,
    ) -> Result<(), EngineError> {
        state.validate()?;
        self.upload_params(gpu, buffers, image, state)
    }
}

/// GPU buffers for one tile. See [`Renderer::allocate`].
pub struct TileBuffers {
    params: wgpu::Buffer,
    cfa: wgpu::Buffer,
    out: wgpu::Buffer,
    hue_sat: wgpu::Buffer,
    staging: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    padded: u32,
    out_size: u64,
    table_cells: usize,
    /// CPU staging for the gather. Interior-mutable so that rendering a tile
    /// takes `&self`: the canvas holds one set of buffers and draws from a
    /// shared reference.
    scratch: std::cell::RefCell<Vec<f32>>,
    /// Bound by the bind group but never touched by name again.
    _held: Vec<wgpu::Buffer>,
}

/// What the canvas holds.
///
/// Display-referred **linear**, in half floats. Not 8-bit, because these values
/// have no transfer function applied yet and eight bits of linear bands visibly
/// in the shadows once encoded; not 32-bit, because half floats already carry
/// more precision than a display can resolve and a canvas spends its budget on
/// bandwidth.
pub const CANVAS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// A GPU-resident destination for rendered tiles.
///
/// The pixels stay here. A surface blit samples this texture and applies the
/// output transform on the way to the screen; nothing on the interactive path
/// copies them to the CPU, which is the entire point.
pub struct Canvas {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    size: [u32; 2],
}

impl Canvas {
    /// The underlying texture, for a surface blit to sample.
    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    pub fn size(&self) -> [u32; 2] {
        self.size
    }

    /// Pull the canvas back to the CPU as interleaved RGBA.
    ///
    /// For tests and for anything offline. Deliberately *not* part of drawing a
    /// frame: this is the synchronising, stalling operation that
    /// [`Renderer::draw_tile`] exists to keep off the hot path.
    pub fn read_back(&self, gpu: &Gpu) -> Result<Vec<f32>, EngineError> {
        const BYTES_PER_PIXEL: u32 = 8;
        let [w, h] = self.size;
        // Texture-to-buffer copies want rows aligned to 256 bytes, which an
        // arbitrary canvas width will not be. Pad the copy and drop the padding
        // on the way out rather than constraining what widths are allowed.
        let padded_row = (w * BYTES_PER_PIXEL).div_ceil(256) * 256;
        let staging = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("canvas readback"),
            size: (padded_row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("canvas readback"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
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
        gpu.queue.submit([encoder.finish()]);

        let (tx, rx) = std::sync::mpsc::channel();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| EngineError::DeviceRequest(e.to_string()))?;
        rx.recv()
            .map_err(|_| EngineError::DeviceRequest("readback never completed".into()))?
            .map_err(|e| EngineError::DeviceRequest(e.to_string()))?;

        let mut out = Vec::with_capacity((w * h * 4) as usize);
        {
            let view = staging.slice(..).get_mapped_range();
            for y in 0..h {
                let row = (y * padded_row) as usize;
                let halves: &[u16] =
                    bytemuck::cast_slice(&view[row..row + (w * BYTES_PER_PIXEL) as usize]);
                out.extend(halves.iter().copied().map(half_to_f32));
            }
        }
        staging.unmap();
        Ok(out)
    }
}

/// IEEE 754 binary16 to binary32.
///
/// Written out rather than pulled from a crate: it is fifteen lines, it is used
/// only off the hot path, and every dependency in this workspace costs a licence
/// review (see `docs/licence-policy.md`).
fn half_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) as u32) << 31;
    let exponent = ((h >> 10) & 0x1f) as u32;
    let mantissa = (h & 0x3ff) as u32;
    let bits = match exponent {
        // Zero or subnormal. Every subnormal half is a *normal* float, so the
        // leading bit has to be located and the exponent rebuilt around it —
        // the one branch here with real arithmetic in it, and the reason
        // `half_floats_decode_exactly` covers it explicitly.
        0 if mantissa == 0 => sign,
        0 => {
            let leading = mantissa.leading_zeros();
            let msb = 31 - leading;
            sign | ((134 - leading) << 23) | ((mantissa << (23 - msb)) & 0x7f_ffff)
        }
        // Infinity or NaN.
        31 => sign | 0x7f80_0000 | (mantissa << 13),
        _ => sign | ((exponent + 127 - 15) << 23) | (mantissa << 13),
    };
    f32::from_bits(bits)
}

/// A Bayer mosaic and its phase-preserving reductions.
///
/// # Why a mosaic pyramid rather than an image pyramid
///
/// The obvious way to show a zoomed-out photo is to demosaic it and shrink the
/// result. That costs a full-resolution demosaic for a view that shows a
/// fraction of the pixels — precisely the work fit-to-screen should avoid.
///
/// Reducing the *mosaic* instead keeps the cost proportional to what is shown.
/// The reduction has to preserve the CFA pattern to do it: each output 2x2 block
/// averages the corresponding site of four input 2x2 blocks, so red averages
/// with red and the two greens stay distinct. Striding by a power of two instead
/// would land every output pixel on the same colour and produce a mosaic that is
/// no longer Bayer.
///
/// Built once per image, because the alternative — reducing on demand — reads
/// the whole frame for every coarse tile.
///
/// # The one place preview and export can differ
///
/// Averaging softens the edges of blown highlights: a sample that was at the
/// clipping level can average with unclipped neighbours and land below it, so
/// reconstruction does not fire there. Large blown areas are unaffected, because
/// their interior averages clipped with clipped. At level 0 — a 1:1 view, and
/// every export — there is no averaging and no difference at all.
pub struct Pyramid<'a> {
    base: &'a [f32],
    base_size: (u32, u32),
    reduced: Vec<(Vec<f32>, u32, u32)>,
}

impl<'a> Pyramid<'a> {
    /// Reduce until one tile covers the whole mosaic. Going further would build
    /// levels no viewport can ask for.
    pub fn build(image: &Frame<'a>, tile: u32) -> Self {
        let mut reduced: Vec<(Vec<f32>, u32, u32)> = Vec::new();
        let (mut w, mut h) = (image.width, image.height);
        while w.max(h) > tile {
            let (data, nw, nh) = match reduced.last() {
                Some((prev, pw, ph)) => reduce(prev, *pw, *ph),
                None => reduce(image.data, image.width, image.height),
            };
            // A mosaic smaller than one 2x2 block has no pattern left to keep.
            if nw < 2 || nh < 2 {
                break;
            }
            w = nw;
            h = nh;
            reduced.push((data, nw, nh));
        }
        Self {
            base: image.data,
            base_size: (image.width, image.height),
            reduced,
        }
    }

    /// The mosaic at `level`, or `None` if the pyramid does not go that deep.
    pub fn level(&self, level: u8) -> Option<(&[f32], u32, u32)> {
        match level {
            0 => Some((self.base, self.base_size.0, self.base_size.1)),
            n => self
                .reduced
                .get(n as usize - 1)
                .map(|(d, w, h)| (d.as_slice(), *w, *h)),
        }
    }

    /// The coarsest level available.
    pub fn levels(&self) -> u8 {
        self.reduced.len() as u8
    }
}

/// Halve a Bayer mosaic while keeping its pattern.
///
/// Output pixel `(2bx + i, 2by + j)` is the mean of the four input pixels at the
/// same position `(i, j)` within the four 2x2 blocks that make up the region —
/// so the colour at each site is unchanged and the phase is identical.
///
/// Dimensions round down to a whole number of output blocks, which can drop up
/// to three pixels at the right and bottom edge per level. That is invisible in
/// a preview and cannot affect an export, which always renders at level 0.
fn reduce(src: &[f32], w: u32, h: u32) -> (Vec<f32>, u32, u32) {
    let blocks_x = w / 4;
    let blocks_y = h / 4;
    let (nw, nh) = (blocks_x * 2, blocks_y * 2);
    let mut dst = vec![0.0f32; (nw * nh) as usize];
    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            for j in 0..2u32 {
                for i in 0..2u32 {
                    let mut sum = 0.0f32;
                    for l in 0..2u32 {
                        for k in 0..2u32 {
                            let sx = 4 * bx + 2 * k + i;
                            let sy = 4 * by + 2 * l + j;
                            sum += src[(sy * w + sx) as usize];
                        }
                    }
                    dst[((2 * by + j) * nw + 2 * bx + i) as usize] = sum * 0.25;
                }
            }
        }
    }
    (dst, nw, nh)
}

/// Everything the develop stage needs to know about colour, resolved from the
/// frame's profile and the edit together.
struct Colour {
    multipliers: [f32; 3],
    cam_to_display: Matrix3,
    working_to_display: Matrix3,
    hue_sat: Option<crate::profile::HueSatMap>,
}

/// Fill `out` with the tile at `(ox, oy)` plus its halo, clamping at the image
/// edge.
///
/// Clamping here has to match what the shader does when it reads out of bounds,
/// or the tiled result would differ from an untiled one along the image border.
/// Both clamp to the nearest edge pixel.
#[allow(clippy::too_many_arguments)]
fn gather_padded(
    mosaic: &[f32],
    width: u32,
    height: u32,
    ox: u32,
    oy: u32,
    padded: u32,
    out: &mut [f32],
) {
    let w = width as i64;
    let h = height as i64;
    for py in 0..padded as i64 {
        let gy = (oy as i64 - HALO as i64 + py).clamp(0, h - 1);
        let row = (gy * w) as usize;
        let dst = (py * padded as i64) as usize;
        for px in 0..padded as i64 {
            let gx = (ox as i64 - HALO as i64 + px).clamp(0, w - 1);
            out[dst + px as usize] = mosaic[row + gx as usize];
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

#[cfg(test)]
mod tests {
    use super::half_to_f32;

    /// Hand-written bit manipulation, so it gets a test that names values rather
    /// than trusting it to look right. Subnormals are the branch worth the
    /// trouble: they are the only one that has to find a leading bit and rebuild
    /// an exponent, and a canvas would show the error as a black shadow region
    /// rather than as anything obviously wrong.
    #[test]
    fn half_floats_decode_exactly() {
        let cases: [(u16, f32); 10] = [
            (0x0000, 0.0),
            (0x8000, -0.0),
            (0x3c00, 1.0),
            (0xc000, -2.0),
            (0x3800, 0.5),
            (0x3555, 0.333_251_95),
            // Largest and smallest normal halves.
            (0x7bff, 65504.0),
            (0x0400, 6.103_515_6e-5),
            // Subnormals: the smallest positive half, and the largest subnormal.
            (0x0001, 5.960_464_5e-8),
            (0x03ff, 6.097_555e-5),
        ];
        for (bits, expected) in cases {
            let found = half_to_f32(bits);
            assert_eq!(
                found.to_bits(),
                expected.to_bits(),
                "0x{bits:04x} decoded to {found:e}, not {expected:e}"
            );
        }
        assert!(half_to_f32(0x7c00).is_infinite());
        assert!(half_to_f32(0x7e00).is_nan());
    }
}
