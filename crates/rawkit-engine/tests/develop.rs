//! The develop stage: white balance, camera profile, exposure, tone map.
//!
//! These test *properties*, not numbers. Asserting that a particular input
//! produces 0.4213 would pin the tone curve's exact shape, and the shape is
//! expected to change — it is a taste problem with its own iteration loop. What
//! must not change is the behaviour the rest of the pipeline relies on: mid-grey
//! stays put, nothing clips, order is preserved, and a stop is a stop.
//!
//! `cargo test -p rawkit-engine --test develop -- --ignored`

use rawkit_editstate::EditState;
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const N: u32 = 64;

/// A camera whose native primaries *are* sRGB's, so the profile stage has
/// nothing to do and its transform comes out as the identity.
///
/// Note this is not the identity matrix: a profile stores XYZ-to-camera, so the
/// camera that needs no correction is the one holding the inverse of sRGB's
/// primaries. Using the identity here instead would silently insert a real
/// colour transform into tests that mean to isolate tone and white balance —
/// which is exactly what it did before this comment existed.
fn neutral_profile() -> CameraProfile {
    use rawkit_engine::profile::{invert, XYZ_FROM_SRGB};
    CameraProfile::from_color_matrix(invert(&XYZ_FROM_SRGB).expect("sRGB primaries are invertible"))
}

/// Render a flat frame of one scene-linear value and return the developed
/// result at its centre.
///
/// A flat mosaic demosaics to a flat image, so this isolates the develop stage
/// from the interpolation entirely: whatever comes back is the tone response of
/// the given value.
fn develop(gpu: &Gpu, renderer: &Renderer, value: f32, state: &EditState) -> [f32; 3] {
    let cfa = vec![value; (N * N) as usize];
    let out = renderer
        .run(
            gpu,
            &Frame {
                data: &cfa,
                width: N,
                height: N,
                phase: BayerPhase::Rggb,
                as_shot_wb: [1.0, 1.0, 1.0],
                // These measure the tone curve, with inputs far above full
                // scale on purpose. Reconstruction would rewrite exactly the
                // values under test.
                clip_level: f32::INFINITY,
                profile: neutral_profile(),
                recorded_orientation: rawkit_editstate::Orientation::AsShot,
            },
            state,
            Output::Display,
        )
        .expect("render failed")
        .pixels;
    let i = ((N / 2 * N + N / 2) * 4) as usize;
    [out[i], out[i + 1], out[i + 2]]
}

#[test]
#[ignore = "requires a GPU adapter"]
fn mid_grey_survives_the_tone_map() {
    // If the tone map moved mid-grey, it would be a second brightness control
    // and exposure would no longer mean what it says. Everything about the
    // scene-linear core depends on exposure being the one thing that moves
    // brightness.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);
    // 0.072 and not 0.18, and that is the correction rather than a fudge: a
    // camera meters below the middle to keep its highlights, so a *photographed*
    // mid-grey lands near 7% of the sensor's full scale, not 18%. Measured
    // across ten frames against this body's own rendering of them. Asserting
    // 0.18 here is what let every default render sit 1.3 stops dark.
    let out = develop(&gpu, &renderer, 0.072, &EditState::default());
    for (c, v) in out.iter().enumerate() {
        assert!(
            (v - 0.18).abs() < 0.005,
            "a photographed mid-grey rendered to {v} in channel {c}, not 0.18"
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn highlights_roll_off_instead_of_clipping() {
    // A sensor routinely records values past full scale, and a clipping curve
    // turns those into flat patches — worse, into *coloured* flat patches when
    // one channel saturates first. That is the magenta-highlight artefact, and
    // the tone map is the first line of defence against it.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);
    let state = EditState::default();

    let bright = develop(&gpu, &renderer, 4.0, &state)[0];
    let brighter = develop(&gpu, &renderer, 40.0, &state)[0];

    assert!(bright < 1.0, "4x full scale already clipped: {bright}");
    assert!(brighter < 1.0, "40x full scale clipped: {brighter}");
    assert!(
        brighter > bright,
        "the curve stopped responding between 4x and 40x ({bright} -> {brighter}); \
         detail above full scale is being thrown away"
    );
    assert!(
        brighter > 0.9,
        "40x full scale should be near white, got {brighter}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_curve_is_monotonic() {
    // A non-monotonic curve inverts local contrast: a brighter part of the scene
    // comes out darker. It looks like a solarisation artefact and is not the
    // kind of thing that gets noticed in one image.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);
    let state = EditState::default();

    let mut previous = -1.0f32;
    for step in 0..24 {
        let scene = 0.01 * 1.4f32.powi(step);
        let display = develop(&gpu, &renderer, scene, &state)[0];
        assert!(
            display > previous,
            "scene {scene} rendered to {display}, not above the previous {previous}"
        );
        previous = display;
    }
    assert!(previous <= 1.0, "the curve escaped its range: {previous}");
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_stop_of_exposure_is_a_stop_of_light() {
    // The property that makes exposure meaningful in a scene-linear pipeline:
    // +1 EV on half the light must land exactly where the full light lands with
    // no adjustment. Note this is checked *through* the tone map without knowing
    // anything about its shape — which is why it stays true when the curve
    // changes.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);

    let mut lifted = EditState::default();
    lifted.tone.exposure_ev = 1.0;

    for scene in [0.02, 0.09, 0.35, 1.5] {
        let reference = develop(&gpu, &renderer, scene * 2.0, &EditState::default())[0];
        let exposed = develop(&gpu, &renderer, scene, &lifted)[0];
        assert!(
            (reference - exposed).abs() < 1e-4,
            "+1 EV on {scene} gave {exposed}, but {} unadjusted gives {reference}",
            scene * 2.0
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn white_balance_multiplies_channels_independently() {
    // Scene-linear is what makes white balance three multiplies rather than a
    // colour-appearance model. Stated exactly rather than as an ordering:
    // channel c of a neutral frame with multiplier m must equal a neutral frame
    // of `value * m` rendered with no white balance at all.
    //
    // **With hue preservation off, and that is not a workaround.** The equality
    // above needs everything after the multiply to be per-channel, and the tone
    // map is deliberately no longer per-channel by default — it asks the curve
    // once at the colour's largest channel and applies that gain to all three,
    // so a saturated colour keeps its hue as it is compressed. Comparing a
    // channel of a balanced render against a *neutral* render of the same value
    // is comparing two colours the curve now treats differently, on purpose.
    //
    // So this pins the per-channel path, where the property is exactly true,
    // and `hue_preservation_keeps_a_colours_ratios` below pins the default one.
    let per_channel = EditState {
        tone: rawkit_editstate::Tone {
            hue_preservation: 0.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);
    let wb = [2.0f32, 1.0, 1.5];
    let value = 0.1f32;

    let cfa = vec![value; (N * N) as usize];
    let out = renderer
        .run(
            &gpu,
            &Frame {
                data: &cfa,
                width: N,
                height: N,
                phase: BayerPhase::Rggb,
                as_shot_wb: wb,
                clip_level: f32::INFINITY,
                profile: neutral_profile(),
                recorded_orientation: rawkit_editstate::Orientation::AsShot,
            },
            &per_channel,
            Output::Display,
        )
        .expect("render failed")
        .pixels;
    let i = ((N / 2 * N + N / 2) * 4) as usize;

    for (c, m) in wb.iter().enumerate() {
        let expected = develop(&gpu, &renderer, value * m, &per_channel)[c];
        assert!(
            (out[i + c] - expected).abs() < 1e-4,
            "channel {c} with multiplier {m} gave {}, but {} unbalanced gives {expected}",
            out[i + c],
            value * m
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn setting_a_temperature_warms_or_cools_the_render() {
    // The white-balance slider, end to end: an explicit temperature now becomes
    // multipliers through the profile instead of being refused. Direction is the
    // thing to pin — a slider that works perfectly and backwards is a real
    // failure mode, and no test of the maths alone would catch the wiring being
    // reversed between EditState and the kernel.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);
    let profile = CameraProfile::from_color_matrix([
        [0.6941, -0.2164, -0.0644],
        [-0.3850, 1.1349, 0.2779],
        [-0.0031, 0.1055, 0.6511],
    ]);

    let cfa = vec![0.2f32; (N * N) as usize];
    let render_at = |kelvin: f32| {
        let mut state = EditState::default();
        state.white_balance.temperature_k = Some(kelvin);
        let out = renderer
            .run(
                &gpu,
                &Frame {
                    data: &cfa,
                    width: N,
                    height: N,
                    phase: BayerPhase::Rggb,
                    as_shot_wb: [1.0, 1.0, 1.0],
                    clip_level: f32::INFINITY,
                    profile: profile.clone(),
                    recorded_orientation: rawkit_editstate::Orientation::AsShot,
                },
                &state,
                Output::Display,
            )
            .expect("render failed")
            .pixels;
        let i = ((N / 2 * N + N / 2) * 4) as usize;
        [out[i], out[i + 1], out[i + 2]]
    };

    let cool = render_at(3000.0);
    let warm = render_at(9000.0);
    assert!(
        warm[0] / warm[2] > cool[0] / cool[2],
        "raising the stated temperature did not warm the image: \
         3000K gave {cool:?}, 9000K gave {warm:?}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn as_shot_reports_a_plausible_temperature() {
    // "As Shot 5200 K" is a label the UI has to produce from multipliers, and
    // the render uses the same conversion to pick its matrix. Checking it here
    // means the number shown to a user and the number used to render cannot
    // drift apart.
    let profile = CameraProfile::from_color_matrix([
        [0.6941, -0.2164, -0.0644],
        [-0.3850, 1.1349, 0.2779],
        [-0.0031, 0.1055, 0.6511],
    ]);
    let cfa = vec![0.2f32; (N * N) as usize];
    // The real ILCE-6400 sample's as-shot multipliers.
    let frame = Frame {
        data: &cfa,
        width: N,
        height: N,
        phase: BayerPhase::Rggb,
        as_shot_wb: [2.750, 1.0, 1.695],
        clip_level: 1.0,
        recorded_orientation: rawkit_editstate::Orientation::AsShot,
        profile,
    };
    let (temperature, tint) = frame.as_shot_temperature();
    println!("as-shot: {temperature:.0} K, tint {tint:.1}");
    assert!(
        (2000.0..12000.0).contains(&temperature),
        "as-shot temperature {temperature} K is not a temperature a camera would report"
    );
    assert!(tint.abs() < 60.0, "implausible as-shot tint {tint}");
}

#[test]
#[ignore = "requires a GPU adapter"]
fn hue_preservation_keeps_a_colours_ratios() {
    // The property the control exists to deliver, stated as a ratio because
    // that is what a hue *is*. At full preservation the curve is asked once, at
    // the colour's largest channel, and the answer is applied to all three as a
    // single gain — so whatever proportion the channels arrived in, they leave
    // in.
    //
    // Compressing them separately cannot do this and the difference is not
    // subtle: the largest channel compresses proportionally hardest, so every
    // colour walks towards white along a path that is not constant hue. Three
    // stops of exposure on a real frame rotates an amber window light by 13.9
    // degrees, with nothing clipped anywhere.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);

    // A strongly coloured light, made with the white balance because a flat
    // mosaic is the only way to isolate the develop stage from the demosaic —
    // the multipliers are what put three different values in front of the tone
    // map. Far enough up that the sigmoid is doing real work: at this value the
    // largest channel is compressed to about two thirds of the way to white,
    // which is where a per-channel curve does its damage.
    let wb = [4.0f32, 1.0, 0.5];
    let value = 0.25f32;
    let cfa = vec![value; (N * N) as usize];

    let mut error = std::collections::BTreeMap::new();
    for keep in [0u32, 1] {
        let state = EditState {
            tone: rawkit_editstate::Tone {
                hue_preservation: keep as f32,
                ..Default::default()
            },
            ..Default::default()
        };
        let out = renderer
            .run(
                &gpu,
                &Frame {
                    data: &cfa,
                    width: N,
                    height: N,
                    phase: BayerPhase::Rggb,
                    as_shot_wb: wb,
                    clip_level: f32::INFINITY,
                    profile: neutral_profile(),
                    recorded_orientation: rawkit_editstate::Orientation::AsShot,
                },
                &state,
                Output::Display,
            )
            .expect("render failed")
            .pixels;
        let i = ((N / 2 * N + N / 2) * 4) as usize;
        let (r, g, b) = (out[i], out[i + 1], out[i + 2]);
        // Against the ratios the light arrived in, which the multipliers set.
        let red = (r / g - wb[0] / wb[1]).abs();
        let blue = (b / g - wb[2] / wb[1]).abs();
        println!("keep {keep}: {r}/{g}/{b}  red error {red:.4}  blue error {blue:.4}");
        error.insert(keep, (red, blue));
    }

    // **The two ends against each other, not against an absolute tolerance.**
    //
    // This asserted `red < 0.002` at full preservation, and the vault recorded
    // why that passed: the test colour happened to sit at 0.752 on the curve,
    // below the bleach, "a small piece of luck worth knowing about if anyone
    // retunes the threshold". `BASE_CONTRAST` is not a retune of the threshold
    // but it moves where a colour sits, and the luck ran out — the shape step
    // is applied per channel, so a curve that is always on moves ratios on its
    // own account and an absolute bound on them measures the curve rather than
    // this control.
    //
    // The claim never needed an absolute bound. It is comparative: hue
    // preservation keeps a colour's ratios and per-channel compression does
    // not, and both ends go through the identical pipeline, so whatever else is
    // downstream cancels.
    //
    // Summed across both ratios rather than asserted on each. The two channels
    // are not equally informative and pretending they are would pick the
    // threshold off the weaker one: measured here, preservation buys **8.4x**
    // on red and **1.9x** on blue. The blue ratio moves least because the
    // baseline curve is applied per channel and green and blue sit at
    // different depths below the pivot, so a power about the pivot moves their
    // ratio on its own account — see `BASE_CONTRAST`. That is the tone
    // *curve*, not the tone *map*, and it is per channel in every editor there
    // is; hue preservation is a claim about the compression, which is the
    // stage this measures.
    let (kept_red, kept_blue) = error[&1];
    let (lost_red, lost_blue) = error[&0];
    let (kept, lost) = (kept_red + kept_blue, lost_red + lost_blue);
    assert!(
        lost > kept * 3.0,
        "preservation bought almost nothing: {kept:.4} of total ratio error \
         against {lost:.4} — red {kept_red:.4}/{lost_red:.4}, blue \
         {kept_blue:.4}/{lost_blue:.4}"
    );
    // And the other end, so the pair cannot both be tiny: per-channel has to be
    // turning the colour by an amount somebody would see.
    assert!(
        lost_red > 0.5,
        "with preservation off the red ratio missed by only {lost_red:.4}, so \
         the per-channel curve is not turning the colour and this test proves \
         nothing"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_near_white_cast_bleaches_before_a_colour_does() {
    // Two colours at the same brightness, one a faint tint on a near-white and
    // one unmistakably coloured, and the bleach has to treat them differently.
    //
    // It is the property `HUE_BLEACH_NEUTRAL` exists for. A cast on a near-white
    // highlight is as likely to be flare, a clipping residue or the profile's
    // error as it is to be the subject, and the eye is least forgiving of one
    // there because it knows what colour a cloud is. A saturated highlight is
    // not ambiguous, and taking its colour away is the artefact rather than the
    // fix — which is what a single brightness threshold had to choose between,
    // because a grey cloud and an orange one sit at the same height.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);

    // High enough that the near-neutral is past its onset and low enough that
    // the coloured one is short of its own. Between the two thresholds is
    // exactly where the two answers have to differ; outside it they agree by
    // construction and the test would prove nothing.
    // The two share a largest multiplier on purpose. The onset is a function of
    // brightness *and* saturation, so two colours compared at different
    // brightnesses would not isolate which of the two moved the answer — and
    // the largest channel is what sets the brightness the bleach reads.
    // Green is 1.0 in both because the renderer normalises the multipliers by
    // it, and a largest channel that only matched *before* that division would
    // put the two colours at different brightnesses again.
    let value = 0.90f32;
    let pale = develop_colour(&gpu, &renderer, value, [1.10, 1.00, 0.95]);
    let vivid = develop_colour(&gpu, &renderer, value, [1.10, 0.50, 0.30]);

    fn saturation(rgb: [f32; 3]) -> f32 {
        let hi = rgb[0].max(rgb[1]).max(rgb[2]);
        let lo = rgb[0].min(rgb[1]).min(rgb[2]);
        if hi <= 0.0 {
            0.0
        } else {
            (hi - lo) / hi
        }
    }
    // What each arrived as, in the same measure: the multipliers are the colour
    // going in, and the render is the colour coming out.
    let asked_pale = saturation([1.10, 1.00, 0.95]);
    let asked_vivid = saturation([1.10, 0.50, 0.30]);
    println!(
        "pale  asked {asked_pale:.3} got {:.3}  {pale:?}\n\
         vivid asked {asked_vivid:.3} got {:.3}  {vivid:?}",
        saturation(pale),
        saturation(vivid)
    );

    // The tone map compresses, so neither comes out at the saturation it went
    // in with and the absolute numbers are not the point. The *ratio* is: the
    // pale one has to give up a larger share of its colour than the vivid one,
    // and by a margin too wide to be the curve's own doing.
    let pale_kept = saturation(pale) / asked_pale;
    let vivid_kept = saturation(vivid) / asked_vivid;
    assert!(
        vivid_kept > pale_kept * 1.5,
        "the bleach did not tell them apart: the pale cast kept {pale_kept:.3} of \
         its saturation and the vivid colour {vivid_kept:.3}. A single brightness \
         threshold gives these two the same answer, which is the thing \
         HUE_BLEACH_NEUTRAL is there to stop."
    );
    // And the other end, so the margin above cannot be met by an operator that
    // does nothing to either: the pale cast has to actually come off.
    //
    // The vivid colour is not asserted to be untouched, and that is not a
    // loosened bound. The profile matrix mixes the channels, so a saturated
    // colour's largest channel lands higher than a pale one's from the same
    // multiplier — 0.857 against 0.750 here — and at 0.857 the vivid colour is
    // past `HUE_BLEACH_FROM` on its own account. Both are bleaching; the claim
    // is that the pale one bleaches *sooner*, which is the ratio above.
    assert!(
        pale_kept < 0.6,
        "the pale cast kept {pale_kept:.3} of its saturation, so nothing came \
         off it and the ratio above is met by both colours being left alone"
    );
}

/// Render a flat frame through the given multipliers, so the develop stage sees
/// a colour rather than a neutral. A flat mosaic demosaics to a flat image, so
/// the multipliers are the only way to put three different values in front of
/// the tone map without the interpolation having an opinion.
fn develop_colour(gpu: &Gpu, renderer: &Renderer, value: f32, wb: [f32; 3]) -> [f32; 3] {
    let cfa = vec![value; (N * N) as usize];
    let out = renderer
        .run(
            gpu,
            &Frame {
                data: &cfa,
                width: N,
                height: N,
                phase: BayerPhase::Rggb,
                as_shot_wb: wb,
                clip_level: f32::INFINITY,
                profile: neutral_profile(),
                recorded_orientation: rawkit_editstate::Orientation::AsShot,
            },
            &EditState::default(),
            Output::Display,
        )
        .expect("render failed")
        .pixels;
    let i = ((N / 2 * N + N / 2) * 4) as usize;
    [out[i], out[i + 1], out[i + 2]]
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_blend_never_moves_the_brightest_channel() {
    // The invariant that makes a brightness-dependent weight safe to have at
    // all. All three candidates the tone map mixes between agree exactly on a
    // colour's largest channel: the per-channel curve gives `curve(norm)`, the
    // ratio-preserving path gives `norm * curve(norm) / norm`, and the neutral
    // the bleach heads for is `curve(norm)` in every channel. One number, three
    // times.
    //
    // So the control moves chroma and never brightness — and a weight that
    // varies with brightness therefore cannot fold the curve back on itself,
    // which would read as a contour in a sky rather than as a bug here.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);
    let wb = [3.0f32, 1.0, 0.4];

    // Across the whole curve, including well past the bleach threshold, so the
    // taper is active for the upper values and not for the lower ones.
    for value in [0.02f32, 0.07, 0.2, 0.5, 1.5, 6.0] {
        let mut peak = f32::NEG_INFINITY;
        for keep in [0.0f32, 0.5, 1.0] {
            let state = EditState {
                tone: rawkit_editstate::Tone {
                    hue_preservation: keep,
                    ..Default::default()
                },
                ..Default::default()
            };
            let cfa = vec![value; (N * N) as usize];
            let out = renderer
                .run(
                    &gpu,
                    &Frame {
                        data: &cfa,
                        width: N,
                        height: N,
                        phase: BayerPhase::Rggb,
                        as_shot_wb: wb,
                        clip_level: f32::INFINITY,
                        profile: neutral_profile(),
                        recorded_orientation: rawkit_editstate::Orientation::AsShot,
                    },
                    &state,
                    Output::Display,
                )
                .expect("render failed")
                .pixels;
            let i = ((N / 2 * N + N / 2) * 4) as usize;
            let here = out[i].max(out[i + 1]).max(out[i + 2]);
            if peak.is_finite() {
                assert!(
                    (here - peak).abs() < 2e-3,
                    "at {value} the brightest channel moved to {here} at keep {keep}, \
                     against {peak} — the control is changing brightness, which it \
                     must not"
                );
            }
            peak = here;
        }
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_colour_keeps_its_hue_until_it_is_bright_enough_to_bleach() {
    // The two halves of the operator, and they have to be measured separately
    // because one was built by confusing them. Tapering towards the *per-channel*
    // curve looked like a bleach and was not: per-channel desaturates and
    // rotates at the same time, so it handed the artefact back along with the
    // look — 18.9 degrees of drift on a real frame where the untapered operator
    // left 0.2.
    //
    // A bleach desaturates along **constant hue**. So: below the threshold a
    // colour holds both, and above it saturation falls while hue stays put.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let renderer = Renderer::new(&gpu);
    let wb = [3.0f32, 1.0, 0.4];

    let hue_of = |c: [f32; 3]| {
        let (max, min) = (c[0].max(c[1]).max(c[2]), c[0].min(c[1]).min(c[2]));
        let span = max - min;
        if span <= 0.0 {
            return 0.0;
        }
        // The red sector, which is where these multipliers put it. Degrees, so
        // the tolerances below read as angles.
        60.0 * (c[1] - c[2]) / span
    };
    let saturation = |c: [f32; 3]| {
        let (max, min) = (c[0].max(c[1]).max(c[2]), c[0].min(c[1]).min(c[2]));
        if max <= 0.0 {
            0.0
        } else {
            (max - min) / max
        }
    };

    // Two values a stop apart, both comfortably below the bleach threshold.
    let dim = develop_colour(&gpu, &renderer, 0.08, wb);
    let mid = develop_colour(&gpu, &renderer, 0.16, wb);
    assert!(
        (hue_of(dim) - hue_of(mid)).abs() < 1.0,
        "a stop of light below the threshold turned the hue from {} to {}",
        hue_of(dim),
        hue_of(mid)
    );
    assert!(
        (saturation(dim) - saturation(mid)).abs() < 0.03,
        "a stop of light below the threshold moved saturation from {} to {}",
        saturation(dim),
        saturation(mid)
    );

    // And far above it, where a specular lives. Saturation must collapse and
    // the hue must not follow it.
    let blown = develop_colour(&gpu, &renderer, 8.0, wb);
    assert!(
        saturation(blown) < saturation(mid) * 0.4,
        "a colour eight times over full scale kept saturation {} against the \
         midtone's {} — nothing is bleaching",
        saturation(blown),
        saturation(mid)
    );
    assert!(
        (hue_of(blown) - hue_of(mid)).abs() < 6.0,
        "the bleach turned the hue from {} to {}, which is a per-channel curve's \
         answer rather than a desaturation",
        hue_of(mid),
        hue_of(blown)
    );
}
