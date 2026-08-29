//! Reading a capture time out of a TIFF-based RAW, without a vendor's help.
//!
//! # Why this exists
//!
//! Capture time was coming from Sony's maker note, which works for the one body
//! this project targets and produces **nothing at all** for any other. Capture
//! time is not decoration: it orders the library, it orders a cull, and it is one
//! third of the duplicate-detection triple. A column that is NULL for most
//! cameras is a support question waiting to happen.
//!
//! LibRaw does expose a timestamp, and it is deliberately not used — see
//! [`crate::RawMetadata::captured_at`]. It runs the EXIF string through `mktime`,
//! so the value depends on the timezone of the machine that read the file. What
//! is wanted is the characters the camera wrote, and the only general way to get
//! those is to read the tag.
//!
//! # Reading a file we did not write
//!
//! Every offset is checked against the file's length before it is used, entry
//! counts are capped, and nothing here recurses. A RAW arrives from a camera, a
//! card reader, or a stranger, and a parser that trusts its offsets is a parser
//! that can be handed a file which reads somewhere it should not.
//!
//! Reads are seeks rather than a slurp: five small ones, precise, with no guess
//! about how far into the file the tags might be. Reading a prefix and hoping is
//! the other way to write this, and it fails silently on the files that put their
//! Exif IFD late.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// TIFF tag numbers, by the names the specification gives them.
const EXIF_IFD_POINTER: u16 = 0x8769;
const DATE_TIME_ORIGINAL: u16 = 0x9003;
const DATE_TIME_DIGITIZED: u16 = 0x9004;
/// IFD0's own, which is the file's modification time rather than the shutter's.
/// Last resort, and only because a file with nothing else is better served by an
/// approximate date than by none.
const DATE_TIME: u16 = 0x0132;

/// More entries than any real IFD has. A corrupt count is otherwise an
/// invitation to read twelve bytes a million times.
const MAX_ENTRIES: u16 = 512;
/// `YYYY:MM:DD HH:MM:SS` plus its terminator.
const TIMESTAMP_LEN: usize = 20;

/// The camera's own clock at capture, as the characters it wrote.
///
/// `None` when the file is not TIFF-based, has no such tag, or is damaged in any
/// of the ways a bounds check catches. Never an error: a photograph with no
/// readable date is an ordinary thing, and the caller records the absence.
pub fn capture_time(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();

    let mut header = [0u8; 8];
    file.read_exact(&mut header).ok()?;
    let big_endian = match &header[..2] {
        b"MM" => true,
        b"II" => false,
        _ => return None,
    };
    let read16 = |b: &[u8]| -> u16 {
        if big_endian {
            u16::from_be_bytes([b[0], b[1]])
        } else {
            u16::from_le_bytes([b[0], b[1]])
        }
    };
    let read32 = |b: &[u8]| -> u32 {
        if big_endian {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        }
    };
    // 42, which is the TIFF magic and the reason to be sure of the byte order
    // before trusting anything else.
    if read16(&header[2..4]) != 42 {
        return None;
    }

    let ifd0 = read32(&header[4..8]) as u64;
    let entries0 = read_ifd(&mut file, ifd0, length, &read16)?;

    // The Exif IFD holds the shutter's own time. IFD0's DATE_TIME is the file's,
    // which for a camera is usually the same and for an edited file is not.
    if let Some(exif) = find(&entries0, EXIF_IFD_POINTER, &read16, &read32) {
        if let Some(entries) = read_ifd(&mut file, exif as u64, length, &read16) {
            for tag in [DATE_TIME_ORIGINAL, DATE_TIME_DIGITIZED] {
                if let Some(at) = find(&entries, tag, &read16, &read32) {
                    if let Some(text) = read_timestamp(&mut file, at as u64, length) {
                        return Some(text);
                    }
                }
            }
        }
    }
    let at = find(&entries0, DATE_TIME, &read16, &read32)?;
    read_timestamp(&mut file, at as u64, length)
}

/// One IFD's entries, as raw twelve-byte records.
fn read_ifd(
    file: &mut File,
    at: u64,
    length: u64,
    read16: &impl Fn(&[u8]) -> u16,
) -> Option<Vec<[u8; 12]>> {
    if at < 8 || at + 2 > length {
        return None;
    }
    file.seek(SeekFrom::Start(at)).ok()?;
    let mut count = [0u8; 2];
    file.read_exact(&mut count).ok()?;
    let count = read16(&count).min(MAX_ENTRIES);

    let bytes = count as u64 * 12;
    if at + 2 + bytes > length {
        return None;
    }
    let mut raw = vec![0u8; bytes as usize];
    file.read_exact(&mut raw).ok()?;
    Some(
        raw.chunks_exact(12)
            .map(|c| {
                let mut entry = [0u8; 12];
                entry.copy_from_slice(c);
                entry
            })
            .collect(),
    )
}

/// The value field of `tag`, read as an offset.
///
/// Both tags this looks for hold something longer than four bytes — a pointer,
/// or a twenty-character string — so the field is always an offset and never an
/// inline value. That is TIFF's classic trap and it is worth saying out loud: a
/// shorter value would be *in* those four bytes rather than at them.
fn find(
    entries: &[[u8; 12]],
    tag: u16,
    read16: &impl Fn(&[u8]) -> u16,
    read32: &impl Fn(&[u8]) -> u32,
) -> Option<u32> {
    entries
        .iter()
        .find(|entry| read16(&entry[..2]) == tag)
        .map(|entry| read32(&entry[8..12]))
}

fn read_timestamp(file: &mut File, at: u64, length: u64) -> Option<String> {
    if at < 8 || at + TIMESTAMP_LEN as u64 > length {
        return None;
    }
    file.seek(SeekFrom::Start(at)).ok()?;
    let mut raw = [0u8; TIMESTAMP_LEN];
    file.read_exact(&mut raw).ok()?;
    let text: String = raw
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| *b as char)
        .collect();
    // A camera with no clock set writes zeroes, and the caller's range check
    // would reject them anyway — but returning them as a string would make an
    // absent date look like a present one to anything that only checks for None.
    (text.len() >= 19 && text.starts_with(|c: char| c.is_ascii_digit())).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The smallest TIFF that carries a capture time: a header, IFD0 with a
    /// pointer to an Exif IFD, that IFD with `DateTimeOriginal`, and the string.
    ///
    /// Built rather than committed, so the test needs no fixture and so both
    /// byte orders are exercised on every machine — a parser that only ever sees
    /// little-endian files is a parser with an untested half.
    fn tiff(big_endian: bool, tag: u16, when: &[u8]) -> Vec<u8> {
        let u16b = |v: u16| {
            if big_endian {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        };
        let u32b = |v: u32| {
            if big_endian {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        };

        // Layout: header 8, IFD0 at 8 (1 entry = 2 + 12 + 4 = 18), Exif IFD at
        // 26 (1 entry = 18), string at 44.
        let (ifd0, exif_ifd, text_at) = (8u32, 26u32, 44u32);
        let mut out = Vec::new();
        out.extend_from_slice(if big_endian { b"MM" } else { b"II" });
        out.extend(u16b(42));
        out.extend(u32b(ifd0));

        out.extend(u16b(1));
        out.extend(u16b(EXIF_IFD_POINTER));
        out.extend(u16b(4)); // LONG
        out.extend(u32b(1));
        out.extend(u32b(exif_ifd));
        out.extend(u32b(0)); // no next IFD

        out.extend(u16b(1));
        out.extend(u16b(tag));
        out.extend(u16b(2)); // ASCII
        out.extend(u32b(TIMESTAMP_LEN as u32));
        out.extend(u32b(text_at));
        out.extend(u32b(0));

        assert_eq!(out.len(), text_at as usize, "layout drifted");
        out.extend_from_slice(when);
        out
    }

    fn write(bytes: &[u8], name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("rawkit-exif-{}-{name}", std::process::id()));
        let mut file = File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        path
    }

    #[test]
    fn a_capture_time_is_read_in_either_byte_order() {
        for big_endian in [false, true] {
            let bytes = tiff(big_endian, DATE_TIME_ORIGINAL, b"2019:09:12 17:36:01\0");
            let path = write(&bytes, if big_endian { "mm" } else { "ii" });
            assert_eq!(
                capture_time(&path).as_deref(),
                Some("2019:09:12 17:36:01"),
                "byte order big={big_endian}"
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn a_file_that_is_not_tiff_is_none_rather_than_a_guess() {
        let path = write(b"not a tiff at all, not even close", "junk");
        assert_eq!(capture_time(&path), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_offset_past_the_end_of_the_file_is_refused() {
        // The failure that matters for a file somebody else produced. Truncating
        // after the tags leaves every offset pointing outside the file, and a
        // parser that trusts them reads whatever is there.
        let full = tiff(false, DATE_TIME_ORIGINAL, b"2019:09:12 17:36:01\0");
        for cut in [10, 26, 30, 44, 50] {
            let path = write(&full[..cut.min(full.len())], &format!("cut{cut}"));
            // No panic and no nonsense: either the real answer or nothing.
            let got = capture_time(&path);
            assert!(
                got.is_none() || got.as_deref() == Some("2019:09:12 17:36:01"),
                "truncated at {cut} produced {got:?}"
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn an_unset_camera_clock_is_not_a_date() {
        // Zeroes are what a camera with a flat battery writes, and returning
        // them as a string would make an absent date look present to anything
        // that only checks for None.
        let bytes = tiff(false, DATE_TIME_ORIGINAL, &[0u8; TIMESTAMP_LEN]);
        let path = write(&bytes, "zeroes");
        assert_eq!(capture_time(&path), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ifd0s_own_date_is_the_last_resort() {
        // A file with no Exif IFD at all still has a date worth having, and it
        // is better than nothing — but it is the file's time, not the shutter's,
        // which is why it comes last.
        let mut bytes = tiff(false, DATE_TIME_ORIGINAL, b"2019:09:12 17:36:01\0");
        // Point IFD0's single entry at DATE_TIME instead, with the string inline
        // at the same place, and break the Exif pointer.
        bytes[10..12].copy_from_slice(&DATE_TIME.to_le_bytes());
        bytes[12..14].copy_from_slice(&2u16.to_le_bytes());
        bytes[14..18].copy_from_slice(&(TIMESTAMP_LEN as u32).to_le_bytes());
        bytes[18..22].copy_from_slice(&44u32.to_le_bytes());
        let path = write(&bytes, "ifd0");
        assert_eq!(capture_time(&path).as_deref(), Some("2019:09:12 17:36:01"));
        let _ = std::fs::remove_file(&path);
    }
}
