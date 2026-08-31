//! The desktop shell, and the compositing probe it grew out of — **whose result
//! is that no single arrangement works on all three platforms.**
//!
//! # Which route runs where
//!
//! | platform | route | why |
//! |---|---|---|
//! | Linux | 3, a native child widget | route 1's layers fight; see below |
//! | macOS | 1, transparent cutout | no GTK to hang a widget on |
//! | Windows | 1, transparent cutout | the same |
//!
//! They are not fallbacks for one another. Route 3 needs a native widget to put
//! the surface on, which so far means a GTK one; route 1 needs a transparent
//! window, which is exactly what Linux/WebKitGTK cannot composite. Which is
//! right depends on the compositor, so the default is chosen per platform and
//! `RAWKIT_PROBE_ROUTE` overrides it.
//!
//! On macOS that costs Tauri's `macos-private-api`, and so bars **App Store**
//! distribution — not notarised direct download, which is how a tool like this
//! ships. The alternative was a child `NSView`: several hundred lines of FFI
//! that nobody here can run, to buy back a channel this project is not using.
//!
//! # Pointer input
//!
//! Two things deliver it and they have nothing in common: GTK hands route 3 real
//! events on the canvas's own window, and under a cutout the webview owns every
//! pixel so the page forwards what it receives. Both end in [`pointer::route`],
//! which is where a drag becomes a pan and a wheel becomes a zoom — because two
//! copies of that would eventually disagree about which.
//!
//! **Neither macOS nor Windows has been run.** CI proves the shell compiles and
//! links on both, and for window code that is a weak claim: a surface behind an
//! opaque view is something no compiler notices. What *is* verified is
//! everything short of the compositing — route 1 on Linux reports the right
//! surface and canvas rectangles, draws the photograph below the chrome, and
//! pans and zooms from forwarded pointer events while a drag on the chrome
//! leaves the canvas alone. The compositing itself rests on the probe research
//! rather than on a machine.
//!
//! # The question
//!
//! The design's hard rule is that the engine owns the canvas and React renders
//! chrome around it. On the desktop that needs a native GPU surface and a
//! webview in one window, and Tauri has no supported way to do it: the feature
//! request (tauri#8246) is closed, and the wgpu-plus-transparency flicker on
//! Linux/WebKitGTK (tauri#9220) is closed as *not planned*.
//!
//! Three arrangements were tested before any UI was built on one, because
//! discovering it afterwards is a rewrite. **The third works; the first two do
//! not.**
//!
//! 1. **Transparent cutout** — the webview paints chrome and leaves a hole; wgpu
//!    draws behind it. What RapidRAW ships, having first tried and abandoned
//!    JPEG-over-IPC as too slow. Known good on macOS and Windows. **Fails here:
//!    the layers fight, see below.**
//! 2. **Disjoint child webviews** — panels and canvas never overlap, so nothing
//!    needs transparency at all. Costs an unstable Tauri API. **Fails here for a
//!    different reason: the API does not position the child.**
//! 3. **A native widget for the canvas** — a `GtkDrawingArea` packed beside the
//!    webview, with the surface on *its* X window rather than the toplevel.
//!    **Works.** See below, and `canvas.rs` for how.
//!
//! # The result, on X11 / WebKitGTK / Vulkan (Radeon 780M, RADV)
//!
//! **The two layers fight, and whoever painted last wins.** Six captures a
//! second apart, with the page animating so it repaints continuously:
//!
//! | captures | left third | right two thirds |
//! |---|---|---|
//! | 1, 2, 6 | red — the page | black — the page |
//! | 3, 4, 5 | green — the GPU | green — the GPU |
//!
//! Both write the *whole* window. There is no z-order to exploit: the webview
//! does not sit above the surface with a hole in it, it takes turns with it.
//! That is tauri#9220 reproduced rather than merely feared, so **route 1 is not
//! a risk here, it is a dead end.**
//!
//! Two controls make that reading safe. With `RAWKIT_PROBE_NO_GPU=1` the page
//! renders correctly on its own — left red, right black — so the webview and
//! the transparent background both work and the failure is purely one of
//! compositing. And the alternation only appears once the page repaints
//! continuously; a static page paints once, loses, and stays lost, which a
//! single screenshot would have reported as a clean stable z-order.
//!
//! # Route 2: the child webview cannot be placed
//!
//! `Window::add_child` takes a position and a size, and on Linux/GTK the child
//! ignores both and fills the window. `bounds()` reports `0x0` immediately after
//! creation and `1200x800` — the whole window — once the page has loaded, after
//! `set_auto_resize(false)`, `set_position` and `set_size` have each been called
//! twice. With the GPU disabled the entire window is the panel's red, which is
//! the visual form of the same fact.
//!
//! So route 2 fails before compositing is even reached. Whether X would have
//! clipped the parent's present out of a correctly-placed child's rectangle is
//! still unknown, and it is the question worth carrying forward: the mechanism
//! is sound, the API that would arrange it is not.
//!
//! # Route 3: give the canvas its own window
//!
//! Stable across every capture, with **one exact boundary and nothing else**:
//! red above y=1000, green below, no alternation and no bleed.
//!
//! The confirming detail is an accident that turned into the proof. The surface
//! was configured at 2400x1200 while the widget is only 2400x600, and the green
//! still stops exactly at the widget's edge — **X clipped the overspill rather
//! than letting it reach the webview.** That is the mechanism this route is
//! built on, demonstrated rather than assumed: output to a window is clipped to
//! that window, so two siblings cannot contend.
//!
//! Both failures above share one cause, which is why this fixes them: the
//! surface was attached to the toplevel, and the toplevel is also the webview's
//! window. It was never a transparency problem.
//!
//! The split here is 500 logical pixels of webview above 300 of canvas, rather
//! than the 200 requested — the size request did not take. That is a layout
//! detail and an ordinary one; the compositing question it was asked to answer
//! is settled.
//!
//! # Scope
//!
//! X11. Wayland composites differently and is untested —
//! GTK uses subsurfaces there — and macOS and Windows are where route 1 is
//! reported to work. This says nothing about them, which is the point of
//! writing down what was actually measured.
//!
//! # Reading it yourself
//!
//! ```text
//! RAWKIT_PROBE_CLEAR=1 RAWKIT_PROBE_ROUTE=1 cargo run -p rawkit-shell &
//! xwd -name rawkit -out probe.xwd
//! python3 crates/rawkit-shell/probe/check-composite.py probe.xwd
//! ```
//!
//! wgpu clears the window to a green no interface would ever use, so the check
//! is by value rather than by eye. Take several captures a second apart.

#[cfg(target_os = "linux")]
mod canvas;
mod library;
mod pointer;
mod session_canvas;
mod window_state;

use anyhow::{anyhow, Result};
use library::{CullAction, CullView, Library, Loaded, Saver};
use rawkit_editstate::EditState;
use rawkit_engine::{render::DEFAULT_TILE, Gpu, Presenter};
use rawkit_session::{Command, Event, Session};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::{
    webview::WebviewBuilder, window::WindowBuilder, LogicalPosition, LogicalSize, Manager,
    WebviewUrl, WebviewWindowBuilder,
};

/// Deliberately a colour no UI would ever use, so a screenshot can be checked by
/// value rather than by eye.
///
/// Still here because the compositing check documented above has to be runnable
/// on macOS and Windows, where the routes have not been tested. Set
/// `RAWKIT_PROBE_CLEAR=1` and the window fills with this instead of a render.
const PROBE_GREEN: wgpu::Color = wgpu::Color {
    r: 0.0,
    g: 1.0,
    b: 0.0,
    a: 1.0,
};

/// Which arrangement to build. `RAWKIT_PROBE_ROUTE=1` for the cutout, `2` for
/// child webviews, anything else for the native child that actually works.
///
/// The first two stay in the code after being ruled out on Linux, because they
/// are reported to work on macOS and Windows and this is how that gets checked
/// there. A probe that only tests the arrangement you settled on cannot tell you
/// when another one starts working.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Route {
    /// One full-window webview with a transparent hole, wgpu behind it.
    Cutout,
    /// A window with no webview of its own and the chrome as a child webview in
    /// its own rectangle. Nothing overlaps, so nothing needs transparency.
    ChildWebview,
    /// A native window for the canvas, created directly rather than borrowed
    /// from a widget, with the chrome beside it. X clips each to its own
    /// rectangle and they never contend.
    NativeChild,
}

/// Width of the chrome, in logical pixels.
///
/// A column rather than a strip, and the reason is arithmetic as much as taste.
/// In a 1440x800 window a top strip left the photograph 1440x576, and a 3:2
/// frame fitted to that is 864x576; a 360-wide column leaves 1080x800, and the
/// same frame comes out 1080x720. **Half again as much photograph**, because a
/// landscape picture is limited by its short edge and a strip takes from
/// exactly that edge.
///
/// It also scales, which a strip does not: a column scrolls, so the per-band
/// mixer's two dozen controls and whatever a mask list turns out to need have
/// somewhere to go.
///
/// 360 rather than less: a control is a 58px label, a readout, and a slider,
/// and below about 280 the slider is too short to place a value on.
const PANEL_WIDTH: f64 = 360.0;

/// The panel's width right now, in logical pixels, once the divider can move it.
///
/// A static because three places need it and none of them can hold it: the GTK
/// size-allocate handler that places the canvas child, the render loop that
/// configures the surface, and the command the page calls when the divider is
/// dragged. Stored as whole logical pixels — the divider snaps to them anyway,
/// and an integer is a value the three can agree on exactly.
static PANEL_NOW: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(PANEL_WIDTH as i32);

/// The width the panel is actually being given, after the window has had its
/// say. Differs from [`PANEL_NOW`] only in a window too narrow for the width
/// somebody chose — and the page has to be told, or it draws a panel wider than
/// the space the canvas left it and the photograph covers half the controls.
static PANEL_SHOWN: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(PANEL_WIDTH as i32);

/// A window resize nobody has acted on yet, in *physical* pixels.
///
/// The event arrives on the main thread and the surface is reconfigured on the
/// render loop, which on Linux is the same thread and elsewhere is not. A slot
/// rather than a channel because only the newest size matters: a drag emits
/// hundreds and every one but the last is already wrong by the time it is read.
static PENDING_RESIZE: Mutex<Option<(u32, u32)>> = Mutex::new(None);

/// The narrowest useful window, in logical pixels.
///
/// Below this the panel would be most of the window and the photograph a strip.
/// Enforced by the window manager rather than by clamping after the fact, so the
/// window simply cannot be dragged smaller.
const MIN_WINDOW: (f64, f64) = (720.0, 480.0);

/// What the divider may do to the panel, in logical pixels.
///
/// The lower bound is where the widest label — the noise-reduction pair — starts
/// wrapping, measured rather than guessed; the upper is where the photograph
/// stops being the larger half of a minimum-width window.
const PANEL_MIN: f64 = 280.0;
const PANEL_MAX: f64 = 640.0;

/// The least photograph worth showing, in logical pixels.
///
/// What gives way in a window too narrow for both: the panel, not the picture.
/// A control panel that has squeezed the photograph into a strip has stopped
/// being a photo editor, and the panel's own minimum is the smaller loss.
const PHOTO_MIN: f64 = 320.0;
/// The same width where a platform wants whole pixels.
///
/// The equivalents on macOS and Windows will be a child NSView and a child
/// HWND, and those will bring their own geometry — but the constant is not
/// gated, because a value only some platforms can see is how this file has
/// broken the build four times.
const PANEL_PIXELS: i32 = PANEL_WIDTH as i32;
// The sections used to sit side by side and needed about 1280 between them.
// They are a column now, so this is only what a photograph wants: 1080 of
// canvas beside the chrome, which shows a 3:2 frame at 1080x720.
const WINDOW: (f64, f64) = (1440.0, 800.0);

/// The session, shared between the page's commands and the render loop.
///
/// A mutex rather than a channel because the two sides want different things:
/// the page wants to apply a command and see the result immediately, and the
/// loop wants a consistent snapshot. Contention is a lock held for the length of
/// `apply`, which touches a handful of floats.
struct Shared(Arc<Mutex<Session>>);

/// The only way the page can change anything.
///
/// Note what does *not* cross: the return is an [`Event`], which has no variant
/// that can hold an image. The page learns that the edit changed and what
/// generation it is at; the pixels go to the canvas, which the page cannot see.
#[tauri::command]
fn apply(state: tauri::State<'_, Shared>, command: Command) -> Event {
    state.0.lock().expect("session lock").apply(command)
}

/// What the page needs to draw its own controls: the edit, and where the view is.
#[tauri::command]
fn snapshot(state: tauri::State<'_, Shared>) -> serde_json::Value {
    let session = state.0.lock().expect("session lock");
    serde_json::json!({
        "state": session.state(),
        "viewport": session.viewport(),
        "image": session.image_size(),
        "generation": session.generation(),
        // Which local adjustment the panel is showing, and whether the next
        // drag on the photograph redraws it. Neither is part of the *edit* —
        // they are where the hands are, not what the picture is — so they live
        // beside it rather than in it. `null` for none.
        "selected_mask": match SELECTED_MASK.load(std::sync::atomic::Ordering::Relaxed) {
            usize::MAX => None,
            index => Some(index),
        },
        "placing_mask": PLACING_MASK.load(std::sync::atomic::Ordering::Relaxed),
        // `None` means the decoder's own matrix, which is a state worth naming
        // rather than an absence to be guessed at.
        "profile": *PROFILE_NAME.lock().expect("profile lock"),
        // Quarter-turns clockwise, both halves. Numbers rather than words
        // because the page does every other bit of formatting and there is no
        // reason for this one to be phrased twice.
        //
        // `recorded` is not in `state` and cannot be: it is what the camera
        // said, not what anyone decided. It is here so the panel can *say* why
        // a photograph opened turned — a reason the user cannot see is
        // indistinguishable from a bug.
        // A one-shot line from the render loop, which has no command to return
        // a refusal through.
        "notice": NOTICE.lock().expect("notice lock").take(),
        // What the panel is actually being given. Polled rather than pushed
        // because the window can narrow without the page doing anything, and a
        // stylesheet that has not heard draws a column wider than the canvas
        // left it — the photograph then covers the controls' left edge.
        "panel": PANEL_SHOWN.load(std::sync::atomic::Ordering::Relaxed),
        "orientation": {
            "recorded": session.recorded_orientation().turns(),
            "effective": session.effective_orientation().turns(),
        },
    })
}

/// The distribution of the developed photograph, for the page to draw.
///
/// Counts rather than heights, and a peak to scale them by. Normalising here
/// would be the interface's decision made in the wrong place: how a histogram is
/// drawn — linear, square-root, clipped to a percentile — is a question about
/// reading it, and the answer would then be baked into the only copy of the
/// numbers.
/// `seen` is the generation the caller already drew, and returning nothing for
/// it is what makes this cheap to poll: the page can ask ten times a second and
/// pay for a lock and a comparison until there is actually something new.
#[tauri::command]
fn histogram(seen: Option<u64>) -> Option<serde_json::Value> {
    let scope = SCOPE.lock().expect("histogram lock");
    let survey = scope.as_ref()?;
    if seen == Some(survey.generation) {
        return None;
    }
    Some(serde_json::json!({
        "generation": survey.generation,
        "pixels": survey.histogram.pixels,
        "peak": survey.histogram.peak(),
        "clipped_white": survey.histogram.clipped_white,
        "clipped_black": survey.histogram.clipped_black,
        "red": &survey.histogram.red[..],
        "green": &survey.histogram.green[..],
        "blue": &survey.histogram.blue[..],
        "luma": &survey.histogram.luma[..],
    }))
}

/// Choose where an export goes, and leave it for the render loop to start.
///
/// Returns as soon as the dialog is open. The picker's own callback is what
/// records the destination, so nothing here waits on a person — a command that
/// blocked until someone had finished browsing their disk would hold a Tauri
/// thread for as long as they took.
#[tauri::command]
fn export(
    app: tauri::AppHandle,
    state: tauri::State<'_, Shelf>,
    scope: String,
) -> Result<(), String> {
    use tauri_plugin_dialog::DialogExt;

    let Some(library) = state.0.clone() else {
        return Err("no catalog is open, so there is nothing to export".into());
    };
    let (selection, name) = {
        let library = library.lock().expect("library lock");
        match scope.as_str() {
            "current" => {
                let current = library.current();
                let stem = std::path::Path::new(&current.filename)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| current.filename.clone());
                (
                    rawkit_deliver::Selection::Image(current.id),
                    Some(format!("{stem}.jpg")),
                )
            }
            // Picks rather than the marked set: a pick is the culling verdict and
            // it is stored, while marking is the transient "these ones" gesture
            // that compare and paste already use. Delivering the keepers is what
            // the flag is for.
            "picks" => (rawkit_deliver::Selection::Picks, None),
            other => return Err(format!("{other} is not something to export")),
        }
    };

    // One photograph is saved as a file the user names; a set goes into a folder.
    // The picker's own kind is what decides which, so the two cannot disagree.
    let one_file = name.is_some();
    let leave = move |chosen: Option<tauri_plugin_dialog::FilePath>| {
        let Some(path) = chosen.and_then(|p| p.into_path().ok()) else {
            return; // Cancelled, which is an answer and not an error.
        };
        let destination = if one_file {
            rawkit_deliver::Destination::File(path)
        } else {
            rawkit_deliver::Destination::Folder(path)
        };
        *PENDING_EXPORT.lock().expect("export lock") = Some((selection, destination));
    };
    let dialog = app.dialog().clone();
    match name {
        Some(filename) => dialog.file().set_file_name(filename).save_file(leave),
        None => dialog.file().pick_folder(leave),
    }
    Ok(())
}

/// How far along the export is, for the page to draw.
#[tauri::command]
fn export_progress() -> Option<serde_json::Value> {
    let exporting = EXPORTING.lock().expect("export lock");
    let exporting = exporting.as_ref()?;
    Some(serde_json::json!({
        "done": exporting.done,
        "total": exporting.total,
        "filename": exporting.filename,
        "finished": exporting.finished,
    }))
}

/// Choose a camera profile for the body on screen, and remember it.
///
/// Per camera rather than per photograph, because that is what a DCP describes.
/// The choice goes in the catalog, so an export made from the terminal renders
/// the same colour as the window did.
#[tauri::command]
fn choose_profile(app: tauri::AppHandle, state: tauri::State<'_, Shelf>) -> Result<(), String> {
    use tauri_plugin_dialog::DialogExt;

    let Some(library) = state.0.clone() else {
        return Err("no catalog is open, so there is nowhere to remember a profile".into());
    };
    let Some((make, model)) = CURRENT_CAMERA.lock().expect("camera lock").clone() else {
        return Err("no photograph is open, so there is no camera to profile".into());
    };

    let leave = move |chosen: Option<tauri_plugin_dialog::FilePath>| {
        let Some(path) = chosen.and_then(|p| p.into_path().ok()) else {
            return;
        };
        // Parsed before it is remembered: a file that is not a profile should
        // be refused at the moment somebody picks it, not silently stored and
        // then quietly ignored on every future open.
        let parsed = std::fs::read(&path)
            .ok()
            .and_then(|bytes| rawkit_engine::profile::dcp::parse(&bytes).ok());
        let Some(profile) = parsed else {
            eprintln!("profile    : {} is not a camera profile", path.display());
            return;
        };
        let library = library.lock().expect("library lock");
        if let Err(e) = rawkit_catalog::profiles::remember(
            library.catalog(),
            &make,
            &model,
            &path.to_string_lossy(),
            profile.name.as_deref(),
        ) {
            eprintln!("profile    : could not remember it: {e}");
            return;
        }
        eprintln!("profile    : {model} renders with {}", path.display());
        PROFILE_CHANGED.store(true, std::sync::atomic::Ordering::Relaxed);
    };
    app.dialog()
        .file()
        .add_filter("Camera profile", &["dcp"])
        .pick_file(leave);
    Ok(())
}

/// Go back to the decoder's own matrix for this camera.
#[tauri::command]
fn clear_profile(state: tauri::State<'_, Shelf>) -> Result<(), String> {
    let Some(library) = state.0.clone() else {
        return Err("no catalog is open".into());
    };
    let Some((make, model)) = CURRENT_CAMERA.lock().expect("camera lock").clone() else {
        return Err("no photograph is open".into());
    };
    rawkit_catalog::profiles::forget(
        library.lock().expect("library lock").catalog(),
        &make,
        &model,
    )
    .map_err(|e| e.to_string())?;
    PROFILE_CHANGED.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// The looks saved in this catalog, and what each one carries.
///
/// Group *names* cross the boundary, not indices: the page shows them, the
/// catalog stores them, and one vocabulary between the three means a reordered
/// enum cannot silently repoint a saved preset.
#[tauri::command]
fn presets(state: tauri::State<'_, Shelf>) -> Result<Vec<serde_json::Value>, String> {
    let Some(library) = state.0.clone() else {
        return Ok(Vec::new()); // No catalog is not an error, it is no presets.
    };
    let library = library.lock().expect("library lock");
    let saved = rawkit_catalog::presets::all(library.catalog()).map_err(|e| e.to_string())?;
    Ok(saved
        .into_iter()
        .map(|preset| {
            serde_json::json!({
                "name": preset.name,
                "groups": preset.groups.iter().map(|g| g.as_str()).collect::<Vec<_>>(),
            })
        })
        .collect())
}

/// Which parts of the edit on screen differ from the camera's own rendering.
///
/// What a save dialogue should tick. Offering everything would make every preset
/// carry six neutral settings that quietly reset whatever the target frame had.
#[tauri::command]
fn touched_groups(state: tauri::State<'_, Shared>) -> Vec<serde_json::Value> {
    let session = state.0.lock().expect("session lock");
    let touched = session.state().touched_groups();
    rawkit_editstate::Group::ALL
        .iter()
        .map(|group| {
            serde_json::json!({
                "name": group.as_str(),
                "label": group.label(),
                "touched": touched.contains(group),
            })
        })
        .collect()
}

/// Save the edit on screen as a look, carrying only the named groups.
#[tauri::command]
fn save_preset(
    shelf: tauri::State<'_, Shelf>,
    shared: tauri::State<'_, Shared>,
    name: String,
    groups: Vec<String>,
) -> Result<(), String> {
    let Some(library) = shelf.0.clone() else {
        return Err("no catalog is open, so there is nowhere to save a preset".into());
    };
    let groups = parse_groups(&groups)?;
    let state = shared.0.lock().expect("session lock").state().clone();
    let library = library.lock().expect("library lock");
    rawkit_catalog::presets::save(library.catalog(), &name, &state, &groups)
        .map_err(|e| e.to_string())
}

/// Put a saved look onto the photograph on screen.
///
/// Through the command bus like any other edit, so one press of undo takes it
/// back off. A preset that could not be undone would be the one edit in the
/// application that a user had to be careful with.
#[tauri::command]
fn apply_preset(
    shelf: tauri::State<'_, Shelf>,
    shared: tauri::State<'_, Shared>,
    name: String,
) -> Result<Event, String> {
    let Some(library) = shelf.0.clone() else {
        return Err("no catalog is open".into());
    };
    let preset = {
        let library = library.lock().expect("library lock");
        rawkit_catalog::presets::get(library.catalog(), &name).map_err(|e| e.to_string())?
    };
    let preset = preset.ok_or_else(|| format!("there is no preset called {name:?}"))?;

    let (event, applied) = {
        let mut session = shared.0.lock().expect("session lock");
        let mut state = session.state().clone();
        preset.apply_to(&mut state);
        let event = session.apply(Command::SetEditState(Box::new(state)));
        // Read back rather than reuse: a refused command leaves the session
        // where it was, and writing the state we *asked* for would record a
        // decision that never took effect.
        (event, session.state().clone())
    };

    // Written here, as a `Preset`, rather than left to the autosave — which
    // knows only that the edit changed and would file it as `User`. That column
    // exists to say where an edit came from, and a preset application followed
    // by hand corrections is exactly the pair it was put there to capture. The
    // autosave's own write lands on the same hash a moment later and does
    // nothing, so this costs one row rather than two.
    let library = library.lock().expect("library lock");
    let image = library.current().id;
    if let Err(e) = rawkit_catalog::edits::save(
        library.catalog(),
        image,
        &applied,
        rawkit_editstate::EditSource::Preset,
    ) {
        eprintln!("preset     : applied but not recorded: {e}");
    }
    Ok(event)
}

#[tauri::command]
fn forget_preset(state: tauri::State<'_, Shelf>, name: String) -> Result<(), String> {
    let Some(library) = state.0.clone() else {
        return Err("no catalog is open".into());
    };
    let library = library.lock().expect("library lock");
    rawkit_catalog::presets::forget(library.catalog(), &name).map_err(|e| e.to_string())
}

/// The places this photograph can be returned to.
#[tauri::command]
fn snapshots(state: tauri::State<'_, Shelf>) -> Result<Vec<serde_json::Value>, String> {
    let Some(library) = state.0.clone() else {
        return Ok(Vec::new());
    };
    let library = library.lock().expect("library lock");
    let taken = rawkit_catalog::snapshots::all(library.catalog(), library.current().id)
        .map_err(|e| e.to_string())?;
    Ok(taken
        .into_iter()
        .map(|s| serde_json::json!({ "name": s.name, "version": s.version }))
        .collect())
}

/// Name where this photograph is now.
///
/// The state is passed from the session rather than read from the catalog,
/// because the autosave has a settle delay: a snapshot of the last *written*
/// version would sometimes be a snapshot of something the user had already
/// moved on from, and they would not find out until they came back to it.
#[tauri::command]
fn take_snapshot(
    shelf: tauri::State<'_, Shelf>,
    shared: tauri::State<'_, Shared>,
    name: String,
) -> Result<(), String> {
    let Some(library) = shelf.0.clone() else {
        return Err("no catalog is open, so there is nowhere to keep a snapshot".into());
    };
    let state = shared.0.lock().expect("session lock").state().clone();
    let library = library.lock().expect("library lock");
    let image = library.current().id;
    rawkit_catalog::snapshots::take(library.catalog(), image, &name, &state)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Go back to a snapshot — which is a step *forward* in the history, and so
/// undoable like anything else.
#[tauri::command]
fn restore_snapshot(
    shelf: tauri::State<'_, Shelf>,
    shared: tauri::State<'_, Shared>,
    name: String,
) -> Result<Event, String> {
    let Some(library) = shelf.0.clone() else {
        return Err("no catalog is open".into());
    };
    let saved = {
        let library = library.lock().expect("library lock");
        let image = library.current().id;
        rawkit_catalog::snapshots::read(library.catalog(), image, &name)
            .map_err(|e| e.to_string())?
    };
    let saved = saved.ok_or_else(|| format!("there is no snapshot called {name:?}"))?;
    let mut session = shared.0.lock().expect("session lock");
    Ok(session.apply(Command::SetEditState(Box::new(saved))))
}

#[tauri::command]
fn forget_snapshot(state: tauri::State<'_, Shelf>, name: String) -> Result<(), String> {
    let Some(library) = state.0.clone() else {
        return Err("no catalog is open".into());
    };
    let library = library.lock().expect("library lock");
    let image = library.current().id;
    rawkit_catalog::snapshots::forget(library.catalog(), image, &name).map_err(|e| e.to_string())
}

/// A group name from the page, refused rather than skipped when it is not one
/// of ours — a preset that silently dropped a group would be a look nobody
/// designed, saved under a name somebody trusted.
fn parse_groups(names: &[String]) -> Result<Vec<rawkit_editstate::Group>, String> {
    names
        .iter()
        .map(|name| {
            rawkit_editstate::Group::parse(name)
                .ok_or_else(|| format!("{name:?} is not a part of an edit"))
        })
        .collect()
}

/// Move the boundary between the photograph and the controls.
///
/// The page owns the gesture and the shell owns the consequence: the canvas is a
/// window of its own on Linux and a hole in a surface elsewhere, and neither is
/// something a stylesheet can move. So the drag arrives here as a number and
/// leaves through the same slot a window resize uses — one path to relayout,
/// exercised by both, rather than a second one that only the divider takes.
///
/// Clamped here rather than in the page. A refusal the page could talk its way
/// past is not a limit, and the numbers are the shell's: they are what keeps the
/// widest label from wrapping and the photograph from becoming a strip.
#[tauri::command]
fn set_panel_width(window: tauri::Window, px: f64) -> Result<f64, String> {
    if !px.is_finite() {
        return Err("a panel width must be a number".into());
    }
    let px = px.clamp(PANEL_MIN, PANEL_MAX);
    PANEL_NOW.store(px as i32, std::sync::atomic::Ordering::Relaxed);
    // The window has not changed size; asking for a relayout at the size it
    // already is is what makes the divider and a resize the same operation.
    let size = window.inner_size().map_err(|e| e.to_string())?;
    *PENDING_RESIZE.lock().expect("resize lock") = Some((size.width, size.height));
    // Remembered with the rest of the geometry: reopening at the right size with
    // the panel back at its default is only half of coming back to where you
    // were. Rate-limited inside, so a drag does not write a file per frame.
    remember_window(&window.app_handle().clone(), &window, false);
    // Handed back so the page can settle on what it actually got rather than on
    // what it asked for — the clamp is invisible otherwise, and a divider that
    // stops moving while the pointer keeps going reads as a stuck window.
    Ok(px)
}

/// In and out of fullscreen.
///
/// A toggle rather than a setter because the key is a toggle, and asking the
/// window what it currently is keeps the two from drifting — a page holding its
/// own idea of the state would be wrong the moment the window manager did it.
#[tauri::command]
fn toggle_fullscreen(window: tauri::Window) -> Result<bool, String> {
    let now = window.is_fullscreen().map_err(|e| e.to_string())?;
    window.set_fullscreen(!now).map_err(|e| e.to_string())?;
    Ok(!now)
}

/// What the panel is currently, so the page can draw itself the right width on
/// load without a flash at the default.
#[tauri::command]
fn panel_width() -> f64 {
    PANEL_SHOWN.load(std::sync::atomic::Ordering::Relaxed) as f64
}

/// The open library, when a catalog was what was opened.
///
/// Separate state from [`Shared`] because the two answer to different things: a
/// slider changes the session, an arrow key changes the library, and only the
/// render loop ever needs both at once. Keeping them apart also keeps the lock
/// a keypress takes away from the lock a drag is holding sixty times a second.
struct Shelf(Option<Arc<Mutex<Library>>>);

/// Move through the shoot, or record what this frame is worth.
///
/// Returns what to draw in the status line. Errors come back as strings rather
/// than panicking the command thread: a failed write is something the page
/// should say, not something that should take the window down.
/// A pointer event from the page, in canvas pixels.
///
/// Only the cutout arrangement sends these: where the canvas has a window of its
/// own, GTK delivers the same events without a round trip. The page does the
/// same small job `canvas.rs` does — say where, in the surface's own
/// coordinates — and the meaning is decided in one place for both.
#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PointerEvent {
    Press { x: f64, y: f64, double: bool },
    Motion { x: f64, y: f64 },
    Release,
    Scroll { x: f64, y: f64, notches: f64 },
}

#[tauri::command]
fn canvas_pointer(state: tauri::State<'_, Shared>, event: PointerEvent) {
    let routed = match event {
        PointerEvent::Press { x, y, double } => pointer::Pointer::Press { at: [x, y], double },
        PointerEvent::Motion { x, y } => pointer::Pointer::Motion { at: [x, y] },
        PointerEvent::Release => pointer::Pointer::Release,
        PointerEvent::Scroll { x, y, notches } => pointer::Pointer::Scroll {
            at: [x, y],
            notches,
        },
    };
    pointer::route(routed, &state.0);
}

#[tauri::command]
fn cull(
    state: tauri::State<'_, Shelf>,
    session: tauri::State<'_, Shared>,
    action: CullAction,
) -> Result<CullView, String> {
    let Some(library) = &state.0 else {
        return Err("no library is open; pass a .rawkit catalog".into());
    };
    // Resolved here rather than in the library, because these are about the
    // *layout* — which view is showing and how many cells fit across it — and
    // the library knows about photographs, not pixels.
    let action = match action {
        CullAction::Grid => {
            MODE.store(MODE_GRID, std::sync::atomic::Ordering::Relaxed);
            CullAction::SelectBy(0)
        }
        // A survey needs at least two frames to be a comparison. Refusing is
        // better than showing one photograph in a view named for choosing
        // between several.
        CullAction::Survey => {
            let enough = library.lock().expect("library lock").marked().len() >= 2;
            if !enough {
                return Err("mark at least two frames with M before comparing them".into());
            }
            MODE.store(MODE_SURVEY, std::sync::atomic::Ordering::Relaxed);
            CullAction::SelectMarked(0)
        }
        // Copying reads the *session*, not the catalog: what should travel is
        // what is on screen, including slider movements that have not settled
        // into a saved version yet.
        CullAction::CopyEdit => {
            let state = session.0.lock().expect("session lock").state().clone();
            library.lock().expect("library lock").copy_edit(state);
            CullAction::SelectBy(0)
        }
        // Crop is a mode of the loupe rather than a view of its own: the same
        // photograph, at the same zoom, with a rectangle on it.
        CullAction::Crop => {
            let next = if mode() == MODE_CROP {
                MODE_LOUPE
            } else {
                MODE_CROP
            };
            *CANVAS_MARQUEE.lock().expect("marquee lock") = None;
            MODE.store(next, std::sync::atomic::Ordering::Relaxed);
            CullAction::SelectBy(0)
        }
        CullAction::CropApply => {
            // Turning a rectangle into a crop needs the viewport, which lives in
            // the render loop. This only says that it should happen.
            CROP_COMMIT.store(true, std::sync::atomic::Ordering::Relaxed);
            CullAction::SelectBy(0)
        }
        CullAction::CropCancel => {
            *CANVAS_MARQUEE.lock().expect("marquee lock") = None;
            MODE.store(MODE_LOUPE, std::sync::atomic::Ordering::Relaxed);
            CullAction::SelectBy(0)
        }
        CullAction::Loupe => {
            MODE.store(MODE_LOUPE, std::sync::atomic::Ordering::Relaxed);
            // The loupe has been showing something else since it last ran, so
            // the selection has to be asked for even though it did not move.
            library.lock().expect("library lock").reopen();
            CullAction::SelectBy(0)
        }
        CullAction::Cells(steps) => {
            let cell = GRID_CELL.load(std::sync::atomic::Ordering::Relaxed) as i32;
            let next = (cell + steps * 80).clamp(120, 900) as u32;
            GRID_CELL.store(next, std::sync::atomic::Ordering::Relaxed);
            CullAction::SelectBy(0)
        }
        // A row is however many cells fit across right now.
        CullAction::SelectBy(rows) if mode() == MODE_GRID => CullAction::SelectBy(
            rows * GRID_COLUMNS.load(std::sync::atomic::Ordering::Relaxed) as i32,
        ),
        // In the loupe there are no rows, so up and down are just neighbours.
        CullAction::SelectBy(rows) => {
            if rows < 0 {
                CullAction::Previous
            } else if rows > 0 {
                CullAction::Next
            } else {
                CullAction::SelectBy(0)
            }
        }
        // In a survey, arrows move within the comparison and P and X narrow it.
        // Same keys, same meanings, over a smaller set.
        CullAction::Next if mode() == MODE_SURVEY => CullAction::SelectMarked(1),
        CullAction::Previous if mode() == MODE_SURVEY => CullAction::SelectMarked(-1),
        CullAction::Pick if mode() == MODE_SURVEY => CullAction::SurveyJudge(true),
        CullAction::Reject if mode() == MODE_SURVEY => CullAction::SurveyJudge(false),
        // Arrows move without loading in the grid and with loading in the loupe,
        // which is the same key meaning the same thing in both.
        CullAction::Next if mode() == MODE_GRID => CullAction::SelectNext,
        CullAction::Previous if mode() == MODE_GRID => CullAction::SelectPrevious,
        other => other,
    };

    library
        .lock()
        .expect("library lock")
        .act(action)
        .map_err(|e| e.to_string())
}

/// The status line as it stands, for a page that has just loaded.
#[tauri::command]
fn cull_view(state: tauri::State<'_, Shelf>) -> Option<CullView> {
    let library = state.0.as_ref()?;
    library.lock().expect("library lock").view().ok()
}

fn main() -> Result<()> {
    // Before GTK, before Tauri, before anything opens a display.
    #[cfg(target_os = "linux")]
    canvas::init_threads();

    let route = match std::env::var("RAWKIT_PROBE_ROUTE").as_deref() {
        Ok("1") => Route::Cutout,
        Ok("2") => Route::ChildWebview,
        Ok("3") => Route::NativeChild,
        // The default is per platform, and each is the one the probe found
        // works there. Route 3 needs a native widget to hang the surface on,
        // which so far means a GTK one; route 1 needs a transparent window,
        // which is exactly what Linux/WebKitGTK cannot composite. Neither is a
        // fallback for the other — they are the two answers to one question,
        // and which is right depends on the compositor.
        _ if cfg!(target_os = "linux") => Route::NativeChild,
        _ => Route::Cutout,
    };
    // One positional argument. A `.rawkit` file opens a library and its first
    // image; anything else is a raw opened directly, which is how the shell has
    // always worked and stays useful when there is no catalog to hand.
    let target = std::env::args().nth(1).map(PathBuf::from);
    eprintln!("route      : {route:?}");

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            apply,
            snapshot,
            cull,
            cull_view,
            canvas_pointer,
            histogram,
            export,
            export_progress,
            choose_profile,
            clear_profile,
            presets,
            touched_groups,
            save_preset,
            apply_preset,
            forget_preset,
            snapshots,
            take_snapshot,
            restore_snapshot,
            forget_snapshot,
            set_panel_width,
            panel_width,
            toggle_fullscreen,
            arm_target,
            pick_white_balance,
            add_mask,
            remove_mask,
            select_mask,
            place_mask,
            set_mask
        ])
        .setup(move |app| {
            // The surface is created on the main thread because the raw window
            // handle comes from GTK on this platform and GTK is not thread-safe.
            // Where it was last time, before anything is built from a size.
            // The panel is part of it, and has to be in place before the canvas
            // is created — otherwise the first frame is drawn at the default
            // width and the window visibly settles a moment after it opens.
            let saved = window_state::load(&app.handle().clone());
            if let Some(saved) = saved {
                PANEL_NOW.store(
                    saved.panel.clamp(PANEL_MIN, PANEL_MAX) as i32,
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
            let (gpu, surface, layout, window_handle) = build_window(app, route, saved)?;
            eprintln!(
                "gpu        : {} ({:?})",
                gpu.adapter_info.name, gpu.adapter_info.backend
            );
            let config = surface
                .get_default_config(&gpu.adapter, layout.surface.width, layout.surface.height)
                .ok_or_else(|| anyhow!("this surface supports no configuration we can use"))?;
            eprintln!(
                "surface    : {:?} {}x{}",
                config.format, layout.surface.width, layout.surface.height
            );
            if layout.canvas != layout.surface {
                eprintln!(
                    "canvas     : {}x{} (the chrome floats over the rest of the surface)",
                    layout.canvas.width, layout.canvas.height
                );
            }
            surface.configure(&gpu.device, &config);
            let mut config = config;

            // The window manager may resize us at any moment; the surface can
            // only be reconfigured between frames. So the event leaves the new
            // size in a slot and the render loop picks it up — see
            // `PENDING_RESIZE` for why a slot and not a queue.
            //
            // A floor rather than a clamp afterwards: below this the panel would
            // be most of the window and the photograph a strip, and the honest
            // way to say that is to make the window refuse to go there.
            window_handle
                .set_min_size(Some(tauri::LogicalSize::new(MIN_WINDOW.0, MIN_WINDOW.1)))?;
            let remembering = window_handle.clone();
            let app_handle = app.handle().clone();
            window_handle.on_window_event(move |event| {
                match event {
                    tauri::WindowEvent::Resized(size) => {
                        *PENDING_RESIZE.lock().expect("resize lock") =
                            Some((size.width, size.height));
                    }
                    // Moving changes nothing about what is drawn, and is worth
                    // hearing only so the window comes back where it was.
                    tauri::WindowEvent::Moved(_) => {}
                    // The last chance: a window manager sends this before it
                    // takes the window away, and `Drop` will not run for a
                    // process that is killed.
                    tauri::WindowEvent::CloseRequested { .. } => {
                        remember_window(&app_handle, &remembering, true);
                        return;
                    }
                    _ => return,
                }
                remember_window(&app_handle, &remembering, false);
            });

            // A library, if that is what was passed. The catalog is what makes
            // an edit outlive the process, so it is opened before the image it
            // describes rather than bolted on after.
            let library = match target.as_deref().filter(|p| is_catalog(p)) {
                Some(path) => Some(Arc::new(Mutex::new(Library::open(path)?))),
                None => None,
            };
            let raw = match &library {
                Some(library) => Some(PathBuf::from(
                    &library.lock().expect("library lock").current().path,
                )),
                None => target.clone().filter(|p| !is_catalog(p)),
            };
            app.manage(Shelf(library.clone()));

            let mut loaded = Loaded::open(raw.as_deref(), DEFAULT_TILE)?;
            *PROFILE_NAME.lock().expect("profile lock") =
                apply_profile(library.as_ref(), &mut loaded);
            let mut session = Session::new(
                loaded.size,
                DEFAULT_TILE,
                EditState::default(),
                loaded.orientation,
            );
            session.apply(Command::Resize {
                width: layout.canvas.width,
                height: layout.canvas.height,
            });
            session.apply(Command::FitToView);

            let shared = Arc::new(Mutex::new(session));
            app.manage(Shared(shared.clone()));

            let mut saver = Saver::new(library.clone(), shared.clone());
            // For the export, which reads the catalog on this thread and writes
            // on another.
            let exporting_from = library.clone();
            // Whatever was last decided about this photograph, if anything was.
            saver.restore(&mut shared.lock().expect("session lock"));

            // Routes 1 and 2 put the canvas over the whole window; route 3 gives
            // it a window of its own, ending where the chrome begins.
            let panel = if route == Route::NativeChild {
                PANEL_PIXELS
            } else {
                0
            };
            attach_input(&window_handle, panel, shared.clone())?;

            let mut canvas_renderer = session_canvas::CanvasRenderer::new(
                &gpu,
                &loaded.frame(),
                [layout.canvas.width, layout.canvas.height],
            );
            let blit = rawkit_engine::PreviewBlit::new(&gpu);
            // One white pixel, uploaded once. The crop outline is drawn as four
            // thin cells whose edge colour covers them entirely, so this is
            // never sampled — but a cell wants an image, and inventing one per
            // frame would allocate a texture thirty times a second.
            let white = blit.upload(&gpu, &[255, 255, 255, 255], 1, 1)?;
            let mut grid = Grid {
                cells: std::collections::HashMap::new(),
                scroll: 0.0,
            };
            let mut showing = Showing {
                path: raw.clone(),
                size: loaded.size,
                orientation: loaded.orientation,
                preview: None,
                raw: Some(loaded),
            };

            // Correct for the monitor when the desktop says what it is. The
            // table already encodes, so the surface has to stop doing it —
            // hence reconfiguring to the non-sRGB form of the same format.
            let lut = display_profile().and_then(|bytes| {
                match rawkit_export::display::DisplayLut::from_icc(&bytes) {
                    Ok(Some(lut)) => {
                        eprintln!("display    : correcting for {}", lut.description());
                        Some(lut)
                    }
                    Ok(None) => {
                        eprintln!("display    : monitor profile matches sRGB, no correction");
                        None
                    }
                    Err(e) => {
                        eprintln!("display    : unusable monitor profile: {e}");
                        None
                    }
                }
            });
            match &lut {
                Some(lut) => {
                    let plain = config.format.remove_srgb_suffix();
                    if plain != config.format {
                        config.format = plain;
                        config.view_formats = vec![plain];
                        surface.configure(&gpu.device, &config);
                    }
                    canvas_renderer.target_with_lut(&gpu, config.format, lut);
                }
                None => canvas_renderer.target(&gpu, config.format),
            }

            // One frame: let the session decide, draw what it asked for, blit.
            //
            // The counters are here because the next optimisation — caching a
            // texture per tile so a pan redraws only what it exposes — should be
            // decided by what a pan actually costs rather than by the fact that
            // it obviously costs something.
            let mut surface_size = [layout.canvas.width, layout.canvas.height];
            // Where the photograph goes inside the swapchain. Under route 3 that
            // is all of it; under a cutout it starts below the chrome.
            // The origin under every route now: the chrome is a column beside
            // the photograph rather than a strip above it, so there is nothing
            // to push it down past.
            let mut canvas_rect = [0, 0, layout.canvas.width, layout.canvas.height];
            let mut stats = FrameStats::default();
            let navigating = library.clone();
            // The rectangle the canvas currently carries, so a settled one is
            // not redrawn on every frame. See the crop block below.
            let mut last_marquee: Option<[f64; 4]> = None;
            // When the histogram was last recomputed. See `SURVEY_INTERVAL`.
            let mut last_survey: Option<std::time::Instant> = None;
            // What the coarse-while-dragging decision is made from: the edit
            // generation last seen and when it changed, and how long the last
            // frame that actually drew tiles took.
            //
            // The decision is taken once, when a gesture *starts*, and held for
            // its duration. Re-deciding every frame is the obvious way and the
            // wrong one: going coarse makes frames fast, fast frames say full
            // resolution would be fine, and the picture oscillates between sharp
            // and soft for as long as the slider is moving.
            let mut seen_generation = 0u64;
            let mut edited_at: Option<std::time::Instant> = None;
            let mut last_draw = std::time::Duration::ZERO;
            // Read once. Following a window onto a monitor with a different
            // HiDPI factor is a separate problem and deliberately not this one;
            // the value is captured here rather than re-read so that nothing
            // half-handles it.
            let scale = window_handle.scale_factor()?;
            // Only the native-child canvas has a window of its own to move; a
            // cutout's surface is the toplevel's and the layout is all there is.
            #[cfg(target_os = "linux")]
            let resizing = window_handle.clone();

            let mut tick = move || -> Result<()> {
                let started = std::time::Instant::now();

                // A resize the window manager has already performed, applied
                // between frames because that is the only moment a swapchain can
                // be rebuilt. Everything downstream is derived from `layout`, so
                // this is the one place that has to change.
                if let Some((width, height)) = PENDING_RESIZE.lock().expect("resize lock").take() {
                    let now = Layout::for_window(
                        route,
                        tauri::PhysicalSize::new(width.max(1), height.max(1)),
                        scale,
                    );
                    // On Linux the canvas is a window of its own, and GTK has
                    // already moved it — this covers the other case, the divider
                    // shifting where the panel ends while the window stays put.
                    #[cfg(target_os = "linux")]
                    if route == Route::NativeChild {
                        canvas::reposition(&resizing, &PANEL_SHOWN)?;
                    }
                    config.width = now.surface.width;
                    config.height = now.surface.height;
                    surface.configure(&gpu.device, &config);
                    surface_size = [now.canvas.width, now.canvas.height];
                    canvas_rect = [0, 0, now.canvas.width, now.canvas.height];
                    // The session measures the viewport in these pixels, so it
                    // has to be told before anything asks it what to draw.
                    let mut session = shared.lock().expect("session lock");
                    session.apply(Command::Resize {
                        width: surface_size[0],
                        height: surface_size[1],
                    });
                    canvas_renderer.invalidate();
                }

                // The grid draws from the same cache the loupe does and touches
                // nothing else: no decode, no session, no viewport. Returning
                // here is what keeps the two views from having to know about
                // each other.
                if in_grid() {
                    if let Some(library) = &navigating {
                        let drawn = draw_grid(
                            &gpu,
                            &blit,
                            &mut canvas_renderer,
                            library,
                            &mut grid,
                            surface_size,
                        )?;
                        paint(
                            &gpu,
                            &surface,
                            canvas_renderer.presenter(),
                            canvas_renderer.canvas(),
                            canvas_rect,
                        )?;
                        stats.record(drawn, started.elapsed());
                        return Ok(());
                    }
                    // No library, so nothing to lay out. Fall through to the
                    // loupe rather than showing an empty grid.
                    MODE.store(MODE_LOUPE, std::sync::atomic::Ordering::Relaxed);
                }

                // A keypress left a request; this is where it costs anything.
                // Decoding here rather than in the command handler keeps the IPC
                // call immediate and keeps a fifth of a second of CPU off the
                // thread the compositor is waiting on.
                // Before anything else: a paste writes to the catalog under the
                // frame on screen, so an unsettled slider movement has to land
                // first or the pending save would put the old edit straight back
                // — and the paste would look like it had skipped one frame.
                let pasting = navigating
                    .as_ref()
                    .is_some_and(|l| l.lock().expect("library lock").take_paste());
                if pasting {
                    saver.flush();
                    let library = navigating.as_ref().expect("a paste implies a library");
                    let outcome = library.lock().expect("library lock").paste_into_marked();
                    match outcome {
                        Ok(count) => eprintln!("paste      : {count} frame(s)"),
                        Err(e) => eprintln!("paste      : {e}"),
                    }
                    // The frame on screen may have been one of them, so its edit
                    // is re-read rather than assumed unchanged.
                    let mut session = shared.lock().expect("session lock");
                    saver.restore(&mut session);
                }

                let requested = navigating
                    .as_ref()
                    .and_then(|l| l.lock().expect("library lock").take_request());
                if let Some(path) = requested {
                    let opening = std::time::Instant::now();
                    // Before the photograph changes, not after: the edit being
                    // written belongs to the one on screen now.
                    saver.flush();

                    let library = navigating.as_ref().expect("a request implies a library");
                    // A header parse, so the viewport can be set up for a
                    // photograph nothing has decoded.
                    let (size, orientation) =
                        library.lock().expect("library lock").size_of_current()?;

                    let mut session = shared.lock().expect("session lock");
                    if (size, orientation) != (showing.size, showing.orientation) {
                        // A different body, or a different orientation. Nothing
                        // about the old view means anything, so start again.
                        //
                        // The orientation half of that comment was a claim this
                        // code could not make until the header parse started
                        // reporting it: two frames off one body are the same
                        // sensor size whichever way up they were taken.
                        *session =
                            Session::new(size, DEFAULT_TILE, EditState::default(), orientation);
                        session.apply(Command::Resize {
                            width: surface_size[0],
                            height: surface_size[1],
                        });
                        session.apply(Command::FitToView);
                    }
                    // Same size: the viewport is left exactly as it was, which
                    // is what carries a 1:1 sharpness check from frame to frame.
                    showing.size = size;
                    showing.orientation = orientation;
                    showing.path = Some(PathBuf::from(&path));
                    showing.raw = None;
                    // The edit first, because which preview is current depends on
                    // it — a preview of an edit that has since changed is not a
                    // preview of this photograph.
                    saver.restore(&mut session);

                    let needed = needed_pixels(&session, size);
                    let hash = session.state().content_hash();
                    let found = library
                        .lock()
                        .expect("library lock")
                        .preview_for(needed, &hash)?;
                    showing.preview = match found {
                        Some(decoded) => {
                            Some(blit.upload(&gpu, &decoded.rgba, decoded.width, decoded.height)?)
                        }
                        None => None,
                    };
                    eprintln!(
                        "open       : {} in {:.0} ms",
                        if showing.preview.is_some() {
                            "preview"
                        } else {
                            "no preview, decoding"
                        },
                        opening.elapsed().as_secs_f64() * 1000.0
                    );
                }

                saver.tick();

                // A profile the picker has just stored. Applied on this thread
                // because the profile changes the size of a GPU buffer, and
                // this is the thread that owns them.
                if PROFILE_CHANGED.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    if let Some(loaded) = showing.raw.as_mut() {
                        *PROFILE_NAME.lock().expect("profile lock") =
                            apply_profile(exporting_from.as_ref(), loaded);
                        canvas_renderer.reload(&gpu, &loaded.frame());
                        canvas_renderer.invalidate();
                        // The histogram describes colour, so it is stale too.
                        *SCOPE.lock().expect("histogram lock") = None;
                    }
                }

                // A destination the picker has already collected. Gathered here
                // rather than in the command, because this is the thread that
                // owns the saver: `flush` puts the edit you were making a moment
                // ago into the catalog before anything reads it back out.
                let chosen = PENDING_EXPORT.lock().expect("export lock").take();
                if let Some((selection, destination)) = chosen {
                    saver.flush();
                    if let Err(e) = begin_export(exporting_from.as_ref(), selection, destination) {
                        eprintln!("export     : {e:#}");
                        *EXPORTING.lock().expect("export lock") = Some(Exporting {
                            done: 0,
                            total: 0,
                            filename: String::new(),
                            finished: Some(format!("export failed: {e}")),
                        });
                    }
                }

                // Does what is on disk still have enough pixels for what the view
                // is showing? Zooming in past it is the one thing that makes a
                // decode necessary, and it is also the one time it is worth it.
                let covered = {
                    let session = shared.lock().expect("session lock");
                    let needed = needed_pixels(&session, showing.size);
                    showing
                        .preview
                        .as_ref()
                        .is_some_and(|p| p.width.max(p.height) >= needed)
                };
                if !covered && showing.raw.is_none() {
                    let decoding = std::time::Instant::now();
                    let mut next = Loaded::open(showing.path.as_deref(), DEFAULT_TILE)?;
                    // Before the reload, not after: the profile decides how big
                    // the table buffer is, and `reload` is what allocates it.
                    *PROFILE_NAME.lock().expect("profile lock") =
                        apply_profile(exporting_from.as_ref(), &mut next);
                    canvas_renderer.reload(&gpu, &next.frame());
                    showing.size = next.size;
                    showing.raw = Some(next);
                    // From here on this photograph renders. Holding the preview
                    // as well would mean two answers to what is on screen.
                    showing.preview = None;
                    eprintln!(
                        "decode     : {:.0} ms",
                        decoding.elapsed().as_secs_f64() * 1000.0
                    );
                }

                // Coarse while the edit is moving, but only when full
                // resolution is not keeping up — measured from the last frame
                // that actually drew something, not assumed.
                let hurrying =
                    {
                        let mut session = shared.lock().expect("session lock");
                        let generation = session.generation();
                        let moving =
                            if generation != seen_generation {
                                let starting = edited_at.is_none_or(|at: std::time::Instant| {
                                    at.elapsed() >= EDIT_SETTLES
                                });
                                seen_generation = generation;
                                edited_at = Some(std::time::Instant::now());
                                // The decision, taken once per gesture. Between the
                                // first change and the settle it is simply held.
                                if starting && session.set_haste(last_draw > FRAME_BUDGET) {
                                    eprintln!(
                                "haste      : {} (a full-resolution pass last took {:.0} ms)",
                                if last_draw > FRAME_BUDGET { "coarse" } else { "sharp" },
                                last_draw.as_secs_f64() * 1000.0
                            );
                                }
                                true
                            } else {
                                edited_at.is_some_and(|at| at.elapsed() < EDIT_SETTLES)
                            };
                        // Settled: back to the level the zoom asked for, which makes
                        // the fine tiles stale and renders them over the next frame
                        // or two. That is the sharpening-up, and it is the whole
                        // reason the coarse pass is allowed to be soft.
                        if !moving {
                            session.set_haste(false);
                        }
                        session.level() != session.viewport().level(session.max_level())
                    };

                // A white-balance pick. Resolved here because turning a canvas
                // position into a rectangle of *sensor* needs the viewport, the
                // geometry and the mosaic, and this is where all three meet.
                if let (Some(at), Some(loaded)) = (
                    WB_PICK.lock().expect("pick lock").take(),
                    showing.raw.as_ref(),
                ) {
                    let rect = {
                        let session = shared.lock().expect("session lock");
                        let half = WB_SAMPLE / 2.0;
                        let a = session.viewport().image_at([at[0] - half, at[1] - half]);
                        let b = session.viewport().image_at([at[0] + half, at[1] + half]);
                        // Through the geometry, so a rotated or straightened
                        // frame picks the photosites actually under the pointer
                        // rather than the ones that would have been there before
                        // the frame was turned.
                        session
                            .geometry()
                            .sensor_rect([a[0], a[1], b[0], b[1]], session.image_size())
                    };
                    match loaded.channels_over(rect) {
                        None => notice("that is outside the photograph"),
                        Some(camera) => match neutralising(camera, loaded.profile()) {
                            Err(why) => notice(why),
                            Ok((kelvin, tint)) => {
                                let mut session = shared.lock().expect("session lock");
                                session.apply(Command::SetTemperature(Some(kelvin)));
                                session.apply(Command::SetTint(tint));
                                notice(format!("white balance {kelvin:.0} K, tint {tint:.0}"));
                            }
                        },
                    }
                }

                // A gradient being placed. Resolved here for the same reason the
                // white-balance pick is: turning two canvas positions into two
                // places on the *sensor* needs the viewport and the geometry,
                // and this is where both are to hand.
                if let (Some((from, to)), Some(index)) =
                    (*MASK_DRAG.lock().expect("mask drag lock"), placing_mask())
                {
                    // A press with no travel is not a gradient — and a
                    // zero-length one has no direction, which the state layer
                    // refuses. Left alone until the hand has actually moved.
                    let moved = (to[0] - from[0]).hypot(to[1] - from[1]);
                    if moved > 4.0 {
                        let mut session = shared.lock().expect("session lock");
                        let existing = session.state().masks.get(index).map(|m| m.shape).unwrap_or(
                            rawkit_editstate::MaskShape::Linear {
                                from: [0.5, 0.0],
                                to: [0.5, 0.35],
                            },
                        );
                        let shape = shape_from_drag(
                            existing,
                            from,
                            to,
                            session.viewport(),
                            &session.geometry(),
                            session.image_size(),
                        );
                        let mut masks = session.state().masks.clone();
                        if let Some(mask) = masks.get_mut(index) {
                            if mask.shape != shape {
                                mask.shape = shape;
                                session.apply(Command::SetMasks {
                                    masks,
                                    // The shape, of this mask. A drag is one
                                    // undo step however many frames it spans.
                                    control: (index as u8) * 8,
                                });
                            }
                        }
                    }
                }

                // An aim taken but not yet resolved: the press said where, and
                // this is the only place that holds the canvas and can say what
                // colour is there. One small readback per gesture, not per
                // frame, and taken here — before the canvas pass — for the same
                // reason the survey is: a poll waits for the queue.
                if let Some(control) = targeting() {
                    let pending = {
                        let aim = TARGET_AIM.lock().expect("aim lock");
                        aim.as_ref().filter(|a| a.picked.is_none()).map(|a| a.at)
                    };
                    if let Some(at) = pending {
                        let sampled = canvas_renderer.canvas().sample(
                            &gpu,
                            [at[0].max(0.0) as u32, at[1].max(0.0) as u32],
                            TARGET_SAMPLE,
                        )?;
                        // A grey has no hue to aim by, and the mixer leaves it
                        // alone in any case — so pointing at one arms nothing
                        // rather than handing the gesture to red.
                        let picked = sampled.and_then(hue_of).map(|hue| {
                            let bands = rawkit_editstate::Band::spanning(hue);
                            let session = shared.lock().expect("session lock");
                            let hsl = &session.state().hsl;
                            let start = [
                                hsl.mix(bands[0].0).get(control),
                                hsl.mix(bands[1].0).get(control),
                            ];
                            (bands, start)
                        });
                        match picked {
                            Some(picked) => {
                                if let Some(aim) = TARGET_AIM.lock().expect("aim lock").as_mut() {
                                    aim.picked = Some(picked);
                                }
                            }
                            // Nothing to aim at, so the gesture ends rather than
                            // waiting for a colour that is not coming.
                            None => *TARGET_AIM.lock().expect("aim lock") = None,
                        }
                    }
                }

                // The survey goes *before* the canvas, and the order is the
                // whole cost of it.
                //
                // Surveying ends in a blocking device poll, and a poll waits for
                // everything already queued — so behind a full canvas pass it was
                // mostly waiting for tiles it had nothing to do with. Measured on
                // the same drag: **33-45 ms after the canvas, 6-9 ms before it**,
                // with the render itself unchanged at around 94,000 pixels. The
                // placement used to be incidental; it is not any more, and moving
                // it back would quietly cost four times as much.
                //
                // Recomputed when the *edit* changes and not when the view
                // does, because that is the difference between a histogram of
                // the photograph and a histogram of the window. During a slider
                // drag this runs at most once a frame however many commands
                // arrived, for the same reason only the last command is drawn.
                if let Some(loaded) = &showing.raw {
                    let (generation, state) = {
                        let session = shared.lock().expect("session lock");
                        (session.generation(), session.state().clone())
                    };
                    let known = SCOPE
                        .lock()
                        .expect("histogram lock")
                        .as_ref()
                        .map(|survey| survey.generation);
                    let due = last_survey
                        .is_none_or(|at: std::time::Instant| at.elapsed() >= SURVEY_INTERVAL);
                    if known != Some(generation) && due {
                        let counting = std::time::Instant::now();
                        last_survey = Some(counting);
                        let counted = canvas_renderer
                            .survey(&gpu, &loaded.frame(), &loaded.pyramid(), &state)
                            .and_then(|developed| {
                                let size = [developed.width, developed.height];
                                rawkit_export::histogram::Histogram::of(
                                    &developed.pixels,
                                    developed.width,
                                    developed.height,
                                )
                                .map(|histogram| (size, histogram))
                                .map_err(anyhow::Error::from)
                            });
                        match counted {
                            Ok(([width, height], histogram)) => {
                                // Once, because the coarsest level is one tile
                                // whatever the photograph is, so the second
                                // measurement would say the same as the first.
                                static REPORTED: std::sync::Once = std::sync::Once::new();
                                REPORTED.call_once(|| {
                                    eprintln!(
                                        "histogram  : {width}x{height} in {:.1} ms",
                                        counting.elapsed().as_secs_f64() * 1000.0
                                    )
                                });
                                *SCOPE.lock().expect("histogram lock") = Some(Survey {
                                    generation,
                                    histogram,
                                });
                            }
                            // Not fatal. The photograph is on screen; a missing
                            // histogram is a missing readout, not a broken
                            // editor, and stopping the frame loop over one would
                            // take the picture away as well.
                            Err(e) => eprintln!("histogram  : {e:#}"),
                        }
                    }
                }
                paint(
                    &gpu,
                    &surface,
                    canvas_renderer.presenter(),
                    canvas_renderer.canvas(),
                    canvas_rect,
                )?;
                let rendering = std::time::Instant::now();
                let drawn = match (&showing.raw, &showing.preview) {
                    (Some(loaded), _) => {
                        let mut session = shared.lock().expect("session lock");
                        canvas_renderer.advance(
                            &gpu,
                            &mut session,
                            &loaded.frame(),
                            &loaded.pyramid(),
                            surface_size,
                        )?
                    }
                    (None, Some(preview)) => {
                        let session = shared.lock().expect("session lock");
                        canvas_renderer.show_preview(
                            &gpu,
                            &blit,
                            preview,
                            &session,
                            showing.size,
                            surface_size,
                        );
                        0
                    }
                    // Unreachable: the branch above decodes whenever no preview
                    // covers the view. Drawing nothing is the safe answer if that
                    // ever stops being true.
                    (None, None) => 0,
                };
                let drawing = rendering.elapsed();
                // The outline goes on after the tiles, into the same canvas, and
                // the canvas is only written where a tile landed — so a moving
                // rectangle would leave its previous position behind. Redrawing
                // every visible tile each frame is what stops that; at fit zoom
                // it is about six of them.
                if in_crop() {
                    let marquee = *CANVAS_MARQUEE.lock().expect("marquee lock");
                    if CROP_COMMIT.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        if let Some(marquee) = marquee {
                            let mut session = shared.lock().expect("session lock");
                            if let Some(crop) = crop_from(&session, &marquee) {
                                // No explicit fit: the session refits whenever
                                // the photograph changes shape, so asking again
                                // here would be a second opinion on the same
                                // question.
                                session.apply(Command::SetCrop(crop));
                            }
                        }
                        *CANVAS_MARQUEE.lock().expect("marquee lock") = None;
                        MODE.store(MODE_LOUPE, std::sync::atomic::Ordering::Relaxed);
                        canvas_renderer.invalidate();
                    } else if let Some(marquee) = marquee {
                        let session = shared.lock().expect("session lock");
                        draw_marquee(&gpu, &blit, &white, &canvas_renderer, &session, &marquee);
                        // Only when the rectangle actually moved. The outline is
                        // drawn into the canvas, so erasing the old one means
                        // redrawing every visible tile — the cost of a pan. Doing
                        // that unconditionally is a full redraw every frame
                        // forever, and since this loop is GTK's main thread, the
                        // window stops answering: it does not look like a slow
                        // canvas, it looks like the application has hung.
                        let rect = marquee.rect();
                        if last_marquee != Some(rect) {
                            last_marquee = Some(rect);
                            canvas_renderer.invalidate();
                        }
                    } else {
                        last_marquee = None;
                    }
                } else if last_marquee.take().is_some() {
                    // Escape left crop mode, and the outline is still painted
                    // into the canvas. One redraw takes it off; without this it
                    // stays until something else happens to move the view.
                    canvas_renderer.invalidate();
                }
                let cost = started.elapsed();
                // Only frames that drew tiles say anything about what rendering
                // costs; an idle frame is a lock and a blit, and letting one
                // reset the estimate would tell the next gesture that full
                // resolution is cheap.
                //
                // The *tiles*, timed on their own rather than the whole frame.
                // A frame also surveys the histogram and presents, and neither
                // gets cheaper at a coarser level — including them would have
                // the decision react to costs it cannot do anything about.
                // Only a *full-resolution* pass says what full resolution costs.
                //
                // Measuring a coarse frame here is how the decision eats itself:
                // going coarse makes the pass cheap, the next gesture reads that
                // as "sharp would be fine", renders sharp, and the drag stutters
                // — which is exactly what the latch was supposed to prevent, and
                // it happened anyway until this line asked which level the
                // number came from. `haste : sharp (last tile pass 27 ms)` in the
                // log, with 27 ms being the coarse pass it had just made cheap.
                if drawn > 0 && !hurrying {
                    last_draw = drawing;
                }
                stats.record(drawn, cost);
                Ok(())
            };

            // On Linux the frame runs on the main thread, driven by GTK's own
            // loop. Not a style preference: presenting from a spawned thread
            // means two threads on one Xlib connection, and while XInitThreads
            // licenses that, a canvas should draw on the compositor's schedule
            // rather than on a sleep. This timer is a step towards GTK's frame
            // clock, not the destination.
            #[cfg(target_os = "linux")]
            gtk::glib::timeout_add_local(
                std::time::Duration::from_millis(16),
                move || match tick() {
                    Ok(()) => gtk::glib::ControlFlow::Continue,
                    Err(e) => {
                        eprintln!("paint: {e}");
                        gtk::glib::ControlFlow::Break
                    }
                },
            );
            // Elsewhere there is no equivalent constraint and no GTK loop to
            // hook, so a thread it is — until each platform's canvas arrives
            // with its own idea of when a frame should happen.
            #[cfg(not(target_os = "linux"))]
            std::thread::spawn(move || loop {
                if let Err(e) = tick() {
                    eprintln!("paint: {e}");
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(16));
            });

            Ok(())
        })
        .run(tauri::generate_context!())?;
    Ok(())
}

/// Read the catalog for what was chosen, then hand the writing to a thread.
///
/// The split is the point. Gathering holds the library's lock, and holding it
/// for a whole export would freeze navigation for as long as the export took;
/// writing holds nothing, so the window stays live and you can carry on culling
/// while files appear.
fn begin_export(
    library: Option<&Arc<Mutex<Library>>>,
    selection: rawkit_deliver::Selection,
    destination: rawkit_deliver::Destination,
) -> Result<()> {
    let library = library.ok_or_else(|| anyhow::anyhow!("no catalog is open"))?;
    let chosen = {
        let library = library.lock().expect("library lock");
        rawkit_deliver::check_destination(library.catalog(), &destination)?;
        rawkit_deliver::gather(library.catalog(), selection)?
    };

    let total = chosen.len();
    *EXPORTING.lock().expect("export lock") = Some(Exporting {
        done: 0,
        total,
        filename: String::new(),
        finished: None,
    });

    // A file the user named through a save dialog has already been asked about,
    // and answering that question twice — once in the dialog, once by silently
    // skipping — would make the second answer a lie. Into a folder, an existing
    // file is skipped and counted, because nothing asked.
    let overwrite = matches!(destination, rawkit_deliver::Destination::File(_));
    let where_to = match &destination {
        rawkit_deliver::Destination::File(path) => path.display().to_string(),
        rawkit_deliver::Destination::Folder(dir) => dir.display().to_string(),
    };

    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        // Full resolution, always. An export is a delivery, and the pyramid's
        // averaging softens the edge of a blown highlight — acceptable in
        // something you look at, not in something you send.
        let outcome = rawkit_deliver::write(
            &chosen,
            &destination,
            rawkit_deliver::Delivery {
                max_dim: 0,
                // Nothing was resized, so there is nothing for output
                // sharpening to restore. The window has no control for it yet,
                // nor for the file format; when it grows them, they belong
                // beside the size, not beside the edit.
                sharpening: rawkit_deliver::OutputSharpening::None,
                overwrite,
                jobs: EXPORT_JOBS,
                ..Default::default()
            },
            |done, total, filename| {
                *EXPORTING.lock().expect("export lock") = Some(Exporting {
                    done,
                    total,
                    filename: filename.to_string(),
                    finished: None,
                });
            },
        );
        let finished = match outcome {
            Ok(report) => {
                // No path in the readout. It is the longest part of the line
                // and the least useful — the user chose it a moment ago — and
                // on a narrow bar it pushed the key hints into a column ten
                // lines deep. It goes to the log, where it can be as long as
                // it likes.
                eprintln!("export     : into {where_to}");
                let mut line = format!(
                    "exported {} file(s), {:.1} MB in {:.1}s",
                    report.written,
                    report.bytes as f64 / 1_000_000.0,
                    started.elapsed().as_secs_f64()
                );
                if report.skipped > 0 {
                    line += &format!(" · {} already there", report.skipped);
                }
                if !report.failed.is_empty() {
                    line += &format!(" · {} failed", report.failed.len());
                    for (name, why) in &report.failed {
                        eprintln!("export     : {name}: {why}");
                    }
                }
                line
            }
            Err(e) => {
                eprintln!("export     : {e:#}");
                format!("export failed: {e}")
            }
        };
        eprintln!("export     : {finished}");
        let mut state = EXPORTING.lock().expect("export lock");
        let done = state.as_ref().map_or(total, |e| e.done);
        *state = Some(Exporting {
            done,
            total,
            filename: String::new(),
            finished: Some(finished),
        });
    });
    Ok(())
}

/// The monitor's ICC profile, if the desktop is managing colour.
///
/// One signature per platform, per the rule in AGENTS.md. ColorSync on macOS
/// and ICM on Windows are the equivalents and are not written yet, so those
/// platforms say so and assume sRGB — which is what they did before, only now
/// out loud.
#[cfg(target_os = "linux")]
fn display_profile() -> Option<Vec<u8>> {
    match canvas::display_profile() {
        Ok(profile) => profile,
        Err(e) => {
            eprintln!("display    : could not read the monitor profile: {e}");
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn display_profile() -> Option<Vec<u8>> {
    eprintln!("display    : reading the monitor profile is implemented for X11 only so far");
    None
}

/// Route pointer input over the canvas into the session.
///
/// One signature for every platform, and a body per platform, rather than a
/// call the other two cannot see. Four CI failures in this file have had the
/// same shape: something used only inside a `cfg` block, so the platforms
/// without it saw dead code. A stub that says what is missing costs one
/// function and cannot rot.
#[cfg(target_os = "linux")]
fn attach_input(
    window: &tauri::Window,
    panel_width: i32,
    session: Arc<Mutex<Session>>,
) -> Result<()> {
    canvas::attach_input(window, panel_width, session)
}

#[cfg(not(target_os = "linux"))]
fn attach_input(
    _window: &tauri::Window,
    _panel_width: i32,
    _session: Arc<Mutex<Session>>,
) -> Result<()> {
    // Nothing to attach *to*: under a cutout the canvas has no window of its
    // own, so the pointer belongs to the webview and the page would have to
    // forward it. That is not written, which leaves the zoom and pan buttons in
    // the View panel as the way to move around — they go through the same
    // command bus and work everywhere.
    eprintln!(
        "input      : dragging on the canvas needs a native child window; use the View buttons"
    );
    Ok(())
}

/// Where the swapchain is, and where in it the photograph goes.
///
/// The same rectangle only when the canvas has a window of its own. Under a
/// transparent cutout the swapchain is the whole window and the chrome floats
/// over the right of it, so the photograph has to be told to stop where the
/// chrome starts — otherwise its right edge is behind the panel and not
/// visible.
#[derive(Debug, Clone, Copy)]
struct Layout {
    /// What the surface is configured at: always the window.
    surface: tauri::PhysicalSize<u32>,
    /// The photograph's rectangle within it, which starts at the origin — the
    /// chrome is to the right of it, so there is no offset to carry.
    canvas: tauri::PhysicalSize<u32>,
}

/// Write the window's geometry down, at most once a second unless it is the
/// last chance.
///
/// Rate-limited because a drag emits geometry events far faster than anyone
/// changes their mind, and the file is only ever read at startup — the writes in
/// between are all superseded. `force` is the close, where the current value is
/// the one that matters and there is no later write to supersede it.
fn remember_window(app: &tauri::AppHandle, window: &tauri::Window, force: bool) {
    use std::sync::atomic::Ordering;
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    if !force {
        let mut last = LAST.lock().expect("window state lock");
        let recent = last.is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(1));
        if recent {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    let panel = PANEL_NOW.load(Ordering::Relaxed) as f64;
    if let Some(state) = window_state::of(window, panel) {
        window_state::save(app, state);
    }
}

/// Put the window back where it was, in the order the two settings require.
///
/// Position before maximise: a maximised window's own geometry is the screen's,
/// so setting the position afterwards would move the *maximised* window and lose
/// the rectangle to come back to when it is restored.
fn place_window(window: &tauri::Window, saved: Option<window_state::Remembered>) -> Result<()> {
    let Some(saved) = saved else { return Ok(()) };
    window.set_position(tauri::LogicalPosition::new(saved.x, saved.y))?;
    if saved.maximised {
        window.maximize()?;
    }
    Ok(())
}

impl Layout {
    /// How a window of this size divides, for this arrangement.
    ///
    /// One function rather than the three it used to be, because a resize has to
    /// arrive at exactly the division the window was built with — and the way to
    /// guarantee that is for there to be only one description of it.
    ///
    /// The routes differ in what the surface covers, not in where the photograph
    /// goes. Under a cutout the surface is the whole window and the canvas is the
    /// part the chrome does not float over; with a native child the surface *is*
    /// the child, so the two are the same rectangle.
    fn for_window(route: Route, window: tauri::PhysicalSize<u32>, scale: f64) -> Self {
        // The panel yields to the window rather than the other way round, and
        // yields *here* rather than by writing a smaller number back: the width
        // someone chose is theirs, and a window that narrows and widens again
        // should give it back rather than having quietly forgotten it.
        let room = window.width as f64 / scale;
        let wanted = PANEL_NOW.load(std::sync::atomic::Ordering::Relaxed) as f64;
        let shown = wanted.min((room - PHOTO_MIN).max(PANEL_MIN));
        PANEL_SHOWN.store(shown as i32, std::sync::atomic::Ordering::Relaxed);
        let panel = (shown * scale) as u32;
        let canvas = tauri::PhysicalSize::new(
            window.width.saturating_sub(panel).max(1),
            window.height.max(1),
        );
        match route {
            Route::Cutout => Layout {
                surface: window,
                canvas,
            },
            // The probe route, which has never placed its chrome correctly; it
            // gets the whole window so that what it does draw is at least whole.
            Route::ChildWebview => Layout {
                surface: window,
                canvas: window,
            },
            Route::NativeChild => Layout {
                surface: canvas,
                canvas,
            },
        }
    }
}

/// Build the window for the chosen arrangement and put a surface on it.
#[allow(clippy::type_complexity)]
fn build_window(
    app: &tauri::App,
    route: Route,
    saved: Option<window_state::Remembered>,
) -> Result<(Gpu, wgpu::Surface<'static>, Layout, tauri::Window)> {
    let (width, height) = saved.map_or(WINDOW, |s| (s.width, s.height));
    match route {
        Route::Cutout => {
            // The real interface, told to leave the canvas area unpainted. The
            // probe page it used to load answered a different question and has
            // no controls on it.
            let builder =
                WebviewWindowBuilder::new(app, "main", WebviewUrl::App("panel.html?cutout".into()))
                    .title("rawkit")
                    .inner_size(width, height);
            // Transparent on every platform, macOS included. That needs
            // Tauri's `macos-private-api`, which bars *App Store* distribution
            // and nothing else — notarised direct download, which is how a tool
            // like this ships, is unaffected. The alternative was a child
            // NSView, several hundred lines of FFI nobody here can run.
            let builder = builder.transparent(true);
            let window = builder.build()?;
            place_window(&window.as_ref().window(), saved)?;
            let layout = Layout::for_window(route, window.inner_size()?, window.scale_factor()?);
            let (gpu, surface) = Gpu::with_surface(window.clone())?;
            let handle = window.as_ref().window();
            Ok((gpu, surface, layout, handle))
        }
        Route::ChildWebview => {
            let window = WindowBuilder::new(app, "main")
                .title("rawkit")
                .inner_size(width, height)
                .build()?;
            place_window(&window, saved)?;
            let panel = window.add_child(
                WebviewBuilder::new("panel", WebviewUrl::App("panel.html".into())),
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(PANEL_WIDTH, height),
            )?;
            panel.set_auto_resize(false)?;
            panel.set_position(LogicalPosition::new(0.0, 0.0))?;
            panel.set_size(LogicalSize::new(PANEL_WIDTH, height))?;
            eprintln!(
                "panel      : {:?} (ignored on Linux; see the module docs)",
                panel.bounds()?
            );
            let layout = Layout::for_window(route, window.inner_size()?, window.scale_factor()?);
            let (gpu, surface) = Gpu::with_surface(window.clone())?;
            // A probe route, kept for the record rather than shipped: its chrome
            // is a side strip and nothing has ever placed it correctly.
            Ok((gpu, surface, layout, window))
        }
        #[cfg(target_os = "linux")]
        Route::NativeChild => {
            let window =
                WebviewWindowBuilder::new(app, "main", WebviewUrl::App("panel.html".into()))
                    .title("rawkit")
                    .inner_size(width, height)
                    .build()?;
            place_window(&window.as_ref().window(), saved)?;
            // The layout first, because it is what settles `PANEL_SHOWN` — and
            // the canvas is placed from that, not from the width somebody asked
            // for. Attaching first would put the child window at the unclamped
            // width and configure a surface at the clamped one, which is two
            // rectangles for one canvas and looks like half a photograph.
            let layout = Layout::for_window(route, window.inner_size()?, window.scale_factor()?);
            let canvas = canvas::attach(&window.as_ref().window(), &PANEL_SHOWN)?;
            eprintln!("canvas     : X window {canvas:?}");
            let (gpu, surface) = Gpu::with_surface(canvas)?;
            let handle = window.as_ref().window();
            // The canvas has a window of its own, so the surface *is* the
            // photograph's rectangle and there is nothing to offset.
            Ok((gpu, surface, layout, handle))
        }
        #[cfg(not(target_os = "linux"))]
        Route::NativeChild => Err(anyhow!(
            "the native-child canvas is implemented for X11 only so far"
        )),
    }
}

/// Which view the window is showing.
///
/// Shared as an atomic rather than held in the render loop, because the page
/// changes it — a keypress arrives on the IPC thread and the next frame has to
/// act on it. Two values, so a byte is enough and no lock is needed.
static MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
/// How many cells the grid currently fits across, written by the render loop and
/// read by the page's up/down keys.
///
/// The layout belongs to whoever measures the canvas, and that is not the page.
static GRID_COLUMNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
/// The cell edge in canvas pixels, changed by `-` and `=`.
static GRID_CELL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(400);

/// A click on the canvas the grid has not acted on yet, and whether it was a
/// double.
///
/// The canvas widget records where a click landed and nothing more; **which cell
/// that is** is worked out by the render loop, which is the only place that knows
/// the layout. Publishing the column count and pitch instead would put the same
/// arithmetic in two files and let them disagree.
/// Which HSL control a drag on the photograph is currently aiming, if any.
///
/// 0 is off; otherwise it is a [`BandControl`] by index. An atomic rather than a
/// mode, because targeting is *orthogonal* to loupe and crop — it is a thing the
/// pointer means while a control is armed, and the view underneath does not
/// change.
static TARGET: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn targeting() -> Option<rawkit_editstate::BandControl> {
    match TARGET.load(std::sync::atomic::Ordering::Relaxed) {
        1 => Some(rawkit_editstate::BandControl::Hue),
        2 => Some(rawkit_editstate::BandControl::Saturation),
        3 => Some(rawkit_editstate::BandControl::Luminance),
        _ => None,
    }
}

/// A drag that is adjusting the colour it was started on.
///
/// The press records where; the render loop, which is the only thing holding the
/// canvas, turns that into *which colour* and therefore which two bands. So the
/// gesture begins un-aimed and acquires its aim a frame later, which is
/// invisible at sixty frames a second and is why `picked` is an `Option` rather
/// than the press doing the sampling itself.
/// The two bands a sampled colour lies between with their weights, and what
/// those bands read when the hand went down.
pub(crate) type Aimed = ([(rawkit_editstate::Band, f32); 2], [f32; 2]);

pub(crate) struct Aim {
    pub at: [f64; 2],
    /// The two bands the sampled colour lies between with their weights, and
    /// what those bands read before the drag started. Both are needed: the
    /// deltas are relative to where the sliders were when the hand went down,
    /// not to where they are now, or a slow drag would compound.
    pub picked: Option<Aimed>,
}

pub(crate) static TARGET_AIM: Mutex<Option<Aim>> = Mutex::new(None);

/// Canvas pixels of vertical drag for the full range of a control.
///
/// In canvas pixels rather than logical ones because that is what the pointer
/// delivers — which does mean the gesture is twice as fine on a HiDPI screen,
/// and that is the right way round: a display with more pixels can aim better.
pub(crate) const TARGET_RANGE_PX: f64 = 400.0;

/// The square averaged when picking a colour, in canvas pixels.
///
/// Small enough to be *this* leaf rather than the hedge, large enough that the
/// answer does not change when the hand moves by one — and a demosaiced pixel is
/// partly its neighbours anyway, so a single one would be a false precision.
pub(crate) const TARGET_SAMPLE: u32 = 9;

/// The hue of a colour, in degrees, or `None` for a grey.
///
/// The same measure the shader takes: `mix_bands` reads `rgb_to_hsv` on
/// display-referred values, which is what the canvas holds, so a hue read here
/// names the band the renderer would have used.
///
/// One caveat, and it is inherent rather than an oversight: the canvas has
/// already been through the mixer, so a colour the user has *already* shifted is
/// sampled where it now is rather than where it came from. Shifts are capped at
/// thirty degrees and the bands are thirty to sixty apart, so this can only
/// mis-aim after a large existing adjustment to the very band being aimed at.
pub(crate) fn hue_of(rgb: [f32; 3]) -> Option<f32> {
    let high = rgb[0].max(rgb[1]).max(rgb[2]);
    let low = rgb[0].min(rgb[1]).min(rgb[2]);
    let chroma = high - low;
    if chroma <= 1e-4 || high <= 0.0 {
        return None;
    }
    let hue = if high == rgb[0] {
        60.0 * (((rgb[1] - rgb[2]) / chroma) % 6.0)
    } else if high == rgb[1] {
        60.0 * ((rgb[2] - rgb[0]) / chroma + 2.0)
    } else {
        60.0 * ((rgb[0] - rgb[1]) / chroma + 4.0)
    };
    Some(hue.rem_euclid(360.0))
}

/// Point at the photograph to move the sliders for the colour under the pointer.
///
/// The mixer's difficulty is not the arithmetic, it is aiming: a lawn is about
/// 69% Yellow and 31% Green, and nothing on screen says so. Armed, a vertical
/// drag distributes the change over exactly those two weights, so the colour
/// under the pointer receives all of it and the sliders show where it went.
#[tauri::command]
fn arm_target(control: Option<String>) -> Result<u8, String> {
    let armed = match control.as_deref() {
        None | Some("") => 0,
        Some("hue") => 1,
        Some("saturation") => 2,
        Some("luminance") => 3,
        Some(other) => {
            return Err(format!(
                "{other:?} is not one of the mixer's three controls"
            ))
        }
    };
    TARGET.store(armed, std::sync::atomic::Ordering::Relaxed);
    if armed == 0 {
        *TARGET_AIM.lock().expect("aim lock") = None;
    }
    Ok(armed)
}

/// Whether the next click on the photograph sets the white balance from what it
/// lands on.
static PICKING_WB: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn picking_wb() -> bool {
    PICKING_WB.load(std::sync::atomic::Ordering::Relaxed)
}

/// A click awaiting the render loop, which is the only place that can turn a
/// canvas position into a rectangle of sensor.
pub(crate) static WB_PICK: Mutex<Option<[f64; 2]>> = Mutex::new(None);

/// Something the shell needs to say, for the page to show once.
///
/// The status line is fed by the events commands return, and a refusal decided
/// in the render loop has no command to return through. Taken rather than read,
/// so a message appears once and does not sit there describing a click from two
/// photographs ago.
pub(crate) static NOTICE: Mutex<Option<String>> = Mutex::new(None);

/// The square a pick averages, in canvas pixels.
///
/// Larger than the mixer's nine, because this one is asking about *noise*: a
/// white balance derived from a handful of photosites moves with the grain, and
/// a patch a user thinks of as "that grey card" is far bigger than a pixel.
pub(crate) const WB_SAMPLE: f64 = 21.0;

/// The temperature and tint that would render this camera-space patch neutral,
/// or why it cannot mean anything.
///
/// Refusals rather than a clamped answer, because every one of these is a
/// plausible-looking number with no relationship to the light: the commonest way
/// to misuse an eyedropper is to click the brightest thing in the frame, and a
/// blown highlight's ratios are the sensor's ceiling rather than the scene's
/// colour.
pub(crate) fn neutralising(
    camera: [f32; 3],
    profile: &rawkit_engine::CameraProfile,
) -> Result<(f32, f32), String> {
    let high = camera[0].max(camera[1]).max(camera[2]);
    let low = camera[0].min(camera[1]).min(camera[2]);
    // `normalise` puts the sensor's white level at 1.0.
    if high >= 0.98 {
        return Err(
            "that patch is blown — its channels are at the sensor's limit, \
                    not the light's colour"
                .into(),
        );
    }
    if high < 0.02 || low <= 0.0 {
        return Err("that patch is too dark to say anything about the light".into());
    }

    // Green-referenced, like every other multiplier in this project: the numbers
    // that would make the three channels equal.
    let multipliers = [camera[1] / camera[0], 1.0, camera[1] / camera[2]];
    let (kelvin, tint) = profile.temperature_from_multipliers(multipliers);
    // The inverse clamps to the locus it can describe, so landing on an end is
    // not a temperature — it is the answer running out.
    if kelvin <= rawkit_engine::profile::MIN_TEMPERATURE + 1.0
        || kelvin >= rawkit_engine::profile::MAX_TEMPERATURE - 1.0
    {
        return Err(format!(
            "no daylight-locus temperature makes that neutral ({kelvin:.0} K is the end of the scale)"
        ));
    }
    Ok((kelvin, tint))
}

pub(crate) fn notice(what: impl Into<String>) {
    *NOTICE.lock().expect("notice lock") = Some(what.into());
}

/// Two canvas positions to the shape they describe, in the kind already there.
///
/// The result is in fractions of the **sensor**, which is the frame that does
/// not move when the picture is turned or trimmed — so a mask drawn across the
/// sky stays across the sky afterwards, rather than sliding as soon as somebody
/// adjusts the crop. Getting that wrong is invisible while the frame is upright
/// and uncropped, which is most of the time and none of the interesting cases.
///
/// The existing shape decides what the drag *means*: a gradient runs from where
/// the hand went down to where it let go, and an ellipse grows out of it. One
/// gesture, read two ways, rather than two modes to be in.
fn shape_from_drag(
    existing: rawkit_editstate::MaskShape,
    from: [f64; 2],
    to: [f64; 2],
    viewport: rawkit_session::Viewport,
    geometry: &rawkit_editstate::Geometry,
    size: [u32; 2],
) -> rawkit_editstate::MaskShape {
    let sensor = |at: [f64; 2]| {
        let p = viewport.image_at(at);
        // A degenerate rectangle, because `sensor_rect` is the one conversion
        // that already knows about rotation, straightening and the crop window,
        // and a second one written for points would be a second one to keep
        // right.
        let r = geometry.sensor_rect([p[0], p[1], p[0], p[1]], size);
        [
            (r[0] / size[0] as f64) as f32,
            (r[1] / size[1] as f64) as f32,
        ]
    };
    let (a, b) = (sensor(from), sensor(to));
    match existing {
        rawkit_editstate::MaskShape::Linear { .. } => {
            rawkit_editstate::MaskShape::Linear { from: a, to: b }
        }
        // Out from the centre, and the two radii follow the drag's own width and
        // height — so one drag gives any aspect of ellipse, and a circle drawn
        // on the photograph is stored as the two different fractions that make
        // it come back a circle.
        //
        // A floor on each radius rather than a refusal: a press that barely
        // moves is a slip, and leaving an ellipse a few pixels across is kinder
        // than leaving the previous one and looking broken.
        rawkit_editstate::MaskShape::Radial { feather, .. } => {
            rawkit_editstate::MaskShape::Radial {
                centre: a,
                radii: [(b[0] - a[0]).abs().max(1e-3), (b[1] - a[1]).abs().max(1e-3)],
                feather,
            }
        }
    }
}

/// Which local adjustment the panel is showing, and whether the next drag on
/// the photograph places it.
///
/// Two atomics rather than one selection object, because they are read from the
/// pointer routing on whichever thread delivered the event and written from a
/// command on another. `usize::MAX` is "none selected".
static SELECTED_MASK: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(usize::MAX);
static PLACING_MASK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn placing_mask() -> Option<usize> {
    if !PLACING_MASK.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    match SELECTED_MASK.load(std::sync::atomic::Ordering::Relaxed) {
        usize::MAX => None,
        index => Some(index),
    }
}

/// Where the placing drag started, in canvas pixels.
///
/// The gradient's near end. Held here rather than in the pointer module because
/// turning it into a place on the *sensor* needs the viewport and the geometry,
/// which only the render loop has — the same arrangement the white-balance pick
/// uses, and for the same reason.
pub(crate) static MASK_DRAG: Mutex<Option<([f64; 2], [f64; 2])>> = Mutex::new(None);

/// Add a local adjustment, select it, and arm the next drag to place it.
///
/// It arrives darkening by a stop rather than doing nothing. A gradient that
/// changes nothing is invisible, and an invisible thing that has to be dragged
/// into position is a thing nobody can place — so the first press already shows
/// where it is, and the sliders take it from there.
#[tauri::command]
fn add_mask(kind: Option<String>, state: tauri::State<'_, Shared>) -> Result<usize, String> {
    let shape = match kind.as_deref() {
        None | Some("") | Some("gradient") => rawkit_editstate::Mask::default().shape,
        // Middle of the frame, a third across. Visible, and clear of the edges
        // where a drag to reposition it would be awkward.
        Some("radial") => rawkit_editstate::MaskShape::Radial {
            centre: [0.5, 0.5],
            radii: [0.2, 0.3],
            feather: 0.5,
        },
        Some(other) => return Err(format!("{other:?} is not a kind of local adjustment")),
    };
    let mut session = state.0.lock().expect("session lock");
    let mut masks = session.state().masks.clone();
    if masks.len() >= rawkit_editstate::MAX_MASKS {
        return Err(format!(
            "{} local adjustments is the most a photograph may carry",
            rawkit_editstate::MAX_MASKS
        ));
    }
    masks.push(rawkit_editstate::Mask {
        shape,
        exposure_ev: -1.0,
        ..rawkit_editstate::Mask::default()
    });
    let index = masks.len() - 1;
    session.apply(Command::SetMasks {
        masks,
        control: u8::MAX,
    });
    drop(session);
    SELECTED_MASK.store(index, std::sync::atomic::Ordering::Relaxed);
    PLACING_MASK.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(index)
}

/// Throw one away.
#[tauri::command]
fn remove_mask(index: usize, state: tauri::State<'_, Shared>) -> Result<usize, String> {
    let mut session = state.0.lock().expect("session lock");
    let mut masks = session.state().masks.clone();
    if index >= masks.len() {
        return Err("there is no such local adjustment".into());
    }
    masks.remove(index);
    let left = masks.len();
    session.apply(Command::SetMasks {
        masks,
        control: u8::MAX,
    });
    drop(session);
    // Selecting the one before it rather than nothing: removing the third of
    // four and being left with no selection means finding the list again.
    SELECTED_MASK.store(
        if left == 0 {
            usize::MAX
        } else {
            index.min(left - 1)
        },
        std::sync::atomic::Ordering::Relaxed,
    );
    PLACING_MASK.store(false, std::sync::atomic::Ordering::Relaxed);
    Ok(left)
}

/// Which one the panel is showing.
#[tauri::command]
fn select_mask(index: Option<usize>) -> usize {
    let chosen = index.unwrap_or(usize::MAX);
    SELECTED_MASK.store(chosen, std::sync::atomic::Ordering::Relaxed);
    if index.is_none() {
        PLACING_MASK.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    chosen
}

/// Whether the next drag on the photograph redraws the selected gradient.
#[tauri::command]
fn place_mask(armed: bool) -> bool {
    PLACING_MASK.store(armed, std::sync::atomic::Ordering::Relaxed);
    if !armed {
        *MASK_DRAG.lock().expect("mask drag lock") = None;
    }
    armed
}

/// Move one control of one local adjustment.
#[tauri::command]
fn set_mask(
    index: usize,
    control: String,
    value: f32,
    state: tauri::State<'_, Shared>,
) -> Result<(), String> {
    let mut session = state.0.lock().expect("session lock");
    let mut masks = session.state().masks.clone();
    let mask = masks
        .get_mut(index)
        .ok_or_else(|| "there is no such local adjustment".to_string())?;
    let slot = match control.as_str() {
        "exposure" => {
            mask.exposure_ev = value;
            1
        }
        "warmth" => {
            mask.warmth = value;
            2
        }
        "tint" => {
            mask.tint = value;
            3
        }
        "feather" => match &mut mask.shape {
            rawkit_editstate::MaskShape::Radial { feather, .. } => {
                *feather = value;
                4
            }
            // A gradient's feather is the distance between its two ends, so
            // there is no separate number to move — said rather than ignored.
            _ => return Err("a gradient is feathered by where you drew it".into()),
        },
        // Not coalesced with anything: a tick is a discrete act, and folding it
        // into whichever slider moved last would make one undo take both.
        "invert" => {
            mask.invert = value != 0.0;
            u8::MAX
        }
        other => return Err(format!("{other:?} is not one of a mask's controls")),
    };
    session.apply(Command::SetMasks {
        masks,
        // Which control of which mask, so dragging exposure on one and then on
        // another opens two undo steps rather than folding into one.
        control: if slot == u8::MAX {
            u8::MAX
        } else {
            (index as u8) * 8 + slot
        },
    });
    Ok(())
}

/// Set the white balance from a patch that ought to be neutral.
#[tauri::command]
fn pick_white_balance(armed: bool) -> bool {
    PICKING_WB.store(armed, std::sync::atomic::Ordering::Relaxed);
    if !armed {
        *WB_PICK.lock().expect("pick lock") = None;
    }
    armed
}

pub(crate) static CANVAS_CLICK: Mutex<Option<([f64; 2], bool)>> = Mutex::new(None);
/// Wheel notches since the last frame, positive downwards.
pub(crate) static CANVAS_SCROLL: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(0);

/// What a colour label looks like, in linear light.
///
/// The names are Lightroom's, because the keys are: 6 to 9 do there what they do
/// here, and a photographer's hands already know them.
fn label_colour(name: &str) -> Option<[f32; 3]> {
    Some(match name {
        "red" => [0.52, 0.06, 0.06],
        "yellow" => [0.62, 0.48, 0.04],
        "green" => [0.09, 0.42, 0.13],
        "blue" => [0.06, 0.22, 0.55],
        _ => return None,
    })
}

const MODE_LOUPE: u8 = 0;
const MODE_GRID: u8 = 1;
/// A handful of frames side by side, to choose between.
const MODE_SURVEY: u8 = 2;
/// The loupe, with a rectangle being drawn on it.
///
/// A mode rather than a flag because it changes what the same keys mean: Enter
/// commits the rectangle instead of returning to the loupe, and Escape discards
/// it instead of doing nothing.
const MODE_CROP: u8 = 3;

fn mode() -> u8 {
    MODE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether the canvas is currently showing a grid.
///
/// Named rather than a comparison at each call site because the canvas widget
/// asks it, and that file is the one where platform-specific code is allowed to
/// live — the less it knows about the rest of the shell, the better.
/// Whether the canvas is laying out cells rather than showing one photograph.
/// Both the grid and a survey are; the canvas widget only needs to know that
/// clicks pick a cell rather than pan an image.
pub(crate) fn in_grid() -> bool {
    matches!(mode(), MODE_GRID | MODE_SURVEY)
}

/// Whether a drag on the canvas draws a rectangle rather than panning.
pub(crate) fn in_crop() -> bool {
    mode() == MODE_CROP
}

/// The rectangle being drawn on the canvas, in surface pixels.
///
/// There is no "finished" flag: letting go of the button stops the motion
/// handler updating it, which is the same fact expressed once instead of twice.
/// The version that carried one was dead code on the two platforms whose canvas
/// does not exist yet.
///
/// Surface pixels because that is what the pointer produces and what the outline
/// is drawn in; turning it into a crop needs the viewport, which lives in the
/// render loop. Same division of labour as the grid's click: this widget reports
/// where, and the loop works out what.
/// How often the histogram is allowed to be recomputed.
///
/// It is not free and the reason is measured: developing the coarsest pyramid
/// level takes about the same as a canvas tile, but unlike a canvas tile it has
/// to be *read back*, which drains the queue and stalls the pipeline. On this
/// machine that put a survey at 15–40 ms against a frame budget of 33.
///
/// So it runs at most ten times a second rather than once a frame. Ten is above
/// the rate at which a changing readout stops looking like steps and starts
/// looking like movement, so a slider drag still shows a histogram that follows
/// the slider; what it no longer does is stall every frame to say something
/// nobody could read that fast. A fast fling lags by up to this long and then
/// catches up, because the "is it stale" test stays true until a survey
/// actually happens.
const SURVEY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// The frame time above which an edit in flight renders a level coarser.
///
/// Twenty frames a second, measured on the tile pass alone — the only part a
/// coarser level makes cheaper.
///
/// The threshold is a judgement; what is measured is whether a given window on a
/// given machine crosses it. On this one a full-resolution pass takes **38 ms**
/// windowed and **138 ms** at full screen, so a windowed drag stays sharp and a
/// full-screen one does not — which is the intent, since softening a drag that
/// was already tracking trades sharpness for speed nobody asked for. On faster
/// hardware neither would engage, and that is the point of deciding it here
/// rather than once, in a constant, for every machine.
const FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(50);

/// How long after the last change an edit counts as finished.
///
/// Short enough that the sharp pass feels like part of the same gesture, long
/// enough that the gap between two slider positions does not read as the end of
/// one drag and the start of another — which would sharpen and re-soften
/// repeatedly through a single movement.
const EDIT_SETTLES: std::time::Duration = std::time::Duration::from_millis(150);

/// The body of the photograph on screen, so the picker knows which camera it is
/// choosing a profile for. `None` before anything is open, and for the
/// synthetic mosaic, which no profile describes.
static CURRENT_CAMERA: Mutex<Option<(String, String)>> = Mutex::new(None);
/// What that camera is currently rendered with, for the page to show. `None`
/// means the decoder's own matrix.
static PROFILE_NAME: Mutex<Option<String>> = Mutex::new(None);
/// Set when the choice changes, so the render loop reloads with it. The loop
/// owns the GPU buffers and the profile decides how big one of them is.
static PROFILE_CHANGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Render this photograph with whatever profile its camera has been given.
///
/// Returns what to call it, or `None` for the decoder's own matrix — which is
/// also what a profile that has moved falls back to, loudly.
fn apply_profile(library: Option<&Arc<Mutex<Library>>>, loaded: &mut Loaded) -> Option<String> {
    let camera = loaded.camera().cloned()?;
    *CURRENT_CAMERA.lock().expect("camera lock") =
        Some((camera.make.clone(), camera.model.clone()));

    let chosen = rawkit_catalog::profiles::chosen(
        library?.lock().expect("library lock").catalog(),
        &camera.make,
        &camera.model,
    )
    .ok()??;

    match std::fs::read(&chosen.path)
        .ok()
        .and_then(|bytes| rawkit_engine::profile::dcp::parse(&bytes).ok())
    {
        Some(profile) => {
            loaded.set_profile(profile);
            Some(chosen.name.unwrap_or(chosen.path))
        }
        // Named rather than silently ignored. A profile that has moved should
        // say so; the alternative is a photograph quietly changing colour
        // between one session and the next.
        None => {
            eprintln!(
                "profile    : {} is missing or unreadable; rendering with the decoder's \
                 own matrix",
                chosen.path
            );
            None
        }
    }
}

/// An export the user has chosen a destination for, waiting for the render loop
/// to pick it up.
///
/// The dialog runs on a Tauri thread and the catalog is read on the render
/// loop's, and that split is not incidental: the loop owns the `Saver`, and the
/// edit you were making a second ago is still sitting in its settle timer. An
/// export that read the catalog without flushing first would deliver the
/// photograph as it was *before* the last thing you did to it — which looks
/// like the export ignoring your edit, and is really a race with a debounce.
static PENDING_EXPORT: Mutex<Option<(rawkit_deliver::Selection, rawkit_deliver::Destination)>> =
    Mutex::new(None);

/// How far along an export is, for the page to show. `None` between exports.
static EXPORTING: Mutex<Option<Exporting>> = Mutex::new(None);

struct Exporting {
    done: usize,
    total: usize,
    filename: String,
    /// Set once, when there is nothing left to do. Kept rather than cleared so
    /// the result stays on screen — an export that finished by the readout
    /// simply vanishing tells you nothing about whether it worked.
    finished: Option<String>,
}

/// How many photographs to render at once from the window.
///
/// The command line uses four, measured: 595 ms a photograph at one job, 218 at
/// four. This is deliberately lower, and the trade is stated rather than
/// measured — the window is still drawing while this runs, and an export that
/// finishes a third sooner while the canvas stutters is the wrong way round for
/// something you started and are now watching.
const EXPORT_JOBS: usize = 2;

/// The distribution of the photograph the render loop last developed.
///
/// A static for the same reason the marquee is one: the page asks for it over
/// IPC, on a thread that has no GPU and must not wait for one. The loop leaves
/// the answer here and the command reads whatever is there — which may be one
/// edit behind, and that is the right failure. A histogram that blocked a
/// keypress until a render finished would be worse than one that lags it.
static SCOPE: Mutex<Option<Survey>> = Mutex::new(None);

/// A histogram and the edit it describes, kept together so a stale one cannot
/// be mistaken for a fresh one.
struct Survey {
    generation: u64,
    histogram: rawkit_export::histogram::Histogram,
}

pub(crate) static CANVAS_MARQUEE: Mutex<Option<Marquee>> = Mutex::new(None);
/// Set when the page asks for the rectangle to be taken.
static CROP_COMMIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Debug, Clone, Copy)]
pub(crate) struct Marquee {
    pub start: [f64; 2],
    pub end: [f64; 2],
}

impl Marquee {
    /// `[x, y, w, h]`, however the drag was made — up-left is a rectangle too.
    pub(crate) fn rect(&self) -> [f64; 4] {
        let x = self.start[0].min(self.end[0]);
        let y = self.start[1].min(self.end[1]);
        [
            x,
            y,
            (self.start[0] - self.end[0]).abs(),
            (self.start[1] - self.end[1]).abs(),
        ]
    }
}

pub(crate) fn mode_name() -> &'static str {
    match mode() {
        MODE_CROP => "crop",
        MODE_GRID => "grid",
        MODE_SURVEY => "survey",
        _ => "loupe",
    }
}

/// The grid's own state: what is on the GPU, and where the view is.
struct Grid {
    /// Uploaded thumbnails, keyed by position in the sequence. Not an LRU by
    /// time but by distance from the view, which for a grid is the same thing
    /// and needs no bookkeeping.
    cells: std::collections::HashMap<usize, rawkit_engine::PreviewImage>,
    /// Vertical offset in canvas pixels.
    scroll: f64,
}

/// How many thumbnails to load in one frame.
///
/// The profile found the only over-budget frame was the first, filling an empty
/// grid: at large cells that is thirty fetches at about two milliseconds each.
/// Spreading them over frames turns a sixty-millisecond stall into cells that
/// appear over the next half second, which is what a placeholder is for.
const LOADS_PER_FRAME: usize = 4;

/// What the canvas can currently draw for the photograph on screen.
///
/// A cached preview if there is one with enough pixels, and the decoded RAW only
/// once something actually needs more detail than the preview has. During a cull
/// that second thing never happens, which is the whole point: moving to the next
/// frame costs a file read and a texture upload rather than a decode and a
/// pyramid.
struct Showing {
    /// The RAW, for when it does have to be decoded. `None` only for the
    /// synthetic mosaic the shell falls back to with no file at all.
    path: Option<PathBuf>,
    /// The photograph's full resolution — from a header parse, not a decode. The
    /// viewport is expressed in these coordinates whichever source is drawing.
    size: [u32; 2],
    /// And which way up the camera says it goes, from the same header parse.
    /// Held beside the size because the two decide the viewport together: the
    /// sensor's dimensions alone are the wrong shape for every portrait frame.
    orientation: rawkit_editstate::Orientation,
    preview: Option<rawkit_engine::PreviewImage>,
    raw: Option<Loaded>,
}

/// How many pixels along its longest edge the photograph currently occupies on
/// screen.
///
/// The bar a preview has to clear: at least one preview pixel per screen pixel.
/// Below that it would be upscaled, which is exactly what makes a preview look
/// like a preview. At 1:1 nothing on disk clears it and the renderer takes over,
/// which is the right answer — a sharpness check is not a job for a JPEG.
fn needed_pixels(session: &Session, image: [u32; 2]) -> u32 {
    let longest = image[0].max(image[1]) as f64;
    (longest * session.viewport().scale).ceil().max(1.0) as u32
}

/// Lay the sequence out, load what is newly visible, and draw it.
///
/// Everything here is in canvas pixels, and the canvas is the surface, so a cell
/// asked for at 400 is 400. Cells keep the photograph's aspect inside a square
/// slot, which is what makes a mixed-orientation shoot line up in rows.
fn draw_grid(
    gpu: &Gpu,
    blit: &rawkit_engine::PreviewBlit,
    canvas_renderer: &mut session_canvas::CanvasRenderer,
    library: &Mutex<Library>,
    grid: &mut Grid,
    surface: [u32; 2],
) -> Result<usize> {
    canvas_renderer.fit_surface(gpu, surface);
    // The requested cell size picks the column count; the columns then set the
    // actual size, so a row always fills the width. Laying out at the requested
    // size instead leaves a ragged strip down the right — which reads as a
    // margin nobody asked for, and wastes an eighth of the window.
    // A survey shows only what was set aside, and sizes its cells to fill the
    // canvas rather than to a chosen density — the whole point is to see a few
    // frames as large as they will go.
    let survey = mode() == MODE_SURVEY;
    let chosen: Vec<usize> = library.lock().expect("library lock").marked().to_vec();
    if survey && chosen.is_empty() {
        // Nothing left to compare, which is what winnowing down to one and then
        // judging it looks like. Back to the grid rather than an empty canvas.
        MODE.store(MODE_GRID, std::sync::atomic::Ordering::Relaxed);
    }
    let survey = survey && !chosen.is_empty();

    let asked = if survey {
        best_survey_cell(chosen.len(), surface)
    } else {
        GRID_CELL.load(std::sync::atomic::Ordering::Relaxed).max(80) as f64
    };
    let columns = ((surface[0] as f64 / asked).round() as usize).max(1);
    GRID_COLUMNS.store(columns, std::sync::atomic::Ordering::Relaxed);
    let pitch_x = surface[0] as f64 / columns as f64;
    let gap = (pitch_x * 0.05).round().max(4.0);
    let cell = pitch_x - gap;
    // Slots are 3:2 rather than square. A square slot letterboxes every
    // landscape frame top and bottom, and the empty band it leaves reads as an
    // enormous gap between rows — the layout looking broken because of the shape
    // of the photographs in it. A portrait frame fits by its height instead and
    // simply comes out narrow, which is what it is.
    let slot_h = (cell / 1.5).round();
    let pitch_y = slot_h + gap;

    let (total, mut selected) = {
        let library = library.lock().expect("library lock");
        (library.count(), library.index())
    };
    // What the layout is over: every photograph, or only the comparison.
    let shown: Vec<usize> = if survey { chosen } else { (0..total).collect() };
    let count = shown.len();
    let rows = count.div_ceil(columns);

    // A survey never fills the canvas exactly — three 3:2 frames across a wide
    // window are one row of a third its height — so the block is centred. Left
    // at the top it reads as a layout that ran out rather than one that chose.
    let centring = if survey {
        ((surface[1] as f64 - rows as f64 * pitch_y) / 2.0).max(0.0)
    } else {
        0.0
    };

    // A survey never scrolls: everything in it is on screen by construction.
    // A wheel notch moves half a row, which is small enough to feel like
    // scrolling and large enough to get somewhere.
    let notches = CANVAS_SCROLL.swap(0, std::sync::atomic::Ordering::Relaxed);
    if notches != 0 {
        grid.scroll += notches as f64 * pitch_y * 0.5;
    }

    // A click picks the cell under it; a double-click opens that cell.
    if let Some(([x, y], double)) = CANVAS_CLICK.lock().expect("click lock").take() {
        let column = (x / pitch_x).floor() as i64;
        let row = ((y + grid.scroll - centring) / pitch_y).floor() as i64;
        if (0..columns as i64).contains(&column) && row >= 0 {
            let slot = row as usize * columns + column as usize;
            if let Some(&index) = shown.get(slot) {
                selected = index;
                let mut library = library.lock().expect("library lock");
                library.select(index);
                if double {
                    library.reopen();
                    MODE.store(MODE_LOUPE, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }

    // Follow the selection rather than making the user chase it. Only ever by
    // the minimum needed, so a selection already on screen does not move the
    // view at all.
    let at = shown.iter().position(|i| *i == selected).unwrap_or(0);
    let selected_row = (at / columns) as f64;
    let top = selected_row * pitch_y;
    let bottom = top + pitch_y;
    if top < grid.scroll {
        grid.scroll = top;
    } else if bottom > grid.scroll + surface[1] as f64 {
        grid.scroll = bottom - surface[1] as f64;
    }
    let furthest = (rows as f64 * pitch_y - surface[1] as f64).max(0.0);
    grid.scroll = grid.scroll.clamp(0.0, furthest);

    let first_row = (grid.scroll / pitch_y).floor() as usize;
    let last_row = (((grid.scroll + surface[1] as f64) / pitch_y).ceil() as usize).min(rows);
    let (from, to) = (first_row * columns, (last_row * columns).min(count));

    // Load a bounded number per frame, nearest to the selection first, so what
    // you are looking at fills in before what you are not.
    let mut wanted: Vec<usize> = shown[from.min(count)..to.min(count)]
        .iter()
        .copied()
        .filter(|i| !grid.cells.contains_key(i))
        .collect();
    wanted.sort_by_key(|i| i.abs_diff(selected));
    let needed = cell.round() as u32;
    for index in wanted.into_iter().take(LOADS_PER_FRAME) {
        let decoded = library
            .lock()
            .expect("library lock")
            .preview_at(index, needed, None)?;
        if let Some(decoded) = decoded {
            let image = blit.upload(gpu, &decoded.rgba, decoded.width, decoded.height)?;
            grid.cells.insert(index, image);
        }
    }
    // Anything far outside the view is not coming back soon.
    let keep: std::collections::HashSet<usize> = shown
        [from.saturating_sub(columns * 4).min(count)..(to + columns * 4).min(count)]
        .iter()
        .copied()
        .collect();
    grid.cells.retain(|index, _| keep.contains(index));

    let mut cells = Vec::new();
    for (slot, &index) in shown
        .iter()
        .enumerate()
        .take(to.min(count))
        .skip(from.min(count))
    {
        let Some(image) = grid.cells.get(&index) else {
            continue;
        };
        let row = slot / columns;
        let column = slot % columns;
        // Fit the photograph inside the slot, keeping its shape.
        let (iw, ih) = (image.width as f64, image.height as f64);
        let scale = (cell / iw).min(slot_h / ih);
        let (w, h) = (iw * scale, ih * scale);
        let slot_x = gap / 2.0 + column as f64 * pitch_x;
        let slot_y = centring + gap / 2.0 + row as f64 * pitch_y - grid.scroll;

        let (flag, label) = library
            .lock()
            .expect("library lock")
            .flags_in(index, index + 1)?
            .pop()
            .unwrap_or((None, None));
        let tint = match flag {
            // A third the brightness. The shape of a cull becomes visible
            // without reading anything, which is the whole point of a grid.
            Some(rawkit_catalog::cull::Flag::Reject) => [0.33, 0.33, 0.33],
            _ => [1.0, 1.0, 1.0],
        };
        let edge = if index == selected {
            ([0.95, 0.95, 0.98], 3.0)
        } else {
            match flag {
                // Cyan, and deliberately not green: green is one of the four
                // colour labels, so a green edge made a picked frame and a
                // green-labelled one look identical. Selection is white, labels
                // are red/yellow/green/blue, and a flag needs a hue of its own.
                Some(rawkit_catalog::cull::Flag::Pick) => ([0.16, 0.58, 0.64], 2.0),
                _ => ([0.0; 3], 0.0),
            }
        };

        let inner = label
            .as_deref()
            .and_then(label_colour)
            .map(|colour| (colour, 3.0))
            .unwrap_or(([0.0; 3], 0.0));

        cells.push(rawkit_engine::Cell {
            image,
            dest: [
                (slot_x + (cell - w) / 2.0).round() as i32,
                (slot_y + (slot_h - h) / 2.0).round() as i32,
                w.round() as i32,
                h.round() as i32,
            ],
            tint,
            edge,
            inner,
        });
    }

    let drawn = cells.len();
    blit.draw_over(gpu, canvas_renderer.canvas(), &cells);
    Ok(drawn)
}

/// The cell size that lets `count` frames fill the canvas.
///
/// Tries every column count and keeps the one that makes the cells largest. A
/// survey of three is a row of three; a survey of five is three above two,
/// because that is bigger than one row of five on a wide canvas.
fn best_survey_cell(count: usize, surface: [u32; 2]) -> f64 {
    let (width, height) = (surface[0] as f64, surface[1] as f64);
    let mut best = 80.0f64;
    for columns in 1..=count {
        let rows = count.div_ceil(columns) as f64;
        let cell_w = width / columns as f64;
        // Slots are 3:2, so the height a row needs is two thirds of the width.
        let cell_h = (height / rows) * 1.5;
        best = best.max(cell_w.min(cell_h));
    }
    best
}

/// Whether a path names a catalog rather than a photograph.
fn is_catalog(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("rawkit"))
}

/// What frames are costing, reported only while something is happening.
///
/// An idle window redraws nothing and would otherwise fill the log with zeros.
struct FrameStats {
    frames: u32,
    tiles: usize,
    busy: std::time::Duration,
    slowest: std::time::Duration,
    since: Option<std::time::Instant>,
}

impl Default for FrameStats {
    fn default() -> Self {
        Self {
            frames: 0,
            tiles: 0,
            busy: std::time::Duration::ZERO,
            slowest: std::time::Duration::ZERO,
            since: None,
        }
    }
}

impl FrameStats {
    fn record(&mut self, tiles: usize, elapsed: std::time::Duration) {
        if tiles == 0 {
            return;
        }
        let since = *self.since.get_or_insert_with(std::time::Instant::now);
        self.frames += 1;
        self.tiles += tiles;
        self.busy += elapsed;
        self.slowest = self.slowest.max(elapsed);
        if since.elapsed() >= std::time::Duration::from_secs(1) {
            eprintln!(
                "frames     : {} drawing, {} tiles, mean {:.1} ms, worst {:.1} ms",
                self.frames,
                self.tiles,
                self.busy.as_secs_f64() * 1000.0 / self.frames as f64,
                self.slowest.as_secs_f64() * 1000.0,
            );
            *self = Self::default();
        }
    }
}

/// Blit the canvas to the surface and present. Repeated at ~30Hz rather than
/// drawn once: flicker was the failure mode two of the three arrangements had,
/// and a single frame cannot show it.
fn paint(
    gpu: &Gpu,
    surface: &wgpu::Surface<'static>,
    presenter: &Presenter,
    canvas: &rawkit_engine::Canvas,
    at: [u32; 4],
) -> Result<()> {
    use wgpu::CurrentSurfaceTexture as Current;
    let frame = match surface.get_current_texture() {
        Current::Success(f) | Current::Suboptimal(f) => f,
        // A resize invalidates the swapchain. The probe does not resize, but
        // returning quietly beats a spurious error in the log that would
        // muddy the result it exists to report.
        Current::Outdated | Current::Timeout | Current::Occluded => return Ok(()),
        other => return Err(anyhow!("surface unusable: {other:?}")),
    };
    let view = frame
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    if std::env::var_os("RAWKIT_PROBE_CLEAR").is_some() {
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("probe clear"),
            });
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("probe clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(PROBE_GREEN),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        gpu.queue.submit([encoder.finish()]);
    } else {
        presenter.draw_into(gpu, canvas, &view, at)?;
    }
    frame.present();
    Ok(())
}

/// A Bayer mosaic of a scene with structure at every scale.
///
/// For running the shell with no file to hand. Everything downstream of it is
/// the real pipeline, so what appears in the window is a genuine render either
/// way — only the sensor is imaginary.
pub(crate) fn test_mosaic(width: u32, height: u32) -> Vec<f32> {
    // Even dimensions: an odd one would put the last row or column on the wrong
    // half of a CFA block.
    let (width, height) = (width & !1, height & !1);
    let mut samples = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            let fx = x as f32 / width as f32;
            let fy = y as f32 / height as f32;
            let luma = 0.5 + 0.35 * (fx * 12.0).sin() * (fy * 9.0).cos();
            let rgb = [luma * 1.15, luma, luma * 0.8];
            let channel = if (x + y) % 2 == 1 {
                1
            } else if y % 2 == 0 {
                0
            } else {
                2
            };
            samples.push(rgb[channel]);
        }
    }
    samples
}

/// The four sides of the rectangle being drawn, as thin filled cells.
///
/// Four cells rather than one outlined one: a cell is opaque everywhere, so a
/// single rectangle with an edge would paint over the photograph it is meant to
/// be drawn on. The image handed to each side is never sampled — the edge colour
/// covers the whole of a cell this thin — but the type wants one.
fn draw_marquee(
    gpu: &Gpu,
    blit: &rawkit_engine::PreviewBlit,
    white: &rawkit_engine::PreviewImage,
    canvas_renderer: &session_canvas::CanvasRenderer,
    session: &Session,
    marquee: &Marquee,
) {
    // Surface pixels to canvas pixels. In the loupe the canvas is sized in level
    // pixels, so the outline has to shrink by the same factor the presenter
    // magnifies by, or it would sit somewhere else entirely when zoomed.
    let viewport = session.viewport();
    let level = viewport.level(session.max_level());
    let scale = viewport.scale * (1u32 << level) as f64;
    if scale <= 0.0 {
        return;
    }
    let [x, y, w, h] = marquee.rect();
    let to_canvas = |v: f64| (v / scale).round() as i32;
    let (x, y, w, h) = (to_canvas(x), to_canvas(y), to_canvas(w), to_canvas(h));
    if w <= 0 || h <= 0 {
        return;
    }
    // Two *screen* pixels, whatever the zoom. In canvas pixels the same line
    // would thin out as you zoom away — at fit on a 24 MP frame it came to a
    // single pixel, which on white foam is invisible.
    let t = (2.0 / scale).ceil().max(1.0) as i32;
    let colour = [0.98, 0.98, 0.98];
    let sides = [
        [x, y, w, t],
        [x, y + h - t, w, t],
        [x, y, t, h],
        [x + w - t, y, t, h],
    ];
    let cells: Vec<rawkit_engine::Cell<'_>> = sides
        .iter()
        .map(|dest| rawkit_engine::Cell {
            image: white,
            dest: *dest,
            tint: [1.0, 1.0, 1.0],
            // Thickness is a fraction of the cell's short side, so `t` on a cell
            // `t` thick covers all of it — which is the point: the image below
            // is never sampled.
            edge: (colour, t as f32),
            inner: ([0.0; 3], 0.0),
        })
        .collect();
    blit.draw_over(gpu, canvas_renderer.canvas(), &cells);
}

/// The rectangle on screen, as a crop of the photograph.
///
/// Composed with the crop already in force rather than replacing it: the drag
/// happened on what is *currently* visible, so a second crop names a region of
/// the first. Replacing would make every crop after the first jump somewhere
/// else, which reads as the rectangle being ignored.
fn crop_from(session: &Session, marquee: &Marquee) -> Option<rawkit_editstate::Crop> {
    let [x, y, w, h] = marquee.rect();
    if w < 8.0 || h < 8.0 {
        // A click, or a twitch. Cropping to a few pixels is never what was
        // meant, and the photograph would vanish.
        return None;
    }
    let viewport = session.viewport();
    let [dw, dh] = session.developed_size();
    let (dw, dh) = (dw as f64, dh as f64);

    let top_left = viewport.image_at([x, y]);
    let bottom_right = viewport.image_at([x + w, y + h]);
    let fraction = |v: f64, extent: f64| (v / extent).clamp(0.0, 1.0) as f32;

    let now = session.state().crop;
    let (span_x, span_y) = (now.right - now.left, now.bottom - now.top);
    let crop = rawkit_editstate::Crop {
        left: now.left + span_x * fraction(top_left[0], dw),
        top: now.top + span_y * fraction(top_left[1], dh),
        right: now.left + span_x * fraction(bottom_right[0], dw),
        bottom: now.top + span_y * fraction(bottom_right[1], dh),
        ..rawkit_editstate::Crop::default()
    };
    crop.validate().ok().map(|()| crop)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A camera whose primaries are XYZ's, so the profile stage contributes
    /// nothing and what is measured is the eyedropper's own arithmetic.
    fn plain() -> rawkit_engine::CameraProfile {
        rawkit_engine::CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY)
    }

    #[test]
    fn a_blown_patch_is_refused_rather_than_answered() {
        // The commonest way to misuse an eyedropper is to click the brightest
        // thing in the frame. Its channels are the sensor's ceiling, so their
        // ratios describe the limit rather than the light — and a temperature
        // derived from them looks exactly like a good one.
        let refused = neutralising([0.99, 1.0, 0.985], &plain());
        assert!(refused.is_err(), "a blown patch gave {refused:?}");
        assert!(refused.unwrap_err().contains("blown"));
    }

    #[test]
    fn a_patch_with_nothing_in_it_is_refused() {
        assert!(neutralising([0.001, 0.002, 0.0015], &plain()).is_err());
        // Zero in one channel would divide by nothing a moment later.
        assert!(neutralising([0.4, 0.4, 0.0], &plain()).is_err());
    }

    #[test]
    fn a_neutral_patch_asks_for_no_correction() {
        // Equal channels are already balanced, so whatever temperature comes
        // back must be the one whose multipliers are all one — which is the
        // round trip this leans on, checked from the eyedropper's side.
        let (kelvin, tint) = neutralising([0.4, 0.4, 0.4], &plain()).expect("a usable patch");
        // With the tint it came back with, not zero: equal camera channels are
        // equal *XYZ* under this profile, and the equal-energy white point is
        // not on the Planckian locus — so a grey legitimately carries a tint,
        // and asking for the multipliers without it is asking a different
        // question. (This test failed that way first.)
        let multipliers = plain().multipliers_for(kelvin, tint);
        for m in multipliers {
            assert!(
                (m - 1.0).abs() < 0.05,
                "a grey asked for {multipliers:?}, which is not no correction"
            );
        }
    }

    #[test]
    fn a_bluer_patch_asks_for_a_warmer_render() {
        // The direction, which is the half a sign error would get wrong: a patch
        // the sensor saw as blue is under blue light, so making it neutral means
        // rendering the whole frame warmer — a higher colour temperature.
        let grey = neutralising([0.4, 0.4, 0.4], &plain()).expect("grey");
        let blue = neutralising([0.3, 0.4, 0.55], &plain()).expect("a blue patch");
        assert!(
            blue.0 > grey.0,
            "a blue patch gave {:.0} K against a grey's {:.0} K",
            blue.0,
            grey.0
        );
    }
}

#[cfg(test)]
mod gradient_tests {
    use super::*;
    use rawkit_editstate::{Crop, EditState, MaskShape, Orientation};
    use rawkit_session::{Command, Session};

    const SENSOR: [u32; 2] = [6000, 4000];

    /// A session fitted to a window, which is what the shell hands the drag.
    fn fitted(state: EditState) -> Session {
        let mut session = Session::new(SENSOR, 512, state, Orientation::AsShot);
        session.apply(Command::Resize {
            width: 1200,
            height: 800,
        });
        session
    }

    const GRADIENT: MaskShape = MaskShape::Linear {
        from: [0.5, 0.0],
        to: [0.5, 0.35],
    };

    fn drawn(session: &Session, from: [f64; 2], to: [f64; 2]) -> ([f32; 2], [f32; 2]) {
        match shape_from_drag(
            GRADIENT,
            from,
            to,
            session.viewport(),
            &session.geometry(),
            session.image_size(),
        ) {
            MaskShape::Linear { from, to } => (from, to),
            other => panic!("a gradient drag produced {other:?}"),
        }
    }

    /// Where the photograph sits inside the window, so a drag can be aimed at
    /// the picture rather than at the letterboxing beside it.
    ///
    /// `Viewport` maps canvas to image and not the other way, so this inverts
    /// it by probing: the mapping is affine, so two points along each axis give
    /// its origin and its step exactly.
    fn frame(session: &Session) -> [f64; 4] {
        let view = session.viewport();
        let origin = view.image_at([0.0, 0.0]);
        let step = view.image_at([1.0, 1.0]);
        let canvas = |image: [f64; 2]| {
            [
                (image[0] - origin[0]) / (step[0] - origin[0]),
                (image[1] - origin[1]) / (step[1] - origin[1]),
            ]
        };
        let size = session.geometry().output_size(session.image_size());
        let a = canvas([0.0, 0.0]);
        let b = canvas([size[0] as f64, size[1] as f64]);
        [a[0], a[1], b[0], b[1]]
    }

    #[test]
    fn a_drag_down_the_picture_is_a_gradient_down_the_sensor() {
        let session = fitted(EditState::default());
        let [x0, y0, x1, y1] = frame(&session);
        let middle = (x0 + x1) / 2.0;
        let (from, to) = drawn(&session, [middle, y0 + 1.0], [middle, (y0 + y1) / 2.0]);
        println!("upright: {from:?} -> {to:?}");
        assert!(from[1] < 0.01, "the near end is not at the top: {from:?}");
        assert!(
            (to[1] - 0.5).abs() < 0.02,
            "the far end is not half way down: {to:?}"
        );
        assert!(
            (from[0] - 0.5).abs() < 0.02 && (to[0] - 0.5).abs() < 0.02,
            "a straight drag came out slanted: {from:?} -> {to:?}"
        );
    }

    #[test]
    fn a_drag_on_a_turned_frame_lands_where_the_hand_drew_it() {
        // The whole reason the shape is stored in sensor fractions. Turned a
        // quarter, a drag down the *screen* runs across the sensor — and if it
        // were stored in screen fractions instead, the gradient would swing
        // round the moment the picture was turned back.
        let session = fitted(EditState {
            orientation: Orientation::Rotate90Cw,
            ..EditState::default()
        });
        let [x0, y0, x1, y1] = frame(&session);
        let middle = (x0 + x1) / 2.0;
        let (from, to) = drawn(&session, [middle, y0 + 1.0], [middle, (y0 + y1) / 2.0]);
        println!("turned: {from:?} -> {to:?}");
        // Down the screen is along one of the sensor's *horizontal* axes now.
        assert!(
            (from[1] - to[1]).abs() < 0.02,
            "a drag down a turned frame moved down the sensor too: {from:?} -> {to:?}"
        );
        assert!(
            (from[0] - to[0]).abs() > 0.3,
            "a drag down a turned frame did not move across the sensor: {from:?} -> {to:?}"
        );
    }

    #[test]
    fn a_drag_on_a_cropped_frame_is_measured_against_the_whole_sensor() {
        // A crop shows the middle half. Dragging from the top of what is *shown*
        // must land a quarter of the way down the sensor, not at the top of it —
        // otherwise the gradient would jump the moment the crop changed.
        let session = fitted(EditState {
            crop: Crop {
                left: 0.25,
                top: 0.25,
                right: 0.75,
                bottom: 0.75,
                angle_deg: 0.0,
            },
            ..EditState::default()
        });
        let [x0, y0, x1, y1] = frame(&session);
        let middle = (x0 + x1) / 2.0;
        let (from, to) = drawn(&session, [middle, y0 + 1.0], [middle, y1 - 1.0]);
        println!("cropped: {from:?} -> {to:?}");
        assert!(
            (from[1] - 0.25).abs() < 0.02,
            "the top of the crop is not a quarter down the sensor: {from:?}"
        );
        assert!(
            (to[1] - 0.75).abs() < 0.02,
            "the bottom of the crop is not three quarters down: {to:?}"
        );
    }

    #[test]
    fn a_gradient_survives_the_state_it_is_stored_in() {
        // End to end through the session, the way the window does it: add,
        // place, adjust. The point is that `SetMasks` accepts what the drag
        // produces — a shape rejected here would leave the mask at whatever
        // placement it had and look like a dead drag.
        let mut session = fitted(EditState::default());
        session.apply(Command::SetMasks {
            masks: vec![rawkit_editstate::Mask {
                exposure_ev: -1.0,
                ..rawkit_editstate::Mask::default()
            }],
            control: u8::MAX,
        });
        let [x0, y0, x1, y1] = frame(&session);
        let shape = shape_from_drag(
            GRADIENT,
            [(x0 + x1) / 2.0, y0 + 1.0],
            [(x0 + x1) / 2.0, (y0 + y1) / 2.0],
            session.viewport(),
            &session.geometry(),
            session.image_size(),
        );
        let mut masks = session.state().masks.clone();
        masks[0].shape = shape;
        masks[0].warmth = 0.5;
        session.apply(Command::SetMasks { masks, control: 0 });

        let stored = &session.state().masks[0];
        assert_eq!(stored.shape, shape, "the placement was refused");
        assert_eq!(stored.warmth, 0.5);
        assert!(
            session.state().validate().is_ok(),
            "a placement the window can make is not a state the catalog will hold"
        );
    }
}

#[cfg(test)]
mod radial_tests {
    use super::*;
    use rawkit_editstate::{EditState, MaskShape, Orientation};
    use rawkit_session::{Command, Session};

    const SENSOR: [u32; 2] = [6000, 4000];

    const ELLIPSE: MaskShape = MaskShape::Radial {
        centre: [0.5, 0.5],
        radii: [0.2, 0.3],
        feather: 0.5,
    };

    fn fitted() -> Session {
        let mut session = Session::new(SENSOR, 512, EditState::default(), Orientation::AsShot);
        session.apply(Command::Resize {
            width: 1200,
            height: 800,
        });
        session
    }

    fn dragged(session: &Session, from: [f64; 2], to: [f64; 2]) -> ([f32; 2], [f32; 2], f32) {
        match shape_from_drag(
            ELLIPSE,
            from,
            to,
            session.viewport(),
            &session.geometry(),
            session.image_size(),
        ) {
            MaskShape::Radial {
                centre,
                radii,
                feather,
            } => (centre, radii, feather),
            other => panic!("a radial drag produced {other:?}"),
        }
    }

    /// Canvas positions of the displayed frame's corners. `Viewport` maps canvas
    /// to image and not back, so this inverts it by probing an affine map.
    fn frame(session: &Session) -> [f64; 4] {
        let view = session.viewport();
        let origin = view.image_at([0.0, 0.0]);
        let step = view.image_at([1.0, 1.0]);
        let canvas = |image: [f64; 2]| {
            [
                (image[0] - origin[0]) / (step[0] - origin[0]),
                (image[1] - origin[1]) / (step[1] - origin[1]),
            ]
        };
        let size = session.geometry().output_size(session.image_size());
        let a = canvas([0.0, 0.0]);
        let b = canvas([size[0] as f64, size[1] as f64]);
        [a[0], a[1], b[0], b[1]]
    }

    #[test]
    fn an_ellipse_grows_out_of_where_the_hand_went_down() {
        let session = fitted();
        let [x0, y0, x1, y1] = frame(&session);
        let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        // Out to a quarter of the frame's width and an eighth of its height.
        let (centre, radii, feather) = dragged(
            &session,
            [cx, cy],
            [cx + (x1 - x0) / 4.0, cy + (y1 - y0) / 8.0],
        );
        println!("centre {centre:?} radii {radii:?}");
        assert!(
            (centre[0] - 0.5).abs() < 0.01 && (centre[1] - 0.5).abs() < 0.01,
            "the ellipse is not centred where the press was: {centre:?}"
        );
        assert!(
            (radii[0] - 0.25).abs() < 0.01,
            "the horizontal radius is {}, not a quarter of the frame",
            radii[0]
        );
        assert!(
            (radii[1] - 0.125).abs() < 0.01,
            "the vertical radius is {}, not an eighth of the frame",
            radii[1]
        );
        assert_eq!(feather, 0.5, "the drag changed the feather");
    }

    #[test]
    fn a_drag_that_barely_moves_still_leaves_an_ellipse() {
        // A slip is not a shape, and a radius of zero is a state the catalog
        // refuses — so the floor is here rather than an error the window would
        // have to explain.
        let session = fitted();
        let [x0, y0, ..] = frame(&session);
        let (_, radii, _) = dragged(&session, [x0 + 400.0, y0 + 300.0], [x0 + 400.0, y0 + 300.0]);
        assert!(
            radii[0] > 0.0 && radii[1] > 0.0,
            "a press with no travel left an ellipse with no size: {radii:?}"
        );
        let mut session = fitted();
        session.apply(Command::SetMasks {
            masks: vec![rawkit_editstate::Mask {
                shape: MaskShape::Radial {
                    centre: [0.5, 0.5],
                    radii,
                    feather: 0.5,
                },
                ..rawkit_editstate::Mask::default()
            }],
            control: u8::MAX,
        });
        assert!(
            session.state().validate().is_ok(),
            "the floor is not high enough for the state layer to accept it"
        );
    }

    #[test]
    fn the_drag_is_read_as_whatever_kind_is_already_there() {
        // One gesture, two meanings, decided by the mask being placed rather
        // than by a mode the window has to be in — and getting that backwards
        // would turn every radial into a gradient the moment it was redrawn.
        let session = fitted();
        let [x0, y0, x1, y1] = frame(&session);
        let (a, b) = ([(x0 + x1) / 2.0, (y0 + y1) / 2.0], [x1 - 10.0, y1 - 10.0]);
        let size = session.image_size();
        let linear = shape_from_drag(
            MaskShape::Linear {
                from: [0.0, 0.0],
                to: [1.0, 1.0],
            },
            a,
            b,
            session.viewport(),
            &session.geometry(),
            size,
        );
        let radial = shape_from_drag(ELLIPSE, a, b, session.viewport(), &session.geometry(), size);
        assert!(matches!(linear, MaskShape::Linear { .. }), "{linear:?}");
        assert!(matches!(radial, MaskShape::Radial { .. }), "{radial:?}");
    }
}
