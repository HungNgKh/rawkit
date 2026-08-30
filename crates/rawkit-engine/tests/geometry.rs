//! Does cropping change any pixel it keeps?
//!
//! It must not. Orientation and crop select and permute — they never resample —
//! so a cropped render has to be **bit-identical** to the same region of an
//! uncropped one, and a rotated one has to carry exactly the same values in a
//! different arrangement.
//!
//! That claim is worth a GPU test rather than only a unit test on the mapping.
//! The mapping is exhaustively covered in `geometry.rs`; what this checks is the
//! *wiring* — that `run` actually applies the geometry, that it applies it after
//! the pipeline rather than to some intermediate, and that changing the crop does
//! not perturb the render of the pixels that stayed. The last one is the
//! interesting failure: if geometry were pushed inside the tile loop, a crop
//! would change tile boundaries, halos would land differently, and the kept
//! pixels would come back subtly different. Nobody would see it on a photograph.
//!
//! GPU-gated like the rest: `cargo test -- --ignored`.

use rawkit_editstate::{Crop, EditState, Geometry, Orientation};
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const W: u32 = 192;
const H: u32 = 128;
const TILE: u32 = 64;

fn colour_at(x: u32, y: u32) -> usize {
    if (x + y) % 2 == 1 {
        1
    } else if y % 2 == 0 {
        0
    } else {
        2
    }
}

/// Structure everywhere, so a shifted region would show as a mismatch rather
/// than average into agreement.
fn mosaic() -> Vec<f32> {
    (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| {
            let fx = x as f32 / W as f32;
            let fy = y as f32 / H as f32;
            let lum = 0.5 + 0.3 * (fx * 11.0).sin() * (fy * 7.0).cos();
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
        recorded_orientation: rawkit_editstate::Orientation::AsShot,
    }
}

/// An edit that changes pixel values, so the comparison is not accidentally
/// between two copies of the identity render.
fn edited() -> EditState {
    let mut state = EditState::default();
    state.tone.exposure_ev = 0.7;
    state.tone.contrast = 0.4;
    state
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_cropped_render_is_bit_identical_to_the_region_it_kept() {
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let cfa = mosaic();
    let image = frame(&cfa);
    let renderer = Renderer::with_tile_size(&gpu, TILE);

    let whole = renderer
        .run(&gpu, &image, &edited(), Output::Display)
        .expect("uncropped render");

    // Deliberately not aligned to the 64-pixel tile grid: a crop that happened
    // to land on tile boundaries would hide exactly the bug this is looking for.
    let crop = Crop {
        left: 0.2,
        top: 0.3,
        right: 0.85,
        bottom: 0.9,
        ..Crop::default()
    };
    let mut state = edited();
    state.crop = crop;
    let cropped = renderer
        .run(&gpu, &image, &state, Output::Display)
        .expect("cropped render");

    let geometry = Geometry::new(&state, Orientation::AsShot);
    let [ow, oh] = geometry.output_size([W, H]);
    assert_eq!([cropped.width, cropped.height], [ow, oh]);
    assert!(
        ow < W && oh < H,
        "the crop has to actually remove something"
    );

    for y in 0..oh {
        for x in 0..ow {
            let [sx, sy] = geometry.source_of([x, y], [W, H]);
            let from = ((sy * W + sx) * 4) as usize;
            let to = ((y * ow + x) * 4) as usize;
            assert_eq!(
                &cropped.pixels[to..to + 4],
                &whole.pixels[from..from + 4],
                "({x}, {y}) of the crop differs from ({sx}, {sy}) of the whole frame"
            );
        }
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn rotating_rearranges_the_pixels_and_changes_none_of_them() {
    // Every value that went in comes out, in a different place. Checked as a
    // multiset rather than position by position, so this cannot pass by
    // agreeing with the same mapping it is testing.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let cfa = mosaic();
    let image = frame(&cfa);
    let renderer = Renderer::with_tile_size(&gpu, TILE);

    let upright = renderer
        .run(&gpu, &image, &edited(), Output::Display)
        .expect("upright render");

    let mut state = edited();
    state.orientation = Orientation::Rotate90Cw;
    let turned = renderer
        .run(&gpu, &image, &state, Output::Display)
        .expect("rotated render");

    assert_eq!([turned.width, turned.height], [H, W], "the axes swap");

    let sorted = |pixels: &[f32]| {
        let mut bits: Vec<u32> = pixels.iter().map(|v| v.to_bits()).collect();
        bits.sort_unstable();
        bits
    };
    assert_eq!(
        sorted(&upright.pixels),
        sorted(&turned.pixels),
        "rotation invented or lost a value"
    );
}

/// Half-float storage: the canvas keeps what the resampler produced at
/// `rgba16float`, so the two paths agree to the format's precision, not exactly.
const F16_TOLERANCE: f32 = 1e-3;

#[test]
#[ignore = "requires a GPU adapter"]
fn the_canvas_straightens_the_same_way_the_export_does() {
    // The claim the whole design rests on. The export walks the map on the CPU;
    // the canvas hands a six-float transform to a compute kernel and gathers.
    // They are separate code, and if they disagree the photograph on screen is
    // framed differently from the file that gets written — which nobody would
    // catch without putting the two side by side.
    //
    // No orientation and a full-frame crop, so flat space *is* sensor space and
    // `apply` computes exactly what the kernel should.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);
    let (w, h) = (96u32, 64u32);

    // Structure at every scale, so a mis-set tap shows as a mismatch rather
    // than averaging into agreement.
    let mut pixels = vec![0.0f32; (w * h) as usize * 4];
    for y in 0..h {
        for x in 0..w {
            let at = ((y * w + x) * 4) as usize;
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            pixels[at] = 0.5 + 0.4 * (fx * 17.0).sin();
            pixels[at + 1] = 0.5 + 0.4 * (fy * 13.0).cos();
            pixels[at + 2] = 0.5 + 0.3 * ((fx + fy) * 9.0).sin();
            pixels[at + 3] = 1.0;
        }
    }

    let flat = renderer.create_canvas(&gpu, w, h);
    flat.write(&gpu, &pixels);
    // Read it back, so the CPU side starts from the same half-float values the
    // kernel will: comparing against the f32 originals would measure the
    // storage format rather than the two implementations.
    let stored = flat.read_back(&gpu).expect("flat readback");

    let geometry = Geometry::from_parts(
        Orientation::AsShot,
        Orientation::AsShot,
        Crop {
            angle_deg: 7.5,
            ..Crop::default()
        },
    );
    let [ow, oh] = geometry.output_size([w, h]);
    let canvas = renderer.create_canvas(&gpu, ow, oh);
    renderer.straighten(
        &gpu,
        &flat,
        &canvas,
        &geometry,
        rawkit_engine::StraightenView {
            level_image: [w, h],
            straight_origin: [0.0, 0.0],
            flat_origin: [0.0, 0.0],
        },
    );
    let drawn = canvas.read_back(&gpu).expect("canvas readback");

    let (expected, size) = rawkit_engine::geometry::apply(&geometry, &stored, [w, h]);
    assert_eq!(size, [ow, oh]);

    let mut worst = 0.0f32;
    for (a, b) in drawn.iter().zip(&expected) {
        worst = worst.max((a - b).abs());
    }
    println!("worst difference between canvas and export straighten: {worst:e}");
    assert!(
        worst < F16_TOLERANCE,
        "the canvas and the export disagree by {worst:e}"
    );
}
