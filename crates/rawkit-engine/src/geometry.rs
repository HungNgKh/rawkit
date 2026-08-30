//! Applying the geometry to developed pixels.
//!
//! The map itself lives in [`rawkit_editstate::Geometry`], because
//! `rawkit-session` needs the same arithmetic and cannot see this crate. What is
//! left here is the one part that touches pixels.

use rawkit_editstate::Geometry;

/// Rearrange a developed RGBA frame into the photograph.
///
/// Returns the pixels and their size together, because the two are only
/// correct as a pair — a caller holding cropped pixels and the sensor's
/// width indexes into the wrong row and gets a picture that looks sheared.
pub fn apply(geometry: &Geometry, rgba: &[f32], image: [u32; 2]) -> (Vec<f32>, [u32; 2]) {
    if geometry.is_identity() {
        return (rgba.to_vec(), image);
    }
    if geometry.resamples() {
        return straighten(geometry, rgba, image);
    }
    let [ow, oh] = geometry.output_size(image);
    let mut out = vec![0.0f32; (ow as usize) * (oh as usize) * 4];
    for y in 0..oh {
        for x in 0..ow {
            let [sx, sy] = geometry.source_of([x, y], image);
            let from = ((sy as usize) * image[0] as usize + sx as usize) * 4;
            let to = ((y as usize) * ow as usize + x as usize) * 4;
            out[to..to + 4].copy_from_slice(&rgba[from..from + 4]);
        }
    }
    (out, [ow, oh])
}

/// Rearrange *and* resample, for a frame that has been straightened.
///
/// The one place in the pipeline that interpolates. Every other geometric
/// operation lands each output pixel on exactly one source pixel; a fraction of
/// a degree does not, so this is where a photograph stops being a rearrangement
/// of its own samples.
///
/// # Why Catmull-Rom
///
/// Bilinear would be four taps and no overshoot, and it visibly softens: a
/// one-degree correction would leave the whole frame slightly less sharp than it
/// went in, which a photographer notices at 100% and blames on the lens.
/// Catmull-Rom is the standard photographic choice and keeps the detail. It can
/// overshoot at a hard edge — a faint halo across a black-to-white boundary —
/// which is the accepted cost and the reason the weights are written out rather
/// than hidden in a sampler.
///
/// # Why the taps are explicit
///
/// A GPU's own filtering uses reduced-precision weights, so anything sampled
/// through hardware could never match a render done here. Writing the arithmetic
/// out is what will let the canvas and the export agree when the canvas learns
/// to straighten.
fn straighten(geometry: &Geometry, rgba: &[f32], image: [u32; 2]) -> (Vec<f32>, [u32; 2]) {
    let [ow, oh] = geometry.output_size(image);
    let mut out = vec![0.0f32; (ow as usize) * (oh as usize) * 4];
    for y in 0..oh {
        for x in 0..ow {
            let at = geometry.source_at([x as f32, y as f32], image);
            let pixel = sample(rgba, image, at);
            let to = ((y as usize) * ow as usize + x as usize) * 4;
            out[to..to + 4].copy_from_slice(&pixel);
        }
    }
    (out, [ow, oh])
}

/// One Catmull-Rom tap set, 4x4 around `at`.
///
/// Alpha is carried through the same filter as the colour rather than being
/// forced to 1: the develop stage writes a constant there today, and a filter
/// that quietly disagrees with the other three channels is the kind of thing
/// that only shows up once something starts using it.
fn sample(rgba: &[f32], image: [u32; 2], at: [f32; 2]) -> [f32; 4] {
    let (w, h) = (image[0] as i64, image[1] as i64);
    // The sample sits at a pixel *centre*, so the nearest texel index is the
    // floor of the position less half a pixel.
    let base = [(at[0] - 0.5).floor(), (at[1] - 0.5).floor()];
    let frac = [at[0] - 0.5 - base[0], at[1] - 0.5 - base[1]];
    let wx = weights(frac[0]);
    let wy = weights(frac[1]);

    let mut acc = [0.0f32; 4];
    for (j, vertical) in wy.iter().enumerate() {
        // Clamped to the edge. The crop is pulled in far enough that this should
        // not fire, so it is a guard rather than a behaviour — but a rounding
        // error at the last row must read a pixel, not panic.
        let sy = (base[1] as i64 + j as i64 - 1).clamp(0, h - 1);
        for (i, horizontal) in wx.iter().enumerate() {
            let sx = (base[0] as i64 + i as i64 - 1).clamp(0, w - 1);
            let weight = horizontal * vertical;
            let from = ((sy * w + sx) * 4) as usize;
            for (c, channel) in acc.iter_mut().enumerate() {
                *channel += rgba[from + c] * weight;
            }
        }
    }
    acc
}

/// Catmull-Rom weights for the four taps around a fraction.
///
/// The a = -0.5 form, which is the interpolating cubic that passes through every
/// source pixel — so a straighten of exactly zero would return the image
/// unchanged even if it took this path.
fn weights(t: f32) -> [f32; 4] {
    let t2 = t * t;
    let t3 = t2 * t;
    [
        -0.5 * t3 + t2 - 0.5 * t,
        1.5 * t3 - 2.5 * t2 + 1.0,
        -1.5 * t3 + 2.0 * t2 + 0.5 * t,
        0.5 * t3 - 0.5 * t2,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawkit_editstate::{Crop, EditState, Orientation};

    /// A frame whose every pixel names its own position, so a rearrangement can
    /// be checked by reading rather than by comparing against another
    /// rearrangement written the same way.
    fn frame(w: u32, h: u32) -> Vec<f32> {
        (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                [x as f32, y as f32, 0.0, 1.0]
            })
            .collect()
    }

    fn at(pixels: &[f32], w: u32, x: u32, y: u32) -> (f32, f32) {
        let i = ((y * w + x) * 4) as usize;
        (pixels[i], pixels[i + 1])
    }

    fn with(orientation: Orientation, crop: Crop) -> Geometry {
        Geometry::from_parts(Orientation::AsShot, orientation, crop)
    }

    #[test]
    fn the_identity_hands_back_exactly_what_it_was_given() {
        // The property that keeps an unedited photograph unchanged by the
        // existence of this module.
        let pixels = frame(7, 5);
        let (out, size) = apply(
            &Geometry::new(&EditState::default(), Orientation::AsShot),
            &pixels,
            [7, 5],
        );
        assert_eq!(size, [7, 5]);
        assert_eq!(out, pixels);
    }

    #[test]
    fn a_quarter_turn_moves_the_top_left_corner_to_the_top_right() {
        // Stated as a corner rather than as a formula, because the formula is
        // what is being tested and the two would agree by construction.
        let pixels = frame(4, 3);
        let g = with(Orientation::Rotate90Cw, Crop::default());
        let (out, size) = apply(&g, &pixels, [4, 3]);
        assert_eq!(size, [3, 4], "a portrait frame from a landscape one");
        assert_eq!(
            at(&out, 3, 2, 0),
            (0.0, 0.0),
            "source (0,0) is now top-right"
        );
        assert_eq!(
            at(&out, 3, 0, 0),
            (0.0, 2.0),
            "source (0,2) is now top-left"
        );
    }

    #[test]
    fn four_quarter_turns_are_no_turns() {
        // Rotation has to be exact, and four of them is the cheapest way to say
        // so without restating the mapping.
        let (w, h) = (5u32, 3u32);
        let mut pixels = frame(w, h);
        let mut size = [w, h];
        for _ in 0..4 {
            let (next, next_size) = apply(
                &with(Orientation::Rotate90Cw, Crop::default()),
                &pixels,
                size,
            );
            pixels = next;
            size = next_size;
        }
        assert_eq!(size, [w, h]);
        assert_eq!(pixels, frame(w, h));
    }

    #[test]
    fn a_half_turn_is_two_quarter_turns() {
        let pixels = frame(4, 3);
        let (half, half_size) = apply(
            &with(Orientation::Rotate180, Crop::default()),
            &pixels,
            [4, 3],
        );
        let (once, size) = apply(
            &with(Orientation::Rotate90Cw, Crop::default()),
            &pixels,
            [4, 3],
        );
        let (twice, twice_size) =
            apply(&with(Orientation::Rotate90Cw, Crop::default()), &once, size);
        assert_eq!(half_size, twice_size);
        assert_eq!(half, twice);
    }

    #[test]
    fn a_crop_takes_the_region_it_names_and_nothing_else() {
        let pixels = frame(10, 10);
        let g = with(
            Orientation::AsShot,
            Crop {
                left: 0.2,
                top: 0.4,
                right: 0.5,
                bottom: 0.9,
                ..Crop::default()
            },
        );
        let (out, size) = apply(&g, &pixels, [10, 10]);
        assert_eq!(size, [3, 5]);
        assert_eq!(at(&out, 3, 0, 0), (2.0, 4.0), "the top-left of the crop");
        assert_eq!(
            at(&out, 3, 2, 4),
            (4.0, 8.0),
            "the bottom-right of the crop"
        );
    }

    #[test]
    fn the_rotation_happens_first_and_the_crop_is_read_in_the_rotated_frame() {
        // The ordering decision, made visible. Read the other way round, this
        // crop would name a region of the landscape frame and the result would
        // be the wrong shape — which is the bug a user reports as "my crop
        // jumped when I rotated".
        let pixels = frame(8, 4);
        let g = with(
            Orientation::Rotate90Cw,
            Crop {
                left: 0.0,
                top: 0.0,
                right: 0.5,
                bottom: 0.25,
                ..Crop::default()
            },
        );
        let (out, size) = apply(&g, &pixels, [8, 4]);
        // Rotated the frame is 4x8, so half of *that* width is 2 and a quarter
        // of *that* height is 2.
        assert_eq!(size, [2, 2]);
        assert_eq!(
            at(&out, 2, 0, 0),
            (0.0, 3.0),
            "top-left of the rotated frame"
        );
    }

    #[test]
    fn a_crop_that_rounds_to_nothing_still_has_a_pixel() {
        // A legal crop on a large frame can round to zero on a thumbnail, and a
        // zero-pixel image is not a smaller picture — it is a panic somewhere
        // downstream, in code that has every right to assume a row exists.
        let g = with(
            Orientation::AsShot,
            Crop {
                left: 0.5,
                top: 0.5,
                right: 0.501,
                bottom: 0.501,
                ..Crop::default()
            },
        );
        assert_eq!(g.output_size([8, 8]), [1, 1]);
        let (out, size) = apply(&g, &frame(8, 8), [8, 8]);
        assert_eq!(size, [1, 1]);
        assert_eq!(out.len(), 4);
    }

    fn tilted(degrees: f32) -> Geometry {
        Geometry::from_parts(
            Orientation::AsShot,
            Orientation::AsShot,
            Crop {
                angle_deg: degrees,
                ..Crop::default()
            },
        )
    }

    #[test]
    fn the_weights_always_add_up_to_one() {
        // A filter whose weights do not sum to 1 changes the brightness of the
        // photograph, by an amount that varies with the sub-pixel phase — so a
        // straightened frame would come out very slightly mottled rather than
        // obviously wrong.
        for step in 0..=1000 {
            let t = step as f32 / 1000.0;
            let w = weights(t);
            let sum: f32 = w.iter().sum();
            assert!((sum - 1.0).abs() < 1e-5, "weights at {t} sum to {sum}");
        }
        // And at a whole pixel it is the identity, so a sample that happens to
        // land exactly on a source pixel is that pixel.
        assert_eq!(weights(0.0), [0.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn a_flat_field_stays_flat() {
        // Weights summing to one, checked through the whole path rather than in
        // isolation: mapping, clamping and accumulation all have to agree, and a
        // constant image is the case where any disagreement shows as texture in
        // something that has none.
        let image = [64u32, 48];
        let pixels: Vec<f32> = vec![0.37; 64 * 48 * 4];
        let (out, [ow, oh]) = apply(&tilted(7.0), &pixels, image);
        assert!(ow > 0 && oh > 0);
        for (i, v) in out.iter().enumerate() {
            assert!(
                (v - 0.37).abs() < 1e-5,
                "sample {i} came out {v} on a flat field"
            );
        }
    }

    #[test]
    fn a_linear_ramp_is_reproduced_exactly() {
        // The strongest check available, and the reason it is worth writing the
        // weights out: Catmull-Rom reproduces linear functions exactly, so every
        // output pixel has an *expected value* rather than a tolerance — and it
        // is a value derived from the mapping, so this checks where each pixel
        // was read from and how it was filtered at the same time.
        let image = [80u32, 56];
        let mut pixels = vec![0.0f32; (image[0] * image[1]) as usize * 4];
        for y in 0..image[1] {
            for x in 0..image[0] {
                let at = ((y * image[0] + x) * 4) as usize;
                // Value at the pixel *centre*, which is what the sampler assumes.
                pixels[at] = x as f32 + 0.5;
                pixels[at + 1] = y as f32 + 0.5;
                pixels[at + 2] = 0.0;
                pixels[at + 3] = 1.0;
            }
        }

        let g = tilted(9.0);
        let (out, [ow, oh]) = apply(&g, &pixels, image);
        let mut worst = 0.0f32;
        for y in 0..oh {
            for x in 0..ow {
                let want = g.source_at([x as f32, y as f32], image);
                let at = ((y * ow + x) * 4) as usize;
                worst = worst.max((out[at] - want[0]).abs());
                worst = worst.max((out[at + 1] - want[1]).abs());
            }
        }
        // A thousandth of a pixel: f32 accumulation over sixteen taps, and
        // nothing else. The first version of this allowed two thousandths and
        // still failed at 0.073 — the crop was not reserving room for the
        // filter's own taps, so every straightened frame had a smeared border.
        assert!(worst < 1e-3, "worst departure from the ramp was {worst}");
    }

    #[test]
    fn a_straighten_of_zero_never_reaches_the_resampler() {
        // Cropping alone must stay exact. If a zero angle took the interpolating
        // path it would still be correct — Catmull-Rom passes through every
        // source pixel — but it would no longer be *bit*-identical, and that is
        // the property the crop tests rest on.
        let pixels = frame(9, 7);
        let g = Geometry::from_parts(
            Orientation::AsShot,
            Orientation::Rotate180,
            Crop {
                left: 0.1,
                top: 0.2,
                right: 0.8,
                bottom: 0.9,
                angle_deg: 0.0,
            },
        );
        assert!(!g.resamples());
        let (out, size) = apply(&g, &pixels, [9, 7]);
        for y in 0..size[1] {
            for x in 0..size[0] {
                let [sx, sy] = g.source_of([x, y], [9, 7]);
                assert_eq!(at(&out, size[0], x, y), (sx as f32, sy as f32));
            }
        }
    }
}
