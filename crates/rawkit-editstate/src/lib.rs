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

pub use geometry::Geometry;

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
    #[error("colour is out of range: {0}")]
    InvalidColour(String),
    #[error("hue mixer is out of range: {0}")]
    InvalidHsl(String),
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
            detail: Detail::default(),
            colour: Colour::default(),
            hsl: Hsl::default(),
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
        self.colour.validate()?;
        self.hsl.validate()?;
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
    pub highlights: f32,
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
