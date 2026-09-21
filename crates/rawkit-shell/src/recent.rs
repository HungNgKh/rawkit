//! The catalogs this machine has opened, most recent first.
//!
//! What a bare launch reopens, and what the welcome screen lists. A file beside
//! the window's geometry and for the same reason: it describes this machine,
//! not a library. And like that file, nothing here refuses — a list that cannot
//! be read is an empty list, and one that cannot be written is a launch that
//! will not remember, neither of which is a reason not to open a window.
//!
//! Takes a directory rather than finding one, so that what it does can be
//! tested without an application to ask.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How many are kept. The list is for getting back to something, and the
/// eleventh-most-recent catalog is found faster with the file picker.
const KEPT: usize = 10;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recent {
    pub path: PathBuf,
    /// How many photographs it held when it was last opened. Said on the
    /// welcome screen, where it is what tells two catalogs with sensible names
    /// apart; never relied on.
    pub photographs: usize,
    /// Seconds since the epoch.
    pub opened_at: u64,
}

fn file(dir: &Path) -> PathBuf {
    dir.join("recent.json")
}

/// Everything remembered, most recent first. A catalog that is no longer there
/// is still listed: it may be on a drive that is not plugged in, and a list
/// that quietly dropped it would be one more thing to wonder about.
pub fn all(dir: &Path) -> Vec<Recent> {
    std::fs::read_to_string(file(dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// The catalog a launch with no argument should reopen: the last one opened,
/// if it is still there. Only the last — falling back to the one before would
/// open something the person did not leave, without saying why.
pub fn last(dir: &Path) -> Option<PathBuf> {
    all(dir)
        .into_iter()
        .next()
        .map(|recent| recent.path)
        .filter(|path| path.is_file())
}

/// Note that a catalog was opened just now.
pub fn note(dir: &Path, path: &Path, photographs: usize, now: u64) {
    // Absolute, because the next launch may start somewhere else. Not
    // canonical: a symlinked library is opened by the name its owner uses.
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut list = all(dir);
    list.retain(|recent| recent.path != path);
    list.insert(
        0,
        Recent {
            path,
            photographs,
            opened_at: now,
        },
    );
    list.truncate(KEPT);
    if let Ok(text) = serde_json::to_string_pretty(&list) {
        let _ = std::fs::write(file(dir), text);
    }
}

/// Take one off the list — what "it is not there any more" leads to.
pub fn forget(dir: &Path, path: &Path) {
    let mut list = all(dir);
    list.retain(|recent| recent.path != path);
    if let Ok(text) = serde_json::to_string_pretty(&list) {
        let _ = std::fs::write(file(dir), text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::tests::Scratch;

    #[test]
    fn the_last_one_opened_comes_first_and_is_listed_once() {
        let scratch = Scratch::new("recent-order");
        let (a, b) = (scratch.0.join("a.rawkit"), scratch.0.join("b.rawkit"));
        note(&scratch.0, &a, 10, 100);
        note(&scratch.0, &b, 20, 200);
        note(&scratch.0, &a, 12, 300);
        let list = all(&scratch.0);
        assert_eq!(
            list.iter()
                .map(|r| (r.path.clone(), r.photographs))
                .collect::<Vec<_>>(),
            vec![(a, 12), (b, 20)]
        );
    }

    #[test]
    fn only_so_many_are_kept() {
        let scratch = Scratch::new("recent-kept");
        for n in 0..KEPT + 3 {
            note(
                &scratch.0,
                &scratch.0.join(format!("{n}.rawkit")),
                n,
                n as u64,
            );
        }
        let list = all(&scratch.0);
        assert_eq!(list.len(), KEPT);
        assert_eq!(list[0].photographs, KEPT + 2, "and the newest are the ones");
    }

    #[test]
    fn a_bare_launch_reopens_the_last_one_only_if_it_is_there() {
        let scratch = Scratch::new("recent-last");
        let (here, gone) = (scratch.0.join("here.rawkit"), scratch.0.join("gone.rawkit"));
        std::fs::write(&here, b"").unwrap();
        note(&scratch.0, &here, 1, 100);
        assert_eq!(last(&scratch.0), Some(here.clone()));
        // The most recent has gone — a drive not plugged in. Not the one before
        // it: that is a catalog nobody left open.
        note(&scratch.0, &gone, 1, 200);
        assert_eq!(last(&scratch.0), None);
        assert_eq!(all(&scratch.0).len(), 2, "and it is still listed");
        forget(&scratch.0, &gone);
        assert_eq!(last(&scratch.0), Some(here));
    }

    #[test]
    fn a_list_that_cannot_be_read_is_an_empty_one() {
        let scratch = Scratch::new("recent-unreadable");
        std::fs::write(file(&scratch.0), b"{ not json").unwrap();
        assert!(all(&scratch.0).is_empty());
        assert_eq!(last(&scratch.0), None);
    }
}
