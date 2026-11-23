"""Build platform app icons from the Branvio icon geometry.

Usage: python3 scripts/build_app_icons.py
Writes:
  packaging/macos/branvio.icns   (via macOS `iconutil`; padded to Apple's 824/1024 icon grid)
  packaging/windows/branvio.ico  (PNG-compressed entries 16-256 px, Windows Vista and later)
"""
import pathlib
import shutil
import struct
import subprocess
import sys
import tempfile

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from render_brand_png import ROOT, encode_png, render  # noqa: E402

MACOS_PADDING = 100 / 1024  # body is 824 px of a 1024 px canvas
# (file name inside the .iconset, pixel size)
ICONSET = [
    ("icon_16x16.png", 16),
    ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32),
    ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128),
    ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256),
    ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512),
    ("icon_512x512@2x.png", 1024),
]
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]


def build_icns():
    if shutil.which("iconutil") is None:
        sys.exit("iconutil not found: build the .icns on macOS")
    cache = {}
    with tempfile.TemporaryDirectory() as tmp:
        iconset = pathlib.Path(tmp) / "branvio.iconset"
        iconset.mkdir()
        for name, size in ICONSET:
            if size not in cache:
                cache[size] = encode_png(size, render(size, padding=MACOS_PADDING))
            (iconset / name).write_bytes(cache[size])
        out = ROOT / "packaging" / "macos" / "branvio.icns"
        subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(out)], check=True)
    print(f"wrote {out.relative_to(ROOT)}")


def build_ico():
    images = [encode_png(size, render(size)) for size in ICO_SIZES]
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = len(header) + 16 * len(images)
    entries = b""
    for size, data in zip(ICO_SIZES, images):
        dim = 0 if size == 256 else size  # 0 means 256 in the ICO directory
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    out = ROOT / "packaging" / "windows" / "branvio.ico"
    out.write_bytes(header + entries + b"".join(images))
    print(f"wrote {out.relative_to(ROOT)} ({', '.join(map(str, ICO_SIZES))} px)")


if __name__ == "__main__":
    build_ico()
    build_icns()
