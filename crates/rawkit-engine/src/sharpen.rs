//! Output sharpening: answering what the resize took away.
//!
//! The second of this project's two sharpening passes, and they are not the
//! same operation asked for twice.
//!
//! **Capture sharpening** lives in stage J of `demosaic_rcd.wgsl` and belongs to
//! the *photograph*: a demosaiced frame is soft because two thirds of every
//! pixel was interpolated, which is true of the image however it is later used.
//! It is in [`rawkit_editstate::Detail`], stored with the edit, and it runs on
//! the GPU because it runs on every frame the canvas draws.
//!
//! **Output sharpening** belongs to the *file*. Resampling 6024 pixels down to
//! 2000 discards two thirds of the detail and leaves the result softer than the
//! frame it came from — by an amount that depends entirely on the size asked
//! for. It runs here, on the CPU, once, after the resize, on the handful of
//! megapixels that survived it.
//!
//! # Why this is not in `EditState`
//!
//! Because the correct amount is a property of the output size, and an edit does
//! not know its output size. One edit exported at 6000 pixels and at 800 wants
//! different amounts of it — the same stored number would be wrong for one of
//! them. Keeping it out also keeps it out of the catalog, out of presets and
//! snapshots, and out of [`rawkit_editstate::Group`], none of which would have
//! anything true to say about it.
//!
//! # Default off
//!
//! Unlike capture sharpening, which defaults to 0.4. The asymmetry is the same
//! one the noise controls carry: a demosaiced frame is soft as a matter of
//! physics and needs answering whatever anyone asked for, whereas an export at
//! full resolution has lost nothing and has nothing to restore. A converter
//! that quietly adds sharpening to a file the user did not resize is one whose
//! output cannot be compared with anything.

/// How much output sharpening a file gets.
///
/// Named amounts rather than a number, because the number is not the thing
/// being chosen: nobody knows what 0.5 means here, and everybody knows what
/// "more than standard" means. The same three-plus-off shape every other
/// converter's export dialogue offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputSharpening {
    #[default]
    None,
    Low,
    Standard,
    High,
}

impl OutputSharpening {
    /// Every setting, for a command line's help text and a panel's menu.
    pub const ALL: [OutputSharpening; 4] = [
        OutputSharpening::None,
        OutputSharpening::Low,
        OutputSharpening::Standard,
        OutputSharpening::High,
    ];

    /// The name a user types and a file records.
    pub fn as_str(self) -> &'static str {
        match self {
            OutputSharpening::None => "none",
            OutputSharpening::Low => "low",
            OutputSharpening::Standard => "standard",
            OutputSharpening::High => "high",
        }
    }

    /// Parse one, case-insensitively. `None` for anything else, so a caller can
    /// say what was wrong rather than silently sharpening by some default.
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|s| s.as_str() == name)
    }

    /// The unsharp-mask amount, on the same 0-to-1 scale as
    /// [`rawkit_editstate::Detail::sharpen_amount`], so the two passes can be
    /// reasoned about against each other.
    ///
    /// Standard is deliberately below the capture default: this runs *after*
    /// that one, on a frame that has already been sharpened once, and the two
    /// compound.
    fn amount(self) -> f32 {
        match self {
            OutputSharpening::None => 0.0,
            OutputSharpening::Low => 0.15,
            OutputSharpening::Standard => 0.3,
            OutputSharpening::High => 0.55,
        }
    }
}

/// The blur's radius in pixels.
///
/// Small, and fixed. Output sharpening is restoring the edge a resampler
/// rounded off, which is a one-pixel affair by construction — a wider radius
/// would be sharpening *shapes*, which is a look and belongs to the edit, not
/// to the file. Capture sharpening's radius is adjustable for exactly that
/// reason and this one is not.
const RADIUS: f32 = 0.7;

/// How far the blur reaches, in pixels. Two sigmas of [`RADIUS`] rounded up: the
/// weight beyond it is under a thousandth and costs nine taps to collect.
const REACH: i32 = 2;

/// Unsharp mask on luminance, in place.
///
/// `rgba` is four floats per pixel, in linear light, as it comes out of
/// [`crate::resize::resample`]. Alpha is not touched.
///
/// Luminance rather than colour, and the same correction added to all three
/// channels — so a sharpened edge moves along the grey axis and gains contrast
/// without gaining a coloured fringe. That is the same choice stage J makes,
/// and for the same reason.
pub fn sharpen(rgba: &mut [f32], width: u32, height: u32, strength: OutputSharpening) {
    let amount = strength.amount();
    // Exactly zero changes exactly nothing, rather than nearly nothing: that is
    // what lets the default be "off" and mean it.
    if amount <= 0.0 || width == 0 || height == 0 {
        return;
    }
    let (w, h) = (width as usize, height as usize);
    if rgba.len() < w * h * 4 {
        return;
    }

    // A separate plane, because a neighbourhood operation cannot read the
    // buffer it is writing: half the neighbours would already be sharpened and
    // which half depends on the order the loop happened to reach them.
    let luma: Vec<f32> = rgba
        .chunks_exact(4)
        .map(|p| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2])
        .collect();

    let falloff = -0.5 / (RADIUS * RADIUS);
    for y in 0..h {
        for x in 0..w {
            // Normalised by the weight actually used rather than by a constant,
            // so a tap clamped at the frame's edge does not darken the blur and
            // turn the border into a bright line.
            let mut blurred = 0.0;
            let mut total = 0.0;
            for j in -REACH..=REACH {
                let sy = (y as i32 + j).clamp(0, h as i32 - 1) as usize;
                for i in -REACH..=REACH {
                    let sx = (x as i32 + i).clamp(0, w as i32 - 1) as usize;
                    let weight = ((i * i + j * j) as f32 * falloff).exp();
                    blurred += luma[sy * w + sx] * weight;
                    total += weight;
                }
            }
            let p = y * w + x;
            let correction = amount * (luma[p] - blurred / total);
            for c in 0..3 {
                // Never below zero: an undershoot at a hard edge is not a
                // quantity of light, and it poisons every `pow` downstream.
                rgba[p * 4 + c] = (rgba[p * 4 + c] + correction).max(0.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(width: usize, height: usize) -> Vec<f32> {
        let mut pixels = Vec::with_capacity(width * height * 4);
        for _ in 0..height {
            for x in 0..width {
                let v = if x < width / 2 { 0.2 } else { 0.6 };
                pixels.extend_from_slice(&[v, v, v, 1.0]);
            }
        }
        pixels
    }

    /// Contrast across the step in the middle of the frame.
    fn step(pixels: &[f32], width: usize, y: usize) -> f32 {
        let row = y * width * 4;
        pixels[row + (width / 2) * 4] - pixels[row + (width / 2 - 1) * 4]
    }

    #[test]
    fn off_is_bit_identical() {
        let mut pixels = edge(32, 16);
        let before = pixels.clone();
        sharpen(&mut pixels, 32, 16, OutputSharpening::None);
        assert_eq!(pixels, before, "'none' is not allowed to be nearly none");
    }

    #[test]
    fn each_step_sharpens_more_than_the_one_below() {
        let mut measured = Vec::new();
        for strength in OutputSharpening::ALL {
            let mut pixels = edge(32, 16);
            sharpen(&mut pixels, 32, 16, strength);
            measured.push((strength, step(&pixels, 32, 8)));
        }
        for pair in measured.windows(2) {
            let ((low, weak), (high, strong)) = (pair[0], pair[1]);
            assert!(
                strong > weak + 0.01,
                "{} gave {weak:.4} across the edge and {} gave {strong:.4}",
                low.as_str(),
                high.as_str()
            );
        }
    }

    #[test]
    fn a_flat_field_is_left_alone() {
        // Nothing to sharpen means nothing added — including at the border,
        // where the kernel is clamped and an unnormalised blur would leave a
        // bright rim.
        let mut pixels = vec![0.3f32; 24 * 24 * 4];
        sharpen(&mut pixels, 24, 24, OutputSharpening::High);
        for (i, v) in pixels.iter().enumerate() {
            assert!(
                (v - 0.3).abs() < 1e-5,
                "sample {i} moved to {v} on a field with no detail in it"
            );
        }
    }

    #[test]
    fn an_edge_gains_no_colour() {
        // The reason it works on luminance. A coloured edge must come back with
        // its hue where it was; a per-channel unsharp mask would fringe it.
        let mut pixels = Vec::new();
        for _ in 0..16 {
            for x in 0..32 {
                if x < 16 {
                    pixels.extend_from_slice(&[0.30, 0.15, 0.10, 1.0]);
                } else {
                    pixels.extend_from_slice(&[0.60, 0.30, 0.20, 1.0]);
                }
            }
        }
        let before = pixels.clone();
        sharpen(&mut pixels, 32, 16, OutputSharpening::High);
        // The same number added to all three channels, wherever it was added.
        for p in 0..32 * 16 {
            let d: Vec<f32> = (0..3)
                .map(|c| pixels[p * 4 + c] - before[p * 4 + c])
                .collect();
            assert!(
                (d[0] - d[1]).abs() < 1e-6 && (d[1] - d[2]).abs() < 1e-6,
                "pixel {p} moved by {d:?}, which is a colour and not a correction"
            );
        }
    }

    #[test]
    fn the_names_round_trip() {
        for strength in OutputSharpening::ALL {
            assert_eq!(OutputSharpening::parse(strength.as_str()), Some(strength));
        }
        assert_eq!(
            OutputSharpening::parse("  HIGH "),
            Some(OutputSharpening::High)
        );
        assert_eq!(OutputSharpening::parse("medium"), None);
    }
}
