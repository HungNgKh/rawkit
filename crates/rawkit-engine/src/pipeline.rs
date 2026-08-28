//! The pipeline stage order, as a type.
//!
//! Every op in the editor belongs to exactly one stage, and the stages run in
//! the order declared here. Writing it down as an enum rather than as prose in a
//! design document means the ordering can be asserted by a test — and the one
//! ordering fact that actually matters (what is before the tone map and what is
//! after) is checked rather than remembered.
//!
//! ```text
//! decode → demosaic → lens/CA → white balance → camera profile →
//! scene-linear ops → local adjustments → TONE MAP →
//! display-referred ops → curve/HSL/grading → look → output transform → export
//! ```
//!
//! Two rules carried from the design and enforced below:
//!
//! - **Masks are textures composited at [`Stage::LocalAdjustments`].** A brush
//!   stroke, a luminance range, a depth band and (later) a segmentation mask all
//!   enter identically. Building that stage generically is the one place v1 pays
//!   a small cost for a v2 capability, and it is worth it: retrofitting a mask
//!   source into a shader designed for brushes only is a rewrite.
//! - **Exactly one tone map, at a fixed position.** Everything before it is
//!   scene-linear and physical; everything after is display-referred and
//!   unitless. Sliders that feel display-referred to the user may still
//!   parameterise ops before it — that is the design, not a contradiction.

/// The light domain a stage operates in. This is what makes the difference
/// between "exposure" (a stop, in linear light) and "highlights" (a unitless
/// nudge, after the tone map) a property of the code rather than a convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    /// Sensor data: mosaiced or freshly demosaiced, not yet colour-managed.
    Sensor,
    /// Scene-linear light. Physical units; exposure is a multiply here.
    SceneLinear,
    /// The tone map itself — the boundary between the two worlds.
    ToneMap,
    /// After the tone map. Unitless, clamped, taste-driven.
    DisplayReferred,
    /// Leaving the engine: ICC transform, resize, output sharpening.
    Output,
}

/// A pipeline stage. Declaration order *is* execution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// LibRaw hands back the sensor mosaic (`rawkit-decode`).
    Decode,
    /// RCD, ported to WGSL. The P0 go/no-go spike.
    Demosaic,
    /// Distortion, vignette, chromatic aberration, defringe.
    LensCorrection,
    /// Channel multipliers, from `EditState` or the camera's as-shot values —
    /// and, with them, highlight reconstruction.
    ///
    /// Reconstruction belongs here and not with the other scene-linear ops,
    /// which is where the design first placed it. Clipping is a fact about the
    /// sensor, so it is only legible while the channels are still the sensor's
    /// own; one matrix multiply later they are mixed and there is no longer any
    /// such thing as "the green channel clipped".
    WhiteBalance,
    /// DCP: camera matrix, forward matrix, HSL look table, tone curve.
    CameraProfile,
    /// Exposure and noise reduction. Highlight reconstruction was expected here
    /// and is not — see [`Stage::WhiteBalance`].
    SceneLinearOps,
    /// Mask textures composited. Generic by construction — see module docs.
    LocalAdjustments,
    /// Fixed sigmoid. The boundary.
    ToneMap,
    /// Contrast, highlights/shadows, clarity, dehaze, texture.
    DisplayReferredOps,
    /// Tone curve, HSL / colour mixer, colour grading.
    ColourAdjustments,
    /// LUT / look application. v2's taste layer lands here.
    Look,
    /// lcms2 ICC transform to the display or export profile.
    OutputTransform,
    /// Resize and output sharpening, on export only.
    Export,
}

impl Stage {
    /// Every stage, in execution order.
    pub const ALL: [Stage; 13] = [
        Stage::Decode,
        Stage::Demosaic,
        Stage::LensCorrection,
        Stage::WhiteBalance,
        Stage::CameraProfile,
        Stage::SceneLinearOps,
        Stage::LocalAdjustments,
        Stage::ToneMap,
        Stage::DisplayReferredOps,
        Stage::ColourAdjustments,
        Stage::Look,
        Stage::OutputTransform,
        Stage::Export,
    ];

    pub fn domain(self) -> Domain {
        match self {
            Stage::Decode | Stage::Demosaic => Domain::Sensor,
            Stage::LensCorrection
            | Stage::WhiteBalance
            | Stage::CameraProfile
            | Stage::SceneLinearOps
            | Stage::LocalAdjustments => Domain::SceneLinear,
            Stage::ToneMap => Domain::ToneMap,
            Stage::DisplayReferredOps | Stage::ColourAdjustments | Stage::Look => {
                Domain::DisplayReferred
            }
            Stage::OutputTransform | Stage::Export => Domain::Output,
        }
    }

    /// Whether this stage runs when producing a screen preview.
    ///
    /// Preview and export share kernels; they differ in resolution, tiling and
    /// this one predicate. Export-only work (resize, output sharpening) must not
    /// leak into the interactive path, or preview and export stop matching.
    pub fn runs_in_preview(self) -> bool {
        self != Stage::Export
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declaration_order_is_execution_order() {
        let mut sorted = Stage::ALL;
        sorted.sort();
        assert_eq!(sorted, Stage::ALL, "ALL must be in pipeline order");
    }

    #[test]
    fn there_is_exactly_one_tone_map() {
        let count = Stage::ALL
            .iter()
            .filter(|s| s.domain() == Domain::ToneMap)
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn the_two_light_domains_do_not_interleave() {
        // The invariant that makes "exposure is a stop" and "highlights is a
        // nudge" different kinds of thing: everything scene-linear happens
        // before the tone map, everything display-referred after it.
        let tone_map = Stage::ALL
            .iter()
            .position(|s| *s == Stage::ToneMap)
            .expect("pipeline has a tone map");

        for (i, stage) in Stage::ALL.iter().enumerate() {
            match stage.domain() {
                Domain::Sensor | Domain::SceneLinear => assert!(
                    i < tone_map,
                    "{stage:?} is scene-linear but runs after the tone map"
                ),
                Domain::DisplayReferred | Domain::Output => assert!(
                    i > tone_map,
                    "{stage:?} is display-referred but runs before the tone map"
                ),
                Domain::ToneMap => assert_eq!(i, tone_map),
            }
        }
    }

    #[test]
    fn masks_composite_before_the_tone_map() {
        // Local adjustments act on scene-linear light so that a mask carrying an
        // exposure change behaves like exposure does globally.
        assert_eq!(Stage::LocalAdjustments.domain(), Domain::SceneLinear);
        assert!(Stage::LocalAdjustments < Stage::ToneMap);
    }

    #[test]
    fn only_export_stages_are_export_only() {
        for stage in Stage::ALL {
            assert_eq!(stage.runs_in_preview(), stage != Stage::Export);
        }
    }
}
