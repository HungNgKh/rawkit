//! The profile's hue/saturation correction table, end to end through the GPU.
//!
//! The table is measurement, not taste — a profile carries one per calibration
//! illuminant, and a look would not depend on the light. That is why this is
//! adopted while `ProfileLookTable` and `ProfileToneCurve` are not.
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
