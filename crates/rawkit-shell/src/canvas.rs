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
pub fn attach(vbox: &gtk::Box, panel_height: i32) -> Result<CanvasWindow> {
    let area = gtk::DrawingArea::new();
    area.set_size_request(-1, -1);
    area.set_vexpand(true);
    area.set_hexpand(true);
    vbox.pack_start(&area, true, true, 0);

    // The webview keeps a fixed strip; the canvas takes the rest. Whichever of
    // them wry packed first, both now have their own rectangle.
    for child in vbox.children() {
        if child != area {
            child.set_size_request(-1, panel_height);
            child.set_vexpand(false);
        }
    }
    vbox.show_all();

    // A widget has no GdkWindow until it is realized, and realizing is what
    // creates the X window this whole approach depends on.
    area.realize();
    let gdk_window = area
        .window()
        .ok_or_else(|| anyhow!("the canvas widget realized without an X window"))?;

    // Ask the plain GdkWindow for its display and screen *before* downcasting:
    // the X11 subclasses inherit the accessors but not the trait bounds that
    // make them callable.
    let x11_display: gdkx11::X11Display = gdk_window
        .display()
        .downcast()
        .map_err(|_| anyhow!("the canvas is not on an X11 display"))?;
    let x11_screen: gdkx11::X11Screen = gdk_window
        .screen()
        .downcast()
        .map_err(|_| anyhow!("the canvas is not on an X11 screen"))?;
    let x11_window: gdkx11::X11Window = gdk_window
        .downcast()
        .map_err(|_| anyhow!("the canvas is not on an X11 window"))?;

    // SAFETY: `x11_display` is a live GdkX11Display; the call returns the
    // connection GTK is already using rather than opening a new one, which is
    // precisely what makes this different from asking Tauri for a display
    // handle (that hands back a fresh pointer every time).
    let display =
        unsafe { gdkx11::ffi::gdk_x11_display_get_xdisplay(x11_display.to_glib_none().0) };

    Ok(CanvasWindow {
        window: NonZeroU32::new(x11_window.xid() as u32)
            .ok_or_else(|| anyhow!("the canvas widget has no X window id"))?,
        display: NonNull::new(display.cast())
            .ok_or_else(|| anyhow!("GTK reported a null X display"))?,
        screen: x11_screen.screen_number(),
    })
}
