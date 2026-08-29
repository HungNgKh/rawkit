//! The render engine — `EditState` in, pixels out.
//!
//! # The invariant this crate exists to protect
//!
//! > Same RAW + same `EditState` → same pixels on Linux, Windows and macOS.
//!
//! That is the whole reason there is one engine rather than a per-platform
//! best-of-breed. wgpu abstracts Vulkan, DX12 and Metal and the WGSL is shared
//! verbatim, so the risk is not portability but *divergence* — which is why the
//! golden tests run on all three in CI rather than on the dev box.
//!
//! It is also why an Apple-only render path (CIRAWFilter) is rejected: it would
//! split the product's look by operating system.
//!
//! # Two structural decisions
//!
//! - **The core is scene-linear.** Display-referred sliders parameterise ops
//!   that run *before* the tone map; exposure is a true stop adjustment applied
//!   in linear light, which is what makes it a single scalar multiply.
//! - **Preview and export share kernels, not code paths.** Same WGSL, different
//!   resolution and tiling. Tile-based rendering is architecture, not
//!   optimisation — retrofitting it is a rewrite.

use rawkit_editstate::EditState;

pub mod pipeline;
pub mod present;
pub mod preview;
pub mod profile;
pub mod render;
mod tone;

pub use pipeline::{Domain, Stage};
pub use present::Presenter;
pub use preview::{Cell, PreviewBlit, PreviewImage};
pub use profile::CameraProfile;
pub use render::{
    normalise, BayerPhase, Canvas, Frame, Output, Pyramid, Renderer, TileBuffers, CANVAS_FORMAT,
};

/// Failures the engine can produce that are not programmer error.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("no GPU adapter available — a Vulkan, DX12 or Metal device is required")]
    NoAdapter,
    #[error("GPU device request refused: {0}")]
    DeviceRequest(String),
    /// An edit the renderer understands but cannot honour yet. Distinct from a
    /// malformed edit: the parameters are valid, the code is missing.
    #[error("not implemented: {0}")]
    Unsupported(&'static str),
    /// A buffer whose length does not match the geometry it claims to have.
    /// Caught at the boundary rather than as a garbled texture.
    #[error("{0}")]
    WrongSize(String),
    #[error(transparent)]
    EditState(#[from] rawkit_editstate::EditStateError),
}

/// An initialised GPU device and queue.
///
/// Held for the lifetime of the app: adapter enumeration is slow and doing it
/// per render would be visible as stutter.
pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub adapter_info: wgpu::AdapterInfo,
    /// Kept because a surface has to be configured against the adapter that
    /// will present to it, not merely against a device.
    pub adapter: wgpu::Adapter,
}

impl Gpu {
    /// Acquire a headless device — no surface, because export, thumbnailing and
    /// the golden tests all render without a window. The interactive canvas
    /// attaches a surface to this same device later rather than getting its own.
    pub fn new() -> Result<Self, EngineError> {
        pollster::block_on(Self::new_async())
    }

    async fn new_async() -> Result<Self, EngineError> {
        // `_from_env` honours WGPU_BACKEND / WGPU_ADAPTER_NAME, which is how CI
        // pins Linux to software Vulkan and how a divergence report gets
        // reproduced on a specific backend.
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
            })
            .await
            .map_err(|_| EngineError::NoAdapter)?;

        // WebGPU's **default** limits, deliberately, even though this adapter
        // would grant more. They cap a buffer at 256 MB and a storage binding at
        // 128 MB, which a 24 MP frame (388 MB as RGBA f32) cannot fit — so
        // asking for defaults is a standing assertion that nothing in the engine
        // sizes a buffer by the image.
        //
        // Requesting the adapter's limits instead would work on this machine and
        // silently produce a build that fails on a smaller one, which is the
        // divergence the whole engine exists to prevent. Everything is tiled;
        // tiling is what makes the floor sufficient.
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("rawkit engine"),
                ..Default::default()
            })
            .await
            .map_err(|e| EngineError::DeviceRequest(e.to_string()))?;

        Ok(Self {
            adapter_info: adapter.get_info(),
            adapter,
            device,
            queue,
        })
    }

    /// Acquire a device that can present to a window, plus the surface itself.
    ///
    /// The same device as [`Gpu::new`] in every respect that matters — same
    /// default limits, same everything — differing only in that the adapter is
    /// chosen for compatibility with this surface. An adapter that cannot
    /// present to the window it is meant to draw into is a real possibility on a
    /// multi-GPU laptop, and finding out at present time gives no useful error.
    ///
    /// The instance is created *without* a display handle, and the window
    /// supplies both handles when the surface is made. That is not a
    /// simplification — it is the only thing that works here.
    ///
    /// wgpu refuses to create a surface when the instance was given one display
    /// handle and the target reports another, and **Tauri returns a different
    /// Xlib display pointer on every call**: asking twice in a row gives two
    /// addresses, so it appears to open a fresh X connection each time. Passing
    /// a handle obtained from the window and then letting wgpu ask the same
    /// window again is therefore guaranteed to mismatch.
    ///
    /// Taking both handles from one call to the target sidesteps it entirely.
    /// X11 resource IDs are server-side and valid across connections, so a
    /// surface built this way is sound even if the connection differs from the
    /// one that created the window.
    pub fn with_surface<'w>(
        window: impl Into<wgpu::SurfaceTarget<'w>>,
    ) -> Result<(Self, wgpu::Surface<'w>), EngineError> {
        pollster::block_on(Self::with_surface_async(window))
    }

    async fn with_surface_async<'w>(
        window: impl Into<wgpu::SurfaceTarget<'w>>,
    ) -> Result<(Self, wgpu::Surface<'w>), EngineError> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance
            .create_surface(window)
            .map_err(|e| EngineError::DeviceRequest(e.to_string()))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
            })
            .await
            .map_err(|_| EngineError::NoAdapter)?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("rawkit engine"),
                ..Default::default()
            })
            .await
            .map_err(|e| EngineError::DeviceRequest(e.to_string()))?;

        Ok((
            Self {
                adapter_info: adapter.get_info(),
                adapter,
                device,
                queue,
            },
            surface,
        ))
    }
}

/// Exposure as the renderer applies it: a single scalar multiply in scene-linear
/// light, at [`Stage::SceneLinearOps`].
///
/// This is the payoff of the scene-linear core and the reason exposure is the
/// one tone control with a physical unit. It is a free function rather than a
/// method on `EditState` deliberately — `rawkit-editstate` describes *what* the
/// edit is and must not acquire opinions about how it is rendered.
pub fn exposure_multiplier(state: &EditState) -> f32 {
    state.tone.exposure_ev.exp2()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposure_is_stops_not_a_percentage() {
        let mut s = EditState::default();
        assert_eq!(exposure_multiplier(&s), 1.0, "identity edit must not scale");

        s.tone.exposure_ev = 1.0;
        assert_eq!(exposure_multiplier(&s), 2.0, "+1 EV is twice the light");

        s.tone.exposure_ev = -1.0;
        assert_eq!(exposure_multiplier(&s), 0.5);
    }

    /// Opt-in: hosted CI runners have no real GPU. Linux CI runs this against
    /// lavapipe (`cargo test -- --ignored`); a test that silently skips would be
    /// worse than one that is explicitly gated.
    #[test]
    #[ignore = "requires a GPU adapter"]
    fn a_gpu_device_can_be_acquired() {
        let gpu = Gpu::new().expect("no usable GPU adapter");
        println!(
            "adapter: {} ({:?}, {:?})",
            gpu.adapter_info.name, gpu.adapter_info.backend, gpu.adapter_info.device_type
        );
    }
}
