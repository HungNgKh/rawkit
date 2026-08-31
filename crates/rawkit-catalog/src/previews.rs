//! Rendered copies at a few sizes, so looking at a library is not decoding it.
//!
//! # Why a preview is not a cache
//!
//! It behaves like one — regenerable, discardable, keyed by a hash — but the
//! catalog has to *know* what it holds, because the interesting question is not
//! "is this file here" but "is this file still what this photograph looks like".
//! An edit invalidates every preview of that image, and answering that by opening
//! files would mean opening every file. So the row carries the
//! `edit_state_hash` and the answer is a query.
//!
//! # Files on disk, not blobs in the catalog
//!
//! A library's previews are gigabytes. Putting them in SQLite would make every
//! rolling backup copy them, which turns the safety net into the thing that
//! fills the disk. They live in a directory beside the catalog — the same place
//! and the same reasoning as the backups: they travel with the library.
//!
//! # What is not built in bulk
//!
//! [`Level::OneToOne`]. A full-resolution preview of a 24 MP frame is several
//! megabytes, so a whole library of them is hundreds of gigabytes; it is a
//! per-tile thing generated for the region being looked at, which is a different
//! mechanism and a later slice. The schema allows the value so that adding it
//! does not mean another table rebuild.

use crate::{db::Catalog, CatalogError};
use rawkit_editstate::EditState;
use std::path::{Path, PathBuf};

/// A preview size. The pixel figure is the *longest edge*, so it means the same
/// thing for a portrait frame as for a landscape one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Grid.
    Thumb,
    /// Loupe fit, filmstrip.
    Small,
    /// Compare, survey, most editing.
    Standard,
    /// Sharpness checks at 1:1. Reserved — nothing writes it yet.
    OneToOne,
}

impl Level {
    /// The levels a bulk build produces. Deliberately not all of them.
    pub const BULK: &'static [Level] = &[Level::Thumb, Level::Small, Level::Standard];

    /// Longest edge in pixels, or `None` for the native-resolution level.
    pub fn longest_edge(self) -> Option<u32> {
        match self {
            Level::Thumb => Some(256),
            Level::Small => Some(1024),
            Level::Standard => Some(2560),
            Level::OneToOne => None,
        }
    }

    /// The stored spelling, written from here so it cannot drift from the
    /// schema's `CHECK`.
    pub fn column(self) -> &'static str {
        match self {
            Level::Thumb => "thumb",
            Level::Small => "small",
            Level::Standard => "standard",
            Level::OneToOne => "one_to_one",
        }
    }

    fn parse(text: &str) -> Result<Self, CatalogError> {
        match text {
            "thumb" => Ok(Level::Thumb),
            "small" => Ok(Level::Small),
            "standard" => Ok(Level::Standard),
            "one_to_one" => Ok(Level::OneToOne),
            other => Err(CatalogError::Sqlite(format!(
                "unknown preview level {other:?}"
            ))),
        }
    }
}

/// One preview, as the catalog knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub level: Level,
    /// Relative to [`directory`], so a library that moves keeps its previews.
    pub path: String,
    pub edit_state_hash: String,
    /// Which build rendered it, from `rawkit_engine::renderer_version`. Opaque
    /// here: this table stores it and compares it for equality, and has no
    /// opinion about how a renderer identifies itself.
    pub renderer: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
}

/// Where a catalog's previews live: a sibling directory, like the backups.
pub fn directory(catalog: &Catalog) -> Option<PathBuf> {
    let path = catalog.path()?;
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    Some(path.with_file_name(format!("{stem}-previews")))
}

/// Where one preview goes inside that directory.
///
/// Sharded into 256 subdirectories by image id, because a library of twenty
/// thousand photographs is sixty thousand files and a single directory holding
/// them makes every listing slow on every filesystem that has ever shipped.
///
/// The hash is in the filename as well as in the row so that regenerating after
/// an edit writes a *new* file rather than overwriting one something may still
/// be reading.
pub fn relative_path(image_id: i64, level: Level, edit_state_hash: &str) -> String {
    let shard = (image_id.unsigned_abs() % 256) as u8;
    let short: String = edit_state_hash.chars().take(12).collect();
    format!("{shard:02x}/{image_id}-{}-{short}.jpg", level.column())
}

/// Record a preview, replacing any previous one at that level.
pub fn record(catalog: &Catalog, image_id: i64, preview: &Preview) -> Result<(), CatalogError> {
    catalog.connection().execute(
        "INSERT INTO previews
              (image_id, level, path, edit_state_hash, renderer, width, height, bytes, created_at)
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (image_id, level) DO UPDATE SET
              path = excluded.path,
              edit_state_hash = excluded.edit_state_hash,
              renderer = excluded.renderer,
              width = excluded.width,
              height = excluded.height,
              bytes = excluded.bytes,
              created_at = excluded.created_at",
        rusqlite::params![
            image_id,
            preview.level.column(),
            preview.path,
            preview.edit_state_hash,
            preview.renderer,
            preview.width,
            preview.height,
            preview.bytes as i64,
            seconds_now(),
        ],
    )?;
    Ok(())
}

/// The preview at this level, whatever edit it shows.
pub fn lookup(
    catalog: &Catalog,
    image_id: i64,
    level: Level,
) -> Result<Option<Preview>, CatalogError> {
    let row = catalog
        .connection()
        .query_row(
            "SELECT level, path, edit_state_hash, renderer, width, height, bytes
               FROM previews WHERE image_id = ?1 AND level = ?2",
            rusqlite::params![image_id, level.column()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            },
        )
        .ok();
    let Some((level, path, hash, renderer, width, height, bytes)) = row else {
        return Ok(None);
    };
    Ok(Some(Preview {
        level: Level::parse(&level)?,
        path,
        edit_state_hash: hash,
        renderer,
        width: width as u32,
        height: height as u32,
        bytes: bytes as u64,
    }))
}

/// The cheapest preview that still has enough pixels for what is being asked.
///
/// `needed` is the longest edge, in image pixels, that the view can actually
/// show. Anything larger is bytes read for nothing; anything smaller would be
/// upscaled, which is what makes a preview look like a preview.
///
/// The hash has to match. A preview of an edit that has since been changed is
/// not a preview of this photograph — showing it would put the previous version
/// of someone's decisions on screen, which is worse than a pause.
///
/// So does the renderer, for the same reason one step further back: a preview
/// this build would no longer produce is not a preview of this photograph
/// either, and it is the harder case to notice, because nothing about a
/// wrong-but-plausible thumbnail says it is wrong. A mismatch is simply not
/// found, and the caller decodes — which is what it already does when no preview
/// is large enough. Nothing is deleted here; the row is replaced when that image
/// and level are rebuilt, and [`sweep`] collects the file.
pub fn covering(
    catalog: &Catalog,
    image_id: i64,
    needed: u32,
    edit_state_hash: &str,
    renderer: &str,
) -> Result<Option<Preview>, CatalogError> {
    let row = catalog
        .connection()
        .query_row(
            "SELECT level, path, edit_state_hash, renderer, width, height, bytes
               FROM previews
              WHERE image_id = ?1 AND edit_state_hash = ?2 AND renderer = ?3
                AND max(width, height) >= ?4
              ORDER BY max(width, height) ASC
              LIMIT 1",
            rusqlite::params![image_id, edit_state_hash, renderer, needed],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            },
        )
        .ok();
    let Some((level, path, hash, renderer, width, height, bytes)) = row else {
        return Ok(None);
    };
    Ok(Some(Preview {
        level: Level::parse(&level)?,
        path,
        edit_state_hash: hash,
        renderer,
        width: width as u32,
        height: height as u32,
        bytes: bytes as u64,
    }))
}

/// An image that needs previews built, and what it needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Wanted {
    pub image_id: i64,
    /// The RAW, ready to open.
    pub path: String,
    pub filename: String,
    /// The edit these previews should show, and the hash that keys them.
    pub state: EditState,
    pub edit_state_hash: String,
    pub missing: Vec<Level>,
}

/// Every present image whose previews are missing or describe a different edit.
///
/// Stale and absent are the same answer on purpose: a preview of an edit nobody
/// is looking at any more is not a preview of this photograph, and treating it
/// as one is how a grid shows a version of a picture that no longer exists.
///
/// `renderer` has to match for the same reason, and this is the half that is
/// easy to leave out. [`covering`] declining a preview only stops it being
/// *shown*; if the builder does not also consider it stale then it is never
/// replaced, and the library ends up in the worst of both states — previews on
/// disk that nothing will ever use, and a decode on every photograph, for good.
pub fn outstanding(
    catalog: &Catalog,
    levels: &[Level],
    renderer: &str,
) -> Result<Vec<Wanted>, CatalogError> {
    let mut wanted = Vec::new();
    for image in crate::cull::sequence(catalog)? {
        // The edit the photograph currently has — the stored one, or as shot.
        let state = crate::edits::latest(catalog, image.id)?
            .map(|(_, state)| state)
            .unwrap_or_default();
        let hash = state.content_hash();

        let mut missing = Vec::new();
        for &level in levels {
            match lookup(catalog, image.id, level)? {
                Some(existing)
                    if existing.edit_state_hash == hash && existing.renderer == renderer => {}
                _ => missing.push(level),
            }
        }
        if !missing.is_empty() {
            wanted.push(Wanted {
                image_id: image.id,
                path: image.path,
                filename: image.filename,
                state,
                edit_state_hash: hash,
                missing,
            });
        }
    }
    Ok(wanted)
}

/// How many previews there are and what they weigh, for a caller to report.
pub fn tally(catalog: &Catalog) -> Result<(usize, u64), CatalogError> {
    Ok(catalog.connection().query_row(
        "SELECT count(*), ifnull(sum(bytes), 0) FROM previews",
        [],
        |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as u64)),
    )?)
}

/// Delete preview files the catalog no longer refers to.
///
/// Regenerating after an edit writes a new filename and leaves the old file
/// behind, so without this a library that is edited often grows a directory of
/// orphans that nothing will ever open. Only files under `dir` that look like
/// ours are considered — the same caution the backup rotation takes, and for the
/// same reason: this is the only code here that deletes.
pub fn sweep(catalog: &Catalog, dir: &Path) -> Result<(usize, u64), CatalogError> {
    let mut known = std::collections::HashSet::new();
    {
        let mut statement = catalog.connection().prepare("SELECT path FROM previews")?;
        for path in statement.query_map([], |r| r.get::<_, String>(0))? {
            known.insert(path?);
        }
    }

    let (mut removed, mut freed) = (0usize, 0u64);
    let Ok(shards) = std::fs::read_dir(dir) else {
        return Ok((0, 0));
    };
    for shard in shards.flatten() {
        let Ok(files) = std::fs::read_dir(shard.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            // Ours by shape: a `.jpg` inside a shard directory. Anything else in
            // there was put there by someone else and is not ours to remove.
            if !name.ends_with(".jpg") {
                continue;
            }
            let relative = format!("{}/{name}", shard.file_name().to_string_lossy());
            if known.contains(&relative) {
                continue;
            }
            let size = file.metadata().map(|m| m.len()).unwrap_or(0);
            if std::fs::remove_file(file.path()).is_ok() {
                removed += 1;
                freed += size;
            }
        }
    }
    Ok((removed, freed))
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
    use crate::db::tests::{tempdir, Scratch};
    use crate::scan::FileMetadata;

    fn library(dir: &Scratch, names: &[&str]) -> Catalog {
        let photos = dir.join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        for name in names {
            std::fs::write(photos.join(name), b"raw").unwrap();
        }
        let mut catalog = Catalog::open(&dir.join("library.rawkit")).unwrap();
        crate::scan::scan_on(
            &mut catalog,
            &photos,
            crate::VolumeId::Uuid("test-volume".into()),
            |path: &Path| {
                let stem = path.file_stem()?.to_string_lossy().into_owned();
                Some(FileMetadata {
                    // From the name, never from directory order.
                    captured_at: Some(stem.bytes().map(i64::from).sum()),
                    ..FileMetadata::default()
                })
            },
        )
        .unwrap();
        catalog
    }

    /// The build these fixtures pretend rendered with. Any string works; what
    /// matters is that a *different* one is not offered.
    const BUILD: &str = "engine-1/libraw-1";

    fn sample(hash: &str) -> Preview {
        Preview {
            level: Level::Thumb,
            path: "00/1-thumb-abc.jpg".into(),
            edit_state_hash: hash.into(),
            renderer: BUILD.into(),
            width: 256,
            height: 171,
            bytes: 9_000,
        }
    }

    #[test]
    fn a_preview_replaces_the_one_it_supersedes() {
        // One per image per level: a second row for the same level is not a
        // variant, it is the old one that should have gone.
        let dir = tempdir();
        let catalog = library(&dir, &["a.ARW"]);
        let id = crate::cull::sequence(&catalog).unwrap()[0].id;

        record(&catalog, id, &sample("first")).unwrap();
        record(&catalog, id, &sample("second")).unwrap();
        assert_eq!(
            lookup(&catalog, id, Level::Thumb)
                .unwrap()
                .unwrap()
                .edit_state_hash,
            "second"
        );
        assert_eq!(tally(&catalog).unwrap(), (1, 9_000));
    }

    #[test]
    fn an_edit_makes_every_preview_of_that_image_outstanding() {
        // The reason the hash is a column. Without it, a grid keeps showing a
        // version of a photograph that no longer exists, and nothing anywhere
        // knows it is wrong.
        let dir = tempdir();
        let catalog = library(&dir, &["a.ARW"]);
        let id = crate::cull::sequence(&catalog).unwrap()[0].id;

        let as_shot = EditState::default();
        for &level in Level::BULK {
            record(
                &catalog,
                id,
                &Preview {
                    level,
                    edit_state_hash: as_shot.content_hash(),
                    ..sample("")
                },
            )
            .unwrap();
        }
        assert!(
            outstanding(&catalog, Level::BULK, BUILD)
                .unwrap()
                .is_empty(),
            "everything is current"
        );

        let edited = EditState {
            tone: rawkit_editstate::Tone {
                exposure_ev: 1.0,
                ..Default::default()
            },
            ..Default::default()
        };
        crate::edits::save(&catalog, id, &edited, rawkit_editstate::EditSource::User).unwrap();

        let work = outstanding(&catalog, Level::BULK, BUILD).unwrap();
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].missing, Level::BULK, "all three, not just one");
        assert_eq!(work[0].edit_state_hash, edited.content_hash());
    }

    #[test]
    fn an_unpreviewed_library_is_entirely_outstanding() {
        let dir = tempdir();
        let catalog = library(&dir, &["a.ARW", "b.ARW", "c.ARW"]);
        let work = outstanding(&catalog, Level::BULK, BUILD).unwrap();
        assert_eq!(work.len(), 3);
        assert!(work.iter().all(|w| w.missing.len() == 3));
        // As shot, because none of them has been edited.
        assert!(work.iter().all(|w| w.state == EditState::default()));
    }

    #[test]
    fn the_cheapest_preview_that_is_big_enough_is_the_one_chosen() {
        let dir = tempdir();
        let catalog = library(&dir, &["a.ARW"]);
        let id = crate::cull::sequence(&catalog).unwrap()[0].id;
        let hash = EditState::default().content_hash();
        for (level, width, height) in [
            (Level::Thumb, 256, 171),
            (Level::Small, 1024, 684),
            (Level::Standard, 2560, 1710),
        ] {
            record(
                &catalog,
                id,
                &Preview {
                    level,
                    width,
                    height,
                    edit_state_hash: hash.clone(),
                    ..sample("")
                },
            )
            .unwrap();
        }

        let pick = |needed| {
            covering(&catalog, id, needed, &hash, BUILD)
                .unwrap()
                .map(|p| p.level)
        };
        assert_eq!(pick(100), Some(Level::Thumb));
        assert_eq!(pick(256), Some(Level::Thumb), "exactly enough is enough");
        assert_eq!(pick(257), Some(Level::Small));
        assert_eq!(pick(2000), Some(Level::Standard));
        assert_eq!(
            pick(4000),
            None,
            "zoomed past every preview, so the RAW has to be decoded"
        );
    }

    #[test]
    fn a_preview_of_a_different_edit_is_not_offered() {
        // Showing it would put the previous version of someone's decisions on
        // screen, which is worse than waiting for a decode.
        let dir = tempdir();
        let catalog = library(&dir, &["a.ARW"]);
        let id = crate::cull::sequence(&catalog).unwrap()[0].id;
        record(
            &catalog,
            id,
            &Preview {
                level: Level::Small,
                width: 1024,
                height: 684,
                edit_state_hash: "an older edit".into(),
                ..sample("")
            },
        )
        .unwrap();
        let now = EditState::default().content_hash();
        assert!(covering(&catalog, id, 100, &now, BUILD).unwrap().is_none());
        assert!(covering(&catalog, id, 100, "an older edit", BUILD)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_preview_from_another_build_is_not_offered() {
        // The harder half of the same idea. A preview of the wrong *edit* is at
        // least a picture somebody once asked for; a preview from a build whose
        // renderer has since changed is a picture nothing would produce now, and
        // there is nothing about it on screen to say so.
        let dir = tempdir();
        let catalog = library(&dir, &["a.ARW"]);
        let id = crate::cull::sequence(&catalog).unwrap()[0].id;
        let hash = EditState::default().content_hash();
        record(
            &catalog,
            id,
            &Preview {
                level: Level::Small,
                width: 1024,
                height: 684,
                edit_state_hash: hash.clone(),
                renderer: "an older engine".into(),
                ..sample("")
            },
        )
        .unwrap();

        assert!(
            covering(&catalog, id, 100, &hash, BUILD).unwrap().is_none(),
            "the edit matches and the renderer does not, which is still stale"
        );
        assert!(
            covering(&catalog, id, 100, &hash, "an older engine")
                .unwrap()
                .is_some(),
            "and the build that made it would still be offered it"
        );
    }

    #[test]
    fn a_preview_from_before_the_column_existed_is_never_offered() {
        // Migration 5 defaults existing rows to the empty string, which is not a
        // version any build produces — so a library upgraded into this scheme
        // rebuilds rather than trusting previews whose provenance is unknown.
        let dir = tempdir();
        let catalog = library(&dir, &["a.ARW"]);
        let id = crate::cull::sequence(&catalog).unwrap()[0].id;
        let hash = EditState::default().content_hash();
        record(
            &catalog,
            id,
            &Preview {
                level: Level::Small,
                width: 1024,
                height: 684,
                edit_state_hash: hash.clone(),
                renderer: String::new(),
                ..sample("")
            },
        )
        .unwrap();
        assert!(covering(&catalog, id, 100, &hash, BUILD).unwrap().is_none());
    }

    #[test]
    fn a_missing_file_is_not_offered_for_previewing() {
        // It cannot be opened, so rendering it is work that can only fail.
        let dir = tempdir();
        let catalog = library(&dir, &["a.ARW", "b.ARW"]);
        catalog
            .connection()
            .execute("UPDATE files SET missing = 1 WHERE filename = 'b.ARW'", [])
            .unwrap();
        let work = outstanding(&catalog, Level::BULK, BUILD).unwrap();
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].filename, "a.ARW");
    }

    #[test]
    fn previews_shard_by_image_so_no_directory_holds_them_all() {
        assert_eq!(
            relative_path(1, Level::Thumb, "abcdef0123456789"),
            "01/1-thumb-abcdef012345.jpg"
        );
        assert_eq!(
            relative_path(258, Level::Standard, "abcdef0123456789"),
            "02/258-standard-abcdef012345.jpg"
        );
        // A different edit is a different file, so regenerating never overwrites
        // one something may still be reading.
        assert_ne!(
            relative_path(1, Level::Thumb, "aaaa"),
            relative_path(1, Level::Thumb, "bbbb")
        );
    }

    #[test]
    fn the_sweep_removes_orphans_and_nothing_else() {
        // The only code here that deletes, so it matches by shape and by what
        // the catalog actually refers to.
        let dir = tempdir();
        let catalog = library(&dir, &["a.ARW"]);
        let id = crate::cull::sequence(&catalog).unwrap()[0].id;
        let previews = dir.join("previews");
        std::fs::create_dir_all(previews.join("00")).unwrap();

        let kept = previews.join("00/kept.jpg");
        let orphan = previews.join("00/orphan.jpg");
        let stranger = previews.join("00/README.txt");
        std::fs::write(&kept, vec![0u8; 100]).unwrap();
        std::fs::write(&orphan, vec![0u8; 40]).unwrap();
        std::fs::write(&stranger, b"not ours").unwrap();
        record(
            &catalog,
            id,
            &Preview {
                path: "00/kept.jpg".into(),
                ..sample("h")
            },
        )
        .unwrap();

        assert_eq!(sweep(&catalog, &previews).unwrap(), (1, 40));
        assert!(kept.exists(), "a preview the catalog refers to");
        assert!(!orphan.exists(), "one it does not");
        assert!(stranger.exists(), "and a file that was never ours");
    }
}
