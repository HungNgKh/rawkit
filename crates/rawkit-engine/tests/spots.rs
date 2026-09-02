//! Spot removal, through the whole renderer.
//!
//! The unit tests beside [`rawkit_engine::spot`] check the patch itself. These
//! check the two things that can only go wrong once it is wired in: that a
//! repair made in the mosaic survives the demosaic and the develop chain, and
//! that it does not come apart where the renderer changes tile.
//!
//! `cargo test -p rawkit-engine --test spots -- --ignored`

use rawkit_editstate::{EditState, Spot, SpotMode};
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const W: u32 = 1024;
const H: u32 = 512;

/// A frame with something in it for the demosaic to do.
///
/// Flat data would let a broken repair pass: with nothing varying, any patch
/// looks like its surroundings. This has a slow diagonal ramp and a fine ripple,
/// so a disc taken from the wrong place is visible and a disc taken from the
/// wrong photosite is glaring.
fn textured() -> Vec<f32> {
    let mut m = vec![0f32; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            let ramp = 0.25 + 0.3 * (x as f32 / W as f32) + 0.2 * (y as f32 / H as f32);
            let ripple = 0.03 * ((x as f32 * 0.31).sin() + (y as f32 * 0.27).sin());
            // A green-heavy mosaic, as a real one is.
            let cfa = match ((y & 1) * 2 + (x & 1)) as u8 {
                0 => 0.9,
                3 => 0.7,
                _ => 1.0,
            };
            m[(y * W + x) as usize] = ((ramp + ripple) * cfa).clamp(0.0, 1.0);
        }
    }
    m
}

/// Multiply a disc by `by`, the way a speck of dust on the sensor does.
fn blemish(mosaic: &mut [f32], centre: [f32; 2], radius: f32, by: f32) {
    let (cx, cy) = (centre[0] * W as f32, centre[1] * H as f32);
    for y in 0..H {
        for x in 0..W {
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            if (dx * dx + dy * dy).sqrt() <= radius {
                mosaic[(y * W + x) as usize] *= by;
            }
        }
    }
}

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

/// Mean absolute luma difference over a disc.
fn residual(a: &[f32], b: &[f32], centre: [f32; 2], radius: f32) -> f32 {
    let (cx, cy) = (centre[0] * W as f32, centre[1] * H as f32);
    let mut sum = 0.0;
    let mut n = 0u32;
    for y in 0..H {
        for x in 0..W {
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            if (dx * dx + dy * dy).sqrt() <= radius {
                sum += (luma(a, x, y) - luma(b, x, y)).abs();
                n += 1;
            }
        }
    }
    sum / n as f32
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_blemish_is_gone_from_the_rendered_frame() {
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let clean = textured();
    let mut dusty = clean.clone();
    let centre = [0.3, 0.5];
    blemish(&mut dusty, centre, 14.0, 0.55);

    let state = EditState {
        spots: vec![Spot {
            centre,
            source: [0.3, 0.75],
            radius: 18.0 / W as f32,
            feather: 0.3,
            mode: SpotMode::Heal,
        }],
        ..EditState::default()
    };

    let before = render(&gpu, &dusty, &EditState::default());
    let after = render(&gpu, &dusty, &state);
    let reference = render(&gpu, &clean, &EditState::default());

    let was = residual(&before, &reference, centre, 12.0);
    let now = residual(&after, &reference, centre, 12.0);
    assert!(
        was > 0.05,
        "the synthetic blemish only moved the render by {was}, so this proves nothing"
    );
    assert!(
        now < was * 0.25,
        "the mark is still {now} against a reference it was {was} from"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_spot_across_a_tile_boundary_has_no_seam() {
    // The renderer draws in 512-wide tiles, so a spot sitting on x = 512 is
    // patched by two separate gathers reading a source that is nowhere near
    // either tile. If the two halves disagreed — a different correction, an
    // offset snapped differently — the join would show as a vertical step.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let mut dusty = textured();
    let centre = [512.0 / W as f32, 0.5];
    blemish(&mut dusty, centre, 12.0, 0.5);

    let state = EditState {
        spots: vec![Spot {
            centre,
            source: [0.75, 0.5],
            radius: 16.0 / W as f32,
            feather: 0.2,
            mode: SpotMode::Heal,
        }],
        ..EditState::default()
    };
    let patched = render(&gpu, &dusty, &state);
    let reference = render(&gpu, &textured(), &EditState::default());

    // The step across the join, against the step the untouched frame has there
    // anyway. A repair that came apart shows up as the first being far larger.
    let step = |px: &[f32]| {
        let y0 = (0.5 * H as f32) as u32;
        (y0 - 6..y0 + 6)
            .map(|y| (luma(px, 512, y) - luma(px, 511, y)).abs())
            .fold(0.0f32, f32::max)
    };
    let seam = step(&patched);
    let natural = step(&reference);
    assert!(
        seam < natural + 0.01,
        "the tile join steps by {seam} where the frame itself steps by {natural}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn everything_outside_the_spot_is_untouched() {
    // Bit for bit, and that is the point: a repair that quietly perturbed the
    // rest of the frame would be invisible here and infuriating in a print.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let mosaic = textured();
    let centre = [0.3, 0.5];
    let state = EditState {
        spots: vec![Spot {
            centre,
            source: [0.6, 0.5],
            radius: 20.0 / W as f32,
            feather: 0.4,
            mode: SpotMode::Heal,
        }],
        ..EditState::default()
    };
    let plain = render(&gpu, &mosaic, &EditState::default());
    let patched = render(&gpu, &mosaic, &state);

    // Outside the spot and outside the demosaic's reach around it. Inside that
    // margin the repair legitimately changes neighbours it was interpolated
    // from, which is the whole reason it happens before the demosaic.
    let (cx, cy) = (centre[0] * W as f32, centre[1] * H as f32);
    let mut differing = 0u32;
    for y in 0..H {
        for x in 0..W {
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            if (dx * dx + dy * dy).sqrt() <= 32.0 {
                continue;
            }
            let i = ((y * W + x) * 4) as usize;
            if plain[i..i + 3] != patched[i..i + 3] {
                differing += 1;
            }
        }
    }
    assert_eq!(differing, 0, "{differing} pixels changed outside the spot");
}
