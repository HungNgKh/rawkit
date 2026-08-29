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
//! **It does not read metadata.** `captured_at` and the camera columns stay
//! NULL; they come from the decoder, and wiring that in has its own failure
//! modes on files that are not really raws.

use crate::path::{CatalogPath, PathConvention};
use crate::{db::Catalog, CatalogError, VolumeId};
use std::path::{Path, PathBuf};

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
}

/// Index every supported file under `root`.
///
/// One transaction for the whole scan: a half-scanned catalog is worse than an
/// unscanned one, and SQLite is far faster batching inserts this way than
/// committing per row.
pub fn scan(catalog: &mut Catalog, root: &Path) -> Result<ScanReport, CatalogError> {
    let resolved = root
        .canonicalize()
        .map_err(|e| CatalogError::Io(format!("{}: {e}", root.display())))?;
    let volume = VolumeId::resolve(&resolved)?;
    scan_on(catalog, root, volume)
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

        let existing: Option<(i64, i64, i64)> = transaction
            .query_row(
                "SELECT id, size, mtime FROM files WHERE folder_id = ?1 AND filename_key = ?2",
                rusqlite::params![folder_id, key.key()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();

        match existing {
            Some((id, size, mtime)) => {
                if size == file.size && mtime == file.mtime {
                    // Unchanged, but possibly previously flagged missing — a
                    // reconnected drive should clear that.
                    transaction.execute("UPDATE files SET missing = 0 WHERE id = ?1", [id])?;
                    report.unchanged += 1;
                } else {
                    // Different bytes, so any hash we hold describes a file that
                    // no longer exists. Keeping it would be worse than having
                    // none, because relink trusts it.
                    transaction.execute(
                        "UPDATE files
                            SET size = ?2, mtime = ?3, content_hash = NULL, missing = 0
                          WHERE id = ?1",
                        rusqlite::params![id, file.size, file.mtime],
                    )?;
                    report.updated += 1;
                }
                seen.push(id);
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
                seen.push(id);
            }
        }
    }

    report.missing = mark_missing(&transaction, volume_id, &seen)?;
    transaction.commit()?;
    Ok(report)
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
    let convention = match convention {
        PathConvention::Exact => "exact",
        PathConvention::CaseInsensitive => "case_insensitive",
        PathConvention::CaseInsensitiveNormalised => "case_insensitive_normalised",
    };
    let (kind, uuid, serial, host, share) = match volume {
        VolumeId::Uuid(u) => ("uuid", Some(u.clone()), None, None, None),
        VolumeId::WindowsSerial(s) => ("windows_serial", None, Some(*s as i64), None, None),
        VolumeId::NetworkShare { host, share } => (
            "network_share",
            None,
            None,
            Some(host.clone()),
            Some(share.clone()),
        ),
    };
    let mount = root.to_string_lossy().into_owned();

    transaction.execute(
        "INSERT INTO volumes (kind, uuid, windows_serial, host, share, last_mount_path, path_convention)
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT DO UPDATE SET last_mount_path = excluded.last_mount_path",
        rusqlite::params![kind, uuid, serial, host, share, mount, convention],
    )?;
    Ok(transaction.query_row(
        "SELECT id FROM volumes
          WHERE kind = ?1 AND ifnull(uuid,'') = ifnull(?2,'')
            AND ifnull(windows_serial,-1) = ifnull(?3,-1)
            AND ifnull(host,'') = ifnull(?4,'') AND ifnull(share,'') = ifnull(?5,'')",
        rusqlite::params![kind, uuid, serial, host, share],
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

fn is_supported(path: &Path) -> bool {
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::db::tests::{tempdir, Scratch};

    /// A scan against a fixed volume, so these tests do not require the machine
    /// running them to have a filesystem UUID — CI's does not.
    fn test_scan(catalog: &mut Catalog, root: &Path) -> ScanReport {
        scan_on(catalog, root, VolumeId::Uuid("test-volume".into())).unwrap()
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
