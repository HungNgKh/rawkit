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

pub mod demosaic;
pub mod pipeline;

pub use demosaic::{normalise, BayerPhase, Demosaic, Mosaic};
pub use pipeline::{Domain, Stage};

/// Failures the engine can produce that are not programmer error.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("no GPU adapter available — a Vulkan, DX12 or Metal device is required")]
    NoAdapter,
    #[error("GPU device request refused: {0}")]
    DeviceRequest(String),
}

/// An initialised GPU device and queue.
///
/// Held for the lifetime of the app: adapter enumeration is slow and doing it
/// per render would be visible as stutter.
pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub adapter_info: wgpu::AdapterInfo,
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

        // Ask for what this adapter can actually do rather than WebGPU's
        // defaults, which cap a buffer at 256 MB and a storage binding at
        // 128 MB. A 24 MP frame as RGBA f32 is 388 MB, so the defaults cannot
        // hold one whole image — on any GPU, however large.
        //
        // Raising the limits makes the whole-frame path work today; it does not
        // make it right. Desktop GPUs differ in what they allow, so a render
        // that fits on this machine and not on someone else's would break the
        // one invariant the engine exists to hold. **The frame has to be tiled,
        // and this is the reason** — it is an architectural requirement that
        // happens to also be the 60fps requirement, not a memory optimisation.
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("rawkit engine"),
                required_limits: adapter.limits(),
                ..Default::default()
            })
            .await
            .map_err(|e| EngineError::DeviceRequest(e.to_string()))?;

        Ok(Self {
            adapter_info: adapter.get_info(),
            device,
            queue,
        })
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
