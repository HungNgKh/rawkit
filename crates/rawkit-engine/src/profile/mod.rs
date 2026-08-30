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
//! # The line this module draws
//!
//! A profile file carries two different kinds of thing, and only one of them is
//! adopted here.
//!
//! **Measurement — used.** The colour matrices, the forward matrices, and the
//! hue/saturation table. These describe how the sensor responds. The tell is
//! that a profile carries *two* of each, one per calibration illuminant,
//! interpolated by temperature: a description of the sensor has to depend on the
//! light, and a look does not.
//!
//! **Look — deliberately not used.** `ProfileLookTable` and `ProfileToneCurve`,
//! of which there is exactly one regardless of illuminant. These encode what
//! Adobe thinks a photograph should look like. Adopting them would mean the
//! product renders differently depending on whether a user happened to have
//! Adobe software installed, would leave our tone map dead code for profiled
//! files, and would quietly reverse the decision to stop chasing rendering
//! parity. The tags are named in [`dcp`] so the omission reads as a choice.
//!
//! Adobe's own bundled `.dcp` files are not redistributable in any case, so this
//! reads what the *user* already has; the project can never ship one.

/// A 3x3 matrix, row-major. Small enough that a dedicated type earns nothing
/// beyond an alias, and this keeps it obvious which way round the rows are.
pub type Matrix3 = [[f32; 3]; 3];

pub mod dcp;

/// CIE XYZ (D50) to linear sRGB, Bradford-adapted.
///
/// D50 rather than D65 because that is the connection space the DNG forward
/// matrices land in — a profile's forward matrix rows sum to the D50 white
/// point by definition, which is what makes a balanced neutral come out neutral
/// without any further normalisation.
/// Quoted at published precision for the same reason as the locus polynomial:
/// the extra digits do not survive `f32`, and their job is to be checkable
/// against the source rather than to be stored.
#[allow(clippy::excessive_precision)]
pub const SRGB_FROM_XYZ_D50: Matrix3 = [
    [3.1338561, -1.6168667, -0.4906146],
    [-0.9787684, 1.9161415, 0.0334540],
    [0.0719453, -0.2289914, 1.4052427],
];

/// CIE XYZ (D50) to linear ProPhoto RGB.
///
/// The working space the hue/saturation table is defined in. It has to be this
/// one and not ours: the table's hue angles were measured in ProPhoto, and
/// applying them in a different space rotates every correction by the angle
/// between the two gamuts — which shows up as a profile making colour slightly
/// worse rather than better.
#[allow(clippy::excessive_precision)]
pub const PROPHOTO_FROM_XYZ_D50: Matrix3 = [
    [1.3459433, -0.2556075, -0.0511118],
    [-0.5445989, 1.5081673, 0.0205351],
    [0.0000000, 0.0000000, 1.2118128],
];

/// Linear ProPhoto RGB to CIE XYZ (D50).
#[allow(clippy::excessive_precision)]
pub const XYZ_D50_FROM_PROPHOTO: Matrix3 = [
    [0.7976749, 0.1351917, 0.0313534],
    [0.2880402, 0.7118741, 0.0000857],
    [0.0000000, 0.0000000, 0.8252100],
];

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

/// A profile's hue/saturation correction table.
///
/// A 3x3 matrix is a linear map, and a sensor's departure from one is not
/// linear: deep blues drift purple, foliage greens go yellow, skin tones shift.
/// This table is where a profile stores those corrections, as deltas indexed by
/// hue, saturation and value.
///
/// **This is measurement, not taste**, and the structure says so: a profile
/// carries *two* of these, one per calibration illuminant, interpolated exactly
/// like the colour matrices. A look would not depend on what light the scene was
/// under. That is why this is adopted while `ProfileLookTable` and
/// `ProfileToneCurve` — of which there is one, regardless of illuminant — are
/// deliberately not.
#[derive(Debug, Clone, PartialEq)]
pub struct HueSatMap {
    pub hue_divisions: u32,
    pub sat_divisions: u32,
    pub value_divisions: u32,
    /// `(hue shift in degrees, saturation scale, value scale)` per cell, in the
    /// DNG ordering: value outermost, then hue, then saturation.
    pub deltas: Vec<[f32; 3]>,
}

impl HueSatMap {
    pub fn cell_count(&self) -> usize {
        (self.hue_divisions * self.sat_divisions * self.value_divisions) as usize
    }

    /// Whether the table is well-formed. A profile whose declared dimensions do
    /// not match its data is corrupt, and applying it would read whatever
    /// happened to be next in the file.
    pub fn is_valid(&self) -> bool {
        self.hue_divisions > 0
            && self.sat_divisions > 0
            && self.value_divisions > 0
            && self.deltas.len() == self.cell_count()
    }

    /// The table that sits `t` of the way from `self` to `other`, where 1 is
    /// fully `other`.
    ///
    /// Interpolating the tables rather than applying both and blending the
    /// results is what the DNG specification calls for, and it is also the only
    /// version that is cheap: one table is uploaded per render instead of two
    /// sampled per pixel.
    fn blend(&self, other: &Self, t: f32) -> Option<Self> {
        if self.hue_divisions != other.hue_divisions
            || self.sat_divisions != other.sat_divisions
            || self.value_divisions != other.value_divisions
        {
            // Mismatched dimensions mean the two illuminants disagree about the
            // shape of the correction, which is not something to average.
            return None;
        }
        let deltas = self
            .deltas
            .iter()
            .zip(&other.deltas)
            .map(|(a, b)| {
                [
                    a[0] * (1.0 - t) + b[0] * t,
                    a[1] * (1.0 - t) + b[1] * t,
                    a[2] * (1.0 - t) + b[2] * t,
                ]
            })
            .collect();
        Some(Self {
            hue_divisions: self.hue_divisions,
            sat_divisions: self.sat_divisions,
            value_divisions: self.value_divisions,
            deltas,
        })
    }
}

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
    /// Camera to CIE XYZ (D50) for each calibration illuminant, when the
    /// profile provides them.
    ///
    /// This is the path Adobe's own renderer takes, and it is not merely the
    /// inverse of the colour matrix: a forward matrix is fitted so that the
    /// *white-balanced* camera values map to XYZ, which lets the profile encode
    /// corrections the single matrix cannot express. When present it is used;
    /// when absent the colour matrix is inverted instead, which is what a
    /// decoder's built-in table leaves us with.
    forward: Vec<Option<Matrix3>>,
    /// Hue/saturation correction per calibration illuminant.
    hue_sat: Vec<Option<HueSatMap>>,
    /// What the profile calls itself, for a UI to show. `None` for a profile
    /// synthesised from a decoder table, which has no name to give.
    pub name: Option<String>,
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
        Self::from_color_matrix_at(6504.0, xyz_to_camera)
    }

    /// A single-illuminant profile whose calibration temperature is known.
    ///
    /// The temperature is **recorded, not applied**. [`Self::xyz_to_camera`]
    /// interpolates between calibrations, and with one there is nothing to
    /// interpolate — so this matrix is used whatever the scene was lit by. That
    /// is the single-illuminant limitation stated precisely: not that the
    /// adaptation is approximate, but that there is no adaptation at all. The
    /// value earns its keep only when a second calibration joins it.
    pub fn from_color_matrix_at(cct: f32, xyz_to_camera: Matrix3) -> Self {
        Self {
            calibrations: vec![Calibration { cct, xyz_to_camera }],
            forward: vec![None],
            hue_sat: vec![None],
            name: None,
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
        Self {
            calibrations,
            forward: vec![None, None],
            hue_sat: vec![None, None],
            name: None,
        }
    }

    /// Attach a forward matrix to the calibration it belongs to, named by its
    /// illuminant temperature.
    ///
    /// Taking the temperature rather than a position is deliberate. The
    /// constructor sorts calibrations, so an index-based API would let a caller
    /// pass matrices in file order and pair a tungsten forward matrix with a
    /// daylight colour matrix — colour that is subtly wrong everywhere, correct
    /// nowhere, and has nothing to point at. Stating the temperature makes that
    /// mistake unrepresentable instead of merely documented.
    pub fn set_forward_matrix(&mut self, cct: f32, matrix: Matrix3) {
        if let Some(i) = self.nearest_calibration(cct) {
            self.forward[i] = Some(matrix);
        }
    }

    /// Which calibration a given illuminant temperature belongs to.
    ///
    /// Shared by everything that attaches per-illuminant data, so a forward
    /// matrix and a hue/saturation table quoted at the same temperature always
    /// land on the same calibration.
    fn nearest_calibration(&self, cct: f32) -> Option<usize> {
        self.calibrations
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| (a.cct - cct).abs().total_cmp(&(b.cct - cct).abs()))
            .map(|(i, _)| i)
    }

    /// Attach a hue/saturation table to the calibration it belongs to, named by
    /// its illuminant temperature — for the same reason forward matrices are.
    pub fn set_hue_sat_map(&mut self, cct: f32, map: HueSatMap) {
        if !map.is_valid() {
            return;
        }
        if let Some(i) = self.nearest_calibration(cct) {
            self.hue_sat[i] = Some(map);
        }
    }

    /// The hue/saturation table for a temperature, interpolated between the
    /// calibration illuminants.
    ///
    /// `None` when the profile has none, when the two tables disagree about
    /// their dimensions, or — deliberately — when the profile has no forward
    /// matrix. See [`Self::camera_to_working`] for why that last one.
    pub fn hue_sat_map(&self, cct: f32) -> Option<HueSatMap> {
        // Gated on the transform actually being available at *this*
        // temperature, not on the profile merely carrying one somewhere. The
        // first version asked `has_forward_matrix()` and the two answers could
        // differ, which a real profile — one forward matrix, two illuminants —
        // turned into a panic.
        self.camera_to_working(cct)?;
        match (self.calibrations.as_slice(), self.hue_sat.as_slice()) {
            ([_], [only]) => only.clone(),
            ([low, high], [a, b]) => match (a, b) {
                (Some(a), Some(b)) => a.blend(b, 1.0 - mired_blend(low.cct, high.cct, cct)),
                (Some(only), None) | (None, Some(only)) => Some(only.clone()),
                (None, None) => None,
            },
            _ => None,
        }
    }

    /// Whether this profile carries the forward matrices Adobe's rendering path
    /// uses, or only a colour matrix to invert.
    pub fn has_forward_matrix(&self) -> bool {
        self.forward.iter().any(Option::is_some)
    }

    /// Whether the profile characterises the sensor under two illuminants. One
    /// means every scene is rendered with the same characterisation regardless
    /// of its light, which is the main thing a real profile improves on.
    pub fn is_dual_illuminant(&self) -> bool {
        self.calibrations.len() > 1
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
                let t = mired_blend(low.cct, high.cct, cct);
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

    /// Camera to the working space the hue/saturation table lives in, and that
    /// space back to the display.
    ///
    /// Returns `None` when there is no table to apply, in which case the caller
    /// uses [`Self::camera_to_display`] directly and the extra hop is skipped
    /// entirely.
    ///
    /// **Only offered for profiles with a forward matrix.** The DNG reference
    /// space is reached *through* the forward matrix, and with a colour matrix
    /// alone there is no unambiguous route to it — the white balance is folded
    /// into the matrix by a normalisation of our own choosing, so "which linear
    /// space is this" has no single answer. Applying the table anyway would
    /// rotate its hue corrections by an unknown angle, making colour worse in a
    /// way that looks like the profile being wrong. Skipping it is the honest
    /// option and is what [`Self::hue_sat_map`] enforces.
    pub fn camera_to_working(&self, temperature_k: f32) -> Option<(Matrix3, Matrix3)> {
        let forward = self.forward_matrix(temperature_k)?;
        Some((
            multiply(&PROPHOTO_FROM_XYZ_D50, &forward),
            multiply(&SRGB_FROM_XYZ_D50, &XYZ_D50_FROM_PROPHOTO),
        ))
    }

    /// The forward matrix for a temperature, interpolated the same way the
    /// colour matrices are. `None` when the profile has none — a decoder table
    /// never does.
    fn forward_matrix(&self, cct: f32) -> Option<Matrix3> {
        match (self.calibrations.as_slice(), self.forward.as_slice()) {
            ([_], [only]) => *only,
            ([low, high], [a, b]) => match (a, b) {
                (Some(a), Some(b)) => {
                    let t = mired_blend(low.cct, high.cct, cct);
                    let mut out = [[0.0f32; 3]; 3];
                    for (r, row) in out.iter_mut().enumerate() {
                        for (c, cell) in row.iter_mut().enumerate() {
                            *cell = b[r][c] * (1.0 - t) + a[r][c] * t;
                        }
                    }
                    Some(out)
                }
                // A profile may give one forward matrix for both illuminants,
                // and the specification says to use it unconditionally. Doing
                // so also keeps this in step with `has_forward_matrix`: two
                // notions of "has one" that disagree is how a caller ends up
                // holding an impossible state.
                (Some(only), None) | (None, Some(only)) => Some(*only),
                (None, None) => None,
            },
            _ => None,
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
        if let Some(forward) = self.forward_matrix(temperature_k) {
            // The forward matrix already maps *balanced* camera values to XYZ
            // D50, and its rows sum to the D50 white point by construction, so
            // a balanced neutral arrives neutral with no normalisation of our
            // own. All that remains is the connection space.
            return multiply(&SRGB_FROM_XYZ_D50, &forward);
        }
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

/// How far `cct` sits between two calibration temperatures, in mireds, where 1
/// is fully the lower temperature.
///
/// Shared by the colour and forward matrices so the two cannot interpolate
/// differently — which would produce a profile that is self-consistent at each
/// calibration point and wrong everywhere between them.
fn mired_blend(low_cct: f32, high_cct: f32, cct: f32) -> f32 {
    let (a, b) = (1e6 / low_cct, 1e6 / high_cct);
    let m = 1e6 / cct.clamp(MIN_TEMPERATURE, MAX_TEMPERATURE);
    if (a - b).abs() < 1e-6 {
        return 0.0;
    }
    ((m - b) / (a - b)).clamp(0.0, 1.0)
}

fn multiply(a: &Matrix3, b: &Matrix3) -> Matrix3 {
    let mut out = [[0.0f32; 3]; 3];
    for (r, row) in out.iter_mut().enumerate() {
        for (c, cell) in row.iter_mut().enumerate() {
            *cell = (0..3).map(|k| a[r][k] * b[k][c]).sum();
        }
    }
    out
}

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
    fn sony_matrix() -> Matrix3 {
        [
            [0.6941, -0.2164, -0.0644],
            [-0.3850, 1.1349, 0.2779],
            [-0.0031, 0.1055, 0.6511],
        ]
    }

    fn sony_profile() -> CameraProfile {
        CameraProfile::from_color_matrix(sony_matrix())
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

    /// Rows sum to the D50 white point, which is what the DNG specification
    /// requires of a forward matrix and what makes neutral survive it.
    const FORWARD_D50: Matrix3 = [
        [0.6000, 0.2500, 0.1142],
        [0.2500, 0.7000, 0.0500],
        [0.0100, 0.0400, 0.7749],
    ];

    #[test]
    fn a_forward_matrix_carries_neutral_through_to_neutral() {
        // The property that makes the forward-matrix path usable without any
        // normalisation of our own: its rows sum to D50 white, so balanced
        // camera (1,1,1) lands on the D50 white point, and the connection
        // matrix takes that to sRGB (1,1,1).
        //
        // If this drifts, every image gets a cast that looks like a
        // white-balance error — the same failure the row normalisation exists
        // to prevent on the other path, arriving by a different route.
        let mut profile = CameraProfile::from_color_matrix(sony_matrix());
        profile.set_forward_matrix(6504.0, FORWARD_D50);
        assert!(profile.has_forward_matrix());

        let display = apply(&profile.camera_to_display(5000.0), [1.0, 1.0, 1.0]);
        for (c, v) in display.iter().enumerate() {
            assert!(
                (v - 1.0).abs() < 0.01,
                "balanced neutral rendered {display:?}; channel {c} is not neutral"
            );
        }
    }

    #[test]
    fn forward_matrices_stay_paired_with_their_illuminant() {
        // The trap this guards: construction sorts calibrations by temperature,
        // so a caller passing matrices in file order can end up with the
        // tungsten forward matrix paired to the daylight colour matrix. The
        // result is colour that is subtly wrong everywhere and correct nowhere,
        // with nothing to point at.
        let tungsten = [[0.9, 0.0, 0.0642], [0.0, 1.0, 0.0], [0.0, 0.0, 0.8249]];
        let daylight = FORWARD_D50;

        // Built with the illuminants the "wrong" way round, so the constructor
        // sorts them and any positional pairing would be inverted.
        let mut profile =
            CameraProfile::from_dual_illuminant((6504.0, sony_matrix()), (2856.0, sony_matrix()));
        profile.set_forward_matrix(6504.0, daylight);
        profile.set_forward_matrix(2856.0, tungsten);

        // At the daylight end the daylight matrix must dominate.
        let at_daylight = profile.camera_to_display(6504.0);
        let mut expected = CameraProfile::from_color_matrix(sony_matrix());
        expected.set_forward_matrix(6504.0, daylight);
        let reference = expected.camera_to_display(6504.0);
        for r in 0..3 {
            for c in 0..3 {
                assert!(
                    (at_daylight[r][c] - reference[r][c]).abs() < 1e-3,
                    "forward matrices are paired with the wrong illuminants"
                );
            }
        }
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
