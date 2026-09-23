//! Looking after the catalog, from the window.
//!
//! Two jobs, and both are about the day something goes wrong rather than about
//! today: **finding photographs that moved**, and **writing down what each
//! photograph is** so that they can be found.
//!
//! # Why contents, and why it has to be done in advance
//!
//! A catalog addresses a file by its path, and a path is the first thing to
//! break: a folder is renamed, a year moves to a bigger disk, a card import is
//! tidied up afterwards. What survives all of that is the file's contents, so
//! `files.content_hash` is the fallback identity and
//! [`rawkit_catalog::relink::search`] is what uses it.
//!
//! The catch is the order: a photograph can only be recognised later if what it
//! is was written down **while it was still there**. A copy off a card records
//! it as it verifies each file, so photographs that arrive that way are covered
//! from the start; a library added in place is not, because a scan reads
//! headers rather than whole files. Hence the second job: read every photograph
//! that has no note yet, once, and write it down.
//!
//! # Its own connection, and a window that stands still
//!
//! The same arrangement an import uses, and for the same reason: this writes to
//! the catalog on a thread of its own, SQLite here does not wait for a lock, so
//! the render loop flushes the edit in hand and then writes nothing until the
//! job ends. The sheet covers the window and takes the keyboard.

use rawkit_catalog::{db::Catalog, relink, scan, CatalogError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// What is being done to the catalog.
#[derive(Debug, Clone, PartialEq)]
pub enum Task {
    /// Look through this folder for the files of photographs that have gone.
    Find(PathBuf),
    /// Write down what each photograph is, for the ones with no note yet.
    Note,
}

impl Task {
    /// What the sheet is called while it runs.
    pub fn title(&self) -> &'static str {
        match self {
            Task::Find(_) => "Find photographs that moved",
            Task::Note => "Write down what each photograph is",
        }
    }

    /// The folder being looked through, for the sheet to show.
    pub fn where_at(&self) -> String {
        match self {
            Task::Find(folder) => folder.display().to_string(),
            Task::Note => String::new(),
        }
    }
}

/// Where the job has got to, for the page to draw.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum Stage {
    /// Listing the folder; `found` is how many files are worth reading.
    Looking {
        found: usize,
    },
    /// Reading files, one at a time.
    Reading {
        done: usize,
        total: usize,
        name: String,
    },
    /// Over, and this is what happened.
    Done {
        said: String,
    },
    Failed {
        why: String,
    },
}

#[derive(Default)]
pub struct Shared {
    stage: Mutex<Option<Stage>>,
    stop: AtomicBool,
}

impl Shared {
    pub fn stage(&self) -> Option<Stage> {
        self.stage.lock().expect("care stage").clone()
    }

    fn set(&self, stage: Option<Stage>) {
        *self.stage.lock().expect("care stage") = stage;
    }

    /// Keep a sentence on the sheet while the window goes away to reopen.
    pub fn said(&self, said: String) {
        self.set(Some(Stage::Done { said }));
    }

    pub fn fail(&self, why: String) {
        self.set(Some(Stage::Failed { why }));
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn going(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }
}

/// How a job ended.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// What to say, and how many photographs came back — which is what decides
    /// whether the catalog is worth reopening.
    Finished {
        said: String,
        put_back: usize,
    },
    Cancelled,
    /// And [`Stage::Failed`] says why.
    Failed,
}

/// The job this process is running, if it is.
pub struct Running {
    pub shared: std::sync::Arc<Shared>,
    pub task: Task,
}

pub fn run(shared: &Shared, catalog: &Path, task: &Task) -> Outcome {
    let done = match task {
        Task::Find(folder) => find(shared, catalog, folder),
        Task::Note => note(shared, catalog),
    };
    match done {
        Ok(outcome) => {
            // A sentence stays on the sheet only when it is about a failure;
            // everything else is said where the rest of the window says things.
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

fn find(shared: &Shared, catalog: &Path, folder: &Path) -> Result<Outcome, CatalogError> {
    let mut catalog = Catalog::open(catalog)?;
    shared.set(Some(Stage::Looking { found: 0 }));
    let found = relink::search(&mut catalog, folder, false, |looking| {
        match looking {
            relink::Looking::Walking { found } => shared.set(Some(Stage::Looking { found })),
            relink::Looking::Reading { done, total, name } => shared.set(Some(Stage::Reading {
                done,
                total,
                name: name.to_string(),
            })),
        }
        shared.going()
    })?;

    let mut said = match found.relinked {
        0 => "No photograph in there is one this catalog had lost".to_string(),
        1 => "Found 1 photograph that had moved, and put it back".to_string(),
        n => format!("Found {n} photographs that had moved, and put them back"),
    };
    if found.still_missing > 0 {
        said.push_str(&match found.still_missing {
            1 => "; 1 is still missing".to_string(),
            n => format!("; {n} are still missing"),
        });
    }
    if found.unhashed > 0 {
        said.push_str(&format!(
            ", {} of them with no note of what they are — nothing can find those",
            found.unhashed
        ));
    }
    if found.ambiguous > 0 {
        said.push_str(&match found.ambiguous {
            1 => "; 1 file here matches more than one of them and was left alone".to_string(),
            n => format!("; {n} files here match more than one of them and were left alone"),
        });
    }
    Ok(Outcome::Finished {
        said,
        put_back: found.relinked,
    })
}

fn note(shared: &Shared, catalog: &Path) -> Result<Outcome, CatalogError> {
    let mut catalog = Catalog::open(catalog)?;
    shared.set(Some(Stage::Reading {
        done: 0,
        total: 0,
        name: String::new(),
    }));
    let mut stopped = false;
    let (noted, unreadable) = scan::hash_missing(&mut catalog, |done, total| {
        shared.set(Some(Stage::Reading {
            done,
            total,
            name: String::new(),
        }));
        stopped = !shared.going();
        !stopped
    })?;
    let mut said = match noted {
        0 => "Every photograph already has a note of what it is".to_string(),
        1 => "Wrote down what 1 photograph is".to_string(),
        n => format!("Wrote down what {n} photographs are"),
    };
    if unreadable > 0 {
        said.push_str(&format!("; {unreadable} could not be read"));
    }
    if stopped {
        said.push_str(", and it stopped there");
    }
    Ok(Outcome::Finished { said, put_back: 0 })
}
