// A cached preview onto the canvas.
//
// The only shader in the engine whose input is *encoded* rather than linear.
// That is not an exception to the rule, it is the rule being enforced by the
// hardware: the texture is declared `Rgba8UnormSrgb`, so every sample is decoded
// to linear before it reaches this code, for free. The canvas it lands in is
// linear, and the presenter downstream encodes exactly once, as it always did.
//
// Writing the encoded bytes straight into a linear canvas would produce an image
// that is far too bright — the washed-out failure, the mirror of the too-dark
// one. Nothing here does that, and nothing here can: there is no path from the
// texture to the output that skips the sampler.

struct Region {
    // Where the visible rectangle starts in the preview, in [0, 1].
    origin: vec2<f32>,
    // How much of the preview it spans. Greater than 1 when the view is zoomed
    // out far enough that the whole image does not fill the canvas.
    span: vec2<f32>,
}

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

// One triangle, not two — same reasoning as `present.wgsl`.
@vertex
fn vs(@builtin(vertex_index) index: u32) -> VsOut {
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);
    var out: VsOut;
    out.uv = vec2<f32>(x, y);
    out.position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

@group(0) @binding(0) var image: texture_2d<f32>;
@group(0) @binding(1) var image_sampler: sampler;
@group(0) @binding(2) var<uniform> region: Region;

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let uv = region.origin + in.uv * region.span;
    // Outside the photograph is black, not the edge pixel smeared outwards.
    // Clamping would paint a border of stretched sky wherever the image does not
    // fill the view, which reads as part of the picture.
    if (uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    return vec4<f32>(textureSample(image, image_sampler, uv).rgb, 1.0);
}
