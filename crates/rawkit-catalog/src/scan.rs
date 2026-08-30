//! Walking a folder into the catalog.
//!
//! Until this existed, every mechanism around it was unreachable: a schema with
//! no rows, backups of an empty file, a relink that had never seen a path a real
//! directory produced.
//!
//! # What a scan does not do
//!
//! **It does not hash.** A scan records `size` and `mtime` and leaves
//! `content_hash` NULL, which the column was declared nullable for. Hashing
//! twenty thousand raw files means reading half a terabyte — three minutes on an
//! internal SSD and closer to an hour on the external drive where photo
//! libraries actually live. That cost is real, so it is deferred and made
//! explicit: `hash_missing` fills them in when the user asks.
//!
//! **It does not delete.** A file that has gone is flagged `missing`, never
//! removed. An unplugged drive must not erase a library, and this is the vault's
//! "flagged on scan, not on access".
//!
//! **It does read metadata, and that is not a contradiction.** Capture time,
//! camera and lens come from a header parse — 0.6 ms per file against 120 ms for
//! a decode and considerably more for a hash — so a twenty-thousand-file library
//! costs about twelve seconds, not forty minutes. The cost that was deferred is
//! reading whole files; reading their first few kilobytes was never the same
//! expense, and a library you cannot sort by date is barely a library.
//!
//! **It does not depend on the decoder.** The reader arrives as a parameter, for
//! the reason `PathConvention` does: the catalog stays free of LibRaw and its
//! CDDL, and the failure path — a file with an `.ARW` name that is not a RAW —
//! is testable without one.

use crate::path::{CatalogPath, PathConvention};
use crate::{db::Catalog, CatalogError, VolumeId};
use std::path::{Path, PathBuf};

/// What a scan records about a file beyond its name and size.
///
/// Every field is optional because every field is something a particular camera
/// may not write. `rawkit_decode::read_metadata` is what fills this in; the
/// catalog only knows the shape.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileMetadata {
    /// The camera's wall clock at capture, read as if UTC. Never a converted
    /// instant: an EXIF capture time has no timezone, and applying the reading
    /// machine's is how one file gets two times in one library.
    pub captured_at: Option<i64>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub camera_serial: Option<String>,
    pub shutter_count: Option<i64>,
    pub lens: Option<String>,
}

/// A reader that reads nothing, for a caller that wants only the filesystem.
///
/// Spelled out rather than left as a closure so that skipping metadata is
/// visible at the call site as a decision.
pub fn no_metadata(_: &Path) -> Option<FileMetadata> {
    None
}

/// What a scan indexes. One constant, because the editor renders exactly one
/// thing: rows it cannot open would do nothing when clicked.
pub const EXTENSIONS: &[&str] = &["arw"];

/// What a scan did, for a caller to report.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
    /// Catalogued before, not found now.
    pub missing: usize,
    /// Directories that could not be read. A scan that aborts on one permission
    /// error is useless on a real disk, so these are counted and carried.
    pub unreadable: Vec<PathBuf>,
    /// Symlinks not followed, to avoid cycles and double-indexing.
    pub symlinks: usize,
    /// Files whose metadata could not be read — an `.ARW` that is not a RAW, a
    /// truncated download, a body this build does not know. The row is still
    /// catalogued; only its camera columns stay NULL.
    pub without_metadata: usize,
}

/// Index every supported file under `root`.
///
/// One transaction for the whole scan: a half-scanned catalog is worse than an
/// unscanned one, and SQLite is far faster batching inserts this way than
/// committing per row.
pub fn scan(
    catalog: &mut Catalog,
    root: &Path,
    metadata: impl FnMut(&Path) -> Option<FileMetadata>,
) -> Result<ScanReport, CatalogError> {
    let resolved = root
        .canonicalize()
        .map_err(|e| CatalogError::Io(format!("{}: {e}", root.display())))?;
    let volume = VolumeId::resolve(&resolved)?;
    scan_on(catalog, root, volume, metadata)
}

/// The same, against a volume the caller has already identified.
///
/// Split out because resolving a volume and walking a folder are different
/// concerns with different failure modes — and because tests should not need
/// the machine they run on to have a filesystem UUID. CI found that the hard
/// way: its runners are on a filesystem without one, so every scan test failed
/// on a refusal that was entirely correct.
pub fn scan_on(
    catalog: &mut Catalog,
    root: &Path,
    volume: VolumeId,
    mut metadata: impl FnMut(&Path) -> Option<FileMetadata>,
) -> Result<ScanReport, CatalogError> {
    let root = root
        .canonicalize()
        .map_err(|e| CatalogError::Io(format!("{}: {e}", root.display())))?;
    let convention = PathConvention::host();

    let mut report = ScanReport::default();
    let mut found = Vec::new();
    walk(&root, &mut found, &mut report);

    let transaction = catalog.connection_mut().transaction()?;
    let volume_id = upsert_volume(&transaction, &volume, &root, convention)?;
    let now = seconds_now();

    // Everything this scan touched, so the sweep below can tell absence from
    // "in a folder we did not visit".
    let mut seen = Vec::new();

    for file in &found {
        let parent = file.path.parent().unwrap_or(&root);
        let relative = parent.strip_prefix(&root).unwrap_or(Path::new(""));
        let folder_id = upsert_folder(&transaction, volume_id, relative, convention)?;
        let name = file
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let key = CatalogPath::new(Path::new(&name), convention)
            .map_err(|e| CatalogError::Io(e.to_string()))?;

        let existing: Option<(i64, i64, i64, bool)> = transaction
            .query_row(
                "SELECT id, size, mtime, captured_at IS NULL
                   FROM files WHERE folder_id = ?1 AND filename_key = ?2",
                rusqlite::params![folder_id, key.key()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .ok();

        // Read metadata for a file whose bytes we have not seen before, and for
        // one catalogued by an older build that never read any. Never for a file
        // that is unchanged and already described: that is what keeps a rescan
        // proportional to what moved rather than to the size of the library.
        let (id, wants_metadata) = match existing {
            Some((id, size, mtime, undescribed)) => {
                if size == file.size && mtime == file.mtime {
                    // Unchanged, but possibly previously flagged missing — a
                    // reconnected drive should clear that.
                    transaction.execute("UPDATE files SET missing = 0 WHERE id = ?1", [id])?;
                    report.unchanged += 1;
                    (id, undescribed)
                } else {
                    // Different bytes, so any hash we hold describes a file that
                    // no longer exists. Keeping it would be worse than having
                    // none, because relink trusts it — and everything the camera
                    // columns say is stale for exactly the same reason, so they
                    // are cleared here rather than merely overwritten below. If
                    // the new contents will not parse, the row must end up
                    // saying nothing, not saying what the old contents said.
                    transaction.execute(
                        "UPDATE files
                            SET size = ?2, mtime = ?3, content_hash = NULL, missing = 0,
                                captured_at = NULL, camera_make = NULL, camera_model = NULL,
                                camera_serial = NULL, shutter_count = NULL, lens = NULL
                          WHERE id = ?1",
                        rusqlite::params![id, file.size, file.mtime],
                    )?;
                    report.updated += 1;
                    (id, true)
                }
            }
            None => {
                transaction.execute(
                    "INSERT INTO files (folder_id, filename, filename_key, size, mtime, imported_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![folder_id, key.stored(), key.key(), file.size, file.mtime, now],
                )?;
                let id = transaction.last_insert_rowid();
                // One image per file. A virtual copy is something a person asks
                // for, never something a scan invents.
                transaction.execute(
                    "INSERT INTO images (file_id, created_at) VALUES (?1, ?2)",
                    rusqlite::params![id, now],
                )?;
                report.added += 1;
                (id, true)
            }
        };

        if wants_metadata {
            match metadata(&file.path) {
                Some(found) => write_metadata(&transaction, id, &found)?,
                // Left NULL rather than marked tried: the next scan retries,
                // which costs a header read and is the right answer when the
                // failure was a flaky drive rather than a file that is not a RAW.
                None => report.without_metadata += 1,
            }
        }
        seen.push(id);
    }

    report.missing = mark_missing(&transaction, volume_id, &seen)?;
    transaction.commit()?;
    Ok(report)
}

/// Record what the reader found, overwriting whatever was there.
///
/// Overwriting rather than merging is deliberate: this runs when the file is new
/// or its bytes have changed, and in the second case the old values describe a
/// photograph that is no longer in that file.
fn write_metadata(
    transaction: &rusqlite::Transaction<'_>,
    id: i64,
    found: &FileMetadata,
) -> Result<(), CatalogError> {
    transaction.execute(
        "UPDATE files
            SET captured_at = ?2, camera_make = ?3, camera_model = ?4,
                camera_serial = ?5, shutter_count = ?6, lens = ?7
          WHERE id = ?1",
        rusqlite::params![
            id,
            found.captured_at,
            found.camera_make,
            found.camera_model,
            found.camera_serial,
            found.shutter_count,
            found.lens,
        ],
    )?;
    Ok(())
}

/// Flag rows on this volume that the walk did not reach.
fn mark_missing(
    transaction: &rusqlite::Transaction<'_>,
    volume_id: i64,
    seen: &[i64],
) -> Result<usize, CatalogError> {
    // Building an IN list rather than a temp table: `seen` is one row per file
    // and SQLite's default limit is around a million parameters, which is far
    // more than a library scan produces in one pass.
    let list = seen
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "UPDATE files SET missing = 1
          WHERE missing = 0
            AND folder_id IN (SELECT id FROM folders WHERE volume_id = ?1)
            AND id NOT IN ({list})"
    );
    Ok(transaction.execute(&sql, [volume_id])?)
}

fn upsert_volume(
    transaction: &rusqlite::Transaction<'_>,
    volume: &VolumeId,
    root: &Path,
    convention: PathConvention,
) -> Result<i64, CatalogError> {
    // The same spelling as `folders.relative_path`, because the two are
    // concatenated with `/` to rebuild a path — by `hash_missing` below, by
    // `cull::sequence`, by previews and by export. Stored raw, a Windows root
    // arrives here as `\\?\C:\Users\...` and the result is a mixed-separator
    // verbatim path, which is the one shape Windows will not open.
    let mount = CatalogPath::new(root, convention)
        .map_err(|e| CatalogError::Io(e.to_string()))?
        .stored()
        .to_string();
    let convention = match convention {
        PathConvention::Exact => "exact",
        PathConvention::CaseInsensitive => "case_insensitive",
        PathConvention::CaseInsensitiveNormalised => "case_insensitive_normalised",
    };
    let (kind, uuid, serial, host, share, mount_path) = match volume {
        VolumeId::Uuid(u) => ("uuid", Some(u.clone()), None, None, None, None),
        VolumeId::WindowsSerial(s) => ("windows_serial", None, Some(*s as i64), None, None, None),
        VolumeId::NetworkShare { host, share } => (
            "network_share",
            None,
            None,
            Some(host.clone()),
            Some(share.clone()),
            None,
        ),
        VolumeId::MountPath(at) => ("mount_path", None, None, None, None, Some(at.clone())),
    };
    transaction.execute(
        "INSERT INTO volumes
              (kind, uuid, windows_serial, host, share, mount_path, last_mount_path, path_convention)
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT DO UPDATE SET last_mount_path = excluded.last_mount_path",
        rusqlite::params![kind, uuid, serial, host, share, mount_path, mount, convention],
    )?;
    Ok(transaction.query_row(
        "SELECT id FROM volumes
          WHERE kind = ?1 AND ifnull(uuid,'') = ifnull(?2,'')
            AND ifnull(windows_serial,-1) = ifnull(?3,-1)
            AND ifnull(host,'') = ifnull(?4,'') AND ifnull(share,'') = ifnull(?5,'')
            AND ifnull(mount_path,'') = ifnull(?6,'')",
        rusqlite::params![kind, uuid, serial, host, share, mount_path],
        |row| row.get(0),
    )?)
}

/// Create the folder row and every ancestor between it and the root.
fn upsert_folder(
    transaction: &rusqlite::Transaction<'_>,
    volume_id: i64,
    relative: &Path,
    convention: PathConvention,
) -> Result<i64, CatalogError> {
    let mut parent: Option<i64> = None;

    // The root itself is a folder, so the empty path gets a row too.
    for component in std::iter::once(PathBuf::new()).chain(
        relative
            .components()
            .map(|c| PathBuf::from(c.as_os_str()))
            .scan(PathBuf::new(), |acc, part| {
                acc.push(part);
                Some(acc.clone())
            }),
    ) {
        let path = CatalogPath::new(&component, convention)
            .map_err(|e| CatalogError::Io(e.to_string()))?;
        transaction.execute(
            "INSERT INTO folders (volume_id, parent_id, relative_path, path_key)
                  VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT DO NOTHING",
            rusqlite::params![volume_id, parent, path.stored(), path.key()],
        )?;
        parent = Some(transaction.query_row(
            "SELECT id FROM folders WHERE volume_id = ?1 AND path_key = ?2",
            rusqlite::params![volume_id, path.key()],
            |row| row.get(0),
        )?);
    }
    parent.ok_or_else(|| CatalogError::Io("no folder for this file".into()))
}

struct Found {
    path: PathBuf,
    size: i64,
    mtime: i64,
}

/// Depth-first, not following symlinks, never fatal.
fn walk(dir: &Path, out: &mut Vec<Found>, report: &mut ScanReport) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            report.unreadable.push(dir.to_path_buf());
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        // `symlink_metadata` rather than `metadata`: the latter follows the
        // link, and a link into an ancestor would make this recurse forever.
        let Ok(meta) = entry.path().symlink_metadata() else {
            report.unreadable.push(path);
            continue;
        };
        if meta.is_symlink() {
            report.symlinks += 1;
            continue;
        }
        if meta.is_dir() {
            walk(&path, out, report);
            continue;
        }
        if !is_supported(&path) {
            continue;
        }
        out.push(Found {
            size: meta.len() as i64,
            mtime: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            path,
        });
    }
}

pub(crate) fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|s| e.eq_ignore_ascii_case(s)))
}

/// Fill in `content_hash` for every file that has none, reporting progress.
///
/// The deferred cost of a scan, made explicit and user-triggered. Files that
/// cannot be read are left NULL and counted rather than failing the run — an
/// unplugged drive should not stop the rest from being hashed.
pub fn hash_missing(
    catalog: &mut Catalog,
    mut progress: impl FnMut(usize, usize),
) -> Result<(usize, usize), CatalogError> {
    let pending: Vec<(i64, String)> = {
        let mut statement = catalog.connection().prepare(
            "SELECT f.id, v.last_mount_path || '/' || d.relative_path || '/' || f.filename
               FROM files f
               JOIN folders d ON d.id = f.folder_id
               JOIN volumes v ON v.id = d.volume_id
              WHERE f.content_hash IS NULL AND f.missing = 0",
        )?;
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };

    let total = pending.len();
    let (mut hashed, mut failed) = (0, 0);
    for (index, (id, path)) in pending.into_iter().enumerate() {
        progress(index, total);
        // `//` from joining an empty relative path is harmless to the OS, but
        // tidy it so what lands in an error message is readable.
        let path = path.replace("//", "/");
        match crate::relink::hash_file(Path::new(&path)) {
            Ok(hash) => {
                catalog.connection().execute(
                    "UPDATE files SET content_hash = ?2 WHERE id = ?1",
                    rusqlite::params![id, hash],
                )?;
                hashed += 1;
            }
            Err(_) => failed += 1,
        }
    }
    progress(total, total);
    Ok((hashed, failed))
}

fn seconds_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::{tempdir, Scratch};

    /// A scan against a fixed volume, so these tests do not require the machine
    /// running them to have a filesystem UUID — CI's does not.
    fn test_scan(catalog: &mut Catalog, root: &Path) -> ScanReport {
        scan_on(
            catalog,
            root,
            VolumeId::Uuid("test-volume".into()),
            no_metadata,
        )
        .unwrap()
    }

    /// The path the scan will hand the reader for a file the test just wrote.
    ///
    /// Not the same string the test built: `scan_on` canonicalises its root, and
    /// what that changes differs per platform — macOS resolves the temp
    /// directory's `/var` to `/private/var`, and Windows returns the
    /// extended-length `\\?\C:\...` form. Comparing against the raw spelling
    /// passed on Linux and failed on both of the others, which is the failure
    /// mode AGENTS.md is about.
    fn as_opened(path: PathBuf) -> PathBuf {
        path.canonicalize().expect("the test just wrote this file")
    }

    /// A reader standing in for the decoder, recording what it was asked for.
    ///
    /// It answers for `.ARW` files whose contents begin with `raw` and refuses
    /// everything else, which is the shape of the real failure: a file named
    /// like a photograph that is not one.
    fn fake_reader(seen: &mut Vec<PathBuf>) -> impl FnMut(&Path) -> Option<FileMetadata> + '_ {
        move |path: &Path| {
            seen.push(path.to_path_buf());
            let contents = std::fs::read(path).ok()?;
            if !contents.starts_with(b"raw") {
                return None;
            }
            Some(FileMetadata {
                captured_at: Some(1_786_382_890),
                camera_make: Some("Sony".into()),
                camera_model: Some("ILCE-6400".into()),
                camera_serial: Some("14ff0000260d".into()),
                shutter_count: Some(14_562),
                lens: Some("E 70-350mm F4.5-6.3 G OSS".into()),
            })
        }
    }

    fn described(catalog: &Catalog) -> Vec<(String, Option<i64>, Option<String>)> {
        let mut statement = catalog
            .connection()
            .prepare("SELECT filename, captured_at, camera_model FROM files ORDER BY filename")
            .unwrap();
        let rows = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    }

    fn library(dir: &Scratch) -> Catalog {
        Catalog::open(&dir.join("library.rawkit")).unwrap()
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn filenames(catalog: &Catalog) -> Vec<(String, i64)> {
        let mut statement = catalog
            .connection()
            .prepare("SELECT filename, missing FROM files ORDER BY filename")
            .unwrap();
        let rows = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    }

    #[test]
    fn only_raws_are_catalogued_and_case_does_not_matter() {
        let dir = tempdir();
        let photos = dir.join("photos");
        for name in [
            "a.ARW",
            "b.arw",
            "c.Arw", // all three are raws
            "a.JPG",
            "clip.MP4",
            "notes",
            "d.ARW.bak",
        ] {
            write(&photos.join(name), b"x");
        }
        let mut catalog = library(&dir);
        let report = test_scan(&mut catalog, &photos);

        assert_eq!(report.added, 3, "only the raws, whatever their case");
        let names: Vec<String> = filenames(&catalog).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["a.ARW", "b.arw", "c.Arw"]);
    }

    #[test]
    fn a_scan_does_not_hash_and_says_so_by_leaving_null() {
        // The whole reason import is fast. If this ever starts hashing, a scan
        // of a real library goes from seconds to an hour without anyone
        // choosing that.
        let dir = tempdir();
        let photos = dir.join("photos");
        write(&photos.join("a.ARW"), b"x");
        let mut catalog = library(&dir);
        test_scan(&mut catalog, &photos);

        let unhashed: i64 = catalog
            .connection()
            .query_row(
                "SELECT count(*) FROM files WHERE content_hash IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unhashed, 1);
    }

    #[test]
    fn rescanning_changes_nothing() {
        let dir = tempdir();
        let photos = dir.join("photos");
        write(&photos.join("a.ARW"), b"x");
        write(&photos.join("2026/b.ARW"), b"y");
        let mut catalog = library(&dir);

        let first = test_scan(&mut catalog, &photos);
        assert_eq!((first.added, first.unchanged), (2, 0));

        let second = test_scan(&mut catalog, &photos);
        assert_eq!(
            (second.added, second.unchanged, second.missing),
            (0, 2, 0),
            "a second scan of an unchanged folder must be a no-op"
        );
        let files: i64 = catalog
            .connection()
            .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(files, 2, "rescanning must not duplicate rows");
    }

    #[test]
    fn a_vanished_file_is_flagged_and_kept_not_deleted() {
        // The property that makes an unplugged drive survivable. Deleting the
        // row would take the ratings and edits with it.
        let dir = tempdir();
        let photos = dir.join("photos");
        write(&photos.join("a.ARW"), b"x");
        write(&photos.join("b.ARW"), b"y");
        let mut catalog = library(&dir);
        test_scan(&mut catalog, &photos);

        std::fs::remove_file(photos.join("b.ARW")).unwrap();
        let report = test_scan(&mut catalog, &photos);
        assert_eq!(report.missing, 1);
        assert_eq!(
            filenames(&catalog),
            [("a.ARW".to_string(), 0), ("b.ARW".to_string(), 1)],
            "the row survives, flagged"
        );

        let images: i64 = catalog
            .connection()
            .query_row("SELECT count(*) FROM images", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            images, 2,
            "and so does its image, and anything hanging off it"
        );

        // Put it back: a reconnected drive clears the flag.
        write(&photos.join("b.ARW"), b"y");
        test_scan(&mut catalog, &photos);
        assert_eq!(
            filenames(&catalog),
            [("a.ARW".into(), 0), ("b.ARW".into(), 0)]
        );
    }

    #[test]
    fn changed_bytes_discard_the_old_hash() {
        // A stale hash is worse than none: relink trusts it, so it would match a
        // file that no longer has those contents.
        let dir = tempdir();
        let photos = dir.join("photos");
        write(&photos.join("a.ARW"), b"first");
        let mut catalog = library(&dir);
        test_scan(&mut catalog, &photos);
        catalog
            .connection()
            .execute("UPDATE files SET content_hash = 'stale'", [])
            .unwrap();

        // Different length, so the change is visible without waiting for the
        // clock's resolution on mtime.
        write(&photos.join("a.ARW"), b"second and longer");
        let report = test_scan(&mut catalog, &photos);
        assert_eq!(report.updated, 1);

        let hash: Option<String> = catalog
            .connection()
            .query_row("SELECT content_hash FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hash, None, "the hash described bytes that are gone");
    }

    #[test]
    fn the_recorded_root_is_spelled_the_way_the_rest_of_the_path_is() {
        // Every rebuilt path in the catalog is `last_mount_path` + '/' +
        // `relative_path` + '/' + `filename`, so the root has to follow the same
        // separator rule as the halves it is glued to. It did not: it went in
        // via `to_string_lossy`, which on Windows means backslashes and an
        // extended-length prefix, and the concatenation then produced a path
        // that looked right and opened nothing.
        let dir = tempdir();
        let photos = dir.join("photos");
        write(&photos.join("a.ARW"), b"x");
        let mut catalog = library(&dir);
        test_scan(&mut catalog, &photos);

        let root: String = catalog
            .connection()
            .query_row("SELECT last_mount_path FROM volumes", [], |row| row.get(0))
            .unwrap();
        assert!(
            !root.contains('\\'),
            "{root} carries a separator the rebuilt paths do not use"
        );
        assert!(
            !root.starts_with("//?/"),
            "{root} still carries an extended-length prefix"
        );
    }

    #[test]
    fn nested_folders_are_linked_to_their_parents() {
        let dir = tempdir();
        let photos = dir.join("photos");
        write(&photos.join("2026/january/a.ARW"), b"x");
        let mut catalog = library(&dir);
        test_scan(&mut catalog, &photos);

        let mut statement = catalog
            .connection()
            .prepare("SELECT relative_path, parent_id FROM folders ORDER BY id")
            .unwrap();
        let rows: Vec<(String, Option<i64>)> = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(rows.len(), 3, "root, 2026, 2026/january");
        assert_eq!(rows[0].0, "");
        assert_eq!(rows[0].1, None, "the root has no parent");
        assert_eq!(rows[1], ("2026".into(), Some(1)));
        assert_eq!(rows[2], ("2026/january".into(), Some(2)));
    }

    // The behaviour under test is portable — a scan that aborts on one
    // permission error is useless on any real disk — but the only way to *make*
    // a directory unreadable differs per platform, and on Windows it means an
    // ACL rather than a mode. So this runs on Linux and macOS, where the setup
    // is one call, and the Windows half is a genuine gap rather than a
    // pretended pass.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_is_reported_and_the_scan_continues() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        let photos = dir.join("photos");
        write(&photos.join("visible.ARW"), b"x");
        write(&photos.join("locked/hidden.ARW"), b"y");
        std::fs::set_permissions(
            photos.join("locked"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();

        let mut catalog = library(&dir);
        let report = test_scan(&mut catalog, &photos);

        // Restore before the assertions, or a failure leaves an undeletable dir.
        std::fs::set_permissions(
            photos.join("locked"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        assert_eq!(report.added, 1, "the readable file still made it in");
        assert_eq!(report.unreadable.len(), 1, "and the failure was reported");
    }

    // Windows can make symlinks too, but only with developer mode on or the
    // privilege granted, so a runner without either would fail on the setup and
    // not on the thing being tested.
    #[cfg(unix)]
    #[test]
    fn symlinks_are_counted_and_not_followed() {
        // A link pointing at an ancestor is how a naive walk recurses forever,
        // and a link to a sibling folder is how files get indexed twice.
        let dir = tempdir();
        let photos = dir.join("photos");
        write(&photos.join("a.ARW"), b"x");
        std::os::unix::fs::symlink(&photos, photos.join("loop")).unwrap();

        let mut catalog = library(&dir);
        let report = test_scan(&mut catalog, &photos);
        assert_eq!(report.added, 1);
        assert_eq!(report.symlinks, 1);
    }

    #[test]
    fn a_scan_records_the_camera_and_when_the_shutter_fired() {
        let dir = tempdir();
        let photos = dir.join("photos");
        write(&photos.join("a.ARW"), b"raw bytes");
        let mut catalog = library(&dir);

        let mut seen = Vec::new();
        scan_on(
            &mut catalog,
            &photos,
            VolumeId::Uuid("test-volume".into()),
            fake_reader(&mut seen),
        )
        .unwrap();

        let row = catalog
            .connection()
            .query_row(
                "SELECT captured_at, camera_make, camera_model, camera_serial,
                        shutter_count, lens
                   FROM files",
                [],
                |r| {
                    Ok(FileMetadata {
                        captured_at: r.get(0)?,
                        camera_make: r.get(1)?,
                        camera_model: r.get(2)?,
                        camera_serial: r.get(3)?,
                        shutter_count: r.get(4)?,
                        lens: r.get(5)?,
                    })
                },
            )
            .unwrap();
        // Read back as the same struct that went in, so a column written into
        // the wrong place fails here rather than showing up as a lens called
        // "Sony" in a UI six months from now.
        assert_eq!(
            row,
            FileMetadata {
                captured_at: Some(1_786_382_890),
                camera_make: Some("Sony".into()),
                camera_model: Some("ILCE-6400".into()),
                camera_serial: Some("14ff0000260d".into()),
                shutter_count: Some(14_562),
                lens: Some("E 70-350mm F4.5-6.3 G OSS".into()),
            }
        );
    }

    #[test]
    fn a_file_that_is_not_really_a_raw_is_still_catalogued() {
        // The failure mode that made this a parameter rather than a dependency.
        // A `.ARW` that will not parse must not abort the scan, must not be
        // skipped, and must not acquire invented metadata.
        let dir = tempdir();
        let photos = dir.join("photos");
        write(&photos.join("good.ARW"), b"raw bytes");
        write(&photos.join("truncated.ARW"), b"not a raw at all");
        let mut catalog = library(&dir);

        let mut seen = Vec::new();
        let report = scan_on(
            &mut catalog,
            &photos,
            VolumeId::Uuid("test-volume".into()),
            fake_reader(&mut seen),
        )
        .unwrap();

        assert_eq!(report.added, 2, "both rows exist");
        assert_eq!(report.without_metadata, 1);
        assert_eq!(
            described(&catalog),
            [
                (
                    "good.ARW".to_string(),
                    Some(1_786_382_890),
                    Some("ILCE-6400".to_string())
                ),
                ("truncated.ARW".to_string(), None, None),
            ]
        );
    }

    #[test]
    fn a_rescan_does_not_reread_a_file_it_has_already_described() {
        // The cost argument. Reading a header is cheap per file and ruinous per
        // library if it happens on every scan of every unchanged photograph.
        let dir = tempdir();
        let photos = dir.join("photos");
        write(&photos.join("a.ARW"), b"raw bytes");
        write(&photos.join("b.ARW"), b"raw bytes too");
        let mut catalog = library(&dir);

        let mut first = Vec::new();
        scan_on(
            &mut catalog,
            &photos,
            VolumeId::Uuid("test-volume".into()),
            fake_reader(&mut first),
        )
        .unwrap();
        assert_eq!(first.len(), 2, "both were new");

        let mut second = Vec::new();
        scan_on(
            &mut catalog,
            &photos,
            VolumeId::Uuid("test-volume".into()),
            fake_reader(&mut second),
        )
        .unwrap();
        assert!(
            second.is_empty(),
            "nothing changed, so nothing was reopened"
        );

        // Change one, and only that one is read again.
        write(&photos.join("b.ARW"), b"raw bytes, rather more of them");
        let mut third = Vec::new();
        scan_on(
            &mut catalog,
            &photos,
            VolumeId::Uuid("test-volume".into()),
            fake_reader(&mut third),
        )
        .unwrap();
        assert_eq!(third, [as_opened(photos.join("b.ARW"))]);
    }

    #[test]
    fn a_row_left_undescribed_is_filled_in_by_the_next_scan() {
        // Two cases at once: a library catalogued before this code existed, and
        // a file whose read failed for a reason that has since gone away.
        let dir = tempdir();
        let photos = dir.join("photos");
        write(&photos.join("a.ARW"), b"raw bytes");
        let mut catalog = library(&dir);
        test_scan(&mut catalog, &photos); // no reader at all
        assert_eq!(described(&catalog), [("a.ARW".to_string(), None, None)]);

        let mut seen = Vec::new();
        let report = scan_on(
            &mut catalog,
            &photos,
            VolumeId::Uuid("test-volume".into()),
            fake_reader(&mut seen),
        )
        .unwrap();
        assert_eq!(report.unchanged, 1, "the file itself did not move");
        assert_eq!(
            seen,
            [as_opened(photos.join("a.ARW"))],
            "but it was still described"
        );
        assert_eq!(
            described(&catalog),
            [(
                "a.ARW".to_string(),
                Some(1_786_382_890),
                Some("ILCE-6400".to_string())
            )]
        );
    }

    #[test]
    fn changed_bytes_replace_the_capture_time_rather_than_keeping_it() {
        // The same reasoning as the hash: metadata that describes bytes which
        // are gone is worse than none, because a duplicate check trusts it.
        let dir = tempdir();
        let photos = dir.join("photos");
        write(&photos.join("a.ARW"), b"raw bytes");
        let mut catalog = library(&dir);
        let mut seen = Vec::new();
        scan_on(
            &mut catalog,
            &photos,
            VolumeId::Uuid("test-volume".into()),
            fake_reader(&mut seen),
        )
        .unwrap();

        // Overwritten with something that is no longer a RAW.
        write(&photos.join("a.ARW"), b"a jpeg someone renamed by mistake");
        let mut seen = Vec::new();
        scan_on(
            &mut catalog,
            &photos,
            VolumeId::Uuid("test-volume".into()),
            fake_reader(&mut seen),
        )
        .unwrap();
        assert_eq!(
            described(&catalog),
            [("a.ARW".to_string(), None, None)],
            "the old camera and capture time described the previous contents"
        );
    }

    #[test]
    fn hashing_is_a_separate_step_that_fills_in_what_the_scan_left() {
        let dir = tempdir();
        let photos = dir.join("photos");
        write(&photos.join("a.ARW"), b"contents");
        let mut catalog = library(&dir);
        test_scan(&mut catalog, &photos);

        let (hashed, failed) = hash_missing(&mut catalog, |_, _| {}).unwrap();
        assert_eq!((hashed, failed), (1, 0));

        let hash: Option<String> = catalog
            .connection()
            .query_row("SELECT content_hash FROM files", [], |r| r.get(0))
            .unwrap();
        let expected = crate::relink::hash_file(&photos.join("a.ARW")).unwrap();
        assert_eq!(hash.as_deref(), Some(expected.as_str()));

        // And it is idempotent: nothing left to do on a second pass.
        assert_eq!(hash_missing(&mut catalog, |_, _| {}).unwrap(), (0, 0));
    }
}
