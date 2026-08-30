//! The develop stage: white balance, camera profile, exposure, tone map.
//!
//! These test *properties*, not numbers. Asserting that a particular input
//! produces 0.4213 would pin the tone curve's exact shape, and the shape is
//! expected to change — it is a taste problem with its own iteration loop. What
//! must not change is the behaviour the rest of the pipeline relies on: mid-grey
//! stays put, nothing clips, order is preserved, and a stop is a stop.
//!
//! `cargo test -p rawkit-engine --test develop -- --ignored`

use rawkit_editstate::EditState;
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const N: u32 = 64;

/// A camera whose native primaries *are* sRGB's, so the profile stage has
/// nothing to do and its transform comes out as the identity.
///
/// Note this is not the identity matrix: a profile stores XYZ-to-camera, so the
/// camera that needs no correction is the one holding the inverse of sRGB's
/// primaries. Using the identity here instead would silently insert a real
/// colour transform into tests that mean to isolate tone and white balance —
/// which is exactly what it did before this comment existed.
fn neutral_profile() -> CameraProfile {
    use rawkit_engine::profile::{invert, XYZ_FROM_SRGB};
    CameraProfile::from_color_matrix(invert(&XYZ_FROM_SRGB).expect("sRGB primaries are invertible"))
}

/// Render a flat frame of one scene-linear value and return the developed
/// result at its centre.
///
/// A flat mosaic demosaics to a flat image, so this isolates the develop stage
/// from the interpolation entirely: whatever comes back is the tone response of
/// the given value.
fn develop(gpu: &Gpu, renderer: &Renderer, value: f32, state: &EditState) -> [f32; 3] {
    let cfa = vec![value; (N * N) as usize];
    let out = renderer
        .run(
            gpu,
            &Frame {
                data: &cfa,
                width: N,
                height: N,
                phase: BayerPhase::Rggb,
                as_shot_wb: [1.0, 1.0, 1.0],
                // These measure the tone curve, with inputs far above full
                // scale on purpose. Reconstruction would rewrite exactly the
                // values under test.
                clip_level: f32::INFINITY,
                profile: neutral_profile(),
                recorded_orientation: rawkit_editstate::Orientation::AsShot,
            },
            state,
            Output::Display,
        )
        .expect("render failed")
        .pixels;
    let i = ((N / 2 * N + N / 2) * 4) as usize;
    [out[i], out[i + 1], out[i + 2]]
}

#[test]
#[ignore = "requires a GPU adapter"]
fn mid_grey_survives_the_tone_map() {
    // If the tone map moved mid-grey, it would be a second brightness control
    // and exposure would no longer mean what it says. Everything about the
    // scene-linear core depends on exposure being the one thing that moves
    // brightness.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);
    let out = develop(&gpu, &renderer, 0.18, &EditState::default());
    for (c, v) in out.iter().enumerate() {
        assert!(
            (v - 0.18).abs() < 0.005,
            "scene mid-grey rendered to {v} in channel {c}, not 0.18"
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn highlights_roll_off_instead_of_clipping() {
    // A sensor routinely records values past full scale, and a clipping curve
    // turns those into flat patches — worse, into *coloured* flat patches when
    // one channel saturates first. That is the magenta-highlight artefact, and
    // the tone map is the first line of defence against it.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);
    let state = EditState::default();

    let bright = develop(&gpu, &renderer, 4.0, &state)[0];
    let brighter = develop(&gpu, &renderer, 40.0, &state)[0];

    assert!(bright < 1.0, "4x full scale already clipped: {bright}");
    assert!(brighter < 1.0, "40x full scale clipped: {brighter}");
    assert!(
        brighter > bright,
        "the curve stopped responding between 4x and 40x ({bright} -> {brighter}); \
         detail above full scale is being thrown away"
    );
    assert!(
        brighter > 0.9,
        "40x full scale should be near white, got {brighter}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_curve_is_monotonic() {
    // A non-monotonic curve inverts local contrast: a brighter part of the scene
    // comes out darker. It looks like a solarisation artefact and is not the
    // kind of thing that gets noticed in one image.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);
    let state = EditState::default();

    let mut previous = -1.0f32;
    for step in 0..24 {
        let scene = 0.01 * 1.4f32.powi(step);
        let display = develop(&gpu, &renderer, scene, &state)[0];
        assert!(
            display > previous,
            "scene {scene} rendered to {display}, not above the previous {previous}"
        );
        previous = display;
    }
    assert!(previous <= 1.0, "the curve escaped its range: {previous}");
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_stop_of_exposure_is_a_stop_of_light() {
    // The property that makes exposure meaningful in a scene-linear pipeline:
    // +1 EV on half the light must land exactly where the full light lands with
    // no adjustment. Note this is checked *through* the tone map without knowing
    // anything about its shape — which is why it stays true when the curve
    // changes.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);

    let mut lifted = EditState::default();
    lifted.tone.exposure_ev = 1.0;

    for scene in [0.02, 0.09, 0.35, 1.5] {
        let reference = develop(&gpu, &renderer, scene * 2.0, &EditState::default())[0];
        let exposed = develop(&gpu, &renderer, scene, &lifted)[0];
        assert!(
            (reference - exposed).abs() < 1e-4,
            "+1 EV on {scene} gave {exposed}, but {} unadjusted gives {reference}",
            scene * 2.0
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn white_balance_multiplies_channels_independently() {
    // Scene-linear is what makes white balance three multiplies rather than a
    // colour-appearance model. Stated exactly rather than as an ordering:
    // channel c of a neutral frame with multiplier m must equal a neutral frame
    // of `value * m` rendered with no white balance at all. That holds whatever
    // the tone curve does, because the multiply happens before it.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);
    let wb = [2.0f32, 1.0, 1.5];
    let value = 0.1f32;

    let cfa = vec![value; (N * N) as usize];
    let out = renderer
        .run(
            &gpu,
            &Frame {
                data: &cfa,
                width: N,
                height: N,
                phase: BayerPhase::Rggb,
                as_shot_wb: wb,
                clip_level: f32::INFINITY,
                profile: neutral_profile(),
                recorded_orientation: rawkit_editstate::Orientation::AsShot,
            },
            &EditState::default(),
            Output::Display,
        )
        .expect("render failed")
        .pixels;
    let i = ((N / 2 * N + N / 2) * 4) as usize;

    for (c, m) in wb.iter().enumerate() {
        let expected = develop(&gpu, &renderer, value * m, &EditState::default())[c];
        assert!(
            (out[i + c] - expected).abs() < 1e-4,
            "channel {c} with multiplier {m} gave {}, but {} unbalanced gives {expected}",
            out[i + c],
            value * m
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn setting_a_temperature_warms_or_cools_the_render() {
    // The white-balance slider, end to end: an explicit temperature now becomes
    // multipliers through the profile instead of being refused. Direction is the
    // thing to pin — a slider that works perfectly and backwards is a real
    // failure mode, and no test of the maths alone would catch the wiring being
    // reversed between EditState and the kernel.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);
    let profile = CameraProfile::from_color_matrix([
        [0.6941, -0.2164, -0.0644],
        [-0.3850, 1.1349, 0.2779],
        [-0.0031, 0.1055, 0.6511],
    ]);

    let cfa = vec![0.2f32; (N * N) as usize];
    let render_at = |kelvin: f32| {
        let mut state = EditState::default();
        state.white_balance.temperature_k = Some(kelvin);
        let out = renderer
            .run(
                &gpu,
                &Frame {
                    data: &cfa,
                    width: N,
                    height: N,
                    phase: BayerPhase::Rggb,
                    as_shot_wb: [1.0, 1.0, 1.0],
                    clip_level: f32::INFINITY,
                    profile: profile.clone(),
                    recorded_orientation: rawkit_editstate::Orientation::AsShot,
                },
                &state,
                Output::Display,
            )
            .expect("render failed")
            .pixels;
        let i = ((N / 2 * N + N / 2) * 4) as usize;
        [out[i], out[i + 1], out[i + 2]]
    };

    let cool = render_at(3000.0);
    let warm = render_at(9000.0);
    assert!(
        warm[0] / warm[2] > cool[0] / cool[2],
        "raising the stated temperature did not warm the image: \
         3000K gave {cool:?}, 9000K gave {warm:?}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn as_shot_reports_a_plausible_temperature() {
    // "As Shot 5200 K" is a label the UI has to produce from multipliers, and
    // the render uses the same conversion to pick its matrix. Checking it here
    // means the number shown to a user and the number used to render cannot
    // drift apart.
    let profile = CameraProfile::from_color_matrix([
        [0.6941, -0.2164, -0.0644],
        [-0.3850, 1.1349, 0.2779],
        [-0.0031, 0.1055, 0.6511],
    ]);
    let cfa = vec![0.2f32; (N * N) as usize];
    // The real ILCE-6400 sample's as-shot multipliers.
    let frame = Frame {
        data: &cfa,
        width: N,
        height: N,
        phase: BayerPhase::Rggb,
        as_shot_wb: [2.750, 1.0, 1.695],
        clip_level: 1.0,
        recorded_orientation: rawkit_editstate::Orientation::AsShot,
        profile,
    };
    let (temperature, tint) = frame.as_shot_temperature();
    println!("as-shot: {temperature:.0} K, tint {tint:.1}");
    assert!(
        (2000.0..12000.0).contains(&temperature),
        "as-shot temperature {temperature} K is not a temperature a camera would report"
    );
    assert!(tint.abs() < 60.0, "implausible as-shot tint {tint}");
}
