//! Where the light in this particular photograph actually is.
//!
//! Every number the tone controls are built on is a constant. The taper's
//! reach, how far the black and white points may travel, the tone map's
//! shoulder, the guide's blur radius — all fixed, all chosen once, all applied
//! identically to a backlit silhouette and a studio grey card. That is the
//! whole of what the industry means by a control being *scene adaptive*, and
//! rawkit has none of it.
//!
//! This is the missing half: one measurement of the frame, taken from the guide
//! that already exists, expressed in the only unit a photographer's controls
//! should be parameterised in — **stops from mid-grey**.
//!
//! # Why the guide, and not the image
//!
//! Three reasons, and the third is the one that decides it.
//!
//! - It is already built, already whole-frame, and already about a hundred
//!   thousand texels — enough for a percentile to be stable and few enough that
//!   a pass over it costs nothing beside the render it precedes.
//! - It is **edge-aware**, so a single blown specular does not drag the frame's
//!   white up by itself; the blur has already averaged it against its
//!   surroundings. A percentile handles the rest.
//! - It holds the camera's own RGB from *before* white balance, so this module
//!   develops it through the same two transforms the shader does and nothing
//!   else. What comes out is the scene, not the edit: **exposure is deliberately
//!   excluded.** A statistic that moved when the exposure slider moved would
//!   make every control derived from it chase its own tail.
//!
//! # What "mid-grey" means here, and why it is 0.072
//!
//! [`MID_GREY`] is the value a photographed mid-grey arrives at in the space the
//! tone map reads — after white balance and the camera profile, before exposure.
//! It is not 0.18, and that is a correction rather than a fudge: a camera meters
//! below the middle to keep its highlights, so a grey card lands near 7% of full
//! scale. `mid_grey_survives_the_tone_map` in `tests/develop.rs` is the
//! measurement, and `the_tone_map_puts_mid_grey_where_this_module_says` below is
//! what stops the two drifting apart.
//!
//! # What it does not measure
//!
//! Anything spatial. This is a histogram, so a frame that is half sky and half
//! shadow and a frame that is uniformly mid-grey with the same extremes read the
//! same at the endpoints — only the median separates them. Deciding *where* in
//! the frame the light is belongs to the guide and, later, to a pyramid; this
//! decides only how far apart the ends are.

use crate::guide::Guide;
use crate::profile::Matrix3;

/// The value a photographed mid-grey arrives at, in the space the tone map
/// reads: after white balance and the camera profile, before exposure.
///
/// The tone map is `x / (x + k)`, so pinning mid-grey to 0.18 on the display
/// fixes `k = MID_GREY * (1 - 0.18) / 0.18 = 0.328`, which is the 0.33 in the
/// shader. The two numbers are one decision written down twice, and the test
/// below is what keeps them one decision.
pub const MID_GREY: f32 = 0.072;

/// The darkest and brightest the histogram can express, in stops from mid-grey.
///
/// Sixteen stops down is 1.1 parts per million of full scale — below any
/// sensor's noise floor, so nothing real is lost by pinning the tail there.
/// Eight up is four stops past the brightest specular a raw file holds, and the
/// range has to cover it because a percentile that saturates its own histogram
/// reports the histogram's edge rather than the scene's.
const FLOOR_EV: f32 = -16.0;
const CEILING_EV: f32 = 8.0;

/// How finely the range above is divided. A twentieth of a stop, which is
/// finer than any control derived from this can act on and coarse enough that
/// every bin in a real frame holds samples.
const BIN_EV: f32 = 0.05;

const BINS: usize = ((CEILING_EV - FLOOR_EV) / BIN_EV) as usize;

/// Where the ends of the scene are taken to be.
///
/// Half a percent in from each end, which is the choice Paris et al. make for
/// the same job in the local Laplacian paper's tone mapping. The reasoning is
/// the same reasoning behind using a percentile at all: the brightest texel in
/// a frame is a property of one texel, and an operator anchored to it would be
/// re-anchored by a dust spot.
const TAIL: f32 = 0.005;

/// One photograph's light, measured.
///
/// Every field is in **stops from mid-grey**: zero is a grey card, positive is
/// brighter. That is the unit because it is the one a tone control can be
/// written in without knowing anything about the encoding it will end up in —
/// the same reason darktable's filmic states its endpoints as "white relative
/// exposure" rather than as a level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SceneStats {
    /// Where the shadows run out, at the [`TAIL`] percentile.
    pub black_ev: f32,
    /// Where the highlights run out, at the far [`TAIL`] percentile.
    pub white_ev: f32,
    /// The middle of the frame's light, which is what separates a low-key
    /// photograph from a high-key one. A grey card fills at 0.0; a snowfield
    /// reads positive and a night scene negative.
    pub median_ev: f32,
    /// The fraction of the sensor that clipped, carried through from
    /// [`Guide::clipped`].
    ///
    /// Here because it is the reason to distrust [`SceneStats::white_ev`]: the
    /// brightest thing in a frame that clipped is not the brightest thing in
    /// the *scene*, it is the sensor's ceiling, and an operator that anchored
    /// to it would anchor to the exposure the photographer happened to make.
    pub clipped: f32,
    /// How many guide texels the measurement is over. Carried so a caller can
    /// tell a thin measurement from a thick one rather than having to trust
    /// every one equally.
    pub texels: u32,
}

impl SceneStats {
    /// Measure a frame, or decline to.
    ///
    /// `wb` and `camera_to_display` are the same two transforms the shader
    /// applies before the tone map, in the same order; pass the *combined*
    /// camera-to-display chain, not the profile's half of it, or the luminance
    /// this works in will not be the luminance the renderer works in.
    ///
    /// The frame's clipping is not measured here — it is read off the guide,
    /// which counted it on the mosaic where it is still visible.
    ///
    /// # Why this returns an `Option`
    ///
    /// Because a frame with no measurable range is a real thing and silently
    /// inventing one is the worst available answer. A frame that is entirely
    /// black, or entirely one value — which is every synthetic test frame in
    /// this repo — has `white_ev == black_ev`, and any control that divides by
    /// the range would produce an infinity from it.
    ///
    /// `None` means *use the fixed constants*, which is what everything did
    /// before this module existed. That is what makes an unmeasurable frame
    /// render bit-identically to a build without any of this, rather than
    /// nearly identically.
    pub fn measure(guide: &Guide, wb: [f32; 3], camera_to_display: &Matrix3) -> Option<Self> {
        let mut counts = [0u32; BINS];
        let mut total = 0u32;

        for texel in guide.data.chunks_exact(3) {
            let balanced = [texel[0] * wb[0], texel[1] * wb[1], texel[2] * wb[2]];
            let developed = crate::profile::apply(camera_to_display, balanced);
            // Rec. 709, which is what the developed values are in by this
            // point — the same weights and the same reasoning as `local_tone`
            // in the shader, which asks the same question of the same data.
            let luma = 0.2126 * developed[0] + 0.7152 * developed[1] + 0.0722 * developed[2];

            let ev = (luma.max(1e-9) / MID_GREY).log2();
            let bin = ((ev - FLOOR_EV) / BIN_EV).floor();
            // Not a clamp on `ev` before the divide: a value above the ceiling
            // belongs in the top bin, and a `clamp` on the EV would move it to
            // exactly the ceiling and then floor it into the bin below.
            counts[(bin.max(0.0) as usize).min(BINS - 1)] += 1;
            total += 1;
        }

        if total == 0 {
            return None;
        }

        let at = |fraction: f32| -> f32 {
            let target = (total as f32 * fraction) as u32;
            let mut seen = 0u32;
            for (bin, count) in counts.iter().enumerate() {
                seen += count;
                if seen > target {
                    // The bin's middle, not its edge: the samples in it are
                    // spread across it and its centre is the least wrong single
                    // value to stand for them.
                    return FLOOR_EV + (bin as f32 + 0.5) * BIN_EV;
                }
            }
            CEILING_EV
        };

        let black_ev = at(TAIL);
        let white_ev = at(1.0 - TAIL);
        if white_ev <= black_ev {
            return None;
        }

        Some(Self {
            black_ev,
            white_ev,
            median_ev: at(0.5),
            clipped: guide.clipped,
            texels: total,
        })
    }

    /// How many stops the frame spans between its two tails.
    ///
    /// Always positive: [`SceneStats::measure`] refuses to return a frame where
    /// it would not be.
    pub fn dynamic_range(&self) -> f32 {
        self.white_ev - self.black_ev
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guide of the given camera-RGB values, one texel each, with no blur and
    /// no mosaic behind it.
    ///
    /// Built by hand rather than through `Guide::build` on purpose: this module
    /// is being tested, not the guide, and a synthetic mosaic would put the
    /// reduction and the edge-aware blur between the numbers written here and
    /// the numbers measured.
    fn guide_of(texels: &[[f32; 3]]) -> Guide {
        Guide {
            data: texels.iter().flatten().copied().collect(),
            chroma: [1.0; 3],
            chroma_known: true,
            clipped: 0.0,
            width: texels.len() as u32,
            height: 1,
        }
    }

    /// The identity chain: a camera whose values are already display values.
    const NEUTRAL: Matrix3 = crate::profile::IDENTITY;

    #[test]
    fn a_grey_card_reads_zero_stops() {
        // The one anchoring fact. If a photographed mid-grey does not measure
        // as 0 EV then every control derived from this is offset by however
        // much it is wrong by, uniformly and invisibly.
        let guide = guide_of(&[[MID_GREY; 3]; 64]);
        let stats = SceneStats::measure(&guide, [1.0; 3], &NEUTRAL);
        // Flat, so there is no range to measure and the module says so rather
        // than inventing one.
        assert_eq!(stats, None, "a flat frame has no measurable range");

        // Give it something to span and the median lands on the card.
        let mut texels = vec![[MID_GREY; 3]; 98];
        texels.push([MID_GREY * 16.0; 3]);
        texels.push([MID_GREY / 16.0; 3]);
        let stats = SceneStats::measure(&guide_of(&texels), [1.0; 3], &NEUTRAL)
            .expect("a frame with range is measurable");
        assert!(
            stats.median_ev.abs() < BIN_EV,
            "a frame of mid-grey has a median of {} EV, not 0",
            stats.median_ev
        );
    }

    #[test]
    fn the_ends_are_where_the_light_ends() {
        // A ramp from four stops under to four over, uniformly filled. The
        // tails cut half a percent off each end, so with 800 samples across
        // eight stops they land one bin in from the extremes — close enough
        // that a quarter of a stop is a generous tolerance.
        let texels: Vec<[f32; 3]> = (0..800)
            .map(|i| {
                let ev = -4.0 + 8.0 * i as f32 / 799.0;
                [MID_GREY * ev.exp2(); 3]
            })
            .collect();
        let stats =
            SceneStats::measure(&guide_of(&texels), [1.0; 3], &NEUTRAL).expect("measurable");

        assert!(
            (stats.black_ev + 4.0).abs() < 0.25,
            "black read {} EV, not -4",
            stats.black_ev
        );
        assert!(
            (stats.white_ev - 4.0).abs() < 0.25,
            "white read {} EV, not +4",
            stats.white_ev
        );
        assert!(
            (stats.dynamic_range() - 8.0).abs() < 0.5,
            "range read {} stops, not 8",
            stats.dynamic_range()
        );
    }

    #[test]
    fn one_bright_texel_does_not_decide_the_frame() {
        // The entire reason for a percentile rather than a maximum. A single
        // blown specular in a thousand ordinary texels must not move the
        // frame's white — an operator anchored to the maximum would be
        // re-anchored by a dust spot on the sensor.
        let mut texels = vec![[MID_GREY; 3]; 500];
        texels.extend(vec![[MID_GREY * 4.0; 3]; 500]);
        let ordinary = SceneStats::measure(&guide_of(&texels), [1.0; 3], &NEUTRAL).expect("ok");

        texels.push([MID_GREY * 4096.0; 3]);
        let with_glint = SceneStats::measure(&guide_of(&texels), [1.0; 3], &NEUTRAL).expect("ok");

        assert!(
            (with_glint.white_ev - ordinary.white_ev).abs() < 0.1,
            "one texel at +12 EV moved white from {} to {}",
            ordinary.white_ev,
            with_glint.white_ev
        );
    }

    #[test]
    fn the_measurement_is_of_the_scene_and_not_of_the_exposure() {
        // Exposure is excluded on purpose, and this is what says so. It is not
        // an omission to be tidied up later: a white anchor that moved with the
        // exposure slider would mean pulling exposure up also pulled the
        // shoulder up, and the control would have no effect on the highlights
        // it exists to protect.
        //
        // Nothing in `measure` takes an exposure, so the test is that scaling
        // the *light* moves the answer by exactly that many stops — the
        // renderer's exposure multiply happens after this, on a scene that has
        // already been described.
        let texels: Vec<[f32; 3]> = (0..256)
            .map(|i| [MID_GREY * (i as f32 / 128.0).max(1e-3); 3])
            .collect();
        let here = SceneStats::measure(&guide_of(&texels), [1.0; 3], &NEUTRAL).expect("ok");

        let brighter: Vec<[f32; 3]> = texels.iter().map(|t| [t[0] * 4.0; 3]).collect();
        let there = SceneStats::measure(&guide_of(&brighter), [1.0; 3], &NEUTRAL).expect("ok");

        assert!(
            (there.white_ev - here.white_ev - 2.0).abs() < 0.1,
            "two stops of light moved white by {} stops",
            there.white_ev - here.white_ev
        );
        assert!(
            (there.dynamic_range() - here.dynamic_range()).abs() < 0.1,
            "scaling the light changed the range from {} to {}",
            here.dynamic_range(),
            there.dynamic_range()
        );
    }

    #[test]
    fn white_balance_and_the_profile_are_both_applied() {
        // A statistic taken from raw camera values would be wrong by whatever
        // the white balance and the matrix do — which on a tungsten frame is
        // more than a stop. Both are applied, so a blue-heavy camera reading
        // under a warm multiplier measures the same light a neutral one does.
        let camera = [[MID_GREY * 2.0, MID_GREY, MID_GREY * 0.5]; 256];
        let mut texels: Vec<[f32; 3]> = camera.to_vec();
        for (i, t) in texels.iter_mut().enumerate() {
            let gain = (i as f32 / 128.0).max(1e-3);
            *t = [t[0] * gain, t[1] * gain, t[2] * gain];
        }
        // The multipliers that make this camera's neutral neutral.
        let wb = [0.5, 1.0, 2.0];
        let balanced = SceneStats::measure(&guide_of(&texels), wb, &NEUTRAL).expect("ok");

        let grey: Vec<[f32; 3]> = (0..256)
            .map(|i| [MID_GREY * (i as f32 / 128.0).max(1e-3); 3])
            .collect();
        let reference = SceneStats::measure(&guide_of(&grey), [1.0; 3], &NEUTRAL).expect("ok");

        assert!(
            (balanced.median_ev - reference.median_ev).abs() < 0.1,
            "white balance left the median at {} against {}",
            balanced.median_ev,
            reference.median_ev
        );
    }

    #[test]
    fn the_guide_counts_the_clipping_on_the_mosaic() {
        // Half a frame at full scale, through the real `Guide::build` rather
        // than a hand-made one — this is the path that has the mosaic in front
        // of it, and the count is only meaningful because it is taken there.
        //
        // A flat mosaic, so the reduction has nothing to average across and the
        // answer is exactly the fraction of quads that were at the ceiling.
        const W: u32 = 64;
        let mut mosaic = vec![0.05f32; (W * W) as usize];
        for y in 0..W / 2 {
            for x in 0..W {
                mosaic[(y * W + x) as usize] = 1.0;
            }
        }
        let guide = Guide::build(&mosaic, W, W, crate::BayerPhase::Rggb, 1.0);
        assert!(
            (guide.clipped - 0.5).abs() < 0.02,
            "half a blown frame counted as {}",
            guide.clipped
        );

        // And a frame whose clip level is above anything in it has none.
        let clean = Guide::build(&mosaic, W, W, crate::BayerPhase::Rggb, 4.0);
        assert_eq!(
            clean.clipped, 0.0,
            "nothing reached a clip level of 4.0, but {} of the frame was called blown",
            clean.clipped
        );
    }

    #[test]
    fn the_tone_map_puts_mid_grey_where_this_module_says() {
        // Two copies of one decision: `MID_GREY` here, and the sigmoid's `k` in
        // the WGSL. They are related by `MID_GREY / (MID_GREY + k) = 0.18`, and
        // if either moves without the other then every EV this module reports
        // is offset and nothing says so. `mid_grey_survives_the_tone_map` in
        // `tests/develop.rs` measures the same fact through the GPU; this one
        // catches it at compile time on a machine with no GPU at all.
        let wgsl = include_str!("../shaders/demosaic_rcd.wgsl");
        let k: f32 = wgsl
            .lines()
            .skip_while(|l| !l.contains("fn tone_map("))
            .find_map(|l| l.trim().strip_prefix("let k = "))
            .and_then(|tail| tail.trim_end_matches(';').parse().ok())
            .expect("the shader's tone map has no `let k =`");

        let rendered = MID_GREY / (MID_GREY + k);
        assert!(
            (rendered - 0.18).abs() < 0.005,
            "a mid-grey of {MID_GREY} through a shoulder of {k} renders to {rendered}, not 0.18"
        );
    }
}
