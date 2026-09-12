//! Find what was meant to be level, and say how far it is not.
//!
//! Two faults account for most of the geometry anybody fixes by hand: a horizon
//! that slopes, and a building whose sides lean inwards because the camera was
//! pointed up at it. Both are pure geometry — nothing is invented, only
//! resampled — which is why they can be corrected by pressing one thing.
//!
//! # What this produces, and what it deliberately does not
//!
//! Two numbers: a rotation and a vertical keystone, in exactly the units
//! [`rawkit_editstate::Crop`] already stores. It sets nothing, renders nothing
//! and stores no mode of its own. The caller writes them into the crop, where
//! they become ordinary values a person can drag afterwards — so undo, presets
//! and copied settings all work with no new machinery, and the renderer stays a
//! pure function of the edit.
//!
//! It does not fix horizontal keystone. That matters when a façade is shot from
//! one side rather than from below, which is rarer and much riskier to guess:
//! a wrongly-placed horizontal vanishing point shears the picture, and a shear
//! does not look like a mistake, it looks like a bad lens.
//!
//! # Why the hard part is refusing
//!
//! Detecting lines is the easy half. The half that decides whether the feature
//! is usable is knowing when *not* to act: a portrait, a tree, a close-up of
//! wet rock have no lines that were vertical in the world, and an upright that
//! confidently straightens a tree trunk is worse than one that does nothing.
//! So every stage here has a way to give up, and [`detect`] returns `None`
//! rather than a small correction when it is not sure — a small wrong rotation
//! is more annoying than none, because it looks like the picture was taken
//! badly rather than corrected badly.
//!
//! # The keystone, and where its formula comes from
//!
//! `Geometry::warp` maps a point back to the frame through
//! `(u, v) → (u, v) / (1 - h·u - k·v)` in coordinates normalised to the crop's
//! half-width and half-height. Inverting it, a frame point `(U, V)` lands at
//! `(U, V) / (1 + h·U + k·V)` — so the frame's points at `1 + k·V = 0` are the
//! ones sent to infinity. Verticals that converge at `V_vp` are made parallel
//! by putting the vanishing point there:
//!
//! ```text
//! vertical = -1 / V_vp
//! ```
//!
//! which also produces the documented sign: a camera pointed up puts the
//! vanishing point above the frame, `V_vp` is negative, and `vertical` comes
//! out positive — "bring the top back".
//!
//! # Nothing here crops
//!
//! It does not have to. `Geometry::fit_scale` already solves for the largest
//! the crop can be with all four warped corners still inside the frame, so
//! setting these two numbers produces a picture with no empty corners in it.

/// A correction, in the units the crop stores.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Upright {
    /// Degrees clockwise, for `Crop::angle_deg`.
    pub angle_deg: f32,
    /// For `Crop::vertical`. Positive brings the top back.
    pub vertical: f32,
}

/// How far from an axis a line may sit and still be taken as evidence about it.
///
/// Thirty degrees. Wider admits the diagonals of things — a roofline, a
/// staircase — which are not level and never were; narrower throws away the
/// convergence in exactly the photographs that have most of it, since a
/// strongly keystoned building's sides are a long way from vertical near the
/// edges of the frame.
const AXIS_REACH_DEG: f32 = 30.0;

/// The most rotation this will propose, in degrees.
///
/// A handheld horizon is out by a degree or two. Past about eight the more
/// likely explanation is that the dominant lines are not the ones that were
/// level — a diagonal hillside, a staircase — and a correction built on that
/// reading is confidently wrong. Refusing is the better answer.
const MAX_ANGLE_DEG: f32 = 8.0;

/// The most keystone this will propose.
///
/// Chosen against what the control can express rather than against taste: past
/// this the warp is stretching the far edge so hard that the resampling shows,
/// and `fit_scale` is throwing away most of the frame to hide the corners.
const MAX_VERTICAL: f32 = 0.35;

/// How much of the full convergence to take out.
///
/// **Not all of it, and that is the one piece of taste in this module.** A
/// building rendered to perfectly parallel verticals reads as falling over
/// backwards: the eye expects some convergence when it knows it is looking up,
/// and removing every trace of it is what makes an over-corrected architectural
/// photograph feel wrong without the viewer being able to say why. Adobe's Auto
/// under-corrects for the same reason.
///
/// The remaining sixth is left on the slider, where it can be taken out by
/// hand on the photographs that want it.
const CORRECTION: f32 = 5.0 / 6.0;

/// The fewest lines worth drawing a conclusion from.
const MIN_LINES: usize = 6;

/// One detected line, as its normal's angle and distance from the origin.
#[derive(Debug, Clone, Copy)]
struct Line {
    /// The angle of the line's *normal*, radians. The line runs at
    /// `theta + 90°`.
    theta: f32,
    /// Distance from the image centre along that normal, in pixels.
    rho: f32,
    strength: f32,
}

/// Divisions of the angle accumulator over a half turn.
///
/// A quarter of a degree. The rotation is reported to a tenth, so the bins have
/// to be finer than the answer; and a line's orientation from a gradient is
/// already better than this on anything long enough to matter.
const THETA_BINS: usize = 720;
/// Pixels per distance bin. Coarse on purpose: parallel lines a pixel apart are
/// the same wall, and separating them would split one peak into twenty.
const RHO_STEP: f32 = 2.0;

/// Propose a correction for this picture, or decline to.
///
/// `luma` is the developed brightness of the frame **in the orientation the
/// photographer sees**, at whatever resolution is convenient — a few hundred
/// pixels on the long edge is plenty, because the lines this looks for are the
/// long ones and reducing the image is the cheapest noise rejection there is.
///
/// `None` means it found nothing it was willing to act on. That is a real
/// answer and the common one on frames with no architecture in them.
pub fn detect(luma: &[f32], width: usize, height: usize) -> Option<Upright> {
    let lines = find_lines(luma, width, height)?;
    if lines.len() < MIN_LINES {
        return None;
    }

    let angle_deg = level_from(&lines)?;
    // Verticals are judged *after* the rotation, because a tilted camera makes
    // every vertical lean and that lean is not convergence. Measuring the
    // vanishing point without de-rotating first reads the tilt as keystone and
    // applies it twice.
    let vertical = keystone_from(&lines, width, height, angle_deg.to_radians());

    if angle_deg == 0.0 && vertical == 0.0 {
        return None;
    }
    Some(Upright {
        angle_deg,
        vertical,
    })
}

/// Gradient-weighted Hough, one vote per edge pixel.
///
/// Not the textbook transform, which votes for every angle at every edge pixel
/// and costs a full sweep of the accumulator each time. The gradient already
/// says which way the edge runs, so each pixel votes once, for its own line.
/// That is the same construction and about two orders of magnitude less of it —
/// and it is *sharper*, because the textbook version's sinusoids cross each
/// other and build ridges that are not lines.
fn find_lines(luma: &[f32], width: usize, height: usize) -> Option<Vec<Line>> {
    if width < 16 || height < 16 || luma.len() != width * height {
        return None;
    }
    let (cx, cy) = (width as f32 / 2.0, height as f32 / 2.0);
    let diagonal = (cx * cx + cy * cy).sqrt();
    let rho_bins = (2.0 * diagonal / RHO_STEP).ceil() as usize + 1;
    let mut accumulator = vec![0.0f32; THETA_BINS * rho_bins];

    let at = |x: usize, y: usize| luma[y * width + x];
    let mut magnitudes = Vec::with_capacity((width - 2) * (height - 2));
    let mut votes = Vec::with_capacity((width - 2) * (height - 2));
    for y in 1..height - 1 {
        for x in 1..width - 1 {
            // Sobel. Three-tap because the input is already a reduction of a
            // much larger frame, so anything wider is blurring what has
            // already been blurred.
            let gx = (at(x + 1, y - 1) + 2.0 * at(x + 1, y) + at(x + 1, y + 1))
                - (at(x - 1, y - 1) + 2.0 * at(x - 1, y) + at(x - 1, y + 1));
            let gy = (at(x - 1, y + 1) + 2.0 * at(x, y + 1) + at(x + 1, y + 1))
                - (at(x - 1, y - 1) + 2.0 * at(x, y - 1) + at(x + 1, y - 1));
            let magnitude = gx.hypot(gy);
            if magnitude <= 0.0 || !magnitude.is_finite() {
                continue;
            }
            magnitudes.push(magnitude);
            votes.push((x as f32 - cx, y as f32 - cy, gx, gy, magnitude));
        }
    }
    if magnitudes.len() < 64 {
        return None;
    }

    // A threshold from the picture rather than a constant, because "a strong
    // edge" means something different on a foggy morning than on a white wall.
    //
    // The top *quartile*, not the top decile, and the difference decides
    // whether the feature works at all. Converging lines are shorter and weaker
    // than the parallel ones in the same frame — that is what convergence *is*,
    // the far end of the wall being further away — so a tight threshold keeps
    // the horizon and throws away every vertical that had anything to say. On
    // the test frame that showed as thirteen lines found and **none** of them
    // vertical, on a picture whose verticals fan by ten degrees.
    magnitudes.sort_by(f32::total_cmp);
    let threshold = magnitudes[magnitudes.len() * 3 / 4];

    // And a guard against a picture with no edges in it at all, because a
    // relative threshold always finds something: a smooth gradient across the
    // frame has a top quartile like any other picture, and it is made entirely
    // of "vertical lines", so without this a plain sky gets confidently
    // straightened.
    //
    // **Asked of the strongest edges, not of the threshold.** A ramp's gradient
    // is uniform, so its strongest edge is its only edge; a photograph's
    // strongest edges are far above its seventy-fifth percentile. Testing the
    // threshold instead — which is what this did first — rejects every
    // low-contrast photograph along with the ramps, because three quarters of
    // any real frame is smooth.
    let mut sorted = luma.to_vec();
    sorted.sort_by(f32::total_cmp);
    let range = sorted[sorted.len() * 99 / 100] - sorted[sorted.len() / 100];
    let strongest = magnitudes[magnitudes.len() * 99 / 100];
    if threshold <= 0.0 || strongest < 0.05 * range {
        return None;
    }

    for (dx, dy, gx, gy, magnitude) in votes {
        if magnitude < threshold {
            continue;
        }
        // The gradient *is* the line's normal, so the line through this pixel
        // is fully determined by one pixel. Folded to a half turn: a line and
        // the same line pointing the other way are one line.
        let mut theta = gy.atan2(gx);
        let mut rho = dx * theta.cos() + dy * theta.sin();
        if theta < 0.0 {
            theta += std::f32::consts::PI;
            rho = -rho;
        }
        let tb = ((theta / std::f32::consts::PI) * THETA_BINS as f32) as usize;
        let rb = ((rho + diagonal) / RHO_STEP) as usize;
        if tb < THETA_BINS && rb < rho_bins {
            accumulator[tb * rho_bins + rb] += magnitude;
        }
    }

    // Peaks: a cell that beats its neighbours and a share of the best. The
    // neighbourhood is wider in distance than in angle because `RHO_STEP`
    // already merges nearby parallels and the angle must not be smeared.
    //
    // A twelfth of the best rather than a quarter, for the same reason the
    // threshold above moved: the strongest cell in a frame with a horizon in it
    // belongs to the horizon, and asking every other line to be a quarter as
    // strong as the longest one in the picture excludes the whole converging
    // family. Selectivity comes from having to beat the neighbours, which a
    // texture cannot do.
    let best = accumulator.iter().cloned().fold(0.0f32, f32::max);
    if best <= 0.0 {
        return None;
    }
    let floor = best / 12.0;
    let mut lines = Vec::new();
    for tb in 0..THETA_BINS {
        for rb in 1..rho_bins - 1 {
            let value = accumulator[tb * rho_bins + rb];
            if value < floor {
                continue;
            }
            let mut peak = true;
            for dt in -2i32..=2 {
                for dr in -3i32..=3 {
                    let t = (tb as i32 + dt).rem_euclid(THETA_BINS as i32) as usize;
                    let r = rb as i32 + dr;
                    if r < 0 || r as usize >= rho_bins || (dt == 0 && dr == 0) {
                        continue;
                    }
                    if accumulator[t * rho_bins + r as usize] > value {
                        peak = false;
                    }
                }
            }
            if peak {
                lines.push(Line {
                    theta: (tb as f32 + 0.5) / THETA_BINS as f32 * std::f32::consts::PI,
                    rho: (rb as f32 + 0.5) * RHO_STEP - diagonal,
                    strength: value,
                });
            }
        }
    }
    Some(lines)
}

/// The rotation that puts the most evidence on an axis.
///
/// Every line is asked how far it is from the nearer of horizontal and
/// vertical, which folds the question into a quarter turn and lets a wall and
/// a windowsill vote for the same rotation. The answer is a *weighted median*
/// and not a mean: one very long diagonal — a roofline, a handrail — would drag
/// a mean several degrees, and the whole difficulty of this feature is not
/// being dragged by the one strong line that was never level.
fn level_from(lines: &[Line]) -> Option<f32> {
    let folded: Vec<(f32, f32)> = lines
        .iter()
        .map(|line| {
            let quarter = std::f32::consts::FRAC_PI_2;
            let mut d = line.theta.rem_euclid(quarter);
            if d > quarter / 2.0 {
                d -= quarter;
            }
            (d.to_degrees(), line.strength)
        })
        .filter(|(d, _)| d.abs() <= AXIS_REACH_DEG)
        .collect();
    if folded.len() < MIN_LINES {
        return None;
    }
    // **The dominant orientation, found as a mode and not as a median.**
    //
    // A median is the right summary of one cluster and the wrong summary of
    // two. Real photographs routinely have two: the sunset frame here has 485
    // lines, a strong family at 0 to 1 degrees — the bridge's railings and lamp
    // posts — and a second family ten degrees off it. The median falls in the
    // gap between them, matches neither, and the agreement test then rejects a
    // picture whose dominant orientation was never in doubt.
    //
    // A histogram finds the family instead of averaging across the families.
    const BIN_DEG: f32 = 0.25;
    let bins = (2.0 * AXIS_REACH_DEG / BIN_DEG).ceil() as usize + 1;
    let mut histogram = vec![0.0f32; bins];
    for (d, w) in &folded {
        let b = ((d + AXIS_REACH_DEG) / BIN_DEG) as usize;
        if b < bins {
            histogram[b] += w;
        }
    }
    // Summed over a window rather than read off one bin, because a family's
    // votes land in several neighbouring bins and the tallest single bin is a
    // coin toss between them.
    let window = (1.0 / BIN_DEG) as usize;
    let mut best = (0usize, bins, 0.0f32);
    for centre in 0..bins {
        let lo = centre.saturating_sub(window);
        let hi = (centre + window + 1).min(bins);
        let weight: f32 = histogram[lo..hi].iter().sum();
        if weight > best.2 {
            best = (lo, hi, weight);
        }
    }
    let total: f32 = folded.iter().map(|(_, w)| w).sum();
    if total <= 0.0 {
        return None;
    }

    // **Refined over the window that was scored, not over one around its
    // centre.** Every centre whose window covers the same cluster scores the
    // same, so the first of them wins — and its centre sits a whole window's
    // width to the low side of the data. Re-selecting around that centre found
    // nothing at all, which read as "no dominant orientation" on pictures that
    // plainly had one.
    let inside = |d: f32| {
        let b = ((d + AXIS_REACH_DEG) / BIN_DEG) as usize;
        b >= best.0 && b < best.1
    };
    let near: Vec<&(f32, f32)> = folded.iter().filter(|(d, _)| inside(*d)).collect();
    let near_weight: f32 = near.iter().map(|(_, w)| w).sum();
    if near_weight <= 0.0 {
        return None;
    }
    let median = near.iter().map(|(d, w)| d * w).sum::<f32>() / near_weight;

    // How much of the evidence actually agrees. A picture whose lines point
    // every way has a mode like any other, and it means nothing; requiring that
    // a real share of them sit near it is what stops a tree being straightened.
    //
    // **A seventh, and the number comes from photographs rather than from
    // synthetic bars.** Orientations are folded into a sixty-degree span and the
    // window is two degrees wide, so lines pointing every way at random would
    // put about three percent inside it — this is five times chance.
    //
    // A quarter was tried first, and is what a picture of nothing but one
    // building gives. Real frames do not: across the ten reference photographs
    // the peak's share runs from 6.5% to 31.9%, because the rest of a
    // photograph is foliage, cloud and water and none of it votes. At a quarter
    // the only frame in that set with a real tilt in it — 0.74 degrees, on a
    // 20.1% peak — was refused.
    if near_weight < total / 7.0 {
        return None;
    }
    if median.abs() > MAX_ANGLE_DEG {
        return None;
    }
    // **A degree, and it is set by how accurate this is rather than by what is
    // visible.** A quarter of a degree is the smaller number and was the first
    // one here, on the reasoning that nobody can see less than that.
    //
    // Then it was checked against a photograph. On the one reference frame with
    // any tilt at all it read +0.74 degrees and proposed to take that out — and
    // the sea horizon in that frame, fitted over ninety-seven columns, is at
    // +0.21. Applying the correction would have moved a level horizon to -0.53:
    // the strongest lines in the picture are the rock strata along the shore,
    // not the sea, and they are not level and never were.
    //
    // So the floor is not "what can be seen", it is "what this can measure".
    // Half a degree of error on a frame needing a fifth of a degree makes the
    // picture worse, and a deadband narrower than the error guarantees it. A
    // degree leaves a genuine small tilt uncorrected, which is the right way to
    // be wrong: a photograph that was nearly straight stays nearly straight.
    Some(if median.abs() < 1.0 {
        0.0
    } else {
        // The rotation that *undoes* the tilt.
        -(median * 10.0).round() / 10.0
    })
}

/// Where the verticals meet, turned into a keystone.
///
/// A vanishing point is the least-squares intersection of the lines that head
/// for it. Each line contributes `x·cos θ + y·sin θ = ρ`, so the point nearest
/// all of them is an ordinary two-by-two normal equation — weighted by
/// strength, so a long wall counts for more than a window frame.
///
/// Returns 0.0 rather than `None` when there is nothing to say: a frame can
/// have a perfectly good horizon and no verticals at all, and that is a
/// successful detection with one number in it.
fn keystone_from(lines: &[Line], width: usize, height: usize, rotation: f32) -> f32 {
    let reach = AXIS_REACH_DEG.to_radians();
    let mut a = [[0.0f64; 2]; 2];
    let mut b = [0.0f64; 2];
    let mut count = 0usize;
    // The orientations themselves, unwrapped about vertical, because whether
    // they *fan out* is the question the intersection cannot answer safely.
    let mut fan: Vec<(f32, f32)> = Vec::new();
    for line in lines {
        // De-rotated, so a tilted camera's lean is not read as convergence.
        let theta = line.theta + rotation;
        // A *vertical* line has a horizontal normal: theta near 0 or near pi.
        let from_axis = theta.rem_euclid(std::f32::consts::PI);
        let vertical = from_axis < reach || from_axis > std::f32::consts::PI - reach;
        if !vertical {
            continue;
        }
        let (c, s) = (theta.cos() as f64, theta.sin() as f64);
        let w = line.strength as f64;
        a[0][0] += w * c * c;
        a[0][1] += w * c * s;
        a[1][0] += w * c * s;
        a[1][1] += w * s * s;
        b[0] += w * c * line.rho as f64;
        b[1] += w * s * line.rho as f64;
        count += 1;
        // Unwrapped: a vertical line's normal sits near 0 or near pi, and
        // averaging those two raw gives pi/2, which is horizontal.
        let unwrapped = if from_axis > std::f32::consts::FRAC_PI_2 {
            from_axis - std::f32::consts::PI
        } else {
            from_axis
        };
        fan.push((unwrapped, line.strength));
    }
    if count < 3 {
        return 0.0;
    }

    // **Parallel lines have no vanishing point, and the solve does not know
    // that.** Their normal equations are near-singular, so the intersection it
    // produces is wherever the rounding happened to put it — which on a frame
    // of perfectly parallel bars came out as a keystone of -0.29, a confident
    // correction of a fault that was not there. A determinant test does not
    // catch it either, because the weights are large and the determinant is
    // large with them.
    //
    // Asking whether the lines *fan out* does catch it, and it is the same
    // question in physical terms: convergence is a spread of orientations, and
    // no spread is no convergence.
    let total: f32 = fan.iter().map(|(_, w)| w).sum();
    if total <= 0.0 {
        return 0.0;
    }
    let mean = fan.iter().map(|(a, w)| a * w).sum::<f32>() / total;
    let spread = (fan
        .iter()
        .map(|(a, w)| w * (a - mean) * (a - mean))
        .sum::<f32>()
        / total)
        .sqrt()
        .to_degrees();
    // Half a degree. Below it the lines are parallel to within what the
    // accumulator can resolve, so there is nothing to measure and the honest
    // answer is that the picture is already upright.
    if spread < 0.5 {
        return 0.0;
    }
    let determinant = a[0][0] * a[1][1] - a[0][1] * a[1][0];
    // Near-singular means the lines are parallel — which is the *correct*
    // picture, already upright, with its vanishing point at infinity. Zero is
    // the right answer and the solve would produce an enormous one.
    if determinant.abs() < 1e-6 {
        return 0.0;
    }
    let vy = ((a[0][0] * b[1] - a[1][0] * b[0]) / determinant) as f32;

    // Normalised to half the frame's height, which is the unit the keystone is
    // expressed in. The vanishing point of an upright photograph is far outside
    // the frame, so a value inside it is not a vanishing point — it is three
    // lines that happened to cross, and acting on it would fold the picture.
    let v = vy / (height as f32 / 2.0);
    let _ = width;
    if !v.is_finite() || v.abs() < 1.5 {
        return 0.0;
    }
    let proposed = -CORRECTION / v;
    if proposed.abs() < 0.02 {
        return 0.0;
    }
    proposed.clamp(-MAX_VERTICAL, MAX_VERTICAL)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame of horizontal and vertical bars, rotated by `tilt` degrees and
    /// converging on a vanishing point `vp` half-heights below centre.
    ///
    /// Drawn by asking, for each pixel, where it came from — the same direction
    /// the renderer samples in — so the synthetic picture is warped by exactly
    /// the map the correction has to invert.
    fn ruled(width: usize, height: usize, tilt: f32, vp: Option<f32>) -> Vec<f32> {
        let (cx, cy) = (width as f32 / 2.0, height as f32 / 2.0);
        let (sin, cos) = (-tilt).to_radians().sin_cos();
        let mut out = vec![0.0f32; width * height];
        for y in 0..height {
            for x in 0..width {
                let (dx, dy) = (x as f32 - cx, y as f32 - cy);
                let (mut u, v) = (dx * cos - dy * sin, dx * sin + dy * cos);
                if let Some(vp) = vp {
                    // Drawn as rays from the vanishing point rather than as a
                    // warp, because a warp has two directions and it is easy to
                    // write the wrong one — which is what happened first, and
                    // produced a picture whose verticals converged the way the
                    // detector was about to correct them.
                    //
                    // A ray from `(0, yv)` through this pixel crosses the centre
                    // row at `dx · (-yv) / (dy - yv)`; bars at constant value of
                    // that expression *are* lines through the vanishing point,
                    // with no ambiguity about which way they lean.
                    let yv = vp * cy;
                    if (v - yv).abs() < 1e-3 {
                        continue;
                    }
                    u = u * (-yv) / (v - yv);
                }
                // Bars every 24 units on both axes: enough lines to vote with,
                // spaced enough that the accumulator does not merge them.
                //
                // **Anti-aliased, and that is not cosmetic.** Drawn with a hard
                // per-pixel test the bars come out as staircases, and a
                // staircase's gradients are axis-aligned however the line
                // actually runs — so every tilt in this file measured 90.12
                // degrees and the detector looked broken when the fixture was.
                // A ramp of about three pixels: at one pixel the edge is still near
                // the sampling limit and the orientation quantises, which showed
                // up as errors of a degree either way. Real edges in a reduced
                // frame are this wide anyway.
                // along the true normal.
                let bar = |c: f32| {
                    let from_centre = (c.rem_euclid(24.0) - 12.0).abs();
                    (1.0 - (from_centre - 1.5) / 3.0).clamp(0.0, 1.0)
                };
                out[y * width + x] = 0.2 + 0.8 * bar(u).max(bar(v));
            }
        }
        out
    }

    #[test]
    fn a_level_frame_is_left_alone() {
        // The refusal that matters most: a picture already straight must come
        // back untouched, or pressing the button becomes a thing you have to
        // check afterwards.
        let image = ruled(320, 240, 0.0, None);
        let found = detect(&image, 320, 240);
        assert!(
            found.is_none() || found.unwrap().angle_deg.abs() < 0.2,
            "a level frame was corrected by {found:?}"
        );
    }

    #[test]
    fn a_tilted_frame_is_levelled() {
        for tilt in [-4.0f32, -1.5, 1.5, 4.0] {
            let image = ruled(320, 240, tilt, None);
            let found = detect(&image, 320, 240).unwrap_or_else(|| {
                panic!("{tilt}° of tilt was not detected at all");
            });
            assert!(
                (found.angle_deg + tilt).abs() < 0.6,
                "{tilt}° of tilt drew a correction of {}°, which does not undo it",
                found.angle_deg
            );
        }
    }

    #[test]
    fn converging_verticals_produce_a_keystone_of_the_right_sign() {
        // A camera pointed up sends the verticals to a vanishing point *above*
        // the frame, and the correction that brings the top back is positive.
        // The sign is the whole of this test: a keystone of the right size and
        // the wrong sign doubles the fault instead of removing it, and on a
        // photograph that reads as a much worse lens rather than as a bug.
        let up = ruled(320, 240, 0.0, Some(-4.0));
        let found = detect(&up, 320, 240).expect("convergence not detected");
        assert!(
            found.vertical > 0.05,
            "verticals converging above the frame gave {}, which does not bring \
             the top back",
            found.vertical
        );

        let down = ruled(320, 240, 0.0, Some(4.0));
        let found = detect(&down, 320, 240).expect("convergence not detected");
        assert!(
            found.vertical < -0.05,
            "verticals converging below the frame gave {}",
            found.vertical
        );
    }

    #[test]
    fn a_picture_with_no_structure_is_refused() {
        // Noise, and a smooth gradient: neither has a line in it that was ever
        // level, and both must come back as `None` rather than as a small
        // confident rotation.
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let noise: Vec<f32> = (0..320 * 240)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed >> 40) as f32 / 16777216.0
            })
            .collect();
        assert_eq!(detect(&noise, 320, 240), None, "noise was straightened");

        let ramp: Vec<f32> = (0..320 * 240).map(|i| (i % 320) as f32 / 320.0).collect();
        assert_eq!(detect(&ramp, 320, 240), None, "a gradient was straightened");
    }

    #[test]
    fn a_frame_too_small_to_read_is_refused() {
        assert_eq!(detect(&[0.0; 4], 2, 2), None);
        assert_eq!(detect(&[], 0, 0), None);
    }

    #[test]
    fn the_correction_is_bounded() {
        // Whatever it is shown, it must not propose a warp that throws most of
        // the frame away. A picture whose lines genuinely converge inside the
        // frame is not a photograph of a building.
        for vp in [-1.6f32, 1.6, -20.0, 20.0] {
            if let Some(found) = detect(&ruled(320, 240, 0.0, Some(vp)), 320, 240) {
                assert!(
                    found.vertical.abs() <= MAX_VERTICAL,
                    "vanishing point at {vp} proposed {}",
                    found.vertical
                );
                assert!(found.angle_deg.abs() <= MAX_ANGLE_DEG);
            }
        }
    }
}
