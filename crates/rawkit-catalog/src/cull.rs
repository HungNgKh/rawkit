//! Deciding which photographs are worth keeping.
//!
//! # Why this is separate from `edits`
//!
//! A rating is not an edit. It changes nothing about how the frame is rendered,
//! it is not versioned, and it does not belong in `EditState` — a field there is
//! a promise that the renderer honours it. Culling metadata lives on the `images`
//! row and is overwritten in place, because "I said three stars and now I say
//! four" is a correction, not a history worth keeping.
//!
//! It is on the *image* rather than the file so that two virtual copies of one
//! frame can be rated apart, which is the whole reason `images` exists as its own
//! table.
//!
//! # What a cull sees
//!
//! Only files that are present. A missing file cannot be looked at, so it cannot
//! be judged, and putting it in the sequence would mean an arrow key landing on a
//! frame that will not open.

use crate::{db::Catalog, CatalogError};

/// The keep/discard decision, which is deliberately not a rating.
///
/// Two separate axes because they answer different questions: a flag is the
/// binary pass through a shoot, a rating is how good the survivors are. Merging
/// them into one scale is what makes a cull slow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    Pick,
    Reject,
}

impl Flag {
    /// The stored spelling. Written from here so the strings cannot drift from
    /// the `CHECK` constraint the way an inline literal eventually would.
    pub fn column(self) -> &'static str {
        match self {
            Flag::Pick => "pick",
            Flag::Reject => "reject",
        }
    }

    fn parse(text: &str) -> Result<Self, CatalogError> {
        match text {
            "pick" => Ok(Flag::Pick),
            "reject" => Ok(Flag::Reject),
            other => Err(CatalogError::Sqlite(format!("unknown flag {other:?}"))),
        }
    }
}

/// Everything decided about one image. `None` throughout means undecided, which
/// is a different thing from rejected and is why every field is an `Option`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Judgement {
    pub rating: Option<u8>,
    pub flag: Option<Flag>,
    pub colour: Option<String>,
}

/// One image in the culling sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryImage {
    pub id: i64,
    /// Where the RAW is, ready to open.
    pub path: String,
    /// What to call it in the interface.
    pub filename: String,
}

/// The highest rating that can be stored, matching the schema's `CHECK`.
pub const MAX_RATING: u8 = 5;

/// Every present image, in the order a photographer went through the day.
///
/// Capture time first, then filename — and files with no capture time sort
/// *after* the dated ones rather than before, because a handful of undatable
/// files should not be the first thing a cull opens onto.
pub fn sequence(catalog: &Catalog) -> Result<Vec<LibraryImage>, CatalogError> {
    let mut statement = catalog.connection().prepare(
        "SELECT i.id,
                v.last_mount_path || '/' || d.relative_path || '/' || f.filename,
                f.filename
           FROM images i
           JOIN files f ON f.id = i.file_id
           JOIN folders d ON d.id = f.folder_id
           JOIN volumes v ON v.id = d.volume_id
          WHERE f.missing = 0
          ORDER BY f.captured_at IS NULL, f.captured_at, f.filename, i.id",
    )?;
    let rows = statement
        .query_map([], |r| {
            Ok(LibraryImage {
                id: r.get(0)?,
                path: r.get::<_, String>(1)?.replace("//", "/"),
                filename: r.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// What has been decided about one image.
pub fn judgement(catalog: &Catalog, image_id: i64) -> Result<Judgement, CatalogError> {
    let row: Option<(Option<i64>, Option<String>, Option<String>)> = catalog
        .connection()
        .query_row(
            "SELECT rating, flag, colour_label FROM images WHERE id = ?1",
            [image_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();
    let Some((rating, flag, colour)) = row else {
        return Ok(Judgement::default());
    };
    Ok(Judgement {
        rating: rating.map(|r| r as u8),
        flag: flag.as_deref().map(Flag::parse).transpose()?,
        colour,
    })
}

/// Record a judgement, replacing whatever was there.
///
/// The whole judgement at once, not a field at a time: undo restores what the
/// image looked like before a keypress, and that is only expressible if a
/// keypress writes a whole state.
pub fn set(catalog: &Catalog, image_id: i64, judgement: &Judgement) -> Result<(), CatalogError> {
    // Refused rather than clamped, for the reason the session refuses an
    // out-of-range temperature: a clamp means the number shown and the number
    // stored have quietly diverged. The schema's CHECK would catch it, but as an
    // opaque constraint failure rather than as something a caller can report.
    if let Some(rating) = judgement.rating {
        if rating > MAX_RATING {
            return Err(CatalogError::Sqlite(format!(
                "a rating of {rating} is beyond the {MAX_RATING} stars the schema allows"
            )));
        }
    }
    let changed = catalog.connection().execute(
        "UPDATE images SET rating = ?2, flag = ?3, colour_label = ?4 WHERE id = ?1",
        rusqlite::params![
            image_id,
            judgement.rating.map(i64::from),
            judgement.flag.map(Flag::column),
            judgement.colour,
        ],
    )?;
    if changed == 0 {
        return Err(CatalogError::Sqlite(format!("no image {image_id}")));
    }
    Ok(())
}

/// How many images carry each flag, for a status line worth reading.
///
/// Returned together because they are read together and a cull's only real
/// progress indicator is "how many have I actually decided about".
pub fn tally(catalog: &Catalog) -> Result<(usize, usize, usize), CatalogError> {
    Ok(catalog.connection().query_row(
        "SELECT count(*),
                sum(flag = 'pick'),
                sum(flag = 'reject')
           FROM images i JOIN files f ON f.id = i.file_id
          WHERE f.missing = 0",
        [],
        |r| {
            Ok((
                r.get::<_, i64>(0)? as usize,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0) as usize,
                r.get::<_, Option<i64>>(2)?.unwrap_or(0) as usize,
            ))
        },
    )?)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::db::tests::{tempdir, Scratch};
    use crate::scan::FileMetadata;
    use std::path::Path;

    /// A library of raws with the capture times given, in the order given.
    fn library(dir: &Scratch, taken: &[(&str, Option<i64>)]) -> Catalog {
        let photos = dir.join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        for (name, _) in taken {
            std::fs::write(photos.join(name), b"raw").unwrap();
        }
        let mut catalog = Catalog::open(&dir.join("library.rawkit")).unwrap();
        let times: Vec<(String, Option<i64>)> =
            taken.iter().map(|(n, t)| (n.to_string(), *t)).collect();
        crate::scan::scan_on(
            &mut catalog,
            &photos,
            crate::VolumeId::Uuid("test-volume".into()),
            move |path: &Path| {
                let name = path.file_name()?.to_string_lossy().into_owned();
                let captured_at = times.iter().find(|(n, _)| *n == name)?.1;
                Some(FileMetadata {
                    captured_at,
                    ..FileMetadata::default()
                })
            },
        )
        .unwrap();
        catalog
    }

    fn names(catalog: &Catalog) -> Vec<String> {
        sequence(catalog)
            .unwrap()
            .into_iter()
            .map(|i| i.filename)
            .collect()
    }

    #[test]
    fn the_sequence_follows_the_shutter_not_the_filename() {
        // The reason capture time was worth reading during a scan: a card
        // rollover, two bodies, or a rename all put the filenames out of the
        // order the pictures were actually taken in.
        let dir = tempdir();
        let catalog = library(
            &dir,
            &[
                ("DSC00003.ARW", Some(300)),
                ("DSC00001.ARW", Some(100)),
                ("DSC00002.ARW", Some(200)),
            ],
        );
        assert_eq!(
            names(&catalog),
            ["DSC00001.ARW", "DSC00002.ARW", "DSC00003.ARW"]
        );
    }

    #[test]
    fn undated_files_come_last_rather_than_first() {
        // They sort before everything under SQLite's NULL ordering, which would
        // open a cull onto whichever files happened to be unreadable.
        let dir = tempdir();
        let catalog = library(
            &dir,
            &[
                ("mystery.ARW", None),
                ("DSC00002.ARW", Some(200)),
                ("DSC00001.ARW", Some(100)),
            ],
        );
        assert_eq!(
            names(&catalog),
            ["DSC00001.ARW", "DSC00002.ARW", "mystery.ARW"]
        );
    }

    #[test]
    fn a_missing_file_is_not_in_the_sequence() {
        // An arrow key must never land on a frame that will not open.
        let dir = tempdir();
        let catalog = library(
            &dir,
            &[("DSC00001.ARW", Some(100)), ("DSC00002.ARW", Some(200))],
        );
        catalog
            .connection()
            .execute(
                "UPDATE files SET missing = 1 WHERE filename = 'DSC00002.ARW'",
                [],
            )
            .unwrap();
        assert_eq!(names(&catalog), ["DSC00001.ARW"]);
    }

    #[test]
    fn a_sequence_entry_names_a_file_that_actually_opens() {
        // The path is rebuilt from three columns across three tables — mount
        // point, relative folder, filename — and a wrong separator or a stray
        // empty component produces a string that looks entirely plausible and
        // opens nothing.
        let dir = tempdir();
        let catalog = library(&dir, &[("a.ARW", Some(1))]);
        let image = &sequence(&catalog).unwrap()[0];
        assert_eq!(
            Path::new(&image.path).canonicalize().unwrap(),
            dir.join("photos/a.ARW").canonicalize().unwrap()
        );
    }

    #[test]
    fn a_judgement_survives_being_read_back() {
        let dir = tempdir();
        let catalog = library(&dir, &[("a.ARW", Some(1))]);
        let id = sequence(&catalog).unwrap()[0].id;
        assert_eq!(judgement(&catalog, id).unwrap(), Judgement::default());

        let decided = Judgement {
            rating: Some(4),
            flag: Some(Flag::Pick),
            colour: Some("green".into()),
        };
        set(&catalog, id, &decided).unwrap();
        assert_eq!(judgement(&catalog, id).unwrap(), decided);
    }

    #[test]
    fn undecided_is_not_the_same_as_rejected() {
        // The distinction the whole workflow rests on: clearing a flag has to
        // put the image back to never-having-been-judged, not to a third state
        // that a filter would then have to know about.
        let dir = tempdir();
        let catalog = library(&dir, &[("a.ARW", Some(1))]);
        let id = sequence(&catalog).unwrap()[0].id;

        set(
            &catalog,
            id,
            &Judgement {
                flag: Some(Flag::Reject),
                ..Judgement::default()
            },
        )
        .unwrap();
        set(&catalog, id, &Judgement::default()).unwrap();
        assert_eq!(judgement(&catalog, id).unwrap().flag, None);
    }

    #[test]
    fn an_impossible_rating_is_refused_rather_than_clamped() {
        // Six stars stored as five is a number the interface never showed.
        let dir = tempdir();
        let catalog = library(&dir, &[("a.ARW", Some(1))]);
        let id = sequence(&catalog).unwrap()[0].id;
        let too_many = Judgement {
            rating: Some(6),
            ..Judgement::default()
        };
        assert!(set(&catalog, id, &too_many).is_err());
        assert_eq!(judgement(&catalog, id).unwrap().rating, None);
    }

    #[test]
    fn a_judgement_is_not_a_version_and_does_not_accumulate() {
        // Unlike an edit. Rating the same frame ten times leaves one row saying
        // the last thing, because a rating has no history worth a table.
        let dir = tempdir();
        let catalog = library(&dir, &[("a.ARW", Some(1))]);
        let id = sequence(&catalog).unwrap()[0].id;
        for rating in 1..=5 {
            set(
                &catalog,
                id,
                &Judgement {
                    rating: Some(rating),
                    ..Judgement::default()
                },
            )
            .unwrap();
        }
        let images: i64 = catalog
            .connection()
            .query_row("SELECT count(*) FROM images", [], |r| r.get(0))
            .unwrap();
        assert_eq!(images, 1);
        assert_eq!(judgement(&catalog, id).unwrap().rating, Some(5));
    }

    #[test]
    fn the_tally_counts_what_has_been_decided() {
        let dir = tempdir();
        let catalog = library(
            &dir,
            &[("a.ARW", Some(1)), ("b.ARW", Some(2)), ("c.ARW", Some(3))],
        );
        let images = sequence(&catalog).unwrap();
        set(
            &catalog,
            images[0].id,
            &Judgement {
                flag: Some(Flag::Pick),
                ..Judgement::default()
            },
        )
        .unwrap();
        set(
            &catalog,
            images[1].id,
            &Judgement {
                flag: Some(Flag::Reject),
                ..Judgement::default()
            },
        )
        .unwrap();
        assert_eq!(tally(&catalog).unwrap(), (3, 1, 1));
    }
}
