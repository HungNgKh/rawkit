//! Does rendering one tile at one level show the same picture as rendering
//! everything?
//!
//! The interactive canvas draws a handful of tiles from a reduced mosaic; export
//! draws every pixel of the full one. If those two disagree, the preview lies —
//! and it lies in the direction that is hardest to notice, because a preview is
//! only ever compared against memory.
//!
//! Two claims, and the first is the one that must be exact:
//!
//! 1. A level-0 tile is bit-identical to the same region of a whole-image
//!    render. Same kernels, same buffers, same order — anything less would mean
//!    the canvas and the export had drifted into separate code paths, which is
//!    the failure the engine's single `run` was built to prevent.
//! 2. A reduced level shows the same photograph. Not identical — averaging the
//!    mosaic before demosaicing is a different operation from demosaicing and
//!    then averaging — but close enough that zooming out does not change what
//!    the image looks like.
//!
//! GPU-gated like the rest: `cargo test -- --ignored`.

use rawkit_editstate::EditState;
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Pyramid, Renderer};

const W: u32 = 256;
const H: u32 = 256;
const TILE: u32 = 64;

/// Which colour a Bayer site carries, for the phase under test.
fn colour_at(x: u32, y: u32) -> usize {
    if (x + y) % 2 == 1 {
        1
    } else if y % 2 == 0 {
        0
    } else {
        2
    }
}

/// A scene with real structure at every scale, so a reduction that destroyed
/// detail or shifted phase would show up rather than average away.
fn scene(x: u32, y: u32) -> [f32; 3] {
    let fx = x as f32 / W as f32;
    let fy = y as f32 / H as f32;
    let lum = 0.5 + 0.3 * (fx * 9.0).sin() * (fy * 7.0).cos();
    [lum * 1.1, lum, lum * 0.85]
}

fn mosaic() -> Vec<f32> {
    (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| scene(x, y)[colour_at(x, y)])
        .collect()
}

fn frame(cfa: &[f32]) -> Frame<'_> {
    Frame {
        data: cfa,
        width: W,
        height: H,
        phase: BayerPhase::Rggb,
        as_shot_wb: [1.0, 1.0, 1.0],
        // No clipping, so highlight reconstruction cannot confound the
        // comparison between levels — that difference is real and documented on
        // `Pyramid`, and this test is measuring something else.
        clip_level: f32::INFINITY,
        profile: CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY),
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_level_zero_tile_is_identical_to_the_whole_image_render() {
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let cfa = mosaic();
    let image = frame(&cfa);
    let state = EditState::default();
    let renderer = Renderer::with_tile_size(&gpu, TILE);

    let whole = renderer
        .run(&gpu, &image, &state, Output::Display)
        .expect("whole-image render");

    let pyramid = Pyramid::build(&image, TILE);
    let buffers = renderer.allocate(&gpu, &image);
    renderer
        .set_edit(&gpu, &buffers, &image, &state)
        .expect("upload edit");

    let mut compared = 0usize;
    for ty in 0..H / TILE {
        for tx in 0..W / TILE {
            let tile = renderer
                .render_tile(&gpu, &buffers, &pyramid, 0, tx, ty, Output::Display)
                .expect("tile render");
            for y in 0..TILE {
                for x in 0..TILE {
                    let from_tile = &tile[((y * TILE + x) * 4) as usize..][..3];
                    let gx = tx * TILE + x;
                    let gy = ty * TILE + y;
                    let from_whole = &whole[(((gy * W) + gx) * 4) as usize..][..3];
                    assert_eq!(
                        from_tile, from_whole,
                        "tile ({tx}, {ty}) pixel ({x}, {y}) differs from the whole-image render"
                    );
                    compared += 1;
                }
            }
        }
    }
    assert_eq!(compared, (W * H) as usize, "every pixel should be covered");
}

#[test]
fn reduction_keeps_the_bayer_pattern() {
    // Each colour gets a distinctive value, so a reduction that mixed colours or
    // shifted phase produces a value that belongs to no channel at all.
    let marks = [0.125f32, 0.5, 0.875];
    let cfa: Vec<f32> = (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| marks[colour_at(x, y)])
        .collect();
    let image = frame(&cfa);
    let pyramid = Pyramid::build(&image, TILE);
    assert!(pyramid.levels() >= 2, "need reductions to check");

    for level in 1..=pyramid.levels() {
        let (data, lw, lh) = pyramid.level(level).expect("level exists");
        for y in 0..lh {
            for x in 0..lw {
                let expected = marks[colour_at(x, y)];
                let found = data[(y * lw + x) as usize];
                assert!(
                    (found - expected).abs() < 1e-6,
                    "level {level} ({x}, {y}) holds {found}, not the {expected} its site should carry — \
                     the pattern moved"
                );
            }
        }
    }
}

#[test]
fn levels_halve_until_one_tile_covers_the_mosaic() {
    let cfa = mosaic();
    let image = frame(&cfa);
    let pyramid = Pyramid::build(&image, TILE);

    assert_eq!(pyramid.level(0).map(|l| (l.1, l.2)), Some((W, H)));
    assert_eq!(pyramid.level(1).map(|l| (l.1, l.2)), Some((W / 2, H / 2)));
    assert_eq!(pyramid.level(2).map(|l| (l.1, l.2)), Some((W / 4, H / 4)));
    assert_eq!(
        pyramid.levels(),
        2,
        "256 reduces twice to reach a 64 tile, and stops"
    );
    assert!(
        pyramid.level(3).is_none(),
        "asking past the top must not silently return the coarsest level"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_coarse_tile_shows_the_same_photograph() {
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let cfa = mosaic();
    let image = frame(&cfa);
    let state = EditState::default();
    let renderer = Renderer::with_tile_size(&gpu, TILE);

    let whole = renderer
        .run(&gpu, &image, &state, Output::Display)
        .expect("whole-image render");
    let pyramid = Pyramid::build(&image, TILE);
    let buffers = renderer.allocate(&gpu, &image);
    renderer
        .set_edit(&gpu, &buffers, &image, &state)
        .expect("upload edit");

    // Level 2 covers the whole 256x256 image in one 64x64 tile.
    let coarse = renderer
        .render_tile(&gpu, &buffers, &pyramid, 2, 0, 0, Output::Display)
        .expect("coarse tile");

    // Compare against the full render box-averaged by the same factor. Stay off
    // the border: RCD clamps at the image edge, and a 4x reduction magnifies
    // that into a visible frame.
    let step = 4u32;
    let border = 3u32;
    let mut sum_sq = 0.0f64;
    let mut n = 0.0f64;
    for y in border..(H / step - border) {
        for x in border..(W / step - border) {
            for c in 0..3 {
                let mut acc = 0.0f32;
                for sy in 0..step {
                    for sx in 0..step {
                        let i = (((y * step + sy) * W + x * step + sx) * 4) as usize;
                        acc += whole[i + c];
                    }
                }
                let reference = acc / (step * step) as f32;
                let found = coarse[((y * (W / step) + x) * 4) as usize + c];
                sum_sq += ((found - reference) as f64).powi(2);
                n += 1.0;
            }
        }
    }
    let psnr = 10.0 * (1.0 / (sum_sq / n)).log10();
    println!("coarse vs downsampled full render: {psnr:.2} dB");
    assert!(
        psnr > 30.0,
        "a zoomed-out view should look like the photograph, got {psnr:.2} dB"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_tile_off_the_edge_of_the_mosaic_is_refused() {
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let cfa = mosaic();
    let image = frame(&cfa);
    let renderer = Renderer::with_tile_size(&gpu, TILE);
    let pyramid = Pyramid::build(&image, TILE);
    let buffers = renderer.allocate(&gpu, &image);
    renderer
        .set_edit(&gpu, &buffers, &image, &EditState::default())
        .expect("upload edit");

    // Refusing beats returning black: a canvas that asks for a tile that does
    // not exist has a bug in its viewport maths, and black pixels would hide it.
    assert!(renderer
        .render_tile(&gpu, &buffers, &pyramid, 0, 99, 0, Output::Display)
        .is_err());
    assert!(renderer
        .render_tile(&gpu, &buffers, &pyramid, 7, 0, 0, Output::Display)
        .is_err());
}
