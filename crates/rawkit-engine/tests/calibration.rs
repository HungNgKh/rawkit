//! Does moving a primary leave the photograph's neutral alone?
//!
//! The unit tests beside `calibrate.rs` answer that about the *matrix*. This
//! answers it about pixels, through the whole develop kernel — which is where a
//! neutral would actually be seen to drift, and where a shadow tint that leaked
//! into the highlights would show.
//!
//! GPU-gated: `cargo test -- --ignored`.

use rawkit_editstate::{Calibration, EditState};
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const W: u32 = 64;
const H: u32 = 64;

/// The left half a dark neutral, the right half a bright one — so one frame
/// answers both "did the grey stay grey" and "did the tint stay in the shadows".
fn mosaic() -> Vec<f32> {
    (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| {
            let level = if x < W / 2 { 0.02 } else { 0.55 };
            // RGGB, and the same value in every channel: whatever comes out
            // coloured was coloured by the pipeline.
            let _ = y;
            level
        })
        .collect()
}

fn render(gpu: &Gpu, state: &EditState) -> Vec<f32> {
    let cfa = mosaic();
    let frame = Frame {
        data: &cfa,
        width: W,
        height: H,
        phase: BayerPhase::Rggb,
        as_shot_wb: [1.0, 1.0, 1.0],
        clip_level: f32::INFINITY,
        profile: CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY),
        recorded_orientation: rawkit_editstate::Orientation::AsShot,
    };
    let renderer = Renderer::new(gpu);
    renderer
        .run(gpu, &frame, state, Output::Display)
        .expect("a render")
        .pixels
}

/// The average colour of a column band, away from the edges the demosaic has to
/// guess at.
fn patch(pixels: &[f32], from: u32, to: u32) -> [f32; 3] {
    let mut total = [0.0f64; 3];
    let mut count = 0.0f64;
    for y in 8..H - 8 {
        for x in from..to {
            let at = ((y * W + x) * 4) as usize;
            for channel in 0..3 {
                total[channel] += pixels[at + channel] as f64;
            }
            count += 1.0;
        }
    }
    [
        (total[0] / count) as f32,
        (total[1] / count) as f32,
        (total[2] / count) as f32,
    ]
}

fn cast(rgb: [f32; 3]) -> f32 {
    let mean = (rgb[0] + rgb[1] + rgb[2]) / 3.0;
    rgb.iter().map(|c| (c - mean).abs()).fold(0.0, f32::max) / mean.max(1e-6)
}

#[test]
#[ignore = "requires a GPU adapter"]
fn moving_the_primaries_leaves_a_grey_grey() {
    // The property that separates a calibration from a cast, asserted where it
    // is felt. Rotating three primaries changes where their sum lands, and their
    // sum is the neutral — so without the gain solve every touch of one of these
    // sliders would warm or cool the whole photograph, which reads as the white
    // balance drifting while you adjust a red.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let plain = render(&gpu, &EditState::default());
    let before = cast(patch(&plain, 40, 56));
    assert!(before < 0.02, "the test frame is not neutral to begin with");

    for calibration in [
        Calibration {
            red_hue: 1.0,
            ..Calibration::default()
        },
        Calibration {
            green_hue: -1.0,
            blue_saturation: 1.0,
            ..Calibration::default()
        },
        Calibration {
            red_hue: -0.7,
            green_saturation: 0.8,
            blue_hue: 1.0,
            red_saturation: -1.0,
            ..Calibration::default()
        },
    ] {
        let state = EditState {
            calibration,
            ..EditState::default()
        };
        let pixels = render(&gpu, &state);
        let now = cast(patch(&pixels, 40, 56));
        assert!(
            now < 0.02,
            "{calibration:?} left a {:.1}% cast on a grey",
            now * 100.0
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_shadow_tint_stays_in_the_shadows() {
    // The other half of the panel, and the half that is per pixel. If the weight
    // were wrong this would be a green cast on the whole photograph — which is
    // white balance, and there is already a control for that.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let plain = render(&gpu, &EditState::default());
    let tinted = render(
        &gpu,
        &EditState {
            calibration: Calibration {
                shadow_tint: -1.0,
                ..Calibration::default()
            },
            ..EditState::default()
        },
    );

    let dark = (patch(&plain, 8, 24), patch(&tinted, 8, 24));
    let bright = (patch(&plain, 40, 56), patch(&tinted, 40, 56));
    // Green at negative, so the dark half gains green against its neighbours.
    let greener =
        |was: [f32; 3], now: [f32; 3]| (now[1] / now[0].max(1e-6)) - (was[1] / was[0].max(1e-6));
    let in_shadow = greener(dark.0, dark.1);
    let in_light = greener(bright.0, bright.1);
    assert!(
        in_shadow > 0.02,
        "the shadow tint did not reach the shadows: {in_shadow}"
    );
    assert!(
        in_light.abs() < in_shadow / 4.0,
        "the shadow tint reached the highlights: {in_light} against {in_shadow}"
    );
}
