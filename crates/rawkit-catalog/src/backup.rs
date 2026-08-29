//! Rolling copies of a catalog, so a bad write is recoverable.
//!
//! # Why not a file copy
//!
//! In WAL mode a catalog is a main file plus a `-wal` sidecar, and copying the
//! main file while anything is writing produces a torn database — one that opens
//! and is subtly wrong, which is worse than one that does not open. `VACUUM INTO`
//! asks SQLite for a consistent snapshot in a single statement, and compacts it
//! on the way out.
//!
//! # When these happen, and the honest limit
//!
//! Two moments:
//!
//! - **Before a migration.** The highest-risk write a catalog ever takes, it is
//!   deterministic, and it is the "migrations running on strangers' catalogs"
//!   risk the forward-only runner was built early to survive.
//! - **On close**, via `Drop`.
//!
//! `Drop` does not run when a process is killed, so backup-on-close protects
//! against bad writes and bad migrations, **not** against crashes. The
//! pre-migration backup is the one that always happens, which is why it is the
//! one that matters.

use crate::CatalogError;
use std::path::{Path, PathBuf};

/// How many backups a catalog keeps.
///
/// Ten, uncompressed. A catalog for twenty thousand images is roughly fifty
/// megabytes, so this costs about half a gigabyte beside the library — visible,
/// and cheap against the thing it protects.
pub const KEEP: usize = 10;

const SUFFIX: &str = ".rawkit";

/// Write a snapshot of `catalog` into its backups directory, then prune.
///
/// Returns the file written, or `None` for an in-memory catalog, which has
/// nowhere to put one and nothing that a copy would protect.
pub fn snapshot(catalog: &crate::db::Catalog) -> Result<Option<PathBuf>, CatalogError> {
    let (Some(source), Some(dir)) = (catalog.path(), catalog.backup_dir()) else {
        return Ok(None);
    };
    std::fs::create_dir_all(&dir).map_err(|e| CatalogError::Io(e.to_string()))?;

    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "catalog".into());
    let destination = dir.join(format!("{stem}-{}{SUFFIX}", timestamp(now_seconds())));

    // Already exists means a backup was taken this second; one a second is
    // plenty and overwriting the previous one would be worse than skipping.
    if destination.exists() {
        return Ok(Some(destination));
    }

    // The path goes through SQL, so a quote in it would end the string literal
    // early. Doubling is SQLite's own escape.
    let escaped = destination.to_string_lossy().replace('\'', "''");
    catalog
        .connection()
        .execute_batch(&format!("VACUUM INTO '{escaped}'"))?;

    prune(&dir, &stem)?;
    Ok(Some(destination))
}

/// Delete all but the newest [`KEEP`] backups.
///
/// The only part of this crate that deletes anything, so it is deliberately
/// timid: it considers **only** files in this catalog's own backups directory
/// whose names match the exact pattern this module writes. Anything else — a
/// note the user left, a copy they renamed, a backup of a different catalog —
/// is left alone. A rotation that tidies up a directory is a rotation that
/// eventually deletes something irreplaceable.
fn prune(dir: &Path, stem: &str) -> Result<(), CatalogError> {
    let prefix = format!("{stem}-");
    let mut ours: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| CatalogError::Io(e.to_string()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| is_ours(name, &prefix))
        })
        .collect();

    // Names are `stem-YYYY-MM-DDTHH-MM-SSZ.rawkit`, so lexical order is
    // chronological order and no parsing is needed to sort them.
    ours.sort();
    let excess = ours.len().saturating_sub(KEEP);
    for path in ours.into_iter().take(excess) {
        std::fs::remove_file(&path).map_err(|e| CatalogError::Io(e.to_string()))?;
    }
    Ok(())
}

/// Whether a filename is one this module wrote, checked strictly.
///
/// Length and shape both, because the prefix alone would match a file a user
/// named `library-notes.rawkit`.
fn is_ours(name: &str, prefix: &str) -> bool {
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };
    let Some(stamp) = rest.strip_suffix(SUFFIX) else {
        return false;
    };
    stamp.len() == 20
        && stamp.ends_with('Z')
        && stamp.char_indices().all(|(i, c)| match i {
            4 | 7 => c == '-',
            10 => c == 'T',
            13 | 16 => c == '-',
            19 => c == 'Z',
            _ => c.is_ascii_digit(),
        })
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Seconds since the epoch as `YYYY-MM-DDTHH-MM-SSZ`, UTC.
///
/// Written out rather than pulled from a date crate, for the same reason as the
/// half-float decode in the engine: it is twenty lines, it is needed in exactly
/// one place, and every dependency here costs a licence review. Colons are
/// avoided because Windows will not have them in a filename.
///
/// The civil-from-days conversion is Howard Hinnant's, which is the standard
/// one and is why this is short enough to be worth writing.
fn timestamp(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    let (hour, minute, second) = (time / 3600, (time % 3600) / 60, time % 60);

    // Shift the epoch to 0000-03-01 so leap days land at the end of the cycle.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}-{minute:02}-{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_utc_and_sort_chronologically() {
        // Known epochs, so a sign error or an off-by-one in the leap-year
        // arithmetic cannot pass.
        assert_eq!(timestamp(0), "1970-01-01T00-00-00Z");
        assert_eq!(timestamp(1), "1970-01-01T00-00-01Z");
        assert_eq!(timestamp(86_399), "1970-01-01T23-59-59Z");
        assert_eq!(timestamp(86_400), "1970-01-02T00-00-00Z");
        // 2000-02-29: a leap year that the century rule would wrongly skip.
        assert_eq!(timestamp(951_782_400), "2000-02-29T00-00-00Z");
        // 2100-03-01: the century rule applies, so 2100 is *not* a leap year.
        assert_eq!(timestamp(4_107_542_400), "2100-03-01T00-00-00Z");
        assert_eq!(timestamp(1_724_918_400), "2024-08-29T08-00-00Z");

        let mut stamps = [timestamp(86_400), timestamp(0), timestamp(1_000_000)];
        stamps.sort();
        assert_eq!(
            stamps,
            [timestamp(0), timestamp(86_400), timestamp(1_000_000)],
            "lexical order must be chronological, or rotation deletes the wrong file"
        );
    }

    #[test]
    fn a_backup_is_itself_a_working_catalog() {
        // The property that matters and the one a file copy would fail: the
        // snapshot must open, pass its own integrity check, and be at the same
        // schema version. A torn copy opens too, and is subtly wrong.
        let dir = crate::db::tests::tempdir();
        let path = dir.join("library.rawkit");
        let backup = {
            let catalog = crate::db::Catalog::open(&path).unwrap();
            catalog
                .connection()
                .execute(
                    "INSERT INTO volumes (kind, uuid, path_convention)
                     VALUES ('uuid', 'v1', 'exact')",
                    [],
                )
                .unwrap();
            snapshot(&catalog).unwrap().expect("a catalog on disk")
        };

        let restored = crate::db::Catalog::open(&backup).unwrap();
        assert_eq!(restored.version().unwrap(), crate::SCHEMA_VERSION);
        let volumes: i64 = restored
            .connection()
            .query_row("SELECT count(*) FROM volumes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            volumes, 1,
            "the snapshot must carry the data, not just the schema"
        );
    }

    #[test]
    fn rotation_keeps_the_newest_and_leaves_strangers_alone() {
        let dir = crate::db::tests::tempdir();
        let backups = dir.join("library-backups");
        std::fs::create_dir_all(&backups).unwrap();

        // More than we keep, plus two files we did not write.
        for i in 0..KEEP + 4 {
            let name = format!("library-2026-08-{:02}T00-00-00Z.rawkit", i + 1);
            std::fs::write(backups.join(name), b"x").unwrap();
        }
        std::fs::write(backups.join("library-notes.rawkit"), b"keep me").unwrap();
        std::fs::write(backups.join("README.txt"), b"keep me too").unwrap();

        prune(&backups, "library").unwrap();

        let mut left: Vec<String> = std::fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();

        assert!(
            left.contains(&"library-notes.rawkit".to_string())
                && left.contains(&"README.txt".to_string()),
            "rotation deleted a file it did not write: {left:?}"
        );
        let ours: Vec<&String> = left.iter().filter(|n| is_ours(n, "library-")).collect();
        assert_eq!(
            ours.len(),
            KEEP,
            "kept {} backups, wanted {KEEP}",
            ours.len()
        );
        assert!(
            ours.iter().any(|n| n.contains("2026-08-14")),
            "the newest must survive: {ours:?}"
        );
        assert!(
            !ours.iter().any(|n| n.contains("2026-08-01")),
            "the oldest must go: {ours:?}"
        );
    }

    #[test]
    fn only_our_own_filenames_are_considered_for_deletion() {
        let prefix = "library-";
        assert!(is_ours("library-2026-08-29T07-52-11Z.rawkit", prefix));

        // Everything a user might plausibly leave in that directory.
        for foreign in [
            "library-notes.rawkit",
            "library-2026-08-29T07-52-11Z.rawkit.bak",
            "library-keep-this-one-2026-08-29T07-52-11Z.rawkit",
            "other-2026-08-29T07-52-11Z.rawkit",
            "library-2026-08-29T07-52-1Z.rawkit",
            "library-.rawkit",
            "README.txt",
        ] {
            assert!(
                !is_ours(foreign, prefix),
                "{foreign} is not ours and must never be deleted"
            );
        }
    }
}
