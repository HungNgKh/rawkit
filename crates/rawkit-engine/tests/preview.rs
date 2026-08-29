//! Does a cached preview land in the canvas as the right colour, in the right
//! place?
//!
//! Two things this can get wrong, and both are quiet. **Colour**: a preview is
//! encoded sRGB and the canvas is linear, so writing the bytes through unchanged
//! makes the picture far too bright — the washed-out failure, which reads as a
//! grading problem. **Geometry**: the region a viewport is looking at has to map
//! onto the right part of the preview, and an offset or a flip looks like the
//! photograph rather than like a bug.
//!
//! GPU-gated like the rest: `cargo test -- --ignored`.

use rawkit_engine::{Gpu, PreviewBlit, Renderer};

/// Half floats carry an 11-bit significand; a value near 1.0 is exact to about
/// 5e-4. Eight-bit sRGB input quantises more coarsely than that, so the budget
/// here is one 8-bit step in linear terms near mid-grey.
const TOLERANCE: f32 = 4e-3;

fn gpu() -> Option<Gpu> {
    Gpu::new().ok()
}

/// A solid image of one 8-bit sRGB value.
fn flat(value: u8, width: u32, height: u32) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..width * height {
        rgba.extend_from_slice(&[value, value, value, 255]);
    }
    rgba
}

/// The sRGB transfer function, so the expectation is derived rather than copied.
fn to_linear(encoded: f32) -> f32 {
    if encoded <= 0.040_45 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_preview_is_decoded_to_linear_on_the_way_into_the_canvas() {
    // The number that matters: 118 is what `rawkit-export` writes for linear
    // 0.18, and 0.18 is what has to come back out. If the sRGB texture format
    // ever stopped doing the conversion, this lands on 0.463 — two and a half
    // times too bright, and looking merely "a bit flat" on screen.
    let Some(gpu) = gpu() else { return };
    let renderer = Renderer::new(&gpu);
    let blit = PreviewBlit::new(&gpu);
    let canvas = renderer.create_canvas(&gpu, 32, 32);

    let image = blit.upload(&gpu, &flat(118, 8, 8), 8, 8).expect("upload");
    blit.draw(&gpu, &image, &canvas, [0.0, 0.0], [1.0, 1.0]);

    let pixels = canvas.read_back(&gpu).expect("read back");
    let expected = to_linear(118.0 / 255.0);
    assert!(
        (expected - 0.18).abs() < 0.002,
        "the fixture is wrong before the test even runs: {expected}"
    );
    for (i, pixel) in pixels.chunks_exact(4).enumerate() {
        assert!(
            (pixel[0] - expected).abs() < TOLERANCE,
            "pixel {i} is {} not {expected}",
            pixel[0]
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_region_selects_the_part_of_the_preview_the_view_is_over() {
    // A left half and a right half of different values. Asking for the right
    // half must produce only the right half's value — an offset, a flip, or a
    // span applied to the wrong axis all fail here and all look plausible on a
    // photograph.
    let Some(gpu) = gpu() else { return };
    let renderer = Renderer::new(&gpu);
    let blit = PreviewBlit::new(&gpu);
    let canvas = renderer.create_canvas(&gpu, 16, 16);

    let (w, h) = (16u32, 16u32);
    let mut rgba = Vec::new();
    for _ in 0..h {
        for x in 0..w {
            let value = if x < w / 2 { 0u8 } else { 255 };
            rgba.extend_from_slice(&[value, value, value, 255]);
        }
    }
    let image = blit.upload(&gpu, &rgba, w, h).expect("upload");

    // The right half, avoiding the middle column where filtering blends.
    blit.draw(&gpu, &image, &canvas, [0.6, 0.0], [0.35, 1.0]);
    let pixels = canvas.read_back(&gpu).expect("read back");
    assert!(
        pixels.chunks_exact(4).all(|p| p[0] > 0.9),
        "the right half is white; got {:?}",
        &pixels[..4]
    );

    blit.draw(&gpu, &image, &canvas, [0.05, 0.0], [0.35, 1.0]);
    let pixels = canvas.read_back(&gpu).expect("read back");
    assert!(
        pixels.chunks_exact(4).all(|p| p[0] < 0.05),
        "the left half is black; got {:?}",
        &pixels[..4]
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn outside_the_photograph_is_black_rather_than_a_stretched_edge() {
    // Fit-to-view leaves bars beside a photograph whose aspect does not match
    // the window. Clamping the sampler instead would paint those bars with the
    // edge pixels smeared outwards, which reads as part of the picture.
    let Some(gpu) = gpu() else { return };
    let renderer = Renderer::new(&gpu);
    let blit = PreviewBlit::new(&gpu);
    let canvas = renderer.create_canvas(&gpu, 16, 16);

    let image = blit.upload(&gpu, &flat(255, 8, 8), 8, 8).expect("upload");
    // Twice as much region as there is image, anchored so the image occupies the
    // top-left quarter.
    blit.draw(&gpu, &image, &canvas, [0.0, 0.0], [2.0, 2.0]);

    let pixels = canvas.read_back(&gpu).expect("read back");
    let at = |x: usize, y: usize| pixels[(y * 16 + x) * 4];
    assert!(at(2, 2) > 0.9, "inside the image should be white");
    assert!(at(13, 13) < 0.01, "outside it should be black");
    assert!(at(13, 2) < 0.01, "and so should beside it");
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_buffer_that_does_not_match_its_geometry_is_refused() {
    let Some(gpu) = gpu() else { return };
    let blit = PreviewBlit::new(&gpu);
    assert!(blit.upload(&gpu, &flat(0, 4, 4), 8, 8).is_err());
    assert!(blit.upload(&gpu, &[], 0, 0).is_err());
}
