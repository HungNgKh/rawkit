//! The LibRaw binding — the only place in the workspace that links CDDL code.
//!
//! # What this uses LibRaw for, and what it deliberately does not
//!
//! Used: container parsing, decompression, sensor geometry, black and white
//! levels, CFA layout, as-shot white balance, camera identity.
//!
//! Not used: `dcraw_process` and everything under it. LibRaw ships several
//! demosaic algorithms and a full rendering path; we take the mosaic and stop.
//! That is not squeamishness about quality — it is the single-engine rule. A
//! second demosaic reachable from the same binary is a second answer to "what
//! do these pixels look like", and the whole point of the WGSL kernel is that
//! there is only one.
//!
//! A useful side effect: LibRaw's optional GPL demosaic packs are irrelevant to
//! us by construction. They are not compiled into the vendored build, and we
//! would have no use for them if they were.
//!
//! # Licence
//!
//! LibRaw is triple-licensed (LGPL-2.1, CDDL-1.0, commercial) and we elect
//! **CDDL**, which permits static linking and source inclusion without
//! disclosing the application. CDDL is file-level copyleft: LibRaw's own files
//! stay CDDL permanently. Hence this module rather than a decoder scattered
//! across the workspace.
//!
//! ⚠️ `cargo-deny` cannot see this. The `libraw-rs-sys` crate declares
//! MIT/Apache-2.0 — which is true of the *binding* — and vendors LibRaw's C++
//! sources, which the licence checker never inspects. Machine enforcement stops
//! at crate metadata; see `docs/licence-policy.md`.

use crate::{CameraId, CfaPattern, DecodeError, RawImage, SensorLevels};
use std::ffi::{CStr, CString};
use std::path::Path;

/// Decode a RAW file to its sensor mosaic.
///
/// The returned image is cropped to the visible area — sensors record masked
/// border columns used for black-level calibration, and every downstream stage
/// would otherwise have to know about them.
pub fn decode_file(path: &Path) -> Result<RawImage, DecodeError> {
    let c_path = CString::new(path.as_os_str().to_string_lossy().as_bytes())
        .map_err(|_| DecodeError::Io("path contains a NUL byte".into()))?;

    // SAFETY: `libraw_init` returns either a valid handle or null, and `Handle`
    // owns it from here — every early return below runs its Drop.
    let handle = unsafe { libraw_sys::libraw_init(0) };
    if handle.is_null() {
        return Err(DecodeError::Io("libraw_init returned null".into()));
    }
    let handle = Handle(handle);

    // SAFETY: `handle.0` is non-null and `c_path` outlives the call.
    let rc = unsafe { libraw_sys::libraw_open_file(handle.0, c_path.as_ptr()) };
    if rc != 0 {
        return Err(open_error(rc, path));
    }

    // SAFETY: the file opened successfully, so the handle holds a parsed image.
    let rc = unsafe { libraw_sys::libraw_unpack(handle.0) };
    if rc != 0 {
        return Err(DecodeError::Corrupt);
    }

    // SAFETY: after a successful unpack, LibRaw guarantees these members are
    // populated; we only read them.
    let data = unsafe { &*handle.0 };
    let sizes = data.sizes;
    let idata = data.idata;

    // Bayer only. `filters == 9` is X-Trans and `filters == 0` means the file
    // is already three-colour (a linear DNG, or a Foveon sensor) — neither is a
    // mosaic, and pretending otherwise would produce a confident wrong image.
    let cfa = match idata.filters {
        0 => return Err(DecodeError::UnrecognisedFormat),
        9 => CfaPattern::XTrans,
        _ => bayer_pattern(handle.0),
    };

    let raw = data.rawdata.raw_image;
    if raw.is_null() {
        // A non-null `filters` with no 16-bit mosaic means the file decoded to
        // something we do not handle, such as a three-component sensor.
        return Err(DecodeError::UnrecognisedFormat);
    }

    let width = sizes.width as usize;
    let height = sizes.height as usize;
    let raw_width = sizes.raw_width as usize;
    let left = sizes.left_margin as usize;
    let top = sizes.top_margin as usize;
    if width == 0 || height == 0 {
        return Err(DecodeError::Corrupt);
    }

    // Crop the masked border out of the full sensor readout.
    let mut pixels = Vec::with_capacity(width * height);
    for y in 0..height {
        // SAFETY: `raw_image` has `raw_height * raw_width` samples and the
        // visible window plus its margins is within that by construction.
        let row = unsafe { raw.add((y + top) * raw_width + left) };
        let row = unsafe { std::slice::from_raw_parts(row, width) };
        pixels.extend_from_slice(row);
    }

    let colour = data.color;
    // Per-channel black is the global level plus the channel's own offset.
    // LibRaw can additionally describe a 2D black pattern in `cblack[4..6]`;
    // when it does, ignoring it would leave a faint grid in the shadows, so
    // refuse rather than render it wrong.
    if colour.cblack[4] != 0 || colour.cblack[5] != 0 {
        return Err(DecodeError::Corrupt);
    }
    let mut black = [0u16; 4];
    for (c, b) in black.iter_mut().enumerate() {
        *b = (colour.black + colour.cblack[c]).min(u16::MAX as u32) as u16;
    }

    let camera = CameraId {
        make: c_str(&idata.make),
        model: c_str(&idata.model),
        serial: Some(c_str(&data.shootinginfo.BodySerial)).filter(|s| !s.is_empty()),
    };

    let image = RawImage {
        camera,
        width: width as u32,
        height: height as u32,
        cfa,
        levels: SensorLevels {
            black,
            white: colour.maximum.min(u16::MAX as u32) as u16,
        },
        as_shot_neutral: colour.cam_mul,
        cam_to_xyz: colour.cam_xyz,
        data: pixels,
    };
    image.validate()?;
    Ok(image)
}

/// LibRaw reports the CFA layout through `COLOR(row, col)` in visible-image
/// coordinates, which already accounts for the crop margins. Reading the actual
/// 2x2 block is more robust than decoding the `filters` bitmask by hand — the
/// bitmask has phase conventions that are easy to get backwards, and this asks
/// the question directly.
fn bayer_pattern(handle: *mut libraw_sys::libraw_data_t) -> CfaPattern {
    // SAFETY: called only after a successful unpack, with a non-null handle.
    let colour_at = |row: i32, col: i32| unsafe { libraw_sys::libraw_COLOR(handle, row, col) };
    // LibRaw uses 3 for the second green; both greens are green here.
    let normalise = |c: i32| if c == 3 { 1 } else { c };
    match (
        normalise(colour_at(0, 0)),
        normalise(colour_at(0, 1)),
        normalise(colour_at(1, 0)),
    ) {
        (0, 1, 1) => CfaPattern::Rggb,
        (2, 1, 1) => CfaPattern::Bggr,
        (1, 0, 2) => CfaPattern::Grbg,
        (1, 2, 0) => CfaPattern::Gbrg,
        // Anything else is not a Bayer 2x2; treat it as unrecognised rather
        // than guessing a phase, which would show up as swapped colours.
        _ => CfaPattern::XTrans,
    }
}

fn open_error(rc: i32, path: &Path) -> DecodeError {
    // SAFETY: `libraw_strerror` returns a pointer to a static string.
    let message = unsafe { CStr::from_ptr(libraw_sys::libraw_strerror(rc)) }
        .to_string_lossy()
        .into_owned();
    match rc {
        // LIBRAW_FILE_UNSUPPORTED
        -100_003 => DecodeError::UnsupportedCamera {
            make: String::new(),
            model: path.display().to_string(),
        },
        _ => DecodeError::Io(message),
    }
}

fn c_str(bytes: &[std::os::raw::c_char]) -> String {
    // SAFETY: LibRaw's fixed-size char arrays are NUL-terminated.
    unsafe { CStr::from_ptr(bytes.as_ptr()) }
        .to_string_lossy()
        .trim()
        .to_owned()
}

/// Owns the LibRaw handle so that every error path frees it.
struct Handle(*mut libraw_sys::libraw_data_t);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: constructed only from a non-null `libraw_init`, and closed
        // exactly once because `Handle` is neither `Copy` nor `Clone`.
        unsafe { libraw_sys::libraw_close(self.0) };
    }
}
