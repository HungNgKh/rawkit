//! Finding a file that moved.
//!
//! # Why a hash and not a path
//!
//! Paths are how a catalog *addresses* a file and a poor way to *identify* one.
//! Photographers reorganise: a folder gets renamed, a year gets moved to a
//! bigger disk, a card import lands in the wrong place and is tidied up later.
//! Every one of those breaks a path and none of them changes the photograph.
//!
//! So `files.content_hash` is the fallback identity, and this module is what
//! makes it useful: hash a file, look it up, repoint the row. The volume record
//! already says a network share has no stable identity at all
//! (`VolumeId::is_stable`), which is the case this exists for.
//!
//! # What is here and what is not
//!
//! The mechanism, not the scanner. Walking a subtree, hashing what it finds and
//! re-anchoring a whole folder is a separate piece with its own progress
//! reporting and its own decisions about what to do with ambiguity — and it will
//! call these functions.

use crate::path::{CatalogPath, PathConvention};
use crate::{db::Catalog, CatalogError};
use std::io::Read;
use std::path::{Path, PathBuf};

/// blake3 of a file's contents, hex, as `EditState::content_hash` produces.
///
/// Streamed rather than read whole: a catalog is expected to hash everything it
/// imports, and reading a 25 MB raw — or a video — into memory per file is a
/// cost with no benefit.
pub fn hash_file(path: &Path) -> Result<String, CatalogError> {
    let mut file = std::fs::File::open(path).map_err(|e| CatalogError::Io(e.to_string()))?;
    let mut hasher = blake3::Hasher::new();
    // 64 KiB: comfortably above the syscall overhead, comfortably below the
    // point where the buffer stops fitting in cache.
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| CatalogError::Io(e.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Every catalogued file with this hash.
///
/// More than one is normal rather than exceptional — the same frame imported
/// twice, or a deliberate copy — so this returns all of them and leaves the
/// choice to the caller. A relink that silently picks one would eventually pick
/// the wrong one.
pub fn find_by_hash(catalog: &Catalog, hash: &str) -> Result<Vec<i64>, CatalogError> {
    let mut statement = catalog
        .connection()
        .prepare("SELECT id FROM files WHERE content_hash = ?1 ORDER BY id")?;
    let ids = statement
        .query_map([hash], |row| row.get(0))?
        .collect::<Result<Vec<i64>, _>>()?;
    Ok(ids)
}

/// Point a catalogued file at where it now lives, and clear its `missing` flag.
///
/// The folder must already exist in the catalog: this moves a file between
/// known folders, and creating folders is the scanner's job.
///
/// The filename's comparison key is derived under the **volume's** stored
/// convention rather than the running host's. That column exists for exactly
/// this: a catalog written on a Mac and opened on Linux would otherwise start
/// generating keys under different rules than the ones already in the table,
/// and every subsequent lookup would miss.
pub fn relink(
    catalog: &Catalog,
    file_id: i64,
    folder_id: i64,
    filename: &str,
) -> Result<(), CatalogError> {
    let convention: String = catalog.connection().query_row(
        "SELECT v.path_convention
           FROM folders f JOIN volumes v ON v.id = f.volume_id
          WHERE f.id = ?1",
        [folder_id],
        |row| row.get(0),
    )?;
    let convention = match convention.as_str() {
        "exact" => PathConvention::Exact,
        "case_insensitive" => PathConvention::CaseInsensitive,
        "case_insensitive_normalised" => PathConvention::CaseInsensitiveNormalised,
        other => {
            return Err(CatalogError::Sqlite(format!(
                "volume has an unknown path convention {other:?}"
            )))
        }
    };
    let key = CatalogPath::new(Path::new(filename), convention)
        .map_err(|e| CatalogError::Io(e.to_string()))?;

    let changed = catalog.connection().execute(
        "UPDATE files
            SET folder_id = ?2, filename = ?3, filename_key = ?4, missing = 0
          WHERE id = ?1",
        rusqlite::params![file_id, folder_id, key.stored(), key.key()],
    )?;
    if changed == 0 {
        return Err(CatalogError::Sqlite(format!("no file with id {file_id}")));
    }
    Ok(())
}

/// What a search through a folder found.
#[derive(Debug, Default, PartialEq)]
pub struct Found {
    /// Missing photographs whose file was found here and pointed at it.
    pub relinked: usize,
    /// Missing photographs still missing after this.
    pub still_missing: usize,
    /// Files here whose contents match more than one missing photograph. Left
    /// alone: putting a photograph back on the wrong file is worse than leaving
    /// it missing, and nothing about the folder says which is which.
    pub ambiguous: usize,
    /// Missing photographs with no hash recorded, which cannot be matched by
    /// contents at all. Counted so the sentence can say why they were not found.
    pub unhashed: usize,
    /// Files here that another photograph in the library already holds, and
    /// whose contents match one that is missing. Left alone: this is the same
    /// photograph imported twice, and pointing the missing row at a file that
    /// is already somebody's would leave two rows over one file — so the next
    /// time that file moved, or was culled, both would be wrong.
    pub already_held: usize,
    /// Folders under the root that could not be read.
    pub unreadable: Vec<PathBuf>,
}

/// Where a search has got to.
#[derive(Debug, Clone, Copy)]
pub enum Looking<'a> {
    /// Listing the folder. `found` is how many files might be worth reading.
    Walking { found: usize },
    /// Reading a file to see which photograph it is.
    Reading {
        done: usize,
        total: usize,
        name: &'a str,
    },
}

/// Look through a folder for the files of photographs that have gone missing,
/// and point the catalog at the ones found.
///
/// # How a file is recognised
///
/// By its contents, and only by its contents: a photograph's `content_hash` is
/// what survives being moved, renamed, and copied to another disk. Names and
/// dates are not consulted — a name is what changed in half the cases this
/// exists for.
///
/// Only files whose **size** matches a missing photograph's are read, so the
/// cost is one hash per plausible file rather than one per file in the folder.
/// Everything else is skipped without being opened.
///
/// `dry_run` finds and counts without writing, which is what the window asks
/// before it asks the person.
pub fn search(
    catalog: &mut Catalog,
    root: &Path,
    dry_run: bool,
    mut watch: impl FnMut(Looking<'_>) -> bool,
) -> Result<Found, CatalogError> {
    let root = root
        .canonicalize()
        .map_err(|e| CatalogError::Io(format!("{}: {e}", root.display())))?;
    let mut found = Found::default();

    // Every photograph whose file has gone, and the size it was.
    let mut missing: Vec<(i64, String, i64)> = {
        let mut statement = catalog.connection().prepare(
            "SELECT id, content_hash, size FROM files
              WHERE missing = 1 AND content_hash IS NOT NULL",
        )?;
        let rows = statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let all_missing: i64 = catalog.connection().query_row(
        "SELECT count(*) FROM files WHERE missing = 1",
        [],
        |r| r.get(0),
    )?;
    found.unhashed = all_missing as usize - missing.len();
    found.still_missing = all_missing as usize;
    if missing.is_empty() {
        return Ok(found);
    }
    let sizes: std::collections::HashSet<i64> = missing.iter().map(|(_, _, size)| *size).collect();

    // Where every photograph that is *not* missing is, by comparison key. A
    // file in this set is already somebody's; whatever it matches, it is not
    // free to be handed to a missing row.
    let convention = PathConvention::host();
    let held: std::collections::HashSet<String> = {
        let mut statement = catalog.connection().prepare(
            "SELECT v.last_mount_path || '/' || d.relative_path || '/' || f.filename
               FROM files f
               JOIN folders d ON d.id = f.folder_id
               JOIN volumes v ON v.id = d.volume_id
              WHERE f.missing = 0",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.filter_map(|row| row.ok())
            .filter_map(|path| {
                // `//` from an empty relative path, as everywhere else that
                // rebuilds a path from these three columns.
                CatalogPath::new(Path::new(&path.replace("//", "/")), convention).ok()
            })
            .map(|path| path.key().to_string())
            .collect()
    };

    // The files worth reading: the right size, and a kind this library holds.
    let mut candidates = Vec::new();
    if !walk_for(&root, &sizes, &mut candidates, &mut found, &mut watch) {
        return Err(CatalogError::Cancelled);
    }

    let volume = crate::VolumeId::resolve(&root)?;
    let total = candidates.len();
    let mut back: Vec<(i64, PathBuf)> = Vec::new();
    for (done, file) in candidates.iter().enumerate() {
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !watch(Looking::Reading {
            done,
            total,
            name: &name,
        }) {
            return Err(CatalogError::Cancelled);
        }
        let Ok(hash) = hash_file(file) else { continue };
        let matching: Vec<i64> = missing
            .iter()
            .filter(|(_, theirs, _)| *theirs == hash)
            .map(|(id, _, _)| *id)
            .collect();
        let mine = CatalogPath::new(file, convention)
            .map(|path| held.contains(path.key()))
            .unwrap_or(false);
        match matching.as_slice() {
            [] => {}
            _ if mine => found.already_held += 1,
            [id] => {
                let id = *id;
                missing.retain(|(held, _, _)| *held != id);
                back.push((id, file.clone()));
            }
            _ => found.ambiguous += 1,
        }
    }
    found.relinked = back.len();
    found.still_missing = all_missing as usize - back.len();
    if dry_run || back.is_empty() {
        return Ok(found);
    }

    // One transaction: a half-relinked library is a library whose photographs
    // are in two places according to itself.
    let transaction = catalog.connection_mut().transaction()?;
    // The volume's root **widens** to take this folder in; it is never
    // re-pointed at it. Pointing a relink at one folder of a library used to
    // move the whole volume under that folder, so every photograph not found
    // here — the ones still missing — claimed a path it had never had.
    let (volume_id, base) = crate::scan::root_for(&transaction, &volume, &root, convention)?;
    for (file_id, path) in back {
        let parent = path.parent().unwrap_or(&root);
        let relative = crate::scan::relative_to(parent, &base, convention)?;
        let folder_id =
            crate::scan::upsert_folder(&transaction, volume_id, Path::new(&relative), convention)?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let key = CatalogPath::new(Path::new(&name), convention)
            .map_err(|e| CatalogError::Io(e.to_string()))?;
        transaction.execute(
            "UPDATE files
                SET folder_id = ?2, filename = ?3, filename_key = ?4, missing = 0
              WHERE id = ?1",
            rusqlite::params![file_id, folder_id, key.stored(), key.key()],
        )?;
    }
    transaction.commit()?;
    Ok(found)
}

/// Depth-first, never fatal, and only the files worth reading: the right size
/// and a kind this library holds. An unreadable folder is counted, not fatal —
/// one locked folder should not end the search.
fn walk_for(
    dir: &Path,
    sizes: &std::collections::HashSet<i64>,
    out: &mut Vec<PathBuf>,
    found: &mut Found,
    watch: &mut dyn FnMut(Looking<'_>) -> bool,
) -> bool {
    if !watch(Looking::Walking { found: out.len() }) {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        found.unreadable.push(dir.to_path_buf());
        return true;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if !walk_for(&path, sizes, out, found, watch) {
                return false;
            }
        } else if crate::scan::is_supported(&path)
            && entry
                .metadata()
                .is_ok_and(|m| sizes.contains(&(m.len() as i64)))
        {
            out.push(path);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::tempdir;

    fn seed(catalog: &Catalog, convention: &str) -> i64 {
        let db = catalog.connection();
        db.execute(
            "INSERT INTO volumes (id, kind, uuid, path_convention) VALUES (1, 'uuid', 'v', ?1)",
            [convention],
        )
        .unwrap();
        for (id, path) in [(1, "2026/january"), (2, "2026/february")] {
            db.execute(
                "INSERT INTO folders (id, volume_id, relative_path, path_key) VALUES (?1, 1, ?2, ?2)",
                rusqlite::params![id, path],
            )
            .unwrap();
        }
        db.execute(
            "INSERT INTO files (id, folder_id, filename, filename_key, size, mtime, content_hash, missing, imported_at)
             VALUES (1, 1, 'DSC00881.ARW', 'DSC00881.ARW', 100, 0, 'abc123', 1, 0)",
            [],
        )
        .unwrap();
        1
    }

    #[test]
    fn identical_contents_hash_alike_and_one_byte_does_not() {
        let dir = tempdir();
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        let c = dir.join("c.bin");
        // Larger than the read buffer, so the streaming path is what is tested
        // rather than a single lucky read.
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&a, &payload).unwrap();
        std::fs::write(&b, &payload).unwrap();
        let mut altered = payload.clone();
        altered[123_456] ^= 1;
        std::fs::write(&c, &altered).unwrap();

        assert_eq!(hash_file(&a).unwrap(), hash_file(&b).unwrap());
        assert_ne!(
            hash_file(&a).unwrap(),
            hash_file(&c).unwrap(),
            "one flipped bit must change the identity, or a corrupt copy relinks as the original"
        );
    }

    #[test]
    fn a_moved_file_is_found_by_hash_and_repointed() {
        let dir = tempdir();
        let catalog = Catalog::open(&dir.join("library.rawkit")).unwrap();
        let file_id = seed(&catalog, "exact");

        assert_eq!(find_by_hash(&catalog, "abc123").unwrap(), vec![file_id]);
        assert!(find_by_hash(&catalog, "nothing").unwrap().is_empty());

        // The photographer moved january's shoot into february.
        relink(&catalog, file_id, 2, "DSC00881.ARW").unwrap();
        let (folder, missing): (i64, i64) = catalog
            .connection()
            .query_row(
                "SELECT folder_id, missing FROM files WHERE id = ?1",
                [file_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(folder, 2);
        assert_eq!(missing, 0, "a relinked file is no longer missing");
    }

    #[test]
    fn the_key_follows_the_volume_rather_than_the_host() {
        // The same rename, recorded under two conventions. The stored spelling
        // is identical and the comparison key is not — which is the whole reason
        // volumes carry their convention, and would be invisible if the running
        // host's rules were used instead.
        for (convention, expected_key) in [
            ("exact", "dsc00881.arw".to_uppercase()),
            ("case_insensitive", "dsc00881.arw".to_string()),
        ] {
            let dir = tempdir();
            let catalog = Catalog::open(&dir.join("library.rawkit")).unwrap();
            let file_id = seed(&catalog, convention);
            relink(&catalog, file_id, 2, "DSC00881.ARW").unwrap();

            let (stored, key): (String, String) = catalog
                .connection()
                .query_row(
                    "SELECT filename, filename_key FROM files WHERE id = ?1",
                    [file_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(stored, "DSC00881.ARW", "the real spelling always survives");
            assert_eq!(
                key, expected_key,
                "under {convention} the key should be {expected_key}"
            );
        }
    }

    #[test]
    fn relinking_a_file_that_is_not_there_is_an_error() {
        let dir = tempdir();
        let catalog = Catalog::open(&dir.join("library.rawkit")).unwrap();
        seed(&catalog, "exact");
        assert!(
            relink(&catalog, 999, 2, "x.arw").is_err(),
            "silently changing nothing would look like a successful relink"
        );
    }

    /// A library of two photographs, scanned, then moved somewhere else on the
    /// disk and marked missing — exactly what a tidied-up folder looks like.
    fn moved_library(dir: &crate::db::tests::Scratch) -> (Catalog, std::path::PathBuf) {
        let was = dir.join("photos");
        std::fs::create_dir_all(was.join("day1")).unwrap();
        std::fs::write(was.join("day1/DSC00001.ARW"), b"the first photograph").unwrap();
        std::fs::write(was.join("DSC00002.ARW"), b"the second").unwrap();
        let mut catalog = Catalog::open(&dir.join("library.rawkit")).unwrap();
        crate::scan::scan_on(
            &mut catalog,
            &was,
            crate::VolumeId::Uuid("test-volume".into()),
            crate::scan::no_metadata,
        )
        .unwrap();
        crate::scan::hash_missing(&mut catalog, |_, _| true).unwrap();
        // Moved, and the catalog told by a scan of where they were.
        let now = dir.join("elsewhere");
        std::fs::create_dir_all(&now).unwrap();
        std::fs::rename(&was, now.join("photos")).unwrap();
        std::fs::create_dir_all(&was).unwrap();
        crate::scan::scan_on(
            &mut catalog,
            &was,
            crate::VolumeId::Uuid("test-volume".into()),
            crate::scan::no_metadata,
        )
        .unwrap();
        assert_eq!(missing_count(&catalog), 2, "both are missing to start with");
        (catalog, now)
    }

    fn missing_count(catalog: &Catalog) -> i64 {
        catalog
            .connection()
            .query_row("SELECT count(*) FROM files WHERE missing = 1", [], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn path_of(catalog: &Catalog, name: &str) -> String {
        catalog
            .connection()
            .query_row(
                "SELECT v.last_mount_path || '/' || d.relative_path || '/' || f.filename
                   FROM files f JOIN folders d ON d.id = f.folder_id
                   JOIN volumes v ON v.id = d.volume_id
                  WHERE f.filename = ?1",
                [name],
                |r| r.get::<_, String>(0),
            )
            .map(|p| p.replace("//", "/"))
            .unwrap()
    }

    #[test]
    fn photographs_that_moved_are_found_by_their_contents() {
        let dir = tempdir();
        let (mut catalog, now) = moved_library(&dir);

        // Asked first without writing: the same answer, and nothing changed.
        let looked = search(&mut catalog, &now, true, |_| true).unwrap();
        assert_eq!((looked.relinked, looked.still_missing), (2, 0));
        assert_eq!(missing_count(&catalog), 2, "a dry run writes nothing");

        let found = search(&mut catalog, &now, false, |_| true).unwrap();
        assert_eq!(
            found,
            Found {
                relinked: 2,
                still_missing: 0,
                ambiguous: 0,
                unhashed: 0,
                already_held: 0,
                unreadable: Vec::new()
            }
        );
        assert_eq!(missing_count(&catalog), 0);
        // Pointed at where they are now, folders and all.
        assert!(path_of(&catalog, "DSC00001.ARW").ends_with("elsewhere/photos/day1/DSC00001.ARW"));
        assert!(std::path::Path::new(&path_of(&catalog, "DSC00002.ARW")).exists());
    }

    #[test]
    fn finding_one_folder_does_not_move_what_is_still_missing() {
        // The bug this test is named after, found in the window: the volume's
        // root was re-pointed at the folder being looked in, so the photographs
        // *not* found there came to claim a path under it — one they had never
        // had. A volume's root widens; it is never re-pointed.
        let dir = tempdir();
        let (mut catalog, now) = moved_library(&dir);
        let was = path_of(&catalog, "DSC00002.ARW");

        // Only the subfolder is offered, so the loose photograph is not found.
        let found = search(&mut catalog, &now.join("photos/day1"), false, |_| true).unwrap();
        assert_eq!((found.relinked, found.still_missing), (1, 1));
        assert!(
            path_of(&catalog, "DSC00001.ARW").ends_with("photos/day1/DSC00001.ARW"),
            "{}",
            path_of(&catalog, "DSC00001.ARW")
        );
        assert_eq!(
            path_of(&catalog, "DSC00002.ARW"),
            was,
            "the one still missing kept the path it had"
        );

        // And the rest of the library is found when the folder above is offered.
        let found = search(&mut catalog, &now, false, |_| true).unwrap();
        assert_eq!((found.relinked, found.still_missing), (1, 0));
        assert!(std::path::Path::new(&path_of(&catalog, "DSC00002.ARW")).exists());
    }

    #[test]
    fn two_copies_of_one_photograph_are_left_alone() {
        // The same bytes under two names: nothing here says which of the two
        // missing photographs is which, and guessing would be worse.
        let dir = tempdir();
        let was = dir.join("photos");
        std::fs::create_dir_all(&was).unwrap();
        std::fs::write(was.join("DSC00001.ARW"), b"identical").unwrap();
        std::fs::write(was.join("DSC00002.ARW"), b"identical").unwrap();
        let mut catalog = Catalog::open(&dir.join("library.rawkit")).unwrap();
        let volume = crate::VolumeId::Uuid("test-volume".into());
        crate::scan::scan_on(&mut catalog, &was, volume.clone(), crate::scan::no_metadata).unwrap();
        crate::scan::hash_missing(&mut catalog, |_, _| true).unwrap();
        let now = dir.join("elsewhere");
        std::fs::create_dir_all(&now).unwrap();
        std::fs::rename(was.join("DSC00001.ARW"), now.join("DSC00001.ARW")).unwrap();
        std::fs::rename(was.join("DSC00002.ARW"), now.join("DSC00002.ARW")).unwrap();
        crate::scan::scan_on(&mut catalog, &was, volume, crate::scan::no_metadata).unwrap();

        let found = search(&mut catalog, &now, false, |_| true).unwrap();
        assert_eq!((found.relinked, found.ambiguous), (0, 2));
        assert_eq!(missing_count(&catalog), 2, "both are left missing");
    }

    #[test]
    fn a_file_another_photograph_already_holds_is_left_alone() {
        // The same photograph imported twice: one copy is still where the
        // catalog thinks it is, the other has gone. Matching by contents alone
        // would hand the missing row the copy that is already somebody's, and
        // the library would have two rows over one file.
        let dir = tempdir();
        let was = dir.join("photos");
        std::fs::create_dir_all(&was).unwrap();
        std::fs::write(was.join("DSC00001.ARW"), b"identical").unwrap();
        std::fs::write(was.join("DSC00002.ARW"), b"identical").unwrap();
        let mut catalog = Catalog::open(&dir.join("library.rawkit")).unwrap();
        let volume = crate::VolumeId::Uuid("test-volume".into());
        crate::scan::scan_on(&mut catalog, &was, volume.clone(), crate::scan::no_metadata).unwrap();
        crate::scan::hash_missing(&mut catalog, |_, _| true).unwrap();
        // One of the two is gone; the other stays exactly where it was.
        std::fs::remove_file(was.join("DSC00002.ARW")).unwrap();
        crate::scan::scan_on(&mut catalog, &was, volume, crate::scan::no_metadata).unwrap();
        assert_eq!(missing_count(&catalog), 1);

        let found = search(&mut catalog, &was, false, |_| true).unwrap();
        assert_eq!(
            (found.relinked, found.already_held, found.still_missing),
            (0, 1, 1)
        );
        assert_eq!(missing_count(&catalog), 1, "it is still missing");
        assert!(
            path_of(&catalog, "DSC00001.ARW").ends_with("photos/DSC00001.ARW"),
            "and the photograph that was there still has its file to itself"
        );
    }

    #[test]
    fn a_photograph_with_no_hash_cannot_be_found_and_is_counted() {
        let dir = tempdir();
        let (mut catalog, now) = moved_library(&dir);
        catalog
            .connection()
            .execute(
                "UPDATE files SET content_hash = NULL WHERE filename = 'DSC00002.ARW'",
                [],
            )
            .unwrap();

        let found = search(&mut catalog, &now, false, |_| true).unwrap();
        assert_eq!(
            (found.relinked, found.unhashed, found.still_missing),
            (1, 1, 1)
        );
    }

    #[test]
    fn a_search_can_be_stopped() {
        let dir = tempdir();
        let (mut catalog, now) = moved_library(&dir);
        let stopped = search(&mut catalog, &now, false, |_| false);
        assert!(matches!(stopped, Err(CatalogError::Cancelled)));
        assert_eq!(missing_count(&catalog), 2, "nothing was written");
    }
}
