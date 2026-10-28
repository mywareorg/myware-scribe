#!/usr/bin/env python3
"""Render the Microsoft Store image assets from the source artwork.

The artwork is a round badge on a flat grey field. Square assets keep the
badge and drop the field so the icon sits on the Store's own surface and on
the Windows tile colour; wide and splash assets fill their rectangle, so they
composite onto the artwork's own background colour instead.

Usage:
    python3 scripts/make-store-assets.py
    python3 scripts/make-store-assets.py --source icons/mic-hippo-final.png
"""

import argparse
from pathlib import Path

from PIL import Image, ImageDraw

# The badge's outer ring starts this many pixels from the edge of the 827 px
# source. Scaled to keep the same margin at any size.
SOURCE_MARGIN = 24
SOURCE_SIZE = 827
SUPERSAMPLE = 4


def disc(source: Image.Image) -> Image.Image:
    """Keep the badge, make everything outside its outer ring transparent."""
    size = source.size[0]
    box = Image.new("L", (size * SUPERSAMPLE, size * SUPERSAMPLE), 0)
    margin = round(SOURCE_MARGIN * size / SOURCE_SIZE) * SUPERSAMPLE
    ImageDraw.Draw(box).ellipse(
        [margin, margin, size * SUPERSAMPLE - margin, size * SUPERSAMPLE - margin],
        fill=255,
    )
    out = source.copy()
    out.putalpha(box.resize(source.size, Image.LANCZOS))
    return out


def save(image: Image.Image, width: int, height: int, path: Path, fill) -> None:
    """Centre `image` on a width x height canvas of `fill` and write it."""
    canvas = Image.new("RGBA", (width, height), fill)
    scaled = image.copy()
    scaled.thumbnail((width, height), Image.LANCZOS)
    canvas.alpha_composite(
        scaled, ((width - scaled.width) // 2, (height - scaled.height) // 2)
    )
    canvas.save(path)
    print(f"{path.name}  {width}x{height}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", default="icons/mic-hippo-final.png")
    parser.add_argument("--out", default="packaging/assets")
    args = parser.parse_args()

    source = Image.open(args.source).convert("RGBA")
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    background = source.getpixel((2, 2))
    transparent = (0, 0, 0, 0)
    badge = disc(source)

    # Store listing logo and the Start menu / taskbar / tile assets. These are
    # transparent: Windows and the Store supply their own backdrop.
    save(badge, 300, 300, out / "StoreLogo.png", transparent)
    save(badge, 44, 44, out / "Square44x44Logo.png", transparent)
    save(badge, 88, 88, out / "Square44x44Logo.scale-200.png", transparent)
    save(badge, 176, 176, out / "Square44x44Logo.scale-400.png", transparent)
    save(badge, 150, 150, out / "Square150x150Logo.png", transparent)
    save(badge, 300, 300, out / "Square150x150Logo.scale-200.png", transparent)
    save(badge, 600, 600, out / "Square150x150Logo.scale-400.png", transparent)

    # Unplated variants: Windows draws these without a tile behind them, which
    # is why they must be the bare badge at each exact target size.
    for px in (16, 24, 32, 48, 256):
        save(
            badge,
            px,
            px,
            out / f"Square44x44Logo.targetsize-{px}_altform-unplated.png",
            transparent,
        )

    # Wide tile and splash fill their rectangle, so they keep the artwork's
    # background colour rather than showing a transparent border.
    save(badge, 310, 150, out / "Wide310x150Logo.png", background)
    save(badge, 620, 300, out / "Wide310x150Logo.scale-200.png", background)
    save(badge, 620, 300, out / "SplashScreen.png", background)
    save(badge, 1240, 600, out / "SplashScreen.scale-200.png", background)


if __name__ == "__main__":
    main()
