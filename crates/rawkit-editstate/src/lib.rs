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
//! Only the parameters P0 actually renders: white balance and the tone block.
//! Fields are added as the renderer learns to honour them — an `EditState` field
//! that nothing renders is a lie the whole codebase has to keep.

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
        }
    }
}

impl EditState {
    /// Stable content hash, used as the cache key for rendered previews.
    ///
    /// Stability matters more than speed here: this value is written to disk and
    /// compared against on later runs, possibly by a later build. It is derived
    /// from the serialised form so that adding a field with a default does not
    /// invalidate every cached preview in an existing catalogue.
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
