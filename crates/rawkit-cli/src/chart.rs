//! Measuring a colour chart, so a calibration can be set by a number rather
//! than by eye.
//!
//! # Why the reference values are not in this file
//!
//! A ColorChecker's patches have published L\*a\*b\* values, and there are two
//! sets of them: the chart was reformulated in 2014 and the older numbers are
//! wrong for a newer chart by more than the differences anybody is chasing here.
//! There is also more than one publisher, and they do not agree to the last
//! digit.
//!
//! So this reads them from a file the user supplies rather than shipping a table
//! that would be a number nobody in this project had checked. Every ΔE printed
//! below is against *your* reference, and if it is the wrong reference the tool
//! says so loudly by being wrong about the grey patches.
//!
//! # What ΔE means here, and what it does not
//!
//! **Lightness is matched, not measured.** A render has been through the tone
//! map, and a chart's reference lightness has not — comparing the two would
//! mostly measure the tone curve, which no calibration slider can change. So
//! each patch is compared at the reference's own L\*, and what comes out is the
//! hue and chroma error: precisely what the seven controls move, and precisely
//! what a calibration is for.

use anyhow::{bail, Context, Result};
use rawkit_editstate::EditState;
use rawkit_engine::calibrate::{delta_e_2000, xyz_to_lab, Matrix3};
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};
use std::path::Path;

/// The ColorChecker Classic's layout: six across, four down, dark skin first.
const COLUMNS: usize = 6;
const ROWS: usize = 4;
pub const PATCHES: usize = COLUMNS * ROWS;

/// How much of the gap between patch centres to average over.
///
/// A third, so the square sits well inside a patch even when the four corners
/// were placed by a person with a mouse. Sampling the whole patch would catch
/// its printed border; sampling one pixel would measure the sensor's noise.
const PATCH_FRACTION: f64 = 1.0 / 3.0;

/// Read 24 reference colours: one `L a b` per line, `#` to end of line ignored.
pub fn read_reference(path: &Path) -> Result<Vec<[f32; 3]>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the reference at {}", path.display()))?;
    let mut values = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<f32> = line
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<f32>())
            .collect::<Result<_, _>>()
            .with_context(|| format!("line {} is not three numbers: {line:?}", number + 1))?;
        if parts.len() != 3 {
            bail!("line {} has {} numbers, not three", number + 1, parts.len());
        }
        values.push([parts[0], parts[1], parts[2]]);
    }
    if values.len() != PATCHES {
        bail!(
            "the reference has {} patches; a ColorChecker Classic has {PATCHES}",
            values.len()
        );
    }
    Ok(values)
}

/// The four corners of the chart, as fractions of the frame, naming the
/// **centres** of the corner patches.
///
/// Centres rather than the chart's outer edge, because a centre is a thing a
/// person can put a pointer on exactly and an edge is a judgement about where
/// the printing stops.
#[derive(Debug, Clone, Copy)]
pub struct Corners {
    /// Top left, top right, bottom right, bottom left — as the chart appears in
    /// the photograph, so a chart shot upside down is described upside down.
    pub at: [[f64; 2]; 4],
}

impl Corners {
    /// Parse `x0,y0,x1,y1,x2,y2,x3,y3`.
    pub fn parse(text: &str) -> Result<Self> {
        let numbers: Vec<f64> = text
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<f64>())
            .collect::<Result<_, _>>()
            .context("the corners are eight numbers, x,y four times")?;
        if numbers.len() != 8 {
            bail!(
                "expected eight numbers for four corners, got {}",
                numbers.len()
            );
        }
        if numbers.iter().any(|v| !(0.0..=1.0).contains(v)) {
            bail!("the corners are fractions of the frame, so each runs 0 to 1");
        }
        Ok(Corners {
            at: [
                [numbers[0], numbers[1]],
                [numbers[2], numbers[3]],
                [numbers[4], numbers[5]],
                [numbers[6], numbers[7]],
            ],
        })
    }

    /// Where a patch's centre falls, bilinearly between the four corners.
    ///
    /// Bilinear and not projective: a chart photographed at an angle is a
    /// projection, and the difference over four rows is under a patch's width
    /// for any angle somebody would actually shoot a chart at. Shoot it square
    /// on — which is what you should be doing anyway, because a chart at an
    /// angle is a chart lit unevenly.
    pub fn patch(&self, index: usize) -> [f64; 2] {
        let (column, row) = (index % COLUMNS, index / COLUMNS);
        let u = column as f64 / (COLUMNS - 1) as f64;
        let v = row as f64 / (ROWS - 1) as f64;
        let mut out = [0.0f64; 2];
        for (axis, value) in out.iter_mut().enumerate() {
            let top = self.at[0][axis] * (1.0 - u) + self.at[1][axis] * u;
            let bottom = self.at[3][axis] * (1.0 - u) + self.at[2][axis] * u;
            *value = top * (1.0 - v) + bottom * v;
        }
        out
    }

    /// Half the side of the square to average, as a fraction of the frame.
    fn radius(&self) -> [f64; 2] {
        let step = |a: [f64; 2], b: [f64; 2], axis: usize| (b[axis] - a[axis]).abs();
        [
            step(self.patch(0), self.patch(1), 0).max(1e-4) * PATCH_FRACTION / 2.0,
            step(self.patch(0), self.patch(COLUMNS), 1).max(1e-4) * PATCH_FRACTION / 2.0,
        ]
    }
}

/// What one patch came out as, against what it should have been.
pub struct Measured {
    pub patch: usize,
    pub rendered: [f32; 3],
    pub reference: [f32; 3],
    pub delta_e: f32,
}

/// Render the photograph and measure every patch.
pub fn measure(
    input: &Path,
    profile_path: Option<&Path>,
    state: &EditState,
    corners: Corners,
    reference: &[[f32; 3]],
) -> Result<Vec<Measured>> {
    let raw = rawkit_decode::decode_file(input)
        .with_context(|| format!("decoding {}", input.display()))?;
    let Some(phase) = BayerPhase::from_cfa(raw.cfa) else {
        bail!(
            "{:?} is not a Bayer sensor; RCD cannot demosaic it",
            raw.cfa
        );
    };
    let profile = match profile_path {
        Some(path) => {
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading profile {}", path.display()))?;
            rawkit_engine::profile::dcp::parse(&bytes)
                .with_context(|| format!("parsing profile {}", path.display()))?
        }
        None => rawkit_engine::render::single_illuminant_profile(&raw.xyz_to_camera)
            .unwrap_or_else(|| CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY)),
    };
    let mosaic = rawkit_engine::normalise(&raw);
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
        // `normalise` puts the decoder's white level at 1.0, as `render` says.
        clip_level: 1.0,
        profile,
        recorded_orientation: raw.orientation,
    };

    let gpu = Gpu::new()?;
    let renderer = Renderer::new(&gpu);
    let rendered = renderer.run(&gpu, &frame, state, Output::Display)?;

    // Display-referred *linear* sRGB out of the renderer, so the way to XYZ is
    // the inverse of the matrix that got there — not the D65 constant beside it,
    // which is the mistake that put a cast on a grey when the same round trip
    // was written in the profile.
    let xyz_from_srgb: Matrix3 =
        rawkit_engine::calibrate::invert(&rawkit_engine::profile::SRGB_FROM_XYZ_D50)
            .context("the sRGB matrix has no inverse, which cannot happen")?;

    let radius = corners.radius();
    let (w, h) = (rendered.width as f64, rendered.height as f64);
    let mut out = Vec::with_capacity(PATCHES);
    for (patch, want) in reference.iter().enumerate().take(PATCHES) {
        let centre = corners.patch(patch);
        let x0 = (((centre[0] - radius[0]) * w).floor().max(0.0)) as u32;
        let x1 = (((centre[0] + radius[0]) * w).ceil().min(w - 1.0)) as u32;
        let y0 = (((centre[1] - radius[1]) * h).floor().max(0.0)) as u32;
        let y1 = (((centre[1] + radius[1]) * h).ceil().min(h - 1.0)) as u32;
        let mut total = [0.0f64; 3];
        let mut count = 0.0f64;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let at = ((y * rendered.width + x) * 4) as usize;
                for (channel, sum) in total.iter_mut().enumerate() {
                    *sum += rendered.pixels[at + channel] as f64;
                }
                count += 1.0;
            }
        }
        if count == 0.0 {
            bail!(
                "patch {} of the chart falls outside the photograph",
                patch + 1
            );
        }
        let srgb = [
            (total[0] / count) as f32,
            (total[1] / count) as f32,
            (total[2] / count) as f32,
        ];
        let xyz = [
            (0..3).map(|i| xyz_from_srgb[0][i] * srgb[i]).sum::<f32>(),
            (0..3).map(|i| xyz_from_srgb[1][i] * srgb[i]).sum::<f32>(),
            (0..3).map(|i| xyz_from_srgb[2][i] * srgb[i]).sum::<f32>(),
        ];
        let mut lab = xyz_to_lab(xyz);
        // Lightness matched to the reference: see the module note. What is left
        // is the hue and chroma error, which is what the calibration moves.
        lab[0] = want[0];
        out.push(Measured {
            patch,
            rendered: lab,
            reference: *want,
            delta_e: delta_e_2000(lab, *want),
        });
    }
    Ok(out)
}

/// Print the measurement, worst patches last so the summary is what stays on
/// screen.
pub fn report(measured: &[Measured]) {
    println!("patch     ΔE2000   rendered a*b*        reference a*b*");
    for m in measured {
        println!(
            "{:>3}       {:>6.2}   {:>7.2} {:>7.2}     {:>7.2} {:>7.2}",
            m.patch + 1,
            m.delta_e,
            m.rendered[1],
            m.rendered[2],
            m.reference[1],
            m.reference[2],
        );
    }
    let mean = measured.iter().map(|m| m.delta_e as f64).sum::<f64>() / measured.len() as f64;
    let worst = measured
        .iter()
        .max_by(|a, b| a.delta_e.total_cmp(&b.delta_e))
        .expect("a chart has patches");
    println!();
    println!("mean ΔE2000 : {mean:.2}  (hue and chroma; lightness is matched)");
    println!(
        "worst       : {:.2} at patch {}",
        worst.delta_e,
        worst.patch + 1
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_corner_patches_are_the_corners() {
        // The grid is defined by what the user points at, so the four they
        // pointed at have to come back unmoved — an off-by-one in the
        // interpolation would put every patch half a cell out and the error
        // would look like the camera being wrong.
        let corners = Corners::parse("0.1,0.2 0.9,0.2 0.9,0.8 0.1,0.8").unwrap();
        assert_eq!(corners.patch(0), [0.1, 0.2]);
        assert_eq!(corners.patch(COLUMNS - 1), [0.9, 0.2]);
        assert_eq!(corners.patch(PATCHES - 1), [0.9, 0.8]);
        assert_eq!(corners.patch(PATCHES - COLUMNS), [0.1, 0.8]);
        // And the middle of the top row is the middle.
        let mid = corners.patch(2);
        assert!(
            (mid[0] - 0.42).abs() < 1e-9 && (mid[1] - 0.2).abs() < 1e-9,
            "{mid:?}"
        );
    }

    #[test]
    fn a_chart_shot_at_an_angle_still_has_a_grid() {
        // Bilinear between four corners, so a chart that leans still lands its
        // patches on the patches. Checked by asking that every centre stays
        // inside the quadrilateral its neighbours describe.
        let corners = Corners::parse("0.12,0.18 0.88,0.24 0.86,0.79 0.14,0.74").unwrap();
        for patch in 0..PATCHES {
            let [x, y] = corners.patch(patch);
            assert!(
                (0.1..=0.9).contains(&x) && (0.15..=0.8).contains(&y),
                "{patch}: {x},{y}"
            );
        }
    }

    #[test]
    fn corners_outside_the_frame_are_refused() {
        assert!(Corners::parse("0.1,0.2 1.4,0.2 0.9,0.8 0.1,0.8").is_err());
        assert!(Corners::parse("0.1,0.2 0.9,0.2 0.9,0.8").is_err());
        assert!(Corners::parse("nonsense").is_err());
    }

    #[test]
    fn a_reference_must_have_a_chart_s_worth_of_patches() {
        // The failure this prevents is the quiet one: a file with twenty-three
        // usable lines would measure twenty-three patches against the wrong
        // twenty-three references and print plausible numbers throughout.
        let dir = std::env::temp_dir().join(format!("rawkit-chart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("short.txt");
        std::fs::write(&path, "50 0 0\n50 1 1\n").unwrap();
        assert!(read_reference(&path).is_err());

        let full: String = (0..PATCHES)
            .map(|i| format!("# patch {}\n{} {} {}\n", i + 1, 40 + i, i, -(i as i32)))
            .collect();
        std::fs::write(&path, full).unwrap();
        let values = read_reference(&path).unwrap();
        assert_eq!(values.len(), PATCHES);
        assert_eq!(values[3], [43.0, 3.0, -3.0]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
