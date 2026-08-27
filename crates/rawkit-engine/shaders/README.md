# WGSL kernels

Shared artifact #3: written once, compiled to Vulkan (Linux), DX12 (Windows) and
Metal (macOS) by wgpu. **Never fork a kernel per platform** — the invariant these
shaders exist to hold is *same RAW + same `EditState` → same pixels on all three
OSes*, and a per-platform variant breaks it at the source.

Conventions for kernels landing here:

- One file per pipeline stage, named after the `Stage` variant in
  `src/pipeline.rs` (`demosaic_rcd.wgsl`, `tone_map_sigmoid.wgsl`, …).
- Scene-linear in, scene-linear out, up to and including the tone map. Anything
  that assumes display-referred values must run after it.
- Tile-aware from the first kernel. Preview and export share these kernels and
  differ only in resolution and tiling; a kernel that assumes it sees the whole
  image is a kernel that has to be rewritten for 60fps zoom.
- Attribute ported kernels in the file header. The RCD demosaic comes from
  vkdt (BSD-2-Clause) and that notice travels with it — see `NOTICE`.

The first kernel is the RCD demosaic port, which is the P0 go/no-go spike.
