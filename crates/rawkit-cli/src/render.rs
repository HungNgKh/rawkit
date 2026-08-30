//! `rawkit render` — a RAW file to a viewable image, end to end.
//!
//! # What this is, and what it is not
//!
//! It is the first time the whole chain runs on real sensor data: decode →
//! normalise → RCD demosaic → white balance → camera matrix → sRGB. If a photo
//! comes out of this looking like the photo, then stages A through E of the
//! pipeline are approximately right, which is the thing worth knowing.
//!
//! It is **not** colour management, and the difference matters enough to name:
//!
//! - The camera matrix comes from the decoder's built-in table, not from a DCP
//!   profile. There is no forward matrix, no HSL look table and no synthesised
//!   tone curve, so hue and saturation are approximate.
//! - The tone map is a fixed sigmoid that rolls highlights off and pins
//!   mid-grey. It is the roll-off, not a look — a curve that *feels* like a
//!   photograph is a taste problem with its own iteration loop.
//! - Output *is* colour managed: `rawkit-export` converts through Little CMS
//!   and embeds the profile, so a `.jpg` or `.png` written here says what its
//!   numbers mean. A `.ppm` does not, because the format cannot.
//!
//! Those three are separate P0 items. Until they land this command is a
//! diagnostic, and the PPM it writes should not be judged as a render.

use anyhow::{bail, Context, Result};
use rawkit_editstate::EditState;
use rawkit_engine::{normalise, BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};
use std::path::Path;

pub fn render(
    input: &Path,
    output: &Path,
    max_dim: u32,
    tile: u32,
    profile_path: Option<&Path>,
    state: &EditState,
) -> Result<()> {
    let raw = rawkit_decode::decode_file(input)
        .with_context(|| format!("decoding {}", input.display()))?;
    eprintln!(
        "{} {} · {}x{} · {:?} · black {:?} · white {}",
        raw.camera.make,
        raw.camera.model,
        raw.width,
        raw.height,
        raw.cfa,
        raw.levels.black,
        raw.levels.white
    );

    let Some(phase) = BayerPhase::from_cfa(raw.cfa) else {
        bail!(
            "{:?} is not a Bayer sensor; RCD cannot demosaic it",
            raw.cfa
        );
    };

    let g = raw.as_shot_neutral[1];
    if g > 0.0 {
        eprintln!(
            "as-shot wb : [{:.3}, 1.0, {:.3}]",
            raw.as_shot_neutral[0] / g,
            raw.as_shot_neutral[2] / g
        );
    }

    let profile = match profile_path {
        Some(path) => {
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading profile {}", path.display()))?;
            let profile = rawkit_engine::profile::dcp::parse(&bytes)
                .with_context(|| format!("parsing profile {}", path.display()))?;
            eprintln!(
                "colour     : {} ({}, {})",
                profile.name.as_deref().unwrap_or("unnamed profile"),
                if profile.is_dual_illuminant() {
                    "two illuminants"
                } else {
                    "one illuminant"
                },
                if profile.has_forward_matrix() {
                    "forward matrix"
                } else {
                    "colour matrix only"
                },
            );
            match profile.hue_sat_map(5000.0) {
                Some(map) => eprintln!(
                    "hue/sat    : {}x{}x{} correction table",
                    map.hue_divisions, map.sat_divisions, map.value_divisions
                ),
                None => eprintln!("hue/sat    : none (matrix correction only)"),
            }
            profile
        }
        None => match rawkit_engine::render::single_illuminant_profile(&raw.xyz_to_camera) {
            Some(p) => {
                eprintln!("colour     : decoder camera matrix, single illuminant (no DCP)");
                p
            }
            None => {
                eprintln!(
                    "colour     : NONE — no camera matrix for this body; \
                     the image will be strongly cast"
                );
                CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY)
            }
        },
    };

    let gpu = Gpu::new()?;
    eprintln!(
        "gpu        : {} ({:?})",
        gpu.adapter_info.name, gpu.adapter_info.backend
    );

    let mosaic = normalise(&raw);
    // How much of the sensor's range this exposure actually used. Worth
    // printing because a dark render has two very different causes — a dark
    // scene, or a scaling bug in decode — and they look identical in the image.
    // 1.0 means a channel reached the white level.
    {
        let mut sorted = mosaic.to_vec();
        sorted.sort_by(f32::total_cmp);
        let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q) as usize];
        eprintln!(
            "signal     : median {:.3}, p99 {:.3}, max {:.3} of white level ({:+.1} EV from clipping)",
            at(0.5),
            at(0.99),
            at(1.0),
            at(1.0).max(1e-6).log2(),
        );
    }
    // As-shot white balance and whatever exposure the caller asked for.
    // Everything else the renderer does comes from the frame itself, which is
    // what makes the default a baseline worth looking at.
    let renderer = Renderer::with_tile_size(&gpu, tile);
    let frame = Frame {
        data: &mosaic,
        width: raw.width,
        height: raw.height,
        phase,
        as_shot_wb: [
            raw.as_shot_neutral[0],
            raw.as_shot_neutral[1],
            raw.as_shot_neutral[2],
        ],
        // `normalise` puts the decoder's white level at 1.0.
        clip_level: 1.0,
        profile,
    };
    let (temperature, tint) = frame.as_shot_temperature();
    eprintln!("as-shot    : {temperature:.0} K, tint {tint:+.0}");
    let developed = renderer.run(&gpu, &frame, state, Output::Display)?;
    if developed.width != raw.width || developed.height != raw.height {
        eprintln!(
            "geometry   : {}x{} after orientation and crop",
            developed.width, developed.height
        );
    }

    let step = rawkit_engine::resize::downsample_step(developed.width, developed.height, max_dim);
    let (scaled, out_w, out_h) = rawkit_engine::resize::downsample(
        &developed.pixels,
        developed.width,
        developed.height,
        step,
    );
    write_image(output, &scaled, out_w, out_h)?;
    eprintln!("wrote      : {} ({out_w}x{out_h})", output.display());
    Ok(())
}

/// The sRGB transfer function. Not a tone curve — see the module header.
fn encode_srgb(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// Write the file, choosing the format from the name the user gave it.
///
/// PPM stays available and stays *unmanaged*: the format has nowhere to put a
/// profile, so it is for looking at intermediate results rather than for
/// anything that leaves this machine. Everything else goes through the export
/// crate and carries its profile.
fn write_image(path: &Path, rgba: &[f32], width: u32, height: u32) -> Result<()> {
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();

    if extension.eq_ignore_ascii_case("ppm") {
        let mut buf = format!("P6\n{width} {height}\n255\n").into_bytes();
        for pixel in rgba.chunks_exact(4) {
            for c in &pixel[..3] {
                buf.push((encode_srgb(*c) * 255.0).round() as u8);
            }
        }
        std::fs::write(path, buf).with_context(|| format!("writing {}", path.display()))?;
        eprintln!("note       : PPM carries no profile; use .jpg or .png to export");
        return Ok(());
    }

    let format = rawkit_export::Format::from_extension(&extension).ok_or_else(|| {
        anyhow::anyhow!(
            "do not know how to write {:?}; try .jpg, .png or .ppm",
            extension
        )
    })?;
    let bytes = rawkit_export::encode(rgba, width, height, format)?;
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
