"""Rasterize the Branvio app icon to PNG without third-party dependencies.

The geometry mirrors assets/brand/branvio-icon.svg (256x256 design grid). Keep both in sync.
Usage: python3 scripts/render_brand_png.py [size ...]   (default: 256)
Writes assets/brand/branvio-icon-<size>.png and packaging/linux/branvio.png (256 px).
Other scripts (scripts/build_app_icons.py) import `render` and `encode_png`.
"""
import math
import pathlib
import struct
import sys
import zlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
BG = (0x16, 0x1C, 0x24)
TRUNK = (0x38, 0xBD, 0xF8)
BRANCH = (0xA8, 0x55, 0xF7)
STROKE = 11.0  # half of the 22 px stroke

# Bowl: right half of an ellipse centred (96, 158), rx 58, ry 42, as a polyline.
BOWL_CX, BOWL_CY, BOWL_RX, BOWL_RY = 96.0, 158.0, 58.0, 42.0
BOWL = [
    (BOWL_CX + BOWL_RX * math.sin(a), BOWL_CY - BOWL_RY * math.cos(a))
    for a in [i * math.pi / 96 for i in range(97)]
]
BOWL_SEGS = list(zip(BOWL, BOWL[1:]))


def seg_dist(px, py, ax, ay, bx, by):
    dx, dy = bx - ax, by - ay
    t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
    return math.hypot(px - (ax + t * dx), py - (ay + t * dy))


def in_rounded_rect(x, y):
    r = 56.0
    cx = min(max(x, r), 256 - r)
    cy = min(max(y, r), 256 - r)
    return 0 <= x <= 256 and 0 <= y <= 256 and math.hypot(x - cx, y - cy) <= r


def near_bowl(x, y):
    """Cheap pre-filter: is (x, y) inside the band around the bowl ellipse?"""
    if x < 80 or x > 170 or y < 100 or y > 216:
        return False
    outer = ((x - BOWL_CX) / (BOWL_RX + STROKE + 2)) ** 2 + ((y - BOWL_CY) / (BOWL_RY + STROKE + 2)) ** 2
    inner = ((x - BOWL_CX) / (BOWL_RX - STROKE - 2)) ** 2 + ((y - BOWL_CY) / (BOWL_RY - STROKE - 2)) ** 2
    return outer <= 1.0 and inner >= 1.0


def color_at(x, y):
    """Return the RGBA color of the design at design coordinates (x, y)."""
    if not in_rounded_rect(x, y):
        return (0, 0, 0, 0)
    # Painter's order matches the SVG: background, bowl, trunk on top, commit.
    color = BG
    if near_bowl(x, y) and min(seg_dist(x, y, a[0], a[1], b[0], b[1]) for a, b in BOWL_SEGS) <= STROKE:
        color = BRANCH
    if seg_dist(x, y, 96, 52, 96, 200) <= STROKE:
        color = TRUNK
    d = math.hypot(x - 154, y - 158)
    if d <= 17 + 4:
        color = BRANCH if d <= 17 - 4 else BG
    return (*color, 255)


def render(size, padding=0.0, samples=None):
    """Render the icon as raw PNG scanlines (filter byte + RGBA per row).

    `padding` is the transparent margin on each side as a fraction of `size`
    (macOS icons use ~0.0977 so the body is 824/1024 of the canvas).
    """
    if samples is None:
        samples = 4 if size <= 256 else 3 if size <= 512 else 2
    inner = size * (1.0 - 2.0 * padding)
    offset = size * padding
    scale = 256.0 / inner
    rows = bytearray()
    for j in range(size):
        rows.append(0)  # PNG filter: none
        for i in range(size):
            acc = [0, 0, 0, 0]
            for sj in range(samples):
                for si in range(samples):
                    x = (i + (si + 0.5) / samples - offset) * scale
                    y = (j + (sj + 0.5) / samples - offset) * scale
                    r, g, b, a = color_at(x, y)
                    acc[0] += r * a
                    acc[1] += g * a
                    acc[2] += b * a
                    acc[3] += a
            if acc[3]:
                alpha = acc[3] / (samples * samples)
                rows += bytes(
                    (round(acc[0] / acc[3]), round(acc[1] / acc[3]), round(acc[2] / acc[3]), round(alpha))
                )
            else:
                rows += b"\x00\x00\x00\x00"
    return rows


def encode_png(size, raw):
    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def write_png(path, size, raw):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(encode_png(size, raw))
    print(f"wrote {path.relative_to(ROOT)} ({size}x{size})")


if __name__ == "__main__":
    sizes = [int(s) for s in sys.argv[1:]] or [256]
    for size in sizes:
        raw = render(size)
        write_png(ROOT / "assets" / "brand" / f"branvio-icon-{size}.png", size, raw)
        if size == 256:
            write_png(ROOT / "packaging" / "linux" / "branvio.png", size, raw)
