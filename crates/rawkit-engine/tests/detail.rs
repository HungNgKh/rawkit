//! Does the detail stage do what it says, and only that?
//!
//! Two claims, and both are the kind that a photograph cannot check by eye.
//!
//! 1. **Sharpening a flat field changes nothing.** An unsharp mask subtracts a
//!    blur, and the blur of a constant is that constant — so anything that comes
//!    out of a featureless area is the filter mis-normalising, which on a real
//!    frame reads as noise it invented.
//! 2. **Chroma noise reduction leaves brightness alone**, in the camera's own
//!    space, where it runs. That is the entire reason it can be on by default:
//!    it borrows colour from the neighbourhood and puts each pixel's own
//!    brightness back. If brightness moved, this would be a blur with a
//!    reassuring name.
//!
//!    Measured *there* rather than at the end of the pipeline, because the
//!    colour matrix mixes channels: changing a pixel's colour necessarily moves
//!    its display luminance a little, and that is the profile's arithmetic
//!    rather than this stage's. On a real frame the effect is about 1% — a
//!    shadow's luminance scatter went 21.78 to 21.54 while its colour scatter
//!    halved.
//!
//! GPU-gated like the rest: `cargo test -- --ignored`.

use rawkit_editstate::EditState;
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const W: u32 = 128;
const H: u32 = 96;

fn colour_at(x: u32, y: u32) -> usize {
    if (x + y) % 2 == 1 {
        1
    } else if y % 2 == 0 {
        0
    } else {
        2
    }
}

fn frame(cfa: &[f32]) -> Frame<'_> {
    Frame {
        data: cfa,
        width: W,
        height: H,
        phase: BayerPhase::Rggb,
        as_shot_wb: [1.0, 1.0, 1.0],
        clip_level: f32::INFINITY,
        profile: CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY),
    }
}

fn render(gpu: &Gpu, cfa: &[f32], state: &EditState, intent: Output) -> Vec<f32> {
    Renderer::new(gpu)
        .run(gpu, &frame(cfa), state, intent)
        .expect("render")
        .pixels
}

#[test]
#[ignore = "requires a GPU adapter"]
fn sharpening_a_flat_field_changes_nothing() {
    // The blur of a constant is that constant, so the correction is zero
    // everywhere — including at the edges, where the taps are clamped and a
    // filter normalised by a fixed constant instead of by the weight it used
    // would darken a one-pixel border.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let cfa: Vec<f32> = vec![0.35; (W * H) as usize];

    let mut off = EditState::default();
    off.detail.sharpen_amount = 0.0;
    off.detail.chroma_noise = 0.0;
    let mut on = off.clone();
    on.detail.sharpen_amount = 1.0;

    let plain = render(&gpu, &cfa, &off, Output::Display);
    let sharpened = render(&gpu, &cfa, &on, Output::Display);

    let mut worst = 0.0f32;
    for (a, b) in plain.iter().zip(&sharpened) {
        worst = worst.max((a - b).abs());
    }
    println!("worst change on a flat field: {worst:e}");
    assert!(worst < 1e-5, "sharpening invented {worst:e} of detail");
}

#[test]
#[ignore = "requires a GPU adapter"]
fn chroma_noise_reduction_keeps_every_pixel_its_own_brightness() {
    // Colour is borrowed from the neighbourhood; brightness is not. This is the
    // claim that lets it be on by default, and the one that separates it from a
    // blur — so it is checked per pixel rather than as an average, where a
    // brightening half and a darkening half would cancel and pass.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    // Structure in luminance and *noise* in colour, which is the situation this
    // exists for. Deterministic: a hash of the position, not a random source.
    let cfa: Vec<f32> = (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| {
            let shade = 0.25 + 0.2 * ((x as f32 / 9.0).sin() * (y as f32 / 7.0).cos());
            let scatter = (((x * 1973 + y * 9277) % 101) as f32 / 100.0 - 0.5) * 0.12;
            // Only the red and blue sites get the scatter, so it is colour noise
            // rather than luminance noise.
            match colour_at(x, y) {
                1 => shade,
                _ => shade + scatter,
            }
        })
        .collect();

    let mut off = EditState::default();
    off.detail.sharpen_amount = 0.0;
    off.detail.chroma_noise = 0.0;
    let mut on = off.clone();
    on.detail.chroma_noise = 1.0;

    // Scene-linear: the stage runs before the profile, so this is the space its
    // claim is about. Asking at the end of the pipeline would be measuring the
    // colour matrix.
    let plain = render(&gpu, &cfa, &off, Output::SceneLinear);
    let cleaned = render(&gpu, &cfa, &on, Output::SceneLinear);

    let luma = |p: &[f32]| (p[0] + p[1] + p[2]) / 3.0;
    let mut worst_luma = 0.0f32;
    let mut worst_colour = 0.0f32;
    for i in (0..plain.len()).step_by(4) {
        let (a, b) = (&plain[i..i + 3], &cleaned[i..i + 3]);
        worst_luma = worst_luma.max((luma(a) - luma(b)).abs());
        // How far the colour moved, measured against its own brightness so a
        // bright pixel is not allowed a bigger shift than a dark one.
        for c in 0..3 {
            worst_colour = worst_colour.max((a[c] - luma(a)) - (b[c] - luma(b)));
        }
    }
    println!("worst brightness change: {worst_luma:e}, worst colour change: {worst_colour:e}");
    assert!(
        worst_luma < 1e-5,
        "brightness moved by {worst_luma:e}; this is a blur, not chroma noise reduction"
    );
    assert!(
        worst_colour > 1e-3,
        "colour barely moved ({worst_colour:e}), so nothing was actually smoothed"
    );
}
