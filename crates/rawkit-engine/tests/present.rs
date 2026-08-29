//! Does what reaches the screen carry the transfer function exactly once?
//!
//! The canvas holds display-referred **linear** values. A screen expects encoded
//! ones. Apply the curve zero times and the image is uniformly far too dark;
//! apply it twice and it is washed out. Both look like grading problems, neither
//! announces itself, and no amount of looking at a photograph settles which one
//! is happening.
//!
//! So the test states a number instead. Linear **0.18** — mid grey, the value
//! the tone map is pinned at — must arrive as **118**. Not 46, which is what
//! writing linear values into an encoded target produces, and not 188, which is
//! the curve applied twice.
//!
//! 118 is deliberately the same number `rawkit-export` asserts for a JPEG. The
//! screen and the file are two implementations of one stage, and the way to keep
//! them honest is to make them agree on a value rather than to hope.
//!
//! GPU-gated like the rest: `cargo test -- --ignored`.

use rawkit_engine::{Gpu, Presenter, Renderer};

const SIZE: u32 = 8;

/// f32 to IEEE 754 binary16, so the test can write canvas pixels directly.
/// Round-to-nearest-even, and only correct for the ordinary values used here —
/// it is test scaffolding, not a general conversion.
fn to_half(value: f32) -> u16 {
    if value == 0.0 {
        return 0;
    }
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mantissa = bits & 0x7f_ffff;
    assert!(
        (1..=30).contains(&exponent),
        "{value} is outside the range this helper handles"
    );
    // Round to nearest, ties to even.
    let mut half = sign | ((exponent as u16) << 10) | (mantissa >> 13) as u16;
    let remainder = mantissa & 0x1fff;
    if remainder > 0x1000 || (remainder == 0x1000 && (half & 1) == 1) {
        half += 1;
    }
    half
}

/// Fill a canvas with one colour and present it into `target_format`, returning
/// the first pixel as BGRA bytes.
fn round_trip(gpu: &Gpu, linear: [f32; 3], target_format: wgpu::TextureFormat) -> [u8; 4] {
    let presenter = Presenter::new(gpu, target_format);

    let canvas_texture = {
        // A canvas of a known colour, written directly rather than rendered —
        // this test is about the output transform and nothing before it.
        let engine_canvas = Renderer::new(gpu).create_canvas(gpu, SIZE, SIZE);
        let halves: Vec<u16> = (0..SIZE * SIZE)
            .flat_map(|_| {
                [
                    to_half(linear[0]),
                    to_half(linear[1]),
                    to_half(linear[2]),
                    to_half(1.0),
                ]
            })
            .collect();
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: engine_canvas.texture(),
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&halves),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(SIZE * 8),
                rows_per_image: Some(SIZE),
            },
            wgpu::Extent3d {
                width: SIZE,
                height: SIZE,
                depth_or_array_layers: 1,
            },
        );
        engine_canvas
    };

    let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("target"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: target_format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    presenter
        .draw(gpu, &canvas_texture, &view)
        .expect("present");

    // Read one row back. 256-byte row alignment, as always.
    let staging = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: 256 * SIZE as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: Some(SIZE),
            },
        },
        wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
    );
    gpu.queue.submit([encoder.finish()]);

    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    rx.recv().expect("readback").expect("readback");
    let pixel = {
        let view = staging.slice(..).get_mapped_range();
        [view[0], view[1], view[2], view[3]]
    };
    staging.unmap();
    pixel
}

#[test]
#[ignore = "requires a GPU adapter"]
fn mid_grey_reaches_the_screen_as_118() {
    let gpu = Gpu::new().expect("no usable GPU adapter");
    // Bgra8UnormSrgb is what the surface reported on the dev machine, and the
    // hardware does the encoding for this format.
    let pixel = round_trip(
        &gpu,
        [0.18, 0.18, 0.18],
        wgpu::TextureFormat::Bgra8UnormSrgb,
    );
    println!("linear 0.18 -> BGRA {pixel:?}");
    for channel in &pixel[..3] {
        assert!(
            (117..=119).contains(channel),
            "linear 0.18 arrived as {channel}, not 118. \
             46 would mean the transfer function never ran; 188 would mean it ran twice"
        );
    }
    assert_eq!(pixel[3], 255, "the canvas is opaque");
}

#[test]
#[ignore = "requires a GPU adapter"]
fn a_target_that_does_not_encode_gets_the_same_answer() {
    let gpu = Gpu::new().expect("no usable GPU adapter");
    // A surface does not always offer an -Srgb format. When it does not, the
    // shader has to apply the curve itself, and the result must be the same
    // picture — otherwise the app would look different on two machines for a
    // reason that has nothing to do with the photograph.
    let hardware = round_trip(
        &gpu,
        [0.18, 0.18, 0.18],
        wgpu::TextureFormat::Bgra8UnormSrgb,
    );
    let shader = round_trip(&gpu, [0.18, 0.18, 0.18], wgpu::TextureFormat::Bgra8Unorm);
    println!("hardware {hardware:?} vs shader {shader:?}");
    for (h, s) in hardware.iter().zip(&shader) {
        assert!(
            h.abs_diff(*s) <= 1,
            "the two encode paths disagree: {hardware:?} vs {shader:?}"
        );
    }
}

#[test]
#[ignore = "requires a GPU adapter"]
fn black_and_white_survive_the_ends_of_the_curve() {
    let gpu = Gpu::new().expect("no usable GPU adapter");
    let black = round_trip(&gpu, [0.0, 0.0, 0.0], wgpu::TextureFormat::Bgra8UnormSrgb);
    let white = round_trip(&gpu, [1.0, 1.0, 1.0], wgpu::TextureFormat::Bgra8UnormSrgb);
    assert_eq!(&black[..3], &[0, 0, 0], "linear 0 must stay 0");
    assert_eq!(&white[..3], &[255, 255, 255], "linear 1 must reach full");
}
