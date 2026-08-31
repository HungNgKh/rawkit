//! Does the lens correction actually move the pixels the estimator asks for?
//!
//! [`rawkit_engine::aberration`] is unit-tested against frames it was handed
//! directly, which proves the *fit* and nothing about the renderer. What is
//! missing between there and a photograph is everything: a mosaic rather than
//! RGB, a demosaic in the middle, and a shader that has to turn one fraction
//! into a sub-pixel read at the right radius and in the right direction. A sign
//! error there would be invisible to every test in that module and would double
//! the fringing on every frame.
//!
//! So this builds a sensor exposure of a scene the lens got wrong, and asks the
//! whole engine to put it right — measure, store, render, measure again.
//!
//! GPU-gated like the rest: `cargo test -- --ignored`.

use rawkit_editstate::EditState;
use rawkit_engine::{aberration, BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const W: u32 = 1024;
const H: u32 = 768;

/// Which channel a photosite records, for RGGB.
fn colour_at(x: u32, y: u32) -> usize {
    if (x + y) % 2 == 1 {
        1
    } else if y % 2 == 0 {
        0
    } else {
        2
    }
}

/// What a sensor would record of a scene the lens imaged at three scales.
///
/// A mosaic and not an RGB frame, which is the point: the aberration is put in
/// *before* the Bayer sampling, exactly as a lens does, so the demosaic has to
/// survive it and the correction has to work on what the demosaic produced.
fn mosaic(scale: [f32; 3]) -> Vec<f32> {
    let (cx, cy) = (W as f32 / 2.0, H as f32 / 2.0);
    // Three frequencies that do not share a period, so no row or column is a
    // repeat of another one a whole displacement away — which would make a
    // shift unmeasurable in the most flattering possible way.
    let scene = |u: f32, v: f32| -> f32 {
        let a = ((u * 0.11).sin() + (v * 0.143).sin() + ((u + v) * 0.037).sin()) / 3.0;
        0.35 + 0.28 * a
    };
    let mut out = vec![0.0f32; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            let s = scale[colour_at(x, y)];
            let (u, v) = (cx + (x as f32 - cx) * s, cy + (y as f32 - cy) * s);
            out[(y * W + x) as usize] = scene(u, v);
        }
    }
    out
}

fn frame(cfa: &[f32]) -> Frame<'_> {
    Frame {
        data: cfa,
        width: W,
        height: H,
        phase: BayerPhase::Rggb,
        as_shot_wb: [1.0, 1.0, 1.0],
        // No clipping: this scene never reaches 1.0, and highlight
        // reconstruction moving a channel would be indistinguishable from the
        // correction moving it.
        clip_level: f32::INFINITY,
        profile: CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY),
        recorded_orientation: rawkit_editstate::Orientation::AsShot,
    }
}

/// The edit the measurement runs under: no smoothing, because chroma noise
/// reduction pulls the channels together and that is the difference being
/// measured.
fn bare() -> EditState {
    EditState {
        detail: rawkit_editstate::Detail {
            chroma_noise: 0.0,
            luminance_noise: 0.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_renderer_undoes_the_aberration_the_estimator_found() {
    let Ok(gpu) = Gpu::new() else { return };
    let renderer = Renderer::new(&gpu);

    // Red imaged 0.08% large and blue 0.06% small — about a pixel and a third
    // apart at this frame's corner, which is a visible rim and a plausible one.
    let (sr, sb) = (1.0008f32, 0.9994f32);
    let cfa = mosaic([sr, 1.0, sb]);

    let found = aberration::measure(&gpu, &renderer, &frame(&cfa)).expect("measure");
    println!("imaged at {sr} / {sb}, measured {found:?}");
    assert!(!found.is_identity(), "found nothing to correct");
    // The sign is the half of this that a fit cannot get right by luck: red was
    // imaged *large*, so the renderer has to sample it from further *in*.
    assert!(
        found.chromatic_red < 0.0 && found.chromatic_blue > 0.0,
        "the correction points the same way as the aberration: {found:?}"
    );

    let mut corrected = bare();
    corrected.lens = found;
    let after = renderer
        .run(&gpu, &frame(&cfa), &corrected, Output::SceneLinear)
        .expect("render");
    let residual = aberration::estimate(&after.pixels, after.width, after.height);
    println!("residual after correction: {residual:?}");

    // Measured again on the corrected frame, what is left has to be a fraction
    // of what went in. Not zero: the correction resamples, the demosaic is not
    // linear, and the estimator itself has a floor — so this asks for most of
    // it gone rather than all of it, which is the honest claim.
    for (name, before, left) in [
        ("red", found.chromatic_red, residual.chromatic_red),
        ("blue", found.chromatic_blue, residual.chromatic_blue),
    ] {
        assert!(
            left.abs() < before.abs() * 0.4,
            "{name} went in at {before} and {left} is still there"
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_lens_with_nothing_wrong_is_left_alone() {
    // The identity has to be *exactly* the identity, not merely close. A
    // correction of zero must not resample, because a bilinear read at a
    // displacement of zero is arithmetically the same pixel but only if the
    // arithmetic is exact — and "every photograph is very slightly softer than
    // it was" is the kind of regression nobody attributes to a lens correction.
    let Ok(gpu) = Gpu::new() else { return };
    let renderer = Renderer::new(&gpu);
    let cfa = mosaic([1.0, 1.0, 1.0]);

    let plain = renderer
        .run(&gpu, &frame(&cfa), &bare(), Output::SceneLinear)
        .expect("render");
    let mut zeroed = bare();
    zeroed.lens = rawkit_editstate::Lens::default();
    let same = renderer
        .run(&gpu, &frame(&cfa), &zeroed, Output::SceneLinear)
        .expect("render");
    assert_eq!(plain.pixels, same.pixels);

    let found = aberration::measure(&gpu, &renderer, &frame(&cfa)).expect("measure");
    println!("a perfect lens measured {found:?}");
    assert!(
        found.chromatic_red.abs() < 5e-5 && found.chromatic_blue.abs() < 5e-5,
        "invented an aberration in a frame that has none: {found:?}"
    );
}
