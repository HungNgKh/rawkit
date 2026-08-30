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
    // `.x` is the sharpening amount, `.y` its radius in pixels, `.z` the chroma
    // noise reduction. `.w` unused.
    detail: vec4<f32>,
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
@group(1) @binding(0) var canvas: texture_storage_2d<rgba16float, write>;

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
        ch_r[p] / params.wb.r,
        ch_g[p] / params.wb.g,
        ch_b[p] / params.wb.b,
        1.0,
    );
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
    let camera = rgba_out[p].rgb;

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
    let recovered = reconstruct_highlights(balanced);

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
    let shown = vec3<f32>(
        dot(params.working_to_display[0].rgb, corrected),
        dot(params.working_to_display[1].rgb, corrected),
        dot(params.working_to_display[2].rgb, corrected),
    );

    let exposed = shown * params.develop.x;
    // The profile's curve *instead of* ours, not as well as. Both map the scene
    // to a display, and running two tone maps in series maps the scene twice —
    // which reads as a flat, muddy picture rather than as a bug.
    var mapped = tone_map(exposed);
    if (params.curve.z > 0u) {
        mapped = vec3<f32>(
            profile_tone(exposed.r),
            profile_tone(exposed.g),
            profile_tone(exposed.b),
        );
    }

    // The profile's look, applied here and not beside the hue/saturation
    // correction: a look is authored against a rendered picture. Round-tripped
    // through the profile's working space, because that is where its hue and
    // saturation axes were measured — applying a ProPhoto-authored table to
    // sRGB primaries would read the wrong cell for every colour that is not
    // grey.
    //
    // **Before the user's controls, not after.** It used to be after, from
    // reading the specification's "the look comes after the tone curve" as
    // meaning *ours* — it means the profile's own. A look landing on top of
    // somebody's tone adjustments partly undoes them; the profile's rendering
    // finishes first and the person adjusts the result.
    var looked = mapped;
    if (params.develop.w > 0.5) {
        let working = vec3<f32>(
            dot(params.display_to_working[0].rgb, mapped),
            dot(params.display_to_working[1].rgb, mapped),
            dot(params.display_to_working[2].rgb, mapped),
        );
        let adjusted = apply_look(working);
        looked = vec3<f32>(
            dot(params.working_to_display[0].rgb, adjusted),
            dot(params.working_to_display[1].rgb, adjusted),
            dot(params.working_to_display[2].rgb, adjusted),
        );
    }

    // Stage I -- display-referred ops. The five tone controls live here and not
    // beside exposure, because the sigmoid is the boundary: exposure decides how
    // much light there was, these decide what it should look like.
    var shaped = vec3<f32>(
        tone_curve(looked.r),
        tone_curve(looked.g),
        tone_curve(looked.b),
    );
    // And the hand-drawn curve last of the tone controls, so it shapes what the
    // sliders left rather than competing with them.
    if (params.user_curve.z > 0u) {
        shaped = vec3<f32>(
            user_curve(shaped.r),
            user_curve(shaped.g),
            user_curve(shaped.b),
        );
    }
    // Stage J -- colour adjustments, after the tone curve for the same reason
    // the tone curve is after the tone map: this is about the picture, not the
    // light. In scene-linear it would depend on exposure, and a colour that
    // changed when you brightened the frame is not a colour control.
    // Stage K -- the look, and the last thing that touches colour.
    rgba_out[p] = vec4<f32>(grade_colour(mix_bands(saturate_colour(shaped))), 1.0);
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

/// Mid-grey in the perceptual coordinate: `0.18^(1/2.2)`.
const TONE_PIVOT: f32 = 0.45865646;
/// The exponent that coordinate uses.
const TONE_GAMMA: f32 = 2.2;
/// How far the shadow and highlight exponents may travel from 1. Bounded by
/// monotonicity at 0.8807 -- see the `tone` module in Rust for the derivation,
/// and the test there that checks these three numbers against this file.
const TONE_TAPER: f32 = 0.75;

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
fn tone_curve(y: f32) -> f32 {
    // Bit-identical passthrough when nothing is set, so an identity edit is
    // untouched by all of this rather than merely close to untouched.
    if (params.tone.w < 0.5) {
        return y;
    }

    // The tone map is asymptotic, so `y` is already inside [0, 1) -- but a
    // non-finite exposure would put it outside, and `1.0 - p` going negative
    // would make every `pow` below a NaN. Clamping is one instruction.
    let p0 = clamp(pow(max(y, 0.0), 1.0 / TONE_GAMMA), 0.0, 1.0);

    // Contrast: a power about the pivot, each side of it separately. Both
    // segments carry slope k at the pivot, so this is smooth there and not
    // merely continuous; 0, mid-grey and 1 are all fixed points.
    let k = params.tone.x;
    var p1: f32;
    if (p0 <= TONE_PIVOT) {
        p1 = TONE_PIVOT * pow(p0 / TONE_PIVOT, k);
    } else {
        p1 = 1.0 - (1.0 - TONE_PIVOT) * pow((1.0 - p0) / (1.0 - TONE_PIVOT), k);
    }

    // Shadows and highlights: powers whose exponent tapers to exactly 1 at the
    // pivot. Without the taper each control puts a slope discontinuity in the
    // middle of the frame, and a "highlights" slider visibly moves mid-grey.
    var p2: f32;
    if (p1 <= TONE_PIVOT) {
        let v = p1 / TONE_PIVOT;
        p2 = TONE_PIVOT * pow(v, 1.0 - params.tone.z * TONE_TAPER * (1.0 - v));
    } else {
        let u = (1.0 - p1) / (1.0 - TONE_PIVOT);
        p2 = 1.0 - (1.0 - TONE_PIVOT) * pow(u, 1.0 + params.tone.y * TONE_TAPER * (1.0 - u));
    }

    // The black and white points, and the only place in the whole pipeline that
    // clips. Deliberate: the endpoints are where a photographer asks for
    // clipping, and an editor whose black slider only compresses reads as
    // broken. The points can never cross -- see LEVELS_REACH in Rust.
    let levelled = clamp(
        (p2 - params.levels.x) / (params.levels.y - params.levels.x),
        0.0,
        1.0,
    );
    return pow(levelled, TONE_GAMMA);
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
    return apply_table(0u, params.hsm_dims, rgb);
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
    if (params.develop.w >= 1.5) {
        // Encoded, converted, and decoded again, so the table sees the space it
        // was built in.
        let encoded = vec3<f32>(encode_srgb(rgb.r), encode_srgb(rgb.g), encode_srgb(rgb.b));
        let out = apply_table(params.look_dims.w, params.look_dims, encoded);
        return vec3<f32>(decode_srgb(out.r), decode_srgb(out.g), decode_srgb(out.b));
    }
    return apply_table(params.look_dims.w, params.look_dims, rgb);
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

fn apply_table(base: u32, dims: vec4<u32>, rgb: vec3<f32>) -> vec3<f32> {
    let hsv = rgb_to_hsv(rgb);

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
        let val_pos = clamp(hsv.z, 0.0, 1.0) * f32(dims.z - 1u);
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
    let value = max(hsv.z * delta.z, 0.0);

    return hsv_to_rgb(vec3<f32>(hue, saturation, value));
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
fn reconstruct_highlights(balanced: vec3<f32>) -> vec3<f32> {
    // The sensor clips at one value in its own space; white balance moves that
    // to a different height per channel, which is why the threshold is a vector.
    let thresholds = params.wb.rgb * params.develop.z;
    let clipped = smoothstep(thresholds * (1.0 - CLIP_RUNUP), thresholds, balanced);

    // How far this pixel is a blown highlight at all. The *first* channel to go
    // drives it, because that is when the colour stops being known.
    let blown = max(clipped.r, max(clipped.g, clipped.b));

    // Neutral at the pixel's own brightest channel — which for a fully clipped
    // pixel is the highest threshold, so the "nothing left to believe" case
    // needs no branch of its own. Never below any channel, so this can only
    // raise: a highlight that could darken would grow a dark rim where the
    // run-up begins.
    let level = max(balanced.r, max(balanced.g, balanced.b));

    // `blown` is exactly zero below the run-up, so an unclipped pixel comes
    // through this untouched rather than merely almost untouched.
    return mix(balanced, vec3<f32>(level), blown);
}

/// Fixed sigmoid roll-off, applied per channel.
///
/// `y = x / (x + k)` with `k` chosen so that scene mid-grey (0.18) lands on
/// display mid-grey (0.18): 0.18 = 0.18 / (0.18 + k) gives k = 0.82.
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
    let k = 0.82;
    let clamped = max(x, vec3<f32>(0.0));
    return clamped / (clamped + vec3<f32>(k));
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

@compute @workgroup_size(8, 8)
fn sharpen(@builtin(global_invocation_id) gid: vec3<u32>) {
    let amount = params.detail.x;
    // Exactly zero has to change exactly nothing: it is the claim that lets a
    // stored edit turn this off completely rather than nearly.
    if (amount <= 0.0) {
        return;
    }
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= i32(params.width) || y >= i32(params.height)) {
        return;
    }

    // A Gaussian whose sigma is the radius. Normalised by the weight actually
    // used rather than by a constant, so a tap clamped at the edge of the tile
    // does not darken the blur and turn the border into a bright line.
    let sigma = max(params.detail.y, 0.05);
    let falloff = -0.5 / (sigma * sigma);
    var blurred = 0.0;
    var total = 0.0;
    for (var j = -SHARPEN_REACH; j <= SHARPEN_REACH; j = j + 1) {
        for (var i = -SHARPEN_REACH; i <= SHARPEN_REACH; i = i + 1) {
            let sx = clamp(x + i, 0, i32(params.width) - 1);
            let sy = clamp(y + j, 0, i32(params.height) - 1);
            let weight = exp(f32(i * i + j * j) * falloff);
            blurred = blurred + vh[idx(sx, sy)] * weight;
            total = total + weight;
        }
    }
    let p = idx(x, y);
    // The same amount added to every channel: the pixel moves along the grey
    // axis, so an edge gains contrast without gaining colour.
    let correction = amount * (vh[p] - blurred / total);
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
    rgba_out[p] = vec4<f32>(mix(original, recoloured, amount), 1.0);
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
/// Range tolerance at full strength, in square-root-signal units.
///
/// Calibrated against `noise_falls_and_the_edge_survives` rather than reasoned
/// from a sensor model: the number that has to be right is "how much does a
/// flat area smooth before an edge starts to move", and that is measurable.
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
    let sigma = LUMA_SIGMA * strength;

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
