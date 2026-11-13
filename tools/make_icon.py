#!/usr/bin/env python3
"""Draws the DualMix icon and writes DualMix/Assets/dualmix.ico (multi-size, PNG entries).

The icon is a volume dial seen from above: a ring open at the bottom, its left half in the Game colour (orange),
its right half in the Chat colour (blue), and a white pointer at the top where the two meet: the balanced mix.
Every size is drawn on its own (not scaled down from the large one) so the small ones stay crisp in the
notification area.

    python3 tools/make_icon.py [--preview preview.png]

Needs Pillow (Debian: python3-pil).
"""
import argparse
import io
import math
import struct
from pathlib import Path

from PIL import Image, ImageDraw

SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
SS = 8  # supersampling factor

BG_TOP, BG_BOTTOM = (34, 40, 58), (13, 16, 24)
GAME_A, GAME_B = (255, 160, 64), (255, 84, 54)     # orange, top to bottom
CHAT_A, CHAT_B = (64, 190, 255), (86, 110, 255)    # blue, top to bottom
POINTER = (255, 255, 255)


def vertical_gradient(n, top, bottom):
    img = Image.new("RGB", (1, n))
    px = img.load()
    for y in range(n):
        t = y / max(1, n - 1)
        px[0, y] = tuple(round(a + (b - a) * t) for a, b in zip(top, bottom))
    return img.resize((n, n))


def arc_mask(n, cx, cy, r, width, start, end):
    """Ring segment with round ends. Angles in degrees, 0 = 3 o'clock, clockwise."""
    mask = Image.new("L", (n, n), 0)
    d = ImageDraw.Draw(mask)
    d.arc((cx - r, cy - r, cx + r, cy + r), start, end, fill=255, width=round(width))
    mid = r - width / 2
    for angle in (start, end):
        x = cx + mid * math.cos(math.radians(angle))
        y = cy + mid * math.sin(math.radians(angle))
        d.ellipse((x - width / 2, y - width / 2, x + width / 2, y + width / 2), fill=255)
    return mask


def draw(size):
    n = size * SS
    small = size <= 24
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))

    # background: rounded square, dark, lighter at the top
    pad = 0 if small else n * 0.03
    radius = (n - 2 * pad) * 0.23
    bg_mask = Image.new("L", (n, n), 0)
    ImageDraw.Draw(bg_mask).rounded_rectangle((pad, pad, n - 1 - pad, n - 1 - pad), radius=radius, fill=255)
    img.paste(vertical_gradient(n, BG_TOP, BG_BOTTOM), (0, 0), bg_mask)
    if not small:
        # hairline highlight on the edge
        edge = Image.new("L", (n, n), 0)
        ImageDraw.Draw(edge).rounded_rectangle((pad, pad, n - 1 - pad, n - 1 - pad), radius=radius, outline=255,
                                               width=max(1, round(n * 0.012)))
        img.paste(Image.new("RGBA", (n, n), (255, 255, 255, 38)), (0, 0), edge)

    # dial
    cx, cy = n / 2, n * 0.53
    r = n * (0.345 if small else 0.33)
    width = n * (0.15 if small else 0.118)
    gap = 12 if small else 9          # degrees left free on each side of the pointer
    img.paste(vertical_gradient(n, GAME_A, GAME_B), (0, 0), arc_mask(n, cx, cy, r, width, 135, 270 - gap))
    img.paste(vertical_gradient(n, CHAT_A, CHAT_B), (0, 0), arc_mask(n, cx, cy, r, width, 270 + gap, 405))

    # pointer: from near the centre up through the ring
    d = ImageDraw.Draw(img)
    pw = n * (0.105 if small else 0.078)
    top = cy - r - n * (0.02 if small else 0.035)
    bottom = cy - n * (0.06 if small else 0.045)
    d.rounded_rectangle((cx - pw / 2, top, cx + pw / 2, bottom), radius=pw / 2, fill=POINTER)

    return img.resize((size, size), Image.LANCZOS)


def write_ico(path, images):
    blobs = []
    for im in images:
        buf = io.BytesIO()
        im.save(buf, format="PNG", optimize=True)
        blobs.append(buf.getvalue())
    out = bytearray(struct.pack("<HHH", 0, 1, len(images)))
    offset = 6 + 16 * len(images)
    for im, blob in zip(images, blobs):
        dim = 0 if im.width >= 256 else im.width
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
    for blob in blobs:
        out += blob
    Path(path).write_bytes(out)


def main():
    root = Path(__file__).resolve().parent.parent
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=str(root / "DualMix" / "Assets" / "dualmix.ico"))
    ap.add_argument("--preview", help="also write a sheet with every size (for a look before committing)")
    args = ap.parse_args()

    images = [draw(s) for s in SIZES]
    write_ico(args.out, images)
    print("Icon written:", args.out)
    if args.preview:
        sheet = Image.new("RGBA", (900, 300), (240, 240, 240, 255))
        ImageDraw.Draw(sheet).rectangle((450, 0, 900, 300), fill=(32, 32, 32, 255))
        for half in (0, 450):
            sheet.alpha_composite(images[-1], (half + 12, 22))
            x, y = half + 280, 12
            for im in images[:-2]:
                sheet.alpha_composite(im, (x, y))
                y += im.height + 8
            sheet.alpha_composite(images[-2], (half + 316, 150))
        sheet.save(args.preview)
        print("Preview written:", args.preview)


if __name__ == "__main__":
    main()
