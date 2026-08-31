//! Highlight reconstruction.
//!
//! # The artefact this exists to remove
//!
//! A sensor saturates in its own units, and white balance then scales the
//! channels apart. For a daylight scene green carries the most signal and
//! saturates first: its true value might be 1.5 but it records 1.0. Red is
//! nowhere near its own limit, records correctly, and is then multiplied up. The
//! result is a pixel where red and blue exceed green — magenta — in the one part
//! of the picture the eye most expects to be white.
//!
//! # Two regimes, and which tests are in which
//!
//! Reconstruction now borrows the colour of nearby light that did *not* clip,
//! from [`rawkit_engine::guide`]. That splits these tests in two, and the split
//! is worth knowing before adding another.
//!
//! Everything rendered through `render` uses a **uniform** frame, where either
//! every pixel is blown or none is. There is no unclipped light anywhere, so
//! there is nothing to ask, and reconstruction falls back to neutral — bit for
//! bit the arithmetic it used before it had anywhere to ask. Those tests pin
//! that fallback.
//!
//! Everything rendered through `render_field` puts a blown patch inside lit
//! surroundings, which is the case a photograph is actually in. Those tests pin
//! the propagation: that a highlight takes the colour of the light around it,
//! that it never comes out more saturated than that light, and that when the
//! *last* channel goes too it gives up and renders neutral again.
//!
//! `cargo test -p rawkit-engine --test highlights -- --ignored`

use rawkit_editstate::EditState;
use rawkit_engine::profile::{invert, XYZ_FROM_SRGB};
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const N: u32 = 64;

/// The default edit with capture sharpening turned off.
///
/// This measures highlight reconstruction, and sharpening is a different stage
/// three steps later: it moves a channel by a millionth, which is nothing to a
/// photograph and enough to fail "reconstruction never darkens a channel". The
/// same reason `pyramid.rs` renders with clipping disabled — a test should not
/// be measuring the stage it did not name.
fn unsharpened() -> EditState {
    let mut state = EditState::default();
    state.detail.sharpen_amount = 0.0;
    state
}

/// Sony ILCE-6400 daylight multipliers, which are what make green clip first.
const DAYLIGHT_WB: [f32; 3] = [2.75, 1.0, 1.70];

/// A camera whose primaries are sRGB's, so the profile stage contributes
/// nothing and what comes out is the reconstruction and the tone curve.
fn neutral_profile() -> CameraProfile {
    CameraProfile::from_color_matrix(invert(&XYZ_FROM_SRGB).expect("invertible"))
}

/// Render a flat frame of one camera-space colour.
fn render(gpu: &Gpu, camera: [f32; 3], wb: [f32; 3], clip: f32) -> [f32; 3] {
    let mut cfa = Vec::with_capacity((N * N) as usize);
    for y in 0..N {
        for x in 0..N {
            let channel = if (x + y) % 2 == 1 {
                1
            } else if y % 2 == 0 {
                0
            } else {
                2
            };
            cfa.push(camera[channel]);
        }
    }
    let out = Renderer::new(gpu)
        .run(
            gpu,
            &Frame {
                data: &cfa,
                width: N,
                height: N,
                phase: BayerPhase::Rggb,
                as_shot_wb: wb,
                clip_level: clip,
                profile: neutral_profile(),
                recorded_orientation: rawkit_editstate::Orientation::AsShot,
            },
            &unsharpened(),
            Output::Display,
        )
        .expect("render failed")
        .pixels;
    let i = ((N / 2 * N + N / 2) * 4) as usize;
    [out[i], out[i + 1], out[i + 2]]
}

/// How far a colour is from neutral, as a fraction of its brightest channel.
/// Zero is grey.
fn cast(rgb: [f32; 3]) -> f32 {
    let high = rgb[0].max(rgb[1]).max(rgb[2]);
    let low = rgb[0].min(rgb[1]).min(rgb[2]);
    if high <= 0.0 {
        0.0
    } else {
        (high - low) / high
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_clipped_green_channel_no_longer_goes_magenta() {
    // The exact situation from the sample photograph: green pinned at the
    // sensor limit while red and blue still have room. Without reconstruction
    // the balanced pixel is red-and-blue heavy; with it, the channel nobody
    // believes is raised to the level of the ones we do.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let clipped_green = [0.55, 1.0, 0.75];

    let raw = render(&gpu, clipped_green, DAYLIGHT_WB, f32::INFINITY);
    let fixed = render(&gpu, clipped_green, DAYLIGHT_WB, 1.0);

    println!("without reconstruction: {raw:?} cast {:.3}", cast(raw));
    println!("with reconstruction   : {fixed:?} cast {:.3}", cast(fixed));

    assert!(
        cast(raw) > 0.15,
        "the test case does not actually produce a cast: {raw:?}"
    );
    assert!(
        cast(fixed) < cast(raw) * 0.5,
        "reconstruction barely helped: cast went {:.3} -> {:.3}",
        cast(raw),
        cast(fixed)
    );

    // This comment used to argue the opposite of what it now says, and the
    // photograph is why. It read: do not force the pixel to neutral, because
    // blue is below its own limit and so is real measured data whose spread is
    // real scene colour. That is true of blue on its own and false of the pixel:
    // a colour is the ratio between three channels, and with one of them missing
    // the ratio is not known. Keeping two exactly and inventing the third
    // preserves nothing — it picks a different colour and renders it confidently.
    // See `a_blue_subject_does_not_turn_cyan_when_green_clips`, which is the
    // frame that settled it.
    //
    // So neutral *is* the answer, and green being no longer the channel left
    // behind follows from it rather than being the whole of it.
    assert!(
        fixed[1] >= fixed[0] * 0.99,
        "green is still short of red after reconstruction: {fixed:?}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_blue_subject_does_not_turn_cyan_when_green_clips() {
    // The failure `DSC01588.ARW` found: a bright blue sky with the sun behind a
    // tree, rendered with a mint-green arc across it that the camera's own JPEG
    // does not have.
    //
    // Green's threshold in balanced space is the *lowest* of the three — white
    // balance divides by it — so green clips first whatever colour the subject
    // is. On a neutral subject the survivors agree and raising green to them is
    // right. On a blue one they do not agree, and raising green to the brighter
    // of the two hands the pixel that channel's colour: green meets blue, red is
    // left behind, and the highlight comes out cyan and *more* saturated than
    // the sky it interrupts.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    // Balanced: (0.83, 1.00, 1.28) against thresholds (2.75, 1.00, 1.70) — only
    // green is gone, and the two survivors disagree by half a stop.
    let blue_sky = [0.30, 1.0, 0.75];

    let raw = render(&gpu, blue_sky, DAYLIGHT_WB, f32::INFINITY);
    let fixed = render(&gpu, blue_sky, DAYLIGHT_WB, 1.0);

    println!("without reconstruction: {raw:?} cast {:.3}", cast(raw));
    println!("with reconstruction   : {fixed:?} cast {:.3}", cast(fixed));

    assert!(
        cast(raw) > 0.15,
        "the test case does not actually produce a cast: {raw:?}"
    );
    assert!(
        cast(fixed) < cast(raw) * 0.5,
        "a clipped blue sky kept its cast: {:.3} -> {:.3} ({fixed:?})",
        cast(raw),
        cast(fixed)
    );
    // The specific shape of the artefact, named so a future change that brings
    // it back says which one it is: green arriving at blue while red stays put.
    assert!(
        (fixed[1] - fixed[2]).abs() > 0.02 || (fixed[0] - fixed[1]).abs() < 0.02,
        "green met blue and left red behind — the cyan arc is back: {fixed:?}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn an_unclipped_pixel_is_untouched() {
    // Reconstruction must be invisible everywhere it is not needed. Not
    // "almost invisible" — the run-up uses smoothstep, which is exactly zero
    // below its lower edge, so this is an equality and worth asserting as one.
    // A version that quietly shifted every midtone would be very hard to see
    // and very easy to ship.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    for colour in [
        [0.10, 0.20, 0.15],
        [0.30, 0.42, 0.25],
        [0.05, 0.08, 0.06],
        // Right below the run-up: green at 0.74 against a threshold of 1.0,
        // which `CLIP_RUNUP` puts the lower edge at 0.75. Tied to that constant
        // on purpose — the probe is only meaningful while it sits just outside.
        [0.20, 0.74, 0.30],
    ] {
        let off = render(&gpu, colour, DAYLIGHT_WB, f32::INFINITY);
        let on = render(&gpu, colour, DAYLIGHT_WB, 1.0);
        for c in 0..3 {
            assert_eq!(
                off[c], on[c],
                "reconstruction changed an unclipped pixel {colour:?}: {off:?} vs {on:?}"
            );
        }
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_fully_clipped_pixel_renders_neutral() {
    // When every channel is at its limit there is nothing left to take a level
    // from, and leaving the channels where they are renders the colour of the
    // white balance itself — a strong cast in the brightest part of the frame.
    // The only defensible answer is neutral.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let blown = render(&gpu, [1.0, 1.0, 1.0], DAYLIGHT_WB, 1.0);
    println!("fully clipped: {blown:?} cast {:.3}", cast(blown));
    assert!(
        cast(blown) < 0.06,
        "a blown highlight kept the white balance's colour: {blown:?}"
    );
    assert!(
        blown.iter().all(|&v| v > 0.7),
        "a blown highlight should be bright: {blown:?}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn reconstruction_never_darkens() {
    // Reconstruction only ever raises a channel it does not believe. If it
    // could lower one, a highlight would develop a dark rim where the run-up
    // begins — the artefact the smooth entry exists to avoid, arriving by
    // another route.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    for green in [0.980, 0.985, 0.990, 0.995, 1.0, 1.2] {
        let off = render(&gpu, [0.55, green, 0.75], DAYLIGHT_WB, f32::INFINITY);
        let on = render(&gpu, [0.55, green, 0.75], DAYLIGHT_WB, 1.0);
        for c in 0..3 {
            assert!(
                on[c] >= off[c] - 1e-6,
                "green {green}: reconstruction darkened channel {c}, {off:?} -> {on:?}"
            );
        }
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_transition_into_clipping_is_gradual() {
    // A hard switch at the clip point leaves a visible edge around every
    // highlight, which is a worse artefact than the magenta it replaces and
    // much harder to explain. Stepping across the boundary must not produce a
    // jump larger than the steps either side of it.
    //
    // Swept inside a frame that always has unclipped light in it. Rendered as a
    // uniform frame the sweep would cross from "no light to ask about" to "some"
    // partway along, and report that change of regime as a step at the clip
    // point — which is a fact about the test, not about the picture. No single
    // photograph is ever in both regimes at once.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let sample = |green: f32| {
        render_field(
            &gpu,
            BLOWN,
            |x, y| {
                if (192..320).contains(&x) && (192..320).contains(&y) {
                    [0.55, green, 0.75]
                } else {
                    [0.40, 0.42, 0.44]
                }
            },
            DAYLIGHT_WB,
            1.0,
            (256, 256, 24),
        )[1]
    };

    let steps: Vec<f32> = (0..12).map(|i| sample(0.970 + 0.005 * i as f32)).collect();
    let deltas: Vec<f32> = steps.windows(2).map(|w| w[1] - w[0]).collect();
    let largest = deltas.iter().cloned().fold(f32::MIN, f32::max);
    let typical = deltas.iter().sum::<f32>() / deltas.len() as f32;
    println!("green steps: {steps:?}");
    assert!(
        largest < typical.abs() * 4.0 + 0.02,
        "a step change at the clip point: deltas {deltas:?}"
    );
}

/// A frame whose camera colour varies with position, and the average of a
/// patch of the result.
///
/// The tests above all render a *uniform* frame, where every pixel is blown or
/// none is — so reconstruction has never had anywhere to borrow a colour from
/// and has always fallen back to neutral. Colour propagation is precisely the
/// case those cannot reach: a blown region with unclipped light around it.
fn render_field(
    gpu: &Gpu,
    side: u32,
    camera: impl Fn(u32, u32) -> [f32; 3],
    wb: [f32; 3],
    clip: f32,
    patch: (u32, u32, u32),
) -> [f32; 3] {
    let mut cfa = Vec::with_capacity((side * side) as usize);
    for y in 0..side {
        for x in 0..side {
            let channel = if (x + y) % 2 == 1 {
                1
            } else if y % 2 == 0 {
                0
            } else {
                2
            };
            cfa.push(camera(x, y)[channel]);
        }
    }
    let out = Renderer::new(gpu)
        .run(
            gpu,
            &Frame {
                data: &cfa,
                width: side,
                height: side,
                phase: BayerPhase::Rggb,
                as_shot_wb: wb,
                clip_level: clip,
                profile: neutral_profile(),
                recorded_orientation: rawkit_editstate::Orientation::AsShot,
            },
            &unsharpened(),
            Output::Display,
        )
        .expect("render failed")
        .pixels;

    let (cx, cy, half) = patch;
    let mut total = [0.0f64; 3];
    let mut n = 0.0f64;
    for y in cy - half..cy + half {
        for x in cx - half..cx + half {
            let i = ((y * side + x) * 4) as usize;
            for (c, total) in total.iter_mut().enumerate() {
                *total += out[i + c] as f64;
            }
            n += 1.0;
        }
    }
    [
        (total[0] / n) as f32,
        (total[1] / n) as f32,
        (total[2] / n) as f32,
    ]
}

/// Hue as an angle in degrees, for comparing two colours' *kind* rather than
/// their strength.
fn hue(rgb: [f32; 3]) -> f32 {
    let high = rgb[0].max(rgb[1]).max(rgb[2]);
    let low = rgb[0].min(rgb[1]).min(rgb[2]);
    let span = high - low;
    if span <= 1e-6 {
        return 0.0;
    }
    let h = if high == rgb[0] {
        ((rgb[1] - rgb[2]) / span).rem_euclid(6.0)
    } else if high == rgb[1] {
        2.0 + (rgb[2] - rgb[0]) / span
    } else {
        4.0 + (rgb[0] - rgb[1]) / span
    };
    (h * 60.0).rem_euclid(360.0)
}

/// A scene lit by one colour, with a blown patch in the middle of it.
const BLOWN: u32 = 512;

/// The middle: red and green gone, blue still inside the sensor's range.
///
/// The *shoulder* of a highlight rather than its core, and deliberately. Where
/// every channel is gone the magnitude is unknowable however well the colour is
/// known, and reconstruction says so by rendering neutral — so a fully blown
/// patch could not tell a working propagation from a broken one. This is also
/// the case that covers the most of a real photograph: the ring around a
/// specular, where the channels go one at a time.
const SHOULDER: [f32; 3] = [1.0, 1.0, 0.5];

fn lit_scene(x: u32, y: u32, light: [f32; 3]) -> [f32; 3] {
    if (192..320).contains(&x) && (192..320).contains(&y) {
        SHOULDER
    } else {
        light
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_blown_highlight_takes_the_colour_of_the_light_around_it() {
    // The thing neutral reconstruction gets wrong. A blown sunset is not white:
    // the light going into it is warm, and every unclipped pixel around it says
    // so. Rendering it grey is a defensible answer to "what colour was this?"
    // only while there is nothing to ask.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    // Warm light, comfortably below the sensor's limit in every channel.
    const LIGHT: [f32; 3] = [0.62, 0.34, 0.16];
    let surround = render_field(
        &gpu,
        BLOWN,
        |x, y| lit_scene(x, y, LIGHT),
        DAYLIGHT_WB,
        1.0,
        (96, 256, 24),
    );
    let recovered = render_field(
        &gpu,
        BLOWN,
        |x, y| lit_scene(x, y, LIGHT),
        DAYLIGHT_WB,
        1.0,
        (256, 256, 24),
    );
    println!(
        "surrounding light {surround:?} hue {:.0} cast {:.3}\n\
         blown middle      {recovered:?} hue {:.0} cast {:.3}",
        hue(surround),
        cast(surround),
        hue(recovered),
        cast(recovered)
    );

    assert!(
        cast(recovered) > 0.08,
        "the blown middle came out grey, so nothing was propagated: {recovered:?}"
    );
    let turn = (hue(recovered) - hue(surround) + 540.0).rem_euclid(360.0) - 180.0;
    assert!(
        turn.abs() < 12.0,
        "the blown middle is a different colour from the light around it: \
         {:.0} against {:.0} degrees",
        hue(recovered),
        hue(surround)
    );
    assert!(
        recovered.iter().cloned().fold(0.0f32, f32::max) > 0.7,
        "a blown highlight should still be bright: {recovered:?}"
    );
    // Its dim channels are the *point* — a warm highlight has little blue in
    // it, and requiring every channel to be bright would be requiring grey.
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_highlight_with_nothing_left_still_renders_neutral() {
    // The other side of the same decision, and the reason the fade to neutral
    // is keyed on the *last* channel to go rather than the first. Knowing what
    // colour the light was does not tell you how much of it there was: with
    // every channel pinned there is no anchor to scale the borrowed colour to,
    // and inventing one would paint a blown sun the colour of the sky beside it.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    const LIGHT: [f32; 3] = [0.62, 0.34, 0.16];
    let recovered = render_field(
        &gpu,
        BLOWN,
        |x, y| {
            if (192..320).contains(&x) && (192..320).contains(&y) {
                [1.0, 1.0, 1.0]
            } else {
                LIGHT
            }
        },
        DAYLIGHT_WB,
        1.0,
        (256, 256, 24),
    );
    println!("nothing left: {recovered:?} cast {:.3}", cast(recovered));
    assert!(
        cast(recovered) < 0.06,
        "a highlight with no surviving channel took a colour it cannot know: \
         {recovered:?}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn reconstruction_is_never_more_saturated_than_its_surroundings() {
    // The failure that made this whole area worth revisiting. An earlier
    // version raised a surviving channel to meet another and produced
    // highlights *more* saturated than the sky they interrupted — cyan and
    // lavender arcs around every cloud edge. Borrowing a colour is exactly the
    // operation that could bring it back, so the bound is checked rather than
    // argued: whatever the light, the reconstruction may match its surroundings
    // and may not exceed them.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    for light in [
        [0.62f32, 0.34, 0.16], // warm
        [0.20, 0.36, 0.70],    // cool
        [0.40, 0.40, 0.40],    // neutral
        [0.30, 0.62, 0.28],    // green
    ] {
        // The highlight is the *same light, brighter* — scaled until its
        // strongest channel reaches the sensor's limit. That is the case the
        // bound is about: a subject that is genuinely a different colour from
        // its surroundings may legitimately come out more saturated than they
        // are, and holding reconstruction to their saturation would be holding
        // it to the wrong number. Here there is no such excuse.
        let top = light[0].max(light[1]).max(light[2]);
        let shoulder = [light[0] / top, light[1] / top, light[2] / top];
        let scene = |x: u32, y: u32| {
            if (192..320).contains(&x) && (192..320).contains(&y) {
                shoulder
            } else {
                light
            }
        };
        let surround = render_field(&gpu, BLOWN, scene, DAYLIGHT_WB, 1.0, (96, 256, 24));
        let recovered = render_field(&gpu, BLOWN, scene, DAYLIGHT_WB, 1.0, (256, 256, 24));
        println!(
            "light {light:?}: surround cast {:.3}, blown cast {:.3}",
            cast(surround),
            cast(recovered)
        );
        assert!(
            cast(recovered) <= cast(surround) + 0.02,
            "light {light:?}: the reconstruction is more saturated than the light \
             around it — {:.3} against {:.3}",
            cast(recovered),
            cast(surround)
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_sky_keeps_its_colour_all_the_way_into_clipping() {
    // The test this suite did not have, written after the artefact came back.
    //
    // A blue sky brightening towards the sun: one chromaticity throughout, so
    // whatever the renderer does to it, the *hue* has one right answer. Green's
    // threshold is the lowest of the three, so it saturates first and the
    // reconstruction takes over — and the failure mode is that it hands green
    // more than the sky's share and the highlight comes out cyan.
    //
    // That is exactly what shipped: the reconstruction floored every channel at
    // its *threshold* rather than at what the sensor measured, so a green read
    // at 0.9 of its limit — still an honest number, and inside the run-up that
    // begins a quarter below — was inflated to the limit and blended in. Green
    // rose, red and blue did not, and every cloud edge in a blue sky grew a cyan
    // rim.
    //
    // The suite passed anyway, because the saturation bound it did have is
    // satisfied by a hue that swings without growing.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    const SKY: [f32; 3] = [0.30, 1.0, 0.75];
    // Left to right, from well under the limit to over it. The sensor clips,
    // which is the whole point, so the mosaic is what a sensor would record.
    let brightness = |x: u32| 0.45 + 0.85 * x as f32 / (BLOWN - 1) as f32;
    let scene = |x: u32, _y: u32| {
        let k = brightness(x);
        [
            (SKY[0] * k).min(1.0),
            (SKY[1] * k).min(1.0),
            (SKY[2] * k).min(1.0),
        ]
    };

    let sample = |x: u32| {
        let rgb = render_field(&gpu, BLOWN, scene, DAYLIGHT_WB, 1.0, (x, BLOWN / 2, 8));
        (hue(rgb), cast(rgb), rgb)
    };

    // Where nothing has clipped yet: the sky's own hue, and the answer every
    // brighter column should still be giving.
    let (truth, _, plain) = sample(60);
    println!("unclipped sky {plain:?} hue {truth:.0}");
    let mut worst: (f32, u32) = (0.0, 0);
    for x in (60..BLOWN - 60).step_by(40) {
        let (h, saturation, rgb) = sample(x);
        let turn = (h - truth + 540.0).rem_euclid(360.0) - 180.0;
        println!(
            "  k={:.2} raw green {:.2}  hue {h:5.0} ({turn:+5.1})  cast {saturation:.3}  {rgb:?}",
            brightness(x),
            (SKY[1] * brightness(x)).min(1.0)
        );
        // A pixel with no colour left has no hue to be wrong about.
        if saturation > 0.03 && turn.abs() > worst.0 {
            worst = (turn.abs(), x);
        }
    }
    println!("worst turn {:.1} degrees at x={}", worst.0, worst.1);
    assert!(
        worst.0 < 8.0,
        "the sky turned {:.1} degrees on its way into clipping, at x={} — \
         which is the cyan rim, whatever it is called this time",
        worst.0,
        worst.1
    );
}
