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
//! Tiles are GPU-resident too: `Renderer::draw_tile` writes into a `Canvas`
//! texture and returns without synchronising, so a frame costs one submission
//! rather than one device stall per tile.
//!
//! What is missing is the last hop — **blitting that canvas to a window**. A
//! surface needs a window, which is the Tauri shell, so until that exists this
//! crate has no end-to-end test that closes the loop; one built against a mock
//! canvas would be measuring the mock.
//!
//! The bus was deliberately first. It constrains the other two, and it is the
//! only part that can be fully tested before a window exists.

use rawkit_editstate::{Crop, EditState, Geometry, Orientation};
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

/// How many points the edit history keeps.
///
/// An `EditState` is 84 bytes, so this is about seventeen kilobytes — the bound
/// exists because an unbounded history in a long session grows without a limit,
/// not because the memory is scarce. Two hundred steps is far more than a
/// photograph accumulates in one sitting, and a drag is one of them.
const MAX_STEPS: usize = 200;

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
    /// The visible rectangle, as fractions of the oriented frame.
    SetCrop(Crop),
    /// Capture sharpening, and how wide its blur is. Refused out of range,
    /// because a radius past what the tile halo covers reads demosaic output
    /// that is wrong at a tile edge — a faint grid, not an obvious failure.
    SetSharpen(f32),
    SetSharpenRadius(f32),
    /// Smooth colour without touching luminance.
    SetChromaNoise(f32),
    /// Smooth brightness while sparing edges. Costs detail, unlike the chroma
    /// kind, which is why it is off unless asked for.
    SetLuminanceNoise(f32),
    /// Every colour equally, and the one that spares the vivid ones.
    SetSaturation(f32),
    SetVibrance(f32),
    /// Replace the colour grading.
    ///
    /// The whole thing at once, like the curve and for the same reason: a wheel
    /// sets hue and saturation together, and splitting them would make one
    /// gesture two commands. `control` says which of the eight is being moved,
    /// so the undo history can tell one drag from the next.
    SetGrade {
        grade: Box<rawkit_editstate::Grade>,
        control: u8,
    },
    /// Replace the hand-drawn tone curve.
    ///
    /// Carries the whole curve rather than one point, because inserting and
    /// removing change the shape of the list and a per-point command would need
    /// its own vocabulary for both. `point` is which control the user is
    /// manipulating — not used to render anything, only to keep the undo
    /// history honest: without it, dragging one point and then another would
    /// fold into a single step. `u8::MAX` means a wholesale change, like a
    /// reset, which always opens a step of its own.
    SetCurve {
        points: Vec<[f32; 2]>,
        point: u8,
    },
    /// One number of the eight-band hue mixer. A single command rather than
    /// twenty-four, because the band and the control are data — but see
    /// [`Command::coalesce_slot`] for what that costs the undo history.
    SetHsl {
        band: rawkit_editstate::Band,
        control: rawkit_editstate::BandControl,
        value: f32,
    },
    /// Level a horizon, in degrees clockwise. Refused past the straighten range.
    SetStraighten(f32),
    /// Turn by this many quarter-turns clockwise, from wherever it is now.
    ///
    /// A relative command rather than an absolute one because that is what a
    /// rotate key means, and because the alternative — the interface reading the
    /// current orientation, adding one and sending it back — is a race whenever
    /// anything else can change the edit.
    RotateBy(i32),
    /// Replace the edit wholesale — preset application, undo/redo, or loading a
    /// state from the catalog. Boxed to keep [`Command`] small; see
    /// `commands_and_events_stay_small`.
    SetEditState(Box<EditState>),

    /// Step the edit back to the previous point in the history, and forward
    /// again. Commands rather than methods because they arrive over the same
    /// bus as everything else, and because a refusal — "nothing to undo" — is
    /// something the interface already knows how to show.
    Undo,
    Redo,

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
            Command::SetCrop(_) => "set_crop",
            Command::SetSharpen(_) => "set_sharpen",
            Command::SetSharpenRadius(_) => "set_sharpen_radius",
            Command::SetChromaNoise(_) => "set_chroma_noise",
            Command::SetLuminanceNoise(_) => "set_luminance_noise",
            Command::SetSaturation(_) => "set_saturation",
            Command::SetVibrance(_) => "set_vibrance",
            Command::SetHsl { .. } => "set_hsl",
            Command::SetCurve { .. } => "set_curve",
            Command::SetGrade { .. } => "set_grade",
            Command::SetStraighten(_) => "set_straighten",
            Command::RotateBy(_) => "rotate_by",
            Command::SetEditState(_) => "set_edit_state",
            Command::Undo => "undo",
            Command::Redo => "redo",
            Command::Pan { .. } => "pan",
            Command::ZoomTo { .. } => "zoom_to",
            Command::Resize { .. } => "resize",
            Command::FitToView => "fit_to_view",
        }
    }

    /// Whether a run of these should collapse into a single undo step.
    ///
    /// True for the continuous controls — the ones with a slider behind them,
    /// which emit a command per pointer move. A drag that produced two hundred
    /// steps would be a history nobody could navigate, and the state anyone
    /// wants back is the one from before the drag began, not the one from two
    /// pixels ago.
    ///
    /// False for the discrete ones. Four presses of the rotate key are four
    /// decisions; collapsing them would make one press of undo discard all four.
    ///
    /// Matched exhaustively on purpose. A command added later should have to
    /// state which kind it is rather than inherit an answer from a wildcard.
    /// Which control of its kind this is, for commands where the name alone
    /// does not say.
    ///
    /// Every mixer command is called `set_hsl`, so a history keyed on the name
    /// would fold a drag on red's saturation and a drag on blue's into a single
    /// undo step — one press taking back two decisions about different colours.
    /// The slot separates them, and `two_bands_are_two_steps` is what holds it.
    fn coalesce_slot(&self) -> u8 {
        match self {
            Command::SetCurve { point, .. } => *point,
            Command::SetGrade { control, .. } => *control,
            Command::SetHsl { band, control, .. } => {
                let control = match control {
                    rawkit_editstate::BandControl::Hue => 0,
                    rawkit_editstate::BandControl::Saturation => 1,
                    rawkit_editstate::BandControl::Luminance => 2,
                };
                (band.index() as u8) * 3 + control
            }
            _ => 0,
        }
    }

    fn coalesces(&self) -> bool {
        match self {
            Command::SetExposure(_)
            | Command::SetContrast(_)
            | Command::SetHighlights(_)
            | Command::SetShadows(_)
            | Command::SetWhites(_)
            | Command::SetBlacks(_)
            | Command::SetTemperature(_)
            | Command::SetTint(_)
            | Command::SetSharpen(_)
            | Command::SetSharpenRadius(_)
            | Command::SetChromaNoise(_)
            | Command::SetLuminanceNoise(_)
            | Command::SetSaturation(_)
            | Command::SetVibrance(_)
            | Command::SetHsl { .. }
            | Command::SetStraighten(_) => true,

            // Dragging one control point coalesces; a wholesale change — a
            // reset, or a point appearing or disappearing — is a discrete act
            // and opens a step of its own.
            Command::SetCurve { point, .. } => *point != u8::MAX,
            Command::SetGrade { control, .. } => *control != u8::MAX,

            Command::SetOrientation(_)
            | Command::SetCrop(_)
            | Command::RotateBy(_)
            | Command::SetEditState(_) => false,

            // Not edits at all, so they never reach the history and the answer
            // does not matter — but it still has to be given.
            Command::Pan { .. }
            | Command::ZoomTo { .. }
            | Command::Resize { .. }
            | Command::FitToView
            | Command::Undo
            | Command::Redo => false,
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
    /// The frame after orientation and crop — what the viewport is measured in.
    ///
    /// Cached rather than derived on every call because it changes only when the
    /// edit does, and because deriving it in some places and not others is
    /// exactly how a fit and a pan end up disagreeing about how big the
    /// photograph is.
    developed: [u32; 2],
    tile: u32,
    max_level: u8,
    state: EditState,
    viewport: Viewport,
    generation: u64,
    /// States to step back to, oldest first. See [`Session::apply`] for what
    /// counts as one step.
    past: std::collections::VecDeque<EditState>,
    /// States undo has taken back, newest last. Cleared by any fresh edit,
    /// because redoing onto a history that has since branched would replay a
    /// decision the user has already replaced.
    future: Vec<EditState>,
    /// The control that opened the step now on top of `past`, so a run of the
    /// same one collapses into it. `None` means the next edit starts a new step
    /// whatever it is.
    ///
    /// A name *and* a slot: twenty-four mixer controls share one command name,
    /// and keying on the name alone would make two colours one decision.
    step: Option<(&'static str, u8)>,
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
        let developed = Geometry::new(&state).output_size(image);
        let mut session = Self {
            image,
            developed,
            tile,
            max_level,
            state,
            viewport: Viewport {
                center: [developed[0] as f64 / 2.0, developed[1] as f64 / 2.0],
                scale: 1.0,
                size: [0, 0],
            },
            generation: 1,
            past: std::collections::VecDeque::new(),
            future: Vec::new(),
            step: None,
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

    /// The photograph's size, after orientation and crop.
    ///
    /// What the viewport is measured in, and what a caller wanting to know "how
    /// big is this picture" means — `image_size` is the sensor's answer, which
    /// is a different question.
    pub fn developed_size(&self) -> [u32; 2] {
        self.developed
    }

    /// The map between the sensor's frame and the photograph's.
    pub fn geometry(&self) -> Geometry {
        Geometry::new(&self.state)
    }

    /// Apply one command. Never blocks and never renders.
    ///
    /// # What counts as one undo step
    ///
    /// A step is a control coming to rest. A drag emits a command per pointer
    /// move, so a run of the *same* continuous control collapses into the state
    /// from before the run began; a different control, or any discrete action,
    /// opens a new one. See [`Command::coalesces`].
    ///
    /// The consequence worth stating: a slider dragged, released, and dragged
    /// again is still one step, because nothing in a command says the pointer
    /// was let go. Making that two steps would mean teaching the session about
    /// pointer state or about time, and both are worse than the seam — a session
    /// that took a clock could not be tested the way this one is.
    pub fn apply(&mut self, command: Command) -> Event {
        let name = command.name();
        let coalesces = command.coalesces();
        let slot = command.coalesce_slot();
        // Undo and redo move *through* the history and must not be written into
        // it, or stepping back would leave a step whose only content is that you
        // stepped back.
        let records = !matches!(command, Command::Undo | Command::Redo);
        // Cloned for every command, including the view ones that will never use
        // it. Eighty-four bytes against a list of which commands are edits —
        // and that list is exactly the kind `edit_changed` already refuses to
        // keep, because somebody eventually forgets to add to it.
        let before = self.state.clone();

        let event = self.dispatch(command);

        // Only a change earns a step. A refused command left the state alone, so
        // recording one would put a point in the history that undo could return
        // to without anything visibly happening.
        if records && matches!(event, Event::EditChanged { .. }) {
            self.record((name, slot), before, coalesces);
        }
        event
    }

    /// Replace the edit and forget the history, for a photograph being opened.
    ///
    /// Deliberately not [`Command::SetEditState`], and the difference is the
    /// whole reason this exists: opening a photograph is not something the user
    /// did *to* this photograph. Through the command bus, the first press of
    /// undo after opening one would restore the **previous** image's edit — a
    /// decision about a different picture, arriving as if it were yours.
    pub fn load(&mut self, state: EditState) -> Event {
        if let Err(e) = state.validate() {
            return refused("load", e.to_string());
        }
        if !edit_is_finite(&state) {
            return refused("load", "edit contains a non-finite value");
        }
        self.state = state;
        self.past.clear();
        self.future.clear();
        self.step = None;
        self.edit_changed()
    }

    fn dispatch(&mut self, command: Command) -> Event {
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

            Command::SetSharpen(amount) => {
                let mut detail = self.state.detail;
                detail.sharpen_amount = amount;
                self.detail(name, detail)
            }
            Command::SetSharpenRadius(radius) => {
                let mut detail = self.state.detail;
                detail.sharpen_radius = radius;
                self.detail(name, detail)
            }

            Command::SetChromaNoise(amount) => {
                let mut detail = self.state.detail;
                detail.chroma_noise = amount;
                self.detail(name, detail)
            }
            Command::SetLuminanceNoise(amount) => {
                let mut detail = self.state.detail;
                detail.luminance_noise = amount;
                self.detail(name, detail)
            }

            Command::SetSaturation(v) => {
                let mut colour = self.state.colour;
                colour.saturation = v;
                self.colour(name, colour)
            }
            Command::SetVibrance(v) => {
                let mut colour = self.state.colour;
                colour.vibrance = v;
                self.colour(name, colour)
            }

            Command::SetGrade { grade, .. } => {
                if let Err(e) = grade.validate() {
                    return refused(name, e.to_string());
                }
                self.state.grade = *grade;
                self.edit_changed()
            }

            Command::SetCurve { points, .. } => {
                let curve = rawkit_editstate::Curve { points };
                if let Err(e) = curve.validate() {
                    return refused(name, e.to_string());
                }
                self.state.curve = curve;
                self.edit_changed()
            }

            Command::SetHsl {
                band,
                control,
                value,
            } => {
                let mut hsl = self.state.hsl;
                let mut mix = hsl.mix(band);
                mix.set(control, value);
                hsl.set(band, mix);
                if let Err(e) = hsl.validate() {
                    return refused(name, e.to_string());
                }
                self.state.hsl = hsl;
                self.edit_changed()
            }

            Command::SetStraighten(degrees) => {
                // Through `Crop`, so the range and the refusal are stated in one
                // place rather than once per caller.
                let mut crop = self.state.crop;
                crop.angle_deg = degrees;
                if let Err(e) = crop.validate() {
                    return refused(name, e.to_string());
                }
                self.state.crop = crop;
                self.edit_changed()
            }

            Command::RotateBy(quarters) => {
                let turns = [
                    Orientation::AsShot,
                    Orientation::Rotate90Cw,
                    Orientation::Rotate180,
                    Orientation::Rotate270Cw,
                ];
                let at = turns
                    .iter()
                    .position(|o| *o == self.state.orientation)
                    .unwrap_or(0) as i32;
                self.state.orientation = turns[(at + quarters).rem_euclid(4) as usize];
                self.edit_changed()
            }

            Command::SetCrop(crop) => {
                if let Err(e) = crop.validate() {
                    return refused(name, e.to_string());
                }
                self.state.crop = crop;
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

            Command::Undo => self.step_back(),
            Command::Redo => self.step_forward(),

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
                // Never below fit. See `fit_scale`.
                self.viewport.scale = match self.fit_scale() {
                    Some(smallest) => scale.max(smallest),
                    None => scale,
                };
                let scale = self.viewport.scale;
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
        let seen = self.viewport.visible_rect(self.developed);
        if seen[2] <= seen[0] || seen[3] <= seen[1] {
            return Vec::new();
        }
        // Tiles are addressed in the sensor's frame — the mosaic has not been
        // rotated and never will be, because rotating it would move the CFA
        // phase. So the visible rectangle is carried back across the geometry
        // before it becomes tile indices.
        let rect = Geometry::new(&self.state).sensor_rect(seen, self.image);
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

    /// Through `Detail::validate`, so the range and the refusal are stated once
    /// rather than once per control.
    fn detail(&mut self, name: &'static str, detail: rawkit_editstate::Detail) -> Event {
        if let Err(e) = detail.validate() {
            return refused(name, e.to_string());
        }
        self.state.detail = detail;
        self.edit_changed()
    }

    /// Through `Colour::validate`, so the range is stated once.
    fn colour(&mut self, name: &'static str, colour: rawkit_editstate::Colour) -> Event {
        if let Err(e) = colour.validate() {
            return refused(name, e.to_string());
        }
        self.state.colour = colour;
        self.edit_changed()
    }

    /// Put `before` on the history, unless the same control is still moving.
    fn record(&mut self, control: (&'static str, u8), before: EditState, coalesces: bool) {
        // Any fresh edit branches away from whatever undo had taken back.
        self.future.clear();
        if coalesces && self.step == Some(control) {
            return;
        }
        if self.past.len() == MAX_STEPS {
            self.past.pop_front();
        }
        self.past.push_back(before);
        // A discrete command leaves no step open, so whatever comes next starts
        // its own — two rotates are two steps even though the name is the same.
        self.step = coalesces.then_some(control);
    }

    fn step_back(&mut self) -> Event {
        let Some(previous) = self.past.pop_back() else {
            return refused("undo", "nothing to undo");
        };
        self.future
            .push(std::mem::replace(&mut self.state, previous));
        // The step that was open is now the one we just left, so an edit
        // arriving next must not merge into it.
        self.step = None;
        self.edit_changed()
    }

    fn step_forward(&mut self) -> Event {
        let Some(next) = self.future.pop() else {
            return refused("redo", "nothing to redo");
        };
        self.past
            .push_back(std::mem::replace(&mut self.state, next));
        self.step = None;
        self.edit_changed()
    }

    fn edit_changed(&mut self) -> Event {
        self.generation += 1;
        // Orientation and crop change how big the photograph is, and a stale
        // size would leave the view fitted to the frame before the change.
        // Recomputed for every edit rather than only the geometric ones: it is
        // two multiplications, and remembering which commands are geometric is a
        // rule somebody eventually forgets.
        let was = self.developed;
        self.developed = Geometry::new(&self.state).output_size(self.image);
        if self.developed != was {
            // The photograph is a different shape, so the old scale and centre
            // describe a frame that no longer exists — a rotate would leave it
            // overflowing the canvas and a crop would leave it off to one side.
            //
            // The cost is that nudging a crop while zoomed in throws the view
            // back to fit. That is the right way round: after a geometric change
            // the useful thing to see is the whole of what you now have.
            self.fit_to_view();
        }
        self.clamp_center();
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
        self.viewport.center = [
            self.developed[0] as f64 / 2.0,
            self.developed[1] as f64 / 2.0,
        ];
        if w == 0 || h == 0 {
            return;
        }
        // The *developed* frame: fitting the sensor's would leave a cropped
        // photograph small and off-centre, surrounded by the part it removed.
        self.viewport.scale = self.fit_scale().unwrap_or(self.viewport.scale);
    }

    /// The scale at which the whole photograph is visible, or `None` before the
    /// canvas has a size.
    ///
    /// Also the *smallest* scale allowed. Zooming further out shows a smaller
    /// photograph surrounded by more nothing, and it is not free: the canvas is
    /// sized in level pixels, so halving the scale doubles the texture. Left
    /// unbounded it eventually asks the device for a texture larger than it
    /// allows, which is not a slow frame but a dead process.
    fn fit_scale(&self) -> Option<f64> {
        let [w, h] = self.viewport.size;
        (w != 0 && h != 0)
            .then(|| (w as f64 / self.developed[0] as f64).min(h as f64 / self.developed[1] as f64))
    }

    /// Keep the centre on the image, so the photo cannot be flung off screen.
    fn clamp_center(&mut self) {
        self.viewport.center[0] = self.viewport.center[0].clamp(0.0, self.developed[0] as f64);
        self.viewport.center[1] = self.viewport.center[1].clamp(0.0, self.developed[1] as f64);
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
    fn a_drag_is_one_step_and_not_two_hundred() {
        // The whole reason coalescing exists. Without it, undoing a drag means
        // pressing the key until your finger gets tired, and the user gave one
        // instruction.
        let mut s = session();
        for i in 0..200 {
            s.apply(Command::SetExposure(i as f32 / 100.0));
        }
        assert_eq!(s.state().tone.exposure_ev, 1.99);

        assert!(matches!(s.apply(Command::Undo), Event::EditChanged { .. }));
        assert_eq!(
            s.state().tone.exposure_ev,
            0.0,
            "one press should undo the drag"
        );
        assert!(
            matches!(s.apply(Command::Undo), Event::Refused { .. }),
            "the drag left more than one step behind"
        );
    }

    #[test]
    fn dragging_two_curve_points_is_two_steps() {
        // Every curve command is called `set_curve` and carries the whole
        // curve, so a history keyed on the name alone would fold a drag on one
        // control point and a drag on another into a single step.
        let mut s = session();
        let line = |mid: f32| vec![[0.0, 0.0], [0.25, mid], [0.75, 0.8], [1.0, 1.0]];
        for mid in [0.1, 0.2, 0.3] {
            s.apply(Command::SetCurve {
                points: line(mid),
                point: 1,
            });
        }
        s.apply(Command::SetCurve {
            points: vec![[0.0, 0.0], [0.25, 0.3], [0.75, 0.9], [1.0, 1.0]],
            point: 2,
        });

        s.apply(Command::Undo);
        assert_eq!(
            s.state().curve.points[2],
            [0.75, 0.8],
            "the second drag stayed"
        );
        assert_eq!(
            s.state().curve.points[1],
            [0.25, 0.3],
            "the first drag went back too"
        );

        s.apply(Command::Undo);
        assert!(
            s.state().curve.is_identity(),
            "the run of three left more than one step"
        );
    }

    #[test]
    fn a_reset_is_its_own_step_however_it_follows_a_drag() {
        let mut s = session();
        s.apply(Command::SetCurve {
            points: vec![[0.0, 0.0], [0.5, 0.8], [1.0, 1.0]],
            point: 1,
        });
        s.apply(Command::SetCurve {
            points: rawkit_editstate::Curve::default().points,
            point: u8::MAX,
        });
        s.apply(Command::Undo);
        assert_eq!(
            s.state().curve.points[1],
            [0.5, 0.8],
            "the reset merged into the drag it undid"
        );
    }

    #[test]
    fn a_curve_that_runs_backwards_is_refused() {
        let mut s = session();
        let generation = s.generation();
        assert!(matches!(
            s.apply(Command::SetCurve {
                points: vec![[0.0, 0.0], [0.7, 0.5], [0.3, 0.9], [1.0, 1.0]],
                point: 1,
            }),
            Event::Refused { .. }
        ));
        assert!(s.state().curve.is_identity());
        assert_eq!(s.generation(), generation);
    }

    #[test]
    fn two_bands_are_two_steps() {
        // Every mixer command is called `set_hsl`, so a history keyed on the
        // name alone would fold a drag on one colour and a drag on another into
        // one step — and one press of undo would take back a decision about a
        // colour the user was not looking at.
        use rawkit_editstate::{Band, BandControl};
        let mut s = session();
        for value in [0.1, 0.2, 0.3] {
            s.apply(Command::SetHsl {
                band: Band::Red,
                control: BandControl::Saturation,
                value,
            });
        }
        s.apply(Command::SetHsl {
            band: Band::Blue,
            control: BandControl::Saturation,
            value: 0.5,
        });

        s.apply(Command::Undo);
        assert_eq!(s.state().hsl.blue.saturation, 0.0);
        assert_eq!(s.state().hsl.red.saturation, 0.3, "red went back with blue");

        s.apply(Command::Undo);
        assert_eq!(s.state().hsl.red.saturation, 0.0);
        assert!(
            matches!(s.apply(Command::Undo), Event::Refused { .. }),
            "the run of three on red left more than one step"
        );
    }

    #[test]
    fn two_controls_on_one_band_are_two_steps() {
        // The other half of the same rule: same band, different slider.
        use rawkit_editstate::{Band, BandControl};
        let mut s = session();
        s.apply(Command::SetHsl {
            band: Band::Green,
            control: BandControl::Hue,
            value: 0.4,
        });
        s.apply(Command::SetHsl {
            band: Band::Green,
            control: BandControl::Luminance,
            value: -0.2,
        });

        s.apply(Command::Undo);
        assert_eq!(s.state().hsl.green.luminance, 0.0);
        assert_eq!(s.state().hsl.green.hue, 0.4);
    }

    #[test]
    fn a_mixer_value_out_of_range_is_refused_and_changes_nothing() {
        use rawkit_editstate::{Band, BandControl};
        let mut s = session();
        let generation = s.generation();
        assert!(matches!(
            s.apply(Command::SetHsl {
                band: Band::Aqua,
                control: BandControl::Saturation,
                value: 4.0,
            }),
            Event::Refused { .. }
        ));
        assert_eq!(s.state().hsl, rawkit_editstate::Hsl::default());
        assert_eq!(s.generation(), generation);
    }

    #[test]
    fn moving_to_another_control_starts_a_new_step() {
        let mut s = session();
        s.apply(Command::SetExposure(1.0));
        s.apply(Command::SetContrast(0.5));

        s.apply(Command::Undo);
        assert_eq!(s.state().tone.contrast, 0.0);
        assert_eq!(
            s.state().tone.exposure_ev,
            1.0,
            "undo took back both controls"
        );

        s.apply(Command::Undo);
        assert_eq!(s.state().tone.exposure_ev, 0.0);
    }

    #[test]
    fn going_back_to_a_control_does_not_merge_with_the_earlier_run() {
        // exposure, contrast, exposure is three decisions, even though the first
        // and last are the same slider.
        let mut s = session();
        s.apply(Command::SetExposure(1.0));
        s.apply(Command::SetContrast(0.5));
        s.apply(Command::SetExposure(2.0));

        s.apply(Command::Undo);
        assert_eq!(s.state().tone.exposure_ev, 1.0);
        assert_eq!(s.state().tone.contrast, 0.5);
    }

    #[test]
    fn each_rotate_is_its_own_step() {
        // A discrete command must not coalesce with itself: four presses of the
        // rotate key are four decisions, and collapsing them would make one
        // press of undo throw all four away.
        let mut s = session();
        let start = s.state().orientation;
        s.apply(Command::RotateBy(1));
        s.apply(Command::RotateBy(1));
        let turned = s.state().orientation;
        assert_ne!(turned, start);

        s.apply(Command::Undo);
        assert_ne!(
            s.state().orientation,
            turned,
            "both rotates went back at once"
        );
        s.apply(Command::Undo);
        assert_eq!(s.state().orientation, start);
    }

    #[test]
    fn redo_replays_what_undo_took_back() {
        let mut s = session();
        s.apply(Command::SetExposure(1.5));
        s.apply(Command::Undo);
        assert_eq!(s.state().tone.exposure_ev, 0.0);

        assert!(matches!(s.apply(Command::Redo), Event::EditChanged { .. }));
        assert_eq!(s.state().tone.exposure_ev, 1.5);
        assert!(matches!(s.apply(Command::Redo), Event::Refused { .. }));
    }

    #[test]
    fn a_fresh_edit_after_undo_abandons_the_redo() {
        // Redoing onto a history that has since branched would replay a decision
        // the user has already replaced.
        let mut s = session();
        s.apply(Command::SetExposure(1.5));
        s.apply(Command::Undo);
        s.apply(Command::SetContrast(0.25));

        assert!(matches!(s.apply(Command::Redo), Event::Refused { .. }));
        assert_eq!(s.state().tone.exposure_ev, 0.0);
        assert_eq!(s.state().tone.contrast, 0.25);
    }

    #[test]
    fn an_edit_after_undo_does_not_merge_into_the_step_that_was_undone() {
        let mut s = session();
        s.apply(Command::SetExposure(1.0));
        s.apply(Command::Undo);
        s.apply(Command::SetExposure(2.0));

        s.apply(Command::Undo);
        assert_eq!(
            s.state().tone.exposure_ev,
            0.0,
            "the new drag was absorbed into the step undo had just left"
        );
    }

    #[test]
    fn opening_a_photograph_forgets_the_history() {
        // The trap this exists to avoid: `load` rather than `SetEditState`, so
        // the first undo after opening cannot restore the previous image's edit.
        let mut s = session();
        s.apply(Command::SetExposure(1.0));

        let mut next = EditState::default();
        next.tone.contrast = 0.4;
        assert!(matches!(s.load(next), Event::EditChanged { .. }));

        assert!(matches!(s.apply(Command::Undo), Event::Refused { .. }));
        assert_eq!(
            s.state().tone.exposure_ev,
            0.0,
            "the previous photograph's edit came back"
        );
        assert_eq!(s.state().tone.contrast, 0.4);
    }

    #[test]
    fn pasting_an_edit_is_undoable_but_opening_one_is_not() {
        // Two callers of what used to be the same command. Applying someone
        // else's settings is a decision; opening a photograph is not.
        let mut s = session();
        let mut pasted = EditState::default();
        pasted.tone.shadows = 0.6;
        s.apply(Command::SetEditState(Box::new(pasted)));

        s.apply(Command::Undo);
        assert_eq!(s.state().tone.shadows, 0.0);
    }

    #[test]
    fn a_refused_command_leaves_no_step_behind() {
        // A step whose two ends are identical is one press of undo that appears
        // to do nothing at all.
        let mut s = session();
        s.apply(Command::SetExposure(1.0));
        assert!(matches!(
            s.apply(Command::SetSharpen(99.0)),
            Event::Refused { .. }
        ));

        s.apply(Command::Undo);
        assert_eq!(s.state().tone.exposure_ev, 0.0);
        assert!(matches!(s.apply(Command::Undo), Event::Refused { .. }));
    }

    #[test]
    fn undo_with_nothing_behind_it_changes_nothing() {
        let mut s = session();
        let generation = s.generation();
        assert!(matches!(s.apply(Command::Undo), Event::Refused { .. }));
        assert!(matches!(s.apply(Command::Redo), Event::Refused { .. }));
        assert_eq!(
            s.generation(),
            generation,
            "a refusal still invalidated every tile"
        );
        assert_eq!(*s.state(), EditState::default());
    }

    #[test]
    fn the_history_is_bounded_and_drops_the_oldest() {
        let mut s = session();
        // Alternating controls so nothing coalesces: each is its own step.
        for i in 0..MAX_STEPS + 50 {
            if i % 2 == 0 {
                s.apply(Command::SetExposure(i as f32 / 1000.0));
            } else {
                s.apply(Command::SetContrast(i as f32 / 1000.0));
            }
        }
        assert_eq!(s.past.len(), MAX_STEPS);

        for _ in 0..MAX_STEPS {
            assert!(matches!(s.apply(Command::Undo), Event::EditChanged { .. }));
        }
        assert!(matches!(s.apply(Command::Undo), Event::Refused { .. }));
        // The oldest steps are gone, so this does *not* return to the default —
        // which is the honest consequence of a bound and worth pinning.
        assert_ne!(*s.state(), EditState::default());
    }

    #[test]
    fn undo_and_redo_survive_a_json_round_trip() {
        for command in [Command::Undo, Command::Redo] {
            let json = serde_json::to_string(&command).expect("serialise");
            assert_eq!(
                serde_json::from_str::<Command>(&json).expect("deserialise"),
                command
            );
        }
        assert_eq!(
            serde_json::to_string(&Command::Undo).expect("serialise"),
            r#"{"command":"undo"}"#
        );
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
    fn the_view_cannot_zoom_out_past_the_whole_photograph() {
        // Found by crashing the application. The canvas is sized in level
        // pixels, so every halving of the scale doubles the texture; the level
        // stops rising at the coarsest pyramid level, and after that the canvas
        // grows without limit. Twenty-odd notches out, the device is asked for a
        // texture wider than it allows and the process aborts — a dead window,
        // not a slow one.
        let mut s = session();
        s.apply(Command::FitToView);
        let fitted = s.viewport().scale;

        for _ in 0..40 {
            s.apply(Command::ZoomTo {
                scale: s.viewport().scale / 1.15,
                anchor: [800.0, 500.0],
            });
        }
        assert_eq!(
            s.viewport().scale,
            fitted,
            "the view zoomed out past fit, where the canvas grows without bound"
        );

        // And zooming *in* is still unrestricted, which is the whole point of
        // having a photograph.
        s.apply(Command::ZoomTo {
            scale: 8.0,
            anchor: [800.0, 500.0],
        });
        assert_eq!(s.viewport().scale, 8.0);
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

    #[test]
    fn the_view_is_measured_in_the_photograph_and_not_in_the_sensor() {
        // The whole point of routing the viewport through the geometry. Fitting
        // the sensor's frame would leave a cropped photograph small and
        // off-centre, ringed by the part the user just removed.
        let mut s = session();
        s.apply(Command::FitToView);
        let fitted = s.viewport().scale;
        s.apply(Command::SetCrop(Crop {
            left: 0.25,
            top: 0.25,
            right: 0.75,
            bottom: 0.75,
            ..Crop::default()
        }));

        assert_eq!(s.developed_size(), [3000, 2000]);
        assert_eq!(s.image_size(), IMAGE, "the sensor did not change size");
        assert!(
            (s.viewport().scale - fitted * 2.0).abs() < 1e-9,
            "half the frame should fit at twice the scale"
        );
        assert_eq!(s.viewport().center, [1500.0, 1000.0]);
    }

    #[test]
    fn rotating_swaps_the_photograph_and_refits_it() {
        let mut s = session();
        s.apply(Command::SetOrientation(Orientation::Rotate90Cw));
        assert_eq!(s.developed_size(), [4000, 6000]);
        // No explicit fit: a photograph that changed shape is refitted, because
        // the old scale describes a frame that no longer exists. A 1600x1000
        // canvas around a 4000x6000 photograph fits on height.
        assert!((s.viewport().scale - 1000.0 / 6000.0).abs() < 1e-9);
    }

    #[test]
    fn a_crop_asks_for_the_tiles_under_it_and_no_others() {
        // The conversion that has to be right: the view is in the photograph's
        // frame, the mosaic is in the sensor's, and the mosaic is never rotated
        // because rotating it would move the CFA phase.
        let mut s = session();
        s.apply(Command::FitToView);
        let whole = s.visible_tiles(3).len();

        s.apply(Command::SetCrop(Crop {
            left: 0.0,
            top: 0.0,
            right: 0.25,
            bottom: 0.25,
            ..Crop::default()
        }));
        s.apply(Command::FitToView);
        let corner = s.visible_tiles(3);
        assert!(
            corner.len() < whole,
            "a quarter-frame crop asked for {} of {whole} tiles",
            corner.len()
        );
        // Top-left of the photograph is top-left of the sensor when unrotated.
        assert!(corner.iter().all(|t| t.x < 3 && t.y < 2), "{corner:?}");
    }

    #[test]
    fn a_rotated_crop_reads_the_corner_of_the_sensor_it_actually_covers() {
        // Under a quarter turn the photograph's top-left corner is the sensor's
        // bottom-left. Getting this backwards shows the wrong part of the frame
        // while looking entirely plausible.
        let mut s = session();
        s.apply(Command::SetOrientation(Orientation::Rotate90Cw));
        s.apply(Command::SetCrop(Crop {
            left: 0.0,
            top: 0.0,
            right: 0.25,
            bottom: 0.25,
            ..Crop::default()
        }));
        s.apply(Command::FitToView);

        let tiles = s.visible_tiles(3);
        assert!(!tiles.is_empty());
        let span = TILE << 3;
        let rows = IMAGE[1].div_ceil(span);
        assert!(
            tiles.iter().all(|t| t.y >= rows / 2),
            "expected the lower half of the sensor, got {tiles:?}"
        );
    }

    #[test]
    fn a_crop_that_is_not_a_rectangle_is_refused_and_changes_nothing() {
        let mut s = session();
        let before = s.state().clone();
        let event = s.apply(Command::SetCrop(Crop {
            left: 0.8,
            top: 0.0,
            right: 0.2,
            bottom: 1.0,
            ..Crop::default()
        }));
        assert!(matches!(event, Event::Refused { .. }), "{event:?}");
        assert_eq!(s.state(), &before);
        assert_eq!(s.developed_size(), IMAGE);
    }

    #[test]
    fn rotating_wraps_in_both_directions() {
        // Relative, so it cannot get out of step with an interface that read the
        // orientation a moment ago — and so a rotate key works the same whether
        // you press it four times or twice each way.
        let mut s = session();
        for _ in 0..4 {
            s.apply(Command::RotateBy(1));
        }
        assert_eq!(s.state().orientation, Orientation::AsShot);

        s.apply(Command::RotateBy(-1));
        assert_eq!(s.state().orientation, Orientation::Rotate270Cw);
        assert_eq!(s.developed_size(), [4000, 6000]);

        s.apply(Command::RotateBy(5));
        assert_eq!(s.state().orientation, Orientation::AsShot);
        assert_eq!(s.developed_size(), IMAGE);
    }

    #[test]
    fn straightening_refits_and_refuses_more_than_a_straighten() {
        // The photograph gets smaller, because the crop pulls in to keep the
        // empty corners out — so the view has to refit or it would show the new
        // frame at the old scale, cropped by the canvas.
        let mut s = session();
        let before = s.developed_size();
        s.apply(Command::SetStraighten(6.0));
        let after = s.developed_size();
        assert!(
            after[0] < before[0] && after[1] < before[1],
            "{after:?} is not smaller than {before:?}"
        );

        let event = s.apply(Command::SetStraighten(30.0));
        assert!(matches!(event, Event::Refused { .. }), "{event:?}");
        assert_eq!(s.developed_size(), after, "a refusal changes nothing");
    }

    #[test]
    fn sharpening_past_what_the_halo_covers_is_refused() {
        // Not clamped: a radius wider than the tile halo reads demosaic output
        // that is wrong near a tile edge, and the result is a faint grid on
        // detailed frames rather than anything that looks like a bad setting.
        let mut s = session();
        let event = s.apply(Command::SetSharpenRadius(9.0));
        assert!(matches!(event, Event::Refused { .. }), "{event:?}");
        assert_eq!(
            s.state().detail,
            rawkit_editstate::Detail::default(),
            "a refusal changes nothing"
        );

        assert!(matches!(
            s.apply(Command::SetSharpen(0.0)),
            Event::EditChanged { .. }
        ));
        assert_eq!(
            s.state().detail.sharpen_amount,
            0.0,
            "off is a legal setting"
        );
    }
}
