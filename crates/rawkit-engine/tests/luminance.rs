//! Does luminance noise reduction spare what it is supposed to spare?
//!
//! Two claims, and the second is the reason the filter is shaped the way it is.
//!
//! 1. **A flat area smooths and an edge does not.** Any filter can lower the
//!    noise in a photograph; a blur does it perfectly and takes the picture with
//!    it. What makes this worth having is that it is edge-aware, so the test has
//!    to measure both halves of that bargain at once — the scatter in a flat
//!    field *and* the step across a boundary, from the same render.
//!
//! 2. **One setting means one thing across the whole frame.** Sensor noise is
//!    dominated by photon shot noise, whose standard deviation grows as the
//!    square root of the signal. A threshold applied to linear light is
//!    therefore generous in the shadows, where it swallows real detail, and mean
//!    in the highlights, where the noise it was meant to catch sails past it.
//!
//!    The filter compares neighbours on the **square root** of the signal, which
//!    is the variance-stabilising transform for that noise. This test is what
//!    holds that to account: two patches four and a half stops apart, each
//!    carrying shot-like noise, must be smoothed by comparable *fractions*. It
//!    is a test that could not be written at all if the comparison happened in
//!    linear light.
//!
//! Both measure in `SceneLinear`, where the stage runs — the same lesson the
//! chroma test learned. Measuring at the end of the pipeline would be measuring
//! the tone map's opinion of the noise rather than the noise.
//!
//! GPU-gated like the rest: `cargo test -- --ignored`.

use rawkit_editstate::EditState;
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const W: u32 = 192;
const H: u32 = 128;
/// An even column, so the CFA phase of the two halves matches and the edge is an
/// edge in brightness rather than in pattern.
const EDGE: u32 = 96;
/// Strong, for the edge test: a filter that keeps an edge at its hardest
/// setting keeps it everywhere.
const STRONG: f32 = 0.6;
/// Modest, for the shadows-and-highlights test, and the choice is the finding.
///
/// The two designs — comparing on the signal or on its square root — are
/// **indistinguishable at high strength**, because a wide enough tolerance
/// averages everything regardless of how bright it is. Measured across the same
/// two patches, the gap between what the shadows and the highlights kept was:
///
/// | strength | on the square root | on the signal |
/// |---|---|---|
/// | 0.2 | 1 point | **28 points** |
/// | 0.35 | 1 | 10 |
/// | 0.5 | 1 | 3 |
/// | 0.8 | 2 | 0 |
///
/// So the transform earns itself precisely where somebody would work — enough
/// filtering to help, not so much that detail goes with it. Testing this at
/// full strength would have passed either way and proved nothing, which is what
/// the first version of this test did.
const MODEST: f32 = 0.25;

/// Deterministic noise in [-1, 1]. A hash rather than a generator so that every
/// run, on every machine, measures the same frame — a denoising test whose input
/// moved would report its own seed.
fn noise(x: u32, y: u32) -> f32 {
    let mut h = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2545_F491);
    h ^= h >> 13;
    (h as f32 / u32::MAX as f32) * 2.0 - 1.0
}

fn render(gpu: &Gpu, cfa: &[f32], strength: f32) -> Vec<f32> {
    let mut state = EditState::default();
    state.detail.luminance_noise = strength;
    // Sharpening off. It runs after this and would put back some of what was
    // taken, so a test that left it on would be measuring the two together.
    state.detail.sharpen_amount = 0.0;
    Renderer::new(gpu)
        .run(
            gpu,
            &Frame {
                data: cfa,
                width: W,
                height: H,
                phase: BayerPhase::Rggb,
                as_shot_wb: [1.0, 1.0, 1.0],
                clip_level: f32::INFINITY,
                profile: CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY),
                recorded_orientation: rawkit_editstate::Orientation::AsShot,
            },
            &state,
            Output::SceneLinear,
        )
        .expect("render")
        .pixels
}

/// Mean and standard deviation of the green channel over a window, well inside
/// the frame: RCD clamps at the image border and those pixels are wrong by
/// construction.
fn scatter(pixels: &[f32], x0: u32, x1: u32) -> (f32, f32) {
    let mut values = Vec::new();
    for y in 16..H - 16 {
        for x in x0..x1 {
            values.push(pixels[((y * W + x) * 4 + 1) as usize]);
        }
    }
    let mean = values.iter().sum::<f32>() / values.len() as f32;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / values.len() as f32;
    (mean, variance.sqrt())
}

/// How hard the boundary is: the mean absolute difference between neighbouring
/// columns, taken across the edge only. A blur flattens this; the whole claim is
/// that this filter does not.
fn edge_strength(pixels: &[f32]) -> f32 {
    let mut total = 0.0;
    let mut count = 0.0;
    for y in 16..H - 16 {
        let left = pixels[((y * W + EDGE - 1) * 4 + 1) as usize];
        let right = pixels[((y * W + EDGE) * 4 + 1) as usize];
        total += (right - left).abs();
        count += 1.0;
    }
    total / count
}

#[test]
#[ignore = "requires a GPU adapter"]
fn noise_falls_and_the_edge_survives() {
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    // A dark half and a bright half with a hard boundary, both carrying the
    // same noise.
    let cfa: Vec<f32> = (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| {
            let level = if x < EDGE { 0.18 } else { 0.42 };
            level + 0.012 * noise(x, y)
        })
        .collect();

    let plain = render(&gpu, &cfa, 0.0);
    let denoised = render(&gpu, &cfa, STRONG);

    let (_, before) = scatter(&plain, 16, EDGE - 8);
    let (_, after) = scatter(&denoised, 16, EDGE - 8);
    let (edge_before, edge_after) = (edge_strength(&plain), edge_strength(&denoised));
    println!(
        "flat scatter {before:.5} -> {after:.5} ({:.0}% of it left)   \
         edge {edge_before:.4} -> {edge_after:.4} ({:.0}% of it left)",
        100.0 * after / before,
        100.0 * edge_after / edge_before
    );

    assert!(
        after < before * 0.7,
        "the flat area kept {:.0}% of its scatter, so this is barely filtering",
        100.0 * after / before
    );
    assert!(
        edge_after > edge_before * 0.9,
        "the edge lost {:.0}% of its step, which is a blur rather than a denoiser",
        100.0 * (1.0 - edge_after / edge_before)
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_same_setting_reaches_the_shadows_and_the_highlights() {
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    // Two patches four and a half stops apart, each with noise scaled as shot
    // noise is: proportional to the square root of the signal. In linear units
    // the dark half's noise is a fifth of the bright half's; as a fraction of
    // its own signal it is four times larger.
    const DARK: f32 = 0.02;
    const BRIGHT: f32 = 0.45;
    let cfa: Vec<f32> = (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| {
            let level = if x < EDGE { DARK } else { BRIGHT };
            level + 0.05 * level.sqrt() * noise(x, y)
        })
        .collect();

    let plain = render(&gpu, &cfa, 0.0);
    let denoised = render(&gpu, &cfa, MODEST);

    // Relative scatter, because the two patches have wildly different means and
    // an absolute comparison would only rediscover that.
    let relative = |pixels: &[f32], x0, x1| {
        let (mean, deviation) = scatter(pixels, x0, x1);
        deviation / mean
    };
    let dark = relative(&denoised, 16, EDGE - 8) / relative(&plain, 16, EDGE - 8);
    let bright = relative(&denoised, EDGE + 8, W - 16) / relative(&plain, EDGE + 8, W - 16);
    println!(
        "shadows kept {:.0}% of their relative noise, highlights {:.0}%",
        100.0 * dark,
        100.0 * bright
    );

    assert!(
        dark < 0.75 && bright < 0.75,
        "one of the halves was barely touched: shadows {dark:.2}, highlights {bright:.2}"
    );
    // Eight points. The square root holds this to one or two at every strength
    // tried; comparing on the signal instead opens it to twenty-eight here.
    assert!(
        (dark - bright).abs() < 0.08,
        "the same setting removed {:.0}% of the noise in the shadows and {:.0}% in the \
         highlights, so it does not mean one thing across the frame",
        100.0 * (1.0 - dark),
        100.0 * (1.0 - bright)
    );
}
