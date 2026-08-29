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

    /// The straighten, in radians clockwise. Zero when there is none.
    pub fn angle(&self) -> f32 {
        self.crop.angle_deg.to_radians()
    }

    /// Whether anything here needs a pixel to be interpolated.
    ///
    /// Quarter turns and crops select and permute; only a straighten resamples.
    /// Kept separate from [`is_identity`](Self::is_identity) because the exact
    /// path is worth taking whenever it is available, not only when there is
    /// nothing to do at all.
    pub fn resamples(&self) -> bool {
        self.crop.angle_deg != 0.0
    }

    /// How far outside a sample the renderer's filter reaches, in pixels.
    ///
    /// Catmull-Rom takes four taps per axis, so a sample at the very edge of the
    /// frame would read two pixels past it. [`fit_scale`](Self::fit_scale) keeps
    /// that margin clear, which is why the renderer's edge clamp is a guard
    /// against rounding rather than a thing that happens: without it, the
    /// outermost pixels of a straightened frame are a smear of the edge row and
    /// no longer a faithful resample.
    ///
    /// A filter with a wider support than this needs this number raised with it.
    const FILTER_MARGIN: f32 = 2.0;

    /// How far the crop has to pull in so no empty corner is visible.
    ///
    /// Between 0 and 1, scaling the rectangle about its own centre. Solved
    /// rather than searched: each corner sits at `centre + s · offset`, so
    /// "inside the frame" is linear in `s` and the tightest of the eight bounds
    /// is the answer. A search would need a tolerance, and a tolerance here is a
    /// sliver of empty corner in an export.
    pub fn fit_scale(&self, image: [u32; 2]) -> f32 {
        if !self.resamples() {
            return 1.0;
        }
        let [fw, fh] = self.oriented_size(image);
        let (fw, fh) = (fw as f32, fh as f32);
        let [x0, y0, x1, y1] = self.window(image);
        let (cx, cy) = ((x0 + x1) as f32 / 2.0, (y0 + y1) as f32 / 2.0);
        let (hx, hy) = ((x1 - x0) as f32 / 2.0, (y1 - y0) as f32 / 2.0);

        let (sin, cos) = self.angle().sin_cos();
        // Where the crop's own centre reads from. Everything else is measured
        // from here.
        let (dcx, dcy) = (cx - fw / 2.0, cy - fh / 2.0);
        let centre = [
            fw / 2.0 + dcx * cos + dcy * sin,
            fh / 2.0 - dcx * sin + dcy * cos,
        ];

        let mut scale = 1.0f32;
        for (ox, oy) in [(-hx, -hy), (hx, -hy), (-hx, hy), (hx, hy)] {
            // The corner's offset from the centre, rotated the same way.
            let offset = [ox * cos + oy * sin, -ox * sin + oy * cos];
            for axis in 0..2 {
                let extent = if axis == 0 { fw } else { fh };
                // Inset by the filter's reach, not by nothing: the corner has to
                // be far enough in that its *taps* are on the frame too.
                let (low, high) = (Self::FILTER_MARGIN, extent - Self::FILTER_MARGIN);
                let (at, step) = (centre[axis], offset[axis]);
                if step > 0.0 {
                    scale = scale.min((high - at) / step);
                } else if step < 0.0 {
                    scale = scale.min((low - at) / step);
                }
            }
        }
        scale.clamp(0.0, 1.0)
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
    ///
    /// Smaller than the rectangle asked for when a straighten is in force: the
    /// crop pulls in to keep the empty corners out, and the output shrinks with
    /// it rather than being scaled back up, so one output pixel still stands for
    /// about one sensor pixel.
    pub fn output_size(&self, image: [u32; 2]) -> [u32; 2] {
        let [x0, y0, x1, y1] = self.window(image);
        let scale = self.fit_scale(image);
        [
            (((x1 - x0) as f32 * scale) as u32).max(1),
            (((y1 - y0) as f32 * scale) as u32).max(1),
        ]
    }

    /// Where an output pixel reads from, to sub-pixel precision.
    ///
    /// In sensor coordinates, and fractional — which is the whole difference a
    /// straighten makes: without one every output pixel lands exactly on a
    /// source pixel and [`source_of`](Self::source_of) answers exactly.
    pub fn source_at(&self, out: [f32; 2], image: [u32; 2]) -> [f32; 2] {
        let [fw, fh] = self.oriented_size(image);
        let (fw, fh) = (fw as f32, fh as f32);
        let [x0, y0, x1, y1] = self.window(image);
        let (cx, cy) = ((x0 + x1) as f32 / 2.0, (y0 + y1) as f32 / 2.0);
        let scale = self.fit_scale(image);
        let (hx, hy) = (
            (x1 - x0) as f32 / 2.0 * scale,
            (y1 - y0) as f32 / 2.0 * scale,
        );
        let [ow, oh] = self.output_size(image);

        // Sample at pixel centres, so the first output pixel reads half a pixel
        // in and the last reads half a pixel from the far edge.
        let px = cx - hx + (out[0] + 0.5) * (2.0 * hx / ow as f32);
        let py = cy - hy + (out[1] + 0.5) * (2.0 * hy / oh as f32);

        // Rotate back about the *frame* centre, not the crop's: a straighten
        // levels the photograph, and moving the crop afterwards must not tilt
        // it again.
        let (sin, cos) = self.angle().sin_cos();
        let (dx, dy) = (px - fw / 2.0, py - fh / 2.0);
        let oriented = [
            fw / 2.0 + dx * cos + dy * sin,
            fh / 2.0 - dx * sin + dy * cos,
        ];
        self.orient_to_sensor(oriented, image)
    }

    /// The quarter-turn half of the map, in floating point.
    fn orient_to_sensor(&self, p: [f32; 2], image: [u32; 2]) -> [f32; 2] {
        let (w, h) = (image[0] as f32, image[1] as f32);
        match self.turns() {
            1 => [p[1], h - p[0]],
            2 => [w - p[0], h - p[1]],
            3 => [w - p[1], p[0]],
            _ => p,
        }
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
            ..Crop::default()
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
                    ..Crop::default()
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

    fn tilted(degrees: f32) -> Geometry {
        with(
            Orientation::AsShot,
            Crop {
                angle_deg: degrees,
                ..Crop::default()
            },
        )
    }

    #[test]
    fn no_angle_means_no_resampling_and_no_shrinking() {
        // The exact path has to stay available: cropping alone still lands every
        // output pixel on exactly one source pixel, and that is what keeps a
        // cropped export bit-identical to the region it kept.
        let g = tilted(0.0);
        assert!(!g.resamples());
        assert_eq!(g.fit_scale([100, 60]), 1.0);
        assert_eq!(g.output_size([100, 60]), [100, 60]);
    }

    #[test]
    fn a_positive_angle_turns_the_photograph_clockwise() {
        // Stated as a direction rather than as the formula, because the formula
        // is what is under test — and because a sign error here is invisible
        // until somebody straightens a horizon the wrong way.
        let image = [100u32, 100];
        let g = tilted(10.0);
        let [ow, oh] = g.output_size(image);
        let top_middle = g.source_at([ow as f32 / 2.0, 0.0], image);
        assert!(
            top_middle[0] < 50.0,
            "the top of the frame should read from left of centre, got {top_middle:?}"
        );
        let left_middle = g.source_at([0.0, oh as f32 / 2.0], image);
        assert!(
            left_middle[1] > 50.0,
            "the left of the frame should read from below centre, got {left_middle:?}"
        );
    }

    #[test]
    fn the_crop_pulls_in_far_enough_that_no_corner_is_empty() {
        // The reason `fit_scale` exists. An empty corner in an export is a
        // mistake that is easy to make and hard to notice, so this checks the
        // property rather than the formula: every pixel of the output, at every
        // angle, reads from inside the frame.
        for degrees in [-15.0, -7.5, -1.0, 1.0, 7.5, 15.0] {
            for image in [[100u32, 60], [60, 100], [80, 80]] {
                let g = tilted(degrees);
                assert!(g.fit_scale(image) < 1.0, "{degrees} did not shrink");
                let [ow, oh] = g.output_size(image);
                for y in 0..oh {
                    for x in 0..ow {
                        let [sx, sy] = g.source_at([x as f32, y as f32], image);
                        // Two pixels inside, not merely inside: the renderer's
                        // filter reaches that far, and a sample at the very edge
                        // would read a clamped row rather than the photograph.
                        let m = Geometry::FILTER_MARGIN;
                        assert!(
                            sx >= m - 1e-3
                                && sy >= m - 1e-3
                                && sx <= image[0] as f32 - m + 1e-3
                                && sy <= image[1] as f32 - m + 1e-3,
                            "{degrees}° on {image:?}: ({x}, {y}) reads ({sx}, {sy})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn straightening_composes_with_a_quarter_turn_and_a_crop() {
        // Three geometric operations at once is where an ordering mistake
        // hides — and the answer still has to be inside the sensor.
        let image = [120u32, 80];
        let g = with(
            Orientation::Rotate270Cw,
            Crop {
                left: 0.2,
                top: 0.1,
                right: 0.9,
                bottom: 0.8,
                angle_deg: -6.0,
            },
        );
        let [ow, oh] = g.output_size(image);
        assert!(ow > 0 && oh > 0);
        for (x, y) in [(0, 0), (ow - 1, 0), (0, oh - 1), (ow - 1, oh - 1)] {
            let [sx, sy] = g.source_at([x as f32, y as f32], image);
            assert!(
                (0.0..=image[0] as f32).contains(&sx) && (0.0..=image[1] as f32).contains(&sy),
                "corner ({x}, {y}) reads ({sx}, {sy})"
            );
        }
    }
}
