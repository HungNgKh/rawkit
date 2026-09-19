//! Photographs in an order somebody chose.
//!
//! # Why this is not a saved filter
//!
//! The library already answers "which frames satisfy this rule", and answers it
//! better than a stored list could: a filter is re-evaluated, so it is never out
//! of date. What it cannot do is hold an order. Every filtered view is in the
//! order the library sorts by, and nothing in it lets one frame be put before
//! another.
//!
//! A collection is the other question — *these ones, in this sequence, because I
//! said so*. A print run, a submission, the running order of an edit. That is
//! what [`position`](members) is for, and it is the whole reason this is a table
//! rather than a rule.
//!
//! # Static, deliberately
//!
//! There is no `kind` column and no stored rules. Smart collections are saved
//! *filters*, and the filter language they would be saved in does not exist yet
//! — today's [`Filter`] holds a flag, a rating and a colour. Adding the column
//! now would be storing an answer to a question nobody can ask, and the shape of
//! that answer is exactly what the search work will decide.
//!
//! # A filter still narrows a collection
//!
//! [`members`] takes one, and it is the same [`crate::cull::narrowing`] the rest
//! of the library uses rather than a second opinion about what a pick is. Being
//! in a collection and being a pick are different questions, and a person part
//! way through choosing forty frames wants to ask both at once.

use crate::cull::{narrowing, Filter, LibraryImage};
use crate::{db::Catalog, CatalogError};

/// A collection, and how many photographs are in it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Collection {
    pub id: i64,
    /// `None` at the top level. A parent is an ordinary collection that may hold
    /// photographs of its own — see the migration for why there is no separate
    /// kind of container.
    pub parent_id: Option<i64>,
    pub name: String,
    /// The one a keypress adds to. Exactly one row has this, and the catalog
    /// enforces it rather than the code that creates them.
    pub is_quick: bool,
    /// Members, counted here because a list of collections is always drawn with
    /// them and asking per row is the N+1 this avoids.
    pub count: usize,
}

/// Every collection, the quick one first and the rest by name.
///
/// Flat, with `parent_id` carried: the shape is a tree and the caller is the
/// only one that knows whether it is drawing an indented list, a menu or a
/// breadcrumb. Building the tree here would decide that for it.
pub fn all(catalog: &Catalog) -> Result<Vec<Collection>, CatalogError> {
    let mut statement = catalog.connection().prepare(
        "SELECT c.id, c.parent_id, c.name, c.is_quick, COUNT(m.image_id)
           FROM collections c
           LEFT JOIN collection_images m ON m.collection_id = c.id
          GROUP BY c.id
          ORDER BY c.is_quick DESC, c.name",
    )?;
    let rows = statement
        .query_map([], |r| {
            Ok(Collection {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                name: r.get(2)?,
                is_quick: r.get::<_, i64>(3)? == 1,
                count: r.get::<_, i64>(4)? as usize,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The quick collection's id. It is created with the schema, so this is a read.
pub fn quick(catalog: &Catalog) -> Result<i64, CatalogError> {
    Ok(catalog.connection().query_row(
        "SELECT id FROM collections WHERE is_quick = 1",
        [],
        |r| r.get(0),
    )?)
}

/// Make one, and answer with its id.
pub fn create(catalog: &Catalog, name: &str, parent_id: Option<i64>) -> Result<i64, CatalogError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CatalogError::Unsupported("a collection needs a name"));
    }
    catalog.connection().execute(
        "INSERT INTO collections (parent_id, name, is_quick, created_at)
         VALUES (?1, ?2, 0, ?3)",
        rusqlite::params![parent_id, name, now()],
    )?;
    Ok(catalog.connection().last_insert_rowid())
}

/// Rename one. The quick collection may be renamed like any other — `is_quick`
/// is what identifies it, not what it is called.
pub fn rename(catalog: &Catalog, id: i64, name: &str) -> Result<(), CatalogError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CatalogError::Unsupported("a collection needs a name"));
    }
    catalog.connection().execute(
        "UPDATE collections SET name = ?2 WHERE id = ?1",
        rusqlite::params![id, name],
    )?;
    Ok(())
}

/// Delete one, and everything nested inside it. The photographs are untouched.
///
/// Refuses the quick collection: a keypress has to have somewhere to put a
/// frame, and "there is exactly one" stops being true the moment this can make
/// it zero. Emptying it is [`clear`].
pub fn remove(catalog: &Catalog, id: i64) -> Result<(), CatalogError> {
    let is_quick: Option<i64> = catalog
        .connection()
        .query_row(
            "SELECT is_quick FROM collections WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .ok();
    if is_quick == Some(1) {
        return Err(CatalogError::Unsupported(
            "the quick collection cannot be deleted; empty it instead",
        ));
    }
    catalog.connection().execute(
        "DELETE FROM collections WHERE id = ?1",
        rusqlite::params![id],
    )?;
    Ok(())
}

/// Take every photograph out, and leave the collection.
pub fn clear(catalog: &Catalog, id: i64) -> Result<usize, CatalogError> {
    Ok(catalog.connection().execute(
        "DELETE FROM collection_images WHERE collection_id = ?1",
        rusqlite::params![id],
    )?)
}

/// Put photographs at the end, in the order given, and answer with how many were
/// not already there.
///
/// Adding one twice is not an error: it is somebody pressing the key again on a
/// frame that is already in. The position of a frame already present does not
/// move, because re-adding is not a request to reorder.
pub fn add(catalog: &Catalog, id: i64, images: &[i64]) -> Result<usize, CatalogError> {
    let connection = catalog.connection();
    let transaction = connection.unchecked_transaction()?;
    let mut next: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(position), 0) FROM collection_images WHERE collection_id = ?1",
        rusqlite::params![id],
        |r| r.get(0),
    )?;
    let at = now();
    let mut added = 0;
    for image in images {
        next += 1;
        added += transaction.execute(
            "INSERT INTO collection_images (collection_id, image_id, position, added_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (collection_id, image_id) DO NOTHING",
            rusqlite::params![id, image, next, at],
        )?;
    }
    transaction.commit()?;
    Ok(added)
}

/// Take photographs out, and answer with how many were in.
///
/// The gap this leaves in `position` is left alone. Nothing reads the absolute
/// value, so closing it would be a write per remaining row for no visible
/// difference — see the migration.
pub fn take_out(catalog: &Catalog, id: i64, images: &[i64]) -> Result<usize, CatalogError> {
    let connection = catalog.connection();
    let transaction = connection.unchecked_transaction()?;
    let mut removed = 0;
    for image in images {
        removed += transaction.execute(
            "DELETE FROM collection_images WHERE collection_id = ?1 AND image_id = ?2",
            rusqlite::params![id, image],
        )?;
    }
    transaction.commit()?;
    Ok(removed)
}

/// Whether this photograph is in this collection.
pub fn holds(catalog: &Catalog, id: i64, image: i64) -> Result<bool, CatalogError> {
    Ok(catalog.connection().query_row(
        "SELECT EXISTS (SELECT 1 FROM collection_images
                         WHERE collection_id = ?1 AND image_id = ?2)",
        rusqlite::params![id, image],
        |r| r.get::<_, i64>(0),
    )? == 1)
}

/// Write an order. The list is the collection's sequence from first to last.
///
/// Whole rather than a move-this-one-here, for the reason a judgement is written
/// whole: the caller holds the sequence the person is looking at, and a shuffle
/// expressed as a series of moves can be interrupted half way and leave an order
/// nobody asked for.
///
/// Ids not in the collection are ignored; members left out keep their places
/// after the ones named, which is what dragging a few frames to the front means.
pub fn reorder(catalog: &Catalog, id: i64, images: &[i64]) -> Result<(), CatalogError> {
    let connection = catalog.connection();
    let transaction = connection.unchecked_transaction()?;
    // Named frames take the low positions in the order given; anything else
    // keeps its relative order behind them. Offsetting the rest rather than
    // renumbering them keeps this to one statement plus the named rows.
    transaction.execute(
        "UPDATE collection_images SET position = position + ?2 WHERE collection_id = ?1",
        rusqlite::params![id, images.len() as i64],
    )?;
    for (at, image) in images.iter().enumerate() {
        transaction.execute(
            "UPDATE collection_images SET position = ?3
              WHERE collection_id = ?1 AND image_id = ?2",
            rusqlite::params![id, image, at as i64 + 1],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

/// What is in a collection, in the order somebody put it in.
///
/// The filter narrows *within* the collection and is the library's own
/// [`narrowing`], so "picks in this collection" is one question rather than two
/// answers that have to agree.
pub fn members(
    catalog: &Catalog,
    id: i64,
    filter: &Filter,
) -> Result<Vec<LibraryImage>, CatalogError> {
    let (narrowed, values) = narrowing(filter);
    let mut statement = catalog.connection().prepare(&format!(
        "SELECT i.id,
                v.last_mount_path || '/' || d.relative_path || '/' || f.filename,
                f.filename,
                i.copy_name
           FROM collection_images m
           JOIN images i ON i.id = m.image_id
           JOIN files f ON f.id = i.file_id
           JOIN folders d ON d.id = f.folder_id
           JOIN volumes v ON v.id = d.volume_id
          WHERE m.collection_id = ? AND f.missing = 0{narrowed}
          ORDER BY m.position, i.id"
    ))?;
    // The collection binds first, which is the rule `narrowing` states: its own
    // placeholders are positional and come after anything the caller adds.
    let mut binds: Vec<rusqlite::types::Value> = vec![id.into()];
    binds.extend(values);
    let rows = statement
        .query_map(rusqlite::params_from_iter(binds), |r| {
            Ok(LibraryImage {
                id: r.get(0)?,
                path: r.get::<_, String>(1)?.replace("//", "/"),
                filename: r.get(2)?,
                copy_name: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
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
    use crate::cull::{self, Flagged, Judgement};

    /// A library of `n` photographs, named `a.ARW` upward, with their ids in
    /// filename order.
    fn library_of(n: usize) -> (crate::db::tests::Scratch, Catalog, Vec<i64>) {
        let dir = crate::db::tests::tempdir();
        let photos = dir.join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        for i in 0..n {
            let name = format!("{}.ARW", (b'a' + i as u8) as char);
            std::fs::write(photos.join(name), b"x").unwrap();
        }
        let mut catalog = Catalog::open(&dir.join("library.rawkit")).unwrap();
        crate::scan::scan_on(
            &mut catalog,
            &photos,
            crate::VolumeId::Uuid("test-volume".into()),
            crate::scan::no_metadata,
        )
        .unwrap();
        let ids = {
            let mut statement = catalog
                .connection()
                .prepare(
                    "SELECT i.id FROM images i JOIN files f ON f.id = i.file_id
                           ORDER BY f.filename",
                )
                .unwrap();
            let ids = statement
                .query_map([], |r| r.get::<_, i64>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            ids
        };
        assert_eq!(ids.len(), n, "the scan did not find every photograph");
        (dir, catalog, ids)
    }

    fn ids_in(catalog: &Catalog, id: i64) -> Vec<i64> {
        members(catalog, id, &Filter::default())
            .unwrap()
            .into_iter()
            .map(|image| image.id)
            .collect()
    }

    #[test]
    fn a_collection_keeps_the_order_it_was_given() {
        // **The claim the table exists for.** A filter can express "these
        // frames"; nothing in the library can express "in this order". If this
        // came back in capture order the whole thing would be a saved filter
        // with extra steps.
        let (_dir, catalog, ids) = library_of(3);
        let id = create(&catalog, "Portfolio", None).unwrap();
        let chosen = [ids[2], ids[0], ids[1]];
        assert_eq!(add(&catalog, id, &chosen).unwrap(), 3);
        assert_eq!(ids_in(&catalog, id), chosen);
    }

    #[test]
    fn adding_a_frame_twice_does_not_move_it() {
        // Pressing the key again on a frame already in is not a request to
        // reorder, and it is not an error either — it is somebody making sure.
        let (_dir, catalog, ids) = library_of(3);
        let id = create(&catalog, "Portfolio", None).unwrap();
        add(&catalog, id, &[ids[0], ids[1]]).unwrap();
        assert_eq!(
            add(&catalog, id, &[ids[0], ids[2]]).unwrap(),
            1,
            "only the one that was not already in counts as added"
        );
        assert_eq!(ids_in(&catalog, id), [ids[0], ids[1], ids[2]]);
    }

    #[test]
    fn reorder_puts_the_named_frames_first() {
        let (_dir, catalog, ids) = library_of(4);
        let id = create(&catalog, "Edit", None).unwrap();
        add(&catalog, id, &ids).unwrap();
        // Dragging two frames to the front leaves the rest behind them in the
        // order they already had.
        reorder(&catalog, id, &[ids[3], ids[1]]).unwrap();
        assert_eq!(ids_in(&catalog, id), [ids[3], ids[1], ids[0], ids[2]]);
    }

    #[test]
    fn taking_a_frame_out_leaves_the_others_in_order() {
        // The gap left in `position` is deliberate — see the migration. What
        // matters is that nothing else moved.
        let (_dir, catalog, ids) = library_of(3);
        let id = create(&catalog, "Edit", None).unwrap();
        add(&catalog, id, &ids).unwrap();
        assert_eq!(take_out(&catalog, id, &[ids[1]]).unwrap(), 1);
        assert_eq!(take_out(&catalog, id, &[ids[1]]).unwrap(), 0, "already out");
        assert_eq!(ids_in(&catalog, id), [ids[0], ids[2]]);
    }

    #[test]
    fn a_filter_narrows_within_a_collection() {
        // Being in a collection and being a pick are different questions, and
        // somebody part way through choosing wants both at once. The filter is
        // the library's own, not a second opinion about what a pick is.
        let (_dir, catalog, ids) = library_of(3);
        let id = create(&catalog, "Portfolio", None).unwrap();
        add(&catalog, id, &ids).unwrap();
        cull::set(
            &catalog,
            ids[1],
            &Judgement {
                rating: None,
                flag: Some(cull::Flag::Pick),
                colour: None,
            },
        )
        .unwrap();
        let picks = Filter {
            flagged: Some(Flagged::Pick),
            ..Filter::default()
        };
        let shown: Vec<i64> = members(&catalog, id, &picks)
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(shown, [ids[1]]);
    }

    #[test]
    fn the_quick_collection_is_there_from_the_start_and_cannot_be_deleted() {
        // It is created with the schema so that "there is exactly one" needs no
        // code to be true, and a keypress always has somewhere to put a frame.
        let (_dir, catalog, ids) = library_of(2);
        let quick_id = quick(&catalog).unwrap();
        assert!(
            all(&catalog).unwrap().iter().any(|c| c.is_quick),
            "a catalog with no quick collection has a key that does nothing"
        );
        add(&catalog, quick_id, &[ids[0]]).unwrap();
        assert!(holds(&catalog, quick_id, ids[0]).unwrap());
        assert!(!holds(&catalog, quick_id, ids[1]).unwrap());

        assert!(remove(&catalog, quick_id).is_err(), "it must survive");
        // Emptying it is the thing that was actually wanted.
        assert_eq!(clear(&catalog, quick_id).unwrap(), 1);
        assert!(!holds(&catalog, quick_id, ids[0]).unwrap());
        assert_eq!(quick(&catalog).unwrap(), quick_id, "still exactly one");
    }

    #[test]
    fn deleting_a_collection_takes_its_children_and_leaves_the_photographs() {
        let (_dir, catalog, ids) = library_of(2);
        let parent = create(&catalog, "2026", None).unwrap();
        let child = create(&catalog, "March", Some(parent)).unwrap();
        add(&catalog, child, &ids).unwrap();

        remove(&catalog, parent).unwrap();
        assert!(
            !all(&catalog).unwrap().iter().any(|c| c.id == child),
            "a nested collection outliving its parent is unreachable"
        );
        // The photographs are not the collection's to delete.
        let left: i64 = catalog
            .connection()
            .query_row("SELECT COUNT(*) FROM images", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 2);
    }

    #[test]
    fn siblings_cannot_share_a_name() {
        // Two rows reading "Portfolio" in one list are indistinguishable where
        // they are chosen from. Nesting is what makes the same name legitimate.
        let (_dir, catalog, _ids) = library_of(1);
        let parent = create(&catalog, "2026", None).unwrap();
        create(&catalog, "Portfolio", None).unwrap();
        assert!(create(&catalog, "Portfolio", None).is_err());
        create(&catalog, "Portfolio", Some(parent)).unwrap();
        assert!(create(&catalog, "Portfolio", Some(parent)).is_err());
        assert!(create(&catalog, "   ", None).is_err(), "a name is required");
    }

    #[test]
    fn a_listing_carries_how_many_are_in_each() {
        // Drawn with every list, so counting per row would be the N+1 this
        // avoids.
        let (_dir, catalog, ids) = library_of(3);
        let id = create(&catalog, "Portfolio", None).unwrap();
        add(&catalog, id, &ids[..2]).unwrap();
        let listed = all(&catalog).unwrap();
        let portfolio = listed.iter().find(|c| c.id == id).unwrap();
        assert_eq!(portfolio.count, 2);
        assert_eq!(
            listed.first().map(|c| c.is_quick),
            Some(true),
            "quick leads"
        );
    }
}
