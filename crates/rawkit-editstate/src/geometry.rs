//! Orientation and crop, as a coordinate map.
//!
//! # Why this is here and not in the renderer
//!
//! Two things need the same answer: the renderer, which rearranges a developed
//! frame, and `rawkit-session`, which decides what is on screen and which tiles
//! to ask for. `rawkit-session` deliberately cannot see `rawkit-engine` — it
//! holds no pixel type and no GPU handle, and that is invariant 9 — so a map
//! living in the engine would have to be written twice, and two copies of a
//! coordinate transform is how a canvas ends up framing a photograph differently
//! from the file it exports.
//!
//! Nothing here touches a pixel. It is arithmetic over two `EditState` fields,
//! which is why it belongs beside them.
//!
//! # Why this is not a pipeline stage
//!
//! Every entry in `rawkit_engine::pipeline::Stage` transforms pixel *values* and
//! carries a `Domain` saying what those values mean. Orientation and crop
//! transform neither: they select a region and permute axes, and every value
//! that survives comes through bit-for-bit. Giving them a stage would mean
//! inventing a domain for an operation that does not have one, and the
//! pipeline's whole value is that its declared order is the real order of things
//! that change colour.
//!
//! # Exact, and worth keeping exact
//!
//! Because nothing is resampled, a cropped export is bit-identical to the same
//! region of an uncropped one. That is worth defending: the moment a crop costs
//! a resample, "I cropped it slightly" becomes "I re-rendered it slightly
//! differently", and there is no way to explain that to someone comparing two
//! exports. Free rotation — straighten — *does* need resampling, which is
//! exactly why it is not here: a different kind of operation, with its own
//! decisions about interpolation.

use crate::{Crop, EditState, Orientation};

/// The frame the edit says to show, given the frame the sensor recorded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    orientation: Orientation,
    crop: Crop,
}

impl Geometry {
    /// From the two parts directly.
    ///
    /// Public because a crop is *proposed* before it is stored: the overlay
    /// drags a rectangle and wants to know what the frame would become, and
    /// building a whole `EditState` to ask that would mean the interface could
    /// accidentally answer with a different edit's orientation.
    pub fn from_parts(orientation: Orientation, crop: Crop) -> Self {
        Self { orientation, crop }
    }

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
    pub fn turns(&self) -> u32 {
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

    /// Where a sensor pixel lands in the developed frame.
    ///
    /// Signed, because a pixel the crop removed lands outside it — and the
    /// canvas needs that answer rather than a clamp, since a tile can straddle
    /// the crop edge and only part of it belongs on screen.
    pub fn developed_of(&self, sensor: [u32; 2], image: [u32; 2]) -> [i64; 2] {
        let [x0, y0, _, _] = self.window(image);
        let (sx, sy) = (sensor[0] as i64, sensor[1] as i64);
        let (w, h) = (image[0] as i64, image[1] as i64);
        let (ox, oy) = match self.turns() {
            1 => (h - 1 - sy, sx),
            2 => (w - 1 - sx, h - 1 - sy),
            3 => (sy, w - 1 - sx),
            _ => (sx, sy),
        };
        [ox - x0 as i64, oy - y0 as i64]
    }

    /// How a local offset within a tile moves once the frame is rotated, as the
    /// two columns of a signed permutation.
    ///
    /// The canvas blits a tile by adding this to the tile's corner rather than
    /// by re-deriving the rotation per pixel — one matrix, computed once, so the
    /// GPU does two multiplies instead of a branch.
    pub fn axes(&self) -> [[i32; 2]; 2] {
        match self.turns() {
            // Local (gx, gy) becomes (-gy, gx), and so on round.
            1 => [[0, 1], [-1, 0]],
            2 => [[-1, 0], [0, -1]],
            3 => [[0, -1], [1, 0]],
            _ => [[1, 0], [0, 1]],
        }
    }

    /// The sensor rectangle covering a developed one, as `[x0, y0, x1, y1]`.
    ///
    /// Exact rather than conservative: the map is a signed permutation plus a
    /// translation, so the corners' bounding box *is* the image of the
    /// rectangle. Used to turn "what is on screen" into "which tiles".
    pub fn sensor_rect(&self, developed: [f64; 4], image: [u32; 2]) -> [f64; 4] {
        let [x0, y0, _, _] = self.window(image);
        let (w, h) = (image[0] as f64, image[1] as f64);
        let corner = |dx: f64, dy: f64| -> [f64; 2] {
            let (ox, oy) = (dx + x0 as f64, dy + y0 as f64);
            match self.turns() {
                1 => [oy, h - ox],
                2 => [w - ox, h - oy],
                3 => [w - oy, ox],
                _ => [ox, oy],
            }
        };
        let a = corner(developed[0], developed[1]);
        let b = corner(developed[2], developed[3]);
        [
            a[0].min(b[0]),
            a[1].min(b[1]),
            a[0].max(b[0]),
            a[1].max(b[1]),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(orientation: Orientation, crop: Crop) -> Geometry {
        Geometry { orientation, crop }
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
    fn the_two_directions_agree_with_each_other() {
        // `source_of` is what a still render uses and `developed_of` is what the
        // canvas uses. They are separate code, and the failure if they disagree
        // is the worst kind: the canvas shows one framing and the export writes
        // another, so the user only finds out after the file is on disk.
        let image = [11u32, 7];
        for orientation in [
            Orientation::AsShot,
            Orientation::Rotate90Cw,
            Orientation::Rotate180,
            Orientation::Rotate270Cw,
        ] {
            let g = with(
                orientation,
                Crop {
                    left: 0.1,
                    top: 0.2,
                    right: 0.8,
                    bottom: 0.9,
                },
            );
            let [ow, oh] = g.output_size(image);
            for y in 0..oh {
                for x in 0..ow {
                    let sensor = g.source_of([x, y], image);
                    assert_eq!(
                        g.developed_of(sensor, image),
                        [x as i64, y as i64],
                        "{orientation:?} round trip at ({x}, {y})"
                    );
                }
            }
        }
    }

    #[test]
    fn the_axes_match_the_rotation_they_describe() {
        // `axes` is handed to the GPU and applied per pixel; if it disagreed
        // with `developed_of` the canvas would draw each tile rotated the wrong
        // way inside a correctly-placed rectangle, which looks like corruption
        // rather than like a rotation bug.
        let image = [9u32, 6];
        for orientation in [
            Orientation::AsShot,
            Orientation::Rotate90Cw,
            Orientation::Rotate180,
            Orientation::Rotate270Cw,
        ] {
            let g = with(orientation, Crop::default());
            let [ax, ay] = g.axes();
            let base = g.developed_of([2, 1], image);
            for (dx, dy) in [(1u32, 0u32), (0, 1), (3, 2)] {
                let moved = g.developed_of([2 + dx, 1 + dy], image);
                let predicted = [
                    base[0] + (ax[0] * dx as i32 + ay[0] * dy as i32) as i64,
                    base[1] + (ax[1] * dx as i32 + ay[1] * dy as i32) as i64,
                ];
                assert_eq!(moved, predicted, "{orientation:?} offset ({dx}, {dy})");
            }
        }
    }

    #[test]
    fn a_developed_rectangle_maps_back_to_the_sensor_pixels_under_it() {
        // What turns "what is on screen" into "which tiles". Too small a
        // rectangle means missing tiles and holes in the canvas.
        let image = [100u32, 60];
        for orientation in [
            Orientation::AsShot,
            Orientation::Rotate90Cw,
            Orientation::Rotate180,
            Orientation::Rotate270Cw,
        ] {
            let g = with(orientation, Crop::default());
            let [ow, oh] = g.output_size(image);
            let want = [3.0, 4.0, ow as f64 / 2.0, oh as f64 / 2.0];
            let [sx0, sy0, sx1, sy1] = g.sensor_rect(want, image);
            for y in 4..(oh / 2) {
                for x in 3..(ow / 2) {
                    let [sx, sy] = g.source_of([x, y], image);
                    assert!(
                        (sx0..sx1).contains(&(sx as f64)) && (sy0..sy1).contains(&(sy as f64)),
                        "{orientation:?}: ({x}, {y}) reads ({sx}, {sy}), outside the rect"
                    );
                }
            }
        }
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
