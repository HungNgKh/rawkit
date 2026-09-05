// Where a local adjustment reaches, painted over the photograph.
//
// The mask is already on the GPU: the develop kernel samples it every frame from
// a texture array indexed in image coordinates, rasterised only when a shape
// changes. And the map from a canvas pixel back to the sensor is the same one
// the straighten gathers along. So this pass computes nothing new — it reads
// what is already there and tints it.
//
// It replaces a CPU path that rasterised the mask a second time, resampled it
// through `source_at` per texel, and uploaded the result: 28 ms on a plain frame
// and 76 ms with a straighten, a keystone and a lens correction, on every frame
// of a drag. That cost is what pushed a drag into haste, which is what made the
// overlay visibly jump.

struct Overlay {
    // The canvas pixel of this pass's first texel, in *straight* coordinates.
    straight_origin: vec2<f32>,
    // How many canvas pixels to cover.
    extent: vec2<u32>,
    // Straight to sensor, before the lens is undone. Projective, because a
    // keystone is. Rows of `vec4` so the padding is written down rather than
    // assumed -- see `the_straighten_uniform_is_laid_out_the_way_wgsl_reads_it`
    // for what happens when it is assumed.
    m0: vec4<f32>,
    m1: vec4<f32>,
    m2: vec4<f32>,
    // The optical centre in sensor pixels, and the frame's half-diagonal.
    centre: vec2<f32>,
    corner: f32,
    // How many of the sixteen below are knots. Under two is a photograph
    // nothing is correcting.
    knots: u32,
    // One over the image's size, so a sensor pixel becomes the fraction the
    // mask texture is indexed by.
    inverse_image: vec2<f32>,
    // Which layer to read, and how strongly to lay the tint on. A strength of
    // zero draws the border and nothing else, which is what a selected mask
    // looks like when the coverage tint is not being asked for.
    layer: u32,
    strength: f32,
    // How wide the border is, in canvas pixels, and how bright. Zero draws none.
    border: f32,
    brightness: f32,
    // The tint, in the canvas's own linear light.
    tint: vec4<f32>,
    // The lens's curve, resolved: amount applied, peak subtracted, divisor
    // divided out.
    curve: array<vec4<f32>, 4>,
}

// A render pass rather than a compute one, and not by preference: WebGPU only
// guarantees read-write storage textures for the 32-bit single-channel formats,
// and the canvas is `rgba16float`. Blending is what a render target does anyway,
// so this is the shorter road as well as the available one.
@group(0) @binding(0) var<uniform> overlay: Overlay;
@group(1) @binding(0) var mask_layers: texture_2d_array<f32>;
@group(1) @binding(1) var mask_sampler: sampler;

struct VsOut {
    @builtin(position) position: vec4<f32>,
}

// One triangle covering the target, the same trick `preview.wgsl` uses.
@vertex
fn vs(@builtin(vertex_index) index: u32) -> VsOut {
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);
    var out: VsOut;
    out.position = vec4<f32>(x * 4.0 - 1.0, 1.0 - y * 4.0, 0.0, 1.0);
    return out;
}

fn knot(i: u32) -> f32 {
    let v = overlay.curve[i >> 2u];
    switch (i & 3u) {
        case 0u: { return v.x; }
        case 1u: { return v.y; }
        case 2u: { return v.z; }
        default: { return v.w; }
    }
}

/// Where a point of the corrected photograph sits on the lens's own image.
///
/// The same arithmetic as `SensorMap::at`, walking the same resolved curve --
/// an overlay that undistorted by a slightly different amount from the render
/// would sit a little off the thing it is describing, which reads as the mask
/// drifting rather than as a coordinate bug.
fn undistort(at: vec2<f32>) -> vec2<f32> {
    if (overlay.knots < 2u || overlay.corner <= 0.0) {
        return at;
    }
    let offset = at - overlay.centre;
    let t = clamp(length(offset) / overlay.corner, 0.0, 1.0);
    let x = t * f32(overlay.knots - 1u);
    let i = min(u32(x), overlay.knots - 2u);
    let f = x - f32(i);
    return overlay.centre + offset * (1.0 + knot(i) * (1.0 - f) + knot(i + 1u) * f);
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    // `position.xy` is already at the pixel's centre, which is the convention the
    // resampler and the straighten both sample on.
    let straight = overlay.straight_origin + in.position.xy;
    let p = vec3<f32>(straight, 1.0);
    let homogeneous = vec3<f32>(
        dot(overlay.m0.xyz, p),
        dot(overlay.m1.xyz, p),
        dot(overlay.m2.xyz, p),
    );
    // Behind the horizon there is nothing to describe. The crop is pulled in far
    // enough that a legal edit cannot reach here.
    if (abs(homogeneous.z) < 1e-6) {
        discard;
    }
    let sensor = undistort(homogeneous.xy / homogeneous.z);
    let uv = sensor * overlay.inverse_image;
    // Outside the photograph there is no mask to read, and clamping would smear
    // the frame's edge weight along the whole border.
    if (uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0) {
        discard;
    }

    let weight = clamp(
        textureSampleLevel(mask_layers, mask_sampler, uv, overlay.layer, 0.0).r,
        0.0,
        1.0,
    );

    // The border is the **half-coverage contour of the mask itself**, not a
    // shape drawn from the numbers in the edit. Three things follow, and they
    // are the reason it is done here rather than by placing marks on the CPU:
    //
    // - It cannot disagree with the effect. An outline drawn from the ellipse's
    //   own parameters sits at the exact ellipse, while the adjustment fades
    //   across the raster's resolution — about six image pixels on a 24 MP
    //   frame — so the effect visibly ran past its own border. Here the border
    //   *is* where the mask is half on, so the adjustment is half inside it and
    //   half out, by construction.
    // - It is a **line**, continuous at any zoom and any rotation, rather than a
    //   run of small squares standing in for one.
    // - A brush and a range mask get one too. They have no shape to draw from,
    //   which is why they had no outline at all.
    //
    // `fwidth` is how far the coverage moves between neighbouring pixels, so
    // dividing by it turns a distance in coverage into a distance in *pixels* —
    // which is what makes the line the same width whatever the zoom is and
    // however soft the mask.
    var border = 0.0;
    if (overlay.border > 0.0) {
        let slope = max(fwidth(weight), 1e-5);
        border = 1.0 - smoothstep(0.0, overlay.border, abs(weight - 0.5) / slope);
    }

    let tint = weight * overlay.strength;
    // The border sits on top of the tint rather than adding to it, so a bright
    // line stays bright over a covered area instead of blowing out.
    let alpha = max(tint, border * overlay.brightness);
    if (alpha <= 0.0) {
        discard;
    }
    let colour = mix(overlay.tint.rgb, vec3<f32>(1.0), border);
    // **Premultiplied**, matching a premultiplied blend state, so the alpha is
    // applied exactly once. The canvas holds linear half floats and the tint is
    // in the same light, so nothing here is gamma-aware: the overlay is a mark
    // on the picture rather than a colour in it.
    return vec4<f32>(colour * alpha, alpha);
}
