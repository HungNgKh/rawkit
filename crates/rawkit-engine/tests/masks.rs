//! Local adjustments: does an edit reach only where its mask says?
//!
//! The mask arrives at the renderer as a texture and nothing in the shader
//! knows what shape it is — see [`rawkit_engine::mask`] for why. So these are
//! the claims that matter at this boundary: the adjustment lands where the mask
//! is, it does not land where the mask is not, and a mask that asks for nothing
//! changes nothing at all.
//!
//! `cargo test -p rawkit-engine --test masks -- --ignored`

use rawkit_editstate::{EditState, Mask, MaskShape, Stroke};
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const W: u32 = 512;
const H: u32 = 512;

/// A flat grey frame, so anything that varies in the result came from a mask.
fn flat() -> Vec<f32> {
    vec![0.3f32; (W * H) as usize]
}

fn render(gpu: &Gpu, state: &EditState) -> Vec<f32> {
    let cfa = flat();
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

fn luma(pixels: &[f32], x: u32, y: u32) -> f32 {
    let i = ((y * W + x) * 4) as usize;
    0.2126 * pixels[i] + 0.7152 * pixels[i + 1] + 0.0722 * pixels[i + 2]
}

/// A gradient across the top third, running downwards.
fn across_the_top(exposure_ev: f32) -> Mask {
    Mask {
        shape: MaskShape::Linear {
            from: [0.5, 0.1],
            to: [0.5, 0.4],
        },
        exposure_ev,
        ..Mask::default()
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn an_adjustment_reaches_where_its_mask_is_and_no_further() {
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let plain = render(&gpu, &EditState::default());
    let edited = EditState {
        masks: vec![across_the_top(-2.0)],
        ..EditState::default()
    };
    let masked = render(&gpu, &edited);

    let top = (luma(&masked, W / 2, 10), luma(&plain, W / 2, 10));
    let bottom = (luma(&masked, W / 2, H - 10), luma(&plain, W / 2, H - 10));
    println!(
        "under the mask {:.4} against {:.4}; clear of it {:.4} against {:.4}",
        top.0, top.1, bottom.0, bottom.1
    );
    assert!(
        top.0 < top.1 * 0.75,
        "two stops down did not reach the mask: {:.4} against {:.4}",
        top.0,
        top.1
    );
    assert_eq!(
        bottom.0, bottom.1,
        "the adjustment reached past its mask, into a part of the frame it does not cover"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_falloff_is_smooth_and_monotone() {
    // A graduated filter with a step in it is a graduated filter nobody can
    // use. The mask is rasterised at a bounded resolution and upsampled, so
    // this is also the check that the upsampling does not staircase.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let edited = EditState {
        masks: vec![across_the_top(-2.0)],
        ..EditState::default()
    };
    let masked = render(&gpu, &edited);

    let column: Vec<f32> = (8..H - 8).map(|y| luma(&masked, W / 2, y)).collect();
    let steps: Vec<f32> = column.windows(2).map(|w| w[1] - w[0]).collect();
    let largest = steps.iter().cloned().fold(f32::MIN, f32::max);
    let smallest = steps.iter().cloned().fold(f32::MAX, f32::min);
    let span = column.last().unwrap() - column[0];
    println!("span {span:e}, brightest step {largest:e}, darkest {smallest:e}");
    // Measured against the span rather than against zero. A ramp of half a
    // million float operations does not come back exactly monotone, and the
    // question being asked is whether it *looks* monotone — a reversal worth
    // seeing would be a visible fraction of the change, not four parts in a
    // million of it.
    assert!(
        smallest >= -span * 1e-4,
        "the gradient runs backwards by {smallest:e} across a change of {span:e}"
    );
    // And every forward step is a small fraction of the whole, which is what
    // "no visible edge" means as a number.
    assert!(
        largest < span * 0.02,
        "a step of {largest:e} across a change of {span:e} is an edge"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_mask_that_asks_for_nothing_changes_nothing() {
    // Bit-identical, not nearly. Placing a gradient before touching a slider
    // must leave the photograph exactly as it was, or the act of *considering*
    // a local adjustment would alter the picture.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let plain = render(&gpu, &EditState::default());
    let placed = EditState {
        masks: vec![across_the_top(0.0)],
        ..EditState::default()
    };
    assert_eq!(render(&gpu, &placed), plain);
}

#[test]
#[ignore = "requires a GPU adapter"]
fn warmth_moves_colour_and_not_brightness() {
    // The local white balance is a gain on a neutral, so on a grey frame it must
    // move the channels apart without moving the luminance much. Checked
    // together because getting the matrix conjugation wrong shows up as one or
    // the other: a gain applied in the wrong space darkens, and a gain that is
    // secretly the identity does nothing.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let plain = render(&gpu, &EditState::default());
    let warm = EditState {
        masks: vec![Mask {
            warmth: 1.0,
            ..across_the_top(0.0)
        }],
        ..EditState::default()
    };
    let warmed = render(&gpu, &warm);

    let at = |pixels: &[f32], y: u32| {
        let i = ((y * W + W / 2) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };
    let before = at(&plain, 10);
    let after = at(&warmed, 10);
    println!("neutral {before:?} warmed to {after:?}");
    assert!(
        after[0] > before[0] * 1.02,
        "warming did not raise red: {before:?} -> {after:?}"
    );
    assert!(
        after[2] < before[2] * 0.98,
        "warming did not lower blue: {before:?} -> {after:?}"
    );
    let (a, b) = (luma(&plain, W / 2, 10), luma(&warmed, W / 2, 10));
    assert!(
        (b / a - 1.0).abs() < 0.15,
        "warming changed the brightness by {:.0}%, so it is not a white balance",
        100.0 * (b / a - 1.0)
    );
    assert_eq!(
        at(&warmed, H - 10),
        at(&plain, H - 10),
        "the warmth reached past its mask"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn two_masks_both_apply_where_they_overlap() {
    // Compositing, in the smallest form that can show it: two gradients from
    // opposite edges, each a stop down, meeting in the middle. Where both cover,
    // the result must be darker than either alone — the mask stack multiplies
    // rather than the last one winning.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let top = Mask {
        shape: MaskShape::Linear {
            from: [0.5, 0.0],
            to: [0.5, 1.0],
        },
        exposure_ev: -1.0,
        ..Mask::default()
    };
    let bottom = Mask {
        shape: MaskShape::Linear {
            from: [0.5, 1.0],
            to: [0.5, 0.0],
        },
        exposure_ev: -1.0,
        ..Mask::default()
    };

    let one = EditState {
        masks: vec![top.clone()],
        ..EditState::default()
    };
    let both = EditState {
        masks: vec![top, bottom],
        ..EditState::default()
    };

    let middle_one = luma(&render(&gpu, &one), W / 2, H / 2);
    let middle_both = luma(&render(&gpu, &both), W / 2, H / 2);
    println!("one mask {middle_one:.4}, two {middle_both:.4}");
    assert!(
        middle_both < middle_one * 0.95,
        "the second mask did nothing where the two overlap: {middle_one:.4} then {middle_both:.4}"
    );
}

/// An ellipse in the middle of the frame.
fn in_the_middle(exposure_ev: f32) -> Mask {
    Mask {
        shape: MaskShape::Radial {
            centre: [0.5, 0.5],
            radii: [0.2, 0.2],
            feather: 0.4,
        },
        exposure_ev,
        ..Mask::default()
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_radial_lifts_what_is_inside_it() {
    // The second mask source, and the whole return on building stage G around a
    // texture: this needed a rasteriser and nothing in the shader at all.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let plain = render(&gpu, &EditState::default());
    let lifted = render(
        &gpu,
        &EditState {
            masks: vec![in_the_middle(1.0)],
            ..EditState::default()
        },
    );

    let middle = (luma(&lifted, W / 2, H / 2), luma(&plain, W / 2, H / 2));
    println!("middle {:.4} against {:.4}", middle.0, middle.1);
    assert!(
        middle.0 > middle.1 * 1.3,
        "a stop up did not reach the middle: {:.4} against {:.4}",
        middle.0,
        middle.1
    );
    for corner in [(6, 6), (W - 6, 6), (6, H - 6), (W - 6, H - 6)] {
        assert_eq!(
            luma(&lifted, corner.0, corner.1),
            luma(&plain, corner.0, corner.1),
            "the ellipse reached the corner at {corner:?}"
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn inverting_a_radial_makes_it_a_vignette() {
    // Where the plain one lifts, the inverted one must not, and the other way
    // round — checked as a *complement* rather than as two separate facts,
    // because a mask that changed both would pass two looser assertions.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let plain = render(&gpu, &EditState::default());
    let spotlight = render(
        &gpu,
        &EditState {
            masks: vec![in_the_middle(1.0)],
            ..EditState::default()
        },
    );
    let vignette = render(
        &gpu,
        &EditState {
            masks: vec![Mask {
                invert: true,
                ..in_the_middle(1.0)
            }],
            ..EditState::default()
        },
    );

    let at = |p: &[f32], x, y| luma(p, x, y);
    println!(
        "middle: plain {:.4} spotlight {:.4} vignette {:.4}\n\
         corner: plain {:.4} spotlight {:.4} vignette {:.4}",
        at(&plain, W / 2, H / 2),
        at(&spotlight, W / 2, H / 2),
        at(&vignette, W / 2, H / 2),
        at(&plain, 6, 6),
        at(&spotlight, 6, 6),
        at(&vignette, 6, 6)
    );
    assert_eq!(
        at(&vignette, W / 2, H / 2),
        at(&plain, W / 2, H / 2),
        "the inverted ellipse still reaches its own middle"
    );
    assert!(
        at(&vignette, 6, 6) > at(&plain, 6, 6) * 1.3,
        "the inverted ellipse did not reach the corner"
    );
    assert_eq!(
        at(&spotlight, 6, 6),
        at(&plain, 6, 6),
        "the plain ellipse reached the corner"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_painted_stroke_darkens_only_what_it_covers() {
    // The third mask source, and again nothing in the shader knew about it. A
    // horizontal stroke across the middle: the row it covers comes down, the
    // rows above and below are untouched.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let plain = render(&gpu, &EditState::default());
    let painted = render(
        &gpu,
        &EditState {
            masks: vec![Mask {
                shape: MaskShape::Brush {
                    strokes: vec![Stroke {
                        points: vec![[0.15, 0.5], [0.85, 0.5]],
                        radius: 0.05,
                        erase: false,
                    }],
                    feather: 0.4,
                },
                exposure_ev: -2.0,
                ..Mask::default()
            }],
            ..EditState::default()
        },
    );

    let middle = (luma(&painted, W / 2, H / 2), luma(&plain, W / 2, H / 2));
    println!("under the stroke {:.4} against {:.4}", middle.0, middle.1);
    assert!(
        middle.0 < middle.1 * 0.75,
        "the stroke did not darken what it covers: {:.4} against {:.4}",
        middle.0,
        middle.1
    );
    for y in [8, H - 8] {
        assert_eq!(
            luma(&painted, W / 2, y),
            luma(&plain, W / 2, y),
            "the stroke reached row {y}, which it does not cover"
        );
    }
    // And nothing beyond its ends, which is what says the capsule stops rather
    // than the whole row being painted.
    assert_eq!(
        luma(&painted, 4, H / 2),
        luma(&plain, 4, H / 2),
        "the stroke ran off its own end"
    );
}
