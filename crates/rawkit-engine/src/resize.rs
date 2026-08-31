//! Resampling a developed frame to the size a file was asked for.
//!
//! Not to be confused with the mosaic reduction behind [`crate::Pyramid`], which
//! this sits nowhere near: that one halves a **CFA mosaic** and must preserve its
//! phase, so each colour site averages only with sites of its own colour. This
//! one resamples a finished RGBA frame, where every pixel already carries all
//! three channels and there is no pattern left to keep.
//!
//! # Why not box-averaging
//!
//! This module used to average an integer number of source pixels into each
//! output pixel. That is cheap, and it cannot hit a size: a 6024x4024 frame
//! asked for a 2000-pixel edge divided by 4 and came out **1506** pixels wide,
//! a quarter short in each dimension with nothing said about it. An export that
//! silently ignores the size it was given is worse than one that resizes badly.
//!
//! So the step is a real number now, and hitting a non-integer step needs a
//! reconstruction filter rather than an average. Lanczos-3 — a sinc windowed by
//! a wider sinc — is the usual choice for photographs, and the one whose result
//! looks like what every other converter produces at the same size.
//!
//! # Two things that are easy to get wrong
//!
//! **Downscaling widens the kernel.** A filter evaluated at its nominal width
//! while the image shrinks by four samples only every fourth input pixel, and
//! the three it skipped alias into the result as moire. The kernel is stretched
//! by the reduction factor, so an output pixel sees every input pixel it covers.
//!
//! **Linear light, still.** Averaging encoded values darkens detailed areas,
//! which reads as the resize "losing contrast" and is really a gamma error.
//! `averaging_happens_in_linear_light` pins that as a number.

/// The window width of the Lanczos kernel, in output samples. Three is the
/// photographic default: two is soft, four rings.
const LANCZOS_A: f32 = 3.0;

/// The exact size a frame becomes for a longest edge of `max_dim`.
///
/// `max_dim` of zero means full resolution. So does an image already smaller
/// than the limit: an export never enlarges, because there is nothing to
/// enlarge with and a file twice its real size is a worse lie than a small one.
pub fn fit(width: u32, height: u32, max_dim: u32) -> (u32, u32) {
    let longest = width.max(height);
    if max_dim == 0 || longest <= max_dim || width == 0 || height == 0 {
        return (width, height);
    }
    // The longest edge lands on exactly the number asked for; the other one is
    // rounded, because it is the closest an integer size gets to the aspect
    // ratio the photograph actually has.
    let scale = f64::from(max_dim) / f64::from(longest);
    let other = |edge: u32| ((f64::from(edge) * scale).round() as u32).max(1);
    if width >= height {
        (max_dim, other(height))
    } else {
        (other(width), max_dim)
    }
}

/// Resample to an exact size, in linear light.
///
/// `rgba` is four floats per pixel; alpha comes back as 1, because a photograph
/// does not have one and resampling is not where it would acquire it.
pub fn resample(rgba: &[f32], width: u32, height: u32, out_w: u32, out_h: u32) -> Vec<f32> {
    if (out_w, out_h) == (width, height) {
        return rgba.to_vec();
    }
    let horizontal = plan(width, out_w);
    let vertical = plan(height, out_h);

    // Horizontal first, into an `out_w` by `height` plane — narrow, so the
    // vertical pass reads a quarter of the samples it would have. Three
    // channels rather than four: alpha is invented at the end either way, and
    // carrying it through the intermediate is a quarter of the arithmetic spent
    // on a constant.
    let (w, oh) = (width as usize, height as usize);
    let ow = out_w as usize;
    let mut mid = vec![0.0f32; ow * oh * 3];
    for y in 0..oh {
        let row = y * w * 4;
        for (x, taps) in horizontal.iter().enumerate() {
            let mut acc = [0.0f32; 3];
            for &(sx, weight) in taps {
                let i = row + sx as usize * 4;
                for (c, acc) in acc.iter_mut().enumerate() {
                    *acc += rgba[i + c] * weight;
                }
            }
            mid[(y * ow + x) * 3..][..3].copy_from_slice(&acc);
        }
    }

    let mut out = vec![0.0f32; ow * out_h as usize * 4];
    for (y, taps) in vertical.iter().enumerate() {
        for x in 0..ow {
            let mut acc = [0.0f32; 3];
            for &(sy, weight) in taps {
                let i = (sy as usize * ow + x) * 3;
                for (c, acc) in acc.iter_mut().enumerate() {
                    *acc += mid[i + c] * weight;
                }
            }
            let o = (y * ow + x) * 4;
            // Lanczos has negative lobes, so a hard edge overshoots on both
            // sides of itself. Above one is fine and the encoder clips it;
            // below zero is not a quantity of light, and left alone it becomes
            // a NaN the first time something raises it to a power.
            for (c, acc) in acc.iter().enumerate() {
                out[o + c] = acc.max(0.0);
            }
            out[o + 3] = 1.0;
        }
    }
    out
}

/// Which source samples each output sample is made of, and in what proportion.
///
/// Weights are normalised here rather than at the point of use, so the inner
/// loops are a dot product and nothing else. Taps that fall off the edge are
/// **clamped rather than dropped**, which is what keeps the first and last rows
/// from darkening: the weight is still spent, on the nearest real pixel.
fn plan(src: u32, dst: u32) -> Vec<Vec<(u32, f32)>> {
    let scale = dst as f32 / src as f32;
    // Reducing stretches the kernel by the reduction factor; enlarging does
    // not, because there is no detail between the samples to alias.
    let stretch = if scale < 1.0 { 1.0 / scale } else { 1.0 };
    let radius = LANCZOS_A * stretch;
    let last = src.saturating_sub(1);

    (0..dst)
        .map(|i| {
            // Half-pixel offsets on both sides: sample `i` covers the interval
            // it covers, not the point its index names. Getting this wrong
            // shifts the whole image by half an output pixel, which is
            // invisible in a test and visible in a panorama stitch.
            let centre = (i as f32 + 0.5) / scale - 0.5;
            let first = (centre - radius).ceil() as i64;
            let end = (centre + radius).floor() as i64;

            let mut taps: Vec<(u32, f32)> = Vec::with_capacity((end - first + 1).max(1) as usize);
            let mut total = 0.0;
            for j in first..=end {
                let weight = lanczos((j as f32 - centre) / stretch);
                total += weight;
                taps.push((j.clamp(0, i64::from(last)) as u32, weight));
            }
            // A kernel whose taps happen to cancel would divide by nothing.
            // Cannot occur for Lanczos over its own support, and one branch is
            // cheaper than reasoning about it at every call site.
            if total.abs() > 1e-6 {
                for tap in &mut taps {
                    tap.1 /= total;
                }
            }
            taps
        })
        .collect()
}

/// A sinc windowed by a sinc three times as wide.
fn lanczos(x: f32) -> f32 {
    let x = x.abs();
    if x < 1e-6 {
        return 1.0;
    }
    if x >= LANCZOS_A {
        return 0.0;
    }
    let t = std::f32::consts::PI * x;
    (t.sin() / t) * ((t / LANCZOS_A).sin() / (t / LANCZOS_A))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_longest_edge_is_exactly_what_was_asked_for() {
        // The failure this module was rewritten for: an integer step turned a
        // request for 2000 into 1506 and said nothing.
        assert_eq!(fit(6024, 4024, 2000), (2000, 1336));
        assert_eq!(fit(4024, 6024, 2000), (1336, 2000));
        assert_eq!(fit(4000, 4000, 1000), (1000, 1000));
    }

    #[test]
    fn nothing_is_enlarged() {
        assert_eq!(fit(1200, 800, 2000), (1200, 800));
        assert_eq!(fit(1200, 800, 0), (1200, 800));
        // Exactly at the limit is not a resize either.
        assert_eq!(fit(2000, 1000, 2000), (2000, 1000));
    }

    #[test]
    fn the_same_size_is_a_copy() {
        let pixels: Vec<f32> = (0..4 * 4 * 4).map(|i| i as f32 / 64.0).collect();
        assert_eq!(resample(&pixels, 4, 4, 4, 4), pixels);
    }

    #[test]
    fn a_flat_field_stays_flat() {
        // The claim behind normalising by the weight actually spent: an edge
        // pixel, whose kernel hangs off the frame, must come back the same
        // value as one in the middle. A filter that dropped the outside taps
        // would darken the border instead.
        let pixels = vec![0.25f32; 64 * 48 * 4];
        let out = resample(&pixels, 64, 48, 27, 20);
        for (i, v) in out.chunks_exact(4).enumerate() {
            for (c, v) in v[..3].iter().enumerate() {
                assert!(
                    (v - 0.25).abs() < 1e-4,
                    "pixel {i} channel {c} came back {v}, not 0.25"
                );
            }
            assert_eq!(v[3], 1.0);
        }
    }

    #[test]
    fn averaging_happens_in_linear_light() {
        // Black and white in equal measure averages to 0.5 linear, which is
        // *not* mid-grey once encoded. Averaging encoded values would give 0.5
        // encoded — a visibly darker result, and the gamma error this exists to
        // avoid. Checked as a number rather than described in a comment alone.
        let mut pixels = Vec::new();
        for y in 0..32 {
            for x in 0..32 {
                let v = if (x + y) % 2 == 0 { 0.0 } else { 1.0 };
                pixels.extend_from_slice(&[v, v, v, 1.0]);
            }
        }
        // Down by eight, so each output pixel covers 64 inputs, half of each.
        let out = resample(&pixels, 32, 32, 4, 4);
        for v in out.chunks_exact(4) {
            assert!(
                (v[0] - 0.5).abs() < 0.02,
                "a half-white checkerboard averaged to {}, not 0.5",
                v[0]
            );
        }
    }

    #[test]
    fn a_reduction_does_not_alias() {
        // The reason the kernel is stretched. A one-pixel stripe pattern
        // reduced by four is finer than the output grid can hold, so the right
        // answer is the average — flat grey. A kernel left at its nominal
        // width would sample every fourth column and return the *stripes*,
        // at whatever phase it happened to land on.
        let mut pixels = Vec::new();
        for _ in 0..64 {
            for x in 0..64 {
                let v = if x % 2 == 0 { 0.0 } else { 1.0 };
                pixels.extend_from_slice(&[v, v, v, 1.0]);
            }
        }
        //
        // Measured away from the border, where a stretched kernel hangs off the
        // frame and the taps that fall outside are clamped onto the edge pixel.
        // That pixel is one stripe rather than the average of two, so the outer
        // columns *do* deviate — by 14% here — and would under any boundary
        // policy. It is the filter being asked about, not the edge.
        let out = resample(&pixels, 64, 64, 16, 16);
        let (mut low, mut high) = (f32::MAX, f32::MIN);
        for y in 3..13 {
            for x in 3..13 {
                low = low.min(out[(y * 16 + x) * 4]);
                high = high.max(out[(y * 16 + x) * 4]);
            }
        }
        assert!(
            high - low < 0.01,
            "the stripes aliased through: the interior runs {low:.3} to {high:.3}"
        );
    }

    #[test]
    fn the_image_does_not_shift() {
        // The half-pixel offset in `plan`. A gradient reduced by a whole number
        // keeps its centre of mass; getting the offset wrong slides the whole
        // frame sideways, which no other test in here would notice.
        let mut pixels = Vec::new();
        for _ in 0..16 {
            for x in 0..64 {
                let v = x as f32 / 63.0;
                pixels.extend_from_slice(&[v, v, v, 1.0]);
            }
        }
        let out = resample(&pixels, 64, 16, 16, 4);
        // Mirror-symmetric input, so the output must be mirror-symmetric too.
        for x in 0..8 {
            let left = out[x * 4];
            let right = out[(15 - x) * 4];
            assert!(
                (left + right - 1.0).abs() < 0.01,
                "column {x} is {left:.4} and its mirror {right:.4}: the resize shifted"
            );
        }
    }

    #[test]
    fn nothing_comes_back_negative() {
        // Lanczos undershoots at a hard edge, and negative light poisons every
        // `pow` downstream of here.
        let mut pixels = Vec::new();
        for _ in 0..32 {
            for x in 0..32 {
                let v = if x < 16 { 0.0 } else { 1.0 };
                pixels.extend_from_slice(&[v, v, v, 1.0]);
            }
        }
        let out = resample(&pixels, 32, 32, 11, 11);
        assert!(
            out.iter().all(|v| *v >= 0.0),
            "the filter's undershoot reached the output"
        );
    }
}
