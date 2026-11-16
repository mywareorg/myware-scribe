#!/usr/bin/env python3
"""Render the executable and window icons from the source artwork.

`gui/assets/pencil-icon.ico` is embedded into the .exe by gui/build.rs, and
`icons/pencil-icon.png` is the bitmap the window decodes at startup. Both come
from the same tile as the Store assets, so the app looks the same everywhere.
`gui/assets/icon.*` and `logo.png` are still written for older references.

The source is the corrected MyWare monogram (icons/simple-myware-icon.svg),
rasterized with rsvg-convert and set on its light rounded-square tile. The
finished tile is written back to icons/simple-myware-square-icon.png so the
committed raster stays in sync and builds never need rsvg-convert. With a PNG
--source the committed tile is used as-is.

Small icons get a one-pixel dark keyline so the light field does not vanish
on a light panel. Pass --fill to squash the tile's transparent corners into a
solid square field.

Usage:
    python3 scripts/make-app-icons.py
    python3 scripts/make-app-icons.py --source icons/simple-myware-square-icon.png --fill
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

SOURCE_TILE = Path("icons/simple-myware-square-icon.png")


def load_store_assets():
    """Import make-store-assets.py so the tile and keyline live in one place."""
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
    parser.add_argument("--source", default="icons/simple-myware-icon.svg")
    parser.add_argument("--out", default="gui/assets")
    parser.add_argument("--icons-out", default="icons")
    parser.add_argument(
        "--fill",
        action="store_true",
        help="keep the field as a solid square instead of transparent corners",
    )
    args = parser.parse_args()

    store = load_store_assets()
    image, is_mark = store.load_source(args.source)
    if is_mark:
        # The mark has no field of its own; the tile supplies it. The header
        # logo below draws the mark bare, without the tile.
        mark = image
        badge = store.tile(mark, PNG_SIZE)
        # Keep the committed raster the build embeds in sync with the SVG.
        badge.save(SOURCE_TILE)
        print(f"{SOURCE_TILE}  {PNG_SIZE}x{PNG_SIZE}")
    else:
        badge = image
        mark = None

    # Never upsample: the badge is centred in a transparent square of its own
    # size at most, so the mark keeps the same footprint at every scale.
    transparent = (0, 0, 0, 0)
    png_size = min(PNG_SIZE, badge.width)
    fill = store.FIELD if args.fill else transparent

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    icons_out = Path(args.icons_out)
    icons_out.mkdir(parents=True, exist_ok=True)

    png = render(badge, png_size, fill)
    png.save(out / "icon.png")
    print(f"icon.png  {png_size}x{png_size}")

    # Window / title-bar / taskbar bitmap decoded by gui/src/app.rs.
    png.save(icons_out / "pencil-icon.png")
    print(f"pencil-icon.png  {png_size}x{png_size}")
    png.save(out / "pencil-icon.png")

    # The header logo is the bare monogram: it sits on the window's own light
    # surface, so a light tile behind it would read as a pale ghost square.
    logo_source = mark if mark is not None else badge
    logo = render(logo_source, LOGO_SIZE, transparent)
    logo.save(out / "logo.png")
    print(f"logo.png  {LOGO_SIZE}x{LOGO_SIZE}")

    frames = []
    for size in ICO_SIZES:
        frame = render(badge, size, fill)
        frames.append(store.keyline(frame) if size <= store.KEYLINE_LIMIT else frame)
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
