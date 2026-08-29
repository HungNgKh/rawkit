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

use crate::{CameraId, CfaPattern, DecodeError, RawImage, RawMetadata, SensorLevels};
use std::ffi::{CStr, CString};
use std::path::Path;

/// Read a file's camera, capture time and lens without decoding its pixels.
///
/// `libraw_open_file` parses the container and the maker notes; `libraw_unpack`
/// is what reads and decompresses the sensor data. Stopping between the two is
/// the whole cost argument for reading metadata during a catalog scan: it is a
/// header parse per file, not a decode per file.
pub fn read_metadata(path: &Path) -> Result<RawMetadata, DecodeError> {
    let handle = open(path)?;
    // SAFETY: `open` returns only on a successful parse, and we read fields
    // LibRaw populates before unpacking.
    let data = unsafe { &*handle.0 };
    Ok(metadata(data))
}

/// Decode a RAW file to its sensor mosaic.
///
/// The returned image is cropped to the visible area — sensors record masked
/// border columns used for black-level calibration, and every downstream stage
/// would otherwise have to know about them.
pub fn decode_file(path: &Path) -> Result<RawImage, DecodeError> {
    let handle = open(path)?;

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

    let image = RawImage {
        camera: camera_id(data),
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

/// Open and parse a file, leaving the pixel data unread.
fn open(path: &Path) -> Result<Handle, DecodeError> {
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
    Ok(handle)
}

fn metadata(data: &libraw_sys::libraw_data_t) -> RawMetadata {
    let camera = camera_id(data);
    let sony = camera.make.eq_ignore_ascii_case("sony");

    RawMetadata {
        // The visible area, not the full sensor readout — the same crop
        // `decode_file` applies, so a viewport built from these numbers matches
        // the pixels that arrive later.
        width: data.sizes.width as u32,
        height: data.sizes.height as u32,
        // The maker note carries the capture time as the characters the camera
        // wrote, which is the only form with no timezone applied to it. LibRaw
        // exposes that string per vendor and only ever exposes the timezone-
        // converted `other.timestamp` generally, so a body whose maker note we
        // cannot read gets **no** capture time rather than a shifted one.
        captured_at: sony
            .then(|| wall_clock(&c_str(&data.makernotes.sony.SonyDateTime)))
            .flatten(),
        // Sony's `ImageCount3` (maker note 0x9050) is the shutter actuation
        // count. Zero means the field was not present, not a new camera.
        shutter_count: sony
            .then_some(data.makernotes.sony.ImageCount3)
            .filter(|&n| n != 0),
        lens: [c_str(&data.lens.Lens), c_str(&data.lens.makernotes.Lens)]
            .into_iter()
            .find(|s| !s.is_empty()),
        camera,
    }
}

fn camera_id(data: &libraw_sys::libraw_data_t) -> CameraId {
    CameraId {
        make: c_str(&data.idata.make),
        model: c_str(&data.idata.model),
        // Sony records the body serial in the *internal* field on several
        // bodies and leaves the other empty, so take whichever is there.
        serial: [
            c_str(&data.shootinginfo.BodySerial),
            c_str(&data.shootinginfo.InternalBodySerial),
        ]
        .into_iter()
        .find(|s| !s.is_empty()),
    }
}

/// `"YYYY:MM:DD HH:MM:SS"` to seconds since the epoch, reading the wall clock as
/// if it were UTC.
///
/// Deliberately not a timezone conversion: an EXIF capture time has no zone, so
/// there is nothing to convert *from*, and applying the reader's zone is how the
/// same file ends up with two different times in one library.
fn wall_clock(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let field = |at: usize, len: usize| text.get(at..at + len)?.parse::<i64>().ok();
    let (year, month, day) = (field(0, 4)?, field(5, 2)?, field(8, 2)?);
    let (hour, minute, second) = (field(11, 2)?, field(14, 2)?, field(17, 2)?);

    // A camera with a flat battery writes 0000:00:00, and a corrupt maker note
    // writes anything at all. Both are refused rather than stored as some date
    // in the first century.
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || year < 1970
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Days since 1970-01-01 in the proleptic Gregorian calendar (Howard Hinnant's
/// `days_from_civil`). The same algorithm the backup timestamps run backwards.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
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

/// Read one of LibRaw's fixed-size `char` arrays.
///
/// Bounded rather than `CStr::from_ptr`: these are filled in by maker-note
/// parsers reading a file we did not write, and a field that exactly fills its
/// array with no room for a terminator would otherwise read off the end of the
/// struct.
fn c_str(bytes: &[std::os::raw::c_char]) -> String {
    // SAFETY: reinterpreting `c_char` as `u8` — same size and alignment, and
    // the slice is a live field of a struct LibRaw owns.
    let bytes = unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), bytes.len()) };
    let text = match CStr::from_bytes_until_nul(bytes) {
        Ok(text) => text.to_string_lossy(),
        Err(_) => String::from_utf8_lossy(bytes),
    };
    text.trim().to_owned()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capture_time_is_the_cameras_wall_clock_read_as_utc() {
        // The real frame this project develops against: DSC00881.ARW carries
        // "2026:08:10 17:28:10" five times over, and 1786382890 is that instant
        // in UTC (`TZ=UTC date -d ... +%s`, i.e. not our own arithmetic). The
        // dev box is in JST, so a timezone conversion anywhere in this path
        // would land nine hours out — exactly the failure this function exists
        // to avoid, and exactly the kind that looks fine in a listing.
        assert_eq!(wall_clock("2026:08:10 17:28:10"), Some(1_786_382_890));
    }

    #[test]
    fn the_epoch_and_a_leap_day_are_where_calendars_go_wrong() {
        assert_eq!(wall_clock("1970:01:01 00:00:00"), Some(0));
        assert_eq!(wall_clock("2000:03:01 00:00:00"), Some(951_868_800));
        // 2000 is a leap year and 1900 is not; the day before is the test.
        assert_eq!(wall_clock("2000:02:29 12:00:00"), Some(951_825_600));
        assert_eq!(wall_clock("2024:12:31 23:59:59"), Some(1_735_689_599));
    }

    #[test]
    fn a_camera_with_a_flat_battery_gets_no_capture_time() {
        // Every one of these is something a real file contains, and every one
        // of them parses into *some* number if the ranges are not checked.
        for text in [
            "0000:00:00 00:00:00",
            "2026:13:01 00:00:00",
            "2026:01:32 00:00:00",
            "2026:01:01 24:00:00",
            "1969:12:31 23:59:59",
            "",
            "2026:08:10",
            "not a date at all!!",
        ] {
            assert_eq!(wall_clock(text), None, "{text:?} must not become a date");
        }
    }

    #[test]
    fn a_field_that_fills_its_array_does_not_run_off_the_end() {
        // LibRaw's `SonyDateTime` is 20 bytes and the string it holds is 19,
        // which leaves exactly one byte for a terminator. A parser that wrote
        // one character more would have left none.
        let full: Vec<std::os::raw::c_char> = "2026:08:10 17:28:10"
            .bytes()
            .map(|b| b as std::os::raw::c_char)
            .collect();
        assert_eq!(c_str(&full), "2026:08:10 17:28:10");
    }
}
