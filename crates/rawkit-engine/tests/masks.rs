//! Local adjustments: does an edit reach only where its mask says?
//!
//! The mask arrives at the renderer as a texture and nothing in the shader
//! knows what shape it is — see [`rawkit_engine::mask`] for why. So these are
//! the claims that matter at this boundary: the adjustment lands where the mask
//! is, it does not land where the mask is not, and a mask that asks for nothing
//! changes nothing at all.
//!
//! `cargo test -p rawkit-engine --test masks -- --ignored`

use rawkit_editstate::{EditState, Mask, MaskShape, RangeChannel, Stroke};
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const W: u32 = 512;
const H: u32 = 512;

/// A flat grey frame, so anything that varies in the result came from a mask.
fn flat() -> Vec<f32> {
    vec![0.3f32; (W * H) as usize]
}

fn render(gpu: &Gpu, state: &EditState) -> Vec<f32> {
    render_frame(gpu, state, &flat())
}

fn render_frame(gpu: &Gpu, state: &EditState, cfa: &[f32]) -> Vec<f32> {
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
            angle_deg: 0.0,
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

/// Dark on the left, bright on the right, with nothing else to tell them apart.
fn split() -> Vec<f32> {
    (0..W * H)
        .map(|i| if (i % W) < W / 2 { 0.15f32 } else { 0.6 })
        .collect()
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_range_mask_reaches_the_renderer_at_all() {
    // The first source that is not geometry. Everything downstream was built to
    // take it without being told -- the shader composites a texture and cannot
    // ask what drew it -- so the claim worth checking end to end is the boring
    // one: a band on brightness lands on the half of the picture that is that
    // bright, and nowhere else.
    //
    // A flat frame cannot test this. It has one brightness, so a range over it
    // selects all of the picture or none of it, and both would pass a test that
    // only asked whether *something* changed.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let cfa = split();

    let plain = render_frame(&gpu, &EditState::default(), &cfa);
    let lifted = EditState {
        masks: vec![Mask {
            shape: MaskShape::Range {
                channel: RangeChannel::Luminance,
                from: 0.4,
                to: 1.0,
                feather: 0.02,
            },
            exposure_ev: 1.0,
            ..Mask::default()
        }],
        ..EditState::default()
    };
    let masked = render_frame(&gpu, &lifted, &cfa);

    let dark = (luma(&plain, W / 4, H / 2), luma(&masked, W / 4, H / 2));
    let bright = (
        luma(&plain, 3 * W / 4, H / 2),
        luma(&masked, 3 * W / 4, H / 2),
    );
    println!(
        "dark half {:.4} -> {:.4}, bright half {:.4} -> {:.4}",
        dark.0, dark.1, bright.0, bright.1
    );
    assert!(
        (dark.1 - dark.0).abs() < 0.002,
        "a band on the bright half moved the dark half: {:.4} to {:.4}",
        dark.0,
        dark.1
    );
    assert!(
        bright.1 > bright.0 * 1.2,
        "a band on the bright half did not lift it: {:.4} to {:.4}",
        bright.0,
        bright.1
    );
}

/// The same gradient across the top, carrying one display-referred control.
fn top_with(f: impl Fn(&mut Mask)) -> Mask {
    let mut mask = Mask {
        shape: MaskShape::Linear {
            from: [0.5, 0.1],
            to: [0.5, 0.4],
        },
        ..Mask::default()
    };
    f(&mut mask);
    mask
}

fn saturation(pixels: &[f32], x: u32, y: u32) -> f32 {
    let i = ((y * W + x) * 4) as usize;
    let (r, g, b) = (pixels[i], pixels[i + 1], pixels[i + 2]);
    let high = r.max(g).max(b);
    let low = r.min(g).min(b);
    (high - low) / high.max(1e-6)
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_display_referred_half_reaches_only_where_the_mask_is() {
    // Contrast and saturation are applied on the far side of the tone map,
    // which is a second place the mask has to be read. The claim is the same
    // one the first half already makes and has to be made again here, because
    // "the mask is carried across the boundary" is exactly the sort of thing
    // that can be half-true: the top of the frame moves and the bottom does
    // not.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let plain = render(&gpu, &EditState::default());

    let darkened = EditState {
        masks: vec![top_with(|m| m.contrast = 0.8)],
        ..EditState::default()
    };
    let out = render(&gpu, &darkened);
    let inside = (luma(&plain, W / 2, 8), luma(&out, W / 2, 8));
    let outside = (luma(&plain, W / 2, H - 8), luma(&out, W / 2, H - 8));
    println!(
        "contrast: inside {:.4} -> {:.4}, outside {:.4} -> {:.4}",
        inside.0, inside.1, outside.0, outside.1
    );
    assert!(
        (outside.1 - outside.0).abs() < 0.002,
        "local contrast reached outside its mask: {:.4} to {:.4}",
        outside.0,
        outside.1
    );
    // And it is *contrast*, not brightness -- which means it has to agree with
    // the global control about where the middle is. This frame renders above
    // middle grey, so both must brighten it; on a darker one both must darken
    // it. Asserting a fixed direction instead is what let the two disagree: the
    // local control pivoted on a *linear* 0.46 where the global one pivots on
    // the encoded value of the same number, and a test that only asked whether
    // something moved was happy either way.
    let globally = render(
        &gpu,
        &EditState {
            tone: rawkit_editstate::Tone {
                contrast: 0.8,
                ..Default::default()
            },
            ..EditState::default()
        },
    );
    let global = luma(&globally, W / 2, 8);
    println!(
        "global contrast on the same frame: {:.4} -> {:.4}",
        inside.0, global
    );
    assert!(
        (global - inside.0).abs() > 0.01,
        "the global control did not move this frame, so there is nothing to agree with"
    );
    assert!(
        (inside.1 - inside.0).signum() == (global - inside.0).signum()
            && (inside.1 - inside.0).abs() > 0.01,
        "local contrast moved {:.4} to {:.4} where the global control moved it to {:.4}",
        inside.0,
        inside.1,
        global
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn local_saturation_moves_colour_and_not_brightness() {
    // The same claim the global control is held to, and the reason saturation
    // is measured as distance from grey rather than as a channel ratio: a
    // control that greyed a colour out by darkening it would pass a naive test
    // and look wrong on a photograph.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    // A frame with colour in it, so there is something to take away.
    let cfa: Vec<f32> = (0..W * H)
        .map(|i| {
            let (x, y) = (i % W, i / W);
            // Rggb, and red twice what the rest is: a warm flat field.
            if x % 2 == 0 && y % 2 == 0 {
                0.6
            } else {
                0.3
            }
        })
        .collect();
    let plain = render_frame(&gpu, &EditState::default(), &cfa);
    let greyed = EditState {
        masks: vec![top_with(|m| m.saturation = -1.0)],
        ..EditState::default()
    };
    let out = render_frame(&gpu, &greyed, &cfa);

    let inside = (
        saturation(&plain, W / 2, 8),
        saturation(&out, W / 2, 8),
        luma(&plain, W / 2, 8),
        luma(&out, W / 2, 8),
    );
    let outside = (
        saturation(&plain, W / 2, H - 8),
        saturation(&out, W / 2, H - 8),
    );
    println!(
        "saturation inside {:.4} -> {:.4} at luma {:.4} -> {:.4}; outside {:.4} -> {:.4}",
        inside.0, inside.1, inside.2, inside.3, outside.0, outside.1
    );
    assert!(
        inside.1 < inside.0 * 0.2,
        "local saturation did not take the colour out: {:.4} to {:.4}",
        inside.0,
        inside.1
    );
    assert!(
        (inside.3 - inside.2).abs() < 0.01,
        "taking the colour out changed the brightness: {:.4} to {:.4}",
        inside.2,
        inside.3
    );
    assert!(
        (outside.1 - outside.0).abs() < 0.002,
        "local saturation reached outside its mask: {:.4} to {:.4}",
        outside.0,
        outside.1
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_mask_that_asks_for_nothing_new_still_changes_nothing() {
    // The three new controls all default to zero, so a mask carrying only the
    // old ones must render exactly as it did. This is the test that would have
    // caught a uniform written into the wrong slot -- the two `Params` structs
    // have to agree field for field, and nothing else here would notice.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let before = render(&gpu, &EditState::default());
    let quiet = EditState {
        masks: vec![top_with(|_| {})],
        ..EditState::default()
    };
    let after = render(&gpu, &quiet);
    for (i, (a, b)) in before.iter().zip(&after).enumerate() {
        assert!(
            (a - b).abs() < 1e-5,
            "a mask asking for nothing moved pixel {i}: {a} against {b}"
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn local_clarity_leaves_a_pixel_that_matches_its_surroundings_alone() {
    // The definition of the control, and the one property that separates it from
    // contrast: clarity is contrast against the *neighbourhood*, so on a frame
    // with no local variation there is nothing for it to find and it must do
    // nothing at all. Anything else means the pivot is in a different coordinate
    // from the value being pivoted, which on a real photograph reads as clarity
    // darkening the whole picture.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let plain = render(&gpu, &EditState::default());
    for amount in [1.0f32, -1.0] {
        let edited = EditState {
            masks: vec![Mask {
                shape: MaskShape::Radial {
                    centre: [0.5, 0.5],
                    radii: [0.9, 0.9],
                    feather: 0.0,
                    angle_deg: 0.0,
                },
                clarity: amount,
                ..Mask::default()
            }],
            ..EditState::default()
        };
        let clarified = render(&gpu, &edited);
        let (was, now) = (luma(&plain, W / 2, H / 2), luma(&clarified, W / 2, H / 2));
        assert!(
            (now - was).abs() < 1e-3,
            "clarity of {amount} moved a flat frame from {was} to {now}"
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn an_adjustment_stops_at_the_edge_of_a_hard_mask() {
    // Reported as the effect "flowing outside the mask". The existing test above
    // samples one pixel deep inside and one far outside, which cannot see a
    // spill of a few pixels at the boundary — so this walks outward from the
    // rim and reports how far the adjustment actually reaches.
    //
    // A hard-edged radial, so anything past its rim is spill rather than
    // feather, and a large exposure so a small leak is still visible.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let plain = render(&gpu, &EditState::default());
    let radius = 0.25f32;
    println!(
        "a {W}-wide frame rasterises its masks at {:?}",
        rawkit_engine::mask::dimensions(W, H)
    );
    let edited = EditState {
        masks: vec![Mask {
            shape: MaskShape::Radial {
                centre: [0.5, 0.5],
                radii: [radius, radius],
                feather: 0.0,
                angle_deg: 0.0,
            },
            exposure_ev: 3.0,
            ..Mask::default()
        }],
        ..EditState::default()
    };
    let masked = render(&gpu, &edited);

    let inside = luma(&masked, W / 2, H / 2) - luma(&plain, W / 2, H / 2);
    assert!(inside > 0.05, "the mask did nothing inside: {inside}");

    // Along the horizontal radius, outward from the rim, in pixels.
    let rim = (radius * W as f32) as u32;
    let mut reach = 0i32;
    let mut profile = Vec::new();
    for step in 0..40u32 {
        let x = W / 2 + rim + step;
        if x >= W {
            break;
        }
        let lifted = (luma(&masked, x, H / 2) - luma(&plain, x, H / 2)) / inside;
        if step % 4 == 0 {
            profile.push(format!("{step}:{lifted:.3}"));
        }
        // A hundredth of the effect is the threshold for "still doing something".
        if lifted > 0.01 {
            reach = step as i32 + 1;
        }
    }
    println!("past the rim: {}", profile.join("  "));
    println!("the adjustment reaches {reach} px past a {W}-wide frame's mask edge");
    assert!(
        reach <= 8,
        "the adjustment reaches {reach} px outside a hard mask: {}",
        profile.join("  ")
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_effect_is_half_on_where_the_border_is_drawn() {
    // The claim the border is now built to make, and the reason it is derived
    // from the mask's own coverage rather than drawn from the ellipse's
    // parameters.
    //
    // The raster is capped at 1024 on its longest edge, so on a 24 megapixel
    // frame one texel is about six image pixels and a mask set to no feather
    // still fades across that. An outline drawn at the exact ellipse therefore
    // sat *inside* where the adjustment stopped, and the effect visibly ran past
    // its own border. Drawing the border at the half-coverage contour instead
    // makes "half the effect inside, half outside" true by construction, at any
    // raster resolution.
    //
    // Measured here on the coverage the renderer samples, which is the same
    // texture the overlay pass reads.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let plain = render(&gpu, &EditState::default());
    let radius = 0.25f32;
    let edited = EditState {
        masks: vec![Mask {
            shape: MaskShape::Radial {
                centre: [0.5, 0.5],
                radii: [radius, radius],
                feather: 0.0,
                angle_deg: 0.0,
            },
            exposure_ev: 3.0,
            ..Mask::default()
        }],
        ..EditState::default()
    };
    let masked = render(&gpu, &edited);
    let full = luma(&masked, W / 2, H / 2) - luma(&plain, W / 2, H / 2);
    assert!(full > 0.05, "the mask did nothing inside: {full}");

    // Walk the horizontal radius and find where the effect passes half.
    let mut half_at = None;
    for x in W / 2..W {
        let lifted = (luma(&masked, x, H / 2) - luma(&plain, x, H / 2)) / full;
        if lifted < 0.5 {
            half_at = Some(x - W / 2);
            break;
        }
    }
    let half_at = half_at.expect("the effect never fell below half");
    let rim = (radius * W as f32) as u32;
    println!("the effect is half on at {half_at} px; the ellipse's rim is at {rim} px");
    // The two must agree to within a texel of the raster, which on this frame is
    // one pixel. A border drawn from the parameters would be at `rim`; a border
    // drawn from the coverage is at `half_at`. They are the same place.
    let texel = (W / rawkit_engine::mask::dimensions(W, H).0).max(1);
    assert!(
        half_at.abs_diff(rim) <= texel + 1,
        "the effect is half on at {half_at} px where the shape's rim is {rim}"
    );
}
