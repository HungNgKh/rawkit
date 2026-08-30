//! Does colour grading divide the picture without adding or losing anything?
//!
//! The claim that makes the control predictable is that the three weights
//! **partition** the luminance range: every pixel's weights sum to one, so a
//! tint applied to all three ranges is a uniform tint and nothing is coloured
//! twice. Overlapping curves chosen by feel are the other way to do it, and they
//! are why a grading control can brighten a picture when you only meant to tint
//! it.
//!
//! Checked from outside rather than by reading the weights: grade all three
//! ranges the same and the result must not depend on how bright the pixel was.
//!
//! GPU-gated like the rest: `cargo test -- --ignored`.

use rawkit_editstate::{EditState, Grade, Tint};
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const W: u32 = 96;
const H: u32 = 64;

/// A frame that steps from near-black to near-white across its width, so one
/// render carries every range at once.
///
/// **Doubling rather than adding**: ten stops across the width, because the
/// ranges are perceptual and a ramp that is even in *raw* values is not even in
/// brightness. A linear one spends nine tenths of its width in the upper half of
/// the scale and has no genuinely dark end to test the shadows with — which is
/// exactly the fixture this test started with, and it hid a real defect by
/// finding no shadows anywhere.
fn ramp() -> Vec<f32> {
    (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, _)| 0.0008 * 2f32.powf(10.0 * x as f32 / W as f32))
        .collect()
}

fn render(gpu: &Gpu, state: &EditState) -> Vec<f32> {
    let cfa = ramp();
    Renderer::new(gpu)
        .run(
            gpu,
            &Frame {
                data: &cfa,
                width: W,
                height: H,
                phase: BayerPhase::Rggb,
                as_shot_wb: [1.0, 1.0, 1.0],
                clip_level: f32::INFINITY,
                profile: CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY),
                recorded_orientation: rawkit_editstate::Orientation::AsShot,
            },
            state,
            Output::Display,
        )
        .expect("render")
        .pixels
}

/// Hue angle and distance from grey, at one column.
fn colour_at(pixels: &[f32], x: u32) -> (f32, f32) {
    let mut hue = 0.0;
    let mut chroma = 0.0;
    let mut n = 0.0;
    for y in 12..H - 12 {
        let i = ((y * W + x) * 4) as usize;
        let (r, g, b) = (pixels[i], pixels[i + 1], pixels[i + 2]);
        let grey = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        chroma +=
            ((r - grey).powi(2) + (g - grey).powi(2) + (b - grey).powi(2)).sqrt() / grey.max(1e-4);
        let top = r.max(g).max(b);
        let bottom = r.min(g).min(b);
        let span = top - bottom;
        if span > 0.0 {
            let h = if top == r {
                let v = (g - b) / span;
                if v < 0.0 {
                    v + 6.0
                } else {
                    v
                }
            } else if top == g {
                2.0 + (b - r) / span
            } else {
                4.0 + (r - g) / span
            };
            hue += h * 60.0;
        }
        n += 1.0;
    }
    (hue / n, chroma / n)
}

fn tinted(hue: f32, saturation: f32) -> Tint {
    Tint {
        hue,
        saturation,
        luminance: 0.0,
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn one_colour_in_all_three_ranges_is_a_uniform_tint() {
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let all = EditState {
        grade: Grade {
            shadows: tinted(210.0, 0.6),
            midtones: tinted(210.0, 0.6),
            highlights: tinted(210.0, 0.6),
            ..Grade::default()
        },
        ..EditState::default()
    };
    let pixels = render(&gpu, &all);

    // Across the ramp, from a dark column to a bright one. If the weights summed
    // to more than one anywhere, that luminance would be tinted harder; to less,
    // softer.
    let samples: Vec<(f32, f32)> = [12, 30, 48, 66, 84]
        .iter()
        .map(|x| colour_at(&pixels, *x))
        .collect();
    let chroma: Vec<f32> = samples.iter().map(|s| s.1).collect();
    let lowest = chroma.iter().cloned().fold(f32::MAX, f32::min);
    let highest = chroma.iter().cloned().fold(0.0f32, f32::max);
    println!("tint strength across the ramp: {chroma:?}");
    assert!(
        highest - lowest < 0.06,
        "the same tint reached differently at different brightnesses ({lowest:.3} to \
         {highest:.3}), so the ranges do not partition the picture"
    );
    assert!(lowest > 0.05, "nothing was tinted at all: {chroma:?}");
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_shadow_tint_reaches_the_shadows_and_not_the_highlights() {
    // The control's whole purpose: colour one end without touching the other.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let shadows_only = EditState {
        grade: Grade {
            shadows: tinted(30.0, 0.8),
            // Distinct ranges, so the test measures the split rather than the
            // softest possible version of it.
            blending: 0.0,
            ..Grade::default()
        },
        ..EditState::default()
    };
    let plain = render(&gpu, &EditState::default());
    let graded = render(&gpu, &shadows_only);

    let dark = colour_at(&graded, 10).1 - colour_at(&plain, 10).1;
    let bright = colour_at(&graded, W - 10).1 - colour_at(&plain, W - 10).1;
    println!("shadow tint added: {dark:.3} dark, {bright:.3} bright");
    assert!(dark > 0.1, "the shadows were barely tinted: {dark:.3}");
    assert!(
        bright < dark * 0.35,
        "the highlights took {bright:.3} against the shadows' {dark:.3}, so the range \
         does not select"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn balance_moves_which_pixels_count_as_shadow() {
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let with = |balance: f32| {
        let state = EditState {
            grade: Grade {
                shadows: tinted(30.0, 0.8),
                blending: 0.0,
                balance,
                ..Grade::default()
            },
            ..EditState::default()
        };
        colour_at(&render(&gpu, &state), W / 2).1
    };
    // Pushing the midpoint up makes more of the picture count as shadow, so the
    // middle of the ramp takes more of the shadow tint.
    let (low, high) = (with(-0.8), with(0.8));
    println!("middle of the ramp: {low:.3} at balance -0.8, {high:.3} at +0.8");
    assert!(
        high > low + 0.05,
        "balance did not move the boundary: {low:.3} against {high:.3}"
    );
}
