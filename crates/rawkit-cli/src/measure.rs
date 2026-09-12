//! `rawkit measure` — what the light and the geometry in a photograph are.
//!
//! A diagnostic, and a deliberately dull one: it renders nothing, writes
//! nothing and decides nothing. It exists because the tone controls are about
//! to start deriving their behaviour from [`rawkit_engine::scene::SceneStats`],
//! and a number that drives a control has to be checkable against the
//! photograph it came from before anything is built on it.
//!
//! Reading the output: every figure is in **stops from mid-grey**, so 0.0 is a
//! grey card. A frame whose median is well below zero is low key, one above it
//! is high key, and the range between the two tails is what the tone map has to
//! fit onto a display that holds about eight stops.
//!
//! It needs no GPU. The guide is built on the CPU and the measurement is a
//! histogram over it, so this runs on a machine that cannot render at all —
//! which is also what makes it usable in CI.

use anyhow::{bail, Context, Result};
use rawkit_editstate::EditState;
use rawkit_engine::{normalise, BayerPhase, CameraProfile, Frame};
use std::path::Path;

pub fn measure(inputs: &[std::path::PathBuf], profile_path: Option<&Path>) -> Result<()> {
    // Loaded once for every file rather than per file: a profile is a property
    // of the camera, and reading it ten times to measure ten frames from the
    // same body is ten times the work for one answer.
    let supplied = match profile_path {
        Some(path) => {
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading profile {}", path.display()))?;
            Some(
                rawkit_engine::profile::dcp::parse(&bytes)
                    .with_context(|| format!("parsing profile {}", path.display()))?,
            )
        }
        None => None,
    };

    println!(
        "{:<16} {:>8} {:>8} {:>8} {:>9} {:>9} {:>9} {:>9}",
        "file", "black", "median", "white", "range", "clipped", "straighten", "keystone"
    );

    for input in inputs {
        let name = input
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| input.display().to_string());

        let raw = rawkit_decode::decode_file(input)
            .with_context(|| format!("decoding {}", input.display()))?;
        let Some(phase) = BayerPhase::from_cfa(raw.cfa) else {
            bail!("{name}: {:?} is not a Bayer sensor", raw.cfa);
        };

        let profile = match supplied.clone() {
            Some(p) => p,
            None => rawkit_engine::render::single_illuminant_profile(&raw.xyz_to_camera)
                .unwrap_or_else(|| {
                    CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY)
                }),
        };

        let mosaic = normalise(&raw);
        // The same guide the renderer builds, from the same function, with the
        // same clip level. Not an approximation of it — measuring something
        // adjacent to what the shader reads is how two numbers that are meant
        // to be one number come to disagree.
        let guide = rawkit_engine::guide::Guide::build(&mosaic, raw.width, raw.height, phase, 1.0);
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
            clip_level: 1.0,
            profile,
            recorded_orientation: raw.orientation,
        };

        // The default edit, because the measurement is of the *scene* and the
        // default is the only state that adds nothing to it. A calibration or a
        // white balance the user typed would move the answer, which is correct
        // — but then the number would describe an edit, not a photograph.
        // Reported beside the light because both are measurements of the same
        // photograph and both are checked the same way: against the picture.
        let state = EditState::default();
        let upright = match frame.upright(&state, &guide) {
            Some(u) => format!("{:>+8.1}° {:>+9.3}", u.angle_deg, u.vertical),
            None => format!("{:>9} {:>9}", "—", "—"),
        };
        match frame.scene(&state, &guide) {
            Some(s) => println!(
                "{:<16} {:>+8.2} {:>+8.2} {:>+8.2} {:>7.2} EV {:>8.2}% {upright}",
                name,
                s.black_ev,
                s.median_ev,
                s.white_ev,
                s.dynamic_range(),
                s.clipped * 100.0
            ),
            None => println!("{name:<16} {:>8} {:>44} {upright}", "flat", ""),
        }
    }

    Ok(())
}
