#!/usr/bin/env python3
"""Generate the Budget Tracker PWA placeholder icons (stdlib-only, no Pillow).

Emits valid RGBA PNGs: a dark-slate (#1f2937) tile with a centered white
"ledger card" mark (three bars, the middle one green) — on-brand and geometric.
These are PLACEHOLDERS; replace with real branded art when available.

Run from this directory:  python3 generate_icons.py
"""
import zlib
import struct

SLATE = (31, 41, 55, 255)      # #1f2937 brand background
WHITE = (255, 255, 255, 255)   # card
GREEN = (21, 128, 61, 255)     # #15803d positive bar
MUTED = (148, 163, 184, 255)   # #94a3b8 neutral bars


def write_png(path, width, height, pixel):
    """pixel(x, y) -> (r, g, b, a). Writes an 8-bit RGBA PNG."""
    raw = bytearray()
    for y in range(height):
        raw.append(0)  # filter type 0 (none) per scanline
        for x in range(width):
            raw += bytes(pixel(x, y))

    def chunk(typ, data):
        return (
            struct.pack(">I", len(data))
            + typ
            + data
            + struct.pack(">I", zlib.crc32(typ + data) & 0xFFFFFFFF)
        )

    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    idat = zlib.compress(bytes(raw), 9)
    with open(path, "wb") as f:
        f.write(sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b""))


def icon(size, rounded):
    """Square app icon. rounded=True gives an alpha-masked rounded tile (for the
    'any' purpose); rounded=False is full-bleed (for 'maskable', the OS masks)."""
    r = 0.18 * size
    cx0, cy0, cx1, cy1 = 0.20 * size, 0.24 * size, 0.80 * size, 0.76 * size
    bh = 0.06 * size
    bars = [
        (0.30, 0.355, 0.62, MUTED),
        (0.30, 0.490, 0.70, GREEN),
        (0.30, 0.625, 0.54, MUTED),
    ]

    def pixel(x, y):
        if rounded:
            ax, ay = min(x, size - 1 - x), min(y, size - 1 - y)
            if ax < r and ay < r:
                dx, dy = r - ax, r - ay
                if dx * dx + dy * dy > r * r:
                    return (0, 0, 0, 0)
        if cx0 <= x <= cx1 and cy0 <= y <= cy1:
            for bx0f, byf, bx1f, col in bars:
                if (bx0f * size) <= x <= (bx1f * size) and (byf * size) <= y <= (byf * size + bh):
                    return col
            return WHITE
        return SLATE

    return pixel


def screenshot(width, height):
    """Portrait splash: slate field with the centered card mark."""
    s = min(width, height)
    ox, oy = (width - s) / 2.0, (height - s) / 2.0
    inner = icon(int(s), rounded=False)

    def pixel(x, y):
        ix, iy = x - ox, y - oy
        if 0 <= ix < s and 0 <= iy < s:
            return inner(int(ix), int(iy))
        return SLATE

    return pixel


if __name__ == "__main__":
    write_png("icon-192x192.png", 192, 192, icon(192, rounded=True))
    write_png("icon-512x512.png", 512, 512, icon(512, rounded=True))
    write_png("maskable-icon-192x192.png", 192, 192, icon(192, rounded=False))
    write_png("maskable-icon-512x512.png", 512, 512, icon(512, rounded=False))
    write_png("favicon-32x32.png", 32, 32, icon(32, rounded=True))
    write_png("screenshot-540x720.png", 540, 720, screenshot(540, 720))
    print("Generated PWA icons + screenshot.")
