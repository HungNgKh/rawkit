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

use crate::guide::Guide;
use rawkit_editstate::{Mask, MaskShape, RangeChannel, Stroke};

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
pub fn rasterise(mask: &Mask, width: u32, height: u32, guide: &Guide, out: &mut [f32]) {
    let (w, h) = dimensions(width, height);
    debug_assert!(out.len() >= (w * h) as usize);
    draw(&mask.shape, w, h, guide, out);
    if mask.invert {
        // Applied to the finished weight rather than inside each shape, so it
        // means the same thing for every source there will ever be — and so the
        // shader stays a thing that composites a picture without knowing what
        // made it.
        for v in &mut out[..(w * h) as usize] {
            *v = 1.0 - *v;
        }
    }
}

fn draw(shape: &MaskShape, w: u32, h: u32, guide: &Guide, out: &mut [f32]) {
    match *shape {
        MaskShape::Brush {
            ref strokes,
            feather,
        } => {
            out[..(w * h) as usize].fill(0.0);
            // In order, because that is what makes erasing mean anything: paint,
            // take back the overshoot, paint again.
            for stroke in strokes {
                paint(stroke, feather, w, h, out);
            }
        }

        MaskShape::Range {
            channel,
            from,
            to,
            feather,
        } => {
            let band = Band {
                channel,
                from,
                to,
                feather,
            };
            draw_range(band, w, h, guide, out);
        }

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

        MaskShape::Radial {
            centre,
            radii,
            feather,
        } => {
            if radii[0] <= 0.0 || radii[1] <= 0.0 {
                out[..(w * h) as usize].fill(0.0);
                return;
            }
            // The two radii are separate fractions, so an ellipse drawn as a
            // circle on the photograph comes back a circle: the frame's own
            // proportions are already in the numbers.
            let inner = (1.0 - feather).clamp(0.0, 1.0);
            for y in 0..h {
                let v = ((y as f32 + 0.5) / h as f32 - centre[1]) / radii[1];
                for x in 0..w {
                    let u = ((x as f32 + 0.5) / w as f32 - centre[0]) / radii[0];
                    // One at the centre, one at the edge of the ellipse, so the
                    // feather is a fraction of the radius wherever it is
                    // measured — which is what keeps the falloff even round an
                    // ellipse rather than pinched at its narrow ends.
                    let d = (u * u + v * v).sqrt();
                    // A feather of zero puts `inner` at 1, so the two ends meet
                    // and the ramp never runs: the hard edge falls out of the
                    // same expression rather than needing a case of its own.
                    out[(y * w + x) as usize] = if d <= inner {
                        1.0
                    } else if d >= 1.0 {
                        0.0
                    } else {
                        let s = (d - inner) / (1.0 - inner);
                        1.0 - s * s * (3.0 - 2.0 * s)
                    };
                }
            }
        }
    }
}

/// Lay one stroke down, or lift it.
///
/// # Why this is fast enough to draw with
///
/// The whole mask is redrawn from the stroke list every time the hand moves --
/// no accumulated raster, no incremental state -- because the list is the only
/// thing that is true, and anything cached beside it would have to be rebuilt
/// for every undo, every preset and every reopened photograph anyway.
///
/// What makes that affordable is that a segment is only ever tested against the
/// texels it can *reach*. Work is proportional to the area painted rather than
/// to the area of the frame: a stroke a thousand texels long with a twenty-texel
/// radius touches about forty thousand of them however large the photograph is.
/// Draw a band on what the light is, rather than on where it is.
///
/// # Read off the guide, and in the camera's own RGB
///
/// The guide is the frame reduced to a few hundred texels, in the colour the
/// sensor recorded, and it exists already. Two consequences follow and both are
/// choices rather than accidents.
///
/// **The selection is made before the profile.** "Hue" here is the hue of the
/// sensor's response, not of the finished picture, so a band drawn around a red
/// jacket stays around that jacket when the camera profile changes. Selecting on
/// the developed colour would mean a mask that moved when somebody loaded a
/// `.dcp`, which is a mask nobody could rely on.
///
/// **The selection is soft at a colour boundary.** A guide texel covers roughly
/// sixteen image pixels, so the edge of a range is a gradient a few pixels wide
/// rather than a cut. For selecting *by value* that is the behaviour anyone
/// wants — a hard-edged selection of "the sky" is how a local adjustment
/// announces itself — and it is why this is rasterised rather than evaluated per
/// pixel in the shader. Evaluating it there would also make the shader ask what
/// kind of mask it is, which is the one thing this whole module exists to avoid.
#[derive(Clone, Copy)]
struct Band {
    channel: RangeChannel,
    from: f32,
    to: f32,
    feather: f32,
}

fn draw_range(band: Band, w: u32, h: u32, guide: &Guide, out: &mut [f32]) {
    let (gw, gh) = (guide.width.max(1), guide.height.max(1));
    for y in 0..h {
        // Nearest texel rather than bilinear: the guide is already a heavily
        // smoothed reduction of the frame, and interpolating a smooth thing
        // twice buys nothing a person could see.
        let gy = ((y as u64 * gh as u64) / h.max(1) as u64).min(gh as u64 - 1) as u32;
        for x in 0..w {
            let gx = ((x as u64 * gw as u64) / w.max(1) as u64).min(gw as u64 - 1) as u32;
            let i = (gy as usize * gw as usize + gx as usize) * 3;
            let rgb = [guide.data[i], guide.data[i + 1], guide.data[i + 2]];
            let value = match band.channel {
                // Green, because a Bayer sensor has twice as much of it and it
                // is most of what luminance is made of. The same choice the
                // guide's own blur makes, and it needs no camera matrix -- which
                // belongs to the profile and not to the mosaic.
                RangeChannel::Luminance => rgb[1].clamp(0.0, 1.0),
                RangeChannel::Hue => hue_of(rgb),
            };
            out[(y * w + x) as usize] = weight(band, value);
        }
    }
}

/// Hue in degrees, from camera RGB, by the usual hexagonal construction.
///
/// Zero for anything with no colour in it at all: a grey texel is not at any
/// particular hue, and putting it at red would make every neutral in the frame
/// jump into a band drawn around reds.
fn hue_of(rgb: [f32; 3]) -> f32 {
    let high = rgb[0].max(rgb[1]).max(rgb[2]);
    let low = rgb[0].min(rgb[1]).min(rgb[2]);
    let span = high - low;
    if span <= 1e-6 {
        return 0.0;
    }
    let h = if high == rgb[0] {
        ((rgb[1] - rgb[2]) / span).rem_euclid(6.0)
    } else if high == rgb[1] {
        2.0 + (rgb[2] - rgb[0]) / span
    } else {
        4.0 + (rgb[0] - rgb[1]) / span
    };
    (h * 60.0).rem_euclid(360.0)
}

/// How much of the band a value falls in, from 0 outside to 1 within.
///
/// Feathered at both edges by a smoothstep, so a range has no more of a hard
/// boundary than a radial does.
fn weight(band: Band, value: f32) -> f32 {
    let Band {
        channel,
        from,
        to,
        feather,
    } = band;
    let soft = feather.max(1e-6);
    match channel {
        RangeChannel::Luminance => {
            smoothstep(from - soft, from, value) * (1.0 - smoothstep(to, to + soft, value))
        }
        RangeChannel::Hue => {
            // Distance *around the circle* to the nearer edge of the band, so a
            // band that runs from 350 to 10 covers red rather than everything
            // except it.
            let inside = if from <= to {
                value >= from && value <= to
            } else {
                value >= from || value <= to
            };
            if inside {
                return 1.0;
            }
            let gap = arc(value, from).min(arc(value, to));
            1.0 - smoothstep(0.0, soft, gap)
        }
    }
}

/// The shorter way round the circle between two hues, in degrees.
fn arc(a: f32, b: f32) -> f32 {
    let d = (a - b).abs().rem_euclid(360.0);
    d.min(360.0 - d)
}

fn smoothstep(edge0: f32, edge1: f32, v: f32) -> f32 {
    if edge1 <= edge0 {
        return if v >= edge1 { 1.0 } else { 0.0 };
    }
    let t = ((v - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn paint(stroke: &Stroke, feather: f32, w: u32, h: u32, out: &mut [f32]) {
    if stroke.points.is_empty() || stroke.radius <= 0.0 {
        return;
    }
    // A round brush on a frame that is not square: the radius is a fraction of
    // the longest edge, so each axis is scaled by its share of it. Without this
    // a circular brush paints ellipses on everything but a square photograph.
    let long = w.max(h) as f32;
    let (sx, sy) = (w as f32 / long, h as f32 / long);
    let inner = (1.0 - feather).clamp(0.0, 1.0);

    // A single tap is a dab, which is a segment whose two ends are the same
    // point -- so it needs no case of its own, only somewhere to start.
    let ends: Vec<([f32; 2], [f32; 2])> = if stroke.points.len() == 1 {
        vec![(stroke.points[0], stroke.points[0])]
    } else {
        stroke.points.windows(2).map(|p| (p[0], p[1])).collect()
    };

    for (a, b) in ends {
        // Only the texels this segment can reach. Everything else in the frame
        // is untouched by it, whatever the brush is doing elsewhere.
        //
        // The reach is *per axis*: the radius is a fraction of the longest edge
        // and these coordinates are fractions of each edge, so the shorter one
        // needs a proportionally larger number to mean the same distance. Using
        // the radius directly clips every dab to the frame's aspect — which
        // looks like a working brush on a square photograph and like a squashed
        // one on everything else.
        let (pad_x, pad_y) = (stroke.radius / sx, stroke.radius / sy);
        let bound = |v: f32, span: u32, up: bool| -> u32 {
            let t = v * span as f32;
            let t = if up { t.ceil() } else { t.floor() };
            (t as i64).clamp(0, span as i64 - 1) as u32
        };
        let x0 = bound(a[0].min(b[0]) - pad_x, w, false);
        let x1 = bound(a[0].max(b[0]) + pad_x, w, true);
        let y0 = bound(a[1].min(b[1]) - pad_y, h, false);
        let y1 = bound(a[1].max(b[1]) + pad_y, h, true);

        let (dx, dy) = ((b[0] - a[0]) * sx, (b[1] - a[1]) * sy);
        let length = dx * dx + dy * dy;
        for y in y0..=y1 {
            let v = (y as f32 + 0.5) / h as f32;
            for x in x0..=x1 {
                let u = (x as f32 + 0.5) / w as f32;
                // Distance to the *segment*, which is the distance to the
                // nearest point on it -- so a stroke is a capsule rather than a
                // row of separate dabs with gaps showing between them.
                let (px, py) = ((u - a[0]) * sx, (v - a[1]) * sy);
                let t = if length > 0.0 {
                    ((px * dx + py * dy) / length).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let (ox, oy) = (px - t * dx, py - t * dy);
                let d = (ox * ox + oy * oy).sqrt() / stroke.radius;

                let weight = if d <= inner {
                    1.0
                } else if d >= 1.0 {
                    continue;
                } else {
                    let s = (d - inner) / (1.0 - inner);
                    1.0 - s * s * (3.0 - 2.0 * s)
                };
                let cell = &mut out[(y * w + x) as usize];
                *cell = if stroke.erase {
                    cell.min(1.0 - weight)
                } else {
                    cell.max(weight)
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guide of one flat colour, for the shapes that do not read one.
    ///
    /// Grey rather than black: a range mask drawn against this should find a
    /// frame with light in it and no colour, which is the least surprising thing
    /// for a shape test to sit on top of.
    fn flat_guide(value: f32) -> Guide {
        Guide {
            data: vec![value; 3],
            chroma: [value; 3],
            chroma_known: true,
            width: 1,
            height: 1,
        }
    }

    /// A guide split down the middle: dark on the left, bright on the right.
    fn split_guide(dark: f32, bright: f32) -> Guide {
        let (w, h) = (32u32, 32u32);
        let mut data = vec![0.0f32; (w * h) as usize * 3];
        for y in 0..h {
            for x in 0..w {
                let v = if x < w / 2 { dark } else { bright };
                let i = (y * w + x) as usize * 3;
                data[i] = v;
                data[i + 1] = v;
                data[i + 2] = v;
            }
        }
        Guide {
            data,
            chroma: [1.0; 3],
            chroma_known: true,
            width: w,
            height: h,
        }
    }

    fn draw(mask: &Mask, width: u32, height: u32) -> (Vec<f32>, u32, u32) {
        draw_on(mask, width, height, &flat_guide(0.5))
    }

    fn draw_on(mask: &Mask, width: u32, height: u32, guide: &Guide) -> (Vec<f32>, u32, u32) {
        let (w, h) = dimensions(width, height);
        let mut out = vec![0.0f32; (w * h) as usize];
        rasterise(mask, width, height, guide, &mut out);
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

    fn ellipse(centre: [f32; 2], radii: [f32; 2], feather: f32) -> Mask {
        Mask {
            shape: MaskShape::Radial {
                centre,
                radii,
                feather,
            },
            ..Mask::default()
        }
    }

    #[test]
    fn a_radial_is_whole_in_the_middle_and_gone_outside() {
        let mask = ellipse([0.5, 0.5], [0.25, 0.25], 0.4);
        let (pixels, w, h) = draw(&mask, 800, 800);
        assert_eq!(
            at(&pixels, w, w / 2, h / 2),
            1.0,
            "the middle is not covered"
        );
        assert_eq!(at(&pixels, w, 2, 2), 0.0, "the corner is not clear");
        // On the ellipse itself the effect has just run out.
        let edge = at(&pixels, w, w / 2 + (0.25 * w as f32) as u32 - 1, h / 2);
        assert!(
            edge < 0.02,
            "the edge of the ellipse still covers: {edge:.3}"
        );
    }

    #[test]
    fn a_circle_on_the_photograph_is_a_circle_and_not_an_egg() {
        // The two radii are separate fractions precisely so that a circle drawn
        // on a 3:2 frame survives being stored in a space where the axes are not
        // the same length. Checked by walking out from the centre along both
        // axes *in pixels* and finding the effect ends at the same distance.
        let (iw, ih) = (1500u32, 1000u32);
        // A quarter of the shorter edge, expressed in each axis's fraction.
        let radius = 0.25 * ih as f32;
        let mask = ellipse([0.5, 0.5], [radius / iw as f32, radius / ih as f32], 0.0);
        let (pixels, w, h) = draw(&mask, iw, ih);
        let along = |dx: i32, dy: i32| {
            let mut steps = 0;
            loop {
                let x = (w as i32 / 2 + dx * steps) as u32;
                let y = (h as i32 / 2 + dy * steps) as u32;
                if x >= w || y >= h || at(&pixels, w, x, y) < 0.5 {
                    // Back into the *frame's* own pixels, so the two directions
                    // are compared in the units a photographer sees.
                    return steps as f32
                        * if dx != 0 {
                            iw as f32 / w as f32
                        } else {
                            ih as f32 / h as f32
                        };
                }
                steps += 1;
            }
        };
        let (across, down) = (along(1, 0), along(0, 1));
        println!("reaches {across:.1} px across and {down:.1} px down");
        assert!(
            (across - down).abs() < 0.03 * across,
            "the circle came out {across:.1} by {down:.1} pixels"
        );
    }

    #[test]
    fn feather_decides_how_wide_the_falloff_is() {
        // Zero is a hard edge, and the ramp has to actually widen with the
        // number rather than merely existing.
        let width = |feather: f32| {
            let (pixels, w, h) = draw(&ellipse([0.5, 0.5], [0.3, 0.3], feather), 800, 800);
            let row: Vec<f32> = (w / 2..w).map(|x| at(&pixels, w, x, h / 2)).collect();
            row.iter().filter(|v| **v > 0.01 && **v < 0.99).count()
        };
        let (hard, soft, softest) = (width(0.0), width(0.3), width(0.9));
        println!("falloff spans {hard}, {soft} and {softest} texels");
        assert!(
            hard <= 2,
            "a feather of zero is not a hard edge: {hard} texels"
        );
        assert!(soft > hard + 10, "a feather of 0.3 barely softened: {soft}");
        assert!(
            softest > soft + 10,
            "feather does not widen: {soft} then {softest}"
        );
    }

    #[test]
    fn inverting_swaps_what_is_covered_for_what_is_not() {
        // The vignette, and the one rule that will serve every mask source
        // there ever is: it is a fact about the weight, not about the shape.
        let mut mask = ellipse([0.5, 0.5], [0.25, 0.25], 0.3);
        let (plain, w, h) = draw(&mask, 600, 600);
        mask.invert = true;
        let (turned, _, _) = draw(&mask, 600, 600);
        for (i, (a, b)) in plain.iter().zip(&turned).enumerate() {
            assert!(
                (a + b - 1.0).abs() < 1e-6,
                "texel {i} reads {a} and {b}, which do not make a whole"
            );
        }
        assert_eq!(
            at(&turned, w, w / 2, h / 2),
            0.0,
            "the middle is still covered"
        );
        assert_eq!(at(&turned, w, 2, 2), 1.0, "the corner is still clear");
    }

    fn painted(strokes: Vec<Stroke>, feather: f32) -> Mask {
        Mask {
            shape: MaskShape::Brush { strokes, feather },
            ..Mask::default()
        }
    }

    fn stroke(points: &[[f32; 2]], radius: f32, erase: bool) -> Stroke {
        Stroke {
            points: points.to_vec(),
            radius,
            erase,
        }
    }

    #[test]
    fn a_stroke_is_a_capsule_and_not_a_row_of_dots() {
        // Points arrive as far apart as the hand moved between two frames, so a
        // brush that stamped a disc at each one would leave a dotted line at any
        // speed. Every texel is measured against the *segment*, which is what
        // joins them up.
        let mask = painted(vec![stroke(&[[0.2, 0.5], [0.8, 0.5]], 0.03, false)], 0.3);
        let (pixels, w, h) = draw(&mask, 800, 600);
        let y = h / 2;
        for x in (w as f32 * 0.25) as u32..(w as f32 * 0.75) as u32 {
            assert!(
                at(&pixels, w, x, y) > 0.99,
                "a gap along the stroke at x={x}: {}",
                at(&pixels, w, x, y)
            );
        }
        assert_eq!(at(&pixels, w, w / 2, 4), 0.0, "the stroke reached the top");
    }

    #[test]
    fn a_round_brush_stays_round() {
        // The radius is a fraction of the *longest* edge and each axis is scaled
        // by its share of it. Stored per axis instead, a circular brush would
        // paint ellipses on everything but a square photograph.
        let (iw, ih) = (1600u32, 800u32);
        let mask = painted(vec![stroke(&[[0.5, 0.5]], 0.1, false)], 0.0);
        let (pixels, w, h) = draw(&mask, iw, ih);
        let reach = |dx: i32, dy: i32| {
            let mut steps = 0;
            loop {
                let x = (w as i32 / 2 + dx * steps) as u32;
                let y = (h as i32 / 2 + dy * steps) as u32;
                if x >= w || y >= h || at(&pixels, w, x, y) < 0.5 {
                    return steps as f32
                        * if dx != 0 {
                            iw as f32 / w as f32
                        } else {
                            ih as f32 / h as f32
                        };
                }
                steps += 1;
            }
        };
        let (across, down) = (reach(1, 0), reach(0, 1));
        println!("dab reaches {across:.1} px across and {down:.1} px down");
        assert!(
            (across - down).abs() < 0.05 * across,
            "the dab came out {across:.1} by {down:.1} pixels"
        );
    }

    #[test]
    fn erasing_takes_back_what_painting_put_down() {
        // In order, which is the whole reason `erase` is per stroke rather than
        // per mask: paint, take back the overshoot, paint again.
        let wide = stroke(&[[0.1, 0.5], [0.9, 0.5]], 0.06, false);
        let rubbed = stroke(&[[0.5, 0.5]], 0.04, true);
        let (before, w, h) = draw(&painted(vec![wide.clone()], 0.2), 600, 400);
        let (after, _, _) = draw(&painted(vec![wide.clone(), rubbed.clone()], 0.2), 600, 400);
        assert!(at(&before, w, w / 2, h / 2) > 0.99, "nothing was painted");
        assert_eq!(
            at(&after, w, w / 2, h / 2),
            0.0,
            "the eraser did not reach the middle of the stroke"
        );
        assert!(
            at(&after, w, w / 6, h / 2) > 0.99,
            "the eraser took the whole stroke rather than where it went"
        );
        // And painting over it again puts it back, which only works if the
        // strokes are applied in order.
        let (again, _, _) = draw(&painted(vec![wide.clone(), rubbed, wide], 0.2), 600, 400);
        assert!(
            at(&again, w, w / 2, h / 2) > 0.99,
            "painting over an erased spot did not restore it"
        );
    }

    #[test]
    fn an_unpainted_brush_covers_nothing() {
        // What a brush looks like the moment it is added, and the reason it is
        // the one kind that arrives without a placement: there is nothing to
        // place until a hand moves.
        let (pixels, w, h) = draw(&painted(Vec::new(), 0.5), 400, 300);
        assert!(pixels[..(w * h) as usize].iter().all(|v| *v == 0.0));
    }

    #[test]
    fn painting_costs_the_area_painted_and_not_the_frame() {
        // The claim that makes a brush usable: the mask is redrawn from the
        // stroke list every time the hand moves, and that is affordable only
        // because a segment is tested against the texels it can reach rather
        // than against all of them.
        let across = std::time::Instant::now();
        let long = painted(
            vec![stroke(
                &(0..200)
                    .map(|i| [0.05 + 0.9 * i as f32 / 199.0, 0.5])
                    .collect::<Vec<_>>(),
                0.02,
                false,
            )],
            0.5,
        );
        let (w, h) = dimensions(6024, 4024);
        let mut out = vec![0.0f32; (w * h) as usize];
        rasterise(&long, 6024, 4024, &flat_guide(0.5), &mut out);
        let elapsed = across.elapsed();
        println!("a 200-point stroke across a {w}x{h} raster: {elapsed:?}");
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "redrawing a stroke took {elapsed:?}, which is too slow to draw with"
        );
        assert!(out.iter().any(|v| *v > 0.9), "the stroke painted nothing");
        assert!(h > 0);
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
    #[test]
    fn a_band_over_everything_selects_everything() {
        // And one over nothing selects nothing. The two ends of the control,
        // which is the first thing to be sure of before asking where the edge
        // falls.
        let guide = split_guide(0.2, 0.8);
        let all = Mask {
            shape: MaskShape::Range {
                channel: RangeChannel::Luminance,
                from: 0.0,
                to: 1.0,
                feather: 0.0,
            },
            ..Mask::default()
        };
        let none = Mask {
            shape: MaskShape::Range {
                channel: RangeChannel::Luminance,
                from: 0.95,
                to: 1.0,
                feather: 0.0,
            },
            ..Mask::default()
        };
        let (covered, w, h) = draw_on(&all, 256, 256, &guide);
        let (empty, _, _) = draw_on(&none, 256, 256, &guide);
        let mean = |v: &[f32]| v[..(w * h) as usize].iter().sum::<f32>() / (w * h) as f32;
        println!("all {:.4}, none {:.4}", mean(&covered), mean(&empty));
        assert!(
            mean(&covered) > 0.99,
            "a band over the whole range missed some of it"
        );
        assert!(
            mean(&empty) < 0.01,
            "a band above everything in the frame still selected some"
        );
    }

    #[test]
    fn a_luminance_band_selects_the_half_it_names() {
        // The frame is dark on the left and bright on the right, so a band that
        // takes in the bright value and not the dark one must select exactly
        // half the picture -- and the *right* half.
        let guide = split_guide(0.2, 0.8);
        let mask = Mask {
            shape: MaskShape::Range {
                channel: RangeChannel::Luminance,
                from: 0.6,
                to: 1.0,
                feather: 0.0,
            },
            ..Mask::default()
        };
        let (out, w, h) = draw_on(&mask, 256, 256, &guide);
        let mean = out[..(w * h) as usize].iter().sum::<f32>() / (w * h) as f32;
        let left = at(&out, w, w / 4, h / 2);
        let right = at(&out, w, 3 * w / 4, h / 2);
        println!("selected {mean:.3} of the frame; left {left:.3}, right {right:.3}");
        assert!(
            (mean - 0.5).abs() < 0.02,
            "selected {mean:.3} of the frame, not half"
        );
        assert!(
            left < 0.01 && right > 0.99,
            "selected the wrong half: {left:.3} / {right:.3}"
        );
    }

    #[test]
    fn a_hue_band_wraps_through_red() {
        // Red sits at zero degrees, so the only way to select it is a band that
        // runs off one end of the circle and back on at the other. A band from
        // 350 to 10 that did not wrap would select everything *except* red,
        // which is the failure this exists to catch.
        let red = Guide {
            data: vec![0.8, 0.1, 0.1],
            chroma: [1.0; 3],
            chroma_known: true,
            width: 1,
            height: 1,
        };
        let wrapped = Mask {
            shape: MaskShape::Range {
                channel: RangeChannel::Hue,
                from: 350.0,
                to: 10.0,
                feather: 0.0,
            },
            ..Mask::default()
        };
        let elsewhere = Mask {
            shape: MaskShape::Range {
                channel: RangeChannel::Hue,
                from: 100.0,
                to: 200.0,
                feather: 0.0,
            },
            ..Mask::default()
        };
        let (on, w, h) = draw_on(&wrapped, 64, 64, &red);
        let (off, _, _) = draw_on(&elsewhere, 64, 64, &red);
        let mean = |v: &[f32]| v[..(w * h) as usize].iter().sum::<f32>() / (w * h) as f32;
        println!(
            "red under a wrapped band {:.3}, under a green one {:.3}",
            mean(&on),
            mean(&off)
        );
        assert!(
            mean(&on) > 0.99,
            "a band from 350 to 10 did not take in red"
        );
        assert!(
            mean(&off) < 0.01,
            "a band from 100 to 200 took in red anyway"
        );
    }

    #[test]
    fn inverting_a_range_means_what_it_means_everywhere_else() {
        // `invert` lives on the Mask and not on the shape, so it has to work on
        // a source that is not geometry without anything being added for it.
        let guide = split_guide(0.2, 0.8);
        let shape = MaskShape::Range {
            channel: RangeChannel::Luminance,
            from: 0.6,
            to: 1.0,
            feather: 0.0,
        };
        let (plain, w, h) = draw_on(
            &Mask {
                shape: shape.clone(),
                ..Mask::default()
            },
            256,
            256,
            &guide,
        );
        let (flipped, _, _) = draw_on(
            &Mask {
                shape,
                invert: true,
                ..Mask::default()
            },
            256,
            256,
            &guide,
        );
        for i in 0..(w * h) as usize {
            assert!(
                (plain[i] + flipped[i] - 1.0).abs() < 1e-5,
                "inverted range does not complement the plain one at {i}"
            );
        }
    }
}
