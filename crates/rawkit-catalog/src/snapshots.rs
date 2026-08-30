//! A place to come back to, inside one photograph.
//!
//! # Why there is no snapshot table of edits
//!
//! `edit_states` already keeps every version an image has ever had — that is
//! what makes a history panel possible. A snapshot is therefore not a second
//! copy of an edit; it is a **name on a version that already exists**. Two
//! consequences, both the reason for the design: the name and the edit cannot
//! drift apart, because there is only one edit; and restoring a snapshot is the
//! same operation as any other edit change, so it appends a version like
//! everything else rather than rewriting history behind the user.
//!
//! # Snapshots and presets are not the same thing
//!
//! A preset is partial and travels between photographs. A snapshot is whole and
//! stays in one, crop and orientation included, because "put it back how it was"
//! means all of it.

use crate::{db::Catalog, CatalogError};
use rawkit_editstate::{EditSource, EditState};

/// A named version of one image's edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub name: String,
    /// The `edit_states` version this names.
    pub version: u32,
}

/// Name where this image is now, so it can be returned to.
///
/// Takes the state rather than reading the head, because the state the user is
/// looking at may not have reached the catalog yet — a snapshot of the last
/// saved version would silently be a snapshot of something else. Saving first
/// deduplicates as usual, so snapshotting twice without changing anything names
/// the same version twice rather than making two.
pub fn take(
    catalog: &Catalog,
    image_id: i64,
    name: &str,
    state: &EditState,
) -> Result<u32, CatalogError> {
    if name.trim().is_empty() {
        return Err(CatalogError::Unsupported("a snapshot needs a name"));
    }
    let version = match crate::edits::save(catalog, image_id, state, EditSource::User)? {
        Some(written) => written,
        // Unchanged, so the head is already this state.
        None => head_version(catalog, image_id)?,
    };

    catalog.connection().execute(
        "INSERT INTO snapshots (image_id, version, name, created_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (image_id, name) DO UPDATE
            SET version = excluded.version, created_at = excluded.created_at",
        rusqlite::params![image_id, version, name.trim(), now()],
    )?;
    Ok(version)
}

/// This image's snapshots, oldest first — the order they were taken in, which is
/// the order the work happened in and so the one a user can navigate by.
pub fn all(catalog: &Catalog, image_id: i64) -> Result<Vec<Snapshot>, CatalogError> {
    let mut statement = catalog.connection().prepare(
        "SELECT name, version FROM snapshots WHERE image_id = ?1 ORDER BY created_at, name",
    )?;
    let rows = statement
        .query_map([image_id], |r| {
            Ok(Snapshot {
                name: r.get(0)?,
                version: r.get::<_, i64>(1)? as u32,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The edit a snapshot names.
///
/// Returns it rather than installing it: applying belongs to whatever holds the
/// session, and the version it appends is an ordinary one. Going back is a step
/// forward in the history, which is what makes it undoable.
pub fn read(
    catalog: &Catalog,
    image_id: i64,
    name: &str,
) -> Result<Option<EditState>, CatalogError> {
    let json: Option<String> = catalog
        .connection()
        .query_row(
            "SELECT e.json FROM snapshots s
               JOIN edit_states e ON e.image_id = s.image_id AND e.version = s.version
              WHERE s.image_id = ?1 AND s.name = ?2",
            rusqlite::params![image_id, name],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;

    let Some(json) = json else {
        return Ok(None);
    };
    let state: EditState = serde_json::from_str(&json)
        .map_err(|e| CatalogError::Sqlite(format!("snapshot {name:?}: {e}")))?;
    state.validate()?;
    Ok(Some(state))
}

/// Forget a snapshot. The version it named stays in the history, because it is
/// still a thing that happened.
pub fn forget(catalog: &Catalog, image_id: i64, name: &str) -> Result<(), CatalogError> {
    catalog.connection().execute(
        "DELETE FROM snapshots WHERE image_id = ?1 AND name = ?2",
        rusqlite::params![image_id, name],
    )?;
    Ok(())
}

fn head_version(catalog: &Catalog, image_id: i64) -> Result<u32, CatalogError> {
    catalog
        .connection()
        .query_row(
            "SELECT version FROM edit_states WHERE image_id = ?1 ORDER BY version DESC LIMIT 1",
            [image_id],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v as u32)
        .map_err(|e| CatalogError::Sqlite(format!("image {image_id} has no edits: {e}")))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library_with_one_image() -> (crate::db::tests::Scratch, Catalog, i64) {
        let dir = crate::db::tests::tempdir();
        let photos = dir.join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        std::fs::write(photos.join("a.ARW"), b"x").unwrap();
        let mut catalog = Catalog::open(&dir.join("library.rawkit")).unwrap();
        crate::scan::scan_on(
            &mut catalog,
            &photos,
            crate::VolumeId::Uuid("test-volume".into()),
            crate::scan::no_metadata,
        )
        .unwrap();
        let image: i64 = catalog
            .connection()
            .query_row("SELECT id FROM images", [], |r| r.get(0))
            .unwrap();
        (dir, catalog, image)
    }

    fn at_contrast(contrast: f32) -> EditState {
        let mut s = EditState::default();
        s.tone.contrast = contrast;
        s
    }

    #[test]
    fn an_image_starts_with_no_snapshots() {
        let (_dir, c, image) = library_with_one_image();
        assert!(all(&c, image).unwrap().is_empty());
        assert_eq!(read(&c, image, "Flat").unwrap(), None);
    }

    #[test]
    fn a_snapshot_gives_back_the_state_it_was_taken_from() {
        let (_dir, c, image) = library_with_one_image();
        let state = at_contrast(0.4);
        take(&c, image, "Punchy", &state).unwrap();
        assert_eq!(read(&c, image, "Punchy").unwrap(), Some(state));
    }

    #[test]
    fn a_snapshot_survives_the_edit_moving_on() {
        // The whole point: it is a place to come back to.
        let (_dir, c, image) = library_with_one_image();
        take(&c, image, "Punchy", &at_contrast(0.4)).unwrap();
        crate::edits::save(&c, image, &at_contrast(-0.9), EditSource::User).unwrap();

        assert_eq!(
            crate::edits::latest(&c, image).unwrap().unwrap().1,
            at_contrast(-0.9)
        );
        assert_eq!(
            read(&c, image, "Punchy").unwrap(),
            Some(at_contrast(0.4)),
            "the snapshot still names the version it was taken at"
        );
    }

    #[test]
    fn a_snapshot_names_a_version_that_is_in_the_history() {
        // Not a second copy of the edit — the same row the history panel shows.
        let (_dir, c, image) = library_with_one_image();
        let version = take(&c, image, "Punchy", &at_contrast(0.4)).unwrap();
        let history = crate::edits::history(&c, image).unwrap();
        assert!(history.iter().any(|(v, _, _)| *v == version));
    }

    #[test]
    fn snapshotting_an_unchanged_edit_does_not_add_a_version() {
        // `edits::save` deduplicates; naming the head twice must not defeat it.
        let (_dir, c, image) = library_with_one_image();
        let state = at_contrast(0.4);
        let first = take(&c, image, "One", &state).unwrap();
        let second = take(&c, image, "Two", &state).unwrap();

        assert_eq!(first, second, "one state is one version");
        assert_eq!(crate::edits::history(&c, image).unwrap().len(), 1);
        assert_eq!(all(&c, image).unwrap().len(), 2);
    }

    #[test]
    fn taking_a_snapshot_again_under_one_name_moves_it() {
        let (_dir, c, image) = library_with_one_image();
        take(&c, image, "Here", &at_contrast(0.1)).unwrap();
        take(&c, image, "Here", &at_contrast(0.8)).unwrap();

        assert_eq!(all(&c, image).unwrap().len(), 1);
        assert_eq!(read(&c, image, "Here").unwrap(), Some(at_contrast(0.8)));
    }

    #[test]
    fn forgetting_a_snapshot_keeps_the_version_it_named() {
        // It is still a thing that happened.
        let (_dir, c, image) = library_with_one_image();
        take(&c, image, "Here", &at_contrast(0.1)).unwrap();
        forget(&c, image, "Here").unwrap();

        assert!(all(&c, image).unwrap().is_empty());
        assert_eq!(crate::edits::history(&c, image).unwrap().len(), 1);
    }

    #[test]
    fn a_snapshot_needs_a_name() {
        let (_dir, c, image) = library_with_one_image();
        assert!(take(&c, image, "   ", &at_contrast(0.1)).is_err());
    }

    #[test]
    fn snapshots_of_one_image_are_that_images_alone() {
        let (dir, mut c, first) = library_with_one_image();
        std::fs::write(dir.join("photos").join("b.ARW"), b"y").unwrap();
        crate::scan::scan_on(
            &mut c,
            &dir.join("photos"),
            crate::VolumeId::Uuid("test-volume".into()),
            crate::scan::no_metadata,
        )
        .unwrap();
        let second: i64 = c
            .connection()
            .query_row("SELECT id FROM images WHERE id <> ?1", [first], |r| {
                r.get(0)
            })
            .unwrap();

        take(&c, first, "Here", &at_contrast(0.1)).unwrap();
        assert!(all(&c, second).unwrap().is_empty());
        assert_eq!(read(&c, second, "Here").unwrap(), None);
    }
}
