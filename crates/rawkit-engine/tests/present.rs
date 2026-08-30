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

/// Every pixel of a `SIZE x SIZE` BGRA target, tightly packed.
///
/// Texture-to-buffer copies want rows aligned to 256 bytes and `SIZE` is 8, so
/// each row arrives with 224 bytes of padding behind it. Dropping that here
/// rather than at every call site: the first version returned the padded buffer
/// and indexed it as though it were tight, which read the wrong pixel for every
/// row but the first.
fn read_back(gpu: &Gpu, target: &wgpu::Texture) -> Vec<u8> {
    let staging = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("present readback"),
        size: (256 * SIZE) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: target,
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
    let out = {
        let view = staging.slice(..).get_mapped_range();
        (0..SIZE as usize)
            .flat_map(|y| view[y * 256..y * 256 + SIZE as usize * 4].to_vec())
            .collect()
    };
    staging.unmap();
    out
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

#[test]
#[ignore = "requires a GPU adapter"]
fn drawing_into_a_rectangle_leaves_the_rest_of_the_window_alone() {
    // For the arrangement where the chrome floats over the surface rather than
    // beside it: the swapchain is the whole window, and the photograph belongs
    // only where the interface is not covering. Two things have to hold, and
    // the second is the one a viewport gets wrong — the picture has to be
    // *scaled into* the rectangle, not merely clipped to it.
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(_) => return,
    };
    let format = wgpu::TextureFormat::Bgra8UnormSrgb;
    let presenter = Presenter::new(&gpu, format);
    let canvas = Renderer::new(&gpu).create_canvas(&gpu, SIZE, SIZE);
    // Mid-grey, so a written pixel is unmistakably not the cleared background.
    let halves: Vec<u16> = (0..SIZE * SIZE)
        .flat_map(|_| [to_half(0.5), to_half(0.5), to_half(0.5), to_half(1.0)])
        .collect();
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: canvas.texture(),
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

    let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("windowed target"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());

    // Clear the whole thing to black first, so "untouched" is a value the test
    // can recognise rather than whatever the driver left.
    {
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        gpu.queue.submit([encoder.finish()]);
    }

    let strip = SIZE / 4;
    presenter
        .draw_into(&gpu, &canvas, &view, [0, strip, SIZE, SIZE - strip])
        .expect("present into a rectangle");
    let pixels = read_back(&gpu, &target);

    let at = |x: u32, y: u32| {
        let i = ((y * SIZE + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };
    // Above the rectangle: exactly what was there, because the chrome is drawn
    // over it and anything written would show through wherever it is not.
    for y in 0..strip {
        assert_eq!(at(SIZE / 2, y), [0, 0, 0], "row {y} was painted over");
    }
    // Inside it: the photograph, everywhere including the last row — a viewport
    // that clipped instead of scaling would leave the bottom quarter black.
    for y in [strip, SIZE / 2, SIZE - 1] {
        let got = at(SIZE / 2, y);
        assert!(got[0] > 100, "row {y} is {got:?}, not the canvas");
    }
}
