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
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> cfa: array<f32>;
@group(0) @binding(2) var<storage, read_write> vh: array<f32>;
@group(0) @binding(3) var<storage, read_write> pq: array<f32>;
@group(0) @binding(4) var<storage, read_write> lp: array<f32>;
@group(0) @binding(5) var<storage, read_write> ch_r: array<f32>;
@group(0) @binding(6) var<storage, read_write> ch_g: array<f32>;
@group(0) @binding(7) var<storage, read_write> ch_b: array<f32>;
@group(0) @binding(8) var<storage, read_write> rgba_out: array<vec4<f32>>;

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
    return u32(cy) * params.packed_width + u32((cx + adjust) / 2);
}

fn pq_at(x: i32, y: i32) -> f32 { return pq[packed_index(x, y)]; }
fn lp_at(x: i32, y: i32) -> f32 { return lp[packed_index(x, y)]; }

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
        pq[u32(y) * params.packed_width + u32(x / 2)] = pqs.x / (pqs.x + pqs.y);
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
        lp[u32(y) * params.packed_width + u32(x / 2)] = max(1e-6, low);
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
