//! Decode a real RAW file.
//!
//! Fixtures are large and not redistributable, so they live outside the repo:
//! set `RAWKIT_FIXTURES` to a directory of RAW files, or use the default
//! `~/rawkit-fixtures`. A missing fixture fails loudly with instructions rather
//! than passing quietly — a decode test that silently does nothing is worse than
//! no decode test, because it reports green.
//!
//! `cargo test -p rawkit-decode -- --ignored`

use rawkit_decode::{decode_file, read_metadata, CfaPattern};
use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("RAWKIT_FIXTURES") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").expect("HOME is set");
    PathBuf::from(home).join("rawkit-fixtures")
}

fn any_arw() -> PathBuf {
    let dir = fixture_dir();
    let found = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| {
            panic!(
                "no fixture directory at {}: {e}\n\
             Put a RAW file there or set RAWKIT_FIXTURES.",
                dir.display()
            )
        })
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("arw")));
    found.unwrap_or_else(|| panic!("no .ARW file in {}", dir.display()))
}

#[test]
#[ignore = "requires a RAW fixture"]
fn decodes_a_sony_arw_to_a_plausible_mosaic() {
    let path = any_arw();
    let raw = decode_file(&path).expect("decode failed");

    println!("file    : {}", path.display());
    println!("camera  : {} {}", raw.camera.make, raw.camera.model);
    println!("serial  : {:?}", raw.camera.serial);
    println!("size    : {}x{}", raw.width, raw.height);
    println!("cfa     : {:?}", raw.cfa);
    println!("black   : {:?}", raw.levels.black);
    println!("white   : {}", raw.levels.white);
    println!("as shot : {:?}", raw.as_shot_neutral);

    assert_eq!(raw.camera.make, "Sony");
    assert!(
        raw.width > 1000 && raw.height > 1000,
        "implausible geometry"
    );
    assert!(
        raw.cfa.is_bayer(),
        "expected a Bayer sensor, got {:?}",
        raw.cfa
    );
    assert_eq!(raw.data.len(), (raw.width * raw.height) as usize);

    // The levels have to be usable as levels: black below white, and the data
    // actually spanning a useful part of the range. A decoder that returns a
    // black frame, or one where everything clips, passes a length check and
    // fails this.
    let white = raw.levels.white;
    assert!(raw.levels.black.iter().all(|&b| b < white));
    let max = *raw.data.iter().max().unwrap();
    let min = *raw.data.iter().min().unwrap();
    assert!(min < max, "the mosaic is a constant value");
    assert!(
        max as f32 > white as f32 * 0.1,
        "nothing in the frame reaches a tenth of full scale ({max} of {white}); \
         the data is probably not what we think it is"
    );

    // As-shot multipliers are per channel and green-referenced. Red and blue
    // are above green for essentially every daylight-ish scene, and a
    // multiplier of zero means we read the wrong field.
    assert!(
        raw.as_shot_neutral[..3].iter().all(|&m| m > 0.0),
        "as-shot white balance is missing: {:?}",
        raw.as_shot_neutral
    );

    // The two greens carry separate black levels in LibRaw's model. Everything
    // downstream treats green as one channel, so a file where they differ would
    // be rendered with a faint grid — worth knowing about if it ever appears.
    assert_eq!(
        raw.levels.black[1], raw.levels.black[3],
        "this file has different black levels for the two greens; \
         the single-green assumption downstream no longer holds"
    );
}

#[test]
#[ignore = "requires a RAW fixture"]
fn reads_metadata_without_decoding_and_agrees_with_the_decode() {
    let path = any_arw();
    let meta = read_metadata(&path).expect("metadata read failed");

    println!("camera  : {} {}", meta.camera.make, meta.camera.model);
    println!("serial  : {:?}", meta.camera.serial);
    println!("taken   : {:?}", meta.captured_at);
    println!("shutter : {:?}", meta.shutter_count);
    println!("lens    : {:?}", meta.lens);

    // The catalog's duplicate-detection triple. Every part of it is a field a
    // body may not record, so this is what proves the supported one does.
    assert!(meta.captured_at.is_some(), "no capture time");
    assert!(meta.camera.serial.is_some(), "no body serial");
    assert!(meta.shutter_count.is_some(), "no shutter count");

    // 2010-01-01 to 2100-01-01. A wall clock misparsed by a digit lands outside
    // this; one shifted by a timezone does not, which is why the exact
    // conversion is pinned by a unit test against a known string instead.
    let taken = meta.captured_at.unwrap();
    assert!(
        (1_262_304_000..4_102_444_800).contains(&taken),
        "capture time {taken} is not a plausible date"
    );

    // Two code paths read the same struct, and only one of them unpacks. If
    // they ever disagree about which camera took the picture, the cheap path is
    // reading a field the expensive path does not.
    let decoded = decode_file(&path).expect("decode failed");
    assert_eq!(meta.camera, decoded.camera);
    // And about how big it is. The shell builds a viewport from these numbers
    // before the file is decoded, so a mismatch would place a photograph
    // slightly wrong and only for the moment before its pixels arrive.
    assert_eq!(
        (meta.width, meta.height),
        (decoded.width, decoded.height),
        "the cheap path and the expensive one disagree about the geometry"
    );
}

#[test]
#[ignore = "requires a RAW fixture"]
fn a_missing_file_is_an_error_not_a_panic() {
    let err = decode_file(&fixture_dir().join("definitely-not-here.ARW"));
    assert!(err.is_err());
}

#[test]
fn xtrans_is_reported_as_non_bayer() {
    // Cheap guard on the mapping used by the decoder, without needing a Fuji
    // fixture: the demosaic path keys off this and would otherwise run a Bayer
    // kernel over a 6x6 sensor.
    assert!(!CfaPattern::XTrans.is_bayer());
}
