#!/usr/bin/env python3
"""Render PRIVACY.md into the static pages that GitLab Pages publishes.

The pages job used to build these with printf/sed inside the pipeline, which
breaks on a shell executor. They are committed instead, so publishing needs
nothing but the artifact. Rerun this after editing PRIVACY.md:

    python3 scripts/make-privacy-page.py
"""

from __future__ import annotations

import html
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "PRIVACY.md"
OUTPUT = ROOT / "public"

PAGE = """<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
  :root {{ color-scheme: light dark; }}
  body {{
    margin: 0 auto;
    padding: 3rem 1.5rem 5rem;
    max-width: 46rem;
    font: 16px/1.6 system-ui, -apple-system, "Segoe UI", sans-serif;
    color: #1b1f23;
    background: #fbfbfa;
  }}
  h1 {{ font-size: 1.9rem; line-height: 1.2; margin: 0 0 1.5rem; }}
  h2 {{ font-size: 1.2rem; margin: 2.25rem 0 0.5rem; }}
  h3 {{ font-size: 1rem; margin: 1.5rem 0 0.35rem; }}
  p, li {{ margin: 0.5rem 0; }}
  ul {{ padding-left: 1.25rem; }}
  code {{ font-size: 0.9em; background: rgba(127, 127, 127, 0.15); padding: 0.1em 0.3em; border-radius: 3px; }}
  footer {{ margin-top: 3rem; font-size: 0.85rem; color: #666; }}
  @media (prefers-color-scheme: dark) {{
    body {{ color: #e6e6e6; background: #16181a; }}
    code {{ background: rgba(255, 255, 255, 0.12); }}
    footer {{ color: #9aa0a6; }}
  }}
</style>
{body}
</html>
"""


def inline(text: str) -> str:
    """Escape, then apply the inline forms the policy actually uses."""
    out = html.escape(text, quote=False)
    out = re.sub(r"`([^`]+)`", r"<code>\1</code>", out)
    # List items open with a bold label; without this the asterisks rendered
    # literally on the published page.
    out = re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", out)
    return out


def convert(markdown: str) -> str:
    lines = markdown.splitlines()
    # The first heading becomes the page title, so drop it from the body.
    title = "Privacy policy"
    for index, line in enumerate(lines):
        if line.startswith("# "):
            title = line[2:].strip()
            lines = lines[index + 1 :]
            break

    blocks: list[str] = []
    paragraph: list[str] = []
    listing: list[str] = []

    def flush_paragraph() -> None:
        if paragraph:
            blocks.append("<p>" + inline(" ".join(paragraph)) + "</p>")
            paragraph.clear()

    def flush_list() -> None:
        if listing:
            items = "".join(f"<li>{inline(item)}</li>" for item in listing)
            blocks.append(f"<ul>{items}</ul>")
            listing.clear()

    for raw in lines:
        line = raw.rstrip()
        stripped = line.strip()
        if not stripped:
            flush_paragraph()
            flush_list()
            continue
        if stripped.startswith("#"):
            flush_paragraph()
            flush_list()
            level = len(stripped) - len(stripped.lstrip("#"))
            blocks.append(f"<h{min(level, 3)}>{inline(stripped[level:].strip())}</h{min(level, 3)}>")
            continue
        if stripped.startswith(("- ", "* ")):
            flush_paragraph()
            listing.append(stripped[2:].strip())
            continue
        if listing and line.startswith("  ") and not stripped.startswith("- "):
            # Continuation of the previous bullet.
            listing[-1] = listing[-1] + " " + stripped
            continue
        flush_list()
        paragraph.append(stripped)

    flush_paragraph()
    flush_list()
    body = "\n".join(blocks)
    return PAGE.format(title=html.escape(title), body=body)


def main() -> None:
    html_text = convert(SOURCE.read_text(encoding="utf-8"))
    OUTPUT.mkdir(parents=True, exist_ok=True)
    (OUTPUT / "privacy.html").write_text(html_text, encoding="utf-8")
    (OUTPUT / "index.html").write_text(html_text, encoding="utf-8")
    print(f"wrote {OUTPUT / 'index.html'} and {OUTPUT / 'privacy.html'}")


if __name__ == "__main__":
    main()
