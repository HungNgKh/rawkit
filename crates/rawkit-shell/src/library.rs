//! Culling: moving through a shoot and deciding what survives.
//!
//! # Where the work happens, and why it is split in two
//!
//! A keypress does two very different things. Recording a judgement is a row
//! update and takes microseconds, so it happens on the spot, in the thread the
//! page called from. *Changing which photograph is on screen* means decoding a
//! RAW and reducing a pyramid — a fifth of a second — and it has to happen where
//! the GPU handles live, which on Linux is the main thread.
//!
//! So a navigation keypress does not load anything. It leaves a request, and the
//! render loop picks it up on its next frame. That keeps the IPC call quick,
//! keeps decoding off a thread that must not block the compositor, and means a
//! held-down arrow key coalesces into one load rather than queueing forty — the
//! same reason the session has no command queue.
//!
//! # Pinned zoom comes free
//!
//! The session holds a viewport and an image *size*, and knows nothing about
//! pixels. Two frames from the same body are the same size, so moving between
//! them does not have to touch the viewport at all: a 1:1 look at the eye of a
//! bird stays a 1:1 look at the same place in the next frame. That is the
//! sharpness-check workflow the design calls out Lightroom for handling badly,
//! and here it is the absence of code rather than the presence of it.

use crate::sequence::{Dropped, Sequence, Source};
use anyhow::{anyhow, Context, Result};
use rawkit_catalog::collections::{self, Collection, Placed, Removed};
use rawkit_catalog::cull::{self, Filter, Flag, Judgement, LibraryImage};
use rawkit_catalog::db::Catalog;
use rawkit_catalog::previews;
use rawkit_editstate::EditState;
use rawkit_engine::render::Level;
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Pyramid};
use rawkit_session::Session;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// One photograph, decoded, with its reductions — everything the renderer needs
/// and nothing it does not.
///
/// The mosaic and the levels are owned here and the `Frame` and `Pyramid` are
/// built per use. That is the point of the type: a `Pyramid` borrows its base, so
/// a struct holding both the mosaic and a pyramid over it would refer to itself.
/// Keeping the *levels* instead makes both cheap views, and — the reason this
/// matters — makes an image replaceable. The previous version leaked its mosaic
/// deliberately, which is fine for one photograph and is 96 MB per keypress for a
/// cull.
pub struct Loaded {
    mosaic: Vec<f32>,
    levels: Vec<Level>,
    pub size: [u32; 2],
    phase: BayerPhase,
    wb: [f32; 3],
    profile: CameraProfile,
    /// Which body took it, so the catalog can be asked what to render it with.
    /// `None` for the synthetic mosaic, which no profile describes.
    camera: Option<rawkit_decode::CameraId>,
    /// What the camera says it takes to stand this frame upright. Kept beside
    /// `wb` because it is the same kind of fact: what the file recorded, which
    /// the matching `EditState` field resolves to.
    pub orientation: rawkit_editstate::Orientation,
    /// The maker's own distortion curve for the lens that was mounted, when the
    /// body had a profile for it. `None` for a lens it does not know, and for
    /// the synthetic mosaic, which no lens drew.
    pub distortion: Option<[i16; 16]>,
}

impl Loaded {
    /// Decode a RAW, or synthesise one when there is no file to open.
    pub fn open(path: Option<&Path>, tile: u32) -> Result<Self> {
        let (mosaic, size, phase, wb, profile, camera, orientation, distortion) = match path {
            None => {
                eprintln!("image      : no file given, using a synthetic mosaic");
                let (width, height) = (2048u32, 1365u32);
                (
                    crate::test_mosaic(width, height),
                    [width & !1, height & !1],
                    BayerPhase::Rggb,
                    [1.0, 1.0, 1.0],
                    CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY),
                    None,
                    // A synthetic mosaic has no camera and so nothing to say
                    // about which way up it is, and no lens drew it.
                    rawkit_editstate::Orientation::AsShot,
                    None,
                )
            }
            Some(path) => {
                let raw = rawkit_decode::decode_file(path)
                    .with_context(|| format!("decoding {}", path.display()))?;
                let phase = BayerPhase::from_cfa(raw.cfa).ok_or_else(|| {
                    anyhow!(
                        "{:?} is not a Bayer sensor; RCD cannot demosaic it",
                        raw.cfa
                    )
                })?;
                // The decoder's own matrix, treated as a single D65 illuminant.
                // Defensible and not accurate; a .dcp is what makes it accurate,
                // and the shell has nowhere to ask for one yet.
                //
                // Through the engine rather than rebuilt here. This was the
                // third place that read the matrix and decided what to do with
                // an absent one, and a third reading is a third chance to read
                // it backwards.
                let profile = rawkit_engine::render::profile_for(&raw);
                let wb = [
                    raw.as_shot_neutral[0],
                    raw.as_shot_neutral[1],
                    raw.as_shot_neutral[2],
                ];
                let size = [raw.width, raw.height];
                let camera = raw.camera.clone();
                let orientation = raw.orientation;
                let distortion = raw.distortion;
                (
                    rawkit_engine::normalise(&raw),
                    size,
                    phase,
                    wb,
                    profile,
                    Some(camera),
                    orientation,
                    distortion,
                )
            }
        };

        // Built here and taken apart, so that what this struct holds is the two
        // owned pieces rather than a view into itself.
        let mut loaded = Self {
            mosaic,
            levels: Vec::new(),
            size,
            phase,
            wb,
            profile,
            camera,
            orientation,
            distortion,
        };
        loaded.levels = Pyramid::build(&loaded.frame(), tile).into_levels();
        Ok(loaded)
    }

    /// Which body took this photograph.
    pub fn camera(&self) -> Option<&rawkit_decode::CameraId> {
        self.camera.as_ref()
    }

    /// Render it with a different profile from here on.
    ///
    /// The pyramid does not have to be rebuilt: it reduces the *mosaic*, and a
    /// mosaic is what the sensor recorded rather than what a profile makes of
    /// it.
    pub fn set_profile(&mut self, profile: CameraProfile) {
        self.profile = profile;
    }

    pub fn frame(&self) -> Frame<'_> {
        Frame {
            data: &self.mosaic,
            width: self.size[0],
            height: self.size[1],
            phase: self.phase,
            as_shot_wb: self.wb,
            clip_level: 1.0,
            profile: self.profile.clone(),
            recorded_orientation: self.orientation,
        }
    }

    /// The average of each colour-filter channel over a rectangle of the sensor.
    ///
    /// For the white-balance eyedropper, and taken from the *mosaic* rather than
    /// from the canvas on purpose: white balance is a statement about what the
    /// sensor recorded, and `temperature_from_multipliers` is defined on camera
    /// values. Sampling the rendered picture would ask the question one
    /// transform too late, and a profile with a look table would answer it
    /// differently again.
    ///
    /// No demosaic: the two greens of each quad are averaged with the rest, and
    /// over a patch large enough to matter that is what a demosaic would have
    /// produced anyway, without inventing detail to then average away.
    ///
    /// `None` when the rectangle falls outside the sensor or holds no whole
    /// Bayer quad — a click on the letterbox, or one so zoomed in that the
    /// square covers fewer than four photosites.
    /// The colour profile this frame renders with, for anything that has to
    /// invert what the renderer does — the eyedropper turning camera values back
    /// into a temperature, for one.
    pub fn profile(&self) -> &CameraProfile {
        &self.profile
    }

    pub fn channels_over(&self, rect: [f64; 4]) -> Option<[f32; 3]> {
        let [w, h] = self.size;
        let x0 = rect[0].floor().max(0.0) as u32;
        let y0 = rect[1].floor().max(0.0) as u32;
        let x1 = (rect[2].ceil() as i64).clamp(0, w as i64) as u32;
        let y1 = (rect[3].ceil() as i64).clamp(0, h as i64) as u32;
        if x1 <= x0 + 1 || y1 <= y0 + 1 {
            return None;
        }

        let mut total = [0.0f64; 3];
        let mut counts = [0u32; 3];
        for y in y0..y1 {
            for x in x0..x1 {
                let channel = match (self.phase, x % 2 == 0, y % 2 == 0) {
                    (BayerPhase::Rggb, true, true) | (BayerPhase::Bggr, false, false) => 0,
                    (BayerPhase::Rggb, false, false) | (BayerPhase::Bggr, true, true) => 2,
                    (BayerPhase::Grbg, false, true) | (BayerPhase::Gbrg, true, false) => 0,
                    (BayerPhase::Grbg, true, false) | (BayerPhase::Gbrg, false, true) => 2,
                    _ => 1,
                };
                total[channel] += self.mosaic[(y as usize) * (w as usize) + x as usize] as f64;
                counts[channel] += 1;
            }
        }
        if counts.contains(&0) {
            return None;
        }
        Some([
            (total[0] / counts[0] as f64) as f32,
            (total[1] / counts[1] as f64) as f32,
            (total[2] / counts[2] as f64) as f32,
        ])
    }

    pub fn pyramid(&self) -> Pyramid<'_> {
        Pyramid::from_levels(&self.mosaic, (self.size[0], self.size[1]), &self.levels)
    }
}

/// A preview read off disk and decoded, ready for the GPU.
///
/// Eight-bit **sRGB**, exactly as `rawkit_export::decode` hands it back and
/// exactly what `PreviewBlit::upload` wants. Nothing in between converts it, and
/// nothing should: the texture format is what does the colour management.
pub struct Decoded {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// What the page can ask for. Adjacently tagged for the reason `Command` is: a
/// newtype variant holding a bare number has no other representation serde can
/// round-trip.
/// One reversible thing, so `Z` reverses whichever happened last.
///
/// A judgement and a paste are different in shape but the same to a user: the
/// previous action. Two stacks would mean two keys, and the second one would be
/// pressed by accident.
/// Frames are named by catalog id rather than by position, and that is not a
/// matter of taste: a filter changes what position 12 means, so an undo stack
/// holding positions would put a rating back on whichever photograph had moved
/// into the slot.
#[derive(Debug)]
enum Undone {
    Judged {
        image: i64,
        before: Judgement,
    },
    /// What each frame's edit was before the paste. `None` means it had none —
    /// restoring that writes the identity edit rather than deleting a version,
    /// because the history is append-only and an undo is itself a decision.
    Pasted {
        frames: Vec<(i64, Option<EditState>)>,
    },
    /// Photographs taken out of a collection, and the place each held — one
    /// frame, the marked ones, or all of them when a collection is emptied.
    ///
    /// Every way of taking something out of a collection lands here, K included.
    /// A hand-made order cannot be recomputed, and "some of the ways to lose it
    /// can be undone" is a worse thing to live with than "all of them can".
    TakenOut {
        collection: i64,
        placed: Vec<Placed>,
    },
    /// Photographs put into a collection. Only the ones that were not already
    /// there, so taking this back never removes a frame somebody added earlier.
    Added {
        collection: i64,
        images: Vec<i64>,
    },
    /// A collection that was deleted, with everything nested in it.
    Deleted(Removed),
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "action", content = "value", rename_all = "snake_case")]
pub enum CullAction {
    Next,
    Previous,
    /// Stars, `0` meaning none. Does **not** advance: a digit is a considered
    /// judgement and changing your mind about it should not mean navigating back.
    Rate(u8),
    /// Keep, and move on. Advancing is the point — this is the fast pass.
    Pick,
    /// Discard, and move on.
    Reject,
    /// Undecide. Stays put, because it is a correction like a rating.
    ClearFlag,
    Colour(String),
    ClearColour,
    /// Put back what the last judgement replaced, and go to that frame.
    Undo,
    /// Set this frame aside to compare, or take it back out.
    Mark,
    /// Move within the marked set rather than through the whole shoot.
    SelectMarked(i32),
    /// Judge the frame under the cursor **and drop it from the comparison**, so
    /// the field narrows as you eliminate. `true` keeps it, `false` discards it.
    SurveyJudge(bool),
    /// Empty the comparison.
    ClearMarks,
    /// Take this frame's look, to give to the marked ones.
    CopyEdit,
    /// Give the copied look to every marked frame.
    PasteEdit,
    /// Draw a rectangle on the loupe. Pressed again, it stops.
    Crop,
    /// Put markers on the loupe and let a press place one. Pressed again, it
    /// stops.
    Spot,
    /// Take the rectangle that was drawn, or throw it away.
    CropApply,
    CropCancel,
    /// Walk a collection instead of the whole library; `None` goes back to all
    /// of it. The filter still applies *within* it — see `Library::read`.
    ShowCollection(Option<i64>),
    /// Put this frame in the target collection, or take it out. The same key
    /// either way, like [`CullAction::Mark`]: a toggle is what a one-key gesture
    /// on a single frame can honestly be.
    ///
    /// The target is the quick collection until somebody aims it elsewhere, and
    /// that is the whole of "add this frame to Portfolio" — no second key and no
    /// menu, the one gesture pointed somewhere else.
    TargetToggle,
    /// Put every marked frame in the target, or this one if none are.
    AddMarked,
    /// Take this frame out of the collection being viewed. Refused outside one:
    /// the library is not something a photograph is taken out of with a key.
    TakeOut,
    /// Aim the add-to-collection key at a collection. Remembered by the catalog.
    SetTarget(i64),
    /// Take every photograph out of a collection and leave it standing — what
    /// "start again" is for the quick collection, which cannot be deleted.
    EmptyCollection(i64),
    RenameCollection {
        id: i64,
        name: String,
    },
    /// Delete a collection and everything nested in it. Z brings it back whole.
    DeleteCollection(i64),
    /// Make a collection holding the frames set aside to compare, or this one if
    /// none are. Named, because a collection nobody named is one nobody can find
    /// again.
    NewCollection(String),
    /// Move this frame earlier or later *within* the collection being viewed.
    ///
    /// The hand-made order is the only thing a collection has that a filter
    /// never will, so there has to be a way to make one. Refused outside a
    /// collection, where the order belongs to the library.
    MoveInCollection(i32),
    /// Look at a narrower part of the library. The whole filter at once, for the
    /// same reason a judgement is written whole: the page holds the controls and
    /// sends what they now say, rather than the shell keeping a second copy that
    /// could disagree with them.
    SetFilter(Filter),
    /// A second interpretation of this photograph, carrying the edit that is on
    /// screen. Resolved in the command handler, which is the only place that can
    /// see the session.
    MakeCopy,
    /// Throw away the copy under the cursor. Refused on a photograph that is not
    /// one.
    RemoveCopy,
    /// Move the selection without loading anything — what a grid does. The
    /// loupe uses `Next`/`Previous`, which ask for the photograph as well.
    SelectNext,
    SelectPrevious,
    /// By a whole row, resolved against the grid's current column count.
    SelectBy(i32),
    /// Which view to show.
    Grid,
    Loupe,
    Survey,
    /// Larger or smaller cells, in steps.
    Cells(i32),
}

/// What the page draws after a keypress.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CullView {
    pub filename: String,
    /// Which interpretation is on screen — `None` for the photograph itself.
    /// Beside `filename` rather than folded into it, because the page shows the
    /// two differently and an export names them differently.
    pub copy: Option<String>,
    /// One-based, because it is shown to a person.
    pub position: usize,
    pub total: usize,
    pub rating: Option<u8>,
    pub flag: Option<&'static str>,
    pub colour: Option<String>,
    pub picks: usize,
    pub rejects: usize,
    pub undoable: bool,
    /// Whether a look is on the clipboard. Shown, because a clipboard nobody can
    /// see is a key that sometimes does nothing for no visible reason.
    pub copied: bool,
    /// How many frames are set aside to compare.
    pub marked: usize,
    /// Whether this one is among them.
    pub is_marked: bool,
    /// Which view is showing. Here because the *canvas* can change it — a
    /// double-click on a cell opens the loupe — and the page would otherwise go
    /// on claiming the grid was up.
    pub mode: &'static str,
    /// What is being looked at. Sent back rather than assumed, because the shell
    /// can change it without being asked: a filter that would leave nothing on
    /// screen is turned off, and the controls have to follow.
    pub filter: Filter,
    /// Every collection, with its count, so the page can draw the list without
    /// asking a second time.
    pub collections: Vec<Collection>,
    /// Which one is being walked through, and `None` for the whole library.
    pub viewing: Option<i64>,
    /// Whether this frame is in the target collection, so the key that toggles
    /// it can say which way it will go.
    pub in_target: bool,
    /// How many photographs there are altogether, against `total`'s "how many
    /// the filter admits". Both, because "12 of 47" is the only honest way to
    /// show a narrowed library — a bare count reads as a library that lost
    /// something.
    pub in_library: usize,
}

/// An open catalog and where we are in it.
pub struct Library {
    catalog: Catalog,
    /// What is being walked through: where the photographs come from, which of
    /// them the filter shows, and the photographs themselves — as one value,
    /// because they were three fields once and a keypress could change one
    /// without the others. See [`crate::sequence`]. Never empty.
    sequence: Sequence,
    /// The slot in `sequence` the cursor is on.
    index: usize,
    /// Whether every action checks the sequence against a full read of the
    /// catalog. On in tests, which is where a shortcut that drifted would
    /// otherwise pass; the scale test turns it off, since the full read is the
    /// cost that test exists to show is no longer being paid.
    #[cfg(test)]
    oracle: bool,
    /// How many photographs there are, and how many are picked and rejected.
    ///
    /// **Held, because it was a scan of the whole library per keypress.** The
    /// page shows these after every key, and asking the catalog each time is a
    /// count over every image there is: 1.6 ms at twenty thousand and linear,
    /// for three numbers of which at most one has moved by one. Once the
    /// sequence stopped re-reading itself this was all that was left of a
    /// keypress's cost — every action measured the same, because every action
    /// was mostly this.
    ///
    /// Changed in one place, [`Library::write_judgement`], which is the only
    /// thing that writes a flag; recounted where photographs are added or
    /// removed. The tests check it against the catalog after every action.
    tally: (usize, usize, usize),
    /// Every collection and its count, as of the last time one changed.
    ///
    /// **Held, because the page is sent it after every keypress.** Asking the
    /// catalog each time is a count over every membership there is — its cost
    /// is set by how many collections somebody has made over the years, not by
    /// anything on screen, and on a used library that measured 4.9 ms a key for
    /// an answer that had not changed since the last one. Collections only
    /// change through [`Library::act`], so that is where this is refreshed.
    collections: Vec<Collection>,
    /// Where K puts a photograph. Held because `view` asks "is this frame in
    /// it" after every keypress; changed only by [`CullAction::SetTarget`], by a
    /// delete that takes the target with it, and by the undo of one.
    target: i64,
    /// What each judgement replaced, most recent last.
    ///
    /// Bounded because it is a convenience, not a history: the versioned record
    /// is what `edit_states` is for, and a rating deliberately has none.
    undo: Vec<Undone>,
    /// A navigation the render loop has not acted on yet.
    request: Option<usize>,
    /// Frames set aside to compare against each other, by catalog id, kept in
    /// shoot order so a survey reads left to right the way the day did.
    ///
    /// Ids rather than positions for the reason [`Undone`] uses them: a filter
    /// renumbers the sequence, and a comparison built out of positions would
    /// quietly become a comparison of different photographs. A marked frame the
    /// filter no longer admits stays marked and is simply not shown — narrowing
    /// the view is not a decision about the comparison.
    marked: Vec<i64>,
    /// The look taken from a frame, waiting to be applied to the marked ones.
    ///
    /// Held rather than re-read from the source frame, so navigating away — or
    /// editing the source further — does not change what gets pasted. A
    /// clipboard that quietly follows its source is not a clipboard.
    copied: Option<EditState>,
    /// Set by the paste key, drained by the render loop.
    ///
    /// Not done here, because the frame on screen may have unsaved slider
    /// movements: writing to the catalog under it and letting the pending save
    /// land afterwards would put the old edit back, and the paste would look
    /// like it had silently skipped one frame.
    paste_requested: bool,
}

/// How many judgements can be taken back. Enough to cover a mis-keyed run
/// through a burst, short enough to stay a keypress rather than a browser.
const UNDO_DEPTH: usize = 64;

impl Library {
    /// Open a catalog and stand at the first photograph in it.
    pub fn open(path: &Path) -> Result<Self> {
        let catalog = Catalog::open(path)?;
        let sequence =
            Sequence::read(&catalog, Source::Library, Filter::default())?.ok_or_else(|| {
                anyhow!(
                    "{} has no images; run `rawkit catalog <path> --scan <folder>` first",
                    path.display()
                )
            })?;
        eprintln!(
            "library    : {} · {} image(s)",
            path.display(),
            sequence.len()
        );
        let listed = collections::all(&catalog)?;
        let target = collections::target(&catalog)?;
        let tally = cull::tally(&catalog)?;
        Ok(Self {
            catalog,
            sequence,
            index: 0,
            #[cfg(test)]
            oracle: true,
            tally,
            collections: listed,
            target,
            undo: Vec::new(),
            copied: None,
            paste_requested: false,
            request: None,
            marked: Vec::new(),
        })
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// How big the current photograph is, without decoding it.
    ///
    /// A header parse — under a millisecond — which is what lets a viewport be
    /// set up for a frame whose pixels are going to come from a preview.
    pub fn size_of_current(&self) -> Result<([u32; 2], rawkit_editstate::Orientation)> {
        let path = &self.current().path;
        let meta = rawkit_decode::read_metadata(Path::new(path))
            .with_context(|| format!("reading {path}"))?;
        // The orientation travels with the size because the two are one answer:
        // a viewport built from the sensor's dimensions alone is the wrong shape
        // for every portrait frame, and would correct itself only once the
        // decode landed — a visible flip a moment after the photograph opens.
        Ok(([meta.width, meta.height], meta.orientation))
    }

    /// A rendered copy big enough for what the view is about to show, if there
    /// is one and it is current.
    ///
    /// `needed` is the longest edge in image pixels the view can resolve. Returns
    /// `None` when nothing on disk is large enough — the caller then decodes,
    /// which is the slow path this exists to avoid but is still the right answer
    /// when someone zooms in.
    pub fn preview_for(&self, needed: u32, edit_state_hash: &str) -> Result<Option<Decoded>> {
        self.preview_at(self.index, needed, Some(edit_state_hash))
    }

    /// The frames set aside for comparison, in shoot order.
    /// Take a look, to give to the marked frames.
    pub fn copy_edit(&mut self, state: EditState) {
        self.copied = Some(state);
    }

    /// Whether a paste is waiting, clearing the flag.
    pub fn take_paste(&mut self) -> bool {
        std::mem::take(&mut self.paste_requested)
    }

    /// Give the copied look to every marked frame.
    ///
    /// **Tone and white balance travel; orientation and crop stay.** A pasted
    /// crop silently reframes photographs whose composition differs, and since
    /// previews rebuild on their own the damage is not noticed until the exports
    /// come out wrong. A locked-off sequence that genuinely wants one crop is a
    /// separate command, not a default.
    ///
    /// Returns how many frames changed. Ones already carrying this look are not
    /// counted and cost nothing: `edits::save` is a no-op when the hash matches.
    pub fn paste_into_marked(&mut self) -> Result<usize> {
        let Some(look) = self.copied.clone() else {
            return Err(anyhow!("nothing copied — press S on a frame first"));
        };
        if self.marked.is_empty() {
            return Err(anyhow!("mark the frames to apply it to with M first"));
        }

        let mut undo = Vec::new();
        let mut changed = 0;
        for id in self.marked.clone() {
            let before = rawkit_catalog::edits::latest(&self.catalog, id)?.map(|(_, state)| state);
            let merged = EditState {
                tone: look.tone,
                white_balance: look.white_balance,
                ..before.clone().unwrap_or_default()
            };
            // Nothing to do covers two cases that look different and are not:
            // the frame already carries this look, and the look is the identity
            // on a frame that had no edit. Writing either marks a photograph as
            // edited by a decision nobody made.
            if merged == before.clone().unwrap_or_default() {
                continue;
            }
            if rawkit_catalog::edits::save(
                &self.catalog,
                id,
                &merged,
                rawkit_editstate::EditSource::User,
            )?
            .is_some()
            {
                undo.push((id, before));
                changed += 1;
            }
        }
        if !undo.is_empty() {
            self.undo.push(Undone::Pasted { frames: undo });
            if self.undo.len() > UNDO_DEPTH {
                self.undo.remove(0);
            }
        }
        Ok(changed)
    }

    /// The frames set aside, as positions in what is on screen, in shoot order.
    ///
    /// Computed rather than stored. A mark names a photograph and a position
    /// names a slot, and the filter decides which slot a photograph is in — so
    /// the only way for the two to stay in step is for one of them to be
    /// derived. Marked frames the filter excludes are absent here and still
    /// marked: narrowing the view is not a decision about the comparison.
    pub fn marked(&self) -> Vec<usize> {
        let mut positions: Vec<usize> = self
            .marked
            .iter()
            .filter_map(|id| self.position_of(*id))
            .collect();
        // Sorted here rather than kept sorted, because ids run in the order the
        // files were scanned and the sequence runs in the order the shutter
        // fired. Those are usually the same and are not the same thing.
        positions.sort_unstable();
        positions
    }

    /// Where a photograph sits in the sequence, if the filter admits it.
    fn position_of(&self, id: i64) -> Option<usize> {
        self.sequence.position_of(id)
    }

    /// Which part of the library is on screen — what an export of "what is
    /// shown" is an export of.
    pub fn filter(&self) -> &Filter {
        self.sequence.filter()
    }

    /// A second interpretation of the photograph under the cursor.
    ///
    /// `state` is what is on screen, not what the catalog last wrote: sliders
    /// that have moved and not yet settled into a version are part of the look
    /// somebody is forking, and a copy of the last saved moment is a copy of a
    /// moment nobody chose.
    ///
    /// The cursor lands on the copy, which is where anybody who just made one is
    /// looking. It sits immediately after its original — the sequence sorts by
    /// capture time, then filename, then id, and a copy shares the first two.
    ///
    /// Reported by name so the interface can say which one it made.
    pub fn add_copy(&mut self, state: &EditState) -> Result<String> {
        let source = self.current().id;
        let id = rawkit_catalog::copies::create(&self.catalog, source, None, state)?;
        // A copy starts undecided, so a filter on flag or rating will not have
        // it. Rather than making something the user cannot see, the filter comes
        // off — the same rule as a filter that empties, and for the same reason:
        // the view has to be able to explain itself, and the chips follow it.
        self.resequence(Some(id))?;
        // A copy is a photograph: the library is one larger.
        self.tally = cull::tally(&self.catalog)?;
        let name = self
            .position_of(id)
            .and_then(|at| self.sequence.get(at)?.copy_name.clone())
            .unwrap_or_default();
        Ok(name)
    }

    /// Throw away the copy under the cursor.
    ///
    /// Its edit history, snapshots and preview rows go with it; the preview
    /// files are left for the sweep, which is where every other orphan is dealt
    /// with. Refused on a photograph that is not a copy — see
    /// [`rawkit_catalog::copies::delete`] for what deleting that would cost.
    pub fn remove_copy(&mut self) -> Result<String> {
        let image = self.current().clone();
        rawkit_catalog::copies::delete(&self.catalog, image.id)?;
        // Anything still naming it would act on a row that is gone: a mark would
        // show an empty cell, and an undo would try to put a judgement back on
        // nothing.
        self.marked.retain(|marked| *marked != image.id);
        self.undo.retain(|undone| match undone {
            Undone::Judged { image: id, .. } => *id != image.id,
            Undone::Pasted { frames } => !frames.iter().any(|(id, _)| *id == image.id),
            // These are safe to keep. Putting a deleted photograph back is
            // skipped by the catalog, and taking one out that is not there is
            // nothing — neither can act on a row that is gone.
            Undone::TakenOut { .. } | Undone::Added { .. } | Undone::Deleted(_) => true,
        });
        self.resequence(None)?;
        // Deleting an image takes it out of every collection it was in, and the
        // held list is a *copy* of those counts. This was the mutation that did
        // not know the copy existed: three arms of `act` kept it fresh and this
        // one, which is not in `act` at all, left a chip reading one more than
        // the collection held.
        self.refresh_collections()?;
        // And one smaller, possibly by a pick or a reject as well.
        self.tally = cull::tally(&self.catalog)?;
        Ok(image.label())
    }

    /// Write a judgement, and move the held tally by what it changed.
    ///
    /// **The only thing in this file that writes a flag**, so that the count
    /// and the catalog cannot be changed apart. A judgement and the undo of one
    /// both come through here, which is every way a flag moves.
    fn write_judgement(&mut self, id: i64, before: &Judgement, after: &Judgement) -> Result<()> {
        cull::set(&self.catalog, id, after)?;
        let (_, picks, rejects) = &mut self.tally;
        for (flag, step) in [(before.flag, -1i64), (after.flag, 1)] {
            let count = match flag {
                Some(Flag::Pick) => &mut *picks,
                Some(Flag::Reject) => &mut *rejects,
                None => continue,
            };
            *count = (*count as i64 + step).max(0) as usize;
        }
        Ok(())
    }

    /// The photographs an action is about: what is marked, or what is under the
    /// cursor when nothing is. The same rule paste follows, so every key agrees
    /// about what "these photographs" means.
    fn chosen(&self) -> Vec<i64> {
        if self.marked.is_empty() {
            vec![self.current().id]
        } else {
            self.marked.clone()
        }
    }

    /// Keep something that can be taken back, and forget the oldest.
    fn remember(&mut self, undone: Undone) {
        self.undo.push(undone);
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
    }

    /// Put photographs in a collection, remembering only the ones that were not
    /// already there — so taking this back never removes a frame somebody had
    /// added before.
    fn add_to(&mut self, collection: i64, images: &[i64]) -> Result<()> {
        let mut fresh = Vec::new();
        for image in images {
            if !collections::holds(&self.catalog, collection, *image)? {
                fresh.push(*image);
            }
        }
        if fresh.is_empty() {
            return Ok(());
        }
        collections::add(&self.catalog, collection, &fresh)?;
        self.remember(Undone::Added {
            collection,
            images: fresh,
        });
        let standing = self.current().id;
        self.after_membership_changed(collection, Some(standing))
    }

    /// Take photographs out of a collection, remembering where each was.
    fn take_out_of(&mut self, collection: i64, images: &[i64]) -> Result<()> {
        let placed = collections::take_out(&self.catalog, collection, images)?;
        if placed.is_empty() {
            return Ok(());
        }
        self.remember(Undone::TakenOut { collection, placed });
        self.after_membership_changed(collection, None)
    }

    /// What every change to a collection's members has to be followed by: the
    /// held counts, and the view if it is *of* that collection. One place, so a
    /// new way of changing membership cannot forget half of it.
    ///
    /// `stand_on` is the photograph the cursor should end up on, **and it must
    /// be one the change left in the collection.** `resequence` reads "the frame
    /// I asked for is not showing" as a reason to widen the view, which is right
    /// for a copy a filter would hide and exactly wrong here: handed the frame
    /// that had just been taken out, it widened all the way back to the library,
    /// and taking one photograph out of a collection threw you out of it. `None`
    /// keeps the slot, which after a removal holds whatever came next.
    fn after_membership_changed(&mut self, collection: i64, stand_on: Option<i64>) -> Result<()> {
        self.refresh_collections()?;
        if self.sequence.source() == Source::Collection(collection) {
            self.resequence(stand_on)?;
        }
        Ok(())
    }

    /// Ask the catalog for the collection list again.
    ///
    /// **One way in**, because the list is held rather than asked for per
    /// keypress and a held copy is only as good as the discipline that refreshes
    /// it. Anything that can change which photographs a collection holds calls
    /// this; the one exception is K, which knows the count moved by exactly one
    /// and says so rather than recounting the catalog.
    fn refresh_collections(&mut self) -> Result<()> {
        self.collections = collections::all(&self.catalog)?;
        Ok(())
    }

    /// Walk a collection instead of the whole library, or `None` to go back.
    ///
    /// Refuses an empty one for the reason [`Library::narrow`] refuses a filter
    /// that matches nothing: the sequence is never empty, so `current` always
    /// names a photograph, and "this collection is empty" is a message rather
    /// than a view.
    ///
    /// The new view is built before anything is assigned, so a refusal leaves
    /// the library exactly where it was. This used to write `viewing`, read, and
    /// put `viewing` back by hand on each of three ways out.
    pub fn show_collection(&mut self, id: Option<i64>) -> Result<()> {
        let source = id.map_or(Source::Library, Source::Collection);
        let filter = self.sequence.filter().clone();
        let next = match Sequence::read(&self.catalog, source, filter.clone())? {
            Some(next) => Some(next),
            // A collection with nothing in it that the filter admits is worth
            // showing *whole* rather than refusing outright — the filter is a
            // view of the collection, and dropping it is the smaller surprise.
            None if !filter.is_everything() => {
                Sequence::read(&self.catalog, source, Filter::default())?
            }
            None => None,
        };
        let next = next.ok_or_else(|| anyhow!("that collection is empty"))?;
        // Stay on the same photograph when it is in both views, which is what
        // switching to a collection the current frame is already in should do.
        let standing = self.current().id;
        self.index = next.position_of(standing).unwrap_or(0);
        self.sequence = next;
        self.request = Some(self.index);
        Ok(())
    }

    /// Re-read the sequence after the library itself changed, and stand
    /// somewhere sensible.
    ///
    /// Keeps the filter only while it has something to show *and*, when a
    /// particular photograph is what the action was about, while it can show
    /// that one. Everything else about a filter is the user's decision; this is
    /// the case where honouring it would mean acting and showing nothing.
    ///
    /// Widening in order — the filter goes first, then the collection — and the
    /// first view that has anything in it is the one assigned. A collection
    /// somebody has just emptied cannot be honoured at the cost of an empty
    /// sequence.
    fn resequence(&mut self, prefer: Option<i64>) -> Result<()> {
        let source = self.sequence.source();
        let filter = self.sequence.filter().clone();
        let mut next = Sequence::read(&self.catalog, source, filter.clone())?
            .filter(|next| prefer.is_none_or(|id| next.position_of(id).is_some()));
        if next.is_none() && !filter.is_everything() {
            next = Sequence::read(&self.catalog, source, Filter::default())?;
        }
        if next.is_none() && source != Source::Library {
            next = Sequence::read(&self.catalog, Source::Library, Filter::default())?;
        }
        let next = next.ok_or_else(|| anyhow!("the library has no photographs left in it"))?;
        self.index = prefer
            .and_then(|id| next.position_of(id))
            .unwrap_or_else(|| self.index.min(next.len() - 1));
        self.sequence = next;
        self.request = Some(self.index);
        Ok(())
    }

    /// Look at a narrower part of the library.
    ///
    /// The photograph under the cursor is kept when the new filter admits it,
    /// and otherwise the cursor moves to the first frame *after* it that the
    /// filter does admit. Not to the start: where you had got to in a shoot is
    /// worth more than the tidiness of beginning again, and a filter is usually
    /// set in the middle of a pass rather than before one.
    ///
    /// A filter nothing matches is refused, and that is the rule the rest of
    /// this file leans on: the sequence is never empty, so `current` always
    /// names a photograph. "No photographs" is a message, not a view — a window
    /// showing nothing cannot say why it is showing nothing.
    pub fn narrow(&mut self, filter: Filter) -> Result<()> {
        let standing = self.current().id;
        let Some(index) = self.sequence.narrow(&self.catalog, filter, self.index)? else {
            return Err(anyhow!("no photograph matches that filter"));
        };
        self.index = index;
        if self.current().id != standing {
            self.request = Some(index);
        }
        Ok(())
    }

    pub fn count(&self) -> usize {
        self.sequence.len()
    }

    pub fn index(&self) -> usize {
        self.index
    }

    /// Put the selection somewhere without asking the render loop to load it.
    ///
    /// What a grid does: moving across a contact sheet must not decode anything,
    /// because the cell it lands on is already on screen.
    pub fn select(&mut self, index: usize) {
        self.index = index.min(self.sequence.len() - 1);
    }

    /// Ask for the current photograph to be loaded even though the selection did
    /// not move. Leaving the grid needs this: the loupe has been showing
    /// something else since the last time it ran.
    pub fn reopen(&mut self) {
        self.request = Some(self.index);
    }

    /// The flag on each image in a range of the sequence, for tinting cells.
    ///
    /// One query for the whole visible page rather than one per cell, because a
    /// grid re-reads this every frame — pressing X has to change what the cell
    /// looks like straight away.
    #[allow(clippy::type_complexity)]
    pub fn flags_in(&self, from: usize, to: usize) -> Result<Vec<(Option<Flag>, Option<String>)>> {
        let slice: Vec<&LibraryImage> = self.sequence.slice(from, to).collect();
        if slice.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = slice.iter().map(|i| i.id.to_string()).collect();
        let mut statement = self.catalog.connection().prepare(&format!(
            "SELECT id, flag, colour_label FROM images WHERE id IN ({})",
            ids.join(",")
        ))?;
        #[allow(clippy::type_complexity)]
        let found: std::collections::HashMap<i64, (Option<String>, Option<String>)> = statement
            .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(slice
            .iter()
            .map(|image| {
                let (flag, label) = found.get(&image.id).cloned().unwrap_or((None, None));
                let flag = match flag.as_deref() {
                    Some("pick") => Some(Flag::Pick),
                    Some("reject") => Some(Flag::Reject),
                    _ => None,
                };
                (flag, label)
            })
            .collect())
    }

    /// A preview for any image in the sequence, resolving its edit itself.
    ///
    /// The grid cannot be handed a hash the way the loupe can: it draws thirty
    /// photographs at once and each has its own edit.
    pub fn preview_at(
        &self,
        index: usize,
        needed: u32,
        hash: Option<&str>,
    ) -> Result<Option<Decoded>> {
        let Some(image) = self.sequence.get(index) else {
            return Ok(None);
        };
        let resolved;
        let hash = match hash {
            Some(hash) => hash,
            None => {
                resolved = rawkit_catalog::edits::latest(&self.catalog, image.id)?
                    .map(|(_, state)| state)
                    .unwrap_or_default()
                    .content_hash();
                &resolved
            }
        };
        let Some(dir) = previews::directory(&self.catalog) else {
            return Ok(None);
        };
        let Some(found) = previews::covering(
            &self.catalog,
            image.id,
            needed,
            hash,
            &rawkit_engine::renderer_version(&rawkit_decode::decoder_version()),
        )?
        else {
            return Ok(None);
        };
        let file = dir.join(&found.path);
        // A preview the catalog knows about and the disk does not is an ordinary
        // situation — someone tidied the directory — and the answer is to render
        // rather than to fail.
        let Ok(bytes) = std::fs::read(&file) else {
            return Ok(None);
        };
        let (rgba, width, height) = rawkit_export::decode(&bytes)
            .with_context(|| format!("reading the preview at {}", file.display()))?;
        Ok(Some(Decoded {
            rgba,
            width,
            height,
        }))
    }

    pub fn current(&self) -> &LibraryImage {
        // The sequence is never empty and every write to `index` clamps to it,
        // so there is always a photograph here. The fallback is the first one
        // rather than a panic, because a window that shows the wrong frame can
        // be corrected and one that has crashed cannot.
        self.sequence
            .get(self.index)
            .or_else(|| self.sequence.get(0))
            .expect("a sequence is never empty")
    }

    /// Carry out an action and report the new state.
    pub fn act(&mut self, action: CullAction) -> Result<CullView> {
        match action {
            // Resolved before they reach here — they change which view is
            // showing, not which photograph. Listed rather than caught by a
            // wildcard so adding a cull action still fails to compile here,
            // which is what has kept this match honest.
            CullAction::Crop | CullAction::CropApply | CullAction::CropCancel => {}
            CullAction::SetFilter(filter) => self.narrow(filter)?,
            CullAction::ShowCollection(id) => self.show_collection(id)?,
            CullAction::TargetToggle => {
                let (target, id) = (self.target, self.current().id);
                if collections::holds(&self.catalog, target, id)? {
                    self.take_out_of(target, &[id])?;
                } else {
                    self.add_to(target, &[id])?;
                }
            }
            CullAction::AddMarked => {
                let chosen = self.chosen();
                self.add_to(self.target, &chosen)?;
            }
            CullAction::TakeOut => {
                let Source::Collection(viewing) = self.sequence.source() else {
                    return Err(anyhow!(
                        "taking a photograph out is something done to a collection; \
                         this is the whole library"
                    ));
                };
                let id = self.current().id;
                self.take_out_of(viewing, &[id])?;
            }
            CullAction::SetTarget(id) => {
                collections::set_target(&self.catalog, id)?;
                self.target = id;
                for held in &mut self.collections {
                    held.is_target = held.id == id;
                }
            }
            CullAction::EmptyCollection(id) => {
                let placed = collections::clear(&self.catalog, id)?;
                if !placed.is_empty() {
                    self.remember(Undone::TakenOut {
                        collection: id,
                        placed,
                    });
                }
                self.after_membership_changed(id, None)?;
            }
            CullAction::RenameCollection { id, name } => {
                collections::rename(&self.catalog, id, &name)?;
                self.refresh_collections()?;
            }
            CullAction::DeleteCollection(id) => {
                let removed = collections::remove(&self.catalog, id)?;
                // The ids it had mean nothing now, and SQLite will give them to
                // the next collections made. An undo record still naming one
                // would put its photographs into a stranger.
                let gone: Vec<i64> = removed.ids().collect();
                self.undo.retain(|undone| match undone {
                    Undone::TakenOut { collection, .. } | Undone::Added { collection, .. } => {
                        !gone.contains(collection)
                    }
                    _ => true,
                });
                self.remember(Undone::Deleted(removed));
                // A delete can take the target with it, directly or nested.
                self.target = collections::target(&self.catalog)?;
                self.refresh_collections()?;
                if let Source::Collection(viewing) = self.sequence.source() {
                    if gone.contains(&viewing) {
                        let standing = self.current().id;
                        self.resequence(Some(standing))?;
                    }
                }
            }
            CullAction::NewCollection(name) => {
                // What is marked, or what is under the cursor — the same rule
                // paste follows, so the two keys agree about what "these
                // photographs" means.
                let chosen = self.chosen();
                // With its photographs or not at all: as a create followed by
                // an add, a failure between them left an empty collection
                // carrying a name nobody chose to make empty.
                collections::create_holding(&self.catalog, &name, None, &chosen)?;
                self.refresh_collections()?;
            }
            CullAction::MoveInCollection(step) => {
                let Source::Collection(id) = self.sequence.source() else {
                    return Err(anyhow!(
                        "moving a photograph changes a collection's order; the \
                         library's own order is the shoot's"
                    ));
                };
                let target = self.index as i64 + step as i64;
                if target >= 0 && (target as usize) < self.sequence.len() {
                    let target = target as usize;
                    // Two rows in the catalog and two entries here. This first
                    // handed the whole sequence to `reorder` and then read the
                    // collection back — a write per member followed by a read
                    // of all of them, 90 ms at twenty thousand, to exchange two
                    // neighbours. The sequence in hand is already the answer.
                    let moved = self.current().id;
                    let past = self.sequence.get(target).map(|image| image.id);
                    if let Some(past) = past {
                        collections::swap(&self.catalog, id, moved, past)?;
                        self.sequence.swap(self.index, target);
                        self.index = target;
                    }
                }
            }
            // Both are resolved before they arrive: making one needs the
            // session's edit, which only the command handler can see.
            CullAction::MakeCopy | CullAction::RemoveCopy => {}
            CullAction::Spot => {}
            // The clipboard is filled in the command handler, which is the only
            // place that can see the session — this frame's edit is what is on
            // screen, not what was last written to the catalog.
            CullAction::CopyEdit => {}
            CullAction::PasteEdit => self.paste_requested = true,
            CullAction::Next => self.go(self.index + 1),
            CullAction::SelectNext => self.select(self.index + 1),
            CullAction::SelectPrevious => self.select(self.index.saturating_sub(1)),
            CullAction::SelectBy(step) => {
                let target = self.index as i64 + step as i64;
                self.select(target.clamp(0, self.sequence.len() as i64 - 1) as usize);
            }
            // Saturating, not wrapping: backing off the front of a shoot must
            // stay at the first frame, and `wrapping_sub` would clamp to the
            // *last* one — a silent jump to the far end of the library.
            CullAction::Previous => self.go(self.index.saturating_sub(1)),
            CullAction::Rate(stars) => {
                let rating = (stars > 0).then_some(stars);
                self.judge(|j| Judgement { rating, ..j })?;
            }
            // A frame that leaves the filter has already carried the cursor
            // forward — everything after it moved up a place — so advancing as
            // well would step over its neighbour. That is the one thing a
            // filtered pass must not do: skip a photograph silently.
            CullAction::Pick => {
                let dropped = self.judge(|j| Judgement {
                    flag: Some(Flag::Pick),
                    ..j
                })?;
                if !dropped {
                    self.go(self.index + 1);
                }
            }
            CullAction::Reject => {
                let dropped = self.judge(|j| Judgement {
                    flag: Some(Flag::Reject),
                    ..j
                })?;
                if !dropped {
                    self.go(self.index + 1);
                }
            }
            CullAction::ClearFlag => {
                self.judge(|j| Judgement { flag: None, ..j })?;
            }
            CullAction::Colour(name) => {
                // Pressing the same label again clears it, the way Lightroom's
                // colour keys behave. Without that there is no key for "I was
                // wrong about this one" except reaching for another.
                self.judge(|j| {
                    let colour = if j.colour.as_deref() == Some(name.as_str()) {
                        None
                    } else {
                        Some(name.clone())
                    };
                    Judgement { colour, ..j }
                })?;
            }
            CullAction::ClearColour => {
                self.judge(|j| Judgement { colour: None, ..j })?;
            }
            // Handled by the shell, which owns the layout and the render loop.
            // Listed here so the page has one vocabulary rather than two.
            CullAction::Grid | CullAction::Loupe | CullAction::Survey | CullAction::Cells(_) => {}
            CullAction::Mark => {
                let id = self.current().id;
                match self.marked.iter().position(|marked| *marked == id) {
                    Some(at) => {
                        self.marked.remove(at);
                    }
                    None => self.marked.push(id),
                }
            }
            CullAction::ClearMarks => self.marked.clear(),
            CullAction::SelectMarked(step) => {
                let shown = self.marked();
                if !shown.is_empty() {
                    let at = shown.iter().position(|i| *i == self.index).unwrap_or(0) as i64;
                    let next = (at + step as i64).rem_euclid(shown.len() as i64);
                    self.index = shown[next as usize];
                }
            }
            CullAction::SurveyJudge(keep) => {
                let id = self.current().id;
                let flag = if keep { Flag::Pick } else { Flag::Reject };
                self.judge(|j| Judgement {
                    flag: Some(flag),
                    ..j
                })?;
                // Out of the comparison, and the cursor lands on whatever is
                // still in it — which is what makes this a winnowing rather
                // than a survey you have to leave and re-enter.
                if let Some(at) = self.marked.iter().position(|marked| *marked == id) {
                    self.marked.remove(at);
                    let shown = self.marked();
                    // The nearest one still being compared, forward first. The
                    // judged frame may also have left the filter, in which case
                    // the positions moved under this — which is why it asks
                    // where the marks are now rather than where they were.
                    if let Some(next) = shown
                        .iter()
                        .find(|position| **position >= self.index)
                        .or_else(|| shown.last())
                    {
                        self.index = *next;
                    }
                }
            }
            CullAction::Undo => match self.undo.pop() {
                Some(Undone::Pasted { frames }) => {
                    for (id, before) in &frames {
                        let state = before.clone().unwrap_or_default();
                        rawkit_catalog::edits::save(
                            &self.catalog,
                            *id,
                            &state,
                            rawkit_editstate::EditSource::User,
                        )?;
                    }
                    // Back to a frame it touched, so the reversal is visible
                    // rather than something the user has to go and check.
                    if let Some(index) = frames.first().and_then(|(id, _)| self.position_of(*id)) {
                        self.go(index);
                        self.index = index;
                    }
                }
                Some(Undone::Judged {
                    image,
                    before: previous,
                }) => {
                    let standing = cull::judgement(&self.catalog, image)?;
                    self.write_judgement(image, &standing, &previous)?;
                    // Putting the judgement back can put the *frame* back: a
                    // filtered pass drops what it judges, and an undo that could
                    // not return you to the photograph you were wrong about
                    // would be no use at all.
                    // Asked about the one frame whose judgement changed, in
                    // both directions: it may be coming back into a filtered
                    // view, or — if the filter was changed since — leaving one.
                    // This was a full re-read of the sequence, and the one of
                    // six that forgot which collection it was in.
                    self.settle(image)?;
                    // A survey drops what it judges too, so the same keypress
                    // has to restore the comparison — otherwise the key that
                    // reverses a mistake leaves you looking at a comparison the
                    // mistake is missing from.
                    if !self.marked.is_empty() && !self.marked.contains(&image) {
                        self.marked.push(image);
                    }
                    if let Some(index) = self.position_of(image) {
                        self.go(index);
                        self.index = index;
                    } else {
                        // Only reachable if the frame went missing from disk
                        // between the judgement and the undo. The judgement is
                        // restored either way; the cursor stays where it can be.
                        self.index = self.index.min(self.sequence.len() - 1);
                    }
                }
                Some(Undone::TakenOut { collection, placed }) => {
                    collections::put_back(&self.catalog, collection, &placed)?;
                    // Standing on a frame that came back, when the view is of
                    // that collection, so the reversal is seen rather than
                    // trusted. It is in the collection again, so asking for it
                    // cannot widen the view.
                    let back = placed.first().map(|p| p.image);
                    self.after_membership_changed(collection, back)?;
                }
                Some(Undone::Added { collection, images }) => {
                    collections::take_out(&self.catalog, collection, &images)?;
                    self.after_membership_changed(collection, None)?;
                }
                Some(Undone::Deleted(removed)) => {
                    collections::restore(&self.catalog, &removed)?;
                    // It may have been the target, and comes back as one.
                    self.target = collections::target(&self.catalog)?;
                    self.refresh_collections()?;
                }
                None => {}
            },
        }
        // Every shortcut the sequence takes is a claim that it ends where a full
        // read of the catalog would. In tests, hold it to that after each action.
        #[cfg(test)]
        if self.oracle {
            self.sequence.agrees_with(&self.catalog)?;
            let listed = collections::all(&self.catalog)?;
            anyhow::ensure!(
                listed == self.collections,
                "the held collection list has drifted from the catalog's"
            );
            anyhow::ensure!(
                self.target == collections::target(&self.catalog)?,
                "the held target has drifted from the catalog's"
            );
            let counted = cull::tally(&self.catalog)?;
            anyhow::ensure!(
                counted == self.tally,
                "the held tally {:?} has drifted from the catalog's {counted:?}",
                self.tally
            );
        }
        self.view()
    }

    /// Apply a change to the current image's judgement, remembering what it
    /// replaced.
    ///
    /// Reports whether the judgement pushed the frame out of the filter, which
    /// is the one thing its callers need to know: a frame that left has already
    /// moved the cursor on.
    fn judge(&mut self, change: impl FnOnce(Judgement) -> Judgement) -> Result<bool> {
        let id = self.current().id;
        let before = cull::judgement(&self.catalog, id)?;
        let after = change(before.clone());
        if after == before {
            return Ok(false);
        }
        self.write_judgement(id, &before, &after)?;
        self.undo.push(Undone::Judged { image: id, before });
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.settle(id)
    }

    /// Put the sequence and the cursor back in agreement after a judgement.
    ///
    /// Standing in "picks" and rejecting the frame in front of you means it has
    /// just left the set you are walking through. It goes, and the cursor stays
    /// on the number it was already on — which is now the frame that came next,
    /// so a filtered pass moves forward exactly the way an unfiltered one does
    /// and for the same reason.
    ///
    /// Asked of the catalog rather than worked out from the [`Judgement`] just
    /// written, so that "would this still be shown" is answered by the query
    /// that decides what *is* shown. A second opinion here is a second chance
    /// to disagree.
    fn settle(&mut self, judged: i64) -> Result<bool> {
        let filter = self.sequence.filter();
        if filter.is_everything() || cull::matches(&self.catalog, judged, filter)? {
            // Shown, or shown again: an undo can bring a frame *back* into a
            // filtered view, and that is this same question with the other
            // answer. A no-op when it never left.
            self.sequence.admit(judged);
            return Ok(false);
        }
        // **One frame's admission changed, so one frame goes.** This read the
        // whole sequence again — four joined tables and a path per photograph,
        // 20 ms at twenty thousand and linear — to learn what `matches` had
        // just said about the only photograph that could have moved.
        match self.sequence.drop_shown(judged) {
            Dropped::NotShown => return Ok(false),
            Dropped::Gone => {}
            Dropped::WouldEmpty => {
                // The end of a pass, and the ordinary way one ends: nothing is
                // unflagged any more, or the last pick has been taken back. The
                // filter goes rather than the window emptying — an empty
                // sequence is a state `current` cannot answer for, and a blank
                // canvas cannot explain itself. The view carries the filter
                // back to the page, so the controls follow rather than going on
                // claiming to be set.
                let index = self
                    .sequence
                    .narrow(&self.catalog, Filter::default(), self.index)?
                    .ok_or_else(|| anyhow!("the source of the frame just judged is empty"))?;
                self.index = self.position_of(judged).unwrap_or(index);
                return Ok(true);
            }
        }
        self.index = self.index.min(self.sequence.len() - 1);
        // Only when a different photograph is now under the cursor. Judging the
        // last frame of a filtered set leaves you standing on the one before
        // it, which is already on screen, and asking for it again would decode
        // a RAW to redraw what is already there.
        if self.current().id != judged {
            self.request = Some(self.index);
        }
        Ok(true)
    }

    /// Move to an image, clamped to the ends.
    ///
    /// Stopping at the last frame rather than wrapping: a cull is a pass through
    /// a shoot, and silently starting again is how a frame gets judged twice
    /// while its neighbour is missed.
    fn go(&mut self, index: usize) {
        let index = index.min(self.sequence.len() - 1);
        if index == self.index {
            return;
        }
        self.index = index;
        self.request = Some(index);
    }

    /// The photograph the render loop should now be showing, if it changed.
    pub fn take_request(&mut self) -> Option<String> {
        let index = self.request.take()?;
        self.sequence.get(index).map(|image| image.path.clone())
    }

    pub fn view(&self) -> Result<CullView> {
        let image = self.current();
        let judgement = cull::judgement(&self.catalog, image.id)?;
        // The tally counts the library, not the filter. It is the progress
        // indicator for a cull — how much of the shoot has been decided about —
        // and one that shrank as you narrowed the view would be measuring the
        // view instead of the work.
        let (in_library, picks, rejects) = self.tally;
        Ok(CullView {
            filename: image.filename.clone(),
            copy: image.copy_name.clone(),
            position: self.index + 1,
            total: self.sequence.len(),
            rating: judgement.rating,
            flag: judgement.flag.map(Flag::column),
            colour: judgement.colour,
            picks,
            rejects,
            undoable: !self.undo.is_empty(),
            copied: self.copied.is_some(),
            marked: self.marked().len(),
            is_marked: self.marked.contains(&image.id),
            mode: crate::mode_name(),
            filter: self.sequence.filter().clone(),
            collections: self.collections.clone(),
            viewing: match self.sequence.source() {
                Source::Collection(id) => Some(id),
                Source::Library => None,
            },
            // An indexed point lookup, 4 µs and flat at twenty thousand — the
            // one collection question that really is about this frame, and so
            // the one still asked per keypress.
            in_target: collections::holds(&self.catalog, self.target, image.id)?,
            in_library,
        })
    }
}

/// Writes an edit back to the catalog once it has stopped moving.
///
/// A drag emits commands far faster than anyone decides anything, so writing per
/// command would fill the history with one gesture. The catalog deduplicates
/// identical states, but the honest fix is not to ask it to.
pub struct Saver {
    library: Option<Arc<Mutex<Library>>>,
    session: Arc<Mutex<Session>>,
    /// Which photograph the session is holding an edit *for*.
    ///
    /// Captured when the edit was restored, and deliberately not looked up from
    /// the library at write time. The cursor moves the instant a key is pressed,
    /// while the flush that precedes a load happens a frame later — so asking the
    /// library "which image is this?" gives the answer for the frame being
    /// navigated *to*, and the edit lands on the wrong photograph. That is not
    /// hypothetical: it wrote one image's white balance onto the next one, and
    /// the only visible symptom was the second frame opening with the first
    /// frame's edit already applied, which looks like the feature working.
    image: Option<i64>,
    settled: Option<(u64, std::time::Instant)>,
    /// Where the session stood when this photograph opened. Opening one and not
    /// changing it must not write a version: that would mark every browsed image
    /// as edited and fill the history with decisions nobody made.
    opened_at: u64,
}

/// How long an edit has to stand still before it is written.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(800);

impl Saver {
    pub fn new(library: Option<Arc<Mutex<Library>>>, session: Arc<Mutex<Session>>) -> Self {
        let opened_at = session.lock().expect("session lock").generation();
        Self {
            library,
            session,
            image: None,
            settled: None,
            opened_at,
        }
    }

    /// Called every frame. Writes only once the edit has been still for
    /// [`SETTLE`].
    pub fn tick(&mut self) {
        let generation = self.session.lock().expect("session lock").generation();
        if generation == self.opened_at {
            return;
        }
        match self.settled {
            Some((seen, since)) if seen == generation && since.elapsed() >= SETTLE => {
                self.settled = Some((generation, std::time::Instant::now()));
                self.write();
            }
            Some((seen, _)) if seen == generation => {}
            _ => self.settled = Some((generation, std::time::Instant::now())),
        }
    }

    /// Write now, whatever the timer says.
    ///
    /// Called before the photograph changes. Without it, tweaking exposure and
    /// pressing the arrow key within the settle window loses the edit silently —
    /// which is the worst way to lose one, because nothing looked wrong at the
    /// time.
    pub fn flush(&mut self) {
        if self.session.lock().expect("session lock").generation() != self.opened_at {
            self.write();
        }
    }

    fn write(&self) {
        let (Some(library), Some(image)) = (&self.library, self.image) else {
            return;
        };
        let state = self.session.lock().expect("session lock").state().clone();
        let library = library.lock().expect("library lock");
        match rawkit_catalog::edits::save(
            library.catalog(),
            image,
            &state,
            rawkit_editstate::EditSource::User,
        ) {
            Ok(Some(version)) => eprintln!("edit       : saved v{version} for image {image}"),
            Ok(None) => {}
            Err(e) => eprintln!("edit       : could not save: {e}"),
        }
    }

    /// Put the current photograph's stored edit into the session, and treat that
    /// as the state it opened in.
    pub fn restore(&mut self, session: &mut Session) {
        let Some(library) = &self.library else {
            self.opened_at = session.generation();
            return;
        };
        let (catalog_result, id) = {
            let library = library.lock().expect("library lock");
            (
                rawkit_catalog::edits::latest(library.catalog(), library.current().id),
                library.current().id,
            )
        };
        self.image = Some(id);
        match catalog_result {
            Ok(Some((version, saved))) => {
                eprintln!("edit       : restored v{version} for image {id}");
                // `load`, not a command: opening a photograph is not something
                // the user did to it, and through the command bus the first
                // press of undo would restore the *previous* image's edit.
                session.load(saved);
            }
            Ok(None) => {
                // A photograph nobody has edited opens as shot. Applying the
                // default explicitly rather than assuming the session already
                // holds it, because the previous image's edit is what it holds.
                session.load(EditState::default());
            }
            Err(e) => eprintln!("edit       : could not read: {e}"),
        }
        self.settled = None;
        self.opened_at = session.generation();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rawkit_catalog::scan::FileMetadata;

    /// A catalog of `n` raws, taken in the order their names sort.
    ///
    /// Real files on disk, because a scan walks a directory — but never decoded:
    /// everything here is about which image is *selected*, and selecting one
    /// costs nothing. Loading it is the render loop's job and needs a GPU.
    pub(crate) fn library_at(dir: &Path, n: usize) -> Library {
        let photos = dir.join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        for i in 0..n {
            std::fs::write(photos.join(format!("DSC{i:05}.ARW")), b"raw").unwrap();
        }
        let mut catalog = Catalog::open(&dir.join("library.rawkit")).unwrap();
        rawkit_catalog::scan::scan_on(
            &mut catalog,
            &photos,
            rawkit_catalog::VolumeId::Uuid("test-volume".into()),
            // Capture time from the *name*, never from the order the reader is
            // called in. `read_dir` returns entries in whatever order the
            // filesystem likes, so a counter here makes the sequence depend on
            // the machine — which passed on ext4 and failed on CI, the third
            // time this project has been caught assuming a host filesystem.
            |path: &Path| {
                let name = path.file_stem()?.to_string_lossy().into_owned();
                let index: i64 = name.trim_start_matches("DSC").parse().ok()?;
                Some(FileMetadata {
                    captured_at: Some(1_000 + index),
                    ..FileMetadata::default()
                })
            },
        )
        .unwrap();
        drop(catalog);
        Library::open(&dir.join("library.rawkit")).unwrap()
    }

    /// A directory that cleans itself up, the same shape the catalog's own tests
    /// use — duplicated rather than shared because a `#[cfg(test)]` helper in
    /// another crate is not something this one can reach.
    pub(crate) struct Scratch(pub std::path::PathBuf);

    impl Scratch {
        pub(crate) fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("rawkit-shell-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    use rawkit_catalog::cull::Flagged;

    fn picks() -> Filter {
        Filter::flagged(Flagged::Pick)
    }

    fn filenames(library: &Library) -> Vec<String> {
        library
            .sequence
            .slice(0, library.sequence.len())
            .map(|image| image.filename.clone())
            .collect()
    }

    #[test]
    fn the_page_can_say_what_it_means_by_a_filter() {
        // The seam between the page and the shell, tested with the literal
        // payloads the page sends. Everything else here is Rust talking to
        // Rust; this is the one place a rename or a missing field would show up
        // only as a key that quietly did nothing.
        let parse = |json: &str| serde_json::from_str::<CullAction>(json).expect(json);

        let CullAction::SetFilter(all) = parse(r#"{"action":"set_filter","value":{}}"#) else {
            panic!("not a filter");
        };
        assert!(all.is_everything(), "an empty object is the whole library");

        let CullAction::SetFilter(picks) = parse(
            r#"{"action":"set_filter","value":{"flagged":"pick","min_rating":null,"colour":null}}"#,
        ) else {
            panic!("not a filter");
        };
        assert_eq!(picks, Filter::flagged(Flagged::Pick));

        let CullAction::SetFilter(narrow) = parse(
            r#"{"action":"set_filter","value":
                 {"flagged":"unflagged","min_rating":3,"colour":"red"}}"#,
        ) else {
            panic!("not a filter");
        };
        assert_eq!(
            narrow,
            Filter {
                flagged: Some(Flagged::Unflagged),
                min_rating: Some(3),
                colour: Some("red".into()),
            }
        );

        // And back the other way, because the page draws its chips from what
        // the shell reports rather than from what it asked for.
        let dir = Scratch::new("filter-json");
        let library = library_at(&dir.0, 1);
        let view = serde_json::to_value(library.view().unwrap()).unwrap();
        assert_eq!(view["filter"]["flagged"], serde_json::Value::Null);
        assert_eq!(view["in_library"], 1);
    }

    /// How long one keypress may take, and the same number the catalog's gate
    /// uses: roughly where a key stops appearing to cause its own result. It is
    /// the budget for the *whole* keypress — this, the render request and the
    /// redraw — so a library that eats it has eaten the budget.
    const INSTANT: std::time::Duration = std::time::Duration::from_millis(100);

    #[test]
    #[ignore = "writes twenty thousand files; a few seconds and a lot of inodes"]
    fn a_cull_stays_instant_at_the_size_of_a_real_library() {
        // The catalog has a scale gate and the shell had none — and the shell is
        // where a keypress is actually spent. What it costs is not what any one
        // query costs: it is whatever the shell *chooses to ask* per key, and a
        // full re-read of twenty thousand rows per pick is invisible on the ten
        // photographs every other test here uses.
        let sizes = [2_000usize, 20_000];
        println!(
            "{:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
            "images", "open", "next", "view", "pick*", "undo*", "filter", "on+off"
        );
        let mut worst = (std::time::Duration::ZERO, String::new());
        for n in sizes {
            let dir = Scratch::new(&format!("shell-scale-{n}"));
            let start = std::time::Instant::now();
            let mut library = library_at(&dir.0, n);
            let open = start.elapsed();
            // Every other test here checks each action against a full read of
            // the catalog. That read is the cost this test exists to show is no
            // longer being paid, so here — and only here — it is off.
            library.oracle = false;

            // The median of several, because the first write after an open
            // also pays for creating the write-ahead log and that is not the
            // keypress's cost.
            fn median(
                library: &mut Library,
                f: &mut dyn FnMut(&mut Library),
            ) -> std::time::Duration {
                let mut runs: Vec<std::time::Duration> = (0..9)
                    .map(|_| {
                        let start = std::time::Instant::now();
                        f(library);
                        start.elapsed()
                    })
                    .collect();
                runs.sort();
                runs[runs.len() / 2]
            }

            let next = median(&mut library, &mut |l| {
                l.act(CullAction::Next).unwrap();
            });
            let view = median(&mut library, &mut |l| {
                l.view().unwrap();
            });
            // The pass everybody makes: look at what is undecided, and decide.
            // Every pick drops a frame out of the filter, which is the case
            // that rebuilds the sequence.
            library
                .act(CullAction::SetFilter(Filter::flagged(Flagged::Unflagged)))
                .unwrap();
            let pick = median(&mut library, &mut |l| {
                l.act(CullAction::Pick).unwrap();
            });
            let undo = median(&mut library, &mut |l| {
                l.act(CullAction::Undo).unwrap();
            });
            // Each direction on its own. Timed as one alternating closure, the
            // median of nine landed on clearing the filter — which asks the
            // catalog nothing — and reported 22 µs for an operation whose other
            // half is a query. A median hides whichever half is rarer.
            let narrowing = Filter::flagged(Flagged::Unflagged);
            // Setting one, with the clearing done outside the clock: the last of
            // several, so the write-ahead log is already there.
            let mut filter_on = std::time::Duration::ZERO;
            for _ in 0..5 {
                library
                    .act(CullAction::SetFilter(Filter::default()))
                    .unwrap();
                let start = std::time::Instant::now();
                library
                    .act(CullAction::SetFilter(narrowing.clone()))
                    .unwrap();
                filter_on = start.elapsed();
            }
            let filter_off = median(&mut library, &mut |l| {
                l.act(CullAction::SetFilter(Filter::default())).unwrap();
                l.act(CullAction::SetFilter(narrowing.clone())).unwrap();
            });
            for (what, took) in [
                ("next", next),
                ("view", view),
                ("pick under a filter", pick),
                ("undo under a filter", undo),
                ("setting a filter", filter_on),
                ("setting and clearing a filter", filter_off),
            ] {
                if took > worst.0 {
                    worst = (took, format!("{what} at {n}"));
                }
            }
            println!(
                "{n:>8} {open:>9.1?} {next:>9.1?} {view:>9.1?} {pick:>9.1?} {undo:>9.1?} {filter_on:>9.1?} {filter_off:>9.1?}"
            );
        }
        assert!(
            worst.0 < INSTANT,
            "{} took {:.1?}, past the {INSTANT:?} a keypress has. Two sizes are \
             printed so a constant cost can be told from one that grows.",
            worst.1,
            worst.0
        );
    }

    #[test]
    fn the_page_can_say_what_it_means_by_a_collection() {
        // The same seam the filter test covers, for the same reason: everything
        // else here is Rust talking to Rust, and a rename would show up only as
        // a key that quietly did nothing.
        let parse = |json: &str| serde_json::from_str::<CullAction>(json).expect(json);

        // Going back to the whole library is `null`, not an absent value — the
        // variant carries an `Option`, so the content has to be there and be
        // null. A page that sent `{"action":"show_collection"}` would fail to
        // deserialise, which is the one payload worth pinning.
        assert!(matches!(
            parse(r#"{"action":"show_collection","value":null}"#),
            CullAction::ShowCollection(None)
        ));
        assert!(matches!(
            parse(r#"{"action":"show_collection","value":7}"#),
            CullAction::ShowCollection(Some(7))
        ));
        assert!(matches!(
            parse(r#"{"action":"target_toggle"}"#),
            CullAction::TargetToggle
        ));
        assert!(matches!(
            parse(r#"{"action":"add_marked"}"#),
            CullAction::AddMarked
        ));
        assert!(matches!(
            parse(r#"{"action":"take_out"}"#),
            CullAction::TakeOut
        ));
        assert!(matches!(
            parse(r#"{"action":"set_target","value":4}"#),
            CullAction::SetTarget(4)
        ));
        assert!(matches!(
            parse(r#"{"action":"empty_collection","value":1}"#),
            CullAction::EmptyCollection(1)
        ));
        assert!(matches!(
            parse(r#"{"action":"delete_collection","value":4}"#),
            CullAction::DeleteCollection(4)
        ));
        // The one action here whose value is an object rather than a scalar.
        let CullAction::RenameCollection { id, name } =
            parse(r#"{"action":"rename_collection","value":{"id":4,"name":"Prints"}}"#)
        else {
            panic!("not a rename");
        };
        assert_eq!((id, name.as_str()), (4, "Prints"));
        assert!(matches!(
            parse(r#"{"action":"move_in_collection","value":-1}"#),
            CullAction::MoveInCollection(-1)
        ));
        let CullAction::NewCollection(name) =
            parse(r#"{"action":"new_collection","value":"Portfolio"}"#)
        else {
            panic!("not a collection");
        };
        assert_eq!(name, "Portfolio");
    }

    #[test]
    fn a_collection_is_walked_in_the_order_it_was_put_in() {
        // The claim the whole feature rests on. If this came back in capture
        // order the collection would be a saved filter with extra steps.
        let dir = Scratch::new("collection-order");
        let mut library = library_at(&dir.0, 4);
        let ids: Vec<i64> = cull::sequence(library.catalog(), &Filter::default())
            .unwrap()
            .iter()
            .map(|image| image.id)
            .collect();
        // Made through the library, in the order the frames were marked. This
        // first wrote to the catalog directly, and the oracle that checks the
        // held list objects to that — rightly: a test that goes round the
        // library is a second writer, and the library does not have one.
        for at in [2, 0] {
            library.select(at);
            library.act(CullAction::Mark).unwrap();
        }
        library
            .act(CullAction::NewCollection("Edit".into()))
            .unwrap();
        library.act(CullAction::ClearMarks).unwrap();
        let shelf = named(&library, "Edit").id;

        library
            .act(CullAction::ShowCollection(Some(shelf)))
            .unwrap();
        assert_eq!(library.count(), 2);
        library.select(0);
        assert_eq!(
            library.current().id,
            ids[2],
            "the shoot's order is not this"
        );
        library.select(1);
        assert_eq!(library.current().id, ids[0]);

        // And moving one rewrites that order rather than the library's.
        library.select(1);
        library.act(CullAction::MoveInCollection(-1)).unwrap();
        library.select(0);
        assert_eq!(library.current().id, ids[0]);

        library.act(CullAction::ShowCollection(None)).unwrap();
        assert_eq!(library.count(), 4, "back to the whole library");
    }

    #[test]
    fn judging_inside_a_collection_stays_inside_it() {
        // **The bug this exists for.** Re-reading the sequence happens in six
        // places, and every one that called `cull::sequence` directly was a way
        // for a keypress to drop you back into the whole library without saying
        // so. A pick that ends a filtered pass is the one that does it.
        let dir = Scratch::new("collection-cull");
        let mut library = library_at(&dir.0, 5);
        let ids: Vec<i64> = cull::sequence(library.catalog(), &Filter::default())
            .unwrap()
            .iter()
            .map(|image| image.id)
            .collect();
        // Made through the library, in the order the frames were marked. This
        // first wrote to the catalog directly, and the oracle that checks the
        // held list objects to that — rightly: a test that goes round the
        // library is a second writer, and the library does not have one.
        for at in [1, 3] {
            library.select(at);
            library.act(CullAction::Mark).unwrap();
        }
        library
            .act(CullAction::NewCollection("Edit".into()))
            .unwrap();
        library.act(CullAction::ClearMarks).unwrap();
        let shelf = named(&library, "Edit").id;

        library
            .act(CullAction::ShowCollection(Some(shelf)))
            .unwrap();
        library
            .act(CullAction::SetFilter(Filter::flagged(Flagged::Unflagged)))
            .unwrap();
        assert_eq!(library.count(), 2);

        // Picking drops the frame out of "undecided", so the sequence rebuilds.
        library.act(CullAction::Pick).unwrap();
        let view = library.view().unwrap();
        assert_eq!(
            view.viewing,
            Some(shelf),
            "the pick dropped the collection view and went back to the library"
        );
        assert_eq!(library.count(), 1);
        assert!(
            [ids[1], ids[3]].contains(&library.current().id),
            "the frame left standing is not even in the collection"
        );

        // And undo puts it back without leaving the collection either.
        library.act(CullAction::Undo).unwrap();
        assert_eq!(library.view().unwrap().viewing, Some(shelf));
        assert_eq!(library.count(), 2);
    }

    #[test]
    fn the_quick_key_puts_a_frame_in_and_takes_it_out() {
        let dir = Scratch::new("collection-quick");
        let mut library = library_at(&dir.0, 2);
        assert!(!library.view().unwrap().in_target);
        library.act(CullAction::TargetToggle).unwrap();
        assert!(library.view().unwrap().in_target, "K did not add it");
        // The list the page draws is *held* rather than asked for on every key,
        // so it has to be refreshed by whatever changes it. A count that stayed
        // at zero here would be a chip reading "quick 0" with a photograph in
        // it — the cache going stale, which is the one way holding it can fail.
        let quick_count = |library: &Library| {
            let view = library.view().unwrap();
            view.collections.iter().find(|c| c.is_quick).unwrap().count
        };
        assert_eq!(
            quick_count(&library),
            1,
            "the held list did not follow the key"
        );
        library.act(CullAction::TargetToggle).unwrap();
        assert!(!library.view().unwrap().in_target, "K did not take it out");
        assert_eq!(quick_count(&library), 0);
        // And a new collection shows up in it without being asked for again.
        library
            .act(CullAction::NewCollection("Portfolio".into()))
            .unwrap();
        let view = library.view().unwrap();
        let made = view.collections.iter().find(|c| c.name == "Portfolio");
        assert_eq!(
            made.map(|c| c.count),
            Some(1),
            "made from the frame on screen"
        );

        // And the list the page draws its chips from always has it, so the key
        // has somewhere to put a photograph from the first keystroke.
        let view = library.view().unwrap();
        assert!(view.collections.iter().any(|c| c.is_quick));
    }

    #[test]
    fn the_held_tally_follows_everything_that_can_move_it() {
        // The counts are held rather than scanned for on every keypress, so
        // they are only as good as the places that change them. Judgements and
        // their undo go through `act`, where every test checks the tally
        // against the catalog; copies do not, so they are checked here.
        let dir = Scratch::new("tally-held");
        let mut library = library_at(&dir.0, 3);
        let agrees = |library: &Library| {
            let view = library.view().unwrap();
            let held = (view.in_library, view.picks, view.rejects);
            assert_eq!(held, cull::tally(library.catalog()).unwrap());
            held
        };
        assert_eq!(agrees(&library), (3, 0, 0));
        library.act(CullAction::Pick).unwrap();
        library.act(CullAction::Reject).unwrap();
        assert_eq!(agrees(&library), (3, 1, 1));
        // A reject turned into a pick is one of each moving, not one.
        library.select(1);
        library.act(CullAction::Pick).unwrap();
        assert_eq!(agrees(&library), (3, 2, 0));
        library.act(CullAction::Undo).unwrap();
        assert_eq!(agrees(&library), (3, 1, 1));

        // A copy is a photograph, and throwing one away can take a pick with it.
        library.select(0);
        library.add_copy(&EditState::default()).unwrap();
        assert_eq!(agrees(&library), (4, 1, 1));
        library.act(CullAction::Pick).unwrap();
        library.select(1);
        assert!(library.current().copy_name.is_some());
        assert_eq!(agrees(&library), (4, 2, 1));
        library.remove_copy().unwrap();
        assert_eq!(agrees(&library), (3, 1, 1));
    }

    #[test]
    fn throwing_a_copy_away_takes_it_off_the_chip_too() {
        // The held list is a copy of the catalog's counts, and deleting an image
        // cascades it out of every collection it was in. `remove_copy` is not an
        // arm of `act`, so it was the one mutation that never heard the copy
        // existed — and the chip went on claiming a photograph that was gone.
        let dir = Scratch::new("collection-copy");
        let mut library = library_at(&dir.0, 2);
        let quick_count = |library: &Library| {
            let view = library.view().unwrap();
            view.collections.iter().find(|c| c.is_quick).unwrap().count
        };
        library.add_copy(&EditState::default()).unwrap();
        assert!(
            library.current().copy_name.is_some(),
            "standing on the copy"
        );
        library.act(CullAction::TargetToggle).unwrap();
        assert_eq!(quick_count(&library), 1);

        library.remove_copy().unwrap();
        assert_eq!(
            quick_count(&library),
            0,
            "the copy is gone from the catalog and still counted on the chip"
        );
    }

    /// The photographs in a collection, as the catalog has them.
    fn members_of(library: &Library, id: i64) -> Vec<i64> {
        collections::members(library.catalog(), id, &Filter::default())
            .unwrap()
            .iter()
            .map(|image| image.id)
            .collect()
    }

    fn named(library: &Library, name: &str) -> Collection {
        let view = library.view().unwrap();
        let found = view.collections.iter().find(|c| c.name == name);
        found
            .unwrap_or_else(|| panic!("no collection called {name}"))
            .clone()
    }

    #[test]
    fn the_key_goes_wherever_it_has_been_aimed() {
        // "Add this frame to Portfolio" is not a second key or a menu. It is K,
        // pointed somewhere else — and the catalog remembers where, because an
        // edit takes days and a target that reset every launch would put a
        // sitting's worth of frames in the wrong place.
        let dir = Scratch::new("collection-target");
        let mut library = library_at(&dir.0, 3);
        library
            .act(CullAction::NewCollection("Portfolio".into()))
            .unwrap();
        let portfolio = named(&library, "Portfolio");
        assert!(
            !portfolio.is_target,
            "the quick collection is, to begin with"
        );

        library.act(CullAction::SetTarget(portfolio.id)).unwrap();
        assert!(named(&library, "Portfolio").is_target);
        library.select(1);
        library.act(CullAction::TargetToggle).unwrap();
        assert!(library.view().unwrap().in_target);
        assert_eq!(members_of(&library, portfolio.id).len(), 2);
        assert_eq!(named(&library, "Quick Collection").count, 0, "not there");

        // Remembered by the catalog, not by the session.
        drop(library);
        let reopened = Library::open(&dir.0.join("library.rawkit")).unwrap();
        assert!(named(&reopened, "Portfolio").is_target);
    }

    #[test]
    fn marked_frames_go_in_together_and_come_out_together() {
        let dir = Scratch::new("collection-marked");
        let mut library = library_at(&dir.0, 4);
        let quick = named(&library, "Quick Collection").id;
        // One of the three is already in, and has to survive the undo: taking
        // back "add these" must not remove a frame somebody added before.
        library.select(1);
        library.act(CullAction::TargetToggle).unwrap();
        let already = library.current().id;
        for at in [0, 1, 2] {
            library.select(at);
            library.act(CullAction::Mark).unwrap();
        }
        library.act(CullAction::AddMarked).unwrap();
        assert_eq!(members_of(&library, quick).len(), 3);

        library.act(CullAction::Undo).unwrap();
        assert_eq!(members_of(&library, quick), [already]);
    }

    #[test]
    fn a_frame_taken_out_of_a_collection_comes_back_where_it_was() {
        let dir = Scratch::new("collection-takeout");
        let mut library = library_at(&dir.0, 3);
        for at in [0, 1, 2] {
            library.select(at);
            library.act(CullAction::Mark).unwrap();
        }
        library
            .act(CullAction::NewCollection("Edit".into()))
            .unwrap();
        let edit = named(&library, "Edit").id;
        let before = members_of(&library, edit);

        library.act(CullAction::ShowCollection(Some(edit))).unwrap();
        library.select(1);
        let middle = library.current().id;
        library.act(CullAction::TakeOut).unwrap();
        assert_eq!(library.count(), 2, "and the view followed");
        assert!(!members_of(&library, edit).contains(&middle));

        // The middle again, not the end. The order is the thing being kept.
        library.act(CullAction::Undo).unwrap();
        assert_eq!(members_of(&library, edit), before);
        assert_eq!(library.current().id, middle, "standing on what came back");

        // And outside a collection the key is refused rather than ignored.
        library.act(CullAction::ShowCollection(None)).unwrap();
        assert!(library.act(CullAction::TakeOut).is_err());
    }

    #[test]
    fn a_deleted_collection_is_one_keypress_from_coming_back() {
        // Nothing here asks "are you sure". Everything can be taken back,
        // which is the only kind of safety a keyboard-first cull can have.
        let dir = Scratch::new("collection-delete");
        let mut library = library_at(&dir.0, 3);
        for at in [2, 0] {
            library.select(at);
            library.act(CullAction::Mark).unwrap();
        }
        library
            .act(CullAction::NewCollection("Edit".into()))
            .unwrap();
        let edit = named(&library, "Edit").id;
        let before = members_of(&library, edit);
        library.act(CullAction::SetTarget(edit)).unwrap();
        library.act(CullAction::ShowCollection(Some(edit))).unwrap();

        library.act(CullAction::DeleteCollection(edit)).unwrap();
        let view = library.view().unwrap();
        assert!(!view.collections.iter().any(|c| c.name == "Edit"));
        assert_eq!(
            view.viewing, None,
            "a view of nothing falls back to the library"
        );
        assert!(
            named(&library, "Quick Collection").is_target,
            "and K still has somewhere to put a photograph"
        );

        library.act(CullAction::Undo).unwrap();
        let back = named(&library, "Edit");
        assert_eq!(members_of(&library, back.id), before);
        assert!(back.is_target, "aimed at again, as it was");

        // The quick collection cannot be deleted, only emptied — and that can
        // be taken back too.
        let quick = named(&library, "Quick Collection").id;
        assert!(library.act(CullAction::DeleteCollection(quick)).is_err());
    }

    #[test]
    fn emptying_and_renaming() {
        let dir = Scratch::new("collection-empty");
        let mut library = library_at(&dir.0, 3);
        let quick = named(&library, "Quick Collection").id;
        for at in [0, 2] {
            library.select(at);
            library.act(CullAction::TargetToggle).unwrap();
        }
        let before = members_of(&library, quick);
        library.act(CullAction::EmptyCollection(quick)).unwrap();
        assert_eq!(named(&library, "Quick Collection").count, 0);
        library.act(CullAction::Undo).unwrap();
        assert_eq!(members_of(&library, quick), before);

        library
            .act(CullAction::NewCollection("Portfolio".into()))
            .unwrap();
        let id = named(&library, "Portfolio").id;
        library
            .act(CullAction::RenameCollection {
                id,
                name: "Prints".into(),
            })
            .unwrap();
        assert_eq!(named(&library, "Prints").id, id);
        // The naming rule reaches a rename: no second "prints".
        library
            .act(CullAction::NewCollection("Other".into()))
            .unwrap();
        let other = named(&library, "Other").id;
        assert!(library
            .act(CullAction::RenameCollection {
                id: other,
                name: "PRINTS".into(),
            })
            .is_err());
    }

    #[test]
    fn moving_a_photograph_is_refused_outside_a_collection() {
        // The library's order is the shoot's, and a key that silently did
        // nothing would read as a broken key rather than a wrong one.
        let dir = Scratch::new("collection-move");
        let mut library = library_at(&dir.0, 2);
        assert!(library.act(CullAction::MoveInCollection(1)).is_err());
    }

    #[test]
    fn a_copy_stands_beside_its_original_and_carries_the_edit() {
        let dir = Scratch::new("copy-make");
        let mut library = library_at(&dir.0, 3);
        library.select(1);
        let mut look = EditState::default();
        look.tone.exposure_ev = 1.25;

        let name = library.add_copy(&look).unwrap();
        assert_eq!(name, "copy 1");
        let view = library.view().unwrap();
        assert_eq!(view.total, 4, "a fourth image over three files");
        assert_eq!(view.filename, "DSC00001.ARW", "the same file");
        assert_eq!(
            view.copy.as_deref(),
            Some("copy 1"),
            "and we are on the copy"
        );
        assert_eq!(
            view.position, 3,
            "immediately after the photograph it came from"
        );
        assert!(
            library.take_request().is_some(),
            "the copy has its own edit, so the loop has to restore it"
        );

        let id = library.current().id;
        let (_, state) = rawkit_catalog::edits::latest(library.catalog(), id)
            .unwrap()
            .expect("the copy's edit");
        assert_eq!(state.tone.exposure_ev, 1.25);
    }

    #[test]
    fn a_copy_is_judged_apart_from_the_photograph_it_came_from() {
        // The reason it is an image and not a file. Two interpretations, two
        // decisions — and the export follows the flag, so this is what stops one
        // of them being delivered because the other was liked.
        let dir = Scratch::new("copy-judge");
        let mut library = library_at(&dir.0, 2);
        library.act(CullAction::Rate(5)).unwrap();
        library.act(CullAction::Pick).unwrap();
        library.select(0);

        library.add_copy(&EditState::default()).unwrap();
        let view = library.view().unwrap();
        assert_eq!(view.rating, None, "a copy arrives undecided");
        assert_eq!(view.flag, None);

        library.act(CullAction::Reject).unwrap();
        library.select(0);
        let original = library.view().unwrap();
        assert_eq!(
            original.flag,
            Some("pick"),
            "judging the copy reached the original"
        );
        assert_eq!(original.rating, Some(5));
    }

    #[test]
    fn making_a_copy_a_filter_would_hide_takes_the_filter_off() {
        // A copy starts undecided, so "picks" will not have it — and creating
        // something the user cannot see is worse than changing the view they
        // asked for. The same rule as a filter that empties: the shell may drop
        // one, and the view carries that back so the chips follow.
        let dir = Scratch::new("copy-filtered");
        let mut library = library_at(&dir.0, 3);
        library.act(CullAction::Pick).unwrap();
        library.select(0);
        library
            .act(CullAction::SetFilter(Filter::flagged(Flagged::Pick)))
            .unwrap();
        assert_eq!(library.view().unwrap().total, 1);

        library.add_copy(&EditState::default()).unwrap();
        let view = library.view().unwrap();
        assert!(
            view.filter.is_everything(),
            "the filter hid what was just made"
        );
        assert_eq!(
            view.copy.as_deref(),
            Some("copy 1"),
            "and we are looking at it"
        );
    }

    #[test]
    fn removing_a_copy_leaves_the_photograph_and_forgets_what_named_it() {
        let dir = Scratch::new("copy-remove");
        let mut library = library_at(&dir.0, 2);
        library.add_copy(&EditState::default()).unwrap();
        library.act(CullAction::Mark).unwrap();
        library.act(CullAction::Rate(3)).unwrap();
        assert_eq!(library.view().unwrap().marked, 1);
        assert!(library.view().unwrap().undoable);

        let gone = library.remove_copy().unwrap();
        assert_eq!(gone, "DSC00000.ARW · copy 1");
        let view = library.view().unwrap();
        assert_eq!(view.total, 2, "the two photographs are still there");
        assert_eq!(view.copy, None);
        assert_eq!(view.marked, 0, "a mark on a row that is gone");
        assert!(
            !view.undoable,
            "an undo that would put a judgement back on nothing"
        );
    }

    #[test]
    fn the_photograph_itself_cannot_be_removed_by_the_copy_key() {
        let dir = Scratch::new("copy-refuse");
        let mut library = library_at(&dir.0, 2);
        assert!(library.remove_copy().is_err());
        assert_eq!(library.view().unwrap().total, 2);
    }

    #[test]
    fn a_filter_narrows_the_sequence_the_arrows_walk() {
        // The whole point: marking a shoot up and then being able to look at
        // what you marked. Position and total describe the filtered set, not
        // the library, because that is what the arrows now move through.
        let dir = Scratch::new("filter-narrows");
        let mut library = library_at(&dir.0, 5);
        library.act(CullAction::Pick).unwrap(); // 0
        library.act(CullAction::Pick).unwrap(); // 1
        library.select(4);
        library.act(CullAction::Pick).unwrap(); // 4

        let view = library.act(CullAction::SetFilter(picks())).unwrap();
        assert_eq!(view.total, 3, "three picks");
        assert_eq!(view.in_library, 5, "out of five photographs");
        assert_eq!(
            filenames(&library),
            ["DSC00000.ARW", "DSC00001.ARW", "DSC00004.ARW"]
        );

        // And the sequence is still a sequence: the arrow key steps over the
        // frames the filter left out rather than stopping at them.
        library.select(1);
        let next = library.act(CullAction::Next).unwrap();
        assert_eq!(next.filename, "DSC00004.ARW");
        assert_eq!(next.position, 3);
    }

    #[test]
    fn setting_a_filter_keeps_the_frame_you_were_on() {
        let dir = Scratch::new("filter-stays");
        let mut library = library_at(&dir.0, 6);
        for at in [1usize, 3, 5] {
            library.select(at);
            library.act(CullAction::Pick).unwrap();
        }
        library.select(3);
        let view = library.act(CullAction::SetFilter(picks())).unwrap();
        assert_eq!(view.filename, "DSC00003.ARW", "the same photograph");
        assert_eq!(view.position, 2, "at a new place in a shorter sequence");
    }

    #[test]
    fn a_filter_that_excludes_where_you_stand_moves_forward_not_home() {
        // Where you had got to in a shoot is worth more than the tidiness of
        // starting again — and a filter is usually set in the middle of a pass.
        let dir = Scratch::new("filter-forward");
        let mut library = library_at(&dir.0, 6);
        library.select(0);
        library.act(CullAction::Pick).unwrap();
        library.select(4);
        library.act(CullAction::Pick).unwrap();

        library.select(2); // Unflagged, so the filter is about to exclude it.
        let view = library.act(CullAction::SetFilter(picks())).unwrap();
        assert_eq!(
            view.filename, "DSC00004.ARW",
            "the next pick forward, not the first one"
        );
        assert!(
            library.take_request().is_some(),
            "a different photograph has to be loaded"
        );
    }

    #[test]
    fn judging_a_frame_out_of_the_filter_advances_exactly_one() {
        // The bug this shape exists to prevent: the frame leaves, everything
        // after it moves up a place, and an advance on top of that steps over
        // its neighbour — a photograph silently skipped in a pass whose whole
        // job is to look at every photograph.
        let dir = Scratch::new("filter-drop");
        let mut library = library_at(&dir.0, 4);
        let view = library
            .act(CullAction::SetFilter(Filter::flagged(Flagged::Unflagged)))
            .unwrap();
        assert_eq!(view.total, 4);

        let after = library.act(CullAction::Pick).unwrap();
        assert_eq!(after.total, 3, "the frame left the set");
        assert_eq!(
            after.filename, "DSC00001.ARW",
            "and the cursor is on the next one, not the one after it"
        );
        assert_eq!(after.position, 1);

        let after = library.act(CullAction::Reject).unwrap();
        assert_eq!(after.filename, "DSC00002.ARW");
    }

    #[test]
    fn a_rating_can_drop_a_frame_out_even_though_it_does_not_advance() {
        // Rating is the considered judgement that stays put — but standing in
        // "three stars and better" and pressing 1 still means the frame has
        // gone, and the cursor has to be somewhere real afterwards.
        let dir = Scratch::new("filter-rating");
        let mut library = library_at(&dir.0, 3);
        for at in 0..3 {
            library.select(at);
            library.act(CullAction::Rate(4)).unwrap();
        }
        library.select(1);
        library
            .act(CullAction::SetFilter(Filter::rated(3)))
            .unwrap();

        let after = library.act(CullAction::Rate(1)).unwrap();
        assert_eq!(after.total, 2);
        assert_eq!(after.filename, "DSC00002.ARW");
        assert_eq!(after.rating, Some(4), "and it is the new frame's rating");
    }

    #[test]
    fn undo_brings_back_a_frame_the_filter_had_dropped() {
        // Otherwise the key that reverses a mistake leaves you unable to see
        // what it reversed, which is the same as not having it.
        let dir = Scratch::new("filter-undo");
        let mut library = library_at(&dir.0, 3);
        library
            .act(CullAction::SetFilter(Filter::flagged(Flagged::Unflagged)))
            .unwrap();
        library.act(CullAction::Pick).unwrap();
        assert_eq!(library.view().unwrap().total, 2);

        let back = library.act(CullAction::Undo).unwrap();
        assert_eq!(back.total, 3);
        assert_eq!(back.filename, "DSC00000.ARW", "and we are looking at it");
        assert_eq!(back.flag, None);
    }

    #[test]
    fn a_filter_nothing_matches_is_refused_rather_than_shown_empty() {
        // A window showing nothing cannot say why it is showing nothing. The
        // refusal is a message; an empty sequence would be a blank canvas and a
        // `current` with no photograph to name.
        let dir = Scratch::new("filter-empty");
        let mut library = library_at(&dir.0, 3);
        let refused = library.act(CullAction::SetFilter(picks()));
        assert!(refused.is_err(), "no photograph is picked");
        let view = library.view().unwrap();
        assert_eq!(view.total, 3, "and the library is still what it was");
        assert!(view.filter.is_everything());
    }

    #[test]
    fn the_last_frame_leaving_a_filter_turns_the_filter_off() {
        // The ordinary end of a pass: filter to the undecided, decide the last
        // one. The alternative to this is a blank window at the exact moment
        // the user has finished, which reads as a crash rather than as success.
        let dir = Scratch::new("filter-exhausted");
        let mut library = library_at(&dir.0, 2);
        library
            .act(CullAction::SetFilter(Filter::flagged(Flagged::Unflagged)))
            .unwrap();
        library.act(CullAction::Pick).unwrap();
        let view = library.act(CullAction::Pick).unwrap();
        assert!(
            view.filter.is_everything(),
            "the filter cannot survive emptying the sequence"
        );
        assert_eq!(view.total, 2);
        assert_eq!(
            view.filename, "DSC00001.ARW",
            "standing on the frame that was just judged"
        );
    }

    #[test]
    fn a_mark_follows_its_photograph_through_a_filter() {
        // Marks are stored as ids for this: a filter renumbers the sequence, and
        // a comparison built out of positions would quietly become a comparison
        // of different photographs.
        let dir = Scratch::new("filter-marks");
        let mut library = library_at(&dir.0, 5);
        library.select(1);
        library.act(CullAction::Pick).unwrap();
        library.select(3);
        library.act(CullAction::Pick).unwrap();
        library.select(1);
        library.act(CullAction::Mark).unwrap();
        library.select(3);
        library.act(CullAction::Mark).unwrap();
        library.select(4);
        let unfiltered = library.act(CullAction::Mark).unwrap();
        assert_eq!(unfiltered.marked, 3);

        library.act(CullAction::SetFilter(picks())).unwrap();
        assert_eq!(
            library.marked(),
            [0, 1],
            "the two picks, at their new positions"
        );
        let view = library.view().unwrap();
        assert_eq!(view.marked, 2, "the third is marked and not on screen");

        // And it is still marked when the filter comes off: narrowing the view
        // is not a decision about the comparison.
        library
            .act(CullAction::SetFilter(Filter::default()))
            .unwrap();
        assert_eq!(library.marked(), [1, 3, 4]);
    }

    #[test]
    fn a_flag_advances_and_a_rating_does_not() {
        // The decision the interface is built around: P and X are the fast pass
        // through a shoot, digits are a considered judgement on the frame you
        // are already looking at.
        let dir = Scratch::new("advance");
        let mut library = library_at(&dir.0, 3);
        assert_eq!(library.view().unwrap().position, 1);

        let after_rating = library.act(CullAction::Rate(4)).unwrap();
        assert_eq!(after_rating.position, 1, "a rating stays put");
        assert_eq!(after_rating.rating, Some(4));

        let after_pick = library.act(CullAction::Pick).unwrap();
        assert_eq!(after_pick.position, 2, "a flag moves on");
        assert_eq!(after_pick.flag, None, "and the new frame is undecided");
    }

    #[test]
    fn a_cull_stops_at_both_ends_rather_than_wrapping() {
        // Wrapping is how a frame gets judged twice while its neighbour is
        // missed, and `wrapping_sub` at the front would jump to the far end.
        let dir = Scratch::new("ends");
        let mut library = library_at(&dir.0, 2);
        assert_eq!(library.act(CullAction::Previous).unwrap().position, 1);
        assert_eq!(library.act(CullAction::Next).unwrap().position, 2);
        assert_eq!(library.act(CullAction::Next).unwrap().position, 2);
        assert_eq!(library.act(CullAction::Pick).unwrap().position, 2);
    }

    #[test]
    fn undo_puts_back_the_judgement_and_returns_to_the_frame() {
        // The keypress this exists for is X on a keeper, which both marks the
        // wrong thing *and* moves you off it. Undo has to fix both halves.
        let dir = Scratch::new("undo");
        let mut library = library_at(&dir.0, 3);
        library.act(CullAction::Rate(5)).unwrap();
        let after_reject = library.act(CullAction::Reject).unwrap();
        assert_eq!(after_reject.position, 2);

        let undone = library.act(CullAction::Undo).unwrap();
        assert_eq!(undone.position, 1, "back to the frame that was mis-keyed");
        assert_eq!(undone.flag, None, "and it is unflagged again");
        assert_eq!(undone.rating, Some(5), "without losing the rating");

        // One more step back reaches the state before the rating.
        assert_eq!(library.act(CullAction::Undo).unwrap().rating, None);
        assert!(!library.view().unwrap().undoable);
    }

    #[test]
    fn a_judgement_that_changes_nothing_is_not_undoable() {
        // Otherwise pressing 3 twice costs two undos to get back, and the second
        // one silently does nothing.
        let dir = Scratch::new("noop");
        let mut library = library_at(&dir.0, 2);
        library.act(CullAction::Rate(3)).unwrap();
        library.act(CullAction::Rate(3)).unwrap();
        assert_eq!(library.act(CullAction::Undo).unwrap().rating, None);
        assert!(!library.view().unwrap().undoable);
    }

    #[test]
    fn only_the_latest_navigation_is_left_for_the_render_loop() {
        // A held arrow key repeats far faster than a RAW decodes. Six presses
        // must cost one load, not six — the same coalescing the session gets by
        // having no command queue.
        let dir = Scratch::new("coalesce");
        let mut library = library_at(&dir.0, 6);
        for _ in 0..5 {
            library.act(CullAction::Next).unwrap();
        }
        let requested = library.take_request().expect("a load is pending");
        assert!(requested.ends_with("DSC00005.ARW"), "{requested}");
        assert_eq!(library.take_request(), None, "and it is consumed once");
    }

    #[test]
    fn standing_still_asks_for_no_load_at_all() {
        // Rating the current frame must not make the render loop decode it
        // again, which would turn every digit into a fifth of a second.
        let dir = Scratch::new("still");
        let mut library = library_at(&dir.0, 2);
        library.act(CullAction::Rate(2)).unwrap();
        library.act(CullAction::ClearFlag).unwrap();
        assert_eq!(library.take_request(), None);
    }
}

#[cfg(test)]
mod saver_tests {
    use super::tests::{library_at, Scratch};
    use super::*;
    use rawkit_editstate::EditState;
    use rawkit_session::Command;

    /// An edit made on one frame must not land on the next one.
    ///
    /// Found by running the thing: after navigating, the second photograph's
    /// first saved version was the first photograph's white balance, exactly.
    /// The cursor moves the instant a key is pressed and the flush happens a
    /// frame later, so a saver that asks the library "which image is this?"
    /// gets the answer for the frame being navigated *to*.
    ///
    /// The symptom is nearly invisible: the next frame opens with the previous
    /// frame's edit applied, which looks like an edit being carried forward
    /// rather than like data going to the wrong row.
    /// The seam between `Saver::restore` and the session's undo history.
    ///
    /// `restore` must not go through the command bus, or opening a photograph
    /// becomes a step in *its* history — and the first press of undo hands the
    /// user the previous picture's edit, which they never applied to this one
    /// and cannot tell apart from their own work.
    #[test]
    fn opening_a_photograph_puts_the_previous_edit_out_of_undos_reach() {
        let dir = Scratch::new("restore-undo");
        let library = Arc::new(Mutex::new(library_at(&dir.0, 2)));
        let session = Arc::new(Mutex::new(Session::new(
            [100, 100],
            64,
            EditState::default(),
            rawkit_editstate::Orientation::AsShot,
        )));
        let mut saver = Saver::new(Some(library.clone()), session.clone());
        saver.restore(&mut session.lock().unwrap());

        session.lock().unwrap().apply(Command::SetExposure(1.5));
        library.lock().unwrap().act(CullAction::Next).unwrap();
        library.lock().unwrap().take_request();
        saver.flush();
        saver.restore(&mut session.lock().unwrap());

        assert_eq!(session.lock().unwrap().state().tone.exposure_ev, 0.0);
        assert!(matches!(
            session.lock().unwrap().apply(rawkit_session::Command::Undo),
            rawkit_session::Event::Refused { .. }
        ));
        assert_eq!(
            session.lock().unwrap().state().tone.exposure_ev,
            0.0,
            "undo handed back the previous photograph's edit"
        );
    }

    #[test]
    fn an_edit_is_written_to_the_photograph_it_was_made_on() {
        let dir = Scratch::new("saver");
        let library = Arc::new(Mutex::new(library_at(&dir.0, 2)));
        let first = library.lock().unwrap().current().id;

        let session = Arc::new(Mutex::new(Session::new(
            [100, 100],
            64,
            EditState::default(),
            rawkit_editstate::Orientation::AsShot,
        )));
        let mut saver = Saver::new(Some(library.clone()), session.clone());
        saver.restore(&mut session.lock().unwrap());

        // Change the edit, then navigate before the settle timer could fire.
        session.lock().unwrap().apply(Command::SetExposure(1.5));
        library.lock().unwrap().act(CullAction::Next).unwrap();
        let second = library.lock().unwrap().current().id;
        assert_ne!(first, second);
        library.lock().unwrap().take_request();
        saver.flush();

        let catalog = library.lock().unwrap();
        let on_first = rawkit_catalog::edits::latest(catalog.catalog(), first).unwrap();
        let on_second = rawkit_catalog::edits::latest(catalog.catalog(), second).unwrap();
        assert_eq!(
            on_first.map(|(_, s)| s.tone.exposure_ev),
            Some(1.5),
            "the edit belongs to the frame it was made on"
        );
        assert!(
            on_second.is_none(),
            "and the frame that was never touched has no version at all"
        );
    }

    /// Opening a photograph and moving on must write nothing.
    #[test]
    fn browsing_writes_no_versions() {
        let dir = Scratch::new("browse");
        let library = Arc::new(Mutex::new(library_at(&dir.0, 3)));
        let session = Arc::new(Mutex::new(Session::new(
            [100, 100],
            64,
            EditState::default(),
            rawkit_editstate::Orientation::AsShot,
        )));
        let mut saver = Saver::new(Some(library.clone()), session.clone());

        for _ in 0..3 {
            saver.restore(&mut session.lock().unwrap());
            library.lock().unwrap().act(CullAction::Next).unwrap();
            library.lock().unwrap().take_request();
            saver.flush();
        }
        let versions: i64 = library
            .lock()
            .unwrap()
            .catalog()
            .connection()
            .query_row("SELECT count(*) FROM edit_states", [], |r| r.get(0))
            .unwrap();
        assert_eq!(versions, 0);
    }
}

/// The marked set, and the two things that use it: comparing, and applying one
/// frame's look to the rest.
#[cfg(test)]
mod marked_set_tests {
    use super::tests::{library_at, Scratch};
    use super::*;

    #[test]
    fn marking_is_a_toggle_and_keeps_shoot_order() {
        let dir = Scratch::new("marks");
        let mut library = library_at(&dir.0, 5);
        for index in [3usize, 1, 4] {
            library.select(index);
            library.act(CullAction::Mark).unwrap();
        }
        assert_eq!(library.marked(), [1, 3, 4], "left to right as shot");

        library.select(3);
        library.act(CullAction::Mark).unwrap();
        assert_eq!(library.marked(), [1, 4], "the same key takes it back out");
    }

    #[test]
    fn judging_in_a_survey_narrows_the_field() {
        // The gesture a survey exists for: "which of these". Each judgement
        // takes one out, so what is left is the question that remains.
        let dir = Scratch::new("winnow");
        let mut library = library_at(&dir.0, 6);
        for index in [0usize, 1, 2] {
            library.select(index);
            library.act(CullAction::Mark).unwrap();
        }
        library.select(0);

        library.act(CullAction::SurveyJudge(false)).unwrap();
        assert_eq!(
            library.marked(),
            [1, 2],
            "the rejected one left the comparison"
        );
        assert_eq!(library.index(), 1, "and the cursor is on what remains");

        library.act(CullAction::SurveyJudge(true)).unwrap();
        assert_eq!(
            library.marked(),
            [2],
            "a keeper leaves it too — both narrow"
        );
        assert_eq!(library.index(), 2);

        library.act(CullAction::SurveyJudge(true)).unwrap();
        assert!(library.marked().is_empty(), "and the comparison is over");
    }

    #[test]
    fn undo_puts_a_frame_back_into_the_comparison() {
        // Otherwise the key that reverses a mistake leaves you looking at a
        // comparison the mistake is missing from — the flag is restored and the
        // frame is still gone, which is half a fix and reads as a broken undo.
        let dir = Scratch::new("survey-undo");
        let mut library = library_at(&dir.0, 4);
        for index in [0usize, 1, 2] {
            library.select(index);
            library.act(CullAction::Mark).unwrap();
        }
        library.select(1);
        library.act(CullAction::SurveyJudge(false)).unwrap();
        assert_eq!(library.marked(), [0, 2]);

        let view = library.act(CullAction::Undo).unwrap();
        assert_eq!(
            library.marked(),
            [0, 1, 2],
            "back where it was being compared"
        );
        assert_eq!(view.flag, None, "and unflagged again");
        assert_eq!(library.index(), 1, "with the cursor on it");
    }

    #[test]
    fn moving_within_a_comparison_wraps_and_ignores_everything_else() {
        // Arrows in a survey move between the frames being compared, not
        // through the shoot — otherwise one press leaves the comparison.
        let dir = Scratch::new("survey-move");
        let mut library = library_at(&dir.0, 8);
        for index in [1usize, 5, 6] {
            library.select(index);
            library.act(CullAction::Mark).unwrap();
        }
        library.select(1);
        library.act(CullAction::SelectMarked(1)).unwrap();
        assert_eq!(library.index(), 5);
        library.act(CullAction::SelectMarked(1)).unwrap();
        assert_eq!(library.index(), 6);
        library.act(CullAction::SelectMarked(1)).unwrap();
        assert_eq!(library.index(), 1, "round the end, not off it");
        library.act(CullAction::SelectMarked(-1)).unwrap();
        assert_eq!(library.index(), 6);
    }

    /// The look one frame carries, as stored.
    fn stored(library: &Library, index: usize) -> Option<EditState> {
        rawkit_catalog::edits::latest(&library.catalog, library.sequence.get(index).unwrap().id)
            .unwrap()
            .map(|(_, state)| state)
    }

    fn a_look() -> EditState {
        let mut state = EditState::default();
        state.tone.exposure_ev = 0.8;
        state.tone.contrast = 0.35;
        state.white_balance.temperature_k = Some(4800.0);
        state
    }

    #[test]
    fn a_pasted_look_leaves_each_frame_its_own_framing() {
        // The decision this slice made: tone and white balance travel, crop and
        // orientation stay. A pasted crop reframes photographs whose composition
        // differs, and previews rebuild quietly, so nobody finds out until the
        // exports are wrong.
        let dir = Scratch::new("paste");
        let mut library = library_at(&dir.0, 3);

        // Frame 2 already has a crop of its own, and must keep it.
        let framed = EditState {
            crop: rawkit_editstate::Crop {
                left: 0.1,
                top: 0.1,
                right: 0.6,
                bottom: 0.6,
                ..rawkit_editstate::Crop::default()
            },
            orientation: rawkit_editstate::Orientation::Rotate90Cw,
            ..EditState::default()
        };
        rawkit_catalog::edits::save(
            &library.catalog,
            library.sequence.get(2).unwrap().id,
            &framed,
            rawkit_editstate::EditSource::User,
        )
        .unwrap();

        library.act(CullAction::SelectBy(1)).unwrap();
        library.act(CullAction::Mark).unwrap();
        library.act(CullAction::SelectBy(1)).unwrap();
        library.act(CullAction::Mark).unwrap();

        let source = a_look();
        library.copy_edit(source.clone());
        assert_eq!(library.paste_into_marked().unwrap(), 2);

        for index in [1, 2] {
            let got = stored(&library, index).expect("a look was pasted");
            assert_eq!(got.tone, source.tone, "frame {index} took the tone");
            assert_eq!(got.white_balance, source.white_balance);
        }
        assert_eq!(
            stored(&library, 2).unwrap().crop,
            framed.crop,
            "its own crop"
        );
        assert_eq!(stored(&library, 2).unwrap().orientation, framed.orientation);
        assert!(
            stored(&library, 0).is_none(),
            "an unmarked frame is untouched"
        );
    }

    #[test]
    fn pasting_twice_writes_one_version() {
        // `edits::save` is a no-op when the hash matches, so a second paste over
        // the same frames costs nothing and reports nothing — which is what
        // stops a sync key filling the history with decisions nobody made.
        let dir = Scratch::new("paste-twice");
        let mut library = library_at(&dir.0, 2);
        library.act(CullAction::Mark).unwrap();
        library.copy_edit(a_look());

        assert_eq!(library.paste_into_marked().unwrap(), 1);
        assert_eq!(library.paste_into_marked().unwrap(), 0, "nothing changed");

        // And an identity look on a frame that has no edit is also nothing:
        // writing it would mark the photograph as edited by a decision nobody
        // made. Found by driving the shell, where cropping one frame and pasting
        // its *look* wrote empty versions to the others.
        library.copy_edit(EditState::default());
        library.act(CullAction::ClearMarks).unwrap();
        library.act(CullAction::SelectBy(1)).unwrap();
        library.act(CullAction::Mark).unwrap();
        assert_eq!(library.paste_into_marked().unwrap(), 0);
        assert!(stored(&library, 1).is_none(), "no version was written");
        let history =
            rawkit_catalog::edits::history(&library.catalog, library.sequence.get(0).unwrap().id)
                .unwrap();
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn undo_puts_back_what_each_frame_had() {
        // One key reverses whichever happened last, so a paste has to be on the
        // same stack as a judgement — and it has to restore *per frame*, because
        // the frames it overwrote did not all start from the same place.
        let dir = Scratch::new("paste-undo");
        let mut library = library_at(&dir.0, 3);

        let mut had = EditState::default();
        had.tone.exposure_ev = -1.5;
        rawkit_catalog::edits::save(
            &library.catalog,
            library.sequence.get(1).unwrap().id,
            &had,
            rawkit_editstate::EditSource::User,
        )
        .unwrap();

        library.act(CullAction::SelectBy(1)).unwrap();
        library.act(CullAction::Mark).unwrap();
        library.act(CullAction::SelectBy(1)).unwrap();
        library.act(CullAction::Mark).unwrap();
        library.copy_edit(a_look());
        library.paste_into_marked().unwrap();

        library.act(CullAction::Undo).unwrap();
        assert_eq!(
            stored(&library, 1).unwrap().tone,
            had.tone,
            "frame 1 as it was"
        );
        assert_eq!(
            stored(&library, 2).unwrap(),
            EditState::default(),
            "frame 2 had no edit, so it goes back to the identity"
        );
    }

    #[test]
    fn pasting_says_what_is_missing_rather_than_doing_nothing() {
        let dir = Scratch::new("paste-refuse");
        let mut library = library_at(&dir.0, 2);

        let no_copy = library.paste_into_marked().unwrap_err().to_string();
        assert!(no_copy.contains("nothing copied"), "{no_copy}");

        library.copy_edit(a_look());
        let no_marks = library.paste_into_marked().unwrap_err().to_string();
        assert!(no_marks.contains("mark the frames"), "{no_marks}");
    }
}
