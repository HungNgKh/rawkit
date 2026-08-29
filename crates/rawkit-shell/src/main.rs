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
//! cargo run -p rawkit-shell &
//! xwd -name rawkit -out probe.xwd
//! python3 crates/rawkit-shell/probe/check-composite.py probe.xwd
//! ```
//!
//! wgpu clears the window to a green no interface would ever use, so the check
//! is by value rather than by eye. Take several captures a second apart.

#[cfg(target_os = "linux")]
mod canvas;

use anyhow::{anyhow, Result};
use rawkit_engine::Gpu;
use tauri::{
    webview::WebviewBuilder, window::WindowBuilder, LogicalPosition, LogicalSize, WebviewUrl,
    WebviewWindowBuilder,
};

/// Which arrangement to test. `RAWKIT_PROBE_ROUTE=1` for the cutout, anything
/// else for child webviews.
///
/// Route 1 stays in the code after being ruled out on Linux, because it is
/// reported to work on macOS and Windows and this is how that gets checked
/// there. A probe that only tests the arrangement you settled on cannot tell
/// you when the other one starts working.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Route {
    /// One full-window webview with a transparent hole, wgpu behind it.
    Cutout,
    /// A window with no webview of its own, wgpu across it, and the chrome as a
    /// child webview occupying its own rectangle. Nothing overlaps, so nothing
    /// needs to be transparent.
    ChildWebview,
    /// A native widget packed beside the webview, with the surface on *its* X
    /// window rather than the toplevel. Nothing is transparent and nothing
    /// overlaps, and X clips each window's output to its own rectangle.
    NativeChild,
}

/// Width of the chrome, in logical pixels. The canvas is everything to the
/// right of it.
const PANEL_WIDTH: f64 = 400.0;
/// Height the chrome keeps in route 3. `default_vbox` is a vertical box, and the
/// probe needs two disjoint rectangles rather than the final layout.
///
/// Linux-only, like route 3 itself: the equivalent on macOS and Windows will be
/// a child NSView and a child HWND, and those will bring their own geometry.
#[cfg(target_os = "linux")]
const PANEL_HEIGHT: i32 = 200;
const WINDOW: (f64, f64) = (1200.0, 800.0);

/// Deliberately a colour no UI would ever use, so a screenshot can be checked by
/// value rather than by eye.
const PROBE_GREEN: wgpu::Color = wgpu::Color {
    r: 0.0,
    g: 1.0,
    b: 0.0,
    a: 1.0,
};

fn main() -> Result<()> {
    let route = match std::env::var("RAWKIT_PROBE_ROUTE").as_deref() {
        Ok("1") => Route::Cutout,
        Ok("2") => Route::ChildWebview,
        _ => Route::NativeChild,
    };
    eprintln!("route      : {route:?}");

    tauri::Builder::default()
        .setup(move |app| {
            // The surface is created here, on the main thread, because the raw
            // window handle comes from GTK on this platform and GTK is not
            // thread-safe. Drawing afterwards is fine from anywhere.
            let (gpu, surface, size) = match route {
                Route::Cutout => {
                    let builder = WebviewWindowBuilder::new(
                        app,
                        "main",
                        WebviewUrl::App("index.html".into()),
                    )
                    .title("rawkit")
                    .inner_size(WINDOW.0, WINDOW.1);
                    // A third strike against route 1, found by CI rather than by
                    // the probe: on macOS `transparent` is behind Tauri's
                    // `macos-private-api` feature, and using a private API bars
                    // App Store distribution. So route 1 costs a distribution
                    // channel even on the platform where it works.
                    #[cfg(not(target_os = "macos"))]
                    let builder = builder.transparent(true);
                    let window = builder.build()?;
                    let size = window.inner_size()?;
                    let (gpu, surface) = Gpu::with_surface(window.clone())?;
                    (gpu, surface, size)
                }
                Route::ChildWebview => {
                    // A window with no webview of its own. The chrome becomes a
                    // child occupying the left strip, and the GPU gets the rest
                    // — or rather, gets the whole window and is expected to be
                    // clipped out of the child's rectangle by X.
                    let window = WindowBuilder::new(app, "main")
                        .title("rawkit")
                        .inner_size(WINDOW.0, WINDOW.1)
                        .build()?;
                    let panel = window.add_child(
                        WebviewBuilder::new("panel", WebviewUrl::App("panel.html".into())),
                        LogicalPosition::new(0.0, 0.0),
                        LogicalSize::new(PANEL_WIDTH, WINDOW.1),
                    )?;
                    // Restating the bounds is not belt and braces: the size
                    // given to `add_child` alone left the child filling the
                    // whole window, which would have made the probe answer a
                    // question nobody asked.
                    panel.set_auto_resize(false)?;
                    panel.set_position(LogicalPosition::new(0.0, 0.0))?;
                    panel.set_size(LogicalSize::new(PANEL_WIDTH, WINDOW.1))?;
                    eprintln!("panel      : {:?} (at creation)", panel.bounds()?);
                    // Try again once the page has loaded, in case the bounds
                    // are only honoured after the webview exists in earnest.
                    let later = panel.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                        let _ = later.set_auto_resize(false);
                        let _ = later.set_position(LogicalPosition::new(0.0, 0.0));
                        let _ = later.set_size(LogicalSize::new(PANEL_WIDTH, WINDOW.1));
                        eprintln!("panel      : {:?} (after load)", later.bounds());
                    });
                    let size = window.inner_size()?;
                    let (gpu, surface) = Gpu::with_surface(window.clone())?;
                    (gpu, surface, size)
                }
                #[cfg(target_os = "linux")]
                Route::NativeChild => {
                    // A normal Tauri window, webview and all — the difference is
                    // entirely in where the surface goes.
                    let window = WebviewWindowBuilder::new(
                        app,
                        "main",
                        WebviewUrl::App("panel.html".into()),
                    )
                    .title("rawkit")
                    .inner_size(WINDOW.0, WINDOW.1)
                    .build()?;
                    let canvas = canvas::attach(&window.default_vbox()?, PANEL_HEIGHT)?;
                    eprintln!("canvas     : X window {canvas:?}");
                    // The widget reports 2x2 at this point: it has an X window
                    // but GTK has not laid the window out yet. Size the surface
                    // from what the layout is *going* to be instead of waiting
                    // for an allocation — X clips the surface to the widget
                    // either way, and the probe is asking about clipping, not
                    // about resize handling.
                    let outer = window.inner_size()?;
                    let scale = window.scale_factor()?;
                    let panel = (PANEL_HEIGHT as f64 * scale) as u32;
                    let size = tauri::PhysicalSize::new(
                        outer.width,
                        outer.height.saturating_sub(panel).max(1),
                    );
                    let (gpu, surface) = Gpu::with_surface(canvas)?;
                    (gpu, surface, size)
                }
                #[cfg(not(target_os = "linux"))]
                Route::NativeChild => {
                    return Err(anyhow!(
                        "the native-child canvas is implemented for X11 only so far"
                    )
                    .into())
                }
            };
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

            // Set RAWKIT_PROBE_NO_GPU=1 to leave the surface alone. If the page
            // is red then, the webview works and the question is purely one of
            // ordering; if it is still blank, the page never loaded and the
            // compositing result would have been meaningless.
            if std::env::var_os("RAWKIT_PROBE_NO_GPU").is_some() {
                eprintln!("paint      : disabled (RAWKIT_PROBE_NO_GPU)");
                return Ok(());
            }
            std::thread::spawn(move || loop {
                if let Err(e) = paint(&gpu, &surface) {
                    eprintln!("paint: {e}");
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(33));
            });
            Ok(())
        })
        .run(tauri::generate_context!())?;
    Ok(())
}

/// Clear the whole surface and present. Repeated at ~30Hz rather than drawn
/// once: flicker is the reported failure mode on this platform, and a single
/// frame cannot show it.
fn paint(gpu: &Gpu, surface: &wgpu::Surface<'static>) -> Result<()> {
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
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("probe"),
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
    frame.present();
    Ok(())
}
