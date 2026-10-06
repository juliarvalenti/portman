#!/usr/bin/env python3
"""Generate Portman's app and tray icons (requires Pillow).

Tray icons are 44×44 (22 pt @2x). The normal one is a black template image
that macOS tints for light/dark menu bars; warn/critical are colored.
Run from anywhere: writes into app/src-tauri/icons/.
"""
from pathlib import Path

from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parent.parent / "src-tauri" / "icons"
OUT.mkdir(parents=True, exist_ok=True)


def glyph(size: int, color, bg=None) -> Image.Image:
    """A port: a ring with a solid center pin."""
    s = size * 4  # supersample for smooth edges
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    if bg:
        r = s * 0.22
        d.rounded_rectangle([0, 0, s - 1, s - 1], radius=r, fill=bg)
    pad = s * (0.2 if bg else 0.1)
    width = int(s * 0.11)
    d.ellipse([pad, pad, s - pad, s - pad], outline=color, width=width)
    c = s / 2
    pin = s * 0.13
    d.ellipse([c - pin, c - pin, c + pin, c + pin], fill=color)
    return img.resize((size, size), Image.LANCZOS)


TRAY = {
    "tray-normal.png": (0, 0, 0, 255),
    "tray-warn.png": (245, 158, 11, 255),
    "tray-critical.png": (239, 68, 68, 255),
}
for name, color in TRAY.items():
    glyph(44, color).save(OUT / name)

APP_BG = (24, 24, 27, 255)
APP_FG = (52, 211, 153, 255)
for name, size in {"32x32.png": 32, "128x128.png": 128, "128x128@2x.png": 256, "icon.png": 512}.items():
    glyph(size, APP_FG, APP_BG).save(OUT / name)
glyph(1024, APP_FG, APP_BG).save(OUT / "icon.icns", sizes=[(16, 16), (32, 32), (128, 128), (256, 256), (512, 512), (1024, 1024)])
print(f"wrote icons to {OUT}")
