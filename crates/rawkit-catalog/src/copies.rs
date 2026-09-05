//! A second interpretation of one photograph.
//!
//! # What a virtual copy is, and what it is not
//!
//! It is a second row in `images` over the same file: its own edit history, its
//! own rating, flag and label, and its own previews. It is **not** a second
//! file, and nothing is duplicated on disk — which is the whole point. A colour
//! version and a mono version of one frame cost one RAW.
//!
//! The schema has carried `is_virtual_copy` and `copy_name` since the first
//! migration, and `scan` has always inserted exactly one image per file with a
//! comment saying a copy is something a person asks for. This is the asking.
//!
//! # Why a copy starts undecided
//!
//! It inherits the edit and none of the judgement. Inheriting the flag would put
//! two files in every export of the picks the moment anyone tried an
//! alternative, which is the opposite of what trying an alternative is for: the
//! reason to make a copy is to decide between them, and a copy that arrives
//! already picked has answered the question it was made to ask.

use crate::{db::Catalog, edits, CatalogError};
use rawkit_editstate::{EditSource, EditState};

/// One interpretation of a photograph, for listing what a file carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    pub image_id: i64,
    /// `None` for the photograph itself, which is not a copy of anything.
    pub name: Option<String>,
}

/// Make a virtual copy of an image, carrying `state` as its first edit.
///
/// The state is a parameter rather than something read from the source image,
/// because the edit worth forking is the one **on screen** — sliders that have
/// moved since the last save included. A copy of what the catalog last happened
/// to write is a copy of a moment nobody chose.
///
/// A copy of a copy is a copy of the same file: the tree is one deep, because
/// two levels would need a parent column and nothing has ever wanted one.
pub fn create(
    catalog: &Catalog,
    source: i64,
    name: Option<&str>,
    state: &EditState,
) -> Result<i64, CatalogError> {
    let file_id: i64 = catalog
        .connection()
        .query_row("SELECT file_id FROM images WHERE id = ?1", [source], |r| {
            r.get(0)
        })
        .map_err(|_| CatalogError::Sqlite(format!("no image {source} to copy")))?;

    let taken: Vec<String> = all_for_file(catalog, file_id)?
        .into_iter()
        .filter_map(|copy| copy.name)
        .collect();
    let name = match name {
        Some(given) => given.trim().to_string(),
        None => next_name(&taken),
    };
    if name.is_empty() {
        return Err(CatalogError::Unsupported("a copy needs a name"));
    }
    // Refused rather than allowed to collide: the name is how a person tells two
    // interpretations apart in a grid, and it reaches the filename of anything
    // exported. Two called the same thing would overwrite each other's file.
    if taken.contains(&name) {
        return Err(CatalogError::Sqlite(format!(
            "this photograph already has a copy called {name:?}"
        )));
    }

    catalog.connection().execute(
        "INSERT INTO images (file_id, is_virtual_copy, copy_name, created_at)
         VALUES (?1, 1, ?2, ?3)",
        rusqlite::params![file_id, name, seconds_now()],
    )?;
    let id = catalog.connection().last_insert_rowid();
    // Not in one transaction with the insert, and deliberately: an image with no
    // edit is an ordinary state — every scanned photograph is one — so a failure
    // here leaves a copy that renders as shot rather than a catalog to repair.
    edits::save(catalog, id, state, EditSource::User)?;
    Ok(id)
}

/// Remove a virtual copy, and everything hanging off it.
///
/// Its edit history, its snapshots and its preview rows go with it, by the
/// cascades the schema declares. The preview *files* are left for
/// [`crate::previews::sweep`], which is where every other orphan is dealt with.
///
/// Refused for a photograph that is not a copy. Deleting that row would leave a
/// file in the catalog that no image refers to — invisible to a cull, invisible
/// to an export, and recoverable only by rescanning.
pub fn delete(catalog: &Catalog, image_id: i64) -> Result<(), CatalogError> {
    let is_copy: Option<i64> = catalog
        .connection()
        .query_row(
            "SELECT is_virtual_copy FROM images WHERE id = ?1",
            [image_id],
            |r| r.get(0),
        )
        .ok();
    match is_copy {
        None => Err(CatalogError::Sqlite(format!("no image {image_id}"))),
        Some(0) => Err(CatalogError::Unsupported(
            "that is the photograph itself, not a copy of it",
        )),
        Some(_) => {
            catalog
                .connection()
                .execute("DELETE FROM images WHERE id = ?1", [image_id])?;
            Ok(())
        }
    }
}

/// Every interpretation of one file, the original first.
pub fn all_for_file(catalog: &Catalog, file_id: i64) -> Result<Vec<Copy>, CatalogError> {
    let mut statement = catalog.connection().prepare(
        "SELECT id, copy_name FROM images WHERE file_id = ?1
          ORDER BY is_virtual_copy, id",
    )?;
    let rows = statement
        .query_map([file_id], |r| {
            Ok(Copy {
                image_id: r.get(0)?,
                name: r.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The first `copy N` nobody has used.
///
/// Counting rather than numbering: deleting `copy 2` and making another should
/// give `copy 2` back, not `copy 3` with a hole where the second one was.
fn next_name(taken: &[String]) -> String {
    (1..)
        .map(|n| format!("copy {n}"))
        .find(|name| !taken.contains(name))
        .expect("an unused name exists in an unbounded sequence")
}

fn seconds_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cull;
    use crate::db::tests::{tempdir, Scratch};

    fn library(dir: &Scratch) -> Catalog {
        let photos = dir.join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        std::fs::write(photos.join("a.ARW"), b"raw").unwrap();
        std::fs::write(photos.join("b.ARW"), b"raw").unwrap();
        let mut catalog = Catalog::open(&dir.join("library.rawkit")).unwrap();
        crate::scan::scan_on(
            &mut catalog,
            &photos,
            crate::VolumeId::Uuid("test-volume".into()),
            |path: &std::path::Path| {
                let name = path.file_stem()?.to_string_lossy().into_owned();
                Some(crate::scan::FileMetadata {
                    captured_at: Some(if name == "a" { 100 } else { 200 }),
                    ..crate::scan::FileMetadata::default()
                })
            },
        )
        .unwrap();
        catalog
    }

    fn warmer() -> EditState {
        let mut state = EditState::default();
        state.tone.exposure_ev = 0.8;
        state
    }

    #[test]
    fn a_copy_carries_the_edit_it_was_made_from() {
        // The reason the state is a parameter: what gets forked is what is on
        // screen, and a copy that arrived as shot would throw away the work that
        // prompted somebody to try an alternative.
        let dir = tempdir();
        let catalog = library(&dir);
        let original = cull::sequence(&catalog, &cull::Filter::default()).unwrap()[0].id;

        let copy = create(&catalog, original, None, &warmer()).unwrap();
        let (version, state) = edits::latest(&catalog, copy).unwrap().expect("an edit");
        assert_eq!(version, 1, "a copy starts its own history");
        assert_eq!(state.tone.exposure_ev, 0.8);
    }

    #[test]
    fn a_copy_is_a_second_image_over_one_file_and_not_a_second_file() {
        let dir = tempdir();
        let catalog = library(&dir);
        let original = cull::sequence(&catalog, &cull::Filter::default()).unwrap()[0].id;
        create(&catalog, original, Some("mono"), &EditState::default()).unwrap();

        let files: i64 = catalog
            .connection()
            .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(files, 2, "no file was invented");
        let images = cull::sequence(&catalog, &cull::Filter::default()).unwrap();
        assert_eq!(images.len(), 3);
        // And it stands next to the photograph it came from, because the
        // sequence sorts by capture time and then by id — a copy shares the
        // first and follows on the second.
        assert_eq!(images[0].copy_name, None);
        assert_eq!(images[1].copy_name.as_deref(), Some("mono"));
        assert_eq!(images[0].filename, images[1].filename);
        assert_eq!(images[2].filename, "b.ARW");
    }

    #[test]
    fn a_copy_is_judged_on_its_own() {
        // The whole reason culling metadata sits on the image rather than the
        // file. Two interpretations of one frame are two decisions.
        let dir = tempdir();
        let catalog = library(&dir);
        let original = cull::sequence(&catalog, &cull::Filter::default()).unwrap()[0].id;
        cull::set(
            &catalog,
            original,
            &cull::Judgement {
                flag: Some(cull::Flag::Pick),
                rating: Some(5),
                colour: None,
            },
        )
        .unwrap();

        let copy = create(&catalog, original, None, &EditState::default()).unwrap();
        assert_eq!(
            cull::judgement(&catalog, copy).unwrap(),
            cull::Judgement::default(),
            "a copy arrived already picked, which answers the question it was made to ask"
        );

        cull::set(
            &catalog,
            copy,
            &cull::Judgement {
                flag: Some(cull::Flag::Reject),
                ..cull::Judgement::default()
            },
        )
        .unwrap();
        assert_eq!(
            cull::judgement(&catalog, original).unwrap().flag,
            Some(cull::Flag::Pick),
            "judging the copy changed the original"
        );
    }

    #[test]
    fn copies_are_named_for_the_gaps_rather_than_counted() {
        let dir = tempdir();
        let catalog = library(&dir);
        let original = cull::sequence(&catalog, &cull::Filter::default()).unwrap()[0].id;
        let one = create(&catalog, original, None, &EditState::default()).unwrap();
        let two = create(&catalog, original, None, &EditState::default()).unwrap();
        assert_eq!(
            all_for_file(&catalog, file_of(&catalog, original))
                .unwrap()
                .iter()
                .filter_map(|c| c.name.clone())
                .collect::<Vec<_>>(),
            ["copy 1", "copy 2"]
        );

        delete(&catalog, one).unwrap();
        let again = create(&catalog, original, None, &EditState::default()).unwrap();
        assert_ne!(again, two);
        let names: Vec<String> = all_for_file(&catalog, file_of(&catalog, original))
            .unwrap()
            .into_iter()
            .filter_map(|c| c.name)
            .collect();
        assert!(
            names.contains(&"copy 1".to_string()),
            "the freed name was not reused: {names:?}"
        );
    }

    fn file_of(catalog: &Catalog, image: i64) -> i64 {
        catalog
            .connection()
            .query_row("SELECT file_id FROM images WHERE id = ?1", [image], |r| {
                r.get(0)
            })
            .unwrap()
    }

    #[test]
    fn two_copies_cannot_share_a_name() {
        // The name reaches the filename of anything exported, so a collision is
        // one file overwriting another.
        let dir = tempdir();
        let catalog = library(&dir);
        let original = cull::sequence(&catalog, &cull::Filter::default()).unwrap()[0].id;
        create(&catalog, original, Some("mono"), &EditState::default()).unwrap();
        assert!(create(&catalog, original, Some("mono"), &EditState::default()).is_err());
        assert!(create(&catalog, original, Some("  "), &EditState::default()).is_err());
    }

    #[test]
    fn deleting_a_copy_takes_its_edits_and_leaves_the_photograph() {
        let dir = tempdir();
        let catalog = library(&dir);
        let original = cull::sequence(&catalog, &cull::Filter::default()).unwrap()[0].id;
        let copy = create(&catalog, original, None, &warmer()).unwrap();
        crate::snapshots::take(&catalog, copy, "before", &warmer()).unwrap();

        delete(&catalog, copy).unwrap();
        assert!(edits::latest(&catalog, copy).unwrap().is_none());
        let snapshots: i64 = catalog
            .connection()
            .query_row(
                "SELECT count(*) FROM snapshots WHERE image_id = ?1",
                [copy],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(snapshots, 0, "a snapshot outlived the copy it named");
        assert_eq!(
            cull::sequence(&catalog, &cull::Filter::default())
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn the_photograph_itself_cannot_be_deleted_this_way() {
        // It would leave a file no image refers to: invisible to a cull, to an
        // export and to a filter, and recoverable only by rescanning.
        let dir = tempdir();
        let catalog = library(&dir);
        let original = cull::sequence(&catalog, &cull::Filter::default()).unwrap()[0].id;
        assert!(delete(&catalog, original).is_err());
        assert!(delete(&catalog, 9999).is_err());
    }
}
