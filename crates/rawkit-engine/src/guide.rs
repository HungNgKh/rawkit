//! A low-resolution, edge-aware picture of where the light is.
//!
//! Highlights and shadows used to move the tone *range*: a pixel at 0.8 was
//! treated as a highlight wherever it sat, so recovering a blown sky also
//! flattened a face lit to the same value. That is what "not spatially adaptive"
//! meant, and it is the difference between the control every editor ships and
//! the one this project had.
//!
//! Making them local needs the pixel's *neighbourhood*, over a radius of a
//! hundred pixels or more. The tile halo is 18 pixels, so it cannot come from
//! there — see [`crate::render::HALO`]. It comes from here instead: the whole
//! frame, reduced to a few hundred pixels on its longest edge, uploaded once
//! when the image opens and sampled bilinearly by every tile at every zoom
//! level.
//!
//! # Three properties that make this usable
//!
//! - **It is edit-independent.** What is stored is the camera's own RGB, before
//!   white balance and before anything the user did. The shader develops it
//!   through the *same* functions it develops a pixel with, so moving a slider
//!   changes what the guide means without rebuilding it. A drag costs nothing
//!   here.
//! - **It is indexed in image coordinates, not tile coordinates.** So two tiles
//!   agree exactly where they meet — there is no seam to hide — and a tile
//!   rendered at level 3 samples the same guide as the same region at level 0,
//!   which is what keeps a photograph from changing as you zoom.
//! - **It is edge-aware.** A plain blur puts a bright halo around every dark
//!   object against a bright sky, because the guide bleeds the sky's brightness
//!   across the boundary and the recovery follows it. The blur here weights by
//!   brightness difference as well as distance, so an edge of more than about a
//!   stop stops it.
//!
//! # It carries a second field: the colour of unclipped light
//!
//! Highlight reconstruction has the same problem pointing the other way. A
//! pixel whose blue channel clipped is *at least* as blue as it looks, and the
//! honest thing to do with a measurement that stopped meaning anything is to
//! replace it — but with what? Grey is what shipped, and grey turns a blown
//! sunset white.
//!
//! So each texel also carries the camera RGB of the light that did **not** clip
//! there, and a texel with no unclipped light of its own takes its neighbours'
//! by diffusion. That last part is why this is a whole-frame structure rather
//! than a search inside the tile halo: the middle of a large blown region has no
//! unclipped pixel within eighteen pixels of it, and a reconstruction that
//! coloured only the *rim* of a highlight would be a worse artefact than the
//! white middle it replaced.
//!
//! # What it is not
//!
//! Not a bilateral filter in the strict sense: the two passes are separable and
//! a true bilateral is not. Run separably it can leave faint streaks across a
//! corner where two strong edges meet. This is a *control signal* sampled at
//! one part in sixteen of the frame, not a picture anyone looks at, and the
//! approximation costs an order of magnitude — the honest version of the
//! trade-off, rather than a claim it is exact.

use crate::render::BayerPhase;

/// The longest edge the guide is built at.
///
/// Sets the operator's reach: at 384 across a 6024-pixel frame one guide texel
/// spans about 16 image pixels, so the blur below reaches roughly 190 of them.
/// Larger would make the control more local and more prone to halo; smaller
/// makes it approach the global behaviour it replaced.
pub const MAX_EDGE: u32 = 384;

/// The blur's sigma, as a fraction of the guide's longest edge.
///
/// Proportional rather than absolute, so the operator reaches the same
/// *fraction* of the picture whatever its dimensions — a control that behaved
/// differently on a crop than on the frame it came from would be unusable.
const SIGMA_DIVISOR: f32 = 32.0;

/// How much brightness difference stops the blur, in stops.
///
/// One stop: enough that texture and shading flow together, little enough that
/// a skyline does not.
const RANGE_STOPS: f32 = 1.0;

/// How far a quad's two greens may differ before it is taken to span an edge,
/// as a fraction of their sum.
///
/// A tenth is about a fifth of a stop across two pixels — well beyond sensor
/// noise on anything bright enough to matter as a chroma reference, and far
/// short of the edge of a twig, which reads several hundred percent. The
/// consequence of being too strict is a guide with fewer chroma samples in it,
/// which the diffusion covers; the consequence of being too loose is an invented
/// colour spreading across a whole sky, which nothing covers.
const FLAT_ENOUGH: f32 = 0.1;

/// A sample this close to the clip level is not to be trusted.
///
/// The mosaic arrives with the decoder's white level at 1.0, and sensors do not
/// clip cleanly at it — the last percent is where the response goes non-linear.
/// A chroma reference built from values inside that shoulder would carry the
/// very skew it exists to correct.
const TRUSTED: f32 = 0.98;

/// The largest guide any allocation has to hold, in floats.
///
/// Square, because [`MAX_EDGE`] bounds the longest edge and the shortest is
/// never longer. Three floats a texel, for the one field there is — the light.
/// The colour of the light that did not clip is a single triple and travels in
/// the uniforms; see [`Guide::chroma`].
pub const CAPACITY: usize = (MAX_EDGE * MAX_EDGE) as usize * 3;

/// The whole frame at a few hundred pixels, in the camera's own RGB.
#[derive(Debug, Clone)]
pub struct Guide {
    /// Three floats per texel, row major: red, green, blue as the sensor saw
    /// them, white balance not applied.
    pub data: Vec<f32>,
    /// The colour of the light that did not clip, in camera RGB, for the whole
    /// frame.
    ///
    /// **One triple, not a field, and that is the point.** It used to be a
    /// per-texel map, filled outwards from the texels that had unclipped light
    /// of their own until it covered the frame. On a dusk seascape the middle of
    /// a blown cloud came out violet, and the reason was structural rather than
    /// a bug to find: the colour arriving there had been carried about forty-
    /// seven texels, so it was already a long-range average — but an
    /// ill-defined one, the running mean of whichever diffusion front reached
    /// the texel first. All the cost of a local method and none of the locality.
    ///
    /// Measured on that frame: the flat unclipped quads read *slightly green* at
    /// every brightness, frame-wide and within five hundred pixels of the cloud,
    /// while the diffused value produced magenta. A frame-wide mean of the same
    /// quads is both better defined and closer to what the sensor actually saw.
    ///
    /// It is also where the field has settled. RawTherapee's Color Propagation
    /// spreads neighbouring colour the way this used to, and has an open report
    /// of purple fill on sun reflecting off water — the same photograph, the
    /// same artefact. darktable's default since 4.2 computes one chrominance
    /// correction from the whole image instead.
    ///
    /// The cost is real and worth stating: a frame with two blown regions under
    /// different light gets one answer for both. Estimating each region from its
    /// own boundary is the better method and a larger one, and this is the
    /// floor it would be built on.
    pub chroma: [f32; 3],
    /// Whether any unclipped light was found at all. False for a frame blown
    /// end to end, where [`Guide::chroma`] is neutral and reconstruction falls
    /// back to the grey it used to produce unconditionally.
    pub chroma_known: bool,
    /// The fraction of the sensor's 2x2 quads with at least one channel at or
    /// above the trusted level — the frame's clipping, as a number.
    ///
    /// **Counted on the mosaic, not on the texels below it**, and that is the
    /// whole reason it lives here rather than being recovered later. The
    /// reduction and the edge-aware blur both average clipped samples against
    /// unclipped neighbours, so by the time anything reads [`Guide::data`] a
    /// small blown specular has been smoothed away to nothing and a large blown
    /// region has soft edges. Asking the mosaic is asking the only thing that
    /// still knows.
    pub clipped: f32,
    pub width: u32,
    pub height: u32,
}

/// How big a guide for an image this size is.
///
/// Never more than half the image in either direction, which is what makes each
/// texel cover at least one whole 2x2 block — and a 2x2 block of a Bayer mosaic
/// holds one red, two green and one blue wherever it starts. Below that a texel
/// could land on a single sample and two of its three channels would have
/// nothing in them.
fn dimensions(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    let scale = if longest > MAX_EDGE {
        f64::from(MAX_EDGE) / f64::from(longest)
    } else {
        1.0
    };
    let edge = |v: u32| (((f64::from(v) * scale).round() as u32).max(1)).min((v / 2).max(1));
    (edge(width), edge(height))
}

impl Guide {
    /// Reduce a mosaic to a guide, smooth it without crossing edges, and work
    /// out the colour of the light that did not clip.
    ///
    /// `clip_level` is where the sensor saturates, in the same units as
    /// `mosaic` — 1.0 for anything that came through `normalise`.
    ///
    /// Costs one pass over the mosaic — 133 ms for a 24 MP frame, the chroma
    /// field and its diffusion included — and is paid once when the image
    /// opens, never while editing. Against the decode that
    /// precedes it that is small; against a slider drag it does not exist,
    /// which is the property that matters.
    pub fn build(
        mosaic: &[f32],
        width: u32,
        height: u32,
        phase: BayerPhase,
        clip_level: f32,
    ) -> Guide {
        let (gw, gh) = dimensions(width, height);
        let cells = (gw as usize) * (gh as usize);
        let mut sum = vec![0.0f32; cells * 3];
        let mut count = vec![0.0f32; cells];
        let mut chroma = [0.0f32; 3];
        let mut chroma_weight = 0.0f32;
        // How many quads there were and how many of them clipped. Counted in
        // the pass that already asks the question, so the frame's clipping
        // costs two increments rather than a second walk over the mosaic.
        let mut quads = 0u32;
        let mut blown = 0u32;

        // Which guide column each image column falls in, worked out once rather
        // than as a 64-bit divide per pixel — it is the same answer every row.
        // Worth 8% and no more: this pass is bound by the scattered
        // accumulation and not by its arithmetic, which is worth knowing before
        // anyone optimises the arithmetic again.
        let column: Vec<u32> = (0..width)
            .map(|x| ((x as u64 * gw as u64) / width as u64).min(gw as u64 - 1) as u32)
            .collect();

        // Walked in 2x2 blocks rather than pixel by pixel. A block is a whole
        // Bayer quad wherever it starts, so each one yields a complete RGB
        // triple — which the chroma field *needs*: "did this light clip" is a
        // question about a pixel, and a lone red sample cannot answer it.
        let (dx, dy) = phase.offset();
        let trusted = clip_level * TRUSTED;
        for by in (0..height.saturating_sub(1)).step_by(2) {
            let gy = ((by as u64 * gh as u64) / height as u64).min(gh as u64 - 1) as u32;
            for bx in (0..width.saturating_sub(1)).step_by(2) {
                let mut rgb = [0.0f32; 3];
                let mut clipped = false;
                // The quad's two greens, kept apart rather than summed. Their
                // *difference* is what says whether this quad is one colour or
                // two — see below, and see `chroma` for what it costs when
                // nobody asks.
                let mut greens = [0.0f32; 2];
                let mut seen = 0usize;
                for j in 0..2u32 {
                    for i in 0..2u32 {
                        let (x, y) = (bx + i, by + j);
                        let v = mosaic[y as usize * width as usize + x as usize];
                        clipped |= v >= trusted;
                        // The same rule as `colour_at` in the shader, and the
                        // only place this module knows a mosaic is a mosaic.
                        let (px, py) = (x + dx, y + dy);
                        let c = if (px + py) % 2 == 1 {
                            1
                        } else if py % 2 == 0 {
                            0
                        } else {
                            2
                        };
                        // Green twice per quad, so it is halved on the way in.
                        rgb[c] += if c == 1 { v * 0.5 } else { v };
                        // Counted rather than indexed by position: which two
                        // corners of the quad hold green depends on the pattern
                        // phase, and a formula for that is a fourth place to get
                        // the phase wrong.
                        if c == 1 && seen < 2 {
                            greens[seen] = v;
                            seen += 1;
                        }
                    }
                }

                quads += 1;
                blown += u32::from(clipped);

                let cell = gy as usize * gw as usize + column[bx as usize] as usize;
                for (c, v) in rgb.iter().enumerate() {
                    sum[cell * 3 + c] += v;
                }
                count[cell] += 1.0;

                // A quad that spans an edge is not a colour.
                //
                // Its red comes from one side of the edge and its blue from the
                // other, so the triple is a colour that is in neither — and
                // since the whole point of this field is to be *borrowed*, a
                // colour nothing in the frame has is the worst possible thing to
                // put in it. On a dark twig against a blown sky the straddling
                // quads are the only unclipped ones anywhere near, so their
                // invented colour is what the diffusion below then spreads
                // across the entire sky. Measured on a synthetic frame that is
                // neutral everywhere: the quad on the edge reads (0.32, 0.47,
                // 0.05) against the truth's (0.04, 0.10, 0.05), and the sky
                // three hundred pixels away inherits it.
                //
                // The two greens are what detect it. They sit on opposite
                // corners of the quad and see the same light in a flat area, so
                // a difference between them is an edge inside the quad — and it
                // is the only measurement available that compares like with
                // like, since red and blue have one sample each and no way to
                // disagree with themselves.
                let level = greens[0] + greens[1];
                let flat = (greens[0] - greens[1]).abs() <= FLAT_ENOUGH * level;
                if !clipped && flat {
                    // Weighted by the quad's own brightness, so the reference
                    // is the colour of the *bright* light near a highlight
                    // rather than an average that a large shadow would drag
                    // towards its own cast. One weight for all three channels:
                    // weighting each by itself would bias the chroma outwards,
                    // which is the direction that produces the artefact this
                    // whole field exists to avoid.
                    let weight = rgb[1].max(0.0);
                    for (c, v) in rgb.iter().enumerate() {
                        chroma[c] += v * weight;
                    }
                    chroma_weight += weight;
                }
            }
        }

        let mut data = vec![0.0f32; cells * 3];
        for cell in 0..cells {
            // A texel with no complete quad cannot happen for one covering 2x2
            // or more, which `dimensions` guarantees. Left at zero it would read
            // as a black neighbourhood and pull the local tone the wrong way — a
            // silent wrong answer where a visible one would be better.
            let n = count[cell].max(1.0);
            for c in 0..3 {
                data[cell * 3 + c] = sum[cell * 3 + c] / n;
            }
        }

        let sigma = (gw.max(gh) as f32 / SIGMA_DIVISOR).max(1.0);
        blur(&mut data, gw, gh, sigma);
        let chroma_known = chroma_weight > 0.0;
        let chroma = if chroma_known {
            [
                chroma[0] / chroma_weight,
                chroma[1] / chroma_weight,
                chroma[2] / chroma_weight,
            ]
        } else {
            // Nothing in the frame stayed inside the sensor's range, so there
            // is no light to ask about. Flagged rather than guessed: what
            // neutral *means* in camera RGB depends on the white balance, which
            // is an edit and not a property of the mosaic, so the shader
            // supplies it. See `unclipped_colour`.
            [0.0; 3]
        };

        Guide {
            data,
            chroma,
            chroma_known,
            clipped: if quads == 0 {
                0.0
            } else {
                blown as f32 / quads as f32
            },
            width: gw,
            height: gh,
        }
    }
}

impl Guide {
    /// How strong the haze veil is, in white-balanced camera RGB.
    ///
    /// # What this is, and what it is not
    ///
    /// The dark-channel prior's atmospheric light, reduced to one number. The
    /// prior observes that in a clear patch of a photograph at least one colour
    /// channel goes nearly black, so wherever *no* channel does, something
    /// white is being added — and how much is the strength of the veil. Haze is
    /// neutral once white balance has run, which is why a scalar is enough: its
    /// colour is the frame's own white and only its magnitude is in question.
    ///
    /// **The spatial minimum the prior asks for is not taken here.** The guide
    /// is already an edge-aware blur spanning about 190 image pixels, so the
    /// per-texel minimum over the three channels stands in for a minimum over a
    /// window. On a smooth region the two agree; at an edge the blur has already
    /// mixed the two sides, so the estimate is *higher* than the true dark
    /// channel and the correction it produces is weaker. Erring toward
    /// under-correcting is the right direction for a control whose failure mode
    /// is a halo.
    ///
    /// A high percentile rather than the maximum, for the reason every estimator
    /// of a maximum uses one: a single blown specular highlight would otherwise
    /// decide the whole frame's haze.
    ///
    /// # What is returned, and the mistake it corrects
    ///
    /// The **intensity** at the haziest texels, not the dark channel's own value
    /// there. The prior locates the airlight by finding where the dark channel is
    /// highest — that is the haziest part of the frame — and then reads the
    /// *brightness* at those texels. Returning the dark channel instead is a
    /// shortcut that reads far too low, because the whole point of the haziest
    /// region is that its darkest channel is still well below its brightest.
    ///
    /// The consequence was measurable and large: `t = 1 - w * amount * dark / A`
    /// with `A` too small drives the transmission to its floor, and dehaze
    /// overshot by about five times. A synthetic veil that took 45% of the light
    /// was undone at an amount of **0.2**, and full strength put the frame seven
    /// times further from the truth than the veil had.
    pub fn veil(&self, wb: [f32; 3]) -> f32 {
        // Histograms, because this runs on every slider move and a sort of a
        // hundred and fifty thousand values does not.
        const BINS: usize = 1024;
        const PERCENTILE: f32 = 0.999;
        let dark_of = |t: &[f32]| {
            (t[0] * wb[0])
                .min(t[1] * wb[1])
                .min(t[2] * wb[2])
                .clamp(0.0, 1.0)
        };
        let mut counts = [0u32; BINS];
        let mut total = 0u32;
        for texel in self.data.chunks_exact(3) {
            counts[((dark_of(texel) * (BINS - 1) as f32) as usize).min(BINS - 1)] += 1;
            total += 1;
        }
        if total == 0 {
            return 0.0;
        }
        // Where the dark channel's top tenth of a percent begins.
        let target = (total as f32 * PERCENTILE) as u32;
        let mut seen = 0u32;
        let mut threshold = 1.0f32;
        for (bin, count) in counts.iter().enumerate() {
            seen += count;
            if seen >= target {
                threshold = bin as f32 / BINS as f32;
                break;
            }
        }
        // And how bright the frame is there. The brightest channel, because the
        // airlight is what a fully hazy pixel would read and haze is neutral
        // once white balance has run — so its channels agree, and the largest is
        // the least corrupted by whatever scene is showing through.
        let mut sum = 0.0f64;
        let mut n = 0u32;
        for texel in self.data.chunks_exact(3) {
            if dark_of(texel) >= threshold {
                let bright = (texel[0] * wb[0])
                    .max(texel[1] * wb[1])
                    .max(texel[2] * wb[2]);
                sum += f64::from(bright);
                n += 1;
            }
        }
        if n == 0 {
            return threshold.max(1e-4);
        }
        ((sum / f64::from(n)) as f32).max(1e-4)
    }
}

/// Separable edge-aware blur, weighted by distance and by brightness together.
///
/// Green is the brightness the weights are measured on: it is what a Bayer
/// sensor has most of and what luminance is mostly made of, and using it here
/// avoids needing the camera matrix, which belongs to the profile and not to
/// the mosaic.
fn blur(data: &mut [f32], width: u32, height: u32, sigma: f32) {
    let reach = (2.0 * sigma).ceil() as i32;
    let spatial = -0.5 / (sigma * sigma);
    let range = -0.5 / (RANGE_STOPS * RANGE_STOPS);
    let (w, h) = (width as i32, height as i32);

    // Horizontal, then vertical, over the same buffer through a scratch copy.
    for axis in 0..2 {
        let source = data.to_vec();
        let brightness = |x: i32, y: i32| -> f32 {
            let i = (y.clamp(0, h - 1) as usize * width as usize + x.clamp(0, w - 1) as usize) * 3;
            // Log brightness, so "a stop" is a distance and a shadow is not
            // compressed into a range term that cannot separate anything.
            source[i + 1].max(1e-6).log2()
        };
        for y in 0..h {
            for x in 0..w {
                let centre = brightness(x, y);
                let mut acc = [0.0f32; 3];
                let mut total = 0.0f32;
                for step in -reach..=reach {
                    let (sx, sy) = if axis == 0 {
                        (x + step, y)
                    } else {
                        (x, y + step)
                    };
                    let d = brightness(sx, sy) - centre;
                    let weight = ((step * step) as f32 * spatial + d * d * range).exp();
                    let i = (sy.clamp(0, h - 1) as usize * width as usize
                        + sx.clamp(0, w - 1) as usize)
                        * 3;
                    for (c, acc) in acc.iter_mut().enumerate() {
                        *acc += source[i + c] * weight;
                    }
                    total += weight;
                }
                let o = (y as usize * width as usize + x as usize) * 3;
                for (c, acc) in acc.iter().enumerate() {
                    data[o + c] = acc / total;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An RGGB mosaic from a function of position, in camera RGB.
    fn mosaic(width: u32, height: u32, rgb: impl Fn(u32, u32) -> [f32; 3]) -> Vec<f32> {
        (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .map(|(x, y)| {
                let c = rgb(x, y);
                match (x % 2 == 0, y % 2 == 0) {
                    (true, true) => c[0],
                    (false, false) => c[2],
                    _ => c[1],
                }
            })
            .collect()
    }

    #[test]
    fn every_texel_covers_a_whole_bayer_quad() {
        // The claim `dimensions` exists for: no texel may be smaller than 2x2,
        // or two of its three channels have no sample to average.
        for (w, h) in [
            (6024, 4024),
            (4024, 6024),
            (64, 64),
            (4, 4),
            (2, 2),
            (800, 3),
        ] {
            let (gw, gh) = dimensions(w, h);
            assert!(gw >= 1 && gh >= 1, "{w}x{h} gave an empty guide");
            assert!(
                gw * 2 <= w.max(2) && gh * 2 <= h.max(2),
                "{w}x{h} gave {gw}x{gh}, whose texels are under 2x2"
            );
            assert!(gw <= MAX_EDGE && gh <= MAX_EDGE, "{w}x{h} exceeded the cap");
            assert!(
                (gw * gh) as usize * 3 <= CAPACITY,
                "{w}x{h} needs more than the allocation reserves"
            );
        }
    }

    #[test]
    fn a_flat_frame_gives_a_flat_guide() {
        // Including at the border, where the kernel is clamped: normalising by
        // the weight actually spent is what keeps the edges from darkening, and
        // a guide that darkened at the edges would recover the corners of every
        // photograph differently from its middle.
        let data = mosaic(128, 96, |_, _| [0.4, 0.5, 0.2]);
        let guide = Guide::build(&data, 128, 96, BayerPhase::Rggb, 1.0);
        for (i, texel) in guide.data.chunks_exact(3).enumerate() {
            for (c, (v, expected)) in texel.iter().zip([0.4, 0.5, 0.2]).enumerate() {
                assert!(
                    (v - expected).abs() < 1e-4,
                    "texel {i} channel {c} is {v}, not {expected}"
                );
            }
        }
    }

    #[test]
    fn the_guide_carries_the_frames_own_colour() {
        // Each channel comes back where it went in. A phase mix-up here would
        // give the local operator a red frame's brightness for a blue one, which
        // is invisible until a picture looks wrong.
        let data = mosaic(64, 64, |_, _| [0.8, 0.4, 0.1]);
        let guide = Guide::build(&data, 64, 64, BayerPhase::Rggb, 1.0);
        let mid = ((guide.height / 2) * guide.width + guide.width / 2) as usize * 3;
        assert!((guide.data[mid] - 0.8).abs() < 1e-3);
        assert!((guide.data[mid + 1] - 0.4).abs() < 1e-3);
        assert!((guide.data[mid + 2] - 0.1).abs() < 1e-3);
    }

    #[test]
    fn a_quad_on_an_edge_does_not_invent_a_colour() {
        // The chroma field is *borrowed* — a blown pixel takes its colour from
        // it — so a colour that is nowhere in the frame is the worst thing that
        // can be in it. A quad straddling an edge produces exactly that: its red
        // sample comes from one side and its blue from the other.
        //
        // This is not hypothetical. On a bare twig against a blown sky the
        // straddling quads are the *only* unclipped ones anywhere near, so the
        // invented colour is what the diffusion then spreads over the whole sky
        // — which is what a green cast around a tree turned out to be.
        //
        // A blown neutral sky, with a dark neutral stripe down it. Nothing here
        // has any colour, so anything the guide reports is something it made.
        let (w, h) = (64u32, 64u32);
        //
        // The edge *ramps*, and takes two goes to get right. A hard step from
        // sky to stripe is not enough: every quad touching it holds a sample at
        // the clip level and is thrown out by the trust test before the flat
        // test is reached. And a stripe starting on an even column is not enough
        // either, because the quad grid then falls either side of the boundary
        // and nothing straddles anything. What actually happens on a twig is
        // both together — an edge that lands on an odd column and passes through
        // intermediate values on the way down, so a quad can hold two very
        // different brightnesses while every sample in it is honest.
        // Repeated across the frame rather than once down the middle. The
        // colour of the light that did not clip is now one number for the whole
        // photograph, and an average is robust to a handful of bad quads by
        // construction — so a single stripe proves nothing about the test that
        // rejects them. A frame ruled with stripes is the case that separates
        // them: it is what a bare tree against a bright sky looks like to a
        // guide texel, and there the invented colours are the majority.
        let level = |x: u32| match x % 8 {
            0..=2 => 1.0,
            3 => 0.90,
            4 => 0.50,
            5 => 0.05,
            6 => 0.50,
            7 => 0.90,
            _ => 1.0,
        };
        let data = mosaic(w, h, |x, _| [level(x); 3]);
        let guide = Guide::build(&data, w, h, BayerPhase::Rggb, 1.0);

        // Two acceptable answers and one unacceptable one. Reporting no colour
        // is honest here — every quad in this frame straddles an edge, so there
        // is nothing to report and the shader renders neutral. Reporting a
        // colourless colour would be fine too. Reporting a *colour*, on a frame
        // that has none, is the failure: it is invented, and it is now the only
        // answer the whole photograph gets.
        if !guide.chroma_known {
            println!("no unclipped light survived, which is the honest answer here");
            return;
        }

        // One triple for the frame, so one number to check — and the stakes are
        // higher than when this was a field, not lower: a bad quad no longer
        // spoils a neighbourhood, it spoils the only answer there is.
        let c = guide.chroma;
        let mean = (c[0] + c[1] + c[2]) / 3.0;
        assert!(mean > 1e-6, "the frame's colour came out black");
        let cast = (c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])) / mean;
        println!("invented cast {cast:.4} on a frame that is neutral everywhere");
        assert!(
            cast < 0.02,
            "invented a cast of {cast:.3} from a frame with no colour in it"
        );
    }

    #[test]
    fn brightness_does_not_cross_a_hard_edge() {
        // The reason the blur is edge-aware. A dark half against a bright half:
        // a plain Gaussian would carry the bright side's value well into the
        // dark one, and every such boundary in a photograph would grow a halo
        // once highlights were pulled down.
        let data = mosaic(256, 256, |x, _| {
            let v = if x < 128 { 0.05 } else { 0.8 };
            [v, v, v]
        });
        let guide = Guide::build(&data, 256, 256, BayerPhase::Rggb, 1.0);
        let row = (guide.height / 2) as usize * guide.width as usize;
        let half = guide.width as usize / 2;
        // Four texels back from the boundary, well inside the blur's reach.
        let dark = guide.data[(row + half - 4) * 3 + 1];
        let bright = guide.data[(row + half + 4) * 3 + 1];
        assert!(
            dark < 0.12,
            "the bright half bled into the dark one: {dark:.4} against 0.05"
        );
        assert!(
            bright > 0.7,
            "the dark half bled into the bright one: {bright:.4} against 0.8"
        );
    }

    #[test]
    fn a_gradient_survives_the_blur() {
        // The other half of the same claim: the filter must still be a blur.
        // Edge-aware weighting that refused to move anything would pass the
        // test above and be useless.
        let data = mosaic(256, 256, |x, _| {
            let v = 0.1 + 0.5 * x as f32 / 255.0;
            [v, v, v]
        });
        let guide = Guide::build(&data, 256, 256, BayerPhase::Rggb, 1.0);
        let row = (guide.height / 2) as usize * guide.width as usize;
        let left = guide.data[(row + 8) * 3 + 1];
        let right = guide.data[(row + guide.width as usize - 8) * 3 + 1];
        assert!(
            right - left > 0.35,
            "the gradient was flattened: {left:.3} to {right:.3}"
        );
    }
}
