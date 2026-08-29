//! RAW decoding: file on disk → sensor data the engine can demosaic.
//!
//! # Why this is its own crate
//!
//! Two reasons, and the licensing one is the reason it is a *hard* boundary
//! rather than a tidy one.
//!
//! 1. **Licence quarantine.** LibRaw is used in CDDL mode. CDDL is file-level
//!    copyleft: those files stay CDDL permanently and can never be relicensed
//!    under the project's Apache-2.0. Keeping every line that links them inside
//!    this crate means the obligation has a known blast radius — and that the
//!    decoder can be swapped (`rawler`, or a DNG-only path) without the rest of
//!    the workspace noticing.
//! 2. **Decoding is not rendering.** Everything here is camera-specific and
//!    ugly; everything downstream is generic and mathematical. The type below is
//!    the seam.
//!
//! # What is not here
//!
//! No demosaic. This crate hands back the sensor mosaic exactly as recorded,
//! with the metadata needed to interpret it. Demosaic is stage B in
//! `rawkit-engine` and runs on the GPU.
//!
//! The binding lives in [`libraw`]; everything below is the shape it produces,
//! which is the part other crates depend on.

pub mod exif;
pub mod libraw;

pub use libraw::{decode_file, read_metadata};

/// Failures that decoding can produce. Every variant is something a user can
/// hit with a real file, which is why "unsupported camera" is a first-class
/// case rather than a generic error: it has a specific answer (run the file
/// through Adobe DNG Converter), and the UI is expected to say so.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("not a RAW file, or a container this build does not recognise")]
    UnrecognisedFormat,
    #[error("camera not supported by this build: {make} {model} — convert to DNG and re-import")]
    UnsupportedCamera { make: String, model: String },
    #[error("RAW data is truncated or corrupt")]
    Corrupt,
    #[error("io error: {0}")]
    Io(String),
}

/// The colour filter array layout, given as the top-left 2x2 block.
///
/// Stored as the layout rather than as a per-camera name so the demosaic kernel
/// stays camera-agnostic. `XTrans` is deliberately a distinct variant and not a
/// pattern: it is 6x6 and needs a different kernel entirely, so code that only
/// handles Bayer must fail to compile rather than fail at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CfaPattern {
    Rggb,
    Bggr,
    Grbg,
    Gbrg,
    XTrans,
}

impl CfaPattern {
    /// Whether the RCD demosaic kernel (stage B) can handle this layout.
    ///
    /// RCD is Bayer-only. X-Trans needs Markesteijn or similar, which is not in
    /// P0 scope — Sony ARW is the P0 target and is Bayer.
    pub fn is_bayer(self) -> bool {
        !matches!(self, CfaPattern::XTrans)
    }
}

/// Per-channel sensor levels, needed before any linear maths means anything.
///
/// Black is subtracted and white is what full scale divides by; getting either
/// wrong shows up as a colour cast in the shadows or clipped highlights that
/// are not actually clipped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SensorLevels {
    pub black: [u16; 4],
    pub white: u16,
}

/// Camera identity, carried through to profile selection and to the catalog.
///
/// `serial` is here because the catalog's duplicate-detection heuristic is
/// `(capture_time, camera_serial, shutter_count)` — it is not decoration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraId {
    pub make: String,
    pub model: String,
    pub serial: Option<String>,
}

/// What a catalog needs from a file, without decoding a single pixel.
///
/// Reading this costs a header parse rather than a decompression — milliseconds
/// against roughly a second — which is why a scan can afford it per file and
/// hashing cannot.
///
/// Every field but the camera is optional, because every one of them is
/// something a particular body may simply not record. A missing value is stored
/// as missing; nothing here is guessed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawMetadata {
    pub camera: CameraId,
    /// The visible sensor area, the same numbers [`RawImage`] reports.
    ///
    /// Here so a catalogue can set up a viewport for a photograph it has not
    /// decoded — which is the whole point of showing a cached preview.
    pub width: u32,
    pub height: u32,
    /// When the photograph was taken, as the camera's own clock read it.
    ///
    /// **This is a wall clock, not an instant.** EXIF capture times carry no
    /// timezone, so the value here is the camera's local date and time
    /// *interpreted as if it were UTC* — the same convention every photo
    /// catalogue uses, and the only one that gives the same number on a laptop
    /// that has flown somewhere.
    ///
    /// LibRaw's own `timestamp` field is deliberately **not** used: it runs the
    /// EXIF string through `mktime`, so its value depends on the timezone of the
    /// machine that read the file. On this dev box, in JST, that is nine hours
    /// of silent error in a column that sorts a library.
    pub captured_at: Option<i64>,
    /// Frames the shutter has fired, where the maker note records it. Part of
    /// the catalog's duplicate-detection triple with `captured_at` and the
    /// camera serial: together they identify a frame that has been renamed,
    /// which a content hash cannot do once the file has been re-saved.
    pub shutter_count: Option<u32>,
    pub lens: Option<String>,
}

/// One decoded RAW: the sensor mosaic plus everything needed to interpret it.
///
/// `data` is one sample per photosite, in sensor order — not RGB. It becomes
/// three channels at stage B, on the GPU.
#[derive(Debug, Clone)]
pub struct RawImage {
    pub camera: CameraId,
    pub width: u32,
    pub height: u32,
    pub cfa: CfaPattern,
    pub levels: SensorLevels,
    /// As-shot white balance as per-channel multipliers, as the camera recorded
    /// them. `EditState::white_balance` being `None` (as-shot) resolves to this.
    pub as_shot_neutral: [f32; 4],
    /// Camera native RGB to CIE XYZ, from the decoder's built-in camera table.
    ///
    /// A stopgap, and labelled as one: the real colour path is a DCP profile
    /// with a forward matrix, an HSL look table and a tone curve. This is the
    /// matrix alone, which is enough to see whether an image is right and not
    /// enough to call the result colour-managed. All-zero when the decoder has
    /// no data for the body.
    pub cam_to_xyz: [[f32; 3]; 4],
    pub data: Vec<u16>,
}

impl RawImage {
    /// Sanity check on the relationship between the declared geometry and the
    /// data actually present, so a truncated file is caught at the boundary
    /// rather than as a garbled render.
    pub fn validate(&self) -> Result<(), DecodeError> {
        let expected = (self.width as usize) * (self.height as usize);
        if self.data.len() != expected {
            return Err(DecodeError::Corrupt);
        }
        if self.levels.white == 0 || self.levels.black.iter().any(|&b| b >= self.levels.white) {
            return Err(DecodeError::Corrupt);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(width: u32, height: u32) -> RawImage {
        RawImage {
            camera: CameraId {
                make: "SONY".into(),
                model: "ILCE-6700".into(),
                serial: None,
            },
            width,
            height,
            cfa: CfaPattern::Rggb,
            levels: SensorLevels {
                black: [512; 4],
                white: 16383,
            },
            as_shot_neutral: [2.1, 1.0, 1.5, 1.0],
            cam_to_xyz: [[0.0; 3]; 4],
            data: vec![1000; (width * height) as usize],
        }
    }

    #[test]
    fn geometry_must_match_the_data() {
        let mut img = sample(4, 4);
        assert!(img.validate().is_ok());
        img.data.pop();
        assert!(matches!(img.validate(), Err(DecodeError::Corrupt)));
    }

    #[test]
    fn impossible_levels_are_corrupt_not_rendered() {
        let mut img = sample(4, 4);
        img.levels.black[0] = img.levels.white;
        assert!(matches!(img.validate(), Err(DecodeError::Corrupt)));
    }

    #[test]
    fn xtrans_is_not_offered_to_the_bayer_kernel() {
        assert!(CfaPattern::Rggb.is_bayer());
        assert!(!CfaPattern::XTrans.is_bayer());
    }
}
