//! A native widget for the canvas, so the GPU is not fighting the webview for a
//! window.
//!
//! # Why this exists
//!
//! The two portable arrangements both failed on Linux (see the module docs of
//! `main.rs`): a transparent webview over the surface *takes turns* with it, and
//! Tauri's child webviews cannot be positioned. Both failures share one cause —
//! **the GPU surface is attached to the toplevel window, which is also the
//! webview's window.**
//!
//! X has an answer that predates all of this: give the canvas its own window.
//! Output to a window is clipped to exclude its mapped children, so a surface on
//! a child cannot paint over a sibling, and a sibling cannot paint over it.
//! There is nothing to contend for.
//!
//! Tauri exposes `Window::default_vbox()` on Linux — the `GtkBox` wry packs the
//! webview into — so the widget can be packed beside it with no patching and no
//! private API. `GtkDrawingArea` is used because it owns a `GdkWindow` rather
//! than drawing on its parent's, which is the entire property being relied on.
//!
//! # What this costs
//!
//! Per-OS shell code. macOS wants a child `NSView` and Windows a child `HWND`,
//! and those are separate implementations of this file rather than a
//! cross-platform abstraction pretending to be one. That cost is confined to the
//! shell: the engine still sees one surface and one canvas, so "same RAW + same
//! EditState -> same pixels" is untouched.

use anyhow::{anyhow, Result};
use gtk::gdk;
use gtk::glib::translate::ToGlibPtr;
use gtk::prelude::*;
use std::ffi::c_void;
use std::num::NonZeroU32;
use std::ptr::NonNull;
use wgpu::rwh;

/// Raw handles for the canvas's own X window.
///
/// Deliberately holds no GTK object. GTK types are neither `Send` nor `Sync` and
/// wgpu requires a surface target that is both, so what crosses the boundary is
/// three plain values. The widget itself stays owned by the `GtkBox` it was
/// packed into, which outlives the window.
#[derive(Debug, Clone, Copy)]
pub struct CanvasWindow {
    window: NonZeroU32,
    display: NonNull<c_void>,
    screen: i32,
}

// SAFETY: the two pointers are an Xlib `Display*` and an X resource ID, both
// owned by GTK for the lifetime of the toplevel window. The `Display*` is
// dereferenced only when the surface is created, which happens on the main
// thread before any render thread exists; afterwards presentation goes through
// the Vulkan driver and never touches it again. Sending the values themselves is
// what every winit-based application does.
unsafe impl Send for CanvasWindow {}
unsafe impl Sync for CanvasWindow {}

impl rwh::HasWindowHandle for CanvasWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let handle = rwh::XlibWindowHandle::new(self.window.get() as std::ffi::c_ulong);
        // SAFETY: the window ID is valid for as long as the GTK widget is, and
        // the widget is owned by the window this handle came from.
        Ok(unsafe { rwh::WindowHandle::borrow_raw(rwh::RawWindowHandle::Xlib(handle)) })
    }
}

impl rwh::HasDisplayHandle for CanvasWindow {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        let handle = rwh::XlibDisplayHandle::new(Some(self.display), self.screen);
        // SAFETY: as above — GTK owns the connection and outlives the surface.
        Ok(unsafe { rwh::DisplayHandle::borrow_raw(rwh::RawDisplayHandle::Xlib(handle)) })
    }
}

/// Pack a canvas widget into the window, beside whatever webview is already
/// there, and return handles to its X window.
///
/// `panel_height` is how much of the window the webview keeps. The split is
/// vertical because `default_vbox` is a vertical box and the probe only needs
/// two disjoint rectangles, not the final layout.
///
/// No size comes back: at this point the widget has an X window but GTK has not
/// laid the toplevel out, so it measures 2x2 and would be a trap to trust.
/// The caller sizes the surface from the layout it intends. X clips the surface
/// to the widget regardless, which is the whole reason this arrangement works —
/// a surface configured too large spills nowhere.
/// Make Xlib safe to use from more than one thread.
///
/// **Must run before anything opens an X connection** — before Tauri starts GTK,
/// hence the first line of `main`.
///
/// Painting happens on the main thread, so this looks unnecessary and is not.
/// Mesa's X11 present path runs its own thread on the connection GTK is using,
/// and without this the failures are `xcb_xlib_threads_sequence_lost` (an abort
/// inside libxcb) and `RenderBadPicture` from GTK's own drawing — the second
/// arriving tens of seconds after the frame that caused it, naming nothing
/// useful. Moving painting to the main thread made them rarer, which is worse
/// than leaving them frequent, because rare looks like fixed.
pub fn init_threads() {
    // SAFETY: called before any X connection exists, which is the documented
    // requirement and the only one.
    unsafe {
        x11::xlib::XInitThreads();
    }
}

/// Create the canvas's X window as a child of the toplevel, and return handles
/// to it.
///
/// # Why not a GtkDrawingArea
///
/// That was the first attempt and it does not survive. A widget's `GdkWindow`
/// stays GTK's: every frame GTK opens a paint cycle on it, creating and freeing
/// an XRender `Picture`, while Vulkan presents to the same window. The two
/// collide as `RenderBadPicture` — reported asynchronously, ten seconds later,
/// naming nothing that caused it. Neither `set_app_paintable`, a `draw` handler
/// that does nothing, nor the deprecated `set_double_buffered(false)` stops it;
/// the last made it happen sooner.
///
/// So the window is created directly instead. GTK has no widget for it, never
/// lays it out and never paints it, and X clips the toplevel's own drawing to
/// exclude it. The cost is that nothing moves or resizes it for us — which for a
/// canvas is control rather than a chore.
pub fn attach(window: &tauri::Window, panel_height: i32) -> Result<CanvasWindow> {
    let gtk_window = window.gtk_window()?;
    let parent = gtk_window
        .window()
        .ok_or_else(|| anyhow!("the toplevel has no X window yet"))?;

    let (width, height) = (gtk_window.allocated_width(), gtk_window.allocated_height());
    let attributes = gdk::WindowAttr {
        x: Some(0),
        y: Some(panel_height),
        width: width.max(1),
        height: (height - panel_height).max(1),
        window_type: gdk::WindowType::Child,
        wclass: gdk::WindowWindowClass::InputOutput,
        // The parent's visual, so the child is the same depth as the surface
        // wgpu will build for it.
        visual: Some(parent.visual()),
        ..Default::default()
    };
    let child = gdk::Window::new(Some(&parent), &attributes);
    child.show();

    let x11_display: gdkx11::X11Display = child
        .display()
        .downcast()
        .map_err(|_| anyhow!("the canvas is not on an X11 display"))?;
    let x11_screen: gdkx11::X11Screen = child
        .screen()
        .downcast()
        .map_err(|_| anyhow!("the canvas is not on an X11 screen"))?;
    let x11_window: gdkx11::X11Window = child
        .clone()
        .downcast()
        .map_err(|_| anyhow!("the canvas is not on an X11 window"))?;

    // SAFETY: `x11_display` is a live GdkX11Display; this returns the connection
    // GTK already uses rather than opening one, which is what separates it from
    // asking Tauri for a display handle — that hands back a fresh pointer every
    // call.
    let display =
        unsafe { gdkx11::ffi::gdk_x11_display_get_xdisplay(x11_display.to_glib_none().0) };

    // The window must outlive every surface built on it, and nothing else here
    // has a lifetime long enough to own it. One window, once, for as long as the
    // process runs.
    std::mem::forget(child);

    Ok(CanvasWindow {
        window: NonZeroU32::new(x11_window.xid() as u32)
            .ok_or_else(|| anyhow!("the canvas window has no X window id"))?,
        display: NonNull::new(display.cast())
            .ok_or_else(|| anyhow!("GTK reported a null X display"))?,
        screen: x11_screen.screen_number(),
    })
}

/// Route pointer input over the canvas into the session.
///
/// # Why this hangs off the toplevel and not the canvas window
///
/// The canvas has no GTK widget — that is the whole point of creating it
/// directly — so GTK has nothing to deliver its events to. Rather than
/// associating it with a widget after the fact, the canvas simply selects no
/// events at all, and X does the rest: an event a window has not asked for
/// propagates to its ancestor, which here is the toplevel, which does have a
/// widget.
///
/// So the handlers below see pointer events over the canvas as if they happened
/// on the window, in the window's coordinates, and the only thing to do is
/// notice which side of the panel they fall on.
pub fn attach_input(
    window: &tauri::Window,
    panel_height: i32,
    session: std::sync::Arc<std::sync::Mutex<rawkit_session::Session>>,
) -> Result<()> {
    use gtk::gdk::EventMask;
    use rawkit_session::Command;

    let gtk_window = window.gtk_window()?;
    gtk_window.add_events(
        EventMask::BUTTON_PRESS_MASK
            | EventMask::BUTTON_RELEASE_MASK
            | EventMask::POINTER_MOTION_MASK
            | EventMask::SCROLL_MASK,
    );

    let scale = gtk_window.scale_factor() as f64;
    // Logical coordinates from GTK, physical everywhere in the session and the
    // surface. Converting here means the rest of the shell never has to know
    // which it is holding.
    let to_canvas = move |(x, y): (f64, f64)| -> Option<[f64; 2]> {
        let y = y - panel_height as f64;
        (y >= 0.0).then_some([x * scale, y * scale])
    };

    let dragging: std::rc::Rc<std::cell::Cell<Option<(f64, f64)>>> =
        std::rc::Rc::new(std::cell::Cell::new(None));

    let held = dragging.clone();
    gtk_window.connect_button_press_event(move |_, event| {
        let Some(at) = to_canvas(event.position()) else {
            return gtk::glib::Propagation::Proceed;
        };
        if crate::in_grid() {
            // The grid works out which cell this is, because it is the only
            // place that knows the layout. This widget's whole job is turning
            // GTK's logical coordinates into canvas ones.
            let double = event.event_type() == gtk::gdk::EventType::DoubleButtonPress;
            *crate::CANVAS_CLICK.lock().expect("click lock") = Some((at, double));
        } else if crate::in_crop() {
            // A new drag replaces whatever rectangle was there. Starting from
            // the old one instead would mean a crop could only ever shrink.
            *crate::CANVAS_MARQUEE.lock().expect("marquee lock") = Some(crate::Marquee {
                start: at,
                end: at,
                settled: false,
            });
            held.set(Some(event.position()));
        } else {
            held.set(Some(event.position()));
        }
        gtk::glib::Propagation::Proceed
    });

    let held = dragging.clone();
    gtk_window.connect_button_release_event(move |_, _| {
        held.set(None);
        if let Some(marquee) = crate::CANVAS_MARQUEE.lock().expect("marquee lock").as_mut() {
            // Settled, not applied. The rectangle stays on screen until Enter
            // takes it, so a drag that came out wrong can be redrawn rather than
            // undone.
            marquee.settled = true;
        }
        gtk::glib::Propagation::Proceed
    });

    let held = dragging.clone();
    let dragged = session.clone();
    gtk_window.connect_motion_notify_event(move |_, event| {
        if crate::in_crop() {
            if held.get().is_some() {
                if let (Some(at), Some(marquee)) = (
                    to_canvas(event.position()),
                    crate::CANVAS_MARQUEE.lock().expect("marquee lock").as_mut(),
                ) {
                    marquee.end = at;
                }
            }
            return gtk::glib::Propagation::Proceed;
        }
        if let Some((lx, ly)) = held.get() {
            let (x, y) = event.position();
            held.set(Some((x, y)));
            // A drag emits one of these per motion event, far faster than the
            // GPU draws. The session has no queue, so the extra ones cost a
            // lock and two floats each and only the last is ever rendered —
            // this is the arrangement that claim was written for.
            dragged.lock().expect("session lock").apply(Command::Pan {
                dx: (x - lx) * scale,
                dy: (y - ly) * scale,
            });
        }
        gtk::glib::Propagation::Proceed
    });

    let zoomed = session;
    gtk_window.connect_scroll_event(move |_, event| {
        let Some(anchor) = to_canvas(event.position()) else {
            return gtk::glib::Propagation::Proceed;
        };
        if crate::in_grid() {
            // A grid scrolls; it does not zoom. Accumulated rather than applied,
            // for the same reason the click is: the layout lives elsewhere.
            let notches = match event.direction() {
                gtk::gdk::ScrollDirection::Up => -1,
                gtk::gdk::ScrollDirection::Down => 1,
                gtk::gdk::ScrollDirection::Smooth => event.delta().1.round() as i32,
                _ => 0,
            };
            crate::CANVAS_SCROLL.fetch_add(notches, std::sync::atomic::Ordering::Relaxed);
            return gtk::glib::Propagation::Proceed;
        }
        let step = match event.direction() {
            gtk::gdk::ScrollDirection::Up => 1.15,
            gtk::gdk::ScrollDirection::Down => 1.0 / 1.15,
            // Trackpads send Smooth with a delta rather than a direction.
            gtk::gdk::ScrollDirection::Smooth => (-event.delta().1 * 0.15).exp2(),
            _ => return gtk::glib::Propagation::Proceed,
        };
        let mut session = zoomed.lock().expect("session lock");
        let scale = session.viewport().scale * step;
        // Hold the image point under the cursor still, which is what makes
        // scroll-to-zoom feel like examining a print rather than driving a
        // camera.
        session.apply(Command::ZoomTo { scale, anchor });
        gtk::glib::Propagation::Proceed
    });

    Ok(())
}

/// The monitor's ICC profile, as the desktop advertises it.
///
/// X11 has carried this since the ICC Profiles in X specification: the colour
/// manager — colord under GNOME, here — writes the profile bytes onto the root
/// window as `_ICC_PROFILE`, and every application is expected to read it. No
/// D-Bus, no colord dependency, and it works whatever set the property.
///
/// `Ok(None)` means the desktop is not managing colour, which is ordinary and
/// not an error: the renderer then does what it always did and assumes sRGB.
///
/// Only the first monitor's profile is read. `_ICC_PROFILE_AT_N` carries the
/// others, and picking between them needs to know which monitor the window is
/// actually on — a question worth answering when the app can be dragged between
/// two calibrated displays, and not before.
pub fn display_profile() -> Result<Option<Vec<u8>>> {
    use x11::xlib;

    // SAFETY: every call below is an ordinary Xlib read against a display we
    // open and close ourselves. The property may be absent, which is the
    // `format == 0` case rather than an error.
    unsafe {
        let display = xlib::XOpenDisplay(std::ptr::null());
        if display.is_null() {
            return Ok(None);
        }
        let atom = xlib::XInternAtom(
            display,
            c"_ICC_PROFILE".as_ptr() as *const _,
            xlib::True, // only if it already exists
        );
        if atom == 0 {
            xlib::XCloseDisplay(display);
            return Ok(None);
        }

        let root = xlib::XDefaultRootWindow(display);
        let mut kind: xlib::Atom = 0;
        let mut format: i32 = 0;
        let mut count: u64 = 0;
        let mut remaining: u64 = 0;
        let mut data: *mut u8 = std::ptr::null_mut();
        // 8 MB in 32-bit words is far more than any profile, and asking for a
        // fixed large amount avoids the two-call length dance.
        let status = xlib::XGetWindowProperty(
            display,
            root,
            atom,
            0,
            2 * 1024 * 1024,
            xlib::False,
            xlib::AnyPropertyType as xlib::Atom,
            &mut kind,
            &mut format,
            &mut count,
            &mut remaining,
            &mut data,
        );

        let profile = if status == xlib::Success as i32 && !data.is_null() && format == 8 {
            Some(std::slice::from_raw_parts(data, count as usize).to_vec())
        } else {
            None
        };
        if !data.is_null() {
            xlib::XFree(data.cast());
        }
        xlib::XCloseDisplay(display);
        Ok(profile)
    }
}
