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
//! Two routes existed. This binary tested the first before any UI was built on
//! it, because discovering it afterwards is a rewrite:
//!
//! 1. **Transparent cutout** — the webview paints chrome and leaves a hole; wgpu
//!    draws behind it. What RapidRAW ships, having first tried and abandoned
//!    JPEG-over-IPC as too slow. Known good on macOS and Windows.
//! 2. **Disjoint child webviews** — panels and canvas never overlap, so nothing
//!    needs transparency at all. Costs an unstable Tauri API.
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
//! Scope of the finding: X11. Wayland composites differently and is untested —
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

use anyhow::{anyhow, Result};
use rawkit_engine::Gpu;
use tauri::Manager;

/// Deliberately a colour no UI would ever use, so a screenshot can be checked by
/// value rather than by eye.
const PROBE_GREEN: wgpu::Color = wgpu::Color {
    r: 0.0,
    g: 1.0,
    b: 0.0,
    a: 1.0,
};

fn main() -> Result<()> {
    tauri::Builder::default()
        .setup(|app| {
            let window = app
                .get_webview_window("main")
                .ok_or_else(|| anyhow!("the config declares a window labelled 'main'"))?;
            // The surface is created here, on the main thread, because the raw
            // window handle comes from GTK on this platform and GTK is not
            // thread-safe. Drawing afterwards is fine from anywhere.
            let (gpu, surface) = Gpu::with_surface(window.clone())?;
            eprintln!(
                "gpu        : {} ({:?})",
                gpu.adapter_info.name, gpu.adapter_info.backend
            );

            let size = window.inner_size()?;
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
