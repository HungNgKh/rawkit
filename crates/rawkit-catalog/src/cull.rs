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
use rusqlite::types::Value;
use rusqlite::OptionalExtension;

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

/// Which flag a photograph must carry to survive a [`Filter`].
///
/// Three cases rather than `Option<Flag>`, because "carries no flag" is a thing
/// somebody asks for — it is the pile a cull has not reached yet — and `None`
/// already means "do not ask".
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flagged {
    Pick,
    Reject,
    /// Neither picked nor rejected.
    Unflagged,
}

/// Which photographs to look at.
///
/// # One predicate, two callers
///
/// A window narrows a cull with this and an export chooses files with it, and
/// those have to be the same question. An interface that shows a set it cannot
/// then deliver is worse than one that never filtered, and two implementations
/// of "what counts as a pick" is how that happens — one of them acquiring a rule
/// about missing files, or about zero stars, that the other never hears of.
///
/// So the question is expressed once, as SQL, by [`narrowing`]. [`sequence`]
/// asks it of the library and [`matches`] asks it of a single image by adding a
/// clause to the same query, rather than by re-deciding it in Rust.
///
/// Every field is an `Option`, and they combine with AND. All-`None` is the
/// whole library, which is what [`Filter::default`] is.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
// A field left out is a field not asked about, which is what the whole library
// is. Without this the page would have to send three nulls to say "everything",
// and forgetting one would be a deserialisation error rather than a filter.
#[serde(default)]
pub struct Filter {
    pub flagged: Option<Flagged>,
    /// Stars, at least this many.
    pub min_rating: Option<u8>,
    /// One colour label, by the name it is stored under.
    pub colour: Option<String>,
}

impl Filter {
    /// A filter that lets everything through — the same thing as no filter, and
    /// worth being able to ask because an interface has to say which it is.
    ///
    /// Not derived from `== Filter::default()`: zero stars is not a constraint,
    /// so `min_rating: Some(0)` is also everything and would compare unequal.
    pub fn is_everything(&self) -> bool {
        self.flagged.is_none() && self.min_rating.unwrap_or(0) == 0 && self.colour.is_none()
    }

    /// Only the photographs carrying this flag.
    pub fn flagged(flagged: Flagged) -> Self {
        Self {
            flagged: Some(flagged),
            ..Self::default()
        }
    }

    /// Only the photographs with at least this many stars.
    pub fn rated(stars: u8) -> Self {
        Self {
            min_rating: Some(stars),
            ..Self::default()
        }
    }
}

/// The `WHERE` terms a filter adds, and the values to bind to them.
///
/// Positional `?` throughout, so a caller that binds something of its own must
/// bind it first. [`matches`] is the only one that does.
pub(crate) fn narrowing(filter: &Filter) -> (String, Vec<Value>) {
    let mut sql = String::new();
    let mut values = Vec::new();
    match filter.flagged {
        None => {}
        Some(Flagged::Unflagged) => sql.push_str(" AND i.flag IS NULL"),
        Some(Flagged::Pick) | Some(Flagged::Reject) => {
            let flag = if filter.flagged == Some(Flagged::Pick) {
                Flag::Pick
            } else {
                Flag::Reject
            };
            sql.push_str(" AND i.flag = ?");
            // Through `column` rather than as a literal, for the reason `set`
            // writes it that way: the schema's CHECK and this string are the
            // same fact and must not be typed twice.
            values.push(Value::Text(flag.column().to_string()));
        }
    }
    // Zero stars is everybody. An unrated photograph stores NULL, and `NULL >= 0`
    // is NULL rather than true — so binding it would make "no fewer than zero
    // stars" exclude most of a library, which is the opposite of what it says.
    if let Some(stars) = filter.min_rating.filter(|s| *s > 0) {
        sql.push_str(" AND i.rating >= ?");
        values.push(Value::Integer(stars.into()));
    }
    if let Some(colour) = &filter.colour {
        sql.push_str(" AND i.colour_label = ?");
        values.push(Value::Text(colour.clone()));
    }
    (sql, values)
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
    /// Which interpretation of that file this is, and `None` for the photograph
    /// itself. Two rows here can name the same `path` and the same `filename`;
    /// this is the only thing that tells them apart.
    pub copy_name: Option<String>,
}

impl LibraryImage {
    /// What to show a person: the file, and which version of it.
    pub fn label(&self) -> String {
        match &self.copy_name {
            Some(name) => format!("{} · {name}", self.filename),
            None => self.filename.clone(),
        }
    }

    /// The stem an exported file gets.
    ///
    /// A copy has to reach the filename or two interpretations of one frame
    /// write the same file and the second is skipped as already there — which
    /// looks like an export that quietly lost half its work.
    ///
    /// Whitespace becomes a hyphen and separators are dropped: a copy is named
    /// by a person, in a text field, and a name with a slash in it would write
    /// outside the folder that was chosen.
    pub fn stem(&self) -> String {
        let base = std::path::Path::new(&self.filename)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.filename.clone());
        let Some(name) = &self.copy_name else {
            return base;
        };
        let suffix: String = name
            .chars()
            .map(|c| match c {
                c if c.is_whitespace() => '-',
                '/' | '\\' | ':' => '-',
                c => c,
            })
            .collect();
        format!("{base}-{suffix}")
    }
}

/// The highest rating that can be stored, matching the schema's `CHECK`.
pub const MAX_RATING: u8 = 5;

/// Every present image the filter admits, in the order a photographer went
/// through the day.
///
/// Capture time first, then filename — and files with no capture time sort
/// *after* the dated ones rather than before, because a handful of undatable
/// files should not be the first thing a cull opens onto.
///
/// Order is not a parameter. A cull is a pass through a shoot, and the shoot
/// happened in one order; narrowing *which* photographs is a different question
/// from re-arranging them, and only the first one has turned out to be missed.
///
/// The trailing `i.id` is what puts a virtual copy immediately after the
/// photograph it was made from: the two share a capture time and a filename, so
/// the id is the first key that separates them, and a copy is always the newer
/// row. That was already true before copies existed — it was there to make the
/// order total rather than host-dependent — and it is worth naming now that
/// something depends on it.
pub fn sequence(catalog: &Catalog, filter: &Filter) -> Result<Vec<LibraryImage>, CatalogError> {
    let (narrowed, values) = narrowing(filter);
    let mut statement = catalog.connection().prepare(&format!(
        "SELECT i.id,
                v.last_mount_path || '/' || d.relative_path || '/' || f.filename,
                f.filename,
                i.copy_name
           FROM images i
           JOIN files f ON f.id = i.file_id
           JOIN folders d ON d.id = f.folder_id
           JOIN volumes v ON v.id = d.volume_id
          WHERE f.missing = 0{narrowed}
          ORDER BY f.captured_at IS NULL, f.captured_at, f.filename, i.id"
    ))?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(values), |r| {
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

/// The ids a filter admits, and nothing else about them.
///
/// For a caller that already holds the photographs and only needs to know which
/// of them to show. [`sequence`] answers that by reading every row again — four
/// tables joined and a path assembled per photograph — which is the right cost
/// for opening a library and the wrong one for changing a filter on a library
/// that is already open: measured, 19 ms against about one at twenty thousand.
///
/// **The predicate is still [`narrowing`]'s**, which is the point. Evaluating a
/// filter in Rust over rows in memory would be faster still and would be a
/// second opinion about what a pick is; this keeps SQL the only one.
///
/// Missing files are *not* excluded here, deliberately: nothing in a filter is
/// about the file, so this is one table with no joins, and the caller's own
/// list — which came from [`sequence`] — has already left them out.
pub fn admitted(catalog: &Catalog, filter: &Filter) -> Result<Vec<i64>, CatalogError> {
    let (narrowed, values) = narrowing(filter);
    let mut statement = catalog
        .connection()
        .prepare(&format!("SELECT i.id FROM images i WHERE 1 = 1{narrowed}"))?;
    let ids = statement
        .query_map(rusqlite::params_from_iter(values), |r| r.get(0))?
        .collect::<Result<Vec<i64>, _>>()?;
    Ok(ids)
}

/// Whether one image would appear in a filtered [`sequence`].
///
/// The question a cull asks the moment after a keypress. Rating a frame down to
/// two stars while standing in "four and better" means it has just left the set
/// under your feet, and the interface has to know that before it decides where
/// to put you next.
///
/// Asked as a count of the same query rather than by testing a [`Judgement`] in
/// Rust, so that it cannot answer differently from the sequence it is about — a
/// missing file included.
pub fn matches(catalog: &Catalog, image_id: i64, filter: &Filter) -> Result<bool, CatalogError> {
    let (narrowed, mut values) = narrowing(filter);
    values.insert(0, Value::Integer(image_id));
    let found: i64 = catalog.connection().query_row(
        &format!(
            "SELECT count(*)
               FROM images i
               JOIN files f ON f.id = i.file_id
              WHERE i.id = ? AND f.missing = 0{narrowed}"
        ),
        rusqlite::params_from_iter(values),
        |r| r.get(0),
    )?;
    Ok(found > 0)
}

/// What a photograph is, as opposed to what anybody thinks of it: when it was
/// taken, and with what.
///
/// From the file's row rather than the image's, because a virtual copy is the
/// same exposure. All three are optional — a scan records what the header
/// offered, and a header can offer nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Taken {
    /// Seconds since the epoch, in whatever zone the camera's clock was set to:
    /// the header carries no offset, so this is wall-clock time where the
    /// photograph was taken and is shown as such, never converted.
    pub captured_at: Option<i64>,
    pub camera: Option<String>,
    pub lens: Option<String>,
}

/// Look up [`Taken`] for one image. A primary-key lookup on each of two tables.
pub fn taken(catalog: &Catalog, image_id: i64) -> Result<Taken, CatalogError> {
    let row = catalog
        .connection()
        .query_row(
            "SELECT f.captured_at, f.camera_make, f.camera_model, f.lens
               FROM images i JOIN files f ON f.id = i.file_id
              WHERE i.id = ?1",
            [image_id],
            |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((captured_at, make, model, lens)) = row else {
        return Ok(Taken::default());
    };
    // "SONY" and "ILCE-6400" read as one thing to a person. Most models do not
    // repeat the make, and the ones that do ("Canon Canon EOS R5") should not.
    let camera = match (make, model) {
        (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => {
            Some(model)
        }
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (make, model) => model.or(make),
    };
    Ok(Taken {
        captured_at,
        camera,
        lens,
    })
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

#[cfg(test)]
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
        filtered(catalog, &Filter::default())
    }

    fn filtered(catalog: &Catalog, filter: &Filter) -> Vec<String> {
        sequence(catalog, filter)
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
        let image = &sequence(&catalog, &Filter::default()).unwrap()[0];
        assert_eq!(
            Path::new(&image.path).canonicalize().unwrap(),
            dir.join("photos/a.ARW").canonicalize().unwrap()
        );
    }

    /// A library of four, judged: two picks (one of them four stars and red),
    /// one reject, one untouched.
    fn judged(dir: &Scratch) -> (Catalog, Vec<i64>) {
        let catalog = library(
            dir,
            &[
                ("a.ARW", Some(1)),
                ("b.ARW", Some(2)),
                ("c.ARW", Some(3)),
                ("d.ARW", Some(4)),
            ],
        );
        let ids: Vec<i64> = sequence(&catalog, &Filter::default())
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect();
        set(
            &catalog,
            ids[0],
            &Judgement {
                flag: Some(Flag::Pick),
                rating: Some(4),
                colour: Some("red".into()),
            },
        )
        .unwrap();
        set(
            &catalog,
            ids[1],
            &Judgement {
                flag: Some(Flag::Pick),
                rating: Some(2),
                colour: None,
            },
        )
        .unwrap();
        set(
            &catalog,
            ids[2],
            &Judgement {
                flag: Some(Flag::Reject),
                ..Judgement::default()
            },
        )
        .unwrap();
        (catalog, ids)
    }

    #[test]
    fn each_axis_narrows_on_its_own() {
        let dir = tempdir();
        let (catalog, _) = judged(&dir);
        assert_eq!(names(&catalog), ["a.ARW", "b.ARW", "c.ARW", "d.ARW"]);
        assert_eq!(
            filtered(&catalog, &Filter::flagged(Flagged::Pick)),
            ["a.ARW", "b.ARW"]
        );
        assert_eq!(
            filtered(&catalog, &Filter::flagged(Flagged::Reject)),
            ["c.ARW"]
        );
        // The one nobody has decided about — not the same as "not picked",
        // which would also hand back the reject.
        assert_eq!(
            filtered(&catalog, &Filter::flagged(Flagged::Unflagged)),
            ["d.ARW"]
        );
        assert_eq!(filtered(&catalog, &Filter::rated(3)), ["a.ARW"]);
        assert_eq!(
            filtered(
                &catalog,
                &Filter {
                    colour: Some("red".into()),
                    ..Filter::default()
                }
            ),
            ["a.ARW"]
        );
    }

    #[test]
    fn the_axes_combine_with_and_rather_than_or() {
        // Picks *and* two stars or better, not picks plus everything rated —
        // an OR here would quietly widen every filter anyone set.
        let dir = tempdir();
        let (catalog, _) = judged(&dir);
        assert_eq!(
            filtered(
                &catalog,
                &Filter {
                    flagged: Some(Flagged::Pick),
                    min_rating: Some(3),
                    ..Filter::default()
                }
            ),
            ["a.ARW"]
        );
        // A combination nothing satisfies comes back empty rather than falling
        // back to something wider.
        assert!(filtered(
            &catalog,
            &Filter {
                flagged: Some(Flagged::Reject),
                min_rating: Some(3),
                ..Filter::default()
            }
        )
        .is_empty());
    }

    #[test]
    fn zero_stars_asks_for_everything_including_the_unrated() {
        // `rating` is NULL until somebody presses a digit, and `NULL >= 0` is
        // NULL rather than true. Bound literally, "no fewer than zero stars"
        // would hide most of a library — which is the reverse of what it says,
        // and the sort of thing nobody notices until a filter is already on.
        let dir = tempdir();
        let (catalog, _) = judged(&dir);
        assert_eq!(filtered(&catalog, &Filter::rated(0)), names(&catalog));
        assert!(Filter::rated(0).is_everything());
        assert!(!Filter::rated(1).is_everything());
    }

    #[test]
    fn one_image_is_tested_by_the_same_question_the_sequence_asks() {
        // The pair that must not drift: what the window shows and what it
        // believes about the frame under the cursor.
        let dir = tempdir();
        let (catalog, ids) = judged(&dir);
        for filter in [
            Filter::default(),
            Filter::flagged(Flagged::Pick),
            Filter::flagged(Flagged::Unflagged),
            Filter::rated(3),
            Filter {
                flagged: Some(Flagged::Pick),
                min_rating: Some(2),
                colour: Some("red".into()),
            },
        ] {
            let shown: Vec<i64> = sequence(&catalog, &filter)
                .unwrap()
                .into_iter()
                .map(|i| i.id)
                .collect();
            for id in &ids {
                assert_eq!(
                    matches(&catalog, *id, &filter).unwrap(),
                    shown.contains(id),
                    "image {id} disagrees with the sequence under {filter:?}"
                );
            }
        }
    }

    #[test]
    fn a_missing_file_is_absent_however_it_was_judged() {
        // A filter must not be a way back to a frame that will not open: the
        // presence rule belongs to the sequence, and every narrowing of it is
        // still a narrowing.
        let dir = tempdir();
        let (catalog, ids) = judged(&dir);
        catalog
            .connection()
            .execute("UPDATE files SET missing = 1 WHERE filename = 'a.ARW'", [])
            .unwrap();
        assert_eq!(
            filtered(&catalog, &Filter::flagged(Flagged::Pick)),
            ["b.ARW"]
        );
        assert!(!matches(&catalog, ids[0], &Filter::flagged(Flagged::Pick)).unwrap());
    }

    #[test]
    fn a_judgement_survives_being_read_back() {
        let dir = tempdir();
        let catalog = library(&dir, &[("a.ARW", Some(1))]);
        let id = sequence(&catalog, &Filter::default()).unwrap()[0].id;
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
        let id = sequence(&catalog, &Filter::default()).unwrap()[0].id;

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
        let id = sequence(&catalog, &Filter::default()).unwrap()[0].id;
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
        let id = sequence(&catalog, &Filter::default()).unwrap()[0].id;
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
        let images = sequence(&catalog, &Filter::default()).unwrap();
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
