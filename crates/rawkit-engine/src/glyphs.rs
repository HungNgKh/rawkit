//! The marks a grid cell carries: stars, a flag, a cross, a tick, a copy.
//!
//! # Why these are drawn and not typeset
//!
//! The canvas has no text. Putting a font on the GPU side is a glyph atlas, a
//! shaping question and a dependency under the licence gate, for five shapes —
//! so the decision (Q7 of the interface plan) was sprites for the marks and
//! words in the HTML bars. And five shapes do not need a font: each is a
//! distance function, which is a few lines, resolution-independent, and
//! anti-aliased for nothing — coverage is how far inside the edge a pixel's
//! centre is, clamped to a pixel's width.
//!
//! # Why every mark has a dark rim
//!
//! They sit on photographs, and a white star on a white sky is no star. The rim
//! is baked into the texture — straight alpha, white inside, near-black round
//! it — so the shader's one `tint` colours the mark and leaves the rim dark:
//! dark times any colour is dark.
//!
//! Pure CPU and no GPU types, so all of it is tested without a device.

/// One rasterised mark: 8-bit sRGB, straight alpha, row-major.
#[derive(Debug, Clone, PartialEq)]
pub struct Glyph {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Which mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mark {
    /// A row of this many stars, one to five. One texture rather than five
    /// cells: at the smallest cell size a thousand photographs are on screen,
    /// and a draw call each for the rating is the difference that shows.
    Stars(u8),
    /// A flag on a pole: picked.
    Pick,
    /// A cross: rejected.
    Reject,
    /// A tick on a disc: selected. The one mark that is not tinted — the disc
    /// is the light part and the tick is cut out of it dark.
    Selected,
    /// Two overlapping frames: a virtual copy.
    Copy,
}

type Point = (f32, f32);

fn length((x, y): Point) -> f32 {
    (x * x + y * y).sqrt()
}

/// Distance from `p` to the segment `a`–`b`.
fn segment(p: Point, a: Point, b: Point) -> f32 {
    let (pa, ba) = ((p.0 - a.0, p.1 - a.1), (b.0 - a.0, b.1 - a.1));
    let h = ((pa.0 * ba.0 + pa.1 * ba.1) / (ba.0 * ba.0 + ba.1 * ba.1)).clamp(0.0, 1.0);
    length((pa.0 - ba.0 * h, pa.1 - ba.1 * h))
}

/// Signed distance to a closed polygon: negative inside. The even-odd walk is
/// the standard one; it is here rather than a crate because it is fifteen lines.
fn polygon(p: Point, points: &[Point]) -> f32 {
    let mut nearest = f32::MAX;
    let mut inside = false;
    let mut j = points.len() - 1;
    for i in 0..points.len() {
        let (a, b) = (points[i], points[j]);
        nearest = nearest.min(segment(p, a, b));
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < (b.0 - a.0) * (p.1 - a.1) / (b.1 - a.1) + a.0 {
            inside = !inside;
        }
        j = i;
    }
    if inside {
        -nearest
    } else {
        nearest
    }
}

/// A five-pointed star of radius `r` about the origin, point up.
fn star(p: Point, r: f32) -> f32 {
    let points: Vec<Point> = (0..10)
        .map(|i| {
            let angle = std::f32::consts::PI * (i as f32 / 5.0) - std::f32::consts::FRAC_PI_2;
            // The inner radius that makes the arms straight lines through
            // opposite points — the star a person draws without lifting the pen.
            let radius = if i % 2 == 0 { r } else { r * 0.381_966 };
            (radius * angle.cos(), radius * angle.sin())
        })
        .collect();
    polygon(p, &points)
}

/// Signed distance to the mark, in units where the glyph is one across and the
/// origin is its centre. `column` is which star of a row this is.
fn distance(mark: Mark, p: Point) -> f32 {
    match mark {
        Mark::Stars(_) => star(p, 0.40),
        Mark::Pick => {
            let pole = segment(p, (-0.24, -0.34), (-0.24, 0.36)) - 0.045;
            let cloth = polygon(p, &[(-0.24, -0.34), (0.32, -0.20), (-0.24, -0.02)]);
            pole.min(cloth)
        }
        Mark::Reject => {
            let one = segment(p, (-0.26, -0.26), (0.26, 0.26));
            let other = segment(p, (-0.26, 0.26), (0.26, -0.26));
            one.min(other) - 0.075
        }
        Mark::Selected => length(p) - 0.40,
        Mark::Copy => {
            let frame = |centre: Point| {
                let d = ((p.0 - centre.0).abs() - 0.22, (p.1 - centre.1).abs() - 0.22);
                let outside = length((d.0.max(0.0), d.1.max(0.0)));
                (outside + d.0.max(d.1).min(0.0)).abs() - 0.045
            };
            frame((-0.09, 0.09)).min(frame((0.10, -0.10)))
        }
    }
}

/// The tick cut out of the selected disc.
fn tick(p: Point) -> f32 {
    let down = segment(p, (-0.19, 0.02), (-0.05, 0.16));
    let up = segment(p, (-0.05, 0.16), (0.20, -0.14));
    down.min(up) - 0.06
}

/// How much of a pixel a shape covers, from its signed distance in pixels.
fn coverage(distance_px: f32) -> f32 {
    (0.5 - distance_px).clamp(0.0, 1.0)
}

/// Rasterise `mark` at `size` pixels high. A row of stars is that many sizes
/// wide; everything else is square.
pub fn rasterise(mark: Mark, size: u32) -> Glyph {
    let size = size.max(4);
    let columns = match mark {
        Mark::Stars(n) => u32::from(n.clamp(1, 5)),
        _ => 1,
    };
    let (width, height) = (size * columns, size);
    // About a pixel and a bit, whatever the size: thin enough not to fatten a
    // ten-pixel star into a blob, present enough to hold against a bright sky.
    let rim = (size as f32 * 0.07).max(1.1);
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            // The pixel's centre, in the units of the column it falls in.
            let column = x / size;
            let p = (
                ((x - column * size) as f32 + 0.5) / size as f32 - 0.5,
                (y as f32 + 0.5) / size as f32 - 0.5,
            );
            let d = distance(mark, p) * size as f32;
            let body = coverage(d);
            let with_rim = coverage(d - rim);
            if with_rim <= 0.0 {
                continue;
            }
            // Light inside, dark rim; for the selected disc, the tick is dark too.
            let mut light = body / with_rim;
            if mark == Mark::Selected {
                light *= 1.0 - coverage(tick(p) * size as f32);
            }
            let value = (8.0 + light * 247.0).round() as u8;
            let at = ((y * width + x) * 4) as usize;
            rgba[at..at + 4].copy_from_slice(&[
                value,
                value,
                value,
                (with_rim * 255.0).round() as u8,
            ]);
        }
    }
    Glyph {
        rgba,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(glyph: &Glyph, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * glyph.width + x) * 4) as usize;
        glyph.rgba[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn a_star_is_light_in_the_middle_dark_at_the_rim_and_nothing_at_the_corner() {
        let star = rasterise(Mark::Stars(1), 48);
        assert_eq!((star.width, star.height), (48, 48));
        assert_eq!(pixel(&star, 24, 24), [255, 255, 255, 255]);
        assert_eq!(pixel(&star, 0, 0)[3], 0, "a star's outline, not a square's");
        // Walking up from the centre to the top point and past it: light, then
        // a dark rim that is still opaque, then nothing.
        let column: Vec<[u8; 4]> = (0..24).rev().map(|y| pixel(&star, 24, y)).collect();
        let rim = column
            .iter()
            .find(|p| p[3] > 200 && p[0] < 60)
            .expect("a dark, opaque rim between the star and the photograph");
        assert!(rim[0] < 60);
        assert_eq!(column.last().unwrap()[3], 0);
    }

    #[test]
    fn a_rating_is_one_texture_as_wide_as_its_stars() {
        for n in 1..=5u8 {
            let row = rasterise(Mark::Stars(n), 20);
            assert_eq!((row.width, row.height), (20 * u32::from(n), 20));
            // Every star is there: the middle of each column is lit.
            for column in 0..u32::from(n) {
                assert_eq!(
                    pixel(&row, column * 20 + 10, 10)[3],
                    255,
                    "star {column} of {n}"
                );
            }
        }
    }

    #[test]
    fn every_mark_draws_something_at_the_smallest_size_a_cell_asks_for() {
        // Ten pixels is what an eighty-pixel cell gets. A mark that rasterises
        // to nothing there is a rating nobody can see at the size a cull of a
        // thousand frames is done at.
        for mark in [
            Mark::Stars(3),
            Mark::Pick,
            Mark::Reject,
            Mark::Selected,
            Mark::Copy,
        ] {
            let glyph = rasterise(mark, 10);
            let lit = glyph
                .rgba
                .chunks(4)
                .filter(|p| p[3] > 128 && p[0] > 128)
                .count();
            assert!(lit >= 8, "{mark:?} has {lit} lit pixels at 10 px");
        }
    }

    #[test]
    fn the_selected_mark_has_its_tick_cut_out_dark() {
        let selected = rasterise(Mark::Selected, 40);
        // On the disc and off the tick: light. On the tick's long stroke: dark,
        // and still opaque — it is part of the mark, not a hole in it.
        assert!(pixel(&selected, 10, 12)[0] > 200);
        let on_tick = pixel(&selected, 23, 21);
        assert!(on_tick[0] < 80 && on_tick[3] == 255, "{on_tick:?}");
    }
}
