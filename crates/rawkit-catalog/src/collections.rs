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
use rusqlite::{Connection, OptionalExtension};
use unicode_normalization::UnicodeNormalization;

/// A name as it is compared, which is not as it is shown.
///
/// Composed and lower-cased, so "Portfolio", "portfolio" and the same word typed
/// with a decomposed accent are one name. Full Unicode lower-casing rather than
/// the ASCII folding paths get: a path is compared the way a *filesystem*
/// compares it, and claiming more would merge names the disk keeps apart — but
/// a collection's name is compared by a person reading a list, and nobody reads
/// "Été" and "été" as two places to put a photograph.
pub fn name_key(name: &str) -> String {
    name.trim().nfc().collect::<String>().to_lowercase()
}

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
    /// Where the add-to-collection key puts a photograph. Exactly one row has
    /// this too; it starts on the quick collection and moves when somebody
    /// points it elsewhere.
    pub is_target: bool,
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
    // **The count is of what the collection will show**, which is not every
    // membership: a photograph whose file has gone missing stays a member and is
    // left out of the view, because nothing can open it. Counting it anyway put
    // "40" on a chip that opened on "1 of 38".
    //
    // Subtracted rather than joined. Counting memberships is one covering-index
    // range per collection; joining each of them to its file to ask whether it
    // is missing is two lookups per membership, to find what is nearly always
    // nothing. So the missing ones are found from *their* side — few files,
    // then their images, then the memberships the image-first key makes a
    // search rather than a scan.
    let mut statement = catalog.connection().prepare(
        // **`CROSS JOIN` is doing work here: in SQLite it pins the join order.**
        // Written as plain joins, the planner started from the memberships —
        // nothing tells it that missing files are the small side, there being no
        // index on `missing` — and looked up the image and the file behind every
        // one of them. That is exactly the join the paragraph above says this
        // avoids, and the scale gate caught it: 43 ms of a 45 ms query, to
        // subtract zero. I blamed the CTE first and was wrong; the plan said so.
        // Pinned, it reads the files once, finds the few that are gone, and
        // reaches their memberships through the image-first key.
        "WITH gone AS MATERIALIZED (
             SELECT m.collection_id AS id, COUNT(*) AS n
               FROM files f
              CROSS JOIN images i ON i.file_id = f.id
              CROSS JOIN collection_images m ON m.image_id = i.id
              WHERE f.missing = 1
              GROUP BY m.collection_id)
         SELECT c.id, c.parent_id, c.name, c.is_quick, c.is_target,
                (SELECT COUNT(*) FROM collection_images m WHERE m.collection_id = c.id)
                    - COALESCE(gone.n, 0)
           FROM collections c
           LEFT JOIN gone ON gone.id = c.id
          ORDER BY c.is_quick DESC, c.name_key",
    )?;
    let rows = statement
        .query_map([], |r| {
            Ok(Collection {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                name: r.get(2)?,
                is_quick: r.get::<_, i64>(3)? == 1,
                is_target: r.get::<_, i64>(4)? == 1,
                count: r.get::<_, i64>(5)?.max(0) as usize,
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

/// The collection the add-to-collection key puts a photograph in.
pub fn target(catalog: &Catalog) -> Result<i64, CatalogError> {
    Ok(catalog.connection().query_row(
        "SELECT id FROM collections WHERE is_target = 1",
        [],
        |r| r.get(0),
    )?)
}

/// Point the key somewhere else.
///
/// The old target is cleared first and in the same transaction: the index
/// allows one, so setting the new one before clearing the old is a constraint
/// failure, and clearing without setting would leave a key with nowhere to put
/// a photograph if the second half failed.
pub fn set_target(catalog: &Catalog, id: i64) -> Result<(), CatalogError> {
    let transaction = catalog.connection().unchecked_transaction()?;
    transaction.execute(
        "UPDATE collections SET is_target = 0 WHERE is_target = 1",
        [],
    )?;
    let set = transaction.execute(
        "UPDATE collections SET is_target = 1 WHERE id = ?1",
        rusqlite::params![id],
    )?;
    if set == 0 {
        // Dropped without a commit, so the old target is still the target.
        return Err(CatalogError::Unsupported("there is no such collection"));
    }
    transaction.commit()?;
    Ok(())
}

/// Make one, and answer with its id.
pub fn create(catalog: &Catalog, name: &str, parent_id: Option<i64>) -> Result<i64, CatalogError> {
    insert(catalog.connection(), name, parent_id)
}

/// Make one that already holds these photographs, or make nothing.
///
/// One transaction, because the two halves are one act to the person doing it.
/// As two — which is how the shell first did this — a failure between them
/// leaves an empty collection with a name on it that nobody chose to make empty.
pub fn create_holding(
    catalog: &Catalog,
    name: &str,
    parent_id: Option<i64>,
    images: &[i64],
) -> Result<i64, CatalogError> {
    let transaction = catalog.connection().unchecked_transaction()?;
    let id = insert(&transaction, name, parent_id)?;
    append(&transaction, id, images)?;
    transaction.commit()?;
    Ok(id)
}

// The two writes, against a connection rather than a catalog.
//
// A `Transaction` dereferences to a `Connection`, so taking one is what lets a
// caller decide where the transaction's edges are. When every function opened
// its own, nothing could be built out of two of them that either happened or
// did not — which is the shape an import of somebody's whole library will need.
fn insert(
    connection: &Connection,
    name: &str,
    parent_id: Option<i64>,
) -> Result<i64, CatalogError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CatalogError::Unsupported("a collection needs a name"));
    }
    connection.execute(
        "INSERT INTO collections (parent_id, name, name_key, is_quick, created_at)
         VALUES (?1, ?2, ?3, 0, ?4)",
        rusqlite::params![parent_id, name, name_key(name), now()],
    )?;
    Ok(connection.last_insert_rowid())
}

fn append(connection: &Connection, id: i64, images: &[i64]) -> Result<usize, CatalogError> {
    let mut next: i64 = connection.query_row(
        "SELECT COALESCE(MAX(position), 0) FROM collection_images WHERE collection_id = ?1",
        rusqlite::params![id],
        |r| r.get(0),
    )?;
    // Prepared once, for the reason `place` is: shift-K on a marked shoot is a
    // row per photograph, and compiling the statement cost more than running it.
    let mut insert = connection.prepare(
        "INSERT INTO collection_images (collection_id, image_id, position)
         VALUES (?1, ?2, ?3)
         ON CONFLICT (image_id, collection_id) DO NOTHING",
    )?;
    let mut added = 0;
    for image in images {
        next += 1;
        added += insert.execute(rusqlite::params![id, image, next])?;
    }
    Ok(added)
}

/// Rename one. The quick collection may be renamed like any other — `is_quick`
/// is what identifies it, not what it is called.
pub fn rename(catalog: &Catalog, id: i64, name: &str) -> Result<(), CatalogError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CatalogError::Unsupported("a collection needs a name"));
    }
    catalog.connection().execute(
        "UPDATE collections SET name = ?2, name_key = ?3 WHERE id = ?1",
        rusqlite::params![id, name, name_key(name)],
    )?;
    Ok(())
}

/// A photograph and the place it held in a collection.
///
/// What taking something out hands back, so that it can be put back where it
/// was. The order is the one thing about a collection that cannot be recomputed
/// — nobody can re-derive why frame forty came before frame twelve — so
/// everything that destroys some of it says exactly what it destroyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placed {
    pub image: i64,
    pub position: i64,
}

/// A deleted collection and everything that was nested in it, as [`restore`]
/// needs them. Opaque: the only thing to do with one is hand it back.
#[derive(Debug, Clone)]
pub struct Removed {
    /// Parents before children, so restoring in order never names a parent that
    /// is not there yet.
    nodes: Vec<RemovedNode>,
}

#[derive(Debug, Clone)]
struct RemovedNode {
    id: i64,
    parent_id: Option<i64>,
    name: String,
    was_target: bool,
    created_at: i64,
    members: Vec<Placed>,
}

impl Removed {
    /// How many photographs the deleted collections held between them.
    pub fn photographs(&self) -> usize {
        self.nodes.iter().map(|node| node.members.len()).sum()
    }

    /// The ids the deleted collections had. They mean nothing any more — SQLite
    /// will hand them to the next collections made — which is exactly why a
    /// caller holding one needs to know to let go of it.
    pub fn ids(&self) -> impl Iterator<Item = i64> + '_ {
        self.nodes.iter().map(|node| node.id)
    }

    /// What the deleted collection was called.
    pub fn name(&self) -> &str {
        self.nodes.first().map_or("", |node| node.name.as_str())
    }
}

/// Delete one, and everything nested inside it, and say what that was. The
/// photographs are untouched.
///
/// Refuses the quick collection: a keypress has to have somewhere to put a
/// frame, and "there is exactly one" stops being true the moment this can make
/// it zero. Emptying it is [`clear`].
///
/// **What it answers with is the whole of what it destroyed** — every nested
/// collection, every member and the place each held — because a hand-made order
/// cannot be recomputed and an undo has to be able to put it back.
pub fn remove(catalog: &Catalog, id: i64) -> Result<Removed, CatalogError> {
    let transaction = catalog.connection().unchecked_transaction()?;
    // `optional`, not `ok`. The first version turned *every* failure here into
    // "not the quick collection" — a locked database included — and went on to
    // run the delete. No row is an answer; an error is not one.
    let is_quick: Option<i64> = transaction
        .query_row(
            "SELECT is_quick FROM collections WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(is_quick) = is_quick else {
        return Err(CatalogError::Unsupported("there is no such collection"));
    };
    if is_quick == 1 {
        return Err(CatalogError::Unsupported(
            "the quick collection cannot be deleted; empty it instead",
        ));
    }

    // The collection and everything under it, shallowest first.
    let mut nodes: Vec<RemovedNode> = {
        let mut statement = transaction.prepare(
            "WITH RECURSIVE under (id, depth) AS (
                 SELECT ?1, 0
                 UNION ALL
                 SELECT c.id, under.depth + 1
                   FROM collections c JOIN under ON c.parent_id = under.id)
             SELECT c.id, c.parent_id, c.name, c.is_target, c.created_at
               FROM under JOIN collections c ON c.id = under.id
              ORDER BY under.depth, c.id",
        )?;
        let rows = statement
            .query_map(rusqlite::params![id], |r| {
                Ok(RemovedNode {
                    id: r.get(0)?,
                    parent_id: r.get(1)?,
                    name: r.get(2)?,
                    was_target: r.get::<_, i64>(3)? == 1,
                    created_at: r.get(4)?,
                    members: Vec::new(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    for node in &mut nodes {
        node.members = placed_in(&transaction, node.id)?;
    }

    transaction.execute(
        "DELETE FROM collections WHERE id = ?1",
        rusqlite::params![id],
    )?;
    // The index allows at most one target; this is what keeps it at *least*
    // one. Asked of the table rather than of `nodes`, so it is right however
    // the target came to be missing — the collection itself, or something
    // nested in it that the cascade took.
    transaction.execute(
        "UPDATE collections SET is_target = 1
          WHERE is_quick = 1
            AND NOT EXISTS (SELECT 1 FROM collections WHERE is_target = 1)",
        [],
    )?;
    transaction.commit()?;
    Ok(Removed { nodes })
}

/// Put back what [`remove`] took, and answer with the collection's new id.
///
/// **New ids, deliberately.** SQLite hands a deleted row's id to the next
/// insert, so by the time an undo arrives the old id may belong to a collection
/// made in between — and restoring under it would either fail or, worse, merge
/// into a stranger. The ids are mapped as the tree is rebuilt, and nothing
/// outside this function is given the old ones to hold.
///
/// A photograph deleted since is quietly not restored: the collection coming
/// back without a frame that no longer exists is what an undo can honestly do.
/// A *name* taken since is an error, because two collections a person cannot
/// tell apart is the thing the naming rule exists to prevent.
pub fn restore(catalog: &Catalog, removed: &Removed) -> Result<i64, CatalogError> {
    let transaction = catalog.connection().unchecked_transaction()?;
    let mut renumbered = std::collections::HashMap::new();
    let mut root = None;
    let mut target = None;
    for node in &removed.nodes {
        // A parent inside the deleted tree has a new id now; one outside it is
        // whatever it always was.
        let parent = node
            .parent_id
            .map(|old| renumbered.get(&old).copied().unwrap_or(old));
        transaction.execute(
            "INSERT INTO collections (parent_id, name, name_key, is_quick, is_target, created_at)
             VALUES (?1, ?2, ?3, 0, 0, ?4)",
            rusqlite::params![parent, node.name, name_key(&node.name), node.created_at],
        )?;
        let id = transaction.last_insert_rowid();
        renumbered.insert(node.id, id);
        root.get_or_insert(id);
        if node.was_target {
            target = Some(id);
        }
        place(&transaction, id, &node.members)?;
    }
    if let Some(id) = target {
        transaction.execute(
            "UPDATE collections SET is_target = 0 WHERE is_target = 1",
            [],
        )?;
        transaction.execute(
            "UPDATE collections SET is_target = 1 WHERE id = ?1",
            rusqlite::params![id],
        )?;
    }
    transaction.commit()?;
    root.ok_or(CatalogError::Unsupported("there was nothing to restore"))
}

/// Everything in a collection and where it sits, in order.
fn placed_in(connection: &Connection, id: i64) -> Result<Vec<Placed>, CatalogError> {
    let mut statement = connection.prepare(
        "SELECT image_id, position FROM collection_images
          WHERE collection_id = ?1 ORDER BY position, image_id",
    )?;
    let rows = statement
        .query_map(rusqlite::params![id], |r| {
            Ok(Placed {
                image: r.get(0)?,
                position: r.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Put photographs at the places they are recorded as having held.
///
/// Skips one that no longer exists, and one that is already a member — the
/// place it has now is the newer decision.
fn place(connection: &Connection, id: i64, placed: &[Placed]) -> Result<usize, CatalogError> {
    // Prepared once. `Connection::execute` compiles its SQL on every call, and
    // undoing the deletion of a collection that held a whole library is twenty
    // thousand calls — the scale gate put that at 109 ms, most of it parsing.
    let mut insert = connection.prepare(
        "INSERT INTO collection_images (collection_id, image_id, position)
         SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM images WHERE id = ?2)
         ON CONFLICT (image_id, collection_id) DO NOTHING",
    )?;
    let mut back = 0;
    for entry in placed {
        back += insert.execute(rusqlite::params![id, entry.image, entry.position])?;
    }
    Ok(back)
}

/// Put back what [`take_out`] or [`clear`] took, where it was.
pub fn put_back(catalog: &Catalog, id: i64, placed: &[Placed]) -> Result<usize, CatalogError> {
    let transaction = catalog.connection().unchecked_transaction()?;
    let back = place(&transaction, id, placed)?;
    transaction.commit()?;
    Ok(back)
}

/// Take every photograph out, leave the collection, and say what was in it.
pub fn clear(catalog: &Catalog, id: i64) -> Result<Vec<Placed>, CatalogError> {
    let transaction = catalog.connection().unchecked_transaction()?;
    let were = placed_in(&transaction, id)?;
    transaction.execute(
        "DELETE FROM collection_images WHERE collection_id = ?1",
        rusqlite::params![id],
    )?;
    transaction.commit()?;
    Ok(were)
}

/// Put photographs at the end, in the order given, and answer with how many were
/// not already there.
///
/// Adding one twice is not an error: it is somebody pressing the key again on a
/// frame that is already in. The position of a frame already present does not
/// move, because re-adding is not a request to reorder.
pub fn add(catalog: &Catalog, id: i64, images: &[i64]) -> Result<usize, CatalogError> {
    let transaction = catalog.connection().unchecked_transaction()?;
    let added = append(&transaction, id, images)?;
    transaction.commit()?;
    Ok(added)
}

/// Take photographs out, and answer with the ones that were in and the place
/// each held — which is what [`put_back`] needs to undo it.
///
/// The gap this leaves in `position` is left alone. Nothing reads the absolute
/// value, so closing it would be a write per remaining row for no visible
/// difference — and an undo puts the frame straight back into it.
pub fn take_out(catalog: &Catalog, id: i64, images: &[i64]) -> Result<Vec<Placed>, CatalogError> {
    let transaction = catalog.connection().unchecked_transaction()?;
    let mut taken = Vec::new();
    {
        // One statement that deletes and says where the row was, rather than a
        // read and then a write: a photograph that was never in the collection
        // returns no row, which is the "skip it" the undo record needs.
        let mut remove = transaction.prepare(
            "DELETE FROM collection_images WHERE collection_id = ?1 AND image_id = ?2
             RETURNING position",
        )?;
        for image in images {
            let position: Option<i64> = remove
                .query_row(rusqlite::params![id, image], |r| r.get(0))
                .optional()?;
            if let Some(position) = position {
                taken.push(Placed {
                    image: *image,
                    position,
                });
            }
        }
    }
    transaction.commit()?;
    Ok(taken)
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

/// Exchange two photographs' places, and nothing else.
///
/// **What moving one frame one place has to cost**, and the reason this exists
/// beside [`reorder`]. The shell first did a move by handing `reorder` the whole
/// sequence with two entries swapped, which is a write per member: measured on a
/// collection of twenty thousand, 70 ms for one press of an arrow key — most of
/// what an interaction is allowed, spent rewriting 19 998 rows to the values
/// they already had. Two rows changed, so two rows are written.
///
/// Works the same under a filter, where the neighbour on screen may be a long
/// way off in the stored order: exchanging the two *values* still puts the frame
/// on the other side of the one it was moved past.
pub fn swap(catalog: &Catalog, id: i64, a: i64, b: i64) -> Result<(), CatalogError> {
    // Read inside the transaction that writes. Outside it, the two positions
    // are a fact about a moment that has passed by the time they are used.
    let transaction = catalog.connection().unchecked_transaction()?;
    let place = |image: i64| -> Result<i64, CatalogError> {
        Ok(transaction.query_row(
            "SELECT position FROM collection_images WHERE collection_id = ?1 AND image_id = ?2",
            rusqlite::params![id, image],
            |r| r.get(0),
        )?)
    };
    let (at_a, at_b) = (place(a)?, place(b)?);
    for (image, position) in [(a, at_b), (b, at_a)] {
        transaction.execute(
            "UPDATE collection_images SET position = ?3
              WHERE collection_id = ?1 AND image_id = ?2",
            rusqlite::params![id, image, position],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

/// Write an order. The list is the collection's sequence from first to last.
///
/// **A write per member, so not for a keypress** — see [`swap`] for moving one
/// frame. This is for an order that really has changed wholesale.
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
    {
        let mut put = transaction.prepare(
            "UPDATE collection_images SET position = ?3
              WHERE collection_id = ?1 AND image_id = ?2",
        )?;
        for (at, image) in images.iter().enumerate() {
            put.execute(rusqlite::params![id, image, at as i64 + 1])?;
        }
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
          ORDER BY m.position, m.image_id"
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
    fn swapping_two_frames_moves_only_those_two() {
        // What one press of an arrow key is. The first version rewrote every
        // row in the collection to say so.
        let (_dir, catalog, ids) = library_of(4);
        let id = create(&catalog, "Edit", None).unwrap();
        add(&catalog, id, &ids).unwrap();
        swap(&catalog, id, ids[1], ids[2]).unwrap();
        assert_eq!(ids_in(&catalog, id), [ids[0], ids[2], ids[1], ids[3]]);
        // Not neighbours, which is what a move under a filter looks like.
        swap(&catalog, id, ids[0], ids[3]).unwrap();
        assert_eq!(ids_in(&catalog, id), [ids[3], ids[2], ids[1], ids[0]]);
        // A frame that is not in the collection is an error, not a silent no-op
        // that leaves the screen and the catalog disagreeing.
        assert!(swap(&catalog, id, ids[0], 999_999).is_err());
    }

    #[test]
    fn taking_a_frame_out_leaves_the_others_in_order() {
        // The gap left in `position` is deliberate — see the migration. What
        // matters is that nothing else moved.
        let (_dir, catalog, ids) = library_of(3);
        let id = create(&catalog, "Edit", None).unwrap();
        add(&catalog, id, &ids).unwrap();
        assert_eq!(take_out(&catalog, id, &[ids[1]]).unwrap().len(), 1);
        assert!(
            take_out(&catalog, id, &[ids[1]]).unwrap().is_empty(),
            "already out"
        );
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
        assert_eq!(clear(&catalog, quick_id).unwrap().len(), 1);
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
    fn a_name_is_compared_the_way_a_person_reads_it() {
        // Two rows that differ by a capital are a coin toss every time one is
        // picked from a list. Decided before any catalog exists that breaks the
        // rule, because a uniqueness rule can be loosened later and cannot be
        // tightened.
        let (_dir, catalog, _ids) = library_of(1);
        create(&catalog, "Portfolio", None).unwrap();
        assert!(create(&catalog, "portfolio", None).is_err());
        assert!(create(&catalog, "  PORTFOLIO ", None).is_err());
        // The same word, typed with the accent as a separate combining mark.
        create(&catalog, "\u{00c9}t\u{00e9}", None).unwrap();
        assert!(create(&catalog, "E\u{0301}te\u{0301}", None).is_err());
        // Renaming obeys it too, or it is a rule with a way round it.
        let other = create(&catalog, "Prints", None).unwrap();
        assert!(rename(&catalog, other, "PORTFOLIO").is_err());
        // And the name somebody typed is what comes back, not the key.
        assert!(all(&catalog).unwrap().iter().any(|c| c.name == "Portfolio"));
    }

    #[test]
    fn the_quick_collections_name_is_not_taken() {
        // Its identity is a flag and its name is an English label the migration
        // chose. A library imported from somewhere else with a top-level
        // collection called the same thing must not fail on a row the user
        // never made.
        let (_dir, catalog, _ids) = library_of(1);
        create(&catalog, "Quick Collection", None).unwrap();
        assert_eq!(
            all(&catalog).unwrap().iter().filter(|c| c.is_quick).count(),
            1
        );
    }

    #[test]
    fn the_quick_collection_cannot_be_put_inside_another() {
        // `remove` guards the id it is handed. Nested, the quick collection
        // would go when its *parent* was deleted, and the shell cannot open a
        // catalog that has none. Nothing re-parents yet; the rule is in the
        // schema so that whatever does cannot get this wrong.
        let (_dir, catalog, _ids) = library_of(1);
        let parent = create(&catalog, "2026", None).unwrap();
        let quick_id = quick(&catalog).unwrap();
        assert!(catalog
            .connection()
            .execute(
                "UPDATE collections SET parent_id = ?1 WHERE id = ?2",
                rusqlite::params![parent, quick_id],
            )
            .is_err());
    }

    #[test]
    fn removing_what_is_not_there_is_an_error() {
        // The lookup used `.ok()`, which turned every failure — a locked
        // database included — into "not the quick collection" and carried on.
        let (_dir, catalog, _ids) = library_of(1);
        assert!(remove(&catalog, 999_999).is_err());
    }

    #[test]
    fn a_collection_is_made_with_its_photographs_or_not_at_all() {
        let (_dir, catalog, ids) = library_of(2);
        let made = create_holding(&catalog, "Edit", None, &ids).unwrap();
        assert_eq!(ids_in(&catalog, made), ids);

        // An image that does not exist fails the second half. As two
        // transactions that left an empty collection called "Broken" behind.
        assert!(create_holding(&catalog, "Broken", None, &[ids[0], 999_999]).is_err());
        assert!(
            !all(&catalog).unwrap().iter().any(|c| c.name == "Broken"),
            "half of it happened"
        );
    }

    #[test]
    fn the_count_is_of_what_the_collection_will_show() {
        // A photograph whose file has gone stays a member and is left out of
        // the view. Counting it anyway put "40" on a chip that opened on
        // "1 of 38".
        let (_dir, catalog, ids) = library_of(3);
        let id = create_holding(&catalog, "Edit", None, &ids).unwrap();
        catalog
            .connection()
            .execute(
                "UPDATE files SET missing = 1
                  WHERE id = (SELECT file_id FROM images WHERE id = ?1)",
                [ids[1]],
            )
            .unwrap();
        let listed = all(&catalog).unwrap();
        let counted = listed.iter().find(|c| c.id == id).unwrap().count;
        assert_eq!(counted, ids_in(&catalog, id).len());
        assert_eq!(counted, 2);
    }

    #[test]
    fn deleting_a_photograph_takes_it_out_of_every_collection() {
        // The cascade, which the image-first key turned from a scan of every
        // membership into a search. It has to still *work*.
        let (_dir, catalog, ids) = library_of(3);
        let a = create_holding(&catalog, "A", None, &ids).unwrap();
        let b = create_holding(&catalog, "B", None, &[ids[1]]).unwrap();
        catalog
            .connection()
            .execute("DELETE FROM images WHERE id = ?1", [ids[1]])
            .unwrap();
        assert_eq!(ids_in(&catalog, a), [ids[0], ids[2]]);
        assert!(ids_in(&catalog, b).is_empty());
    }

    #[test]
    fn the_key_has_exactly_one_place_to_put_a_photograph() {
        let (_dir, catalog, _ids) = library_of(1);
        let quick_id = quick(&catalog).unwrap();
        assert_eq!(
            target(&catalog).unwrap(),
            quick_id,
            "it starts on the quick one"
        );

        let portfolio = create(&catalog, "Portfolio", None).unwrap();
        set_target(&catalog, portfolio).unwrap();
        assert_eq!(target(&catalog).unwrap(), portfolio);
        let targets = all(&catalog)
            .unwrap()
            .iter()
            .filter(|c| c.is_target)
            .count();
        assert_eq!(targets, 1, "moved, not copied");

        // Aiming it at nothing fails *and leaves it where it was*. Clearing the
        // old target and then failing to set the new one would leave a key with
        // nowhere to put a photograph.
        assert!(set_target(&catalog, 999_999).is_err());
        assert_eq!(target(&catalog).unwrap(), portfolio);
    }

    #[test]
    fn deleting_the_target_hands_the_key_back_to_the_quick_collection() {
        // The index allows at most one target. "At least one" is `remove`'s to
        // keep — including when the target goes by *cascade*, nested inside the
        // collection that was actually deleted.
        let (_dir, catalog, _ids) = library_of(1);
        let quick_id = quick(&catalog).unwrap();
        let parent = create(&catalog, "2026", None).unwrap();
        let child = create(&catalog, "March", Some(parent)).unwrap();
        set_target(&catalog, child).unwrap();

        remove(&catalog, parent).unwrap();
        assert_eq!(target(&catalog).unwrap(), quick_id);
    }

    #[test]
    fn a_deleted_collection_comes_back_whole() {
        // A hand-made order cannot be recomputed, so deleting one has to be
        // something that can be taken back: the name, the nesting, every member
        // and the place each held, and whether the key was aimed at it.
        let (_dir, catalog, ids) = library_of(4);
        let parent = create(&catalog, "2026", None).unwrap();
        let child = create(&catalog, "March", Some(parent)).unwrap();
        add(&catalog, parent, &[ids[3], ids[0]]).unwrap();
        add(&catalog, child, &[ids[2], ids[1], ids[0]]).unwrap();
        set_target(&catalog, child).unwrap();

        let removed = remove(&catalog, parent).unwrap();
        assert_eq!(removed.name(), "2026");
        assert_eq!(removed.photographs(), 5);
        assert!(!all(&catalog).unwrap().iter().any(|c| c.name == "March"));

        // Something else takes the id the deleted collection had, which SQLite
        // will hand to the very next insert. Restoring under the old id would
        // fail — or merge into a stranger.
        let stranger = create(&catalog, "Unrelated", None).unwrap();

        let back = restore(&catalog, &removed).unwrap();
        assert_ne!(back, stranger);
        let listed = all(&catalog).unwrap();
        let march = listed.iter().find(|c| c.name == "March").unwrap();
        assert_eq!(
            march.parent_id,
            Some(back),
            "nested under the *new* parent id"
        );
        assert!(march.is_target, "and the key is aimed at it again");
        assert_eq!(ids_in(&catalog, back), [ids[3], ids[0]]);
        assert_eq!(ids_in(&catalog, march.id), [ids[2], ids[1], ids[0]]);
        assert!(
            ids_in(&catalog, stranger).is_empty(),
            "nothing leaked into it"
        );
    }

    #[test]
    fn an_undo_does_not_conjure_a_photograph_that_has_gone() {
        let (_dir, catalog, ids) = library_of(3);
        let id = create_holding(&catalog, "Edit", None, &ids).unwrap();
        let removed = remove(&catalog, id).unwrap();
        catalog
            .connection()
            .execute("DELETE FROM images WHERE id = ?1", [ids[1]])
            .unwrap();
        let back = restore(&catalog, &removed).unwrap();
        assert_eq!(ids_in(&catalog, back), [ids[0], ids[2]]);

        // But a *name* taken in the meantime is refused: two collections nobody
        // can tell apart is what the naming rule is for.
        let again = remove(&catalog, back).unwrap();
        create(&catalog, "edit", None).unwrap();
        assert!(restore(&catalog, &again).is_err());
    }

    #[test]
    fn what_is_taken_out_goes_back_where_it_was() {
        // Not at the end. The middle frame of three, taken out and put back,
        // has to be the middle frame again — and so does a whole collection
        // that was emptied.
        let (_dir, catalog, ids) = library_of(3);
        let id = create_holding(&catalog, "Edit", None, &[ids[2], ids[0], ids[1]]).unwrap();
        let taken = take_out(&catalog, id, &[ids[0]]).unwrap();
        assert_eq!(ids_in(&catalog, id), [ids[2], ids[1]]);
        assert_eq!(put_back(&catalog, id, &taken).unwrap(), 1);
        assert_eq!(ids_in(&catalog, id), [ids[2], ids[0], ids[1]]);
        assert_eq!(put_back(&catalog, id, &taken).unwrap(), 0, "already back");

        let were = clear(&catalog, id).unwrap();
        assert!(ids_in(&catalog, id).is_empty());
        put_back(&catalog, id, &were).unwrap();
        assert_eq!(ids_in(&catalog, id), [ids[2], ids[0], ids[1]]);
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
