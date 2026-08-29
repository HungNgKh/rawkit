#!/usr/bin/env python3
"""Read an X11 window dump and report what is painted where.

The compositing probe's verifier. Screenshots get judged by eye, and eyes are
bad at "is this the same green"; this reads pixel values, which is also what
makes the result reproducible on someone else's machine.

    cargo run -p rawkit-shell &
    xwd -name rawkit -out probe.xwd
    python3 crates/rawkit-shell/probe/check-composite.py probe.xwd

Take several captures a second apart. A single one cannot tell a stable z-order
from two layers taking turns, and taking turns is the actual behaviour on
Linux/WebKitGTK. See the module docs of src/main.rs for the result.

No dependencies, on purpose: XWD is a fixed 100-byte big-endian header followed
by pixels, and a diagnostic that needs a package installed is a diagnostic that
does not get run.
"""

import struct
import sys


def load(path):
    d = open(path, "rb").read()
    f = struct.unpack(">25I", d[:100])
    header_size, depth, width, height = f[0], f[3], f[4], f[5]
    byte_order, bits_per_pixel, bytes_per_line = f[7], f[11], f[12]
    ncolors = f[19]
    return dict(
        w=width,
        h=height,
        bpp=bits_per_pixel,
        bpl=bytes_per_line,
        # Colormap entries sit between the header and the pixels, 12 bytes each.
        off=header_size + ncolors * 12,
        data=d,
        byte_order=byte_order,
        rmask=f[14],
        gmask=f[15],
        bmask=f[16],
        depth=depth,
    )


def pixel(img, x, y):
    i = img["off"] + y * img["bpl"] + x * (img["bpp"] // 8)
    raw = img["data"][i : i + img["bpp"] // 8]
    v = int.from_bytes(raw, "big" if img["byte_order"] else "little")

    def channel(mask):
        if mask == 0:
            return 0
        shift = (mask & -mask).bit_length() - 1
        width = bin(mask >> shift).count("1")
        return ((v & mask) >> shift) * 255 // ((1 << width) - 1)

    return channel(img["rmask"]), channel(img["gmask"]), channel(img["bmask"])


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    img = load(sys.argv[1])
    print(f"  {img['w']}x{img['h']} depth={img['depth']} bpp={img['bpp']}")
    # The page paints the left third opaque red and leaves the rest alone; the
    # GPU clears the whole window green. Sampling three rows per column catches
    # a partial repaint, which a single sample would read as a clean result.
    for label, fx in [
        ("left  (webview panel)", 0.15),
        ("right (cutout)", 0.70),
        ("far right", 0.92),
    ]:
        samples = [pixel(img, int(img["w"] * fx), int(img["h"] * fy)) for fy in (0.3, 0.5, 0.7)]
        print(f"  {label:22} {samples}")


if __name__ == "__main__":
    main()
