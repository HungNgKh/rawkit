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
//! **The whole-image path pays that sync once per tile, and does not stand
//! still for it.** [`Renderer::run`] queues tile *n* before collecting tile
//! *n-1*, so the GPU renders while the CPU gathers and uploads the next tile.
//! Two readback buffers, used alternately, because one cannot be the
//! destination of a copy while it is mapped. Measured on a 24 megapixel frame:
//! 830 ms to 675 ms, about a fifth, and the pixels are bit-identical.
//!
//! That only works because the wait names *which* submission it is waiting
//! for. `wait_indefinitely` blocks until everything queued has finished, so a
//! loop that submitted the next tile first would wait for that one too and gain
//! exactly nothing — see the paragraph below, which is the same fact from the
//! other side.
//!
//! **A poll waits for the whole queue, not for this caller's work.** So the cost
//! of a small readback depends on what was submitted before it, and *when* a
//! caller reads back matters as much as how much it reads. The shell's histogram
//! survey is 94,000 pixels either way and took 33-45 ms behind a canvas pass
//! against 6-9 ms in front of one. Anything that reads pixels back while a
//! canvas is being drawn is paying for the canvas.
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
use rawkit_editstate::{EditState, Geometry};

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
    pub(crate) fn offset(self) -> (u32, u32) {
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
    /// What the camera says it takes to stand this frame upright.
    ///
    /// Beside `as_shot_wb` because it is the same kind of thing: a fact the file
    /// carries, which the matching `EditState` field resolves to when the user
    /// has not overridden it. `Orientation::AsShot` means *this*, and a rotation
    /// the user asks for composes on top of it.
    ///
    /// A field with no default, so a caller that has one and forgets to pass it
    /// fails to compile rather than rendering every portrait frame on its side.
    pub recorded_orientation: rawkit_editstate::Orientation,
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
        let (multipliers, temperature, tint) = match state.white_balance.temperature_k {
            Some(temperature) => (
                self.profile
                    .multipliers_for(temperature, state.white_balance.tint),
                temperature,
                state.white_balance.tint,
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
                let (temperature, tint) = self.profile.temperature_from_multipliers(as_shot);
                (as_shot, temperature, tint)
            }
        };
        // With a hue/saturation table the render goes camera -> working space
        // -> table -> display; without one it goes straight to display and the
        // second matrix is the identity, so the kernel has a single path either
        // way.
        // Ask for the transform and the table together, so there is no way to
        // end up with one and not the other.
        // The working-space hop is worth taking for *either* table. It used to
        // be conditional on the hue/saturation correction alone, which meant a
        // Camera Matching profile — which carries no such correction and keeps
        // everything in its look — took the direct path and had its look
        // discarded.
        let working = self.profile.camera_to_working(temperature);
        let hue_sat = self.profile.hue_sat_map(temperature);
        let look = self.profile.look_table().cloned();

        Ok(match working {
            Some((to_working, to_display)) if hue_sat.is_some() || look.is_some() => Colour {
                multipliers,
                temperature,
                tint,
                cam_to_display: to_working,
                working_to_display: to_display,
                hue_sat,
                look,
                look_is_srgb: self.profile.look_is_srgb,
                tone: self.profile.tone_curve().map(|lut| lut.to_vec()),
                user_curve: crate::tone::user_curve_lut(&state.curve),
            },
            // No forward matrix means no way to reach the space the tables were
            // authored in, so there is nothing to apply them to.
            _ => Colour {
                multipliers,
                temperature,
                tint,
                cam_to_display: self.profile.camera_to_display(temperature),
                working_to_display: crate::profile::IDENTITY,
                hue_sat: None,
                look: None,
                look_is_srgb: false,
                // The curve needs no working space, so it survives the path
                // that has no forward matrix to reach one.
                tone: self.profile.tone_curve().map(|lut| lut.to_vec()),
                user_curve: crate::tone::user_curve_lut(&state.curve),
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
    /// The display space back to the profile's working space, so the look can
    /// be applied where it was authored. The inverse of `working_to_display`,
    /// and only meaningful when a look is active.
    display_to_working: [[f32; 4]; 3],
    /// `[hue, saturation, value]` divisions of the look table, and `.w` its
    /// offset in cells into the shared table buffer.
    look_dims: [u32; 4],
    /// `[offset, entries, active, unused]` for the profile's tone curve, in the
    /// same shared buffer.
    curve: [u32; 4],
    /// `[hue, saturation, luminance, unused]` for the shadows, midtones and
    /// highlights in that order.
    grade: [[f32; 4]; 3],
    /// `[blending, balance, active, unused]`.
    grade_shape: [f32; 4],
    /// The same, for the user's curve.
    ///
    /// Its slot in the buffer is reserved whatever the edit says, unlike the
    /// profile's: a profile is fixed when a photograph opens and an edit is not,
    /// so sizing this from the edit would mean a buffer that has to grow when
    /// somebody adds a control point.
    user_curve: [u32; 4],
    /// `[contrast exponent, highlights, shadows, active]` — see [`crate::tone`].
    tone: [f32; 4],
    /// `[black point, white point, unused, unused]`.
    levels: [f32; 4],
    /// `[sharpen amount, sharpen radius, chroma noise, luminance noise]`.
    detail: [f32; 4],
    /// `[saturation, vibrance, hue mixer active, unused]`.
    colour: [f32; 4],
    /// The eight-band mixer, one control per array, two bands to a row: a
    /// uniform array's stride is a `vec4` whatever it holds, so eight floats
    /// occupy two of them.
    hsl_hue: [[f32; 4]; 2],
    hsl_saturation: [[f32; 4]; 2],
    hsl_luminance: [[f32; 4]; 2],
    /// The local-tone guide: `[start in the cfa buffer, width, height, active]`.
    ///
    /// It rides in the tail of the `cfa` binding because group 0 already spends
    /// all eight storage buffers WebGPU guarantees on its default limits — the
    /// same reason `pq` and `lp` share one. Read-only and written once when the
    /// image opens, so sharing costs a longer buffer and nothing else.
    guide: [u32; 4],
    /// Guide texels per image pixel, `[x, y]`; `.z` is 1 when the guide's chroma
    /// field holds a real colour and 0 when the frame was blown end to end.
    /// A constant of the image; `.w` unused.
    guide_scale: [f32; 4],
    /// How many local adjustments are bound, in `[0]`.
    masks: [u32; 4],
    /// One over the image's size, `[x, y]`: image pixels to the normalised
    /// coordinates a mask texture is sampled in.
    mask_scale: [f32; 4],
    /// What each local adjustment multiplies by at full strength.
    ///
    /// Exposure and white balance arrive combined, because both are multiplies
    /// in the space the mask composites in and the shader has no reason to know
    /// which part came from which control.
    mask_gain: [[f32; 4]; rawkit_editstate::MAX_MASKS],
    /// `[dest_x, dest_y, tile, halo]`. Rewritten per tile; everything above it
    /// moves only when the edit does, which is why this sits last and is
    /// patched in place rather than re-uploading the whole uniform.
    ///
    /// Signed: once the image can be panned, a tile beginning left of or above
    /// the viewport is ordinary rather than exceptional.
    present: [i32; 4],
    /// How much of the tile is inside the image, in pixels. See `present`.
    extent: [i32; 4],
    /// The rotation, as the two columns of a signed permutation: where a step
    /// along the tile's x and y axes lands on the canvas. Identity when the
    /// photograph is not turned. Written with `present`, since both are per-tile.
    axes: [i32; 4],
    /// `[image_x, image_y, step, halo]` — where this tile's first interior
    /// pixel sits in the *full-resolution* image, and how many image pixels one
    /// tile pixel spans at this level.
    ///
    /// The guide is indexed through this rather than through tile coordinates,
    /// which is what makes two tiles agree where they meet and a tile at level 3
    /// read the same guide as the same region at level 0. Written per tile, and
    /// by both render paths — unlike `present`, which only the canvas needs.
    source: [i32; 4],
}

/// One control across all eight bands, in `Band::ALL` order, laid out the way a
/// uniform array wants it.
///
/// The order is the contract with `band_centre` in the shader, and
/// `band_centres_match_the_shader` is what holds the two to it.
fn pack_bands(
    hsl: &rawkit_editstate::Hsl,
    pick: impl Fn(rawkit_editstate::BandMix) -> f32,
) -> [[f32; 4]; 2] {
    let mut packed = [[0.0f32; 4]; 2];
    for (i, band) in rawkit_editstate::Band::ALL.into_iter().enumerate() {
        packed[i / 4][i % 4] = pick(hsl.mix(band));
    }
    packed
}

/// Copy a rendered tile's interior into the frame it belongs to.
///
/// The halo exists to make the interior correct and is discarded, and a tile at
/// the right or bottom edge overhangs the image and is trimmed. Lifted out of
/// the loop because the pipeline drains once more after it, and two copies of
/// this arithmetic is two chances to trim differently.
#[allow(clippy::too_many_arguments)]
fn place(
    result: &mut [f32],
    tile: &[f32],
    width: usize,
    padded: u32,
    edge: u32,
    image: &Frame<'_>,
    ox: u32,
    oy: u32,
) {
    let valid_w = edge.min(image.width - ox) as usize;
    let valid_h = edge.min(image.height - oy) as usize;
    for y in 0..valid_h {
        let src = ((y + HALO as usize) * padded as usize + HALO as usize) * 4;
        let dst = ((oy as usize + y) * width + ox as usize) * 4;
        result[dst..dst + valid_w * 4].copy_from_slice(&tile[src..src + valid_w * 4]);
    }
}

/// A tile whose work is on the queue and whose pixels have been asked for.
///
/// Holds the submission it belongs to, so [`Renderer::collect_tile`] can wait
/// for that one rather than for everything submitted since.
struct InFlight {
    index: wgpu::SubmissionIndex,
    slot: usize,
    rx: std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>,
}

/// Byte offset of `Params::present`, for the per-tile partial write.
///
/// Computed rather than written down. It was 176 until the tone controls added
/// two `vec4`s above it, and a hand-maintained offset that silently drifts
/// writes a tile's position over somebody else's uniform — which shows up as a
/// wrong-looking picture, not as an error.
const PRESENT_OFFSET: u64 = std::mem::offset_of!(Params, present) as u64;

/// Byte offset of `Params::source`, for the same reason and by the same route.
///
/// Written separately from the `present` block because the whole-image path
/// needs this one and not that one: an export has no canvas to place a tile on,
/// and it still has to tell the guide where the tile is.
const SOURCE_OFFSET: u64 = std::mem::offset_of!(Params, source) as u64;

/// Where the two buffers are looking, for [`Renderer::straighten`].
///
/// Three values that are only meaningful together: the level they are measured
/// in, and the point each buffer's top-left pixel stands for. Passed as one
/// because a caller holding a straight origin and a flat buffer sized for a
/// different level draws a photograph that is subtly in the wrong place.
#[derive(Debug, Clone, Copy)]
pub struct StraightenView {
    /// The mosaic's size at the level being drawn.
    pub level_image: [u32; 2],
    /// The straight-space point of the canvas's top-left pixel.
    pub straight_origin: [f32; 2],
    /// The flat-space point of the flat buffer's top-left pixel.
    pub flat_origin: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct StraightenParams {
    straight_origin: [f32; 2],
    flat_origin: [f32; 2],
    origin: [f32; 2],
    dx: [f32; 2],
    dy: [f32; 2],
    extent: [u32; 2],
    photograph: [f32; 2],
}

/// A developed photograph: the pixels, and how wide they are.
///
/// The two travel together because they are only correct together. Before crop
/// existed a caller could reasonably assume the result was the sensor's size;
/// now that assumption is wrong exactly when someone has cropped, which is the
/// case least likely to be tried before shipping. Handing back a width makes the
/// mistake impossible rather than unlikely.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    /// Interleaved RGBA, `width * height * 4` long.
    pub pixels: Vec<f32>,
    pub width: u32,
    pub height: u32,
}

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

const STAGES: [&str; 12] = [
    "conv",
    "green_at_rb",
    "rb_at_br",
    "rb_at_g",
    "pack",
    "chroma_blur",
    "chroma_mix",
    "luminance_blur",
    "luminance_mix",
    "develop",
    "luma",
    "sharpen",
];

/// How many of those stages leave the frame in scene-linear light.
///
/// Named rather than counted back from the end: it used to be `len() - 1`, which
/// was right only while `develop` was last and would have silently started
/// returning sharpened pixels the moment anything was appended.
const SCENE_LINEAR_STAGES: usize = 9;

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
/// | `chroma_blur` | RGB ±2 (itself reach 11) | 13 |
/// | `luminance_blur` | RGB ±3 (itself reach 13) | 16 |
/// | `sharpen` | `vh` ±2 (itself the denoised value, reach 16) | 18 |
///
/// Kept **even**, which is not cosmetic: an odd halo shifts the CFA phase inside
/// the tile, and every pixel would come out the wrong colour.
///
/// It was 12 before capture sharpening, 14 before chroma noise reduction and 16
/// before luminance noise reduction. That last one is why the bilateral reaches
/// three rather than two: at ±2 the chain comes to 17 and rounds to 18 anyway,
/// so the wider kernel is free.
/// Get this wrong and the symptom is a faint grid at the tile boundaries on
/// detailed frames — which is why `a_level_zero_tile_is_identical_to_the_whole_image_render`
/// is the test that guards it rather than anything that looks at a photograph.
pub const HALO: u32 = 18;

/// Tile edge in pixels, excluding halo. 512 keeps every buffer far inside
/// WebGPU's default limits while leaving the halo a small fraction of the work
/// (a 512 tile carries 536² samples, so ~9% overhead).
pub const DEFAULT_TILE: u32 = 512;

/// Compiled RCD pipelines. Build once, reuse — shader compilation is far too
/// slow to sit on a render path.
pub struct Renderer {
    layout: wgpu::BindGroupLayout,
    mask_layout: wgpu::BindGroupLayout,
    canvas_layout: wgpu::BindGroupLayout,
    pipelines: Vec<wgpu::ComputePipeline>,
    present: wgpu::ComputePipeline,
    straighten_layout: wgpu::BindGroupLayout,
    straighten_pipeline: wgpu::ComputePipeline,
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
        // Local adjustments, in their own group: a sampled texture array and a
        // filtering sampler. Separate from group 0 because the masks change when
        // the *edit* changes and the buffers beside them do not, and separate
        // from the canvas because that changes when the *window* does.
        let mask_layout = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("masks"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2Array,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let pipeline_layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("rcd layout"),
                bind_group_layouts: &[Some(&layout), Some(&mask_layout)],
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
                bind_group_layouts: &[Some(&layout), Some(&mask_layout), Some(&canvas_layout)],
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

        // Straighten stands apart from the six-stage chain: it reads a texture
        // rather than the tile buffers, so it needs its own layout rather than
        // three unused bindings in the shared one.
        let straighten_layout =
            gpu.device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("straighten layout"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::COMPUTE,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::COMPUTE,
                            ty: wgpu::BindingType::Texture {
                                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                                view_dimension: wgpu::TextureViewDimension::D2,
                                multisampled: false,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 2,
                            visibility: wgpu::ShaderStages::COMPUTE,
                            ty: wgpu::BindingType::StorageTexture {
                                access: wgpu::StorageTextureAccess::WriteOnly,
                                format: CANVAS_FORMAT,
                                view_dimension: wgpu::TextureViewDimension::D2,
                            },
                            count: None,
                        },
                    ],
                });
        let straighten_module = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("straighten"),
                source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/straighten.wgsl").into()),
            });
        let straighten_pipeline =
            gpu.device
                .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some("straighten"),
                    layout: Some(&gpu.device.create_pipeline_layout(
                        &wgpu::PipelineLayoutDescriptor {
                            label: Some("straighten layout"),
                            bind_group_layouts: &[Some(&straighten_layout)],
                            immediate_size: 0,
                        },
                    )),
                    module: &straighten_module,
                    entry_point: Some("straighten"),
                    compilation_options: Default::default(),
                    cache: None,
                });

        Self {
            layout,
            mask_layout,
            canvas_layout,
            pipelines,
            present,
            straighten_layout,
            straighten_pipeline,
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
        // Room for both tables, since they share the buffer. A profile without
        // one still gets a single identity cell, because a bind group cannot
        // have holes.
        let table_cells = image
            .profile
            .hue_sat_map(5000.0)
            .map(|m| m.cell_count())
            .unwrap_or(1)
            .max(1)
            + image
                .profile
                .look_table()
                .map(|m| m.cell_count())
                .unwrap_or(1)
                .max(1)
            + image
                .profile
                .tone_curve()
                .map(|lut| lut.len())
                .unwrap_or(1)
                .max(1)
            // The user's curve, whether or not there is one yet.
            + crate::profile::TONE_LUT;

        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rcd params"),
            size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // The tile's mosaic, and behind it the local-tone guide. One buffer
        // because the eight storage bindings are all spent; see `Params::guide`.
        // Capacity is a constant rather than this image's size, so the
        // allocation does not depend on what was opened.
        let cfa = plane(
            "cfa+guide",
            px + crate::guide::CAPACITY,
            wgpu::BufferUsages::COPY_DST,
        );
        let guide = crate::guide::Guide::build(
            image.data,
            image.width,
            image.height,
            image.phase,
            image.clip_level,
        );
        gpu.queue.write_buffer(
            &cfa,
            (px * std::mem::size_of::<f32>()) as u64,
            bytemuck::cast_slice(&guide.data),
        );
        gpu.queue.write_buffer(
            &cfa,
            ((px + guide.data.len()) * std::mem::size_of::<f32>()) as u64,
            bytemuck::cast_slice(&guide.chroma),
        );
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
        // Two, so a tile can be read while the next one is being written into
        // the other. A buffer cannot be the destination of a copy while it is
        // mapped, which is the entire reason the number is two and not one.
        let staging = std::array::from_fn(|i| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(if i == 0 { "readback a" } else { "readback b" }),
                size: out_size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            })
        });

        // One layer per possible local adjustment. Sixteen-bit float rather
        // than eight-bit: a mask multiplies a gain, so a step in the mask is a
        // step in the picture, and 256 of them across a clear sky is the kind of
        // banding this project has already refused once for the canvas.
        let (mask_w, mask_h) = crate::mask::dimensions(image.width, image.height);
        let mask_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("masks"),
            size: wgpu::Extent3d {
                width: mask_w,
                height: mask_h,
                depth_or_array_layers: rawkit_editstate::MAX_MASKS as u32,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mask_view = mask_texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let mask_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("mask"),
            // Clamped, so a gradient keeps whatever it says at the frame's edge
            // rather than wrapping round to the other side of the picture.
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let mask_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("masks"),
            layout: &self.mask_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&mask_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&mask_sampler),
                },
            ],
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
            guide_offset: px,
            guide_size: [guide.width, guide.height],
            chroma_known: guide.chroma_known,
            mask_texture,
            mask_bind_group,
            mask_size: [mask_w, mask_h],
            uploaded_masks: std::cell::RefCell::new(Vec::new()),
            mask_scratch: std::cell::RefCell::new(vec![0.0; (mask_w * mask_h) as usize]),
            guide_scale: [
                guide.width as f32 / image.width.max(1) as f32,
                guide.height as f32 / image.height.max(1) as f32,
            ],
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
    ) -> Result<Rendered, EngineError> {
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

        // One tile ahead of itself. The GPU renders tile *n* while this gathers
        // and uploads tile *n+1*, and only then waits for tile *n* — which by
        // that point is usually already done.
        //
        // The order inside the loop is the whole trick, and it is the opposite
        // of the obvious one: submit first, collect second. Collecting first
        // would leave the GPU idle for as long as a gather takes, every tile.
        //
        // Measured on a 24 megapixel frame: 96 tiles spent 209 ms of an 830 ms
        // render waiting for readbacks that a blocking loop could not overlap
        // with anything.
        let origins: Vec<(u32, u32)> = (0..image.height)
            .step_by(self.tile as usize)
            .flat_map(|oy| {
                (0..image.width)
                    .step_by(self.tile as usize)
                    .map(move |ox| (ox, oy))
            })
            .collect();

        let mut pending: Option<(InFlight, u32, u32)> = None;
        for (i, &(ox, oy)) in origins.iter().enumerate() {
            let flight = self.submit_tile(
                gpu,
                &buffers,
                image.data,
                image.width,
                image.height,
                intent,
                ox,
                oy,
                1,
                // Alternating, and safe: the slot this tile writes into was
                // released two iterations ago, when the tile before last was
                // collected.
                i % 2,
            );
            if let Some((previous, px, py)) = pending.take() {
                let tile = self.collect_tile(gpu, &buffers, previous)?;
                place(
                    &mut result,
                    &tile,
                    w,
                    buffers.padded,
                    self.tile,
                    image,
                    px,
                    py,
                );
            }
            pending = Some((flight, ox, oy));
        }
        if let Some((last, px, py)) = pending {
            let tile = self.collect_tile(gpu, &buffers, last)?;
            place(
                &mut result,
                &tile,
                w,
                buffers.padded,
                self.tile,
                image,
                px,
                py,
            );
        }

        // Geometry last, and outside the tile loop: orientation and crop select
        // and permute, so they compose with a finished frame rather than with
        // each tile — and doing it here is what keeps a cropped export
        // bit-identical to the same region of an uncropped one.
        let (pixels, [width, height]) = crate::geometry::apply(
            &Geometry::new(state, image.recorded_orientation),
            &result,
            [image.width, image.height],
        );
        Ok(Rendered {
            pixels,
            width,
            height,
        })
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
        let padded = self.render_at(gpu, buffers, data, lw, lh, intent, ox, oy, 1 << level)?;

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
    /// One tile, halo included, straight off the GPU.
    ///
    /// Submit and collect in one call, for callers that want the pixels and
    /// have nothing else to do while they wait. [`Renderer::run`] does have
    /// something else to do and uses the two halves separately.
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
        step: u32,
    ) -> Result<Vec<f32>, EngineError> {
        let flight = self.submit_tile(gpu, buffers, mosaic, width, height, intent, ox, oy, step, 0);
        self.collect_tile(gpu, buffers, flight)
    }

    /// Queue one tile's work and ask for its pixels, without waiting for either.
    ///
    /// `slot` picks which staging buffer the result lands in. A caller running
    /// tiles back to back must alternate, because a buffer cannot be the
    /// destination of a copy while it is mapped for reading.
    #[allow(clippy::too_many_arguments)]
    fn submit_tile(
        &self,
        gpu: &Gpu,
        buffers: &TileBuffers,
        mosaic: &[f32],
        width: u32,
        height: u32,
        intent: Output,
        ox: u32,
        oy: u32,
        step: u32,
        slot: usize,
    ) -> InFlight {
        {
            let mut scratch = buffers.scratch.borrow_mut();
            gather_padded(mosaic, width, height, ox, oy, buffers.padded, &mut scratch);
            gpu.queue
                .write_buffer(&buffers.cfa, 0, bytemuck::cast_slice(&scratch));
        }
        // Where this tile is, in the full-resolution image. The guide is
        // indexed by that and not by the tile, so a coarse level reads the same
        // neighbourhood as a fine one covering the same ground.
        let source: [i32; 4] = [
            (ox * step) as i32,
            (oy * step) as i32,
            step as i32,
            HALO as i32,
        ];
        gpu.queue.write_buffer(
            &buffers.params,
            SOURCE_OFFSET,
            bytemuck::cast_slice(&source),
        );

        let staging = &buffers.staging[slot];
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("rcd") });
        self.dispatch_stages(&mut encoder, buffers, intent);
        encoder.copy_buffer_to_buffer(&buffers.out, 0, staging, 0, buffers.out_size);
        let index = gpu.queue.submit([encoder.finish()]);

        let (tx, rx) = std::sync::mpsc::channel();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        InFlight { index, slot, rx }
    }

    /// Wait for one submitted tile and take its pixels.
    ///
    /// Waits for **that submission**, not for the queue. The difference is the
    /// whole point: `wait_indefinitely` blocks until everything submitted has
    /// finished, so a caller that queued the next tile first would wait for
    /// that one too and gain nothing. Naming the submission is what lets the
    /// GPU work on tile *n* while this returns tile *n-1*.
    fn collect_tile(
        &self,
        gpu: &Gpu,
        buffers: &TileBuffers,
        flight: InFlight,
    ) -> Result<Vec<f32>, EngineError> {
        gpu.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(flight.index),
                timeout: None,
            })
            .map_err(|e| EngineError::DeviceRequest(e.to_string()))?;
        flight
            .rx
            .recv()
            .map_err(|_| EngineError::DeviceRequest("readback never completed".into()))?
            .map_err(|e| EngineError::DeviceRequest(e.to_string()))?;

        let staging = &buffers.staging[flight.slot];
        let pixels = {
            let view = staging.slice(..).get_mapped_range();
            bytemuck::cast_slice::<u8, f32>(&view).to_vec()
        };
        staging.unmap();
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
            Output::SceneLinear => &self.pipelines[..SCENE_LINEAR_STAGES],
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
            pass.set_bind_group(1, &buffers.mask_bind_group, &[]);
            pass.dispatch_workgroups(groups, groups, 1);
        }
    }

    /// A canvas to draw tiles into. Sized in screen pixels and rebuilt on
    /// resize; the per-image buffers beside it are untouched by that.
    /// Resample a flat canvas into a straightened one.
    ///
    /// The flat canvas holds tiles as they were scattered — rearranged and
    /// exact. This is where the fraction of a degree happens, and it is a
    /// separate pass because it has to be a *gather*: scattering a rotated tile
    /// leaves holes between the pixels it lands on.
    ///
    /// `straight_origin` and `flat_origin` are the straight- and flat-space
    /// points of each buffer's top-left pixel, in level pixels.
    pub fn straighten(
        &self,
        gpu: &Gpu,
        flat: &Canvas,
        canvas: &Canvas,
        geometry: &Geometry,
        view: StraightenView,
    ) {
        let StraightenView {
            level_image,
            straight_origin,
            flat_origin,
        } = view;
        let [origin, dx, dy] = geometry.flat_transform(level_image);
        let extent = canvas.size();
        let params = StraightenParams {
            straight_origin,
            flat_origin,
            origin,
            dx,
            dy,
            extent,
            photograph: {
                let [pw, ph] = geometry.output_size(level_image);
                [pw as f32, ph as f32]
            },
        };
        let uniform = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("straighten params"),
            size: std::mem::size_of::<StraightenParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue
            .write_buffer(&uniform, 0, bytemuck::bytes_of(&params));

        let source = flat
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default());
        let target = canvas
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default());
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("straighten"),
            layout: &self.straighten_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&source),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&target),
                },
            ],
        });

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("straighten"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("straighten"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.straighten_pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.dispatch_workgroups(extent[0].div_ceil(8), extent[1].div_ceil(8), 1);
        }
        gpu.queue.submit(Some(encoder.finish()));
    }

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
            // blit can sample it, COPY_SRC so a test can check what landed, and
            // RENDER_ATTACHMENT so a cached preview can be drawn into it before
            // any tile has been rendered.
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::RENDER_ATTACHMENT
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
        axes: [[i32; 2]; 2],
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

        let t0 = std::time::Instant::now();
        {
            let mut scratch = buffers.scratch.borrow_mut();
            gather_padded(data, lw, lh, ox, oy, buffers.padded, &mut scratch);
            gpu.queue
                .write_buffer(&buffers.cfa, 0, bytemuck::cast_slice(&scratch));
        }
        let t1 = std::time::Instant::now();
        // Patch only the tail of the uniform. The colour half was resolved once
        // by `set_edit` and does not move because a different tile is being
        // drawn — writes and submits are ordered on the queue, so this lands
        // before the dispatch that reads it.
        // Everything the present pass needs, written as one patch at the tail of
        // the uniform: where the tile goes, and how much of it is real.
        let step = 1i32 << level;
        let tail: [i32; 16] = [
            dest[0],
            dest[1],
            self.tile as i32,
            HALO as i32,
            (lw - ox).min(self.tile) as i32,
            (lh - oy).min(self.tile) as i32,
            0,
            0,
            axes[0][0],
            axes[0][1],
            axes[1][0],
            axes[1][1],
            // `source`: this tile's place in the full-resolution image, which
            // is the coordinate the guide is indexed in.
            ox as i32 * step,
            oy as i32 * step,
            step,
            HALO as i32,
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
            pass.set_bind_group(1, &buffers.mask_bind_group, &[]);
            pass.set_bind_group(2, &canvas.bind_group, &[]);
            let groups = self.tile.div_ceil(8);
            pass.dispatch_workgroups(groups, groups, 1);
        }
        gpu.queue.submit([encoder.finish()]);
        // Kept because it answered a real question — whether a pan was slow
        // because of submission overhead or because of the gather — and the
        // next such question deserves the same treatment. It prints per tile,
        // so it costs a few milliseconds a frame while enabled: read the split
        // it reports, not the frame times it distorts.
        if std::env::var_os("RAWKIT_TIME_TILES").is_some() {
            eprintln!(
                "tile       : gather+upload {:.3} ms, encode+submit {:.3} ms",
                (t1 - t0).as_secs_f64() * 1000.0,
                t1.elapsed().as_secs_f64() * 1000.0
            );
        }
        Ok(())
    }

    /// Resolve the edit against the frame's profile and upload the result.
    ///
    /// Separate from rendering because it depends on the *edit* and not on which
    /// tile is being drawn: a slider move rewrites this and nothing else, while
    /// a pan rewrites nothing at all.
    /// Paint any mask whose shape has moved, and leave the rest alone.
    ///
    /// Rasterising is a few milliseconds a layer, which is nothing when a
    /// gradient is dragged and everything if it happens on every slider. So the
    /// masks last written are kept and compared: a change to a mask's *exposure*
    /// rewrites a uniform and no texels at all.
    fn upload_masks(
        &self,
        gpu: &Gpu,
        buffers: &TileBuffers,
        image: &Frame<'_>,
        live: &[rawkit_editstate::Mask],
    ) {
        let mut uploaded = buffers.uploaded_masks.borrow_mut();
        let [mask_w, mask_h] = buffers.mask_size;
        let mut scratch = buffers.mask_scratch.borrow_mut();
        for (slot, mask) in live.iter().enumerate() {
            // The shape and the inversion decide the texels; everything else
            // about a mask lives in the uniform, so a slider that moves neither
            // repaints nothing. Both, not just the shape: inverting is done to
            // the raster, so leaving it out here would show a vignette as a
            // spotlight until something else happened to move the mask.
            if uploaded
                .get(slot)
                .is_some_and(|old| old.shape == mask.shape && old.invert == mask.invert)
            {
                continue;
            }
            crate::mask::rasterise(mask, image.width, image.height, &mut scratch);
            let half: Vec<u16> = scratch[..(mask_w * mask_h) as usize]
                .iter()
                .map(|v| f32_to_f16(*v))
                .collect();
            gpu.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &buffers.mask_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: 0,
                        z: slot as u32,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::cast_slice(&half),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(mask_w * 2),
                    rows_per_image: Some(mask_h),
                },
                wgpu::Extent3d {
                    width: mask_w,
                    height: mask_h,
                    depth_or_array_layers: 1,
                },
            );
        }
        *uploaded = live.to_vec();
    }

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
        let tone = crate::tone::ToneCurve::new(&state.tone);

        // Only the adjustments that would change something. A mask sitting at
        // its defaults still costs a texture layer and a sample per pixel, and
        // skipping it here is also what makes placing a gradient before touching
        // a slider cost nothing.
        let live: Vec<rawkit_editstate::Mask> = state
            .masks
            .iter()
            .filter(|m| !m.is_identity())
            .take(rawkit_editstate::MAX_MASKS)
            .cloned()
            .collect();
        self.upload_masks(gpu, buffers, image, &live);
        let (wb, m) = (colour.multipliers, colour.cam_to_display);
        let working = colour.working_to_display;
        let hsm = colour.hue_sat.as_ref();
        // Where the look begins in the shared table buffer, and the tone curve
        // after it.
        let hsm_cells = hsm.map(|m| m.cell_count()).unwrap_or(1);
        let look_cells = colour.look.as_ref().map(|m| m.cell_count()).unwrap_or(1);
        let tone_cells = colour.tone.as_ref().map(|lut| lut.len()).unwrap_or(1);
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
                // 0 no look, 1 a look in linear light, 2 a look in sRGB-encoded
                // light. The encoding is not cosmetic: a table with sixteen
                // value divisions indexed the wrong way reads a different slice
                // for almost every pixel.
                match (&colour.look, colour.look_is_srgb) {
                    (None, _) => 0.0,
                    (Some(_), false) => 1.0,
                    (Some(_), true) => 2.0,
                },
            ],
            tone: tone.shape(),
            levels: tone.levels(),
            detail: [
                state.detail.sharpen_amount,
                state.detail.sharpen_radius,
                state.detail.chroma_noise,
                state.detail.luminance_noise,
            ],
            colour: [
                state.colour.saturation,
                state.colour.vibrance,
                // A flag rather than a test in the shader: whether twenty-four
                // numbers are all zero is a question to answer once per edit,
                // not once per pixel.
                if state.hsl.is_identity() { 0.0 } else { 1.0 },
                0.0,
            ],
            grade: {
                let tint = |t: rawkit_editstate::Tint| [t.hue, t.saturation, t.luminance, 0.0];
                [
                    tint(state.grade.shadows),
                    tint(state.grade.midtones),
                    tint(state.grade.highlights),
                ]
            },
            grade_shape: [
                state.grade.blending,
                state.grade.balance,
                if state.grade.is_identity() { 0.0 } else { 1.0 },
                0.0,
            ],
            hsl_hue: pack_bands(&state.hsl, |mix| mix.hue),
            hsl_saturation: pack_bands(&state.hsl, |mix| mix.saturation),
            hsl_luminance: pack_bands(&state.hsl, |mix| mix.luminance),
            guide: [
                buffers.guide_offset as u32,
                buffers.guide_size[0],
                buffers.guide_size[1],
                // Off unless a local control is actually asking for it. The
                // guide costs a develop per pixel, and a photograph whose
                // highlights and shadows are at zero should not pay for a
                // neighbourhood nothing consults — nor differ by one bit from a
                // build that never had this.
                u32::from(tone.active && (tone.highlights != 0.0 || tone.shadows != 0.0)),
            ],
            masks: [live.len() as u32, 0, 0, 0],
            mask_scale: [
                1.0 / image.width.max(1) as f32,
                1.0 / image.height.max(1) as f32,
                0.0,
                0.0,
            ],
            mask_gain: {
                let mut gains = [[1.0f32, 1.0, 1.0, 0.0]; rawkit_editstate::MAX_MASKS];
                for (slot, mask) in live.iter().enumerate() {
                    gains[slot] = mask_gain(image, &colour, mask);
                }
                gains
            },
            guide_scale: [
                buffers.guide_scale[0],
                buffers.guide_scale[1],
                // Whether highlight reconstruction has a colour to borrow. Not
                // the same question as whether the local tone is switched on:
                // reconstruction runs on every frame with a blown pixel in it,
                // whatever the tone controls say.
                if buffers.chroma_known { 1.0 } else { 0.0 },
                0.0,
            ],
            // Per-tile, and rewritten by both render paths before the develop
            // stage reads it. Level zero at the origin, with the halo, is what
            // a single-tile whole-image render would want if nobody wrote it.
            source: [0, 0, 1, HALO as i32],
            // Per-tile, and rewritten before every present. The value here only
            // has to be something valid for the whole-image path, which never
            // rotates in the shader — geometry is applied to the finished frame.
            axes: [1, 0, 0, 1],
            hsm_dims: match hsm {
                Some(m) => [m.hue_divisions, m.sat_divisions, m.value_divisions, 0],
                None => [1, 1, 1, 0],
            },
            display_to_working: {
                // Only reached when a look is active, and a look is only active
                // when the working matrices are real — so an identity here is a
                // matrix nobody consults rather than a silently wrong one.
                let back = crate::profile::invert(&working).unwrap_or(crate::profile::IDENTITY);
                [
                    [back[0][0], back[0][1], back[0][2], 0.0],
                    [back[1][0], back[1][1], back[1][2], 0.0],
                    [back[2][0], back[2][1], back[2][2], 0.0],
                ]
            },
            curve: match &colour.tone {
                Some(lut) => [(hsm_cells + look_cells) as u32, lut.len() as u32, 1, 0],
                None => [0, 1, 0, 0],
            },
            user_curve: {
                let at = (hsm_cells + look_cells + tone_cells) as u32;
                match &colour.user_curve {
                    Some(lut) => [at, lut.len() as u32, 1, 0],
                    None => [at, 1, 0, 0],
                }
            },
            look_dims: match &colour.look {
                Some(m) => [
                    m.hue_divisions,
                    m.sat_divisions,
                    m.value_divisions,
                    hsm_cells as u32,
                ],
                None => [1, 1, 1, hsm_cells as u32],
            },
            present: [0, 0, self.tile as i32, HALO as i32],
            extent: [self.tile as i32, self.tile as i32, 0, 0],
        };
        gpu.queue
            .write_buffer(&buffers.params, 0, bytemuck::bytes_of(&params));

        // Both tables in one buffer, the hue/saturation correction first and the
        // look behind it. Not tidiness: WebGPU guarantees eight storage buffers
        // per stage and this shader already uses all eight, so a second table
        // had to go somewhere that was not a second binding.
        let mut table: Vec<[f32; 4]> = match hsm {
            Some(m) => m.deltas.iter().map(|d| [d[0], d[1], d[2], 0.0]).collect(),
            None => vec![[0.0, 1.0, 1.0, 0.0]],
        };
        match &colour.look {
            Some(m) => table.extend(m.deltas.iter().map(|d| [d[0], d[1], d[2], 0.0])),
            None => table.push([0.0, 1.0, 1.0, 0.0]),
        }
        // And the tone curve behind both, one entry per `vec4`. Wasteful of
        // three lanes and worth it: a packed curve would need its index
        // unpacking in the shader, and this table is four kilobytes.
        match &colour.tone {
            Some(lut) => table.extend(lut.iter().map(|v| [*v, 0.0, 0.0, 0.0])),
            None => table.push([0.0, 0.0, 0.0, 0.0]),
        }
        // The user's curve last, and always the full length: its slot was
        // reserved when the photograph opened, because the edit can grow a curve
        // long after the buffers were sized.
        let mut user = vec![[0.0f32; 4]; crate::profile::TONE_LUT];
        if let Some(lut) = &colour.user_curve {
            for (cell, value) in user.iter_mut().zip(lut) {
                cell[0] = *value;
            }
        }
        table.extend(user);
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
    /// Two readback buffers, used alternately. See `Renderer::submit_tile`.
    staging: [wgpu::Buffer; 2],
    bind_group: wgpu::BindGroup,
    padded: u32,
    out_size: u64,
    table_cells: usize,
    /// Where the guide starts in `cfa`, in floats, and how big it is.
    ///
    /// Fixed when the image opens: the guide is the camera's own RGB, which no
    /// edit changes. A slider move rewrites the uniform and not this.
    guide_offset: usize,
    guide_size: [u32; 2],
    /// Whether the frame had any unclipped light to borrow a colour from.
    chroma_known: bool,
    /// One texture layer per local adjustment, and the group that binds it.
    ///
    /// Allocated for the maximum whatever the edit currently says, because a
    /// render must not allocate and a mask can be added while one is running.
    mask_texture: wgpu::Texture,
    mask_bind_group: wgpu::BindGroup,
    /// The size of one mask layer, and the masks last written into them — so a
    /// slider that does not move a mask does not repaint every layer.
    mask_size: [u32; 2],
    uploaded_masks: std::cell::RefCell<Vec<rawkit_editstate::Mask>>,
    /// CPU staging for one mask layer.
    mask_scratch: std::cell::RefCell<Vec<f32>>,
    /// Guide texels per image pixel. Carried rather than recomputed so the
    /// uniform and the buffer can never describe different mappings.
    guide_scale: [f32; 2],
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

    /// The average colour of a small square, as linear RGB.
    ///
    /// For pointing at the photograph and asking what is there — a targeted
    /// adjustment, and later an eyedropper. A square rather than a single pixel
    /// because one pixel of a demosaiced photograph is partly its neighbours
    /// anyway, and a small average is what makes the answer stable when the hand
    /// moves by one.
    ///
    /// This synchronises, like [`read_back`](Self::read_back) and for the same
    /// reason — but it copies a few hundred bytes rather than the canvas, and it
    /// happens once when a gesture starts rather than once a frame. See the
    /// module note on when a caller reads back: the wait is for the queue, so
    /// this is cheap only because nothing much is in it at that moment.
    ///
    /// `None` when the square falls entirely outside the canvas, which is what a
    /// click on the letterbox around a fitted photograph is.
    pub fn sample(
        &self,
        gpu: &Gpu,
        at: [u32; 2],
        span: u32,
    ) -> Result<Option<[f32; 3]>, EngineError> {
        const BYTES_PER_PIXEL: u32 = 8;
        let [w, h] = self.size;
        let half = span / 2;
        let x0 = at[0].saturating_sub(half).min(w.saturating_sub(1));
        let y0 = at[1].saturating_sub(half).min(h.saturating_sub(1));
        let width = span.min(w - x0).max(1);
        let height = span.min(h - y0).max(1);
        if at[0] >= w || at[1] >= h {
            return Ok(None);
        }

        let padded_row = (width * BYTES_PER_PIXEL).div_ceil(256) * 256;
        let staging = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("canvas sample"),
            size: (padded_row * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("canvas sample"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: x0, y: y0, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
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
            .map_err(|_| EngineError::DeviceRequest("sample never completed".into()))?
            .map_err(|e| EngineError::DeviceRequest(e.to_string()))?;

        let mut total = [0.0f64; 3];
        {
            let view = staging.slice(..).get_mapped_range();
            for row in 0..height as usize {
                let start = row * padded_row as usize;
                let halves: &[u16] =
                    bytemuck::cast_slice(&view[start..start + (width * BYTES_PER_PIXEL) as usize]);
                for pixel in halves.chunks_exact(4) {
                    for (c, total) in total.iter_mut().enumerate() {
                        *total += half_to_f32(pixel[c]) as f64;
                    }
                }
            }
        }
        staging.unmap();
        let n = (width * height) as f64;
        Ok(Some([
            (total[0] / n) as f32,
            (total[1] / n) as f32,
            (total[2] / n) as f32,
        ]))
    }

    /// Pull the canvas back to the CPU as interleaved RGBA.
    ///
    /// For tests and for anything offline. Deliberately *not* part of drawing a
    /// frame: this is the synchronising, stalling operation that
    /// [`Renderer::draw_tile`] exists to keep off the hot path.
    /// Fill the canvas from interleaved RGBA floats.
    ///
    /// The counterpart of [`read_back`](Self::read_back), and the reason it
    /// exists: a test that only reads can compare two GPU paths to each other
    /// but never to an answer worked out on the CPU.
    ///
    /// # Panics
    ///
    /// If `pixels` is not `width * height * 4` long.
    pub fn write(&self, gpu: &Gpu, pixels: &[f32]) {
        let [w, h] = self.size;
        assert_eq!(
            pixels.len(),
            (w * h * 4) as usize,
            "a {w}x{h} canvas wants {} samples",
            w * h * 4
        );
        let halves: Vec<u16> = pixels.iter().map(|v| f32_to_f16(*v)).collect();
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&halves),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 8),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
    }

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
#[cfg(test)]
mod half_tests {
    use super::{f32_to_f16, half_to_f32, pack_bands};

    #[test]
    fn band_centres_match_the_shader() {
        // The band centres exist twice — once as `Band::centre_deg`, once as
        // `band_centre` in WGSL — and the weights are only a partition of the
        // hue circle while the two agree. Drift here would not error: it would
        // send a hue to a slider that is not the one under the pointer.
        let wgsl = include_str!("../shaders/demosaic_rcd.wgsl");
        let body = wgsl
            .split_once("fn band_centre(i: i32) -> f32 {")
            .expect("the shader has no band_centre")
            .1
            // A brace at the start of a line: every `if` inside the body has
            // braces of its own, and splitting on the first of those found one
            // centre and called the other seven missing.
            .split_once("\n}")
            .expect("band_centre is not closed")
            .0;
        let found: Vec<f32> = body
            .split("return ")
            .skip(1)
            .filter_map(|tail| tail.split(';').next()?.trim().parse::<f32>().ok())
            .collect();
        let expected: Vec<f32> = rawkit_editstate::Band::ALL
            .into_iter()
            .map(|b| b.centre_deg())
            .collect();
        assert_eq!(found, expected, "the shader's band centres have drifted");

        // And the shift range, for the same reason.
        let range = wgsl
            .lines()
            .find(|l| l.trim_start().starts_with("const HSL_HUE_RANGE"))
            .and_then(|l| l.rsplit('=').next())
            .and_then(|tail| tail.trim().trim_end_matches(';').parse::<f32>().ok())
            .expect("the shader has no HSL_HUE_RANGE");
        assert_eq!(range, rawkit_editstate::MAX_HUE_SHIFT_DEG);
    }

    #[test]
    fn a_band_is_packed_where_the_shader_looks_for_it() {
        let mut hsl = rawkit_editstate::Hsl::default();
        hsl.blue.saturation = 0.5;
        let packed = pack_bands(&hsl, |mix| mix.saturation);
        // Blue is the sixth band, so index 5: second row, second column.
        assert_eq!(packed[1][1], 0.5);
        assert_eq!(packed[0], [0.0; 4]);
        assert_eq!(packed[1], [0.0, 0.5, 0.0, 0.0]);
    }

    #[test]
    fn every_half_survives_a_round_trip() {
        // Exhaustive: there are only 65536 of them, and the encoder has four
        // branches — subnormal, normal, overflow and NaN — that are otherwise
        // exercised by whichever values a test image happens to contain.
        for bits in 0..=u16::MAX {
            let exponent = (bits >> 10) & 0x1f;
            let mantissa = bits & 0x3ff;
            if exponent == 0x1f && mantissa != 0 {
                continue; // NaN payloads are not preserved and need not be.
            }
            let value = half_to_f32(bits);
            assert_eq!(f32_to_f16(value), bits, "{bits:#06x} became {value}");
        }
    }

    #[test]
    fn rounding_goes_to_nearest_even_like_the_hardware() {
        // A test that uploaded with a different rounding rule would measure the
        // rounding rather than the thing it meant to compare.
        let above = half_to_f32(0x3c00) + (half_to_f32(0x3c01) - half_to_f32(0x3c00)) / 2.0;
        assert_eq!(f32_to_f16(above), 0x3c00, "a tie rounds to the even one");
        assert_eq!(f32_to_f16(1.0e30), 0x7c00, "past the range is infinity");
        assert_eq!(f32_to_f16(-1.0e30), 0xfc00);
        assert_eq!(f32_to_f16(1.0e-12), 0x0000, "too small to survive");
    }
}

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
/// A level of reduction: the mosaic, and what size it came out.
pub type Level = (Vec<f32>, u32, u32);

pub struct Pyramid<'a> {
    base: &'a [f32],
    base_size: (u32, u32),
    /// Owned when [`Pyramid::build`] made them, borrowed when a caller is
    /// keeping them across images — see [`Pyramid::from_levels`].
    reduced: std::borrow::Cow<'a, [Level]>,
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
            reduced: std::borrow::Cow::Owned(reduced),
        }
    }

    /// Give up the reductions, to be kept and pointed at again.
    pub fn into_levels(self) -> Vec<Level> {
        self.reduced.into_owned()
    }

    /// A pyramid over reductions someone else is holding.
    ///
    /// The pair with [`Pyramid::into_levels`], and it exists for a shell that
    /// changes which photograph it is showing. A `Pyramid` borrows its base, so
    /// an owner that holds both the mosaic and a pyramid over it is a
    /// self-referential struct; keeping the mosaic and the *levels* instead, and
    /// making the pyramid a view built per frame, is the same thing without the
    /// reference into itself. Building the view costs nothing — reducing a 24 MP
    /// mosaic does not, which is why the levels are kept rather than rebuilt.
    pub fn from_levels(base: &'a [f32], size: (u32, u32), levels: &'a [Level]) -> Self {
        Self {
            base,
            base_size: size,
            reduced: std::borrow::Cow::Borrowed(levels),
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
    /// The temperature and tint those multipliers stand for.
    ///
    /// Carried because a *local* white balance is a shift relative to whatever
    /// the global one settled on, and for an as-shot frame that is a pair
    /// recovered from the camera's own numbers rather than one anybody typed.
    temperature: f32,
    tint: f32,
    cam_to_display: Matrix3,
    working_to_display: Matrix3,
    hue_sat: Option<crate::profile::HueSatMap>,
    /// The profile's look, applied after the tone curve rather than before it.
    look: Option<crate::profile::HueSatMap>,
    look_is_srgb: bool,
    /// The profile's own tone curve, which *replaces* the built-in tone map.
    tone: Option<Vec<f32>>,
    /// The user's hand-shaped curve, which is applied after everything the
    /// profile does.
    user_curve: Option<Vec<f32>>,
}

/// What one local adjustment multiplies by, at full strength.
///
/// Exposure and white balance combined, because at the point a mask composites
/// both are multiplies and the shader has no reason to know which is which.
///
/// # The white balance part, and what it is exactly
///
/// A local temperature is a *shift* from whatever the global one settled on, so
/// what is wanted is the ratio between the multipliers at the shifted setting
/// and the multipliers at the current one. Those are camera-space numbers and
/// the mask composites two matrices later, so the ratio is carried across:
/// `(M·ratio) / (M·1)` is the gain that ratio produces on a **neutral** in the
/// space the mask lives in.
///
/// Exact for neutrals, which is what a white balance is about, and an
/// approximation for saturated colours — the profile's hue/saturation table sits
/// inside that chain and is not a matrix, so no gain can be exactly right for
/// every colour at once. Stated rather than hidden: the alternative is a second
/// full colour path for local adjustments, which would be a great deal of
/// machinery for a difference nobody could see.
fn mask_gain(image: &Frame<'_>, colour: &Colour, mask: &rawkit_editstate::Mask) -> [f32; 4] {
    let exposure = mask.exposure_ev.exp2();
    if mask.warmth == 0.0 && mask.tint == 0.0 {
        return [exposure, exposure, exposure, 0.0];
    }

    // Mireds, so the same slider means the same shift at 3000 K and at 9000 K.
    // Warmer means a *higher* assumed temperature: telling the renderer the
    // light was bluer is what makes the picture come out oranger.
    let mired =
        1e6 / colour.temperature.max(1.0) - mask.warmth * rawkit_editstate::LOCAL_MIRED_REACH;
    let temperature = (1e6 / mired.max(1e-3)).clamp(
        crate::profile::MIN_TEMPERATURE,
        crate::profile::MAX_TEMPERATURE,
    );
    let tint = colour.tint + mask.tint * rawkit_editstate::LOCAL_TINT_REACH;

    let here = image
        .profile
        .multipliers_for(colour.temperature, colour.tint);
    let there = image.profile.multipliers_for(temperature, tint);
    let ratio = [
        there[0] / here[0].max(1e-6),
        there[1] / here[1].max(1e-6),
        there[2] / here[2].max(1e-6),
    ];

    let chain = crate::profile::multiply(&colour.working_to_display, &colour.cam_to_display);
    let shifted = crate::profile::apply(&chain, ratio);
    let plain = crate::profile::apply(&chain, [1.0, 1.0, 1.0]);
    [
        exposure * shifted[0] / plain[0].max(1e-6),
        exposure * shifted[1] / plain[1].max(1e-6),
        exposure * shifted[2] / plain[2].max(1e-6),
        0.0,
    ]
}

/// Fill `out` with the tile at `(ox, oy)` plus its halo, clamping at the image
/// edge.
///
/// Clamping here has to match what the shader does when it reads out of bounds,
/// or the tiled result would differ from an untiled one along the image border.
/// Both clamp to the nearest edge pixel.
///
/// # Why this is written in three parts
///
/// The obvious version clamps every sample, and measurement said that was 58% of
/// what a tile costs — 0.625 ms of scalar work per tile, which at twenty-seven
/// visible tiles is most of a frame's budget spent deciding that coordinates are
/// in range.
///
/// They almost always are. A tile away from the border has every row fully
/// inside the image, so the row is a contiguous run of the source and copies as
/// one memcpy. Only the tiles at the edges need the clamped path, and only for
/// the few columns that actually hang over.
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
    let (w, h) = (width as i64, height as i64);
    let (padded, first_x) = (padded as i64, ox as i64 - HALO as i64);

    // How the row splits: `left` columns clamped to the first pixel, `run`
    // columns copied straight, the rest clamped to the last.
    let left = (-first_x).clamp(0, padded) as usize;
    let start = first_x.max(0);
    let end = (first_x + padded).min(w);
    let run = (end - start).max(0) as usize;
    let right = padded as usize - left - run;

    for py in 0..padded {
        let gy = (oy as i64 - HALO as i64 + py).clamp(0, h - 1);
        let src = (gy * w) as usize;
        let dst = (py * padded) as usize;
        let row = &mut out[dst..dst + padded as usize];

        row[..left].fill(mosaic[src]);
        if run > 0 {
            let from = src + start as usize;
            row[left..left + run].copy_from_slice(&mosaic[from..from + run]);
        }
        row[left + run..].fill(mosaic[src + (w - 1) as usize]);
        debug_assert_eq!(left + run + right, padded as usize);
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
/// Build a profile from the decoder's camera table.
///
/// Here rather than in a caller because it takes a `RawImage` and produces a
/// `CameraProfile`, which is exactly the seam this crate already owns —
/// [`normalise`] is its twin. Two callers had reason to want it and only one
/// had it.
///
/// The table gives one XYZ-to-camera matrix, which the profile treats as a D65
/// characterisation. That is a real limitation and not a placeholder to be
/// embarrassed about: a single illuminant means a tungsten scene is rendered
/// with a daylight characterisation, which is defensible but not accurate. Two
/// illuminants need a `.dcp`.
///
/// LibRaw pads its matrix to four rows for four-colour sensors; we take the
/// three that describe an RGB camera.
pub fn profile_for(raw: &rawkit_decode::RawImage) -> CameraProfile {
    single_illuminant_profile(&raw.xyz_to_camera)
        .unwrap_or_else(|| CameraProfile::from_color_matrix(crate::profile::IDENTITY))
}

/// The same table, with "this body has no matrix at all" still distinguishable.
///
/// [`profile_for`] folds that case into an identity profile, which is the right
/// default and the wrong thing for a caller that wants to *say* the colour will
/// be badly cast. Both exist because both answers are wanted.
pub fn single_illuminant_profile(cam_xyz: &[[f32; 3]; 4]) -> Option<CameraProfile> {
    if cam_xyz.iter().flatten().all(|&v| v == 0.0) {
        return None;
    }
    Some(CameraProfile::from_color_matrix([
        cam_xyz[0], cam_xyz[1], cam_xyz[2],
    ]))
}

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

/// The other direction, for [`Canvas::write`].
///
/// Round-to-nearest-even, matching what the hardware does on a store — a test
/// that uploaded with a different rounding rule would measure the rounding
/// rather than the thing it meant to compare. Values past the half-float range
/// saturate to infinity, which is what a store does too.
fn f32_to_f16(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x7f_ffff;

    if exponent == 0xff {
        // Infinity, or a NaN kept as one rather than turned into infinity.
        let payload = if mantissa != 0 { 0x200 } else { 0 };
        return sign | 0x7c00 | payload;
    }
    let unbiased = exponent - 127 + 15;
    if unbiased >= 0x1f {
        return sign | 0x7c00;
    }
    if unbiased <= 0 {
        // Subnormal, or too small to survive at all.
        if unbiased < -10 {
            return sign;
        }
        let with_leading = mantissa | 0x80_0000;
        let shift = (14 - unbiased) as u32;
        let value = with_leading >> shift;
        let round = (with_leading >> (shift - 1)) & 1;
        let sticky = u32::from(with_leading & ((1 << (shift - 1)) - 1) != 0);
        let up = round & (sticky | (value & 1));
        return sign | (value + up) as u16;
    }
    let value = ((unbiased as u32) << 10) | (mantissa >> 13);
    let round = (mantissa >> 12) & 1;
    let sticky = u32::from(mantissa & 0xfff != 0);
    let up = round & (sticky | (value & 1));
    sign | (value + up) as u16
}
