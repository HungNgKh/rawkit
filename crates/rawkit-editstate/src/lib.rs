//! `EditState` — the canonical description of how one image is rendered.
//!
//! # Why this crate exists, and why it is first
//!
//! `EditState` is **ours**, not Lightroom's. Its fields are defined by what our
//! WGSL renderer does, never by another application's serialisation format. That
//! single property is what lets the rest of the project change direction cheaply:
//! renderers, UIs, import/export backends and (later) models all read and write
//! this type, so none of them are coupled to each other.
//!
//! Two rules that keep it that way:
//!
//! 1. **Never pass edits around as anything else.** Not as prose, not as another
//!    app's field names, not as a bag of floats.
//! 2. **Never let a foreign schema leak in.** Importers translate *into* this type
//!    at the boundary and are allowed to be lossy; nothing downstream should be
//!    able to tell where an `EditState` came from.
//!
//! # Scope right now
//!
//! Only the parameters the renderer actually honours: white balance, the tone
//! block, and the geometry — orientation and crop.
//! Fields are added as the renderer learns to honour them — an `EditState` field
//! that nothing renders is a lie the whole codebase has to keep.

pub mod geometry;
pub mod groups;

pub use geometry::{DistortionMap, Geometry};
pub use groups::Group;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Bumped whenever the meaning of an existing field changes, or a field is
/// removed. Adding an optional field with a `Default` does not require a bump.
///
/// Persisted alongside every stored `EditState`, so a future version can always
/// tell what it is reading.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum EditStateError {
    #[error(
        "unsupported EditState schema version {found} (this build understands {SCHEMA_VERSION})"
    )]
    UnsupportedVersion { found: u32 },
    #[error("serialisation failed: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("crop is not a rectangle: {0}")]
    InvalidCrop(String),
    #[error("detail is out of range: {0}")]
    InvalidDetail(String),
    #[error("lens correction is out of range: {0}")]
    InvalidLens(String),
    #[error("a local adjustment is not usable: {0}")]
    InvalidMask(String),
    #[error("{0} local adjustments, and {MAX_MASKS} is the most a frame may carry")]
    TooManyMasks(usize),
    #[error("an effect is out of range: {0}")]
    InvalidEffects(String),
    #[error("spot is not usable: {0}")]
    InvalidSpot(String),
    #[error("more spots than a photograph may carry: {0}")]
    TooManySpots(usize),
    #[error("colour is out of range: {0}")]
    InvalidColour(String),
    #[error("hue mixer is out of range: {0}")]
    InvalidHsl(String),
    #[error("tone curve is not usable: {0}")]
    InvalidCurve(String),
    #[error("colour grading is out of range: {0}")]
    InvalidGrade(String),
}

/// How the image should be rendered. `Default` is the identity edit: the photo
/// as the camera recorded it, with no adjustment applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditState {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub white_balance: WhiteBalance,
    #[serde(default)]
    pub tone: Tone,
    #[serde(default)]
    pub orientation: Orientation,
    #[serde(default)]
    pub crop: Crop,
    #[serde(default)]
    pub detail: Detail,
    #[serde(default)]
    pub lens: Lens,
    #[serde(default)]
    pub colour: Colour,
    #[serde(default)]
    pub hsl: Hsl,
    #[serde(default)]
    pub curve: Curve,
    #[serde(default)]
    pub grade: Grade,
    /// The vignette and the grain, which go on after everything else.
    #[serde(default)]
    pub effects: Effects,
    /// Local adjustments: what changes, and where.
    #[serde(default)]
    pub masks: Vec<Mask>,
    /// Blemishes to cover, and where to borrow the cover from.
    ///
    /// Not a mask, and deliberately not stored as one: a mask says *where* an
    /// adjustment applies and the renderer composites it in scene-linear light,
    /// while a spot replaces sensor data before the demosaic has run. They are
    /// two different kinds of thing that happen to both be round.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spots: Vec<Spot>,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

impl Default for EditState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            white_balance: WhiteBalance::default(),
            tone: Tone::default(),
            orientation: Orientation::default(),
            crop: Crop::default(),
            masks: Vec::new(),
            spots: Vec::new(),
            detail: Detail::default(),
            lens: Lens::default(),
            colour: Colour::default(),
            hsl: Hsl::default(),
            curve: Curve::default(),
            grade: Grade::default(),
            effects: Effects::default(),
        }
    }
}

impl EditState {
    /// Stable content hash, used as the cache key for rendered previews.
    ///
    /// Stability matters more than speed here: this value is written to disk and
    /// compared against on later runs, possibly by a later build.
    ///
    /// It is derived from the serialised form, so **adding a field rebuilds
    /// every cached preview once**, even for photographs whose rendering did not
    /// change. (This comment used to claim the opposite; adding `crop` is what
    /// showed it was wrong.) That is the right trade: the alternative is
    /// omitting defaults from the JSON, which would make the schema — shared
    /// artifact #1, consumed from outside this workspace — inconsistent about
    /// which fields exist, to save a rebuild that happens once per release.
    pub fn content_hash(&self) -> String {
        let canonical = serde_json::to_vec(self).expect("EditState is always serialisable");
        blake3::hash(&canonical).to_hex().to_string()
    }

    /// Reject states this build cannot faithfully render, rather than silently
    /// rendering them wrongly. A wrong render that looks plausible is worse than
    /// a refusal, because the user cannot tell it happened.
    pub fn validate(&self) -> Result<(), EditStateError> {
        if self.schema_version > SCHEMA_VERSION {
            return Err(EditStateError::UnsupportedVersion {
                found: self.schema_version,
            });
        }
        self.crop.validate()?;
        self.detail.validate()?;
        self.lens.validate()?;
        if self.masks.len() > MAX_MASKS {
            return Err(EditStateError::TooManyMasks(self.masks.len()));
        }
        for mask in &self.masks {
            mask.validate()?;
        }
        if self.spots.len() > MAX_SPOTS {
            return Err(EditStateError::TooManySpots(self.spots.len()));
        }
        for spot in &self.spots {
            spot.validate()?;
        }
        self.colour.validate()?;
        self.hsl.validate()?;
        self.curve.validate()?;
        self.grade.validate()?;
        self.effects.validate()?;
        Ok(())
    }

    /// The JSON Schema for this type — shared artifact #1, consumed by the
    /// (later) Python lab so both sides derive from one definition.
    pub fn json_schema() -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(EditState))
            .expect("schema is always serialisable")
    }
}

/// White balance as the user thinks about it. Converted to channel multipliers
/// by the renderer, using the camera profile — never stored as multipliers,
/// which would bake in a specific camera's calibration.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WhiteBalance {
    /// Correlated colour temperature in Kelvin. `None` means "as shot": use the
    /// camera's own recorded WB. This is deliberately distinct from any specific
    /// numeric value, because "as shot" varies per file.
    pub temperature_k: Option<f32>,
    /// Green/magenta axis. 0.0 is neutral.
    pub tint: f32,
}

impl Default for WhiteBalance {
    fn default() -> Self {
        Self {
            temperature_k: None,
            tint: 0.0,
        }
    }
}

/// The largest number of local adjustments one photograph may carry.
///
/// A cap rather than an open list, because the renderer holds their gains in a
/// uniform and their masks in a fixed set of texture layers — both sized once
/// when the image opens, because a render must not allocate. Eight is well past
/// what a photograph needs and small enough that the arrays cost nothing.
pub const MAX_MASKS: usize = 8;

/// How far the local temperature control reaches, in mireds at full deflection.
///
/// Mireds and not Kelvin. A thousand Kelvin at 3000 K is a different picture
/// from a thousand Kelvin at 9000 K, and a control that did one thing at one end
/// of its range and another at the other is not a control. The reciprocal scale
/// is near enough perceptually uniform that the same number means the same shift
/// wherever the global temperature sits — which is why the profile's own inverse
/// search runs in mireds too.
pub const LOCAL_MIRED_REACH: f32 = 50.0;

/// How far the local tint control reaches, in the units [`WhiteBalance::tint`]
/// uses.
pub const LOCAL_TINT_REACH: f32 = 40.0;

/// The most points one brush mask may carry, over all its strokes.
///
/// Not a limit anyone paints into: thinning bounds a stroke by its length, so
/// reaching this would take a mask covered many times over. It is here because
/// an `EditState` can arrive from a file, and a file can say anything — and the
/// thing on the other side of this number is a rasteriser that would sit there
/// for minutes.
pub const MAX_BRUSH_POINTS: usize = 20_000;

/// Where a local adjustment applies.
///
/// One shape so far. The renderer does not know about shapes at all — it is
/// handed a raster and composites it — so a second one is a rasteriser here and
/// nothing there, which is the arrangement that lets a future mask come from
/// somewhere other than a drawing.
/// Not `Copy`, and that is the brush's doing: a painted mask carries its strokes
/// and a stroke carries its points. Everything that holds a shape clones it,
/// which happens when an edit changes and not when a pixel is drawn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MaskShape {
    /// A gradient running from full effect at `from` to none at `to`.
    ///
    /// Both are fractions of the **unoriented, uncropped sensor frame**, which
    /// is the only frame that does not move when the picture is turned or
    /// trimmed. Stored in the displayed frame instead, a mask would slide across
    /// the photograph the moment somebody adjusted the crop.
    ///
    /// Beyond `from` the effect is full and beyond `to` it is absent, so a
    /// gradient covers the whole frame rather than a band across the middle of
    /// it — which is what a graduated filter in front of a lens does.
    Linear { from: [f32; 2], to: [f32; 2] },
    /// An ellipse: full effect inside, fading to none at its edge.
    ///
    /// `centre` and `radii` are fractions of the same sensor frame, and the two
    /// radii are independent — so a circle drawn on a 3:2 photograph is stored
    /// as two *different* fractions, which is what makes it come back a circle
    /// rather than an egg.
    ///
    /// `feather` is how much of the radius the fade occupies, from 0 for a hard
    /// edge to 1 for a falloff that begins at the very centre.
    Radial {
        centre: [f32; 2],
        radii: [f32; 2],
        feather: f32,
        /// Clockwise, in degrees, about the ellipse's own centre.
        ///
        /// Applied in **pixels** rather than in the fractions the radii are
        /// stored as. A rotation of a shape whose axes are scaled differently is
        /// not a rotation — it is a shear — so an ellipse turned in the
        /// normalised frame would come out the wrong shape on any photograph
        /// that is not square.
        #[serde(default)]
        angle_deg: f32,
    },
    /// Painted by hand: a list of strokes, applied in the order they were made.
    ///
    /// The *strokes* are stored and not the picture they make. A raster would
    /// not fit in the JSON an edit is, would not survive being applied to a
    /// different size, and could not be undone a stroke at a time. Redrawing it
    /// from the list costs a few milliseconds and buys all three.
    ///
    /// `feather` is shared by every dab, like a radial's — a brush whose
    /// softness changed from stroke to stroke would be a brush nobody could
    /// keep track of.
    Brush { strokes: Vec<Stroke>, feather: f32 },
    /// Selected by what the light *is* rather than by where it is.
    ///
    /// The first source that is not geometry, and the one the mask array was
    /// built to accept: a band on a per-pixel quantity, drawn into the same
    /// raster as a gradient and composited by a shader that still cannot tell
    /// them apart. An AI matte will arrive the same way.
    ///
    /// The band runs `from` to `to` in the channel's own units, with `feather`
    /// the width of the roll-off at each edge in those units. On [`RangeChannel::Hue`]
    /// a band with `from` above `to` wraps through red, which is the only way to
    /// select reds at all.
    Range {
        channel: RangeChannel,
        from: f32,
        to: f32,
        feather: f32,
    },
}

/// What a [`MaskShape::Range`] is a band of.
///
/// Two, and both are read off the guide in the camera's own RGB — see
/// `rawkit_engine::mask` for why the selection is made before the profile
/// rather than after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RangeChannel {
    /// How much light, 0 to 1, normalised to the sensor's own range.
    Luminance,
    /// Which light, 0 to 360 degrees, and circular.
    Hue,
}

/// Whether a shape is a shape at all, whichever part of a mask it is.
///
/// Lifted out of [`Mask::validate`] when a mask stopped being one shape: a
/// subtracted ellipse with a negative radius is exactly as unusable as a base
/// one, and a second copy of these rules would be a second place for them to
/// drift.
fn validate_shape(shape: &MaskShape) -> Result<(), EditStateError> {
    let finite = |v: f32| v.is_finite();
    match *shape {
        MaskShape::Linear { from, to } => {
            if !from.iter().chain(&to).copied().all(finite) {
                return Err(EditStateError::InvalidMask(format!(
                    "a gradient runs from {from:?} to {to:?}, which is not a place"
                )));
            }
            if from == to {
                return Err(EditStateError::InvalidMask(
                    "a gradient whose ends are the same point has no direction".into(),
                ));
            }
        }
        MaskShape::Brush {
            ref strokes,
            feather,
        } => {
            if !finite(feather) || !(0.0..=1.0).contains(&feather) {
                return Err(EditStateError::InvalidMask(format!(
                    "feather is {feather}, and runs from 0 to 1"
                )));
            }
            let total: usize = strokes.iter().map(|s| s.points.len()).sum();
            if total > MAX_BRUSH_POINTS {
                return Err(EditStateError::InvalidMask(format!(
                    "{total} brush points, and {MAX_BRUSH_POINTS} is the most a mask may carry"
                )));
            }
            for stroke in strokes {
                if !finite(stroke.radius) || stroke.radius <= 0.0 || stroke.radius > 1.0 {
                    return Err(EditStateError::InvalidMask(format!(
                        "a brush radius of {} is not a width",
                        stroke.radius
                    )));
                }
                if !stroke.points.iter().flatten().copied().all(finite) {
                    return Err(EditStateError::InvalidMask(
                        "a brush stroke passes through somewhere that is not a place".into(),
                    ));
                }
            }
        }
        MaskShape::Range {
            channel,
            from,
            to,
            feather,
        } => {
            if !finite(from) || !finite(to) || !finite(feather) {
                return Err(EditStateError::InvalidMask(format!(
                    "a range of {from} to {to} feathered by {feather} is not a band"
                )));
            }
            let full = match channel {
                RangeChannel::Luminance => 1.0,
                RangeChannel::Hue => 360.0,
            };
            for (name, v) in [("from", from), ("to", to)] {
                if !(0.0..=full).contains(&v) {
                    return Err(EditStateError::InvalidMask(format!(
                        "a range's {name} is {v}, and this channel runs from 0 to {full}"
                    )));
                }
            }
            // Only hue is circular, so only hue may run backwards. On
            // luminance an inverted band is a mistake with a plausible
            // meaning, which is the worst kind to accept silently.
            if channel == RangeChannel::Luminance && from > to {
                return Err(EditStateError::InvalidMask(format!(
                    "a luminance range runs from {from} down to {to}, and luminance does not wrap"
                )));
            }
            if !(0.0..=full).contains(&feather) {
                return Err(EditStateError::InvalidMask(format!(
                    "a range's feather is {feather}, and runs from 0 to {full}"
                )));
            }
        }
        MaskShape::Radial {
            centre,
            radii,
            feather,
            angle_deg,
        } => {
            if !centre.iter().chain(&radii).copied().all(finite) || !finite(feather) {
                return Err(EditStateError::InvalidMask(format!(
                    "an ellipse at {centre:?} of {radii:?} is not a shape"
                )));
            }
            if radii[0] <= 0.0 || radii[1] <= 0.0 {
                return Err(EditStateError::InvalidMask(format!(
                    "an ellipse needs two radii above zero, not {radii:?}"
                )));
            }
            if !(0.0..=1.0).contains(&feather) {
                return Err(EditStateError::InvalidMask(format!(
                    "feather is {feather}, and runs from 0 to 1"
                )));
            }
            // Unbounded but finite: an ellipse turned by 400 degrees is the same
            // ellipse, so there is nothing here to refuse except a number that
            // is not one.
            if !finite(angle_deg) {
                return Err(EditStateError::InvalidMask(
                    "an ellipse turned by something that is not a number".into(),
                ));
            }
        }
    }
    Ok(())
}

/// One more shape, and what it does to what is already there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Refinement {
    pub op: MaskOp,
    pub shape: MaskShape,
}

/// How a refinement joins what came before it.
///
/// The three of fuzzy set theory, and deliberately those: a mask weight is a
/// membership in `[0, 1]`, so union is the larger of the two, intersection the
/// smaller, and complement is one minus. Multiplying instead would be defensible
/// for a single step and wrong over three — an intersection of three shapes that
/// all cover a texel fully would come out at less than full, and the selection
/// would fade as it was refined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MaskOp {
    /// Wherever either covers: the larger of the two weights.
    Add,
    /// Take this shape away: the smaller of what is there and what this does not
    /// cover.
    Subtract,
    /// Only where both cover: the smaller of the two weights.
    Intersect,
}

/// How many shapes one local adjustment may be built from, beyond its first.
///
/// Bounded for the same reason everything else here is: a mask is redrawn from
/// its parts on every change, and an edit is JSON somebody's catalog has to
/// hold. Eight is past the point where a person can still say what a selection
/// means.
pub const MAX_REFINEMENTS: usize = 8;

/// One pass of the brush.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Stroke {
    /// Where the hand went, in fractions of the sensor frame.
    ///
    /// Thinned as they are recorded: a point landing within a third of a radius
    /// of the last one kept is dropped, because the dabs overlap many times over
    /// and the difference is not visible. That bounds a stroke by its *length*
    /// rather than by how slowly it was drawn — which matters because undo
    /// stores a whole `EditState` per step, so an unthinned stroke would be
    /// carried again by every step in the history.
    pub points: Vec<[f32; 2]>,
    /// Half the brush's width, as a fraction of the frame's **longest** edge.
    ///
    /// The longest edge and not each axis, so a round brush stays round: a
    /// radius stored per axis would paint ellipses on anything but a square
    /// photograph.
    pub radius: f32,
    /// Whether this stroke takes away what earlier ones put down.
    ///
    /// Per stroke rather than per mask, because that is what makes the order
    /// meaningful: paint, erase the overshoot, paint again.
    #[serde(default)]
    pub erase: bool,
}

/// One local adjustment: a region, and what to do inside it.
///
/// Both controls are scene-referred multiplies, which is why they are these two
/// and not others: the mask composites before the tone map, and these are the
/// operations that belong there. A local contrast or a local clarity lives on
/// the far side of that boundary and needs the mask carried across it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Mask {
    pub shape: MaskShape,
    /// What else the mask is made of, applied to the base shape in order.
    ///
    /// Optional and empty by default, which is what lets it arrive without a
    /// schema bump: an edit written before this existed reads back as a mask of
    /// one shape and renders exactly as it did.
    ///
    /// **In order, and not as a set.** A subtraction followed by an addition is
    /// not the same picture as the addition followed by the subtraction, and a
    /// person building a selection up out of parts is thinking in steps. A flat
    /// set would have to pick one of those meanings and would surprise them half
    /// the time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refinements: Vec<Refinement>,
    /// Swap what the mask covers for what it does not.
    ///
    /// On the mask rather than on the shape, deliberately. It is a fact about
    /// the *weight*, so one rule serves every source: it turns a radial into a
    /// vignette, reverses a gradient, and one day will take the background of a
    /// segmentation matte instead of its subject. A shape-specific "outside"
    /// flag would have to be invented again for each of those.
    #[serde(default)]
    pub invert: bool,
    /// Stops, inside the mask.
    pub exposure_ev: f32,
    /// Warm or cool, -1 to 1. See [`LOCAL_MIRED_REACH`].
    pub warmth: f32,
    /// Green or magenta, -1 to 1. Positive is magenta, matching
    /// [`WhiteBalance::tint`].
    pub tint: f32,

    // The three below are display-referred, and that is the whole reason they
    // are separate fields rather than more of the same. Exposure and white
    // balance are multiplies in scene-linear light, so the mask composites them
    // before the tone map; contrast, saturation and clarity are statements about
    // the *picture*, so the mask has to be carried across the tone map to reach
    // them. Optional with defaults, so an edit stored before they existed reads
    // back unchanged.
    /// About the mask's own midtone, -1 to 1.
    #[serde(default)]
    pub contrast: f32,
    /// Distance from grey, -1 to 1. Negative to grey out, positive to lift.
    #[serde(default)]
    pub saturation: f32,
    /// Local contrast at the scale of a neighbourhood, -1 to 1 — the control
    /// people mean by "texture" or "clarity".
    #[serde(default)]
    pub clarity: f32,
}

impl Default for Mask {
    fn default() -> Self {
        Self {
            // Across the top of the frame, running down: the graduated filter
            // somebody reaches for first, and a placement that is visible
            // straight away rather than one that has to be found.
            shape: MaskShape::Linear {
                from: [0.5, 0.0],
                to: [0.5, 0.35],
            },
            refinements: Vec::new(),
            invert: false,
            exposure_ev: 0.0,
            warmth: 0.0,
            tint: 0.0,
            contrast: 0.0,
            saturation: 0.0,
            clarity: 0.0,
        }
    }
}

impl Mask {
    /// The largest exposure a local adjustment may carry, in stops.
    pub const EXPOSURE_REACH: f32 = 4.0;

    /// Whether this adjustment would change anything at all.
    ///
    /// A mask with every control at zero still costs a texture layer and a
    /// sample, so the renderer skips it — and a user placing a gradient before
    /// touching a slider sees nothing happen, which is correct and is why the
    /// window draws the placement itself rather than relying on the picture.
    pub fn is_identity(&self) -> bool {
        self.exposure_ev == 0.0
            && self.warmth == 0.0
            && self.tint == 0.0
            && self.contrast == 0.0
            && self.saturation == 0.0
            && self.clarity == 0.0
    }

    fn validate(&self) -> Result<(), EditStateError> {
        let finite = |v: f32| v.is_finite();
        validate_shape(&self.shape)?;
        if self.refinements.len() > MAX_REFINEMENTS {
            return Err(EditStateError::InvalidMask(format!(
                "{} shapes past the first, and {MAX_REFINEMENTS} is the most one adjustment may be built from",
                self.refinements.len()
            )));
        }
        for refinement in &self.refinements {
            validate_shape(&refinement.shape)?;
        }
        if !finite(self.exposure_ev) || self.exposure_ev.abs() > Self::EXPOSURE_REACH {
            return Err(EditStateError::InvalidMask(format!(
                "local exposure is {}, and runs to {} stops",
                self.exposure_ev,
                Self::EXPOSURE_REACH
            )));
        }
        for (name, v) in [
            ("warmth", self.warmth),
            ("tint", self.tint),
            ("contrast", self.contrast),
            ("saturation", self.saturation),
            ("clarity", self.clarity),
        ] {
            if !finite(v) || !(-1.0..=1.0).contains(&v) {
                return Err(EditStateError::InvalidMask(format!(
                    "local {name} is {v}, and runs from -1 to 1"
                )));
            }
        }
        Ok(())
    }
}

/// The most spots one photograph may carry.
///
/// A sensor with sixty-four visible dust marks wants cleaning, not retouching.
/// The number is here for the same reason [`MAX_MASKS`] is — an `EditState` can
/// arrive from a file, and a file can say anything — but unlike masks these cost
/// nothing on the GPU, so the cap is loose rather than structural.
pub const MAX_SPOTS: usize = 64;

/// How a spot fills itself in.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SpotMode {
    /// Take the source's texture and the destination's light level.
    ///
    /// The default, and what "remove this dust mark" means: the surroundings
    /// decide how bright the patch is, so a source borrowed from a slightly
    /// different part of the sky does not arrive as a disc of the wrong blue.
    #[default]
    Heal,
    /// Take the source exactly.
    ///
    /// For when the destination's own light level is the thing being replaced —
    /// a blown highlight, a lens flare — where matching it would reproduce the
    /// problem.
    Clone,
}

/// A blemish, and where to borrow the pixels that replace it.
///
/// # Why the coordinates are fractions of the sensor frame
///
/// The same reason [`MaskShape`]'s are: it is the only frame that does not move
/// when the photograph is turned or trimmed. A spot stored in the displayed
/// frame would slide off the dust mark the moment somebody adjusted the crop,
/// which is exactly when they are looking closely enough to place one.
///
/// # Why one radius and not two
///
/// A `MaskShape::Radial` carries two, so that a circle drawn on a 3:2 frame
/// comes back a circle rather than an egg. A spot needs the opposite: it is a
/// circle in *sensor pixels*, because the thing it covers is a speck of dust
/// sitting on the sensor. Bayer photosites are square, so one fraction of the
/// width is the radius in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spot {
    /// What to cover.
    pub centre: [f32; 2],
    /// Where to take the replacement from.
    ///
    /// Stored rather than searched for at render time, and that is a deliberate
    /// decision rather than a cache. A search that ran during the render would
    /// make the same `EditState` render differently as the search improved, and
    /// "same RAW plus same EditState gives the same pixels" would stop being
    /// true without anything appearing to change. What the search finds is a
    /// *proposal*, made once when the spot is placed, which the user can then
    /// drag somewhere better.
    pub source: [f32; 2],
    /// As a fraction of the frame's width.
    pub radius: f32,
    /// How much of the radius the edge fade occupies, 0 for a hard edge and 1
    /// for a falloff that begins at the centre. The same meaning it has on a
    /// radial mask.
    pub feather: f32,
    #[serde(default)]
    pub mode: SpotMode,
}

impl Default for Spot {
    fn default() -> Self {
        Self {
            centre: [0.5, 0.5],
            source: [0.5, 0.5],
            radius: 0.01,
            feather: 0.5,
            mode: SpotMode::default(),
        }
    }
}

impl Spot {
    /// The largest a spot may be, as a fraction of the width.
    ///
    /// A quarter of the frame. Past that the tool being reached for is not this
    /// one, and the annulus a heal measures its light level from would be
    /// most of the photograph.
    pub const MAX_RADIUS: f32 = 0.25;

    fn validate(&self) -> Result<(), EditStateError> {
        let finite = |v: [f32; 2]| v[0].is_finite() && v[1].is_finite();
        if !finite(self.centre) || !finite(self.source) {
            return Err(EditStateError::InvalidSpot(
                "a spot has a coordinate that is not a number".into(),
            ));
        }
        if !self.radius.is_finite() || self.radius <= 0.0 || self.radius > Self::MAX_RADIUS {
            return Err(EditStateError::InvalidSpot(format!(
                "spot radius is {}, and runs above 0 up to {}",
                self.radius,
                Self::MAX_RADIUS
            )));
        }
        if !self.feather.is_finite() || !(0.0..=1.0).contains(&self.feather) {
            return Err(EditStateError::InvalidSpot(format!(
                "spot feather is {}, and runs from 0 to 1",
                self.feather
            )));
        }
        Ok(())
    }
}

/// A lens's radial distortion, as the camera that took the photograph described
/// it.
///
/// # Where the numbers come from
///
/// Sony's maker note, deciphered — see `rawkit_decode::exif::distortion`, which
/// also records how the format was found and how far it is trusted. Nothing here
/// parses anything; this is the shape the answer arrives in.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Distortion {
    /// Sixteen knots outward from the optical centre.
    ///
    /// The first is zero because a lens does not distort in the middle of its
    /// own image, and the trailing zeros are padding rather than knots — a lens
    /// that used all sixteen would have a non-zero one at the end.
    pub knots: [i16; 16],
    /// How much of it to apply, 0 to 1.
    ///
    /// A slider rather than a switch because a correction is somebody's taste as
    /// well as a measurement: a little barrel on a portrait is often wanted, and
    /// halfway is a legitimate place to stand.
    pub amount: f32,
}

/// Walk a curve of `n` points spread evenly from the centre to the corner.
///
/// Linear between the knots, and flat outside them — a point past the corner is
/// a corner as far as a lens is concerned.
pub(crate) fn sample_curve(curve: &[f32], t: f32) -> f32 {
    if curve.len() < 2 {
        return 0.0;
    }
    let x = t.clamp(0.0, 1.0) * (curve.len() - 1) as f32;
    let i = (x as usize).min(curve.len() - 2);
    let f = x - i as f32;
    curve[i] * (1.0 - f) + curve[i + 1] * f
}

impl Distortion {
    /// What the stored numbers are a fraction of.
    ///
    /// **Measured, not published.** The curve was fitted against the camera's own
    /// JPEGs — four frames of a Sony 70-350 mm at 128, 284 and 350 mm, compared
    /// patch by patch against our render of the same RAW. The knots run out to
    /// the half-*diagonal*, which the same fit pins to within a percent.
    ///
    /// The data cannot separate a divisor of 16384 from one of about 17400: 1.5
    /// px rms against 1.2, on a 1616-pixel-wide render whose measurement floor is
    /// 0.6 px. Two to the fourteenth is chosen from that range because it is the
    /// sort of number a camera would store, and because at that value the
    /// correction lands on what the body did with no further rescaling.
    ///
    /// What 1.5 px rms means: about 5 px on a 24 MP frame, against a distortion
    /// whose own size is 4% of the radius — so **roughly 97% of it comes out**.
    /// The rest is measurement noise, one frame whose patches matched poorly, and
    /// the 0.4% the camera crops off its own JPEG, which is a thing it does to
    /// its output rather than part of the lens.
    pub const DIVISOR: f32 = 16384.0;

    /// The knots that are knots.
    ///
    /// Trailing zeros are padding. At least two come back, because one point is
    /// not a curve and the interpolation below would have nothing to walk.
    fn curve(&self) -> &[i16] {
        let mut end = self.knots.len();
        while end > 2 && self.knots[end - 1] == 0 {
            end -= 1;
        }
        &self.knots[..end]
    }

    /// How far out to read, for an output point `t` of the way to the corner.
    ///
    /// Below 1 everywhere, and that is the whole trick: the curve is shifted so
    /// its largest value sits at exactly 1, which means the correction never
    /// asks for a sample from outside the frame and so can never show a black
    /// edge. Any constant added to a radial curve is a pure zoom, so shifting it
    /// costs nothing but magnification — where the camera would instead crop.
    /// Choosing magnification is the safer default: a correction that quietly
    /// trimmed the frame would change what a crop means.
    pub fn scale_at(&self, t: f32) -> f32 {
        let (curve, used) = self.resolved();
        1.0 + sample_curve(&curve[..used as usize], t)
    }

    /// The curve as a renderer wants it: the amount already applied, the peak
    /// already subtracted, the divisor already divided out.
    ///
    /// Both the CPU resampler and the shader read *this* rather than the raw
    /// knots, so there is one place where the anchoring and the units are
    /// decided. Two copies of that arithmetic is how a preview ends up framing a
    /// photograph differently from the file it exports.
    pub fn resolved(&self) -> ([f32; 16], u32) {
        let curve = self.curve();
        let peak = f32::from(curve.iter().copied().max().unwrap_or(0));
        let mut out = [0.0f32; 16];
        for (slot, knot) in out.iter_mut().zip(curve) {
            *slot = self.amount * (f32::from(*knot) - peak) / Self::DIVISOR;
        }
        (out, curve.len() as u32)
    }

    /// Whether this would move a pixel at all.
    pub fn is_identity(&self) -> bool {
        self.amount == 0.0 || self.knots.iter().all(|k| *k == 0)
    }

    fn validate(&self) -> Result<(), EditStateError> {
        if !self.amount.is_finite() || !(0.0..=1.0).contains(&self.amount) {
            return Err(EditStateError::InvalidLens(format!(
                "distortion amount is {}, and runs from 0 to 1",
                self.amount
            )));
        }
        Ok(())
    }
}

/// The effects that go on last: a vignette, and grain.
///
/// # Why these are not the lens's
///
/// [`Lens::vignette`] undoes what the glass did, so it is a plain multiply in
/// scene-linear light about the *optical axis*. This is a decision about the
/// picture: it is centred on the **crop**, because a vignette that stayed where
/// the sensor was would sit off-centre the moment somebody trimmed one side —
/// and it *holds the highlights back* rather than scaling everything equally,
/// which is what makes it read as a lens rather than as a grey wash laid over
/// the corner.
///
/// The two do meet in the middle: a negative corner brightness and a positive
/// vignette both darken corners. They are still different operations, in
/// different light, about different centres, and a single control could only be
/// wrong about one of them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Effects {
    /// Negative darkens the corners, positive lifts them.
    #[serde(default)]
    pub vignette: f32,
    /// Where the falloff is half done, as a fraction of the way to a corner.
    #[serde(default = "half")]
    pub midpoint: f32,
    /// The shape it falls off along: -1 is a diamond, 0 an ellipse in the
    /// crop's own proportions, 1 a rectangle.
    ///
    /// The exponent of a superellipse, which is the one number that carries all
    /// three and everything between them.
    #[serde(default)]
    pub roundness: f32,
    /// How wide the transition is, as a fraction of the radius. Zero is a hard
    /// edge, which is a thing somebody might want and is never an accident.
    #[serde(default = "half")]
    pub feather: f32,
    /// How much grain to add, 0 to 1.
    ///
    /// Luminance only — the same amount to all three channels, so the pixel
    /// moves along the grey axis and nothing gains colour. That is what film
    /// grain looks like, and it is the rule capture sharpening already follows;
    /// per-channel noise reads as a high-ISO sensor, which is usually the thing
    /// grain is being added to disguise.
    #[serde(default)]
    pub grain: f32,
    /// How big a grain is, in **sensor pixels**.
    ///
    /// Sensor and not output pixels, and that is the whole reason it is a number
    /// rather than a constant: a 2000-pixel export and a full-resolution one
    /// would otherwise carry visibly different films, and the same edit would
    /// mean two things.
    #[serde(default = "two")]
    pub grain_size: f32,
}

fn half() -> f32 {
    0.5
}

fn two() -> f32 {
    2.0
}

impl Default for Effects {
    fn default() -> Self {
        Self {
            vignette: 0.0,
            midpoint: 0.5,
            roundness: 0.0,
            feather: 0.5,
            grain: 0.0,
            grain_size: 2.0,
        }
    }
}

impl Effects {
    /// The largest a grain may be, in sensor pixels.
    pub const MAX_GRAIN_SIZE: f32 = 12.0;

    /// Whether this leaves every pixel exactly where it found it.
    pub fn is_identity(&self) -> bool {
        self.vignette == 0.0 && self.grain == 0.0
    }

    pub fn validate(&self) -> Result<(), EditStateError> {
        for (name, value, range) in [
            ("vignette", self.vignette, -1.0..=1.0),
            ("roundness", self.roundness, -1.0..=1.0),
            ("midpoint", self.midpoint, 0.0..=1.0),
            ("feather", self.feather, 0.0..=1.0),
            ("grain", self.grain, 0.0..=1.0),
            ("grain size", self.grain_size, 0.5..=Self::MAX_GRAIN_SIZE),
        ] {
            if !value.is_finite() || !range.contains(&value) {
                return Err(EditStateError::InvalidEffects(format!(
                    "{name} is {value}, and runs from {} to {}",
                    range.start(),
                    range.end()
                )));
            }
        }
        Ok(())
    }
}

/// The tone block.
///
/// `exposure_ev` is applied in scene-linear light, before the tone map, and is
/// therefore a true stop adjustment. The remaining sliders are display-referred:
/// they parameterise operations that run *after* the tone map, which is why they
/// are unitless and clamped rather than physical.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Tone {
    /// Stops. Positive brightens.
    pub exposure_ev: f32,
    pub contrast: f32,
    /// Negative recovers, positive lifts.
    ///
    /// **Spatially adaptive**: which part of the curve a pixel gets is decided
    /// by how bright its *neighbourhood* is, not by its own value, and the
    /// result is applied as a gain so local contrast survives. Recovering a sky
    /// therefore leaves a face at the same brightness alone. The neighbourhood
    /// comes from `rawkit_engine::guide`, and the trade-off it makes is written
    /// down there.
    pub highlights: f32,
    /// Positive lifts, negative deepens. Spatially adaptive; see
    /// [`Tone::highlights`].
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    /// Contrast against the neighbourhood rather than against a fixed grey.
    ///
    /// The same operation the local one performs and against the same
    /// neighbourhood — `rawkit_engine::guide`, which spans about 190 image
    /// pixels — so the two agree where they overlap and a mask refines what the
    /// global slider did rather than arguing with it.
    #[serde(default)]
    pub clarity: f32,
    /// The same idea at a radius of a few pixels: fine detail, not local tone.
    ///
    /// A separate control and not a second clarity, because the two do visibly
    /// different things to a face — clarity hollows the cheeks, texture finds
    /// the pores — and one slider at a compromise radius does neither.
    #[serde(default)]
    pub texture: f32,
    /// Haze, undone by the strength of it rather than by more contrast.
    ///
    /// Runs in **scene-linear light**, before the tone map, and that placement is
    /// the whole reason it is not a contrast slider: haze is airlight added to
    /// the scene, `I = J·t + A·(1 - t)`, and that equation is about light. After
    /// a tone curve the numbers are no longer the light and subtracting `A` from
    /// them subtracts the wrong quantity — which shows up as the sky going grey
    /// while the foreground barely moves.
    #[serde(default)]
    pub dehaze: f32,
}

impl Default for Tone {
    fn default() -> Self {
        Self {
            exposure_ev: 0.0,
            contrast: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            clarity: 0.0,
            texture: 0.0,
            dehaze: 0.0,
        }
    }
}

/// The visible rectangle, as fractions of the oriented frame.
///
/// # Why fractions
///
/// A crop outlives the pixels it was drawn on. The same edit is applied to a
/// full-resolution export, to a 2560-pixel preview and to a thumbnail, and
/// eventually to a smart preview that is not the original size at all —
/// fractions mean one number is right for all of them, where pixel coordinates
/// would need a scale factor carried alongside and would be wrong the moment
/// somebody forgot it.
///
/// # Why it is in *oriented* coordinates
///
/// [`Orientation`] is applied first, then this. That is what makes rotating a
/// cropped photograph rotate the crop with it, which is what every editor does
/// and what a user expects — and it means the rectangle the interface drew is
/// the rectangle that gets stored, with no frame conversion in between to get
/// backwards.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Crop {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    /// Straighten, in degrees clockwise. Applied *after* [`Orientation`] and
    /// before the rectangle above is read, so the rectangle is always a
    /// rectangle in the frame the user is looking at.
    ///
    /// It lives here rather than beside `orientation` because the two are one
    /// decision: rotating by a fraction of a degree leaves empty corners, and
    /// the only thing that can keep them out of the picture is the crop. They
    /// are stored together because they are resolved together.
    ///
    /// Bounded to ±15°. Past that it is not straightening a horizon, and whole
    /// quarter turns are [`Orientation`]'s job.
    #[serde(default)]
    pub angle_deg: f32,
    /// Keystone: how far the frame is tilted away from the plane it is looking
    /// at, up and down.
    ///
    /// Positive magnifies the top, which is what corrects a photograph taken
    /// looking *up* at something — the common case by a wide margin, and the
    /// reason that is the direction the sign points.
    ///
    /// Here for the same reason `angle_deg` is, and more so — a projective warp
    /// leaves *larger* empty corners than a rotation, and the only thing that
    /// can keep them out of the picture is the rectangle above. Stored together
    /// because they are resolved together, by one fit.
    #[serde(default)]
    pub vertical: f32,
    /// The same, left and right: positive magnifies the left.
    #[serde(default)]
    pub horizontal: f32,
    /// Stretch, as a ratio: above 1 widens, below 1 heightens.
    ///
    /// The fourth control every editor puts beside the other three, and it is
    /// here rather than in the crop's edges because it is a *warp* — it moves
    /// what is inside the rectangle rather than choosing a different rectangle.
    #[serde(default = "one")]
    pub aspect: f32,
}

fn one() -> f32 {
    1.0
}

/// The largest straighten this is, in degrees.
pub const MAX_STRAIGHTEN_DEG: f32 = 15.0;

/// How far a keystone may be pushed.
///
/// The number is the projective denominator's swing across half the frame, so
/// 0.35 means the far edge is read from about a third nearer the centre than
/// the near one. Past that the correction is stretching a few rows of pixels
/// across most of the picture and no crop can hide what it costs — which is a
/// limit of the operation rather than a taste, and so a refusal rather than a
/// clamp.
pub const MAX_KEYSTONE: f32 = 0.35;

/// The most the frame may be stretched, and its reciprocal is the least.
pub const MAX_ASPECT: f32 = 1.5;

impl Default for Crop {
    /// The whole frame.
    fn default() -> Self {
        Self {
            left: 0.0,
            top: 0.0,
            right: 1.0,
            bottom: 1.0,
            angle_deg: 0.0,
            vertical: 0.0,
            horizontal: 0.0,
            aspect: 1.0,
        }
    }
}

impl Crop {
    /// Whether this is the whole frame, and so has nothing to do.
    pub fn is_full_frame(&self) -> bool {
        *self == Self::default()
    }

    /// Refused rather than clamped, unlike the tone sliders.
    ///
    /// A slider outside its range has an obvious nearest meaning. A rectangle
    /// whose right edge is left of its left edge does not: clamping it would
    /// invent a crop the user never asked for and render it as though they had.
    pub fn validate(&self) -> Result<(), EditStateError> {
        for (name, value) in [
            ("left", self.left),
            ("top", self.top),
            ("right", self.right),
            ("bottom", self.bottom),
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(EditStateError::InvalidCrop(format!(
                    "{name} is {value}, and every edge is a fraction from 0 to 1"
                )));
            }
        }
        if !self.angle_deg.is_finite() || self.angle_deg.abs() > MAX_STRAIGHTEN_DEG {
            return Err(EditStateError::InvalidCrop(format!(
                "straighten is {} degrees, and runs from -{MAX_STRAIGHTEN_DEG} to \
                 {MAX_STRAIGHTEN_DEG}; whole quarter turns are the orientation's job",
                self.angle_deg
            )));
        }
        for (name, value) in [("vertical", self.vertical), ("horizontal", self.horizontal)] {
            if !value.is_finite() || value.abs() > MAX_KEYSTONE {
                return Err(EditStateError::InvalidCrop(format!(
                    "{name} keystone is {value}, and runs from -{MAX_KEYSTONE} to {MAX_KEYSTONE}"
                )));
            }
        }
        if !self.aspect.is_finite() || !(1.0 / MAX_ASPECT..=MAX_ASPECT).contains(&self.aspect) {
            return Err(EditStateError::InvalidCrop(format!(
                "aspect is {}, and runs from {} to {MAX_ASPECT}",
                self.aspect,
                1.0 / MAX_ASPECT
            )));
        }
        if self.left >= self.right || self.top >= self.bottom {
            return Err(EditStateError::InvalidCrop(format!(
                "left {} must be less than right {}, and top {} less than bottom {}",
                self.left, self.right, self.top, self.bottom
            )));
        }
        Ok(())
    }
}

/// How saturated the photograph is, in two controls that are not the same knob.
///
/// Both run after the tone map, in `Stage::ColourAdjustments`, because
/// saturation is about the picture rather than about the light: doing it in
/// scene-linear would make the effect depend on exposure, and a colour that
/// changed when you brightened the frame is not a colour control.
///
/// Unlike sharpening, both default to zero. A demosaiced frame is soft as a
/// matter of physics and needs answering; there is no equivalent reason a
/// photograph arrives under-saturated, and a converter that quietly adds colour
/// is one whose output cannot be compared with anything.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Colour {
    /// Every colour, equally. -1 is grey; 1 is twice as far from it.
    pub saturation: f32,
    /// Saturation that moves colours **towards the middle of the range**:
    /// positive lifts the flat ones and leaves the vivid alone, negative pulls
    /// the vivid back and leaves the flat alone.
    ///
    /// That is what makes it usable where plain saturation is not — a sky can
    /// come up without the one red jacket in the frame turning to poster paint.
    /// Weighted by how saturated each pixel already is, which protects skin
    /// partly and by accident; protecting it *by hue* belongs with the per-band
    /// mixer, where the bands exist to be reasoned about.
    pub vibrance: f32,
}

impl Colour {
    pub fn validate(&self) -> Result<(), EditStateError> {
        for (name, value) in [("saturation", self.saturation), ("vibrance", self.vibrance)] {
            if !value.is_finite() || !(-1.0..=1.0).contains(&value) {
                return Err(EditStateError::InvalidColour(format!(
                    "{name} is {value}, and runs from -1 to 1"
                )));
            }
        }
        Ok(())
    }
}

/// Sharpening.
///
/// # Why this has a non-zero default
///
/// A demosaiced frame is soft by construction: two thirds of every pixel was
/// interpolated. Every raw converter answers that with capture sharpening, and
/// one that does not looks worse than its neighbours for a reason the user
/// cannot see and would not guess at. So the default is a real number, and
/// opening a photograph shows something worth looking at rather than something
/// that needs a slider found first.
///
/// **The cost is deliberate and worth naming**: `EditState::default()` is no
/// longer the identity, so a render of an unedited file is not the demosaic's
/// own output any more. The golden references were re-blessed once for it. What
/// remains true is the narrower claim that matters — `sharpen_amount` of zero
/// changes nothing at all, and the shader returns before touching a pixel.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Detail {
    /// How much of the difference between the image and its blur to add back.
    /// 0 is off; 1 is strong. Applied to luminance only, so it cannot introduce
    /// colour fringing along an edge.
    pub sharpen_amount: f32,
    /// The blur's radius in pixels, which sets what counts as detail. Small
    /// values sharpen texture; large ones sharpen shapes and start to look like
    /// clarity.
    pub sharpen_radius: f32,
    /// How far to smooth *colour* while leaving luminance alone. 0 is off.
    ///
    /// Chroma noise is the ugly kind: coloured blotches in the shadows that no
    /// amount of exposure fixes and that survive being printed. Smoothing colour
    /// costs nothing visible, because the eye takes its detail from luminance —
    /// which is why this has a default and [`Detail::luminance_noise`] does not.
    pub chroma_noise: f32,
    /// How much of the clipping cast to take off an edge that sits beside a
    /// blown highlight. 0 is off.
    ///
    /// # Why this exists, and why it has a default
    ///
    /// A sensor clips at one value; white balance moves that to a different
    /// height per channel, so a blown neutral sky arrives *magenta* and
    /// highlight reconstruction replaces it with neutral. A pixel on the edge of
    /// a bare twig is a mixture of that light with honest dark content — it
    /// carries the whole lie, scaled down, and reconstruction cannot see it
    /// because reconstruction asks about the pixel's own level and this pixel is
    /// nowhere near clipping. The result is a violet rim on every twig against a
    /// bright sky, and it is manufactured entirely by the pipeline: on a
    /// synthetic scene that is neutral everywhere in the truth, the cast is
    /// 0.000 with the sky just below the clip point and 2.229 at three times it.
    ///
    /// So it defaults on, for the same reason capture sharpening does: it
    /// repairs something the rendering itself introduces, and a converter that
    /// left it there would look worse than its neighbours for a reason the user
    /// cannot see. It is a slider rather than a switch because how much colour
    /// beside a highlight is *real* depends on the photograph — a sunset's cloud
    /// edge genuinely is orange.
    pub defringe: f32,
    /// How far to smooth *brightness*, sparing edges. 0 is off, and off is the
    /// default.
    ///
    /// The asymmetry with [`Detail::chroma_noise`] is the whole point. Smoothing
    /// colour takes nothing you can see; smoothing luminance takes detail,
    /// because luminance is where all of the detail is. What the right amount is
    /// depends on the ISO the frame was shot at and on whether you like grain,
    /// and neither is something a converter can decide for you — one that made
    /// that trade unasked would be softening photographs for a reason its user
    /// could not see.
    pub luminance_noise: f32,
}

impl Default for Detail {
    fn default() -> Self {
        Self {
            // Modest. Enough that a raw looks like a photograph rather than a
            // scan, and short of the amount that makes edges announce
            // themselves.
            sharpen_amount: 0.4,
            sharpen_radius: 1.0,
            // Modest, and safe to have on: it removes blotches and takes no
            // detail with them.
            chroma_noise: 0.5,
            // Off. See the field's own note: this one costs detail, and which
            // frames want it is not ours to assume.
            luminance_noise: 0.0,
            // Full, and that is a measurement rather than a preference. The
            // step is the one that leaves the least colour behind, so it cannot
            // overshoot however hard it is pushed — and what it costs a subject
            // that *is* magenta was measured rather than assumed: 2% of the
            // chroma at the middle of one, because the reach is a few pixels and
            // a dark pixel is bounded by how much blown light it could hold.
            // The slider is there for the photograph that is the exception.
            defringe: 1.0,
        }
    }
}

/// The largest sharpening radius the renderer will honour, in pixels.
///
/// The tile halo is sized for it: a neighbourhood operation inside a tile can
/// only read as far as the halo makes correct, so this number and `HALO` in the
/// engine move together.
pub const MAX_SHARPEN_RADIUS: f32 = 2.0;

impl Detail {
    /// Refused rather than clamped, like a crop and unlike a tone slider: a
    /// radius past what the halo covers would read demosaic output that is
    /// wrong near a tile edge, and the result is a faint grid nobody would
    /// attribute to sharpening.
    pub fn validate(&self) -> Result<(), EditStateError> {
        if !self.sharpen_amount.is_finite() || !(0.0..=1.0).contains(&self.sharpen_amount) {
            return Err(EditStateError::InvalidDetail(format!(
                "sharpen amount is {}, and runs from 0 to 1",
                self.sharpen_amount
            )));
        }
        if !self.sharpen_radius.is_finite()
            || !(0.1..=MAX_SHARPEN_RADIUS).contains(&self.sharpen_radius)
        {
            return Err(EditStateError::InvalidDetail(format!(
                "sharpen radius is {} pixels, and runs from 0.1 to {MAX_SHARPEN_RADIUS}",
                self.sharpen_radius
            )));
        }
        if !self.chroma_noise.is_finite() || !(0.0..=1.0).contains(&self.chroma_noise) {
            return Err(EditStateError::InvalidDetail(format!(
                "chroma noise reduction is {}, and runs from 0 to 1",
                self.chroma_noise
            )));
        }
        if !self.luminance_noise.is_finite() || !(0.0..=1.0).contains(&self.luminance_noise) {
            return Err(EditStateError::InvalidDetail(format!(
                "luminance noise reduction is {}, and runs from 0 to 1",
                self.luminance_noise
            )));
        }
        if !self.defringe.is_finite() || !(0.0..=1.0).contains(&self.defringe) {
            return Err(EditStateError::InvalidDetail(format!(
                "defringe is {}, and runs from 0 to 1",
                self.defringe
            )));
        }
        Ok(())
    }
}

/// The largest lateral chromatic aberration this build will apply, as a
/// fraction of the radius.
///
/// A fifth of a percent is already ten pixels at the corner of a 24 megapixel
/// frame, which is far past any lens a photographer would keep. Past it the
/// number did not come from a lens, and applying it would put fringing into a
/// picture that did not have any.
///
/// It is also what the tile halo can afford: a displacement is a read from a
/// neighbour, and a read past the halo lands on demosaic output that is wrong
/// near a tile edge. The engine clamps to the halo as well, so this bound and
/// that one have to be reconciled rather than merely both true — see
/// `HALO` in `rawkit-engine`.
pub const MAX_LATERAL: f32 = 0.002;

/// Lens corrections.
///
/// # Why a measurement is stored in the edit
///
/// These numbers are measured from the photograph — see `rawkit_engine::aberration`
/// — and a measurement looks like a fact about the file rather than a decision
/// about it, which argues for carrying it beside the mosaic instead.
///
/// It cannot go there, and the reason is the invariant the engine exists to
/// protect: *same RAW + same `EditState` -> same pixels*. A measurement made by
/// a later build, or on a different machine, or from a different resolution
/// level, need not come back byte-identical — and if the renderer read it from
/// the frame, the same file and the same edit would quietly render differently.
/// Storing what was measured makes the render reproducible, the correction
/// undoable, and the number something a person can see and overrule.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Lens {
    /// How far red must be rescaled about the optical centre to line up with
    /// green, as a fraction of the radius.
    ///
    /// Operational rather than descriptive: this is what the *renderer*
    /// multiplies the radius by when sampling red, so a positive value reaches
    /// further out. Green is the reference because a Bayer sensor has twice as
    /// much of it, so its edges are the least invented.
    pub chromatic_red: f32,
    /// The same, for blue.
    pub chromatic_blue: f32,
    /// How much to lift the corners, to undo the lens's own falloff.
    ///
    /// Positive brightens them, negative darkens. A **plain multiply in
    /// scene-linear light**, because that is what a lens does: it attenuates,
    /// and undoing an attenuation is a division. The creative vignette in
    /// [`Effects`] is a different operation for a different reason — see there.
    ///
    /// Centred on the sensor and not on the crop. The falloff is about the
    /// optical axis, so a crop moves the picture and leaves the darkening where
    /// the lens put it.
    #[serde(default)]
    pub vignette: f32,
    /// The maker's own distortion curve for the lens that was mounted, and how
    /// much of it to apply. `None` is a photograph nothing is correcting.
    ///
    /// The curve is *stored in the edit* rather than re-read from the file at
    /// render time, and that is the same decision [`Lens::chromatic_red`]
    /// records for a different reason. Here it buys two things: a photograph
    /// whose body had no profile for the lens cannot silently acquire one when
    /// the parser improves, and an edit carried to another machine reproduces
    /// the same geometry without that machine having to read a maker note. It is
    /// sixteen numbers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distortion: Option<Distortion>,
}

impl Lens {
    /// Whether this leaves every pixel where it found it.
    ///
    /// Exact zeros, not a tolerance. The renderer skips the resampling entirely
    /// on this, and "close enough to zero to skip" and "small enough not to
    /// matter" are different questions that would drift apart.
    ///
    /// The distortion is deliberately *not* part of this: chromatic aberration
    /// is a rescale of two channels inside the develop kernel, and a distortion
    /// is a coordinate map that belongs with the crop and the straighten. The
    /// two are asked about by different code at different stages, and one
    /// answer for both would have to be the pessimistic one.
    pub fn is_identity(&self) -> bool {
        self.chromatic_red == 0.0 && self.chromatic_blue == 0.0 && self.vignette == 0.0
    }

    /// Refused rather than clamped, like a sharpening radius and for the same
    /// reason: a value past the halo does not soften the correction, it reads
    /// pixels the demosaic got wrong, and the result is a grid at the tile seams
    /// that nobody would blame on a lens correction.
    pub fn validate(&self) -> Result<(), EditStateError> {
        for (name, value) in [("red", self.chromatic_red), ("blue", self.chromatic_blue)] {
            if !value.is_finite() || value.abs() > MAX_LATERAL {
                return Err(EditStateError::InvalidLens(format!(
                    "{name} is scaled by {value}, and the range is +/-{MAX_LATERAL}"
                )));
            }
        }
        if !self.vignette.is_finite() || self.vignette.abs() > 1.0 {
            return Err(EditStateError::InvalidLens(format!(
                "corner brightness is {}, and runs from -1 to 1",
                self.vignette
            )));
        }
        if let Some(distortion) = &self.distortion {
            distortion.validate()?;
        }
        Ok(())
    }
}

/// One of the eight hue bands the mixer divides the colour circle into.
///
/// Eight, at these centres, because that is the division every photographer
/// already has in their hands — the same set and the same names Lightroom uses,
/// so a person arriving with an idea of what "orange" means finds it here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Band {
    Red,
    Orange,
    Yellow,
    Green,
    Aqua,
    Blue,
    Purple,
    Magenta,
}

impl Band {
    /// In the order they appear on the hue circle, which is also the order the
    /// weights in the shader are indexed by. The two must agree, and
    /// `the_bands_partition_the_hue_circle` is what notices if they stop.
    pub const ALL: [Band; 8] = [
        Band::Red,
        Band::Orange,
        Band::Yellow,
        Band::Green,
        Band::Aqua,
        Band::Blue,
        Band::Purple,
        Band::Magenta,
    ];

    /// Where this band sits on the hue circle, in degrees.
    ///
    /// Unevenly spaced on purpose: there is far more of the spectrum a person
    /// calls "green" than there is "orange", and evenly spaced centres would
    /// give the greens one control between them and the warm tones three.
    pub fn centre_deg(self) -> f32 {
        match self {
            Band::Red => 0.0,
            Band::Orange => 30.0,
            Band::Yellow => 60.0,
            Band::Green => 120.0,
            Band::Aqua => 180.0,
            Band::Blue => 240.0,
            Band::Purple => 280.0,
            Band::Magenta => 320.0,
        }
    }

    /// The two bands a hue lies between, and how much of each applies to it.
    ///
    /// The mirror of `band_span` in the shader, and it has to stay one: a
    /// targeted adjustment distributes a change across these weights so that the
    /// colour under the pointer receives all of it, and weights that disagreed
    /// with the renderer's would move the wrong sliders by the wrong amounts.
    /// `the_rust_weights_match_the_shader` is what holds them together.
    ///
    /// Weights sum to one by construction — the same partition property that
    /// makes the eight sliders seamless.
    pub fn spanning(hue_deg: f32) -> [(Band, f32); 2] {
        let hue = hue_deg.rem_euclid(360.0);
        for (index, &band) in Band::ALL.iter().enumerate() {
            let lower = band.centre_deg();
            // Red again, a turn later: the last span closes the circle.
            let upper = if index == 7 {
                360.0
            } else {
                Band::ALL[index + 1].centre_deg()
            };
            if hue >= lower && hue < upper {
                let t = (hue - lower) / (upper - lower);
                return [(band, 1.0 - t), (Band::ALL[(index + 1) % 8], t)];
            }
        }
        // Unreachable for a hue in [0, 360), which `rem_euclid` guarantees.
        [(Band::Red, 1.0), (Band::Orange, 0.0)]
    }

    pub fn index(self) -> usize {
        Band::ALL.iter().position(|b| *b == self).unwrap_or(0)
    }
}

/// Which of a band's three numbers a command means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BandControl {
    Hue,
    Saturation,
    Luminance,
}

/// The largest hue shift a band can be given, in degrees.
///
/// Thirty is a band's own width in the warm end of the circle, so at full
/// deflection a colour lands on its neighbour's centre and no further. Enough to
/// move a sky from cyan to blue; short of the range where a hue slider becomes a
/// way to make a photograph of something else.
pub const MAX_HUE_SHIFT_DEG: f32 = 30.0;

/// What one band's colours are asked to do. All three are -1 to 1, and zero
/// everywhere is the identity.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BandMix {
    /// Rotation around the hue circle, scaled by [`MAX_HUE_SHIFT_DEG`].
    pub hue: f32,
    /// Distance from grey, scaled by `1 + saturation`. The same measure the
    /// global saturation control uses, so the two compose predictably.
    pub saturation: f32,
    /// Brightness, scaled by `1 + luminance`, which leaves hue and saturation
    /// exactly where they were.
    pub luminance: f32,
}

impl BandMix {
    pub fn get(&self, control: BandControl) -> f32 {
        match control {
            BandControl::Hue => self.hue,
            BandControl::Saturation => self.saturation,
            BandControl::Luminance => self.luminance,
        }
    }

    pub fn set(&mut self, control: BandControl, value: f32) {
        match control {
            BandControl::Hue => self.hue = value,
            BandControl::Saturation => self.saturation = value,
            BandControl::Luminance => self.luminance = value,
        }
    }
}

/// The eight-band hue mixer.
///
/// Named fields rather than an array, for the same reason [`crate::Tone`] has
/// them: a stored edit should be readable, and `{"orange":{"saturation":-0.4}}`
/// says what was decided in a way `[[0,0,0],[0,-0.4,0]]` does not.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Hsl {
    pub red: BandMix,
    pub orange: BandMix,
    pub yellow: BandMix,
    pub green: BandMix,
    pub aqua: BandMix,
    pub blue: BandMix,
    pub purple: BandMix,
    pub magenta: BandMix,
}

impl Hsl {
    pub fn mix(&self, band: Band) -> BandMix {
        match band {
            Band::Red => self.red,
            Band::Orange => self.orange,
            Band::Yellow => self.yellow,
            Band::Green => self.green,
            Band::Aqua => self.aqua,
            Band::Blue => self.blue,
            Band::Purple => self.purple,
            Band::Magenta => self.magenta,
        }
    }

    pub fn set(&mut self, band: Band, mix: BandMix) {
        let slot = match band {
            Band::Red => &mut self.red,
            Band::Orange => &mut self.orange,
            Band::Yellow => &mut self.yellow,
            Band::Green => &mut self.green,
            Band::Aqua => &mut self.aqua,
            Band::Blue => &mut self.blue,
            Band::Purple => &mut self.purple,
            Band::Magenta => &mut self.magenta,
        };
        *slot = mix;
    }

    /// Whether every band is at zero, which lets the renderer skip the stage
    /// rather than multiply by one twenty-four times a pixel.
    pub fn is_identity(&self) -> bool {
        *self == Hsl::default()
    }

    pub fn validate(&self) -> Result<(), EditStateError> {
        for band in Band::ALL {
            let mix = self.mix(band);
            for (name, value) in [
                ("hue", mix.hue),
                ("saturation", mix.saturation),
                ("luminance", mix.luminance),
            ] {
                if !value.is_finite() || !(-1.0..=1.0).contains(&value) {
                    return Err(EditStateError::InvalidHsl(format!(
                        "{band:?} {name} is {value}, and runs from -1 to 1"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// One range's colour, as a hue and how much of it.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Tint {
    /// Degrees around the colour circle. Meaningless at zero saturation, and
    /// kept anyway so that turning saturation back up returns the hue you had
    /// rather than red.
    pub hue: f32,
    /// How far towards that hue, 0 to 1.
    pub saturation: f32,
    /// Brightness for this range alone, -1 to 1.
    pub luminance: f32,
}

/// Colour grading: a different tint for the shadows, the midtones and the
/// highlights.
///
/// The three weights **partition the luminance range** rather than overlapping
/// by taste — setting all three to one colour is a uniform tint by construction,
/// which is both the property that makes the control predictable and the test
/// that proves it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Grade {
    pub shadows: Tint,
    pub midtones: Tint,
    pub highlights: Tint,
    /// How gradually one range gives way to the next. 0 keeps them distinct,
    /// 1 lets them overlap broadly.
    pub blending: f32,
    /// Where the midtones sit between black and white, -1 to 1. Negative moves
    /// the midpoint down, so more of the picture counts as highlight.
    pub balance: f32,
}

impl Default for Grade {
    fn default() -> Self {
        Self {
            shadows: Tint::default(),
            midtones: Tint::default(),
            highlights: Tint::default(),
            blending: 0.5,
            balance: 0.0,
        }
    }
}

impl Grade {
    /// Whether anything is actually tinted. Blending and balance shape *where*
    /// the ranges are and mean nothing on their own, so a grade with no colour
    /// and no luminance offset is the identity whatever they say.
    pub fn is_identity(&self) -> bool {
        [self.shadows, self.midtones, self.highlights]
            .iter()
            .all(|t| t.saturation == 0.0 && t.luminance == 0.0)
    }

    pub fn validate(&self) -> Result<(), EditStateError> {
        for (name, tint) in [
            ("shadows", self.shadows),
            ("midtones", self.midtones),
            ("highlights", self.highlights),
        ] {
            if !tint.hue.is_finite() || !(0.0..=360.0).contains(&tint.hue) {
                return Err(EditStateError::InvalidGrade(format!(
                    "{name} hue is {}, and runs from 0 to 360 degrees",
                    tint.hue
                )));
            }
            if !tint.saturation.is_finite() || !(0.0..=1.0).contains(&tint.saturation) {
                return Err(EditStateError::InvalidGrade(format!(
                    "{name} saturation is {}, and runs from 0 to 1",
                    tint.saturation
                )));
            }
            if !tint.luminance.is_finite() || !(-1.0..=1.0).contains(&tint.luminance) {
                return Err(EditStateError::InvalidGrade(format!(
                    "{name} luminance is {}, and runs from -1 to 1",
                    tint.luminance
                )));
            }
        }
        if !self.blending.is_finite() || !(0.0..=1.0).contains(&self.blending) {
            return Err(EditStateError::InvalidGrade(format!(
                "blending is {}, and runs from 0 to 1",
                self.blending
            )));
        }
        if !self.balance.is_finite() || !(-1.0..=1.0).contains(&self.balance) {
            return Err(EditStateError::InvalidGrade(format!(
                "balance is {}, and runs from -1 to 1",
                self.balance
            )));
        }
        Ok(())
    }
}

/// The most control points a curve may carry.
///
/// A bound rather than a preference: the resampled curve rides in a GPU buffer
/// sized when the photograph is opened, and an unbounded list would be an
/// unbounded allocation driven by how many times somebody clicked. Sixteen is
/// past the point where a tone curve is a curve rather than a drawing.
pub const MAX_CURVE_POINTS: usize = 16;

/// The user's tone curve: a hand-shaped mapping from what the tone map produced
/// to what should be shown.
///
/// **Composite only** — one curve acting on all three channels together, so
/// shaping tone cannot shift colour. Per-channel curves are the same widget with
/// a channel selector and are not here yet.
///
/// Stored as control points rather than as a sampled curve, because the points
/// are what a person edited and a resampling is a derived thing. Interpolation
/// is the renderer's business; see `rawkit_engine::tone`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Curve {
    /// `[input, output]` pairs in `0..=1`, ordered by input.
    pub points: Vec<[f32; 2]>,
}

impl Default for Curve {
    fn default() -> Self {
        // The identity, written out rather than left empty: a curve editor needs
        // two ends to drag, and "no points" and "a straight line" would be two
        // representations of one thing.
        Self {
            points: vec![[0.0, 0.0], [1.0, 1.0]],
        }
    }
}

impl Curve {
    /// Whether this curve changes anything, so the renderer can skip it.
    pub fn is_identity(&self) -> bool {
        *self == Curve::default()
    }

    /// Refused rather than repaired. A curve whose inputs do not increase has no
    /// single answer at the repeated input, and quietly sorting somebody's
    /// points would move a control they were dragging.
    pub fn validate(&self) -> Result<(), EditStateError> {
        if self.points.len() < 2 {
            return Err(EditStateError::InvalidCurve(format!(
                "a curve needs at least two points, and this has {}",
                self.points.len()
            )));
        }
        if self.points.len() > MAX_CURVE_POINTS {
            return Err(EditStateError::InvalidCurve(format!(
                "{} points, and the most a curve may carry is {MAX_CURVE_POINTS}",
                self.points.len()
            )));
        }
        let mut previous = f32::NEG_INFINITY;
        for [x, y] in &self.points {
            if !x.is_finite() || !y.is_finite() {
                return Err(EditStateError::InvalidCurve(
                    "a point is not a finite number".into(),
                ));
            }
            if !(0.0..=1.0).contains(x) || !(0.0..=1.0).contains(y) {
                return Err(EditStateError::InvalidCurve(format!(
                    "({x}, {y}) is outside the unit square"
                )));
            }
            if *x <= previous {
                return Err(EditStateError::InvalidCurve(format!(
                    "input {x} does not come after {previous}; points run left to right"
                )));
            }
            previous = *x;
        }
        Ok(())
    }
}

/// Rotation in 90-degree steps, applied on top of the camera's recorded
/// orientation. Free rotation belongs to the crop module and is not this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    #[default]
    AsShot,
    Rotate90Cw,
    Rotate180,
    Rotate270Cw,
}

impl Orientation {
    /// Quarter-turns clockwise.
    pub fn turns(self) -> u32 {
        match self {
            Orientation::AsShot => 0,
            Orientation::Rotate90Cw => 1,
            Orientation::Rotate180 => 2,
            Orientation::Rotate270Cw => 3,
        }
    }

    /// The rotation that `turns` quarter-turns clockwise amounts to, wrapping.
    pub fn from_turns(turns: u32) -> Self {
        match turns % 4 {
            1 => Orientation::Rotate90Cw,
            2 => Orientation::Rotate180,
            3 => Orientation::Rotate270Cw,
            _ => Orientation::AsShot,
        }
    }

    /// This rotation applied after `first`.
    ///
    /// What makes `AsShot` mean *as shot*: the camera's recorded orientation is
    /// the first turn, and whatever the user asked for turns the result. So a
    /// portrait frame opens upright, and `[` still turns it a quarter from
    /// wherever it is rather than from the sensor's own axes.
    pub fn after(self, first: Orientation) -> Self {
        Orientation::from_turns(first.turns() + self.turns())
    }
}

/// Where an `EditState` came from.
///
/// This is not bookkeeping. From the moment the editor ships it is what makes a
/// future training set self-labelling: a `Model` proposal that a user then
/// corrects to a `User` state is exactly one supervised example, recorded without
/// anyone having to plan for it. Losing this column later cannot be backfilled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EditSource {
    #[default]
    User,
    Preset,
    Import,
    Model,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hue_is_shared_between_the_two_bands_it_lies_between() {
        // The number this exists for, measured off a photograph before the
        // function did: the lawn in `ILCE-6400_DSC00087.ARW` sits at 78.8
        // degrees, which is mostly Yellow and not, whatever it is called, Green.
        let [(first, a), (second, b)] = Band::spanning(78.8);
        assert_eq!(first, Band::Yellow);
        assert_eq!(second, Band::Green);
        assert!(
            (a - 0.687).abs() < 0.002,
            "the lawn should be about 69% yellow, got {a}"
        );
        assert!((a + b - 1.0).abs() < 1e-6, "the weights must partition");
    }

    #[test]
    fn every_hue_is_fully_accounted_for() {
        // The partition, across the whole circle including the seam at red.
        for step in 0..720 {
            let hue = step as f32 * 0.5;
            let [(_, a), (_, b)] = Band::spanning(hue);
            assert!(
                (a + b - 1.0).abs() < 1e-5,
                "hue {hue} splits {a} + {b}, which is not one"
            );
            assert!(a >= 0.0 && b >= 0.0, "hue {hue} gave a negative weight");
        }
    }

    #[test]
    fn a_hue_on_a_centre_belongs_wholly_to_that_band() {
        for band in Band::ALL {
            let [(first, weight), _] = Band::spanning(band.centre_deg());
            assert_eq!(first, band);
            assert!((weight - 1.0).abs() < 1e-6, "{band:?} got {weight}");
        }
    }

    #[test]
    fn a_hue_outside_the_circle_is_brought_back_onto_it() {
        // Hue arithmetic wraps, and a caller that has added a shift may hand
        // over 370 or -10 rather than normalising first.
        assert_eq!(Band::spanning(370.0), Band::spanning(10.0));
        assert_eq!(Band::spanning(-10.0), Band::spanning(350.0));
    }

    #[test]
    fn default_is_the_identity_edit() {
        let s = EditState::default();
        assert_eq!(s.tone.exposure_ev, 0.0);
        assert_eq!(
            s.white_balance.temperature_k, None,
            "default must be as-shot"
        );
        assert_eq!(s.orientation, Orientation::AsShot);
    }

    #[test]
    fn round_trips_through_json() {
        let mut s = EditState::default();
        s.tone.exposure_ev = -0.75;
        s.white_balance.temperature_k = Some(5200.0);

        let encoded = serde_json::to_string(&s).unwrap();
        let decoded: EditState = serde_json::from_str(&encoded).unwrap();
        assert_eq!(s, decoded);
    }

    #[test]
    fn hash_tracks_content_not_identity() {
        let a = EditState::default();
        let b = EditState::default();
        assert_eq!(a.content_hash(), b.content_hash());

        let mut c = EditState::default();
        c.tone.exposure_ev = 0.5;
        assert_ne!(a.content_hash(), c.content_hash(), "cache key must change");
    }

    #[test]
    fn future_versions_are_refused_not_guessed() {
        let s = EditState {
            schema_version: SCHEMA_VERSION + 1,
            ..Default::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        // Guards rule 2: a foreign schema must not leak in unnoticed.
        let json = r#"{"schema_version":1,"crs:Exposure2012":0.5}"#;
        assert!(serde_json::from_str::<EditState>(json).is_err());
    }

    #[test]
    fn schema_is_generatable() {
        let schema = EditState::json_schema();
        assert!(schema.get("properties").is_some());
    }
    #[test]
    fn an_edit_written_before_refinements_existed_still_reads() {
        // The whole reason `refinements` is optional with a default: an edit
        // stored by a build that had never heard of it must come back as a mask
        // of one shape and render exactly as it did. If this needs a schema
        // bump, it needs a migration, and a migration has to run on strangers'
        // catalogs for the fifteen months between the beta and 1.0.
        let json = r#"{
            "schema_version": 1,
            "masks": [{
                "shape": { "kind": "linear", "from": [0.5, 0.0], "to": [0.5, 0.4] },
                "invert": false,
                "exposure_ev": -1.0,
                "warmth": 0.0,
                "tint": 0.0
            }]
        }"#;
        let state: EditState =
            serde_json::from_str(json).expect("an edit from before this existed");
        assert_eq!(state.schema_version, SCHEMA_VERSION);
        assert!(state.masks[0].refinements.is_empty());
        state.validate().expect("and it is still a usable edit");

        // And it goes back out the way it came: an empty list is not written, so
        // a catalog full of unrefined masks does not grow a field apiece.
        let encoded = serde_json::to_string(&state).unwrap();
        assert!(
            !encoded.contains("refinements"),
            "an empty refinement list was written out: {encoded}"
        );
    }

    /// The Sony E 70-350 mm at 284 mm, from `DSC00775.ARW`.
    fn measured_lens() -> Distortion {
        Distortion {
            knots: [
                0, 8, 8, 24, 56, 100, 160, 236, 328, 440, 572, 724, 0, 0, 0, 0,
            ],
            amount: 1.0,
        }
    }

    #[test]
    fn the_curve_reproduces_what_the_camera_did() {
        // The acceptance test for the whole correction, and the numbers on the
        // right are not from a specification — there is none. They are the radial
        // displacement measured between this frame's own out-of-camera JPEG and
        // our render of the same RAW, on a 1616-pixel-wide comparison, averaged
        // over every patch that matched in each hundred-pixel band.
        //
        // The tolerance is a pixel and a half because that is where the fit
        // actually sits. The strongest correction measured — the same lens at
        // 128 mm — agrees to about four, and most of that gap is one systematic
        // percent that no choice of divisor removes from all four frames at once.
        let lens = measured_lens();
        let corner = (808.0f32 * 808.0 + 540.0 * 540.0).sqrt();
        for (radius, camera) in [
            (50.0f32, -2.02f32),
            (150.0, -6.45),
            (250.0, -10.58),
            (350.0, -13.45),
            (550.0, -17.47),
            (750.0, -15.03),
        ] {
            let ours = radius * (lens.scale_at(radius / corner) - 1.0);
            assert!(
                (ours - camera).abs() < 1.5,
                "at radius {radius} the camera moved the picture by {camera} and we move it by {ours}"
            );
        }
    }

    #[test]
    fn nothing_is_ever_read_from_outside_the_frame() {
        // The property that makes a black edge impossible, and it has to hold for
        // a lens that bows the other way too — so here is one that does.
        let barrel = Distortion {
            knots: [
                0, -8, -30, -70, -130, -210, -310, -430, 0, 0, 0, 0, 0, 0, 0, 0,
            ],
            amount: 1.0,
        };
        for lens in [measured_lens(), barrel] {
            for step in 0..=100 {
                let t = step as f32 / 100.0;
                let scale = lens.scale_at(t);
                assert!(
                    scale <= 1.0 + f32::EPSILON,
                    "at {t} of the way out the correction reads from {scale} of the radius"
                );
            }
            // And it reaches 1 somewhere, or the correction is cropping the frame
            // for no reason.
            let widest = (0..=100)
                .map(|s| lens.scale_at(s as f32 / 100.0))
                .fold(f32::MIN, f32::max);
            assert!((widest - 1.0).abs() < 1e-6, "the widest read is {widest}");
        }
    }

    #[test]
    fn an_amount_of_zero_changes_nothing_and_says_so() {
        let off = Distortion {
            amount: 0.0,
            ..measured_lens()
        };
        assert!(off.is_identity());
        for step in 0..=10 {
            assert_eq!(off.scale_at(step as f32 / 10.0), 1.0);
        }
        // Halfway is halfway, because a person is allowed to want some of it.
        let half = Distortion {
            amount: 0.5,
            ..measured_lens()
        };
        let (full, part) = (measured_lens().scale_at(0.0), half.scale_at(0.0));
        assert!(((1.0 - part) / (1.0 - full) - 0.5).abs() < 1e-5);
    }

    #[test]
    fn the_padding_at_the_end_is_not_a_knot() {
        // Twelve of the sixteen slots are used by this lens. Reading the zeros as
        // knots would bend the curve back to nothing at the corner, which is the
        // opposite of what it does and would show as the corners staying bent.
        let lens = measured_lens();
        let ours = lens.scale_at(1.0);
        assert!(
            (ours - 1.0).abs() < 1e-6,
            "the last real knot is the widest read, and it gave {ours}"
        );
        // A lens that genuinely used all sixteen keeps all sixteen.
        let full = Distortion {
            knots: [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
            amount: 1.0,
        };
        assert!(full.scale_at(1.0) > full.scale_at(0.9));
    }

    #[test]
    fn an_edit_written_before_the_perspective_existed_still_reads() {
        // `aspect` is the field that makes this worth pinning: it defaults to
        // *one* rather than to zero, and a serde default that fell back to zero
        // would turn every stored crop into a divide by nothing. Every edit
        // anybody has is one of these.
        let json = r#"{
            "schema_version": 1,
            "crop": { "left": 0.1, "top": 0.2, "right": 0.9, "bottom": 0.8, "angle_deg": 3.0 }
        }"#;
        let state: EditState = serde_json::from_str(json).expect("an edit from before this");
        assert_eq!(state.crop.aspect, 1.0);
        assert_eq!(state.crop.vertical, 0.0);
        assert_eq!(state.crop.horizontal, 0.0);
        state.validate().expect("and it is still a usable edit");
    }

    #[test]
    fn a_keystone_past_what_the_map_can_carry_is_refused() {
        for bad in [MAX_KEYSTONE * 1.5, -MAX_KEYSTONE * 1.5, f32::NAN] {
            let state = EditState {
                crop: Crop {
                    vertical: bad,
                    ..Crop::default()
                },
                ..EditState::default()
            };
            assert!(
                state.validate().is_err(),
                "a keystone of {bad} was accepted"
            );
        }
        // And an aspect of zero, which is the one that would divide by nothing
        // rather than merely look wrong.
        for bad in [0.0, -1.0, MAX_ASPECT * 2.0, f32::NAN] {
            let state = EditState {
                crop: Crop {
                    aspect: bad,
                    ..Crop::default()
                },
                ..EditState::default()
            };
            assert!(state.validate().is_err(), "an aspect of {bad} was accepted");
        }
    }

    #[test]
    fn an_edit_written_before_spots_existed_still_reads() {
        // The same contract `refinements` has, and it matters more here: every
        // edit anybody has stored so far predates this field, and a catalog that
        // needed a migration to open would need one that runs on strangers'
        // catalogs for the fifteen months between the beta and 1.0.
        let json = r#"{"schema_version": 1, "tone": {"exposure_ev": 0.5, "contrast": 0.0,
            "highlights": 0.0, "shadows": 0.0, "whites": 0.0, "blacks": 0.0}}"#;
        let state: EditState = serde_json::from_str(json).expect("an edit from before this");
        assert!(state.spots.is_empty());
        state.validate().expect("and it is still a usable edit");

        // And an edit with no blemishes does not grow a field, so the content
        // hash of every stored edit is what it was.
        let encoded = serde_json::to_string(&state).unwrap();
        assert!(
            !encoded.contains("spots"),
            "an empty spot list was written: {encoded}"
        );
        assert_eq!(
            state.content_hash(),
            EditState {
                tone: state.tone,
                ..EditState::default()
            }
            .content_hash()
        );
    }

    #[test]
    fn a_spot_round_trips_and_keeps_its_mode() {
        let state = EditState {
            spots: vec![Spot {
                centre: [0.4, 0.6],
                source: [0.5, 0.6],
                radius: 0.01,
                feather: 0.25,
                mode: SpotMode::Clone,
            }],
            ..EditState::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: EditState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, state);
        // Named in the JSON rather than numbered: the schema is read from
        // outside this workspace, and `"mode": 1` would mean nothing there.
        assert!(json.contains(r#""mode":"clone""#), "{json}");
    }

    #[test]
    fn a_spot_with_no_size_is_refused() {
        // A radius of zero covers nothing and a negative one is not a shape.
        // Refused rather than clamped, for the reason every other refusal here
        // exists: a render that quietly did nothing looks exactly like a render
        // that worked.
        for bad in [0.0, -0.01, f32::NAN, Spot::MAX_RADIUS * 2.0] {
            let state = EditState {
                spots: vec![Spot {
                    radius: bad,
                    ..Spot::default()
                }],
                ..EditState::default()
            };
            assert!(state.validate().is_err(), "a radius of {bad} was accepted");
        }
        let state = EditState {
            spots: vec![Spot::default(); MAX_SPOTS + 1],
            ..EditState::default()
        };
        assert!(matches!(
            state.validate(),
            Err(EditStateError::TooManySpots(_))
        ));
    }

    #[test]
    fn a_refinement_is_held_to_the_same_rules_as_a_base_shape() {
        // The reason shape validation was lifted out of `Mask::validate`. A
        // subtracted ellipse with no radius is exactly as unusable as a base
        // one, and the rule must not have two copies to drift between.
        let mut state = EditState {
            masks: vec![Mask {
                refinements: vec![Refinement {
                    op: MaskOp::Subtract,
                    shape: MaskShape::Radial {
                        centre: [0.5, 0.5],
                        radii: [0.0, 0.2],
                        feather: 0.5,
                        angle_deg: 0.0,
                    },
                }],
                ..Mask::default()
            }],
            ..EditState::default()
        };
        let why = state
            .validate()
            .expect_err("a radius of zero is not a shape");
        println!("refused with: {why}");

        state.masks[0].refinements = (0..=MAX_REFINEMENTS)
            .map(|_| Refinement {
                op: MaskOp::Add,
                shape: MaskShape::Linear {
                    from: [0.0, 0.0],
                    to: [1.0, 1.0],
                },
            })
            .collect();
        let why = state
            .validate()
            .expect_err("one more than the most is too many");
        println!("refused with: {why}");
    }
}
