//! Orientation and crop: which pixels, and in what order.
//!
//! # Why this is not a pipeline stage
//!
//! Every entry in [`Stage`](crate::pipeline::Stage) transforms pixel *values*
//! and has a [`Domain`](crate::pipeline::Domain) saying what those values mean.
//! Orientation and crop transform neither: they select a region and permute
//! axes, and every value that survives comes through bit-for-bit. Giving them a
//! stage would mean inventing a domain for an operation that does not have one,
//! and the pipeline's whole value is that its declared order is the real order
//! of things that *change colour*.
//!
//! So geometry wraps the pipeline instead of sitting inside it. The renderer
//! develops the frame, and this decides what part of the result is the
//! photograph.
//!
//! # Exact, and worth keeping exact
//!
//! Because nothing is resampled, a cropped export is bit-identical to the same
//! region of an uncropped one. That is a property worth defending: the moment a
//! crop costs a resample, "I cropped it slightly" becomes "I re-rendered it
//! slightly differently", and there is no way to explain that to someone
//! comparing two exports. Free rotation — straighten — *does* need resampling,
//! which is exactly why it is not here yet: it is a different kind of operation
//! and deserves its own decisions about interpolation.

use rawkit_editstate::{Crop, EditState, Orientation};

/// The frame the edit says to show, given the frame the sensor recorded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    orientation: Orientation,
    crop: Crop,
}

impl Geometry {
    /// Reads only the two fields it needs, rather than holding the whole edit —
    /// so nothing here can accidentally depend on a tone value.
    pub fn new(state: &EditState) -> Self {
        Self {
            orientation: state.orientation,
            crop: state.crop,
        }
    }

    /// Whether there is nothing to do.
    ///
    /// Checked so the identity path can hand back the render untouched, which
    /// keeps "an unedited photograph is bit-identical to one from a build before
    /// this existed" true rather than approximately true.
    pub fn is_identity(&self) -> bool {
        self.orientation == Orientation::AsShot && self.crop.is_full_frame()
    }

    /// Quarter-turns clockwise.
    fn turns(&self) -> u32 {
        match self.orientation {
            Orientation::AsShot => 0,
            Orientation::Rotate90Cw => 1,
            Orientation::Rotate180 => 2,
            Orientation::Rotate270Cw => 3,
        }
    }

    /// The frame's size after rotation, before cropping.
    fn oriented_size(&self, image: [u32; 2]) -> [u32; 2] {
        if self.turns() % 2 == 1 {
            [image[1], image[0]]
        } else {
            image
        }
    }

    /// The crop rectangle in oriented pixels, as `[x0, y0, x1, y1]`.
    ///
    /// Always at least one pixel on each axis. A crop can be legal — `left`
    /// strictly less than `right` — and still round to nothing on a small
    /// enough thumbnail, and a zero-pixel image is not a smaller picture, it is
    /// a crash somewhere downstream.
    fn window(&self, image: [u32; 2]) -> [u32; 4] {
        let [ow, oh] = self.oriented_size(image);
        let edge = |fraction: f32, extent: u32| -> u32 {
            (fraction * extent as f32).round().clamp(0.0, extent as f32) as u32
        };
        let x0 = edge(self.crop.left, ow).min(ow.saturating_sub(1));
        let y0 = edge(self.crop.top, oh).min(oh.saturating_sub(1));
        let x1 = edge(self.crop.right, ow).clamp(x0 + 1, ow.max(1));
        let y1 = edge(self.crop.bottom, oh).clamp(y0 + 1, oh.max(1));
        [x0, y0, x1, y1]
    }

    /// The size of the developed photograph.
    pub fn output_size(&self, image: [u32; 2]) -> [u32; 2] {
        let [x0, y0, x1, y1] = self.window(image);
        [x1 - x0, y1 - y0]
    }

    /// Where an output pixel came from, in sensor coordinates.
    ///
    /// The inverse direction on purpose: a forward map has to worry about which
    /// source pixels land nowhere, and this way every output pixel is filled
    /// exactly once by construction.
    pub fn source_of(&self, out: [u32; 2], image: [u32; 2]) -> [u32; 2] {
        let [x0, y0, _, _] = self.window(image);
        let (ox, oy) = (x0 + out[0], y0 + out[1]);
        let [w, h] = image;
        match self.turns() {
            1 => [oy, h - 1 - ox],
            2 => [w - 1 - ox, h - 1 - oy],
            3 => [w - 1 - oy, ox],
            _ => [ox, oy],
        }
    }

    /// Rearrange a developed RGBA frame into the photograph.
    ///
    /// Returns the pixels and their size together, because the two are only
    /// correct as a pair — a caller holding cropped pixels and the sensor's
    /// width indexes into the wrong row and gets a picture that looks sheared.
    pub fn apply(&self, rgba: &[f32], image: [u32; 2]) -> (Vec<f32>, [u32; 2]) {
        if self.is_identity() {
            return (rgba.to_vec(), image);
        }
        let [ow, oh] = self.output_size(image);
        let mut out = vec![0.0f32; (ow as usize) * (oh as usize) * 4];
        for y in 0..oh {
            for x in 0..ow {
                let [sx, sy] = self.source_of([x, y], image);
                let from = ((sy as usize) * image[0] as usize + sx as usize) * 4;
                let to = ((y as usize) * ow as usize + x as usize) * 4;
                out[to..to + 4].copy_from_slice(&rgba[from..from + 4]);
            }
        }
        (out, [ow, oh])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        Geometry { orientation, crop }
    }

    #[test]
    fn the_identity_hands_back_exactly_what_it_was_given() {
        // The property that keeps an unedited photograph unchanged by the
        // existence of this module.
        let pixels = frame(7, 5);
        let (out, size) = Geometry::new(&EditState::default()).apply(&pixels, [7, 5]);
        assert_eq!(size, [7, 5]);
        assert_eq!(out, pixels);
    }

    #[test]
    fn a_quarter_turn_moves_the_top_left_corner_to_the_top_right() {
        // Stated as a corner rather than as a formula, because the formula is
        // what is being tested and the two would agree by construction.
        let pixels = frame(4, 3);
        let g = with(Orientation::Rotate90Cw, Crop::default());
        let (out, size) = g.apply(&pixels, [4, 3]);
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
            let (next, next_size) =
                with(Orientation::Rotate90Cw, Crop::default()).apply(&pixels, size);
            pixels = next;
            size = next_size;
        }
        assert_eq!(size, [w, h]);
        assert_eq!(pixels, frame(w, h));
    }

    #[test]
    fn a_half_turn_is_two_quarter_turns() {
        let pixels = frame(4, 3);
        let (half, half_size) =
            with(Orientation::Rotate180, Crop::default()).apply(&pixels, [4, 3]);
        let (once, size) = with(Orientation::Rotate90Cw, Crop::default()).apply(&pixels, [4, 3]);
        let (twice, twice_size) = with(Orientation::Rotate90Cw, Crop::default()).apply(&once, size);
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
        let (out, size) = g.apply(&pixels, [10, 10]);
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
        let (out, size) = g.apply(&pixels, [8, 4]);
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
    fn the_same_crop_names_the_same_region_at_any_resolution() {
        // What fractions are *for*. The same stored edit is applied to a
        // full-resolution export and to a thumbnail, and if these disagreed the
        // grid would show a different photograph from the one exported.
        let crop = Crop {
            left: 0.25,
            top: 0.5,
            right: 0.75,
            bottom: 1.0,
        };
        let g = with(Orientation::AsShot, crop);
        assert_eq!(g.output_size([400, 200]), [200, 100]);
        assert_eq!(g.output_size([40, 20]), [20, 10]);
        // And the corner lands proportionally in the same place.
        assert_eq!(g.source_of([0, 0], [400, 200]), [100, 100]);
        assert_eq!(g.source_of([0, 0], [40, 20]), [10, 10]);
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
        let (out, size) = g.apply(&frame(8, 8), [8, 8]);
        assert_eq!(size, [1, 1]);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn every_output_pixel_comes_from_inside_the_frame() {
        // The arithmetic has four branches and off-by-ones in the two that
        // subtract are exactly the kind that produce one wrong edge column,
        // which is easy to miss by eye on a photograph.
        let image = [9u32, 5];
        for orientation in [
            Orientation::AsShot,
            Orientation::Rotate90Cw,
            Orientation::Rotate180,
            Orientation::Rotate270Cw,
        ] {
            let g = with(orientation, Crop::default());
            let [ow, oh] = g.output_size(image);
            let mut seen = vec![false; (image[0] * image[1]) as usize];
            for y in 0..oh {
                for x in 0..ow {
                    let [sx, sy] = g.source_of([x, y], image);
                    assert!(
                        sx < image[0] && sy < image[1],
                        "{orientation:?} read outside"
                    );
                    let slot = (sy * image[0] + sx) as usize;
                    assert!(!seen[slot], "{orientation:?} read ({sx}, {sy}) twice");
                    seen[slot] = true;
                }
            }
            assert!(
                seen.iter().all(|&s| s),
                "{orientation:?} left a pixel behind"
            );
        }
    }
}
