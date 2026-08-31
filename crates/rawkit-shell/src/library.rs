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

use anyhow::{anyhow, Context, Result};
use rawkit_catalog::cull::{self, Flag, Judgement, LibraryImage};
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
}

impl Loaded {
    /// Decode a RAW, or synthesise one when there is no file to open.
    pub fn open(path: Option<&Path>, tile: u32) -> Result<Self> {
        let (mosaic, size, phase, wb, profile, camera, orientation) = match path {
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
                    // about which way up it is.
                    rawkit_editstate::Orientation::AsShot,
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
                (
                    rawkit_engine::normalise(&raw),
                    size,
                    phase,
                    wb,
                    profile,
                    Some(camera),
                    orientation,
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
#[derive(Debug)]
enum Undone {
    Judged {
        index: usize,
        before: Judgement,
    },
    /// What each frame's edit was before the paste. `None` means it had none —
    /// restoring that writes the identity edit rather than deleting a version,
    /// because the history is append-only and an undo is itself a decision.
    Pasted {
        frames: Vec<(usize, Option<EditState>)>,
    },
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
    /// Take the rectangle that was drawn, or throw it away.
    CropApply,
    CropCancel,
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
}

/// An open catalog and where we are in it.
pub struct Library {
    catalog: Catalog,
    images: Vec<LibraryImage>,
    index: usize,
    /// What each judgement replaced, most recent last.
    ///
    /// Bounded because it is a convenience, not a history: the versioned record
    /// is what `edit_states` is for, and a rating deliberately has none.
    undo: Vec<Undone>,
    /// A navigation the render loop has not acted on yet.
    request: Option<usize>,
    /// Frames set aside to compare against each other, as positions in the
    /// sequence, kept in order so a survey reads left to right the way the shoot
    /// happened.
    marked: Vec<usize>,
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
        let images = cull::sequence(&catalog)?;
        if images.is_empty() {
            return Err(anyhow!(
                "{} has no images; run `rawkit catalog <path> --scan <folder>` first",
                path.display()
            ));
        }
        eprintln!(
            "library    : {} · {} image(s)",
            path.display(),
            images.len()
        );
        Ok(Self {
            catalog,
            images,
            index: 0,
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
        for index in self.marked.clone() {
            let id = self.images[index].id;
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
                undo.push((index, before));
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

    pub fn marked(&self) -> &[usize] {
        &self.marked
    }

    pub fn count(&self) -> usize {
        self.images.len()
    }

    pub fn index(&self) -> usize {
        self.index
    }

    /// Put the selection somewhere without asking the render loop to load it.
    ///
    /// What a grid does: moving across a contact sheet must not decode anything,
    /// because the cell it lands on is already on screen.
    pub fn select(&mut self, index: usize) {
        self.index = index.min(self.images.len() - 1);
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
        let from = from.min(self.images.len());
        let slice = &self.images[from..to.min(self.images.len())];
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
        let Some(image) = self.images.get(index) else {
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
        &self.images[self.index]
    }

    /// Carry out an action and report the new state.
    pub fn act(&mut self, action: CullAction) -> Result<CullView> {
        match action {
            // Resolved before they reach here — they change which view is
            // showing, not which photograph. Listed rather than caught by a
            // wildcard so adding a cull action still fails to compile here,
            // which is what has kept this match honest.
            CullAction::Crop | CullAction::CropApply | CullAction::CropCancel => {}
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
                self.select(target.clamp(0, self.images.len() as i64 - 1) as usize);
            }
            // Saturating, not wrapping: backing off the front of a shoot must
            // stay at the first frame, and `wrapping_sub` would clamp to the
            // *last* one — a silent jump to the far end of the library.
            CullAction::Previous => self.go(self.index.saturating_sub(1)),
            CullAction::Rate(stars) => {
                let rating = (stars > 0).then_some(stars);
                self.judge(|j| Judgement { rating, ..j })?;
            }
            CullAction::Pick => {
                self.judge(|j| Judgement {
                    flag: Some(Flag::Pick),
                    ..j
                })?;
                self.go(self.index + 1);
            }
            CullAction::Reject => {
                self.judge(|j| Judgement {
                    flag: Some(Flag::Reject),
                    ..j
                })?;
                self.go(self.index + 1);
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
            CullAction::Mark => match self.marked.iter().position(|i| *i == self.index) {
                Some(at) => {
                    self.marked.remove(at);
                }
                None => {
                    self.marked.push(self.index);
                    self.marked.sort_unstable();
                }
            },
            CullAction::ClearMarks => self.marked.clear(),
            CullAction::SelectMarked(step) => {
                if !self.marked.is_empty() {
                    let at = self
                        .marked
                        .iter()
                        .position(|i| *i == self.index)
                        .unwrap_or(0) as i64;
                    let next = (at + step as i64).rem_euclid(self.marked.len() as i64);
                    self.index = self.marked[next as usize];
                }
            }
            CullAction::SurveyJudge(keep) => {
                let flag = if keep { Flag::Pick } else { Flag::Reject };
                self.judge(|j| Judgement {
                    flag: Some(flag),
                    ..j
                })?;
                // Out of the comparison, and the cursor lands on whatever is
                // still in it — which is what makes this a winnowing rather
                // than a survey you have to leave and re-enter.
                if let Some(at) = self.marked.iter().position(|i| *i == self.index) {
                    self.marked.remove(at);
                    if let Some(next) = self.marked.get(at).or_else(|| self.marked.last()) {
                        self.index = *next;
                    }
                }
            }
            CullAction::Undo => match self.undo.pop() {
                Some(Undone::Pasted { frames }) => {
                    for (index, before) in &frames {
                        let state = before.clone().unwrap_or_default();
                        rawkit_catalog::edits::save(
                            &self.catalog,
                            self.images[*index].id,
                            &state,
                            rawkit_editstate::EditSource::User,
                        )?;
                    }
                    // Back to a frame it touched, so the reversal is visible
                    // rather than something the user has to go and check.
                    if let Some((index, _)) = frames.first() {
                        self.go(*index);
                        self.index = *index;
                    }
                }
                Some(Undone::Judged {
                    index,
                    before: previous,
                }) => {
                    cull::set(&self.catalog, self.images[index].id, &previous)?;
                    // A survey drops what it judges, so undoing a judgement has
                    // to put the frame back where it was being compared —
                    // otherwise the key that reverses a mistake leaves you
                    // looking at a comparison the mistake is missing from.
                    if !self.marked.is_empty() && !self.marked.contains(&index) {
                        self.marked.push(index);
                        self.marked.sort_unstable();
                    }
                    self.go(index);
                    self.index = index;
                }
                None => {}
            },
        }
        self.view()
    }

    /// Apply a change to the current image's judgement, remembering what it
    /// replaced.
    fn judge(&mut self, change: impl FnOnce(Judgement) -> Judgement) -> Result<()> {
        let id = self.current().id;
        let before = cull::judgement(&self.catalog, id)?;
        let after = change(before.clone());
        if after == before {
            return Ok(());
        }
        cull::set(&self.catalog, id, &after)?;
        self.undo.push(Undone::Judged {
            index: self.index,
            before,
        });
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
        Ok(())
    }

    /// Move to an image, clamped to the ends.
    ///
    /// Stopping at the last frame rather than wrapping: a cull is a pass through
    /// a shoot, and silently starting again is how a frame gets judged twice
    /// while its neighbour is missed.
    fn go(&mut self, index: usize) {
        let index = index.min(self.images.len() - 1);
        if index == self.index {
            return;
        }
        self.index = index;
        self.request = Some(index);
    }

    /// The photograph the render loop should now be showing, if it changed.
    pub fn take_request(&mut self) -> Option<String> {
        let index = self.request.take()?;
        Some(self.images[index].path.clone())
    }

    pub fn view(&self) -> Result<CullView> {
        let image = self.current();
        let judgement = cull::judgement(&self.catalog, image.id)?;
        let (_, picks, rejects) = cull::tally(&self.catalog)?;
        Ok(CullView {
            filename: image.filename.clone(),
            position: self.index + 1,
            total: self.images.len(),
            rating: judgement.rating,
            flag: judgement.flag.map(Flag::column),
            colour: judgement.colour,
            picks,
            rejects,
            undoable: !self.undo.is_empty(),
            copied: self.copied.is_some(),
            marked: self.marked.len(),
            is_marked: self.marked.contains(&self.index),
            mode: crate::mode_name(),
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
mod tests {
    use super::*;
    use rawkit_catalog::scan::FileMetadata;

    /// A catalog of `n` raws, taken in the order their names sort.
    ///
    /// Real files on disk, because a scan walks a directory — but never decoded:
    /// everything here is about which image is *selected*, and selecting one
    /// costs nothing. Loading it is the render loop's job and needs a GPU.
    pub(super) fn library_at(dir: &Path, n: usize) -> Library {
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
    pub(super) struct Scratch(pub std::path::PathBuf);

    impl Scratch {
        pub(super) fn new(name: &str) -> Self {
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
        rawkit_catalog::edits::latest(&library.catalog, library.images[index].id)
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
            library.images[2].id,
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
            rawkit_catalog::edits::history(&library.catalog, library.images[0].id).unwrap();
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
            library.images[1].id,
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
