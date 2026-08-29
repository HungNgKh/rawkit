//! The compositing probe — **and its result, which is that route 1 does not
//! work on Linux/WebKitGTK.**
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
fn cull(state: tauri::State<'_, Shelf>, action: CullAction) -> Result<CullView, String> {
    let Some(library) = &state.0 else {
        return Err("no library is open; pass a .rawkit catalog".into());
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
        _ => Route::NativeChild,
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
            let (gpu, surface, size, window_handle) = build_window(app, route)?;
            eprintln!(
                "gpu        : {} ({:?})",
                gpu.adapter_info.name, gpu.adapter_info.backend
            );
            let config = surface
                .get_default_config(&gpu.adapter, size.width, size.height)
                .ok_or_else(|| anyhow!("this surface supports no configuration we can use"))?;
            eprintln!(
                "surface    : {:?} {}x{}",
                config.format, size.width, size.height
            );
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
                width: size.width,
                height: size.height,
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
                [size.width, size.height],
            );
            let blit = rawkit_engine::PreviewBlit::new(&gpu);
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
            let surface_size = [size.width, size.height];
            let mut stats = FrameStats::default();
            let navigating = library.clone();
            let mut tick = move || -> Result<()> {
                let started = std::time::Instant::now();

                // A keypress left a request; this is where it costs anything.
                // Decoding here rather than in the command handler keeps the IPC
                // call immediate and keeps a fifth of a second of CPU off the
                // thread the compositor is waiting on.
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
                paint(
                    &gpu,
                    &surface,
                    canvas_renderer.presenter(),
                    canvas_renderer.canvas(),
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
    eprintln!("input      : pointer input on the canvas is implemented for X11 only so far");
    Ok(())
}

/// Build the window for the chosen arrangement and put a surface on it.
#[allow(clippy::type_complexity)]
fn build_window(
    app: &tauri::App,
    route: Route,
) -> Result<(
    Gpu,
    wgpu::Surface<'static>,
    tauri::PhysicalSize<u32>,
    tauri::Window,
)> {
    match route {
        Route::Cutout => {
            let builder =
                WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                    .title("rawkit")
                    .inner_size(WINDOW.0, WINDOW.1);
            // A third strike against route 1, found by CI rather than by the
            // probe: on macOS `transparent` is behind Tauri's
            // `macos-private-api` feature, and shipping a private API bars App
            // Store distribution. So route 1 costs a distribution channel even
            // on the platform where it works.
            #[cfg(not(target_os = "macos"))]
            let builder = builder.transparent(true);
            let window = builder.build()?;
            let size = window.inner_size()?;
            let (gpu, surface) = Gpu::with_surface(window.clone())?;
            let handle = window.as_ref().window();
            Ok((gpu, surface, size, handle))
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
            Ok((gpu, surface, size, window))
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
            Ok((gpu, surface, size, handle))
        }
        #[cfg(not(target_os = "linux"))]
        Route::NativeChild => Err(anyhow!(
            "the native-child canvas is implemented for X11 only so far"
        )),
    }
}

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
        presenter.draw(gpu, canvas, &view)?;
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
