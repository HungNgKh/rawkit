//! Camera colour profiles — pipeline stage E.
//!
//! # What a profile is for
//!
//! A sensor's red, green and blue are not anybody's red, green and blue. They
//! are three arbitrary spectral responses, and turning them into a colour a
//! person would recognise takes two things: knowing what illuminant the scene
//! was lit by, and knowing how this particular sensor responds to it. That is
//! the whole job of this module.
//!
//! It is also, less obviously, what makes the white-balance slider possible.
//! "5200 K" is not a property of the image — it is a claim about the light, and
//! turning it into three channel multipliers requires the sensor's response to
//! that light. Without a profile, temperature is not a hard feature, it is a
//! meaningless one.
//!
//! # What this implements, and what it does not
//!
//! Implemented: the Adobe colour-matrix model — one or two calibration
//! illuminants, interpolated by correlated colour temperature; temperature and
//! tint in the CIE 1960 UCS the way Adobe defines them, so the numbers are
//! comparable with what other editors show; and the white-balance-aware
//! camera-to-display transform.
//!
//! **Not implemented:** parsing `.dcp` files, `ProfileHueSatMap`,
//! `ProfileLookTable` and the profile's own tone curve. Those carry the parts of
//! a profile that are a *look* rather than a measurement, and they are what
//! separates "colour that is defensible" from "colour that matches Adobe". The
//! structure here is shaped to take them without rework: a profile is already a
//! value that the renderer asks for a matrix, rather than a matrix the renderer
//! assumes.
//!
//! Note also that Adobe's own bundled `.dcp` files are not redistributable, so
//! shipping profiles is not a thing this project can do regardless — a parser
//! reads what the *user* already has.

/// A 3x3 matrix, row-major. Small enough that a dedicated type earns nothing
/// beyond an alias, and this keeps it obvious which way round the rows are.
pub type Matrix3 = [[f32; 3]; 3];

/// sRGB primaries to CIE XYZ (D65).
pub const XYZ_FROM_SRGB: Matrix3 = [
    [0.412_453, 0.357_580, 0.180_423],
    [0.212_671, 0.715_160, 0.072_169],
    [0.019_334, 0.119_193, 0.950_227],
];

/// Adobe's tint scale: tint is an offset in CIE 1960 `v`, divided by this. Using
/// Adobe's constant is deliberate — it makes a tint of +10 here mean roughly
/// what +10 means in Lightroom, which is the only reason a number on a slider
/// is worth anything to a user coming from somewhere else.
const TINT_SCALE: f32 = 3000.0;

/// The temperature range the Planckian approximation below is valid over.
/// Outside it the polynomial diverges, so callers are clamped rather than
/// allowed to produce nonsense.
pub const MIN_TEMPERATURE: f32 = 1667.0;
pub const MAX_TEMPERATURE: f32 = 25000.0;

/// One calibration illuminant: the temperature it represents, and the matrix
/// taking CIE XYZ to this camera's native RGB under it.
#[derive(Debug, Clone, Copy)]
struct Calibration {
    cct: f32,
    xyz_to_camera: Matrix3,
}

/// A camera's colour characterisation.
///
/// Holds one or two calibration illuminants. Two is the Adobe norm — typically
/// a tungsten-ish standard illuminant and a daylight one — and a single matrix
/// is the common case for a decoder's built-in table, which is what most
/// non-DNG files give us.
#[derive(Debug, Clone)]
pub struct CameraProfile {
    calibrations: Vec<Calibration>,
}

impl CameraProfile {
    /// A profile from a single XYZ-to-camera matrix, as a decoder's camera table
    /// provides.
    ///
    /// Treated as a D65 characterisation, because that is what those tables are
    /// derived from. The consequence is worth stating: with one illuminant there
    /// is nothing to interpolate, so a tungsten scene is rendered with a
    /// daylight characterisation and the colour will be *defensible but not
    /// accurate*. A two-illuminant profile is the fix, and it needs a `.dcp`.
    pub fn from_color_matrix(xyz_to_camera: Matrix3) -> Self {
        Self {
            calibrations: vec![Calibration {
                cct: 6504.0,
                xyz_to_camera,
            }],
        }
    }

    /// A two-illuminant profile, as a `.dcp` provides.
    pub fn from_dual_illuminant(low: (f32, Matrix3), high: (f32, Matrix3)) -> Self {
        let mut calibrations = vec![
            Calibration {
                cct: low.0,
                xyz_to_camera: low.1,
            },
            Calibration {
                cct: high.0,
                xyz_to_camera: high.1,
            },
        ];
        calibrations.sort_by(|a, b| a.cct.total_cmp(&b.cct));
        Self { calibrations }
    }

    /// The XYZ-to-camera matrix for a given colour temperature.
    ///
    /// Adobe interpolates in reciprocal temperature (mireds) rather than in
    /// Kelvin, because perceptual steps in illuminant colour are roughly even in
    /// mireds and wildly uneven in Kelvin — 2000 K to 3000 K is an enormous
    /// change, 9000 K to 10000 K is barely visible.
    pub fn xyz_to_camera(&self, cct: f32) -> Matrix3 {
        match self.calibrations.as_slice() {
            [only] => only.xyz_to_camera,
            [low, high] => {
                let (a, b) = (1e6 / low.cct, 1e6 / high.cct);
                let m = 1e6 / cct.clamp(MIN_TEMPERATURE, MAX_TEMPERATURE);
                // `a` is the larger mired value (lower temperature).
                let t = ((m - b) / (a - b)).clamp(0.0, 1.0);
                let mut out = [[0.0f32; 3]; 3];
                for (r, row) in out.iter_mut().enumerate() {
                    for (c, cell) in row.iter_mut().enumerate() {
                        *cell = high.xyz_to_camera[r][c] * (1.0 - t) + low.xyz_to_camera[r][c] * t;
                    }
                }
                out
            }
            _ => unreachable!("a profile always has one or two calibrations"),
        }
    }

    /// White-balance multipliers for a temperature and tint.
    ///
    /// The multipliers are what the camera's channels must be scaled by so that
    /// the named illuminant comes out neutral. Green is normalised to 1.0, which
    /// is both the camera convention and something the demosaic kernel relies
    /// on.
    pub fn multipliers_for(&self, temperature_k: f32, tint: f32) -> [f32; 3] {
        let xyz = white_point_xyz(temperature_k, tint);
        let camera = apply(&self.xyz_to_camera(temperature_k), xyz);
        // A channel that responds barely at all to the illuminant would produce
        // an enormous multiplier and a wall of noise; refusing to divide by
        // almost-zero keeps a pathological profile from producing a
        // pathological image.
        let safe = |v: f32| if v.abs() < 1e-6 { 1e-6 } else { v };
        let m = [
            1.0 / safe(camera[0]),
            1.0 / safe(camera[1]),
            1.0 / safe(camera[2]),
        ];
        [m[0] / m[1], 1.0, m[2] / m[1]]
    }

    /// The inverse: what temperature and tint do these multipliers describe?
    ///
    /// This is how "As Shot" becomes a number the user can see and then nudge.
    ///
    /// Solved directly rather than searched, because correlated colour
    /// temperature *is* a direct definition: undo the multipliers to recover the
    /// illuminant's camera response, put it back into XYZ, move to the CIE 1960
    /// UCS, and the answer is the nearest point on the Planckian locus with the
    /// perpendicular distance as tint. That is the definition of the two
    /// quantities, not an approximation of them.
    ///
    /// The loop exists only because a two-illuminant profile's matrix depends on
    /// the temperature being solved for. It converges in two passes and runs
    /// three; for a single-illuminant profile the first pass is already exact.
    pub fn temperature_from_multipliers(&self, multipliers: [f32; 3]) -> (f32, f32) {
        let neutral_camera = [
            1.0 / multipliers[0].max(1e-6),
            1.0 / multipliers[1].max(1e-6),
            1.0 / multipliers[2].max(1e-6),
        ];

        let mut temperature = 5000.0f32;
        let mut tint = 0.0f32;
        for _ in 0..3 {
            let Some(camera_to_xyz) = invert(&self.xyz_to_camera(temperature)) else {
                return (temperature, tint);
            };
            let xyz = apply(&camera_to_xyz, neutral_camera);
            let (u, v) = uv_from_xyz(xyz);

            // Nearest point on the locus, searched in mireds because that is
            // where the locus is roughly evenly spaced — searching in Kelvin
            // would spend most of its samples above 10000 K where nothing moves.
            let mired = search(1e6 / MAX_TEMPERATURE, 1e6 / MIN_TEMPERATURE, 128, |m| {
                let (lu, lv) = uv_from_cct(1e6 / m);
                (lu - u).powi(2) + (lv - v).powi(2)
            });
            temperature = (1e6 / mired).clamp(MIN_TEMPERATURE, MAX_TEMPERATURE);

            let (lu, lv) = uv_from_cct(temperature);
            let (pu, pv) = locus_normal(temperature);
            tint = ((u - lu) * pu + (v - lv) * pv) * TINT_SCALE;
        }
        (temperature, tint)
    }

    /// Camera-native RGB to the display's linear primaries, for a given white
    /// balance.
    ///
    /// The white balance has to be part of this and not a separate step applied
    /// before it. The rows are normalised so that the balanced camera neutral
    /// maps to display neutral; skip that and every image carries a cast which
    /// looks exactly like a white-balance error and is not one.
    pub fn camera_to_display(&self, temperature_k: f32) -> Matrix3 {
        let xyz_to_camera = self.xyz_to_camera(temperature_k);
        // Compose to get sRGB-primaries -> camera, then normalise each row so a
        // neutral input gives a neutral output, then invert.
        let mut camera_from_srgb = [[0.0f32; 3]; 3];
        for (r, row) in camera_from_srgb.iter_mut().enumerate() {
            for (c, cell) in row.iter_mut().enumerate() {
                *cell = (0..3)
                    .map(|k| xyz_to_camera[r][k] * XYZ_FROM_SRGB[k][c])
                    .sum();
            }
            let sum: f32 = row.iter().sum();
            if sum.abs() > 1e-6 {
                for cell in row.iter_mut() {
                    *cell /= sum;
                }
            }
        }
        invert(&camera_from_srgb).unwrap_or(IDENTITY)
    }
}

pub const IDENTITY: Matrix3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// The XYZ of the illuminant named by a temperature and tint.
///
/// Temperature places a point on the Planckian locus; tint moves it
/// perpendicular to the locus in the CIE 1960 UCS, which is where "perpendicular
/// to the locus" is a meaningful thing to say. Positive tint is green, negative
/// magenta, matching every other editor's convention.
fn white_point_xyz(temperature_k: f32, tint: f32) -> [f32; 3] {
    let t = temperature_k.clamp(MIN_TEMPERATURE, MAX_TEMPERATURE);
    let (u, v) = uv_from_cct(t);
    let (pu, pv) = locus_normal(t);

    let offset = tint / TINT_SCALE;
    let (u, v) = (u + pu * offset, v + pv * offset);

    // CIE 1960 UCS back to xy.
    let denom = 2.0 * u - 8.0 * v + 4.0;
    let x = 3.0 * u / denom;
    let y = 2.0 * v / denom;
    if y.abs() < 1e-9 {
        return [0.9642, 1.0, 0.8249];
    }
    [x / y, 1.0, (1.0 - x - y) / y]
}

/// The unit normal to the Planckian locus, which is the direction tint moves
/// along.
///
/// Taken numerically. A closed form exists and is one more approximation to get
/// wrong, while the locus is smooth everywhere in range — and, more usefully,
/// sharing this between the forward and inverse conversions means the two
/// cannot drift apart, which is the failure that would show up as a
/// white-balance slider that does not return where it started.
///
/// **Sign convention: positive tint is magenta, negative is green.** This
/// matches Lightroom, whose tint slider runs green on the left to magenta on
/// the right. Matching an arbitrary convention is worth more than picking a
/// nicer one, because the number is meaningless except by comparison.
fn locus_normal(t: f32) -> (f32, f32) {
    let step = t * 0.001;
    let (u0, v0) = uv_from_cct((t - step).max(MIN_TEMPERATURE));
    let (u1, v1) = uv_from_cct((t + step).min(MAX_TEMPERATURE));
    let (du, dv) = (u1 - u0, v1 - v0);
    let len = (du * du + dv * dv).sqrt().max(1e-9);
    (-dv / len, du / len)
}

/// CIE XYZ to the CIE 1960 UCS.
fn uv_from_xyz(xyz: [f32; 3]) -> (f32, f32) {
    let denom = xyz[0] + 15.0 * xyz[1] + 3.0 * xyz[2];
    if denom.abs() < 1e-9 {
        return (0.0, 0.0);
    }
    (4.0 * xyz[0] / denom, 6.0 * xyz[1] / denom)
}

/// Planckian locus in CIE 1960 UCS, via the standard cubic approximation to
/// blackbody chromaticity (Kim et al.), valid over 1667–25000 K.
///
/// The coefficients are quoted at their published precision rather than rounded
/// to what `f32` can hold. Clippy is right that the extra digits do not survive
/// the literal, and they are kept anyway: their job is to be checkable against
/// the source they came from, and a reader comparing them against the paper
/// should find the same numbers.
#[allow(clippy::excessive_precision)]
fn uv_from_cct(t: f32) -> (f32, f32) {
    let inv = 1.0 / t;
    let inv2 = inv * inv;
    let inv3 = inv2 * inv;
    let x = if t < 4000.0 {
        -0.2661239e9 * inv3 - 0.2343589e6 * inv2 + 0.8776956e3 * inv + 0.179_910
    } else {
        -3.0258469e9 * inv3 + 2.1070379e6 * inv2 + 0.2226347e3 * inv + 0.240_390
    };
    let x2 = x * x;
    let x3 = x2 * x;
    let y = if t < 2222.0 {
        -1.1063814 * x3 - 1.348_110_2 * x2 + 2.185_558_3 * x - 0.202_196_83
    } else if t < 4000.0 {
        -0.9549476 * x3 - 1.374_185_9 * x2 + 2.091_370_2 * x - 0.167_488_67
    } else {
        3.081_758 * x3 - 5.873_386_7 * x2 + 3.751_13 * x - 0.370_014_83
    };
    let denom = -2.0 * x + 12.0 * y + 3.0;
    (4.0 * x / denom, 6.0 * y / denom)
}

fn apply(m: &Matrix3, v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

pub fn invert(m: &Matrix3) -> Option<Matrix3> {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if det.abs() < 1e-9 {
        return None;
    }
    let d = 1.0 / det;
    Some([
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * d,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * d,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * d,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * d,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * d,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * d,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * d,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * d,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * d,
        ],
    ])
}

/// Coarse scan then local refinement. The objective is smooth and unimodal over
/// these ranges, so this converges quickly and — unlike a gradient method —
/// cannot wander off when a pathological profile makes it not quite unimodal.
fn search(mut low: f32, mut high: f32, steps: usize, cost: impl Fn(f32) -> f32) -> f32 {
    let mut best = low;
    for _ in 0..3 {
        let mut best_cost = f32::MAX;
        for i in 0..=steps {
            let x = low + (high - low) * i as f32 / steps as f32;
            let c = cost(x);
            if c < best_cost {
                best_cost = c;
                best = x;
            }
        }
        let window = (high - low) / steps as f32;
        low = best - window;
        high = best + window;
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Roughly a Sony APS-C sensor: the decoder's table for an ILCE-6400,
    /// XYZ to camera. Real numbers rather than an identity, so the tests
    /// exercise a matrix that actually mixes channels.
    fn sony_profile() -> CameraProfile {
        CameraProfile::from_color_matrix([
            [0.6941, -0.2164, -0.0644],
            [-0.3850, 1.1349, 0.2779],
            [-0.0031, 0.1055, 0.6511],
        ])
    }

    #[test]
    fn temperature_round_trips_through_multipliers() {
        // The property that makes a white-balance UI trustworthy: the number
        // shown for a set of multipliers, fed back in, must give those
        // multipliers again. Without it, opening and closing a panel drifts the
        // image.
        let profile = sony_profile();
        for temperature in [2000.0, 3200.0, 4800.0, 5500.0, 6504.0, 9000.0, 16000.0] {
            for tint in [-40.0, 0.0, 25.0] {
                let multipliers = profile.multipliers_for(temperature, tint);
                let (t, g) = profile.temperature_from_multipliers(multipliers);
                assert!(
                    (t - temperature).abs() / temperature < 0.02,
                    "{temperature}K tint {tint} came back as {t}K"
                );
                assert!(
                    (g - tint).abs() < 3.0,
                    "{temperature}K tint {tint} came back as tint {g}"
                );
            }
        }
    }

    #[test]
    fn a_neutral_subject_renders_neutral() {
        // The definition of white balance, stated as a test: light a neutral
        // surface with the illuminant, and after balancing and profiling it must
        // come out with equal display channels. This is the single check that
        // catches a matrix used in the wrong direction, which otherwise produces
        // an image that merely looks oddly graded.
        let profile = sony_profile();
        for temperature in [2800.0, 5000.0, 7500.0] {
            let multipliers = profile.multipliers_for(temperature, 0.0);
            let xyz = white_point_xyz(temperature, 0.0);
            let camera = apply(&profile.xyz_to_camera(temperature), xyz);

            let balanced = [
                camera[0] * multipliers[0],
                camera[1] * multipliers[1],
                camera[2] * multipliers[2],
            ];
            let display = apply(&profile.camera_to_display(temperature), balanced);

            let mean = (display[0] + display[1] + display[2]) / 3.0;
            for (c, v) in display.iter().enumerate() {
                assert!(
                    (v - mean).abs() / mean < 0.02,
                    "at {temperature}K a neutral subject rendered {display:?}, \
                     channel {c} is off by {:.1}%",
                    100.0 * (v - mean).abs() / mean
                );
            }
        }
    }

    #[test]
    fn raising_the_temperature_warms_the_image() {
        // Direction matters and is easy to get backwards: a *higher* stated
        // temperature means the light is claimed to be bluer, so less blue gain
        // is needed to neutralise it, and the picture warms up. Getting this
        // inverted produces a slider that works perfectly and backwards.
        let profile = sony_profile();
        let cool = profile.multipliers_for(3000.0, 0.0);
        let warm = profile.multipliers_for(9000.0, 0.0);
        assert!(
            warm[0] / warm[2] > cool[0] / cool[2],
            "red:blue ratio fell as temperature rose ({cool:?} -> {warm:?})"
        );
    }

    #[test]
    fn positive_tint_is_magenta_as_in_lightroom() {
        // The convention is arbitrary and therefore worth pinning: Lightroom's
        // tint slider runs green on the left, magenta on the right, so positive
        // is magenta here too. A test rather than a comment because the sign
        // lives in a numerically-derived normal, where flipping it is a one
        // character change nobody would notice.
        let profile = sony_profile();
        let neutral = profile.multipliers_for(5500.0, 0.0);
        let magenta = profile.multipliers_for(5500.0, 60.0);
        let green = profile.multipliers_for(5500.0, -60.0);

        // Multipliers neutralise the illuminant. A magenta illuminant needs less
        // red and blue gain to bring it back to neutral; a green one needs more.
        assert!(
            magenta[0] < neutral[0] && neutral[0] < green[0],
            "tint sign is inverted: magenta {magenta:?}, neutral {neutral:?}, green {green:?}"
        );
        assert!(magenta[2] < neutral[2] && neutral[2] < green[2]);
    }

    #[test]
    fn two_illuminants_interpolate_between_themselves() {
        let low = [[0.5, 0.0, 0.0], [0.0, 0.5, 0.0], [0.0, 0.0, 0.5]];
        let high = [[1.5, 0.0, 0.0], [0.0, 1.5, 0.0], [0.0, 0.0, 1.5]];
        let profile = CameraProfile::from_dual_illuminant((2856.0, low), (6504.0, high));

        assert!((profile.xyz_to_camera(2856.0)[0][0] - 0.5).abs() < 1e-4);
        assert!((profile.xyz_to_camera(6504.0)[0][0] - 1.5).abs() < 1e-4);
        let middle = profile.xyz_to_camera(4000.0)[0][0];
        assert!(
            middle > 0.5 && middle < 1.5,
            "interpolation left the range: {middle}"
        );
        // Outside the calibrated range it clamps rather than extrapolating —
        // extrapolating a measurement is how a profile invents colours.
        assert!((profile.xyz_to_camera(1000.0)[0][0] - 0.5).abs() < 1e-4);
        assert!((profile.xyz_to_camera(40000.0)[0][0] - 1.5).abs() < 1e-4);
    }

    #[test]
    fn interpolation_is_even_in_mireds_not_kelvin() {
        // 2000K->3000K is an enormous change in illuminant colour and
        // 9000K->10000K is barely visible. Interpolating linearly in Kelvin
        // would spend most of the profile's precision where nothing happens.
        let low = [[0.0, 0.0, 0.0], [0.0, 0.0, 0.0], [0.0, 0.0, 0.0]];
        let high = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        let profile = CameraProfile::from_dual_illuminant((2500.0, low), (10000.0, high));
        // The midpoint in mireds is 1e6 / ((400 + 100) / 2) = 4000K.
        let at_4000 = profile.xyz_to_camera(4000.0)[0][0];
        assert!(
            (at_4000 - 0.5).abs() < 0.02,
            "4000K should be the halfway point in mireds, got {at_4000}"
        );
    }
}
