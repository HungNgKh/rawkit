//! The invariant, as a test.
//!
//! > Same RAW + same `EditState` → same pixels on Linux, Windows and macOS.
//!
//! Everything else in the project is arranged to make that true; this is where
//! it gets checked. The references in `golden/refs` are rendered once, reviewed,
//! and committed — they are *our* reference, not Adobe's, because parity with
//! another renderer was retired as a goal.
//!
//! # Why the input is synthetic
//!
//! A golden test needs the same input everywhere, and RAW fixtures are large and
//! not redistributable, so a hosted runner cannot have one. The frames here are
//! generated from a formula: identical on every machine, in the repository at
//! zero bytes, and — because the pattern sweeps past the sampling limit — harder
//! on the demosaic than most photographs.
//!
//! That does leave a gap worth stating: this proves the *engine* agrees across
//! platforms, not the *decoder*. LibRaw producing different pixels on Windows
//! would slip past. Closing that needs a fixture on each runner, and is a
//! separate problem from this one.
//!
//! # Bless workflow
//!
//! `RAWKIT_BLESS=1 cargo test -p rawkit-engine --test golden -- --ignored`
//! writes the references instead of comparing. Blessing is a deliberate act:
//! the diff is the review, and a reference that changes without anyone looking
//! at the picture is not a reference.

use rawkit_editstate::EditState;
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};
use std::path::PathBuf;

const N: u32 = 96;

/// How far a channel may differ from the reference, in 16-bit units.
///
/// **Measured, not guessed.** The first CI run across three platforms, against
/// references blessed on Linux/Vulkan/AMD RADV:
///
/// | Platform | Backend | Adapter | Worst |
/// |---|---|---|---|
/// | macOS | Metal | Apple Paravirtual | 1 |
/// | Windows | Dx12 | Microsoft Basic Render Driver (WARP, software) | 1 |
///
/// So the expectation that these kernels — add, multiply, divide, min/max and
/// mix, no transcendentals, no fast-math — would be *bit*-identical across
/// vendors was wrong: the hashes differ. They agree to one part in 65535, which
/// is last-bit rounding and about four orders of magnitude below visible.
///
/// 8 leaves eight times the observed headroom while staying sensitive enough to
/// catch a real algorithmic difference. Loosening it is a reviewable change and
/// wants a reason recorded here; the printed hashes and worst-difference lines
/// are what such a reason would be built from.
const TOLERANCE: u16 = 8;

struct Case {
    name: &'static str,
    phase: BayerPhase,
    state: fn() -> EditState,
}

const CASES: &[Case] = &[
    // The baseline: what the camera saw, with the identity edit.
    Case {
        name: "chirp_rggb_identity",
        phase: BayerPhase::Rggb,
        state: EditState::default,
    },
    // A different CFA phase must survive the whole pipeline, not just the
    // demosaic — the phase offset reaches into the packed helper buffers.
    Case {
        name: "chirp_bggr_identity",
        phase: BayerPhase::Bggr,
        state: EditState::default,
    },
    // An edit that actually changes the pixels, so the reference would notice
    // if EditState stopped being wired to the render.
    Case {
        name: "chirp_rggb_plus_one_ev",
        phase: BayerPhase::Rggb,
        state: || {
            let mut s = EditState::default();
            s.tone.exposure_ev = 1.0;
            s
        },
    },
];

/// A deterministic mosaic: a radial chirp whose frequency rises towards the
/// corners, carried by smoothly varying chroma.
///
/// Written as an exact formula with no randomness and no floating-point
/// accumulation across pixels, so every platform generates the identical input.
/// If the input differed, the test would be measuring the generator.
fn synthetic_mosaic(phase: BayerPhase) -> Vec<f32> {
    let (dx, dy) = match phase {
        BayerPhase::Rggb => (0u32, 0u32),
        BayerPhase::Bggr => (1, 1),
        BayerPhase::Grbg => (1, 0),
        BayerPhase::Gbrg => (0, 1),
    };
    let mut out = Vec::with_capacity((N * N) as usize);
    for y in 0..N {
        for x in 0..N {
            let fx = x as f32 / N as f32 - 0.5;
            let fy = y as f32 / N as f32 - 0.5;
            let lum = 0.35 + 0.3 * (150.0 * (fx * fx + fy * fy)).sin();
            let rgb = [
                lum * (0.9 + 0.4 * (x as f32 / N as f32)),
                lum,
                lum * (1.2 - 0.4 * (y as f32 / N as f32)),
            ];
            let px = x + dx;
            let py = y + dy;
            let channel = if (px + py) % 2 == 1 {
                1
            } else if py % 2 == 0 {
                0
            } else {
                2
            };
            out.push(rgb[channel]);
        }
    }
    out
}

fn refs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../golden/refs")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../golden/refs"))
}

fn to_16bit(pixels: &[f32]) -> Vec<u16> {
    // RGB only: alpha is always 1.0 and storing it would be a third of the file
    // spent on a constant.
    pixels
        .chunks_exact(4)
        .flat_map(|p| {
            [
                (p[0].clamp(0.0, 1.0) * 65535.0).round() as u16,
                (p[1].clamp(0.0, 1.0) * 65535.0).round() as u16,
                (p[2].clamp(0.0, 1.0) * 65535.0).round() as u16,
            ]
        })
        .collect()
}

fn write_png(path: &PathBuf, data: &[u16]) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("create refs dir");
    let file = std::fs::File::create(path).expect("create reference");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), N, N);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Sixteen);
    let mut writer = encoder.write_header().expect("png header");
    let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_be_bytes()).collect();
    writer.write_image_data(&bytes).expect("png data");
}

fn read_png(path: &PathBuf) -> Option<Vec<u16>> {
    let file = std::fs::File::open(path).ok()?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    Some(
        buf[..info.buffer_size()]
            .chunks_exact(2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .collect(),
    )
}

#[test]
#[ignore = "requires a GPU adapter"]
fn renders_match_the_committed_references() {
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);
    let backend = format!("{:?}", gpu.adapter_info.backend);
    let adapter = gpu.adapter_info.name.clone();
    // Printed on every run so that a divergence report never has to ask which
    // machine produced which pixels. Three CI logs answer "are these platforms
    // bit-identical?" without anyone building tooling for it.
    println!("platform: {} · {backend} · {adapter}", std::env::consts::OS);

    let bless = std::env::var("RAWKIT_BLESS").is_ok_and(|v| v == "1");
    let mut failures = Vec::new();
    let mut blessed = Vec::new();

    for case in CASES {
        let cfa = synthetic_mosaic(case.phase);
        let rendered = renderer
            .run(
                &gpu,
                &Frame {
                    data: &cfa,
                    width: N,
                    height: N,
                    phase: case.phase,
                    as_shot_wb: [1.9, 1.0, 1.4],
                    clip_level: 1.0,
                    // A real camera's matrix rather than the identity, so a
                    // reference would catch the profile stage silently becoming
                    // a no-op. These are the decoder's numbers for an
                    // ILCE-6400: XYZ to camera, which is the direction a
                    // profile stores.
                    profile: CameraProfile::from_color_matrix([
                        [0.6941, -0.2164, -0.0644],
                        [-0.3850, 1.1349, 0.2779],
                        [-0.0031, 0.1055, 0.6511],
                    ]),
                },
                &(case.state)(),
                Output::Display,
            )
            .expect("render failed");

        let actual = to_16bit(&rendered);
        let digest = blake3::hash(bytemuck::cast_slice(&actual));
        println!("  {}: {}", case.name, &digest.to_hex()[..16]);

        let path = refs_dir().join(format!("{}.png", case.name));
        match read_png(&path) {
            _ if bless => {
                write_png(&path, &actual);
                blessed.push(case.name);
            }
            None => failures.push(format!(
                "{}: no reference at {} — run with RAWKIT_BLESS=1 to create it, \
                 then look at the image before committing it",
                case.name,
                path.display()
            )),
            Some(expected) if expected.len() != actual.len() => failures.push(format!(
                "{}: reference is {} samples, render is {}",
                case.name,
                expected.len(),
                actual.len()
            )),
            Some(expected) => {
                let mut worst = 0u16;
                let mut worst_at = 0usize;
                for (i, (a, b)) in actual.iter().zip(&expected).enumerate() {
                    let d = a.abs_diff(*b);
                    if d > worst {
                        worst = d;
                        worst_at = i;
                    }
                }
                println!("    worst difference: {worst} (tolerance {TOLERANCE})");
                if worst > TOLERANCE {
                    let px = worst_at / 3;
                    failures.push(format!(
                        "{}: diverged by {worst} at pixel ({}, {}) channel {} \
                         on {} / {backend} / {adapter} — tolerance is {TOLERANCE}",
                        case.name,
                        px as u32 % N,
                        px as u32 / N,
                        worst_at % 3,
                        std::env::consts::OS,
                    ));
                }
            }
        }
    }

    assert!(
        !bless,
        "blessed {} reference(s): {blessed:?} — inspect them, then commit",
        blessed.len()
    );
    assert!(
        failures.is_empty(),
        "golden renders diverged:\n{}",
        failures.join("\n")
    );
}
