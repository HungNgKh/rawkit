//! The vignette and the grain, and the two things about them that are decisions
//! rather than arithmetic: what a vignette is centred on, and what it does to a
//! highlight.
//!
//! `cargo test -p rawkit-engine --test effects -- --ignored`

use rawkit_editstate::{Crop, EditState, Effects, Lens};
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const W: u32 = 512;
const H: u32 = 512;

struct Shot {
    pixels: Vec<f32>,
    width: u32,
}

impl Shot {
    fn luma(&self, x: u32, y: u32) -> f32 {
        let i = ((y * self.width + x) * 4) as usize;
        0.2126 * self.pixels[i] + 0.7152 * self.pixels[i + 1] + 0.0722 * self.pixels[i + 2]
    }
}

fn render(gpu: &Gpu, cfa: &[f32], state: &EditState) -> Shot {
    let out = Renderer::new(gpu)
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
        .expect("render");
    Shot {
        pixels: out.pixels,
        width: out.width,
    }
}

fn flat() -> Vec<f32> {
    vec![0.3f32; (W * H) as usize]
}

/// Dark on the left, bright on the right — so one test can ask what the vignette
/// does to a shadow and to a highlight in the same frame.
fn split() -> Vec<f32> {
    let mut m = vec![0.0f32; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            // Six, not something under one: the tone map lands full scale at
            // about 0.55, so a "bright" half at 0.85 develops to a *midtone* and
            // a test written against it would be measuring the curve rather than
            // the vignette. `clip_level` is infinite here, so this is light the
            // sensor did not clip.
            m[(y * W + x) as usize] = if x < W / 2 { 0.08 } else { 6.0 };
        }
    }
    m
}

fn effects(f: impl Fn(&mut Effects)) -> EditState {
    let mut effects = Effects::default();
    f(&mut effects);
    EditState {
        effects,
        ..EditState::default()
    }
}

fn gpu() -> Option<Gpu> {
    Gpu::new().ok()
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_vignette_darkens_the_corners_and_leaves_the_middle() {
    let Some(gpu) = gpu() else { return };
    let plain = render(&gpu, &flat(), &EditState::default());
    let dark = render(&gpu, &flat(), &effects(|e| e.vignette = -0.8));

    let middle = (plain.luma(W / 2, H / 2), dark.luma(W / 2, H / 2));
    assert!(
        (middle.1 - middle.0).abs() < 1e-3,
        "the middle moved from {} to {}",
        middle.0,
        middle.1
    );
    for (x, y) in [(6u32, 6u32), (W - 7, 6), (6, H - 7), (W - 7, H - 7)] {
        let (was, now) = (plain.luma(x, y), dark.luma(x, y));
        assert!(
            now < was * 0.75,
            "the corner at ({x}, {y}) only went from {was} to {now}"
        );
    }
    // And the other way: positive lifts them.
    let light = render(&gpu, &flat(), &effects(|e| e.vignette = 0.8));
    assert!(light.luma(6, 6) > plain.luma(6, 6) * 1.1);
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_vignette_holds_a_highlight_back() {
    // The decision that makes it read as a lens rather than as a grey wash. The
    // same vignette, at the same radius, on a dark corner and a bright one: the
    // bright one has to keep very nearly all of its luminosity.
    let Some(gpu) = gpu() else { return };
    let plain = render(&gpu, &split(), &EditState::default());
    let dark = render(&gpu, &split(), &effects(|e| e.vignette = -0.9));

    // Two corners on the same row, equally far out, one over the shadow and one
    // over the highlight.
    let (shadow, highlight) = ((8u32, 8u32), (W - 9, 8));
    let shadow_ratio = dark.luma(shadow.0, shadow.1) / plain.luma(shadow.0, shadow.1);
    let highlight_ratio =
        dark.luma(highlight.0, highlight.1) / plain.luma(highlight.0, highlight.1);
    println!("shadow keeps {shadow_ratio:.3}, highlight keeps {highlight_ratio:.3}");
    assert!(
        shadow_ratio < 0.6,
        "the shadow corner kept {shadow_ratio}, so the vignette is barely doing anything"
    );
    assert!(
        highlight_ratio > 0.75,
        "the highlight corner kept only {highlight_ratio}, which is a grey wash rather \
         than a vignette"
    );
    // The claim that survives whatever the amount is set to: whatever the
    // shadow gives up, the highlight gives up far less.
    assert!(
        highlight_ratio > shadow_ratio * 2.0,
        "shadow kept {shadow_ratio} and highlight {highlight_ratio}, which is the same \
         darkening applied to both"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_vignette_follows_the_crop_and_the_lens_correction_does_not() {
    // The two controls exist because these two answers differ, and this is the
    // frame where they differ: a crop of the top-left quarter, so the sensor's
    // centre is the *bottom-right corner* of what comes out.
    let Some(gpu) = gpu() else { return };
    let quarter = Crop {
        left: 0.0,
        top: 0.0,
        right: 0.5,
        bottom: 0.5,
        ..Crop::default()
    };
    let plain = render(
        &gpu,
        &flat(),
        &EditState {
            crop: quarter,
            ..EditState::default()
        },
    );

    // The creative one is centred on what is left, so it is symmetric about the
    // middle of the output.
    let vignetted = render(
        &gpu,
        &flat(),
        &EditState {
            crop: quarter,
            effects: Effects {
                vignette: -0.8,
                ..Effects::default()
            },
            ..EditState::default()
        },
    );
    let (w, h) = (plain.width, plain.pixels.len() as u32 / 4 / plain.width);
    let ratio = |s: &Shot, x: u32, y: u32| s.luma(x, y) / plain.luma(x, y);
    let corners: Vec<f32> = [(6, 6), (w - 7, 6), (6, h - 7), (w - 7, h - 7)]
        .iter()
        .map(|(x, y)| ratio(&vignetted, *x, *y))
        .collect();
    let spread = corners.iter().fold(f32::MIN, |a, b| a.max(*b))
        - corners.iter().fold(f32::MAX, |a, b| a.min(*b));
    assert!(
        spread < 0.05,
        "the crop's four corners were darkened by {corners:?}, which is not symmetric \
         about the crop"
    );

    // The lens correction is centred on the sensor, whose middle is now this
    // crop's bottom-right corner — so the correction has to be *lopsided*, and
    // strongest at the top-left where the glass is furthest off axis.
    let corrected = render(
        &gpu,
        &flat(),
        &EditState {
            crop: quarter,
            lens: Lens {
                vignette: 0.8,
                ..Lens::default()
            },
            ..EditState::default()
        },
    );
    let near_axis = ratio(&corrected, w - 7, h - 7);
    let far_off_axis = ratio(&corrected, 6, 6);
    println!("lens lift: near the axis {near_axis:.3}, far off it {far_off_axis:.3}");
    assert!(
        far_off_axis > near_axis * 1.2,
        "the lens correction lifted the sensor's own centre by {near_axis} and its \
         corner by {far_off_axis}, so it is following the crop"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn grain_moves_a_pixel_along_grey_and_its_size_is_a_size() {
    let Some(gpu) = gpu() else { return };
    let plain = render(&gpu, &flat(), &EditState::default());
    let grained = render(&gpu, &flat(), &effects(|e| e.grain = 0.8));

    // It is there.
    let variation = |s: &Shot| {
        let values: Vec<f32> = (100..200)
            .flat_map(|y| (100..200).map(move |x| (x, y)))
            .map(|(x, y)| s.luma(x, y))
            .collect();
        let mean = values.iter().sum::<f32>() / values.len() as f32;
        (values.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / values.len() as f32).sqrt()
    };
    assert!(
        variation(&grained) > variation(&plain) + 0.005,
        "no grain arrived: {} against {}",
        variation(&plain),
        variation(&grained)
    );

    // And it is grey. A pixel that gained colour would mean the noise went in
    // per channel, which reads as a high-ISO sensor rather than as film.
    let mut worst = 0.0f32;
    for y in 100..200 {
        for x in 100..200 {
            let i = ((y * grained.width + x) * 4) as usize;
            let (r, g, b) = (
                grained.pixels[i],
                grained.pixels[i + 1],
                grained.pixels[i + 2],
            );
            let j = ((y * plain.width + x) * 4) as usize;
            let before = [plain.pixels[j], plain.pixels[j + 1], plain.pixels[j + 2]];
            // The three channels must all have moved by the same amount.
            let moved = [r - before[0], g - before[1], b - before[2]];
            worst = worst
                .max((moved[0] - moved[1]).abs())
                .max((moved[1] - moved[2]).abs());
        }
    }
    assert!(worst < 1e-4, "grain moved the channels apart by {worst}");

    // The size control is a size: fine grain carries more energy between
    // neighbours than coarse grain does.
    let roughness = |s: &Shot| {
        let mut sum = 0.0;
        for y in 100..200 {
            for x in 101..200 {
                sum += (s.luma(x, y) - s.luma(x - 1, y)).abs();
            }
        }
        sum
    };
    let fine = render(
        &gpu,
        &flat(),
        &effects(|e| {
            e.grain = 0.8;
            e.grain_size = 1.5;
        }),
    );
    let coarse = render(
        &gpu,
        &flat(),
        &effects(|e| {
            e.grain = 0.8;
            e.grain_size = 10.0;
        }),
    );
    assert!(
        roughness(&fine) > roughness(&coarse) * 1.5,
        "grain at 1.5 px is no finer than at 10: {} against {}",
        roughness(&fine),
        roughness(&coarse)
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn neither_effect_at_rest_changes_a_bit() {
    let Some(gpu) = gpu() else { return };
    let plain = render(&gpu, &split(), &EditState::default());
    let idle = render(&gpu, &split(), &effects(|e| e.midpoint = 0.2));
    assert_eq!(
        plain.pixels, idle.pixels,
        "a midpoint on a vignette of nothing changed the picture"
    );
    let no_lens = render(
        &gpu,
        &split(),
        &EditState {
            lens: Lens::default(),
            ..EditState::default()
        },
    );
    assert_eq!(plain.pixels, no_lens.pixels);
}
