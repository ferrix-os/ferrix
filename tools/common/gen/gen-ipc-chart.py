#!/usr/bin/env python3
"""Draw the native channel round trip's progress as SVG.

    python3 tools/common/gen/gen-ipc-chart.py

writes docs/img/ipc-round-trip.svg: bench-ipc's `domain-call` p50 on a log
scale, one point per landing on `main`, joined by a line, the unlanded branch
figures as hollow points, and dashed lines for the figures it is measured
against. It uses the standard library only, like gen-roadmap-charts.py, whose
colours, fonts and label handling it shares. The numbers are the ones
docs/roadmap/ipc-round-trip.md's table gives; change them here when those
change, and rerun.

The points are spaced one to a landing, not by the clock: five of them fall
on 2026-10-06 and five on 2026-10-07.
"""

import math
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent.parent.parent / "docs" / "img"

# Each point: date, tag under the axis, p50 in ns, on_main, and where the
# figure comes from. Every figure is from a commit message on main, from
# docs/OPAQUE-KERNEL.md (which quotes the run), or from a log, as said.
POINTS = [
    # origin/main before step 1: OPAQUE-KERNEL.md §9.1's table, "37 us"
    # (the run's exact 37,191 is the customer's log F1).
    ("10-01", "before 1", 37191, True),
    # os-ipc/zircon-trip, channel_write_read with the sync wake: §9.1's table,
    # "6.5 us" (exact 6,508, log F3). A branch, never on main in this form.
    ("10-01", "sync wake", 6508, False),
    # The same branch with the speculation domain: 3,021 (log F4; §9.9 quotes
    # it as "3,021 ns after step 1"). The histogram's bucket, so quantised.
    ("10-01", "domain", 3021, False),
    # Step 1 on main (the domain landed 2026-10-01; ipc-bench's domain-call
    # line, 3b7a935af, 2026-10-02 22:37, is what reads it). d750fd452 of the
    # same evening only retargets the plan and has no figure. §9.9: 3,021.
    ("10-02", "step 1", 3021, True),
    # 3349682db (2026-10-05 19:04): its message gives only the old
    # histogram's steps (2556, 2789, 3021); the exact 2,546 is §9.9's
    # "bench-2" run of main without 2f (4a8b8dfee).
    ("10-05", "exact bench", 2546, True),
    # 445d09420, 2f (2026-10-06 09:04): "2427 ns here against 2526 to 2546".
    ("10-06", "2f", 2427, True),
    # a1d456820, 3a/3b (10:45): "2,287 ns against 2,427 ns, ratio 0.942". The
    # 2,397 of §9.9 is the same tree before the rebase onto 2f, against
    # 2,546; the commit's own figure is used.
    ("10-06", "3a/3b", 2287, True),
    # 3b9b1e267, ERAPS (12:53): "-60 to -100 ns a round trip" against main
    # without 3a/3b (2,457 to 2,477 against 2,536). 2,212 is 2,287 less the
    # middle of that range: derived, not a run of that tree.
    ("10-06", "ERAPS", 2212, True),
    # 0edd644b2, step 4 (17:58): "1548-1558 ns against 2576", fast path on.
    # This is the high boot mode (about 1,550 ns on every tree, §9.11).
    ("10-06", "step 4", 1553, True),
    # 4c9c078cc (2026-10-07 11:37): the low mode of the same code, "about
    # 1,240" in its message, 1,248 in 690754979's and 4066f41dd's.
    ("10-07", "low mode", 1248, True),
    # 690754979, DS and ES (12:08): "low mode 1,118 and 1,128 against 1,248".
    ("10-07", "DS/ES", 1123, True),
    # 4066f41dd, link-time hooks (12:50): its own run is 1,208 to 1,228
    # against 1,248 on 4c9c078cc alone; 1,048 is the base 7b06cef25's
    # message measured in the low mode ("1,048 to 1,058").
    ("10-07", "hooks", 1048, True),
    # 7b06cef25, cut 2 (18:05): "low mode 1,028 against 1,078, one boot each"
    # (preliminary). The first form of the cut read 1,008 to 1,018.
    ("10-07", "cut 2", 1028, True),
    # 27d3e23c8, FS and GS left unloaded 0 over 0 (3c), landed 21:5x in batch
    # 20261007T190628Z: 873 in the fast mode (858 and 888 against 998),
    # measured on its branch in the quiet window of 20:11-20:14
    # (logs/po10/quietbench.out); the commit cites the slow mode, -160..-190.
    ("10-07", "FS/GS", 873, True),
]
# Where the boot mode the figures read changes: 0edd644b2's 1,553 is the high
# mode, the next point is the low mode of the same code.
MODE_SPLIT_AFTER = 8

# Figures it is measured against (OPAQUE-KERNEL.md §9.6 and §9.6a).
REFERENCES = [
    ("seL4 matched, 440 ns", 440, "#bf3989", "6 4"),
    ("Redox 0.9.0, 1,965 ns", 1965, "#8c959f", "2 3"),
    ("Linux pipe ping-pong, 2,105 ns", 2105, "#1a7f37", "8 3 2 3"),
]

FONT = "system-ui, -apple-system, 'Segoe UI', Helvetica, Arial, sans-serif"
INK = "#1f2328"
MUTED = "#656d76"
GRID = "#d8dee4"
ACTIVE_C = "#0969da"


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


class Svg:
    def __init__(self, w, h, title):
        self.w, self.h = w, h
        self.parts = [
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" '
            f'viewBox="0 0 {w} {h}" font-family="{FONT}" font-size="12" fill="{INK}">',
            f"<title>{esc(title)}</title>",
            # An opaque background, so the chart reads on a dark page too.
            f'<rect width="{w}" height="{h}" fill="#ffffff"/>',
        ]

    def add(self, s):
        self.parts.append(s)

    def text(self, x, y, s, anchor="start", size=12, fill=INK, weight="normal",
             halo=False):
        # A white halo keeps a label legible where the line crosses it.
        h = ' stroke="#ffffff" stroke-width="4" paint-order="stroke"' if halo else ""
        self.add(
            f'<text x="{x:.1f}" y="{y:.1f}" text-anchor="{anchor}" font-size="{size}" '
            f'fill="{fill}" font-weight="{weight}"{h}>{esc(s)}</text>'
        )

    def line(self, x1, y1, x2, y2, stroke, width=1, dash=None):
        d = f' stroke-dasharray="{dash}"' if dash else ""
        self.add(
            f'<line x1="{x1:.1f}" y1="{y1:.1f}" x2="{x2:.1f}" y2="{y2:.1f}" '
            f'stroke="{stroke}" stroke-width="{width}"{d}/>'
        )

    def polyline(self, pts, stroke, width=2, dash=None):
        p = " ".join(f"{x:.1f},{y:.1f}" for x, y in pts)
        d = f' stroke-dasharray="{dash}"' if dash else ""
        self.add(
            f'<polyline points="{p}" fill="none" stroke="{stroke}" '
            f'stroke-width="{width}" stroke-linejoin="round"{d}/>'
        )

    def dot(self, x, y, fill, r=3.5, hollow=False):
        if hollow:
            self.add(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="{r + 0.5}" fill="#ffffff" '
                     f'stroke="{fill}" stroke-width="2"/>')
        else:
            self.add(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="{r}" fill="{fill}"/>')

    def write(self, path):
        path.parent.mkdir(parents=True, exist_ok=True)
        with open(path, "w", encoding="utf-8", newline="\n") as f:
            f.write("\n".join(self.parts + ["</svg>"]) + "\n")


def num(v):
    return f"{v:,}"


def chart():
    w, h = 1060, 420
    svg = Svg(w, h, "Ferrix native channel round trip, domain-call p50")
    left, right = 70, w - 200
    top, bottom = 90, 370
    svg.text(24, 28, "The native channel round trip: bench-ipc domain-call p50, ns, log scale",
             size=16, weight="bold")
    svg.text(24, 46, "One point to a landing on main, spaced evenly, not by the clock; "
             "hollow: measured on a branch, not on main", fill=MUTED)
    svg.text(24, 62, "Every mitigation on, one speculation domain, KVM, one pinned "
             "processor on nazuna (Zen 5, no PCID)", fill=MUTED)

    lo, hi = 300, 60000
    y = lambda v: bottom - (bottom - top) * math.log(v / lo) / math.log(hi / lo)
    n = len(POINTS)
    x = lambda i: left + 20 + (right - left - 40) * i / (n - 1)

    for v in (500, 1000, 2000, 5000, 10000, 20000, 50000):
        svg.line(left, y(v), right, y(v), GRID)
        svg.text(left - 8, y(v) + 4, num(v), anchor="end", fill=MUTED)
    svg.text(18, (top + bottom) / 2, "ns", fill=MUTED, anchor="middle")

    # The reference lines, labelled at their right end. Redox and Linux lie
    # 4 px apart: one label goes above its line, the other below.
    for name, v, colour, dash in REFERENCES:
        svg.line(left, y(v), right, y(v), colour, width=1.5, dash=dash)
        if v == 1965:
            svg.text(right + 8, y(v) + 14, name, fill=colour)
        elif v == 2105:
            svg.text(right + 8, y(v) - 4, name, fill=colour)
        else:
            svg.text(right + 8, y(v) + 4, name, fill=colour)

    # The boot mode's change, between step 4 and the low-mode figure.
    sx = (x(MODE_SPLIT_AFTER) + x(MODE_SPLIT_AFTER + 1)) / 2
    svg.line(sx, top - 6, sx, bottom, MUTED, dash="1 3")
    svg.text(sx + 5, y(14000), "from here the faster of the two boot modes", fill=MUTED, size=11)

    # The line joins main's points; a hollow point hangs off the last of them
    # by a dotted line, and the early branch points are not joined.
    main = [(x(i), y(p)) for i, (_, _, p, m) in enumerate(POINTS) if m]
    svg.polyline(main, ACTIVE_C, width=2.5)
    last_main = max(i for i, pt in enumerate(POINTS) if pt[3])
    for i, (_, _, p, m) in enumerate(POINTS):
        if not m and i > last_main:
            svg.line(x(last_main), y(POINTS[last_main][2]), x(i), y(p), ACTIVE_C,
                     width=1.5, dash="2 3")
    for i, (d, tag, p, m) in enumerate(POINTS):
        svg.dot(x(i), y(p), ACTIVE_C, hollow=not m)
        svg.text(x(i), y(p) - 10, num(p), anchor="middle", size=11,
                 weight="bold" if m else "normal", fill=ACTIVE_C if m else MUTED,
                 halo=True)
        svg.text(x(i), bottom + 18, d, anchor="middle", fill=MUTED, size=11)
        svg.text(x(i), bottom + 32, tag, anchor="middle", fill=MUTED, size=10)

    # The target below the last point, in words.
    svg.text(right + 8, y(440) + 22, "target: seL4's, then < 400", fill="#bf3989", size=11)
    svg.write(OUT / "ipc-round-trip.svg")


if __name__ == "__main__":
    chart()
    print(f"wrote {OUT / 'ipc-round-trip.svg'}")
