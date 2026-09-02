//! Clarity, texture and haze: the three controls that ask about a pixel's
//! surroundings rather than about the pixel.
//!
//! They differ by the size of the question — the guide's ~190 pixels, a few
//! pixels, and the whole frame — which is what these check. A control that
//! answered at the wrong scale would still move the picture, so "something
//! changed" is never the assertion here.
//!
//! `cargo test -p rawkit-engine --test local_contrast -- --ignored`

use rawkit_editstate::{EditState, Tone};
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const W: u32 = 512;
const H: u32 = 512;

fn render(gpu: &Gpu, cfa: &[f32], state: &EditState) -> Vec<f32> {
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
            state,
            Output::Display,
        )
        .expect("render")
        .pixels
}

fn luma(pixels: &[f32], x: u32, y: u32) -> f32 {
    let i = ((y * W + x) * 4) as usize;
    0.2126 * pixels[i] + 0.7152 * pixels[i + 1] + 0.0722 * pixels[i + 2]
}

fn tone(f: impl Fn(&mut Tone)) -> EditState {
    let mut tone = Tone::default();
    f(&mut tone);
    EditState {
        tone,
        ..EditState::default()
    }
}

/// Flat, so anything that varies came from a control.
fn flat() -> Vec<f32> {
    vec![0.3f32; (W * H) as usize]
}

/// A broad soft blob on the left half, and a fine ripple on the right.
///
/// Two scales in one frame, which is what lets clarity and texture be told apart
/// rather than merely observed to do something.
fn two_scales() -> Vec<f32> {
    let mut m = vec![0.3f32; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            let (fx, fy) = (x as f32, y as f32);
            let blob = (-(((fx - 128.0).powi(2) + (fy - 256.0).powi(2)) / 8000.0)).exp();
            let ripple = if x > W / 2 {
                0.05 * ((fx * 0.9).sin() * (fy * 0.9).cos())
            } else {
                0.0
            };
            m[(y * W + x) as usize] = (0.3 + 0.15 * blob + ripple).clamp(0.01, 1.0);
        }
    }
    m
}

/// The same shapes, but with a colour the dark-channel prior can work on.
///
/// The prior's whole assumption is that a clear patch has a channel that goes
/// nearly black. A grey frame has none — every channel sits at the same value —
/// so it reads as hazy whatever is done to it, and a dehaze measured on one
/// measures the frame's violation of the assumption rather than the code. Red is
/// held low here so there is a real dark channel to see the veil against.
fn colourful() -> Vec<f32> {
    let mut m = two_scales();
    for y in 0..H {
        for x in 0..W {
            let i = (y * W + x) as usize;
            // Red sites of an RGGB mosaic.
            if x % 2 == 0 && y % 2 == 0 {
                m[i] *= 0.12;
            }
        }
    }
    m
}

/// How much a small window varies, as a standard deviation of luminance.
fn variation(pixels: &[f32], cx: u32, cy: u32, half: u32) -> f32 {
    let mut values = Vec::new();
    for y in cy - half..=cy + half {
        for x in cx - half..=cx + half {
            values.push(luma(pixels, x, y));
        }
    }
    let mean = values.iter().sum::<f32>() / values.len() as f32;
    (values.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / values.len() as f32).sqrt()
}

#[test]
#[ignore = "requires a GPU adapter"]
fn clarity_works_at_the_neighbourhoods_scale_and_not_at_the_pixels() {
    // The definition. Clarity is contrast against what surrounds a pixel, so on
    // a frame with nothing around it to differ from there is nothing to do —
    // and on one with a broad shape in it, the shape gains contrast.
    let Some(gpu) = gpu() else { return };
    let flat_off = render(&gpu, &flat(), &EditState::default());
    let flat_on = render(&gpu, &flat(), &tone(|t| t.clarity = 1.0));
    let (was, now) = (luma(&flat_off, W / 2, H / 2), luma(&flat_on, W / 2, H / 2));
    assert!(
        (now - was).abs() < 1e-3,
        "clarity moved a flat frame from {was} to {now}, so its pivot is in the \
         wrong coordinate"
    );

    let off = render(&gpu, &two_scales(), &EditState::default());
    let on = render(&gpu, &two_scales(), &tone(|t| t.clarity = 1.0));
    // The blob's flank, where the picture differs most from its own
    // neighbourhood at the guide's scale.
    let (before, after) = (variation(&off, 128, 180, 24), variation(&on, 128, 180, 24));
    assert!(
        after > before * 1.05,
        "the broad shape gained no contrast: {before:.5} to {after:.5}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn texture_finds_the_fine_detail_and_leaves_the_broad_shape() {
    // The reason there are two controls and not one at a compromise radius, and
    // the separation runs the other way from the tempting guess. Clarity does
    // reach fine detail — it is a power about a smooth pivot, so everything
    // above that pivot's scale gains — and what makes it a *different* control
    // is that it also moves the broad shape, which texture must not.
    //
    // The left half of this frame carries a soft blob and no ripple; the right
    // half carries a ripple two pixels wide.
    let Some(gpu) = gpu() else { return };
    let frame = two_scales();
    let off = render(&gpu, &frame, &EditState::default());
    let textured = render(&gpu, &frame, &tone(|t| t.texture = 1.0));
    let clarified = render(&gpu, &frame, &tone(|t| t.clarity = 1.0));

    let plain = variation(&off, 384, 256, 6);
    let with_texture = variation(&textured, 384, 256, 6);
    assert!(
        with_texture > plain * 1.15,
        "texture did not lift the fine detail: {plain:.5} to {with_texture:.5}"
    );

    // The broad shape, where the two controls have to part company.
    let broad = variation(&off, 128, 180, 24);
    let broad_texture = variation(&textured, 128, 180, 24);
    let broad_clarity = variation(&clarified, 128, 180, 24);
    assert!(
        (broad_texture - broad).abs() < broad * 0.1,
        "texture moved a shape far wider than its radius: {broad:.5} to {broad_texture:.5}"
    );
    assert!(
        broad_clarity > broad * 1.05,
        "clarity left the broad shape alone: {broad:.5} to {broad_clarity:.5}"
    );
    // And the other way round: negative texture takes it away, which is what
    // makes the control a smoother rather than a blur.
    let smoothed = render(&gpu, &frame, &tone(|t| t.texture = -1.0));
    assert!(
        variation(&smoothed, 384, 256, 6) < plain * 0.9,
        "negative texture left the detail alone"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn dehaze_takes_back_the_veil_it_was_given() {
    // Haze, synthesised the way the model says it arrives: `I = J*t + A*(1 - t)`
    // in the light, before anything renders it. If the control is doing what it
    // claims, putting that in and taking it out again has to land near where the
    // frame started.
    let Some(gpu) = gpu() else { return };
    let clean = colourful();
    let transmission = 0.55f32;
    let airlight = 0.9f32;
    let hazy: Vec<f32> = clean
        .iter()
        .map(|v| v * transmission + airlight * (1.0 - transmission))
        .collect();

    let reference = render(&gpu, &clean, &EditState::default());
    let veiled = render(&gpu, &hazy, &EditState::default());
    let cleared = render(&gpu, &hazy, &tone(|t| t.dehaze = 1.0));

    // Contrast is what haze takes away, so contrast is what to measure. A mean
    // would be recovered by an exposure slider and would prove nothing.
    let (there, hazed, fixed) = (
        variation(&reference, 128, 180, 24),
        variation(&veiled, 128, 180, 24),
        variation(&cleared, 128, 180, 24),
    );
    assert!(
        hazed < there * 0.75,
        "the synthetic haze only took {there:.5} down to {hazed:.5}, so this proves nothing"
    );
    println!("haze: {there:.5} clean, {hazed:.5} veiled, {fixed:.5} recovered");
    assert!(
        fixed > hazed * 4.0,
        "dehaze recovered {hazed:.5} to only {fixed:.5}, against {there:.5} unhazed"
    );
    // And it does not invent contrast the photograph never had. Full strength
    // means "take the veil off", not "take it off and keep going" — a control
    // whose top end overshoots is one nobody can put at the top end.
    assert!(
        fixed < there * 1.1,
        "dehaze overshot to {fixed:.5} against {there:.5} unhazed"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn none_of_the_three_at_zero_changes_a_bit() {
    // The claim that lets these be added to a build without moving any stored
    // edit: at zero they are not nearly off, they are off. It also catches a
    // uniform written into the wrong slot, which nothing else here would.
    let Some(gpu) = gpu() else { return };
    let frame = two_scales();
    let plain = render(&gpu, &frame, &EditState::default());
    let zeroed = render(
        &gpu,
        &frame,
        &tone(|t| {
            t.clarity = 0.0;
            t.texture = 0.0;
            t.dehaze = 0.0;
        }),
    );
    assert_eq!(plain, zeroed);
}

fn gpu() -> Option<Gpu> {
    Gpu::new().ok()
}
