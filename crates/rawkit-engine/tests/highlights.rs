//! Highlight reconstruction.
//!
//! # The artefact this exists to remove
//!
//! A sensor saturates in its own units, and white balance then scales the
//! channels apart. For a daylight scene green carries the most signal and
//! saturates first: its true value might be 1.5 but it records 1.0. Red is
//! nowhere near its own limit, records correctly, and is then multiplied up. The
//! result is a pixel where red and blue exceed green — magenta — in the one part
//! of the picture the eye most expects to be white.
//!
//! `cargo test -p rawkit-engine --test highlights -- --ignored`

use rawkit_editstate::EditState;
use rawkit_engine::profile::{invert, XYZ_FROM_SRGB};
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const N: u32 = 64;

/// The default edit with capture sharpening turned off.
///
/// This measures highlight reconstruction, and sharpening is a different stage
/// three steps later: it moves a channel by a millionth, which is nothing to a
/// photograph and enough to fail "reconstruction never darkens a channel". The
/// same reason `pyramid.rs` renders with clipping disabled — a test should not
/// be measuring the stage it did not name.
fn unsharpened() -> EditState {
    let mut state = EditState::default();
    state.detail.sharpen_amount = 0.0;
    state
}

/// Sony ILCE-6400 daylight multipliers, which are what make green clip first.
const DAYLIGHT_WB: [f32; 3] = [2.75, 1.0, 1.70];

/// A camera whose primaries are sRGB's, so the profile stage contributes
/// nothing and what comes out is the reconstruction and the tone curve.
fn neutral_profile() -> CameraProfile {
    CameraProfile::from_color_matrix(invert(&XYZ_FROM_SRGB).expect("invertible"))
}

/// Render a flat frame of one camera-space colour.
fn render(gpu: &Gpu, camera: [f32; 3], wb: [f32; 3], clip: f32) -> [f32; 3] {
    let mut cfa = Vec::with_capacity((N * N) as usize);
    for y in 0..N {
        for x in 0..N {
            let channel = if (x + y) % 2 == 1 {
                1
            } else if y % 2 == 0 {
                0
            } else {
                2
            };
            cfa.push(camera[channel]);
        }
    }
    let out = Renderer::new(gpu)
        .run(
            gpu,
            &Frame {
                data: &cfa,
                width: N,
                height: N,
                phase: BayerPhase::Rggb,
                as_shot_wb: wb,
                clip_level: clip,
                profile: neutral_profile(),
            },
            &unsharpened(),
            Output::Display,
        )
        .expect("render failed")
        .pixels;
    let i = ((N / 2 * N + N / 2) * 4) as usize;
    [out[i], out[i + 1], out[i + 2]]
}

/// How far a colour is from neutral, as a fraction of its brightest channel.
/// Zero is grey.
fn cast(rgb: [f32; 3]) -> f32 {
    let high = rgb[0].max(rgb[1]).max(rgb[2]);
    let low = rgb[0].min(rgb[1]).min(rgb[2]);
    if high <= 0.0 {
        0.0
    } else {
        (high - low) / high
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_clipped_green_channel_no_longer_goes_magenta() {
    // The exact situation from the sample photograph: green pinned at the
    // sensor limit while red and blue still have room. Without reconstruction
    // the balanced pixel is red-and-blue heavy; with it, the channel nobody
    // believes is raised to the level of the ones we do.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let clipped_green = [0.55, 1.0, 0.75];

    let raw = render(&gpu, clipped_green, DAYLIGHT_WB, f32::INFINITY);
    let fixed = render(&gpu, clipped_green, DAYLIGHT_WB, 1.0);

    println!("without reconstruction: {raw:?} cast {:.3}", cast(raw));
    println!("with reconstruction   : {fixed:?} cast {:.3}", cast(fixed));

    assert!(
        cast(raw) > 0.15,
        "the test case does not actually produce a cast: {raw:?}"
    );
    assert!(
        cast(fixed) < cast(raw) * 0.5,
        "reconstruction barely helped: cast went {:.3} -> {:.3}",
        cast(raw),
        cast(fixed)
    );

    // Note what is *not* asserted: that the result is neutral. Blue here is
    // below its own limit, so it is real measured data and the spread it
    // creates is real scene colour. Forcing the pixel all the way to grey would
    // be discarding a channel we have every reason to believe — the opposite
    // mistake to the one being fixed, and just as wrong.
    //
    // What must be true is that green is no longer the channel left behind,
    // because that deficiency is an artefact of the sensor and nothing else.
    assert!(
        fixed[1] >= fixed[0] * 0.99,
        "green is still short of red after reconstruction: {fixed:?}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn an_unclipped_pixel_is_untouched() {
    // Reconstruction must be invisible everywhere it is not needed. Not
    // "almost invisible" — the run-up uses smoothstep, which is exactly zero
    // below its lower edge, so this is an equality and worth asserting as one.
    // A version that quietly shifted every midtone would be very hard to see
    // and very easy to ship.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    for colour in [
        [0.10, 0.20, 0.15],
        [0.30, 0.42, 0.25],
        [0.05, 0.08, 0.06],
        // Right below the run-up: green at 0.97 against a threshold of 1.0.
        [0.20, 0.97, 0.30],
    ] {
        let off = render(&gpu, colour, DAYLIGHT_WB, f32::INFINITY);
        let on = render(&gpu, colour, DAYLIGHT_WB, 1.0);
        for c in 0..3 {
            assert_eq!(
                off[c], on[c],
                "reconstruction changed an unclipped pixel {colour:?}: {off:?} vs {on:?}"
            );
        }
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_fully_clipped_pixel_renders_neutral() {
    // When every channel is at its limit there is nothing left to take a level
    // from, and leaving the channels where they are renders the colour of the
    // white balance itself — a strong cast in the brightest part of the frame.
    // The only defensible answer is neutral.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let blown = render(&gpu, [1.0, 1.0, 1.0], DAYLIGHT_WB, 1.0);
    println!("fully clipped: {blown:?} cast {:.3}", cast(blown));
    assert!(
        cast(blown) < 0.06,
        "a blown highlight kept the white balance's colour: {blown:?}"
    );
    assert!(
        blown.iter().all(|&v| v > 0.7),
        "a blown highlight should be bright: {blown:?}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn reconstruction_never_darkens() {
    // Reconstruction only ever raises a channel it does not believe. If it
    // could lower one, a highlight would develop a dark rim where the run-up
    // begins — the artefact the smooth entry exists to avoid, arriving by
    // another route.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    for green in [0.980, 0.985, 0.990, 0.995, 1.0, 1.2] {
        let off = render(&gpu, [0.55, green, 0.75], DAYLIGHT_WB, f32::INFINITY);
        let on = render(&gpu, [0.55, green, 0.75], DAYLIGHT_WB, 1.0);
        for c in 0..3 {
            assert!(
                on[c] >= off[c] - 1e-6,
                "green {green}: reconstruction darkened channel {c}, {off:?} -> {on:?}"
            );
        }
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_transition_into_clipping_is_gradual() {
    // A hard switch at the clip point leaves a visible edge around every
    // highlight, which is a worse artefact than the magenta it replaces and
    // much harder to explain. Stepping across the boundary must not produce a
    // jump larger than the steps either side of it.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let sample = |green: f32| render(&gpu, [0.55, green, 0.75], DAYLIGHT_WB, 1.0)[1];

    let steps: Vec<f32> = (0..12).map(|i| sample(0.970 + 0.005 * i as f32)).collect();
    let deltas: Vec<f32> = steps.windows(2).map(|w| w[1] - w[0]).collect();
    let largest = deltas.iter().cloned().fold(f32::MIN, f32::max);
    let typical = deltas.iter().sum::<f32>() / deltas.len() as f32;
    println!("green steps: {steps:?}");
    assert!(
        largest < typical.abs() * 4.0 + 0.02,
        "a step change at the clip point: deltas {deltas:?}"
    );
}
