//! Storing what someone decided about a photograph.
//!
//! # Versions, not overwrites
//!
//! Every save appends. That is what makes a history panel possible, and it is
//! what makes the `source` column worth having: a `Model` proposal followed by a
//! `User` correction is one supervised example, recorded years before anything
//! needs it and impossible to reconstruct if the earlier row were overwritten.
//!
//! # Why saving is deduplicated
//!
//! A slider drag produces commands far faster than anyone means to make
//! decisions. Writing a row per command would fill a library with thousands of
//! versions describing one gesture, and the history panel would be unreadable
//! for the same reason the disk was full.
//!
//! So [`save`] compares against the latest version and does nothing when the
//! edit has not actually changed. The comparison is `EditState::content_hash`,
//! which the catalog already stores per version because the preview cache is
//! keyed on it — one value serving both jobs rather than two that can disagree.

use crate::{db::Catalog, CatalogError};
use rawkit_editstate::{EditSource, EditState};

/// The most recent edit for an image, if it has one.
pub fn latest(catalog: &Catalog, image_id: i64) -> Result<Option<(u32, EditState)>, CatalogError> {
    let row: Option<(i64, String)> = catalog
        .connection()
        .query_row(
            "SELECT version, json FROM edit_states
              WHERE image_id = ?1 ORDER BY version DESC LIMIT 1",
            [image_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();

    let Some((version, json)) = row else {
        return Ok(None);
    };
    let state: EditState = serde_json::from_str(&json).map_err(|e| {
        // A state we cannot parse is one this build does not understand. Saying
        // so beats rendering the photograph with a default edit and letting the
        // user believe that is what they had saved.
        CatalogError::Sqlite(format!("edit_states v{version} for image {image_id}: {e}"))
    })?;
    state.validate()?;
    Ok(Some((version as u32, state)))
}

/// Append a version, unless nothing changed.
///
/// Returns the version written, or `None` when the edit was identical to the
/// one already on top.
pub fn save(
    catalog: &Catalog,
    image_id: i64,
    state: &EditState,
    source: EditSource,
) -> Result<Option<u32>, CatalogError> {
    let hash = state.content_hash();
    let head: Option<(i64, String)> = catalog
        .connection()
        .query_row(
            "SELECT version, edit_state_hash FROM edit_states
              WHERE image_id = ?1 ORDER BY version DESC LIMIT 1",
            [image_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();

    if let Some((_, previous)) = &head {
        if previous == &hash {
            return Ok(None);
        }
    }

    let version = head.map(|(v, _)| v).unwrap_or(0) + 1;
    let json = serde_json::to_string(state)
        .map_err(|e| CatalogError::Sqlite(format!("serialising an edit: {e}")))?;
    catalog.connection().execute(
        "INSERT INTO edit_states (image_id, version, json, edit_state_hash, source, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            image_id,
            version,
            json,
            hash,
            crate::source_column(source),
            seconds_now()
        ],
    )?;
    Ok(Some(version as u32))
}

/// Every version of an image's edit, oldest first, for a history panel.
pub fn history(
    catalog: &Catalog,
    image_id: i64,
) -> Result<Vec<(u32, EditSource, String)>, CatalogError> {
    let mut statement = catalog.connection().prepare(
        "SELECT version, source, edit_state_hash FROM edit_states
          WHERE image_id = ?1 ORDER BY version",
    )?;
    let rows = statement
        .query_map([image_id], |r| {
            Ok((
                r.get::<_, i64>(0)? as u32,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    rows.into_iter()
        .map(|(version, source, hash)| {
            let source = match source.as_str() {
                "user" => EditSource::User,
                "preset" => EditSource::Preset,
                "import" => EditSource::Import,
                "model" => EditSource::Model,
                other => {
                    return Err(CatalogError::Sqlite(format!(
                        "edit_states v{version} has an unknown source {other:?}"
                    )))
                }
            };
            Ok((version, source, hash))
        })
        .collect()
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
    use crate::db::tests::tempdir;

    fn library_with_one_image() -> (crate::db::tests::Scratch, Catalog, i64) {
        let dir = tempdir();
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

    #[test]
    fn an_image_with_no_edits_has_none() {
        let (_dir, catalog, image) = library_with_one_image();
        assert!(latest(&catalog, image).unwrap().is_none());
        assert!(history(&catalog, image).unwrap().is_empty());
    }

    #[test]
    fn saving_appends_and_reloading_gives_back_what_went_in() {
        let (_dir, catalog, image) = library_with_one_image();
        let mut state = EditState::default();
        state.tone.exposure_ev = 1.2;
        state.white_balance.temperature_k = Some(5200.0);

        assert_eq!(
            save(&catalog, image, &state, EditSource::User).unwrap(),
            Some(1)
        );
        let (version, loaded) = latest(&catalog, image).unwrap().unwrap();
        assert_eq!(version, 1);
        assert_eq!(loaded, state, "what comes back must be what went in");
    }

    #[test]
    fn an_unchanged_edit_writes_nothing() {
        // The property that keeps a slider drag from filling the library with
        // thousands of versions of one gesture.
        let (_dir, catalog, image) = library_with_one_image();
        let state = EditState::default();
        assert_eq!(
            save(&catalog, image, &state, EditSource::User).unwrap(),
            Some(1)
        );
        assert_eq!(
            save(&catalog, image, &state, EditSource::User).unwrap(),
            None,
            "an identical edit is not a new decision"
        );
        assert_eq!(history(&catalog, image).unwrap().len(), 1);
    }

    #[test]
    fn returning_to_a_previous_value_is_still_a_new_version() {
        // Only the *latest* is compared, deliberately. Undoing to an earlier
        // state is a decision the history should show, not one it should hide by
        // noticing the value existed before.
        let (_dir, catalog, image) = library_with_one_image();
        let plain = EditState::default();
        let mut brighter = EditState::default();
        brighter.tone.exposure_ev = 1.0;

        save(&catalog, image, &plain, EditSource::User).unwrap();
        save(&catalog, image, &brighter, EditSource::User).unwrap();
        assert_eq!(
            save(&catalog, image, &plain, EditSource::User).unwrap(),
            Some(3)
        );
        assert_eq!(history(&catalog, image).unwrap().len(), 3);
    }

    #[test]
    fn a_model_proposal_and_a_correction_are_both_kept_and_distinguishable() {
        // The row pair the whole `source` column exists for.
        let (_dir, catalog, image) = library_with_one_image();
        let mut proposed = EditState::default();
        proposed.tone.exposure_ev = 0.8;
        let mut corrected = proposed.clone();
        corrected.tone.exposure_ev = 0.4;

        save(&catalog, image, &proposed, EditSource::Model).unwrap();
        save(&catalog, image, &corrected, EditSource::User).unwrap();

        let history = history(&catalog, image).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].1, EditSource::Model);
        assert_eq!(history[1].1, EditSource::User);
        assert_ne!(
            history[0].2, history[1].2,
            "two different decisions must have two different hashes"
        );
    }

    #[test]
    fn the_stored_hash_is_the_one_the_preview_cache_uses() {
        // One value serving both jobs, so a cached preview and a stored edit
        // cannot disagree about which edit they describe.
        let (_dir, catalog, image) = library_with_one_image();
        let mut state = EditState::default();
        state.tone.contrast = 0.3;
        save(&catalog, image, &state, EditSource::User).unwrap();

        let stored: String = catalog
            .connection()
            .query_row("SELECT edit_state_hash FROM edit_states", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, state.content_hash());
    }

    #[test]
    fn an_edit_this_build_cannot_read_is_refused_rather_than_defaulted() {
        // Rendering with a default edit and letting the user believe that is
        // what they saved is the worst available answer.
        let (_dir, catalog, image) = library_with_one_image();
        catalog
            .connection()
            .execute(
                "INSERT INTO edit_states (image_id, version, json, edit_state_hash, source, created_at)
                 VALUES (?1, 1, '{\"schema_version\":99}', 'h', 'user', 0)",
                [image],
            )
            .unwrap();
        assert!(latest(&catalog, image).is_err());
    }
}
