//! What the file would hold, counted — and where it stops holding anything.
//!
//! # Why this lives beside the encoder rather than in the engine
//!
//! A histogram is only useful if it describes the picture you are about to
//! produce, and the engine does not produce a picture: it hands back
//! display-referred **linear** light, in which a mid-grey sits at 0.18 and four
//! fifths of the bins would be empty. Every photographic histogram is drawn in
//! the encoded space, so something has to apply the transfer function first.
//!
//! There is exactly one transfer function in this tree, it comes from Little CMS
//! by way of [`crate::to_eight_bit`], and it is the one an export writes. Asking
//! it means the histogram cannot drift from the file — which is the failure this
//! placement exists to prevent, and the same reason [`crate::encode`] takes its
//! embedded profile from the object it transforms through.
//!
//! # It counts the photograph, not the screen
//!
//! Nothing here knows about the monitor profile, and it must not: that profile
//! describes a particular piece of glass, and a histogram that changed when you
//! dragged the window to the other display would be describing the wrong thing.

use crate::ExportError;

/// One bin per eight-bit value, because the file has 256 of them.
///
/// Not a free parameter. Binning finer would invent resolution the output does
/// not have; binning coarser would hide the single value at each end that the
/// clipping counts are about.
pub const BINS: usize = 256;

/// Rec. 709 luminance weights — correct here specifically because the working
/// space is sRGB primaries at D65, which is the space these coefficients were
/// derived for. If the working space ever widens, these move with it.
const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// The distribution of one rendered frame, in the values a file would carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Histogram {
    pub red: [u32; BINS],
    pub green: [u32; BINS],
    pub blue: [u32; BINS],
    /// Luminance, computed in **linear** light and then encoded, rather than
    /// weighted after encoding. The latter is common and is a photometric
    /// mistake: it averages numbers that are not proportional to light.
    pub luma: [u32; BINS],
    pub pixels: u32,
    /// Pixels where **any** channel reached the top of the range.
    pub clipped_white: u32,
    /// Pixels where **every** channel reached the bottom.
    ///
    /// Asymmetric with the white count, deliberately. A channel at full scale
    /// has certainly lost information — brighter values existed upstream and
    /// were flattened onto the same number. A channel at zero usually has not:
    /// a saturated red legitimately holds no blue, and warning about it would
    /// paint every strong colour in the frame as a fault. Only a pixel with
    /// nothing in any channel is black in the sense the warning means.
    pub clipped_black: u32,
}

impl Histogram {
    /// Count one rendered frame.
    ///
    /// `pixels` is four floats per pixel in the engine's output — linear, sRGB
    /// primaries, nominally 0–1 — which is the same thing [`crate::encode`]
    /// takes, so a caller can hand the identical buffer to both.
    pub fn of(pixels: &[f32], width: u32, height: u32) -> Result<Self, ExportError> {
        let count = width as usize * height as usize;
        if pixels.len() != count * 4 {
            return Err(ExportError::WrongSize {
                width,
                height,
                expected: count * 4,
                actual: pixels.len(),
            });
        }

        let rgb: Vec<[f32; 3]> = pixels.chunks_exact(4).map(|p| [p[0], p[1], p[2]]).collect();
        let grey: Vec<[f32; 3]> = rgb
            .iter()
            .map(|p| {
                let y = LUMA[0] * p[0] + LUMA[1] * p[1] + LUMA[2] * p[2];
                [y, y, y]
            })
            .collect();

        let encoded = crate::to_eight_bit(&rgb)?;
        // A grey through a well-formed output space comes back grey, so which
        // of the three answers is read does not matter — and
        // `a_grey_survives_the_output_transform_as_a_grey` checks that rather
        // than leaving it as an assumption.
        let encoded_luma = crate::to_eight_bit(&grey)?;

        let mut histogram = Histogram {
            red: [0; BINS],
            green: [0; BINS],
            blue: [0; BINS],
            luma: [0; BINS],
            pixels: count as u32,
            clipped_white: 0,
            clipped_black: 0,
        };
        for (pixel, grey) in encoded.chunks_exact(3).zip(encoded_luma.chunks_exact(3)) {
            histogram.red[pixel[0] as usize] += 1;
            histogram.green[pixel[1] as usize] += 1;
            histogram.blue[pixel[2] as usize] += 1;
            histogram.luma[grey[1] as usize] += 1;
            if pixel.contains(&u8::MAX) {
                histogram.clipped_white += 1;
            }
            if pixel.iter().all(|&v| v == 0) {
                histogram.clipped_black += 1;
            }
        }
        Ok(histogram)
    }

    /// The tallest bin in any channel, which is what a drawing has to scale by.
    ///
    /// Here rather than in the interface because every consumer needs the same
    /// number, and one that scaled each channel to its own maximum would show
    /// three traces whose heights could not be compared.
    pub fn peak(&self) -> u32 {
        [&self.red, &self.green, &self.blue, &self.luma]
            .iter()
            .flat_map(|bins| bins.iter().copied())
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame with something in every part of the range, plus values outside
    /// it, so nothing here is exercised only in the middle.
    fn frame(width: u32, height: u32) -> Vec<f32> {
        let (w, h) = (width as f32, height as f32);
        (0..height)
            .flat_map(|y| (0..width).map(move |x| (x as f32 / w, y as f32 / h)))
            .flat_map(|(u, v)| [1.4 * u * u, v, u * v, 1.0])
            .collect()
    }

    #[test]
    fn the_counts_describe_the_file_that_would_be_written() {
        // The whole claim of this module, checked against something that is not
        // this module: encode the same pixels to a PNG, read the file back with
        // the decoder, and count *those* bytes. If the two disagree the
        // histogram is describing a picture nobody will ever see.
        let (w, h) = (61u32, 37u32);
        let pixels = frame(w, h);
        let file = crate::encode(&pixels, w, h, crate::Format::Png8).expect("encode");

        let mut reader = png::Decoder::new(std::io::Cursor::new(&file))
            .read_info()
            .expect("png header");
        let mut bytes = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut bytes).expect("png pixels");
        assert_eq!(info.color_type, png::ColorType::Rgb);

        let mut red = [0u32; BINS];
        let mut green = [0u32; BINS];
        let mut blue = [0u32; BINS];
        for pixel in bytes[..info.buffer_size()].chunks_exact(3) {
            red[pixel[0] as usize] += 1;
            green[pixel[1] as usize] += 1;
            blue[pixel[2] as usize] += 1;
        }

        let histogram = Histogram::of(&pixels, w, h).expect("histogram");
        assert_eq!(histogram.pixels, w * h);
        assert_eq!(histogram.red, red, "red");
        assert_eq!(histogram.green, green, "green");
        assert_eq!(histogram.blue, blue, "blue");
    }

    #[test]
    fn a_grey_survives_the_output_transform_as_a_grey() {
        // The assumption behind reading one channel of the encoded luminance.
        // It holds because the output space has one transfer function, and it
        // would stop holding the moment the working space widened — at which
        // point this fails rather than the luminance trace quietly tilting.
        let ramp: Vec<[f32; 3]> = (0..=64).map(|i| [i as f32 / 64.0; 3]).collect();
        let encoded = crate::to_eight_bit(&ramp).expect("transform");
        for (i, pixel) in encoded.chunks_exact(3).enumerate() {
            assert_eq!(
                (pixel[0], pixel[1], pixel[2]),
                (pixel[1], pixel[1], pixel[1]),
                "grey {i} came back coloured"
            );
        }
    }

    #[test]
    fn clipping_counts_what_the_file_cannot_hold() {
        // Four pixels, each making one point: blown in one channel only, blown
        // in all three, a saturated colour with two channels at zero, and an
        // ordinary mid-tone.
        let pixels: Vec<f32> = [
            [1.6f32, 0.2, 0.2, 1.0],
            [3.0, 3.0, 3.0, 1.0],
            [0.5, 0.0, 0.0, 1.0],
            [0.18, 0.18, 0.18, 1.0],
        ]
        .concat();
        let histogram = Histogram::of(&pixels, 4, 1).expect("histogram");

        assert_eq!(histogram.clipped_white, 2, "one channel at full is enough");
        assert_eq!(
            histogram.clipped_black, 0,
            "a saturated red was reported as crushed shadow"
        );
        assert_eq!(histogram.red[BINS - 1], 2);
        assert_eq!(histogram.green[0], 1, "only the red pixel has no green");

        // And the black test does fire on something that is actually black.
        let black =
            Histogram::of(&[0.0, 0.0, 0.0, 1.0, -0.3, -0.3, -0.3, 1.0], 2, 1).expect("histogram");
        assert_eq!(black.clipped_black, 2);
        assert_eq!(black.clipped_white, 0);
    }

    #[test]
    fn a_mid_grey_lands_where_the_output_transform_puts_it() {
        // 0.18 linear is the scene mid-grey the tone map pivots on, and sRGB
        // encodes it near 118 of 255. A histogram that put it at 46 would be
        // binning linear light — the mistake this module exists to not make.
        let histogram = Histogram::of(&[0.18, 0.18, 0.18, 1.0], 1, 1).expect("histogram");
        let bin = histogram
            .luma
            .iter()
            .position(|&n| n == 1)
            .expect("one bin");
        assert!(
            (115..=121).contains(&bin),
            "mid-grey landed in bin {bin}, not where sRGB puts it"
        );
    }

    #[test]
    fn a_frame_whose_length_does_not_match_its_size_is_refused() {
        let err = Histogram::of(&[0.0; 8], 4, 1).expect_err("should refuse");
        assert!(matches!(err, ExportError::WrongSize { .. }));
    }

    #[test]
    fn the_peak_is_the_tallest_bin_in_any_channel() {
        let histogram = Histogram::of(&frame(64, 64), 64, 64).expect("histogram");
        let tallest = [
            &histogram.red,
            &histogram.green,
            &histogram.blue,
            &histogram.luma,
        ]
        .iter()
        .filter_map(|bins| bins.iter().copied().max())
        .max()
        .expect("four channels");
        assert!(tallest > 0);
        assert_eq!(histogram.peak(), tallest);
    }
}
