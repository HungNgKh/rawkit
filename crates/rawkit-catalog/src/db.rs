//! Opening a catalog, and bringing its schema up to date.
//!
//! # The rule that shapes everything here
//!
//! This file will run on other people's libraries for most of the project's
//! life — the public beta is roughly fifteen months before v1.0 — and a catalog
//! app that loses a library does not get a second chance. So:
//!
//! - **Migrations are forward-only.** A down-migration on a photo library
//!   destroys data the user cannot get back. The rollback story is a restored
//!   backup, and the backup is taken before the first migration runs.
//! - **A catalog from the future is refused, not opened.** Half-understanding a
//!   schema is worse than declining it, because the damage is silent.
//! - **Each migration runs inside one transaction with its own version bump.**
//!   A crash mid-upgrade leaves a catalog at the last version that fully
//!   applied, never at a version whose tables are half there.

use crate::{pending, CatalogError, Migration};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

fn backup_dir_for(path: Option<&Path>) -> Option<PathBuf> {
    let path = path?;
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    Some(path.with_file_name(format!("{stem}-backups")))
}

fn describe(backups: &Option<PathBuf>) -> String {
    backups
        .as_ref()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| "(none: this catalog is in memory)".into())
}

/// Whether SQLite is telling us the *file* is broken, as opposed to the query.
///
/// `NotADatabase` counts: a file truncated or overwritten at the header is
/// damaged in the same way and wants the same answer, even though SQLite
/// describes it differently.
fn is_corruption(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase,
                ..
            },
            _
        )
    )
}

impl From<rusqlite::Error> for CatalogError {
    fn from(e: rusqlite::Error) -> Self {
        CatalogError::Sqlite(e.to_string())
    }
}

/// An open catalog, migrated to [`SCHEMA_VERSION`].
pub struct Catalog {
    connection: Connection,
    journal_mode: String,
    path: Option<PathBuf>,
}

impl Catalog {
    /// Open or create a catalog at `path`, applying whatever migrations it needs.
    pub fn open(path: &Path) -> Result<Self, CatalogError> {
        Self::from_connection(Connection::open(path)?, Some(path.to_path_buf()))
    }

    /// A catalog held only in memory. For tests, and for asking what a fresh
    /// schema looks like without writing one to disk.
    pub fn in_memory() -> Result<Self, CatalogError> {
        Self::from_connection(Connection::open_in_memory()?, None)
    }

    fn from_connection(
        connection: Connection,
        path: Option<PathBuf>,
    ) -> Result<Self, CatalogError> {
        // Corruption does not wait for the integrity check to ask about it: a
        // damaged file fails whichever statement first touches a bad page, which
        // in practice is the first pragma below. Mapping it here is what makes
        // the refusal say where the backups are instead of relaying SQLite's
        // "database disk image is malformed" and leaving the user nowhere.
        let backups = backup_dir_for(path.as_deref());
        let refuse = |e: rusqlite::Error| -> CatalogError {
            if is_corruption(&e) {
                CatalogError::Corrupt {
                    report: e.to_string(),
                    backups: describe(&backups),
                }
            } else {
                CatalogError::Sqlite(e.to_string())
            }
        };

        // Foreign keys are off by default in SQLite, which means every
        // `REFERENCES` in the schema is decoration until this runs. Deleting a
        // volume would silently orphan every folder under it.
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(refuse)?;

        // Write-ahead logging: readers do not block the writer, so browsing a
        // library stays responsive while a scan is filling it.
        //
        // The result is *checked* rather than assumed. WAL needs shared memory
        // and does not work on a network filesystem — which is exactly the
        // `VolumeId::NetworkShare` case the schema already models as having no
        // stable identity. Falling back is fine; believing we are in WAL when we
        // are not is what would make a later durability claim false.
        let journal_mode: String = connection
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(refuse)?;

        // The standard companion to WAL, and a trade worth naming: NORMAL is
        // safe against this process crashing and at risk only from an OS crash
        // or power loss, in exchange for not fsyncing every commit. FULL would
        // make a library scan several times slower for a guarantee that the
        // backups already provide.
        connection
            .execute_batch("PRAGMA synchronous = NORMAL;")
            .map_err(refuse)?;

        let mut catalog = Self {
            connection,
            journal_mode,
            path,
        };
        catalog.check_integrity()?;
        catalog.migrate()?;
        Ok(catalog)
    }

    /// Ask SQLite whether the file is sound, and refuse it if not.
    ///
    /// The full check rather than `quick_check`: it reads every page and
    /// verifies every index, and on a catalog sized for a real library — tens of
    /// megabytes, not the photos themselves — it costs single-digit
    /// milliseconds. The weaker check would save time that nobody is spending.
    fn check_integrity(&self) -> Result<(), CatalogError> {
        let report: String = self
            .connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(|e| {
                if is_corruption(&e) {
                    CatalogError::Corrupt {
                        report: e.to_string(),
                        backups: describe(&self.backup_dir()),
                    }
                } else {
                    CatalogError::Sqlite(e.to_string())
                }
            })?;
        if report == "ok" {
            return Ok(());
        }
        Err(CatalogError::Corrupt {
            report,
            backups: describe(&self.backup_dir()),
        })
    }

    /// Where this catalog's backups live: `<stem>-backups/` beside the file.
    ///
    /// Beside it on purpose — a backup that does not travel when the library is
    /// copied to another disk is not a backup of that library.
    pub fn backup_dir(&self) -> Option<PathBuf> {
        backup_dir_for(self.path.as_deref())
    }

    /// The journal mode SQLite actually settled on, which is not always the one
    /// that was asked for. See [`Catalog::from_connection`].
    pub fn journal_mode(&self) -> &str {
        &self.journal_mode
    }

    /// Where this catalog lives, or `None` for an in-memory one.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The schema version this catalog is at.
    ///
    /// SQLite's own `user_version` rather than a table of our own: it is a
    /// header field, so reading it cannot fail on a catalog whose tables we do
    /// not yet understand — which is exactly the catalog we most need to read a
    /// version from.
    pub fn version(&self) -> Result<u32, CatalogError> {
        Ok(self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? as u32)
    }

    fn migrate(&mut self) -> Result<(), CatalogError> {
        let current = self.version()?;
        self.apply_all(current, pending(current)?)
    }

    /// The body of [`Catalog::migrate`], with the list passed in.
    ///
    /// Split out so the pre-migration backup can be tested. With a single
    /// migration shipped, `current > 0` and "there is work to do" are never true
    /// together in production, so the guarantee that matters most here would
    /// otherwise be the one thing with no test behind it — and it would stay
    /// that way until the day migration 2 ran on somebody's library.
    fn apply_all(&mut self, current: u32, pending: &[Migration]) -> Result<(), CatalogError> {
        if pending.is_empty() {
            return Ok(());
        }
        // A copy before the riskiest write a catalog takes, and the one backup
        // that always happens — unlike the one on close, which a killed process
        // skips. Skipped when `current` is 0, because a catalog that has never
        // had a schema has nothing yet to lose and the copy would be noise.
        if current > 0 {
            if let Some(path) = crate::backup::snapshot(self)? {
                eprintln!(
                    "catalog    : backed up to {} before migrating",
                    path.display()
                );
            }
        }
        for migration in pending {
            self.apply(migration)?;
        }
        Ok(())
    }

    /// Run one migration with foreign keys off, and refuse to commit if that
    /// left anything dangling.
    ///
    /// SQLite cannot alter a `CHECK` in place, so changing one means rebuilding
    /// the table — and dropping a table that others reference, with foreign keys
    /// *on*, performs an implicit delete that fires every `ON DELETE CASCADE`
    /// hanging off it. Rebuilding `volumes` that way would take every folder,
    /// file and image in the library with it, and would look like a successful
    /// migration.
    ///
    /// `PRAGMA foreign_keys` is a no-op inside a transaction, so it has to be
    /// set here rather than in the SQL. `foreign_key_check` inside the
    /// transaction is what keeps the safety the pragma is holding open: a
    /// rebuild that lost a row fails instead of committing.
    fn apply(&mut self, migration: &Migration) -> Result<(), CatalogError> {
        self.connection.pragma_update(None, "foreign_keys", false)?;
        let result = self.apply_within(migration);
        // Restored whether or not the migration worked: a connection left with
        // foreign keys off treats every `REFERENCES` in the schema as a comment.
        self.connection.pragma_update(None, "foreign_keys", true)?;
        result
    }

    fn apply_within(&mut self, migration: &Migration) -> Result<(), CatalogError> {
        let transaction = self.connection.transaction()?;
        transaction.execute_batch(migration.sql)?;

        let dangling: i64 =
            transaction.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })?;
        if dangling > 0 {
            return Err(CatalogError::Sqlite(format!(
                "migration {} ({}) left {dangling} row(s) referencing something that is gone; \
                 rolled back",
                migration.version, migration.name
            )));
        }

        // Bumped inside the same transaction as the schema change. Separately,
        // a crash between the two would leave a catalog claiming a version whose
        // tables never arrived — the failure this ordering exists to prevent.
        transaction.pragma_update(None, "user_version", migration.version as i64)?;
        transaction.commit()?;
        Ok(())
    }

    /// The underlying connection, for the queries that live in this crate.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Mutable access, for the transactions a scan runs in.
    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }
}

impl Drop for Catalog {
    /// A rolling copy on the way out.
    ///
    /// Best effort by necessity: `Drop` cannot fail usefully and does not run at
    /// all when a process is killed. That is why the backup before a migration
    /// exists — this one is the convenience, that one is the guarantee.
    fn drop(&mut self) {
        if self.path.is_none() {
            return;
        }
        if let Err(e) = crate::backup::snapshot(self) {
            eprintln!("catalog    : could not back up on close: {e}");
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::SCHEMA_VERSION;

    /// A scratch directory that cleans itself up.
    ///
    /// Hand-rolled rather than `tempfile`: it is nine lines, it is only needed
    /// by tests, and every dependency in this workspace costs a licence review.
    pub(crate) struct Scratch(PathBuf);

    impl std::ops::Deref for Scratch {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn tempdir() -> Scratch {
        // The address of a local is unique among live allocations, which is
        // enough to keep concurrent tests apart without a counter.
        let unique = &0u8 as *const u8 as usize;
        let path = std::env::temp_dir().join(format!(
            "rawkit-catalog-{unique:x}-{:?}",
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }

    fn tables(catalog: &Catalog) -> Vec<String> {
        let mut statement = catalog
            .connection()
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap();
        let names = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        names
    }

    /// A catalog carrying only migration 1, the way a user's would be before
    /// upgrading. Built by hand because `open` always migrates to the top.
    fn at_version_one() -> Catalog {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .unwrap();
        let mut catalog = Catalog {
            connection,
            journal_mode: "memory".into(),
            path: None,
        };
        catalog.apply_all(0, &crate::MIGRATIONS[..1]).unwrap();
        assert_eq!(catalog.version().unwrap(), 1);
        catalog
    }

    #[test]
    fn migrating_to_v2_keeps_everything_hanging_off_the_rebuilt_volumes_table() {
        // The hazard this migration is built around. Changing a CHECK means
        // rebuilding the table, and `DROP TABLE volumes` with foreign keys ON
        // performs an implicit delete that fires every ON DELETE CASCADE
        // beneath it — taking every folder, file and image in the library, and
        // reporting success. A library that opens and is empty.
        let mut catalog = at_version_one();
        catalog
            .connection
            .execute_batch(
                "INSERT INTO volumes (id, kind, uuid, last_mount_path, path_convention)
                      VALUES (1, 'uuid', 'abc-123', '/photos', 'exact');
                 INSERT INTO folders (id, volume_id, relative_path, path_key)
                      VALUES (1, 1, '2026', '2026');
                 INSERT INTO files (id, folder_id, filename, filename_key, size, mtime, imported_at)
                      VALUES (1, 1, 'a.ARW', 'a.arw', 10, 20, 30);
                 INSERT INTO images (id, file_id, created_at) VALUES (1, 1, 30);",
            )
            .unwrap();

        catalog.apply_all(1, &crate::MIGRATIONS[1..]).unwrap();
        assert_eq!(catalog.version().unwrap(), SCHEMA_VERSION);

        let counts: Vec<i64> = ["volumes", "folders", "files", "images"]
            .iter()
            .map(|table| {
                catalog
                    .connection
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                    .unwrap()
            })
            .collect();
        assert_eq!(counts, [1, 1, 1, 1], "the library survived the rebuild");

        // And the volume kept its identity rather than being copied as a blank.
        let uuid: String = catalog
            .connection
            .query_row("SELECT uuid FROM volumes WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(uuid, "abc-123");
    }

    #[test]
    fn v2_admits_a_volume_with_no_stable_identity() {
        // The point of the rebuild: a filesystem with no UUID can be catalogued.
        let catalog = Catalog::in_memory().unwrap();
        catalog
            .connection
            .execute(
                "INSERT INTO volumes (kind, mount_path, path_convention)
                      VALUES ('mount_path', '/tmp', 'exact')",
                [],
            )
            .unwrap();

        // ...and the CHECK still refuses a row claiming two identities at once,
        // which is how a relink matches the wrong drive.
        assert!(catalog
            .connection
            .execute(
                "INSERT INTO volumes (kind, uuid, mount_path, path_convention)
                      VALUES ('mount_path', 'abc', '/tmp2', 'exact')",
                [],
            )
            .is_err());
    }

    #[test]
    fn a_fresh_catalog_arrives_at_the_current_version() {
        let catalog = Catalog::in_memory().unwrap();
        assert_eq!(catalog.version().unwrap(), SCHEMA_VERSION);
        assert_eq!(
            tables(&catalog),
            [
                "edit_states",
                "files",
                "folders",
                "images",
                "previews",
                "volumes"
            ],
            "the spine plus previews, and nothing speculative alongside them"
        );
    }

    #[test]
    fn migrating_twice_changes_nothing() {
        // Reopening a catalog is the common case, and it must be a no-op rather
        // than an attempt to create tables that already exist.
        let catalog = Catalog::in_memory().unwrap();
        let before = tables(&catalog);
        let mut catalog = catalog;
        catalog.migrate().unwrap();
        assert_eq!(tables(&catalog), before);
        assert_eq!(catalog.version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn a_catalog_on_disk_uses_write_ahead_logging() {
        // Readers not blocking the writer is what keeps browsing responsive
        // while a scan is filling the library, so it is worth asserting rather
        // than assuming the pragma took.
        let dir = tempdir();
        let catalog = Catalog::open(&dir.join("library.rawkit")).unwrap();
        assert_eq!(catalog.journal_mode(), "wal");
    }

    #[test]
    fn an_in_memory_catalog_reports_whatever_it_actually_got() {
        // Memory databases cannot do WAL, and the point of recording the mode
        // rather than assuming it is that this case is visible instead of a
        // false claim.
        let catalog = Catalog::in_memory().unwrap();
        assert_ne!(catalog.journal_mode(), "wal");
        assert!(!catalog.journal_mode().is_empty());
    }

    #[test]
    fn a_damaged_catalog_is_refused_and_says_where_the_backups_are() {
        use std::io::{Seek, SeekFrom, Write};
        let dir = tempdir();
        let path = dir.join("library.rawkit");
        {
            let catalog = Catalog::open(&path).unwrap();
            // Something to corrupt. An empty file's damage can hide in slack.
            for i in 0..200 {
                catalog
                    .connection()
                    .execute(
                        "INSERT INTO volumes (kind, uuid, path_convention)
                         VALUES ('uuid', ?1, 'exact')",
                        [format!("volume-{i}")],
                    )
                    .unwrap();
            }
        }

        // Scribble over the middle of the file, past the header so it is still
        // recognisably a database — the realistic shape of disk corruption, and
        // the one an eager `open` would happily write more data into.
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let len = file.metadata().unwrap().len();
        file.seek(SeekFrom::Start(len / 2)).unwrap();
        file.write_all(&[0xa5; 2048]).unwrap();
        file.sync_all().unwrap();
        drop(file);

        match Catalog::open(&path) {
            Err(CatalogError::Corrupt { backups, .. }) => {
                assert!(
                    backups.ends_with("library-backups"),
                    "the refusal must name where the copies are, got {backups}"
                );
            }
            Err(e) => panic!("refused, but not as corruption: {e}"),
            Ok(_) => panic!("a damaged catalog was opened, which is how damage spreads"),
        }
    }

    #[test]
    fn backups_live_beside_the_catalog() {
        let dir = tempdir();
        let path = dir.join("library.rawkit");
        let catalog = Catalog::open(&path).unwrap();
        assert_eq!(catalog.backup_dir().unwrap(), dir.join("library-backups"));
    }

    #[test]
    fn a_catalog_with_a_schema_is_backed_up_before_it_is_migrated() {
        let dir = tempdir();
        let path = dir.join("library.rawkit");
        let backups = dir.join("library-backups");
        let mut catalog = Catalog::open(&path).unwrap();
        // A fresh catalog is not backed up: it has never had a schema, so there
        // is nothing a copy would protect.
        assert!(!backups.exists(), "an empty catalog needs no backup");

        catalog
            .connection()
            .execute(
                "INSERT INTO volumes (kind, uuid, path_convention) VALUES ('uuid', 'v', 'exact')",
                [],
            )
            .unwrap();

        // A migration that would be catastrophic if it ran without a copy first.
        let destructive = [Migration {
            version: 2,
            name: "test-only",
            sql: "DROP TABLE volumes;",
        }];
        catalog.apply_all(1, &destructive).unwrap();

        let saved: Vec<_> = std::fs::read_dir(&backups).unwrap().collect();
        assert_eq!(
            saved.len(),
            1,
            "the pre-migration copy is the one guarantee"
        );

        let restored = Catalog::open(&saved[0].as_ref().unwrap().path()).unwrap();
        let volumes: i64 = restored
            .connection()
            .query_row("SELECT count(*) FROM volumes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(volumes, 1, "the backup predates the destructive migration");
    }

    #[test]
    fn closing_a_catalog_leaves_a_copy() {
        let dir = tempdir();
        let path = dir.join("library.rawkit");
        let backups = dir.join("library-backups");
        drop(Catalog::open(&path).unwrap());
        assert!(
            backups.read_dir().unwrap().next().is_some(),
            "a clean close should leave a backup behind"
        );
    }

    #[test]
    fn a_catalog_from_the_future_is_refused() {
        let catalog = Catalog::in_memory().unwrap();
        catalog
            .connection()
            .pragma_update(None, "user_version", SCHEMA_VERSION as i64 + 1)
            .unwrap();
        let mut catalog = catalog;
        assert!(matches!(
            catalog.migrate(),
            Err(CatalogError::CatalogIsNewer { .. })
        ));
    }

    #[test]
    fn foreign_keys_are_enforced_rather_than_decorative() {
        // Off by default in SQLite, which would make every REFERENCES in the
        // schema a comment.
        let catalog = Catalog::in_memory().unwrap();
        let orphan = catalog.connection().execute(
            "INSERT INTO folders (volume_id, relative_path, path_key) VALUES (99, 'a', 'a')",
            [],
        );
        assert!(
            orphan.is_err(),
            "a folder on no volume must not be storable"
        );
    }

    #[test]
    fn a_volume_must_carry_exactly_one_kind_of_identity() {
        let catalog = Catalog::in_memory().unwrap();
        let insert = |kind: &str, uuid: Option<&str>, serial: Option<i64>| {
            catalog.connection().execute(
                "INSERT INTO volumes (kind, uuid, windows_serial, path_convention)
                 VALUES (?1, ?2, ?3, 'exact')",
                rusqlite::params![kind, uuid, serial],
            )
        };
        assert!(insert("uuid", Some("abc"), None).is_ok());
        assert!(
            insert("uuid", None, Some(1)).is_err(),
            "a uuid volume identified by a Windows serial is a contradiction"
        );
        assert!(
            insert("windows_serial", Some("abc"), Some(1)).is_err(),
            "two identities at once is how a relink matches the wrong drive"
        );
    }

    #[test]
    fn edit_states_are_versioned_not_overwritten() {
        let catalog = Catalog::in_memory().unwrap();
        let db = catalog.connection();
        db.execute(
            "INSERT INTO volumes (id, kind, uuid, path_convention) VALUES (1, 'uuid', 'v', 'exact')",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO folders (id, volume_id, relative_path, path_key) VALUES (1, 1, 'p', 'p')",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO files (id, folder_id, filename, filename_key, size, mtime, imported_at)
             VALUES (1, 1, 'a.arw', 'a.arw', 1, 0, 0)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO images (id, file_id, created_at) VALUES (1, 1, 0)",
            [],
        )
        .unwrap();

        let add = |version: i64, source: &str| {
            db.execute(
                "INSERT INTO edit_states (image_id, version, json, edit_state_hash, source, created_at)
                 VALUES (1, ?1, '{}', 'h', ?2, 0)",
                rusqlite::params![version, source],
            )
        };
        assert!(add(1, "model").is_ok());
        assert!(add(2, "user").is_ok(), "a correction is a new version");
        assert!(
            add(2, "user").is_err(),
            "versions are unique per image, or history has two version 2s"
        );

        let count: i64 = db
            .query_row("SELECT count(*) FROM edit_states", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 2,
            "the model proposal and the user's correction are one supervised example; \
             overwriting would have destroyed it"
        );
    }

    #[test]
    fn an_unknown_edit_source_is_refused() {
        // The strings are a compatibility surface: a row written today is read
        // by a build years from now, so the set is closed in the schema as well
        // as in the enum.
        let catalog = Catalog::in_memory().unwrap();
        let db = catalog.connection();
        db.execute("INSERT INTO volumes (id, kind, uuid, path_convention) VALUES (1, 'uuid', 'v', 'exact')", []).unwrap();
        db.execute(
            "INSERT INTO folders (id, volume_id, relative_path, path_key) VALUES (1, 1, 'p', 'p')",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO files (id, folder_id, filename, filename_key, size, mtime, imported_at) VALUES (1, 1, 'a', 'a', 1, 0, 0)", []).unwrap();
        db.execute(
            "INSERT INTO images (id, file_id, created_at) VALUES (1, 1, 0)",
            [],
        )
        .unwrap();
        assert!(db
            .execute(
                "INSERT INTO edit_states (image_id, version, json, edit_state_hash, source, created_at)
                 VALUES (1, 1, '{}', 'h', 'ai', 0)",
                [],
            )
            .is_err(), "'ai' was the old name for 'model' and must not be storable");
    }

    /// Every value `EditSource` can take must be one the schema accepts.
    #[test]
    fn the_enum_and_the_schema_agree_on_every_source() {
        use rawkit_editstate::EditSource;
        let catalog = Catalog::in_memory().unwrap();
        let db = catalog.connection();
        db.execute("INSERT INTO volumes (id, kind, uuid, path_convention) VALUES (1, 'uuid', 'v', 'exact')", []).unwrap();
        db.execute(
            "INSERT INTO folders (id, volume_id, relative_path, path_key) VALUES (1, 1, 'p', 'p')",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO files (id, folder_id, filename, filename_key, size, mtime, imported_at) VALUES (1, 1, 'a', 'a', 1, 0, 0)", []).unwrap();
        db.execute(
            "INSERT INTO images (id, file_id, created_at) VALUES (1, 1, 0)",
            [],
        )
        .unwrap();

        for (version, source) in [
            EditSource::User,
            EditSource::Preset,
            EditSource::Import,
            EditSource::Model,
        ]
        .into_iter()
        .enumerate()
        {
            let column = crate::source_column(source);
            db.execute(
                "INSERT INTO edit_states (image_id, version, json, edit_state_hash, source, created_at)
                 VALUES (1, ?1, '{}', 'h', ?2, 0)",
                rusqlite::params![version as i64 + 1, column],
            )
            .unwrap_or_else(|e| panic!("the schema rejects {column:?}, which the enum produces: {e}"));
        }
    }
}
