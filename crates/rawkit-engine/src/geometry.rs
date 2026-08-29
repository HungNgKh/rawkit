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
        Geometry::from_parts(orientation, crop)
    }

    #[test]
    fn the_identity_hands_back_exactly_what_it_was_given() {
        // The property that keeps an unedited photograph unchanged by the
        // existence of this module.
        let pixels = frame(7, 5);
        let (out, size) = apply(&Geometry::new(&EditState::default()), &pixels, [7, 5]);
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
            },
        );
        assert_eq!(g.output_size([8, 8]), [1, 1]);
        let (out, size) = apply(&g, &frame(8, 8), [8, 8]);
        assert_eq!(size, [1, 1]);
        assert_eq!(out.len(), 4);
    }
}
