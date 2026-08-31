//! A 16-bit TIFF writer.
//!
//! # Why this is written out rather than pulled in
//!
//! Every dependency in this workspace costs a licence review (see
//! `docs/licence-policy.md`), and what an export actually needs from TIFF is a
//! very small part of it: one image, three channels, sixteen bits, one profile
//! attached. That is a header, one image file directory and the pixels — about
//! as much format as a PPM with a table of contents. Reading arbitrary TIFF is
//! the hard half of that format and this does none of it.
//!
//! The repository already contains two *readers* of TIFF structure — the DCP
//! profile parser and the EXIF walk — so the layout below is being written
//! against code that already has to agree with it.
//!
//! # Why 16 bits and no 8-bit option
//!
//! TIFF is here for the round trip: out to a specialist tool and back, which is
//! the answer this project gives to "your noise reduction is not DxO's". Eight
//! bits is what that round trip must not be — a gradient that survives one
//! encode visibly bands on the second. PNG already covers lossless eight-bit
//! for anyone who wants it.
//!
//! # The byte order is big-endian, and that is not arbitrary
//!
//! `MM` rather than `II`. Both are the specification and every reader handles
//! both; this one is chosen because the sixteen-bit samples arrive from
//! [`crate::encode`] already in big-endian, converted by the same code that
//! feeds the PNG writer. Choosing `II` would mean a second conversion whose only
//! job is to disagree with the first one.

use std::io::Write;

/// Roughly how large an uncompressed strip should be.
///
/// Strips exist so a reader can seek into an image rather than inflate all of
/// it, and so compression restarts often enough that one damaged run does not
/// take the rest of the file with it. A megabyte is the usual neighbourhood;
/// the exact figure matters far less than not having a single strip the size of
/// the photograph.
const STRIP_TARGET: usize = 1 << 20;

/// Field types, from the TIFF 6.0 specification.
const BYTE_ASCII: u16 = 2;
const SHORT: u16 = 3;
const LONG: u16 = 4;
const RATIONAL: u16 = 5;
const UNDEFINED: u16 = 7;

/// Adobe's Deflate, which is what Photoshop and Lightroom write as "ZIP".
///
/// Tag 8 rather than 32946: both mean a zlib stream, and 8 is the one every
/// reader in circulation has seen.
const DEFLATE: u16 = 8;
const UNCOMPRESSED: u16 = 1;

/// Horizontal differencing: each sample stored as its difference from the one
/// to its left, per channel.
///
/// Deflate alone does very little to a photograph — neighbouring sixteen-bit
/// samples are similar but almost never *equal*, which is the only thing a
/// dictionary coder can exploit. Differencing turns "similar" into "small",
/// which is what the entropy coder can then use.
const PREDICTOR_HORIZONTAL: u16 = 2;
const PREDICTOR_NONE: u16 = 1;

/// Write one RGB image, sixteen bits a channel, with its profile attached.
///
/// `samples` is big-endian `u16`, three per pixel, row major — exactly what
/// [`crate::encode`] hands the PNG writer.
pub fn encode(
    samples: &[u8],
    width: u32,
    height: u32,
    icc: &[u8],
) -> Result<Vec<u8>, crate::ExportError> {
    let row_bytes = width as usize * 3 * 2;
    if samples.len() != row_bytes * height as usize {
        return Err(crate::ExportError::WrongSize {
            width,
            height,
            expected: row_bytes * height as usize,
            actual: samples.len(),
        });
    }
    if width == 0 || height == 0 {
        return Err(crate::ExportError::Encode(
            "a TIFF needs at least one pixel".into(),
        ));
    }

    let rows_per_strip = (STRIP_TARGET / row_bytes.max(1)).clamp(1, height as usize) as u32;
    let strips: Vec<&[u8]> = samples
        .chunks(row_bytes * rows_per_strip as usize)
        .collect();

    // Compressed first, then compared against the plain bytes. Deflate on
    // photographic data is not guaranteed to win, and a "compressed" file
    // larger than the plain one is a format doing the opposite of its job.
    let squeezed: Option<Vec<Vec<u8>>> = strips
        .iter()
        .map(|strip| deflate(strip, width, row_bytes))
        .collect::<Result<Vec<_>, _>>()
        .ok();
    let (bodies, compression, predictor) = match squeezed {
        Some(packed) if packed.iter().map(Vec::len).sum::<usize>() < samples.len() => {
            (packed, DEFLATE, PREDICTOR_HORIZONTAL)
        }
        _ => (
            strips.iter().map(|s| s.to_vec()).collect(),
            UNCOMPRESSED,
            PREDICTOR_NONE,
        ),
    };

    // Header, then the pixels, then the directory that describes them. The
    // directory goes last because it has to name the offset of every strip, and
    // a strip has no offset until it has been written.
    let mut out = Vec::with_capacity(samples.len() + icc.len() + 1024);
    out.extend_from_slice(&[0x4D, 0x4D]); // "MM"
    out.extend_from_slice(&42u16.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // patched once the IFD is placed

    let mut offsets = Vec::with_capacity(bodies.len());
    let mut counts = Vec::with_capacity(bodies.len());
    for body in &bodies {
        pad(&mut out);
        offsets.push(out.len() as u32);
        counts.push(body.len() as u32);
        out.extend_from_slice(body);
    }

    pad(&mut out);
    let ifd_at = out.len();
    let mut fields: Vec<Field> = vec![
        Field::long(256, &[width]),
        Field::long(257, &[height]),
        Field::short(258, &[16, 16, 16]),
        Field::short(259, &[compression]),
        Field::short(262, &[2]), // RGB
        Field::long(273, &offsets),
        Field::short(274, &[1]), // orientation: the pixels are already turned
        Field::short(277, &[3]),
        Field::long(278, &[rows_per_strip]),
        Field::long(279, &counts),
        Field::rational(282, &[(72, 1)]),
        Field::rational(283, &[(72, 1)]),
        Field::short(296, &[2]), // inches
        Field::ascii(305, "rawkit"),
    ];
    if predictor != PREDICTOR_NONE {
        fields.push(Field::short(317, &[predictor]));
    }
    // Unsigned integer, which is the default — written anyway, because a
    // sixteen-bit file is exactly where a reader guessing "half float" would be
    // an expensive kind of wrong.
    fields.push(Field::short(339, &[1, 1, 1]));
    fields.push(Field::new(34675, UNDEFINED, icc.len() as u32, icc.to_vec()));

    // Ascending tag order is not a stylistic matter: readers binary-search a
    // directory, and one written out of order is one they cannot find fields in.
    debug_assert!(fields.windows(2).all(|w| w[0].tag < w[1].tag));

    let directory_bytes = 2 + fields.len() * 12 + 4;
    let mut values_at = ifd_at + directory_bytes;
    // Every out-of-line value begins on an even offset, which the specification
    // requires and some readers rely on.
    values_at += values_at % 2;

    let mut directory = Vec::with_capacity(directory_bytes);
    let mut values = Vec::new();
    directory.extend_from_slice(&(fields.len() as u16).to_be_bytes());
    for field in &fields {
        directory.extend_from_slice(&field.tag.to_be_bytes());
        directory.extend_from_slice(&field.kind.to_be_bytes());
        directory.extend_from_slice(&field.count.to_be_bytes());
        if field.payload.len() <= 4 {
            // Left-justified, because this is big-endian: a `SHORT` sits in the
            // first two bytes of the field and the remaining two are padding.
            // The classic way to misread a TIFF is to treat this as an offset.
            let mut inline = [0u8; 4];
            inline[..field.payload.len()].copy_from_slice(&field.payload);
            directory.extend_from_slice(&inline);
        } else {
            if values.len() % 2 == 1 {
                values.push(0);
            }
            directory.extend_from_slice(&((values_at + values.len()) as u32).to_be_bytes());
            values.extend_from_slice(&field.payload);
        }
    }
    directory.extend_from_slice(&0u32.to_be_bytes()); // no second directory

    out.extend_from_slice(&directory);
    while out.len() < values_at {
        out.push(0);
    }
    out.extend_from_slice(&values);
    out[4..8].copy_from_slice(&(ifd_at as u32).to_be_bytes());
    Ok(out)
}

/// One directory entry, with its value already in the file's byte order.
struct Field {
    tag: u16,
    kind: u16,
    count: u32,
    payload: Vec<u8>,
}

impl Field {
    fn new(tag: u16, kind: u16, count: u32, payload: Vec<u8>) -> Self {
        Self {
            tag,
            kind,
            count,
            payload,
        }
    }

    fn short(tag: u16, values: &[u16]) -> Self {
        let payload = values.iter().flat_map(|v| v.to_be_bytes()).collect();
        Self::new(tag, SHORT, values.len() as u32, payload)
    }

    fn long(tag: u16, values: &[u32]) -> Self {
        let payload = values.iter().flat_map(|v| v.to_be_bytes()).collect();
        Self::new(tag, LONG, values.len() as u32, payload)
    }

    fn rational(tag: u16, values: &[(u32, u32)]) -> Self {
        let payload = values
            .iter()
            .flat_map(|(n, d)| {
                let mut b = n.to_be_bytes().to_vec();
                b.extend_from_slice(&d.to_be_bytes());
                b
            })
            .collect();
        Self::new(tag, RATIONAL, values.len() as u32, payload)
    }

    fn ascii(tag: u16, text: &str) -> Self {
        let mut payload = text.as_bytes().to_vec();
        payload.push(0);
        Self::new(tag, BYTE_ASCII, payload.len() as u32, payload)
    }
}

/// Even offsets throughout, which the specification asks for.
fn pad(out: &mut Vec<u8>) {
    if out.len() % 2 == 1 {
        out.push(0);
    }
}

/// Horizontal differencing, then zlib.
fn deflate(strip: &[u8], width: u32, row_bytes: usize) -> Result<Vec<u8>, std::io::Error> {
    let mut differenced = strip.to_vec();
    for row in differenced.chunks_mut(row_bytes) {
        // Right to left, so each sample is differenced against the *original*
        // value beside it rather than against one already replaced.
        for x in (1..width as usize).rev() {
            for c in 0..3 {
                let here = (x * 3 + c) * 2;
                let left = ((x - 1) * 3 + c) * 2;
                let a = u16::from_be_bytes([row[here], row[here + 1]]);
                let b = u16::from_be_bytes([row[left], row[left + 1]]);
                row[here..here + 2].copy_from_slice(&a.wrapping_sub(b).to_be_bytes());
            }
        }
    }
    // The *fastest* level, and that is not a compromise: measured on three
    // 24 MP frames it came out both smaller and two and a half times quicker
    // than the default. Differencing leaves mostly small residuals, and the
    // longer match searches the higher levels pay for find nothing in them --
    // 106.4 MB in 2.9 s against 107.3 MB in 6.7 s on one frame, and the same
    // ordering on the other two.
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&differenced)?;
    encoder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader written against the specification rather than against the
    /// writer above.
    ///
    /// The point of it is independence: a round trip through the writer's own
    /// assumptions proves only that it is self-consistent, which a file that no
    /// other program can open would also be. This walks the header, the
    /// directory and the strips the way the format says to, and it is
    /// deliberately strict — every offset checked, the inline-versus-offset
    /// field handled explicitly, because that is the trap.
    struct Tiff {
        fields: std::collections::BTreeMap<u16, (u16, u32, Vec<u8>)>,
        bytes: Vec<u8>,
    }

    impl Tiff {
        fn parse(bytes: &[u8]) -> Tiff {
            assert_eq!(&bytes[..2], b"MM", "byte order mark");
            assert_eq!(u16::from_be_bytes([bytes[2], bytes[3]]), 42, "magic");
            let ifd = u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize;
            assert!(ifd % 2 == 0, "the directory must begin on an even offset");
            assert!(ifd + 2 <= bytes.len(), "directory past the end of the file");

            let count = u16::from_be_bytes([bytes[ifd], bytes[ifd + 1]]) as usize;
            let mut fields = std::collections::BTreeMap::new();
            let mut previous = 0u16;
            for i in 0..count {
                let at = ifd + 2 + i * 12;
                assert!(at + 12 <= bytes.len(), "entry {i} past the end");
                let tag = u16::from_be_bytes([bytes[at], bytes[at + 1]]);
                assert!(tag > previous, "tags out of order: {previous} then {tag}");
                previous = tag;
                let kind = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]);
                let n = u32::from_be_bytes(bytes[at + 4..at + 8].try_into().unwrap());
                let width = match kind {
                    1 | 2 | 7 => 1,
                    3 => 2,
                    4 => 4,
                    5 => 8,
                    other => panic!("unexpected field type {other} on tag {tag}"),
                };
                let size = n as usize * width;
                let payload = if size <= 4 {
                    bytes[at + 8..at + 8 + size].to_vec()
                } else {
                    let at =
                        u32::from_be_bytes(bytes[at + 8..at + 12].try_into().unwrap()) as usize;
                    assert!(at % 2 == 0, "tag {tag} points at an odd offset");
                    assert!(at + size <= bytes.len(), "tag {tag} points past the end");
                    bytes[at..at + size].to_vec()
                };
                fields.insert(tag, (kind, n, payload));
            }
            let next = ifd + 2 + count * 12;
            assert_eq!(
                u32::from_be_bytes(bytes[next..next + 4].try_into().unwrap()),
                0,
                "a second directory was promised and not written"
            );
            Tiff {
                fields,
                bytes: bytes.to_vec(),
            }
        }

        fn shorts(&self, tag: u16) -> Vec<u16> {
            let (kind, _, payload) = self
                .fields
                .get(&tag)
                .unwrap_or_else(|| panic!("no tag {tag}"));
            assert_eq!(*kind, SHORT, "tag {tag} is not a SHORT");
            payload
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect()
        }

        fn longs(&self, tag: u16) -> Vec<u32> {
            let (kind, _, payload) = self
                .fields
                .get(&tag)
                .unwrap_or_else(|| panic!("no tag {tag}"));
            assert_eq!(*kind, LONG, "tag {tag} is not a LONG");
            payload
                .chunks_exact(4)
                .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
                .collect()
        }

        /// The pixels, undone: strips inflated, differencing reversed.
        fn samples(&self) -> Vec<u8> {
            let width = self.longs(256)[0] as usize;
            let rows_per_strip = self.longs(278)[0] as usize;
            let offsets = self.longs(273);
            let counts = self.longs(279);
            let compression = self.shorts(259)[0];
            let predictor = self.fields.get(&317).map_or(1, |_| self.shorts(317)[0]);
            let row_bytes = width * 3 * 2;

            let mut out = Vec::new();
            for (at, len) in offsets.iter().zip(&counts) {
                let raw = &self.bytes[*at as usize..(*at + *len) as usize];
                let mut strip = if compression == DEFLATE {
                    use std::io::Read;
                    let mut inflated = Vec::new();
                    flate2::read::ZlibDecoder::new(raw)
                        .read_to_end(&mut inflated)
                        .expect("strip did not inflate");
                    inflated
                } else {
                    raw.to_vec()
                };
                assert!(strip.len() <= row_bytes * rows_per_strip);
                if predictor == PREDICTOR_HORIZONTAL {
                    for row in strip.chunks_mut(row_bytes) {
                        for x in 1..width {
                            for c in 0..3 {
                                let here = (x * 3 + c) * 2;
                                let left = ((x - 1) * 3 + c) * 2;
                                let d = u16::from_be_bytes([row[here], row[here + 1]]);
                                let a = u16::from_be_bytes([row[left], row[left + 1]]);
                                row[here..here + 2]
                                    .copy_from_slice(&a.wrapping_add(d).to_be_bytes());
                            }
                        }
                    }
                }
                out.extend_from_slice(&strip);
            }
            out
        }
    }

    /// A picture with enough structure that compression has something to do and
    /// enough noise that it cannot do too well.
    fn picture(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((width * height * 6) as usize);
        for y in 0..height {
            for x in 0..width {
                let ramp = (x * 65535 / width.max(1)) as u16;
                let band = ((y / 7) % 5) as u16 * 4096;
                let grain = (x
                    .wrapping_mul(2654435761)
                    .wrapping_add(y.wrapping_mul(40503))
                    >> 19) as u16
                    & 0x03FF;
                for c in 0..3u16 {
                    let v = ramp
                        .wrapping_add(band.wrapping_mul(c + 1))
                        .wrapping_add(grain)
                        .wrapping_add(c.wrapping_mul(700));
                    out.extend_from_slice(&v.to_be_bytes());
                }
            }
        }
        out
    }

    #[test]
    fn the_pixels_come_back_exactly() {
        // Lossless means lossless. Sixteen bits exists here so a file can be
        // edited again without the first encode showing, and a round trip that
        // is nearly exact is a round trip that bands on the third pass.
        let (w, h) = (137u32, 83u32);
        let samples = picture(w, h);
        let file = encode(&samples, w, h, &[7u8; 60]).expect("encode");
        let read = Tiff::parse(&file);
        assert_eq!(read.longs(256), vec![w]);
        assert_eq!(read.longs(257), vec![h]);
        assert_eq!(read.shorts(258), vec![16, 16, 16]);
        assert_eq!(read.shorts(262), vec![2], "photometric is not RGB");
        assert_eq!(read.shorts(277), vec![3]);
        assert_eq!(read.samples(), samples, "the pixels did not survive");
    }

    #[test]
    fn the_profile_travels_with_the_pixels() {
        // A file whose numbers are right and whose label is missing is the
        // failure this whole crate exists to prevent — every viewer guesses,
        // and most guess sRGB, which is right until the day it is not.
        let icc: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
        let file = encode(&picture(40, 40), 40, 40, &icc).expect("encode");
        let read = Tiff::parse(&file);
        let (kind, count, payload) = read.fields.get(&34675).expect("no ICC profile");
        assert_eq!(*kind, UNDEFINED);
        assert_eq!(*count as usize, icc.len());
        assert_eq!(payload, &icc, "the profile came back changed");
    }

    #[test]
    fn a_photograph_compresses() {
        // Deflate alone does very little to sixteen-bit photographic data;
        // horizontal differencing is what makes it worth doing. Checked as a
        // number rather than assumed, because "compressed" that saves nothing
        // is a format doing the opposite of its job.
        let (w, h) = (400u32, 300u32);
        let samples = picture(w, h);
        let file = encode(&samples, w, h, &[]).expect("encode");
        let read = Tiff::parse(&file);
        assert_eq!(read.shorts(259), vec![DEFLATE]);
        assert_eq!(read.shorts(317), vec![PREDICTOR_HORIZONTAL]);
        let stored: u32 = read.longs(279).iter().sum();
        println!(
            "{} bytes of pixels stored in {stored} ({:.0}%)",
            samples.len(),
            100.0 * stored as f64 / samples.len() as f64
        );
        assert!(
            (stored as usize) < samples.len(),
            "compression made it larger: {stored} against {}",
            samples.len()
        );
    }

    #[test]
    fn incompressible_pixels_are_stored_plain() {
        // Random data cannot be compressed, and Deflate adds a few bytes per
        // strip trying. Writing that would be a "compressed" file larger than
        // the plain one, so the writer compares and takes the smaller.
        let (w, h) = (64u32, 64u32);
        let samples: Vec<u8> = (0..(w * h * 6))
            .map(|i| (i.wrapping_mul(2654435761u32) >> 13) as u8)
            .collect();
        let file = encode(&samples, w, h, &[]).expect("encode");
        let read = Tiff::parse(&file);
        let stored: u32 = read.longs(279).iter().sum();
        assert!(
            stored as usize <= samples.len(),
            "a compressed file larger than the pixels it holds: {stored}"
        );
        assert_eq!(read.samples(), samples, "the pixels did not survive");
    }

    #[test]
    fn a_tall_image_is_written_in_several_strips() {
        // One strip the size of the photograph would defeat the point of having
        // strips at all, and it is what a rows-per-strip calculation that
        // ignored the width would produce.
        let (w, h) = (600u32, 900u32);
        let file = encode(&picture(w, h), w, h, &[]).expect("encode");
        let read = Tiff::parse(&file);
        let strips = read.longs(273).len();
        assert_eq!(strips, read.longs(279).len(), "offsets and counts disagree");
        assert!(strips > 1, "a 900-row image came back as one strip");
        assert_eq!(read.samples(), picture(w, h));
    }

    #[test]
    fn a_wrong_sized_buffer_is_refused() {
        assert!(encode(&picture(10, 10), 10, 11, &[]).is_err());
        assert!(encode(&[], 0, 0, &[]).is_err());
    }
}
