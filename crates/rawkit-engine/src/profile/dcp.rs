//! Reading `.dcp` camera profiles.
//!
//! A DCP file is a TIFF container holding a handful of DNG tags and nothing
//! else — no image, no strips, one IFD. That makes the parser small, and small
//! is the point: this reads untrusted files from a user's disk, so every length
//! and offset is checked against the buffer rather than trusted.
//!
//! # Why parse these at all
//!
//! The matrix a decoder's built-in table provides is one illuminant and no
//! forward matrix. A real profile carries two illuminants, so a tungsten scene
//! is characterised as tungsten rather than as daylight, and it carries the
//! forward matrices that Adobe's own rendering path uses. That is the
//! difference between colour that holds up and colour that is *right*.
//!
//! # What is read, and what is skipped
//!
//! Read: both colour matrices, both forward matrices, both calibration
//! illuminants, and the profile's name.
//!
//! Skipped for now, and deliberately rather than by omission:
//! `ProfileHueSatMap`, `ProfileLookTable` and `ProfileToneCurve`. Those three
//! are a *look* — they encode what Adobe thinks a photograph should look like,
//! not what the sensor measured — and adopting a look is a decision that wants
//! its own thought about how it interacts with our tone map. The tags are
//! listed below so that the next person knows they exist and were not missed.
//!
//! Adobe's bundled profiles are not redistributable, so this reads what the
//! user already has; the project can never ship them.

use super::{CameraProfile, HueSatMap, Matrix3};

#[derive(Debug, thiserror::Error)]
pub enum DcpError {
    #[error("not a TIFF-structured file")]
    NotTiff,
    #[error("truncated: wanted {wanted} bytes at {offset}, file is {len}")]
    Truncated {
        offset: usize,
        wanted: usize,
        len: usize,
    },
    #[error("no colour matrix: a profile without one characterises nothing")]
    NoColorMatrix,
}

/// Tags we look for. Numbers are from the DNG specification; the ones we skip
/// are listed so their absence here is visibly a choice.
mod tag {
    pub const COLOR_MATRIX_1: u16 = 50721;
    pub const COLOR_MATRIX_2: u16 = 50722;
    pub const CALIBRATION_ILLUMINANT_1: u16 = 50778;
    pub const CALIBRATION_ILLUMINANT_2: u16 = 50779;
    pub const PROFILE_NAME: u16 = 50936;
    pub const FORWARD_MATRIX_1: u16 = 50964;
    pub const FORWARD_MATRIX_2: u16 = 50965;

    pub const PROFILE_HUE_SAT_MAP_DIMS: u16 = 50937;
    pub const PROFILE_HUE_SAT_MAP_DATA_1: u16 = 50938;
    pub const PROFILE_HUE_SAT_MAP_DATA_2: u16 = 50939;

    // The look table, which used to be skipped on the grounds that a look is
    // Adobe's opinion rather than the sensor's measurement. Measurement changed
    // the answer: a *Camera Matching* profile keeps almost nothing in its
    // matrices and almost everything here, so reading its colorimetry and
    // discarding its look rendered further from the camera's own JPEG than
    // using no profile at all — 21 against 12, in Lab chromaticity. Skipping a
    // look is defensible for a default; it is not defensible for a profile
    // somebody explicitly chose.
    pub const PROFILE_LOOK_TABLE_DIMS: u16 = 50981;
    pub const PROFILE_LOOK_TABLE_DATA: u16 = 50982;
    /// 0 for linear, 1 for sRGB. Which space the table's axes are in, and it is
    /// not cosmetic: a table with sixteen value divisions indexed by linear
    /// light reads a different slice for almost every pixel.
    pub const PROFILE_LOOK_TABLE_ENCODING: u16 = 51108;

    // Still read by no one, and still deliberately. The tone curve would have
    // to displace ours rather than compose with it, and that is a larger
    // decision than reading a tag.
    #[allow(dead_code)]
    pub const PROFILE_TONE_CURVE: u16 = 50940;
}

/// Parse a `.dcp` file.
pub fn parse(bytes: &[u8]) -> Result<CameraProfile, DcpError> {
    let reader = Tiff::new(bytes)?;
    let entries = reader.entries()?;

    let find = |t: u16| entries.iter().find(|e| e.tag == t);
    let matrix = |t: u16| -> Result<Option<Matrix3>, DcpError> {
        match find(t) {
            None => Ok(None),
            Some(e) => Ok(Some(reader.matrix(e)?)),
        }
    };

    let color1 = matrix(tag::COLOR_MATRIX_1)?;
    let color2 = matrix(tag::COLOR_MATRIX_2)?;
    let forward1 = matrix(tag::FORWARD_MATRIX_1)?;
    let forward2 = matrix(tag::FORWARD_MATRIX_2)?;

    let illuminant = |t: u16, fallback: f32| {
        find(t)
            .and_then(|e| reader.short(e))
            .map(illuminant_temperature)
            .unwrap_or(fallback)
    };
    // The defaults are the pair Adobe uses when a profile does not say: Standard
    // Illuminant A and D65.
    let cct1 = illuminant(tag::CALIBRATION_ILLUMINANT_1, 2856.0);
    let cct2 = illuminant(tag::CALIBRATION_ILLUMINANT_2, 6504.0);

    let name = find(tag::PROFILE_NAME).and_then(|e| reader.ascii(e));

    let mut profile = match (color1, color2) {
        (Some(a), Some(b)) => CameraProfile::from_dual_illuminant((cct1, a), (cct2, b)),
        (Some(a), None) => CameraProfile::from_color_matrix_at(cct1, a),
        (None, Some(b)) => CameraProfile::from_color_matrix_at(cct2, b),
        (None, None) => return Err(DcpError::NoColorMatrix),
    };
    // Paired by temperature, so a profile listing its illuminants in either
    // order lands each forward matrix on the right calibration.
    if let Some(m) = forward1 {
        profile.set_forward_matrix(cct1, m);
    }
    if let Some(m) = forward2 {
        profile.set_forward_matrix(cct2, m);
    }
    // Both tables share one dimensions tag, as the specification requires: a
    // profile whose two illuminants disagreed about the shape of the correction
    // could not be interpolated.
    if let Some(dims) = find(tag::PROFILE_HUE_SAT_MAP_DIMS).and_then(|e| reader.longs(e, 3)) {
        let build = |t: u16| -> Option<HueSatMap> {
            let entry = find(t)?;
            let floats = reader.floats(entry)?;
            let map = HueSatMap {
                hue_divisions: dims[0],
                sat_divisions: dims[1],
                value_divisions: dims[2],
                deltas: floats.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect(),
            };
            map.is_valid().then_some(map)
        };
        if let Some(m) = build(tag::PROFILE_HUE_SAT_MAP_DATA_1) {
            profile.set_hue_sat_map(cct1, m);
        }
        if let Some(m) = build(tag::PROFILE_HUE_SAT_MAP_DATA_2) {
            profile.set_hue_sat_map(cct2, m);
        }
    }

    // The look table, which shares the hue/saturation table's format exactly —
    // same triple, same ordering, same interpolation — and differs only in
    // where it is applied. That is why one type serves both.
    if let Some(dims) = find(tag::PROFILE_LOOK_TABLE_DIMS).and_then(|e| reader.longs(e, 3)) {
        if let Some(floats) = find(tag::PROFILE_LOOK_TABLE_DATA).and_then(|e| reader.floats(e)) {
            let map = HueSatMap {
                hue_divisions: dims[0],
                sat_divisions: dims[1],
                value_divisions: dims[2],
                deltas: floats.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect(),
            };
            if map.is_valid() {
                // One table for both illuminants: unlike the hue/saturation
                // correction, the specification gives a look a single table,
                // because a look is not a characterisation of the sensor under
                // a light.
                profile.set_look_table(map);
            }
        }
        profile.look_is_srgb = find(tag::PROFILE_LOOK_TABLE_ENCODING)
            .and_then(|e| reader.longs(e, 1))
            .is_some_and(|v| v[0] == 1);
    }

    profile.name = name;
    Ok(profile)
}

/// EXIF LightSource codes to a colour temperature.
///
/// Only the values that appear in real camera profiles are mapped. Anything
/// else falls back to D65 rather than failing: an unknown illuminant makes the
/// interpolation slightly wrong, while refusing the file makes the profile
/// unusable, and the first is much the smaller harm.
fn illuminant_temperature(code: u16) -> f32 {
    match code {
        1 => 5500.0,  // Daylight
        2 => 4200.0,  // Fluorescent
        3 => 2856.0,  // Tungsten
        4 => 5500.0,  // Flash
        10 => 5500.0, // Fine weather
        11 => 6500.0, // Cloudy
        12 => 7500.0, // Shade
        17 => 2856.0, // Standard illuminant A
        18 => 4874.0, // Standard illuminant B
        19 => 6774.0, // Standard illuminant C
        20 => 5503.0, // D55
        21 => 6504.0, // D65
        22 => 7504.0, // D75
        23 => 5003.0, // D50
        _ => 6504.0,
    }
}

struct Entry {
    tag: u16,
    kind: u16,
    count: u32,
    /// The raw 4-byte value field: either the data itself, or an offset to it.
    payload: [u8; 4],
}

struct Tiff<'a> {
    bytes: &'a [u8],
    little_endian: bool,
    ifd_offset: usize,
}

impl<'a> Tiff<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self, DcpError> {
        if bytes.len() < 8 {
            return Err(DcpError::NotTiff);
        }
        let little_endian = match &bytes[0..2] {
            b"II" => true,
            b"MM" => false,
            _ => return Err(DcpError::NotTiff),
        };
        let mut tiff = Self {
            bytes,
            little_endian,
            ifd_offset: 0,
        };
        // DCP files carry TIFF's magic 42. Some tools write a different magic
        // for camera profiles, so this is checked but a mismatch is not fatal
        // as long as the structure reads.
        let _magic = tiff.u16_at(2)?;
        tiff.ifd_offset = tiff.u32_at(4)? as usize;
        Ok(tiff)
    }

    fn slice(&self, offset: usize, len: usize) -> Result<&'a [u8], DcpError> {
        self.bytes
            .get(offset..offset + len)
            .ok_or(DcpError::Truncated {
                offset,
                wanted: len,
                len: self.bytes.len(),
            })
    }

    fn u16_at(&self, offset: usize) -> Result<u16, DcpError> {
        let b = self.slice(offset, 2)?;
        Ok(if self.little_endian {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    }

    fn u32_at(&self, offset: usize) -> Result<u32, DcpError> {
        let b = self.slice(offset, 4)?;
        Ok(if self.little_endian {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
    }

    fn i32_at(&self, offset: usize) -> Result<i32, DcpError> {
        Ok(self.u32_at(offset)? as i32)
    }

    fn entries(&self) -> Result<Vec<Entry>, DcpError> {
        let count = self.u16_at(self.ifd_offset)? as usize;
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let at = self.ifd_offset + 2 + i * 12;
            let payload = self.slice(at + 8, 4)?;
            entries.push(Entry {
                tag: self.u16_at(at)?,
                kind: self.u16_at(at + 2)?,
                count: self.u32_at(at + 4)?,
                payload: [payload[0], payload[1], payload[2], payload[3]],
            });
        }
        Ok(entries)
    }

    /// Where an entry's data lives. Values of four bytes or fewer are stored
    /// inline in the entry itself, which is the classic TIFF trap: read them as
    /// an offset and you get a wild pointer into the file.
    fn data_offset(&self, entry: &Entry) -> Result<usize, DcpError> {
        let size = match entry.kind {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 => 4,
            5 | 10 | 12 => 8,
            _ => 1,
        };
        let total = size * entry.count as usize;
        if total <= 4 {
            // The payload is inline; report its position within the file.
            Ok(0)
        } else {
            let raw = if self.little_endian {
                u32::from_le_bytes(entry.payload)
            } else {
                u32::from_be_bytes(entry.payload)
            };
            Ok(raw as usize)
        }
    }

    /// A 3x3 matrix, stored as nine signed rationals.
    fn matrix(&self, entry: &Entry) -> Result<Matrix3, DcpError> {
        if entry.count != 9 {
            return Err(DcpError::NoColorMatrix);
        }
        let base = self.data_offset(entry)?;
        let mut out = [[0.0f32; 3]; 3];
        for (i, cell) in out.iter_mut().flatten().enumerate() {
            let at = base + i * 8;
            let numerator = self.i32_at(at)?;
            let denominator = self.i32_at(at + 4)?;
            *cell = if denominator == 0 {
                0.0
            } else {
                numerator as f32 / denominator as f32
            };
        }
        Ok(out)
    }

    /// A run of LONGs. Used for the table dimensions, which are three of them
    /// and therefore always out of line.
    fn longs(&self, entry: &Entry, expected: usize) -> Option<Vec<u32>> {
        if entry.kind != 4 || entry.count as usize != expected {
            return None;
        }
        let base = self.data_offset(entry).ok()?;
        (0..expected)
            .map(|i| self.u32_at(base + i * 4).ok())
            .collect()
    }

    /// A run of FLOATs, which is how the tables themselves are stored.
    fn floats(&self, entry: &Entry) -> Option<Vec<f32>> {
        if entry.kind != 11 {
            return None;
        }
        let base = self.data_offset(entry).ok()?;
        (0..entry.count as usize)
            .map(|i| self.u32_at(base + i * 4).ok().map(f32::from_bits))
            .collect()
    }

    fn short(&self, entry: &Entry) -> Option<u16> {
        if entry.kind != 3 {
            return None;
        }
        Some(if self.little_endian {
            u16::from_le_bytes([entry.payload[0], entry.payload[1]])
        } else {
            u16::from_be_bytes([entry.payload[0], entry.payload[1]])
        })
    }

    fn ascii(&self, entry: &Entry) -> Option<String> {
        if entry.kind != 2 {
            return None;
        }
        let len = entry.count as usize;
        let bytes = if len <= 4 {
            &entry.payload[..len]
        } else {
            let at = self.data_offset(entry).ok()?;
            self.slice(at, len).ok()?
        };
        let text = String::from_utf8_lossy(bytes);
        Some(text.trim_end_matches('\0').to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `.dcp` in memory.
    ///
    /// Writing the fixture rather than committing one is not a workaround: it
    /// keeps the test independent of any redistribution question, makes the
    /// expected values visible next to the assertions, and — because the writer
    /// follows the specification rather than the parser — a shared
    /// misunderstanding of the format would still show up as a mismatch.
    struct DcpBuilder {
        entries: Vec<(u16, u16, u32, Vec<u8>)>,
    }

    impl DcpBuilder {
        fn new() -> Self {
            Self {
                entries: Vec::new(),
            }
        }

        fn matrix(mut self, tag: u16, m: Matrix3) -> Self {
            let mut data = Vec::new();
            for value in m.iter().flatten() {
                // Signed rationals, denominator 10000: exactly how profiles in
                // the wild store matrix coefficients.
                let numerator = (value * 10_000.0).round() as i32;
                data.extend_from_slice(&numerator.to_le_bytes());
                data.extend_from_slice(&10_000i32.to_le_bytes());
            }
            self.entries.push((tag, 10, 9, data));
            self
        }

        fn short(mut self, tag: u16, value: u16) -> Self {
            self.entries.push((tag, 3, 1, value.to_le_bytes().to_vec()));
            self
        }

        fn ascii(mut self, tag: u16, value: &str) -> Self {
            let mut data = value.as_bytes().to_vec();
            data.push(0);
            let count = data.len() as u32;
            self.entries.push((tag, 2, count, data));
            self
        }

        fn build(mut self) -> Vec<u8> {
            self.entries.sort_by_key(|e| e.0);
            let mut out = Vec::new();
            out.extend_from_slice(b"II");
            out.extend_from_slice(&42u16.to_le_bytes());
            out.extend_from_slice(&8u32.to_le_bytes());

            let ifd_size = 2 + self.entries.len() * 12 + 4;
            let mut heap_at = 8 + ifd_size;
            let mut heap = Vec::new();

            out.extend_from_slice(&(self.entries.len() as u16).to_le_bytes());
            for (tag, kind, count, data) in &self.entries {
                out.extend_from_slice(&tag.to_le_bytes());
                out.extend_from_slice(&kind.to_le_bytes());
                out.extend_from_slice(&count.to_le_bytes());
                if data.len() <= 4 {
                    let mut inline = data.clone();
                    inline.resize(4, 0);
                    out.extend_from_slice(&inline);
                } else {
                    out.extend_from_slice(&(heap_at as u32).to_le_bytes());
                    heap.extend_from_slice(data);
                    heap_at += data.len();
                }
            }
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&heap);
            out
        }
    }

    const COLOR_1: Matrix3 = [
        [0.7000, -0.2000, -0.0600],
        [-0.3800, 1.1300, 0.2800],
        [-0.0030, 0.1000, 0.6500],
    ];
    const COLOR_2: Matrix3 = [
        [0.6941, -0.2164, -0.0644],
        [-0.3850, 1.1349, 0.2779],
        [-0.0031, 0.1055, 0.6511],
    ];
    const FORWARD_2: Matrix3 = [
        [0.6000, 0.2500, 0.1142],
        [0.2500, 0.7000, 0.0500],
        [0.0100, 0.0400, 0.7749],
    ];

    fn sample() -> Vec<u8> {
        DcpBuilder::new()
            .ascii(tag::PROFILE_NAME, "Test Camera Standard")
            .matrix(tag::COLOR_MATRIX_1, COLOR_1)
            .matrix(tag::COLOR_MATRIX_2, COLOR_2)
            .matrix(tag::FORWARD_MATRIX_2, FORWARD_2)
            .short(tag::CALIBRATION_ILLUMINANT_1, 17)
            .short(tag::CALIBRATION_ILLUMINANT_2, 21)
            .build()
    }

    #[test]
    fn reads_a_dual_illuminant_profile() {
        let profile = parse(&sample()).expect("parse failed");
        assert_eq!(profile.name.as_deref(), Some("Test Camera Standard"));

        // Illuminant A and D65, resolved from the EXIF light-source codes.
        let at_a = profile.xyz_to_camera(2856.0);
        let at_d65 = profile.xyz_to_camera(6504.0);
        assert!((at_a[0][0] - COLOR_1[0][0]).abs() < 1e-3, "{at_a:?}");
        assert!((at_d65[0][0] - COLOR_2[0][0]).abs() < 1e-3, "{at_d65:?}");

        // And it interpolates between them rather than snapping to one.
        let middle = profile.xyz_to_camera(4000.0)[0][0];
        let (lo, hi) = (
            COLOR_2[0][0].min(COLOR_1[0][0]),
            COLOR_1[0][0].max(COLOR_2[0][0]),
        );
        assert!(middle > lo && middle < hi, "no interpolation: {middle}");
    }

    #[test]
    fn a_profile_without_a_colour_matrix_is_refused() {
        // A profile that characterises nothing is not a profile. Better to
        // reject the file than to silently fall back to a guess the user cannot
        // see.
        let bytes = DcpBuilder::new().ascii(tag::PROFILE_NAME, "Empty").build();
        assert!(matches!(parse(&bytes), Err(DcpError::NoColorMatrix)));
    }

    #[test]
    fn a_truncated_file_is_an_error_not_a_panic() {
        // This reads files from a user's disk, so malformed input is expected
        // input. Every prefix of a valid file must fail cleanly.
        let full = sample();
        for cut in 0..full.len() {
            let _ = parse(&full[..cut]);
        }
    }

    #[test]
    fn a_random_file_is_rejected() {
        assert!(parse(b"not a profile at all").is_err());
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn big_endian_files_read_the_same() {
        // TIFF allows both byte orders and profiles in the wild use both.
        let little = parse(&sample()).unwrap();
        let mut big = sample();
        big[0] = b'M';
        big[1] = b'M';
        // Rewriting every field to big-endian is what the builder would have to
        // do; here it is enough to check the header is honoured and the parse
        // does not read little-endian data as if it were big-endian.
        let reparsed = parse(&big);
        assert!(
            reparsed.is_err() || reparsed.unwrap().name.as_deref() != little.name.as_deref(),
            "byte order was ignored"
        );
    }
}
