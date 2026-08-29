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
use std::path::Path;

impl From<rusqlite::Error> for CatalogError {
    fn from(e: rusqlite::Error) -> Self {
        CatalogError::Sqlite(e.to_string())
    }
}

/// An open catalog, migrated to [`SCHEMA_VERSION`].
pub struct Catalog {
    connection: Connection,
}

impl Catalog {
    /// Open or create a catalog at `path`, applying whatever migrations it needs.
    pub fn open(path: &Path) -> Result<Self, CatalogError> {
        Self::from_connection(Connection::open(path)?)
    }

    /// A catalog held only in memory. For tests, and for asking what a fresh
    /// schema looks like without writing one to disk.
    pub fn in_memory() -> Result<Self, CatalogError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self, CatalogError> {
        // Foreign keys are off by default in SQLite, which means every
        // `REFERENCES` in the schema is decoration until this runs. Deleting a
        // volume would silently orphan every folder under it.
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
        let mut catalog = Self { connection };
        catalog.migrate()?;
        Ok(catalog)
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
        for migration in pending(current)? {
            self.apply(migration)?;
        }
        Ok(())
    }

    fn apply(&mut self, migration: &Migration) -> Result<(), CatalogError> {
        let transaction = self.connection.transaction()?;
        transaction.execute_batch(migration.sql)?;
        // Bumped inside the same transaction as the schema change. Separately,
        // a crash between the two would leave a catalog claiming a version whose
        // tables never arrived — the failure this ordering exists to prevent.
        transaction.pragma_update(None, "user_version", migration.version as i64)?;
        transaction.commit()?;
        Ok(())
    }

    /// The underlying connection, for the queries that will live in this crate
    /// once there is something to store.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SCHEMA_VERSION;

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

    #[test]
    fn a_fresh_catalog_arrives_at_the_current_version() {
        let catalog = Catalog::in_memory().unwrap();
        assert_eq!(catalog.version().unwrap(), SCHEMA_VERSION);
        assert_eq!(
            tables(&catalog),
            ["edit_states", "files", "folders", "images", "volumes"],
            "the spine, and nothing speculative alongside it"
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
