//! Do highlights and shadows key on the neighbourhood rather than on the pixel?
//!
//! The controls used to move the tone *range*: a value near 0.8 was a highlight
//! wherever it sat, so recovering a sky flattened everything else at the same
//! brightness. They are keyed on a low-resolution guide now — see
//! [`rawkit_engine::guide`] — and these are the two claims that makes.
//!
//! GPU-gated like the rest: `cargo test -- --ignored`.

use rawkit_editstate::EditState;
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

/// An RGGB mosaic of a grey image described by a function of position.
fn mosaic(width: u32, height: u32, value: impl Fn(u32, u32) -> f32) -> Vec<f32> {
    (0..height)
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .map(|(x, y)| value(x, y))
        .collect()
}

fn render(gpu: &Gpu, tile: u32, cfa: &[f32], w: u32, h: u32, state: &EditState) -> Vec<f32> {
    Renderer::with_tile_size(gpu, tile)
        .run(
            gpu,
            &Frame {
                data: cfa,
                width: w,
                height: h,
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

fn luma(pixels: &[f32], w: u32, x: u32, y: u32) -> f32 {
    let i = ((y * w + x) * 4) as usize;
    0.2126 * pixels[i] + 0.7152 * pixels[i + 1] + 0.0722 * pixels[i + 2]
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_same_value_is_treated_by_where_it_is() {
    // The claim, in the smallest form that can hold it. Two identical thin
    // stripes of one value, one crossing a bright field and one crossing a dark
    // one. A curve keyed on the pixel maps both to the same number, whatever it
    // does to them. A curve keyed on the neighbourhood cannot.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    const W: u32 = 1024;
    const H: u32 = 512;
    const STRIPE: f32 = 0.30;
    let cfa = mosaic(W, H, |x, y| {
        if (150..154).contains(&y) || (350..354).contains(&y) {
            STRIPE
        } else if x < W / 2 {
            0.75
        } else {
            0.02
        }
    });

    let mut recovered = EditState::default();
    recovered.tone.highlights = -1.0;
    let plain = render(&gpu, 256, &cfa, W, H, &EditState::default());
    let after = render(&gpu, 256, &cfa, W, H, &recovered);

    // Sampled in the middle of each half, far from the vertical boundary.
    let measure = |x: u32| {
        let (mut before, mut now, mut n) = (0.0f64, 0.0f64, 0.0f64);
        for y in [151, 152, 351, 352] {
            for x in x - 60..x + 60 {
                before += luma(&plain, W, x, y) as f64;
                now += luma(&after, W, x, y) as f64;
                n += 1.0;
            }
        }
        (before / n, now / n)
    };
    let (bright_before, bright_after) = measure(W / 4);
    let (dark_before, dark_after) = measure(3 * W / 4);

    println!(
        "stripe in the bright half: {bright_before:.4} -> {bright_after:.4}\n\
         stripe in the dark half:   {dark_before:.4} -> {dark_after:.4}"
    );
    assert!(
        (bright_before - dark_before).abs() < 0.02,
        "the two stripes did not start equal: {bright_before:.4} against {dark_before:.4}"
    );
    assert!(
        bright_after < bright_before * 0.9,
        "the stripe in the bright half was not recovered at all: \
         {bright_before:.4} -> {bright_after:.4}"
    );
    assert!(
        dark_after > bright_after * 1.15,
        "the same value was treated the same in both halves, so the control is \
         still keyed on the pixel: bright {bright_after:.4}, dark {dark_after:.4}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn recovery_keeps_the_texture_it_recovers() {
    // The other half, and the reason the neighbourhood picks an exponent that is
    // then applied as a *gain*. Remapping each pixel by its own value compresses
    // the differences between neighbours — which is what makes a global
    // highlight recovery read as flat. A gain is a multiply, so the ratio
    // between two neighbours survives however hard the control is pushed.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    const W: u32 = 512;
    const H: u32 = 512;
    // A bright field carrying a gentle ripple: detail inside a highlight.
    let cfa = mosaic(W, H, |x, y| {
        let ripple = ((x / 8 + y / 8) % 2) as f32;
        0.70 + 0.07 * ripple
    });

    let mut recovered = EditState::default();
    recovered.tone.highlights = -1.0;
    let plain = render(&gpu, 256, &cfa, W, H, &EditState::default());
    let after = render(&gpu, 256, &cfa, W, H, &recovered);

    // Relative contrast: the spread of the ripple against the level it sits on.
    // That is the quantity a photographer sees as texture, and the one a
    // compressive remapping destroys.
    let spread = |pixels: &[f32]| {
        let (mut lo, mut hi, mut sum, mut n) = (f32::MAX, f32::MIN, 0.0f64, 0.0f64);
        for y in 64..H - 64 {
            for x in 64..W - 64 {
                let v = luma(pixels, W, x, y);
                lo = lo.min(v);
                hi = hi.max(v);
                sum += v as f64;
                n += 1.0;
            }
        }
        ((hi - lo) as f64 / (sum / n), sum / n)
    };
    let (before, level_before) = spread(&plain);
    let (now, level_after) = spread(&after);
    println!(
        "level {level_before:.4} -> {level_after:.4}, \
         relative texture {before:.4} -> {now:.4} ({:.0}% kept)",
        100.0 * now / before
    );
    assert!(
        level_after < level_before * 0.9,
        "nothing was recovered, so there is nothing to say about its texture"
    );
    assert!(
        now > before * 0.9,
        "the recovery flattened the texture inside it: {before:.4} -> {now:.4}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_guide_leaves_no_seam_between_tiles() {
    // The guide is indexed in full-resolution image coordinates precisely so
    // that two tiles compute the same neighbourhood where they meet. Index it in
    // tile coordinates instead and every tile boundary becomes a step — which
    // the existing seam test would not catch, because it renders with the tone
    // controls at their defaults and the guide switched off.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    const W: u32 = 640;
    const H: u32 = 256;
    let cfa = mosaic(W, H, |x, _| 0.25 + 0.5 * (x as f32 / W as f32));

    let mut edited = EditState::default();
    edited.tone.highlights = -1.0;
    edited.tone.shadows = 1.0;
    const TILE: u32 = 128;
    let pixels = render(&gpu, TILE, &cfa, W, H, &edited);

    // Along a row, the second difference at a tile boundary against the second
    // difference everywhere else. A smooth ramp has none worth speaking of; a
    // seam is a step, and a step is all second difference.
    let bend = |x: u32| {
        let y = H / 2;
        (luma(&pixels, W, x - 1, y) + luma(&pixels, W, x + 1, y) - 2.0 * luma(&pixels, W, x, y))
            .abs()
    };
    let mut worst_interior = 0.0f32;
    for x in 8..W - 8 {
        if x % TILE > 2 && x % TILE < TILE - 2 {
            worst_interior = worst_interior.max(bend(x));
        }
    }
    for boundary in (TILE..W).step_by(TILE as usize) {
        for x in boundary - 1..=boundary + 1 {
            let seam = bend(x);
            assert!(
                seam <= worst_interior * 4.0 + 1e-5,
                "a seam at x={x}: the picture bends by {seam:.6} there against \
                 {worst_interior:.6} anywhere else"
            );
        }
    }
    println!("worst bend away from a boundary: {worst_interior:.6}");
}

#[test]
#[ignore = "requires a GPU adapter"]
fn zooming_out_does_not_change_the_picture() {
    // The guide is indexed in full-resolution image coordinates, and a tile at
    // level 1 covers twice the ground of one at level 0. The `step` that
    // converts between them is written per tile by both render paths; get it
    // wrong and the local operator keys on the wrong part of the frame at every
    // zoom level but one — so the photograph would change as you zoomed, which
    // is the one thing a resolution pyramid must never do.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    const W: u32 = 512;
    const H: u32 = 512;
    // Bright on the left, dark on the right: a frame where *where* a pixel is
    // decides what happens to it, so a misread guide cannot pass unnoticed.
    let cfa = mosaic(W, H, |x, _| if x < W / 2 { 0.72 } else { 0.05 });

    let mut edited = EditState::default();
    edited.tone.highlights = -1.0;
    edited.tone.shadows = 1.0;

    let renderer = Renderer::with_tile_size(&gpu, 256);
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
    let buffers = renderer.allocate(&gpu, &frame);
    renderer
        .set_edit(&gpu, &buffers, &frame, &edited)
        .expect("set edit");
    let pyramid = rawkit_engine::Pyramid::build(&frame, 256);

    // One coarse tile covers the whole frame; two fine ones are needed to see
    // the same ground. Both halves matter: a `step` left at 1 would map the
    // coarse tile's right half onto the *left* of the guide, so the dark side
    // would be given the bright side's treatment — and a comparison that only
    // looked at the bright half would report perfect agreement.
    let fine: Vec<Vec<f32>> = (0..2)
        .map(|tx| {
            renderer
                .render_tile(&gpu, &buffers, &pyramid, 0, tx, 0, Output::Display)
                .expect("level 0")
        })
        .collect();
    let coarse = renderer
        .render_tile(&gpu, &buffers, &pyramid, 1, 0, 0, Output::Display)
        .expect("level 1");

    // A coarse pixel stands for a 2x2 block of fine ones, so compare each
    // against the average of the block it represents — away from the vertical
    // boundary, which the two levels resolve differently for reasons that have
    // nothing to do with the guide.
    let (mut worst, mut worst_at) = (0.0f32, (0u32, 0u32));
    for y in 8..120u32 {
        for x in 8..248u32 {
            // The frame's own vertical step, which the two levels resolve
            // differently for reasons that have nothing to do with the guide.
            if (120..136).contains(&x) {
                continue;
            }
            let (tile, fx) = if x < 128 {
                (0, x * 2)
            } else {
                (1, x * 2 - 256)
            };
            let mut fine_mean = 0.0;
            for dy in 0..2 {
                for dx in 0..2 {
                    fine_mean += luma(&fine[tile], 256, fx + dx, y * 2 + dy) / 4.0;
                }
            }
            let d = (luma(&coarse, 256, x, y) - fine_mean).abs();
            if d > worst {
                worst = d;
                worst_at = (x, y);
            }
        }
    }
    println!("worst level-0 against level-1 disagreement: {worst:.5}");
    assert!(
        worst < 0.02,
        "the two levels disagree by {worst:.5} at {worst_at:?}, so the guide is \
         being read at the wrong place when the view is zoomed out"
    );
}
