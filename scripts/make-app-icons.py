#!/usr/bin/env python3
"""Render the executable and window icons from the source artwork.

`gui/assets/icon.ico` is embedded into the .exe by gui/build.rs, and
`gui/assets/icon.png` is the fallback bitmap. Both come from the same badge as
the Store assets, so the app looks the same everywhere.

By default the badge keeps transparency outside its ring. Pass --fill to keep
the artwork's flat background instead, which is what a tile-shaped icon looks
like.

Usage:
    python3 scripts/make-app-icons.py
    python3 scripts/make-app-icons.py --source icons/mic-hippo-final.png --fill
"""

import argparse
import importlib.util
from pathlib import Path

from PIL import Image

# Sizes Windows asks for: title bar, taskbar, Alt-Tab, Explorer, and the 256 px
# large-icon view. Pillow writes PNG frames, which Vista and later accept.
ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)
PNG_SIZE = 1024
# The in-app header logo. Small enough to embed cheaply in the binary and to
# stay sharp at the 67 px it is drawn at on a 4x display.
LOGO_SIZE = 256


def load_store_assets():
    """Import make-store-assets.py so the badge mask stays in one place."""
    path = Path(__file__).with_name("make-store-assets.py")
    spec = importlib.util.spec_from_file_location("store_assets", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def render(badge: Image.Image, size: int, fill) -> Image.Image:
    canvas = Image.new("RGBA", (size, size), fill)
    scaled = badge.copy()
    scaled.thumbnail((size, size), Image.LANCZOS)
    canvas.alpha_composite(
        scaled, ((size - scaled.width) // 2, (size - scaled.height) // 2)
    )
    return canvas


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", default="icons/mic-hippo-final.png")
    parser.add_argument("--out", default="gui/assets")
    parser.add_argument(
        "--fill",
        action="store_true",
        help="keep the artwork background instead of a transparent surround",
    )
    args = parser.parse_args()

    source = Image.open(args.source).convert("RGBA")
    badge = load_store_assets().disc(source)
    fill = source.getpixel((2, 2)) if args.fill else (0, 0, 0, 0)

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    png = render(badge, PNG_SIZE, fill)
    png.save(out / "icon.png")
    print(f"icon.png  {PNG_SIZE}x{PNG_SIZE}")

    logo = render(badge, LOGO_SIZE, fill)
    logo.save(out / "logo.png")
    print(f"logo.png  {LOGO_SIZE}x{LOGO_SIZE}")

    frames = [render(badge, size, fill) for size in ICO_SIZES]
    # Pillow uses a supplied frame verbatim when its size matches, so each
    # entry is downsampled from the 827 px source rather than from a sibling.
    frames[-1].save(
        out / "icon.ico",
        format="ICO",
        sizes=[(size, size) for size in ICO_SIZES],
        append_images=frames[:-1],
    )
    print(f"icon.ico  {', '.join(str(size) for size in ICO_SIZES)}")


if __name__ == "__main__":
    main()
