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
        recorded_orientation: rawkit_editstate::Orientation::AsShot,
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

/// A flat field with sensor noise of a chosen depth scattered over every site.
///
/// **Every site, not only red and blue**, and that is not incidental. A sensor
/// puts noise on all of its photosites, and `Guide::noise` measures it where it
/// can be seen without an assumption — between a quad's two greens, which saw
/// the same light. A fixture that scattered only red and blue left the greens
/// identical, the frame measured as clean however deep the colour noise went,
/// and the test below could not tell its two cases apart.
fn sensor_noise(depth: f32) -> Vec<f32> {
    (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| {
            let hash = (x * 1973 + y * 9277 + (x * y) % 7919) % 101;
            0.25 + (hash as f32 / 100.0 - 0.5) * depth
        })
        .collect()
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_noisy_frame_gets_more_reduction_than_a_clean_one_at_the_same_setting() {
    // The defect this is the fix for: the amount was a fixed number that knew
    // nothing about the photograph it was cleaning, so one slider position had
    // to serve a base-ISO frame with fine colour detail in it and a pushed one
    // full of blotches. Measured on the two ends of that, at full strength the
    // ISO 1000 reference frame loses 58% of its high-frequency chroma — the
    // noise — and the ISO 200 one loses 37%, which is its windows.
    //
    // The frame itself says which end it is on. `Guide::noise` measures it, and
    // this is the claim that the measurement reaches the renderer: two frames,
    // identical but for how much colour noise is on them, at the *same* slider
    // position, must not be cleaned by the same amount.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let mut state = EditState::default();
    state.detail.sharpen_amount = 0.0;
    state.detail.chroma_noise = 0.5;

    let luma = |p: &[f32]| (p[0] + p[1] + p[2]) / 3.0;
    // How much colour is left, as a mean distance from neutral. Cleaning colour
    // noise off a field that has no real colour in it can only reduce this.
    let left = |p: &[f32]| {
        let mut total = 0.0f64;
        let mut n = 0u32;
        for i in (0..p.len()).step_by(4) {
            let px = &p[i..i + 3];
            let m = luma(px);
            total += px.iter().map(|c| (c - m).abs() as f64).sum::<f64>();
            n += 1;
        }
        total / n as f64
    };

    let mut kept = Vec::new();
    // Chosen to span the range the reference photographs actually occupy
    // rather than to be far apart: `Guide::noise` reads 0.0024 to 0.0031 at ISO
    // 100-200 and 0.0046 to 0.0054 at ISO 500-1000, and the scaling is clamped
    // either side of that. Two depths outside the clamp would pass this test
    // while proving the renderer responds only to values no photograph has.
    for depth in [0.004f32, 0.03] {
        let cfa = sensor_noise(depth);
        let plain = render(
            &gpu,
            &cfa,
            &{
                let mut s = state.clone();
                s.detail.chroma_noise = 0.0;
                s
            },
            Output::SceneLinear,
        );
        let cleaned = render(&gpu, &cfa, &state, Output::SceneLinear);
        let fraction = left(&cleaned) / left(&plain);
        println!(
            "depth {depth}: {:.1}% of the colour noise survives",
            fraction * 100.0
        );
        kept.push(fraction);
    }

    assert!(
        kept[1] < kept[0] * 0.85,
        "the same setting cleaned both frames the same: {:.3} of the noise left \
         on the quiet one against {:.3} on the noisy one. The frame's own noise \
         is not reaching the renderer.",
        kept[0],
        kept[1]
    );
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
