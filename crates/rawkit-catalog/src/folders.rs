//! The folders photographs are in, as a tree with counts, for somebody to
//! choose one from.
//!
//! # Why a tree with totals, and not only what is in each folder
//!
//! A shoot is often a folder of folders — a card per day, a subfolder per
//! camera — and the folder holding them has nothing of its own. Showing it as
//! empty would say the shoot is not there. So each folder carries both: what is
//! in it, and what is in it and everything under it, which is what choosing it
//! shows.
//!
//! # What is counted
//!
//! Photographs, not files: a virtual copy is a second photograph of one file and
//! is shown as one. Files that have gone missing are not counted, because they
//! are not shown — a count that promised forty and showed thirty-one would be
//! the kind of number nobody trusts twice. A folder with nothing left to show is
//! left out altogether: choosing it could only be refused.

use crate::db::Catalog;
use crate::CatalogError;
use std::collections::HashMap;

/// One folder, flat. The shape is a tree and the caller is the one that knows
/// how it wants to draw one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Folder {
    pub id: i64,
    /// `None` for a volume's root, or for a folder whose parent is not there.
    pub parent_id: Option<i64>,
    /// Its own name: the last part of its path, or for a volume's root, the
    /// last part of where the volume was last seen.
    pub name: String,
    /// Where it is, in full, as the filesystem spells it.
    pub path: String,
    /// Photographs directly in it.
    pub own: u32,
    /// Photographs in it and every folder under it.
    pub total: u32,
}

/// Every folder with something in it to show, ordered by path.
pub fn tree(catalog: &Catalog) -> Result<Vec<Folder>, CatalogError> {
    let mut statement = catalog.connection().prepare(
        "SELECT d.id, d.parent_id, d.relative_path, v.last_mount_path, v.label,
                (SELECT count(*) FROM files f JOIN images i ON i.file_id = f.id
                  WHERE f.folder_id = d.id AND f.missing = 0)
           FROM folders d JOIN volumes v ON v.id = d.volume_id
          ORDER BY v.id, d.path_key",
    )?;
    let mut folders: Vec<Folder> = statement
        .query_map([], |r| {
            let relative: String = r.get(2)?;
            let mount: Option<String> = r.get(3)?;
            let label: Option<String> = r.get(4)?;
            let mount = mount.unwrap_or_default();
            let path = if relative.is_empty() {
                mount.clone()
            } else {
                format!("{}/{relative}", mount.trim_end_matches('/'))
            };
            let name = last_part(&relative)
                .or_else(|| last_part(&mount))
                .or(label)
                .unwrap_or_else(|| "/".to_string());
            let own: u32 = r.get(5)?;
            Ok(Folder {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                name,
                path,
                own,
                total: own,
            })
        })?
        .collect::<Result<_, _>>()?;

    // Each folder's own count added to every folder above it. Walked up from
    // each folder rather than summed down a built tree: there are a few hundred
    // folders in a big library, and a chain is a few links long. A parent that
    // is not in the table ends the walk — and so does a loop, which the schema
    // does not forbid and which must not hang the window.
    let at: HashMap<i64, usize> = folders.iter().enumerate().map(|(i, f)| (f.id, i)).collect();
    for index in 0..folders.len() {
        let own = folders[index].own;
        if own == 0 {
            continue;
        }
        let mut parent = folders[index].parent_id;
        let mut steps = 0;
        while let Some(&above) = parent.as_ref().and_then(|id| at.get(id)) {
            steps += 1;
            if steps > folders.len() {
                break;
            }
            folders[above].total += own;
            parent = folders[above].parent_id;
        }
    }
    // A parent that is not there makes its child a root, so the caller never
    // has to decide what to do with a row that hangs from nothing.
    let known: std::collections::HashSet<i64> = folders.iter().map(|f| f.id).collect();
    for folder in &mut folders {
        if folder.parent_id.is_some_and(|id| !known.contains(&id)) {
            folder.parent_id = None;
        }
    }
    let kept: std::collections::HashSet<i64> = folders
        .iter()
        .filter(|f| f.total > 0)
        .map(|f| f.id)
        .collect();
    folders.retain(|f| kept.contains(&f.id));
    Ok(folders)
}

fn last_part(path: &str) -> Option<String> {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|part| !part.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cull::{sequence_in, Filter};
    use crate::db::tests::{tempdir, Scratch};

    /// A shoot: two days in their own folders, one photograph loose beside them.
    fn shoot(dir: &Scratch) -> Catalog {
        let photos = dir.join("shoot");
        for (folder, name) in [
            ("day1", "a.arw"),
            ("day1", "b.arw"),
            ("day2", "c.arw"),
            ("", "d.arw"),
        ] {
            let at = photos.join(folder);
            std::fs::create_dir_all(&at).unwrap();
            std::fs::write(at.join(name), b"raw").unwrap();
        }
        let mut catalog = Catalog::open(&dir.join("library.rawkit")).unwrap();
        crate::scan::scan_on(
            &mut catalog,
            &photos,
            crate::VolumeId::Uuid("test-volume".into()),
            |_: &std::path::Path| None,
        )
        .unwrap();
        catalog
    }

    fn named<'a>(folders: &'a [Folder], name: &str) -> &'a Folder {
        folders.iter().find(|f| f.name == name).unwrap()
    }

    #[test]
    fn a_folder_counts_what_is_in_it_and_under_it() {
        let dir = tempdir();
        let catalog = shoot(&dir);
        let folders = tree(&catalog).unwrap();
        let root = named(&folders, "shoot");
        assert_eq!((root.own, root.total, root.parent_id), (1, 4, None));
        let day1 = named(&folders, "day1");
        assert_eq!(
            (day1.own, day1.total, day1.parent_id),
            (2, 2, Some(root.id))
        );
        assert!(day1.path.ends_with("shoot/day1"), "{}", day1.path);
    }

    #[test]
    fn choosing_a_folder_shows_it_and_everything_under_it() {
        let dir = tempdir();
        let catalog = shoot(&dir);
        let folders = tree(&catalog).unwrap();
        let names = |id| {
            let mut n: Vec<String> = sequence_in(&catalog, id, &Filter::default())
                .unwrap()
                .into_iter()
                .map(|i| i.filename)
                .collect();
            n.sort();
            n
        };
        assert_eq!(names(named(&folders, "day1").id), ["a.arw", "b.arw"]);
        assert_eq!(
            names(named(&folders, "shoot").id),
            ["a.arw", "b.arw", "c.arw", "d.arw"]
        );
    }

    #[test]
    fn a_missing_file_is_not_counted_and_an_emptied_folder_is_not_offered() {
        let dir = tempdir();
        let catalog = shoot(&dir);
        catalog
            .connection()
            .execute("UPDATE files SET missing = 1 WHERE filename = 'c.arw'", [])
            .unwrap();
        let folders = tree(&catalog).unwrap();
        assert!(folders.iter().all(|f| f.name != "day2"));
        assert_eq!(named(&folders, "shoot").total, 3);
    }

    #[test]
    fn a_virtual_copy_is_a_second_photograph() {
        let dir = tempdir();
        let catalog = shoot(&dir);
        let image: i64 = catalog
            .connection()
            .query_row(
                "SELECT i.id FROM images i JOIN files f ON f.id = i.file_id WHERE f.filename = 'a.arw'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        crate::copies::create(
            &catalog,
            image,
            None,
            &rawkit_editstate::EditState::default(),
        )
        .unwrap();
        let folders = tree(&catalog).unwrap();
        assert_eq!(named(&folders, "day1").own, 3);
        assert_eq!(named(&folders, "shoot").total, 5);
    }
}
