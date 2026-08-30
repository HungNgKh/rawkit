//! Is vibrance a different control from saturation, or just a weaker one?
//!
//! It has to be different, or it does not earn a slider. Saturation scales every
//! colour's distance from grey by the same factor; vibrance moves colours
//! **towards the middle of the range** — lifting the flat ones and sparing the
//! vivid, and in the negative direction calming the vivid and sparing the flat.
//!
//! That is what makes it usable where saturation is not: a sky can come up
//! without the one red jacket in the frame turning to poster paint. The test is
//! therefore comparative — how much each control moved a flat colour against how
//! much it moved a vivid one — because "it changed something" is true of both
//! and says nothing.
//!
//! GPU-gated like the rest: `cargo test -- --ignored`.

use rawkit_editstate::EditState;
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const W: u32 = 64;
const H: u32 = 64;

/// A mosaic whose left half is nearly grey and whose right half is strongly
/// coloured, so one render carries both cases and neither can drift from the
/// other through a difference in exposure or profile.
fn mosaic() -> Vec<f32> {
    (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| {
            let vivid = x >= W / 2;
            let (r, g, b) = if vivid {
                (0.45, 0.18, 0.15)
            } else {
                (0.32, 0.30, 0.29)
            };
            // RGGB.
            match ((x % 2 == 0), (y % 2 == 0)) {
                (true, true) => r,
                (false, false) => b,
                _ => g,
            }
        })
        .collect()
}

fn saturation_halves(gpu: &Gpu, state: &EditState) -> (f32, f32) {
    let cfa = mosaic();
    let pixels = Renderer::new(gpu)
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
        .pixels;

    // Distance from grey, relative to brightness — *not* the HSV saturation the
    // shader weights vibrance by. That measure compresses as a colour approaches
    // full, so a ratio of it is not linear in the control, and the first version
    // of this test read the measure's own non-linearity as saturation treating
    // the two halves differently. This one scales exactly with the saturation
    // slider, which is what makes the comparison mean anything.
    //
    // Sampled well inside each half, so the demosaic's reach across the boundary
    // cannot touch it.
    let mean = |x0: u32, x1: u32| {
        let mut total = 0.0;
        let mut count = 0.0;
        for y in 8..H - 8 {
            for x in x0..x1 {
                let i = ((y * W + x) * 4) as usize;
                let (r, g, b) = (pixels[i], pixels[i + 1], pixels[i + 2]);
                let grey = 0.2126 * r + 0.7152 * g + 0.0722 * b;
                let chroma = ((r - grey).powi(2) + (g - grey).powi(2) + (b - grey).powi(2)).sqrt();
                total += chroma / grey.max(1e-6);
                count += 1.0;
            }
        }
        total / count
    };
    (mean(8, W / 2 - 8), mean(W / 2 + 8, W - 8))
}

fn with(saturation: f32, vibrance: f32) -> EditState {
    let mut state = EditState::default();
    state.colour.saturation = saturation;
    state.colour.vibrance = vibrance;
    state
}

#[test]
#[ignore = "requires a GPU adapter"]
fn vibrance_spares_what_is_already_vivid_and_saturation_does_not() {
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let (flat, vivid) = saturation_halves(&gpu, &with(0.0, 0.0));
    assert!(vivid > flat * 2.0, "the fixture is not testing two cases");

    let (sat_flat, sat_vivid) = saturation_halves(&gpu, &with(0.6, 0.0));
    let (vib_flat, vib_vivid) = saturation_halves(&gpu, &with(0.0, 0.6));

    let lift = |after: f32, before: f32| after / before;
    println!(
        "saturation lift: flat {:.3}, vivid {:.3}   vibrance lift: flat {:.3}, vivid {:.3}",
        lift(sat_flat, flat),
        lift(sat_vivid, vivid),
        lift(vib_flat, flat),
        lift(vib_vivid, vivid),
    );

    // Saturation scales the distance from grey by one factor, so measured as a
    // distance from grey it lifts both halves by the same amount.
    let sat_gap = lift(sat_flat, flat) - lift(sat_vivid, vivid);
    assert!(
        sat_gap.abs() < 0.05,
        "saturation should treat both alike, and the lifts differ by {sat_gap:.3}"
    );

    // Vibrance does not, and that is the whole point of it.
    assert!(
        lift(vib_flat, flat) > lift(vib_vivid, vivid) + 0.15,
        "vibrance lifted the vivid half nearly as much as the flat one, so it is \
         only a weaker saturation"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn negative_vibrance_calms_the_vivid_and_leaves_the_flat_alone() {
    // The other direction, which is deliberately *not* Lightroom's: both ends of
    // this control move colours towards the middle of the range, so pulling it
    // down takes the vivid back and leaves a nearly-grey pixel where it was.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let (flat, vivid) = saturation_halves(&gpu, &with(0.0, 0.0));
    let (down_flat, down_vivid) = saturation_halves(&gpu, &with(0.0, -0.6));
    assert!(
        down_flat / flat > down_vivid / vivid + 0.15,
        "negative vibrance took as much from the flat half as the vivid one"
    );
    assert!(down_vivid < vivid, "it did not calm the vivid half at all");
}
