//! The five tone controls, as one curve.
//!
//! `EditState::Tone` carries contrast, highlights, shadows, whites and blacks
//! alongside exposure. Exposure is a scene-linear multiply and lives with the
//! white balance; these five are [`Stage::DisplayReferredOps`] and run *after*
//! the tone map, which is what the declared pipeline order says and not a
//! convenience — the sigmoid is the boundary, and a control that shapes what
//! the eye will see belongs on the far side of it.
//!
//! [`Stage::DisplayReferredOps`]: crate::pipeline::Stage::DisplayReferredOps
//!
//! # Why there is a perceptual coordinate
//!
//! The tone map's output is display-referred **linear**, so mid-grey sits at
//! 0.18 — a fifth of the way up the numeric range, not the middle of it. A
//! contrast curve pivoting on 0.5 there would pivot two thirds of a stop above
//! mid-grey, and "shadows" and "highlights" would name the wrong parts of the
//! picture.
//!
//! So the curve runs in `p = y^(1/2.2)`, where mid-grey lands at 0.459 and the
//! regions mean what their names say. This is a *coordinate*, not an encoding:
//! the value comes back to linear before it leaves, because the transfer
//! function belongs to the output transform and baking one in here would give
//! every consumer a second one to undo.
//!
//! # Why the shape of each control is what it is
//!
//! - **Contrast** is a power about the pivot, applied to each side separately.
//!   Both segments carry slope `k` at the pivot, so the curve is smooth there
//!   rather than merely continuous, and 0, mid-grey and 1 are all fixed —
//!   contrast changes contrast and does not secretly change brightness.
//! - **Highlights and shadows** are powers whose exponent *tapers to exactly 1
//!   at the pivot*. The obvious version — a plain power on the upper segment —
//!   leaves a slope discontinuity in the middle of the frame, which shows up in
//!   a smooth sky as a band. The taper costs one multiply and removes it.
//!
//!   They are also the two controls that are **spatially adaptive**: the
//!   exponent is chosen from the pixel's neighbourhood rather than from the
//!   pixel, and applied as a gain. The parameters below are the same either
//!   way — what changes is the value they are keyed on, which is resolved in
//!   the shader from [`crate::guide`]. Nothing in this module knows about it,
//!   deliberately: the curve's shape and where it is sampled are separate
//!   questions, and only the second one needs a picture.
//! - **Whites and blacks** are a black point and a white point, and they
//!   **clip**. That is deliberate: nothing else in the pipeline clips, and the
//!   tone map is asymptotic precisely so that it does not — but an editor whose
//!   black slider only ever compresses reads as broken, and the endpoints are
//!   where a photographer *asks* for clipping. It happens because the user
//!   moved a control, never behind them.
//!
//! # Monotonicity is a bound, not a hope
//!
//! Every step is monotonic by construction, and for the tapered powers that is
//! a real constraint rather than an observation. Writing the exponent as
//! `e(u) = 1 + c(1 - u)`, the derivative of `u^e(u)` stays positive exactly when
//! `1 + c·g(u) > 0` for `g(u) = 1 - u - u·ln u`, whose maximum on `(0, 1]` is
//! `1.1354` at `u = e^-2`. So `|c| < 1/1.1354 = 0.8807`, and [`TAPER`] is 0.75
//! to leave margin. Past that bound the curve folds back on itself and local
//! contrast inverts — which looks like a contour, not like a bug.

use rawkit_editstate::Tone;

// The curve's own constants live in the shader, because that is where the
// arithmetic happens. These are the specification: the mirror below is written
// against them, and `the_shader_uses_the_constants_documented_here` checks the
// WGSL still agrees. Two copies of a number is a smell; two copies where one
// checks the other every build is a guard.

/// Mid-grey in the perceptual coordinate: `0.18^(1/2.2)`.
///
/// The same 0.18 the tone map fixes. If that constant ever moves, this one
/// moves with it or the controls stop pivoting on middle grey.
///
/// Not test-only, unlike the gamma below it. The curve itself lives in the
/// shader and this side only passed sliders through — until the local operator
/// needed *bounds* on the neighbourhood it reads a gain from, which are a
/// property of the curve's shape and so have to be worked out where the shape
/// is known. `the_constants_match_the_shader` is what keeps the two copies
/// honest, and it is why duplicating them is affordable at all.
const PIVOT: f32 = 0.45865646;

/// The exponent the perceptual coordinate uses.
#[cfg(test)]
const GAMMA: f32 = 2.2;

/// How far the shadow and highlight exponents may travel from 1.
///
/// Bounded by monotonicity at 0.8807; see the module docs for the derivation.
/// Not test-only, for the reason given on [`PIVOT`].
const TAPER: f32 = 0.75;

/// How far the black and white points may travel from their defaults.
///
/// A quarter of the perceptual range each. At the extremes that leaves the two
/// points 0.5 apart, so they can never cross and the levels step can never
/// invert — the same "monotonic by construction" argument as the taper, and the
/// reason this is a constant rather than an unbounded slider.
pub(crate) const LEVELS_REACH: f32 = 0.25;

/// The five controls, reduced to what the shader needs.
///
/// The slider-to-curve mapping happens once per frame here rather than once per
/// pixel there. It is also the only place that knows a slider runs `-1..1`, so
/// a stored edit carrying something outside that range is clamped at this
/// boundary instead of reaching arithmetic that assumes it cannot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ToneCurve {
    /// Contrast as the power about the pivot: `2^contrast`, so `1` is identity
    /// and the slider is symmetric in log.
    pub contrast_exponent: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub black_point: f32,
    pub white_point: f32,
    /// The brightest and darkest a *neighbourhood* may be taken to be, when the
    /// local operator reads a gain off the curve.
    ///
    /// # Why a gain read off this curve needs bounding at all
    ///
    /// The local path multiplies a pixel by `curve(reference) / reference` —
    /// how much the curve moves a neighbourhood of that brightness. It is a
    /// gain rather than a remap so that two neighbours keep their ratio, which
    /// is the whole reason the control does not read as flat.
    ///
    /// But the curve pins both its endpoints: `curve(0) = 0` and `curve(1) = 1`.
    /// So that ratio is **not monotone**. Towards white it turns around and
    /// climbs back to exactly 1, and the highlights slider stops doing anything
    /// precisely where the neighbourhood is brightest — measured on a real
    /// frame, a gain of 0.786 at a reference of 0.95 and 1.000 at 1.0, which
    /// rendered as a bright blob of untouched sky sitting in a sky that had
    /// been pulled down. Towards black it does not turn around at all, it
    /// diverges: 16x at a reference of 0.01 and 556x at 0.0001, so a glint
    /// inside a shadow is multiplied by that and clips to white.
    ///
    /// Both ends are the same defect and take the same repair: stop reading the
    /// gain past the point where the curve stops becoming more effective, and
    /// hold the strongest gain it reached. Clamping the *reference* rather than
    /// the gain is what makes that one line in the shader instead of two cases.
    pub highlight_reference: f32,
    pub shadow_reference: f32,
    /// Whether any control is off its default.
    ///
    /// Carried explicitly so the shader can return the tone-mapped value
    /// untouched. Not an optimisation: it makes an identity edit **bit**-
    /// identical to a build without any of this, which is what lets the
    /// existing golden references stand unchanged and proves the addition is
    /// additive.
    pub active: bool,
}

impl ToneCurve {
    pub fn new(tone: &Tone) -> Self {
        let clamp = |v: f32| {
            if v.is_finite() {
                v.clamp(-1.0, 1.0)
            } else {
                0.0
            }
        };
        let (contrast, highlights, shadows, whites, blacks) = (
            clamp(tone.contrast),
            clamp(tone.highlights),
            clamp(tone.shadows),
            clamp(tone.whites),
            clamp(tone.blacks),
        );
        Self {
            contrast_exponent: contrast.exp2(),
            highlights,
            shadows,
            // Negative crushes, which is the direction every editor's black
            // slider moves: pulling it left raises the black point so the
            // darkest values meet it and clip.
            black_point: -blacks * LEVELS_REACH,
            white_point: 1.0 - whites * LEVELS_REACH,
            highlight_reference: highlight_reference(highlights),
            shadow_reference: shadow_reference(shadows),
            active: [contrast, highlights, shadows, whites, blacks]
                .iter()
                .any(|v| *v != 0.0),
        }
    }

    /// `[contrast exponent, highlights, shadows, active]`.
    pub fn shape(&self) -> [f32; 4] {
        [
            self.contrast_exponent,
            self.highlights,
            self.shadows,
            if self.active { 1.0 } else { 0.0 },
        ]
    }

    /// `[black point, white point, unused, unused]`.
    pub fn levels(&self) -> [f32; 4] {
        [self.black_point, self.white_point, 0.0, 0.0]
    }

    /// `[highlight reference, shadow reference, unused, unused]`.
    ///
    /// Beside `levels` rather than in it: the endpoints are where a photograph
    /// clips, and these are how far the *local* operator will trust its own
    /// neighbourhood. Two different ideas that happen to be two numbers each.
    pub fn local(&self) -> [f32; 4] {
        [self.highlight_reference, self.shadow_reference, 0.0, 0.0]
    }
}

/// The highlight branch's gain, `curve(r) / r`, over the range it applies to.
fn highlight_gain(r: f32, highlights: f32) -> f32 {
    let u = (1.0 - r) / (1.0 - PIVOT);
    (1.0 - (1.0 - PIVOT) * u.powf(1.0 + highlights * TAPER * (1.0 - u))) / r
}

/// The same, below the pivot.
fn shadow_gain(r: f32, shadows: f32) -> f32 {
    let v = r / PIVOT;
    PIVOT * v.powf(1.0 - shadows * TAPER * (1.0 - v)) / r
}

/// The brightest a neighbourhood is worth reading a gain at.
///
/// Found by scanning rather than solved, and that is a deliberate trade: the
/// turning point moves with the slider — measured between 0.80 and 0.95 across
/// the range — and a closed form for it would be a page of algebra that has to
/// be re-derived the first time anybody reshapes the curve. This runs once per
/// edit over a few hundred steps of arithmetic, which is nothing beside the
/// render it precedes, and it stays correct if the curve changes shape.
fn highlight_reference(highlights: f32) -> f32 {
    if highlights == 0.0 {
        // The gain is 1 everywhere, so there is nothing to hold and no reason
        // to move the reference at all. Exactness matters here: it is what
        // keeps an edit that never touched this control bit-identical.
        return 1.0;
    }
    let mut best = (PIVOT, 1.0f32);
    for i in 0..=STEPS {
        let r = PIVOT + (1.0 - PIVOT) * i as f32 / STEPS as f32;
        let g = highlight_gain(r.min(1.0 - f32::EPSILON), highlights);
        // Furthest from unity in the direction the slider asked for.
        if (highlights < 0.0 && g < best.1) || (highlights > 0.0 && g > best.1) {
            best = (r, g);
        }
    }
    best.0
}

/// The darkest a neighbourhood is worth reading a gain at.
///
/// Unlike the highlight side this one never turns around — the gain diverges as
/// the neighbourhood approaches black — so the bound is a stated maximum lift
/// rather than a discovered extremum.
fn shadow_reference(shadows: f32) -> f32 {
    if shadows == 0.0 {
        return 0.0;
    }
    // Scanned upwards from black, where the gain is at its most extreme, and
    // stopped at the first reference that is *inside* the bound — not the last
    // one outside it, which is a step too far and leaves the bound exceeded by
    // exactly one step's worth.
    for i in 0..=STEPS {
        let r = PIVOT * i as f32 / STEPS as f32;
        if r <= 0.0 {
            continue;
        }
        let g = shadow_gain(r, shadows);
        let inside = (1.0 / MAX_LOCAL_GAIN..=MAX_LOCAL_GAIN).contains(&g);
        if inside {
            return r;
        }
    }
    PIVOT
}

/// How far the local operator may take a pixel, as a multiple.
///
/// Three stops. The gain only exceeds it below a reference of about 0.025,
/// which is 0.0003 of full scale once the output gamma is applied — black, in
/// any photograph anybody is looking at. What lives above that bound is not a
/// shadow being lifted but a bright speck being multiplied by the darkness
/// around it, and the visible result of leaving it unbounded is a white dot.
const MAX_LOCAL_GAIN: f32 = 8.0;

/// How finely the two references above are scanned.
///
/// The gain is smooth and its extremum is broad, so this decides a fraction of
/// a percent of the reference and nothing a person could see; it is here to be
/// a number rather than a magic literal in two places.
const STEPS: usize = 512;

/// The user's hand-shaped curve, resampled to a lookup the shader can index.
///
/// `None` when the curve is the identity, so an untouched photograph pays
/// nothing and the buffer holding it stays one cell.
///
/// # Monotone cubic, not plain cubic
///
/// Fritsch–Carlson. A natural spline through the same points overshoots between
/// them — put a point low and the next high and the curve dips *below* the first
/// on its way up. In a tone curve that is not a cosmetic wobble: a segment that
/// runs backwards inverts local contrast, and the result reads as a contour in a
/// smooth sky rather than as a bug in an interpolator. The limiter is what makes
/// "the curve you drew is the curve you get" true rather than nearly true.
///
/// Linear interpolation would also be monotone and is what the profile curve
/// uses — but that one arrives with 128 points and this one with as few as two,
/// where straight segments meeting at a corner are plainly visible.
pub fn user_curve_lut(curve: &rawkit_editstate::Curve) -> Option<Vec<f32>> {
    if curve.is_identity() || curve.validate().is_err() {
        return None;
    }
    let points = &curve.points;
    let n = points.len();

    // Secants, and the tangents that start as their averages.
    let secant: Vec<f32> = (0..n - 1)
        .map(|i| {
            let run = points[i + 1][0] - points[i][0];
            if run <= 0.0 {
                0.0
            } else {
                (points[i + 1][1] - points[i][1]) / run
            }
        })
        .collect();
    let mut tangent = vec![0.0f32; n];
    tangent[0] = secant[0];
    tangent[n - 1] = secant[n - 2];
    for i in 1..n - 1 {
        tangent[i] = (secant[i - 1] + secant[i]) / 2.0;
    }

    // The limiter. A flat segment pins both its tangents to zero, and anywhere
    // the tangents are too steep for the secant they are scaled back onto the
    // circle of radius three — which is the condition for the Hermite segment
    // to stay monotone.
    for i in 0..n - 1 {
        if secant[i] == 0.0 {
            tangent[i] = 0.0;
            tangent[i + 1] = 0.0;
            continue;
        }
        let alpha = tangent[i] / secant[i];
        let beta = tangent[i + 1] / secant[i];
        let size = alpha * alpha + beta * beta;
        if size > 9.0 {
            let scale = 3.0 / size.sqrt();
            tangent[i] = scale * alpha * secant[i];
            tangent[i + 1] = scale * beta * secant[i];
        }
    }

    let at = |x: f32| -> f32 {
        if x <= points[0][0] {
            return points[0][1];
        }
        if x >= points[n - 1][0] {
            return points[n - 1][1];
        }
        let i = points
            .windows(2)
            .position(|w| x >= w[0][0] && x <= w[1][0])
            .unwrap_or(0);
        let (x0, y0) = (points[i][0], points[i][1]);
        let (x1, y1) = (points[i + 1][0], points[i + 1][1]);
        let h = x1 - x0;
        if h <= 0.0 {
            return y1;
        }
        let t = (x - x0) / h;
        let t2 = t * t;
        let t3 = t2 * t;
        // Hermite basis.
        (2.0 * t3 - 3.0 * t2 + 1.0) * y0
            + (t3 - 2.0 * t2 + t) * h * tangent[i]
            + (-2.0 * t3 + 3.0 * t2) * y1
            + (t3 - t2) * h * tangent[i + 1]
    };

    Some(
        (0..crate::profile::TONE_LUT)
            .map(|i| {
                let x = i as f32 / (crate::profile::TONE_LUT - 1) as f32;
                at(x).clamp(0.0, 1.0)
            })
            .collect(),
    )
}

/// The curve the shader applies, in Rust.
///
/// **This is a mirror, and mirrors drift**, so it earns its place only by what
/// it is used for: the properties below — monotonic, fixed points, clipping —
/// are true of the *maths*, and checking them here costs no GPU and runs on
/// every platform. That the shader implements the same maths is a separate
/// claim, and the golden renders are what carry it. Neither test substitutes
/// for the other.
#[cfg(test)]
fn curve(y: f32, c: &ToneCurve) -> f32 {
    if !c.active {
        return y;
    }
    let p = y.max(0.0).powf(1.0 / GAMMA).min(1.0);

    // Contrast: a power about the pivot, each side separately.
    let p = if p <= PIVOT {
        PIVOT * (p / PIVOT).powf(c.contrast_exponent)
    } else {
        1.0 - (1.0 - PIVOT) * ((1.0 - p) / (1.0 - PIVOT)).powf(c.contrast_exponent)
    };

    // Shadows and highlights: exponents that taper to 1 at the pivot.
    let p = if p <= PIVOT {
        let v = p / PIVOT;
        PIVOT * v.powf(1.0 - c.shadows * TAPER * (1.0 - v))
    } else {
        let u = (1.0 - p) / (1.0 - PIVOT);
        1.0 - (1.0 - PIVOT) * u.powf(1.0 + c.highlights * TAPER * (1.0 - u))
    };

    // The endpoints, and the only place anything clips.
    let p = ((p - c.black_point) / (c.white_point - c.black_point)).clamp(0.0, 1.0);
    p.powf(GAMMA)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_straight_curve_through_extra_points_is_still_straight() {
        // The identity is skipped by `is_identity`, so this is the identity
        // written the long way — three points on the diagonal. Anything the
        // interpolator adds here it would add to every curve.
        let curve = rawkit_editstate::Curve {
            points: vec![[0.0, 0.0], [0.5, 0.5], [1.0, 1.0]],
        };
        let lut = super::user_curve_lut(&curve).expect("not the identity by value");
        for (i, value) in lut.iter().enumerate() {
            let x = i as f32 / (lut.len() - 1) as f32;
            assert!((value - x).abs() < 1e-4, "at {x}: {value}");
        }
    }

    #[test]
    fn a_lifted_middle_lifts_the_middle_and_pins_the_ends() {
        let curve = rawkit_editstate::Curve {
            points: vec![[0.0, 0.0], [0.5, 0.7], [1.0, 1.0]],
        };
        let lut = super::user_curve_lut(&curve).expect("a curve");
        let at = |x: f32| lut[(x * (lut.len() - 1) as f32).round() as usize];
        assert!(
            (at(0.0) - 0.0).abs() < 1e-4,
            "the black end moved: {}",
            at(0.0)
        );
        assert!(
            (at(1.0) - 1.0).abs() < 1e-4,
            "the white end moved: {}",
            at(1.0)
        );
        // Three thousandths, because 0.5 falls between two entries of a
        // 256-step lookup and the nearest one is a fifth of a percent along the
        // curve from it. That is the grid, not the interpolator.
        assert!((at(0.5) - 0.7).abs() < 3e-3, "the point moved: {}", at(0.5));
    }

    #[test]
    fn a_curve_cannot_fold_back_however_it_is_drawn() {
        // The property the monotone limiter exists for. A plain cubic through
        // these points dips below its own starting value on the way up, and a
        // tone curve that runs backwards inverts local contrast — which looks
        // like a contour in a smooth sky rather than like a bug here.
        for points in [
            vec![[0.0, 0.0], [0.45, 0.05], [0.55, 0.95], [1.0, 1.0]],
            vec![[0.0, 0.2], [0.2, 0.21], [0.8, 0.99], [1.0, 1.0]],
            vec![[0.0, 0.0], [0.1, 0.9], [0.9, 0.95], [1.0, 1.0]],
        ] {
            let lut = super::user_curve_lut(&rawkit_editstate::Curve {
                points: points.clone(),
            })
            .expect("a curve");
            let mut previous = f32::NEG_INFINITY;
            for (i, value) in lut.iter().enumerate() {
                assert!(
                    *value >= previous - 1e-6,
                    "{points:?} folds back at entry {i}: {value} after {previous}"
                );
                previous = *value;
            }
        }
    }

    #[test]
    fn the_identity_curve_costs_nothing() {
        assert!(super::user_curve_lut(&rawkit_editstate::Curve::default()).is_none());
    }

    use super::*;

    /// Every combination of the five controls at their extremes, plus the
    /// midpoints. 3^5 = 243 curves, which is cheap and exhaustive enough that
    /// no combination has to be argued about.
    fn every_extreme() -> Vec<ToneCurve> {
        let levels = [-1.0f32, 0.0, 1.0];
        let mut out = Vec::new();
        for &contrast in &levels {
            for &highlights in &levels {
                for &shadows in &levels {
                    for &whites in &levels {
                        for &blacks in &levels {
                            out.push(ToneCurve::new(&Tone {
                                exposure_ev: 0.0,
                                contrast,
                                highlights,
                                shadows,
                                whites,
                                blacks,
                                ..Tone::default()
                            }));
                        }
                    }
                }
            }
        }
        out
    }

    #[test]
    fn the_shader_uses_the_constants_documented_here() {
        // The mirror above is only worth having if it is a mirror. These three
        // numbers exist twice — once as the specification, once as WGSL — and
        // this is what stops the copies drifting apart in silence, which would
        // leave every property below true of a curve nobody renders.
        let wgsl = include_str!("../shaders/demosaic_rcd.wgsl");
        for (name, value) in [
            ("TONE_PIVOT", PIVOT),
            ("TONE_GAMMA", GAMMA),
            ("TONE_TAPER", TAPER),
        ] {
            let line = wgsl
                .lines()
                .find(|l| l.trim_start().starts_with(&format!("const {name}")))
                .unwrap_or_else(|| panic!("the shader has no {name}"));
            let literal = line
                .rsplit('=')
                .next()
                .and_then(|tail| tail.trim().trim_end_matches(';').parse::<f32>().ok())
                .unwrap_or_else(|| panic!("cannot read a number out of `{line}`"));
            assert_eq!(literal, value, "{name} disagrees with the shader");
        }
    }

    /// The gain the local operator actually applies at a neighbourhood of `r`,
    /// bounds included — which is the thing that has to behave, rather than the
    /// raw ratio it is derived from.
    fn effective_gain(c: &ToneCurve, r: f32) -> f32 {
        let r = r.clamp(c.shadow_reference, c.highlight_reference).max(1e-6);
        if r <= PIVOT {
            super::shadow_gain(r, c.shadows)
        } else {
            super::highlight_gain(r.min(1.0 - f32::EPSILON), c.highlights)
        }
    }

    #[test]
    fn the_local_gain_never_gives_up_where_it_is_most_needed() {
        // The defect this exists for: the gain is `curve(r)/r`, and the curve
        // pins both endpoints — so towards white the ratio turns around and
        // climbs back to exactly 1, and the highlights slider stops doing
        // anything precisely where the neighbourhood is brightest. On a real
        // photograph that rendered as a bright patch of untouched sky sitting
        // inside a sky that had been pulled down.
        //
        // Stated as monotonicity rather than as "no blob": once the gain has
        // started moving away from 1 it may not come back towards it, at either
        // end. That is the property the artefact violated, and it is checkable.
        for c in every_extreme() {
            if !c.active {
                continue;
            }
            // Walked from the pivot *upwards*: the brighter the neighbourhood,
            // the further the gain may be from 1 and never nearer. Weakening as
            // a neighbourhood gets dimmer is the operator working; weakening as
            // it gets brighter is the bug.
            let mut worst: Option<(f32, f32, f32)> = None;
            let mut previous = (effective_gain(&c, PIVOT) - 1.0).abs();
            for step in 0..=2000 {
                let r = PIVOT + (1.0 - PIVOT) * step as f32 / 2000.0;
                let strength = (effective_gain(&c, r) - 1.0).abs();
                // A relative slack, because the reference bound is found by a
                // scan and the gain either side of it agrees only to within a
                // step of that scan.
                if strength + 1e-4 < previous && worst.is_none() {
                    worst = Some((r, previous, strength));
                }
                previous = previous.max(strength);
            }
            assert!(
                worst.is_none(),
                "highlights {:+.2}: the gain weakens as the neighbourhood brightens \
                 — at {:?}",
                c.highlights,
                worst
            );
        }
    }

    #[test]
    fn the_local_gain_cannot_run_away_in_the_dark() {
        // The same defect at the other end, and the one that has not been seen
        // yet only because nobody has pulled the shadows up on a frame with a
        // glint in a dark corner. Unbounded, the gain reaches 16x at a
        // neighbourhood of 0.01 and 556x at 0.0001 — and it multiplies the
        // *pixel*, not the neighbourhood, so anything brighter than the darkness
        // around it clips to white.
        for c in every_extreme() {
            if !c.active {
                continue;
            }
            for step in 0..=2000 {
                let r = step as f32 / 2000.0;
                let g = effective_gain(&c, r);
                assert!(
                    g.is_finite()
                        && (1.0 / MAX_LOCAL_GAIN / 1.001..=MAX_LOCAL_GAIN * 1.001).contains(&g),
                    "shadows {:+.2}: a neighbourhood of {r} gives a gain of {g}",
                    c.shadows
                );
            }
        }
    }

    #[test]
    fn a_control_left_alone_moves_no_reference() {
        // The bounds must not become a second way for an untouched slider to
        // change a photograph. Exact values, because the shader clamps by them
        // unconditionally and 0.999999 would be a quiet, permanent nudge.
        let c = ToneCurve::new(&Tone {
            exposure_ev: 0.0,
            contrast: 0.5,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            ..Tone::default()
        });
        assert_eq!(c.local(), [1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn the_curve_never_folds_back_on_itself() {
        // The property the taper bound exists for. A non-monotonic tone curve
        // inverts local contrast, which does not look like a bug — it looks
        // like a contour in a smooth sky, and gets blamed on the demosaic.
        for shape in every_extreme() {
            let mut previous = f32::NEG_INFINITY;
            for step in 0..=2000 {
                let y = step as f32 / 2000.0;
                let out = curve(y, &shape);
                assert!(
                    out >= previous - 1e-6,
                    "{shape:?} folds back at y = {y}: {out} after {previous}"
                );
                previous = out;
            }
        }
    }

    #[test]
    fn the_default_edit_changes_nothing_at_all() {
        // Not "close enough": the identity has to be *exact*, because that is
        // what lets the golden references blessed before this existed stand
        // unchanged, and what makes the whole addition provably additive.
        let shape = ToneCurve::new(&Tone::default());
        assert!(!shape.active);
        for step in 0..=1000 {
            let y = step as f32 / 1000.0;
            assert_eq!(curve(y, &shape), y);
        }
    }

    #[test]
    fn contrast_pivots_on_middle_grey() {
        // The reason the perceptual coordinate exists. Mid-grey has to come
        // through untouched, or "contrast" is a brightness control wearing a
        // different label.
        let grey = 0.18f32;
        for contrast in [-1.0, -0.5, 0.5, 1.0] {
            let shape = ToneCurve::new(&Tone {
                contrast,
                ..Tone::default()
            });
            let out = curve(grey, &shape);
            assert!(
                (out - grey).abs() < 1e-4,
                "contrast {contrast} moved mid-grey to {out}"
            );
        }
        // And it is contrast: darker below, brighter above.
        let up = ToneCurve::new(&Tone {
            contrast: 1.0,
            ..Tone::default()
        });
        assert!(curve(0.05, &up) < 0.05);
        assert!(curve(0.5, &up) > 0.5);
    }

    #[test]
    fn highlights_and_shadows_leave_the_other_half_alone() {
        // The taper's second job. Each control is named for a region, and a
        // "highlights" slider that visibly moves the shadows is one the user
        // cannot reason about.
        let grey = 0.18f32;
        for (name, tone) in [
            (
                "highlights",
                Tone {
                    highlights: -1.0,
                    ..Tone::default()
                },
            ),
            (
                "shadows",
                Tone {
                    shadows: 1.0,
                    ..Tone::default()
                },
            ),
        ] {
            let shape = ToneCurve::new(&tone);
            assert!(
                (curve(grey, &shape) - grey).abs() < 1e-4,
                "{name} moved mid-grey"
            );
        }

        let recover = ToneCurve::new(&Tone {
            highlights: -1.0,
            ..Tone::default()
        });
        // A bright value comes down usefully far — this is highlight recovery,
        // so a token change would be worse than none.
        let bright = 0.78f32;
        assert!(
            curve(bright, &recover) < bright * 0.75,
            "highlight recovery moved {bright} to {}",
            curve(bright, &recover)
        );
        // And a shadow is untouched.
        assert!((curve(0.01, &recover) - 0.01).abs() < 1e-4);

        let lift = ToneCurve::new(&Tone {
            shadows: 1.0,
            ..Tone::default()
        });
        assert!(curve(0.01, &lift) > 0.02);
        assert!((curve(0.8, &lift) - 0.8).abs() < 1e-4);
    }

    #[test]
    fn the_endpoints_clip_and_nothing_else_does() {
        // The decision this slice made explicit: whites and blacks are where
        // clipping is allowed, because the user asked for it there.
        let crush = ToneCurve::new(&Tone {
            blacks: -1.0,
            ..Tone::default()
        });
        assert_eq!(curve(0.0, &crush), 0.0);
        // 0.25 in the perceptual coordinate is 0.25^2.2 in linear.
        assert_eq!(curve(0.25f32.powf(GAMMA) * 0.9, &crush), 0.0);
        assert!(curve(0.5, &crush) > 0.0);

        let blow = ToneCurve::new(&Tone {
            whites: 1.0,
            ..Tone::default()
        });
        assert_eq!(curve(0.75f32.powf(GAMMA) * 1.1, &blow), 1.0);
        assert!(curve(0.2, &blow) < 1.0);

        // Every other control leaves the range open at the top, because the
        // tone map is asymptotic and they must not undo that.
        for tone in [
            Tone {
                contrast: 1.0,
                ..Tone::default()
            },
            Tone {
                highlights: 1.0,
                ..Tone::default()
            },
        ] {
            let shape = ToneCurve::new(&tone);
            assert!(curve(0.999, &shape) < 1.0, "{tone:?} clipped a highlight");
        }
    }

    #[test]
    fn the_points_can_never_cross() {
        // If they did, the levels step would divide by a negative number and
        // the curve would run backwards. The reach constant is what prevents
        // it, so the guarantee is asserted rather than left to arithmetic.
        for shape in every_extreme() {
            assert!(
                shape.white_point - shape.black_point >= 0.5,
                "{shape:?} left the endpoints {} apart",
                shape.white_point - shape.black_point
            );
        }
    }

    #[test]
    fn a_stored_edit_from_outside_the_slider_range_is_clamped_here() {
        // The renderer is the boundary. `EditState` is JSON somebody could have
        // hand-edited, and the taper bound is only safe for `-1..1` — so this
        // is defence at the edge, not a formality.
        let wild = ToneCurve::new(&Tone {
            contrast: 40.0,
            highlights: -12.0,
            shadows: f32::NAN,
            whites: f32::INFINITY,
            blacks: -3.0,
            exposure_ev: 0.0,
            ..Tone::default()
        });
        assert_eq!(wild.contrast_exponent, 2.0);
        assert_eq!(wild.highlights, -1.0);
        assert_eq!(wild.shadows, 0.0, "NaN is not a slider position");
        assert!(wild.active, "the finite controls are still set");
        // Infinity is no more a slider position than NaN is, so it lands on
        // the default rather than on the extreme it superficially resembles.
        assert_eq!(wild.white_point, 1.0);
        assert_eq!(wild.black_point, LEVELS_REACH);

        let mut previous = f32::NEG_INFINITY;
        for step in 0..=1000 {
            let out = curve(step as f32 / 1000.0, &wild);
            assert!(out >= previous - 1e-6);
            previous = out;
        }
    }
}
