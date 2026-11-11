#!/usr/bin/env python3
"""Render the executable and window icons from the source artwork.

`gui/assets/pencil-icon.ico` is embedded into the .exe by gui/build.rs, and
`icons/pencil-icon.png` is the bitmap the window decodes at startup. They come
from the desktop icon artwork, which is kept separate from the Store tiles that
`make-store-assets.py` renders. `gui/assets/icon.*` and `logo.png` are still
written for older references.

By default the badge keeps transparency outside its ring. Pass --fill to keep
the artwork's flat background instead, which is what a tile-shaped icon looks
like.

Usage:
    python3 scripts/make-app-icons.py
    python3 scripts/make-app-icons.py --source icons/simple-hippo-square-icon.png --fill
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
    parser.add_argument("--source", default="icons/desktop-clipboard.png")
    parser.add_argument("--out", default="gui/assets")
    parser.add_argument("--icons-out", default="icons")
    parser.add_argument(
        "--fill",
        action="store_true",
        help="keep the artwork background instead of a transparent surround",
    )
    args = parser.parse_args()

    source = Image.open(args.source).convert("RGBA")
    corner = source.getpixel((2, 2))
    # Artwork that already has a transparent surround keeps its own silhouette;
    # masking it would cut a second, smaller shape out of the field. Only
    # art with an opaque corner is treated as a badge on a flat background.
    badge = load_store_assets().disc(source) if corner[3] != 0 else source
    fill = corner if args.fill else (0, 0, 0, 0)

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    icons_out = Path(args.icons_out)
    icons_out.mkdir(parents=True, exist_ok=True)

    # Never upsample: the badge is centred in a transparent square of its own
    # size at most, so the mark keeps the same footprint at every scale.
    png_size = min(PNG_SIZE, badge.width)
    png = render(badge, png_size, fill)
    png.save(out / "icon.png")
    print(f"icon.png  {png_size}x{png_size}")

    # Window / title-bar / taskbar bitmap decoded by gui/src/app.rs.
    png.save(icons_out / "pencil-icon.png")
    print(f"pencil-icon.png  {png_size}x{png_size}")

    logo = render(badge, LOGO_SIZE, fill)
    logo.save(out / "logo.png")
    print(f"logo.png  {LOGO_SIZE}x{LOGO_SIZE}")

    frames = [render(badge, size, fill) for size in ICO_SIZES]
    # Pillow uses a supplied frame verbatim when its size matches, so each
    # entry is downsampled from the source rather than from a sibling.
    for name in ("pencil-icon.ico", "icon.ico"):
        frames[-1].save(
            out / name,
            format="ICO",
            sizes=[(size, size) for size in ICO_SIZES],
            append_images=frames[:-1],
        )
        print(f"{name}  {', '.join(str(size) for size in ICO_SIZES)}")


if __name__ == "__main__":
    main()
