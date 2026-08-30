//! The profile's hue/saturation correction table, end to end through the GPU.
//!
//! The table is measurement, not taste — a profile carries one per calibration
//! illuminant, and a look would not depend on the light. The **look** table is
//! here too, and is the same format applied somewhere else: after the tone
//! curve, because a look is authored against a rendered picture rather than
//! against the light. `ProfileToneCurve` is still not adopted.
//!
//! `cargo test -p rawkit-engine --test hue_sat_map -- --ignored`

use rawkit_editstate::EditState;
use rawkit_engine::profile::{HueSatMap, Matrix3, PROPHOTO_FROM_XYZ_D50};
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const N: u32 = 64;

/// A forward matrix whose rows sum to the D50 white point, as the DNG
/// specification requires. Without one the table is deliberately skipped, so
/// every test here needs it.
const FORWARD_D50: Matrix3 = [
    [0.6000, 0.2500, 0.1142],
    [0.2500, 0.7000, 0.0500],
    [0.0100, 0.0400, 0.7749],
];

fn profile_with(map: Option<HueSatMap>) -> CameraProfile {
    let mut profile = CameraProfile::from_color_matrix([
        [0.6941, -0.2164, -0.0644],
        [-0.3850, 1.1349, 0.2779],
        [-0.0031, 0.1055, 0.6511],
    ]);
    profile.set_forward_matrix(6504.0, FORWARD_D50);
    if let Some(map) = map {
        profile.set_hue_sat_map(6504.0, map);
    }
    profile
}

/// A table of the given size that changes nothing.
fn identity_map(hue: u32, sat: u32, value: u32) -> HueSatMap {
    HueSatMap {
        hue_divisions: hue,
        sat_divisions: sat,
        value_divisions: value,
        deltas: vec![[0.0, 1.0, 1.0]; (hue * sat * value) as usize],
    }
}

/// A table that applies the same delta everywhere, so its effect can be
/// predicted without reasoning about interpolation.
fn uniform_map(hue: u32, sat: u32, value: u32, delta: [f32; 3]) -> HueSatMap {
    HueSatMap {
        hue_divisions: hue,
        sat_divisions: sat,
        value_divisions: value,
        deltas: vec![delta; (hue * sat * value) as usize],
    }
}

/// Render a flat frame of one colour and return the developed centre pixel.
///
/// The mosaic is built so that each Bayer site carries its own channel's value,
/// which demosaics back to that colour exactly — isolating the develop stage
/// from the interpolation.
fn render_colour(gpu: &Gpu, profile: &CameraProfile, camera_rgb: [f32; 3]) -> [f32; 3] {
    render_with(gpu, profile, camera_rgb, &EditState::default())
}

/// The same, with an edit of the caller's choosing.
fn render_with(
    gpu: &Gpu,
    profile: &CameraProfile,
    camera_rgb: [f32; 3],
    state: &EditState,
) -> [f32; 3] {
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
            cfa.push(camera_rgb[channel]);
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
                as_shot_wb: [1.0, 1.0, 1.0],
                clip_level: 1.0,
                profile: profile.clone(),
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

/// A profile carrying only a look — no hue/saturation correction — which is
/// exactly the shape of an Adobe *Camera Matching* profile.
fn profile_with_look(look: HueSatMap) -> CameraProfile {
    let mut profile = profile_with(None);
    profile.set_look_table(look);
    profile
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_hand_drawn_curve_shapes_what_the_tone_map_left() {
    // The user's curve is the last of the tone controls and runs on the
    // rendered picture, so a curve that flattens everything to one value must
    // flatten the output — whatever the scene was.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let flat = EditState {
        curve: rawkit_editstate::Curve {
            points: vec![[0.0, 0.5], [1.0, 0.5]],
        },
        ..EditState::default()
    };
    let dark = render_with(&gpu, &profile_with(None), [0.05, 0.04, 0.03], &flat);
    let bright = render_with(&gpu, &profile_with(None), [0.80, 0.70, 0.60], &flat);
    for c in 0..3 {
        assert!(
            (dark[c] - bright[c]).abs() < 3e-3,
            "channel {c} still varies with the scene: {} against {}",
            dark[c],
            bright[c]
        );
    }

    // And a curve that lifts the middle renders brighter than one that does not,
    // so the curve is read rather than merely accepted.
    let lifted = EditState {
        curve: rawkit_editstate::Curve {
            points: vec![[0.0, 0.0], [0.4, 0.75], [1.0, 1.0]],
        },
        ..EditState::default()
    };
    let colour = [0.30, 0.28, 0.26];
    let plain = render_with(&gpu, &profile_with(None), colour, &EditState::default());
    let raised = render_with(&gpu, &profile_with(None), colour, &lifted);
    let luma = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    println!(
        "hand-drawn curve: {:.4} -> {:.4}",
        luma(plain),
        luma(raised)
    );
    assert!(
        luma(raised) > luma(plain) * 1.5,
        "the curve did not lift: {:.4} against {:.4}",
        luma(raised),
        luma(plain)
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_profile_tone_curve_replaces_the_tone_map() {
    // A curve that answers the same thing for every input must flatten the
    // picture completely. If ours ran as well as the profile's, the output would
    // still vary with the scene — two tone maps in series map the scene twice,
    // and the symptom is a flat muddy picture rather than an error.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let mut profile = profile_with(None);
    profile.set_tone_curve(&[(0.0, 0.5), (1.0, 0.5)]);

    let dark = render_colour(&gpu, &profile, [0.05, 0.04, 0.03]);
    let bright = render_colour(&gpu, &profile, [0.80, 0.70, 0.60]);
    for c in 0..3 {
        assert!(
            (dark[c] - bright[c]).abs() < 2e-3,
            "channel {c} still varies with the scene: {} against {}",
            dark[c],
            bright[c]
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_higher_profile_curve_renders_brighter() {
    // And the curve is read rather than merely present: the same photograph
    // through two curves differing only in height comes out at two brightnesses,
    // in the order the curves are in.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let colour = [0.30, 0.28, 0.26];
    let with = |points: &[(f32, f32)]| {
        let mut profile = profile_with(None);
        profile.set_tone_curve(points);
        let out = render_colour(&gpu, &profile, colour);
        0.2126 * out[0] + 0.7152 * out[1] + 0.0722 * out[2]
    };
    let low = with(&[(0.0, 0.0), (1.0, 0.4)]);
    let high = with(&[(0.0, 0.0), (1.0, 0.9)]);
    println!("profile curve height: {low:.4} against {high:.4}");
    assert!(
        high > low * 1.5,
        "the taller curve did not render brighter: {high:.4} against {low:.4}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_look_is_applied_even_with_no_hue_saturation_table() {
    // The regression this exists for. The working-space hop used to be taken
    // only when a profile had a hue/saturation correction, so a Camera Matching
    // profile — which has none and keeps everything in its look — had its look
    // silently discarded. Measured against six real frames, that rendered
    // *further* from the camera's own JPEG than using no profile at all.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let colour = [0.55, 0.30, 0.22];

    let plain = render_colour(&gpu, &profile_with_look(identity_map(6, 4, 4)), colour);
    // Two thirds of the saturation, everywhere in the table.
    let calmed = render_colour(
        &gpu,
        &profile_with_look(uniform_map(6, 4, 4, [0.0, 0.667, 1.0])),
        colour,
    );

    let distance = |c: [f32; 3]| {
        let grey = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        ((c[0] - grey).powi(2) + (c[1] - grey).powi(2) + (c[2] - grey).powi(2)).sqrt()
    };
    let ratio = distance(calmed) / distance(plain);
    println!("look table took saturation to {ratio:.3} of what it was");
    assert!(
        ratio < 0.85,
        "the look table did nothing: saturation is {ratio:.3} of what it was"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn an_identity_look_changes_nothing() {
    // The same argument as the identity hue/saturation table, for the other
    // table: an identity look still takes the hop into the working space and
    // back, so an asymmetric round trip shows up here with nothing to blame it
    // on.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let colour = [0.55, 0.30, 0.22];
    let without = render_colour(&gpu, &profile_with(None), colour);
    let with = render_colour(&gpu, &profile_with_look(identity_map(8, 4, 4)), colour);
    for c in 0..3 {
        assert!(
            (without[c] - with[c]).abs() < 2e-3,
            "an identity look moved channel {c}: {} against {}",
            with[c],
            without[c]
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn an_identity_table_changes_nothing() {
    // The strongest single test here. An identity table still exercises the
    // whole path — the extra hop into the working space, the RGB-to-HSV
    // conversion, the trilinear lookup, and the conversion back — so anything
    // wrong in the round trip shows up as a colour shift with no correction
    // applied to cause it.
    //
    // In particular this is what catches an HSV conversion that is subtly
    // asymmetric, which would otherwise look like the profile "doing something"
    // and be accepted.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let without = profile_with(None);
    let with = profile_with(Some(identity_map(6, 4, 3)));

    for colour in [
        [0.30, 0.20, 0.10],
        [0.10, 0.30, 0.15],
        [0.12, 0.14, 0.40],
        [0.25, 0.25, 0.25],
        [0.40, 0.05, 0.30],
    ] {
        let plain = render_colour(&gpu, &without, colour);
        let mapped = render_colour(&gpu, &with, colour);
        for c in 0..3 {
            assert!(
                (plain[c] - mapped[c]).abs() < 2e-3,
                "identity table shifted {colour:?}: {plain:?} became {mapped:?}"
            );
        }
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_saturation_delta_changes_saturation_and_not_hue() {
    // What the table is for: pulling saturation without moving hue. If the HSV
    // round trip were wrong these would move together, which is exactly the
    // artefact that makes a profile look like it is "off" without saying how.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let plain = render_colour(&gpu, &profile_with(None), [0.35, 0.12, 0.10]);
    let desaturated = render_colour(
        &gpu,
        &profile_with(Some(uniform_map(6, 4, 1, [0.0, 0.5, 1.0]))),
        [0.35, 0.12, 0.10],
    );

    let spread = |c: [f32; 3]| c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2]);
    assert!(
        spread(desaturated) < spread(plain) * 0.75,
        "halving saturation barely changed the colour: {plain:?} -> {desaturated:?}"
    );
    // Still the same hue family: red stays the largest channel.
    assert!(
        desaturated[0] > desaturated[1] && desaturated[0] > desaturated[2],
        "saturation change moved the hue: {desaturated:?}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_hue_shift_rotates_the_colour() {
    // A 120-degree rotation is the one shift whose result is obvious without
    // trusting the implementation: red becomes green.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let rotated = render_colour(
        &gpu,
        &profile_with(Some(uniform_map(6, 4, 1, [120.0, 1.0, 1.0]))),
        [0.35, 0.05, 0.05],
    );
    assert!(
        rotated[1] > rotated[0] && rotated[1] > rotated[2],
        "a 120 degree hue shift did not turn red into green: {rotated:?}"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn hue_wraps_rather_than_clamping_at_red() {
    // Hue is circular and the table's first and last cells are neighbours. A
    // lookup that clamps instead of wrapping puts a seam at 0 degrees, which
    // lands squarely on reds — the one place a seam is most visible and least
    // excusable.
    //
    // Two nearly identical reds either side of the wrap must stay nearly
    // identical after a table that varies with hue.
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let mut map = identity_map(4, 2, 1);
    // Vary saturation scale by hue cell, so a mis-wrapped lookup lands on a
    // different cell and produces a different result.
    for (i, delta) in map.deltas.iter_mut().enumerate() {
        delta[1] = 0.6 + 0.1 * (i % 4) as f32;
    }
    let profile = profile_with(Some(map));

    // Just above 0 degrees, and just below 360.
    let just_above = render_colour(&gpu, &profile, [0.30, 0.101, 0.100]);
    let just_below = render_colour(&gpu, &profile, [0.30, 0.100, 0.101]);
    for c in 0..3 {
        assert!(
            (just_above[c] - just_below[c]).abs() < 0.02,
            "a seam at the hue wrap: {just_above:?} vs {just_below:?}"
        );
    }
}

#[test]
fn one_forward_matrix_across_two_illuminants_is_usable() {
    // The shape that caused a panic on a real file: a profile with two colour
    // matrices and only *one* forward matrix. The first implementation had two
    // disagreeing notions of "has a forward matrix" — any, versus one at this
    // temperature — and a caller could hold a table it had no transform for.
    //
    // The specification says a single forward matrix applies to both
    // illuminants, so this must simply work rather than fall back.
    let mut profile = CameraProfile::from_dual_illuminant(
        (
            2856.0,
            [[0.7, -0.2, -0.06], [-0.38, 1.13, 0.28], [-0.003, 0.1, 0.65]],
        ),
        (
            6504.0,
            [
                [0.694, -0.216, -0.064],
                [-0.385, 1.135, 0.278],
                [-0.003, 0.105, 0.651],
            ],
        ),
    );
    profile.set_forward_matrix(6504.0, FORWARD_D50);
    profile.set_hue_sat_map(6504.0, identity_map(4, 2, 1));

    for temperature in [2856.0, 4500.0, 6504.0] {
        assert!(
            profile.camera_to_working(temperature).is_some(),
            "no working transform at {temperature}K"
        );
        assert!(
            profile.hue_sat_map(temperature).is_some(),
            "no table at {temperature}K"
        );
    }
}

#[test]
fn the_table_is_skipped_without_a_forward_matrix() {
    // Not a GPU test: this is the decision, and it belongs where it can be read.
    //
    // The table's hue angles were measured in ProPhoto, and the route to that
    // space runs through the forward matrix. With a colour matrix alone the
    // white balance is folded in by a normalisation of our own choosing, so
    // "which linear space is this" has no single answer — and applying the
    // table anyway would rotate every correction by an unknown angle, making
    // colour worse in a way that looks like the profile being wrong.
    let mut profile = CameraProfile::from_color_matrix(PROPHOTO_FROM_XYZ_D50);
    profile.set_hue_sat_map(6504.0, identity_map(4, 2, 1));
    assert!(
        profile.hue_sat_map(6504.0).is_none(),
        "the table was offered without a forward matrix to reach its space"
    );

    profile.set_forward_matrix(6504.0, FORWARD_D50);
    assert!(
        profile.hue_sat_map(6504.0).is_some(),
        "the table was withheld even with a forward matrix present"
    );
}
