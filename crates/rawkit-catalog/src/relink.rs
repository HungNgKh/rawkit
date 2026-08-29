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
use std::path::Path;

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
}
