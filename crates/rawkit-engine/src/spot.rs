//! Covering a blemish with sensor data borrowed from somewhere else.
//!
//! # Why this happens to the mosaic
//!
//! A dust mark is a speck sitting on the sensor, so the honest place to remove
//! it is the sensor's own data. Three things follow from patching there rather
//! than patching developed pixels, and all three are the reason:
//!
//! - The demosaic interpolates *through* the repair. Patch afterwards and the
//!   covered disc has been interpolated from the blemish's own neighbours, so it
//!   carries a different sharpness from everything around it — visible at the
//!   exact magnification somebody retouching is working at.
//! - One copy carries all three colours. On developed RGB the three channels
//!   were each interpolated from different neighbours and have to be reconciled.
//! - Highlight reconstruction, white balance and the profile all run downstream,
//!   so the patch is subject to the same colour decisions as its surroundings
//!   instead of being pasted in after them.
//!
//! # Parity, which is the whole trap
//!
//! The borrowed texel has to be the same colour as the one it replaces. On a
//! Bayer sensor that means the offset from destination to source must be **even
//! in both axes** — an odd offset copies red into green and the repair arrives
//! as confetti. The pyramid's reduction keeps the 2×2 phase at every
//! level while halving the units, so the offset is snapped to even in *each
//! level's own coordinates* rather than once at full resolution. The two
//! snappings disagree by at most a pixel at a level where a dust mark is
//! smaller than one.
//!
//! # What is resolved when
//!
//! [`resolve`] runs once per edit, against the full-resolution mosaic, and
//! measures how much light the destination has compared with the source. That
//! ratio is scale-free, so every pyramid level uses the one measurement — which
//! also means a spot cannot change its mind about brightness as somebody zooms.
//! [`apply`] runs per tile and does nothing but copy, scale and blend.

use rawkit_editstate::{Spot, SpotMode};

/// How far past the spot the light level is measured, as a multiple of the
/// radius.
///
/// A ring rather than a disc, and outside the blemish rather than over it:
/// measuring across the dust mark would fold the very darkening being removed
/// into the correction, and the repair would come out half-dark.
const ANNULUS: f32 = 1.6;

/// How far a heal's correction may go before it is refused.
///
/// Two stops either way. A larger ratio means the source is not remotely like
/// the destination — somebody has dragged it onto a highlight — and scaling it
/// into place produces a bright disc rather than a repair. Clamping keeps the
/// failure proportionate to the mistake.
const MAX_GAIN: f32 = 4.0;

/// A spot with its correction worked out, ready for any pyramid level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resolved {
    /// Fractions of the unoriented sensor frame, as stored.
    centre: [f32; 2],
    source: [f32; 2],
    /// A fraction of the frame's width.
    radius: f32,
    feather: f32,
    /// What to multiply a borrowed value by, one per position in the 2×2 CFA
    /// block, indexed `(y & 1) * 2 + (x & 1)`.
    ///
    /// Per position and not one number, because the source and the destination
    /// can sit under different-coloured light and a single grey correction would
    /// leave the patch the wrong hue. The offset is even, so a texel and the one
    /// it borrows from share this index.
    gain: [f32; 4],
}

/// Work out each spot's correction against the full-resolution mosaic.
///
/// Spots whose source lands entirely outside the frame are dropped rather than
/// clamped: a repair made from the mirrored edge is not a repair, and silently
/// substituting one is worse than leaving the blemish visible where the user can
/// see it and drag the source somewhere real.
pub fn resolve(spots: &[Spot], mosaic: &[f32], width: u32, height: u32) -> Vec<Resolved> {
    let (w, h) = (width as i64, height as i64);
    spots
        .iter()
        .filter_map(|spot| {
            let radius = spot.radius * width as f32;
            if radius < 0.5 {
                return None;
            }
            let at = |p: [f32; 2]| [p[0] * width as f32, p[1] * height as f32];
            let (dest, src) = (at(spot.centre), at(spot.source));
            let gain = match spot.mode {
                SpotMode::Clone => [1.0; 4],
                SpotMode::Heal => {
                    let outer = radius * ANNULUS;
                    let d = annulus(mosaic, w, h, dest, radius, outer);
                    let s = annulus(mosaic, w, h, src, radius, outer);
                    let mut gain = [1.0f32; 4];
                    for i in 0..4 {
                        // No samples on one side, or a source ring with no light
                        // in it: there is nothing to match, and dividing by it
                        // is how a repair turns into a firefly.
                        if let (Some(d), Some(s)) = (d[i], s[i]) {
                            if s > 0.0 && d > 0.0 {
                                gain[i] = (d / s).clamp(1.0 / MAX_GAIN, MAX_GAIN);
                            }
                        }
                    }
                    gain
                }
            };
            Some(Resolved {
                centre: spot.centre,
                source: spot.source,
                radius: spot.radius,
                feather: spot.feather,
                gain,
            })
        })
        .collect()
}

/// Mean value on a ring, one per position in the 2×2 CFA block.
fn annulus(
    mosaic: &[f32],
    w: i64,
    h: i64,
    centre: [f32; 2],
    inner: f32,
    outer: f32,
) -> [Option<f32>; 4] {
    let mut sum = [0f64; 4];
    let mut count = [0u32; 4];
    let reach = outer.ceil() as i64;
    let (cx, cy) = (centre[0], centre[1]);
    let (x0, y0) = (cx.floor() as i64 - reach, cy.floor() as i64 - reach);
    for y in y0..=y0 + 2 * reach {
        if y < 0 || y >= h {
            continue;
        }
        for x in x0..=x0 + 2 * reach {
            if x < 0 || x >= w {
                continue;
            }
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            let r = (dx * dx + dy * dy).sqrt();
            if r < inner || r > outer {
                continue;
            }
            let i = ((y & 1) * 2 + (x & 1)) as usize;
            sum[i] += f64::from(mosaic[(y * w + x) as usize]);
            count[i] += 1;
        }
    }
    std::array::from_fn(|i| (count[i] > 0).then(|| (sum[i] / f64::from(count[i])) as f32))
}

/// Patch the spots that reach into one gathered tile.
///
/// `origin` is the image coordinate of `out`'s first texel — negative in the
/// halo of a tile at the top-left of the frame. `mosaic`, `width` and `height`
/// are this pyramid level's, and the source is read from them rather than from
/// `out` because it is routinely further away than the halo reaches.
pub fn apply(
    patches: &[Resolved],
    mosaic: &[f32],
    width: u32,
    height: u32,
    origin: [i64; 2],
    padded: u32,
    out: &mut [f32],
) {
    let (w, h) = (width as i64, height as i64);
    let padded = padded as i64;
    for patch in patches {
        let radius = patch.radius * width as f32;
        if radius < 0.5 {
            continue;
        }
        let cx = patch.centre[0] * width as f32;
        let cy = patch.centre[1] * height as f32;
        // Snapped down to an even offset so a photosite borrows from one of its
        // own colour. See the parity note at the top of the module.
        let even = |v: f32| {
            let n = v.round() as i64;
            n - (n & 1)
        };
        let dx = even(patch.source[0] * width as f32 - cx);
        let dy = even(patch.source[1] * height as f32 - cy);
        if dx == 0 && dy == 0 {
            continue;
        }

        // The spot's bounding box, in this tile's coordinates, clipped to the
        // tile and to the frame. Everything outside the frame is halo that the
        // gather mirrored, and mirroring a repair back over itself is not
        // something to attempt — a spot within a halo's reach of the edge simply
        // does not extend into it.
        let reach = radius.ceil() as i64;
        let lo = |c: f32, o: i64| (c.floor() as i64 - reach - o).max(0).max(-o);
        let hi = |c: f32, o: i64, extent: i64| {
            (c.ceil() as i64 + reach - o)
                .min(padded - 1)
                .min(extent - 1 - o)
        };
        let (px0, px1) = (lo(cx, origin[0]), hi(cx, origin[0], w));
        let (py0, py1) = (lo(cy, origin[1]), hi(cy, origin[1], h));

        let inner = radius * (1.0 - patch.feather);
        for py in py0..=py1 {
            let gy = py + origin[1];
            let sy = gy + dy;
            if sy < 0 || sy >= h {
                continue;
            }
            for px in px0..=px1 {
                let gx = px + origin[0];
                let sx = gx + dx;
                if sx < 0 || sx >= w {
                    continue;
                }
                let (ox, oy) = (gx as f32 + 0.5 - cx, gy as f32 + 0.5 - cy);
                let r = (ox * ox + oy * oy).sqrt();
                if r > radius {
                    continue;
                }
                // Full effect inside, easing to nothing at the rim. The same
                // curve a radial mask fades with, for the same reason: a linear
                // ramp leaves a visible crease where it meets the untouched
                // pixels.
                let weight = if r <= inner || radius <= inner {
                    1.0
                } else {
                    let t = ((radius - r) / (radius - inner)).clamp(0.0, 1.0);
                    t * t * (3.0 - 2.0 * t)
                };
                let i = ((gy & 1) * 2 + (gx & 1)) as usize;
                let borrowed = mosaic[(sy * w + sx) as usize] * patch.gain[i];
                let cell = &mut out[(py * padded + px) as usize];
                *cell += (borrowed - *cell) * weight;
            }
        }
    }
}

/// Where to borrow from, for a spot somebody has just placed.
///
/// Run once, in the interface, and the answer is **stored in the edit** — see
/// [`rawkit_editstate::Spot::source`] for why it must not be run again at render
/// time. What comes back is a starting point rather than an answer; the person
/// placing the spot can see both circles and drag this one somewhere better.
///
/// The search is over two rings of candidates around the blemish, scored on two
/// things: how featureless the borrowed disc is, and how closely its light level
/// already matches. Featureless wins ties, because a patch made of flat sky
/// survives being scaled and a patch with a branch in it does not.
pub fn propose_source(
    mosaic: &[f32],
    width: u32,
    height: u32,
    centre: [f32; 2],
    radius: f32,
    taken: &[Spot],
) -> [f32; 2] {
    let (w, h) = (width as i64, height as i64);
    let r = radius * width as f32;
    let dest = [centre[0] * width as f32, centre[1] * height as f32];
    let target = disc(mosaic, w, h, dest, r * ANNULUS).map(|(mean, _)| mean);

    let mut best: Option<([f32; 2], f32)> = None;
    for ring in [2.0f32, 3.0] {
        for step in 0..16 {
            let angle = std::f32::consts::TAU * step as f32 / 16.0;
            let at = [
                dest[0] + angle.cos() * r * ring,
                dest[1] + angle.sin() * r * ring,
            ];
            // Wholly inside the frame, with room for the ring the heal measures
            // its correction over. A source that hangs off the edge would be
            // matched against a partial annulus.
            let margin = r * ANNULUS;
            if at[0] - margin < 0.0
                || at[1] - margin < 0.0
                || at[0] + margin >= w as f32
                || at[1] + margin >= h as f32
            {
                continue;
            }
            // Never borrow from another blemish, which is a repair that moves
            // the problem rather than removing it.
            if taken.iter().any(|other| {
                let o = [
                    other.centre[0] * width as f32,
                    other.centre[1] * height as f32,
                ];
                let (dx, dy) = (o[0] - at[0], o[1] - at[1]);
                (dx * dx + dy * dy).sqrt() < r + other.radius * width as f32
            }) {
                continue;
            }
            let Some((mean, variance)) = disc(mosaic, w, h, at, r) else {
                continue;
            };
            if mean <= 0.0 {
                continue;
            }
            // Relative, so a candidate in shadow is not preferred merely for
            // having less light to vary.
            let texture = variance.sqrt() / mean;
            let mismatch = match target {
                Some(t) if t > 0.0 => (mean / t).ln().abs(),
                _ => 0.0,
            };
            let score = texture + 2.0 * mismatch;
            if best.is_none_or(|(_, b)| score < b) {
                best = Some(([at[0] / width as f32, at[1] / height as f32], score));
            }
        }
    }

    // Nothing was eligible — a spot placed in the corner of a small frame, or one
    // surrounded by others. Offering the blemish itself would look like the tool
    // had failed silently, so it goes a couple of radii to the left and the user
    // moves it.
    best.map(|(at, _)| at)
        .unwrap_or([(centre[0] - 2.5 * radius).clamp(0.0, 1.0), centre[1]])
}

/// Mean and variance over a disc, with the CFA checkerboard taken out.
///
/// Computed per position in the 2×2 block and then averaged, because a Bayer
/// mosaic's raw variance is dominated by the difference between the colours
/// rather than by anything in the photograph — every candidate would score the
/// same and the search would be picking at random.
fn disc(mosaic: &[f32], w: i64, h: i64, centre: [f32; 2], radius: f32) -> Option<(f32, f32)> {
    let mut sum = [0f64; 4];
    let mut squares = [0f64; 4];
    let mut count = [0u32; 4];
    let reach = radius.ceil() as i64;
    let (x0, y0) = (
        centre[0].floor() as i64 - reach,
        centre[1].floor() as i64 - reach,
    );
    for y in y0..=y0 + 2 * reach {
        if y < 0 || y >= h {
            continue;
        }
        for x in x0..=x0 + 2 * reach {
            if x < 0 || x >= w {
                continue;
            }
            let (dx, dy) = (x as f32 + 0.5 - centre[0], y as f32 + 0.5 - centre[1]);
            if (dx * dx + dy * dy).sqrt() > radius {
                continue;
            }
            let i = ((y & 1) * 2 + (x & 1)) as usize;
            let v = f64::from(mosaic[(y * w + x) as usize]);
            sum[i] += v;
            squares[i] += v * v;
            count[i] += 1;
        }
    }
    let mut mean = 0f64;
    let mut variance = 0f64;
    let mut used = 0;
    for i in 0..4 {
        if count[i] < 2 {
            continue;
        }
        let n = f64::from(count[i]);
        let m = sum[i] / n;
        mean += m;
        variance += (squares[i] / n - m * m).max(0.0);
        used += 1;
    }
    (used > 0).then(|| {
        (
            (mean / f64::from(used)) as f32,
            (variance / f64::from(used)) as f32,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: u32 = 128;

    /// A mosaic whose value says which colour it is.
    ///
    /// The integer part is the position in the 2×2 block and the fraction is a
    /// slow ramp, so a texel that borrows from the wrong photosite is not merely
    /// a little bit off — it lands in a different band entirely, and the
    /// assertions can say so rather than hoping a tolerance catches it.
    fn striped() -> Vec<f32> {
        let mut m = vec![0f32; (N * N) as usize];
        for y in 0..N {
            for x in 0..N {
                let colour = ((y & 1) * 2 + (x & 1)) as f32;
                m[(y * N + x) as usize] = colour * 8.0 + ((x + y) % 7) as f32 * 0.1;
            }
        }
        m
    }

    fn band(v: f32) -> i32 {
        (v / 8.0).floor() as i32
    }

    fn patch(spot: Spot, mosaic: &[f32]) -> Vec<f32> {
        let resolved = resolve(&[spot], mosaic, N, N);
        let mut out = mosaic.to_vec();
        apply(&resolved, mosaic, N, N, [0, 0], N, &mut out);
        out
    }

    #[test]
    fn a_clone_borrows_a_photosite_of_its_own_colour() {
        // The source is offset by a fractional, odd number of pixels on purpose:
        // 5.4 rounds to 5, which is odd, and taking it as it comes would copy
        // red into green. It must be snapped down to 4.
        let mosaic = striped();
        let spot = Spot {
            centre: [0.5, 0.5],
            source: [0.5 + 5.4 / N as f32, 0.5 + 3.6 / N as f32],
            radius: 8.0 / N as f32,
            feather: 0.0,
            mode: SpotMode::Clone,
        };
        let out = patch(spot, &mosaic);

        let (cx, cy) = (0.5 * N as f32, 0.5 * N as f32);
        let mut checked = 0;
        for y in 0..N {
            for x in 0..N {
                let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                let inside = (dx * dx + dy * dy).sqrt() <= 8.0;
                let i = (y * N + x) as usize;
                if !inside {
                    assert_eq!(out[i], mosaic[i], "({x}, {y}) is outside the spot");
                    continue;
                }
                let borrowed = mosaic[((y + 4) * N + x + 4) as usize];
                assert_eq!(out[i], borrowed, "({x}, {y}) took the wrong texel");
                assert_eq!(
                    band(out[i]),
                    band(mosaic[i]),
                    "({x}, {y}) changed colour: the offset was not even"
                );
                checked += 1;
            }
        }
        assert!(checked > 150, "only {checked} texels were covered");
    }

    #[test]
    fn a_dust_mark_is_removed() {
        // A dust mark is a multiplicative shadow, so this is one. It is drawn
        // smaller than the spot placed over it, which is how anybody uses the
        // tool and also what keeps the ring the correction is measured over on
        // clean data.
        let clean = striped();
        let mut dusty = clean.clone();
        let (cx, cy) = (0.5 * N as f32, 0.5 * N as f32);
        let mut inside = Vec::new();
        for y in 0..N {
            for x in 0..N {
                let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                let r = (dx * dx + dy * dy).sqrt();
                if r <= 8.0 {
                    dusty[(y * N + x) as usize] *= 0.5;
                }
                if r <= 7.0 {
                    inside.push((y * N + x) as usize);
                }
            }
        }

        let out = patch(
            Spot {
                centre: [0.5, 0.5],
                source: [0.25, 0.25],
                radius: 10.0 / N as f32,
                feather: 0.0,
                mode: SpotMode::Heal,
            },
            &dusty,
        );

        let residual = |m: &[f32]| {
            inside.iter().map(|&i| (m[i] - clean[i]).abs()).sum::<f32>() / inside.len() as f32
        };
        let before = residual(&dusty);
        let after = residual(&out);
        assert!(
            before > 3.0,
            "the synthetic dust mark was only {before} deep, so this proves nothing"
        );
        assert!(
            after < 0.4,
            "the mark is still {after} off the clean frame, against {before} before"
        );
    }

    #[test]
    fn a_heal_scales_the_borrowed_light_and_a_clone_does_not() {
        // The source sits in shade: the left half of this frame has half the
        // light of the right. A clone brings that shade across with it, which is
        // the disc-of-the-wrong-brightness every naive repair produces. A heal
        // is the same copy with the destination's own light level put back.
        let mut mosaic = striped();
        for y in 0..N {
            for x in 0..N / 2 {
                mosaic[(y * N + x) as usize] *= 0.5;
            }
        }
        let spot = Spot {
            centre: [0.75, 0.5],
            source: [0.25, 0.5],
            radius: 8.0 / N as f32,
            feather: 0.0,
            mode: SpotMode::Heal,
        };
        let at = |m: &[f32]| m[(64 * N + 96) as usize];
        let healed = at(&patch(spot, &mosaic));
        let cloned = at(&patch(
            Spot {
                mode: SpotMode::Clone,
                ..spot
            },
            &mosaic,
        ));
        let neighbour = mosaic[(64 * N + 110) as usize];

        assert!(
            (healed / neighbour - 1.0).abs() < 0.1,
            "the heal landed at {healed} beside surroundings of {neighbour}"
        );
        assert!(
            cloned < healed * 0.75,
            "the clone at {cloned} should have arrived in shade beside the heal at {healed}"
        );
    }

    #[test]
    fn the_rim_fades_rather_than_steps() {
        let mosaic = striped();
        let spot = Spot {
            centre: [0.5, 0.5],
            source: [0.25, 0.5],
            radius: 12.0 / N as f32,
            feather: 0.6,
            mode: SpotMode::Clone,
        };
        let out = patch(spot, &mosaic);
        let i = |x: u32, y: u32| (y * N + x) as usize;
        // Eleven pixels out of twelve: inside the spot, and well into the fade.
        let rim = i(75, 64);
        let borrowed = mosaic[i(75 - 32, 64)];
        let original = mosaic[rim];
        let between = (out[rim] - original) / (borrowed - original);
        assert!(
            (0.01..0.99).contains(&between),
            "the rim landed at {between} of the way across, so it stepped"
        );
        // The very centre is not fading at all.
        assert_eq!(out[i(64, 64)], mosaic[i(32, 64)]);
    }

    #[test]
    fn a_spot_this_tile_cannot_see_leaves_it_alone() {
        let mosaic = striped();
        let resolved = resolve(
            &[Spot {
                centre: [0.1, 0.1],
                source: [0.3, 0.3],
                radius: 4.0 / N as f32,
                feather: 0.0,
                mode: SpotMode::Clone,
            }],
            &mosaic,
            N,
            N,
        );
        // A 32-square tile at the far corner, gathered as if by the renderer.
        let tile = 32u32;
        let origin = [90i64, 90];
        let mut out = vec![0f32; (tile * tile) as usize];
        for y in 0..tile {
            for x in 0..tile {
                let (gx, gy) = (origin[0] as u32 + x, origin[1] as u32 + y);
                out[(y * tile + x) as usize] = mosaic[(gy * N + gx) as usize];
            }
        }
        let before = out.clone();
        apply(&resolved, &mosaic, N, N, origin, tile, &mut out);
        assert_eq!(out, before);
    }

    #[test]
    fn a_proposed_source_avoids_the_blemish_it_is_for() {
        let mosaic = striped();
        let centre = [0.5, 0.5];
        let radius = 6.0 / N as f32;
        let source = propose_source(&mosaic, N, N, centre, radius, &[]);
        let (dx, dy) = (
            (source[0] - centre[0]) * N as f32,
            (source[1] - centre[1]) * N as f32,
        );
        let distance = (dx * dx + dy * dy).sqrt();
        assert!(
            distance > 6.0,
            "the proposal landed {distance} pixels away, inside the spot"
        );
        // And inside the frame, with room for the ring a heal measures over.
        for (v, extent) in [(source[0], N), (source[1], N)] {
            let px = v * extent as f32;
            assert!(
                px > 6.0 && px < extent as f32 - 6.0,
                "the proposal at {px} hangs off the edge"
            );
        }

        // Now block the place it chose with another blemish. Borrowing from one
        // is a repair that moves the problem rather than removing it, so the
        // search has to go somewhere else — anywhere else.
        let occupied = Spot {
            centre: source,
            radius,
            ..Spot::default()
        };
        let moved = propose_source(&mosaic, N, N, centre, radius, &[occupied]);
        let apart = ((moved[0] - source[0]) * N as f32).hypot((moved[1] - source[1]) * N as f32);
        assert!(
            apart > 6.0,
            "the proposal stayed {apart} pixels from a source it was told not to use"
        );
    }
}
