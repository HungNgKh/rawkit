//! A low-resolution, edge-aware picture of where the light is.
//!
//! Highlights and shadows used to move the tone *range*: a pixel at 0.8 was
//! treated as a highlight wherever it sat, so recovering a blown sky also
//! flattened a face lit to the same value. That is what "not spatially adaptive"
//! meant, and it is the difference between the control every editor ships and
//! the one this project had.
//!
//! Making them local needs the pixel's *neighbourhood*, over a radius of a
//! hundred pixels or more. The tile halo is 18 pixels, so it cannot come from
//! there — see [`crate::render::HALO`]. It comes from here instead: the whole
//! frame, reduced to a few hundred pixels on its longest edge, uploaded once
//! when the image opens and sampled bilinearly by every tile at every zoom
//! level.
//!
//! # Three properties that make this usable
//!
//! - **It is edit-independent.** What is stored is the camera's own RGB, before
//!   white balance and before anything the user did. The shader develops it
//!   through the *same* functions it develops a pixel with, so moving a slider
//!   changes what the guide means without rebuilding it. A drag costs nothing
//!   here.
//! - **It is indexed in image coordinates, not tile coordinates.** So two tiles
//!   agree exactly where they meet — there is no seam to hide — and a tile
//!   rendered at level 3 samples the same guide as the same region at level 0,
//!   which is what keeps a photograph from changing as you zoom.
//! - **It is edge-aware.** A plain blur puts a bright halo around every dark
//!   object against a bright sky, because the guide bleeds the sky's brightness
//!   across the boundary and the recovery follows it. The blur here weights by
//!   brightness difference as well as distance, so an edge of more than about a
//!   stop stops it.
//!
//! # What it is not
//!
//! Not a bilateral filter in the strict sense: the two passes are separable and
//! a true bilateral is not. Run separably it can leave faint streaks across a
//! corner where two strong edges meet. This is a *control signal* sampled at
//! one part in sixteen of the frame, not a picture anyone looks at, and the
//! approximation costs an order of magnitude — the honest version of the
//! trade-off, rather than a claim it is exact.

use crate::render::BayerPhase;

/// The longest edge the guide is built at.
///
/// Sets the operator's reach: at 384 across a 6024-pixel frame one guide texel
/// spans about 16 image pixels, so the blur below reaches roughly 190 of them.
/// Larger would make the control more local and more prone to halo; smaller
/// makes it approach the global behaviour it replaced.
pub const MAX_EDGE: u32 = 384;

/// The blur's sigma, as a fraction of the guide's longest edge.
///
/// Proportional rather than absolute, so the operator reaches the same
/// *fraction* of the picture whatever its dimensions — a control that behaved
/// differently on a crop than on the frame it came from would be unusable.
const SIGMA_DIVISOR: f32 = 32.0;

/// How much brightness difference stops the blur, in stops.
///
/// One stop: enough that texture and shading flow together, little enough that
/// a skyline does not.
const RANGE_STOPS: f32 = 1.0;

/// The largest guide any allocation has to hold, in floats.
///
/// Square, because [`MAX_EDGE`] bounds the longest edge and the shortest is
/// never longer. Three floats a texel: RGB, and no alpha to invent.
pub const CAPACITY: usize = (MAX_EDGE * MAX_EDGE) as usize * 3;

/// The whole frame at a few hundred pixels, in the camera's own RGB.
#[derive(Debug, Clone)]
pub struct Guide {
    /// Three floats per texel, row major: red, green, blue as the sensor saw
    /// them, white balance not applied.
    pub data: Vec<f32>,
    pub width: u32,
    pub height: u32,
}

/// How big a guide for an image this size is.
///
/// Never more than half the image in either direction, which is what makes each
/// texel cover at least a 2x2 block — and a 2x2 block of a Bayer mosaic holds
/// one red, two green and one blue wherever it starts. Below that a texel could
/// land on a single sample and two of its three channels would have nothing in
/// them.
fn dimensions(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    let scale = if longest > MAX_EDGE {
        f64::from(MAX_EDGE) / f64::from(longest)
    } else {
        1.0
    };
    let edge = |v: u32| (((f64::from(v) * scale).round() as u32).max(1)).min((v / 2).max(1));
    (edge(width), edge(height))
}

impl Guide {
    /// Reduce a mosaic to a guide, and smooth it without crossing edges.
    ///
    /// Costs one pass over the mosaic — 98 ms for a 24 MP frame — and is paid
    /// once when the image opens, never while editing. Against the decode that
    /// precedes it that is small; against a slider drag it does not exist,
    /// which is the property that matters.
    pub fn build(mosaic: &[f32], width: u32, height: u32, phase: BayerPhase) -> Guide {
        let (gw, gh) = dimensions(width, height);
        let cells = (gw as usize) * (gh as usize);
        let mut sum = vec![0.0f32; cells * 3];
        let mut count = vec![0.0f32; cells * 3];

        // Which guide column each image column falls in, worked out once rather
        // than as a 64-bit divide per pixel — it is the same answer every row.
        // Worth 8% and no more: at 98 ms for 24 megapixels this pass is bound by
        // the scattered accumulation and not by its arithmetic, which is worth
        // knowing before anyone optimises the arithmetic again.
        let column: Vec<u32> = (0..width)
            .map(|x| ((x as u64 * gw as u64) / width as u64).min(gw as u64 - 1) as u32)
            .collect();

        let (dx, dy) = phase.offset();
        for y in 0..height {
            let gy = ((y as u64 * gh as u64) / height as u64).min(gh as u64 - 1) as u32;
            let py = y + dy;
            for x in 0..width {
                let gx = column[x as usize];
                let px = x + dx;
                // The same rule as `colour_at` in the shader, and the only place
                // this module knows the mosaic is a mosaic.
                let c = if (px + py) % 2 == 1 {
                    1
                } else if py % 2 == 0 {
                    0
                } else {
                    2
                };
                let i = (gy as usize * gw as usize + gx as usize) * 3 + c;
                sum[i] += mosaic[(y as usize) * width as usize + x as usize];
                count[i] += 1.0;
            }
        }

        // A channel with nothing in it cannot happen for a texel covering 2x2 or
        // more, which `dimensions` guarantees. It is filled from the frame's own
        // mean rather than left at zero, because a zero here would read as a
        // black neighbourhood and pull the recovery the wrong way — a silent
        // wrong answer where a visible one would be better.
        let mean: Vec<f32> = (0..3)
            .map(|c| {
                let (s, n): (f32, f32) = (0..cells).fold((0.0, 0.0), |(s, n), i| {
                    (s + sum[i * 3 + c], n + count[i * 3 + c])
                });
                if n > 0.0 {
                    s / n
                } else {
                    0.0
                }
            })
            .collect();
        let mut data = vec![0.0f32; cells * 3];
        for i in 0..cells * 3 {
            data[i] = if count[i] > 0.0 {
                sum[i] / count[i]
            } else {
                mean[i % 3]
            };
        }

        let sigma = (gw.max(gh) as f32 / SIGMA_DIVISOR).max(1.0);
        blur(&mut data, gw, gh, sigma);
        Guide {
            data,
            width: gw,
            height: gh,
        }
    }
}

/// Separable edge-aware blur, weighted by distance and by brightness together.
///
/// Green is the brightness the weights are measured on: it is what a Bayer
/// sensor has most of and what luminance is mostly made of, and using it here
/// avoids needing the camera matrix, which belongs to the profile and not to
/// the mosaic.
fn blur(data: &mut [f32], width: u32, height: u32, sigma: f32) {
    let reach = (2.0 * sigma).ceil() as i32;
    let spatial = -0.5 / (sigma * sigma);
    let range = -0.5 / (RANGE_STOPS * RANGE_STOPS);
    let (w, h) = (width as i32, height as i32);

    // Horizontal, then vertical, over the same buffer through a scratch copy.
    for axis in 0..2 {
        let source = data.to_vec();
        let brightness = |x: i32, y: i32| -> f32 {
            let i = (y.clamp(0, h - 1) as usize * width as usize + x.clamp(0, w - 1) as usize) * 3;
            // Log brightness, so "a stop" is a distance and a shadow is not
            // compressed into a range term that cannot separate anything.
            source[i + 1].max(1e-6).log2()
        };
        for y in 0..h {
            for x in 0..w {
                let centre = brightness(x, y);
                let mut acc = [0.0f32; 3];
                let mut total = 0.0f32;
                for step in -reach..=reach {
                    let (sx, sy) = if axis == 0 {
                        (x + step, y)
                    } else {
                        (x, y + step)
                    };
                    let d = brightness(sx, sy) - centre;
                    let weight = ((step * step) as f32 * spatial + d * d * range).exp();
                    let i = (sy.clamp(0, h - 1) as usize * width as usize
                        + sx.clamp(0, w - 1) as usize)
                        * 3;
                    for (c, acc) in acc.iter_mut().enumerate() {
                        *acc += source[i + c] * weight;
                    }
                    total += weight;
                }
                let o = (y as usize * width as usize + x as usize) * 3;
                for (c, acc) in acc.iter().enumerate() {
                    data[o + c] = acc / total;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An RGGB mosaic from a function of position, in camera RGB.
    fn mosaic(width: u32, height: u32, rgb: impl Fn(u32, u32) -> [f32; 3]) -> Vec<f32> {
        (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .map(|(x, y)| {
                let c = rgb(x, y);
                match (x % 2 == 0, y % 2 == 0) {
                    (true, true) => c[0],
                    (false, false) => c[2],
                    _ => c[1],
                }
            })
            .collect()
    }

    #[test]
    fn every_texel_covers_a_whole_bayer_quad() {
        // The claim `dimensions` exists for: no texel may be smaller than 2x2,
        // or two of its three channels have no sample to average.
        for (w, h) in [
            (6024, 4024),
            (4024, 6024),
            (64, 64),
            (4, 4),
            (2, 2),
            (800, 3),
        ] {
            let (gw, gh) = dimensions(w, h);
            assert!(gw >= 1 && gh >= 1, "{w}x{h} gave an empty guide");
            assert!(
                gw * 2 <= w.max(2) && gh * 2 <= h.max(2),
                "{w}x{h} gave {gw}x{gh}, whose texels are under 2x2"
            );
            assert!(gw <= MAX_EDGE && gh <= MAX_EDGE, "{w}x{h} exceeded the cap");
            assert!(
                (gw * gh) as usize * 3 <= CAPACITY,
                "{w}x{h} needs more than the allocation reserves"
            );
        }
    }

    #[test]
    fn a_flat_frame_gives_a_flat_guide() {
        // Including at the border, where the kernel is clamped: normalising by
        // the weight actually spent is what keeps the edges from darkening, and
        // a guide that darkened at the edges would recover the corners of every
        // photograph differently from its middle.
        let data = mosaic(128, 96, |_, _| [0.4, 0.5, 0.2]);
        let guide = Guide::build(&data, 128, 96, BayerPhase::Rggb);
        for (i, texel) in guide.data.chunks_exact(3).enumerate() {
            for (c, (v, expected)) in texel.iter().zip([0.4, 0.5, 0.2]).enumerate() {
                assert!(
                    (v - expected).abs() < 1e-4,
                    "texel {i} channel {c} is {v}, not {expected}"
                );
            }
        }
    }

    #[test]
    fn the_guide_carries_the_frames_own_colour() {
        // Each channel comes back where it went in. A phase mix-up here would
        // give the local operator a red frame's brightness for a blue one, which
        // is invisible until a picture looks wrong.
        let data = mosaic(64, 64, |_, _| [0.8, 0.4, 0.1]);
        let guide = Guide::build(&data, 64, 64, BayerPhase::Rggb);
        let mid = ((guide.height / 2) * guide.width + guide.width / 2) as usize * 3;
        assert!((guide.data[mid] - 0.8).abs() < 1e-3);
        assert!((guide.data[mid + 1] - 0.4).abs() < 1e-3);
        assert!((guide.data[mid + 2] - 0.1).abs() < 1e-3);
    }

    #[test]
    fn brightness_does_not_cross_a_hard_edge() {
        // The reason the blur is edge-aware. A dark half against a bright half:
        // a plain Gaussian would carry the bright side's value well into the
        // dark one, and every such boundary in a photograph would grow a halo
        // once highlights were pulled down.
        let data = mosaic(256, 256, |x, _| {
            let v = if x < 128 { 0.05 } else { 0.8 };
            [v, v, v]
        });
        let guide = Guide::build(&data, 256, 256, BayerPhase::Rggb);
        let row = (guide.height / 2) as usize * guide.width as usize;
        let half = guide.width as usize / 2;
        // Four texels back from the boundary, well inside the blur's reach.
        let dark = guide.data[(row + half - 4) * 3 + 1];
        let bright = guide.data[(row + half + 4) * 3 + 1];
        assert!(
            dark < 0.12,
            "the bright half bled into the dark one: {dark:.4} against 0.05"
        );
        assert!(
            bright > 0.7,
            "the dark half bled into the bright one: {bright:.4} against 0.8"
        );
    }

    #[test]
    fn a_gradient_survives_the_blur() {
        // The other half of the same claim: the filter must still be a blur.
        // Edge-aware weighting that refused to move anything would pass the
        // test above and be useless.
        let data = mosaic(256, 256, |x, _| {
            let v = 0.1 + 0.5 * x as f32 / 255.0;
            [v, v, v]
        });
        let guide = Guide::build(&data, 256, 256, BayerPhase::Rggb);
        let row = (guide.height / 2) as usize * guide.width as usize;
        let left = guide.data[(row + 8) * 3 + 1];
        let right = guide.data[(row + guide.width as usize - 8) * 3 + 1];
        assert!(
            right - left > 0.35,
            "the gradient was flattened: {left:.3} to {right:.3}"
        );
    }
}
