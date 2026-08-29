//! The command bus — the seam between the user interface and the engine.
//!
//! # The rule this crate exists to make structural
//!
//! > **React never touches the hot path.** The engine owns the canvas; the UI
//! > sends intent and subscribes to state.
//!
//! That rule is easy to state, easy to agree with, and easy to violate by
//! accident — because routing pixels through the webview *works fine* on a
//! 2 MP preview and falls apart at 24 MP. Every other invariant in this project
//! is protected by something mechanical (pipeline order is a type, licences are
//! `cargo-deny`, cross-platform agreement is the golden harness). This one was
//! protected by intent alone. This crate is the mechanism.
//!
//! The mechanism is a subtraction: **a [`Session`] has no pixel type and no GPU
//! handle.** It cannot send a frame to the UI because it has nothing to send. It
//! decides *what* should be rendered — which tiles, at which resolution, for
//! which edit — and the engine, on the other side of the boundary, does the
//! rendering into a surface the UI never sees. A future contributor who wants to
//! ship pixels over the bus has to add a dependency to do it, which is a
//! reviewable act rather than a quiet one.
//!
//! # Why there is no queue
//!
//! A slider drag emits commands far faster than a GPU renders. The usual
//! reaction is a queue plus a coalescing rule, and the usual bug is a queue that
//! grows under load until the image lags the cursor by a second.
//!
//! There is no queue here. [`Session::apply`] mutates state immediately and
//! bumps a generation counter; [`Session::pending_work`] reports what is stale
//! *right now*. A thousand commands between two frames leave exactly one thing
//! to render, because intermediate states were never recorded anywhere. That is
//! coalescing by construction — the strongest kind, since there is no policy to
//! get wrong.
//!
//! # Why tiles carry a level
//!
//! Fit-to-screen on a 24 MP image is a ~0.06 scale factor: rendering every pixel
//! would do 256× the work the screen can show. [`TileId`] therefore names a
//! resolution level as well as a position, and [`Viewport::level`] picks the
//! coarsest level that still meets screen resolution.
//!
//! This is the same reasoning that made tiling unconditional in the engine.
//! Level is part of a tile's *identity*, not a parameter beside it, because a
//! cache keyed on position alone cannot hold two resolutions of the same region
//! — and discovering that later means rewriting the cache rather than extending
//! it.
//!
//! # What is not connected yet
//!
//! A [`RenderJob`] is executable: `rawkit_engine::Renderer::render_tile` takes a
//! level and a tile position and draws exactly that, and a level-0 tile is
//! bit-identical to the same region of a whole-image render.
//!
//! What is missing is **presentation**. That render reads its pixels back to the
//! CPU, which is right for export and wrong for a canvas; the interactive path
//! has to keep the result on the GPU and blit it to a surface. A surface needs a
//! window, which is the Tauri shell — so until that exists, this crate has no
//! end-to-end test that closes the loop, and one built against a mock canvas
//! would be measuring the mock.
//!
//! The bus was deliberately first. It constrains the other two, and it is the
//! only part that can be fully tested before a window exists.

use rawkit_editstate::{EditState, Orientation};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Temperatures the renderer will accept, in Kelvin.
///
/// The bounds exist because the camera profile interpolates in mireds
/// (reciprocal megakelvin), so a temperature at or near zero is a division by
/// zero and a very large one is unbounded extrapolation from two measured
/// illuminants. The range is wider than any UI should offer and narrow enough
/// that the arithmetic stays meaningful.
pub const TEMPERATURE_RANGE_K: std::ops::RangeInclusive<f32> = 1000.0..=50_000.0;

/// What the user asked for. The only way to change a [`Session`].
///
/// One variant per thing the UI can do, rather than a generic patch, for the
/// same reason `EditState` refuses unknown fields: the set of things the editor
/// can be told to do should be enumerable by reading a type.
///
/// Adjacently tagged rather than internally tagged, because serde cannot
/// internally tag a newtype variant holding a bare `f32` — and a slider command
/// *is* a bare number. The wire form is `{"command":"set_exposure","params":-0.75}`,
/// which is also the shape a JavaScript caller writes most naturally.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", content = "params", rename_all = "snake_case")]
pub enum Command {
    SetExposure(f32),
    SetContrast(f32),
    SetHighlights(f32),
    SetShadows(f32),
    SetWhites(f32),
    SetBlacks(f32),
    /// `None` restores the camera's own white balance. Distinct from any
    /// numeric value, because as-shot differs per file.
    SetTemperature(Option<f32>),
    SetTint(f32),
    SetOrientation(Orientation),
    /// Replace the edit wholesale — preset application, undo/redo, or loading a
    /// state from the catalog. Boxed to keep [`Command`] small; see
    /// `commands_and_events_stay_small`.
    SetEditState(Box<EditState>),

    /// Drag the image by a distance in *screen* pixels. Screen rather than image
    /// pixels because that is what a pointer produces, and converting at the
    /// boundary keeps the conversion in one place.
    Pan {
        dx: f64,
        dy: f64,
    },
    /// Zoom to an absolute scale, holding the image point under `anchor` fixed.
    /// `anchor` is in screen pixels from the canvas's top-left.
    ZoomTo {
        scale: f64,
        anchor: [f64; 2],
    },
    /// The canvas changed size — a window resize, or a panel opening.
    Resize {
        width: u32,
        height: u32,
    },
    /// Scale and centre so the whole image is visible.
    FitToView,
}

impl Command {
    /// A stable name for logging and for [`Event::Refused`]. Not the `Debug`
    /// output, which would leak parameter values into UI text.
    pub fn name(&self) -> &'static str {
        match self {
            Command::SetExposure(_) => "set_exposure",
            Command::SetContrast(_) => "set_contrast",
            Command::SetHighlights(_) => "set_highlights",
            Command::SetShadows(_) => "set_shadows",
            Command::SetWhites(_) => "set_whites",
            Command::SetBlacks(_) => "set_blacks",
            Command::SetTemperature(_) => "set_temperature",
            Command::SetTint(_) => "set_tint",
            Command::SetOrientation(_) => "set_orientation",
            Command::SetEditState(_) => "set_edit_state",
            Command::Pan { .. } => "pan",
            Command::ZoomTo { .. } => "zoom_to",
            Command::Resize { .. } => "resize",
            Command::FitToView => "fit_to_view",
        }
    }
}

/// What the session tells the UI. Deliberately incapable of carrying an image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The edit changed, so everything on screen is stale. Carries the
    /// generation so a UI can tell its own echo from someone else's change.
    EditChanged { generation: u64 },
    /// The view moved or resized. The edit is unchanged, so already-rendered
    /// tiles stay valid — this is why panning is cheap and a slider is not.
    ViewChanged,
    /// A command was rejected, with a reason fit to show a person.
    ///
    /// Refusing beats clamping: a clamped value means the number the UI displays
    /// and the number the renderer used have quietly diverged, which is the
    /// failure mode the as-shot temperature readout was designed to avoid.
    Refused {
        command: &'static str,
        reason: String,
    },
}

/// A rectangular region of the image at one resolution level.
///
/// Level 0 is full resolution. Level *n* covers `tile << n` image pixels and
/// renders them into `tile` output pixels, so every level costs the same work
/// per tile — which is what makes zooming out cheap instead of expensive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TileId {
    pub level: u8,
    pub x: u32,
    pub y: u32,
}

/// Where the canvas is looking.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    /// The image pixel at the centre of the canvas.
    pub center: [f64; 2],
    /// Screen pixels per image pixel. 1.0 is a 1:1 view; 0.5 is zoomed out.
    pub scale: f64,
    /// Canvas size in screen pixels.
    pub size: [u32; 2],
}

impl Viewport {
    /// The image point under a screen point, both in their own pixels.
    pub fn image_at(&self, screen: [f64; 2]) -> [f64; 2] {
        [
            self.center[0] + (screen[0] - self.size[0] as f64 / 2.0) / self.scale,
            self.center[1] + (screen[1] - self.size[1] as f64 / 2.0) / self.scale,
        ]
    }

    /// The visible region in image pixels, as `[x0, y0, x1, y1]`, clamped to the
    /// image. Empty (`x1 <= x0`) when the view is entirely off the image.
    pub fn visible_rect(&self, image: [u32; 2]) -> [f64; 4] {
        let top_left = self.image_at([0.0, 0.0]);
        let bottom_right = self.image_at([self.size[0] as f64, self.size[1] as f64]);
        [
            top_left[0].max(0.0),
            top_left[1].max(0.0),
            bottom_right[0].min(image[0] as f64),
            bottom_right[1].min(image[1] as f64),
        ]
    }

    /// The coarsest resolution level that still has at least one image sample
    /// per screen pixel.
    ///
    /// At `scale >= 1` (1:1 or magnified) that is level 0. At 0.5 it is level 1,
    /// at 0.25 level 2, and so on — each level halves linear resolution, so
    /// fit-to-screen on a 24 MP image lands around level 4 and renders roughly
    /// 1/256 of the pixels. Rounding *down* to the finer level is deliberate:
    /// showing a slightly sharper image than needed is invisible, showing a
    /// blurrier one is the thing users notice immediately.
    pub fn level(&self, max_level: u8) -> u8 {
        if !self.scale.is_finite() || self.scale >= 1.0 {
            return 0;
        }
        // scale <= 0 cannot happen (Session refuses it) but must not panic here.
        if self.scale <= 0.0 {
            return max_level;
        }
        let level = (1.0 / self.scale).log2().floor();
        (level.max(0.0) as u8).min(max_level)
    }
}

/// The work the engine should do next.
///
/// The edit is carried *in* the job rather than read separately from the
/// session, so a caller cannot render tiles with one state while the session has
/// moved to another. This is the same lesson the camera profile taught: when two
/// values are only correct together, hand them over together.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderJob {
    /// The edit generation these tiles belong to. Pass it back to
    /// [`Session::tile_rendered`]; a tile finished after the edit moved on is
    /// discarded rather than cached against the wrong state.
    pub generation: u64,
    pub state: EditState,
    /// Stale visible tiles, nearest the centre of the view first, so refinement
    /// arrives where the user is looking rather than at a corner.
    pub tiles: Vec<TileId>,
}

impl RenderJob {
    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }
}

/// One open image, its edit, and where the canvas is looking.
///
/// Holds no pixels, no GPU handle and no decoded frame — only the sizes and the
/// state needed to decide what to render.
pub struct Session {
    image: [u32; 2],
    tile: u32,
    max_level: u8,
    state: EditState,
    viewport: Viewport,
    generation: u64,
    /// Tile → the edit generation it was last rendered at. A tile is fresh when
    /// that equals `generation`.
    ///
    /// Never evicted, and it does not need to be: the map is bounded by the
    /// total tile count across every level, which for a 24 MP image at a 512
    /// tile is about 125 entries.
    rendered: HashMap<TileId, u64>,
}

impl Session {
    /// # Panics
    ///
    /// If `tile` is odd, or either image dimension is zero. An odd tile is
    /// refused for the same reason the engine refuses one: tile origins must
    /// land on the same CFA phase as the image, and an odd tile guarantees they
    /// do not. The two constants are checked against each other in
    /// `tile_size_matches_the_engine`.
    pub fn new(image: [u32; 2], tile: u32, state: EditState) -> Self {
        assert!(tile > 0 && tile % 2 == 0, "tile size must be even");
        assert!(
            image[0] > 0 && image[1] > 0,
            "an image with no pixels has nothing to render"
        );
        let max_level = max_level(image, tile);
        let mut session = Self {
            image,
            tile,
            max_level,
            state,
            viewport: Viewport {
                center: [image[0] as f64 / 2.0, image[1] as f64 / 2.0],
                scale: 1.0,
                size: [0, 0],
            },
            generation: 1,
            rendered: HashMap::new(),
        };
        // Start fitted. A canvas of zero size shows nothing until the first
        // Resize, which is exactly what a window that has not been laid out yet
        // should do.
        session.fit_to_view();
        session
    }

    pub fn state(&self) -> &EditState {
        &self.state
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The image size this session was opened with.
    pub fn image_size(&self) -> [u32; 2] {
        self.image
    }

    /// Apply one command. Never blocks and never renders.
    pub fn apply(&mut self, command: Command) -> Event {
        // Taken before the match, because `SetEditState` moves its payload out
        // of `command` and the refusal path still needs the name.
        let name = command.name();
        match command {
            Command::SetExposure(v) => self.edit(name, v, |s, v| s.tone.exposure_ev = v),
            Command::SetContrast(v) => self.edit(name, v, |s, v| s.tone.contrast = v),
            Command::SetHighlights(v) => self.edit(name, v, |s, v| s.tone.highlights = v),
            Command::SetShadows(v) => self.edit(name, v, |s, v| s.tone.shadows = v),
            Command::SetWhites(v) => self.edit(name, v, |s, v| s.tone.whites = v),
            Command::SetBlacks(v) => self.edit(name, v, |s, v| s.tone.blacks = v),
            Command::SetTint(v) => self.edit(name, v, |s, v| s.white_balance.tint = v),

            Command::SetTemperature(None) => {
                self.state.white_balance.temperature_k = None;
                self.edit_changed()
            }
            Command::SetTemperature(Some(k)) => {
                if !k.is_finite() {
                    return refused(name, "temperature must be a finite number");
                }
                if !TEMPERATURE_RANGE_K.contains(&k) {
                    return refused(
                        name,
                        format!(
                            "{k:.0} K is outside {:.0}–{:.0} K",
                            TEMPERATURE_RANGE_K.start(),
                            TEMPERATURE_RANGE_K.end()
                        ),
                    );
                }
                self.state.white_balance.temperature_k = Some(k);
                self.edit_changed()
            }

            Command::SetOrientation(o) => {
                self.state.orientation = o;
                self.edit_changed()
            }

            Command::SetEditState(next) => {
                // The one command that can carry a foreign state, so it is the
                // one that has to check. `validate` refuses a schema version
                // this build cannot faithfully render.
                if let Err(e) = next.validate() {
                    return refused(name, e.to_string());
                }
                if !edit_is_finite(&next) {
                    return refused(name, "edit contains a non-finite value");
                }
                self.state = *next;
                self.edit_changed()
            }

            Command::Pan { dx, dy } => {
                if !dx.is_finite() || !dy.is_finite() {
                    return refused(name, "pan distance must be finite");
                }
                // Dragging right moves the image right, so the centre moves left.
                let scale = self.viewport.scale;
                self.viewport.center[0] -= dx / scale;
                self.viewport.center[1] -= dy / scale;
                self.clamp_center();
                Event::ViewChanged
            }

            Command::ZoomTo { scale, anchor } => {
                if !scale.is_finite() || scale <= 0.0 {
                    return refused(name, "scale must be a positive number");
                }
                if !anchor[0].is_finite() || !anchor[1].is_finite() {
                    return refused(name, "anchor must be finite");
                }
                // Hold the image point under the anchor still. Without this,
                // zooming drifts toward the centre and the thing being examined
                // slides out from under the cursor.
                let fixed = self.viewport.image_at(anchor);
                self.viewport.scale = scale;
                let half = [
                    self.viewport.size[0] as f64 / 2.0,
                    self.viewport.size[1] as f64 / 2.0,
                ];
                self.viewport.center = [
                    fixed[0] - (anchor[0] - half[0]) / scale,
                    fixed[1] - (anchor[1] - half[1]) / scale,
                ];
                self.clamp_center();
                Event::ViewChanged
            }

            Command::Resize { width, height } => {
                self.viewport.size = [width, height];
                self.clamp_center();
                Event::ViewChanged
            }

            Command::FitToView => {
                self.fit_to_view();
                Event::ViewChanged
            }
        }
    }

    /// The stale visible tiles, or an empty job when the canvas is up to date.
    ///
    /// Idempotent: calling it twice without an intervening
    /// [`Session::tile_rendered`] returns the same work, because nothing has
    /// been rendered yet. It is the render loop, not this call, that decides how
    /// much of the job to do per frame.
    pub fn pending_work(&self) -> RenderJob {
        let level = self.viewport.level(self.max_level);
        let mut tiles = self.visible_tiles(level);
        tiles.retain(|t| self.rendered.get(t) != Some(&self.generation));

        // Centre-out, so refinement appears where the user is looking. Ties
        // broken by TileId so the order is deterministic — a render order that
        // varies run to run makes a progressive-refinement bug unreproducible.
        let centre = self.viewport.center;
        let span = (self.tile << level) as f64;
        tiles.sort_by(|a, b| {
            let d = |t: &TileId| {
                let cx = (t.x as f64 + 0.5) * span - centre[0];
                let cy = (t.y as f64 + 0.5) * span - centre[1];
                cx * cx + cy * cy
            };
            d(a).total_cmp(&d(b)).then_with(|| a.cmp(b))
        });

        RenderJob {
            generation: self.generation,
            state: self.state.clone(),
            tiles,
        }
    }

    /// Record that a tile has been drawn. Ignores tiles from a superseded
    /// generation, which is what makes a slow render harmless rather than
    /// corrupting: the pixels are simply thrown away.
    pub fn tile_rendered(&mut self, tile: TileId, generation: u64) {
        if generation == self.generation {
            self.rendered.insert(tile, generation);
        }
    }

    /// Every tile intersecting the visible region at `level`.
    pub fn visible_tiles(&self, level: u8) -> Vec<TileId> {
        let rect = self.viewport.visible_rect(self.image);
        if rect[2] <= rect[0] || rect[3] <= rect[1] {
            return Vec::new();
        }
        let span = (self.tile << level) as f64;
        let tiles_x = (self.image[0] as f64 / span).ceil() as u32;
        let tiles_y = (self.image[1] as f64 / span).ceil() as u32;

        let x0 = (rect[0] / span).floor().max(0.0) as u32;
        let y0 = (rect[1] / span).floor().max(0.0) as u32;
        // `- 1e-9` so a rect ending exactly on a tile boundary does not pull in
        // the next tile, which would render a column of tiles nobody can see.
        let x1 = ((rect[2] - 1e-9) / span).floor().max(0.0) as u32;
        let y1 = ((rect[3] - 1e-9) / span).floor().max(0.0) as u32;

        let mut tiles = Vec::new();
        for y in y0..=y1.min(tiles_y.saturating_sub(1)) {
            for x in x0..=x1.min(tiles_x.saturating_sub(1)) {
                tiles.push(TileId { level, x, y });
            }
        }
        tiles
    }

    /// The coarsest level available for this image — one tile covers everything.
    pub fn max_level(&self) -> u8 {
        self.max_level
    }

    fn edit<T: Copy + Into<f64>>(
        &mut self,
        name: &'static str,
        value: T,
        set: impl FnOnce(&mut EditState, T),
    ) -> Event {
        // Non-finite values are the classic gift from a numeric bridge: they do
        // not error, they render black. Catch them at the boundary, where there
        // is still a name to put in the message.
        if !value.into().is_finite() {
            return refused(name, "value must be a finite number");
        }
        set(&mut self.state, value);
        self.edit_changed()
    }

    fn edit_changed(&mut self) -> Event {
        self.generation += 1;
        // Every tile is stale, at every level. Clearing beats marking: the
        // generation check would already reject these entries, and holding them
        // only costs memory.
        self.rendered.clear();
        Event::EditChanged {
            generation: self.generation,
        }
    }

    fn fit_to_view(&mut self) {
        let [w, h] = self.viewport.size;
        self.viewport.center = [self.image[0] as f64 / 2.0, self.image[1] as f64 / 2.0];
        if w == 0 || h == 0 {
            return;
        }
        self.viewport.scale =
            (w as f64 / self.image[0] as f64).min(h as f64 / self.image[1] as f64);
    }

    /// Keep the centre on the image, so the photo cannot be flung off screen.
    fn clamp_center(&mut self) {
        self.viewport.center[0] = self.viewport.center[0].clamp(0.0, self.image[0] as f64);
        self.viewport.center[1] = self.viewport.center[1].clamp(0.0, self.image[1] as f64);
    }
}

fn refused(command: &'static str, reason: impl Into<String>) -> Event {
    Event::Refused {
        command,
        reason: reason.into(),
    }
}

/// Levels from full resolution down to the one where a single tile covers the
/// whole image. Beyond that there is nothing left to halve.
fn max_level(image: [u32; 2], tile: u32) -> u8 {
    let mut level = 0u8;
    let mut span = tile as u64;
    while span < image[0].max(image[1]) as u64 && level < u8::MAX {
        span *= 2;
        level += 1;
    }
    level
}

/// Every float in an edit, checked in one place.
///
/// Listed field by field rather than derived, so that adding a field to
/// `EditState` without deciding whether it can be NaN is a compile error rather
/// than a black tile.
fn edit_is_finite(state: &EditState) -> bool {
    let t = &state.tone;
    let wb = &state.white_balance;
    [
        t.exposure_ev,
        t.contrast,
        t.highlights,
        t.shadows,
        t.whites,
        t.blacks,
        wb.tint,
    ]
    .iter()
    .all(|v| v.is_finite())
        && wb.temperature_k.is_none_or(|k| k.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 24 MP frame, the size the design targets.
    const IMAGE: [u32; 2] = [6000, 4000];
    const TILE: u32 = 512;

    fn session() -> Session {
        let mut s = Session::new(IMAGE, TILE, EditState::default());
        s.apply(Command::Resize {
            width: 1600,
            height: 1000,
        });
        s
    }

    #[test]
    fn a_thousand_slider_commands_leave_one_thing_to_render() {
        let mut s = session();
        s.apply(Command::ZoomTo {
            scale: 1.0,
            anchor: [800.0, 500.0],
        });
        let expected = s.pending_work().tiles.len();

        for i in 0..1000 {
            s.apply(Command::SetExposure(i as f32 / 1000.0));
        }

        let job = s.pending_work();
        assert_eq!(
            job.tiles.len(),
            expected,
            "a drag must leave the same work as a single move, not 1000x it"
        );
        assert!(
            (job.state.tone.exposure_ev - 0.999).abs() < 1e-6,
            "only the final state matters; intermediates were never recorded"
        );
    }

    #[test]
    fn panning_keeps_already_rendered_tiles() {
        let mut s = session();
        s.apply(Command::ZoomTo {
            scale: 1.0,
            anchor: [800.0, 500.0],
        });
        let job = s.pending_work();
        let generation = job.generation;
        for tile in &job.tiles {
            s.tile_rendered(*tile, generation);
        }
        assert!(s.pending_work().is_empty(), "nothing left after rendering");

        // A nudge smaller than a tile must not invalidate the whole view.
        s.apply(Command::Pan {
            dx: -300.0,
            dy: 0.0,
        });
        let after = s.pending_work();
        assert!(
            !after.is_empty() && after.tiles.len() < job.tiles.len(),
            "a pan should need the newly exposed tiles and no more, got {} of {}",
            after.tiles.len(),
            job.tiles.len()
        );
        assert_eq!(after.generation, generation, "panning is not an edit");
    }

    #[test]
    fn an_edit_invalidates_everything() {
        let mut s = session();
        let job = s.pending_work();
        for tile in &job.tiles {
            s.tile_rendered(*tile, job.generation);
        }
        assert!(s.pending_work().is_empty());

        s.apply(Command::SetExposure(0.5));
        let after = s.pending_work();
        assert_eq!(
            after.tiles.len(),
            job.tiles.len(),
            "an edit changes every pixel, so every visible tile is stale"
        );
        assert_ne!(after.generation, job.generation);
    }

    #[test]
    fn a_late_tile_from_a_stale_edit_is_discarded() {
        let mut s = session();
        let job = s.pending_work();
        let tile = job.tiles[0];

        // The edit moves on while that tile is still on the GPU.
        s.apply(Command::SetExposure(1.0));
        s.tile_rendered(tile, job.generation);

        assert!(
            s.pending_work().tiles.contains(&tile),
            "pixels for a superseded edit must not satisfy the new one"
        );
    }

    #[test]
    fn zoom_holds_the_point_under_the_cursor() {
        let mut s = session();
        let anchor = [1500.0, 100.0]; // a corner, where drift is most visible
        let before = s.viewport().image_at(anchor);

        s.apply(Command::ZoomTo { scale: 2.0, anchor });
        let after = s.viewport().image_at(anchor);

        assert!(
            (before[0] - after[0]).abs() < 1e-6 && (before[1] - after[1]).abs() < 1e-6,
            "the image point under the cursor moved: {before:?} -> {after:?}"
        );
    }

    #[test]
    fn fit_shows_the_whole_image_and_zooming_out_costs_less() {
        let mut s = session();
        s.apply(Command::FitToView);
        let fitted = s.viewport();
        assert!(
            fitted.scale < 0.3,
            "24 MP in a 1600px canvas is a big reduction, got {}",
            fitted.scale
        );
        let rect = fitted.visible_rect(IMAGE);
        assert!(
            rect[0] <= 0.0 && rect[1] <= 0.0 && rect[2] >= IMAGE[0] as f64 - 1.0,
            "fit must show the whole image, got {rect:?}"
        );

        let fit_work = s.pending_work().tiles.len();
        s.apply(Command::ZoomTo {
            scale: 1.0,
            anchor: [800.0, 500.0],
        });
        let full_work = s.pending_work().tiles.len();
        assert!(
            fit_work <= full_work,
            "zoomed out must not cost more tiles than 1:1: {fit_work} vs {full_work}"
        );
        // The real claim: fitting renders a fraction of the image's pixels.
        assert!(
            fit_work < 20,
            "fit-to-screen should be a handful of coarse tiles, got {fit_work}"
        );
    }

    #[test]
    fn level_follows_zoom() {
        let mut v = Viewport {
            center: [0.0, 0.0],
            scale: 1.0,
            size: [100, 100],
        };
        let max = max_level(IMAGE, TILE);
        assert_eq!(v.level(max), 0, "1:1 renders at full resolution");
        v.scale = 4.0;
        assert_eq!(v.level(max), 0, "magnifying never needs more than level 0");
        v.scale = 0.5;
        assert_eq!(v.level(max), 1);
        v.scale = 0.25;
        assert_eq!(v.level(max), 2);
        v.scale = 0.3;
        assert_eq!(v.level(max), 1, "rounds toward the sharper level");
        v.scale = 1e-9;
        assert_eq!(
            v.level(max),
            max,
            "clamped to the coarsest level that exists"
        );
    }

    #[test]
    fn refinement_starts_at_the_centre_of_the_view() {
        let mut s = session();
        s.apply(Command::ZoomTo {
            scale: 1.0,
            anchor: [800.0, 500.0],
        });
        let job = s.pending_work();
        assert!(
            job.tiles.len() > 4,
            "need several tiles for order to matter"
        );

        let centre = s.viewport().center;
        let span = (TILE << job.tiles[0].level) as f64;
        let distance = |t: &TileId| {
            let cx = (t.x as f64 + 0.5) * span - centre[0];
            let cy = (t.y as f64 + 0.5) * span - centre[1];
            cx * cx + cy * cy
        };
        let first = distance(&job.tiles[0]);
        let last = distance(job.tiles.last().unwrap());
        assert!(
            first < last,
            "the first tile rendered should be the one being looked at"
        );
    }

    #[test]
    fn non_finite_values_are_refused_not_rendered() {
        let mut s = session();
        let before = s.generation();

        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let event = s.apply(Command::SetExposure(bad));
            assert!(
                matches!(event, Event::Refused { .. }),
                "{bad} must be refused, got {event:?}"
            );
        }
        assert_eq!(s.generation(), before, "a refused command changes nothing");
        assert_eq!(s.state().tone.exposure_ev, 0.0);
    }

    #[test]
    fn temperature_outside_the_usable_range_is_refused_rather_than_clamped() {
        let mut s = session();

        let event = s.apply(Command::SetTemperature(Some(0.0)));
        assert!(
            matches!(event, Event::Refused { .. }),
            "0 K divides by zero"
        );
        assert_eq!(
            s.state().white_balance.temperature_k,
            None,
            "refusing must not half-apply: the value stays as-shot"
        );

        assert!(matches!(
            s.apply(Command::SetTemperature(Some(100_000.0))),
            Event::Refused { .. }
        ));

        assert!(matches!(
            s.apply(Command::SetTemperature(Some(5200.0))),
            Event::EditChanged { .. }
        ));
        assert_eq!(s.state().white_balance.temperature_k, Some(5200.0));

        // As-shot is always available, and is not a temperature.
        assert!(matches!(
            s.apply(Command::SetTemperature(None)),
            Event::EditChanged { .. }
        ));
        assert_eq!(s.state().white_balance.temperature_k, None);
    }

    #[test]
    fn a_state_from_the_future_is_refused() {
        let mut s = session();
        let future = EditState {
            schema_version: rawkit_editstate::SCHEMA_VERSION + 1,
            ..Default::default()
        };
        let event = s.apply(Command::SetEditState(Box::new(future)));
        assert!(
            matches!(event, Event::Refused { .. }),
            "the bus is a trust boundary; a newer schema must not be guessed at"
        );
        assert_eq!(s.state().schema_version, rawkit_editstate::SCHEMA_VERSION);
    }

    #[test]
    fn a_wholesale_state_with_a_nan_is_refused() {
        let mut s = session();
        let mut bad = EditState::default();
        bad.tone.contrast = f32::NAN;
        assert!(matches!(
            s.apply(Command::SetEditState(Box::new(bad))),
            Event::Refused { .. }
        ));
        assert!(s.state().tone.contrast.is_finite());
    }

    #[test]
    fn the_image_cannot_be_flung_off_screen() {
        let mut s = session();
        s.apply(Command::ZoomTo {
            scale: 1.0,
            anchor: [800.0, 500.0],
        });
        s.apply(Command::Pan { dx: 1e9, dy: -1e9 });
        let rect = s.viewport().visible_rect(IMAGE);
        assert!(
            rect[2] > rect[0] && rect[3] > rect[1],
            "some of the image must remain visible, got {rect:?}"
        );
        assert!(
            !s.pending_work().is_empty(),
            "and therefore something to draw"
        );
    }

    #[test]
    fn a_canvas_with_no_size_asks_for_no_work() {
        // A window that has not been laid out yet. Rendering into it would be
        // work for nobody.
        let s = Session::new(IMAGE, TILE, EditState::default());
        assert_eq!(s.viewport().size, [0, 0]);
        assert!(s.pending_work().is_empty());
    }

    /// The rule, made mechanical. `Command` and `Event` cross a process-ish
    /// boundary many times a second; if either could hold an image, someone
    /// eventually would put one there.
    #[test]
    fn commands_and_events_stay_small() {
        let command = std::mem::size_of::<Command>();
        let event = std::mem::size_of::<Event>();
        assert!(
            command <= 32,
            "Command grew to {command} bytes — is something carrying pixels?"
        );
        assert!(
            event <= 48,
            "Event grew to {event} bytes — is something carrying pixels?"
        );
    }

    /// The bus is the boundary the UI talks across, so its vocabulary has to
    /// survive a JSON round trip. A command that cannot be named in JSON is a
    /// command the UI cannot send.
    #[test]
    fn commands_round_trip_as_json() {
        let commands = [
            Command::SetExposure(-0.75),
            Command::SetTemperature(Some(5200.0)),
            Command::SetTemperature(None),
            Command::SetOrientation(Orientation::Rotate90Cw),
            Command::Pan { dx: 3.0, dy: -4.0 },
            Command::ZoomTo {
                scale: 2.0,
                anchor: [10.0, 20.0],
            },
            Command::FitToView,
        ];
        for command in commands {
            let json = serde_json::to_string(&command).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(command, back, "{json} did not survive the round trip");
        }
    }

    #[test]
    fn levels_bottom_out_when_one_tile_covers_the_image() {
        assert_eq!(max_level([512, 512], 512), 0, "already one tile");
        assert_eq!(max_level([513, 512], 512), 1);
        assert_eq!(max_level([6000, 4000], 512), 4, "512 << 4 = 8192 >= 6000");
    }

    /// Both crates size work by the tile, so a disagreement would show up as a
    /// render that is subtly the wrong shape rather than as an error. Checked
    /// against the engine's own constant rather than a copy of its value, which
    /// is the difference between a test and a restatement.
    #[test]
    fn tile_size_matches_the_engine() {
        assert_eq!(TILE, rawkit_engine::render::DEFAULT_TILE);
    }
}
