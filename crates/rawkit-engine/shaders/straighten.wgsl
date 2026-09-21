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
    // `flat = (m * vec3(straight, 1)).xy / .z` -- the straight-to-flat map, as
    // the three-by-three the Rust side builds. A homography and not an affine
    // triple, because a keystone is a projective warp: the divide is the whole
    // difference between "the frame is rotated" and "the frame is leaning away".
    // Three rows of `vec4` rather than `mat3x3`, so the padding is written down
    // rather than assumed.
    m0: vec4<f32>,
    m1: vec4<f32>,
    m2: vec4<f32>,
    // How much of the canvas to fill, in pixels.
    extent: vec2<u32>,
    // The photograph's own size in straight pixels. Beyond it is not more
    // photograph: the flat buffer still holds the part the crop removed, and
    // sampling it would draw the frame the straighten was meant to trim.
    photograph: vec2<f32>,
    // The lens's own centre, in flat coordinates, and how far its corner is.
    // A quarter turn is an isometry, so undoing a radial distortion about the
    // image of the optical centre is the same map as undoing it in sensor
    // coordinates -- which is why the canvas does not have to unrotate first.
    optical_centre: vec2<f32>,
    corner: f32,
    // How many of the sixteen below are knots. Zero is a lens nothing is
    // correcting, and the whole radial step is skipped.
    knots: u32,
    // The correction, already resolved on the CPU: the amount applied, the peak
    // subtracted so nothing is ever read from outside the frame, the divisor
    // divided out. Read as sixteen floats; packed in fours because that is what
    // a uniform will hold.
    curve: array<vec4<f32>, 4>,
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

/// One of the sixteen resolved knots.
///
/// Indexed rather than indexable: a uniform array of `vec4` cannot be addressed
/// by a runtime value component-wise, so the component is picked by branching.
fn knot(i: u32) -> f32 {
    let v = params.curve[i >> 2u];
    switch (i & 3u) {
        case 0u: { return v.x; }
        case 1u: { return v.y; }
        case 2u: { return v.z; }
        default: { return v.w; }
    }
}

/// Where a point of the corrected photograph reads from on the lens's own image.
///
/// The same arithmetic as `Distortion::scale_at`, walking the same resolved
/// curve, because a preview that corrected by a slightly different amount from
/// the export would be a difference nobody could see until they compared files.
fn undistort(at: vec2<f32>) -> vec2<f32> {
    if (params.knots < 2u || params.corner <= 0.0) {
        return at;
    }
    let offset = at - params.optical_centre;
    let t = clamp(length(offset) / params.corner, 0.0, 1.0);
    let x = t * f32(params.knots - 1u);
    let i = min(u32(x), params.knots - 2u);
    let f = x - f32(i);
    let scale = 1.0 + knot(i) * (1.0 - f) + knot(i + 1u) * f;
    return params.optical_centre + offset * scale;
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
        // Transparent: the presenter puts the chosen surround here.
        textureStore(canvas, gid.xy, vec4<f32>(0.0, 0.0, 0.0, 0.0));
        return;
    }
    let homogeneous = vec3<f32>(
        dot(params.m0.xyz, vec3<f32>(straight, 1.0)),
        dot(params.m1.xyz, vec3<f32>(straight, 1.0)),
        dot(params.m2.xyz, vec3<f32>(straight, 1.0)),
    );
    // Behind the horizon there is no answer. The fit pulls the crop in far
    // enough that a legal edit cannot reach here, so this is a guard rather than
    // a behaviour -- but a divide by nothing must not become a texture read at
    // whatever coordinate that produces.
    if (abs(homogeneous.z) < 1e-6) {
        textureStore(canvas, gid.xy, vec4<f32>(0.0, 0.0, 0.0, 0.0));
        return;
    }
    var placed = homogeneous.xy / homogeneous.z;
    placed = undistort(placed);
    let flat = placed - params.flat_origin;

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
