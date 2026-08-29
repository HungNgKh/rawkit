//! Does an export carry the edit you made, or the photograph as shot?
//!
//! The loop this closes is the whole point of the command, and it is the kind of
//! claim that is easy to believe and easy to get wrong: `render` looked like it
//! exported your work for months and never read a catalog at all. So this drives
//! the real binary — scan, store an edit, export twice — and compares the pixels.
//!
//! `cargo test -p rawkit-cli --test export_edit -- --ignored --nocapture`

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture() -> Option<PathBuf> {
    let dir = std::env::var("RAWKIT_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("rawkit-fixtures")
        });
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("arw")))
}

fn rawkit(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_rawkit"))
        .args(args)
        .output()
        .expect("running rawkit");
    assert!(
        out.status.success(),
        "rawkit {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Mean of the green channel, as a stand-in for how bright the picture is.
fn brightness(path: &Path) -> f64 {
    let bytes = std::fs::read(path).expect("an exported file");
    let (rgba, width, height) = rawkit_export::decode(&bytes).expect("decode");
    let total: u64 = rgba.chunks_exact(4).map(|p| p[1] as u64).sum();
    total as f64 / (width as f64 * height as f64)
}

#[test]
#[ignore = "requires a GPU adapter and a RAW fixture"]
fn an_export_carries_the_stored_edit_rather_than_the_photograph_as_shot() {
    let Some(raw) = fixture() else {
        panic!("no .ARW fixture; set RAWKIT_FIXTURES");
    };
    let dir = std::env::temp_dir().join(format!("rawkit-export-loop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let photos = dir.join("photos");
    std::fs::create_dir_all(&photos).unwrap();
    std::fs::copy(&raw, photos.join("ONE.ARW")).unwrap();

    let library = dir.join("lib.rawkit");
    rawkit(&[
        "catalog",
        library.to_str().unwrap(),
        "--scan",
        photos.to_str().unwrap(),
    ]);

    // As shot.
    let plain = dir.join("as-shot");
    rawkit(&[
        "export",
        library.to_str().unwrap(),
        "--to",
        plain.to_str().unwrap(),
        "--all",
        "--max-dim",
        "800",
    ]);

    // Now store an edit the way the shell does, and export again.
    {
        let catalog = rawkit_catalog::db::Catalog::open(&library).unwrap();
        let image = rawkit_catalog::cull::sequence(&catalog).unwrap()[0].id;
        let state = rawkit_editstate::EditState {
            tone: rawkit_editstate::Tone {
                exposure_ev: 2.0,
                ..Default::default()
            },
            ..Default::default()
        };
        rawkit_catalog::edits::save(&catalog, image, &state, rawkit_editstate::EditSource::User)
            .unwrap();
    }
    let edited = dir.join("edited");
    rawkit(&[
        "export",
        library.to_str().unwrap(),
        "--to",
        edited.to_str().unwrap(),
        "--all",
        "--max-dim",
        "800",
    ]);

    let before = brightness(&plain.join("ONE.jpg"));
    let after = brightness(&edited.join("ONE.jpg"));
    println!("as shot {before:.1}, +2 EV {after:.1}");
    assert!(
        after > before + 20.0,
        "two stops should be plainly brighter: {before:.1} then {after:.1}"
    );

    // And an export refuses to replace a file it did not just write.
    let out = Command::new(env!("CARGO_BIN_EXE_rawkit"))
        .args([
            "export",
            library.to_str().unwrap(),
            "--to",
            edited.to_str().unwrap(),
            "--all",
        ])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("skipped"),
        "a second export should have skipped rather than overwritten"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
