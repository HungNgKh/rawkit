//! Box-averaging a developed frame down to a requested size.
//!
//! Not to be confused with the mosaic reduction behind [`crate::Pyramid`], which
//! this sits nowhere near: that one halves a **CFA mosaic** and must preserve its
//! phase, so each colour site averages only with sites of its own colour. This
//! one averages a finished RGBA frame, where every pixel already carries all
//! three channels and there is no pattern left to keep.

/// How many source pixels go into one output pixel, for a longest edge of
/// `max_dim`. Zero means full resolution, and is not a special case anywhere
/// else — a step of one copies.
pub fn downsample_step(width: u32, height: u32, max_dim: u32) -> u32 {
    if max_dim == 0 {
        return 1;
    }
    let longest = width.max(height);
    (longest.div_ceil(max_dim)).max(1)
}

/// Box-average down to the requested size, in linear light.
///
/// Averaging before the transfer function is the only correct order;
/// downsampling encoded values darkens detailed areas, which reads as the resize
/// "losing contrast" and is really a gamma error.
pub fn downsample(rgba: &[f32], width: u32, height: u32, step: u32) -> (Vec<f32>, u32, u32) {
    if step <= 1 {
        return (rgba.to_vec(), width, height);
    }
    let (out_w, out_h) = (width / step, height / step);
    let mut out = Vec::with_capacity((out_w * out_h * 4) as usize);
    for oy in 0..out_h {
        for ox in 0..out_w {
            let mut acc = [0.0f32; 3];
            let mut n = 0.0f32;
            for sy in 0..step {
                for sx in 0..step {
                    let i = (((oy * step + sy) * width + ox * step + sx) * 4) as usize;
                    for c in 0..3 {
                        acc[c] += rgba[i + c];
                    }
                    n += 1.0;
                }
            }
            out.extend_from_slice(&[acc[0] / n, acc[1] / n, acc[2] / n, 1.0]);
        }
    }
    (out, out_w, out_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_of_one_is_a_copy() {
        let pixels = vec![0.25f32; 4 * 4];
        let (out, w, h) = downsample(&pixels, 2, 2, downsample_step(2, 2, 0));
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, pixels);
    }

    #[test]
    fn the_step_covers_the_longest_edge() {
        assert_eq!(downsample_step(6024, 4024, 2000), 4);
        assert_eq!(downsample_step(6024, 4024, 0), 1);
        // Rounds up, so the result never exceeds what was asked for.
        assert_eq!(downsample_step(2001, 100, 2000), 2);
        assert_eq!(downsample_step(2000, 100, 2000), 1);
    }

    #[test]
    fn averaging_happens_in_linear_light() {
        // Black and white in equal measure averages to 0.5 linear, which is
        // *not* mid-grey once encoded. Averaging encoded values would give 0.5
        // encoded — a visibly darker result, and the gamma error this exists to
        // avoid. Checked as a number rather than described in a comment alone.
        let mut pixels = Vec::new();
        for i in 0..4 {
            let v = if i % 2 == 0 { 0.0 } else { 1.0 };
            pixels.extend_from_slice(&[v, v, v, 1.0]);
        }
        let (out, w, h) = downsample(&pixels, 2, 2, 2);
        assert_eq!((w, h), (1, 1));
        assert_eq!(out[..3], [0.5, 0.5, 0.5]);
    }
}
