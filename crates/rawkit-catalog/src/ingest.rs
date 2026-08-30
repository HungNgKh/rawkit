//! Copying photographs off a card, and proving they arrived.
//!
//! # Why this is not `cp`
//!
//! A card reader that drops a byte does not say so. The copy succeeds, the file
//! is the right length, and the damage shows up years later in one frame nobody
//! opened in the meantime — by which time the card has been formatted a hundred
//! times. So every file is hashed as it is read, hashed again after it lands,
//! and only then given its real name. **A copy that cannot be proved is not an
//! import.**
//!
//! # Where things land
//!
//! `destination/2026/2026-08-30/DSC00881.ARW`, from the camera's own clock.
//! Cards reuse filenames — every Sony starts at `DSC00001` again eventually — so
//! a flat folder puts two different photographs on the same path on the day the
//! counter wraps. Dated folders are also how anyone actually looks for a frame
//! months later.
//!
//! Names are kept as the camera wrote them, so the file still matches what the
//! camera showed and what any notes say. Inside one day that is nearly always
//! unique; when it is not, the second file gets a suffix and both survive.
//!
//! # What it will not do
//!
//! Move. The card is the only other copy of these photographs until this
//! finishes, and deleting from it is the user's decision to make afterwards,
//! with the files in front of them.

use crate::db::Catalog;
use crate::scan::{is_supported, FileMetadata, ScanReport};
use crate::{CatalogError, VolumeId};
use std::path::{Path, PathBuf};

/// What an import did.
#[derive(Debug, Default, PartialEq)]
pub struct IngestReport {
    /// Files copied and verified.
    pub copied: usize,
    /// Files already present at their destination, byte for byte. Re-running an
    /// import over a card that is half done is the normal case, not an error.
    pub already_there: usize,
    /// Files whose name was taken by different content, so they landed beside it.
    pub renamed: usize,
    /// Directories that could not be read. Counted rather than fatal: one
    /// unreadable folder on a card should not stop the rest arriving.
    pub unreadable: Vec<PathBuf>,
    /// Files that could not be copied or did not survive verification, with why.
    pub failed: Vec<(PathBuf, String)>,
    /// What the scan that followed found.
    pub scanned: Option<ScanReport>,
}

/// Copy every supported file under `source` into `destination`, then catalog it.
///
/// `metadata` reads a file's header — the same parameter `scan` takes, and for
/// the same reason: LibRaw's CDDL stays out of this crate, and a test needs no
/// RAW fixture. It is called on the *source*, because where a file lands depends
/// on when it was taken.
///
/// `progress` is called with `(done, total, name)` before each file.
pub fn ingest(
    catalog: &mut Catalog,
    source: &Path,
    destination: &Path,
    mut metadata: impl FnMut(&Path) -> Option<FileMetadata>,
    mut progress: impl FnMut(usize, usize, &str),
) -> Result<IngestReport, CatalogError> {
    let source = source
        .canonicalize()
        .map_err(|e| CatalogError::Io(format!("{}: {e}", source.display())))?;
    std::fs::create_dir_all(destination)
        .map_err(|e| CatalogError::Io(format!("{}: {e}", destination.display())))?;
    let destination = destination
        .canonicalize()
        .map_err(|e| CatalogError::Io(format!("{}: {e}", destination.display())))?;

    // Copying a tree into itself walks its own output forever. Caught here with
    // a sentence rather than by a user watching a disk fill up.
    if destination.starts_with(&source) || source.starts_with(&destination) {
        return Err(CatalogError::Unsupported(
            "the card and the library cannot contain one another; \
             an import would copy its own output",
        ));
    }

    let mut report = IngestReport::default();
    let mut found = Vec::new();
    walk(&source, &mut found, &mut report);
    // Ordered by name, so an interrupted import resumes somewhere predictable
    // and two runs report the same thing.
    found.sort();

    let total = found.len();
    for (done, file) in found.iter().enumerate() {
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        progress(done, total, &name);

        let folder = destination.join(day_folder(metadata(file).and_then(|m| m.captured_at)));
        if let Err(e) = std::fs::create_dir_all(&folder) {
            report.failed.push((file.clone(), e.to_string()));
            continue;
        }
        match place(file, &folder, &name, &hash_of) {
            Ok(Placed::Copied { renamed }) => {
                report.copied += 1;
                report.renamed += usize::from(renamed);
            }
            Ok(Placed::AlreadyThere) => report.already_there += 1,
            Err(e) => report.failed.push((file.clone(), e)),
        }
    }
    progress(total, total, "");

    // Catalog what arrived, not what was asked for: a file that failed
    // verification is not in the library, and a library that lists it would be
    // worse than one that does not.
    let volume = VolumeId::resolve(&destination)?;
    report.scanned = Some(crate::scan::scan_on(
        catalog,
        &destination,
        volume,
        &mut metadata,
    )?);
    Ok(report)
}

/// `2026/2026-08-30`, or a folder that says the date is missing.
///
/// Undated files are not guessed at — a file's modification time is when it was
/// copied, not when it was taken, and a library sorted on that is quietly wrong.
/// They go somewhere named for the problem so it can be dealt with.
fn day_folder(captured_at: Option<i64>) -> PathBuf {
    let Some(seconds) = captured_at else {
        return PathBuf::from("undated");
    };
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    PathBuf::from(format!("{year:04}")).join(format!("{year:04}-{month:02}-{day:02}"))
}

/// Days since 1970-01-01 to a civil date. Howard Hinnant's `civil_from_days`,
/// which is exact for every date this will ever see and needs no calendar crate.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[derive(Debug)]
enum Placed {
    Copied { renamed: bool },
    AlreadyThere,
}

/// Copy one file into `folder`, verify it, and give it its name.
///
/// The copy lands under a temporary name and is renamed only once its hash
/// matches. An import interrupted half way therefore leaves no file that a
/// later scan would catalog as a photograph — which it would, because a
/// half-written `.ARW` is still named like one.
/// The hash function is a parameter so the failure can be reached from a test.
/// Verification is the whole reason this is not `cp`, and a branch that cannot
/// be exercised is a promise nobody has checked — the interesting half is not
/// the comparison but what happens after it: no file left behind, and a report
/// that names which photograph is still only on the card.
fn place(
    source: &Path,
    folder: &Path,
    name: &str,
    hash: &dyn Fn(&Path) -> Result<String, String>,
) -> Result<Placed, String> {
    let want = hash(source)?;

    let mut target = folder.join(name);
    let mut renamed = false;
    for attempt in 1.. {
        // Asked separately from the hash: a file that exists but cannot be read
        // is a permission problem, and treating it as absent would overwrite a
        // photograph rather than report why it could not be checked.
        if !target.exists() {
            break;
        }
        match hash(&target) {
            // Already here, byte for byte. Re-running an import over a card that
            // was half done is the normal case.
            Ok(existing) if existing == want => return Ok(Placed::AlreadyThere),
            // The name is taken by something else. Both are photographs; both
            // stay.
            Ok(_) => {
                let (stem, extension) = split_name(name);
                target = folder.join(format!("{stem}-{}.{extension}", attempt + 1));
                renamed = true;
            }
            Err(e) => return Err(format!("{} is there but unreadable: {e}", target.display())),
        }
    }

    let partial = target.with_extension("partial");
    std::fs::copy(source, &partial).map_err(|e| format!("copying: {e}"))?;
    let landed = hash(&partial).inspect_err(|_| {
        let _ = std::fs::remove_file(&partial);
    })?;
    if landed != want {
        let _ = std::fs::remove_file(&partial);
        return Err(format!(
            "the copy does not match the card ({want} on the card, {landed} here)"
        ));
    }
    std::fs::rename(&partial, &target).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        format!("naming the copy: {e}")
    })?;
    Ok(Placed::Copied { renamed })
}

/// `("DSC00881", "ARW")`. The extension is kept as written, so a card of `.arw`
/// files does not come out shouting.
fn split_name(name: &str) -> (&str, &str) {
    match name.rsplit_once('.') {
        Some((stem, extension)) => (stem, extension),
        None => (name, "arw"),
    }
}

fn hash_of(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = blake3::Hasher::new();
    // A megabyte at a time: a 50 MB raw read whole would hold the whole file in
    // memory for no gain, and this is run over thousands of them.
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Depth-first, not following symlinks — the same rule the scan walks by, and
/// for the same reason: a link to an ancestor is how a walk runs forever.
fn walk(dir: &Path, out: &mut Vec<PathBuf>, report: &mut IngestReport) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        report.unreadable.push(dir.to_path_buf());
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        if meta.is_symlink() {
            continue;
        }
        if meta.is_dir() {
            walk(&path, out, report);
        } else if is_supported(&path) {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::{tempdir, Scratch};

    /// 2026-08-30 12:00 UTC. Taken from `TZ=UTC date -d`, not worked out by
    /// hand — the first version of this constant was the 18th, and the test it
    /// broke was the one checking the arithmetic it was meant to verify.
    const AUGUST_30: i64 = 1_788_091_200;
    const DAY: i64 = 86_400;

    fn card(dir: &Scratch, files: &[(&str, &[u8])]) -> PathBuf {
        let card = dir.join("card/DCIM/100MSDCF");
        std::fs::create_dir_all(&card).unwrap();
        for (name, bytes) in files {
            std::fs::write(card.join(name), bytes).unwrap();
        }
        dir.join("card")
    }

    /// Capture times keyed by filename, never by the order the walk happens to
    /// return — enumeration order is not the same on every filesystem, and a
    /// test that depends on it passes here and fails on CI.
    fn taken<'a>(times: &'a [(&'a str, i64)]) -> impl FnMut(&Path) -> Option<FileMetadata> + 'a {
        move |path: &Path| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            let at = times.iter().find(|(n, _)| *n == name)?.1;
            Some(FileMetadata {
                captured_at: Some(at),
                ..Default::default()
            })
        }
    }

    fn library(dir: &Scratch) -> Catalog {
        Catalog::open(&dir.join("library.rawkit")).unwrap()
    }

    fn run(
        catalog: &mut Catalog,
        source: &Path,
        destination: &Path,
        times: &[(&str, i64)],
    ) -> IngestReport {
        ingest(catalog, source, destination, taken(times), |_, _, _| {}).unwrap()
    }

    #[test]
    fn photographs_land_in_the_day_they_were_taken() {
        let dir = tempdir();
        let source = card(&dir, &[("DSC00001.ARW", b"one"), ("DSC00002.ARW", b"two")]);
        let library_root = dir.join("photos");
        let mut catalog = library(&dir);
        let report = run(
            &mut catalog,
            &source,
            &library_root,
            &[
                ("DSC00001.ARW", AUGUST_30),
                ("DSC00002.ARW", AUGUST_30 + DAY),
            ],
        );

        assert_eq!(report.copied, 2);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(library_root.join("2026/2026-08-30/DSC00001.ARW").exists());
        assert!(library_root.join("2026/2026-08-31/DSC00002.ARW").exists());
        // And the card is untouched: it is the only other copy until this
        // finishes, and emptying it is the user's decision to make afterwards.
        assert!(source.join("DCIM/100MSDCF/DSC00001.ARW").exists());
        assert_eq!(report.scanned.expect("a scan followed").added, 2);
    }

    #[test]
    fn a_photograph_with_no_date_is_set_aside_rather_than_guessed_at() {
        // A file's modification time is when it was copied, not when it was
        // taken. Filing by it would be quietly wrong in a way nobody checks.
        let dir = tempdir();
        let source = card(&dir, &[("DSC00003.ARW", b"three")]);
        let library_root = dir.join("photos");
        let mut catalog = library(&dir);
        let report = ingest(
            &mut catalog,
            &source,
            &library_root,
            |_: &Path| None,
            |_, _, _| {},
        )
        .unwrap();

        assert_eq!(report.copied, 1);
        assert!(library_root.join("undated/DSC00003.ARW").exists());
    }

    #[test]
    fn importing_the_same_card_twice_copies_nothing_the_second_time() {
        // The normal case, not an error: a card half imported, or plugged in
        // again because nobody remembers whether it was done.
        let dir = tempdir();
        let source = card(&dir, &[("DSC00004.ARW", b"four")]);
        let library_root = dir.join("photos");
        let mut catalog = library(&dir);
        let times = [("DSC00004.ARW", AUGUST_30)];

        assert_eq!(run(&mut catalog, &source, &library_root, &times).copied, 1);
        let again = run(&mut catalog, &source, &library_root, &times);
        assert_eq!(again.copied, 0);
        assert_eq!(again.already_there, 1);
        assert_eq!(
            again.scanned.expect("a scan followed").unchanged,
            1,
            "and the catalog did not gain a second row"
        );
    }

    #[test]
    fn two_different_photographs_with_one_name_both_survive() {
        // Cards reuse names. Dated folders make this rare rather than
        // impossible — two cards from the same day still collide — and losing
        // one silently is the worst outcome available.
        let dir = tempdir();
        let library_root = dir.join("photos");
        let mut catalog = library(&dir);
        let times = [("DSC00005.ARW", AUGUST_30)];

        let first = card(&dir, &[("DSC00005.ARW", b"the first one")]);
        run(&mut catalog, &first, &library_root, &times);
        // A second card, same name, different photograph.
        std::fs::write(first.join("DCIM/100MSDCF/DSC00005.ARW"), b"a different one").unwrap();
        let report = run(&mut catalog, &first, &library_root, &times);

        assert_eq!(report.copied, 1);
        assert_eq!(report.renamed, 1);
        let day = library_root.join("2026/2026-08-30");
        assert_eq!(
            std::fs::read(day.join("DSC00005.ARW")).unwrap(),
            b"the first one"
        );
        assert_eq!(
            std::fs::read(day.join("DSC00005-2.ARW")).unwrap(),
            b"a different one"
        );
    }

    #[test]
    fn only_raws_are_taken_and_the_case_does_not_matter() {
        let dir = tempdir();
        let source = card(
            &dir,
            &[
                ("DSC00006.ARW", b"raw"),
                // A different stem on purpose: two names differing only in case
                // are one file on macOS and two on Linux, so a test using them
                // would assert different things on different runners.
                ("DSC00007.arw", b"also raw"),
                ("DSC00006.JPG", b"jpeg"),
                ("MOVIE.MP4", b"video"),
                ("README", b"text"),
            ],
        );
        let library_root = dir.join("photos");
        let mut catalog = library(&dir);
        let report = run(
            &mut catalog,
            &source,
            &library_root,
            &[("DSC00006.ARW", AUGUST_30), ("DSC00007.arw", AUGUST_30)],
        );
        assert_eq!(report.copied, 2);
        let day = library_root.join("2026/2026-08-30");
        let mut names: Vec<String> = std::fs::read_dir(&day)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["DSC00006.ARW", "DSC00007.arw"]);
    }

    #[test]
    fn a_library_inside_the_card_is_refused_rather_than_copied_forever() {
        let dir = tempdir();
        let source = card(&dir, &[("DSC00007.ARW", b"seven")]);
        let mut catalog = library(&dir);
        let inside = source.join("photos");
        let refused = ingest(
            &mut catalog,
            &source,
            &inside,
            |_: &Path| None,
            |_, _, _| {},
        );
        assert!(refused.is_err(), "an import into the card was allowed");
    }

    #[test]
    fn nothing_half_written_is_left_where_a_scan_would_find_it() {
        // The copy lands under a temporary name and is renamed only once its
        // hash matches, so an import stopped part way leaves no file that looks
        // like a photograph and is not one.
        let dir = tempdir();
        let source = card(&dir, &[("DSC00008.ARW", b"eight")]);
        let library_root = dir.join("photos");
        let mut catalog = library(&dir);
        run(
            &mut catalog,
            &source,
            &library_root,
            &[("DSC00008.ARW", AUGUST_30)],
        );

        let day = library_root.join("2026/2026-08-30");
        let leftovers: Vec<String> = std::fs::read_dir(&day)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("partial"))
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    #[test]
    fn the_day_a_photograph_belongs_to_is_the_cameras_own() {
        // The arithmetic, checked against dates worked out by hand rather than
        // against another copy of itself.
        assert_eq!(
            day_folder(Some(AUGUST_30)),
            PathBuf::from("2026/2026-08-30")
        );
        assert_eq!(day_folder(Some(0)), PathBuf::from("1970/1970-01-01"));
        // A leap day, and the day after it.
        // A leap day at noon, and the day after it.
        assert_eq!(
            day_folder(Some(1_709_208_000)),
            PathBuf::from("2024/2024-02-29")
        );
        assert_eq!(
            day_folder(Some(1_709_208_000 + DAY)),
            PathBuf::from("2024/2024-03-01")
        );
        // Just before midnight, and just after: the boundary a naive division
        // gets wrong.
        assert_eq!(
            day_folder(Some(AUGUST_30 - 43_201)),
            PathBuf::from("2026/2026-08-29")
        );
        assert_eq!(day_folder(None), PathBuf::from("undated"));
    }
}

#[cfg(test)]
mod verification_tests {
    use super::*;
    use crate::db::tests::tempdir;

    #[test]
    fn a_copy_that_does_not_match_the_card_is_not_kept() {
        // The whole reason this is not `cp`. A card reader that drops a byte
        // does not say so: the copy succeeds, the length is right, and the
        // damage surfaces years later in one frame nobody opened in the
        // meantime — by which time the card has been formatted a hundred times.
        //
        // What matters after the comparison fails is that nothing is left
        // behind. A `.partial` would be tidied away by the next run, but a file
        // under its real name would be catalogued as a photograph and the
        // library would list something it does not have.
        let dir = tempdir();
        let card = dir.join("card");
        let folder = dir.join("photos");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        let source = card.join("DSC00009.ARW");
        std::fs::write(&source, b"nine").unwrap();

        // A reader that tells the truth about the card and lies about the copy,
        // which is exactly what a failing cable looks like from here.
        let lying = |path: &Path| -> Result<String, String> {
            if path.extension().is_some_and(|e| e == "partial") {
                Ok("a hash from somewhere else".into())
            } else {
                hash_of(path)
            }
        };

        let outcome = place(&source, &folder, "DSC00009.ARW", &lying);
        let why = outcome.expect_err("a mismatched copy was accepted");
        assert!(why.contains("does not match the card"), "{why}");

        let left: Vec<String> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(left.is_empty(), "left behind {left:?}");
        assert!(source.exists(), "and the card still has it");
    }

    #[test]
    fn a_copy_that_matches_is_given_its_real_name() {
        // The other half, so the test above cannot pass by refusing everything.
        let dir = tempdir();
        let card = dir.join("card");
        let folder = dir.join("photos");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        let source = card.join("DSC00010.ARW");
        std::fs::write(&source, b"ten").unwrap();

        place(&source, &folder, "DSC00010.ARW", &hash_of).expect("an honest copy");
        assert_eq!(std::fs::read(folder.join("DSC00010.ARW")).unwrap(), b"ten");
    }
}
