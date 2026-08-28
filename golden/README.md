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

- A per-pixel tolerance of **24 / 65535**, roughly 4 parts in 10,000: far below
  anything visible, far above rounding. It is a committed constant with the
  reasoning next to it, and loosening it is a reviewable change that wants a
  recorded reason.
- Failures report **which platform diverged, on which adapter and backend, at
  which pixel**. A three-way test whose output does not say who disagreed is a
  test nobody acts on.
- Every run also prints a hash per case. Three CI logs therefore answer "are
  these platforms bit-identical, or merely within tolerance?" without anyone
  building tooling for it. As of the first run, the kernels use only add,
  multiply, divide, min/max and mix — all IEEE-exact, no transcendentals — so
  bit-identical is a reasonable expectation rather than a hope.

## Blessing a reference

```sh
RAWKIT_BLESS=1 cargo test -p rawkit-engine --test golden -- --ignored
```

Writes the references instead of comparing, then fails deliberately so the run
cannot be mistaken for a pass. **Look at the images before committing them.** The
diff is the review, and a reference that changes without anyone looking at the
picture has stopped being a reference.
