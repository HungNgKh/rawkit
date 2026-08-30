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
    // The display-referred tone controls, already reduced to curve parameters
    // on the CPU: `.x` is the contrast exponent, `.y` highlights, `.z` shadows,
    // and `.w` is 1 when any of the five is off its default. See `tone_curve`.
    tone: vec4<f32>,
    // `.xy` are the black and white points. `.zw` unused.
    levels: vec4<f32>,
    // `.x` is the sharpening amount, `.y` its radius in pixels, `.z` the chroma
    // noise reduction. `.w` unused.
    detail: vec4<f32>,
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
    let mapped = tone_map(exposed);

    // Stage I -- display-referred ops. The five tone controls live here and not
    // beside exposure, because the sigmoid is the boundary: exposure decides how
    // much light there was, these decide what it should look like.
    rgba_out[p] = vec4<f32>(
        vec3<f32>(
            tone_curve(mapped.r),
            tone_curve(mapped.g),
            tone_curve(mapped.b),
        ),
        1.0,
    );
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
    let hsv = rgb_to_hsv(rgb);
    let dims = params.hsm_dims;

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

    let c000 = hsm_at(h0, s0, v0);
    let c100 = hsm_at(h1, s0, v0);
    let c010 = hsm_at(h0, s1, v0);
    let c110 = hsm_at(h1, s1, v0);
    let c001 = hsm_at(h0, s0, v1);
    let c101 = hsm_at(h1, s0, v1);
    let c011 = hsm_at(h0, s1, v1);
    let c111 = hsm_at(h1, s1, v1);

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
const CLIP_RUNUP: f32 = 0.02;

/// Estimate what a clipped channel should have been.
///
/// # Why blown highlights go magenta without this
///
/// The sensor saturates in its own units, and white balance then scales the
/// channels apart. For a daylight scene green carries the most signal, so green
/// saturates first: its true value might be 1.5 but it records 1.0. Red is
/// nowhere near its own limit and records correctly, then gets multiplied up.
/// The result is a pixel where red and blue exceed green — magenta — in the one
/// part of the picture the eye most expects to be white.
///
/// # What this does, and what it does not
///
/// A channel we no longer believe is raised to the brightest channel we do
/// believe, which restores neutrality and keeps whatever structure the surviving
/// channels still carry. When nothing is left to believe, the only defensible
/// estimate is neutral at the highest clip point.
///
/// This is **not** colour propagation. The strong version of this algorithm
/// borrows the ratio between channels from unclipped neighbours, so a blown red
/// flower stays red instead of being pulled toward white; that needs a
/// neighbourhood search, its own halo, and its own pass. What is here is the
/// local approximation: right for specular highlights, skies and light sources,
/// which is most of what actually blows out, and wrong in the direction of white
/// for a saturated subject that clips.
fn reconstruct_highlights(balanced: vec3<f32>) -> vec3<f32> {
    // The sensor clips at one value in its own space; white balance moves that
    // to a different height per channel, which is why the threshold is a vector.
    let thresholds = params.wb.rgb * params.develop.z;
    let clipped = smoothstep(thresholds * (1.0 - CLIP_RUNUP), thresholds, balanced);

    // The brightest channel still below its own limit.
    let believable = balanced * (1.0 - clipped);
    let brightest = max(believable.r, max(believable.g, believable.b));

    // Everything clipped: nothing survives to take a level from, so aim for
    // neutral at the highest clip point rather than leaving the channels at
    // their own limits, which is the colour of the white balance itself.
    let all_clipped = min(clipped.r, min(clipped.g, clipped.b));
    let neutral = max(thresholds.r, max(thresholds.g, thresholds.b));
    let level = mix(brightest, neutral, all_clipped);

    // `clipped` is exactly zero below the run-up, so an unclipped pixel comes
    // through this untouched rather than merely almost untouched.
    return mix(balanced, max(balanced, vec3<f32>(level)), clipped);
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
