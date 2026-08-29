//! Do tiles drawn straight to the GPU land where they should, and say what the
//! readback path says?
//!
//! `render_tile` ends in a `map_async` and a blocking device poll — a full
//! CPU/GPU sync for every tile. That is correct for export, which wants the
//! pixels, and fatal for a canvas: at a dozen visible tiles it is a dozen stalls
//! a frame, and neither tiling nor resolution levels can recover from that.
//! `draw_tile` submits and returns.
//!
//! The risk in going GPU-resident is that the two paths drift, and that the
//! drift is invisible — a canvas that is subtly offset, or that smears its edge
//! tiles, looks like a rendering quirk rather than a bug. So the tests here
//! check the two things the present pass can actually get wrong: **where** a
//! tile lands, and **whether** it is the same picture.
//!
//! GPU-gated like the rest: `cargo test -- --ignored`.

use rawkit_editstate::EditState;
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Pyramid, Renderer};

const W: u32 = 256;
const H: u32 = 256;
const TILE: u32 = 64;

/// Half floats carry an 11-bit significand, so a value near 1.0 is exact to
/// about 5e-4. This is the format's precision, not a fudge factor: a real
/// disagreement between the two paths is a structural one and misses by far
/// more than a quantisation step.
const F16_TOLERANCE: f32 = 1e-3;

fn colour_at(x: u32, y: u32) -> usize {
    if (x + y) % 2 == 1 {
        1
    } else if y % 2 == 0 {
        0
    } else {
        2
    }
}

fn mosaic() -> Vec<f32> {
    (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| {
            let fx = x as f32 / W as f32;
            let fy = y as f32 / H as f32;
            let lum = 0.5 + 0.3 * (fx * 9.0).sin() * (fy * 7.0).cos();
            [lum * 1.1, lum, lum * 0.85][colour_at(x, y)]
        })
        .collect()
}

fn frame(cfa: &[f32]) -> Frame<'_> {
    Frame {
        data: cfa,
        width: W,
        height: H,
        phase: BayerPhase::Rggb,
        as_shot_wb: [1.0, 1.0, 1.0],
        clip_level: f32::INFINITY,
        profile: CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY),
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_tile_drawn_to_the_canvas_is_the_tile_read_back() {
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let cfa = mosaic();
    let image = frame(&cfa);
    let state = EditState::default();
    let renderer = Renderer::with_tile_size(&gpu, TILE);
    let pyramid = Pyramid::build(&image, TILE);
    let buffers = renderer.allocate(&gpu, &image);
    renderer
        .set_edit(&gpu, &buffers, &image, &state)
        .expect("upload edit");

    let canvas = renderer.create_canvas(&gpu, TILE, TILE);
    renderer
        .draw_tile(
            &gpu,
            &buffers,
            &canvas,
            &pyramid,
            0,
            2,
            1,
            [0, 0],
            Output::Display,
        )
        .expect("draw");
    let drawn = canvas.read_back(&gpu).expect("canvas readback");

    let expected = renderer
        .render_tile(&gpu, &buffers, &pyramid, 0, 2, 1, Output::Display)
        .expect("readback path");

    assert_eq!(drawn.len(), expected.len());
    let mut worst = 0.0f32;
    for (i, (a, b)) in drawn.iter().zip(&expected).enumerate() {
        // Alpha is written as a constant 1.0 by both paths.
        if i % 4 == 3 {
            continue;
        }
        worst = worst.max((a - b).abs());
    }
    // Measured at 4.879e-4, which is 2^-11 — the spacing of half floats near
    // 1.0, to three digits. The two paths do not merely agree closely; they
    // agree to the exact precision of the format, which is what rules out a
    // structural difference hiding under a loose tolerance.
    println!("worst difference between canvas and readback: {worst:e}");
    assert!(
        worst < F16_TOLERANCE,
        "the two paths disagree by {worst:e}, which is more than half-float quantisation"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn tiles_land_where_they_are_told() {
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let cfa = mosaic();
    let image = frame(&cfa);
    let renderer = Renderer::with_tile_size(&gpu, TILE);
    let pyramid = Pyramid::build(&image, TILE);
    let buffers = renderer.allocate(&gpu, &image);
    renderer
        .set_edit(&gpu, &buffers, &image, &EditState::default())
        .expect("upload edit");

    // Four tiles into one canvas, in one pass, with no synchronisation between
    // them — which is the shape a real frame has.
    let canvas = renderer.create_canvas(&gpu, TILE * 2, TILE * 2);
    for (tx, ty) in [(0u32, 0u32), (1, 0), (0, 1), (1, 1)] {
        renderer
            .draw_tile(
                &gpu,
                &buffers,
                &canvas,
                &pyramid,
                0,
                tx,
                ty,
                [(tx * TILE) as i32, (ty * TILE) as i32],
                Output::Display,
            )
            .expect("draw");
    }
    let assembled = canvas.read_back(&gpu).expect("canvas readback");

    for (tx, ty) in [(0u32, 0u32), (1, 0), (0, 1), (1, 1)] {
        let tile = renderer
            .render_tile(&gpu, &buffers, &pyramid, 0, tx, ty, Output::Display)
            .expect("readback path");
        for y in 0..TILE {
            for x in 0..TILE {
                let cx = tx * TILE + x;
                let cy = ty * TILE + y;
                for c in 0..3 {
                    let found = assembled[(((cy * TILE * 2) + cx) * 4 + c) as usize];
                    let want = tile[(((y * TILE) + x) * 4 + c) as usize];
                    assert!(
                        (found - want).abs() < F16_TOLERANCE,
                        "canvas ({cx}, {cy}) holds {found}, but tile ({tx}, {ty}) says {want} — \
                         a tile landed in the wrong place"
                    );
                }
            }
        }
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_tile_overhanging_the_canvas_is_clipped_not_wrapped() {
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let cfa = mosaic();
    let image = frame(&cfa);
    let renderer = Renderer::with_tile_size(&gpu, TILE);
    let pyramid = Pyramid::build(&image, TILE);
    let buffers = renderer.allocate(&gpu, &image);
    renderer
        .set_edit(&gpu, &buffers, &image, &EditState::default())
        .expect("upload edit");

    // A canvas smaller than the tile grid, which is the normal case: a viewport
    // rarely ends on a tile boundary.
    let (cw, ch) = (100u32, 100u32);
    let canvas = renderer.create_canvas(&gpu, cw, ch);
    renderer
        .draw_tile(
            &gpu,
            &buffers,
            &canvas,
            &pyramid,
            0,
            0,
            0,
            [64, 64],
            Output::Display,
        )
        .expect("an overhanging tile is not an error");
    let drawn = canvas.read_back(&gpu).expect("canvas readback");

    // Everything above and left of the destination must be untouched. Writing
    // there would mean the bounds check wrapped rather than dropped, which shows
    // up as a copy of the image in the corner of the canvas.
    for y in 0..64 {
        for x in 0..cw {
            let i = ((y * cw + x) * 4) as usize;
            assert_eq!(
                drawn[i], 0.0,
                "({x}, {y}) was written, but the tile starts at (64, 64)"
            );
        }
    }
    // And the part that does fit is real picture, not zeros.
    let inside = drawn[(((70 * cw) + 70) * 4) as usize];
    assert!(inside > 0.0, "the visible corner of the tile is blank");
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_tile_starting_before_the_canvas_is_clipped_on_that_side_too() {
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let cfa = mosaic();
    let image = frame(&cfa);
    let renderer = Renderer::with_tile_size(&gpu, TILE);
    let pyramid = Pyramid::build(&image, TILE);
    let buffers = renderer.allocate(&gpu, &image);
    renderer
        .set_edit(&gpu, &buffers, &image, &EditState::default())
        .expect("upload edit");

    // Once the image can be panned, the first visible tile almost always begins
    // left of and above the viewport — the tile grid is fixed to the image and
    // does not move with the view. A negative destination is the ordinary case,
    // not an edge case, which is why the destination is signed at all.
    let canvas = renderer.create_canvas(&gpu, TILE, TILE);
    let offset = -(TILE as i32) / 4;
    renderer
        .draw_tile(
            &gpu,
            &buffers,
            &canvas,
            &pyramid,
            0,
            1,
            1,
            [offset, offset],
            Output::Display,
        )
        .expect("a tile may start off the canvas");
    let drawn = canvas.read_back(&gpu).expect("canvas readback");

    let expected = renderer
        .render_tile(&gpu, &buffers, &pyramid, 0, 1, 1, Output::Display)
        .expect("readback path");

    // The visible part must be the tail of the tile, shifted — not the head of
    // it, which is what an unsigned destination wrapping to a huge number would
    // have produced, and not blank.
    let shift = (-offset) as u32;
    for y in 0..TILE - shift {
        for x in 0..TILE - shift {
            for c in 0..3 {
                let found = drawn[(((y * TILE) + x) * 4 + c) as usize];
                let want = expected[((((y + shift) * TILE) + x + shift) * 4 + c) as usize];
                assert!(
                    (found - want).abs() < F16_TOLERANCE,
                    "canvas ({x}, {y}) holds {found}; the tile shifted by {offset} says {want}"
                );
            }
        }
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_tile_overhanging_the_image_does_not_draw_its_overhang() {
    let gpu = Gpu::new().expect("no usable GPU adapter");

    // An image whose width is not a multiple of the tile: the last tile column
    // is 32 pixels of picture and 32 of nothing. Sensor sizes are rarely tidy,
    // so this is the normal case rather than a contrived one.
    const ODD: u32 = 160;
    let cfa: Vec<f32> = (0..ODD)
        .flat_map(|y| (0..ODD).map(move |x| (x, y)))
        .map(|(x, y)| [0.6, 0.5, 0.4][colour_at(x, y)])
        .collect();
    let image = Frame {
        data: &cfa,
        width: ODD,
        height: ODD,
        phase: BayerPhase::Rggb,
        as_shot_wb: [1.0, 1.0, 1.0],
        clip_level: f32::INFINITY,
        profile: CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY),
    };
    let renderer = Renderer::with_tile_size(&gpu, TILE);
    let pyramid = Pyramid::build(&image, TILE);
    let buffers = renderer.allocate(&gpu, &image);
    renderer
        .set_edit(&gpu, &buffers, &image, &EditState::default())
        .expect("upload edit");

    let canvas = renderer.create_canvas(&gpu, TILE, TILE);
    renderer
        .draw_tile(
            &gpu,
            &buffers,
            &canvas,
            &pyramid,
            0,
            2,
            0,
            [0, 0],
            Output::Display,
        )
        .expect("draw the last tile");
    let drawn = canvas.read_back(&gpu).expect("canvas readback");

    // Beyond the image the gather clamps, which repeats a column and breaks the
    // CFA phase — and a broken phase demosaics to magenta, not to something
    // merely soft. Those pixels must never reach the canvas.
    let valid = ODD - 2 * TILE;
    for y in 0..TILE {
        for x in valid..TILE {
            let i = (((y * TILE) + x) * 4) as usize;
            assert_eq!(
                &drawn[i..i + 3],
                &[0.0, 0.0, 0.0],
                "({x}, {y}) is past the image edge and was drawn anyway"
            );
        }
    }
    // And the part that is inside the image is real picture.
    let inside = drawn[(((TILE / 2) * TILE + valid / 2) * 4) as usize];
    assert!(inside > 0.0, "the valid part of the tile is blank");
}
