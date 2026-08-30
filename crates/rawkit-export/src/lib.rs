//! Rendered pixels to a colour-managed file — pipeline stages L and M.
//!
//! # Why this is its own crate
//!
//! The engine makes pixels; this turns them into files. Keeping the split means
//! image-format encoders and a C colour-management library stay out of the
//! render path, for the same reason the LibRaw binding lives in its own crate:
//! a dependency that is hard to replace should have a boundary drawn around it
//! while it is still easy to.
//!
//! # What "colour-managed" means here, exactly
//!
//! The engine hands back display-referred **linear** values in sRGB primaries.
//! Two things have to happen for another program to render them correctly:
//!
//! 1. The transfer function has to be applied — linear light is not what an
//!    8-bit file holds.
//! 2. The file has to *say* what its numbers mean, by carrying an ICC profile.
//!    Without that, every viewer guesses, and most guess sRGB, which is right
//!    until the day it is not.
//!
//! Both come from Little CMS, and deliberately from the same `Profile` object.
//! Hand-rolling the transfer function while embedding a profile from somewhere
//! else is the classic way to ship a file whose pixels and whose label disagree
//! — a mismatch nothing warns about and every viewer reproduces faithfully.
//!
//! # Output is sRGB, and that is not a placeholder
//!
//! There is no output-space option because there would be nothing to choose
//! between. The renderer's working space *is* sRGB primaries, so exporting to
//! Adobe RGB or Display P3 today would produce a file with a wider label and no
//! wider colour in it — the gamut was already lost upstream.
//!
//! Widening the working space is a real change with its own consequences (the
//! tone map operates per channel, so its behaviour depends on the space it runs
//! in) and it belongs in its own decision rather than arriving as a side effect
//! of adding a dropdown.

use std::io::Cursor;

pub mod display;
pub mod histogram;

#[derive(Debug, thiserror::Error)]

pub enum ExportError {
    #[error("{width}x{height} needs {expected} samples, got {actual}")]
    WrongSize {
        width: u32,
        height: u32,
        expected: usize,
        actual: usize,
    },
    #[error("colour management failed: {0}")]
    Colour(String),
    #[error("encoding failed: {0}")]
    Encode(String),
}

/// What to write. Depth is part of the format because it is not a free choice:
/// JPEG is eight bits by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Lossy, eight bits per channel. `quality` is the usual 0–100 scale.
    Jpeg { quality: u8 },
    /// Lossless, eight bits. For anything that will be edited again, prefer
    /// sixteen — eight-bit gradients band as soon as they are touched twice.
    Png8,
    /// Lossless, sixteen bits.
    Png16,
}

impl Format {
    /// Guess from a file extension, because that is what a user has already
    /// told us by naming the file.
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Some(Format::Jpeg { quality: 92 }),
            "png" => Some(Format::Png16),
            _ => None,
        }
    }
}

/// Encode display-referred linear RGBA into a colour-managed file.
///
/// `pixels` is four floats per pixel in the engine's output: linear, sRGB
/// primaries, nominally 0–1. Alpha is ignored — a photograph does not have any,
/// and inventing an alpha channel in an export is how a file acquires a
/// transparent sky in the one viewer that respects it.
pub fn encode(
    pixels: &[f32],
    width: u32,
    height: u32,
    format: Format,
) -> Result<Vec<u8>, ExportError> {
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
    let destination = lcms2::Profile::new_srgb();
    let icc = destination
        .icc()
        .map_err(|e| ExportError::Colour(format!("serialising the sRGB profile: {e}")))?;
    let source = linear_srgb_profile()?;

    // Sampled once per process; `None` means the transform is not separable and
    // every pixel goes through Little CMS as it always did.
    let sampled = sampled_transform();

    match format {
        Format::Jpeg { quality } => {
            let eight = to_eight_bit(&rgb)?;
            encode_jpeg(&eight, width, height, quality, &icc)
        }
        Format::Png8 => {
            let eight = to_eight_bit(&rgb)?;
            encode_png(&eight, width, height, png::BitDepth::Eight, &icc)
        }
        Format::Png16 => {
            let bytes = match sampled {
                Some(curves) => curves.to_sixteen(&rgb),
                None => {
                    transform::<[u16; 3]>(&source, &destination, &rgb, lcms2::PixelFormat::RGB_16)?
                        .iter()
                        .flatten()
                        .flat_map(|v| v.to_be_bytes())
                        .collect()
                }
            };
            encode_png(&bytes, width, height, png::BitDepth::Sixteen, &icc)
        }
    }
}

/// Linear display-referred RGB to the eight-bit values a file would carry.
///
/// Extracted because two encoders wanted the same bytes, and then kept because
/// a third caller wanted them for a different reason: [`crate::histogram`]
/// counts what an export would write, and it can only say that truthfully if it
/// asks the same function the export asks.
pub(crate) fn to_eight_bit(rgb: &[[f32; 3]]) -> Result<Vec<u8>, ExportError> {
    match sampled_transform() {
        Some(curves) => Ok(curves.to_eight(rgb)),
        None => Ok(flatten(&transform::<[u8; 3]>(
            &linear_srgb_profile()?,
            &lcms2::Profile::new_srgb(),
            rgb,
            lcms2::PixelFormat::RGB_8,
        )?)),
    }
}

/// How many samples of the transform to take, per channel.
///
/// The interpolation error goes as the square of the spacing. At 8192 the worst
/// case is under a five-thousandth of an eight-bit step, which is far below the
/// rounding that follows it.
const SAMPLES: usize = 8192;

/// Little CMS's answer, tabulated.
///
/// # Why this exists, with the numbers that justify it
///
/// Little CMS is fast when it can precompute a table and slow when it cannot.
/// Measured on a 2560x1710 image with these exact profiles: **8-bit in and out
/// takes 24 ms; 16-bit in takes 563 ms and float in 614 ms.** The difference is
/// not the format, it is that an 8-bit input has 256 possible values so the
/// library builds a device link and looks the answer up. Our input is linear
/// light in floats, and quantising *that* to eight bits before the transfer
/// function is exactly the banding the format notes warn about.
///
/// So this does what Little CMS does, at the precision the input deserves: ask
/// it for the answer at 8192 points and interpolate between them. The numbers
/// still come from Little CMS, and the profile embedded in the file still comes
/// from the same object, so pixels and label cannot disagree.
///
/// # Why it is allowed to be one-dimensional
///
/// Only because it is checked. The working space and the output space share
/// primaries and a white point today, so the transform is per channel — but the
/// module header explicitly anticipates widening the working space, at which
/// point it would not be, and a table built on that assumption would produce
/// colour that is confidently wrong. [`Curves::build`] therefore verifies itself
/// against Little CMS on mixed colours and returns `None` if they disagree,
/// which puts every pixel back through the general path.
struct Curves {
    /// Encoded output for a linear input, one table per channel. Three rather
    /// than one shared table because "the channels happen to match" is another
    /// assumption, and this one costs 96 KB to avoid.
    channel: [Vec<f32>; 3],
}

/// Built once. The profiles never vary, so neither does the table.
fn sampled_transform() -> Option<&'static Curves> {
    static CURVES: std::sync::OnceLock<Option<Curves>> = std::sync::OnceLock::new();
    CURVES.get_or_init(Curves::build).as_ref()
}

impl Curves {
    fn build() -> Option<Curves> {
        let destination = lcms2::Profile::new_srgb();
        let source = linear_srgb_profile().ok()?;

        // A ramp per channel, with the other two held at zero, so what comes
        // back is that channel's own contribution.
        let mut channel: [Vec<f32>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        for (c, table) in channel.iter_mut().enumerate() {
            let ramp: Vec<[f32; 3]> = (0..SAMPLES)
                .map(|i| {
                    let mut pixel = [0.0f32; 3];
                    pixel[c] = i as f32 / (SAMPLES - 1) as f32;
                    pixel
                })
                .collect();
            let out =
                transform::<[f32; 3]>(&source, &destination, &ramp, lcms2::PixelFormat::RGB_FLT)
                    .ok()?;
            *table = out.iter().map(|p| p[c]).collect();
        }
        let curves = Curves { channel };

        // The two checks that make the shortcut legitimate.
        //
        // **Separability**, on mixed colours in range: if the working space ever
        // widens, red will start depending on green and a per-channel table will
        // be confidently wrong everywhere.
        let mixed: Vec<[f32; 3]> = vec![
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [0.18, 0.18, 0.18],
            [0.9, 0.1, 0.35],
            [0.02, 0.55, 0.98],
            [0.33, 0.66, 0.01],
            [0.5, 0.0, 1.0],
            [0.004, 0.002, 0.001],
        ];
        let expected =
            transform::<[f32; 3]>(&source, &destination, &mixed, lcms2::PixelFormat::RGB_FLT)
                .ok()?;
        for (probe, want) in mixed.iter().zip(&expected) {
            let got = curves.apply(*probe);
            for c in 0..3 {
                // A ten-thousandth of full scale, well inside a quarter of an
                // eight-bit step and far above interpolation error. Anything
                // larger means the transform is not separable.
                if (got[c] - want[c]).abs() > 1e-4 {
                    eprintln!(
                        "colour     : the output transform is not separable ({probe:?} → \
                         {got:?} rather than {want:?}); using the slow path"
                    );
                    return None;
                }
            }
        }

        // **Clamping**, on values outside [0, 1], which the renderer does
        // produce. Compared against the eight-bit output rather than the float
        // one, because Little CMS only clamps when it is writing to something
        // that cannot hold the overflow — and the table always clamps, so this
        // is the comparison that says the two agree about what a file gets.
        let outside: Vec<[f32; 3]> = vec![
            [-0.2, 0.5, 1.4],
            [1.4, -0.05, 0.2],
            [-1.0, -1.0, -1.0],
            [2.0, 2.0, 2.0],
        ];
        let clamped =
            transform::<[u8; 3]>(&source, &destination, &outside, lcms2::PixelFormat::RGB_8)
                .ok()?;
        for (probe, want) in outside.iter().zip(&clamped) {
            let got = curves.to_eight(&[*probe]);
            for c in 0..3 {
                if got[c].abs_diff(want[c]) > 1 {
                    eprintln!(
                        "colour     : the table and Little CMS clamp differently ({probe:?} → \
                         {got:?} rather than {want:?}); using the slow path"
                    );
                    return None;
                }
            }
        }

        Some(curves)
    }

    /// One pixel, linear in and encoded out.
    fn apply(&self, pixel: [f32; 3]) -> [f32; 3] {
        let mut out = [0.0f32; 3];
        for c in 0..3 {
            // Clamped, which is what Little CMS does for an integer output and
            // therefore what the probes above compare against.
            let x = pixel[c].clamp(0.0, 1.0) * (SAMPLES - 1) as f32;
            let i = x as usize;
            let table = &self.channel[c];
            out[c] = if i + 1 < SAMPLES {
                let f = x - i as f32;
                table[i] + (table[i + 1] - table[i]) * f
            } else {
                table[SAMPLES - 1]
            };
        }
        out
    }

    fn to_eight(&self, rgb: &[[f32; 3]]) -> Vec<u8> {
        let mut out = Vec::with_capacity(rgb.len() * 3);
        for pixel in rgb {
            for v in self.apply(*pixel) {
                out.push((v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
            }
        }
        out
    }

    fn to_sixteen(&self, rgb: &[[f32; 3]]) -> Vec<u8> {
        let mut out = Vec::with_capacity(rgb.len() * 6);
        for pixel in rgb {
            for v in self.apply(*pixel) {
                let value = (v.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16;
                out.extend_from_slice(&value.to_be_bytes());
            }
        }
        out
    }
}

/// Read back a JPEG this crate wrote, as eight-bit sRGB.
///
/// # Why eight-bit sRGB and not linear floats
///
/// Because the caller is a GPU. Handing back the encoded bytes lets the texture
/// be declared `Rgba8UnormSrgb`, and the hardware does the transfer function for
/// free on every sample. Converting to linear here would mean running lcms over
/// every pixel — 154 ns each, measured — which for a 1024-pixel preview is over
/// a hundred milliseconds, and the whole point of a preview is that it appears
/// immediately.
///
/// # Scope
///
/// This is the inverse of [`encode`] and **not a general image importer**. It
/// assumes the sRGB output that function writes; it does not read the embedded
/// profile and convert from it, because the only files it is pointed at are ones
/// we wrote ten lines away. Pointing it at a stranger's Adobe RGB JPEG would
/// silently misinterpret the colour, so do not.
pub fn decode(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), ExportError> {
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(bytes));
    let pixels = decoder
        .decode()
        .map_err(|e| ExportError::Colour(format!("decoding a preview: {e}")))?;
    let info = decoder
        .info()
        .ok_or_else(|| ExportError::Colour("a decoded jpeg with no header".into()))?;

    let (width, height) = (info.width as u32, info.height as u32);
    let count = width as usize * height as usize;
    // Widened to four channels because that is what a GPU texture wants; three
    // is not a format wgpu offers.
    let mut rgba = vec![255u8; count * 4];
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            if pixels.len() != count * 3 {
                return Err(ExportError::Colour("truncated jpeg".into()));
            }
            for (out, inp) in rgba.chunks_exact_mut(4).zip(pixels.chunks_exact(3)) {
                out[..3].copy_from_slice(inp);
            }
        }
        jpeg_decoder::PixelFormat::L8 => {
            if pixels.len() != count {
                return Err(ExportError::Colour("truncated jpeg".into()));
            }
            for (out, &grey) in rgba.chunks_exact_mut(4).zip(pixels.iter()) {
                out[..3].copy_from_slice(&[grey, grey, grey]);
            }
        }
        other => {
            return Err(ExportError::Colour(format!(
                "a preview in {other:?}, which this build does not read"
            )))
        }
    }
    Ok((rgba, width, height))
}

/// The space the engine renders in: sRGB primaries with a linear transfer
/// function.
///
/// Describing the source to Little CMS rather than converting by hand is what
/// makes the output correct by construction. It also means the day the working
/// space changes, this is the one place that has to know.
fn linear_srgb_profile() -> Result<lcms2::Profile, ExportError> {
    let white = lcms2::CIExyY {
        x: 0.3127,
        y: 0.3290,
        Y: 1.0,
    };
    let primaries = lcms2::CIExyYTRIPLE {
        Red: lcms2::CIExyY {
            x: 0.6400,
            y: 0.3300,
            Y: 1.0,
        },
        Green: lcms2::CIExyY {
            x: 0.3000,
            y: 0.6000,
            Y: 1.0,
        },
        Blue: lcms2::CIExyY {
            x: 0.1500,
            y: 0.0600,
            Y: 1.0,
        },
    };
    let linear = lcms2::ToneCurve::new(1.0);
    lcms2::Profile::new_rgb(&white, &primaries, &[&linear, &linear, &linear])
        .map_err(|e| ExportError::Colour(format!("building the linear source profile: {e}")))
}

fn transform<T: Copy + Default + lcms2::Pod>(
    source: &lcms2::Profile,
    destination: &lcms2::Profile,
    rgb: &[[f32; 3]],
    out_format: lcms2::PixelFormat,
) -> Result<Vec<T>, ExportError> {
    // Relative colorimetric: the source and destination share a white point, so
    // there is no adaptation to do, and perceptual would apply a gamut
    // compression that a matrix profile does not even define.
    let transform = lcms2::Transform::new(
        source,
        lcms2::PixelFormat::RGB_FLT,
        destination,
        out_format,
        lcms2::Intent::RelativeColorimetric,
    )
    .map_err(|e| ExportError::Colour(format!("building the transform: {e}")))?;

    let mut out = vec![T::default(); rgb.len()];
    transform.transform_pixels(rgb, &mut out);
    Ok(out)
}

fn flatten(pixels: &[[u8; 3]]) -> Vec<u8> {
    pixels.iter().flatten().copied().collect()
}

fn encode_jpeg(
    rgb: &[u8],
    width: u32,
    height: u32,
    quality: u8,
    icc: &[u8],
) -> Result<Vec<u8>, ExportError> {
    let mut out = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    encoder
        .add_icc_profile(icc)
        .map_err(|e| ExportError::Encode(format!("attaching the profile: {e}")))?;
    encoder
        .encode(
            rgb,
            width as u16,
            height as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .map_err(|e| ExportError::Encode(e.to_string()))?;
    Ok(out)
}

fn encode_png(
    data: &[u8],
    width: u32,
    height: u32,
    depth: png::BitDepth,
    icc: &[u8],
) -> Result<Vec<u8>, ExportError> {
    let mut out = Vec::new();
    {
        // The profile travels on the `Info`, which is also what a decoder reads
        // it back from — so a file this writes and a file it reads describe the
        // profile the same way.
        let mut info = png::Info::with_size(width, height);
        info.color_type = png::ColorType::Rgb;
        info.bit_depth = depth;
        info.icc_profile = Some(std::borrow::Cow::Owned(icc.to_vec()));

        let encoder = png::Encoder::with_info(Cursor::new(&mut out), info)
            .map_err(|e| ExportError::Encode(e.to_string()))?;
        let mut writer = encoder
            .write_header()
            .map_err(|e| ExportError::Encode(e.to_string()))?;
        writer
            .write_image_data(data)
            .map_err(|e| ExportError::Encode(e.to_string()))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small image with known linear values, including scene mid-grey.
    fn sample() -> (Vec<f32>, u32, u32) {
        let values = [0.0f32, 0.18, 0.5, 1.0];
        let mut pixels = Vec::new();
        for v in values {
            pixels.extend_from_slice(&[v, v, v, 1.0]);
        }
        (pixels, values.len() as u32, 1)
    }

    #[test]
    fn the_table_agrees_with_little_cms_everywhere_it_is_asked() {
        // The whole justification for the shortcut. Little CMS is still the
        // authority on what the numbers are; this only claims to reproduce them.
        // Comparing at eight bits is the comparison that matters, because that is
        // the precision the answer is written at.
        let curves = Curves::build().expect("the transform is separable today");
        let destination = lcms2::Profile::new_srgb();
        let source = linear_srgb_profile().expect("the working space profile");

        // A deterministic spread rather than random: shadows, mid-tones,
        // highlights, saturated corners and the near-black region where the sRGB
        // curve changes shape and a table is most likely to be wrong.
        let mut probes = Vec::new();
        for i in 0..64 {
            let t = i as f32 / 63.0;
            probes.push([t, t, t]);
            probes.push([t, 1.0 - t, t * t]);
            probes.push([t * 0.004, t * 0.02, t * 0.05]);
            probes.push([1.0 - t * t, t, 0.5]);
        }

        let expected =
            transform::<[u8; 3]>(&source, &destination, &probes, lcms2::PixelFormat::RGB_8)
                .expect("reference transform");

        let mut worst = 0i32;
        for (probe, want) in probes.iter().zip(&expected) {
            let got = curves.to_eight(&[*probe]);
            for c in 0..3 {
                let difference = got[c] as i32 - want[c] as i32;
                worst = worst.max(difference.abs());
                assert!(
                    difference.abs() <= 1,
                    "{probe:?} channel {c}: table says {} and Little CMS says {}",
                    got[c],
                    want[c]
                );
            }
        }
        println!("worst disagreement: {worst} of 255");
    }

    #[test]
    fn a_preview_reads_back_as_the_encoded_values_that_were_written() {
        // `decode` is the inverse of `encode` and this is the claim: the eight-
        // bit sRGB it hands a GPU is the same eight-bit sRGB that went into the
        // file. Mid-grey is the number to pin, because linear 0.18 must come
        // back as 118 — if `decode` ever converted to linear on the way out, or
        // `encode` stopped applying the transfer function, this lands on 46.
        let (pixels, width, height) = sample();
        // Quality 100 so JPEG's own losses do not blur the thing being tested.
        let bytes = encode(&pixels, width, height, Format::Jpeg { quality: 100 }).expect("encode");
        let (rgba, w, h) = decode(&bytes).expect("decode");

        assert_eq!((w, h), (width, height));
        assert_eq!(rgba.len(), (width * height * 4) as usize);
        let greys: Vec<u8> = rgba.chunks_exact(4).map(|p| p[0]).collect();
        for (got, expected) in greys.iter().zip([0u8, 118, 188, 255]) {
            assert!(
                got.abs_diff(expected) <= 2,
                "expected about {expected}, got {greys:?}"
            );
        }
        // Opaque throughout: a photograph has no alpha, and a GPU sampling a
        // zero there would show nothing at all.
        assert!(rgba.chunks_exact(4).all(|p| p[3] == 255));
    }

    #[test]
    fn two_encodes_of_the_same_pixels_differ_only_in_the_profiles_timestamp() {
        // Worth pinning because it wastes an afternoon otherwise. Comparing two
        // of our own files byte-for-byte says they differ, which reads as a
        // non-deterministic renderer — and the renderer is not the problem. An
        // ICC profile carries a creation date and Little CMS stamps it when it
        // serialises one, so two bytes of the header move between runs.
        //
        // The lesson, and the reason this is a test rather than a comment:
        // **never compare our output by file bytes.** Compare pixels.
        let (pixels, width, height) = sample();
        let format = Format::Jpeg { quality: 90 };
        let first = encode(&pixels, width, height, format).expect("encode");
        let second = encode(&pixels, width, height, format).expect("encode");

        assert_eq!(first.len(), second.len(), "same content, same length");
        let differing: Vec<usize> = first
            .iter()
            .zip(&second)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| i)
            .collect();
        assert!(
            differing.len() <= 4,
            "expected only the profile's timestamp to move, got {} differing bytes",
            differing.len()
        );

        // And the pixels really are identical, which is the claim that matters.
        assert_eq!(
            decode(&first).expect("decode").0,
            decode(&second).expect("decode").0
        );
    }

    #[test]
    fn a_file_that_is_not_a_jpeg_is_an_error_not_a_panic() {
        // It reads files from a directory beside a user's catalog, which anything
        // could have put something into.
        assert!(decode(b"certainly not a jpeg").is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn the_transfer_function_is_actually_applied() {
        // The failure this catches is writing linear values into a file that
        // claims to be sRGB — which produces an image far too dark, and looks
        // enough like an exposure problem to be misdiagnosed as one.
        //
        // Linear 0.18 is sRGB 0.4613, or 118 of 255. Linear 0.18 written raw
        // would be 46. The difference is not subtle, which is the point: this
        // number is here so the test fails loudly rather than plausibly.
        let (pixels, width, height) = sample();
        let png = encode(&pixels, width, height, Format::Png8).expect("encode");

        let decoder = png::Decoder::new(Cursor::new(&png));
        let mut reader = decoder.read_info().expect("png header");
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).expect("png data");
        let rgb = &buf[..info.buffer_size()];

        assert_eq!(rgb[0], 0, "linear 0.0 should be black");
        assert!(
            (rgb[3] as i32 - 118).abs() <= 1,
            "linear 0.18 encoded to {} rather than sRGB's 118",
            rgb[3]
        );
        assert_eq!(rgb[9], 255, "linear 1.0 should be white");
    }

    #[test]
    fn every_format_carries_a_profile_that_says_srgb() {
        // A file without a profile is a file that means whatever the viewer
        // assumes. Checking the bytes are present is not enough — they have to
        // parse, and describe the space we actually converted to.
        let (pixels, width, height) = sample();

        for format in [Format::Jpeg { quality: 95 }, Format::Png8, Format::Png16] {
            let bytes = encode(&pixels, width, height, format).expect("encode");
            let icc = match format {
                Format::Jpeg { .. } => {
                    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(&bytes));
                    decoder.read_info().expect("jpeg header");
                    decoder.icc_profile().expect("jpeg carries no profile")
                }
                _ => {
                    let decoder = png::Decoder::new(Cursor::new(&bytes));
                    let reader = decoder.read_info().expect("png header");
                    reader
                        .info()
                        .icc_profile
                        .clone()
                        .expect("png carries no profile")
                        .into_owned()
                }
            };

            let profile =
                lcms2::Profile::new_icc(&icc).expect("the embedded profile does not parse");
            let described = profile
                .info(lcms2::InfoType::Description, lcms2::Locale::none())
                .unwrap_or_default();
            assert!(
                described.to_lowercase().contains("srgb"),
                "{format:?} embedded a profile describing {described:?}"
            );
        }
    }

    #[test]
    fn sixteen_bit_output_holds_more_than_eight() {
        // The reason to offer 16-bit at all: an 8-bit file cannot represent the
        // difference between two nearby shadow values, so a gradient that will
        // be edited again bands on the second pass. If both depths produced the
        // same distinct-value count, the option would be a lie.
        let mut pixels = Vec::new();
        for i in 0..256 {
            let v = 0.02 + 0.001 * i as f32;
            pixels.extend_from_slice(&[v, v, v, 1.0]);
        }
        let (width, height) = (256u32, 1u32);

        let distinct = |bytes: &[u8], stride: usize| {
            let decoder = png::Decoder::new(Cursor::new(bytes));
            let mut reader = decoder.read_info().unwrap();
            let mut buf = vec![0; reader.output_buffer_size()];
            let info = reader.next_frame(&mut buf).unwrap();
            let values: std::collections::HashSet<&[u8]> = buf[..info.buffer_size()]
                .chunks_exact(stride * 3)
                .map(|c| &c[..stride])
                .collect();
            values.len()
        };

        let eight = encode(&pixels, width, height, Format::Png8).unwrap();
        let sixteen = encode(&pixels, width, height, Format::Png16).unwrap();
        let (a, b) = (distinct(&eight, 1), distinct(&sixteen, 2));
        println!("distinct shadow values: 8-bit {a}, 16-bit {b} of 256 input");

        // Sixteen bits must lose nothing: every distinct input value survives.
        assert_eq!(
            b,
            256,
            "16-bit collapsed {} of 256 shadow values; the depth is not reaching the file",
            256 - b
        );
        // Eight bits must visibly lose something, or the option would be a lie.
        assert!(
            a < 180,
            "8-bit kept {a} of 256 shadow values, which is more than eight bits \
             can hold in this range — the test is not measuring what it claims"
        );
    }

    #[test]
    fn a_jpeg_round_trips_close_to_what_went_in() {
        let (pixels, width, height) = sample();
        let bytes = encode(&pixels, width, height, Format::Jpeg { quality: 98 }).expect("encode");

        let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(&bytes));
        let decoded = decoder.decode().expect("decode");
        let info = decoder.info().expect("info");
        assert_eq!(info.width as u32, width);
        assert_eq!(info.height as u32, height);

        // Mid-grey has to survive the round trip; JPEG at 98 is close but not
        // exact, and a tolerance here is honest rather than lax.
        assert!(
            (decoded[3] as i32 - 118).abs() <= 3,
            "mid-grey came back as {} rather than 118",
            decoded[3]
        );
    }

    #[test]
    fn a_mismatched_buffer_is_refused() {
        let err = encode(&[0.0; 8], 4, 4, Format::Png8).unwrap_err();
        assert!(matches!(err, ExportError::WrongSize { .. }));
    }

    #[test]
    fn extensions_map_to_formats() {
        assert!(matches!(
            Format::from_extension("JPG"),
            Some(Format::Jpeg { .. })
        ));
        assert_eq!(Format::from_extension("png"), Some(Format::Png16));
        assert_eq!(Format::from_extension("tif"), None);
    }
}
