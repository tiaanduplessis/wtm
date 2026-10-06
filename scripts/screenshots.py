#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["pillow==12.3.0", "pyte==0.8.2"]
# ///
"""Capture real wtm PTY screens using disposable demo repositories.

Run: uv run scripts/screenshots.py target/release/wtm
"""

import argparse
import math
from pathlib import Path
import tempfile

from PIL import Image, ImageDraw, ImageFont

from tui_smoke import Session, git, initialize


ROOT = Path(__file__).resolve().parent.parent
PALETTE = {
    "default": "#d8dee9", "black": "#11131a", "red": "#ef7272",
    "green": "#93d49c", "brown": "#ebc475", "yellow": "#ebc475",
    "blue": "#87aff5", "magenta": "#c799ee", "cyan": "#83d6dc",
    "white": "#e5e9f0", "brightblack": "#818b9e", "brightred": "#ff9393",
    "brightgreen": "#acf2b6", "brightyellow": "#ffe39a", "brightblue": "#a8c5ff",
    "brightmagenta": "#e6bbff", "brightcyan": "#aaf0f5", "brightwhite": "#ffffff",
}
BACKGROUND = "#11131a"


def color(value, background=False):
    if value == "default" and background:
        return BACKGROUND
    return PALETTE.get(value, "#" + value)


def capture(session, destination, font_path):
    # Drain the remainder of the current frame before reading emulator cells.
    for _ in range(3):
        session.pump()
    regular = ImageFont.truetype(str(font_path), 24)
    if font_path.suffix == ".ttc":
        bold = ImageFont.truetype(str(font_path), 24, index=1)
    else:
        bold_path = font_path.with_name(font_path.stem + "-Bold" + font_path.suffix)
        bold = ImageFont.truetype(str(bold_path), 24) if bold_path.exists() else regular
    width = regular.getlength("M")
    ascent, descent = regular.getmetrics()
    height = ascent + descent
    padding = 20
    screen = session.screen
    image = Image.new("RGB", (math.ceil(screen.columns * width) + padding * 2,
                              screen.lines * height + padding * 2), BACKGROUND)
    draw = ImageDraw.Draw(image)
    for row in range(screen.lines):
        for column in range(screen.columns):
            cell = screen.buffer[row][column]
            foreground, background = color(cell.fg), color(cell.bg, background=True)
            if cell.reverse:
                foreground, background = background, foreground
            x, y = padding + column * width, padding + row * height
            draw.rectangle((x, y, x + width, y + height), fill=background)
            if cell.data:
                draw.text((x, y + ascent), cell.data, font=bold if cell.bold else regular,
                          fill=foreground, anchor="ls")
            if cell.underscore:
                draw.line((x, y + height - 2, x + width, y + height - 2), fill=foreground)
    destination.parent.mkdir(parents=True, exist_ok=True)
    image.save(destination, optimize=True)
    print(destination)


def run(binary, output, font):
    with tempfile.TemporaryDirectory(prefix="wtm-demo-", dir="/tmp") as temporary:
        root = Path(temporary).resolve()
        storefront = root / "storefront"
        api = storefront / "services" / "api"
        tooling = root / "packages" / "tooling"
        for repo in (storefront, api, tooling):
            initialize(repo)
            git(repo, "remote", "add", "origin", f"git@github.com:example/{repo.name}.git")
        worktrees = root / ".worktrees"
        search = worktrees / "cart-search"
        cleanup = worktrees / "legacy-css"
        support = worktrees / "release-support"
        retry = worktrees / "retry-controls"
        for owner, path, branch in ((storefront, search, "feat/cart-search"),
                                    (storefront, cleanup, "chore/legacy-css"),
                                    (storefront, support, "release/support"),
                                    (api, retry, "feat/retry-controls")):
            git(owner, "worktree", "add", "-b", branch, path)
        (search / "cart-notes.md").write_text("Demo work in progress.\n")
        git(storefront, "worktree", "lock", "--reason", "Keep for release hotfixes", support)

        ui = Session(binary, root)
        try:
            ui.contains("3 repositories | 7/7 worktrees")
            ui.idle()
            x, y = ui.locate("feat/cart-search")
            ui.mouse(0, x, y)
            ui.contains(f"Worktree: {search}")
            capture(ui, output / "dashboard.png", font)

            x, y = ui.locate("chore/legacy-css")
            ui.mouse(0, x, y)
            ui.contains(f"Worktree: {cleanup}")
            ui.click("[Mark]")
            ui.contains("1 marked")
            ui.click("[Remove]")
            ui.contains("Remove worktrees")
            capture(ui, output / "cleanup-review.png", font)
            ui.cancel("Remove worktrees")
            assert cleanup.is_dir(), "Screenshot capture must not remove a worktree"
            ui.exit()
        except BaseException:
            ui.abort()
            raise


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--output", type=Path, default=ROOT / "docs/images")
    fonts = (Path("/System/Library/Fonts/Menlo.ttc"),
             Path("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"))
    parser.add_argument("--font", type=Path, default=next((font for font in fonts if font.exists()), None))
    args = parser.parse_args()
    if args.font is None or not args.font.is_file():
        parser.error("Provide a monospace font with --font /path/to/font.ttf")
    run(args.binary.resolve(), args.output.resolve(), args.font)
