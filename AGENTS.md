# Working in this repo

Rules for AI agents (and anyone else) contributing to rawkit. Read this before
editing. `CLAUDE.md` points here; there is one copy of these rules, and this is
it.

## Commits

- **No AI attribution.** No `Co-Authored-By` for an assistant, no "generated
  with" footer, no session links. The author of a commit is the person who
  decided to make it.
- Subject line in the imperative, under ~72 characters.
- The body explains **why**, not what — the diff already says what.
- Commit straight to `main` and push. This is a solo repo; branches and pull
  requests are ceremony here, not safety. CI runs on every push to `main` and is
  what catches a bad change.

## Invariants that are not yours to break

Each of these is load-bearing. If a change seems to require breaking one, stop
and say so rather than working around it.

1. **One engine, three platforms.** Same RAW + same `EditState` → same pixels on
   Linux, Windows and macOS. There is no per-platform render path — not even a
   faster one on a platform that offers a nicer API. A look that differs by
   operating system is not a look.
2. **`EditState` is ours.** Its fields are defined by what the renderer does,
   never by another application's serialisation format. Importers translate
   *into* it at the boundary and may be lossy; nothing downstream should be able
   to tell where a state came from. `deny_unknown_fields` stays on.
3. **A field nothing renders does not get added.** `EditState` grows as the
   renderer learns to honour parameters. An unrendered field is a lie every
   other crate has to keep.
4. **CDDL stays inside `rawkit-decode`.** LibRaw's licence is file-level
   copyleft; those files can never be relicensed. No other crate links them.
5. **Scene-linear before the tone map, display-referred after.** `Stage` and
   `Domain` in `rawkit-engine` encode this and tests assert it. Adding a stage
   means deciding its domain deliberately, not making the test pass.
6. **Migrations are forward-only** and the runner keeps working on catalogs it
   did not create. A down-migration on a photo library destroys data the user
   cannot get back; the rollback story is a restored backup.
7. **Never write to a user's original files by default.** Edits live in the
   catalog. This one is release-blocking, not stylistic.
8. **v1 makes no network calls**, ships no telemetry, and contains no AI. Do not
   add a dependency that phones home, and do not add an AI crate to this
   workspace — v2 consumes `EditState` through the public API from outside it.
9. **No pixels cross the command bus.** The UI sends intent and subscribes to
   state; the engine owns the canvas and renders into a surface. Routing frames
   through the webview works on a small preview and fails at full resolution, so
   the failure arrives late and looks like "the app got slow". `rawkit-session`
   holds no pixel type and no GPU handle — if you find yourself wanting to add
   one, that is the invariant, not an obstacle to it.

## Platform code

**Priority: this file has broken CI four times, all the same way.**

`rawkit-shell` is the only crate with `#[cfg(target_os = ...)]` in it, and that
is deliberate — the engine must stay portable. But a `cfg` block that *uses*
something means the platforms without that block see dead code, and `-D warnings`
turns dead code into a build failure on machines you are not developing on.

So: **every `cfg`-gated piece of behaviour gets a counterpart for the other
platforms**, even if the counterpart only prints what is missing and returns
`Ok`. Never a bare `let _ = (...)` to silence it, and never `allow(dead_code)` —
both hide the fact that a platform cannot do the thing, which is exactly what
you want to know.

**A helper that only the platform file calls is the same bug wearing a hat.**
That is failure number five: two small accessors in `main.rs`, written for
`canvas.rs` to call, were dead code on macOS and Windows and stopped the build
there. A `pub(crate) fn` is not exempt from `dead_code` just because it looks
like infrastructure. So either the shared code uses it too, or it should not
exist — the accessors were deleted and the platform file now touches the statics
directly, which is one fewer thing that can be unused.

The engine has never broken the matrix. Keep the platform knowledge here.

## Tests must not assume the host filesystem

Three CI failures, all this shape, none of them reproducible on the dev box:

- a scan test needed the machine to have a **filesystem UUID**, and the runners
  do not have one;
- a culling test derived capture times from the order `read_dir` returned
  entries, which is filename order on ext4 and something else elsewhere.

A test that depends on the filesystem passes locally and fails on the two
platforms you cannot see. So: **derive test data from names and arguments, never
from enumeration order, mount identity, timestamps or case-folding behaviour.**
Where the real code genuinely depends on one of those, take it as a parameter —
`PathConvention` and `scan_on`'s `VolumeId` are both that pattern — so the
behaviour can be exercised on any machine rather than only on the one that has it.

## Dependencies

Every dependency is declared in the root `Cargo.toml` `[workspace.dependencies]`
so licence auditing has one place to look. Adding one anywhere else is a review
smell.

Before adding any dependency, check it against `docs/licence-policy.md` and run
`cargo deny check`. **If the crate vendors or links C/C++, also read the licence
of what it vendors** — `cargo-deny` reads crate metadata and never opens the
vendored directory, so a permissively-declared crate can carry anything at all. GPL-2.0-only is categorically excluded (incompatible with
our Apache-2.0). LGPL is avoided. The allow-list in `deny.toml` is what actually
runs; widening it is a deliberate, reviewable act, never a fix for a red build.

## Code

- **No stubs.** No `todo!()`, no `unimplemented!()`, no TODO comment standing in
  for core functionality, no placeholder that returns a plausible fake. If it is
  too early to build something, define the type or write nothing — do not write
  a function that lies about working.
- **Tests assert invariants, not implementation details.** The valuable tests in
  this repo check things like "there is exactly one tone map" and "a catalog
  from the future is refused". Aim there.
- **Comments explain why.** The code says what it does; a comment earns its place
  by explaining a decision, a trade-off, or a trap. Delete comments that restate
  the line below them.
- Match the surrounding style. British spelling in prose and comments
  (`colour`, `serialisation`); identifiers follow Rust convention.

## Before calling anything done

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo deny check          # when dependencies changed
```

CI runs all of these on Linux, Windows and macOS. A change that only passes on
the dev box is not finished — the whole point of the matrix is that the dev box
is the least interesting of the three.

## Do not commit

- RAW files or other large binaries (`.gitignore` covers the common extensions).
  Golden fixtures resolve from a local directory outside the repo.
- `THIRD-PARTY.md` — it is generated by `cargo about`, and a stale attribution
  file is worse than none.
- Anything under `golden/out/`.

## When the plan is unclear

The design rationale for this project lives outside the repo and is not
public. If a change turns on a decision that is not written down here or in
`docs/`, ask rather than inferring it from the code — the code is downstream of
the reasoning, not a substitute for it.
