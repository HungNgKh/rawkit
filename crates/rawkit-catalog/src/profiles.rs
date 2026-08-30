//! Which colour profile renders a camera's photographs.
//!
//! Keyed by the body rather than by the photograph, because that is what a DCP
//! is: a characterisation of one sensor, not a decision about one picture. The
//! consequence is the useful one — point at a profile once and every frame from
//! that camera uses it, in the window and in an export, without choosing again
//! across a thousand-frame shoot.
//!
//! The *path* is stored, not the profile. Adobe's are not redistributable and
//! run to hundreds of kilobytes each. A catalog carried to another machine
//! therefore loses its profiles until they are pointed at again, and [`Chosen`]
//! keeps the name so a missing one can say what it was rather than only where
//! it used to be.

use crate::db::Catalog;
use crate::CatalogError;

/// A profile a camera has been given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    pub path: String,
    /// The profile's own `ProfileName`, when it had one.
    pub name: Option<String>,
}

/// Render this camera's photographs with the profile at `path`.
///
/// Replaces whatever the camera had. There is one profile per body rather than
/// a list, because a list would need a way to say which of them is in use, and
/// that is the same question asked twice.
pub fn remember(
    catalog: &Catalog,
    make: &str,
    model: &str,
    path: &str,
    name: Option<&str>,
) -> Result<(), CatalogError> {
    catalog.connection().execute(
        "INSERT INTO camera_profiles (camera_make, camera_model, path, name, chosen_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (camera_make, camera_model)
         DO UPDATE SET path = excluded.path, name = excluded.name, chosen_at = excluded.chosen_at",
        rusqlite::params![make, model, path, name, now()],
    )?;
    Ok(())
}

/// What this camera renders with, if anything was chosen.
pub fn chosen(catalog: &Catalog, make: &str, model: &str) -> Result<Option<Chosen>, CatalogError> {
    let found = catalog
        .connection()
        .query_row(
            "SELECT path, name FROM camera_profiles
             WHERE camera_make = ?1 AND camera_model = ?2",
            rusqlite::params![make, model],
            |row| {
                Ok(Chosen {
                    path: row.get(0)?,
                    name: row.get(1)?,
                })
            },
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(found)
}

/// Go back to the decoder's own matrix for this camera.
pub fn forget(catalog: &Catalog, make: &str, model: &str) -> Result<(), CatalogError> {
    catalog.connection().execute(
        "DELETE FROM camera_profiles WHERE camera_make = ?1 AND camera_model = ?2",
        rusqlite::params![make, model],
    )?;
    Ok(())
}

/// The profile for the camera that took a catalogued image.
///
/// One query rather than a decode: the scan already recorded which body took
/// each frame, so an exporter can resolve a whole selection without opening a
/// single RAW.
pub fn for_image(catalog: &Catalog, image_id: i64) -> Result<Option<Chosen>, CatalogError> {
    let found = catalog
        .connection()
        .query_row(
            "SELECT p.path, p.name
               FROM images i
               JOIN files f ON f.id = i.file_id
               JOIN camera_profiles p
                 ON p.camera_make = f.camera_make AND p.camera_model = f.camera_model
              WHERE i.id = ?1",
            rusqlite::params![image_id],
            |row| {
                Ok(Chosen {
                    path: row.get(0)?,
                    name: row.get(1)?,
                })
            },
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(found)
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

    fn catalog() -> Catalog {
        Catalog::in_memory().expect("catalog")
    }

    #[test]
    fn a_camera_with_no_profile_has_none() {
        let c = catalog();
        assert_eq!(chosen(&c, "Sony", "ILCE-6400").unwrap(), None);
    }

    #[test]
    fn a_choice_survives_being_read_back() {
        let c = catalog();
        remember(
            &c,
            "Sony",
            "ILCE-6400",
            "/p/std.dcp",
            Some("Camera Standard"),
        )
        .unwrap();
        assert_eq!(
            chosen(&c, "Sony", "ILCE-6400").unwrap(),
            Some(Chosen {
                path: "/p/std.dcp".into(),
                name: Some("Camera Standard".into())
            })
        );
        // A different body is a different question.
        assert_eq!(chosen(&c, "Sony", "ILCE-7M3").unwrap(), None);
    }

    #[test]
    fn choosing_again_replaces_rather_than_accumulates() {
        // One profile per body, so the second choice is the answer rather than
        // a second row nobody can choose between.
        let c = catalog();
        remember(&c, "Sony", "ILCE-6400", "/p/a.dcp", Some("A")).unwrap();
        remember(&c, "Sony", "ILCE-6400", "/p/b.dcp", Some("B")).unwrap();
        assert_eq!(
            chosen(&c, "Sony", "ILCE-6400").unwrap().unwrap().path,
            "/p/b.dcp"
        );
        let count: i64 = c
            .connection()
            .query_row("SELECT count(*) FROM camera_profiles", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn forgetting_returns_to_the_decoders_own_matrix() {
        let c = catalog();
        remember(&c, "Sony", "ILCE-6400", "/p/a.dcp", None).unwrap();
        forget(&c, "Sony", "ILCE-6400").unwrap();
        assert_eq!(chosen(&c, "Sony", "ILCE-6400").unwrap(), None);
    }
}
