# Golden render tests

The one invariant this harness enforces:

> Same RAW + same `EditState` → same pixels on Linux, Windows and macOS.

Adobe parity is **not** the reference and never will be. The reference is our own
committed render; what the tests guard against is *cross-platform divergence*,
which is the real risk in a stack where the same WGSL runs through three
different driver stacks (Vulkan, DX12, Metal).

Implemented in `crates/rawkit-engine/tests/golden.rs`, and it runs on all three
OSes in CI — the only test in the repository that must, because it is the only
one whose subject is the platforms themselves.

## Layout

```
golden/
  refs/          committed reference renders (16-bit PNG)
  out/           produced by ad-hoc runs; git-ignored
```

## The input is synthetic, on purpose

A golden test needs the same input everywhere, and RAW fixtures are large and
not redistributable, so a hosted runner cannot have one. The frames are
generated from a formula instead: identical on every machine, zero bytes in the
repository, and — because the pattern sweeps past the sampling limit — harder on
the demosaic than most photographs.

**The gap this leaves, stated plainly:** it proves the *engine* agrees across
platforms, not the *decoder*. LibRaw producing different pixels on Windows would
slip past. Closing that needs a fixture on each runner and is a separate problem.

## Tolerance

Bit-exactness across three GPU vendors is not something to demand by default —
a test that fails for last-bit reasons gets disabled within a month. The policy:

- A per-pixel tolerance of **8 / 65535**, set from measurement rather than
  guesswork — see below. It is a committed constant with the reasoning next to
  it, and loosening it is a reviewable change that wants a recorded reason.
- Failures report **which platform diverged, on which adapter and backend, at
  which pixel**. A three-way test whose output does not say who disagreed is a
  test nobody acts on.
- Every run also prints a hash per case, so three CI logs answer "bit-identical
  or merely within tolerance?" without anyone building tooling for it.

## What the first cross-platform run actually showed

References blessed on Linux / Vulkan / AMD RADV, then compared:

| Platform | Backend | Adapter | Worst difference |
|---|---|---|---|
| macOS | Metal | Apple Paravirtual | 1 / 65535 |
| Windows | Dx12 | Microsoft Basic Render Driver (WARP, software) | 1 / 65535 |

**The hashes differ, so the renders are not bit-identical** — which disproves
the expectation this file was originally written with. The kernels use only
add, multiply, divide, min/max and mix, with no transcendentals and no
fast-math, and that still was not enough for three drivers to agree to the last
bit.

They agree to one part in 65535, roughly four orders of magnitude below
anything visible. So the tolerance approach was the right call and the number
is now evidence-based rather than defensive.

Worth noting for CI purposes: the Windows runner has no GPU and wgpu fell back
to WARP, Microsoft's software DX12 rasteriser, without being asked to. The
three-platform matrix therefore costs nothing extra to keep running.

## Blessing a reference

```sh
RAWKIT_BLESS=1 cargo test -p rawkit-engine --test golden -- --ignored
```

Writes the references instead of comparing, then fails deliberately so the run
cannot be mistaken for a pass. **Look at the images before committing them.** The
diff is the review, and a reference that changes without anyone looking at the
picture has stopped being a reference.
