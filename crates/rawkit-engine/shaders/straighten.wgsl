// Straighten — the one geometric step that has to gather.
//
// Orientation and crop land every output pixel on exactly one source pixel, so
// the canvas draws tiles by scattering them into a *flat* buffer: rearranged,
// never resampled, still exact. A fraction of a degree does not divide the plane
// that way. Scattering a rotated tile leaves holes between the pixels it lands
// on, so the rotation is done the other way round — each output pixel asks where
// it comes from, and reads a neighbourhood there.
//
// The map is affine, so it arrives as six floats measured from the same function
// the CPU export walks. Re-deriving the algebra here instead is how a canvas
// ends up framing a photograph differently from the file it writes.
//
// Catmull-Rom, with the taps written out. A sampler's own filtering uses
// reduced-precision weights, so anything read through one could never match the
// export; these weights are the same polynomial the Rust side uses.

struct Params {
    // The straight-space point of this canvas's top-left pixel.
    straight_origin: vec2<f32>,
    // The flat-space point of the flat buffer's top-left pixel.
    flat_origin: vec2<f32>,
    // `flat = origin + dx * straight.x + dy * straight.y`.
    origin: vec2<f32>,
    dx: vec2<f32>,
    dy: vec2<f32>,
    // How much of the canvas to fill, in pixels.
    extent: vec2<u32>,
    // The photograph's own size in straight pixels. Beyond it is not more
    // photograph: the flat buffer still holds the part the crop removed, and
    // sampling it would draw the frame the straighten was meant to trim.
    photograph: vec2<f32>,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var flat_buffer: texture_2d<f32>;
@group(0) @binding(2) var canvas: texture_storage_2d<rgba16float, write>;

/// Catmull-Rom weights for the four taps around a fraction.
///
/// The a = -0.5 form, which passes through every source pixel — so a sample that
/// lands exactly on one is that pixel, not a blend of its neighbours.
fn weights(t: f32) -> vec4<f32> {
    let t2 = t * t;
    let t3 = t2 * t;
    return vec4<f32>(
        -0.5 * t3 + t2 - 0.5 * t,
        1.5 * t3 - 2.5 * t2 + 1.0,
        -1.5 * t3 + 2.0 * t2 + 0.5 * t,
        0.5 * t3 - 0.5 * t2,
    );
}

@compute @workgroup_size(8, 8)
fn straighten(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= params.extent.x || gid.y >= params.extent.y) {
        return;
    }
    // Pixel centres, so the first output pixel reads half a pixel in.
    let straight = params.straight_origin + vec2<f32>(gid.xy) + vec2<f32>(0.5);
    // Outside the photograph is background, not the part of the frame the crop
    // took away. Without this the picture spills past its own edge and only
    // stops where the rotation runs off the sensor — which looks like a bug in
    // the straighten and is really a missing boundary.
    if (any(straight < vec2<f32>(0.0)) || any(straight > params.photograph)) {
        textureStore(canvas, gid.xy, vec4<f32>(0.0, 0.0, 0.0, 1.0));
        return;
    }
    let flat = params.origin
        + params.dx * straight.x
        + params.dy * straight.y
        - params.flat_origin;

    let size = vec2<i32>(textureDimensions(flat_buffer));
    let base = floor(flat - vec2<f32>(0.5));
    let frac = flat - vec2<f32>(0.5) - base;
    let wx = weights(frac.x);
    let wy = weights(frac.y);

    var acc = vec4<f32>(0.0);
    for (var j = 0; j < 4; j = j + 1) {
        // Clamped to the edge. The crop reserves the filter's reach, so this is
        // a guard against rounding rather than something that happens — but a
        // read past the edge must return a pixel, not whatever is there.
        let sy = clamp(i32(base.y) + j - 1, 0, size.y - 1);
        for (var i = 0; i < 4; i = i + 1) {
            let sx = clamp(i32(base.x) + i - 1, 0, size.x - 1);
            acc = acc + textureLoad(flat_buffer, vec2<i32>(sx, sy), 0) * wx[i] * wy[j];
        }
    }
    textureStore(canvas, gid.xy, acc);
}
