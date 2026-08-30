//! Does the RCD port actually demosaic?
//!
//! The obvious test — render an ARW and compare against dcraw or RawTherapee —
//! answers a weaker question than it looks. Those references apply their own
//! black level, white balance and tone handling, so a mismatch tells you
//! something differs without telling you what, and a match tells you the two
//! agree rather than that either is right.
//!
//! Synthetic ground truth avoids that entirely. Take a known RGB image, throw
//! away two thirds of it in a Bayer pattern, reconstruct, and compare against
//! the original. The error is then exactly the demosaic error, measurable in dB,
//! with nothing else in the path.
//!
//! Two things are asserted, and the second matters more than the first:
//!
//! 1. RCD beats a naive baseline by a wide margin. A demosaic that merely
//!    produces plausible-looking output can still be averaging across edges.
//! 2. All four Bayer phases score the same. The phase offset is the easiest part
//!    of the port to get subtly wrong, and a wrong phase still produces an image
//!    — a slightly soft, slightly fringed one that eyeballing would pass.
//!
//! GPU-gated like the rest: `cargo test -- --ignored`.

use rawkit_editstate::EditState;
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const W: u32 = 256;
const H: u32 = 256;
/// RCD reaches four pixels out and the kernel clamps at the edge, so the frame
/// is wrong by construction. Measure inside it.
const BORDER: u32 = 8;

/// A test image with the property that matters: luminance detail well past the
/// Nyquist limit of a single colour plane, carried by smoothly varying chroma.
///
/// That is what makes a demosaic test meaningful — it is the case where
/// green-guided reconstruction can win and channel-independent interpolation
/// cannot. Uncorrelated per-channel detail would defeat every algorithm equally
/// and prove nothing, and a smooth gradient would flatter all of them.
fn ground_truth() -> Vec<[f32; 3]> {
    let mut img = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        for x in 0..W {
            let fx = x as f32 / W as f32;
            let fy = y as f32 / H as f32;

            // Radial chirp: frequency rises towards the corner, sweeping through
            // and past the sampling limit.
            let dx = fx - 0.5;
            let dy = fy - 0.5;
            let r2 = dx * dx + dy * dy;
            let mut lum = 0.5 + 0.42 * (140.0 * r2).sin();

            // Hard edges, where directional interpolation either works or smears.
            if (0.08..0.24).contains(&fx) && (0.60..0.92).contains(&fy) {
                lum = 0.92;
            }
            if (0.30..0.42).contains(&fx) && (0.62..0.90).contains(&fy) {
                lum = 0.06;
            }

            // Chroma varies slowly across the frame, as it does in photographs.
            let r_gain = 0.85 + 0.30 * fx;
            let b_gain = 1.15 - 0.30 * fy;
            img.push([
                (lum * r_gain).clamp(0.0, 1.0),
                lum,
                (lum * b_gain).clamp(0.0, 1.0),
            ]);
        }
    }
    img
}

/// A frame with no colour opinions: identity matrix, neutral white balance.
///
/// The demosaic tests are about interpolation, so anything that would tint the
/// result is deliberately switched off — a PSNR that moved because the matrix
/// changed would be measuring the wrong thing.
fn frame<'a>(cfa: &'a [f32], w: u32, h: u32, phase: BayerPhase) -> Frame<'a> {
    Frame {
        data: cfa,
        width: w,
        height: h,
        phase,
        as_shot_wb: [1.0, 1.0, 1.0],
        clip_level: 1.0,
        profile: CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY),
        recorded_orientation: rawkit_editstate::Orientation::AsShot,
    }
}

fn colour_at(x: u32, y: u32, phase: BayerPhase) -> usize {
    let (dx, dy) = match phase {
        BayerPhase::Rggb => (0, 0),
        BayerPhase::Bggr => (1, 1),
        BayerPhase::Grbg => (1, 0),
        BayerPhase::Gbrg => (0, 1),
    };
    let px = x + dx;
    let py = y + dy;
    if (px + py) % 2 == 1 {
        1
    } else if py % 2 == 0 {
        0
    } else {
        2
    }
}

fn mosaic(img: &[[f32; 3]], phase: BayerPhase) -> Vec<f32> {
    (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| img[(y * W + x) as usize][colour_at(x, y, phase)])
        .collect()
}

/// The baseline: for each pixel and channel, average every sample of that colour
/// in the 3x3 neighbourhood. Channel-independent and direction-blind — which is
/// exactly the behaviour RCD has to beat to be worth porting.
fn naive(cfa: &[f32], phase: BayerPhase) -> Vec<[f32; 3]> {
    let mut out = vec![[0.0f32; 3]; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            let mut sum = [0.0f32; 3];
            let mut count = [0u32; 3];
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let nx = (x as i32 + dx).clamp(0, W as i32 - 1) as u32;
                    let ny = (y as i32 + dy).clamp(0, H as i32 - 1) as u32;
                    let c = colour_at(nx, ny, phase);
                    sum[c] += cfa[(ny * W + nx) as usize];
                    count[c] += 1;
                }
            }
            let px = &mut out[(y * W + x) as usize];
            for c in 0..3 {
                px[c] = if count[c] > 0 {
                    sum[c] / count[c] as f32
                } else {
                    0.0
                };
            }
        }
    }
    out
}

/// Peak signal-to-noise ratio over the interior, in dB. Higher is better; every
/// 6 dB is roughly one bit of accuracy.
fn psnr(truth: &[[f32; 3]], test: impl Fn(u32, u32) -> [f32; 3]) -> f64 {
    let mut sum_sq = 0.0f64;
    let mut n = 0u64;
    for y in BORDER..H - BORDER {
        for x in BORDER..W - BORDER {
            let t = truth[(y * W + x) as usize];
            let v = test(x, y);
            for c in 0..3 {
                let d = (t[c] - v[c]) as f64;
                sum_sq += d * d;
                n += 1;
            }
        }
    }
    let mse = sum_sq / n as f64;
    if mse <= 0.0 {
        return f64::INFINITY;
    }
    10.0 * (1.0 / mse).log10()
}

fn write_ppm(path: &str, pixels: impl Fn(u32, u32) -> [f32; 3]) {
    let mut buf = format!("P6\n{W} {H}\n255\n").into_bytes();
    for y in 0..H {
        for x in 0..W {
            for c in pixels(x, y) {
                buf.push((c.clamp(0.0, 1.0) * 255.0).round() as u8);
            }
        }
    }
    let _ = std::fs::create_dir_all("../../golden/out");
    let _ = std::fs::write(format!("../../golden/out/{path}"), buf);
}

#[test]
#[ignore = "requires a GPU adapter"]
fn rcd_reconstructs_better_than_a_direction_blind_baseline() {
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);
    let truth = ground_truth();
    let cfa = mosaic(&truth, BayerPhase::Rggb);

    let rgba = renderer
        .run(
            &gpu,
            &frame(&cfa, W, H, BayerPhase::Rggb),
            &EditState::default(),
            Output::SceneLinear,
        )
        .expect("demosaic failed")
        .pixels;
    assert_eq!(rgba.len(), (W * H * 4) as usize);

    let rcd_at = |x: u32, y: u32| {
        let i = ((y * W + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]]
    };
    let base = naive(&cfa, BayerPhase::Rggb);
    let base_at = |x: u32, y: u32| base[(y * W + x) as usize];

    let rcd_db = psnr(&truth, rcd_at);
    let base_db = psnr(&truth, base_at);
    println!(
        "RCD {rcd_db:.2} dB · baseline {base_db:.2} dB · gain {:.2} dB",
        rcd_db - base_db
    );

    write_ppm("rcd_truth.ppm", |x, y| truth[(y * W + x) as usize]);
    write_ppm("rcd_output.ppm", rcd_at);
    write_ppm("rcd_baseline.ppm", base_at);

    assert!(
        rcd_db.is_finite() && rcd_db > 24.0,
        "RCD reconstruction is not close to the original: {rcd_db:.2} dB"
    );
    assert!(
        rcd_db > base_db + 3.0,
        "RCD ({rcd_db:.2} dB) must clearly beat the direction-blind baseline \
         ({base_db:.2} dB); a smaller gain means the directional logic is not working"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn every_bayer_phase_reconstructs_equally_well() {
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);
    let truth = ground_truth();

    let phases = [
        BayerPhase::Rggb,
        BayerPhase::Bggr,
        BayerPhase::Grbg,
        BayerPhase::Gbrg,
    ];
    let scores: Vec<f64> = phases
        .iter()
        .map(|&phase| {
            let cfa = mosaic(&truth, phase);
            let rgba = renderer
                .run(
                    &gpu,
                    &frame(&cfa, W, H, phase),
                    &EditState::default(),
                    Output::SceneLinear,
                )
                .expect("demosaic failed")
                .pixels;
            let db = psnr(&truth, |x, y| {
                let i = ((y * W + x) * 4) as usize;
                [rgba[i], rgba[i + 1], rgba[i + 2]]
            });
            println!("{phase:?}: {db:.2} dB");
            db
        })
        .collect();

    let best = scores.iter().cloned().fold(f64::MIN, f64::max);
    let worst = scores.iter().cloned().fold(f64::MAX, f64::min);
    assert!(
        best - worst < 2.0,
        "Bayer phases disagree by {:.2} dB ({scores:?}) — the phase offset is wrong \
         for at least one layout",
        best - worst
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn tiling_does_not_change_a_single_pixel() {
    // The whole argument for tiling is that it is free: a tile boundary must not
    // be visible in the output. "Nearly identical" would not do — a faint seam
    // is exactly the artefact that survives review and shows up in a print. So
    // this asserts bit-equality between a 96-pixel tiling and one tile covering
    // the lot.
    //
    // 96 is chosen because it does *not* divide 256: the last tile in each
    // direction is partial, which is the case a neat power-of-two tiling never
    // exercises and where the interesting index bugs live.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let truth = ground_truth();
    let cfa = mosaic(&truth, BayerPhase::Rggb);
    let whole = Renderer::with_tile_size(&gpu, 512)
        .run(
            &gpu,
            &frame(&cfa, W, H, BayerPhase::Rggb),
            &EditState::default(),
            Output::SceneLinear,
        )
        .expect("single-tile render failed")
        .pixels;
    let tiled = Renderer::with_tile_size(&gpu, 96)
        .run(
            &gpu,
            &frame(&cfa, W, H, BayerPhase::Rggb),
            &EditState::default(),
            Output::SceneLinear,
        )
        .expect("tiled render failed")
        .pixels;

    assert_eq!(whole.len(), tiled.len());
    let mut worst = 0.0f32;
    let mut worst_at = (0u32, 0u32);
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            for c in 0..3 {
                let d = (whole[i + c] - tiled[i + c]).abs();
                if d > worst {
                    worst = d;
                    worst_at = (x, y);
                }
            }
        }
    }
    println!("worst tile-seam difference: {worst:e} at {worst_at:?}");
    assert_eq!(
        worst, 0.0,
        "tiling changed the image by {worst:e} at {worst_at:?}; \
         the halo is too small or the padded phase is wrong"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_full_frame_renders_within_webgpu_default_limits() {
    // The portability floor. A 24 MP frame as RGBA f32 is 388 MB, past WebGPU's
    // default 256 MB buffer cap — so before tiling, rendering one required
    // raising the device limits, which means a render that succeeds on one
    // machine and fails on another. Tiled, the buffers depend on tile size and
    // not on image size at all, and this proves it by rendering a
    // deliberately large frame on a device that asked for nothing special.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let limits = gpu.device.limits();
    let defaults = wgpu::Limits::default();
    assert!(
        limits.max_buffer_size <= defaults.max_buffer_size,
        "this test is meaningless unless the device is on default limits"
    );

    // 4000x3000 = 12 MP: 192 MB as RGBA f32, comfortably past the 128 MB
    // storage-binding cap that an untiled render would need.
    let (w, h) = (4000u32, 3000u32);
    let cfa = vec![0.25f32; (w * h) as usize];
    let out = Renderer::new(&gpu)
        .run(
            &gpu,
            &frame(&cfa, w, h, BayerPhase::Rggb),
            &EditState::default(),
            Output::SceneLinear,
        )
        .expect("a full frame must render on default limits")
        .pixels;

    assert_eq!(out.len(), (w * h * 4) as usize);
    // A flat mosaic must demosaic to a flat image; anything else means the
    // interpolation invented structure that was not there.
    let mid = ((h / 2 * w + w / 2) * 4) as usize;
    for c in 0..3 {
        assert!(
            (out[mid + c] - 0.25).abs() < 1e-4,
            "flat input produced {} in channel {c}",
            out[mid + c]
        );
    }
}
