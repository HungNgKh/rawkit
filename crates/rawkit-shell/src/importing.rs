//! Adding a folder of photographs, from the window.
//!
//! Add *in place*: the photographs stay where they are and the catalog learns
//! where that is. Nothing is copied, moved or renamed — the safe half of
//! "import", and the one that can be trusted with a stranger's library. Copying
//! from a card is a different promise and a later piece of work.
//!
//! # Count first, then ask
//!
//! The folder is walked and compared with the catalog before anything is kept,
//! and the person is told what would happen — "1 268 to add, 212 already here"
//! — on a button that says the number. The count is a dry run of the scan
//! itself ([`rawkit_catalog::scan::scan_watched`]), so it cannot disagree with
//! what the scan then does.
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

/// How an import ended.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// This many photographs are in the catalog that were not before.
    Added(usize),
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
    decide: &Receiver<bool>,
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
    metadata: impl FnMut(&Path) -> Option<FileMetadata>,
    decide: &Receiver<bool>,
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
    }));
    // A sender that has gone away is an answer too, and it is no.
    if !decide.recv().unwrap_or(false) {
        return Err(CatalogError::Cancelled);
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
pub fn name_of(folder: &Path) -> String {
    folder.file_name().map_or_else(
        || folder.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Where the import in this process is, if there is one.
pub struct Running {
    pub shared: std::sync::Arc<Shared>,
    pub decide: std::sync::mpsc::Sender<bool>,
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
                    unreadable: 0
                }
            );
            say.send(true).unwrap();
            assert_eq!(worker.join().unwrap(), Outcome::Added(3));
        });
        assert_eq!(shared.stage(), None);
        assert_eq!(photographs(&catalog), 3);
    }

    #[test]
    fn saying_no_keeps_nothing() {
        let (_scratch, catalog, folder) = fixture("import-declined", 3);
        let shared = Shared::default();
        let (say, decide) = std::sync::mpsc::channel();
        say.send(false).unwrap();
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
        say.send(true).unwrap();
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
        let (_nobody, decide) = std::sync::mpsc::channel::<bool>();
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
        say.send(true).unwrap();
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
        let (_say, decide) = std::sync::mpsc::channel::<bool>();
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
