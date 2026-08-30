//! Does the eight-band mixer divide the hue circle without gaps or overlaps?
//!
//! Two claims, and the first is the one that makes the second worth believing.
//!
//! 1. **The bands partition the circle.** A pixel's adjustment is the linear
//!    blend of the two band centres that bracket its hue, so the weights sum to
//!    one everywhere by construction. The way to check that from outside is to
//!    set all eight bands to the same value and compare against the *global*
//!    saturation control at that value — a control this project already tests.
//!    If the weights summed to less than one anywhere, some hue would come out
//!    short; if to more, some hue would overshoot.
//!
//! 2. **A band reaches its own colours and stops at its neighbours.** With one
//!    band raised, the effect measured across a hue sweep should peak at that
//!    band's centre and fall to nothing at the centres either side. Anything
//!    else is a control that cannot be aimed.
//!
//! Measured in `Display`, because that is where the mixer runs and what the
//! bands are defined against — the tone curve is per channel and moves hue, so
//! the hue a band sees is not the hue the sensor recorded.
//!
//! GPU-gated like the rest: `cargo test -- --ignored`.

use rawkit_editstate::{Band, EditState};
use rawkit_engine::{BayerPhase, CameraProfile, Frame, Gpu, Output, Renderer};

const W: u32 = 256;
const H: u32 = 64;

/// Hue in degrees, saturation and value in 0..1.
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let sector = h / 60.0;
    let i = sector.floor();
    let f = sector - i;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - s * f), v * (1.0 - s * (1.0 - f)));
    match (i as i32).rem_euclid(6) {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

fn rgb_to_hue(rgb: [f32; 3]) -> f32 {
    let top = rgb[0].max(rgb[1]).max(rgb[2]);
    let bottom = rgb[0].min(rgb[1]).min(rgb[2]);
    let span = top - bottom;
    if span <= 0.0 {
        return 0.0;
    }
    let hue = if top == rgb[0] {
        let h = (rgb[1] - rgb[2]) / span;
        if h < 0.0 {
            h + 6.0
        } else {
            h
        }
    } else if top == rgb[1] {
        2.0 + (rgb[2] - rgb[0]) / span
    } else {
        4.0 + (rgb[0] - rgb[1]) / span
    };
    hue * 60.0
}

fn chroma(rgb: [f32; 3]) -> f32 {
    let grey = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
    ((rgb[0] - grey).powi(2) + (rgb[1] - grey).powi(2) + (rgb[2] - grey).powi(2)).sqrt()
}

/// A mosaic sweeping the whole hue circle across its width, so every band is
/// represented and so is every point between two of them.
fn sweep() -> Vec<f32> {
    (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| {
            let rgb = hsv_to_rgb(360.0 * x as f32 / W as f32, 0.8, 0.45);
            // RGGB.
            match (x % 2 == 0, y % 2 == 0) {
                (true, true) => rgb[0],
                (false, false) => rgb[2],
                _ => rgb[1],
            }
        })
        .collect()
}

fn render(gpu: &Gpu, cfa: &[f32], state: &EditState) -> Vec<f32> {
    Renderer::new(gpu)
        .run(
            gpu,
            &Frame {
                data: cfa,
                width: W,
                height: H,
                phase: BayerPhase::Rggb,
                as_shot_wb: [1.0, 1.0, 1.0],
                clip_level: f32::INFINITY,
                profile: CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY),
            },
            state,
            Output::Display,
        )
        .expect("render")
        .pixels
}

fn pixel(pixels: &[f32], i: usize) -> [f32; 3] {
    [pixels[i * 4], pixels[i * 4 + 1], pixels[i * 4 + 2]]
}

#[test]
#[ignore = "requires a GPU adapter"]
fn the_bands_partition_the_hue_circle() {
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let cfa = sweep();

    const LIFT: f32 = 0.4;
    let mut globally = EditState::default();
    globally.colour.saturation = LIFT;

    let mut per_band = EditState::default();
    for band in Band::ALL {
        let mut mix = per_band.hsl.mix(band);
        mix.saturation = LIFT;
        per_band.hsl.set(band, mix);
    }

    let one = render(&gpu, &cfa, &globally);
    let many = render(&gpu, &cfa, &per_band);

    // Away from the frame's edge, where RCD clamps rather than reconstructs.
    let mut worst = 0.0f32;
    for y in 8..H - 8 {
        for x in 8..W - 8 {
            let i = (y * W + x) as usize;
            let (a, b) = (pixel(&one, i), pixel(&many, i));
            for c in 0..3 {
                worst = worst.max((a[c] - b[c]).abs());
            }
        }
    }
    println!("worst disagreement between all-eight-bands and the global control: {worst:.6}");
    assert!(
        worst < 2e-3,
        "the eight bands together are not the global control: {worst:.6} apart. \
         Either the weights do not sum to one, or a hue falls into no band at all"
    );
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_band_reaches_its_own_hue_and_stops_at_its_neighbours() {
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let cfa = sweep();

    let plain = render(&gpu, &cfa, &EditState::default());
    let mut lifted = EditState::default();
    let mut mix = lifted.hsl.mix(Band::Red);
    mix.saturation = 0.6;
    lifted.hsl.set(Band::Red, mix);
    let boosted = render(&gpu, &cfa, &lifted);

    // Bucketed by the hue each pixel actually has *after* development, which is
    // the hue the shader placed it by.
    const BUCKETS: usize = 72;
    let mut ratio = vec![0.0f32; BUCKETS];
    let mut count = vec![0.0f32; BUCKETS];
    for y in 8..H - 8 {
        for x in 8..W - 8 {
            let i = (y * W + x) as usize;
            let before = pixel(&plain, i);
            let base = chroma(before);
            if base < 1e-4 {
                continue;
            }
            let bucket = ((rgb_to_hue(before) / 360.0 * BUCKETS as f32) as usize).min(BUCKETS - 1);
            ratio[bucket] += chroma(pixel(&boosted, i)) / base;
            count[bucket] += 1.0;
        }
    }
    let at = |degrees: f32| {
        let bucket = ((degrees / 360.0 * BUCKETS as f32) as usize).min(BUCKETS - 1);
        ratio[bucket] / count[bucket].max(1.0)
    };

    println!(
        "red band at 0°: {:.3}   15°: {:.3}   30° (orange): {:.3}   \
         180° (aqua): {:.3}   320° (magenta): {:.3}   340°: {:.3}",
        at(2.5),
        at(15.0),
        at(32.5),
        at(180.0),
        at(322.5),
        at(342.5)
    );

    assert!(
        at(2.5) > 1.5,
        "the red band barely moved red: {:.3}",
        at(2.5)
    );
    // Its neighbours' centres get weight zero, which is what "the bands
    // partition the circle" means seen from one band.
    for (name, degrees) in [("orange", 32.5), ("magenta", 322.5), ("aqua", 180.0)] {
        let effect = at(degrees);
        assert!(
            (effect - 1.0).abs() < 0.05,
            "raising red moved {name} by {:.1}%, so the bands overlap",
            100.0 * (effect - 1.0)
        );
    }
    // And it falls off in between rather than stepping.
    assert!(
        at(2.5) > at(15.0) && at(15.0) > at(27.5) && at(15.0) > 1.15,
        "the falloff towards orange is not a ramp: {:.3}, {:.3}, {:.3}",
        at(2.5),
        at(15.0),
        at(27.5)
    );
}
