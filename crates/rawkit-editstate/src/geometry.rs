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
    /// What the camera says it takes to stand the frame upright.
    ///
    /// A fact about the file rather than a decision about it, and held
    /// separately for that reason: folding it into `orientation` would put a
    /// per-file value inside the edit, and the same `EditState` would then mean
    /// different things on two photographs. The counterpart of `as_shot_wb`,
    /// which `WhiteBalance::temperature_k == None` resolves to in exactly the
    /// same way.
    recorded: Orientation,
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
    pub fn from_parts(recorded: Orientation, orientation: Orientation, crop: Crop) -> Self {
        Self {
            recorded,
            orientation,
            crop,
        }
    }

    /// Reads only the two fields it needs, rather than holding the whole edit —
    /// so nothing here can accidentally depend on a tone value.
    /// `recorded` is the camera's own orientation, which is not in the edit and
    /// cannot be — see the field. It is a parameter rather than a default so
    /// that a caller which does not have it has to say so, instead of silently
    /// rendering every portrait frame on its side.
    pub fn new(state: &EditState, recorded: Orientation) -> Self {
        Self {
            recorded,
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
        self.turns() == 0 && self.crop.is_full_frame()
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

    /// Quarter-turns clockwise: the camera's, then the user's.
    pub fn turns(&self) -> u32 {
        self.orientation.after(self.recorded).turns()
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

    /// A point of the straightened photograph, in *flat* coordinates.
    ///
    /// Flat is the frame after the quarter turn and the crop's translation but
    /// before the straighten: the space the canvas draws tiles into, because
    /// tiles land there by permutation and stay exact. The straighten is the one
    /// step that has to be a gather, and this is the map it gathers along.
    pub fn flat_of_straight(&self, straight: [f32; 2], image: [u32; 2]) -> [f32; 2] {
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

        // Into the shrunk rectangle, in oriented pixels.
        let px = cx - hx + straight[0] * (2.0 * hx / ow as f32);
        let py = cy - hy + straight[1] * (2.0 * hy / oh as f32);

        // Rotate back about the *frame* centre, not the crop's: a straighten
        // levels the photograph, and moving the crop afterwards must not tilt
        // it again.
        let (sin, cos) = self.angle().sin_cos();
        let (dx, dy) = (px - fw / 2.0, py - fh / 2.0);
        [
            fw / 2.0 + dx * cos + dy * sin - x0 as f32,
            fh / 2.0 - dx * sin + dy * cos - y0 as f32,
        ]
    }

    /// The straight-to-flat map as an affine transform: `[origin, dx, dy]`.
    ///
    /// A point becomes `origin + dx · straight.x + dy · straight.y`. The map is
    /// affine — a scale, a rotation and a translation — so three evaluations
    /// determine it exactly, and **it is measured from
    /// [`flat_of_straight`](Self::flat_of_straight) rather than derived
    /// alongside it**. That matters: the GPU needs the transform and the CPU
    /// needs the map, and re-deriving the same algebra in two places is how a
    /// canvas ends up framing a photograph differently from the file it exports.
    pub fn flat_transform(&self, image: [u32; 2]) -> [[f32; 2]; 3] {
        let origin = self.flat_of_straight([0.0, 0.0], image);
        let along_x = self.flat_of_straight([1.0, 0.0], image);
        let along_y = self.flat_of_straight([0.0, 1.0], image);
        [
            origin,
            [along_x[0] - origin[0], along_x[1] - origin[1]],
            [along_y[0] - origin[0], along_y[1] - origin[1]],
        ]
    }

    /// The flat-space rectangle covering a straight one, as `[x0, y0, x1, y1]`.
    ///
    /// The corners' bounding box. Used to size the buffer the canvas draws tiles
    /// into: too small and the straighten reads past its edge, which shows as a
    /// wedge of black along whichever side the rotation swung out.
    pub fn flat_rect(&self, straight: [f64; 4], image: [u32; 2]) -> [f64; 4] {
        let corners = [
            [straight[0], straight[1]],
            [straight[2], straight[1]],
            [straight[0], straight[3]],
            [straight[2], straight[3]],
        ];
        let mut out = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
        for corner in corners {
            let flat = self.flat_of_straight([corner[0] as f32, corner[1] as f32], image);
            out[0] = out[0].min(flat[0] as f64);
            out[1] = out[1].min(flat[1] as f64);
            out[2] = out[2].max(flat[0] as f64);
            out[3] = out[3].max(flat[1] as f64);
        }
        out
    }

    /// Where an output pixel reads from, to sub-pixel precision.
    ///
    /// In sensor coordinates, and fractional — which is the whole difference a
    /// straighten makes: without one every output pixel lands exactly on a
    /// source pixel and [`source_of`](Self::source_of) answers exactly.
    pub fn source_at(&self, out: [f32; 2], image: [u32; 2]) -> [f32; 2] {
        // Sample at pixel centres, so the first output pixel reads half a pixel
        // in and the last reads half a pixel from the far edge.
        let flat = self.flat_of_straight([out[0] + 0.5, out[1] + 0.5], image);
        let [x0, y0, _, _] = self.window(image);
        // Flat is measured from the crop's corner; the quarter turn is not.
        let oriented = [flat[0] + x0 as f32, flat[1] + y0 as f32];
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

    /// Where a sensor pixel lands in *flat* space — after the quarter turn and
    /// the crop's translation, before the straighten.
    ///
    /// Signed, because a pixel the crop removed lands outside it — and the
    /// canvas needs that answer rather than a clamp, since a tile can straddle
    /// the crop edge and only part of it belongs on screen.
    ///
    /// Named for flat rather than developed because a straightened photograph is
    /// developed and this is not where it ends up: tiles land here by
    /// permutation, and the angle is a separate gather afterwards.
    pub fn flat_of(&self, sensor: [u32; 2], image: [u32; 2]) -> [i64; 2] {
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

    /// The sensor rectangle covering a straightened one, as `[x0, y0, x1, y1]`.
    ///
    /// The corners' bounding box, mapped through the same chain a pixel takes —
    /// so it stays right once an angle is involved, where the old
    /// permutation-only version would have named a rectangle the rotation had
    /// swung out of. Used to turn "what is on screen" into "which tiles", and
    /// too small a rectangle means missing tiles and holes in the canvas.
    pub fn sensor_rect(&self, straight: [f64; 4], image: [u32; 2]) -> [f64; 4] {
        let [x0, y0, _, _] = self.window(image);
        let corner = |sx: f64, sy: f64| -> [f32; 2] {
            let flat = self.flat_of_straight([sx as f32, sy as f32], image);
            self.orient_to_sensor([flat[0] + x0 as f32, flat[1] + y0 as f32], image)
        };
        let mut out = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
        for (sx, sy) in [
            (straight[0], straight[1]),
            (straight[2], straight[1]),
            (straight[0], straight[3]),
            (straight[2], straight[3]),
        ] {
            let [x, y] = corner(sx, sy);
            out[0] = out[0].min(x as f64);
            out[1] = out[1].min(y as f64);
            out[2] = out[2].max(x as f64);
            out[3] = out[3].max(y as f64);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame the camera recorded upright, so these tests keep measuring the
    /// edit's own rotation. Composition has its own tests below.
    fn with(orientation: Orientation, crop: Crop) -> Geometry {
        Geometry {
            recorded: Orientation::AsShot,
            orientation,
            crop,
        }
    }

    fn shot_as(recorded: Orientation, orientation: Orientation) -> Geometry {
        Geometry {
            recorded,
            orientation,
            crop: Crop::default(),
        }
    }

    #[test]
    fn a_frame_the_camera_turned_opens_upright() {
        // The bug this exists for: a portrait exposure from a body that writes
        // landscape pixels rendered on its side, because `AsShot` meant "no
        // rotation" rather than "what the camera recorded".
        let portrait = shot_as(Orientation::Rotate90Cw, Orientation::AsShot);
        assert_eq!(portrait.turns(), 1);
        assert_eq!(
            portrait.output_size([6000, 4000]),
            [4000, 6000],
            "a turned frame is taller than it is wide"
        );
    }

    #[test]
    fn the_users_rotation_turns_the_upright_frame_not_the_sensor() {
        // What makes `[` and `]` behave: a quarter turn is a quarter turn from
        // what you are looking at, not from the sensor's axes.
        assert_eq!(
            shot_as(Orientation::Rotate90Cw, Orientation::Rotate90Cw).turns(),
            2
        );
        assert_eq!(
            shot_as(Orientation::Rotate90Cw, Orientation::Rotate270Cw).turns(),
            0,
            "turning a portrait frame back a quarter lands on the sensor's own axes"
        );
        assert_eq!(
            shot_as(Orientation::Rotate270Cw, Orientation::Rotate180).turns(),
            1
        );
    }

    #[test]
    fn an_upright_frame_with_no_edit_still_has_nothing_to_do() {
        // The identity path hands the render back untouched, and it is keyed on
        // the *composed* turn now. A landscape frame must not lose that.
        assert!(shot_as(Orientation::AsShot, Orientation::AsShot).is_identity());
        // And a portrait one must not claim it.
        assert!(!shot_as(Orientation::Rotate90Cw, Orientation::AsShot).is_identity());
        // Even though the two rotations cancelling out is genuinely nothing to do.
        assert!(shot_as(Orientation::Rotate90Cw, Orientation::Rotate270Cw).is_identity());
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
        // `source_of` is what a still render uses and `flat_of` is what the
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
                        g.flat_of(sensor, image),
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
        // with `flat_of` the canvas would draw each tile rotated the wrong
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
            let base = g.flat_of([2, 1], image);
            for (dx, dy) in [(1u32, 0u32), (0, 1), (3, 2)] {
                let moved = g.flat_of([2 + dx, 1 + dy], image);
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

    #[test]
    fn the_affine_transform_is_the_map_it_was_measured_from() {
        // The GPU gets the transform and the CPU walks the map. If they parted
        // company the canvas would frame the photograph differently from the
        // file it exports, and only a side-by-side comparison would show it.
        let image = [140u32, 90];
        for degrees in [-12.0, -3.0, 0.5, 8.0, 15.0] {
            let g = with(
                Orientation::Rotate90Cw,
                Crop {
                    left: 0.05,
                    top: 0.15,
                    right: 0.85,
                    bottom: 0.95,
                    angle_deg: degrees,
                },
            );
            let [origin, dx, dy] = g.flat_transform(image);
            for (sx, sy) in [(0.0, 0.0), (13.0, 7.0), (60.5, 41.25), (-4.0, 120.0)] {
                let walked = g.flat_of_straight([sx, sy], image);
                let mapped = [
                    origin[0] + dx[0] * sx + dy[0] * sy,
                    origin[1] + dx[1] * sx + dy[1] * sy,
                ];
                for axis in 0..2 {
                    assert!(
                        (walked[axis] - mapped[axis]).abs() < 2e-3,
                        "{degrees}° at ({sx}, {sy}): {walked:?} against {mapped:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_tiles_asked_for_still_cover_a_straightened_view() {
        // `sensor_rect` used to be a permutation, which is right until an angle
        // swings a corner out of the rectangle it named. Too small a rectangle
        // means tiles nobody asked for and holes in the canvas.
        let image = [200u32, 120];
        let g = with(
            Orientation::AsShot,
            Crop {
                angle_deg: 12.0,
                ..Crop::default()
            },
        );
        let [ow, oh] = g.output_size(image);
        let seen = [0.0, 0.0, ow as f64, oh as f64];
        let [rx0, ry0, rx1, ry1] = g.sensor_rect(seen, image);
        for y in 0..oh {
            for x in 0..ow {
                let [sx, sy] = g.source_at([x as f32, y as f32], image);
                assert!(
                    (rx0..=rx1).contains(&(sx as f64)) && (ry0..=ry1).contains(&(sy as f64)),
                    "({x}, {y}) reads ({sx}, {sy}), outside [{rx0}, {ry0}, {rx1}, {ry1}]"
                );
            }
        }
    }
}
