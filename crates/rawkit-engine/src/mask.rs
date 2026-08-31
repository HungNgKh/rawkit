//! Turning a local adjustment's shape into a picture of where it applies.
//!
//! # Why a raster and not a formula
//!
//! A linear gradient is two lines of arithmetic and could be evaluated in the
//! shader directly. It is rasterised here instead, on purpose, because the shape
//! is not the point: the point is that the renderer takes a *picture* of where
//! an adjustment applies and never asks where it came from. A gradient, a brush
//! stroke, a luminance range, a depth band and a segmentation matte all arrive
//! as the same thing, and only the first of those can be written as a formula.
//!
//! Building the shader around the formula and adding the rest later is the
//! rewrite this avoids — the mask *source* is the thing that varies, and it is
//! the thing kept on this side of the boundary.
//!
//! # The resolution, and why it is bounded
//!
//! A mask is a soft, low-frequency thing: a gradient across a sky, the falloff
//! of a brush. It does not need a texel per pixel, and giving it one would cost
//! 24 MB of upload every time somebody dragged the gradient. So it is capped on
//! its longest edge and sampled bilinearly, indexed in image coordinates
//! exactly as [`crate::guide`] is — which is what makes it agree across tile
//! boundaries and across zoom levels.
//!
//! A hard-edged matte from a segmentation model would want more than this, and
//! that is a constant to raise rather than an arrangement to redo.

use rawkit_editstate::{Mask, MaskShape};

/// The longest edge a mask raster is built at.
pub const MAX_EDGE: u32 = 1024;

/// The size a mask raster is for an image of this shape.
///
/// Bounded on the longest edge, never larger than the image, and at least one
/// texel in each direction so a very narrow frame still has somewhere to put
/// the answer.
pub fn dimensions(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    let scale = if longest > MAX_EDGE {
        f64::from(MAX_EDGE) / f64::from(longest)
    } else {
        1.0
    };
    let edge = |v: u32| ((f64::from(v) * scale).round() as u32).clamp(1, v.max(1));
    (edge(width), edge(height))
}

/// Draw one mask, as a value per texel from 0 (outside) to 1 (full effect).
///
/// The coordinates in the shape are fractions of the sensor frame, so this needs
/// no knowledge of the crop or the orientation — which is the reason they are
/// stored that way.
pub fn rasterise(mask: &Mask, width: u32, height: u32, out: &mut [f32]) {
    let (w, h) = dimensions(width, height);
    debug_assert!(out.len() >= (w * h) as usize);
    match mask.shape {
        MaskShape::Linear { from, to } => {
            // Distance along the gradient's axis, as a fraction of its length.
            // Everything is done in the *frame's* proportions rather than in
            // texel counts, so the gradient is not sheared by a non-square
            // raster — the raster's aspect follows the image's, but only
            // approximately once both edges are rounded to whole texels.
            let axis = [to[0] - from[0], to[1] - from[1]];
            let length = axis[0] * axis[0] + axis[1] * axis[1];
            if length <= f32::MIN_POSITIVE {
                out[..(w * h) as usize].fill(0.0);
                return;
            }
            for y in 0..h {
                // Texel centres, so the gradient does not sit half a texel out.
                let v = (y as f32 + 0.5) / h as f32 - from[1];
                for x in 0..w {
                    let u = (x as f32 + 0.5) / w as f32 - from[0];
                    let t = (u * axis[0] + v * axis[1]) / length;
                    // Smooth at both ends. A plain ramp is continuous but its
                    // slope is not, and a slope that changes abruptly across a
                    // clear sky is exactly the kind of edge the eye finds.
                    let s = t.clamp(0.0, 1.0);
                    out[(y * w + x) as usize] = 1.0 - s * s * (3.0 - 2.0 * s);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(mask: &Mask, width: u32, height: u32) -> (Vec<f32>, u32, u32) {
        let (w, h) = dimensions(width, height);
        let mut out = vec![0.0f32; (w * h) as usize];
        rasterise(mask, width, height, &mut out);
        (out, w, h)
    }

    fn at(pixels: &[f32], w: u32, x: u32, y: u32) -> f32 {
        pixels[(y * w + x) as usize]
    }

    #[test]
    fn a_gradient_is_full_at_one_end_and_absent_at_the_other() {
        // The shape of a graduated filter: beyond the near line the effect is
        // whole, beyond the far line it is gone, and it covers the frame rather
        // than a band across the middle of it.
        let mask = Mask {
            shape: MaskShape::Linear {
                from: [0.5, 0.2],
                to: [0.5, 0.8],
            },
            ..Mask::default()
        };
        let (pixels, w, h) = draw(&mask, 800, 600);
        assert_eq!(
            at(&pixels, w, w / 2, 0),
            1.0,
            "the top is not fully covered"
        );
        assert_eq!(at(&pixels, w, w / 2, h - 1), 0.0, "the bottom is not clear");
        let middle = at(&pixels, w, w / 2, h / 2);
        assert!(
            (middle - 0.5).abs() < 0.05,
            "half way along the axis reads {middle:.3}, not a half"
        );
    }

    #[test]
    fn a_gradient_does_not_vary_across_its_own_direction() {
        // A graduated filter is constant along the line it is drawn on. If the
        // raster's own aspect leaked into the arithmetic, a gradient drawn
        // straight down would come out tilted.
        let mask = Mask {
            shape: MaskShape::Linear {
                from: [0.5, 0.1],
                to: [0.5, 0.9],
            },
            ..Mask::default()
        };
        let (pixels, w, h) = draw(&mask, 1600, 900);
        for y in [h / 4, h / 2, 3 * h / 4] {
            let first = at(&pixels, w, 0, y);
            for x in 0..w {
                assert!(
                    (at(&pixels, w, x, y) - first).abs() < 1e-6,
                    "row {y} varies along itself"
                );
            }
        }
    }

    #[test]
    fn a_diagonal_gradient_runs_along_its_axis() {
        // Drawn corner to corner: the two *other* corners sit on the same line
        // through the middle, so they must read the same, and the ends must be
        // the extremes.
        let mask = Mask {
            shape: MaskShape::Linear {
                from: [0.0, 0.0],
                to: [1.0, 1.0],
            },
            ..Mask::default()
        };
        let (pixels, w, h) = draw(&mask, 1000, 1000);
        assert!(at(&pixels, w, 0, 0) > 0.99);
        assert!(at(&pixels, w, w - 1, h - 1) < 0.01);
        let (a, b) = (at(&pixels, w, w - 1, 0), at(&pixels, w, 0, h - 1));
        assert!(
            (a - b).abs() < 0.01,
            "the off-axis corners disagree: {a:.3} and {b:.3}"
        );
    }

    #[test]
    fn the_raster_is_bounded_and_never_larger_than_the_image() {
        for (w, h) in [(6024, 4024), (4024, 6024), (900, 600), (3, 2), (1, 1)] {
            let (gw, gh) = dimensions(w, h);
            assert!(gw >= 1 && gh >= 1, "{w}x{h} gave an empty raster");
            assert!(gw <= MAX_EDGE && gh <= MAX_EDGE, "{w}x{h} exceeded the cap");
            assert!(gw <= w && gh <= h, "{w}x{h} was enlarged to {gw}x{gh}");
        }
    }

    #[test]
    fn a_gradient_with_no_direction_covers_nothing() {
        // Rejected by `EditState::validate` before it gets here, and answered
        // anyway: a shape with no axis has no inside, and dividing by its length
        // would fill the frame with NaN.
        let mask = Mask {
            shape: MaskShape::Linear {
                from: [0.5, 0.5],
                to: [0.5, 0.5],
            },
            ..Mask::default()
        };
        let (pixels, _, _) = draw(&mask, 400, 400);
        assert!(pixels.iter().all(|v| *v == 0.0));
    }
}
