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
//! - There is no output ICC transform. The sRGB encode below stands in for it,
//!   so the result is sRGB by assertion rather than by conversion.
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
        None => match single_illuminant_profile(&raw.cam_to_xyz) {
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
    // The identity edit: no exposure change, as-shot white balance. Everything
    // the renderer does to this frame comes from the frame itself, which is what
    // makes it a baseline worth looking at.
    let state = EditState::default();
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
    let rgba = renderer.run(&gpu, &frame, &state, Output::Display)?;

    let step = downsample_step(raw.width, raw.height, max_dim);
    write_ppm(output, &rgba, raw.width, raw.height, step)?;
    eprintln!(
        "wrote      : {} ({}x{})",
        output.display(),
        raw.width / step,
        raw.height / step
    );
    Ok(())
}

/// Build a profile from the decoder's camera table.
///
/// The table gives one XYZ-to-camera matrix, which the profile treats as a D65
/// characterisation. That is a real limitation and not a placeholder to be
/// embarrassed about: a single illuminant means a tungsten scene is rendered
/// with a daylight characterisation, which is defensible but not accurate. Two
/// illuminants need a `.dcp`.
///
/// LibRaw pads its matrix to four rows for four-colour sensors; we take the
/// three that describe an RGB camera.
fn single_illuminant_profile(cam_xyz: &[[f32; 3]; 4]) -> Option<CameraProfile> {
    if cam_xyz.iter().flatten().all(|&v| v == 0.0) {
        return None;
    }
    Some(CameraProfile::from_color_matrix([
        cam_xyz[0], cam_xyz[1], cam_xyz[2],
    ]))
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

fn downsample_step(width: u32, height: u32, max_dim: u32) -> u32 {
    if max_dim == 0 {
        return 1;
    }
    let longest = width.max(height);
    (longest.div_ceil(max_dim)).max(1)
}

fn write_ppm(path: &Path, rgba: &[f32], width: u32, height: u32, step: u32) -> Result<()> {
    let out_w = width / step;
    let out_h = height / step;
    let mut buf = format!("P6\n{out_w} {out_h}\n255\n").into_bytes();

    for oy in 0..out_h {
        for ox in 0..out_w {
            // Box average over the source block. Averaging in linear light is
            // the only correct place to do it; downsampling after the transfer
            // function darkens detailed areas.
            let mut acc = [0.0f32; 3];
            let mut n = 0.0f32;
            for sy in 0..step {
                for sx in 0..step {
                    let x = ox * step + sx;
                    let y = oy * step + sy;
                    let i = ((y * width + x) * 4) as usize;
                    for c in 0..3 {
                        acc[c] += rgba[i + c];
                    }
                    n += 1.0;
                }
            }
            // The engine hands back display-referred *linear* values: white
            // balanced, profiled, exposed and tone mapped. All that is left is
            // the transfer function, which is the output transform's job and
            // lives here only until lcms2 does it properly.
            for c in acc {
                buf.push((encode_srgb(c / n) * 255.0).round() as u8);
            }
        }
    }

    std::fs::write(path, buf).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
