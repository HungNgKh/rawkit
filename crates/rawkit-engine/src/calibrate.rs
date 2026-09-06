//! The camera's primaries, moved — and the arithmetic for saying whether that
//! made the colour better.
//!
//! # What a primary slider does here
//!
//! A camera-to-XYZ matrix has one column per primary: the tristimulus a unit of
//! that channel produces. Turning the red primary's hue is therefore not a
//! metaphor — it is rotating that column, and everything the sensor sees follows
//! from it. Nothing is done per pixel; the matrix is rebuilt once and uploaded,
//! and a whole calibration costs three columns of arithmetic.
//!
//! The rotation happens in **CIELAB**, about D50. Not because a camera primary
//! is a colour anyone will look at, but because that is the space the answer is
//! judged in: the acceptance test for this feature is ΔE2000 against a chart,
//! and a hue slider that moved a primary by a degree of something else would be
//! adjusting one thing and being marked on another.
//!
//! # The neutral does not move
//!
//! This is the property that makes it a calibration rather than a cast. Rotating
//! three columns changes their sum, and their sum is where camera neutral lands
//! — so a naive implementation shifts the white balance every time a primary
//! slider is touched, which is felt as the photograph going warm when you were
//! adjusting a red.
//!
//! So after the columns move, the three channel gains are solved exactly for
//! `Σ kᵢ · pᵢ′ = Σ pᵢ`. Those gains *are* a white balance, which is the honest
//! statement of the rule: calibrating the primaries never changes the neutral,
//! because any change to the neutral is white balance and there is a control for
//! that.
//!
//! # Not Adobe's numbers
//!
//! The names, the ranges and the sense are Lightroom Classic's so that a
//! photographer's hands already know what these are. What they compute is the
//! above. Adobe has never published what its sliders do, and a number that
//! claimed to match an unpublished one would be a claim that could not be kept.

use rawkit_editstate::Calibration;

/// A 3×3, row-major, as the rest of the engine spells one.
pub type Matrix3 = [[f32; 3]; 3];

/// CIE XYZ of D50, the white the DNG connection space is referred to.
pub const D50: [f32; 3] = [0.9642, 1.0, 0.8249];

/// The camera-to-XYZ matrix with the calibration applied.
///
/// `camera_to_xyz` maps white-balanced camera values to XYZ D50 — a profile's
/// forward matrix, or what an inverted colour matrix amounts to. The result maps
/// the same values to the same neutral, with the primaries moved.
pub fn calibrated(camera_to_xyz: &Matrix3, calibration: &Calibration) -> Matrix3 {
    if calibration.is_identity() {
        return *camera_to_xyz;
    }
    // Where camera neutral lands today. Everything below exists to put it back.
    let neutral = [
        camera_to_xyz[0].iter().sum::<f32>(),
        camera_to_xyz[1].iter().sum::<f32>(),
        camera_to_xyz[2].iter().sum::<f32>(),
    ];

    // The largest fraction of the asked-for move that still leaves every channel
    // gain positive, found by bisection.
    //
    // Not a guard that gives up: a slider that silently did nothing past some
    // point would be a control the user cannot trust, and where the point is
    // depends on the camera matrix, so nobody could predict it. Backing the move
    // off instead makes the slider saturate — the far end of its travel does
    // less than the middle for a badly behaved sensor, and it always does
    // *something* in the direction asked for.
    //
    // Twelve rounds, so the answer is within a two-thousandth of the range —
    // finer than the slider can be moved, and it runs once per render setup
    // rather than per pixel.
    let mut low = 0.0f32;
    let mut high = 1.0f32;
    let mut best = None;
    for round in 0..12 {
        let t = if round == 0 { 1.0 } else { (low + high) / 2.0 };
        match solve(camera_to_xyz, calibration, neutral, t) {
            Some(matrix) => {
                best = Some(matrix);
                if round == 0 {
                    return matrix;
                }
                low = t;
            }
            None => high = t,
        }
    }
    best.unwrap_or(*camera_to_xyz)
}

/// The calibration at `amount` of its full strength, or `None` when the neutral
/// cannot be restored with positive channel gains.
fn solve(
    camera_to_xyz: &Matrix3,
    calibration: &Calibration,
    neutral: [f32; 3],
    amount: f32,
) -> Option<Matrix3> {
    let mut moved = *camera_to_xyz;
    for primary in 0..3 {
        let (hue, saturation) = calibration.primary(primary);
        if hue == 0.0 && saturation == 0.0 {
            continue;
        }
        let column = [
            camera_to_xyz[0][primary],
            camera_to_xyz[1][primary],
            camera_to_xyz[2][primary],
        ];
        let turned = turn(
            column,
            hue * amount * Calibration::HUE_RANGE_DEG,
            1.0 + saturation * amount * Calibration::SATURATION_RANGE,
        );
        for (row, value) in turned.iter().enumerate() {
            moved[row][primary] = *value;
        }
    }

    // Solve `moved · k = neutral` for the three gains that put the white back.
    // Exactly, not iteratively: it is a 3×3 system and it has an answer.
    let inverse = invert(&moved)?;
    let gains = [
        (0..3).map(|i| inverse[0][i] * neutral[i]).sum::<f32>(),
        (0..3).map(|i| inverse[1][i] * neutral[i]).sum::<f32>(),
        (0..3).map(|i| inverse[2][i] * neutral[i]).sum::<f32>(),
    ];
    // A gain of zero is a channel that has stopped contributing, and a negative
    // one is a channel contributing backwards — both are a rendering nobody
    // would recognise as their photograph. The floor is well clear of either.
    if gains.iter().any(|g| !g.is_finite() || *g < 0.05) {
        return None;
    }
    let mut out = moved;
    for row in out.iter_mut() {
        for (primary, cell) in row.iter_mut().enumerate() {
            *cell *= gains[primary];
        }
    }
    Some(out)
}

/// One primary, rotated in hue and scaled in chroma about D50.
///
/// Lightness is left alone: a primary's *brightness* is the channel's gain, and
/// changing it here would be a white balance wearing a different name — which
/// the neutral solve above would then undo, leaving a slider that did nothing.
fn turn(xyz: [f32; 3], degrees: f32, chroma: f32) -> [f32; 3] {
    let lab = xyz_to_lab(xyz);
    let (sin, cos) = degrees.to_radians().sin_cos();
    let (a, b) = (lab[1], lab[2]);
    lab_to_xyz([
        lab[0],
        (a * cos - b * sin) * chroma,
        (a * sin + b * cos) * chroma,
    ])
}

/// CIE XYZ to L\*a\*b\*, referred to D50.
pub fn xyz_to_lab(xyz: [f32; 3]) -> [f32; 3] {
    // The CIE's own piecewise function. A camera primary can carry a small
    // negative tristimulus, which has no cube root — clamped at zero, because
    // the alternative is a NaN propagating into the matrix.
    let f = |t: f32| {
        let t = t.max(0.0);
        if t > 216.0 / 24389.0 {
            t.cbrt()
        } else {
            (24389.0 / 27.0 * t + 16.0) / 116.0
        }
    };
    let (fx, fy, fz) = (f(xyz[0] / D50[0]), f(xyz[1] / D50[1]), f(xyz[2] / D50[2]));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// The inverse of [`xyz_to_lab`].
pub fn lab_to_xyz(lab: [f32; 3]) -> [f32; 3] {
    let fy = (lab[0] + 16.0) / 116.0;
    let fx = fy + lab[1] / 500.0;
    let fz = fy - lab[2] / 200.0;
    let g = |t: f32| {
        if t.powi(3) > 216.0 / 24389.0 {
            t.powi(3)
        } else {
            (116.0 * t - 16.0) * 27.0 / 24389.0
        }
    };
    [g(fx) * D50[0], g(fy) * D50[1], g(fz) * D50[2]]
}

/// The CIE ΔE2000 difference between two L\*a\*b\* colours.
///
/// The current standard, and the one this project's calibration is judged by. Not
/// ΔE76, which is a plain Euclidean distance and famously disagrees with the eye
/// about blues; not ΔE94, which 2000 supersedes.
pub fn delta_e_2000(one: [f32; 3], two: [f32; 3]) -> f32 {
    let (l1, a1, b1) = (one[0] as f64, one[1] as f64, one[2] as f64);
    let (l2, a2, b2) = (two[0] as f64, two[1] as f64, two[2] as f64);
    let c1 = (a1 * a1 + b1 * b1).sqrt();
    let c2 = (a2 * a2 + b2 * b2).sqrt();
    let bar_c = (c1 + c2) / 2.0;
    let g = 0.5 * (1.0 - (bar_c.powi(7) / (bar_c.powi(7) + 25f64.powi(7))).sqrt());
    let (ap1, ap2) = (a1 * (1.0 + g), a2 * (1.0 + g));
    let cp1 = (ap1 * ap1 + b1 * b1).sqrt();
    let cp2 = (ap2 * ap2 + b2 * b2).sqrt();

    let angle = |a: f64, b: f64, c: f64| {
        if c == 0.0 {
            0.0
        } else {
            b.atan2(a).to_degrees().rem_euclid(360.0)
        }
    };
    let hp1 = angle(ap1, b1, cp1);
    let hp2 = angle(ap2, b2, cp2);

    let dl = l2 - l1;
    let dc = cp2 - cp1;
    let dh = if cp1 * cp2 == 0.0 {
        0.0
    } else if (hp2 - hp1).abs() <= 180.0 {
        hp2 - hp1
    } else if hp2 <= hp1 {
        hp2 - hp1 + 360.0
    } else {
        hp2 - hp1 - 360.0
    };
    let big_dh = 2.0 * (cp1 * cp2).sqrt() * (dh.to_radians() / 2.0).sin();

    let bar_l = (l1 + l2) / 2.0;
    let bar_cp = (cp1 + cp2) / 2.0;
    let bar_hp = if cp1 * cp2 == 0.0 {
        hp1 + hp2
    } else if (hp1 - hp2).abs() <= 180.0 {
        (hp1 + hp2) / 2.0
    } else if hp1 + hp2 < 360.0 {
        (hp1 + hp2 + 360.0) / 2.0
    } else {
        (hp1 + hp2 - 360.0) / 2.0
    };

    let t = 1.0 - 0.17 * (bar_hp - 30.0).to_radians().cos()
        + 0.24 * (2.0 * bar_hp).to_radians().cos()
        + 0.32 * (3.0 * bar_hp + 6.0).to_radians().cos()
        - 0.20 * (4.0 * bar_hp - 63.0).to_radians().cos();
    let d_theta = 30.0 * (-(((bar_hp - 275.0) / 25.0).powi(2))).exp();
    let rc = 2.0 * (bar_cp.powi(7) / (bar_cp.powi(7) + 25f64.powi(7))).sqrt();
    let sl = 1.0 + (0.015 * (bar_l - 50.0).powi(2)) / (20.0 + (bar_l - 50.0).powi(2)).sqrt();
    let sc = 1.0 + 0.045 * bar_cp;
    let sh = 1.0 + 0.015 * bar_cp * t;
    let rt = -(2.0 * d_theta).to_radians().sin() * rc;

    (((dl / sl).powi(2)
        + (dc / sc).powi(2)
        + (big_dh / sh).powi(2)
        + rt * (dc / sc) * (big_dh / sh))
        .max(0.0))
    .sqrt() as f32
}

/// A 3×3 inverse, or `None` when there is not one.
pub fn invert(m: &Matrix3) -> Option<Matrix3> {
    let determinant = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if !determinant.is_finite() || determinant.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / determinant;
    Some([
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv,
        ],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plausible camera-to-XYZ: sRGB's own primaries, adapted to D50. Not a
    /// real sensor, and it does not need to be — what is being tested is what
    /// the transform does to a matrix, and this one has an answer anybody can
    /// check by eye.
    const SRGB_TO_XYZ_D50: Matrix3 = [
        [0.4360747, 0.3850649, 0.1430804],
        [0.2225045, 0.7168786, 0.0606169],
        [0.0139322, 0.0971045, 0.7141733],
    ];

    fn with(hue: [f32; 3], saturation: [f32; 3]) -> Calibration {
        Calibration {
            red_hue: hue[0],
            green_hue: hue[1],
            blue_hue: hue[2],
            red_saturation: saturation[0],
            green_saturation: saturation[1],
            blue_saturation: saturation[2],
            ..Calibration::default()
        }
    }

    fn neutral_of(m: &Matrix3) -> [f32; 3] {
        [m[0].iter().sum(), m[1].iter().sum(), m[2].iter().sum()]
    }

    #[test]
    fn an_untouched_calibration_is_the_matrix_it_was_given() {
        // Bit for bit, not nearly: a photograph nobody has calibrated must not
        // render through a rebuilt matrix that happens to be close.
        let out = calibrated(&SRGB_TO_XYZ_D50, &Calibration::default());
        assert_eq!(out, SRGB_TO_XYZ_D50);
    }

    #[test]
    fn the_neutral_does_not_move_however_the_primaries_do() {
        // The property that makes this a calibration rather than a cast.
        // Rotating three columns changes their sum, and their sum is where
        // camera neutral lands — so without the gain solve every touch of a
        // primary slider would warm or cool the photograph, which is felt as the
        // white balance drifting while you adjust a red.
        let was = neutral_of(&SRGB_TO_XYZ_D50);
        for calibration in [
            with([1.0, 0.0, 0.0], [0.0; 3]),
            with([0.0, -1.0, 0.0], [0.0; 3]),
            with([0.0, 0.0, 1.0], [0.0; 3]),
            with([0.0; 3], [1.0, -1.0, 0.5]),
            with([-0.6, 0.8, -0.3], [0.4, -0.7, 1.0]),
        ] {
            let out = calibrated(&SRGB_TO_XYZ_D50, &calibration);
            let now = neutral_of(&out);
            for axis in 0..3 {
                assert!(
                    (now[axis] - was[axis]).abs() < 1e-4,
                    "{calibration:?} moved the neutral from {was:?} to {now:?}"
                );
            }
        }
    }

    #[test]
    fn a_hue_slider_turns_the_primary_it_names_and_no_other() {
        // What the control claims. Measured as a hue angle in the space the
        // rotation happens in, so the number is the one the slider promised —
        // and the other two primaries are checked as well, because a transform
        // that turned all three would look almost right and be a cast.
        let hue_of = |m: &Matrix3, primary: usize| {
            let lab = xyz_to_lab([m[0][primary], m[1][primary], m[2][primary]]);
            lab[2].atan2(lab[1]).to_degrees().rem_euclid(360.0)
        };
        for primary in 0..3 {
            let mut hue = [0.0f32; 3];
            hue[primary] = 1.0;
            let out = calibrated(&SRGB_TO_XYZ_D50, &with(hue, [0.0; 3]));

            let moved =
                (hue_of(&out, primary) - hue_of(&SRGB_TO_XYZ_D50, primary)).rem_euclid(360.0);
            assert!(
                (moved - Calibration::HUE_RANGE_DEG).abs() < 1.5,
                "primary {primary} turned {moved}°, not {}°",
                Calibration::HUE_RANGE_DEG
            );
            for other in 0..3 {
                if other == primary {
                    continue;
                }
                // The gain solve scales the other columns, which does not turn
                // them: a scale along a ray from the origin leaves the hue.
                let drift = (hue_of(&out, other) - hue_of(&SRGB_TO_XYZ_D50, other))
                    .abs()
                    .min(360.0 - (hue_of(&out, other) - hue_of(&SRGB_TO_XYZ_D50, other)).abs());
                assert!(
                    drift < 0.5,
                    "turning primary {primary} turned primary {other} by {drift}°"
                );
            }
        }
    }

    #[test]
    fn a_saturation_slider_moves_the_primary_away_from_the_white() {
        let chroma_of = |m: &Matrix3, primary: usize| {
            let lab = xyz_to_lab([m[0][primary], m[1][primary], m[2][primary]]);
            lab[1].hypot(lab[2])
        };
        for primary in 0..3 {
            let mut up = [0.0f32; 3];
            up[primary] = 1.0;
            let more = calibrated(&SRGB_TO_XYZ_D50, &with([0.0; 3], up));
            let mut down = [0.0f32; 3];
            down[primary] = -1.0;
            let less = calibrated(&SRGB_TO_XYZ_D50, &with([0.0; 3], down));
            assert!(
                chroma_of(&more, primary) > chroma_of(&SRGB_TO_XYZ_D50, primary),
                "primary {primary} did not gain chroma"
            );
            assert!(
                chroma_of(&less, primary) < chroma_of(&SRGB_TO_XYZ_D50, primary),
                "primary {primary} did not lose chroma"
            );
        }
    }

    #[test]
    fn lab_survives_the_round_trip() {
        for xyz in [
            D50,
            [0.2, 0.1, 0.05],
            [0.05, 0.02, 0.3],
            [0.7, 0.9, 0.4],
            [0.0, 0.0, 0.0],
        ] {
            let back = lab_to_xyz(xyz_to_lab(xyz));
            for axis in 0..3 {
                assert!(
                    (back[axis] - xyz[axis]).abs() < 1e-5,
                    "{xyz:?} came back as {back:?}"
                );
            }
        }
    }

    #[test]
    fn delta_e_2000_agrees_with_the_published_pairs() {
        // Sharma, Wu and Dalal's test data — the reference set the formula's own
        // authors published, because ΔE2000 is full of hue-angle cases that a
        // plausible-looking implementation gets wrong only in one quadrant.
        let cases = [
            ([50.0, 2.6772, -79.7751], [50.0, 0.0, -82.7485], 2.0425),
            ([50.0, 3.1571, -77.2803], [50.0, 0.0, -82.7485], 2.8615),
            ([50.0, 2.8361, -74.0200], [50.0, 0.0, -82.7485], 3.4412),
            ([50.0, -1.3802, -84.2814], [50.0, 0.0, -82.7485], 1.0000),
            ([50.0, 2.5, 0.0], [50.0, 0.0, -2.5], 4.3065),
            (
                [60.2574, -34.0099, 36.2677],
                [60.4626, -34.1751, 39.4387],
                1.2644,
            ),
            (
                [2.0776, 0.0795, -1.1350],
                [0.9033, -0.0636, -0.5514],
                0.9082,
            ),
        ];
        for (a, b, want) in cases {
            let got = delta_e_2000(a, b);
            assert!(
                (got - want).abs() < 1e-3,
                "ΔE({a:?}, {b:?}) came out {got}, not {want}"
            );
        }
        assert_eq!(delta_e_2000([50.0, 2.5, 0.0], [50.0, 2.5, 0.0]), 0.0);
    }
}
