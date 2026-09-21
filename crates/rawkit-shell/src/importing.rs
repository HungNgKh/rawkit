//! Adding a folder of photographs, from the window.
//!
//! Two ways, chosen after the count. **In place**: the photographs stay where
//! they are and the catalog learns where that is; nothing is copied, moved or
//! renamed. **Copied off a card**: each file is copied into a folder for the day
//! it was taken, checked against the original before it is kept, and the copies
//! are added — [`rawkit_catalog::ingest`], the command line's import, with the
//! window's progress and Stop. The card is never written to. A folder with a
//! `DCIM` in it is taken for a card and offered the copy first, because
//! photographs left on a card go when it is formatted.
//!
//! # Count first, then ask
//!
//! The folder is walked and compared with the catalog before anything is kept,
//! and the person is told what would happen — "1 268 to add, 212 already here"
//! — on a button that says the number. The count is a dry run of the scan
//! itself ([`rawkit_catalog::scan::scan_watched`]), so the two apply one rule.
//! They are still two looks at a disk that may be changing — a card still
//! being copied to — so the sentence at the end gives the number that was
//! added, not the number that was promised.
//!
//! # Its own connection, and why that is safe here
//!
//! A scan is one transaction and, at twenty thousand photographs, a dozen
//! seconds. Under the library's mutex that is every keypress blocked for a
//! dozen seconds, so it runs on a thread with a connection of its own. SQLite
//! here has no `busy_timeout`: while that transaction holds the write lock, a
//! write from the window would fail at once. So there are none. The render
//! loop flushes the pending edit and then stands still for the length of the
//! import — no saver, no preview pump — and the page covers the window with
//! the import's sheet, which takes the keyboard.
//!
//! One connection for both halves, held open across the question in between:
//! closing a catalog writes a rolling backup, and an import should cost the
//! rotation one backup, not two.
//!
//! # And then a relaunch
//!
//! The library in this process holds its sequence, its tally and its
//! collections as they were before the folder arrived. Opening anything is a
//! relaunch (`docs/decisions/interface.md`, I16), and what an import produces
//! is a catalog worth opening.

use rawkit_catalog::scan::{self, FileMetadata, Progress};
use rawkit_catalog::{db::Catalog, CatalogError, VolumeId};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Mutex;

/// Where an import has got to, for the page to draw.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum Stage {
    /// Listing the folder. `found` photographs so far.
    Counting { found: usize },
    /// Listed and compared, and waiting to be told to go on.
    Counted {
        /// Not in the catalog yet.
        fresh: usize,
        /// In it already. A changed file counts here: it is re-read, not added.
        already: usize,
        /// Folders that could not be listed. Said, because a number that is
        /// short with no reason given looks like a scan that missed things.
        unreadable: usize,
        /// It looks like a camera's card, so copying is what is offered first.
        /// Photographs left on a card are photographs that go when it is
        /// formatted.
        card: bool,
    },
    /// Copying off a card, before the copies are added.
    Copying {
        done: usize,
        total: usize,
        name: String,
    },
    Adding {
        done: usize,
        total: usize,
        name: String,
    },
    /// In the catalog, and the window is about to reopen on it.
    Opening { added: usize },
    /// It stopped, and why. Stays until dismissed.
    Failed { why: String },
}

/// What the worker and whoever watches it share.
#[derive(Default)]
pub struct Shared {
    stage: Mutex<Option<Stage>>,
    stop: AtomicBool,
}

impl Shared {
    pub fn stage(&self) -> Option<Stage> {
        self.stage.lock().expect("import stage").clone()
    }

    fn set(&self, stage: Option<Stage>) {
        *self.stage.lock().expect("import stage") = stage;
    }

    /// Ask it to stop. Takes effect at the next folder or the next file, and
    /// nothing is kept: a scan is one transaction.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Put away a failure that has been read.
    #[cfg(test)]
    pub fn dismiss(&self) {
        self.set(None);
    }

    /// Added, and about to be shown.
    pub fn opening(&self, added: usize) {
        self.set(Some(Stage::Opening { added }));
    }

    /// Something went wrong after the scan itself.
    pub fn fail(&self, why: String) {
        self.set(Some(Stage::Failed { why }));
    }
}

/// What somebody said to the count.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Add them where they are.
    InPlace,
    /// Copy them into dated folders under here, verify each, and add the copies.
    CopyTo(PathBuf),
    No,
}

/// How an import ended.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// This many photographs are in the catalog that were not before.
    Added(usize),
    /// Copied off a card and added. `already` were at the destination byte for
    /// byte and were left as they were; `failed` could not be copied or did not
    /// survive verification, and the first of them is named with why.
    Copied {
        added: usize,
        already: usize,
        failed: usize,
        first_failure: Option<String>,
        stopped: bool,
    },
    /// Everything in the folder was there already, or there was nothing in it.
    Nothing,
    Cancelled,
    /// And [`Stage::Failed`] says why.
    Failed,
}

/// Count, ask, add. Blocks on `decide` between the count and the scan.
///
/// The volume and the metadata reader arrive as parameters for the reason they
/// do in the catalog: so this can be tested without a filesystem UUID or a
/// decoder.
pub fn run(
    shared: &Shared,
    catalog: &Path,
    folder: &Path,
    volume: impl FnOnce(&Path) -> Result<VolumeId, CatalogError>,
    metadata: impl FnMut(&Path) -> Option<FileMetadata>,
    decide: &Receiver<Decision>,
) -> Outcome {
    shared.set(Some(Stage::Counting { found: 0 }));
    let finished = attempt(shared, catalog, folder, volume, metadata, decide);
    match finished {
        Ok(outcome) => {
            shared.set(None);
            outcome
        }
        Err(CatalogError::Cancelled) => {
            shared.set(None);
            Outcome::Cancelled
        }
        Err(why) => {
            shared.set(Some(Stage::Failed {
                why: why.to_string(),
            }));
            Outcome::Failed
        }
    }
}

fn attempt(
    shared: &Shared,
    catalog: &Path,
    folder: &Path,
    volume: impl FnOnce(&Path) -> Result<VolumeId, CatalogError>,
    mut metadata: impl FnMut(&Path) -> Option<FileMetadata>,
    decide: &Receiver<Decision>,
) -> Result<Outcome, CatalogError> {
    let mut catalog = Catalog::open(catalog)?;
    let volume = volume(folder)?;
    let going = |shared: &Shared| !shared.stop.load(Ordering::Relaxed);

    // Reads no files: the count needs names and sizes, not capture times.
    let counted = scan::scan_watched(
        &mut catalog,
        folder,
        volume.clone(),
        scan::no_metadata,
        true,
        |progress| {
            if let Progress::Walking { found } = progress {
                shared.set(Some(Stage::Counting { found }));
            }
            going(shared)
        },
    )?;
    if counted.added == 0 && counted.updated == 0 {
        // Said by whoever called, who knows what the folder was called.
        return Ok(Outcome::Nothing);
    }
    shared.set(Some(Stage::Counted {
        fresh: counted.added,
        already: counted.unchanged + counted.updated,
        unreadable: counted.unreadable.len(),
        card: looks_like_a_card(folder),
    }));
    // A sender that has gone away is an answer too, and it is no.
    let into = match decide.recv().unwrap_or(Decision::No) {
        Decision::No => return Err(CatalogError::Cancelled),
        Decision::InPlace => None,
        Decision::CopyTo(into) => Some(into),
    };

    // Copied, verified, filed by date, and the copies added — the ingest the
    // command line has, with the window's progress and its Stop.
    if let Some(into) = into {
        let report = rawkit_catalog::ingest::ingest(
            &mut catalog,
            folder,
            &into,
            &mut metadata,
            |done, total, name| {
                if !name.is_empty() {
                    shared.set(Some(Stage::Copying {
                        done,
                        total,
                        name: name.to_string(),
                    }));
                }
                going(shared)
            },
        )?;
        return Ok(Outcome::Copied {
            added: report.scanned.as_ref().map_or(0, |scanned| scanned.added),
            already: report.already_there,
            failed: report.failed.len(),
            first_failure: report
                .failed
                .first()
                .map(|(path, why)| format!("{}: {why}", name_of(path))),
            stopped: report.stopped,
        });
    }

    let added = scan::scan_watched(&mut catalog, folder, volume, metadata, false, |progress| {
        if let Progress::Reading { done, total, name } = progress {
            shared.set(Some(Stage::Adding {
                done,
                total,
                name: name.to_string(),
            }));
        }
        going(shared)
    })?;
    Ok(Outcome::Added(added.added))
}

/// The folder a person would call it by.
/// A camera's card, or a folder on one: `DCIM` is the one folder every camera
/// writes, by the DCF standard.
pub fn looks_like_a_card(folder: &Path) -> bool {
    folder.join("DCIM").is_dir()
        || folder
            .components()
            .any(|part| part.as_os_str().eq_ignore_ascii_case("DCIM"))
}

pub fn name_of(folder: &Path) -> String {
    folder.file_name().map_or_else(
        || folder.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Where the import in this process is, if there is one.
pub struct Running {
    pub shared: std::sync::Arc<Shared>,
    pub decide: std::sync::mpsc::Sender<Decision>,
    pub folder: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::tests::Scratch;

    fn fixture(name: &str, photographs: usize) -> (Scratch, PathBuf, PathBuf) {
        let scratch = Scratch::new(name);
        let folder = scratch.0.join("shoot");
        std::fs::create_dir_all(&folder).unwrap();
        for n in 0..photographs {
            std::fs::write(folder.join(format!("DSC{n:05}.ARW")), b"raw").unwrap();
        }
        let catalog = scratch.0.join("library.rawkit");
        (scratch, catalog, folder)
    }

    fn volume(_: &Path) -> Result<VolumeId, CatalogError> {
        Ok(VolumeId::Uuid("test-volume".into()))
    }

    fn photographs(catalog: &Path) -> i64 {
        Catalog::open(catalog)
            .unwrap()
            .connection()
            .query_row("SELECT count(*) FROM images", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn it_counts_asks_and_then_adds() {
        let (_scratch, catalog, folder) = fixture("import-adds", 3);
        let shared = Shared::default();
        let (say, decide) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let (shared, catalog, folder) = (&shared, &catalog, &folder);
            let worker = scope
                .spawn(move || run(shared, catalog, folder, volume, scan::no_metadata, &decide));
            // Nothing is kept until somebody says so.
            let counted = loop {
                match shared.stage() {
                    Some(stage @ Stage::Counted { .. }) => break stage,
                    _ => std::thread::yield_now(),
                }
            };
            assert_eq!(
                counted,
                Stage::Counted {
                    fresh: 3,
                    already: 0,
                    unreadable: 0,
                    card: false,
                }
            );
            say.send(Decision::InPlace).unwrap();
            assert_eq!(worker.join().unwrap(), Outcome::Added(3));
        });
        assert_eq!(shared.stage(), None);
        assert_eq!(photographs(&catalog), 3);
    }

    #[test]
    fn a_card_is_offered_for_copying_and_the_copies_are_added() {
        let scratch = Scratch::new("import-card");
        let card = scratch.0.join("CARD");
        let shots = card.join("DCIM").join("100MSDCF");
        std::fs::create_dir_all(&shots).unwrap();
        for n in 0..2 {
            std::fs::write(shots.join(format!("DSC{n:05}.ARW")), format!("raw {n}")).unwrap();
        }
        let catalog = scratch.0.join("library.rawkit");
        let into = scratch.0.join("Pictures");
        let shared = Shared::default();
        let (say, decide) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let (shared, catalog, card) = (&shared, &catalog, &card);
            let worker =
                scope.spawn(move || run(shared, catalog, card, volume, scan::no_metadata, &decide));
            loop {
                if let Some(Stage::Counted { card, fresh, .. }) = shared.stage() {
                    assert!(card, "a folder holding DCIM is a card");
                    assert_eq!(fresh, 2);
                    break;
                }
                std::thread::yield_now();
            }
            say.send(Decision::CopyTo(into.clone())).unwrap();
            assert_eq!(
                worker.join().unwrap(),
                Outcome::Copied {
                    added: 2,
                    already: 0,
                    failed: 0,
                    first_failure: None,
                    stopped: false
                }
            );
        });
        // Copied, not moved: the card still has both, and the library has the
        // copies — undated here, because this test reads no headers.
        assert!(shots.join("DSC00000.ARW").exists());
        assert!(into.join("undated").join("DSC00001.ARW").exists());
        assert_eq!(photographs(&catalog), 2);
    }

    #[test]
    fn saying_no_keeps_nothing() {
        let (_scratch, catalog, folder) = fixture("import-declined", 3);
        let shared = Shared::default();
        let (say, decide) = std::sync::mpsc::channel();
        say.send(Decision::No).unwrap();
        let outcome = run(
            &shared,
            &catalog,
            &folder,
            volume,
            scan::no_metadata,
            &decide,
        );
        assert_eq!(outcome, Outcome::Cancelled);
        assert_eq!(photographs(&catalog), 0);
    }

    #[test]
    fn a_folder_already_in_the_catalog_is_nothing_to_add() {
        let (_scratch, catalog, folder) = fixture("import-again", 2);
        let (say, decide) = std::sync::mpsc::channel();
        say.send(Decision::InPlace).unwrap();
        let shared = Shared::default();
        assert_eq!(
            run(
                &shared,
                &catalog,
                &folder,
                volume,
                scan::no_metadata,
                &decide
            ),
            Outcome::Added(2)
        );
        // Nobody is asked anything the second time: there is nothing to decide.
        let (_nobody, decide) = std::sync::mpsc::channel::<Decision>();
        assert_eq!(
            run(
                &shared,
                &catalog,
                &folder,
                volume,
                scan::no_metadata,
                &decide
            ),
            Outcome::Nothing
        );
    }

    #[test]
    fn stopped_part_way_it_keeps_nothing() {
        let (_scratch, catalog, folder) = fixture("import-stopped", 6);
        let shared = Shared::default();
        let (say, decide) = std::sync::mpsc::channel();
        say.send(Decision::InPlace).unwrap();
        let mut read = 0;
        let outcome = run(
            &shared,
            &catalog,
            &folder,
            volume,
            |_| {
                read += 1;
                if read == 3 {
                    shared.stop();
                }
                None
            },
            &decide,
        );
        assert_eq!(outcome, Outcome::Cancelled);
        assert_eq!(photographs(&catalog), 0, "one transaction, rolled back");
    }

    #[test]
    fn a_folder_that_is_not_there_is_a_failure_that_stays_until_read() {
        let (scratch, catalog, _folder) = fixture("import-nowhere", 0);
        let shared = Shared::default();
        let (_say, decide) = std::sync::mpsc::channel::<Decision>();
        let outcome = run(
            &shared,
            &catalog,
            &scratch.0.join("no-such-folder"),
            volume,
            scan::no_metadata,
            &decide,
        );
        assert_eq!(outcome, Outcome::Failed);
        assert!(matches!(shared.stage(), Some(Stage::Failed { .. })));
        shared.dismiss();
        assert_eq!(shared.stage(), None);
    }
}
