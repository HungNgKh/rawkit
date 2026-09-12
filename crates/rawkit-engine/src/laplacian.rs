//! An edge-aware smoothing that works at every scale at once.
//!
//! # The problem one radius cannot solve
//!
//! [`crate::guide`] smooths the frame with a range-weighted blur: neighbours
//! count less the further away they are *and* the more their brightness
//! differs. One radius, one range width, and the two things it is asked to do
//! turn out to pull in opposite directions. Measured, in
//! `how_small_an_object_may_be_and_still_be_its_own_region`:
//!
//! - a dark object **two stops** under its surroundings and smaller than about a
//!   quarter of the frame is keyed almost entirely by what is around it, so a
//!   shadow lift misses it by more than a stop;
//! - and tightening the range term until that stops happening also stops a
//!   four-pixel line being carried by the field it crosses, which is the
//!   property the blur exists to have.
//!
//! The two cases are the same configuration with opposite right answers, and
//! they differ in *scale*. No single radius expresses that.
//!
//! # Why a Gaussian range term cannot be tuned out of it
//!
//! Because it leaks. A neighbour two stops away still carries `exp(-2) = 0.135`
//! of the weight of one at the same brightness, and a small object has twenty
//! texels of surround against its own few — so the surround wins on sheer
//! count. Narrowing the Gaussian to shut that out narrows it for the thin line
//! as well; the falloff has no way to treat 1.3 stops and 2.0 stops as
//! different in kind, only as different in degree.
//!
//! What is wanted is a **threshold**: below it, flatten; above it, leave alone.
//! A bilateral filter cannot have one — a hard cut in an averaging weight puts a
//! discontinuity wherever a neighbour crosses it. This can, and that is the
//! whole of what it is for.
//!
//! # How it avoids the discontinuity
//!
//! Local Laplacian filtering (Paris, Hasinoff and Kautz, SIGGRAPH 2011 —
//! the method Adobe credited for Lightroom's PV2012 Highlights and Shadows).
//! It never averages across the threshold. Instead, for each coefficient of the
//! *output's* Laplacian pyramid it remaps the whole image about that
//! coefficient's own local level, builds a pyramid of the remapped image, and
//! takes the one coefficient it wanted. Clipping the input is fine; clipping the
//! coefficients is what rounds edges off, and this clips the input.
//!
//! Keyed on the local level *at that scale*, which is what makes the answer
//! multi-scale: a fine coefficient asks about a fine neighbourhood and a coarse
//! one about a coarse neighbourhood, and the same object can be detail at one
//! scale and an edge at another.
//!
//! # The acceleration, and what it costs
//!
//! Done literally that is `O(N log N)` with a pyramid built per coefficient —
//! seconds per megapixel. [`smooth`] uses the sampling scheme from *Fast Local
//! Laplacian Filters* (Aubry, Paris, Hasinoff, Kautz and Durand, TOG 2014):
//! precompute a pyramid for each of a handful of levels spread across the
//! range, then linearly interpolate each output coefficient between the two
//! that bracket its own local level. One sample per `sigma` is the paper's
//! stated rate and what [`smooth`] uses.
//!
//! This runs on the guide, which is a few hundred pixels on its longest edge —
//! about a tenth of a megapixel. That is the reason it is affordable at all, and
//! also the ceiling on what it can see: structure finer than a guide texel is
//! not here to be resolved, whatever the operator does with it.
//!
//! # ⚠️ Nothing uses this yet, and the measurement is why
//!
//! It was wired into [`crate::guide`] in place of the blur's level half, swept,
//! and taken back out. Stated plainly so that the next person to reach for it
//! starts from the numbers rather than from the argument above.
//!
//! Against the two properties the guide has to hold at once — the object table
//! in `how_small_an_object_may_be_and_still_be_its_own_region`, and the
//! thin-line separation in `the_same_value_is_treated_by_where_it_is`:
//!
//! | level filter | 2-texel object, +2 EV | thin-line separation |
//! |---|---|---|
//! | blur, range 1.0 (shipped) | 0.08 | **1.315** |
//! | blur, range 0.5 | **1.01** | 1.232 |
//! | this, sigma 1.6 | 0.88 | 1.168 |
//! | this, sigma 1.0 | 0.98 | 1.120 |
//! | this, sigma 0.8 | **1.00** | 1.122 |
//!
//! **A one-constant change to the blur beats it on both axes.** There is no
//! sigma here that holds the line separation where the blur holds it, and at
//! the sigma that would (1.6) large objects are *worse* than they are today —
//! 0.62 against 1.00 — because a two-stop step is then only 1.25 sigma, and for
//! a centre midway between its two sides both fall inside the threshold and get
//! pulled together. The threshold has to sit well under half the step it is
//! meant to preserve, and under half of two stops there is nothing left to
//! separate 1.3 stops from 2.0 with.
//!
//! # The structural reason, which is the useful part
//!
//! The thin-line property is delivered by **spatial reach**: a four-pixel line
//! is carried by the field it crosses because the blur averages over a radius
//! far wider than the line. This filter has no spatial radius — it separates by
//! range, at every scale — so it has no mechanism for that property at all, and
//! the measurement above is what that absence looks like.
//!
//! Which says the two are not alternatives. A filter that held both would need
//! the blur's reach *and* this one's threshold, and finding out whether that
//! composes is a piece of research rather than a slice. The module is left here,
//! tested and correct in itself, as the floor such a thing would be built on.
//!
//! It also has a cheaper implication worth not losing: the whole defect may be
//! one constant. `RANGE_STOPS` at 0.5 takes every cell of the object table to
//! 1.00 and costs 0.08 of line separation, which is a trade somebody should
//! look at in the window rather than in a table.

/// Burt and Adelson's 5-tap kernel, the one the method is defined against.
const KERNEL: [f32; 5] = [0.0625, 0.25, 0.375, 0.25, 0.0625];

/// A pyramid level: values, and the size they are laid out at.
type Level = (Vec<f32>, usize, usize);

fn at(src: &[f32], w: usize, h: usize, x: isize, y: isize) -> f32 {
    // Clamped at the border. Mirroring would be defensible too; what matters is
    // that every level uses the same rule, or the collapse does not invert the
    // split and a seam appears at the frame edge.
    let x = x.clamp(0, w as isize - 1) as usize;
    let y = y.clamp(0, h as isize - 1) as usize;
    src[y * w + x]
}

/// Blur and drop every other sample.
fn downsample(src: &[f32], w: usize, h: usize) -> Level {
    let (nw, nh) = (w.div_ceil(2), h.div_ceil(2));
    let mut rows = vec![0.0f32; nw * h];
    for y in 0..h {
        for x in 0..nw {
            let cx = (x * 2) as isize;
            rows[y * nw + x] = KERNEL
                .iter()
                .enumerate()
                .map(|(k, weight)| weight * at(src, w, h, cx + k as isize - 2, y as isize))
                .sum();
        }
    }
    let mut out = vec![0.0f32; nw * nh];
    for y in 0..nh {
        let cy = (y * 2) as isize;
        for x in 0..nw {
            out[y * nw + x] = KERNEL
                .iter()
                .enumerate()
                .map(|(k, weight)| weight * at(&rows, nw, h, x as isize, cy + k as isize - 2))
                .sum();
        }
    }
    (out, nw, nh)
}

/// Put the samples back where they came from and blur the gaps closed.
///
/// The target size is passed rather than doubled, because `(w + 1) / 2` loses
/// the parity on the way down and an odd level upsampled to an even one is off
/// by a column for every row beneath it.
fn upsample(src: &[f32], w: usize, h: usize, tw: usize, th: usize) -> Vec<f32> {
    let mut rows = vec![0.0f32; tw * h];
    for y in 0..h {
        for x in 0..tw {
            // Twice the kernel, because half the taps land on inserted zeros
            // and the weight has to come back.
            let mut sum = 0.0;
            for (k, weight) in KERNEL.iter().enumerate() {
                let sx = x as isize + k as isize - 2;
                if sx % 2 == 0 {
                    sum += 2.0 * weight * at(src, w, h, sx / 2, y as isize);
                }
            }
            rows[y * tw + x] = sum;
        }
    }
    let mut out = vec![0.0f32; tw * th];
    for y in 0..th {
        for x in 0..tw {
            let mut sum = 0.0;
            for (k, weight) in KERNEL.iter().enumerate() {
                let sy = y as isize + k as isize - 2;
                if sy % 2 == 0 {
                    sum += 2.0 * weight * at(&rows, tw, h, x as isize, sy / 2);
                }
            }
            out[y * tw + x] = sum;
        }
    }
    out
}

/// Successively halved copies, coarsest last.
fn gaussian_pyramid(values: &[f32], w: usize, h: usize, levels: usize) -> Vec<Level> {
    let mut out = vec![(values.to_vec(), w, h)];
    for _ in 1..levels {
        let (ref v, vw, vh) = out[out.len() - 1];
        out.push(downsample(v, vw, vh));
    }
    out
}

/// What each level of a Gaussian pyramid adds over the one above it.
///
/// The last entry is the residual — the coarsest Gaussian level itself — so a
/// collapse has somewhere to start.
fn laplacian_from(gauss: &[Level]) -> Vec<Level> {
    let mut out = Vec::with_capacity(gauss.len());
    for i in 0..gauss.len() - 1 {
        let (ref fine, fw, fh) = gauss[i];
        let (ref coarse, cw, ch) = gauss[i + 1];
        let up = upsample(coarse, cw, ch, fw, fh);
        out.push((
            fine.iter().zip(&up).map(|(a, b)| a - b).collect::<Vec<_>>(),
            fw,
            fh,
        ));
    }
    out.push(gauss[gauss.len() - 1].clone());
    out
}

/// Add the levels back together, coarsest first.
fn collapse(levels: &[Level]) -> Vec<f32> {
    let (mut acc, mut aw, mut ah) = levels[levels.len() - 1].clone();
    for i in (0..levels.len() - 1).rev() {
        let (ref detail, dw, dh) = levels[i];
        let up = upsample(&acc, aw, ah, dw, dh);
        acc = detail.iter().zip(&up).map(|(a, b)| a + b).collect();
        aw = dw;
        ah = dh;
    }
    acc
}

/// The point-wise remapping, about a local level.
///
/// Inside `sigma` of `centre` the difference is raised to `alpha`, which for
/// `alpha > 1` shrinks it — that is the flattening. Outside, the value is
/// returned untouched, so an edge keeps both its amplitude and its profile.
///
/// Continuous at `centre ± sigma` by construction: at a difference of exactly
/// `sigma` the normalised difference is 1, and 1 to any power is 1.
fn remap(value: f32, centre: f32, sigma: f32, alpha: f32) -> f32 {
    let d = value - centre;
    let mag = d.abs();
    if mag >= sigma {
        return value;
    }
    centre + d.signum() * sigma * (mag / sigma).powf(alpha)
}

/// How many pyramid levels are worth building for a field this size.
///
/// Stops while a level still has enough samples for the 5-tap kernel to mean
/// anything. Past that the levels are noise dressed as structure.
fn depth(w: usize, h: usize) -> usize {
    let mut levels = 1;
    let (mut cw, mut ch) = (w, h);
    while cw > 8 && ch > 8 && levels < 10 {
        cw = cw.div_ceil(2);
        ch = ch.div_ceil(2);
        levels += 1;
    }
    levels
}

/// Flatten everything within `sigma` of its surroundings and leave everything
/// beyond it alone, at every scale.
///
/// `values` is expected in a **logarithmic** unit — stops, for the guide — so
/// that `sigma` is a ratio and means the same thing at every brightness. Giving
/// it linear light would make the operator strong in the highlights and absent
/// in the shadows, which is the defect the log domain exists to avoid.
///
/// `alpha` above 1 flattens; 1 is the identity and returns the input unchanged.
///
/// # Cost
///
/// One Gaussian pyramid of the input, plus one remapped pyramid per sample
/// across the range — the samples are the expensive part and there are
/// `range / sigma` of them, bounded below at 4 and above at 24. On the guide's
/// 384-by-384 that is a few tens of milliseconds.
pub fn smooth(values: &[f32], w: usize, h: usize, sigma: f32, alpha: f32) -> Vec<f32> {
    if w == 0 || h == 0 || values.len() != w * h || sigma <= 0.0 || alpha == 1.0 {
        return values.to_vec();
    }
    let (mut low, mut high) = (f32::INFINITY, f32::NEG_INFINITY);
    for v in values {
        if v.is_finite() {
            low = low.min(*v);
            high = high.max(*v);
        }
    }
    if !low.is_finite() || high <= low {
        // Flat, or nothing finite in it. Either way there is no structure to
        // separate and the honest answer is what arrived.
        return values.to_vec();
    }

    let levels = depth(w, h);
    let gauss = gaussian_pyramid(values, w, h, levels);

    // One sample per sigma, which is the rate the fast paper derives and
    // measures. Bounded at both ends: fewer than four cannot describe a range
    // at all, and past two dozen the interpolation error is already below what
    // the guide's own quantisation contributes.
    let samples = (((high - low) / sigma).ceil() as usize).clamp(4, 24);
    let step = (high - low) / (samples - 1) as f32;

    // The output pyramid, built coefficient by coefficient out of the
    // precomputed ones. The residual is taken from the *input's* coarsest
    // level: it carries the frame's overall brightness, and remapping it would
    // move the whole picture rather than flattening anything within it.
    let mut out: Vec<Level> = (0..levels - 1)
        .map(|i| {
            (
                vec![0.0f32; gauss[i].1 * gauss[i].2],
                gauss[i].1,
                gauss[i].2,
            )
        })
        .collect();
    out.push(gauss[levels - 1].clone());

    let mut previous: Option<Vec<Level>> = None;
    for s in 0..samples {
        let centre = low + step * s as f32;
        let remapped: Vec<f32> = values
            .iter()
            .map(|v| remap(*v, centre, sigma, alpha))
            .collect();
        let current = laplacian_from(&gaussian_pyramid(&remapped, w, h, levels));

        // Every coefficient whose local level falls between this sample and the
        // one before it is finished now, which is what keeps two pyramids in
        // memory rather than all of them.
        if let Some(before) = previous.take() {
            let lower = centre - step;
            for level in 0..levels - 1 {
                let g = &gauss[level].0;
                for (i, local) in g.iter().enumerate() {
                    if *local < lower || *local >= centre {
                        continue;
                    }
                    let a = (local - lower) / step;
                    out[level].0[i] = before[level].0[i] * (1.0 - a) + current[level].0[i] * a;
                }
            }
        }
        // The ends, which no interval brackets: a coefficient below the first
        // sample or at or above the last takes that sample's value outright
        // rather than being extrapolated from a slope it is not on.
        if s == 0 || s == samples - 1 {
            let edge = if s == 0 { low } else { high };
            for level in 0..levels - 1 {
                let g = &gauss[level].0;
                for (i, local) in g.iter().enumerate() {
                    let outside = if s == 0 {
                        *local <= edge
                    } else {
                        *local >= edge - f32::EPSILON
                    };
                    if outside {
                        out[level].0[i] = current[level].0[i];
                    }
                }
            }
        }
        previous = Some(current);
    }

    collapse(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_alpha_returns_what_it_was_given() {
        // The passthrough, and it has to be exact rather than close: this is
        // what lets the operator be introduced without moving a picture that
        // has not asked for it.
        let values: Vec<f32> = (0..64 * 64).map(|i| (i % 17) as f32 * 0.1).collect();
        let out = smooth(&values, 64, 64, 1.0, 1.0);
        assert_eq!(out, values, "alpha of 1 changed the field");
    }

    #[test]
    fn a_flat_field_survives_the_round_trip() {
        // Pyramid up, pyramid down. If the split and the collapse do not invert
        // each other then everything else here is measuring their disagreement
        // rather than the filter.
        let values = vec![0.37f32; 61 * 43];
        let gauss = gaussian_pyramid(&values, 61, 43, depth(61, 43));
        let back = collapse(&laplacian_from(&gauss));
        for (i, v) in back.iter().enumerate() {
            assert!(
                (v - 0.37).abs() < 1e-4,
                "a flat field came back as {v} at {i}"
            );
        }
    }

    #[test]
    fn an_odd_sized_field_round_trips_too() {
        // The parity bug this is here to catch: `(w + 1) / 2` on the way down
        // loses whether the level was odd, and an upsample that assumes even
        // shifts every row beneath it by a column. A ramp makes that visible
        // where a flat field cannot.
        let (w, h) = (61usize, 43usize);
        let values: Vec<f32> = (0..w * h)
            .map(|i| (i % w) as f32 * 0.03 + (i / w) as f32 * 0.017)
            .collect();
        let gauss = gaussian_pyramid(&values, w, h, depth(w, h));
        let back = collapse(&laplacian_from(&gauss));
        let worst = values
            .iter()
            .zip(&back)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-3, "a ramp came back wrong by {worst}");
    }

    #[test]
    fn a_step_keeps_its_height_and_its_texture_does_not() {
        // The whole claim, in the smallest field that can hold it. A step of
        // three sigma with a ripple of a third of a sigma riding on it: the step
        // must survive and the ripple must not.
        let (w, h) = (96usize, 96usize);
        let sigma = 1.0f32;
        let values: Vec<f32> = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                let step = if x < w / 2 { 0.0 } else { 3.0 * sigma };
                let ripple = if (x + y) % 2 == 0 { sigma / 3.0 } else { 0.0 };
                step + ripple
            })
            .collect();
        let out = smooth(&values, w, h, sigma, 4.0);

        // Sampled well away from the boundary, so this measures the filter and
        // not the transition.
        let band = |x0: usize, x1: usize| {
            let mut lo = f32::INFINITY;
            let mut hi = f32::NEG_INFINITY;
            let mut sum = 0.0f64;
            let mut n = 0u32;
            for y in 8..h - 8 {
                for x in x0..x1 {
                    let v = out[y * w + x];
                    lo = lo.min(v);
                    hi = hi.max(v);
                    sum += f64::from(v);
                    n += 1;
                }
            }
            (sum as f32 / n as f32, hi - lo)
        };
        let (left, left_ripple) = band(8, w / 2 - 8);
        let (right, right_ripple) = band(w / 2 + 8, w - 8);

        assert!(
            (right - left - 3.0 * sigma).abs() < 0.35 * sigma,
            "the step arrived {} high, not {}",
            right - left,
            3.0 * sigma
        );
        let before = sigma / 3.0;
        assert!(
            left_ripple < before * 0.5 && right_ripple < before * 0.5,
            "the ripple survived at {left_ripple} / {right_ripple} against {before}"
        );
    }

    #[test]
    fn a_small_region_is_not_swallowed_by_a_large_one() {
        // The defect the whole module exists for, at the scale the guide sees
        // it: a square eight samples across, two sigma below its surroundings,
        // in a field sixteen times its width. A range-weighted blur reads the
        // surround here because there is so much more of it; this must read the
        // square.
        let (w, h) = (128usize, 128usize);
        let sigma = 1.0f32;
        let values: Vec<f32> = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                let inside = (60..68).contains(&x) && (60..68).contains(&y);
                if inside {
                    -2.0 * sigma
                } else {
                    0.0
                }
            })
            .collect();
        let out = smooth(&values, w, h, sigma, 4.0);

        let centre = out[63 * w + 63];
        let far = out[8 * w + 8];
        assert!(
            (centre + 2.0 * sigma).abs() < 0.6 * sigma,
            "the square's centre read {centre}, not {} — it has been swallowed \
             by its surroundings",
            -2.0 * sigma
        );
        assert!(
            far.abs() < 0.2 * sigma,
            "the surroundings read {far} rather than 0, so the square has leaked \
             outwards"
        );
    }
}
