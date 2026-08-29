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
    // Multiplied into the sample. White for the loupe; a third for a frame that
    // has been rejected, so the shape of a cull is visible without reading
    // anything. In linear light, because that is what the canvas holds.
    tint: vec4<f32>,
    // Edge colour, and its thickness as a fraction of the cell. Thickness zero
    // means no edge — which is every cell the loupe ever draws.
    edge: vec4<f32>,
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
    // The edge is drawn in the cell's own rectangle rather than as extra
    // geometry, so selection and flags cost no draw calls and no second
    // pipeline. `in.uv` runs 0 to 1 across whatever rectangle was set.
    let thickness = region.edge.w;
    if (thickness > 0.0) {
        let inset = min(min(in.uv.x, in.uv.y), min(1.0 - in.uv.x, 1.0 - in.uv.y));
        if (inset < thickness) {
            return vec4<f32>(region.edge.rgb, 1.0);
        }
    }

    let uv = region.origin + in.uv * region.span;
    // Outside the photograph is black, not the edge pixel smeared outwards.
    // Clamping would paint a border of stretched sky wherever the image does not
    // fill the view, which reads as part of the picture.
    if (uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    return vec4<f32>(textureSample(image, image_sampler, uv).rgb * region.tint.rgb, 1.0);
}
