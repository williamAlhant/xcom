#!/usr/bin/env python3
"""Pixel-level assertions for a PPM frame dump from `xcom --dump-ppm`.

Frame counts prove plumbing; this proves the picture. The M0/m0 test pattern
(white box on black) is ideal for it: any garbage or stale content shows up as
colors that cannot exist in the pattern.

Usage: check-ppm.py <file.ppm> [box]
"""
import sys


def fail(msg: str) -> None:
    print(f"check-ppm: FAIL - {msg}")
    sys.exit(1)


def main() -> None:
    if len(sys.argv) < 2:
        fail("usage: check-ppm.py <file.ppm> [box]")
    path = sys.argv[1]
    mode = sys.argv[2] if len(sys.argv) > 2 else "box"

    data = open(path, "rb").read()
    if not data.startswith(b"P6"):
        fail("not a binary P6 PPM")
    header = data.split(b"\n", 3)
    try:
        w, h = map(int, header[1].split())
    except Exception:
        fail("bad PPM header")
    pixels = header[3]
    total = w * h
    if len(pixels) < total * 3:
        fail(f"truncated image: {len(pixels)} bytes for {w}x{h}")

    colors = {}
    for i in range(0, total * 3, 3):
        c = pixels[i : i + 3]
        colors[c] = colors.get(c, 0) + 1

    black = colors.get(b"\x00\x00\x00", 0)
    white = colors.get(b"\xff\xff\xff", 0)
    print(
        f"check-ppm: {w}x{h}, {len(colors)} distinct colors, "
        f"black={black} ({100 * black / total:.1f}%), white={white} ({100 * white / total:.1f}%)"
    )

    if mode == "box":
        # The pattern is a pure white box on pure black. Anything else is
        # garbage (uninitialized pixmap memory, stale buffers, misaligned blits).
        bad = [c for c in colors if c not in (b"\x00\x00\x00", b"\xff\xff\xff")]
        if bad:
            fail(f"colors outside the pattern (garbage content): {bad[:5]}")
        if black < total * 0.3:
            fail(f"too little black: {black}/{total} (box should cover a small area)")
        if not (100 <= white <= total * 0.5):
            fail(f"white area out of range: {white}/{total} (expected one 120x120 box)")
    elif mode == "content":
        if len(colors) < 2:
            fail("image is a single color (no real content)")
    elif mode == "xterm":
        # A terminal with text: white background with black glyph rows. Empty
        # or wiped content shows as an all-white window.
        glyph_rows = 0
        for y in range(h):
            row = y * w * 3
            n = sum(
                1 for x in range(w) if pixels[row + x * 3 : row + x * 3 + 3] == b"\x00\x00\x00"
            )
            if n >= 3:
                glyph_rows += 1
        if glyph_rows < 3:
            fail(f"no text rows found ({glyph_rows}) - window content is empty/wiped")
        print(f"check-ppm: text rows={glyph_rows}")
    else:
        fail(f"unknown mode {mode!r}")

    print("check-ppm: OK")


if __name__ == "__main__":
    main()
