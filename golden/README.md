# Golden render tests

The one invariant this harness enforces:

> Same RAW + same `EditState` → same pixels on Linux, Windows and macOS.

Adobe parity is **not** the reference and never will be. The reference is our own
committed render; what the tests guard against is *cross-platform divergence*,
which is the real risk in a stack where the same WGSL runs through three
different driver stacks (Vulkan, DX12, Metal).

## Layout

```
golden/
  cases/         one .json EditState per case, plus the RAW it applies to
  refs/          committed reference renders (PNG, 16-bit)
  out/           produced by test runs; git-ignored
```

RAW files are large and not redistributable, so `cases/` records the *identity*
of each file (camera, capture time, content hash) and the harness resolves it
against a local fixture directory. A missing fixture skips loudly, never
silently.

## Tolerance

Bit-exactness across three GPU vendors is not achievable and demanding it would
produce a test that is disabled within a month. The policy instead:

- A per-pixel tolerance small enough that a real algorithmic difference fails,
  and large enough that last-bit floating-point ordering does not.
- Failures report **which platform diverged from which**, not just "mismatch".
  A three-way test whose output does not say who disagreed is a test nobody acts
  on.
- The tolerance is a committed constant with a comment explaining the number.
  Loosening it is a reviewable change, not a quick fix for a red build.

## Status

Not implemented — the harness lands once there is a render to compare. The
directory and this contract exist first so it is written against a decision
rather than around whatever the renderer happens to do by then.
