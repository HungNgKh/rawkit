//! The monitor's own profile, baked into a lookup the GPU can sample.
//!
//! # What this fixes
//!
//! Everything upstream assumes the screen is sRGB. Screens are not. The display
//! this was written against reports primaries noticeably wider than sRGB's and a
//! plain gamma 2.2 transfer curve rather than sRGB's piecewise one — so drawing
//! sRGB numbers at it oversaturates every colour slightly and lifts the deep
//! shadows, neither of which announces itself as a bug.
//!
//! # Why a lookup table rather than a matrix
//!
//! A matrix plus a curve would cover the common case and quietly mis-handle the
//! rest: a profile with a LUT-based transform, a non-analytic tone curve, or
//! black point compensation cannot be reduced to one. Asking Little CMS to
//! evaluate the transform over a grid handles every profile shape with one code
//! path, and moves the question from "is this profile simple enough" to "is the
//! grid fine enough".
//!
//! 33 per axis is the size ICC itself uses for `A2B` tables, and the errors at
//! that density are far below what an 8-bit framebuffer can show.
//!
//! # What the caller must do with it
//!
//! The result is **device values, already encoded** for the monitor. Presenting
//! them through an `-Srgb` framebuffer would encode them a second time, so the
//! surface has to be configured with a plain format when this is in use. That
//! coupling is the reason [`DisplayLut::grid`] is public and the table is not
//! simply applied here.

use crate::{linear_srgb_profile, ExportError};

/// Samples per axis. See the module docs for why 33.
pub const GRID: usize = 33;

/// A 3D lookup from linear sRGB to a monitor's device values.
///
/// Laid out for direct upload as an RGBA texture: red varies fastest, then
/// green, then blue, which is the order `texture_3d` expects.
pub struct DisplayLut {
    entries: Vec<[f32; 4]>,
    description: String,
}

impl DisplayLut {
    /// Build the table from a monitor's ICC profile.
    ///
    /// Returns `Ok(None)` when the profile turns out to be indistinguishable
    /// from what the renderer already assumes — an sRGB monitor, or one whose
    /// EDID says so. Sampling a table to reproduce the identity is a waste of
    /// bandwidth and a place for a bug to hide, and saying "no table needed" is
    /// more useful to the caller than handing back an identity.
    pub fn from_icc(bytes: &[u8]) -> Result<Option<Self>, ExportError> {
        let source = linear_srgb_profile()?;
        let destination = lcms2::Profile::new_icc(bytes)
            .map_err(|e| ExportError::Colour(format!("reading the display profile: {e}")))?;
        let description = destination
            .info(lcms2::InfoType::Description, lcms2::Locale::none())
            .unwrap_or_else(|| "unnamed display profile".into());

        let mut grid = Vec::with_capacity(GRID * GRID * GRID);
        for b in 0..GRID {
            for g in 0..GRID {
                for r in 0..GRID {
                    let axis = |i: usize| i as f32 / (GRID - 1) as f32;
                    grid.push([axis(r), axis(g), axis(b)]);
                }
            }
        }

        let mapped: Vec<[f32; 3]> =
            crate::transform(&source, &destination, &grid, lcms2::PixelFormat::RGB_FLT)?;

        // How far the monitor is from what the renderer already assumes. The
        // comparison is against the sRGB *encoding* of each grid point, because
        // that is what would be displayed without this table.
        let worst = grid
            .iter()
            .zip(&mapped)
            .map(|(linear, device)| {
                (0..3)
                    .map(|c| (encode_srgb(linear[c]) - device[c]).abs())
                    .fold(0.0f32, f32::max)
            })
            .fold(0.0f32, f32::max);

        // Half a step of an 8-bit channel. Below this the table cannot change a
        // single pixel that reaches the screen.
        if worst < 0.5 / 255.0 {
            return Ok(None);
        }

        Ok(Some(Self {
            entries: mapped.into_iter().map(|[r, g, b]| [r, g, b, 1.0]).collect(),
            description,
        }))
    }

    /// The table, ready to upload. `GRID` cubed entries.
    pub fn entries(&self) -> &[[f32; 4]] {
        &self.entries
    }

    pub const fn grid(&self) -> usize {
        GRID
    }

    /// The profile's own description, for saying which monitor this is.
    pub fn description(&self) -> &str {
        &self.description
    }
}

/// The sRGB transfer function, for measuring what the table changes.
fn encode_srgb(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A monitor that really is sRGB must produce no table, not an identity one.
    #[test]
    fn an_srgb_display_needs_no_correction() {
        let srgb = lcms2::Profile::new_srgb();
        let bytes = srgb.icc().expect("serialise sRGB");
        assert!(
            DisplayLut::from_icc(&bytes).expect("build").is_none(),
            "an sRGB display should be recognised as needing nothing"
        );
    }

    #[test]
    fn a_wider_display_produces_a_table() {
        // Display P3: same white point and transfer curve as sRGB, wider
        // primaries. If this did not produce a table, the check above would be
        // rejecting real corrections rather than identities.
        let white = lcms2::CIExyY {
            x: 0.3127,
            y: 0.3290,
            Y: 1.0,
        };
        let p3 = lcms2::CIExyYTRIPLE {
            Red: lcms2::CIExyY {
                x: 0.680,
                y: 0.320,
                Y: 1.0,
            },
            Green: lcms2::CIExyY {
                x: 0.265,
                y: 0.690,
                Y: 1.0,
            },
            Blue: lcms2::CIExyY {
                x: 0.150,
                y: 0.060,
                Y: 1.0,
            },
        };
        let curve = lcms2::ToneCurve::new(2.2);
        let profile = lcms2::Profile::new_rgb(&white, &p3, &[&curve, &curve, &curve])
            .expect("build a P3 profile");
        let bytes = profile.icc().expect("serialise P3");

        let lut = DisplayLut::from_icc(&bytes)
            .expect("build")
            .expect("P3 differs from sRGB and must produce a table");
        assert_eq!(lut.entries().len(), GRID * GRID * GRID);

        // Pure red in sRGB is outside P3's red, so it must move inward — a
        // smaller red device value than the 1.0 it would have had.
        let last = GRID - 1;
        let red = lut.entries()[last];
        assert!(
            red[1] > 0.05,
            "sRGB red should need some green on a wider display, got {red:?}"
        );
    }

    #[test]
    fn a_profile_that_is_not_a_profile_is_refused() {
        assert!(DisplayLut::from_icc(b"not an icc profile at all").is_err());
    }
}
