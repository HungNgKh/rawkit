//! The catalog: SQLite is authoritative at runtime, the schema is documented and
//! versioned on purpose.
//!
//! # Why the migration runner exists before the schema does
//!
//! The public beta ships at the end of P1 — roughly fifteen months before v1.0 —
//! which means schema migrations have to run on *other people's* catalogs for
//! most of the build. That is affordable only if the runner exists from the
//! first schema. Added later, two years of user catalogs cannot be carried
//! forward, and a catalog app that loses a library is unrecoverable
//! reputationally.
//!
//! So this crate starts with the migration machinery and no tables. The tables
//! are P1; the ability to change them safely is P0.
//!
//! # Two invariants that are cheaper to hold now than to add later
//!
//! - **Volume identity is not a path.** See [`VolumeId`]. The column set has to
//!   cover Linux, Windows and macOS from the first schema, because changing it
//!   later means migrating strangers' catalogs.
//! - **`edit_states.source` is recorded from the first write.** Every edit is
//!   stored as a versioned `(image → EditState)` pair tagged with where it came
//!   from. A model proposal that a user then corrects is exactly one supervised
//!   example — recorded years before anything needs it, and impossible to
//!   backfill if the column is added later.

use serde::{Deserialize, Serialize};

pub mod backup;
pub mod cull;
pub mod db;
pub mod edits;
pub mod path;
pub mod previews;
pub mod relink;
pub mod scan;
pub mod volume;

/// The schema version this build expects, and the last entry in [`MIGRATIONS`].
/// A catalog reporting anything higher was written by a newer build and is
/// refused rather than half-understood.
pub const SCHEMA_VERSION: u32 = 2;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// The catalog was written by a newer build. Refuse rather than guess: a
    /// half-understood catalog that opens is worse than one that does not.
    #[error("catalog schema v{found} is newer than this build understands (v{expected}) — upgrade rawkit")]
    CatalogIsNewer { found: u32, expected: u32 },
    #[error("migration set is not contiguous: v{expected} is missing")]
    MigrationGap { expected: u32 },
    #[error("sqlite: {0}")]
    Sqlite(String),
    #[error("filesystem: {0}")]
    Io(String),
    #[error("{0}")]
    Unsupported(&'static str),
    /// A stored edit this build cannot faithfully render.
    #[error(transparent)]
    EditState(#[from] rawkit_editstate::EditStateError),
    /// The catalog failed SQLite's own integrity check.
    ///
    /// Opening it anyway and then writing to it makes the damage worse, so this
    /// is a refusal. The message names the backups directory because a corrupt
    /// catalog is the one moment a user needs to be told where the copies are.
    #[error(
        "this catalog is damaged and was not opened.\n{report}\nRecent backups are in {backups}"
    )]
    Corrupt { report: String, backups: String },
}

/// One forward-only schema change.
///
/// Forward-only on purpose: down-migrations on a photo library are a way to
/// destroy data that the user cannot get back, and the rollback story is a
/// restored backup rather than a reversed migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Migration {
    /// Applied in ascending order; must be contiguous from 1.
    pub version: u32,
    /// Human-readable, appears in logs and in the backup filename.
    pub name: &'static str,
    /// Executed inside a transaction, together with the version bump.
    pub sql: &'static str,
}

/// Every migration, in order.
///
/// The SQL lives in files rather than in string literals so it can be read and
/// diffed as SQL. Once a migration has shipped its text is frozen: editing it
/// changes what a catalog already carrying that version *thinks* it has, and the
/// only safe correction is another migration.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "spine",
        sql: include_str!("../migrations/001-spine.sql"),
    },
    Migration {
        version: 2,
        name: "previews-and-weak-volumes",
        sql: include_str!("../migrations/002-previews-and-weak-volumes.sql"),
    },
];

/// Migrations that a catalog at `current_version` still needs.
///
/// Errors if the catalog is *newer* than this build rather than silently doing
/// nothing, because "nothing to migrate" and "this file is from the future" have
/// very different correct responses.
pub fn pending(current_version: u32) -> Result<&'static [Migration], CatalogError> {
    if current_version > SCHEMA_VERSION {
        return Err(CatalogError::CatalogIsNewer {
            found: current_version,
            expected: SCHEMA_VERSION,
        });
    }
    let start = MIGRATIONS
        .iter()
        .position(|m| m.version > current_version)
        .unwrap_or(MIGRATIONS.len());
    Ok(&MIGRATIONS[start..])
}

/// Checks the migration list is well-formed: contiguous from 1, and ending at
/// [`SCHEMA_VERSION`]. Called by the test below, so a mistake is caught in CI
/// rather than on a user's catalog.
pub fn validate_migrations() -> Result<(), CatalogError> {
    for (i, m) in MIGRATIONS.iter().enumerate() {
        let expected = i as u32 + 1;
        if m.version != expected {
            return Err(CatalogError::MigrationGap { expected });
        }
    }
    if MIGRATIONS.len() as u32 != SCHEMA_VERSION {
        return Err(CatalogError::MigrationGap {
            expected: MIGRATIONS.len() as u32 + 1,
        });
    }
    Ok(())
}

/// How a storage volume is identified across sessions and across machines.
///
/// A path is not identity: drives get remounted, letters get reassigned, and
/// external disks arrive somewhere different every time. The three OSes disagree
/// about what *is* identity, so the union is modelled here — designing this
/// column set now is cheaper than migrating strangers' catalogs later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VolumeId {
    /// Linux and macOS: filesystem UUID.
    Uuid(String),
    /// Windows: volume serial number.
    WindowsSerial(u32),
    /// Network shares, which have neither: identified by their mount target.
    /// Weakest form, and flagged as such so relinking can fall back to hashes.
    NetworkShare { host: String, share: String },
    /// A local filesystem that has no stable identity to offer: tmpfs,
    /// overlayfs, a container mount, some network mounts.
    ///
    /// Added because refusing these outright locked out anyone whose photos sit
    /// on one — CI found it, since its runners are on exactly such a filesystem.
    /// It is deliberately *not* spelled as a `NetworkShare`, which would have
    /// satisfied the schema while lying about what the volume is; this says
    /// plainly that the identity is the mount point and therefore weak.
    MountPath(String),
}

impl VolumeId {
    /// Whether this identity survives a remount. Network shares and bare mount
    /// paths do not, which is why `files.content_hash` is the relink fallback
    /// rather than an optimisation.
    pub fn is_stable(&self) -> bool {
        !matches!(self, VolumeId::NetworkShare { .. } | VolumeId::MountPath(_))
    }
}

/// Where a stored `EditState` came from.
///
/// Mirrors `rawkit_editstate::EditSource`; the catalog persists it as a string
/// column so that a value written by a future build round-trips through an older
/// one instead of being dropped.
pub fn source_column(source: rawkit_editstate::EditSource) -> &'static str {
    use rawkit_editstate::EditSource;
    match source {
        EditSource::User => "user",
        EditSource::Preset => "preset",
        EditSource::Import => "import",
        EditSource::Model => "model",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_list_is_contiguous() {
        validate_migrations().expect("migrations must be contiguous from 1");
        assert_eq!(
            MIGRATIONS.last().map(|m| m.version).unwrap_or(0),
            SCHEMA_VERSION,
            "SCHEMA_VERSION must equal the last migration"
        );
    }

    #[test]
    fn a_fresh_catalog_needs_every_migration() {
        assert_eq!(pending(0).unwrap().len(), MIGRATIONS.len());
    }

    #[test]
    fn a_newer_catalog_is_refused_not_opened() {
        let err = pending(SCHEMA_VERSION + 1).unwrap_err();
        assert!(matches!(err, CatalogError::CatalogIsNewer { .. }));
    }

    #[test]
    fn network_shares_are_known_to_be_weak_identities() {
        assert!(VolumeId::Uuid("f0e1".into()).is_stable());
        assert!(VolumeId::WindowsSerial(0xDEAD_BEEF).is_stable());
        assert!(!VolumeId::NetworkShare {
            host: "nas".into(),
            share: "photos".into()
        }
        .is_stable());
    }

    #[test]
    fn every_edit_source_has_a_stable_column_value() {
        use rawkit_editstate::EditSource;
        // Persisted strings are a compatibility surface: changing one silently
        // reinterprets rows written by every earlier build.
        assert_eq!(source_column(EditSource::User), "user");
        assert_eq!(source_column(EditSource::Preset), "preset");
        assert_eq!(source_column(EditSource::Import), "import");
        assert_eq!(source_column(EditSource::Model), "model");
    }
}
