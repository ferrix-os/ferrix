#!/usr/bin/env python3
"""Label and shrink the install guide's captures (docs/INSTALL.md).

    install-shots.py stamp IN.png OUT.png WHERE "what it shows" [--crop L,T,R,B]
    install-shots.py terminal IN.txt OUT.png WHERE "what it shows" [--title T] [--cols N]

WHERE is ferrix, linux-host or windows. Every image gets a banner naming the
system it was taken on, so a picture of a host's terminal is never taken for
Ferrix (the customer's rule of 2026-10-12, docs/brand/screenshots/CAPTIONS.md).

`stamp` puts the banner on a capture as it is: a VNC grab of the guest's
screen, or a host window grabbed by its handle. `terminal` draws a
terminal's text -- a transcript of a command that ran, written by
`script` or tee, its escape sequences stripped -- in the tree's own
monospace font, for a host terminal on a machine that has no screen to grab.
Nothing is redrawn but the text: the lines are the transcript's.

Both write an optimised PNG, reduced to 256 colours when that keeps it
under 500 KB and the full colours do not.
"""

from __future__ import annotations

import argparse
import io
import pathlib
import re
import sys

from PIL import Image, ImageDraw, ImageFont

ROOT = pathlib.Path(__file__).resolve().parents[3]
FONTS = ROOT / "assets" / "fonts" / "liberation"
BOLD = FONTS / "LiberationSans-Bold.ttf"
MONO = FONTS / "LiberationMono-Regular.ttf"

WHERE = {
    "ferrix": ((24, 140, 60), "FERRIX"),
    "linux-host": ((180, 30, 30), "LINUX HOST - not Ferrix"),
    "windows": ((30, 90, 200), "WINDOWS HOST - not Ferrix"),
}
LIMIT = 500 * 1024
# A terminal's colours: Ferrix's brand background and text (docs/marketing/README.md).
BACKGROUND = (13, 15, 18)
TEXT = (238, 241, 245)
MUTED = (154, 164, 178)
ESCAPE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07|\r")


def banner(image: Image.Image, where: str, what: str) -> Image.Image:
    """`image` under a band naming the system it shows."""
    colour, label = WHERE[where]
    image = image.convert("RGB")
    width, height = image.size
    band = max(40, min(64, height // 18))
    out = Image.new("RGB", (width, height + band), colour)
    out.paste(image, (0, band))
    draw = ImageDraw.Draw(out)
    font = ImageFont.truetype(str(BOLD), int(band * 0.5))
    small = ImageFont.truetype(str(BOLD), int(band * 0.36))
    draw.text((band // 3, band // 4), label, fill="white", font=font)
    left = band // 3 + draw.textlength(label, font=font) + band // 2
    draw.text((left, int(band * 0.34)), what, fill="white", font=small)
    return out


def save(image: Image.Image, out: pathlib.Path) -> None:
    """`image` as a PNG, in 256 colours if that is what keeps it small."""
    full = io.BytesIO()
    image.save(full, "PNG", optimize=True)
    data = full.getvalue()
    if len(data) > LIMIT:
        reduced = io.BytesIO()
        image.quantize(256, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.FLOYDSTEINBERG).save(
            reduced, "PNG", optimize=True
        )
        data = reduced.getvalue()
    out.write_bytes(data)
    print(f"{out}: {image.size[0]}x{image.size[1]}, {len(data)} bytes")


def terminal(text: str, title: str, cols: int) -> Image.Image:
    """`text` as a terminal window draws it, `cols` wide, with `title` in its bar."""
    lines = [ESCAPE.sub("", line).expandtabs().rstrip() for line in text.rstrip().split("\n")]
    font = ImageFont.truetype(str(MONO), 17)
    cell = font.getbbox("M")[2]
    line_height = 24
    wrapped: list[str] = []
    for line in lines:
        while len(line) > cols:
            wrapped.append(line[:cols])
            line = line[cols:]
        wrapped.append(line)
    pad, bar = 18, 34
    width = pad * 2 + cell * cols
    height = bar + pad * 2 + line_height * len(wrapped)
    image = Image.new("RGB", (width, height), BACKGROUND)
    draw = ImageDraw.Draw(image)
    draw.rectangle((0, 0, width, bar), fill=(22, 26, 32))
    draw.text((pad, 8), title, fill=MUTED, font=ImageFont.truetype(str(BOLD), 15))
    for row, line in enumerate(wrapped):
        draw.text((pad, bar + pad + row * line_height), line, fill=TEXT, font=font)
    return image


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    stamp = sub.add_parser("stamp")
    term = sub.add_parser("terminal")
    for each in (stamp, term):
        each.add_argument("input", type=pathlib.Path)
        each.add_argument("output", type=pathlib.Path)
        each.add_argument("where", choices=sorted(WHERE))
        each.add_argument("what")
    stamp.add_argument("--crop", help="LEFT,TOP,RIGHT,BOTTOM: keep only this box of the capture")
    term.add_argument("--title", default="Terminal")
    term.add_argument("--cols", type=int, default=100)
    args = parser.parse_args()
    if args.command == "stamp":
        image = Image.open(args.input)
        if args.crop:
            image = image.crop(tuple(int(edge) for edge in args.crop.split(",")))
    else:
        image = terminal(args.input.read_text(errors="replace"), args.title, args.cols)
    save(banner(image, args.where, args.what), args.output)
    return 0


if __name__ == "__main__":
    sys.exit(main())
