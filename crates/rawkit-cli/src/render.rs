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
//! - The camera matrix here comes from the decoder's built-in table, not from a
//!   DCP profile. There is no forward matrix, no HSL look table and no
//!   synthesised tone curve, so hue and saturation are approximate.
//! - There is no tone map. The transfer function below is the plain sRGB curve,
//!   which is not a rendering intent — highlights will feel abrupt compared to
//!   any real editor, because nothing is rolling them off.
//! - There is no output ICC transform. The result is sRGB by assertion.
//!
//! Those three are separate P0 items. Until they land this command is a
//! diagnostic, and the PPM it writes should not be judged as a render.

use anyhow::{bail, Context, Result};
use rawkit_engine::{normalise, BayerPhase, Demosaic, Gpu, Mosaic};
use std::path::Path;

/// sRGB primaries to CIE XYZ (D65). The decoder's camera matrix is the other
/// direction — XYZ to camera — so the two get composed and inverted below.
const XYZ_RGB: [[f32; 3]; 3] = [
    [0.412453, 0.357580, 0.180423],
    [0.212671, 0.715160, 0.072169],
    [0.019334, 0.119193, 0.950227],
];

pub fn render(input: &Path, output: &Path, max_dim: u32) -> Result<()> {
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

    // As-shot multipliers, green-referenced.
    let g = raw.as_shot_neutral[1];
    if g <= 0.0 {
        bail!("as-shot white balance has no green multiplier");
    }
    let wb = [raw.as_shot_neutral[0] / g, 1.0, raw.as_shot_neutral[2] / g];
    eprintln!("as-shot wb : [{:.3}, 1.0, {:.3}]", wb[0], wb[2]);

    let gpu = Gpu::new()?;
    eprintln!(
        "gpu        : {} ({:?})",
        gpu.adapter_info.name, gpu.adapter_info.backend
    );

    let mosaic = normalise(&raw);
    let demosaic = Demosaic::new(&gpu);
    let rgba = demosaic.run(
        &gpu,
        &Mosaic {
            data: &mosaic,
            width: raw.width,
            height: raw.height,
            phase,
            wb,
        },
    )?;

    let matrix = camera_to_srgb(&raw.cam_to_xyz);
    match matrix {
        Some(_) => eprintln!("colour     : camera matrix from the decoder table (no DCP)"),
        None => eprintln!(
            "colour     : NONE — no camera matrix for this body; \
             the image will be strongly cast"
        ),
    }

    let step = downsample_step(raw.width, raw.height, max_dim);
    write_ppm(output, &rgba, raw.width, raw.height, step, wb, matrix)?;
    eprintln!(
        "wrote      : {} ({}x{})",
        output.display(),
        raw.width / step,
        raw.height / step
    );
    Ok(())
}

/// Compose the decoder's XYZ-to-camera matrix with sRGB's primaries and invert,
/// giving camera to sRGB.
///
/// The row normalisation is what makes a neutral subject come out neutral: it
/// forces each camera channel's row to sum to one, so equal camera RGB (after
/// white balance) maps to equal sRGB. Skip it and every image carries a cast
/// that looks like a white-balance error and is not one.
fn camera_to_srgb(cam_xyz: &[[f32; 3]; 4]) -> Option<[[f32; 3]; 3]> {
    if cam_xyz.iter().flatten().all(|&v| v == 0.0) {
        return None;
    }

    let mut cam_rgb = [[0.0f32; 3]; 3];
    for (i, row) in cam_rgb.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = (0..3).map(|k| cam_xyz[i][k] * XYZ_RGB[k][j]).sum();
        }
        let sum: f32 = row.iter().sum();
        if sum.abs() < 1e-6 {
            return None;
        }
        for cell in row.iter_mut() {
            *cell /= sum;
        }
    }
    invert3(&cam_rgb)
}

fn invert3(m: &[[f32; 3]; 3]) -> Option<[[f32; 3]; 3]> {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if det.abs() < 1e-9 {
        return None;
    }
    let inv_det = 1.0 / det;
    Some([
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv_det,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv_det,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv_det,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv_det,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv_det,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv_det,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv_det,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv_det,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv_det,
        ],
    ])
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

#[allow(clippy::too_many_arguments)]
fn write_ppm(
    path: &Path,
    rgba: &[f32],
    width: u32,
    height: u32,
    step: u32,
    wb: [f32; 3],
    matrix: Option<[[f32; 3]; 3]>,
) -> Result<()> {
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
            let linear = [acc[0] / n, acc[1] / n, acc[2] / n];

            // White balance, then camera to sRGB. The demosaic divides its own
            // internal multipliers back out, so this is the first place white
            // balance is actually applied to the image.
            let balanced = [linear[0] * wb[0], linear[1] * wb[1], linear[2] * wb[2]];
            let srgb = match matrix {
                Some(m) => [
                    m[0][0] * balanced[0] + m[0][1] * balanced[1] + m[0][2] * balanced[2],
                    m[1][0] * balanced[0] + m[1][1] * balanced[1] + m[1][2] * balanced[2],
                    m[2][0] * balanced[0] + m[2][1] * balanced[1] + m[2][2] * balanced[2],
                ],
                None => balanced,
            };
            for c in srgb {
                buf.push((encode_srgb(c) * 255.0).round() as u8);
            }
        }
    }

    std::fs::write(path, buf).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
