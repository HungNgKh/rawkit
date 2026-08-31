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

pub use geometry::Geometry;
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
    #[error("a local adjustment is not usable: {0}")]
    InvalidMask(String),
    #[error("{0} local adjustments, and {MAX_MASKS} is the most a frame may carry")]
    TooManyMasks(usize),
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
    pub colour: Colour,
    #[serde(default)]
    pub hsl: Hsl,
    #[serde(default)]
    pub curve: Curve,
    #[serde(default)]
    pub grade: Grade,
    /// Local adjustments: what changes, and where.
    #[serde(default)]
    pub masks: Vec<Mask>,
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
            detail: Detail::default(),
            colour: Colour::default(),
            hsl: Hsl::default(),
            curve: Curve::default(),
            grade: Grade::default(),
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
        if self.masks.len() > MAX_MASKS {
            return Err(EditStateError::TooManyMasks(self.masks.len()));
        }
        for mask in &self.masks {
            mask.validate()?;
        }
        self.colour.validate()?;
        self.hsl.validate()?;
        self.curve.validate()?;
        self.grade.validate()?;
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
}

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
            invert: false,
            exposure_ev: 0.0,
            warmth: 0.0,
            tint: 0.0,
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
        self.exposure_ev == 0.0 && self.warmth == 0.0 && self.tint == 0.0
    }

    fn validate(&self) -> Result<(), EditStateError> {
        let finite = |v: f32| v.is_finite();
        match self.shape {
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
            MaskShape::Radial {
                centre,
                radii,
                feather,
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
            }
        }
        if !finite(self.exposure_ev) || self.exposure_ev.abs() > Self::EXPOSURE_REACH {
            return Err(EditStateError::InvalidMask(format!(
                "local exposure is {}, and runs to {} stops",
                self.exposure_ev,
                Self::EXPOSURE_REACH
            )));
        }
        for (name, v) in [("warmth", self.warmth), ("tint", self.tint)] {
            if !finite(v) || !(-1.0..=1.0).contains(&v) {
                return Err(EditStateError::InvalidMask(format!(
                    "local {name} is {v}, and runs from -1 to 1"
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
}

/// The largest straighten this is, in degrees.
pub const MAX_STRAIGHTEN_DEG: f32 = 15.0;

impl Default for Crop {
    /// The whole frame.
    fn default() -> Self {
        Self {
            left: 0.0,
            top: 0.0,
            right: 1.0,
            bottom: 1.0,
            angle_deg: 0.0,
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
}
