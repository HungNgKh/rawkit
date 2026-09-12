// RCD (Ratio Corrected Demosaicing) — Bayer CFA to RGB.
//
// Ported from vkdt's `rcd_conv.comp` and `rcd_fill.comp`
// (https://github.com/hanatos/vkdt, copyright 2019 johannes hanika,
// BSD-2-Clause). vkdt's readme states its code is BSD-2 unless a file is marked
// otherwise; neither RCD source carries a marking, which is the basis on which
// this port is BSD-2 rather than an assumption from the repository label. The
// algorithm itself is Luis Sanz Rodríguez's RCD; darktable's and RawTherapee's
// implementations of it are GPL and were deliberately not consulted.
//
// # How this differs from vkdt, and why
//
// vkdt runs the interpolation as one tiled kernel with the working set in f16
// workgroup memory. This port splits it into separate dispatches over plain
// storage buffers:
//
//   conv → green_at_rb → rb_at_br → rb_at_g → pack
//
// That is slower — it round-trips through global memory between stages — and it
// is deliberate for the first version. The stage boundaries here are exactly
// vkdt's `barrier()` calls, so each stage can be read against its original, and
// a wrong result can be localised to one dispatch instead of to a tile-indexing
// bug. WebGPU's 16 KB workgroup-storage limit and lack of guaranteed f16 also
// mean the tiled version needs its own tile-size decision, which is an
// optimisation with its own correctness question and does not belong in the
// same change as the port.
//
// # Two assumptions worth stating
//
// - **White balance has green normalised to 1.0.** vkdt scales the mosaic by
//   the WB multipliers on load and divides them out on store, but the green
//   estimate at R/B sites is computed from unscaled CFA values. Those two are
//   only consistent when `wb.g == 1`, which is the usual convention for camera
//   multipliers. The Rust side normalises before upload; do not remove that.
// - **Borders are clamped, not mirrored.** RCD reaches 4 pixels out, and
//   mirroring across an edge flips CFA parity unless it is done in even steps.
//   Clamping keeps the sampling in-bounds and leaves an incorrect frame a few
//   pixels wide, which production covers with a cheaper edge kernel. Callers
//   crop it; the tests measure PSNR on the interior.

struct Params {
    width: u32,
    height: u32,
    // Packed width for the half-resolution helper buffers, = (width + 1) / 2.
    packed_width: u32,
    // Phase of the Bayer pattern: the offset that makes pixel (0,0) behave as
    // the red site of an RGGB block. This is how the four Bayer layouts share
    // one kernel.
    cfa_x_offset: u32,
    cfa_y_offset: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
    // Per-channel multipliers, green normalised to 1.0. `.a` is unused.
    wb: vec4<f32>,
    // Camera-native RGB to the display's linear primaries, one row per vec4 so
    // the uniform stays std140-friendly. `.w` is unused.
    cam_to_display: array<vec4<f32>, 3>,
    // Working space back to display, used only when a hue/saturation table is
    // active. Identity otherwise, so the second multiply is harmless.
    working_to_display: array<vec4<f32>, 3>,
    // `.x` is the exposure multiplier (2^EV). `.y` is 1 when a hue/saturation
    // table is bound, 0 when it is not — the table buffer always exists because
    // a bind group cannot have holes, so a flag is what distinguishes "no
    // correction" from "a correction that happens to be identity". `.z` is the
    // camera-space value the sensor saturates at, normally 1.0.
    develop: vec4<f32>,
    // Hue, saturation and value divisions of the table. `.w` unused.
    hsm_dims: vec4<u32>,
    // The display space back to the profile's working space, so the look is
    // applied where it was authored. Ordered exactly as `Params` in render.rs:
    // the two are one memory layout described twice.
    display_to_working: array<vec4<f32>, 3>,
    // `.xyz` the look table's divisions, `.w` where it starts in the shared
    // table buffer.
    look_dims: vec4<u32>,
    // `[offset, entries, active, unused]` for the profile's tone curve, in the
    // same buffer behind the two tables.
    curve: vec4<u32>,
    // `[hue, saturation, luminance, unused]` for the shadows, midtones and
    // highlights, in that order. Ordered exactly as `Params` in render.rs.
    grade: array<vec4<f32>, 3>,
    // `[blending, balance, active, unused]`.
    grade_shape: vec4<f32>,
    // The same, for the user's own curve, behind the profile's.
    user_curve: vec4<u32>,
    // The display-referred tone controls, already reduced to curve parameters
    // on the CPU: `.x` is the contrast exponent, `.y` highlights, `.z` shadows,
    // and `.w` is 1 when any of the five is off its default. See `tone_curve`.
    tone: vec4<f32>,
    // `.xy` are the black and white points. `.zw` unused.
    levels: vec4<f32>,
    // How far the *local* tone operator will trust its own neighbourhood:
    // `.x` the brightest reference it will read a gain at, `.y` the darkest.
    // `.zw` unused. See `tone_curve`, and `ToneCurve::local` in Rust.
    tone_local: vec4<f32>,
    // The tone *map*'s parameters, as opposed to the tone *controls* above.
    // `.x` is how much of the compression keeps a colour's ratios; see
    // `hue_preserved`. The rest unused. Ordered exactly as `Params` in
    // render.rs.
    tone_map: vec4<f32>,
    // `.x` is the sharpening amount, `.y` its radius in pixels, `.z` the chroma
    // noise reduction. `.w` unused.
    detail: vec4<f32>,
    // `[clarity, texture, dehaze, the airlight's level]` -- local contrast at
    // three scales, and the one number the haze model needs that is not a
    // colour. The level is in white-balanced camera RGB, which is where the veil
    // is measured, so the two are comparable without a matrix.
    local_contrast: vec4<f32>,
    // The airlight in the profile's working space, already scaled by its level,
    // so the shader subtracts it rather than reconstructing it. `.w` unused.
    airlight: vec4<f32>,
    // `[vignette, midpoint, roundness, feather]` -- the post-crop vignette.
    effects: vec4<f32>,
    // `[grain amount, grain size in image pixels, lens vignette, unused]`.
    grain: vec4<f32>,
    // The crop in image pixels: `[centre x, centre y, half width, half height]`.
    // The vignette is centred here and not on the sensor, which is the whole
    // difference between it and the lens correction above.
    vignette_frame: vec4<f32>,
    // `.x` is saturation, `.y` vibrance, `.z` whether the hue mixer does
    // anything at all. `.w` unused.
    colour: vec4<f32>,
    // Per-band hue shift, saturation and luminance: eight bands each, packed
    // two to a row because a uniform array's stride is a `vec4` whatever is in
    // it. Ordered exactly as `Params` in render.rs — the two are one memory
    // layout described twice, and the colour tests are what noticed when they
    // disagreed.
    hsl_hue: array<vec4<f32>, 2>,
    hsl_saturation: array<vec4<f32>, 2>,
    hsl_luminance: array<vec4<f32>, 2>,
    // The local-tone guide: `[where it starts in the cfa buffer, its width, its
    // height, whether it is active]`. It lives in the tail of the `cfa` binding
    // because group 0 already spends all eight storage buffers WebGPU
    // guarantees -- the same reason `pq` and `lp` share one. Read-only and
    // written once when the image opens, so sharing with the per-tile mosaic
    // costs a longer buffer and nothing else.
    guide: vec4<u32>,
    // Guide texels per image pixel, `.xy`. A constant of the image, so the
    // per-tile arithmetic is a multiply. `.z` is 1 when the guide's chroma field
    // holds a real colour.
    guide_scale: vec4<f32>,
    // How many local adjustments are bound, in `.x`. The rest unused.
    masks: vec4<u32>,
    // One over the image's size, `.xy`: image pixels to the normalised
    // coordinates a mask texture is sampled in.
    mask_scale: vec4<f32>,
    // Lateral chromatic aberration: `.xy` are the radial rescalings for red and
    // blue as fractions, `.zw` the optical centre in full-resolution image
    // pixels. Zero in `.xy` is a lens with nothing to correct.
    lateral: vec4<f32>,
    // `.x` is how much of the clipping cast to take off an edge beside a blown
    // highlight. The rest unused.
    defringe: vec4<f32>,

    // The colour of the light that did not clip, for the whole frame: camera
    // RGB in `.xyz`, and `.w` is 1 when there was any to find. One triple and
    // not a field -- see `Guide::chroma`.
    guide_chroma: vec4<f32>,
    // What each local adjustment multiplies by at full strength, `.rgb`.
    //
    // Exposure and white balance arrive already combined, because both are
    // multiplies in this space and the shader has no reason to know which part
    // came from which control. `.a` unused.
    mask_gain: array<vec4<f32>, 8>,

    // The display-referred half of each local adjustment: contrast in `.x`,
    // saturation in `.y`, clarity in `.z`. A separate array because they are
    // applied on the far side of the tone map -- the mask is one texture and is
    // read twice, once for the multiplies before it and once for these after.
    mask_look: array<vec4<f32>, 8>,
    // Where this tile lands in the canvas and how to trim it: `.xy` is the
    // destination pixel, `.z` the tile edge, `.w` the halo width. Rewritten per
    // tile, unlike everything above it, which moves only when the edit does.
    //
    // Signed, because a tile that begins left of or above the viewport is the
    // ordinary case as soon as the image can be panned — the tile grid does not
    // move with the view.
    present: vec4<i32>,
    // How much of this tile is actually inside the image, in pixels: `.xy`.
    //
    // A tile at the right or bottom edge overhangs, and the gather clamps out
    // there — which repeats a column and so breaks the CFA phase, and a broken
    // phase demosaics to magenta rather than to something merely soft. The
    // whole-image path drops the overhang when it copies each tile out; the
    // canvas path has to be told.
    extent: vec4<i32>,
    // The rotation, as the two columns of a signed permutation: `.xy` is where a
    // step along the tile's x axis lands, `.zw` where a step along y lands.
    // Identity when the photograph is not turned.
    axes: vec4<i32>,
    // Where this tile's top-left *interior* pixel sits in the full-resolution
    // image, how many image pixels one tile pixel spans, and the halo width:
    // `[x, y, step, halo]`. Written per tile like the three above it.
    //
    // This is what lets the guide be indexed in image coordinates rather than
    // tile coordinates -- so two tiles agree exactly where they meet, and a
    // tile at level 3 reads the same guide as the same region at level 0.
    source: vec4<i32>,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> cfa: array<f32>;
@group(0) @binding(2) var<storage, read_write> vh: array<f32>;
// The two half-resolution helpers share one binding, `pq` first and `lp` after
// it. WebGPU guarantees only eight storage buffers per stage and the develop
// stage needs one for the profile table, so something had to give — and these
// two are the natural pair: identical shape, written in the same dispatch,
// never aliasing because the ranges are disjoint.
@group(0) @binding(3) var<storage, read_write> helpers: array<f32>;
@group(0) @binding(4) var<storage, read_write> ch_r: array<f32>;
@group(0) @binding(5) var<storage, read_write> ch_g: array<f32>;
@group(0) @binding(6) var<storage, read_write> ch_b: array<f32>;
@group(0) @binding(7) var<storage, read_write> rgba_out: array<vec4<f32>>;
// The profile's hue/saturation deltas, one vec4 per cell: hue shift in degrees,
// saturation scale, value scale. Padded to vec4 because a vec3 array has a
// stride of 16 bytes anyway.
@group(0) @binding(8) var<storage, read> hue_sat_map: array<vec4<f32>>;

// Local adjustments, one texture layer each: where the adjustment applies, from
// 0 outside to 1 at full strength.
//
// A *texture*, and that is the whole design. The renderer is handed a picture of
// where an adjustment applies and never asks where it came from, so a gradient,
// a brush stroke, a luminance range and a segmentation matte all enter the same
// way. Sampled textures are a separate WebGPU limit from storage buffers
// (sixteen per stage against eight), so this costs nothing against the eight
// already spent -- unlike the guide, which had to ride inside `cfa`.
@group(1) @binding(0) var mask_layers: texture_2d_array<f32>;
@group(1) @binding(1) var mask_sampler: sampler;

// The canvas, in its own bind group so it can be resized with the window
// without disturbing the per-image buffers in group 0. Storage textures are a
// separate WebGPU limit from storage buffers (four per stage against eight), so
// this costs nothing against the eight already spent.
//
// rgba16float, not rgba8: the pixels here are display-referred *linear* and the
// transfer function belongs to whoever presents them. Eight bits of linear
// would band visibly in the shadows once encoded. Not rgba32float either —
// half floats carry more precision than a display can show, at half the
// bandwidth, and bandwidth is what a canvas spends.
@group(2) @binding(0) var canvas: texture_storage_2d<rgba16float, write>;

const EPS: f32 = 1e-5;

// 0 = red, 1 = green, 2 = blue. vkdt's `col()`, with the pattern phase applied
// so a non-RGGB sensor needs no second kernel.
fn colour_at(x: i32, y: i32) -> u32 {
    let px = x + i32(params.cfa_x_offset);
    let py = y + i32(params.cfa_y_offset);
    if (((px + py) & 1) == 1) {
        return 1u;
    }
    if ((py & 1) == 0) {
        return 0u;
    }
    return 2u;
}

fn clamp_x(x: i32) -> i32 { return clamp(x, 0, i32(params.width) - 1); }
fn clamp_y(y: i32) -> i32 { return clamp(y, 0, i32(params.height) - 1); }
fn idx(x: i32, y: i32) -> u32 { return u32(clamp_y(y)) * params.width + u32(clamp_x(x)); }

fn cfa_at(x: i32, y: i32) -> f32 { return cfa[idx(x, y)]; }
fn vh_at(x: i32, y: i32) -> f32 { return vh[idx(x, y)]; }

// The half-resolution helpers are stored one value per *pair* of columns, as in
// vkdt: only half the pixels in a row write one, so the writer's `x / 2` is
// dense. The reader has to land on the same slot from any pixel, which means
// knowing which parity of column holds the sites in this row.
//
// vkdt derives that from row parity alone, because it only ever handles RGGB.
// Here the pattern phase shifts it: with `cfa_x_offset == 1` the red and blue
// sites move to the other parity, and reading with vkdt's formula silently
// fetches the neighbouring pair's value. It still produces an image — about
// 6 dB worse, uniformly soft — which is why the four-phase test exists.
fn packed_index(x: i32, y: i32) -> u32 {
    let cx = clamp_x(x);
    let cy = clamp_y(y);
    // Column parity holding the red/blue sites in this row.
    let site_parity = (cy + i32(params.cfa_y_offset) + i32(params.cfa_x_offset)) & 1;
    let adjust = 1 - site_parity;
    // The `+ adjust` rounds up to the next pair, which on the last column of an
    // even-width image lands one slot past the end of the row — and since these
    // buffers are flat, that silently reads the *next row's* first value rather
    // than going out of bounds where anything would notice. Clamp to the row.
    let slot = min(u32((cx + adjust) / 2), params.packed_width - 1u);
    return u32(cy) * params.packed_width + slot;
}

/// Where `lp` starts inside the shared helper buffer.
fn lp_base() -> u32 { return params.packed_width * params.height; }

fn pq_at(x: i32, y: i32) -> f32 { return helpers[packed_index(x, y)]; }
fn lp_at(x: i32, y: i32) -> f32 { return helpers[lp_base() + packed_index(x, y)]; }

// vkdt picks between a discriminator and the mean of its diagonal neighbours,
// preferring whichever is further from 0.5 — i.e. whichever is more confident
// about a direction.
fn discriminate(centre: f32, neighbours: f32) -> f32 {
    return select(centre, neighbours, abs(0.5 - centre) < abs(0.5 - neighbours));
}

// ---------------------------------------------------------------------------
// Stage 1 — directional discriminators, and seeding the colour planes.
//
// vkdt's rcd_conv.comp. The plane seeding is folded in here because this is the
// only stage that already visits every pixel exactly once.
// ---------------------------------------------------------------------------
@compute @workgroup_size(8, 8)
fn conv(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= i32(params.width) || y >= i32(params.height)) {
        return;
    }
    let p = idx(x, y);

    // Vertical/horizontal discriminator, every pixel.
    var vhs = vec2<f32>(0.0);
    for (var i = -1; i <= 1; i = i + 1) {
        vhs = vhs + vec2<f32>(
            cfa_at(x + i, y - 3) - 3.0 * cfa_at(x + i, y - 2) - cfa_at(x + i, y - 1)
                + 6.0 * cfa_at(x + i, y) - cfa_at(x + i, y + 1)
                - 3.0 * cfa_at(x + i, y + 2) + cfa_at(x + i, y + 3),
            cfa_at(x - 3, y + i) - 3.0 * cfa_at(x - 2, y + i) - cfa_at(x - 1, y + i)
                + 6.0 * cfa_at(x, y + i) - cfa_at(x + 1, y + i)
                - 3.0 * cfa_at(x + 2, y + i) + cfa_at(x + 3, y + i),
        );
    }
    vhs = vhs * vhs;
    vh[p] = vhs.x / (EPS + vhs.x + vhs.y);

    let c = colour_at(x, y);
    let v = cfa[p];

    if (c != 1u) {
        // Diagonal discriminator, red and blue sites only.
        var pqs = vec2<f32>(EPS);
        for (var i = -1; i <= 1; i = i + 1) {
            pqs = pqs + vec2<f32>(
                cfa_at(x - 3 + i, y - 3 + i) - cfa_at(x + 1 + i, y - 1 + i)
                    - cfa_at(x + 1 + i, y + 1 + i) + cfa_at(x + 3 + i, y + 3 + i)
                    - 3.0 * (cfa_at(x - 2 + i, y - 2 + i) + cfa_at(x + 2 + i, y + 2 + i))
                    + 6.0 * cfa_at(x + i, y + i),
                cfa_at(x + 3 + i, y - 3 - i) - cfa_at(x + 1 + i, y - 1 - i)
                    - cfa_at(x - 1 + i, y + 1 - i) + cfa_at(x - 3 + i, y + 3 - i)
                    - 3.0 * (cfa_at(x + 2 + i, y - 2 - i) + cfa_at(x - 2 + i, y + 2 - i))
                    + 6.0 * cfa_at(x + i, y - i),
            );
        }
        pqs = pqs * pqs;
        helpers[u32(y) * params.packed_width + u32(x / 2)] = pqs.x / (pqs.x + pqs.y);
    } else {
        // Low-pass, green sites only, but centred on the red/blue column *of
        // this same packed pair* — because that is where it gets read from.
        // Columns pair as (2k, 2k+1) regardless of the pattern phase, so the
        // partner is found by raw column parity and must NOT be offset by the
        // phase: doing that centres the filter on the neighbouring pair's site
        // and costs several dB on exactly the layouts where the phase differs.
        var low = 0.0;
        let off = select(1, -1, (x & 1) == 1);
        let w = array<f32, 3>(0.5, 1.0, 0.5);
        for (var j = -1; j <= 1; j = j + 1) {
            for (var i = -1; i <= 1; i = i + 1) {
                low = low + w[j + 1] * w[i + 1] * cfa_at(x + i + off, y + j);
            }
        }
        helpers[lp_base() + u32(y) * params.packed_width + u32(x / 2)] = max(1e-6, low);
    }

    // Seed the colour planes: each site knows one channel, the other two are 0.
    ch_r[p] = select(0.0, params.wb.r * v, c == 0u);
    ch_g[p] = select(0.0, params.wb.g * v, c == 1u);
    ch_b[p] = select(0.0, params.wb.b * v, c == 2u);
}

// ---------------------------------------------------------------------------
// Stage 2 — green at red and blue sites.
//
// The ratio correction the algorithm is named for: neighbouring green samples
// are scaled by the ratio of local low-pass green, so the estimate follows the
// local signal level instead of averaging across an edge.
// ---------------------------------------------------------------------------
@compute @workgroup_size(8, 8)
fn green_at_rb(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= i32(params.width) || y >= i32(params.height)) {
        return;
    }
    if (colour_at(x, y) == 1u) {
        return;
    }

    let vhc = vh_at(x, y);
    let vhn = 0.25 * (vh_at(x - 1, y - 1) + vh_at(x + 1, y - 1)
                    + vh_at(x - 1, y + 1) + vh_at(x + 1, y + 1));
    let vh_discr = discriminate(vhc, vhn);

    let n_grad = EPS + abs(cfa_at(x, y - 1) - cfa_at(x, y + 1))
                     + abs(cfa_at(x, y) - cfa_at(x, y - 2))
                     + abs(cfa_at(x, y - 1) - cfa_at(x, y - 3))
                     + abs(cfa_at(x, y - 2) - cfa_at(x, y - 4));
    let s_grad = EPS + abs(cfa_at(x, y - 1) - cfa_at(x, y + 1))
                     + abs(cfa_at(x, y) - cfa_at(x, y + 2))
                     + abs(cfa_at(x, y + 1) - cfa_at(x, y + 3))
                     + abs(cfa_at(x, y + 2) - cfa_at(x, y + 4));
    let w_grad = EPS + abs(cfa_at(x - 1, y) - cfa_at(x + 1, y))
                     + abs(cfa_at(x, y) - cfa_at(x - 2, y))
                     + abs(cfa_at(x - 1, y) - cfa_at(x - 3, y))
                     + abs(cfa_at(x - 2, y) - cfa_at(x - 4, y));
    let e_grad = EPS + abs(cfa_at(x - 1, y) - cfa_at(x + 1, y))
                     + abs(cfa_at(x, y) - cfa_at(x + 2, y))
                     + abs(cfa_at(x + 1, y) - cfa_at(x + 3, y))
                     + abs(cfa_at(x + 2, y) - cfa_at(x + 4, y));

    let lp_c = lp_at(x, y);
    let n_est = cfa_at(x, y - 1) * 2.0 * lp_c / (EPS + lp_c + lp_at(x, y - 2));
    let s_est = cfa_at(x, y + 1) * 2.0 * lp_c / (EPS + lp_c + lp_at(x, y + 2));
    let w_est = cfa_at(x - 1, y) * 2.0 * lp_c / (EPS + lp_c + lp_at(x - 2, y));
    let e_est = cfa_at(x + 1, y) * 2.0 * lp_c / (EPS + lp_c + lp_at(x + 2, y));

    let v_est = clamp((s_grad * n_est + n_grad * s_est) / (n_grad + s_grad), 0.0, 65534.0);
    let h_est = clamp((w_grad * e_est + e_grad * w_est) / (e_grad + w_grad), 0.0, 65534.0);

    ch_g[idx(x, y)] = mix(v_est, h_est, vh_discr);
}

// ---------------------------------------------------------------------------
// Stage 3 — the opposite chroma at red and blue sites, along diagonals.
//
// Interpolation happens on the colour *difference* (C − G) rather than on C,
// which is what keeps chroma from bleeding across luminance edges.
// ---------------------------------------------------------------------------
@compute @workgroup_size(8, 8)
fn rb_at_br(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= i32(params.width) || y >= i32(params.height)) {
        return;
    }
    let c = colour_at(x, y);
    if (c == 1u) {
        return;
    }
    let red = c == 0u;

    let pqc = pq_at(x, y);
    let pqn = 0.25 * (pq_at(x - 1, y - 1) + pq_at(x + 1, y - 1)
                    + pq_at(x - 1, y + 1) + pq_at(x + 1, y + 1));
    let pq_discr = discriminate(pqc, pqn);

    // The diagonal neighbours of a red site are blue, and vice versa.
    let nw = sample_opposite(red, x - 1, y - 1);
    let ne = sample_opposite(red, x + 1, y - 1);
    let sw = sample_opposite(red, x - 1, y + 1);
    let se = sample_opposite(red, x + 1, y + 1);
    let nw3 = sample_opposite(red, x - 3, y - 3);
    let ne3 = sample_opposite(red, x + 3, y - 3);
    let sw3 = sample_opposite(red, x - 3, y + 3);
    let se3 = sample_opposite(red, x + 3, y + 3);

    let g_c = ch_g[idx(x, y)];
    let nw_grad = EPS + abs(nw - se) + abs(nw - nw3) + abs(g_c - ch_g[idx(x - 2, y - 2)]);
    let ne_grad = EPS + abs(ne - sw) + abs(ne - ne3) + abs(g_c - ch_g[idx(x + 2, y - 2)]);
    let sw_grad = EPS + abs(ne - sw) + abs(sw - sw3) + abs(g_c - ch_g[idx(x - 2, y + 2)]);
    let se_grad = EPS + abs(nw - se) + abs(se - se3) + abs(g_c - ch_g[idx(x + 2, y + 2)]);

    let nw_est = nw - ch_g[idx(x - 1, y - 1)];
    let ne_est = ne - ch_g[idx(x + 1, y - 1)];
    let sw_est = sw - ch_g[idx(x - 1, y + 1)];
    let se_est = se - ch_g[idx(x + 1, y + 1)];

    let p_est = (nw_grad * se_est + se_grad * nw_est) / (nw_grad + se_grad);
    let q_est = (ne_grad * sw_est + sw_grad * ne_est) / (ne_grad + sw_grad);
    let value = clamp(g_c + mix(p_est, q_est, pq_discr), 0.0, 65535.0);

    if (red) {
        ch_b[idx(x, y)] = value;
    } else {
        ch_r[idx(x, y)] = value;
    }
}

fn sample_opposite(red: bool, x: i32, y: i32) -> f32 {
    let p = idx(x, y);
    return select(ch_r[p], ch_b[p], red);
}

// ---------------------------------------------------------------------------
// Stage 4 — red and blue at green sites, along the axes.
//
// Both chroma channels at once: at a green site the horizontal and vertical
// neighbours are one red pair and one blue pair.
// ---------------------------------------------------------------------------
@compute @workgroup_size(8, 8)
fn rb_at_g(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= i32(params.width) || y >= i32(params.height)) {
        return;
    }
    if (colour_at(x, y) != 1u) {
        return;
    }

    let vhc = vh_at(x, y);
    let vhn = 0.25 * (vh_at(x - 1, y - 1) + vh_at(x + 1, y - 1)
                    + vh_at(x - 1, y + 1) + vh_at(x + 1, y + 1));
    let vh_discr = discriminate(vhc, vhn);

    let g_c = ch_g[idx(x, y)];
    let n1 = EPS + abs(g_c - ch_g[idx(x, y - 2)]);
    let s1 = EPS + abs(g_c - ch_g[idx(x, y + 2)]);
    let w1 = EPS + abs(g_c - ch_g[idx(x - 2, y)]);
    let e1 = EPS + abs(g_c - ch_g[idx(x + 2, y)]);

    for (var c = 0u; c < 2u; c = c + 1u) {
        let n = sample_plane(c, x, y - 1);
        let s = sample_plane(c, x, y + 1);
        let w = sample_plane(c, x - 1, y);
        let e = sample_plane(c, x + 1, y);
        let sn_abs = abs(n - s);
        let ew_abs = abs(w - e);

        let n_grad = n1 + sn_abs + abs(n - sample_plane(c, x, y - 3));
        let s_grad = s1 + sn_abs + abs(s - sample_plane(c, x, y + 3));
        let w_grad = w1 + ew_abs + abs(w - sample_plane(c, x - 3, y));
        let e_grad = e1 + ew_abs + abs(e - sample_plane(c, x + 3, y));

        let n_est = n - ch_g[idx(x, y - 1)];
        let s_est = s - ch_g[idx(x, y + 1)];
        let w_est = w - ch_g[idx(x - 1, y)];
        let e_est = e - ch_g[idx(x + 1, y)];

        let v_est = (n_grad * s_est + s_grad * n_est) / (n_grad + s_grad);
        let h_est = (e_grad * w_est + w_grad * e_est) / (e_grad + w_grad);
        let value = clamp(g_c + mix(v_est, h_est, vh_discr), 0.0, 65535.0);

        if (c == 0u) {
            ch_r[idx(x, y)] = value;
        } else {
            ch_b[idx(x, y)] = value;
        }
    }
}

fn sample_plane(c: u32, x: i32, y: i32) -> f32 {
    let p = idx(x, y);
    return select(ch_b[p], ch_r[p], c == 0u);
}

// ---------------------------------------------------------------------------
// Stage 5 — undo the white-balance scaling and interleave for readback.
// ---------------------------------------------------------------------------
@compute @workgroup_size(8, 8)
fn pack(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= i32(params.width) || y >= i32(params.height)) {
        return;
    }
    let p = idx(x, y);
    rgba_out[p] = vec4<f32>(
        lateral_sample(0u, x, y, params.lateral.x) / params.wb.r,
        ch_g[p] / params.wb.g,
        lateral_sample(2u, x, y, params.lateral.y) / params.wb.b,
        1.0,
    );
}

/// One channel of the demosaiced tile, read from where the lens actually put it.
///
/// Lateral chromatic aberration is a *scale* difference: red and blue land as
/// images of slightly different size about the optical axis, so undoing it means
/// sampling them from a radius scaled by the reciprocal amount. Green is left
/// alone, because green is what the other two are being lined up with.
///
/// This is stage `LensCorrection`, and it happens here rather than anywhere
/// later for a reason that outlives the arithmetic: after the white balance
/// multiply the three channels are still separable, but one matrix multiply
/// after that they are mixed, and there is no longer any such thing as
/// "displace the red channel".
fn lateral_sample(c: u32, x: i32, y: i32, coefficient: f32) -> f32 {
    if (coefficient == 0.0) {
        return select(ch_b[idx(x, y)], ch_r[idx(x, y)], c == 0u);
    }
    // The displacement is radial and proportional to the radius, worked out in
    // *image* pixels so that two tiles agree where they meet and a coarse level
    // reads the same correction as the same region at level zero -- the same
    // reason the guide and the masks are indexed through `image_xy`.
    let offset = image_xy(x, y) - params.lateral.zw;
    // Back into tile pixels, which is what `ch_r` is indexed in. A level-n tile
    // spans `source.z` image pixels per tile pixel, so the same fraction of the
    // radius is a proportionally smaller step here -- which is correct, and is
    // why a zoomed-out view gets the same correction rather than n times it.
    let step = f32(max(params.source.z, 1));
    // Clamped to the halo, and not as a formality: past it the read lands on
    // demosaic output that the tile edge got wrong, and the failure would look
    // like a grid at the seams rather than like a bad correction.
    let reach = f32(params.source.w);
    let d = clamp(offset * coefficient / step, vec2<f32>(-reach), vec2<f32>(reach));

    let fx = f32(x) + d.x;
    let fy = f32(y) + d.y;
    let x0 = i32(floor(fx));
    let y0 = i32(floor(fy));
    let tx = fx - f32(x0);
    let ty = fy - f32(y0);
    // Bilinear, because the whole signal here is sub-pixel: a nearest read would
    // quantise a third of a pixel of aberration to nothing or to one whole
    // pixel, and both are worse than leaving the picture alone.
    let a = mix(sample_plane(c, x0, y0), sample_plane(c, x0 + 1, y0), tx);
    let b = mix(sample_plane(c, x0, y0 + 1), sample_plane(c, x0 + 1, y0 + 1), tx);
    return mix(a, b, ty);
}

// ---------------------------------------------------------------------------
// Stage 5' -- defringe: take the clipping cast off the edge of a highlight.
//
// # The artefact, and why nothing else in the pipeline can see it
//
// A sensor clips at one value in its own space. White balance moves that to a
// different height per channel, so a blown neutral sky arrives at `wb * clip` --
// on an ILCE-6400 that is (2.63, 1.00, 1.82), which is *magenta*. Highlight
// reconstruction knows this and replaces it with neutral, which is why open sky
// renders white.
//
// A pixel on the edge of a bare twig is a **mixture** of that light with honest
// dark content. It carries the whole lie, scaled down: its ratios are still the
// sky's magenta while its level sits at half the threshold. Reconstruction
// cannot see it, because reconstruction asks about the pixel's own level and
// this pixel is nowhere near clipping. Measured on a synthetic scene that is
// neutral everywhere in the truth, a dark line down a sky at 0.95x the clip
// level has a cast of 0.000; at 1.6x it is 0.647; at 3.0x it is 2.229 with green
// driven to zero. Every bit of that colour is manufactured here.
//
// # The correction, which is the same arithmetic reconstruction already does
//
// Clipping displaced the light by `d = max(T) - T`, where `T` is the per-channel
// threshold: the channel with the lowest threshold lost the most, which is why
// the cast is magenta and not some other colour. Undoing it is a step back along
// that vector, and **how far** is the one thing worth solving properly.
//
// The step is chosen to leave the pixel as close to neutral as a move along `d`
// can bring it -- a one-dimensional least squares with a closed form. Two
// properties fall out of that and neither was designed in:
//
// - **It cannot overshoot.** The minimum of a parabola is not past itself, so
//   the correction can never carry a magenta rim through neutral and out the
//   other side into green.
// - **It only moves the green-magenta axis.** `d`'s chromatic part is very
//   nearly (-1, 1, 0) once the multipliers are real numbers, so a genuinely
//   blue or orange subject beside a highlight is barely touched. That is the
//   axis every defringe control in every editor offers, arrived at from the
//   arithmetic rather than from the convention.
//
// And for a *fully* clipped pixel the same formula returns exactly what
// reconstruction returns -- neutral at the brightest channel. The two are one
// idea applied at two levels of the same mixture, which is the reason to trust
// it on the pixels in between.
//
// Two passes, like the chroma and luminance filters: the neighbourhood scalar is
// worked out first, and the second pass is then a per-pixel operation that can
// read and write the same buffer without a race.
// ---------------------------------------------------------------------------

/// How far a blown pixel casts onto its neighbours, in pixels.
///
/// The rim is one to three pixels wide -- the demosaic's own reach plus whatever
/// the lens spread. Four covers it with a margin, and is folded into `HALO` in
/// `render.rs`.
const FRINGE_REACH: i32 = 4;

/// The per-channel value a blown pixel arrives at, after white balance.
///
/// One place, because `reconstruct_highlights` asks the same question and the
/// two answers must not be able to differ.
fn clip_thresholds() -> vec3<f32> {
    return params.wb.rgb * params.develop.z;
}

/// How far gone each channel is. The same measure `reconstruct_highlights`
/// opens with, and shared with it so the two cannot come to disagree about
/// where clipping starts.
fn clipped_at(balanced: vec3<f32>) -> vec3<f32> {
    let t = clip_thresholds();
    return smoothstep(t * (1.0 - CLIP_RUNUP), t, balanced);
}

@compute @workgroup_size(8, 8)
fn defringe_scan(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= i32(params.width) || y >= i32(params.height)) {
        return;
    }
    // How much blown light is close enough to have leaked into this pixel.
    //
    // A *max* with a distance falloff rather than a mean: one blown neighbour is
    // enough to contaminate, and averaging would let the size of the highlight
    // decide how hard the correction pushes -- so a thin bright gap between two
    // dark branches would be treated more gently than a wide sky for no reason.
    // The falloff is what keeps the edge of the correction from being an edge.
    var near = 0.0;
    let reach = f32(FRINGE_REACH) + 1.0;
    for (var j = -FRINGE_REACH; j <= FRINGE_REACH; j = j + 1) {
        for (var i = -FRINGE_REACH; i <= FRINGE_REACH; i = i + 1) {
            let sx = clamp(x + i, 0, i32(params.width) - 1);
            let sy = clamp(y + j, 0, i32(params.height) - 1);
            let c = clipped_at(rgba_out[idx(sx, sy)].rgb * params.wb.rgb);
            // The *first* channel to go, because that is when the neighbour's
            // colour stopped being a measurement and started being able to leak
            // a false one into this pixel.
            let b = max(c.r, max(c.g, c.b));
            let falloff = 1.0 - length(vec2<f32>(f32(i), f32(j))) / reach;
            near = max(near, b * max(falloff, 0.0));
        }
    }
    // The red plane, which the demosaic has finished with and the chroma blur
    // has not started on. Read back by `defringe_apply` and overwritten there
    // after, so nothing downstream sees it.
    ch_r[idx(x, y)] = near;
}

@compute @workgroup_size(8, 8)
fn defringe_apply(@builtin(global_invocation_id) gid: vec3<u32>) {
    let amount = params.defringe.x;
    // Exactly zero changes exactly nothing, the same contract the noise
    // reduction keeps: a stored edit can turn this off rather than nearly off.
    if (amount <= 0.0) {
        return;
    }
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= i32(params.width) || y >= i32(params.height)) {
        return;
    }
    let p = idx(x, y);
    let near = ch_r[p];
    if (near <= 0.0) {
        return;
    }
    let wb = params.wb.rgb;
    let balanced = rgba_out[p].rgb * wb;
    let t = clip_thresholds();
    // Hand over to reconstruction as this pixel's *first* channel goes. Both
    // stages undo the same displacement, so running them in turn on one pixel
    // would undo it twice.
    //
    // The first channel and not the last, and that was measured rather than
    // reasoned. Handing over on the last is tempting — reconstruction is at its
    // worst when the channels disagree, because it then anchors to the ones that
    // have not clipped and treats them as facts, and on a rim pixel a surviving
    // channel is a mixture rather than a fact. But on the frame this was
    // diagnosed from, the pixels with one channel gone are ones reconstruction
    // handles *well*: a pixel whose green is at its threshold while red sits at
    // 0.6 of its own has an honest red to anchor to, and taking it away from
    // reconstruction moved it from a cast of 0.03 to 0.11. The rim itself is
    // nowhere near clipping in any channel, so it is kept either way.
    let clipped = clipped_at(balanced);
    let mine = 1.0 - max(clipped.r, max(clipped.g, clipped.b));
    let weight = amount * near * mine;
    if (weight <= 0.0) {
        return;
    }

    // What clipping did: the channel with the lowest threshold lost the most.
    let displaced = vec3<f32>(max(t.r, max(t.g, t.b))) - t;
    // Only the chromatic part of either matters. A move that changes all three
    // channels together is a brightness change, and brightness is not what is
    // wrong here -- the twig really is dark.
    let dm = displaced - vec3<f32>((displaced.r + displaced.g + displaced.b) / 3.0);
    let pm = balanced - vec3<f32>((balanced.r + balanced.g + balanced.b) / 3.0);
    let span = dot(dm, dm);
    if (span <= EPS) {
        // Neutral multipliers, so clipping had no colour to give and there is
        // nothing here to take away.
        return;
    }
    // The step along `displaced` that leaves the least colour behind. Negative
    // means the pixel is already on the far side, and stepping would *add* the
    // cast rather than remove it.
    let best = -dot(pm, dm) / span;
    // And no more clipped light than the pixel could physically hold: a blown
    // contribution of `a` puts at least `a * t` into every channel, so a dark
    // pixel cannot be mostly highlight however magenta it looks. This is what
    // keeps the correction off a deep shadow that happens to sit beside a
    // window.
    let room = min(balanced.r / t.r, min(balanced.g / t.g, balanced.b / t.b));
    let step = clamp(best, 0.0, room);

    let repaired = balanced + weight * step * displaced;
    rgba_out[p] = vec4<f32>(repaired / wb, 1.0);
}

// ---------------------------------------------------------------------------
// Stage 6 — develop: white balance, camera profile, exposure, tone map.
//
// Runs on the same tile, in the same dispatch chain, reading the demosaiced
// pixels in place. Keeping it here rather than on the CPU is not an
// optimisation: it is what makes preview and export share the arithmetic, and
// what will let the interactive canvas re-render a tile when a slider moves
// without touching the demosaic again.
//
// The pipeline order is `Stage`'s order, not a convenient one:
//   white balance (D) -> camera profile (E) -> exposure (F) -> tone map (H)
// Exposure commutes with the matrix, so applying it after costs nothing and
// keeps the code readable against the declared stage list.
//
// Output is display-referred **linear**, not encoded. The transfer function
// belongs to the output transform (stage L), which is lcms2's job and is not
// written yet; encoding here would bake sRGB into every consumer.
// ---------------------------------------------------------------------------
@compute @workgroup_size(8, 8)
fn develop(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= i32(params.width) || y >= i32(params.height)) {
        return;
    }
    let p = idx(x, y);
    // Where this pixel sits in the *full-resolution image*. Worked out once:
    // highlight reconstruction, the local tone controls and the local
    // adjustments all ask about the same place, and asking three times invites
    // them to drift apart.
    let ixy = image_xy(x, y);
    let looked = develop_rgb(rgba_out[p].rgb, ixy, true);

    // Stage I -- display-referred ops. The five tone controls live here and not
    // beside exposure, because the sigmoid is the boundary: exposure decides how
    // much light there was, these decide what it should look like.
    //
    // The neighbourhood's brightness is resolved once per pixel and handed to
    // all three channels, so shadows and highlights move a colour without
    // turning it -- see `tone_curve`.
    let local = local_tone(ixy);
    var shaped = tone_curve_rgb(looked, local);
    // And the hand-drawn curve last of the tone controls, so it shapes what the
    // sliders left rather than competing with them.
    if (params.user_curve.z > 0u) {
        shaped = vec3<f32>(
            user_curve(shaped.r),
            user_curve(shaped.g),
            user_curve(shaped.b),
        );
    }
    // The display-referred half of the local adjustments, here and not beside
    // the multiplies at stage G: contrast and saturation are about the picture,
    // and in scene-linear light they would both depend on the exposure. The
    // neighbourhood the clarity works against is the local-tone guide's, which
    // is already resolved for this pixel.
    // Clarity: contrast against the neighbourhood rather than against a fixed
    // grey, at the guide's own scale. Before the local half, so a mask refines
    // what the global slider did rather than arguing with it -- the same order
    // every other pair of global and local controls is in.
    //
    // Through the same curve the pixel just went through, or the two are being
    // compared in different coordinates. See `neighbourhood_after_curve`.
    let around = neighbourhood_after_curve(local);
    shaped = against_neighbourhood(shaped, around, params.local_contrast.x);
    shaped = local_look(shaped, ixy, around);

    // Stage J -- colour adjustments, after the tone curve for the same reason
    // the tone curve is after the tone map: this is about the picture, not the
    // light. In scene-linear it would depend on exposure, and a colour that
    // changed when you brightened the frame is not a colour control.
    // Stage K -- the look, and the last thing that touches colour.
    // Stage L -- the effects. A vignette and grain are the last decisions about
    // the picture, and they go on after the look for the same reason a frame
    // goes on after the painting.
    let finished = grade_colour(mix_bands(saturate_colour(shaped)));
    rgba_out[p] = vec4<f32>(apply_effects(finished, ixy), 1.0);
}

/// The camera's RGB to a rendered picture, short of the user's tone controls.
///
/// Split out of `develop` so the local-tone guide can be put through exactly
/// the same chain: the guide is stored as raw camera RGB, and what the operator
/// needs to know is how bright the neighbourhood *ends up*. Developing it by a
/// second, simpler route would make the guide disagree with the picture it is
/// describing, by an amount that changes with every slider.
fn develop_rgb(camera: vec3<f32>, ixy: vec2<f32>, hazy: bool) -> vec3<f32> {
    // White balance is a plain multiply because the working space is
    // scene-linear. That is the payoff of the linear core, and the reason this
    // is three multiplies rather than a colour-appearance model.
    let balanced = camera * params.wb.rgb;

    // Highlight reconstruction runs *here*, between white balance and the
    // profile, and not at the scene-linear stage where the design originally
    // placed it. It has to: a channel's clipping is a fact about the sensor, so
    // it is only visible while the channels are still the sensor's own. One
    // matrix multiply later they are mixed and there is no longer any such
    // thing as "the green channel clipped".
    var recovered = reconstruct_highlights(balanced, ixy);

    // The lens's own falloff, undone where the lens caused it: a plain multiply
    // about the *sensor's* centre, in scene-linear light, because attenuation is
    // what glass does and undoing an attenuation is a division. After the
    // highlight reconstruction and not before it -- the sensor clipped the light
    // the lens had already dimmed, so lifting the corners first would hand the
    // reconstruction a frame whose clipping had moved.
    if (params.grain.z != 0.0) {
        recovered = recovered * lens_falloff(ixy);
    }

    // The shadow tint, before the profile and after the balance. Green at
    // negative, magenta at positive, and weighted to the shadows -- which is the
    // whole of why it is not white balance. A sensor's channels agree least
    // where there is least light, so a cast that is invisible in the midtones
    // can be plain in the darks, and correcting it with the temperature would
    // take the midtones with it.
    //
    // The weight falls off with the balanced luminance rather than with a single
    // channel, so it does not itself depend on the cast it is correcting. Half
    // strength at 5% of full scale, and nothing left by about 30%: the shadows
    // are a small part of the range and this is a correction, not a grade.
    let tint = params.cam_to_display[0].w;
    if (tint != 0.0) {
        let luma = dot(recovered, vec3<f32>(0.2126, 0.7152, 0.0722));
        let weight = 1.0 / (1.0 + max(luma, 0.0) / 0.05);
        // Green one way, magenta the other, as one multiply on the green
        // channel: magenta is the absence of green, so a single scale expresses
        // both directions and cannot pull the picture off neutral in some third
        // way.
        recovered.g = recovered.g * (1.0 - tint * 0.25 * weight);
    }

    let display = vec3<f32>(
        dot(params.cam_to_display[0].rgb, recovered),
        dot(params.cam_to_display[1].rgb, recovered),
        dot(params.cam_to_display[2].rgb, recovered),
    );

    // The hue/saturation correction, when the profile brought one. `display`
    // is in the table's working space at this point rather than in the display
    // space, which is why the second matrix exists.
    var corrected = display;
    if (params.develop.y > 0.5) {
        corrected = apply_hue_sat(display);
    }
    // Everything from here to the last line stays in the profile's working
    // space, and the conversion to display happens once at the end.
    //
    // It used to convert here, run the tone curve in the display space, and
    // convert *back* for the look table — which is the shape of the mistake:
    // whoever wrote the round trip knew the look needed the space its axes were
    // measured in, and the curve immediately before it needed the same space
    // for the same reason. Adobe's reference rendering applies the tone curve in
    // ProPhoto, and a per-channel curve is not something you can move between
    // primaries: narrower primaries make a colour's channel ratios more extreme,
    // and a curve that is steep there pushes them further apart still.
    //
    // Measured against Lightroom's own render of `DSC01643`, on the same
    // photosites: in the band where it bites hardest we were 28% over-saturated
    // and 15 degrees off in hue, and neither moved when the curve's *shape* was
    // varied — which is what said the shape was not the problem.
    //
    // Costs nothing when there is no working space to speak of: a profile with
    // no forward matrix leaves both matrices at identity, so this is the same
    // arithmetic in the same order and the goldens do not move.

    // Stage F -- the scene-linear ops, of which haze removal is one, and the
    // only one that is not a multiply. It runs *here* and not with the other
    // local-contrast controls because `I = J*t + A*(1 - t)` is a statement about
    // light: airlight is added to the scene before anything renders it, and
    // subtracting it after a tone curve subtracts a number that is no longer
    // the light. Only the picture is dehazed, never the guide -- the guide is
    // where the veil is *measured*, so dehazing it would be circular.
    var lit = corrected * params.develop.x;
    if (hazy) {
        lit = remove_haze(lit, ixy);
    }

    // Stage G -- local adjustments. Exposure has just been applied and the
    // tone map has not, which is where the declared pipeline puts this and why
    // the controls a mask carries are the ones that are multiplies here.
    let exposed = local_adjust(lit, ixy);

    // The profile's look, applied to scene-linear light and *before* the tone
    // curve, which is where the specification puts it: "it should be applied
    // later in the processing pipe, after any exposure compensation and/or fill
    // light stages, but before any tone curve stage", in the same colour space
    // as the hue/saturation table.
    //
    // It used to run after the curve, from reading "a look is authored against
    // a rendered picture" into the specification rather than out of it. The
    // difference is not academic: a look that boosts saturation lands on
    // highlights the curve has already compressed, and pushes them out of the
    // space instead of letting the curve roll the boosted colour off. Measured
    // over 96 frames against the camera's own JPEG, moving it before the curve
    // took the highlight hue error from 5.10 to 2.34 and the midtone error from
    // 4.72 to 3.53.
    var looked = exposed;
    if (params.develop.w > 0.5) {
        looked = apply_look(exposed);
    }

    // The profile's curve *instead of* ours, not as well as. Both map the scene
    // to a display, and running two tone maps in series maps the scene twice —
    // which reads as a flat, muddy picture rather than as a bug.
    //
    // Whichever curve runs, the colour's largest channel goes through it a
    // second time on its own: that single value is what the ratio-preserving
    // path needs, and taking it from the curve in use rather than from a fixed
    // one is what stops the control meaning two different things depending on
    // whether a `.dcp` happened to be loaded.
    let norm = max(looked.r, max(looked.g, looked.b));
    var mapped: vec3<f32>;
    var peak: f32;
    if (params.curve.z > 0u) {
        mapped = profile_tone_rgb(looked);
        peak = profile_tone(max(norm, 0.0));
    } else {
        mapped = tone_map(looked);
        peak = tone_sigmoid(norm);
    }
    mapped = hue_preserved(looked, mapped, norm, peak);

    // And out to the display's primaries, once, with everything that wanted the
    // working space behind it.
    return vec3<f32>(
        dot(params.working_to_display[0].rgb, mapped),
        dot(params.working_to_display[1].rgb, mapped),
        dot(params.working_to_display[2].rgb, mapped),
    );
}

/// Saturation and vibrance.
///
/// Vibrance is not a weaker saturation: it moves colours **towards the middle of
/// the range**. Positive lifts the flat ones and leaves the vivid alone;
/// negative pulls the vivid back and leaves the flat alone. That is what lets a
/// sky come up without the one red jacket in the frame turning to poster paint.
fn saturate_colour(rgb: vec3<f32>) -> vec3<f32> {
    let saturation = params.colour.x;
    let vibrance = params.colour.y;
    if (saturation == 0.0 && vibrance == 0.0) {
        return rgb;
    }
    // The grey this colour is a departure from. Rec. 709, matching what the
    // values are in by this point, so a fully desaturated frame has the
    // brightness the eye expects rather than the average of three channels.
    let grey = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));

    // How saturated it already is, on the HSV definition: nothing to do with
    // how bright it is, which is what makes it the right weight for vibrance.
    let top = max(rgb.r, max(rgb.g, rgb.b));
    let bottom = min(rgb.r, min(rgb.g, rgb.b));
    let already = select(0.0, (top - bottom) / max(top, 1e-6), top > 1e-6);

    // Which end vibrance is working from: lifting the flat, or calming the
    // vivid. One expression rather than a branch, because both are the same
    // idea seen from opposite sides.
    let weight = select(clamp(already, 0.0, 1.0), 1.0 - clamp(already, 0.0, 1.0), vibrance > 0.0);
    let scale = (1.0 + saturation) * (1.0 + vibrance * weight);
    return vec3<f32>(grey) + (rgb - vec3<f32>(grey)) * max(scale, 0.0);
}

// ---------------------------------------------------------------------------
// The eight-band hue mixer.
//
// A per-band hue shift, saturation and luminance, applied after the global
// saturation and in the same display-referred light — the same reasoning: these
// are decisions about the picture, and a colour that changed when the exposure
// moved would not be a colour control.
//
// # The bands blend, and they blend exactly
//
// A pixel does not belong to a band; it lies *between* two of them. The
// adjustment it receives is the linear blend of the two centres that bracket
// its hue, which makes the weights sum to one everywhere by construction rather
// than by tuning. That matters twice over: hue boundaries cannot band, and
// setting all eight bands to the same value is exactly the global control at
// that value — which is what `the_bands_partition_the_hue_circle` checks, by
// comparing against a control this project already trusts.
//
// Falloff curves are the other way to do this and are the reason so many mixers
// have seams: two Gaussians do not sum to one, so a hue halfway between their
// centres receives less than either neighbour asked for.
//
// # The band comes from the colour that arrived
//
// Not from the colour that leaves. Shifting orange towards red does not hand it
// over to the red slider — the pixel is still the orange you were adjusting, and
// a control that changed which control owned it would be impossible to aim.
// ---------------------------------------------------------------------------

/// Band centres in degrees, matching `Band::centre_deg` in `rawkit-editstate`.
/// The two are checked against each other by `band_centres_match_the_shader`.
fn band_centre(i: i32) -> f32 {
    if (i == 0) { return 0.0; }
    if (i == 1) { return 30.0; }
    if (i == 2) { return 60.0; }
    if (i == 3) { return 120.0; }
    if (i == 4) { return 180.0; }
    if (i == 5) { return 240.0; }
    if (i == 6) { return 280.0; }
    return 320.0;
}

/// The largest hue shift a band can ask for. `MAX_HUE_SHIFT_DEG` in Rust.
const HSL_HUE_RANGE: f32 = 30.0;

fn band_mix(i: i32) -> vec3<f32> {
    let row = i / 4;
    let col = i % 4;
    return vec3<f32>(
        params.hsl_hue[row][col],
        params.hsl_saturation[row][col],
        params.hsl_luminance[row][col],
    );
}

/// The two bands bracketing this hue, and how far between them it lies.
fn band_span(hue: f32) -> vec3<f32> {
    for (var i = 0; i < 8; i = i + 1) {
        let lower = band_centre(i);
        // Red again, a turn later: the last span closes the circle.
        // (`from` and `to` are both reserved words in WGSL.)
        let upper = select(band_centre(i + 1), 360.0, i == 7);
        if (hue >= lower && hue < upper) {
            return vec3<f32>(f32(i), f32((i + 1) % 8), (hue - lower) / (upper - lower));
        }
    }
    // Unreachable for a hue in [0, 360), which is all `rgb_to_hsv` produces.
    return vec3<f32>(0.0, 1.0, 0.0);
}

fn mix_bands(rgb: vec3<f32>) -> vec3<f32> {
    // Set when any band is non-zero, so an untouched photograph does not pay
    // for twenty-four multiplications by one.
    if (params.colour.z < 0.5) {
        return rgb;
    }
    let hsv = rgb_to_hsv(rgb);
    // A grey has no hue to place, and `rgb_to_hsv` reports 0 for it — which
    // would hand every neutral pixel to the red band.
    if (hsv.y <= 0.0) {
        return rgb;
    }

    let span = band_span(hsv.x);
    let adjust = mix(band_mix(i32(span.x)), band_mix(i32(span.y)), span.z);

    var hue = hsv.x + adjust.x * HSL_HUE_RANGE;
    hue = hue - floor(hue / 360.0) * 360.0;
    var out = hsv_to_rgb(vec3<f32>(hue, hsv.y, hsv.z));

    // Distance from grey, on the same Rec. 709 measure `saturate_colour` uses,
    // so a band and the global control compose the way a reader would expect.
    let grey = dot(out, vec3<f32>(0.2126, 0.7152, 0.0722));
    out = vec3<f32>(grey) + (out - vec3<f32>(grey)) * max(1.0 + adjust.y, 0.0);

    // Scaling the triple leaves hue and saturation exactly where they were.
    return out * max(1.0 + adjust.z, 0.0);
}

// ---------------------------------------------------------------------------
// Stage K -- colour grading: a different tint for the shadows, the midtones and
// the highlights.
//
// # The three ranges partition the picture
//
// A pixel does not belong to a range; it lies between two of them, and the
// weights sum to one everywhere by construction rather than by tuning — the
// same argument as the hue mixer's bands, and it buys the same property:
// **setting all three to one colour is a uniform tint**, which is what makes the
// control predictable and is the test that proves it.
//
// Overlapping curves chosen by feel are the other way to do this, and they are
// why a grading control can brighten a picture when you only meant to tint it:
// weights that sum to more than one at some luminance add colour twice there.
//
// # Balance and blending
//
// Balance moves where the midtones sit. Blending is the *steepness* of the two
// transitions, through `w = t^g / (t^g + (1-t)^g)` — an S whose sharpness is `g`
// and which sums to one with its own complement for any `g`, so the partition
// survives the control rather than being restored afterwards.
// ---------------------------------------------------------------------------

/// The transition shape: 0 at `t = 0`, 1 at `t = 1`, and `w(t) + w(1-t) = 1`.
fn grade_ramp(t: f32, steepness: f32) -> f32 {
    let x = clamp(t, 0.0, 1.0);
    let a = pow(x, steepness);
    let b = pow(1.0 - x, steepness);
    let total = a + b;
    // Both ends are zero only if the exponent has underflowed them, in which
    // case the midpoint is as good an answer as any.
    if (total <= 1e-12) {
        return 0.5;
    }
    return a / total;
}

/// One range's colour, at the pixel's own brightness.
fn grade_tint(rgb: vec3<f32>, tint: vec4<f32>, weight: f32) -> vec3<f32> {
    var out = rgb;
    if (tint.y > 0.0) {
        // `target` is a reserved word in WGSL.
        let wanted = hsv_to_rgb(vec3<f32>(tint.x, 1.0, 1.0));
        let here = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
        let there = dot(wanted, vec3<f32>(0.2126, 0.7152, 0.0722));
        // The tint carried to this pixel's brightness, so grading colours the
        // picture rather than lightening the parts it colours.
        let matched = wanted * (here / max(there, 1e-4));
        out = mix(out, matched, clamp(weight * tint.y * GRADE_REACH, 0.0, 1.0));
    }
    // A multiply rather than an offset: it keeps the hue and cannot lift black
    // off the floor, which an addition does and which reads as a veil.
    return out * max(1.0 + weight * tint.z * GRADE_LIFT, 0.0);
}

/// How far a fully saturated tint carries. Short of 1, deliberately: at 1 the
/// control replaces the colour outright rather than grading it, and every
/// setting near the top of the range would look the same.
const GRADE_REACH: f32 = 0.5;
/// How much a range's luminance control may brighten or darken it.
const GRADE_LIFT: f32 = 0.5;

fn grade_colour(rgb: vec3<f32>) -> vec3<f32> {
    if (params.grade_shape.z < 0.5) {
        return rgb;
    }
    // Encoded, not linear, and the difference is the whole control. In linear
    // light a mid-grey sits at 0.18 — barely a third of the way to a midpoint at
    // 0.5 — so most of a photograph would count as *shadow* and a highlight tint
    // would reach almost nothing. "Shadows" has to mean what looks dark, and
    // what looks dark is a perceptual quantity.
    //
    // Found by grading a real photograph: teal in the shadows and orange in the
    // highlights came out uniformly teal, because the highlights the control
    // named were only the last few percent of the scale.
    let linear = clamp(dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722)), 0.0, 1.0);
    let luma = encode_srgb(linear);

    // Where the midtones sit, and how abruptly the ranges give way. Blending at
    // one is a straight ramp and maximal overlap; at zero the transitions are
    // steep and the ranges stay distinct.
    let pivot = clamp(0.5 + params.grade_shape.y * 0.35, 0.05, 0.95);
    let steepness = 1.0 + (1.0 - clamp(params.grade_shape.x, 0.0, 1.0)) * 4.0;

    var shadows = 0.0;
    var midtones = 0.0;
    var highlights = 0.0;
    if (luma < pivot) {
        let w = grade_ramp(luma / pivot, steepness);
        shadows = 1.0 - w;
        midtones = w;
    } else {
        let w = grade_ramp((luma - pivot) / max(1.0 - pivot, 1e-4), steepness);
        midtones = 1.0 - w;
        highlights = w;
    }

    var out = rgb;
    out = grade_tint(out, params.grade[0], shadows);
    out = grade_tint(out, params.grade[1], midtones);
    out = grade_tint(out, params.grade[2], highlights);
    return out;
}

/// Where a colour starts being allowed to bleach towards white, as a fraction
/// of the way up the curve.
///
/// **Swept, not chosen.** Four values against two real frames, measuring the
/// same pixels in each render rather than whatever qualified:
///
/// | from | sun's core, DSC00775 | its clouds | lit midtones at 0 EV |
/// |---|---|---|---|
/// | 0.60 | 0.051 | 0.538 | already bleaching |
/// | 0.70 | 0.088 | 0.538 | already bleaching |
/// | **0.80** | **0.188** | **0.538** | **0.802 against 0.849** |
/// | 0.90 | 0.619 | 0.538 | untouched |
///
/// (Saturation, `(max - min) / max`.) Three things decide it. The sun's core
/// has to reach white, which rules out 0.90 — 0.619 is still an orange disc.
/// Ordinary lit midtones must not bleach, which rules out 0.70 and below: at
/// 0.60 the amber windows in DSC00794 read 11.1 degrees against the untouched
/// 17.2, so the taper has already reached tones that are nowhere near blown.
/// And the clouds are flat at 0.538 throughout, which says the threshold is not
/// delicate — there is a wide gap between a bright cloud and a specular, and
/// 0.80 sits in it.
const HUE_BLEACH_FROM: f32 = 0.80;

/// Where the bleach starts for a colour that has almost none left.
///
/// # The frame that needed it
///
/// `DSC01588.ARW` — a sky shot into the sun, with a channel clipped across 41%
/// of the sensor. Its clouds render around 0.68 on the curve, well under
/// [`HUE_BLEACH_FROM`], so they keep every scrap of their chroma. Against a
/// near-white ground a few per cent of chroma is not a subtle tint: it reads as
/// magenta and olive-green blotches the size of a cloud, and the eye is least
/// forgiving of a cast exactly there, because it knows what colour a cloud is.
///
/// # Why one threshold could not do it
///
/// The sweep that chose 0.80 is pinned from below by ordinary lit midtones,
/// which begin bleaching at 0.70 and are nowhere near white. Brightness alone
/// therefore cannot separate the two cases: a sunset's orange cloud and this
/// sky's grey one sit at similar heights on the curve and want opposite
/// answers.
///
/// **Saturation is what separates them.** A strongly coloured highlight is
/// unambiguous — an orange cloud is orange, and taking its colour away is the
/// artefact. A highlight already within a few per cent of neutral has no colour
/// worth defending, and what it has is as likely to be flare, a clipping
/// residue or the profile's error as it is to be the subject. So the onset
/// slides: near-neutral colours begin bleaching here, saturated ones not until
/// [`HUE_BLEACH_FROM`], and everything between interpolates.
///
/// Measured across the ten reference frames: the two sunsets are bit-identical
/// in 99.99% of channels and their bright saturation does not move at all
/// (0.5592 and 0.4903, unchanged to four figures), while the two clipped skies
/// move 38% and 26% of their channels by at most 25 levels. That is the
/// targeting this is for.
///
/// # And why it cannot draw a ring
///
/// Because it is a smooth function of the pixel's own colour and nothing else.
/// The obvious alternative — check per pixel whether the borrowed highlight
/// colour explains the channels that survived, and go neutral where it does not
/// — was built and measured first. It draws rings: that residual is only
/// *defined* where two channels survive, a thin band around every highlight,
/// and acting on a quantity that exists only in a ring paints a ring. It put
/// white contour lines through the sunset in `DSC00775.ARW`, which is the
/// artefact class [`CLIP_RUNUP`] exists to prevent.
const HUE_BLEACH_NEUTRAL: f32 = 0.45;

/// The saturation at which a colour is defended in full.
///
/// In the display's coordinate rather than the light's — see the measure in
/// `hue_preserved`, which is what makes these numbers comparable with the sweep
/// that chose [`HUE_BLEACH_FROM`]. There the sunset's clouds read 0.538 and its
/// lit midtones 0.849, both past this and so untouched; the sky that needed the
/// fix reads about 0.21.
///
/// Swept at 0.35, 0.40 and 0.45 against the pair of frames that pull in
/// opposite directions. At 0.35 a lilac wash survives in the sky's upper band;
/// 0.45 buys nothing further and moves the sunset's worst pixel by one more
/// level. The gap between 0.21 and 0.538 is wide, which is what makes the exact
/// value uncritical — and is the same kind of gap the brightness sweep found
/// between a bright cloud and a specular.
const HUE_BLEACH_COLOURED: f32 = 0.40;

/// The tone map's shoulder.
///
/// Not a free number: it is fixed by where a photographed mid-grey has to land.
/// `MID_GREY / (MID_GREY + TONE_MAP_K) = 0.18` with `MID_GREY` at 0.072, and
/// `the_tone_map_puts_mid_grey_where_this_module_says` in `scene.rs` is what
/// holds the two halves of that one decision together.
const TONE_MAP_K: f32 = 0.33;

/// Mid-grey in the perceptual coordinate: `0.18^(1/2.2)`.
const TONE_PIVOT: f32 = 0.45865646;
/// The exponent that coordinate uses.
const TONE_GAMMA: f32 = 2.2;
/// How far the shadow and highlight exponents may travel from 1. Bounded by
/// monotonicity at 0.8807 -- see the `tone` module in Rust for the derivation,
/// and the test there that checks these three numbers against this file.
const TONE_TAPER: f32 = 0.75;

/// How far up towards the pivot the shadow control keeps its authority.
///
/// See `rawkit_engine::tone::SHADOW_REACH`, which is the specification and
/// which `the_shader_uses_the_constants_documented_here` checks this against.
/// In short: a plain `1 - v` taper is spent by about level 100 of 255, which on
/// a frame whose darkest tone is level 86 means the slider does nothing at all.
/// The `v * v` keeps the deep shadows where they were while the upper ones gain
/// five to ten levels.
const TONE_SHADOW_REACH: f32 = 1.5;

/// Contrast: a power about the pivot, each side of it separately.
///
/// Both segments carry slope k at the pivot, so this is smooth there and not
/// merely continuous; 0, mid-grey and 1 are all fixed points.
fn tone_contrast(p0: f32) -> f32 {
    let k = params.tone.x;
    if (p0 <= TONE_PIVOT) {
        return TONE_PIVOT * pow(p0 / TONE_PIVOT, k);
    }
    return 1.0 - (1.0 - TONE_PIVOT) * pow((1.0 - p0) / (1.0 - TONE_PIVOT), k);
}

/// Shadows and highlights: powers whose exponent tapers to exactly 1 at the
/// pivot.
///
/// Without the taper each control puts a slope discontinuity in the middle of
/// the frame, and a "highlights" slider visibly moves mid-grey.
fn tone_shadow_highlight(p1: f32) -> f32 {
    if (p1 <= TONE_PIVOT) {
        let v = p1 / TONE_PIVOT;
        let taper = (1.0 - v) * (1.0 + TONE_SHADOW_REACH * v * v);
        return TONE_PIVOT * pow(v, 1.0 - params.tone.z * TONE_TAPER * taper);
    }
    let u = (1.0 - p1) / (1.0 - TONE_PIVOT);
    return 1.0 - (1.0 - TONE_PIVOT) * pow(u, 1.0 + params.tone.y * TONE_TAPER * (1.0 - u));
}

/// Contrast, highlights, shadows, whites and blacks, as one curve.
///
/// Per channel, deliberately. Working on luminance and re-applying the ratio
/// preserves hue exactly, and also makes an S-curve leave saturation flat --
/// which is not what a photographer means by contrast. Per-channel is what an
/// RGB curve does, and what the eye expects from one.
///
/// Every step is monotonic by construction. That is not a nicety: a tone curve
/// that folds back inverts local contrast, and the result reads as a contour in
/// a smooth sky rather than as a bug in this function.
///
/// # `local`, and why highlights and shadows are not keyed on the pixel
///
/// `local` is the brightness of this pixel's *neighbourhood*, in the same
/// perceptual coordinate as `p0`, or negative when there is no guide. Given
/// one, the shadow and highlight exponents are chosen from the neighbourhood
/// and applied to the pixel **as a gain**:
///
///   p2 = p1 * (curve(local) / local)
///
/// Three things follow, and they are the whole point of the arrangement.
///
/// - **A recovered sky stops flattening the face in front of it.** Keyed on the
///   pixel, every value near 0.8 was a highlight wherever it sat. Keyed on the
///   neighbourhood, only the region that *is* bright is treated as bright.
/// - **Local contrast survives exactly.** A gain is a multiply, so two
///   neighbouring pixels keep their ratio however hard the control is pushed.
///   Remapping them each by their own value would compress the difference
///   between them, which is what makes a naive shadow lift look flat.
/// - **It cannot invert.** The gain is one number for the neighbourhood, and a
///   positive multiple of a monotonic function is monotonic.
///
/// Without a guide the arithmetic is the original expression untouched, not an
/// algebraically equal rearrangement of it -- so an edit that does not use this
/// is bit-identical to a build that never had it.
fn tone_curve_rgb(rgb: vec3<f32>, local: f32) -> vec3<f32> {
    // Bit-identical passthrough when nothing is set, so an identity edit is
    // untouched by all of this rather than merely close to untouched.
    if (params.tone.w < 0.5) {
        return rgb;
    }

    let p2 = vec3<f32>(
        tone_shaped(rgb.r, local),
        tone_shaped(rgb.g, local),
        tone_shaped(rgb.b, local),
    );
    return tone_levels(p2);
}

/// Contrast and the two local controls, on one channel.
///
/// Per channel, deliberately, and the reasoning differs for the two halves.
/// Contrast is an RGB curve and an RGB curve is *meant* to add saturation —
/// that is what a photographer means by contrast and what the eye expects. The
/// shadow and highlight gain is per channel only in form: the number it
/// multiplies by is one scalar for all three, so it cannot turn a colour.
fn tone_shaped(y: f32, local: f32) -> f32 {
    // The tone map is asymptotic, so `y` is already inside [0, 1) -- but a
    // non-finite exposure would put it outside, and `1.0 - p` going negative
    // would make every `pow` below a NaN. Clamping is one instruction.
    let p0 = clamp(pow(max(y, 0.0), 1.0 / TONE_GAMMA), 0.0, 1.0);
    let p1 = tone_contrast(p0);

    var p2: f32;
    if (local < 0.0) {
        p2 = tone_shadow_highlight(p1);
    } else {
        // The neighbourhood carries the same contrast the pixel does, or the
        // two would be compared in different coordinates and a contrast move
        // would drag the local operator with it.
        //
        // The neighbourhood alone, not blended with the pixel's own value. That
        // was measured rather than chosen: blending back towards the pixel buys
        // slider authority in the one place it is already weak and costs the
        // texture that is the whole reason for the change. On a real frame,
        // fully local keeps 76% of the detail inside a recovered highlight
        // against a global curve's 39%, and still delivers 75% of the global
        // shadow lift where the shadows genuinely are. Half-way keeps 61%.
        //
        // The cost, stated: a dark pixel inside a *brighter* region is keyed by
        // that region, so it takes the highlight branch and a shadow lift barely
        // moves it. That is the operator working -- lifting scattered dark
        // texture inside a bright region is what makes a global shadow control
        // look washed out -- but it does mean the slider does less on a frame
        // with no dark regions in it.
        //
        // And *bounded*, which is the difference between a control that works
        // and one that stops exactly where it is wanted. The gain is
        // `curve(r)/r`, and this curve pins both its endpoints — so that ratio
        // is not monotone in `r`. Towards white it turns around and climbs back
        // to 1, which left a bright patch of untouched sky sitting in a sky that
        // had been pulled down; towards black it diverges instead, so a glint
        // inside a shadow was multiplied by the darkness around it and clipped
        // to white. Holding the reference at the point where the curve stops
        // becoming more effective fixes both, and the two numbers come from the
        // CPU because finding them is a scan rather than a formula.
        let reference = clamp(
            tone_contrast(local),
            params.tone_local.y,
            params.tone_local.x,
        );
        p2 = p1 * tone_shadow_highlight(reference) / max(reference, EPS);
    }

    return p2;
}

/// The black and white points: the only place in the whole pipeline that clips.
///
/// Deliberate, that: the endpoints are where a photographer *asks* for
/// clipping, and an editor whose black slider only compresses reads as broken.
/// The points can never cross -- see LEVELS_REACH in Rust.
///
/// # Why this takes all three channels at once
///
/// Because clipping them separately is the last place in the pipeline where
/// making a colour brighter **turns** it. Measured on a real frame, pushing
/// Whites to +1 moved a lit facade's hue by **2.2 degrees**, with two thirds of
/// those pixels pinned at 255 in one channel; it is now 0.0.
///
/// Both halves of the step contributed. The affine stretch is shared across the
/// channels but is not a scaling — subtracting a black point moves the ratios —
/// and that alone was worth 1.5 degrees; the clamp added another degree on top.
///
/// # What this does *not* fix, stated because it was first claimed as a fault
///
/// The colourfulness. It was measured as rising from 0.683 to 0.979 and read as
/// the control saturating a colour rather than bleaching it — which would have
/// been the larger fault of the two. **That was the measure, not the renderer.**
/// HLS saturation is not invariant under scaling: its denominator carries the
/// lightness, so it climbs whenever a colour is made brighter at constant
/// ratios. Against `(max - min) / max`, which is invariant, the same push moves
/// colourfulness 0.759 to 0.724 — a slight bleach — and moves it *identically*
/// with this function and without it.
///
/// So the endpoint was never over-saturating. It was turning the hue, and only
/// that.
///
/// So a colour that would clip is brought inside the ceiling by *scaling*,
/// which leaves its ratios alone, and then bleached towards white by however
/// much clipping there would have been. Same control as `hue_preserved`, same
/// two-mix arrangement, same reason.
///
/// **Only the top end.** A colour crushed against the black point still clamps
/// per channel and still turns as it goes. That half was measured too and is
/// the smaller one — 1.5 degrees against the top's 2.2 — and crushing ends at
/// black, which is where every hue meets anyway.
///
/// At a weight of exactly zero this is the per-channel clamp it replaced,
/// arithmetic for arithmetic.
fn tone_levels(p2: vec3<f32>) -> vec3<f32> {
    let span = params.levels.y - params.levels.x;
    // What each channel would be with no ceiling at all. Per channel this is
    // then clamped, which is the behaviour being replaced.
    let u = (p2 - vec3<f32>(params.levels.x)) / span;
    let per_channel = clamp(u, vec3<f32>(0.0), vec3<f32>(1.0));

    let keep = params.tone_map.x;
    let hi = max(u.r, max(u.g, u.b));
    let lo = min(u.r, min(u.g, u.b));
    // Nothing over the ceiling is nothing to do: below it the clamp is the
    // identity and there is no clipping to preserve the hue through.
    if (keep <= 0.0 || hi <= 1.0) {
        return pow(per_channel, vec3<f32>(TONE_GAMMA));
    }

    // Brought inside the ceiling by scaling, which is the one operation that
    // leaves the ratios — and therefore the hue — exactly alone.
    let fitted = u / hi;

    // **How far to bleach, and this is the number the first attempt got wrong.**
    //
    // Written as "white once the largest channel passes the ceiling", a colour
    // lost all of its chroma the instant it began to clip. Per-channel does not
    // do that: it whitens one channel at a time and only arrives at white when
    // the *smallest* channel reaches the ceiling too. The difference is the
    // ratio between the largest and smallest, which on a saturated colour is
    // several stops — and the golden reference built from a saturated chirp at
    // full contrast came out 98% white, which is not an exaggerated result, it
    // is a blank one.
    //
    // So the bleach is tied to how much clipping there would have been.
    // `hi` at 1 is the first channel touching the ceiling and nothing is
    // bleached; `hi / lo` is where the last one reaches it, and there the
    // colour is white. The two paths therefore arrive at white together, and
    // differ only in the route: this one goes straight there, and the
    // per-channel one walks round the hue circle on its way.
    let full = max(hi / max(lo, EPS), 1.0);
    let t = clamp((hi - 1.0) / max(full - 1.0, EPS), 0.0, 1.0);
    let bleached = mix(fitted, vec3<f32>(1.0), t);

    let levelled = clamp(mix(per_channel, bleached, keep), vec3<f32>(0.0), vec3<f32>(1.0));
    return pow(levelled, vec3<f32>(TONE_GAMMA));
}

/// One texel of one of the guide's two fields, in the camera's own RGB.
fn guide_texel(base: u32, x: i32, y: i32) -> vec3<f32> {
    let i = base + u32(y * i32(params.guide.y) + x) * 3u;
    return vec3<f32>(cfa[i], cfa[i + 1u], cfa[i + 2u]);
}

/// Stage G -- the local adjustments, composited.
///
/// Each mask is a layer, sampled bilinearly at this pixel's place in the image,
/// and each contributes a multiply raised to its own weight. Raised, not mixed:
/// half a mask should be half the *stops*, and half the warmth, which is what a
/// power gives and a linear blend of a multiply does not. It also makes a weight
/// of exactly zero exactly identity, so a photograph with a mask that does not
/// reach it is bit-identical to one with no mask at all.
///
/// Nothing here knows what shape a mask is. That is the point -- see
/// `rawkit_engine::mask`.
fn local_adjust(rgb: vec3<f32>, ixy: vec2<f32>) -> vec3<f32> {
    let count = params.masks.x;
    if (count == 0u) {
        return rgb;
    }
    let uv = ixy * params.mask_scale.xy;
    var out = rgb;
    for (var i = 0u; i < count; i = i + 1u) {
        // No implicit derivatives in a compute shader, so the mip level is
        // named rather than inferred. There is only one level anyway.
        let weight = textureSampleLevel(mask_layers, mask_sampler, uv, i, 0.0).r;
        if (weight > 0.0) {
            let gain = max(params.mask_gain[i].rgb, vec3<f32>(EPS));
            out = out * pow(gain, vec3<f32>(weight));
        }
    }
    return out;
}


/// The half of a local adjustment that is about the picture rather than the
/// light.
///
/// Read from the same mask texture as [`local_adjust`] and applied after the
/// tone map, because that is where these operations mean anything. Contrast in
/// scene-linear would depend on the exposure; saturation would depend on it
/// twice over. The mask is sampled a second time rather than carried in a
/// register, which costs a fetch and keeps the two stages independent of each
/// other's order.
fn local_look(rgb: vec3<f32>, ixy: vec2<f32>, neighbourhood: f32) -> vec3<f32> {
    let count = params.masks.x;
    if (count == 0u) {
        return rgb;
    }
    let uv = ixy * params.mask_scale.xy;
    var out = rgb;
    for (var i = 0u; i < count; i = i + 1u) {
        let look = params.mask_look[i];
        if (look.x == 0.0 && look.y == 0.0 && look.z == 0.0) {
            continue;
        }
        let weight = textureSampleLevel(mask_layers, mask_sampler, uv, i, 0.0).r;
        if (weight <= 0.0) {
            continue;
        }
        var v = out;

        // Contrast about middle grey, on the same terms as the global control.
        if (look.x != 0.0) {
            v = against_grey(v, look.x * weight);
        }

        // Saturation as distance from this pixel's own grey, in Rec. 709 --
        // matching what the values are in by this point, and the same rule the
        // global control uses so the two agree where they overlap.
        if (look.y != 0.0) {
            let grey = dot(v, vec3<f32>(0.2126, 0.7152, 0.0722));
            v = mix(vec3<f32>(grey), v, 1.0 + look.y * weight);
        }

        // Clarity is the operation above with a *moving* pivot: contrast against
        // the neighbourhood rather than against a fixed grey, which is what
        // makes it read as texture rather than as contrast: a pixel brighter
        // than what surrounds it gets brighter still, and the size of "what
        // surrounds it" is the guide's own scale. Nothing new is computed for it
        // -- the local-tone guide already answers this question, and asking it
        // twice at two radii is how a clarity control turns into a halo.
        v = against_neighbourhood(v, neighbourhood, look.z * weight);

        out = mix(out, v, 1.0);
    }
    return out;
}

/// How much of the veil is taken to be visible where the prior says it is.
///
/// One is the whole of it and reads as unnaturally airless -- distance is *made*
/// of a little haze, and a photograph with none looks like a cut-out. The
/// standard value in the literature, and it is a taste constant rather than a
/// measured one.
const HAZE_OMEGA: f32 = 0.95;
/// How far the transmission may be pushed. Below this, dividing by it amplifies
/// whatever noise was in the darkest part of the frame into colour blotches.
const HAZE_FLOOR: f32 = 0.15;
const HAZE_CEILING: f32 = 3.0;

/// Undo the airlight, in the light it was added to.
///
/// The dark-channel prior: in a clear patch at least one channel goes nearly
/// black, so wherever none does, something white is being added. `min` over the
/// white-balanced neighbourhood is that reading, and the level it is measured
/// against comes from the whole frame -- see `Guide::veil` for how, and for what
/// this estimate gives up by taking the guide's blur instead of a true local
/// minimum.
///
/// A negative amount puts haze back: the transmission goes above one, and
/// `(I - A(1 - t))/t` becomes a blend towards the airlight, which is the same
/// equation read the other way.
fn remove_haze(rgb: vec3<f32>, ixy: vec2<f32>) -> vec3<f32> {
    let amount = params.local_contrast.z;
    let level = params.local_contrast.w;
    if (amount == 0.0 || level <= 0.0 || params.guide.w == 0u) {
        return rgb;
    }
    let around = guide_sample(params.guide.x, ixy) * params.wb.rgb;
    let dark = min(around.r, min(around.g, around.b));
    let t = clamp(
        1.0 - HAZE_OMEGA * amount * clamp(dark / level, 0.0, 1.0),
        HAZE_FLOOR,
        HAZE_CEILING,
    );
    // Clamped at zero, because that is what the answer means: the recovered
    // value is how much light the scene sent, and a scene cannot send less than
    // none. Without it, a shadow darker than the airlight's own contribution
    // comes out negative and the tone map -- `x / (x + k)` -- turns it into
    // a value larger than one, which reads as bright speckle in the shadows.
    return max((rgb - params.airlight.rgb * (1.0 - t)) / t, vec3<f32>(0.0));
}

/// Contrast about middle grey, in the perceptual coordinate.
///
/// `TONE_PIVOT` is where scene-linear 0.18 lands once the tone map has run, and
/// it is expressed in the *encoded* coordinate the tone controls work in. The
/// picture here is linear, so it is encoded first -- without that the pivot sits
/// at a linear 0.46, which is a good deal brighter than middle grey, and a local
/// contrast slider disagrees with the global one about where the middle is.
///
/// Below the pivot it darkens and above it brightens, which is what the word
/// means; a power about *zero* would only have made everything brighter, which
/// is a gamma control wearing the wrong label.
fn against_grey(rgb: vec3<f32>, amount: f32) -> vec3<f32> {
    if (amount == 0.0) {
        return rgb;
    }
    let coded = pow(max(rgb, vec3<f32>(0.0)), vec3<f32>(1.0 / TONE_GAMMA));
    let ratio = max(coded / TONE_PIVOT, vec3<f32>(EPS));
    let shaped = TONE_PIVOT * pow(ratio, vec3<f32>(1.0 + amount));
    return pow(max(shaped, vec3<f32>(0.0)), vec3<f32>(TONE_GAMMA));
}

/// Contrast about a moving pivot, in the perceptual coordinate.
///
/// The neighbourhood, put through the curve the pixel has already been through.
///
/// # The bug this is the fix for
///
/// `against_neighbourhood` finds the neutral point of clarity by asking whether
/// a pixel equals what surrounds it. That comparison is only meaningful if the
/// two are in the same coordinate, and by the time clarity runs the pixel has
/// been through the tone curve while the guide's neighbourhood has not.
///
/// It went unnoticed because until `BASE_CONTRAST` existed the curve was
/// *skipped entirely* at defaults, so on a photograph nobody had touched the
/// two were trivially in the same coordinate and clarity's neutral point was
/// exactly right. Turn the curve on for every frame and the neutral point moves
/// by however much the curve moves that brightness: measured on a flat frame,
/// clarity at +1 took it from 0.525 to 0.579 — a control whose whole definition
/// is "does nothing where there is nothing around you to differ from", changing
/// a frame with nothing in it.
///
/// Note that it was *already* wrong for anybody with a contrast, highlights or
/// shadows slider off zero. The baseline did not introduce it; it made it
/// unconditional, which is the only reason a test caught it.
///
/// # Why the global branch
///
/// `tone_shaped` is asked for the neighbourhood's own value with `-1.0`, which
/// selects the branch that keys on the pixel rather than on a neighbourhood.
/// For the neighbourhood *itself* the two agree by construction — the local
/// branch's gain is `curve(reference)/reference` with the reference being this
/// same value — so the global branch is the same answer without the detour.
///
/// Returned in the perceptual coordinate, because that is what
/// `against_neighbourhood` compares against and what `local` already was.
fn neighbourhood_after_curve(local: f32) -> f32 {
    // The sentinel for "no local control is on", which has to survive: it is
    // what tells `against_neighbourhood` there is nothing to do.
    if (local < 0.0 || params.tone.w < 0.5) {
        return local;
    }
    let p2 = tone_shaped(pow(max(local, 0.0), TONE_GAMMA), -1.0);
    let span = params.levels.y - params.levels.x;
    return clamp((p2 - params.levels.x) / span, 0.0, 1.0);
}

/// The shared definition of clarity, global and local. Two things it settles.
///
/// **The coordinate.** The neighbourhood arrives gamma-encoded, because that is
/// what the tone controls work in and what `TONE_PIVOT` is expressed in; the
/// picture at this point is linear. Comparing the two directly would put the
/// neutral point -- where a pixel equals what surrounds it and clarity must do
/// nothing -- at the wrong brightness, and the control would darken a flat frame
/// instead of leaving it alone.
///
/// **The direction.** Above the neighbourhood it brightens and below it darkens,
/// which is what "local contrast" means.
fn against_neighbourhood(rgb: vec3<f32>, neighbourhood: f32, amount: f32) -> vec3<f32> {
    if (amount == 0.0 || neighbourhood < 0.0) {
        return rgb;
    }
    let around = max(neighbourhood, EPS);
    let coded = pow(max(rgb, vec3<f32>(0.0)), vec3<f32>(1.0 / TONE_GAMMA));
    let ratio = max(coded / around, vec3<f32>(EPS));
    let shaped = around * pow(ratio, vec3<f32>(1.0 + amount));
    return pow(max(shaped, vec3<f32>(0.0)), vec3<f32>(TONE_GAMMA));
}

/// How much to scale a pixel by, to undo the lens's corner falloff.
///
/// A cosine-fourth-ish curve, which is what an unremarkable lens does: the
/// falloff goes as the fourth power of the cosine of the angle off axis, and
/// `1 - k*r^4` is that to the accuracy anybody can see, with one number for how
/// deep it goes. Measured against the sensor's own diagonal, so it does not
/// change when the picture is cropped.
fn lens_falloff(ixy: vec2<f32>) -> f32 {
    let amount = params.grain.z;
    // The image's own size, from the reciprocal the masks are indexed by rather
    // than from a second uniform saying the same thing.
    let half = 0.5 / params.mask_scale.xy;
    let corner = length(half);
    if (corner <= 0.0) {
        return 1.0;
    }
    let r = length(ixy - half) / corner;
    let r4 = r * r * r * r;
    // Positive lifts the corners and leaves the centre alone; negative dims
    // them. Clamped away from zero because dividing a black corner by nothing
    // is not a correction.
    return max(1.0 + amount * r4, 0.05);
}

/// How far this pixel is towards a corner of the *crop*, from 0 to 1.
///
/// A superellipse, whose exponent is the roundness: two is an ellipse in the
/// crop's own proportions, larger is a rectangle, smaller a diamond. Normalised
/// by the shape's own corner so that the midpoint slider means the same thing
/// whatever the roundness is — otherwise changing the shape would move the
/// vignette as well.
fn vignette_radius(ixy: vec2<f32>) -> f32 {
    let centre = params.vignette_frame.xy;
    let half = max(params.vignette_frame.zw, vec2<f32>(1.0));
    let n = exp2(1.0 + params.effects.z * 2.0);
    let d = abs(ixy - centre) / half;
    let raw = pow(pow(d.x, n) + pow(d.y, n), 1.0 / n);
    return raw / pow(2.0, 1.0 / n);
}

/// How much of the vignette applies here, from 0 at the middle to 1 outside.
fn vignette_weight(ixy: vec2<f32>) -> f32 {
    let midpoint = params.effects.y;
    // A width rather than an edge, so feather and midpoint are two independent
    // numbers: the band is centred on the midpoint and the feather is how wide
    // it is. Never exactly zero, or the smoothstep divides by nothing and a hard
    // edge becomes a NaN rather than a hard edge.
    let width = max(params.effects.w, 0.004);
    let r = vignette_radius(ixy);
    return smoothstep(midpoint - width * 0.5, midpoint + width * 0.5, r);
}

/// How much a bright pixel is spared, as an exponent on its own luminance.
///
/// Two, measured off nothing but the eye: it leaves a corner of sky within a few
/// percent of where it was while a corner of rock takes the whole darkening.
/// This is what makes a vignette read as a lens rather than as a grey wash --
/// the answer to "what does this do to a bright sky" is "almost nothing".
const VIGNETTE_HOLD: f32 = 2.0;

/// Value noise: a hash per cell, smoothly interpolated.
///
/// Interpolated rather than sampled per cell because a per-cell hash is
/// *square*, and film grain is not. The hash is the usual cheap sine-free one;
/// what matters here is that it is a pure function of the cell, so the same
/// photograph grains the same way every time it is rendered.
fn hash21(p: vec2<f32>) -> f32 {
    var q = fract(p * vec2<f32>(0.1031, 0.1030));
    q = q + dot(q, q.yx + 33.33);
    return fract((q.x + q.y) * q.x);
}

fn value_noise(p: vec2<f32>) -> f32 {
    let cell = floor(p);
    let f = fract(p);
    let w = f * f * (3.0 - 2.0 * f);
    let a = hash21(cell);
    let b = hash21(cell + vec2<f32>(1.0, 0.0));
    let c = hash21(cell + vec2<f32>(0.0, 1.0));
    let d = hash21(cell + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, w.x), mix(c, d, w.x), w.y);
}

/// The vignette and the grain, in that order, at the very end.
///
/// Both are decisions about the picture rather than about the light, so they are
/// after the look and after everything that touches colour.
fn apply_effects(rgb: vec3<f32>, ixy: vec2<f32>) -> vec3<f32> {
    var out = rgb;
    let amount = params.effects.x;
    if (amount != 0.0) {
        let weight = vignette_weight(ixy);
        // Off the luminance and applied to all three channels, so a corner keeps
        // its hue -- the same rule the sharpening and the grain follow.
        let luma = clamp(dot(out, vec3<f32>(0.2126, 0.7152, 0.0722)), 0.0, 1.0);
        // Negative darkens. `1 + amount * weight` is the plain answer; mixing it
        // towards 1 by the pixel's own brightness is what holds the highlights
        // back, and at luminance 1 it does nothing at all.
        let plain = 1.0 + amount * weight;
        let gain = mix(plain, 1.0, pow(luma, VIGNETTE_HOLD));
        out = out * max(gain, 0.0);
    }

    let grain = params.grain.x;
    if (grain > 0.0) {
        // Sized in *image* pixels, so an export and a preview carry the same
        // film. A coarse level covers several image pixels per output pixel, so
        // the cell is never allowed below one output pixel -- grain finer than
        // that is not visible, it is aliasing, and it would flicker as the view
        // zoomed.
        let step = max(f32(params.source.z), 1.0);
        let size = max(params.grain.y, step);
        let n = value_noise(ixy / size) - 0.5;
        // Loudest in the midtones, which is where film's own grain lives: black
        // has no silver to clump and white has all of it. Also keeps the control
        // from speckling a clipped sky.
        let luma = clamp(dot(out, vec3<f32>(0.2126, 0.7152, 0.0722)), 0.0, 1.0);
        let where_it_shows = 4.0 * luma * (1.0 - luma);
        out = out + vec3<f32>(n * grain * GRAIN_REACH * where_it_shows);
    }
    return out;
}

/// How far a full-strength grain moves a midtone.
///
/// Display-referred, so this is a fraction of the whole range. A tenth is
/// plainly visible at 1:1 and stops well short of looking broken.
const GRAIN_REACH: f32 = 0.1;

/// Where a tile pixel sits in the full-resolution image.
///
/// Everything that asks a question about *place* -- the local-tone guide, the
/// chroma reference, the masks -- is indexed through this rather than through
/// tile coordinates. That is what makes the answers independent of which tile is
/// being drawn and of the resolution level it is drawn at.
fn image_xy(x: i32, y: i32) -> vec2<f32> {
    return vec2<f32>(
        f32((x - params.source.w) * params.source.z + params.source.x),
        f32((y - params.source.w) * params.source.z + params.source.y),
    );
}

/// A guide field, sampled bilinearly.
///
/// The camera's RGB is interpolated and *then* developed, rather than the other
/// way round: developing four texels and blending the results would cost four
/// tone maps a pixel, and blending a non-linear result is not obviously the
/// value anyone wants anyway.
fn guide_sample(base: u32, ixy: vec2<f32>) -> vec3<f32> {
    let uv = ixy * params.guide_scale.xy;
    let gw = i32(params.guide.y);
    let gh = i32(params.guide.z);
    // Texel centres sit at half-integers, the same convention the resampler
    // uses; without the half the guide slides by half a texel, which no test
    // would notice and every gradient would.
    let fx = clamp(uv.x - 0.5, 0.0, f32(gw - 1));
    let fy = clamp(uv.y - 0.5, 0.0, f32(gh - 1));
    let x0 = i32(floor(fx));
    let y0 = i32(floor(fy));
    let x1 = min(x0 + 1, gw - 1);
    let y1 = min(y0 + 1, gh - 1);
    let tx = fx - f32(x0);
    let ty = fy - f32(y0);
    let top = mix(guide_texel(base, x0, y0), guide_texel(base, x1, y0), tx);
    let bottom = mix(guide_texel(base, x0, y1), guide_texel(base, x1, y1), tx);
    return mix(top, bottom, ty);
}

/// The colour of the light that did not clip near this pixel, in camera RGB.
///
/// Neutral when the frame had none to offer, which is what makes reconstruction
/// fall back to the grey it used to produce unconditionally.
///
/// **Neutral here is `1 / wb`, not `1`.** This field is in the camera's own RGB,
/// where a grey object does not have equal channels -- undoing that is what the
/// white balance multipliers are *for*. Returning ones would hand
/// reconstruction the multipliers themselves as a colour, and a frame with
/// nothing unclipped in it would come back with a cast instead of the grey it
/// used to get.
fn unclipped_colour() -> vec3<f32> {
    if (params.guide_chroma.w < 0.5) {
        return 1.0 / max(params.wb.rgb, vec3<f32>(EPS));
    }
    return params.guide_chroma.rgb;
}

/// How bright this pixel's neighbourhood is, in the tone curve's coordinate.
///
/// Negative when neither local control is off zero, which is what `tone_curve`
/// reads as "use the pixel's own value" -- so the whole arrangement costs
/// nothing on a photograph that is not using it.
/// How bright this pixel's *neighbourhood* is, in the perceptual coordinate.
///
/// # It is measured on the current rendering, exposure and all
///
/// The guide goes through `develop_rgb`, so what comes back carries the white
/// balance, the profile, **the exposure**, the local adjustments, the look and
/// the tone map — and `tone_curve` then puts the contrast on top of it before
/// using it. Every one of those moves which part of the picture the highlight
/// and shadow controls act on.
///
/// Measured, on a sunset frame at Highlights -1: at 0 EV the scene's 64-128
/// band is untouched, 0.0%, and the 192-224 band is pulled down 13.9%. At +2 EV
/// the same 64-128 band is pulled down 5.4% and the top band 20.9%. **The
/// affected region moves by about the exposure change, and the control also
/// gets stronger.**
///
/// That is deliberate and it is what the industry does. These are display-
/// referred controls: they shape what the eye will see, and after two stops of
/// exposure the things that *look* like highlights are a different set of
/// pixels. Adobe's own guidance for the same controls — set Exposure and
/// Contrast first, then Highlights and Shadows — is only necessary advice
/// because the later controls depend on the earlier ones.
///
/// # And why this is the opposite of `SceneStats`, which is not a contradiction
///
/// [`crate::scene::SceneStats`] excludes exposure on purpose, with the
/// reasoning that a statistic which moved when the exposure slider moved would
/// make everything derived from it chase its own tail. Both are right, because
/// they answer different questions:
///
/// - `SceneStats` describes **the photograph** — a property of the file, which
///   must not move, or the thing it anchors moves with it.
/// - this describes **the rendering** — what the picture looks like now, which
///   must move, or the control acts on something nobody can see.
///
/// From the outside they look like one quantity, "how bright is this region",
/// which is how somebody eventually changes one to match the other. They are
/// two quantities.
fn local_tone(ixy: vec2<f32>) -> f32 {
    if (params.guide.w == 0u) {
        return -1.0;
    }
    // The guide's own blown pixels are reconstructed against the same
    // neighbourhood the picture's are, so the two agree about how bright a
    // highlight ended up.
    let rgb = develop_rgb(guide_sample(params.guide.x, ixy), ixy, false);
    // Rec. 709, which is what the developed values are in by this point. One
    // number for all three channels, so the control moves a colour's brightness
    // and never its hue.
    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    return clamp(pow(max(luma, 0.0), 1.0 / TONE_GAMMA), 0.0, 1.0);
}

/// RGB to hue/saturation/value, with hue in degrees.
///
/// The table is indexed in HSV because that is the space a colour correction is
/// naturally expressed in: "rotate this hue a little, pull this saturation
/// down". Negative components are possible here — a wide working space holds
/// colours outside the sensor's gamut — and are handled by the clamping below
/// rather than by pretending they cannot happen.
fn rgb_to_hsv(rgb: vec3<f32>) -> vec3<f32> {
    let maximum = max(rgb.r, max(rgb.g, rgb.b));
    let minimum = min(rgb.r, min(rgb.g, rgb.b));
    let span = maximum - minimum;

    var hue = 0.0;
    if (span > 0.0) {
        if (maximum == rgb.r) {
            hue = (rgb.g - rgb.b) / span;
            if (hue < 0.0) { hue = hue + 6.0; }
        } else if (maximum == rgb.g) {
            hue = 2.0 + (rgb.b - rgb.r) / span;
        } else {
            hue = 4.0 + (rgb.r - rgb.g) / span;
        }
        hue = hue * 60.0;
    }
    var saturation = 0.0;
    if (maximum > 0.0) {
        saturation = span / maximum;
    }
    return vec3<f32>(hue, saturation, maximum);
}

fn hsv_to_rgb(hsv: vec3<f32>) -> vec3<f32> {
    let sector = hsv.x / 60.0;
    let i = floor(sector);
    let f = sector - i;
    let v = hsv.z;
    let p = v * (1.0 - hsv.y);
    let q = v * (1.0 - hsv.y * f);
    let t = v * (1.0 - hsv.y * (1.0 - f));
    let which = i32(i) % 6;
    if (which == 0) { return vec3<f32>(v, t, p); }
    if (which == 1) { return vec3<f32>(q, v, p); }
    if (which == 2) { return vec3<f32>(p, v, t); }
    if (which == 3) { return vec3<f32>(p, q, v); }
    if (which == 4) { return vec3<f32>(t, p, v); }
    return vec3<f32>(v, p, q);
}

fn table_at(base: u32, dims: vec4<u32>, h: u32, s: u32, v: u32) -> vec4<f32> {
    // DNG ordering: value outermost, then hue, then saturation innermost.
    let index = (v * dims.x + h) * dims.y + s;
    return hue_sat_map[base + index];
}

fn hsm_at(h: u32, s: u32, v: u32) -> vec4<f32> {
    // DNG ordering: value outermost, then hue, then saturation innermost.
    let index = (v * params.hsm_dims.x + h) * params.hsm_dims.y + s;
    return hue_sat_map[index];
}

/// Look the colour up in the profile's table and apply the delta it finds.
///
/// Trilinear, with **hue wrapping** and saturation and value clamping. The
/// wrap is not a detail: hue is circular, so a table sampled without it would
/// produce a visible seam at 0 degrees — which lands squarely on reds.
fn apply_hue_sat(rgb: vec3<f32>) -> vec3<f32> {
    return apply_table(0u, params.hsm_dims, rgb, params.develop.y >= 1.5);
}

/// The profile's look, which is the same table format applied somewhere else.
///
/// Somewhere else matters. A look is authored against a *rendered* picture, so
/// its value axis is indexed by tone-mapped light — and with sixteen value
/// divisions, indexing it with scene-linear light reads a different slice for
/// almost every pixel. `params.develop.w` carries which encoding the table's
/// axes are in, because the specification lets a profile choose and Adobe's
/// camera-matching profiles choose sRGB.
fn apply_look(rgb: vec3<f32>) -> vec3<f32> {
    return apply_table(params.look_dims.w, params.look_dims, rgb, params.develop.w >= 1.5);
}

/// The sRGB transfer function, for a look table whose axes are encoded.
///
/// Clamped below zero rather than mirrored: a negative value here is out of the
/// space the table describes, and the table has nothing to say about it.
fn encode_srgb(v: f32) -> f32 {
    let x = max(v, 0.0);
    if (x <= 0.0031308) {
        return 12.92 * x;
    }
    return 1.055 * pow(x, 1.0 / 2.4) - 0.055;
}

fn decode_srgb(v: f32) -> f32 {
    let x = max(v, 0.0);
    if (x <= 0.04045) {
        return x / 12.92;
    }
    return pow((x + 0.055) / 1.055, 2.4);
}

fn apply_table(base: u32, dims: vec4<u32>, rgb: vec3<f32>, encode_value: bool) -> vec3<f32> {
    let hsv = rgb_to_hsv(rgb);

    // **Only the value coordinate is encoded**, and the specification is
    // explicit about it: convert to HSV from linear, encode V, index and scale
    // with V encoded, decode V, convert back. Encoding red, green and blue
    // separately instead -- which is the obvious reading, and what this did --
    // is a different operation: the transfer curve is not linear, so bending
    // each channel on its own moves the *differences* between them, which is
    // where hue and saturation live. The table then gets asked about a colour
    // that is not the one in hand.
    var value = hsv.z;
    if (encode_value) {
        value = encode_srgb(value);
    }

    // Hue spans the full circle across `hue_divisions` cells and wraps, so the
    // spacing is 360/divisions rather than 360/(divisions-1).
    let hue_step = 360.0 / f32(dims.x);
    let hue_pos = hsv.x / hue_step;
    let h0 = u32(floor(hue_pos)) % dims.x;
    let h1 = (h0 + 1u) % dims.x;
    let hf = fract(hue_pos);

    // Saturation and value are endpoints-inclusive: cell 0 is 0.0 and the last
    // cell is 1.0.
    let sat_pos = clamp(hsv.y, 0.0, 1.0) * f32(dims.y - 1u);
    let s0 = min(u32(floor(sat_pos)), dims.y - 1u);
    let s1 = min(s0 + 1u, dims.y - 1u);
    let sf = fract(sat_pos);

    var v0 = 0u;
    var v1 = 0u;
    var vf = 0.0;
    if (dims.z > 1u) {
        let val_pos = clamp(value, 0.0, 1.0) * f32(dims.z - 1u);
        v0 = min(u32(floor(val_pos)), dims.z - 1u);
        v1 = min(v0 + 1u, dims.z - 1u);
        vf = fract(val_pos);
    }

    let c000 = table_at(base, dims, h0, s0, v0);
    let c100 = table_at(base, dims, h1, s0, v0);
    let c010 = table_at(base, dims, h0, s1, v0);
    let c110 = table_at(base, dims, h1, s1, v0);
    let c001 = table_at(base, dims, h0, s0, v1);
    let c101 = table_at(base, dims, h1, s0, v1);
    let c011 = table_at(base, dims, h0, s1, v1);
    let c111 = table_at(base, dims, h1, s1, v1);

    let d00 = mix(c000, c100, hf);
    let d10 = mix(c010, c110, hf);
    let d01 = mix(c001, c101, hf);
    let d11 = mix(c011, c111, hf);
    let d0 = mix(d00, d10, sf);
    let d1 = mix(d01, d11, sf);
    let delta = mix(d0, d1, vf);

    var hue = hsv.x + delta.x;
    // Wrap rather than clamp, for the same reason the lookup wraps.
    hue = hue - 360.0 * floor(hue / 360.0);
    let saturation = clamp(hsv.y * delta.y, 0.0, 1.0);
    // The scale lands on the encoded value, which is then decoded -- steps 4
    // and 5 of the specification's sRGB method.
    var scaled = max(value * delta.z, 0.0);
    if (encode_value) {
        scaled = decode_srgb(scaled);
    }

    return hsv_to_rgb(vec3<f32>(hue, saturation, scaled));
}

/// How far below the clip point reconstruction starts to take effect.
///
/// Without a run-up, reconstruction switches on at a hard boundary and leaves a
/// visible edge around every highlight — which is a worse artefact than the one
/// being fixed, and harder to explain.
///
/// Widened from 0.02 when the coloured arcs came off `DSC01588.ARW` and left a
/// hard white contour behind them: two per cent of a channel's range is a very
/// narrow band of scene brightness, and across a gentle sky gradient it lands as
/// a line. At 0.25 the roll is smooth and the blown region blends into the sky.
///
/// The width is free where nothing is blown, which is what makes it safe to
/// spend: rendered against the 0.02 version, three unclipped frames came back at
/// a mean absolute difference of **0.000/255** with under 0.005% of channels
/// moving by even one step — the only pixels that move at all are the ones
/// already within a quarter-stop of saturating.
const CLIP_RUNUP: f32 = 0.25;

/// Roll a blown highlight toward white instead of toward a colour.
///
/// # Why blown highlights take a colour without this
///
/// The sensor saturates in its own units, and white balance then scales the
/// channels apart. Green is the reference the other two are divided by, so
/// green's ceiling in balanced space is the *lowest* of the three and green
/// clips first — whatever colour the subject is. Its true value might be 1.5 but
/// it records 1.0. Red is nowhere near its own limit, records correctly, and is
/// then multiplied up. Left alone, the result is a pixel where red and blue
/// exceed green — magenta — in the part of the picture the eye most expects to
/// be white.
///
/// # Why the whole pixel moves, and not just the channel that clipped
///
/// The obvious repair is to raise the missing channel to the brightest one still
/// believed. That is right for a neutral subject, where the survivors agree and
/// their common level is the answer. It is wrong for every other subject,
/// because when the survivors *disagree* the brightest one is a choice, and the
/// pixel comes out wearing that channel's colour.
///
/// `DSC01588.ARW` is what proved it: a blown sky behind a tree came out with a
/// mint-green arc across it, measured at green 0.6086 sitting exactly on blue
/// 0.6086 with red left behind at 0.5015 — green raised to meet blue, and a
/// highlight *more* saturated than the sky it interrupted. The camera's own JPEG
/// has no such arc. `a_blue_subject_does_not_turn_cyan_when_green_clips` is that
/// pixel.
///
/// The reasoning that led there was that the surviving channels are real
/// measured data and should be kept. They are — but a *colour* is the ratio
/// between three channels, and once one of them is missing the ratio is not
/// known. Keeping two exactly and inventing the third does not preserve the
/// colour; it invents a different one. So the honest local answer is that a
/// blown pixel has no colour we can name, and it is taken to neutral at its own
/// brightest channel, in proportion to how far into clipping it is.
///
/// One consequence worth stating: this begins whitening as soon as the *first*
/// channel goes, rather than waiting for the last. That is deliberate — the
/// first channel going is the moment the colour stops being known — and it is
/// what removes the ring artefacts, which were the three channels each clipping
/// at their own brightness with a separate contour for every one.
///
/// This is **not** colour propagation. The strong version borrows the ratio
/// between channels from unclipped neighbours, so a blown red flower stays red
/// instead of being pulled toward white; that needs a neighbourhood search, its
/// own halo, and its own pass. What is here is the local approximation: right
/// for specular highlights, skies and light sources, which is most of what
/// actually blows out, and wrong in the direction of white — uniformly white,
/// now — for a saturated subject that clips.
fn reconstruct_highlights(balanced: vec3<f32>, ixy: vec2<f32>) -> vec3<f32> {
    // The sensor clips at one value in its own space; white balance moves that
    // to a different height per channel, which is why the threshold is a vector.
    let thresholds = params.wb.rgb * params.develop.z;
    let clipped = smoothstep(thresholds * (1.0 - CLIP_RUNUP), thresholds, balanced);

    // How far this pixel is a blown highlight at all. The *first* channel to go
    // drives it, because that is when the colour stops being known.
    let blown = max(clipped.r, max(clipped.g, clipped.b));
    // Exactly zero below the run-up, so an unclipped pixel comes through
    // untouched rather than merely almost untouched.
    if (blown <= 0.0) {
        return balanced;
    }

    // Neutral at the pixel's own brightest channel. Never below any channel, so
    // this can only raise: a highlight that could darken would grow a dark rim
    // where the run-up begins.
    let level = max(balanced.r, max(balanced.g, balanced.b));
    let neutral = vec3<f32>(level);

    // Nothing in the frame stayed inside the sensor's range, so there is no
    // light to ask about. Neutral, which is what this did unconditionally
    // before it had anywhere to ask -- and bit for bit the same arithmetic, so
    // a frame in this regime renders exactly as it used to.
    if (params.guide_scale.z < 0.5) {
        return mix(balanced, neutral, blown);
    }

    // The colour of nearby light that did not clip, brought into this space by
    // the same multipliers the pixel went through.
    //
    // Fetched here rather than by the caller, so it sits past both early
    // returns and only the pixels that are actually blown read the guide. Not
    // an optimisation: measured either way the difference is inside the noise
    // of a full render on this adapter. It is simply where the value is needed.
    let reference = unclipped_colour() * params.wb.rgb;

    // Anchor it to the channels that are still measurements. A channel that did
    // not clip is a *fact*, and a reconstruction has no business contradicting
    // one -- so the borrowed colour is scaled to agree with the survivors and
    // only the channels that stopped meaning anything are replaced.
    //
    // Least squares over all of them, rather than picking the least-clipped.
    // Picking is ambiguous exactly when it matters: with green gone, red and
    // blue both survive and disagree, and choosing the brighter is precisely
    // what produced the cyan arcs. This uses both, weighted by how much of each
    // is left.
    let survives = vec3<f32>(1.0) - clipped;
    let scale = dot(survives, balanced * reference)
        / max(dot(survives, reference * reference), EPS);

    // Never below what the sensor actually recorded. A clipped channel sits at
    // its threshold and its true value is at least that, so the physical bound
    // is kept -- but the floor is the *measurement* and not the threshold, and
    // the difference between those two is a bug this had.
    //
    // The run-up begins a quarter below the threshold, so a channel measured at
    // 0.9 of it is inside the blend while still being an honest number. Floored
    // at the threshold it was inflated to 1.0 and partly mixed in: green rises,
    // red and blue do not, and every cloud edge in a blue sky grows a cyan rim.
    // That is the same artefact this whole function exists to remove, arriving
    // through the clamp meant to make it safe.
    let inferred = max(reference * scale, balanced);
    // Per channel, by how far that channel has gone. A channel still in the
    // clear keeps its measurement exactly; one inside the run-up crosses over
    // smoothly, which is what keeps the boundary of a highlight invisible.
    let borrowed = mix(balanced, inferred, clipped);

    // And when the *last* channel goes too there is no anchor left, so the
    // magnitude is unknowable however well the colour is known. Neutral again,
    // faded in as the final survivor disappears -- which is also what the middle
    // of a genuinely blown highlight looks like.
    let nothing_left = min(clipped.r, min(clipped.g, clipped.b));
    return mix(borrowed, neutral, nothing_left);
}

/// Fixed sigmoid roll-off, applied per channel.
///
/// `y = x / (x + k)` with `k` chosen so that **a photographed** mid-grey lands
/// on display mid-grey.
///
/// `k` was 0.82, from assuming a photographed mid-grey sits at 0.18 of the
/// sensor's full scale — 0.18 = 0.18 / (0.18 + k). It does not. A camera meters
/// below that to keep its highlights, and measured against ten frames and this
/// body's own JPEG rendering of them, mid-grey sits at about **0.072**:
/// 0.18 = 0.072 / (0.072 + k) gives k = 0.33.
///
/// The consequence of the old value was not subtle. Every default render was
/// **1.3 stops dark**, and because the curve is asymptotic a fully clipped
/// sensor value could only reach 0.75 — 196 of 255 — so the top of every
/// histogram was empty. Over those ten frames the error against the camera's own
/// rendering falls from **46.5 levels rms to 31.5**, and a clipped highlight now
/// reaches 226.
///
/// **The curve's shape is untouched**, and so is everything asserted about it:
/// it still never clips, is still monotonic, and 40x full scale still renders
/// below white. What moved is where it sits.
///
/// What is left at 31.5 is the *shape*: a hyperbolic has no shoulder, so it
/// still runs about 30 levels short of a camera's rendering in the brightest
/// tones. That is hard-list item 2 — a curve that feels right — and it is a
/// question of taste and iteration rather than a constant to measure.
///
/// A profile's own tone curve substitutes for this one rather than composing
/// with it, so none of this reaches a render with a `.dcp` loaded: those already
/// matched the camera to within a few levels and are unmoved.
///
/// Three properties matter more than the exact curve:
///
/// - **It never clips.** y approaches 1 asymptotically, so a highlight three
///   stops over full scale still carries detail instead of becoming a flat
///   patch — and, more importantly, does not become a *coloured* flat patch
///   when one channel saturates before the others.
/// - **It is monotonic**, so it cannot invert local contrast.
/// - **Mid-grey is fixed**, so exposure remains the control that moves
///   brightness and the tone map is not secretly a second one.
///
/// This is the roll-off, not the look. A curve that *feels* like a photograph
/// is a taste problem with its own iteration loop, and pretending otherwise by
/// tuning constants here would bury it.
/// The profile's own tone curve, when it brought one.
///
/// **Scene-linear in, display-referred linear out**, with no decoding on the
/// way. The curve's output looks like an encoding — `f(0.18) = 0.481` sits near
/// sRGB's 0.459 — and reading it that way is wrong, which the measurement
/// settled: taken as *linear*, `f(0.05) = 0.090` is L* 35.9 against the camera's
/// 35.2 and `f(0.25) = 0.607` is L* 82.2 against its 82.2. Decoded first, the
/// same curve crushed the shadows to L* 1.
///
/// It resembles an encoding because a display rendering has roughly that shape.
/// It is not one.
///
/// Clamped at one: a tone curve is defined over `[0, 1]` and anything brighter
/// than white is white, which is the whole point of a shoulder.
fn profile_tone(v: f32) -> f32 {
    return sample_curve(params.curve.x, params.curve.y, v);
}

/// The profile's curve over a colour rather than over a number.
///
/// Running the curve down each channel on its own is the obvious thing and it
/// is wrong: a curve steeper in the darks lifts the dimmest channel further
/// than the brightest, which drags the colour towards grey in the shadows and
/// away from it in the highlights. On a clear sky -- red dim, blue bright, green
/// between them -- that shows up as the green pulling clear of red and the whole
/// thing turning cyan.
///
/// So only the darkest and brightest channels go through the curve, and the
/// middle one is put back at the same fraction of the way between them that it
/// started at. Lightness and contrast come from the curve; the hue is the one
/// the profile's matrices already decided. This is what the DNG reference
/// implementation does, and it is why a profile can carry a strong curve
/// without also carrying a colour shift.
fn profile_tone_rgb(c: vec3<f32>) -> vec3<f32> {
    let lo = min(c.r, min(c.g, c.b));
    let hi = max(c.r, max(c.g, c.b));
    let lo_out = profile_tone(lo);
    let hi_out = profile_tone(hi);
    // A neutral has nothing between the ends to place, and the division below
    // would be by zero.
    if (hi <= lo) {
        return vec3<f32>(lo_out, lo_out, lo_out);
    }
    let mid = clamp(c.r + c.g + c.b - lo - hi, lo, hi);
    let mid_out = lo_out + (hi_out - lo_out) * (mid - lo) / (hi - lo);
    // Rebuilt by matching each channel back to the end it came from, so a
    // colour with two equal channels keeps them equal.
    return vec3<f32>(
        select(select(mid_out, hi_out, c.r >= hi), lo_out, c.r <= lo),
        select(select(mid_out, hi_out, c.g >= hi), lo_out, c.g <= lo),
        select(select(mid_out, hi_out, c.b >= hi), lo_out, c.b <= lo),
    );
}

/// The user's own curve, shaped by hand and applied after everything the profile
/// does — the profile decides what the camera saw, the curve decides what to
/// make of it, and the last word belongs to the person.
fn user_curve(v: f32) -> f32 {
    return sample_curve(params.user_curve.x, params.user_curve.y, v);
}

/// One curve lookup, linear between entries and clamped at both ends.
///
/// Clamping is right for a tone curve twice over: it is defined on `[0, 1]`, and
/// anything brighter than white is white, which is what a shoulder is for.
fn sample_curve(base: u32, entries: u32, v: f32) -> f32 {
    let last = entries - 1u;
    let x = clamp(v, 0.0, 1.0) * f32(last);
    let i = min(u32(floor(x)), last);
    let j = min(i + 1u, last);
    return mix(hue_sat_map[base + i].x, hue_sat_map[base + j].x, fract(x));
}

fn tone_map(x: vec3<f32>) -> vec3<f32> {
    let clamped = max(x, vec3<f32>(0.0));
    return clamped / (clamped + vec3<f32>(TONE_MAP_K));
}

/// The same sigmoid on one value, for the channel that decides a colour's
/// compression when the ratios are being kept.
fn tone_sigmoid(x: f32) -> f32 {
    let clamped = max(x, 0.0);
    return clamped / (clamped + TONE_MAP_K);
}

/// Compress a colour without turning it.
///
/// # The defect this exists for
///
/// Compressing each channel on its own compresses the largest one
/// proportionally hardest, so every colour walks towards white along a path
/// that is **not** constant hue. Measured on a real frame, three stops of
/// exposure rotates an amber window light 13.9 degrees towards yellow across
/// nine thousand pixels, with nothing clipped anywhere. That rotation is what
/// "blown out" looks like before anything is actually blown out, and no later
/// colour control can undo it — by then the hue that was photographed is gone.
///
/// # The fix, and why it is max RGB
///
/// Ask the curve how much it compresses at this colour's **largest** channel,
/// and apply that one number to all three. A single gain leaves the ratios
/// between the channels exactly as they were, and the ratios are what hue and
/// saturation are.
///
/// It has to be the largest channel and not a cleverer norm. The output's
/// largest channel is `curve(norm) * peak / norm`, which stays inside the
/// display exactly when `norm >= peak`, and every norm worth having — the power
/// norm `(R^3+G^3+B^3)/(R^2+G^2+B^2)`, luminance, the Euclidean norm — is a
/// weighted mean of the channels and therefore **below** the largest one. Each
/// would push saturated colours out of the display and leave the per-channel
/// clamp at the end of the pipeline to bring them back, which turns the hue at
/// the clamp instead of at the curve. Max RGB is the boundary of the safe
/// family, which is the whole of its theoretical justification and enough.
///
/// The cost, stated: max RGB darkens saturated blues relative to a luminance
/// norm, because a blue's largest channel is a long way above its brightness.
/// That is a known and accepted trade in every engine that ships this.
///
/// # Why it is a blend and not a switch
///
/// Because bleaching towards white is sometimes the photograph. A sunset, a
/// fire, a filament: film does this and the eye expects it, and a perfectly
/// hue-stable sun is a flat orange disc. See `Tone::hue_preservation`.
///
/// # The bleach at the top, and the mistake it is easy to make here
///
/// One weight across the whole range was a compromise that did neither end
/// well. Film holds hue in the midtones and bleaches at the *top*: a specular,
/// a filament, the sun's core all go white, and a rendering that keeps them
/// perfectly saturated turns the sun into a flat orange disc — which is what a
/// flat weight below 1 was buying, and what it was costing.
///
/// The obvious way to add that back is to let the weight fall towards zero near
/// white, since zero is the per-channel curve and the per-channel curve's
/// asymptote *is* white. **That is wrong, and it was built and measured before
/// it was understood.** Per-channel compression does two things at once: it
/// desaturates, and it rotates the hue. Tapering towards it hands back the
/// artefact along with the look. Measured on DSC00794 at +3 EV, where a flat
/// weight of 0.75 left 1.6 degrees of hue drift, tapering towards per-channel
/// left **18.9** — worse than the 13.9 of no preservation at all, because the
/// exposure had pushed the midtones into the bleach zone.
///
/// A bleach is desaturation **along constant hue**: towards the neutral of the
/// same brightness, not towards whatever the per-channel curve happens to
/// produce. That is the same conclusion filmic reached with its extreme-
/// luminance saturation curves, and the two are kept as two separate mixes
/// here precisely so that they cannot be confused again.
///
/// The midtones can then be fully faithful, which is what they should have been
/// all along.
///
/// Nothing is needed at the dark end, and that is provable rather than assumed:
/// for small `x` the sigmoid is `x / TONE_MAP_K`, which is linear, so the
/// per-channel and ratio-preserving answers already agree there. A taper into
/// the shadows would be a no-op with a cost.
///
/// # The invariant that makes this safe
///
/// **Nothing here can change a colour's largest channel.** All three candidates
/// agree on it exactly: per-channel gives `curve(norm)`, the preserved path
/// gives `norm * curve(norm) / norm`, and the neutral the bleach heads for is
/// `peak` in every channel — one number, three times. So this whole control
/// moves chroma and never brightness, and a weight that varies with brightness
/// therefore cannot fold the curve back on itself.
/// `the_blend_never_moves_the_brightest_channel` is what holds it.
///
/// At a weight of exactly zero this returns `mapped` untouched rather than an
/// algebraically equal rearrangement of it, so an edit that turns this off is
/// **bit**-identical to a build that never had it.
fn hue_preserved(source: vec3<f32>, mapped: vec3<f32>, norm: f32, peak: f32) -> vec3<f32> {
    let keep = params.tone_map.x;
    if (keep <= 0.0 || norm <= EPS) {
        return mapped;
    }
    // The ratios, exactly as they arrived.
    let preserved = max(source, vec3<f32>(0.0)) * (peak / norm);
    // And the bleach: towards the neutral *of the same peak*, so saturation
    // falls and hue does not move. Smoothstep rather than a linear ramp because
    // the taper's slope is visible in a gradient that crosses it — a sky
    // running up to a sun — and a corner in the weight reads as a ring around
    // the highlight.
    //
    // **Where it starts depends on how much colour there is to lose**, and
    // that is the half this was missing. See `HUE_BLEACH_NEUTRAL`.
    let floor = min(preserved.r, min(preserved.g, preserved.b));
    // Measured in the *perceptual* coordinate, not in the light. A bright
    // near-white pixel at (0.49, 0.39, 0.65) of full scale is 40% saturated as
    // a ratio of linear values and looks like a faintly tinted white, because
    // the eye reads the ratio of the cube-rootish quantities and not of the
    // photons. Judging "is there colour here worth defending" on the linear
    // ratio defends casts nobody would call a colour -- which is how this
    // measured 0.40 for a grey cloud and 0.538 for an orange one, two numbers
    // that had to be far apart and were not. In the display's coordinate the
    // same pair reads 0.21 and 0.538.
    let saturation = 1.0 - pow(floor / max(peak, EPS), 1.0 / TONE_GAMMA);
    let onset = mix(
        HUE_BLEACH_NEUTRAL,
        HUE_BLEACH_FROM,
        smoothstep(0.0, HUE_BLEACH_COLOURED, saturation),
    );
    let bleach = smoothstep(onset, 1.0, peak);
    let bleached = mix(preserved, vec3<f32>(peak), bleach);
    return mix(mapped, bleached, keep);
}

// ---------------------------------------------------------------------------
// present — a finished tile's interior into the canvas.
//
// The last stage of an interactive render and the reason it can be interactive:
// the result stays on the GPU. Reading it back to the CPU, as export does,
// means a full device sync per tile, and no amount of tiling reaches 60fps
// through a dozen stalls a frame.
//
// Trimming the halo here rather than on the way out is what lets the copy be a
// straight dispatch: the tile's interior begins `halo` pixels in on both axes,
// and everything outside it exists only to make the interior correct.
// ---------------------------------------------------------------------------
@compute @workgroup_size(8, 8)
fn present(@builtin(global_invocation_id) gid: vec3<u32>) {
    let tile = u32(params.present.z);
    if (gid.x >= tile || gid.y >= tile) {
        return;
    }
    if (i32(gid.x) >= params.extent.x || i32(gid.y) >= params.extent.y) {
        return;
    }
    // The tile's own axes, turned onto the canvas. A quarter turn is applied
    // here rather than by rotating the mosaic, because rotating the mosaic would
    // move the CFA phase and every pixel would come out the wrong colour.
    let step = vec2<i32>(gid.xy);
    let dest = params.present.xy
        + vec2<i32>(
            params.axes.x * step.x + params.axes.z * step.y,
            params.axes.y * step.x + params.axes.w * step.y,
        );
    // A tile can overhang the canvas on any side. Dropping those pixels beats
    // clamping them, which would smear an edge column across the view.
    let bounds = vec2<i32>(textureDimensions(canvas));
    if (dest.x < 0 || dest.y < 0 || dest.x >= bounds.x || dest.y >= bounds.y) {
        return;
    }
    let halo = params.present.w;
    let src = idx(i32(gid.x) + halo, i32(gid.y) + halo);
    textureStore(canvas, vec2<u32>(dest), rgba_out[src]);
}

// ---------------------------------------------------------------------------
// Stage J -- capture sharpening.
//
// A demosaiced frame is soft by construction: two thirds of every pixel was
// interpolated. This is the unsharp mask that answers that, and it is two
// passes rather than one because a neighbourhood operation cannot read the
// buffer it is writing -- the neighbours would be a mixture of sharpened and
// unsharpened values, and which is which depends on the order the GPU happened
// to run in.
//
// So `luma` writes the developed luminance into `vh`, a plane the demosaic has
// finished with, and `sharpen` reads *that* neighbourhood and adds the result
// to its own pixel only. No aliasing, and no buffer that did not already exist.
//
// Luminance rather than colour, so an edge cannot pick up a fringe: the same
// correction is added to all three channels, which moves the pixel along the
// grey axis and leaves its hue where it was.
// ---------------------------------------------------------------------------
@compute @workgroup_size(8, 8)
fn luma(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= i32(params.width) || y >= i32(params.height)) {
        return;
    }
    let p = idx(x, y);
    // Rec. 709, which is what the display-referred values are in by this point.
    vh[p] = dot(rgba_out[p].rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
}

/// How far the blur reaches, in pixels. The tile halo is sized to cover it --
/// see `HALO` in `render.rs`, which carries the arithmetic.
const SHARPEN_REACH: i32 = 2;

/// The same, for texture, which asks a wider question.
///
/// Six against the sharpen's two, and the tile halo was widened from 22 to 26 to
/// carry it — see the derivation on `HALO` in `render.rs`. The radius *is* the
/// distinction between this control and capture sharpening, so it is not a
/// number to trim: at ±2 the two sliders would do the same thing. Reading
/// further than the halo covers would take pixels the demosaic got wrong at a
/// tile boundary, which shows as a faint grid rather than as anything anyone
/// would blame on this.
const TEXTURE_REACH: i32 = 6;
const TEXTURE_SIGMA: f32 = 2.5;
/// Where detail stops and an edge begins, in display-referred luminance.
///
/// The one thing that makes texture a different control from sharpening rather
/// than a second copy of it at another radius: the correction is rolled off
/// where the local detail is large, so a hard edge is left alone. That is what
/// lets the slider go *negative* and read as smoothing rather than as blur --
/// small detail goes and the edges stay.
const TEXTURE_KNEE: f32 = 0.06;

/// The mean of `vh` over a Gaussian of this sigma.
///
/// Normalised by the weight actually used rather than by a constant, so a tap
/// clamped at the edge of the tile does not darken the blur and turn the border
/// into a bright line.
fn blur_luma(x: i32, y: i32, reach: i32, sigma: f32) -> f32 {
    let falloff = -0.5 / (sigma * sigma);
    var blurred = 0.0;
    var total = 0.0;
    for (var j = -reach; j <= reach; j = j + 1) {
        for (var i = -reach; i <= reach; i = i + 1) {
            let sx = clamp(x + i, 0, i32(params.width) - 1);
            let sy = clamp(y + j, 0, i32(params.height) - 1);
            let weight = exp(f32(i * i + j * j) * falloff);
            blurred = blurred + vh[idx(sx, sy)] * weight;
            total = total + weight;
        }
    }
    return blurred / total;
}

@compute @workgroup_size(8, 8)
fn sharpen(@builtin(global_invocation_id) gid: vec3<u32>) {
    let amount = params.detail.x;
    let texture = params.local_contrast.y;
    // Exactly zero has to change exactly nothing: it is the claim that lets a
    // stored edit turn this off completely rather than nearly.
    if (amount <= 0.0 && texture == 0.0) {
        return;
    }
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= i32(params.width) || y >= i32(params.height)) {
        return;
    }
    let p = idx(x, y);

    var correction = 0.0;
    if (amount > 0.0) {
        let sigma = max(params.detail.y, 0.05);
        correction = amount * (vh[p] - blur_luma(x, y, SHARPEN_REACH, sigma));
    }
    if (texture != 0.0) {
        let detail = vh[p] - blur_luma(x, y, TEXTURE_REACH, TEXTURE_SIGMA);
        // A Gaussian roll-off on the detail's own size. At the knee the
        // correction is already down to a third, and by twice it there is
        // essentially none -- so an edge keeps the contrast it has and only the
        // grain of the surface moves.
        let ratio = detail / TEXTURE_KNEE;
        correction = correction + texture * detail * exp(-ratio * ratio);
    }
    // The same amount added to every channel: the pixel moves along the grey
    // axis, so an edge gains contrast without gaining colour.
    rgba_out[p] = vec4<f32>(rgba_out[p].rgb + vec3<f32>(correction), 1.0);
}

// ---------------------------------------------------------------------------
// Stage F' -- chroma noise reduction, in scene-linear light because that is
// where the noise is.
//
// Colour blotches in the shadows are the kind of noise that survives being
// printed and that no amount of exposure fixes. Smoothing *colour* removes them
// and costs nothing visible, because the eye takes its detail from luminance —
// so each pixel's own brightness is put back exactly, and only the hue and
// saturation are borrowed from the neighbourhood.
//
// Exactly, in *this* space. The colour matrix downstream mixes channels, so
// changing a pixel's colour does move its final luminance a little; that is the
// profile's arithmetic, not this stage's, and on a real frame it came to about
// one percent while the colour noise halved.
//
// Two passes for the same reason sharpening needs two: a neighbourhood
// operation cannot read the buffer it is writing. The blurred colour goes into
// the three channel planes the demosaic has finished with, and the second pass
// reads its own pixel from each. No aliasing, and no buffer that did not exist.
// ---------------------------------------------------------------------------

/// How far the chroma blur reaches. Folded into `HALO` in `render.rs`.
const CHROMA_REACH: i32 = 2;

/// The frame noise a chroma-noise slider position means what it says at.
///
/// The reduction used to be a fixed amount that knew nothing about the
/// photograph it was cleaning, so one number had to serve both a base-ISO frame
/// with fine colour detail in it and a pushed one full of blotches. Measured on
/// the two ends of that: at full strength the ISO 1000 frame loses 58% of its
/// high-frequency chroma, which is the noise, and the ISO 200 one loses 37%,
/// which is its windows. A single compromise gives away something at both ends,
/// and the frame itself says which end it is on — see `Guide::noise`.
///
/// This is the middle of the measured range, so the slider still means roughly
/// what it did on a typical photograph and the scaling is a correction rather
/// than a re-pitch: ISO 100-200 reads 0.0024 to 0.0031 and ISO 500-1000 reads
/// 0.0046 to 0.0054.
const NOISE_TYPICAL: f32 = 0.0035;

/// How far the frame's own noise may pull the slider, either way.
///
/// Bounded because the estimate is a statistic and a statistic can be wrong:
/// a frame that is one flat wall has little for the percentile to sit on. Two
/// stops of authority is enough to separate a clean exposure from a pushed one
/// and not enough for a bad estimate to turn the control off or run it to full
/// on its own.
const NOISE_STRENGTH_MIN: f32 = 0.5;
const NOISE_STRENGTH_MAX: f32 = 2.0;

/// Above this the reading is not noise, and the scaling gives up on it.
///
/// `Guide::noise` cannot tell noise from detail at the sensor's own limit —
/// nothing that looks at one Bayer quad can. It gets away with that on a
/// photograph because a photograph has flat light somewhere and the
/// quarter-point finds it. A frame that is modulated at pixel scale *all over*
/// has no flat light to find, and the statistic reads the detail instead: the
/// golden chirp, a radial frequency sweep running to Nyquist in every corner,
/// reads **0.033** against 0.0024 to 0.0054 for the ten reference photographs.
///
/// Left alone that saturated the boost and took 13-21% of the texture out of
/// the golden fixtures — a denoiser eating detail, which is the one thing it
/// must not do.
///
/// So a reading outside the range photographs occupy is treated as a *failed
/// measurement*, and the answer to a failed measurement is the behaviour you
/// had before you took it: the strength tapers back to exactly 1, which is the
/// fixed amount this replaced. Tapered rather than switched because the
/// alternative is a frame flipping its rendering on a hair's difference in a
/// statistic — it is per frame so it cannot draw a contour, but it can still
/// make two frames of the same scene disagree.
///
/// The band sits well clear of every real reading: the noisiest reference frame
/// is 0.0054 and this starts at 0.008.
const NOISE_IMPLAUSIBLE_FROM: f32 = 0.008;
const NOISE_IMPLAUSIBLE_BY: f32 = 0.016;

/// How far this photograph's own noise moves a noise control from its nominal
/// setting. One number, used by both stages, so there is one rule for what a
/// measurement means and one place it can be wrong.
///
/// Exactly 1 when there is no measurement and when the measurement is not
/// believable, which in both cases is the behaviour these controls had before
/// they could ask — see `NOISE_IMPLAUSIBLE_FROM`.
fn noise_scale() -> f32 {
    let measured = params.guide_scale.w;
    if (measured <= 0.0) {
        return 1.0;
    }
    let scaled = clamp(
        measured / NOISE_TYPICAL,
        NOISE_STRENGTH_MIN,
        NOISE_STRENGTH_MAX,
    );
    let gave_up = smoothstep(NOISE_IMPLAUSIBLE_FROM, NOISE_IMPLAUSIBLE_BY, measured);
    return mix(scaled, 1.0, gave_up);
}

/// And how much more the shadows get than mid-grey.
///
/// Relative photon noise goes as one over the square root of the signal, so the
/// same sensor is noisier in the dark *as a fraction of what is there* — which
/// is what chroma noise is, and why blotches live in the shadows. Keyed on the
/// pixel rather than the edit on purpose: a shadow is noisy whether or not
/// anybody has lifted it yet, and a denoiser that waits for the slider cleans
/// the picture only after it has already been seen dirty.
const NOISE_DARK_MAX: f32 = 2.0;

/// The value a photographed mid-grey arrives at, where the darkness scaling
/// above is neither raising nor lowering anything. `rawkit_engine::scene`'s
/// `MID_GREY`, and the same 0.18 the tone map is pinned to.
const NOISE_MID_GREY: f32 = 0.072;

@compute @workgroup_size(8, 8)
fn chroma_blur(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= i32(params.width) || y >= i32(params.height)) {
        return;
    }
    var sum = vec3<f32>(0.0);
    var total = 0.0;
    for (var j = -CHROMA_REACH; j <= CHROMA_REACH; j = j + 1) {
        for (var i = -CHROMA_REACH; i <= CHROMA_REACH; i = i + 1) {
            let sx = clamp(x + i, 0, i32(params.width) - 1);
            let sy = clamp(y + j, 0, i32(params.height) - 1);
            sum = sum + rgba_out[idx(sx, sy)].rgb;
            total = total + 1.0;
        }
    }
    let p = idx(x, y);
    let mean = sum / total;
    ch_r[p] = mean.r;
    ch_g[p] = mean.g;
    ch_b[p] = mean.b;
}

@compute @workgroup_size(8, 8)
fn chroma_mix(@builtin(global_invocation_id) gid: vec3<u32>) {
    let amount = params.detail.z;
    // Exactly zero changes exactly nothing, which is what lets a stored edit
    // turn this off rather than nearly off.
    if (amount <= 0.0) {
        return;
    }
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= i32(params.width) || y >= i32(params.height)) {
        return;
    }
    let p = idx(x, y);
    let original = rgba_out[p].rgb;
    let blurred = vec3<f32>(ch_r[p], ch_g[p], ch_b[p]);

    // Camera-space weights would need the profile; an unweighted mean is enough
    // to say "how bright is this" for the purpose of putting the brightness
    // back, and it cannot go negative on a wide-gamut primary the way Rec. 709
    // weights can.
    let was = (original.r + original.g + original.b) / 3.0;
    let now = (blurred.r + blurred.g + blurred.b) / 3.0;
    // A black pixel has no colour to borrow and dividing by its brightness is
    // how a denoiser produces fireflies.
    if (now <= 1e-6) {
        return;
    }
    // The neighbourhood's colour at this pixel's own brightness.
    let recoloured = blurred * (was / now);

    // How much this *photograph* needs, and how much this part of it needs.
    //
    // A guide with no noise measured in it -- a frame flat enough that the
    // percentile found nothing, or a caller that built none -- falls back to
    // the slider on its own, which is what this did before it could ask.
    let dark = clamp(sqrt(NOISE_MID_GREY / max(was, EPS)), 1.0, NOISE_DARK_MAX);
    let effective = clamp(amount * noise_scale() * dark, 0.0, 1.0);
    rgba_out[p] = vec4<f32>(mix(original, recoloured, effective), 1.0);
}

// ---------------------------------------------------------------------------
// Stage F'' -- luminance noise reduction, and the one that costs something.
//
// Smoothing colour takes nothing you can see. Smoothing brightness takes
// detail, because brightness is where all of the detail is — so this has to be
// edge-aware or it is just a blur with a friendlier name. A bilateral filter is
// the answer: neighbours are averaged in proportion to how *similar* they are,
// so a flat area averages freely and an edge averages with almost nothing.
//
// # Why the comparison happens on the square root
//
// Sensor noise is dominated by photon shot noise, whose standard deviation
// grows as the square root of the signal. A threshold applied to linear light
// therefore means two different things in the same frame: generous in the
// shadows, where it swallows real detail, and mean in the highlights, where the
// noise it was meant to catch sails past it.
//
// The square root is the variance-stabilising transform for that noise — after
// it, the noise has roughly the same width everywhere, so **one number means
// one thing across the whole frame**. `the_same_setting_reaches_the_shadows_and
// _the_highlights` is the test that holds this to account, and it is a test that
// could not be written at all without this being true.
//
// # What the strength does
//
// It sets the range threshold and nothing else. At zero the filter has no
// tolerance for difference, every neighbour weighs nothing, and the pixel keeps
// itself; at one the tolerance is wide enough that a flat area averages
// completely. The plastic look at the top of the range is real and is the same
// bargain every denoiser offers — this one just does not make it for you.
//
// Two passes, like chroma: a neighbourhood operation cannot read the buffer it
// is writing. The smoothed brightness goes into the red channel plane the
// demosaic has finished with.
// ---------------------------------------------------------------------------

/// How far the bilateral reaches. Folded into `HALO` in `render.rs`, where 3
/// and 2 happen to cost the same because the halo rounds to an even number.
const LUMA_REACH: i32 = 3;
/// Range tolerance at full strength, in square-root-signal units, **on a frame
/// whose noise is [`NOISE_TYPICAL`]**.
///
/// Calibrated against `noise_falls_and_the_edge_survives` rather than reasoned
/// from a sensor model: the number that has to be right is "how much does a
/// flat area smooth before an edge starts to move", and that is measurable.
///
/// Scaled by `noise_scale` at the point of use, so this is where the slider
/// sits on a typical photograph and other frames move either side of it. Left
/// where the calibration put it deliberately: the change is to make the control
/// consistent across frames, not to re-pitch what its numbers mean.
const LUMA_SIGMA: f32 = 0.075;
/// Spatial falloff across the kernel, in pixels. Wide enough that the corners
/// of a 7x7 still contribute, narrow enough that the nearest ring dominates.
const LUMA_SPATIAL: f32 = 2.0;

/// The brightness this stage smooths.
///
/// An unweighted mean, matching `chroma_mix`, for the same reason: putting a
/// pixel's brightness back needs a measure of it, not a colorimetric one, and an
/// unweighted mean cannot go negative on a wide-gamut primary.
fn brightness(rgb: vec3<f32>) -> f32 {
    return (rgb.r + rgb.g + rgb.b) / 3.0;
}

@compute @workgroup_size(8, 8)
fn luminance_blur(@builtin(global_invocation_id) gid: vec3<u32>) {
    let strength = params.detail.w;
    if (strength <= 0.0) {
        return;
    }
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= i32(params.width) || y >= i32(params.height)) {
        return;
    }

    let centre = brightness(rgba_out[idx(x, y)].rgb);
    // `max` rather than a branch on negatives: a pixel below black is a real
    // thing after white balance, and its square root is not.
    let centre_root = sqrt(max(centre, 0.0));
    // **Relative to what this photograph's noise actually is.**
    //
    // A bilateral's range tolerance is a statement about the noise: neighbours
    // closer together than that are the same thing seen twice, and further
    // apart are two things. `LUMA_SIGMA` was calibrated against a synthetic
    // fixture's noise depth, so the slider meant a different amount of
    // filtering on every photograph — measured on two real ones at the same
    // setting, the noisy frame shed 46% of its noise while the clean one lost
    // 24% of its detail, and nothing in the control knew the difference.
    //
    // This module already makes exactly this argument once, for the square root
    // the comparison happens on: one setting has to mean one thing at every
    // brightness *within* a frame. It means one thing across frames now too.
    let sigma = LUMA_SIGMA * noise_scale() * strength;

    var sum = 0.0;
    var total = 0.0;
    for (var j = -LUMA_REACH; j <= LUMA_REACH; j = j + 1) {
        for (var i = -LUMA_REACH; i <= LUMA_REACH; i = i + 1) {
            let value = brightness(rgba_out[idx(x + i, y + j)].rgb);
            let d = sqrt(max(value, 0.0)) - centre_root;
            let spatial = f32(i * i + j * j) / (2.0 * LUMA_SPATIAL * LUMA_SPATIAL);
            let range = (d * d) / (2.0 * sigma * sigma);
            let w = exp(-spatial - range);
            sum = sum + w * value;
            total = total + w;
        }
    }
    // The centre always weighs 1, so this cannot be zero.
    ch_r[idx(x, y)] = sum / total;
}

@compute @workgroup_size(8, 8)
fn luminance_mix(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (params.detail.w <= 0.0) {
        return;
    }
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= i32(params.width) || y >= i32(params.height)) {
        return;
    }
    let p = idx(x, y);
    let original = rgba_out[p].rgb;
    let was = brightness(original);
    // Nothing to scale, and dividing by it is how a denoiser makes fireflies.
    if (was <= 1e-6) {
        return;
    }
    // Scaling the triple keeps every ratio between the channels, so the colour
    // is exactly what it was and only the brightness moved — the mirror of what
    // `chroma_mix` does, and the reason the two can both run without either
    // undoing the other.
    rgba_out[p] = vec4<f32>(original * (ch_r[p] / was), 1.0);
}
