//! Lateral chromatic aberration: measuring it, so the renderer can undo it.
//!
//! A lens does not focus every wavelength to the same size. Red, green and blue
//! land as images at slightly different scales about the optical axis, so a
//! feature near the edge of the frame sits a fraction of a pixel further out in
//! one channel than another. In the middle of the picture that is invisible; at
//! the corners, on a high-contrast edge — a bare branch against a bright sky —
//! it is a coloured rim.
//!
//! # Why this is measured rather than looked up
//!
//! The maker's own coefficients would be exact, and this exists because they are
//! not there. `DSC01588.ARW` carries every one of Sony's encrypted maker-note
//! tags, and the cipher on them is no obstacle — deciphered, they turn from
//! noise into obviously structured values. Searched for a radial correction
//! curve, all of them together yield nothing, and the same search finds no
//! *distortion* curve either, which is the control saying the search works.
//!
//! Measuring is not merely the fallback: it needs no undocumented format that a
//! firmware update can move, and it works on a manual lens on an adapter, where
//! there is no maker data at all and never will be.
//!
//! # Why this takes demosaiced pixels and not the mosaic
//!
//! Because on a Bayer grid the three channels are not comparable, and the two
//! reasons are each larger than the quarter-pixel signal being measured.
//!
//! - They are sampled in **different places**. Red sits at one corner of every
//!   2x2 block and blue at the opposite one, half a pixel apart on the diagonal.
//! - They are sampled with **different point spreads**. Green has two sites per
//!   block and the others have one, so any green plane built from a mosaic is
//!   blurrier than the red and blue beside it — and comparing a sharp channel
//!   against a blurred one measures the blur, not the displacement.
//!
//! Three attempts at a mosaic-based estimator failed on those two facts, each
//! reading several times under the truth. After the demosaic all three channels
//! are on one grid, and the question becomes the simple one it looks like.

use rawkit_editstate::{Lens, MAX_LATERAL};

/// How many usable edges a fit needs before it is worth trusting.
const ENOUGH: usize = 4_000;

/// How far along the radius each little profile reaches.
const REACH: usize = 3;

/// Measure a photograph's lateral aberration, end to end.
///
/// Demosaics the frame and fits the displacement, returning what the renderer
/// would have to do to undo it. The caller stores that in the [`EditState`] —
/// see [`Lens`] for why a measurement belongs in the edit rather than beside the
/// mosaic.
///
/// # What it renders, and what it turns off
///
/// Scene-linear, so no tone curve has changed the shape of an edge differently
/// at different brightnesses. And **noise reduction off**, which is not a
/// nicety: chroma smoothing works by pulling the channels towards each other,
/// which is precisely the difference being measured. Left on at its default it
/// would report a lens rather better than the one on the camera.
///
/// At full resolution rather than a pyramid level. The answer is a *fraction* of
/// the radius and so is scale-invariant in principle, but halving the image
/// halves the displacement in pixels while leaving the sensor noise where it
/// was, and the quantity here is already a hundredth of a pixel on a good lens.
/// This costs a render; it happens when somebody asks for it, once.
pub fn measure(
    gpu: &crate::Gpu,
    renderer: &crate::render::Renderer,
    image: &crate::render::Frame<'_>,
) -> Result<Lens, crate::EngineError> {
    let state = rawkit_editstate::EditState {
        detail: rawkit_editstate::Detail {
            chroma_noise: 0.0,
            luminance_noise: 0.0,
            ..Default::default()
        },
        // Measured from an uncorrected frame. Measuring through a correction
        // would give a residual, which is a different number that happens to
        // look like this one whenever the correction is zero.
        lens: Lens::default(),
        // Uncropped and unrotated: the fit is radial about the optical centre,
        // and a crop moves the centre of the *picture* without moving the centre
        // of the lens.
        ..Default::default()
    };
    let rendered = renderer.run(gpu, image, &state, crate::render::Output::SceneLinear)?;
    Ok(estimate(&rendered.pixels, rendered.width, rendered.height))
}

/// Measure the aberration in a developed frame.
///
/// `rgba` is four floats per pixel, and wants to be **scene-linear** — before
/// the tone map, so an edge has not been through a curve that changes its shape
/// differently at different brightnesses.
pub fn estimate(rgba: &[f32], width: u32, height: u32) -> Lens {
    let (w, h) = (width as usize, height as usize);
    if w < 128 || h < 128 || rgba.len() < w * h * 4 {
        return Lens::default();
    }
    let at = |x: usize, y: usize, c: usize| rgba[(y * w + x) * 4 + c];
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);

    // Fitted through the origin: aberration is zero on the optical axis by
    // definition, so a free intercept would only give the noise somewhere to go.
    let mut fit = [(0.0f64, 0.0f64); 2];
    let mut used = 0usize;

    let step = 1 + (w * h) / 4_000_000;
    for y in (REACH + 1..h - REACH - 1).step_by(step) {
        for x in (REACH + 1..w - REACH - 1).step_by(step) {
            let (ox, oy) = (x as f32 - cx, y as f32 - cy);
            // The radial direction, snapped to an axis so the profile can be
            // read without interpolating — which would blur away the sub-pixel
            // detail this exists to measure.
            let horizontal = ox.abs() > oy.abs();
            let along = if horizontal { ox } else { oy };
            // How much of the radial displacement lies along that axis: the
            // displacement is `a * (ox, oy)`, so its component on the outward
            // axis is `a * |along|`. Fitting against `|along|` rather than
            // against the full radius is what keeps the answer unbiased.
            let reach = along.abs();
            if reach < (w.min(h) as f32) * 0.12 {
                continue;
            }
            let outward: isize = if along > 0.0 { 1 } else { -1 };
            let sample = |c: usize, k: isize| -> f32 {
                let (sx, sy) = if horizontal {
                    ((x as isize + k * outward) as usize, y)
                } else {
                    (x, (y as isize + k * outward) as usize)
                };
                at(sx, sy, c)
            };

            // Green and its own slope, on the interior of the little profile
            // so every term is a central difference and none is one-sided.
            let mut g = [0.0f32; 2 * REACH - 1];
            let mut gp = [0.0f32; 2 * REACH - 1];
            for (i, slot) in (1..2 * REACH).enumerate() {
                let k = slot as isize - REACH as isize;
                g[i] = sample(1, k);
                gp[i] = (sample(1, k + 1) - sample(1, k - 1)) / 2.0;
            }
            let sgg: f32 = g.iter().map(|v| v * v).sum();
            let spp: f32 = gp.iter().map(|v| v * v).sum();
            let sgp: f32 = g.iter().zip(&gp).map(|(a, b)| a * b).sum();
            // A real edge: the gradient has to be a decent fraction of the level,
            // or the shift is being read off noise.
            if sgg < 1e-6 || spp / sgg < 1e-4 {
                continue;
            }
            let det = sgg * spp - sgp * sgp;
            // And green and its slope have to be distinguishable from each
            // other over this window, or the two-parameter fit below is really
            // one parameter and the answer is whatever the noise says.
            if det <= 0.0 || det / (sgg * spp) < 0.05 {
                continue;
            }

            let mut counted = false;
            for (c, slot) in [(0usize, 0usize), (2, 1)] {
                // Fit `other = level * green + beta * green'`, and the
                // displacement is `beta / level`.
                //
                // Two parameters, fitted together, and *nothing* normalised
                // beforehand. That is the whole lesson of three failed attempts:
                // every way of making the channels comparable in advance —
                // centring on a window mean, dividing by one — computes that
                // statistic over the *shifted* window, and so subtracts the very
                // displacement being measured. On a locally straight edge it
                // removes all of it. Only a joint fit leaves the shift somewhere
                // to go, because `green` and `green'` are different shapes and
                // the level cannot pretend to be the slope.
                let mut sgo = 0.0f32;
                let mut spo = 0.0f32;
                for (i, slot) in (1..2 * REACH).enumerate() {
                    let o = sample(c, slot as isize - REACH as isize);
                    sgo += g[i] * o;
                    spo += gp[i] * o;
                }
                let level = (sgo * spp - spo * sgp) / det;
                let beta = (sgg * spo - sgp * sgo) / det;
                if level.abs() < 0.05 {
                    continue;
                }
                let shift = beta / level;
                if !shift.is_finite() || shift.abs() > 4.0 {
                    continue;
                }
                fit[slot].0 += (reach * shift) as f64;
                fit[slot].1 += (reach * reach) as f64;
                counted = true;
            }
            if counted {
                used += 1;
            }
        }
    }

    if used < ENOUGH {
        return Lens::default();
    }
    let solve = |(num, den): (f64, f64)| -> f32 {
        if den <= 0.0 {
            return 0.0;
        }
        // `num/den` is how far this channel's content is displaced *outward* per
        // pixel of radius. To undo that, sample it that much further *in* — so
        // the correction is the negative of the measurement.
        let a = -(num / den) as f32;
        if !a.is_finite() || a.abs() > MAX_LATERAL {
            0.0
        } else {
            a
        }
    };
    Lens {
        chromatic_red: solve(fit[0]),
        chromatic_blue: solve(fit[1]),
        distortion: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A developed frame with a known aberration in it.
    ///
    /// Every channel is the same scene, sampled at its own scale about the
    /// centre — which is what lateral aberration *is*, so the estimator has a
    /// right answer to be checked against. Bilinear, so a sub-pixel scale makes
    /// a sub-pixel difference rather than none at all.
    ///
    /// Nothing here is a mosaic. The previous attempt at this test built one,
    /// which put the Bayer sampling offset and the aberration into the same
    /// number and made a failing estimator look like a failing measurement.
    fn frame(w: u32, h: u32, scale: [f32; 3], level: [f32; 3]) -> Vec<f32> {
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let scene = |u: f32, v: f32| -> f32 {
            let a = ((u * 0.11).sin() + (v * 0.143).sin() + ((u + v) * 0.037).sin()) / 3.0;
            0.35 + 0.28 * a
        };
        let mut out = vec![0.0f32; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let base = (y as usize * w as usize + x as usize) * 4;
                for c in 0..3 {
                    let s = scale[c];
                    let (u, v) = (cx + (x as f32 - cx) * s, cy + (y as f32 - cy) * s);
                    out[base + c] = scene(u, v) * level[c];
                }
                out[base + 3] = 1.0;
            }
        }
        out
    }

    const W: u32 = 1400;
    const H: u32 = 1000;

    #[test]
    fn a_known_aberration_is_recovered() {
        // Red's content sampled from 0.06% further out, blue's from 0.09%
        // closer in. The correction is the other way round, which is what
        // `Lens` means and what this pins.
        let (sr, sb) = (1.0006f32, 0.9991f32);
        let rgba = frame(W, H, [sr, 1.0, sb], [0.9, 1.0, 1.15]);
        let found = estimate(&rgba, W, H);
        println!("scales {sr} / {sb}  ->  {found:?}");
        assert!(
            (found.chromatic_red - -(sr - 1.0)).abs() < 0.00012,
            "red came back {} and should undo {}",
            found.chromatic_red,
            sr - 1.0
        );
        assert!(
            (found.chromatic_blue - -(sb - 1.0)).abs() < 0.00012,
            "blue came back {} and should undo {}",
            found.chromatic_blue,
            sb - 1.0
        );
    }

    #[test]
    fn different_channel_levels_are_not_a_displacement() {
        // The channels of a photograph differ in brightness everywhere, and none
        // of that is aberration. A frame with three very different levels and no
        // scale difference must measure nothing.
        let found = estimate(&frame(W, H, [1.0, 1.0, 1.0], [0.4, 1.0, 1.9]), W, H);
        println!("levels only: {found:?}");
        assert!(
            found.chromatic_red.abs() < 3e-5 && found.chromatic_blue.abs() < 3e-5,
            "invented an aberration from a level difference: {found:?}"
        );
    }

    #[test]
    fn a_frame_with_nothing_to_measure_is_refused() {
        // Flat grey has no edges to fit, so the honest answer is to say so
        // rather than to fit the noise. Refusal and "a perfect lens" are the
        // same output, which is why the count is checked and not the residual.
        let flat = vec![0.4f32; (W * H * 4) as usize];
        assert_eq!(estimate(&flat, W, H), Lens::default());
        assert_eq!(estimate(&[0.4; 4 * 64 * 64], 64, 64), Lens::default());
    }

    #[test]
    fn an_implausible_fit_is_thrown_away() {
        // Well past any real lens: something that is not aberration has been
        // found, and applying it would be worse than doing nothing.
        let found = estimate(&frame(W, H, [1.02, 1.0, 0.98], [1.0, 1.0, 1.0]), W, H);
        println!("absurd lens: {found:?}");
        assert_eq!(found, Lens::default());
    }
}
