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
//! **Neither macOS nor Windows has been run.** CI proves the shell compiles and
//! links on both, and for window and pointer code that is a weak claim: a
//! surface behind an opaque view, or a canvas that never sees a click, are
//! things no compiler notices. The geometry is verified — route 1 on Linux
//! reports the right surface and canvas rectangles and draws the photograph
//! below the chrome — but the compositing on those two platforms rests on the
//! probe research rather than on a machine.
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
mod session_canvas;

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

/// Width of the chrome in route 2, in logical pixels.
const PANEL_WIDTH: f64 = 400.0;
/// Height the chrome keeps in route 3. `default_vbox` is a vertical box, and two
/// disjoint rectangles is what the arrangement needs.
///
/// The equivalents on macOS and Windows will be a child NSView and a child
/// HWND, and those will bring their own geometry — but the constant is not
/// gated, because a value only some platforms can see is how this file has
/// broken the build four times.
const PANEL_HEIGHT: i32 = 200;
const WINDOW: (f64, f64) = (1200.0, 800.0);

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
    })
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
        .invoke_handler(tauri::generate_handler![apply, snapshot, cull, cull_view])
        .setup(move |app| {
            // The surface is created on the main thread because the raw window
            // handle comes from GTK on this platform and GTK is not thread-safe.
            let (gpu, surface, layout, window_handle) = build_window(app, route)?;
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
            if layout.top > 0 {
                eprintln!(
                    "canvas     : {}x{} at y={} (the chrome floats over the surface)",
                    layout.canvas.width, layout.canvas.height, layout.top
                );
            }
            surface.configure(&gpu.device, &config);
            let mut config = config;

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

            let loaded = Loaded::open(raw.as_deref(), DEFAULT_TILE)?;
            let mut session = Session::new(loaded.size, DEFAULT_TILE, EditState::default());
            session.apply(Command::Resize {
                width: layout.canvas.width,
                height: layout.canvas.height,
            });
            session.apply(Command::FitToView);

            let shared = Arc::new(Mutex::new(session));
            app.manage(Shared(shared.clone()));

            let mut saver = Saver::new(library.clone(), shared.clone());
            // Whatever was last decided about this photograph, if anything was.
            saver.restore(&mut shared.lock().expect("session lock"));

            // Routes 1 and 2 put the canvas over the whole window; route 3
            // reserves a strip for the chrome.
            let panel = if route == Route::NativeChild {
                PANEL_HEIGHT
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
            let surface_size = [layout.canvas.width, layout.canvas.height];
            // Where the photograph goes inside the swapchain. Under route 3 that
            // is all of it; under a cutout it starts below the chrome.
            let canvas_rect = [0, layout.top, layout.canvas.width, layout.canvas.height];
            let mut stats = FrameStats::default();
            let navigating = library.clone();
            // The rectangle the canvas currently carries, so a settled one is
            // not redrawn on every frame. See the crop block below.
            let mut last_marquee: Option<[f64; 4]> = None;
            let mut tick = move || -> Result<()> {
                let started = std::time::Instant::now();

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
                    let size = library.lock().expect("library lock").size_of_current()?;

                    let mut session = shared.lock().expect("session lock");
                    if size != showing.size {
                        // A different body, or a different orientation. Nothing
                        // about the old view means anything, so start again.
                        *session = Session::new(size, DEFAULT_TILE, EditState::default());
                        session.apply(Command::Resize {
                            width: surface_size[0],
                            height: surface_size[1],
                        });
                        session.apply(Command::FitToView);
                    }
                    // Same size: the viewport is left exactly as it was, which
                    // is what carries a 1:1 sharpness check from frame to frame.
                    showing.size = size;
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
                    let next = Loaded::open(showing.path.as_deref(), DEFAULT_TILE)?;
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
                paint(
                    &gpu,
                    &surface,
                    canvas_renderer.presenter(),
                    canvas_renderer.canvas(),
                    canvas_rect,
                )?;
                stats.record(drawn, started.elapsed());
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
    panel_height: i32,
    session: Arc<Mutex<Session>>,
) -> Result<()> {
    canvas::attach_input(window, panel_height, session)
}

#[cfg(not(target_os = "linux"))]
fn attach_input(
    _window: &tauri::Window,
    _panel_height: i32,
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
/// over the top of it, so the photograph has to be told to stay below the
/// chrome — otherwise its top strip is behind the panel and simply not visible.
#[derive(Debug, Clone, Copy)]
struct Layout {
    /// What the surface is configured at: always the window.
    surface: tauri::PhysicalSize<u32>,
    /// The photograph's rectangle within it.
    canvas: tauri::PhysicalSize<u32>,
    /// How far down that rectangle starts, in surface pixels.
    top: u32,
}

/// Build the window for the chosen arrangement and put a surface on it.
#[allow(clippy::type_complexity)]
fn build_window(
    app: &tauri::App,
    route: Route,
) -> Result<(Gpu, wgpu::Surface<'static>, Layout, tauri::Window)> {
    match route {
        Route::Cutout => {
            // The real interface, told to leave the canvas area unpainted. The
            // probe page it used to load answered a different question and has
            // no controls on it.
            let builder =
                WebviewWindowBuilder::new(app, "main", WebviewUrl::App("panel.html?cutout".into()))
                    .title("rawkit")
                    .inner_size(WINDOW.0, WINDOW.1);
            // Transparent on every platform, macOS included. That needs
            // Tauri's `macos-private-api`, which bars *App Store* distribution
            // and nothing else — notarised direct download, which is how a tool
            // like this ships, is unaffected. The alternative was a child
            // NSView, several hundred lines of FFI nobody here can run.
            let builder = builder.transparent(true);
            let window = builder.build()?;
            let surface_size = window.inner_size()?;
            let scale = window.scale_factor()?;
            let top = (PANEL_HEIGHT as f64 * scale) as u32;
            let (gpu, surface) = Gpu::with_surface(window.clone())?;
            let handle = window.as_ref().window();
            Ok((
                gpu,
                surface,
                Layout {
                    surface: surface_size,
                    canvas: tauri::PhysicalSize::new(
                        surface_size.width,
                        surface_size.height.saturating_sub(top).max(1),
                    ),
                    top,
                },
                handle,
            ))
        }
        Route::ChildWebview => {
            let window = WindowBuilder::new(app, "main")
                .title("rawkit")
                .inner_size(WINDOW.0, WINDOW.1)
                .build()?;
            let panel = window.add_child(
                WebviewBuilder::new("panel", WebviewUrl::App("panel.html".into())),
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(PANEL_WIDTH, WINDOW.1),
            )?;
            panel.set_auto_resize(false)?;
            panel.set_position(LogicalPosition::new(0.0, 0.0))?;
            panel.set_size(LogicalSize::new(PANEL_WIDTH, WINDOW.1))?;
            eprintln!(
                "panel      : {:?} (ignored on Linux; see the module docs)",
                panel.bounds()?
            );
            let size = window.inner_size()?;
            let (gpu, surface) = Gpu::with_surface(window.clone())?;
            // A probe route, kept for the record rather than shipped: its chrome
            // is a side strip and nothing has ever placed it correctly.
            Ok((
                gpu,
                surface,
                Layout {
                    surface: size,
                    canvas: size,
                    top: 0,
                },
                window,
            ))
        }
        #[cfg(target_os = "linux")]
        Route::NativeChild => {
            let window =
                WebviewWindowBuilder::new(app, "main", WebviewUrl::App("panel.html".into()))
                    .title("rawkit")
                    .inner_size(WINDOW.0, WINDOW.1)
                    .build()?;
            let canvas = canvas::attach(&window.as_ref().window(), PANEL_HEIGHT)?;
            eprintln!("canvas     : X window {canvas:?}");
            let outer = window.inner_size()?;
            let scale = window.scale_factor()?;
            let panel = (PANEL_HEIGHT as f64 * scale) as u32;
            let size =
                tauri::PhysicalSize::new(outer.width, outer.height.saturating_sub(panel).max(1));
            let (gpu, surface) = Gpu::with_surface(canvas)?;
            let handle = window.as_ref().window();
            // The canvas has a window of its own, so the surface *is* the
            // photograph's rectangle and there is nothing to offset.
            Ok((
                gpu,
                surface,
                Layout {
                    surface: size,
                    canvas: size,
                    top: 0,
                },
                handle,
            ))
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
