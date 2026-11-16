#!/usr/bin/env python3
"""Render the Microsoft Store image assets from the source artwork.

The source is the corrected MyWare monogram: a heavy square-shouldered M,
square-hipped, on a full-width blue baseline that ends in a green terminal,
with equal optical margins on all four sides. App-tile assets put the mark on
a light rounded-square field (22 % corner radius, matching the 16 px radius
the myware-ui panels use at 72 px).

`--source` accepts the SVG mark (rasterized with rsvg-convert) or a PNG. A PNG
with a transparent corner, such as icons/simple-myware-square-icon.png, is
already a finished tile and is used as-is; a PNG on a flat opaque field is
re-masked to the rounded square.

Small icons get a one-pixel dark keyline so the light field does not vanish
on a light panel.

Usage:
    python3 scripts/make-store-assets.py
    python3 scripts/make-store-assets.py --source icons/simple-myware-square-icon.png
"""

import argparse
import shutil
import subprocess
import tempfile
from pathlib import Path

from PIL import Image, ImageColor, ImageDraw, ImageFilter

# The tile the product ships first: light field, matching the light palette.
FIELD = ImageColor.getrgb("#EFF0F3") + (255,)
# 226 px at 1024, matching the myware-ui 16 px radius at 72 px.
RADIUS_RATIO = 226 / 1024
# The raster side the monogram is composed at before any downscaling.
MASTER_SIZE = 1024
SUPERSAMPLE = 4
# At or below this size a one-pixel dark rim keeps a light tile visible on a
# light panel; both repositories draw the same keyline.
KEYLINE_LIMIT = 32
KEYLINE = (24, 32, 40, 255)

PNG_FALLBACK = Path("icons/simple-myware-square-icon.png")


def render_svg(path: Path, size: int) -> Image.Image:
    """Rasterize an SVG with rsvg-convert at exactly `size`."""
    rsvg = shutil.which("rsvg-convert")
    if rsvg is None:
        raise RuntimeError(
            f"{path} is SVG and rsvg-convert is not installed; "
            f"rerun with --source {PNG_FALLBACK}"
        )
    with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as tmp:
        tmp_path = Path(tmp.name)
    try:
        subprocess.run(
            [rsvg, "-w", str(size), "-h", str(size), str(path), "-o", str(tmp_path)],
            check=True,
        )
        return Image.open(tmp_path).convert("RGBA")
    finally:
        tmp_path.unlink(missing_ok=True)


def load_source(arg: str) -> tuple[Image.Image, bool]:
    """Return (image, is_bare_mark).

    A bare mark needs the rounded-square field composed behind it; a PNG tile
    already carries it.
    """
    source = Path(arg)
    if source.suffix.lower() == ".svg":
        return render_svg(source, MASTER_SIZE), True
    image = Image.open(source).convert("RGBA")
    return image, False


def rounded_mask(size: int) -> Image.Image:
    """Supersampled rounded-square alpha mask, radius 22 % of the side."""
    big = size * SUPERSAMPLE
    mask = Image.new("L", (big, big), 0)
    radius = round(big * RADIUS_RATIO)
    ImageDraw.Draw(mask).rounded_rectangle(
        [0, 0, big - 1, big - 1], radius=radius, fill=255
    )
    return mask.resize((size, size), Image.LANCZOS)


def tile(mark: Image.Image, size: int) -> Image.Image:
    """The monogram on its light rounded-square field, side `size`.

    The monogram's own margins are the tile's padding, so the mark scales
    with the field rather than inside it.
    """
    field = Image.new("RGBA", (size, size), FIELD)
    scaled = mark.resize((size, size), Image.LANCZOS)
    field.alpha_composite(scaled)
    return Image.composite(
        field, Image.new("RGBA", (size, size)), rounded_mask(size)
    )


def keyline(icon: Image.Image) -> Image.Image:
    """Give a small icon a one-pixel dark rim.

    The field is #EFF0F3 and the panels it lands on are light too, so the
    tile's own edge is what separates app from backdrop.
    """
    alpha = icon.getchannel("A").point(lambda value: 255 if value > 24 else 0)
    alpha = alpha.filter(ImageFilter.MaxFilter(3))
    rim = Image.new("RGBA", icon.size, KEYLINE)
    rim.putalpha(alpha)
    rim.alpha_composite(icon)
    return rim


def save(image: Image.Image, width: int, height: int, path: Path, fill) -> None:
    """Centre `image` on a width x height canvas of `fill` and write it.

    Images exactly `width` x `height` are written verbatim so exact-size
    assets (the unplated ladder) keep their own rendering.
    """
    canvas = Image.new("RGBA", (width, height), fill)
    scaled = image.copy()
    scaled.thumbnail((width, height), Image.LANCZOS)
    canvas.alpha_composite(
        scaled, ((width - scaled.width) // 2, (height - scaled.height) // 2)
    )
    if min(width, height) <= KEYLINE_LIMIT:
        canvas = keyline(canvas)
    canvas.save(path)
    print(f"{path.name}  {width}x{height}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", default="icons/simple-myware-icon.svg")
    parser.add_argument("--out", default="packaging/assets")
    args = parser.parse_args()

    image, is_mark = load_source(args.source)
    if is_mark:
        badge = tile(image, MASTER_SIZE)
    elif image.getpixel((2, 2))[3] == 0:
        # A transparent-corner PNG is already the finished tile.
        badge = image
    else:
        # Flat-field PNG: re-mask to the rounded square.
        badge = Image.composite(
            image, Image.new("RGBA", image.size), rounded_mask(image.width)
        )

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    transparent = (0, 0, 0, 0)

    # Store listing logo and the Start menu / taskbar / tile assets. These are
    # transparent: Windows and the Store supply their own backdrop. The packaged
    # StoreLogo is 50x50 by schema; WACK rejects any other size.
    save(badge, 50, 50, out / "StoreLogo.png", transparent)
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

    # Wide tile and splash fill their rectangle, so they take the tile's light
    # field colour rather than showing a transparent border.
    save(badge, 310, 150, out / "Wide310x150Logo.png", FIELD)
    save(badge, 620, 300, out / "Wide310x150Logo.scale-200.png", FIELD)
    save(badge, 620, 300, out / "SplashScreen.png", FIELD)
    save(badge, 1240, 600, out / "SplashScreen.scale-200.png", FIELD)


if __name__ == "__main__":
    main()
