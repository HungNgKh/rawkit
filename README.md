# rawkit

A standalone, fully local RAW photo editor with catalog management, for Linux,
Windows and macOS. Free and Apache-2.0.

**Status: P0 — foundations.** Nothing here edits a photo yet. The workspace,
licence enforcement and the tri-platform CI matrix exist first, on purpose: they
are cheap now and expensive to retrofit.

> This editor is free and runs entirely on your machine. It makes no network
> calls and collects nothing. AI assistance is planned for a later version and
> may be a paid add-on. **The editor and catalog stay free.**

That statement is here from the first commit rather than at first release,
because a free-editor-plus-paid-AI split is accepted when it is honest from day
one and resented when it is retrofitted.

## What v1 is, and is not

**Is:** a local editor and library — decode, render, cull, edit, export. Our
pixels, our colour management, our catalog.

**Is not:** a Lightroom companion. No plugin, no XMP round-trip, no cloud, no
telemetry, no payments, and **no AI in v1**. Lightroom appears once, as an
optional one-way import of library metadata (ratings, keywords, collections).

## The invariant everything serves

> Same RAW + same `EditState` → same pixels on Linux, Windows and macOS.

One engine, compiled for three platforms, sharing WGSL verbatim. There is no
per-platform render path — not even a faster one — because a look that differs
by operating system is not a look. Golden render tests run on all three in CI.

## Crates

| Crate | Role |
|---|---|
| `rawkit-editstate` | Canonical edit parameters. The single source of truth for how a photo is rendered, and the seam every other crate hangs off |
| `rawkit-decode` | RAW file → sensor mosaic. The one crate allowed to link CDDL code, and the boundary that keeps it contained |
| `rawkit-engine` | `EditState` → pixels. Pipeline stage order, WGSL kernels, wgpu device |
| `rawkit-catalog` | SQLite schema, forward-only migrations, volume identity |
| `rawkit-export` | Pixels to a colour-managed file. The only crate that knows about image formats |
| `rawkit-session` | The command bus. An editing session as a pure state machine: decides which tiles need rendering, and holds no pixels so it cannot send any |
| `rawkit-shell` | The desktop shell — window, surface, event loop. The only crate that knows Tauri exists |
| `rawkit-cli` | Headless entry point for CI, the golden harness and scripting |

**The v2 AI is deliberately not in this workspace.** It consumes `EditState`
through the public API of `rawkit-editstate` and never forks the editor. That
boundary is reserved now — the proprietary split has to be a real interface from
the start rather than a later disentangling.

## Build

```sh
cargo test --workspace                     # unit tests, no GPU required
cargo test -p rawkit-engine -- --ignored   # GPU-backed tests, needs an adapter
cargo run -p rawkit-cli -- stages          # print the render pipeline
cargo run -p rawkit-cli -- gpu             # which backend would render here
cargo run -p rawkit-cli -- schema          # EditState JSON Schema
cargo deny check                           # licence audit (cargo install cargo-deny)

# A RAW file to a colour-managed image, end to end. Format comes from the
# extension: .jpg, .png, or .ppm for an unmanaged look at intermediate results.
cargo run --release -p rawkit-cli -- render photo.ARW -o out.jpg
cargo run --release -p rawkit-cli -- render photo.ARW -o out.jpg --profile camera.dcp
```

The decode tests want a RAW file, which is large and not redistributable, so
they read from `~/rawkit-fixtures` (or `$RAWKIT_FIXTURES`) and fail loudly when
it is empty rather than passing quietly. CI does not run them; it runs the
golden tests instead, which generate their input from a formula.

The toolchain is pinned in `rust-toolchain.toml` so all three CI runners compile
with the identical compiler.

Contributing — human or AI — starts at [AGENTS.md](AGENTS.md): the invariants
that are not up for negotiation, the dependency rule, and what to run before
calling a change done.

## Licence

Apache-2.0 — see `LICENSE` and `NOTICE`. Dependency licences are enforced in CI
by `cargo-deny`; the allowed list and its reasoning are in
[docs/licence-policy.md](docs/licence-policy.md).
