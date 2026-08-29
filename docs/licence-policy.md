# Licence policy

rawkit ships under **Apache-2.0**. This document is the standing answer to "may
we depend on this?", and the audit trail for the answers already given.

Enforcement is automated: `cargo deny check` runs in CI on every push
(`deny.toml`, `.github/workflows/ci.yml`). That job existing before the
dependencies do is the point — over an 18–24 month build, an incompatible
transitive dependency slipping in unnoticed is far likelier than a bad
deliberate choice, and a one-off manual audit goes stale the week after it is
done.

## Why Apache-2.0 outbound

The editor is free and open; the v2 AI is intended to stay proprietary. Copyleft
outbound would rebuild against our own AI exactly the wall that made darktable
and RawTherapee unusable as a base in the first place — and, as those projects
demonstrate with 365+ and 191 copyright holders and no CLA, accepting a single
outside contribution under it makes that permanent.

Accepted cost: someone may fork rawkit and sell it.

## The rule

**Build v1 as if selling it.** v1 is free and unmonetized, but v2/v3 may not be,
and a copyleft dependency taken now cannot be unwound after two years of code
sits on top of it. "It's fine, we're free software" is not a valid reason to
accept an LGPL dependency here.

## Allowed

Apache-2.0 · Apache-2.0 WITH LLVM-exception · MIT · MIT-0 · BSD-2-Clause ·
BSD-3-Clause · ISC · Zlib · BSL-1.0 · CC0-1.0 · Unicode-3.0 · Unlicense · IJG ·
MPL-2.0

### The two entries that were added rather than chosen at the start

**IJG**, added 2026-08-29. `jpeg-encoder`'s forward DCT descends from the
Independent JPEG Group's code, so the crate is `(MIT OR Apache-2.0) AND IJG` and
`cargo deny` refused it — which is the process working rather than a nuisance.

It is permissive, FSF-recognised and Apache-2.0-compatible. Its one real
obligation is an acknowledgement in the documentation, in the IJG's own words:
*"this software is based in part on the work of the Independent JPEG Group"*.
`NOTICE` carries exactly that sentence, which is the whole cost of the entry.

**MPL-2.0**, added 2026-08-29. Tauri pulls five MPL crates for CSP parsing —
`cssparser`, `cssparser-macros`, `selectors`, `dtoa-short`, `option-ext` — and
`cargo deny` refused the build for the second time.

MPL-2.0 is *file-level* copyleft, the same shape as LibRaw's CDDL: those files
stay MPL and modifications to them stay MPL, but §3.3 explicitly permits
distributing the Larger Work under other terms. Apache-2.0 outbound is
unaffected and the v2 AI can still be proprietary.

**The reason this is allowed where LGPL is excluded is not a softer view of
copyleft — it is the relinking requirement.** LGPL §6 would oblige us to ship
relinkable object files with every release because Rust links statically. MPL
imposes nothing of the sort; its duty is to make the source of *those files*
available, and they are unmodified upstream crates, so crates.io is that.
`cargo-about` records them in `THIRD-PARTY.md`.

Unlike CDDL, MPL is **not** quarantined to one crate. It could be — the crates
are all transitive under `rawkit-shell` — but there is nothing to contain: the
obligation travels with the files, not with what links them, and no rawkit
source is derived from them.

The authoritative copy of this list is `deny.toml`; if the two ever disagree,
`deny.toml` is what actually runs.

## Advisories: vulnerabilities always, unmaintained only where we can act

Adding Tauri brought **sixteen advisories at once and not one vulnerability**:
the whole gtk-rs GTK3 binding family (GTK3 is what webkit2gtk requires, and
gtk-rs has moved on to GTK4), `proc-macro-error`, and the `unic-*` crates under
`urlpattern`. None has a safe upgrade and none is ours to fix.

That left two honest options — relax the rule, or paste sixteen IDs into an
ignore list that rots the moment Tauri changes a dependency. `deny.toml` now
sets `unmaintained = "workspace"`: unmaintained notices fail for crates this
workspace depends on **directly**, where we could actually do something, and not
for transitive ones. Vulnerabilities still fail for every crate in the graph,
and `yanked = "deny"` is untouched.

The cost is worth naming rather than burying: an unmaintained transitive crate
no longer announces itself, so a vulnerability in one is now the only thing that
will.

## Excluded, and why

| Licence | Status | Reason |
|---|---|---|
| GPL-2.0-only | **Categorically excluded** | Legally incompatible with Apache-2.0. Not a judgement call |
| GPL-3.0 / AGPL | Excluded | Would make the whole app copyleft; the FSF linking doctrine is enforced in practice (Stockfish v. ChessBase) |
| LGPL-2.1 / LGPL-3.0 | Excluded in practice | LGPL's escape hatch is dynamic linking; Rust statically links, so §6 would oblige us to ship relinkable object files with every release. This rules out the entire pure-Rust decoder family and `lensfun` — see the audit below, where both are settled |
| CDDL-1.0 | **Quarantined, not banned** | LibRaw. File-level copyleft: those files stay CDDL permanently and can never be relicensed. Contained inside `rawkit-decode`, which is why that crate is a hard boundary rather than a tidy one |

## Component decisions already made

| Component | Licence | Verdict |
|---|---|---|
| LibRaw (CDDL mode) | CDDL-1.0 | ✅ Use, quarantined in `rawkit-decode`. ~1,284 cameras; Adobe DNG Converter is the documented fallback for newer bodies |
| vkdt RCD demosaic kernel | BSD-2-Clause | ✅ **Ported 2026-08-28.** Verified per-file, not per-repo: vkdt ships both `LICENCE.bsd2` and `LICENCE.gpl3`, and its readme says the code is BSD-2 *"if not clearly marked otherwise in the respective source files, which contain a bit of viral GPLv3"*. `rcd_conv.comp` and `rcd_fill.comp` carry no marking, which is what makes the port BSD-2. Attribution and full licence text in `NOTICE`; provenance in the shader header |
| lcms2 | MIT | ✅ ICC handling on export |
| OpenColorIO | BSD-3-Clause | ✅ LUT / look handling |
| Tauri | MIT / Apache-2.0 | ✅ Desktop shell |
| wgpu | MIT / Apache-2.0 | ✅ GPU abstraction |
| Little CMS (lcms2) | MIT | ✅ **In use since 2026-08-29.** Statically linked via `lcms2-sys`, which vendors Little CMS 2.19. Vendored source checked by hand per the rule above: MIT, copyright Marti Maria Saguer, matching the crate's declaration |
| jpeg-encoder | (MIT OR Apache-2.0) AND IJG | ✅ Pure Rust. See the IJG note above |
| png | MIT / Apache-2.0 | ✅ Pure Rust, used for output and for golden references |
| RapidRAW | AGPL | ❌ Reference only. Read it for architecture, never copy code |
| darktable / RawTherapee | GPL | ❌ Ruled out as an engine base; the reason this project builds its own |

## ⚠️ What `cargo-deny` cannot see: vendored source

**It checks crate metadata, not what a crate vendors.**

The live case is `libraw-rs-sys`. It declares `MIT/Apache-2.0`, which is true of
the binding, and ships 2.4 MB of LibRaw C++ inside the crate. `cargo deny check`
passes it without comment, because nothing in the tool inspects the `libraw/`
directory. LibRaw is triple-licensed and we elect CDDL, so the answer happens to
be fine — but the tool did not tell us that, and would not have told us if the
answer were GPL.

The rule that follows, and it is a real one:

> **Any dependency that vendors or links C/C++ gets a manual licence check on
> the vendored source, every time it is added or its version is bumped.**
> `cargo deny` passing is necessary and not sufficient.

For LibRaw specifically, that check found: `LICENSE.CDDL` and `LICENSE.LGPL`
alongside the sources (dual, we elect CDDL); LibRaw's optional GPL demosaic
packs present in the tree as `internal/demosaic_packs.cpp` but **not** in the
build's file list, and unwanted anyway since the engine has its own demosaic.

## Resolved — the pre-code audit (2026-08-28)

All three questions that were blocking are answered. Nothing structural changed:
the stack stays LibRaw-CDDL, and the fallback that avoids the other two turns out
to be the better option on its merits, not just the licence-safe one.

### 1. `rawler` — cannot replace LibRaw. **LGPL-2.1.**

And so is every alternative, which is the more useful finding:

| Crate | Version checked | Licence |
|---|---|---|
| `rawler` | 0.7.2 | LGPL-2.1 |
| `rawloader` | 0.37.2 | LGPL-2.1 |
| `quickraw` | 0.2.1-alpha.1 | LGPL-2.1 |

**There is no permissive pure-Rust RAW decoder.** They share dcraw ancestry and
they share its licence posture. Treat this as settled rather than as something to
re-check every six months — the answer will not change until someone writes a decoder
from scratch, and nobody is going to.

Why LGPL is disqualifying *here* specifically, when many projects live with it:
LGPL's escape hatch is dynamic linking, and Rust statically links. LGPL-2.1 §6
then requires shipping relinkable object files (or the source) so a user can
substitute a modified library — a permanent per-release obligation, and one that
sits badly with App Store distribution. Taking that on to avoid CDDL would be
trading a contained obligation for a diffuse one.

**Consequence:** LibRaw stays, and `rawkit-decode` stays a hard boundary.

### 2. LibRaw — confirmed, and CDDL is the right mode

Triple-licensed: LGPL-2.1, CDDL-1.0, or a legacy commercial licence that is being
phased out. **CDDL explicitly permits static linking and source inclusion without
disclosing the application's source.** Attribution is mandatory under all three
and needs no signed agreement.

The obligation is file-level: modifications to LibRaw's own files stay CDDL
forever. So prefer wrapping over patching, and if a patch is unavoidable, keep it
in a clearly marked file inside `rawkit-decode` rather than folding it into our
code.

### 3. `lensfun` — avoid, and there is a second reason we had not recorded

- Library: **LGPL-3.0**. Bundled applications: GPL-3.0.
- **The database is CC-BY-SA-3.0.**

That second line is the one that matters. The tempting move — ignore the library,
use the profile database — does not dodge anything: share-alike attaches to the
data, so derivative databases inherit it. Avoiding lensfun means avoiding both
halves.

### 4. Adobe `.lcp` — never redistribute

Adobe's profile library ships with the DNG Converter and Camera Raw under Adobe's
EULA, which grants no redistribution right; lensfun's own converter documentation
tells users to mind that agreement rather than assuming it is free. Third-party
vendor profiles are stricter still — Sigma's terms forbid duplicating or
exporting a profile at all.

*Reading* an `.lcp` already installed on the user's own machine is a different act
from redistributing one, and remains a possible convenience later. It is not
needed for v1, and the parser would have to be clean-room: the format is
reverse-engineered and RawTherapee's implementation is GPL.

### 5. The fallback, and why it is an upgrade for Sony

Sony embeds correction data in maker note tag **`0x9405`**:

| Parameter | Format |
|---|---|
| `DistortionCorrParams` | 16-bit signed ints; first value is the count of valid coefficients. Spline knots, not polynomial coefficients |
| `VignettingCorrParams` | Vignetting at 16 equi-spaced knots, frame centre → edge |
| `ChromaticAberrationCorrParams` | Two 16-value curves, red and blue, fine-tuning the green distortion spline |

Independent analysis reports these embedded coefficients give **better CA
correction than community-sourced lensfun profiles** for the same lens. So for the
target bodies this is not a licence-driven compromise.

Honest caveats, all of which the spike should confirm against a real file:

- The format is reverse-engineered and the published descriptions are explicitly
  "best guesses"; the reconstruction does not exactly match Sony's in-camera JPEG.
- Coefficients vary with focus distance, so they cannot be cached as one static
  profile per lens.
- APS-C crop mode uses fewer coefficients than full frame.
- Adapted and fully manual lenses carry no data at all — no correction, and the
  UI should say so rather than silently doing nothing.
- LibRaw's `libraw_lensinfo_t` / `libraw_makernotes_lens_t` cover lens
  *identification*, not these splines. Plan on parsing `0x9405` ourselves via
  LibRaw's makernotes callback. Clean-room from the published descriptions —
  the same approach already planned for dehaze and clarity.

**Net effect on non-Sony bodies:** lens correction exists where the manufacturer
embedded it and not otherwise. That is a documented limitation for the public
release, in the same breath as the camera-support fallback, not a defect.

## Still open (not blocking v1)

- [ ] Training-data licensing for v2 — FiveK and academic weights are
      research-licensed. Blocking for v2, not for the editor.
- [ ] SAM 3's custom Meta licence was assessed as server-side-OK; re-check before
      shipping it **on-device** in v2. Apache-licensed fallback path exists.

## Attribution

Apache-2.0 requires attribution for what we redistribute. `NOTICE` carries the
project-level notice; `THIRD-PARTY.md` is generated by
`cargo about generate about.hbs > THIRD-PARTY.md` and regenerated when preparing
a release. It is not committed — a stale attribution file is worse than none.
