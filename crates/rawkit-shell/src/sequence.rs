//! What is being walked through, as one value that cannot be half right.
//!
//! # The bug this type exists to make impossible
//!
//! The library used to keep four loose fields — the photographs on screen, the
//! filter, which collection was being viewed and the cursor — and re-read the
//! first of them from the catalog in **six** places. Each of those six had to
//! remember, on its own, which collection it was in. Five did. The sixth was in
//! the undo arm, so pressing Z inside a collection dropped you back into the
//! whole library without saying so, and it was found by grepping for a pattern
//! rather than by anything that would have failed.
//!
//! That is a bug a *structure* permits and only discipline prevents. Here the
//! source, the filter and the photographs they produce are one value with one
//! way in — [`Sequence::read`], which takes a source and a filter — so there is
//! no such thing as re-reading the photographs while forgetting where they come
//! from. Nothing outside this file can write the three apart.
//!
//! # And it is never empty
//!
//! [`Sequence::read`] answers `None` rather than building an empty one, and
//! every method that removes something refuses to remove the last. So "the
//! photograph under the cursor" always names one, which the rest of the shell
//! leans on everywhere — and a caller that wants to fall back to a wider view
//! builds that view *first* and assigns it only if it exists, instead of
//! writing a field and then hand-rolling the rollback when the read disappoints.
//!
//! # Why the rows are held and the filter is applied to them
//!
//! A cull is mostly one question asked over and over: this frame has just been
//! judged — is it still in what I am looking at? Answering that by reading the
//! sequence again is four joined tables and a path string per photograph, per
//! keypress: measured, 20 ms at twenty thousand and linear, so the whole of an
//! interaction's budget at a hundred thousand. But nothing about the *source*
//! changed. One frame's admission did.
//!
//! So every photograph the source holds is kept in the source's order, and the
//! filter is a list of which of them to show. A judgement removes one entry from
//! that list; an undo puts one back; changing the filter asks the catalog which
//! ids it admits — one table, no joins — and keeps those. **The catalog is still
//! the only opinion about what a filter means.** Whether a frame is admitted is
//! asked of [`cull::matches`] and which ids a filter admits of
//! [`cull::admitted`], both built on the same `narrowing` the full read uses;
//! none of it is re-decided in Rust.
//!
//! The assumption underneath, stated because it will not always hold: judging a
//! photograph changes only *that* photograph's admission. True of every filter
//! there is — flag, rating, colour. A filter about a frame's neighbours, such as
//! "the top of each stack", would break it, and [`Sequence::agrees_with`] is the
//! full re-read kept as an oracle so the tests say so the day one arrives.

use anyhow::Result;
use rawkit_catalog::collections;
use rawkit_catalog::cull::{self, Filter, LibraryImage};
use rawkit_catalog::db::Catalog;
use std::collections::HashMap;

/// Where the photographs come from, and in whose order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Everything, in the order it was shot.
    Library,
    /// One collection, in the order somebody put it in.
    Collection(i64),
}

/// What happened when a photograph was taken out of what is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dropped {
    /// It was not being shown, so nothing moved.
    NotShown,
    /// It went, and the cursor's slot now holds whatever came after it.
    Gone,
    /// It is the only photograph showing, and it stays. An empty sequence is a
    /// state the shell cannot draw, so the caller widens the view instead.
    WouldEmpty,
}

pub struct Sequence {
    source: Source,
    filter: Filter,
    /// Every photograph the source holds, in the source's order, whatever the
    /// filter says. Read once per source; a filter never touches it.
    rows: Vec<LibraryImage>,
    /// Where each photograph sits in `rows`. What makes "where is this id" a
    /// lookup rather than the walk down the whole sequence it used to be — and
    /// that walk ran once per marked frame, per keypress.
    place: HashMap<i64, u32>,
    /// The places in `rows` the filter admits, ascending. This *is* the view.
    shown: Vec<u32>,
}

impl Sequence {
    /// The only way to make one: from where the photographs come and which of
    /// them to show. `None` when that is nothing.
    pub fn read(catalog: &Catalog, source: Source, filter: Filter) -> Result<Option<Self>> {
        // Unfiltered, deliberately. The filter is applied to these rather than
        // baked into the read, which is what lets it change without reading
        // them again.
        let everything = Filter::default();
        let rows = match source {
            Source::Library => cull::sequence(catalog, &everything)?,
            Source::Collection(id) => collections::members(catalog, id, &everything)?,
        };
        let place = rows
            .iter()
            .enumerate()
            .map(|(at, image)| (image.id, at as u32))
            .collect();
        let mut sequence = Self {
            source,
            filter: everything,
            rows,
            place,
            shown: Vec::new(),
        };
        let shown = sequence.admitted(catalog, &filter)?;
        if shown.is_empty() {
            return Ok(None);
        }
        sequence.shown = shown;
        sequence.filter = filter;
        Ok(Some(sequence))
    }

    /// Which places a filter would show, asked of the catalog.
    fn admitted(&self, catalog: &Catalog, filter: &Filter) -> Result<Vec<u32>> {
        if filter.is_everything() {
            return Ok((0..self.rows.len() as u32).collect());
        }
        let mut places: Vec<u32> = cull::admitted(catalog, filter)?
            .iter()
            // An id the catalog admits and this source does not hold is simply
            // not here: another collection's photograph, or a missing file.
            .filter_map(|id| self.place.get(id).copied())
            .collect();
        places.sort_unstable();
        Ok(places)
    }

    /// Show a different part of the same source, and say where the cursor goes.
    ///
    /// `None` when the filter would show nothing, **and then nothing has
    /// changed** — the new view is worked out in full before any of it is kept.
    ///
    /// The cursor stays on its photograph when the new filter admits it, and
    /// otherwise moves to the first frame *after* it that the filter does admit.
    /// Not to the start: where somebody had got to in a shoot is worth more than
    /// the tidiness of beginning again.
    pub fn narrow(
        &mut self,
        catalog: &Catalog,
        filter: Filter,
        standing: usize,
    ) -> Result<Option<usize>> {
        let shown = self.admitted(catalog, &filter)?;
        if shown.is_empty() {
            return Ok(None);
        }
        // Both lists are places in `rows`, ascending — so "the nearest one
        // forward" is the first new place at or past the old one.
        let from = self.shown.get(standing).copied().unwrap_or(0);
        let index = shown.partition_point(|place| *place < from);
        let index = index.min(shown.len() - 1);
        self.shown = shown;
        self.filter = filter;
        Ok(Some(index))
    }

    pub fn source(&self) -> Source {
        self.source
    }

    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    /// How many photographs are showing. Never zero.
    pub fn len(&self) -> usize {
        self.shown.len()
    }

    /// The photograph in a slot of what is showing.
    pub fn get(&self, index: usize) -> Option<&LibraryImage> {
        self.shown
            .get(index)
            .map(|place| &self.rows[*place as usize])
    }

    /// The photographs in a run of slots, clamped to what there is.
    pub fn slice(&self, from: usize, to: usize) -> impl Iterator<Item = &LibraryImage> {
        let to = to.min(self.shown.len());
        let from = from.min(to);
        self.shown[from..to]
            .iter()
            .map(|place| &self.rows[*place as usize])
    }

    /// Every photograph the source holds, whatever the filter shows of them.
    ///
    /// What the preview builder walks. A filter narrows what somebody is looking
    /// at, not what the library is: a build that followed the view would finish
    /// "everything" on a view of three picks and leave the rest of the shoot
    /// black for whoever clears the filter.
    pub fn everything(&self) -> &[LibraryImage] {
        &self.rows
    }

    /// Which slot a photograph is showing in, if it is showing.
    pub fn position_of(&self, id: i64) -> Option<usize> {
        let place = self.place.get(&id)?;
        self.shown.binary_search(place).ok()
    }

    /// Stop showing a photograph whose judgement has just put it outside the
    /// filter. The rows are untouched: it is still in the source, and an undo
    /// can bring it back without asking the catalog for anything but its
    /// admission.
    pub fn drop_shown(&mut self, id: i64) -> Dropped {
        let Some(index) = self.position_of(id) else {
            return Dropped::NotShown;
        };
        if self.shown.len() == 1 {
            return Dropped::WouldEmpty;
        }
        self.shown.remove(index);
        Dropped::Gone
    }

    /// Show a photograph again, in the slot the source's order gives it.
    ///
    /// `None` when the source does not hold it at all, which an undo can ask
    /// about: the frame may have left the collection, or gone missing, since the
    /// judgement being reversed.
    pub fn admit(&mut self, id: i64) -> Option<usize> {
        let place = *self.place.get(&id)?;
        match self.shown.binary_search(&place) {
            Ok(index) => Some(index),
            Err(index) => {
                self.shown.insert(index, place);
                Some(index)
            }
        }
    }

    /// Exchange the photographs in two slots, leaving everything else where it
    /// is. The in-memory half of moving a frame within a collection — the
    /// catalog's half is `collections::swap`, and the two write the same two.
    pub fn swap(&mut self, a: usize, b: usize) {
        let (Some(&at_a), Some(&at_b)) = (self.shown.get(a), self.shown.get(b)) else {
            return;
        };
        self.rows.swap(at_a as usize, at_b as usize);
        for place in [at_a, at_b] {
            self.place.insert(self.rows[place as usize].id, place);
        }
    }

    /// Whether the catalog, read in full, agrees with what is showing.
    ///
    /// The re-read this type exists to avoid, kept as an oracle: every shortcut
    /// above is a claim that it produces the same sequence the catalog would,
    /// and this is what holds them to it. The tests run it after every action.
    #[cfg(test)]
    pub fn agrees_with(&self, catalog: &Catalog) -> Result<()> {
        let truth: Vec<i64> = match self.source {
            Source::Library => cull::sequence(catalog, &self.filter)?,
            Source::Collection(id) => collections::members(catalog, id, &self.filter)?,
        }
        .iter()
        .map(|image| image.id)
        .collect();
        let held: Vec<i64> = self.slice(0, self.len()).map(|image| image.id).collect();
        anyhow::ensure!(
            truth == held,
            "the sequence in memory has drifted from the catalog: {} photographs \
             held against {} read, first difference at slot {:?}",
            held.len(),
            truth.len(),
            truth.iter().zip(&held).position(|(a, b)| a != b)
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::tests::{library_at, Scratch};
    use rawkit_catalog::cull::{Flag, Flagged, Judgement};

    /// The ids showing, in order.
    fn ids(sequence: &Sequence) -> Vec<i64> {
        sequence.slice(0, sequence.len()).map(|i| i.id).collect()
    }

    fn flag(catalog: &Catalog, id: i64, flag: Option<Flag>) {
        let judgement = Judgement {
            rating: None,
            flag,
            colour: None,
        };
        cull::set(catalog, id, &judgement).unwrap();
    }

    #[test]
    fn there_is_no_such_thing_as_an_empty_one() {
        // The rule the rest of the shell leans on, held here rather than by
        // every caller remembering to check.
        let dir = Scratch::new("sequence-empty");
        let library = library_at(&dir.0, 3);
        let catalog = library.catalog();
        let picks = Filter::flagged(Flagged::Pick);
        assert!(
            Sequence::read(catalog, Source::Library, picks.clone())
                .unwrap()
                .is_none(),
            "nothing is picked, so there is nothing to build"
        );

        let mut all = Sequence::read(catalog, Source::Library, Filter::default())
            .unwrap()
            .unwrap();
        let before = ids(&all);
        // **A refused narrowing changes nothing at all** — neither the filter
        // nor what is showing. The view is worked out in full before any of it
        // is kept, which is what replaced writing a field and rolling it back.
        assert_eq!(all.narrow(catalog, picks, 0).unwrap(), None);
        assert_eq!(ids(&all), before);
        assert!(all.filter().is_everything());
    }

    #[test]
    fn the_last_photograph_showing_cannot_be_dropped() {
        let dir = Scratch::new("sequence-last");
        let library = library_at(&dir.0, 3);
        let catalog = library.catalog();
        let every = ids(&Sequence::read(catalog, Source::Library, Filter::default())
            .unwrap()
            .unwrap());
        flag(catalog, every[1], Some(Flag::Pick));
        let mut picks = Sequence::read(catalog, Source::Library, Filter::flagged(Flagged::Pick))
            .unwrap()
            .unwrap();
        assert_eq!(ids(&picks), [every[1]]);
        assert_eq!(picks.drop_shown(every[1]), Dropped::WouldEmpty);
        assert_eq!(picks.len(), 1, "it stays, and the caller widens the view");
        assert_eq!(picks.drop_shown(every[0]), Dropped::NotShown);
    }

    #[test]
    fn a_dropped_frame_comes_back_where_the_source_had_it() {
        // Undo. The rows never left, so putting one back asks the catalog for
        // nothing — and it has to land in the source's order, not at the end.
        let dir = Scratch::new("sequence-admit");
        let library = library_at(&dir.0, 4);
        let catalog = library.catalog();
        let mut all = Sequence::read(catalog, Source::Library, Filter::default())
            .unwrap()
            .unwrap();
        let every = ids(&all);
        assert_eq!(all.drop_shown(every[1]), Dropped::Gone);
        assert_eq!(ids(&all), [every[0], every[2], every[3]]);
        assert_eq!(all.position_of(every[1]), None);
        assert_eq!(all.admit(every[1]), Some(1));
        assert_eq!(ids(&all), every);
        // Again is harmless, and a photograph the source never held is not
        // something an undo can conjure.
        assert_eq!(all.admit(every[1]), Some(1));
        assert_eq!(all.admit(999_999), None);
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn narrowing_leaves_the_cursor_on_the_nearest_frame_forward() {
        // Where somebody had got to in a shoot is worth more than the tidiness
        // of beginning again.
        let dir = Scratch::new("sequence-cursor");
        let library = library_at(&dir.0, 6);
        let catalog = library.catalog();
        let mut all = Sequence::read(catalog, Source::Library, Filter::default())
            .unwrap()
            .unwrap();
        let every = ids(&all);
        for at in [0, 4, 5] {
            flag(catalog, every[at], Some(Flag::Pick));
        }
        let picks = Filter::flagged(Flagged::Pick);
        // Standing on frame 2, which is not a pick: the next pick forward is 4.
        let index = all.narrow(catalog, picks.clone(), 2).unwrap().unwrap();
        assert_eq!(all.get(index).unwrap().id, every[4]);
        all.agrees_with(catalog).unwrap();

        // Standing on a pick: it stays under the cursor.
        let mut again = Sequence::read(catalog, Source::Library, Filter::default())
            .unwrap()
            .unwrap();
        let index = again.narrow(catalog, picks, 4).unwrap().unwrap();
        assert_eq!(again.get(index).unwrap().id, every[4]);
    }

    #[test]
    fn a_collection_is_shown_in_its_own_order_and_a_filter_narrows_within_it() {
        let dir = Scratch::new("sequence-collection");
        let library = library_at(&dir.0, 5);
        let catalog = library.catalog();
        let every = ids(&Sequence::read(catalog, Source::Library, Filter::default())
            .unwrap()
            .unwrap());
        let shelf =
            collections::create_holding(catalog, "Edit", None, &[every[3], every[0], every[2]])
                .unwrap();
        flag(catalog, every[0], Some(Flag::Pick));
        // A pick that is *not* in the collection, which a filter must not let in.
        flag(catalog, every[4], Some(Flag::Pick));

        let mut held = Sequence::read(catalog, Source::Collection(shelf), Filter::default())
            .unwrap()
            .unwrap();
        assert_eq!(ids(&held), [every[3], every[0], every[2]]);
        held.narrow(catalog, Filter::flagged(Flagged::Pick), 0)
            .unwrap()
            .unwrap();
        assert_eq!(
            ids(&held),
            [every[0]],
            "the library's other pick stayed out"
        );
        held.agrees_with(catalog).unwrap();
    }

    #[test]
    fn swapping_two_slots_matches_what_the_catalog_does_with_them() {
        // The two halves of moving a frame — `collections::swap` on disk and
        // this in memory — have to describe the same move, including under a
        // filter, where the two slots are not neighbours in the stored order.
        let dir = Scratch::new("sequence-swap");
        let library = library_at(&dir.0, 4);
        let catalog = library.catalog();
        let every = ids(&Sequence::read(catalog, Source::Library, Filter::default())
            .unwrap()
            .unwrap());
        let shelf = collections::create_holding(catalog, "Edit", None, &every).unwrap();
        flag(catalog, every[0], Some(Flag::Pick));
        flag(catalog, every[3], Some(Flag::Pick));
        let mut picks = Sequence::read(
            catalog,
            Source::Collection(shelf),
            Filter::flagged(Flagged::Pick),
        )
        .unwrap()
        .unwrap();
        assert_eq!(ids(&picks), [every[0], every[3]]);

        collections::swap(catalog, shelf, every[0], every[3]).unwrap();
        picks.swap(0, 1);
        assert_eq!(ids(&picks), [every[3], every[0]]);
        picks.agrees_with(catalog).unwrap();
        assert_eq!(
            picks.position_of(every[3]),
            Some(0),
            "the lookup followed the move"
        );
    }
}
