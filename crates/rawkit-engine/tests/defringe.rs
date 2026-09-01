//! Does the defringe take off a cast the pipeline invented, and leave alone one
//! the photograph actually has?
//!
//! The whole difficulty of testing this is telling those two apart, and a real
//! photograph cannot: the branch really is brown, so a measurement there cannot
//! say how much of the violet was the lens, the leaf or the rendering. So the
//! scene here is **neutral everywhere in the truth** — one grey sky, one grey
//! line down it — and any colour that comes out the far end was manufactured
//! between the two.
//!
//! What manufactures it: a sensor clips at one value in its own space, white
//! balance moves that to a different height per channel, and a pixel that is
//! *part* clipped sky and part honest dark carries the resulting magenta scaled
//! down — below the level at which highlight reconstruction will look at it.
//!
//! GPU-gated like the rest: `cargo test -- --ignored`.

use rawkit_editstate::EditState;
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const W: u32 = 512;
const H: u32 = 256;
/// The as-shot multipliers of the ILCE-6400 frame this was diagnosed on. Real
/// numbers rather than round ones, because the artefact *is* their inequality:
/// with (1, 1, 1) there is no cast to make and nothing here would fire.
const WB: [f32; 3] = [2.632_812_5, 1.0, 1.820_312_5];
const LINE: u32 = 256;

fn colour_at(x: u32, y: u32) -> usize {
    if (x + y) % 2 == 1 {
        1
    } else if y % 2 == 0 {
        0
    } else {
        2
    }
}

/// A neutral sky `over` times the clip level, with a dark neutral line down it.
///
/// Neutral in *balanced* terms — each channel divided by its multiplier before
/// the sensor sees it — so the truth has no colour in it anywhere. The line's
/// edge is soft rather than a step, because a mixture is the whole subject and a
/// step no lens produces would only be tested at one pixel.
fn cover_at(x: u32) -> f32 {
    let d = (x as f32 - LINE as f32).abs();
    // One pixel of ramp, and that sharpness is the point rather than a
    // convenience. A softer edge was tried and it made the artefact nearly
    // vanish — which is the mechanism confirming itself: where the scene ramps
    // over several pixels the sensor records honest intermediate values and
    // nothing is a mixture of clipped light with unclipped. The rim exists
    // precisely where an edge is sharp against the pixel grid, so the demosaic
    // has to average a saturated photosite with a dark one. Bare twigs against
    // the sky are exactly that, which is where this was reported.
    (1.0 - (d - 1.5).clamp(0.0, 1.0)).clamp(0.0, 1.0)
}

fn mosaic(over: f32) -> Vec<f32> {
    let mut out = vec![0.0f32; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            let scene = over * (1.0 - 0.85 * cover_at(x));
            out[(y * W + x) as usize] = (scene / WB[colour_at(x, y)]).min(1.0);
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
        as_shot_wb: WB,
        clip_level: 1.0,
        profile: CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY),
        recorded_orientation: rawkit_editstate::Orientation::AsShot,
    }
}

/// Sharpening and noise reduction off, so what is measured is this stage and not
/// what the two neighbours of it did to its output.
fn bare(defringe: f32) -> EditState {
    EditState {
        detail: rawkit_editstate::Detail {
            chroma_noise: 0.0,
            luminance_noise: 0.0,
            sharpen_amount: 0.0,
            defringe,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// The worst departure from neutral across the line, as the spread between
/// channels over their mean — a number that is zero for any grey, however
/// bright, and so measures only what should not be there.
fn worst_cast(pixels: &[f32]) -> (f32, usize, [f32; 3]) {
    let y = (H / 2) as usize;
    let mut worst = (0.0f32, 0usize, [0.0f32; 3]);
    for x in (LINE as usize - 14)..(LINE as usize + 14) {
        let p = (y * W as usize + x) * 4;
        let rgb = [pixels[p], pixels[p + 1], pixels[p + 2]];
        let mean = (rgb[0] + rgb[1] + rgb[2]) / 3.0;
        if mean <= 1e-6 {
            continue;
        }
        let cast = (rgb[0].max(rgb[1]).max(rgb[2]) - rgb[0].min(rgb[1]).min(rgb[2])) / mean;
        if cast > worst.0 {
            worst = (cast, x, rgb);
        }
    }
    worst
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_rim_on_a_blown_edge_comes_off() {
    let Ok(gpu) = Gpu::new() else { return };
    let renderer = Renderer::new(&gpu);

    // Past every threshold, so every channel is clipped and the sky's recorded
    // value *is* `wb * clip`. Below `max(wb)` only some channels clip, and what
    // dominates there is a different defect — see `cover_at`, and the note on
    // partial clipping in the module documentation.
    for over in [2.8f32, 4.0] {
        let cfa = mosaic(over);
        let off = renderer
            .run(&gpu, &frame(&cfa), &bare(0.0), Output::Display)
            .expect("render");
        let on = renderer
            .run(&gpu, &frame(&cfa), &bare(1.0), Output::Display)
            .expect("render");
        let (before, bx, brgb) = worst_cast(&off.pixels);
        let (after, ax, argb) = worst_cast(&on.pixels);
        println!(
            "sky {over:.1}x clip: {before:.3} at x={bx} [{:.3} {:.3} {:.3}] -> {after:.3} at x={ax} [{:.3} {:.3} {:.3}]",
            brgb[0], brgb[1], brgb[2], argb[0], argb[1], argb[2]
        );
        assert!(
            before > 0.3,
            "the artefact under test is not present: {before}"
        );
        // Measured: 1.193 -> 0.296 at 2.8 times the clip level, 1.922 -> 0.398
        // at four. The bound is a little looser than either so that an
        // improvement is what fails it, not a rounding difference — and it is
        // stated as a fraction rather than an absolute because the artefact
        // itself grows with how far over the clip the sky is.
        assert!(
            after < before * 0.40,
            "sky at {over}x clip: cast went {before} -> {after}, and that is not a repair"
        );
        // Not merely smaller: it must not have crossed through neutral and come
        // out green on the other side, which is the failure mode a correction
        // that guessed its own step size would have.
        assert!(
            argb[1] <= argb[0].max(argb[2]) + 0.02,
            "overshot into green: {argb:?}"
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_sky_below_the_clip_point_is_untouched() {
    // Nothing clipped, so nothing was displaced and there is nothing to undo.
    // This is the half that makes the other half meaningful: a stage that
    // desaturated every dark edge would pass the test above and ruin every
    // photograph.
    //
    // 0.6 rather than something nearer the clip point, and the first attempt at
    // 0.95 is *why*: `CLIP_RUNUP` is a quarter of the range, so a sky at 0.95 of
    // the threshold is already inside the run-up and already being treated as a
    // highlight — by reconstruction as much as by this. A control has to be
    // outside the band it is controlling for, and at 0.6 nothing in the frame is
    // blown at all, `near` is zero everywhere, and the stage returns before it
    // touches a pixel.
    let Ok(gpu) = Gpu::new() else { return };
    let renderer = Renderer::new(&gpu);
    let cfa = mosaic(0.6);
    let off = renderer
        .run(&gpu, &frame(&cfa), &bare(0.0), Output::Display)
        .expect("render");
    let on = renderer
        .run(&gpu, &frame(&cfa), &bare(1.0), Output::Display)
        .expect("render");
    // The interior, because the outermost few pixels of any frame are wrong by
    // construction: RCD reaches four pixels out and clamps at the edge rather
    // than mirroring, so the demosaic can overshoot there and hand this stage a
    // value that looks blown when the scene is not. That is documented on
    // `Renderer::run` and is not what this test is about.
    let mut worst = 0.0f32;
    for y in 8..(H as usize - 8) {
        for x in 8..(W as usize - 8) {
            for c in 0..3 {
                let p = (y * W as usize + x) * 4 + c;
                worst = worst.max((off.pixels[p] - on.pixels[p]).abs());
            }
        }
    }
    assert_eq!(
        worst, 0.0,
        "changed a frame with nothing clipped, by {worst:e}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn zero_changes_nothing_at_all() {
    // Exactly, not nearly: a stored edit has to be able to turn this off, and a
    // stage that returned *almost* the same pixels would make every golden
    // reference depend on whether it ran.
    let Ok(gpu) = Gpu::new() else { return };
    let renderer = Renderer::new(&gpu);
    let cfa = mosaic(3.0);
    let a = renderer
        .run(&gpu, &frame(&cfa), &bare(0.0), Output::Display)
        .expect("render");
    let b = renderer
        .run(&gpu, &frame(&cfa), &bare(1e-9), Output::Display)
        .expect("render");
    let worst = a
        .pixels
        .iter()
        .zip(&b.pixels)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f32, f32::max);
    // A vanishing amount is not the same as none: this pins the early return,
    // not floating-point luck, so the two differ by whatever `1e-9` of the
    // correction is and no more.
    assert!(
        worst < 1e-5,
        "a vanishing amount moved a pixel by {worst:e}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_colour_the_photograph_really_has_survives() {
    // The guard that decides whether this is a repair or a bleach. A strongly
    // coloured subject beside a blown highlight — a red roof against a white
    // sky — has a colour that is *its own*, and no amount of defringe may take
    // it. The correction only moves along the axis clipping displaced the light
    // on, which for these multipliers is very nearly green against magenta, so a
    // red subject should come through nearly untouched.
    let Ok(gpu) = Gpu::new() else { return };
    let renderer = Renderer::new(&gpu);

    // The same geometry, but the line is a saturated red object rather than a
    // neutral one: it reflects almost no green or blue.
    let mut cfa = mosaic(2.8);
    for y in 0..H {
        for x in 0..W {
            let cover = cover_at(x);
            if cover <= 0.0 {
                continue;
            }
            let c = colour_at(x, y);
            let here = &mut cfa[(y * W + x) as usize];
            // Red keeps its brightness, green and blue lose nearly all of it,
            // in proportion to how much of the pixel the object covers.
            let subject = [0.45f32, 0.02, 0.02][c] / WB[c];
            *here = *here * (1.0 - cover) + subject * cover;
        }
    }

    let off = renderer
        .run(&gpu, &frame(&cfa), &bare(0.0), Output::Display)
        .expect("render");
    let on = renderer
        .run(&gpu, &frame(&cfa), &bare(1.0), Output::Display)
        .expect("render");

    // At the middle of the object, where it is entirely itself.
    let p = ((H / 2) as usize * W as usize + LINE as usize) * 4;
    let before = [off.pixels[p], off.pixels[p + 1], off.pixels[p + 2]];
    let after = [on.pixels[p], on.pixels[p + 1], on.pixels[p + 2]];
    let chroma = |c: [f32; 3]| {
        let mean = (c[0] + c[1] + c[2]) / 3.0;
        (c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])) / mean.max(1e-6)
    };
    println!(
        "red subject: [{:.3} {:.3} {:.3}] (chroma {:.3}) -> [{:.3} {:.3} {:.3}] (chroma {:.3})",
        before[0],
        before[1],
        before[2],
        chroma(before),
        after[0],
        after[1],
        after[2],
        chroma(after)
    );
    assert!(
        chroma(after) > chroma(before) * 0.85,
        "took {:.0}% of a colour the subject actually has",
        (1.0 - chroma(after) / chroma(before)) * 100.0
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_magenta_subject_is_the_hard_case_and_is_stated() {
    // The previous test is the easy half. The correction moves along the axis
    // clipping displaced the light on, and for real multipliers that axis is
    // very nearly green against magenta — so a red subject is nearly orthogonal
    // to it and survives almost untouched. A *magenta* one is not: it lies along
    // the axis, and there is no measurement anywhere in the frame that can tell
    // a magenta object beside a blown sky from a rim that the sky invented.
    //
    // So this pins what the trade actually costs rather than pretending there is
    // none. What bounds it is the reach — nothing further than a few pixels from
    // clipped light is touched at all — and the slider, which is the user's to
    // move when a photograph is the exception.
    let Ok(gpu) = Gpu::new() else { return };
    let renderer = Renderer::new(&gpu);

    let mut cfa = mosaic(2.8);
    for y in 0..H {
        for x in 0..W {
            let cover = cover_at(x);
            if cover <= 0.0 {
                continue;
            }
            let c = colour_at(x, y);
            let here = &mut cfa[(y * W + x) as usize];
            // Red and blue up, green down: magenta, and squarely on the axis.
            let subject = [0.40f32, 0.06, 0.30][c] / WB[c];
            *here = *here * (1.0 - cover) + subject * cover;
        }
    }

    let off = renderer
        .run(&gpu, &frame(&cfa), &bare(0.0), Output::Display)
        .expect("render");
    let on = renderer
        .run(&gpu, &frame(&cfa), &bare(1.0), Output::Display)
        .expect("render");
    let p = ((H / 2) as usize * W as usize + LINE as usize) * 4;
    let before = [off.pixels[p], off.pixels[p + 1], off.pixels[p + 2]];
    let after = [on.pixels[p], on.pixels[p + 1], on.pixels[p + 2]];
    let chroma = |c: [f32; 3]| {
        let mean = (c[0] + c[1] + c[2]) / 3.0;
        (c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])) / mean.max(1e-6)
    };
    println!(
        "magenta subject: [{:.3} {:.3} {:.3}] (chroma {:.3}) -> [{:.3} {:.3} {:.3}] (chroma {:.3}), kept {:.0}%",
        before[0], before[1], before[2], chroma(before),
        after[0], after[1], after[2], chroma(after),
        chroma(after) / chroma(before) * 100.0
    );
    // The middle of a subject wider than the reach keeps its colour, because
    // nothing blown is within reach of it. Only its edge pays, which is the
    // whole bargain: this is a rim repair and its cost is confined to rims.
    assert!(
        chroma(after) > chroma(before) * 0.7,
        "took {:.0}% of a magenta subject, which is past a repair and into a bleach",
        (1.0 - chroma(after) / chroma(before)) * 100.0
    );
}
