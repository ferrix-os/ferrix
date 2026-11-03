#!/usr/bin/env python3
"""Draw the roadmap's burndown and Gantt charts as SVG.

    python3 tools/common/gen/gen-roadmap-charts.py

writes docs/img/burndown.svg and docs/img/gantt.svg. It uses the standard
library only, so it runs wherever the other gen-* scripts do. The numbers
are the ones docs/roadmap/status.md's status table and docs/BACKLOG.md's
*Velocity* give; change them here when those change, and rerun.
"""

import math
from datetime import date, timedelta
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent.parent.parent / "docs" / "img"

TODAY = date(2026, 11, 3)

# Points landed per day (docs/BACKLOG.md, *Velocity*). 09-18 to 09-23 were
# sized afterwards from `git log`; 09-23 is what is left of that backfill.
# From 09-24 a day is what was estimated before the work started plus what
# was sized afterwards, and AFTERWARDS says how much of it is the latter.
LANDED = [
    (date(2026, 9, 14), 131),
    (date(2026, 9, 15), 34),
    (date(2026, 9, 16), 66),
    (date(2026, 9, 17), 214),
    (date(2026, 9, 18), 50),
    (date(2026, 9, 19), 50),
    (date(2026, 9, 20), 55),
    (date(2026, 9, 21), 96),
    (date(2026, 9, 22), 20),
    (date(2026, 9, 23), 53),
    (date(2026, 9, 24), 126),
    (date(2026, 9, 25), 26),
    (date(2026, 9, 26), 278),
    (date(2026, 9, 27), 240),
    (date(2026, 9, 28), 44),
    (date(2026, 9, 29), 62),
    (date(2026, 9, 30), 102),
    (date(2026, 10, 1), 94),
    (date(2026, 10, 2), 150),
    (date(2026, 10, 3), 153),
    (date(2026, 10, 4), 55),
    (date(2026, 10, 5), 115),
    # 2026-10-06 to 10-31, counted on 2026-11-02 (docs/backlog/VELOCITY.md,
    # "Update, 2026-11-02"): 1.1 points a commit that is not documentation,
    # a reservation, a pin or coverage, replaced by the design documents'
    # estimates where they exist and by cluster sizes on 10-26, 10-27, 10-29,
    # 10-30 and 10-31. Dated by the commit's date on develop.
    (date(2026, 10, 6), 63),
    (date(2026, 10, 7), 57),
    (date(2026, 10, 8), 37),
    (date(2026, 10, 9), 1),
    (date(2026, 10, 10), 11),
    (date(2026, 10, 11), 79),
    (date(2026, 10, 12), 23),
    (date(2026, 10, 13), 29),
    (date(2026, 10, 14), 31),
    (date(2026, 10, 15), 75),
    (date(2026, 10, 16), 102),
    (date(2026, 10, 17), 43),
    (date(2026, 10, 18), 62),
    (date(2026, 10, 19), 13),
    (date(2026, 10, 20), 24),
    (date(2026, 10, 21), 34),
    (date(2026, 10, 22), 54),
    (date(2026, 10, 23), 217),
    (date(2026, 10, 24), 69),
    (date(2026, 10, 25), 107),
    (date(2026, 10, 26), 50),
    (date(2026, 10, 27), 3),
    (date(2026, 10, 28), 32),
    (date(2026, 10, 29), 15),
    (date(2026, 10, 30), 80),
    (date(2026, 10, 31), 59),
    # 2026-11-03's count (VELOCITY.md, "Update, 2026-11-03"): one commit a day
    # that counts, the fallible-allocation gate and the mirrors' lock.
    (date(2026, 11, 1), 0),
    (date(2026, 11, 2), 1),
    (date(2026, 11, 3), 1),
]
BACKFILLED = {date(2026, 9, d) for d in range(18, 24)}
AFTERWARDS = {date(2026, 9, 24): 27, date(2026, 9, 25): 26, date(2026, 9, 26): 175,
              date(2026, 9, 27): 150, date(2026, 9, 28): 44, date(2026, 9, 29): 26,
              date(2026, 9, 30): 83, date(2026, 10, 1): 94, date(2026, 10, 2): 128,
              date(2026, 10, 3): 115, date(2026, 10, 4): 55,
              date(2026, 10, 5): 29,
              date(2026, 10, 6): 4, date(2026, 10, 7): 14, date(2026, 10, 8): 10, date(2026,
              10, 9): 1, date(2026, 10, 10): 11, date(2026, 10, 11): 74, date(2026, 10, 12):
              23, date(2026, 10, 13): 29, date(2026, 10, 14): 18, date(2026, 10, 15): 70,
              date(2026, 10, 16): 90, date(2026, 10, 17): 43, date(2026, 10, 18): 62,
              date(2026, 10, 19): 13, date(2026, 10, 20): 24, date(2026, 10, 21): 34,
              date(2026, 10, 22): 54, date(2026, 10, 23): 38, date(2026, 10, 24): 36,
              date(2026, 10, 25): 85, date(2026, 10, 26): 50, date(2026, 10, 27): 3,
              date(2026, 10, 28): 32, date(2026, 10, 29): 15, date(2026, 10, 30): 80,
              date(2026, 10, 31): 59, date(2026, 11, 2): 1, date(2026, 11, 3): 1}
# A day of the lower chart gets its number from this many points.
LABEL_FROM = 100

# The status table's sized, unfinished rows on 2026-11-03 (3afb272b5). Nothing
# sized came off between 10-08 and 11-03: the landings of those weeks (stage 13's
# follow-ons, N3c, lode and tole, the machine-check family, signing S1 to S4)
# were unsized or outside the table. The bubblewrap row's N5 and bwrap-user
# landed 2026-10-26 (3a65627b9) but N6 and N7 are not split out of its 16.
# (Rows as of 2026-10-08 follow.)
# The state says why a row is not moving: "unlanded" is built and waits on a
# landing (controls, `check`, a batch), "idle" has had no session on it since
# 2026-09-26 or longer, "added" came into scope after the 09-26 baseline.
REMAINING = [
    ("Client pages as texture backing", 8, "idle"),
    ("GC400, the rest", 21, "idle"),
    ("Stage 15, auth's rest", 6, ""),
    # L13b landed 2026-10-05 and L13c 2026-10-08 (aae4c0eb1); the second pass
    # and xray (8) landed 2026-10-07 (bab9eed1c) and the controllers (30) the
    # same day (08984c2db..8ec24ef0e). Nothing sized is built and waiting now.
    # Of 50: the armhf volume 3 and the SD card's driver 13 landed 2026-10-08
    # (aae4c0eb1, 03fce4f81); the board run waits for the product owner.
    ("Chrome on the DK1", 34, "idle"),
    # Not idle since 2026-10-26: the scheduler's classes are built on pull
    # requests #38 and #40, not landed, with no share of the 40 sized for them.
    ("Stage 14, real-time", 40, ""),
    # Of 48: zwp_linux_dmabuf_v1 and the GBM-shaped allocator (8) landed
    # 2026-10-08 (03fce4f81); Mesa's virgl is not started.
    ("dmabuf and virgl", 40, "idle"),
    # Re-sized 2026-10-05 from 5: the consultant found B1 and B2 (parked).
    ("Stage 22, bubblewrap's rest", 16, ""),
    ("Stage 22, the rest (guess)", 23, ""),
    ("NVIDIA N5, CUDA", 52, "added"),
    # Sized 2026-10-04 evening, by three sizing agents; ranges 40-75, 30-50
    # and 130-210. Most milestones are guesses.
    ("Stage 20, self-hosting", 45, "added"),
    ("Pixel 7, the USB driver's rest", 37, "added"),
    # C1's five points spent 2026-10-07 (f8b44e04c, seven audit rows closed).
    ("Certification findings, in-repository work", 160, "added"),
    # Added 2026-10-05 at the customer's question; a first guess (D1-D5).
    # D1, D2 and D4 landed for the display kind 2026-10-08 (aae4c0eb1); D3 and
    # the other kinds are left, so the 26 stand whole.
    ("Live driver update", 26, "added"),
    # OPAQUE-KERNEL.md §9.5/§9.7: 69 at 2026-10-05; 2f, step 3 without PCIDs
    # and step 4's fast path (59) landed 2026-10-06, so 10 are left, step 5's
    # squeeze by the profile (range 60-90 for the whole row).
    ("IPC round trip to seL4's figure", 10, "added"),
    # Every row outside the old sized scope, estimated 2026-11-03 (status.md,
    # *Estimated 2026-11-03*, says each one's range and basis).
    ("Stage 23, Endfield's rest", 101, "est"),
    ("Stage 24, the game path's rest", 175, "est"),
    ("Stage 25, Yoru's rest", 46, "est"),
    ("Developing Ferrix on Ferrix, the rest", 39, "est"),
    ("Microkernel, M1's rest to M8", 477, "est"),
    ("Microkernel, maximum-certification streams", 950, "est"),
    ("Certification, ring-0 interference", 125, "est"),
    ("Installer, the rest", 128, "est"),
    ("Product signing, S5 and S6", 21, "est"),
    ("Game mode, the rest", 115, "est"),
    ("Program suspend, S2 to S4", 178, "est"),
    ("lode's rest, tole's U10", 37, "est"),
    ("Stage 21, NVIDIA's rest", 15, "est"),
    ("Chrome: GPU compositing", 8, "est"),
    ("Stage 21, the N150 and its Intel GPU, the rest", 30, "est"),
    ("--everything, the rest", 5, "est"),
    ("Desktop clients, the rest", 5, "est"),
    ("Claude Code, i386's AVX state", 5, "est"),
    ("Claude desktop app, C5 to C7", 12, "est"),
    ("Init, L12 and later", 16, "est"),
    ("Authentication, phase 3", 32, "est"),
    ("Live driver update, the other kinds", 13, "est"),
    ("Stage 20 on ARMv7-A", 13, "est"),
    ("IPC round trip, step 4b", 8, "est"),
]
# The queue's order (customer, 2026-10-04): built rows first, those that only
# need landing ("unlanded"), then every other sized row, smallest points first
# in each group; ties keep the order written above. A new row falls into place.
REMAINING.sort(key=lambda r: (0 if "unlanded" in r[2] else 1, r[1]))
SCOPE = sum(p for _, p, _ in REMAINING)

# The rate the forecast uses is what came off a fixed scope, not what landed.
# Of the 442 points sized on 09-26, 201 had left the table by 10-04: about
# 158 by landings and 43 by a lower guess (stage 22's rest and Venus). 158
# points in 8 days is about 20 a day. Over the same 8 days about 900 points
# landed and 205 of them had an estimate first; the rest was work outside the
# table, which takes nothing off it.
BURN_RATE = 20
# With every row in the scope, what lands all comes off it: 1,370 points in the
# 26 days of 10-06 to 10-31, all but the unsized work's rate being unmeasured.
RATES = [(BURN_RATE, f"{BURN_RATE} a day, what came off the 09-26 scope, 09-27 to 10-04"),
         (53, "53 a day, everything that landed 10-06 to 10-31")]
FORECAST_RATE = BURN_RATE

D = date
DONE = [
    ("Stages 7 to 11, ring-3 disk", D(2026, 9, 13), D(2026, 9, 14)),
    ("Networking (50)", D(2026, 9, 15), D(2026, 9, 16)),
    ("Stages 17 and 18 (170)", D(2026, 9, 16), D(2026, 9, 17)),
    ("GPU path A (52)", D(2026, 9, 18), D(2026, 9, 19)),
    ("Dynamic linking, Arm port (73)", D(2026, 9, 20), D(2026, 9, 23)),
    ("Stage 12, btrfs write (60)", D(2026, 9, 21), D(2026, 9, 21)),
    ("Stage 16, rustc", D(2026, 9, 22), D(2026, 9, 22)),
    ("Cursor plane (13)", D(2026, 9, 23), D(2026, 9, 23)),
    ("sysfs, device queue, Chrome", D(2026, 9, 24), D(2026, 9, 24)),
    ("Init L1 to L11 (69)", D(2026, 9, 24), D(2026, 9, 26)),
    ("cgroups P1, M1, S1 (28)", D(2026, 9, 26), D(2026, 9, 26)),
    ("Audio, alsa-lib, pulsed (45)", D(2026, 9, 26), D(2026, 9, 27)),
    ("Chrome: zygote, speed, ferrousli", D(2026, 9, 26), D(2026, 9, 26)),
    ("i386 ABI I1 to I4 (42)", D(2026, 9, 27), D(2026, 9, 27)),
    ("Installer MVP", D(2026, 9, 28), D(2026, 9, 28)),
    ("yserver, the X server (36)", D(2026, 9, 28), D(2026, 9, 29)),
    ("Mount namespaces N1 to N3 (19)", D(2026, 9, 28), D(2026, 9, 30)),
    ("Stage 13: cgroup controllers (30)", D(2026, 10, 7), D(2026, 10, 7)),
    ("Claude Code on Ferrix", D(2026, 9, 29), D(2026, 10, 1)),
    ("Speculation domain, round trip", D(2026, 10, 1), D(2026, 10, 3)),
    ("btrfs in the certified item", D(2026, 10, 2), D(2026, 10, 2)),
    ("NVIDIA N0 and N1 (60)", D(2026, 10, 2), D(2026, 10, 3)),
    ("NVIDIA N2 to N4 and the TV (64)", D(2026, 10, 3), D(2026, 10, 5)),
    ("Round trip: 2f, step 3, step 4 (59)", D(2026, 10, 5), D(2026, 10, 7)),
    ("Certification C1: audit rows (5)", D(2026, 10, 7), D(2026, 10, 7)),
    ("Stage 19: second pass, xray (8)", D(2026, 10, 7), D(2026, 10, 7)),
    ("Init L13c (3)", D(2026, 10, 8), D(2026, 10, 8)),
    ("dmabuf, GBM allocator (8)", D(2026, 10, 8), D(2026, 10, 8)),
    ("Chrome on ARMv7-A, DK1 SD card (16)", D(2026, 10, 7), D(2026, 10, 8)),
    ("Authentication P1, P2 (desktop as a user)", D(2026, 9, 27), D(2026, 10, 4)),
    ("Network namespaces", D(2026, 10, 1), D(2026, 10, 4)),
    ("Components and apps in repositories", D(2026, 10, 3), D(2026, 10, 4)),
    ("Stage 13 follow-ons, N5, time namespaces (unsized)", D(2026, 10, 26), D(2026, 10, 26)),
    ("N3c: hyprix on the 3060 (8)", D(2026, 10, 25), D(2026, 10, 25)),
    ("lode, steps 1 to 6 and tombstones", D(2026, 10, 23), D(2026, 10, 24)),
    ("tole U1 to U9 (48 + 13)", D(2026, 10, 23), D(2026, 10, 24)),
    ("GNOME-like desktop, parts 1 to 4 (70)", D(2026, 10, 23), D(2026, 10, 24)),
    ("Machine-check family, 8 items", D(2026, 10, 30), D(2026, 10, 31)),
    ("Stage 13, its exit criterion met", D(2026, 9, 23), D(2026, 10, 14)),
]
# Each row: name, first day, and the REMAINING entries its unfinished work is
# (by name). The remainder is drawn to the
# end of the last matching entry in the forecast queue, so the two agree.
ACTIVE = [
    ("Stage 19, the rest", D(2026, 9, 17),
     ["Client pages as texture backing"]),
    ("Stage 20, self-hosting", D(2026, 9, 22),
     ["Stage 20, self-hosting", "Stage 20 on ARMv7-A"]),
    ("Gears (50 of 71)", D(2026, 9, 24), ["GC400, the rest"]),
    ("Certification findings", D(2026, 9, 25),
     ["Certification findings, in-repository work", "Certification, ring-0 interference"]),
    ("Pixel 7: USB beyond the console driver", D(2026, 9, 26),
     ["Pixel 7, the USB driver's rest"]),
    ("Steam: the game step", D(2026, 9, 30),
     ["Stage 22, bubblewrap's rest", "Stage 22, the rest (guess)"]),
    ("Auth P2, the rest (P2.1 landed)", D(2026, 10, 4), ["Stage 15, auth's rest", "Authentication, phase 3"]),
    ("IPC round trip (steps 1 to 4 landed)", D(2026, 10, 1),
     ["IPC round trip to seL4's figure", "IPC round trip, step 4b"]),
    ("Live driver update (display kind landed)", D(2026, 10, 7), ["Live driver update", "Live driver update, the other kinds"]),
    ("Installer, the rest", D(2026, 9, 28), ["Installer, the rest"]),
    ("Stage 21: NVIDIA's driver", D(2026, 10, 2), 
     ["NVIDIA N5, CUDA", "Stage 21, NVIDIA's rest", "Chrome: GPU compositing"]),
    ("Stage 23, Endfield", D(2026, 10, 10), ["Stage 23, Endfield's rest"]),
    ("Stage 24, the game path", D(2026, 10, 10), ["Stage 24, the game path's rest"]),
    ("Developing Ferrix on Ferrix", D(2026, 10, 10),
     ["Developing Ferrix on Ferrix, the rest"]),
    ("Game mode", D(2026, 10, 11), ["Game mode, the rest", "Program suspend, S2 to S4"]),
    ("Stage 21: the N150 box, its Intel GPU", D(2026, 10, 13),
     ["Stage 21, the N150 and its Intel GPU, the rest"]),
    ("Microkernel", D(2026, 10, 13),
     ["Microkernel, M1's rest to M8", "Microkernel, maximum-certification streams"]),
    ("lode's rest, tole's U10", D(2026, 10, 23), ["lode's rest, tole's U10"]),
    ("Stage 25, Yoru", D(2026, 10, 24), ["Stage 25, Yoru's rest"]),
    ("Product signing, S5 and S6", D(2026, 10, 24), ["Product signing, S5 and S6"]),
    ("Stage 14: classes on PRs #38, #40", D(2026, 10, 26), ["Stage 14, real-time"]),
]

FONT = "system-ui, -apple-system, 'Segoe UI', Helvetica, Arial, sans-serif"
INK = "#1f2328"
MUTED = "#656d76"
GRID = "#d8dee4"
DONE_C = "#8c959f"
ACTIVE_C = "#0969da"
FORECAST_C = "#54aeff"
LINE_C = ["#0969da", "#bf3989"]
TODAY_C = "#cf222e"


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

    def text(self, x, y, s, anchor="start", size=12, fill=INK, weight="normal"):
        self.add(
            f'<text x="{x:.1f}" y="{y:.1f}" text-anchor="{anchor}" font-size="{size}" '
            f'fill="{fill}" font-weight="{weight}">{esc(s)}</text>'
        )

    def line(self, x1, y1, x2, y2, stroke, width=1, dash=None):
        d = f' stroke-dasharray="{dash}"' if dash else ""
        self.add(
            f'<line x1="{x1:.1f}" y1="{y1:.1f}" x2="{x2:.1f}" y2="{y2:.1f}" '
            f'stroke="{stroke}" stroke-width="{width}"{d}/>'
        )

    def rect(self, x, y, w, h, fill, stroke=None, dash=None, rx=2):
        s = f' stroke="{stroke}" stroke-width="1"' if stroke else ""
        d = f' stroke-dasharray="{dash}"' if dash else ""
        self.add(
            f'<rect x="{x:.1f}" y="{y:.1f}" width="{w:.1f}" height="{h:.1f}" '
            f'rx="{rx}" fill="{fill}"{s}{d}/>'
        )

    def polyline(self, pts, stroke, width=2, dash=None):
        p = " ".join(f"{x:.1f},{y:.1f}" for x, y in pts)
        d = f' stroke-dasharray="{dash}"' if dash else ""
        self.add(
            f'<polyline points="{p}" fill="none" stroke="{stroke}" '
            f'stroke-width="{width}" stroke-linejoin="round"{d}/>'
        )

    def dot(self, x, y, fill, r=3):
        self.add(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="{r}" fill="{fill}"/>')

    def write(self, path):
        path.parent.mkdir(parents=True, exist_ok=True)
        with open(path, "w", encoding="utf-8", newline="\n") as f:
            f.write("\n".join(self.parts + ["</svg>"]) + "\n")


def label(d):
    return d.strftime("%m-%d")


def burndown():
    w, h = 1040, 800
    svg = Svg(w, h, "Ferrix burndown")
    left, right = 56, w - 24

    # Panel 1: sized points remaining, forecast from today.
    top, bottom = 60, 270
    svg.text(left, 28, f"Sized points remaining at the end of each day, from {label(TODAY)}",
             size=16, weight="bold")
    est = sum(p for _, p, s in REMAINING if "est" in s)
    svg.text(left, 46, f"{SCOPE:,} points in the status table's unfinished rows, {est:,} of "
             "them estimated on 11-03, many as guesses; scope added later moves the date",
             size=12, fill=MUTED)
    days = math.ceil(SCOPE / FORECAST_RATE) + 2
    ystep = 100 if SCOPE <= 1000 else 500
    ymax = -(-SCOPE // ystep) * ystep  # the axis reaches the scope, in whole steps
    x = lambda i: left + (right - left) * i / (days - 1)
    y = lambda v: bottom - (bottom - top) * v / ymax
    for v in range(0, ymax + 1, ystep):
        svg.line(left, y(v), right, y(v), GRID)
        svg.text(left - 8, y(v) + 4, f"{v:,}", anchor="end", fill=MUTED)
    for i in range(0, days, 2 if days <= 40 else 14):
        svg.text(x(i), bottom + 18, label(TODAY + timedelta(days=i)),
                 anchor="middle", fill=MUTED)
    for (rate, name), colour, ly in zip(RATES, LINE_C, (top + 8, top + 26)):
        # Whole days while work is left, then the day it runs out, exactly.
        pts = [(x(i), y(SCOPE - rate * i)) for i in range(days) if SCOPE - rate * i > 0]
        pts.append((x(SCOPE / rate), y(0)))
        svg.polyline(pts, colour)
        svg.dot(*pts[-1], colour, r=4)
        # A tick is the end of its day, so the work runs out during the day
        # after the last tick it has passed.
        end = TODAY + timedelta(days=math.ceil(SCOPE / rate))
        svg.line(right - 420, ly, right - 396, ly, colour, width=2)
        svg.text(right - 388, ly + 4, f"{name}: done {label(end)}")
    svg.dot(x(0), y(SCOPE), INK, r=4)

    # The two panels below share one x axis, a slot a day.
    n = len(LANDED)
    slot = (right - left) / n
    cx = lambda i: left + slot * (i + 0.5)

    def weeks(bottom):
        for i, (d, _) in enumerate(LANDED):
            if i % 7 == 0:
                svg.text(cx(i), bottom + 18, label(d), anchor="middle", fill=MUTED)

    # Panel 2: what has landed, per day. The peaks carry a number, biggest
    # first, and none that would touch a number already placed; every day's
    # figure is in docs/backlog/VELOCITY.md.
    top, bottom = 366, 546
    svg.text(left, 324, "Points landed per day", size=16, weight="bold")
    svg.rect(left, 337, 12, 12, ACTIVE_C)
    svg.text(left + 17, 347, "estimated before the work started", fill=MUTED)
    svg.add('<defs><pattern id="hatch" width="6" height="6" '
            'patternUnits="userSpaceOnUse" patternTransform="rotate(45)">'
            f'<rect width="6" height="6" fill="{FORECAST_C}"/>'
            '<line x1="0" y1="0" x2="0" y2="6" stroke="#ffffff" stroke-width="2"/>'
            '</pattern></defs>')
    svg.rect(left + 222, 337, 12, 12, "url(#hatch)")
    svg.text(left + 239, 347, "sized afterwards from git log; a number on days of "
             f"{LABEL_FROM} or more", fill=MUTED)
    peak = max(p for _, p in LANDED)
    bmax = -(-peak // 50) * 50
    yb = lambda v: bottom - (bottom - top) * v / bmax
    for v in range(0, bmax + 1, 100):
        svg.line(left, yb(v), right, yb(v), GRID)
        svg.text(left - 8, yb(v) + 4, str(v), anchor="end", fill=MUTED)
    for i, (d, p) in enumerate(LANDED):
        after = p if d in BACKFILLED else AFTERWARDS.get(d, 0)
        est = p - after
        if est:
            svg.rect(cx(i) - slot * 0.32, yb(est), slot * 0.64, yb(0) - yb(est), ACTIVE_C)
        if after:
            svg.rect(cx(i) - slot * 0.32, yb(p), slot * 0.64, yb(est) - yb(p), "url(#hatch)")
    placed = []
    for i, (_, p) in sorted(enumerate(LANDED), key=lambda e: -e[1][1]):
        half = 3 * len(str(p))
        if p >= LABEL_FROM and all(
                abs(cx(i) - qx) >= half + qh or abs(yb(p) - qy) >= 12
                for qx, qy, qh in placed):
            svg.text(cx(i), yb(p) - 5, str(p), anchor="middle", size=10)
            placed.append((cx(i), yb(p), half))
    weeks(bottom)

    # Panel 3: the running total, on its own axis because it is another scale.
    top, bottom = 630, 762
    svg.text(left, 604, "Running total of points landed", size=16, weight="bold")
    total = sum(p for _, p in LANDED)
    tmax = -(-total // 1000) * 1000
    yc = lambda v: bottom - (bottom - top) * v / tmax
    for v in range(0, tmax + 1, 1000):
        svg.line(left, yc(v), right, yc(v), GRID)
        svg.text(left - 8, yc(v) + 4, f"{v:,}", anchor="end", fill=MUTED)
    run, pts = 0, []
    for i, (_, p) in enumerate(LANDED):
        run += p
        pts.append((cx(i), yc(run)))
    svg.polyline(pts, LINE_C[1])
    svg.dot(*pts[-1], LINE_C[1], r=4)
    svg.text(pts[-1][0] - 8, pts[-1][1] - 8, f"≈ {total:,}", anchor="end", weight="bold")
    weeks(bottom)
    svg.write(OUT / "burndown.svg")


def gantt():
    # One queue at FORECAST_RATE, in the status table's order.
    forecast, t = [], 0.0
    for name, pts, state in REMAINING:
        d = pts / FORECAST_RATE
        note = f", {state}" if state else ""
        forecast.append((f"{name} ({pts}{note})", t, t + d, state))
        t += d
    start_day = date(2026, 9, 13)
    queue_start = TODAY + timedelta(days=1)
    end_day = queue_start + timedelta(days=int(t) + 1)
    span = (end_day - start_day).days

    rows = len(DONE) + len(ACTIVE) + len(forecast)
    row_h, sec_h = 22, 30
    w = 1040
    left, right = 310, w - 140
    top = 94
    h = top + rows * row_h + 3 * sec_h + 50
    svg = Svg(w, h, "Ferrix Gantt")
    svg.text(24, 28, "Ferrix: done, in progress, and a forecast", size=16,
             weight="bold")
    svg.text(24, 46, f"The forecast is one queue at {FORECAST_RATE} points a day, "
             "built rows first, then shortest first: the size of the work, not a plan; "
             "hollow rows have no session on them",
             fill=MUTED)
    svg.text(24, 62, "In progress: solid is the time since it started; the faint bar is the "
             "wait for the queue below, the light dashed bar the work itself",
             fill=MUTED)
    x = lambda days: left + (right - left) * days / span
    bottom = h - 40
    every = 7 * math.ceil(40 * span / (right - left) / 7)  # weeks a date's label fits in
    for i in range(0, span + 1, every):
        svg.line(x(i), top, x(i), bottom, GRID)
        if i < span:
            svg.text(x(i), bottom + 16, label(start_day + timedelta(days=i)),
                     anchor="middle", fill=MUTED, size=11)
    tx = x((queue_start - start_day).days)
    svg.line(tx, top - 6, tx, bottom, TODAY_C, width=1.5, dash="4 3")
    svg.text(tx, top - 10, f"today, {label(TODAY)}", anchor="middle", fill=TODAY_C,
             weight="bold")

    yy = top

    def section(title):
        nonlocal yy
        yy += sec_h
        svg.text(24, yy - 9, title, weight="bold", size=13)

    def bar(name, a, b, fill, stroke=None, dash=None):
        nonlocal yy
        svg.text(left - 10, yy + row_h / 2 + 4, name, anchor="end")
        svg.rect(x(a), yy + 4, max(x(b) - x(a), 3), row_h - 8, fill, stroke, dash)
        yy += row_h

    day = lambda d: (d - start_day).days
    section("Done")
    for name, a, b in sorted(DONE, key=lambda r: (r[1], r[2])):
        bar(name, day(a), day(b) + 1, DONE_C)
    section("In progress")
    qd = day(queue_start)
    sched = {r[0]: (f[1], f[2]) for r, f in zip(REMAINING, forecast)}
    pts_of = {n: p for n, p, _ in REMAINING}
    def remainder_end(items):
        return max(sched[n][1] for n in items) + qd
    # Drawn by start day, ties by where the remainder ends; the data's order
    # and the forecast queue's order (the status table's) are not changed.
    for name, a, items in sorted(ACTIVE, key=lambda r: (r[1], remainder_end(r[2]))):
        bar(name, day(a), qd, ACTIVE_C)
        by = yy - row_h
        segs = sorted(sched[n] for n in items)
        s0, e = qd + segs[0][0], qd + segs[-1][1]
        left_pts = sum(pts_of[n] for n in items)
        # The wait: from today to where the row's work starts in the queue.
        if s0 > qd:
            svg.rect(x(qd), by + 4, x(s0) - x(qd), row_h - 8, "#eef6fd",
                     "#b6d4f2", "1 3")
        for t0, t1 in segs:
            svg.rect(x(qd + t0), by + 4, max(x(qd + t1) - x(qd + t0), 3),
                     row_h - 8, "#b6e3ff", ACTIVE_C, "4 2")
        svg.text(x(e) + 6, by + row_h / 2 + 4,
                 f"{left_pts:,} pts left, ~{label(start_day + timedelta(days=math.ceil(e)))}",
                 fill=ACTIVE_C, size=11)
    section(f"Forecast, {FORECAST_RATE} a day, one queue")
    q = day(queue_start)
    for name, a, b, state in forecast:
        # An idle row has no session on it, so the queue does not reach it
        # until someone is put on it; it is drawn hollow.
        fill = "#ffffff" if "idle" in state else FORECAST_C
        bar(name, q + a, q + b, fill, ACTIVE_C, "3 2")
    svg.text(x(q + t) + 6, yy - row_h / 2 + 4,
             label(queue_start + timedelta(days=t)), fill=ACTIVE_C, weight="bold")
    svg.write(OUT / "gantt.svg")


if __name__ == "__main__":
    burndown()
    gantt()
    print(f"wrote {OUT / 'burndown.svg'} and {OUT / 'gantt.svg'}")
