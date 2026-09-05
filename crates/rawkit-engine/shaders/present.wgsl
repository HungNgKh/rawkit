// Canvas to screen — pipeline stage L, the output transform.
//
// This is the last thing that happens to a pixel, and the only place the sRGB
// transfer function is applied on the interactive path. Everything upstream is
// linear; a canvas is display-referred *linear*, and linear values written to a
// screen without encoding come out far too dark. That failure looks enough like
// an exposure mistake to be misdiagnosed as one, which is why it gets a test
// naming a number rather than a visual check.

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

// One triangle covering the screen, rather than two making a quad. No vertex
// buffer, no index buffer, and no seam down the diagonal where the two halves
// of a quad meet.
@vertex
fn vs(@builtin(vertex_index) index: u32) -> VsOut {
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);
    var out: VsOut;
    out.uv = vec2<f32>(x, y);
    out.position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

@group(0) @binding(0) var canvas: texture_2d<f32>;
@group(0) @binding(1) var canvas_sampler: sampler;
@group(0) @binding(4) var overlay: texture_2d<f32>;

// The photograph with whatever the interface drew on top of it.
//
// Premultiplied `over`: the overlay's colour is already scaled by its own
// coverage — that is what the mask pass writes and what a cell writes with an
// alpha of one — so compositing is an add and a fade, not a mix.
//
// **Before the encode, deliberately.** These marks used to live in the canvas
// and went through the transfer function with the photograph; doing it here
// keeps them looking exactly as they did, which is what makes moving them out
// of the canvas a change in structure and not in appearance.
fn composited(uv: vec2<f32>) -> vec3<f32> {
    let photograph = textureSample(canvas, canvas_sampler, uv);
    let over = textureSample(overlay, canvas_sampler, uv);
    return photograph.rgb * (1.0 - over.a) + over.rgb;
}

/// For an `-Srgb` target format, where the hardware encodes on write.
///
/// Doing it here as well would apply the curve twice, which washes the image
/// out — the mirror image of the too-dark failure and just as easy to mistake
/// for a grading problem.
@fragment
fn fs_hardware_encode(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(composited(in.uv), 1.0);
}

/// For a target that is not `-Srgb`. Surfaces do not always offer one, and a
/// linear image written to a non-encoding target is the too-dark failure above.
@fragment
fn fs_shader_encode(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(encode_srgb(composited(in.uv)), 1.0);
}

@group(0) @binding(2) var display_lut: texture_3d<f32>;
@group(0) @binding(3) var lut_sampler: sampler;

/// For a monitor whose own profile said it is not sRGB.
///
/// The table maps linear sRGB straight to that monitor's device values —
/// primaries, transfer curve and all — so what comes out here is already
/// encoded and the target must *not* be an `-Srgb` format. Encoding twice is
/// the washed-out failure; this path exists to avoid the subtler one, where a
/// wider-than-sRGB screen quietly oversaturates every colour.
///
/// Sampling is trilinear, done by the sampler. A 33-per-axis grid is what ICC
/// itself uses for `A2B` tables and its interpolation error sits far below what
/// an 8-bit framebuffer can show.
@fragment
fn fs_display_lut(in: VsOut) -> @location(0) vec4<f32> {
    let linear = clamp(composited(in.uv), vec3<f32>(0.0), vec3<f32>(1.0));
    // Sample at texel centres: a value of 0 must land on the first entry and 1
    // on the last, not half a texel outside either.
    let size = vec3<f32>(textureDimensions(display_lut));
    let uvw = (linear * (size - 1.0) + 0.5) / size;
    return vec4<f32>(textureSample(display_lut, lut_sampler, uvw).rgb, 1.0);
}

/// The sRGB transfer function. Not a tone curve: the tone map already ran, in
/// scene-linear light, several stages ago.
fn encode_srgb(c: vec3<f32>) -> vec3<f32> {
    let v = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    let low = v * 12.92;
    let high = 1.055 * pow(v, vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(high, low, v <= vec3<f32>(0.0031308));
}
