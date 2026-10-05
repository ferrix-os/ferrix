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

TODAY = date(2026, 10, 5)

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
]
BACKFILLED = {date(2026, 9, d) for d in range(18, 24)}
AFTERWARDS = {date(2026, 9, 24): 27, date(2026, 9, 25): 26, date(2026, 9, 26): 175,
              date(2026, 9, 27): 150, date(2026, 9, 28): 44, date(2026, 9, 29): 26,
              date(2026, 9, 30): 83, date(2026, 10, 1): 94, date(2026, 10, 2): 128,
              date(2026, 10, 3): 115, date(2026, 10, 4): 55,
              date(2026, 10, 5): 29}

# The status table's sized, unfinished rows after 2026-10-05's landings.
# The state says why a row is not moving: "unlanded" is built and waits on a
# landing (controls, `check`, a batch), "idle" has had no session on it since
# 2026-09-26 or longer, "added" came into scope after the 09-26 baseline.
REMAINING = [
    ("Client pages as texture backing", 8, "idle"),
    ("Second pass and xray", 8, ""),
    ("GC400, the rest", 21, "idle"),
    ("Stage 13, the controllers' rest", 30, "unlanded"),
    ("Stage 15, auth's rest", 6, ""),
    # L13b landed 2026-10-05; L13c is built (po6/l13c) and waits to land.
    ("Stage 15, init L13c", 3, "added, unlanded"),
    ("Chrome on the DK1", 50, "idle"),
    ("Stage 14, real-time", 40, "idle"),
    ("dmabuf and virgl", 48, "idle"),
    # Re-sized 2026-10-05 from 5: the consultant found B1 and B2 (parked).
    ("Stage 22, bubblewrap's rest", 16, ""),
    ("Stage 22, the rest (guess)", 23, ""),
    ("NVIDIA N5, CUDA", 52, "added"),
    # Sized 2026-10-04 evening, by three sizing agents; ranges 40-75, 30-50
    # and 130-210. Most milestones are guesses.
    ("Stage 20, self-hosting", 45, "added"),
    ("Pixel 7, the USB driver's rest", 37, "added"),
    ("Certification findings, in-repository work", 165, "added"),
    # Added 2026-10-05 at the customer's question; a first guess (D1-D5).
    ("Live driver update", 26, "added"),
    # OPAQUE-KERNEL.md §9.5/§9.7, what is left at 2026-10-05: 2f (gated),
    # step 3 without PCIDs, step 4's 39-59, step 5; range 60-90. The exact
    # bench-ipc (1) landed 2026-10-05, so 70 became 69.
    ("IPC round trip to seL4's figure", 69, "added"),
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
RATES = [(BURN_RATE, f"{BURN_RATE} a day, what came off the 09-26 scope, 09-27 to 10-04"),
         (56, "56 a day, if every estimated point came off it")]
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
    ("Claude Code on Ferrix", D(2026, 9, 29), D(2026, 10, 1)),
    ("Speculation domain, round trip", D(2026, 10, 1), D(2026, 10, 3)),
    ("btrfs in the certified item", D(2026, 10, 2), D(2026, 10, 2)),
    ("NVIDIA N0 and N1 (60)", D(2026, 10, 2), D(2026, 10, 3)),
    ("NVIDIA N2 to N4 and the TV (64)", D(2026, 10, 3), D(2026, 10, 5)),
    ("Authentication P1, P2 (desktop as a user)", D(2026, 9, 27), D(2026, 10, 4)),
    ("Network namespaces", D(2026, 10, 1), D(2026, 10, 4)),
    ("Components and apps in repositories", D(2026, 10, 3), D(2026, 10, 4)),
]
# Each row: name, first day, and the REMAINING entries its unfinished work is
# (by name), or None when the row has no size. The remainder is drawn to the
# end of the last matching entry in the forecast queue, so the two agree.
ACTIVE = [
    ("Stage 19, the rest", D(2026, 9, 17),
     ["Client pages as texture backing", "Second pass and xray"]),
    ("Stage 20, self-hosting", D(2026, 9, 22), ["Stage 20, self-hosting"]),
    ("Stage 13: the controllers (S3 landed)", D(2026, 9, 23),
     ["Stage 13, the controllers' rest"]),
    ("Gears (50 of 71)", D(2026, 9, 24), ["GC400, the rest"]),
    ("Certification findings", D(2026, 9, 25),
     ["Certification findings, in-repository work"]),
    ("Pixel 7: USB beyond the console driver", D(2026, 9, 26),
     ["Pixel 7, the USB driver's rest"]),
    ("Steam: the game step", D(2026, 9, 30),
     ["Stage 22, bubblewrap's rest", "Stage 22, the rest (guess)"]),
    ("Init L13c (L13b landed)", D(2026, 10, 4), ["Stage 15, init L13c"]),
    ("Auth P2, the rest (P2.1 landed)", D(2026, 10, 4), ["Stage 15, auth's rest"]),
    ("IPC round trip (steps 1, 2a-2e landed)", D(2026, 10, 1),
     ["IPC round trip to seL4's figure"]),
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
    w, h = 880, 640
    svg = Svg(w, h, "Ferrix burndown")
    left, right = 70, w - 70

    # Panel 1: sized points remaining, forecast from today.
    top, bottom = 60, 290
    svg.text(left, 28, f"Sized points remaining at the end of each day, from {label(TODAY)}",
             size=16, weight="bold")
    added = sum(p for _, p, s in REMAINING if "added" in s)
    svg.text(left, 46, f"{SCOPE} points in the status table's sized, unfinished rows, "
             f"{added} of them added since 09-26; unsized work is outside it, and "
             "scope added later moves the date",
             size=12, fill=MUTED)
    days = math.ceil(SCOPE / FORECAST_RATE) + 2
    ymax = -(-SCOPE // 100) * 100  # the axis reaches the scope, in hundreds
    x = lambda i: left + (right - left) * i / (days - 1)
    y = lambda v: bottom - (bottom - top) * v / ymax
    for v in range(0, ymax + 1, 100):
        svg.line(left, y(v), right, y(v), GRID)
        svg.text(left - 8, y(v) + 4, str(v), anchor="end", fill=MUTED)
    for i in range(0, days, 2):
        svg.text(x(i), bottom + 18, label(TODAY + timedelta(days=i)),
                 anchor="middle", fill=MUTED)
    svg.text(18, (top + bottom) / 2, "points", fill=MUTED,
             anchor="middle")
    for (rate, name), colour, ly in zip(RATES, LINE_C, (top + 8, top + 26)):
        # Whole days while work is left, then the day it runs out, exactly.
        pts = [(x(i), y(SCOPE - rate * i)) for i in range(days) if SCOPE - rate * i > 0]
        pts.append((x(SCOPE / rate), y(0)))
        svg.polyline(pts, colour, width=2.5)
        for px, py in pts:
            svg.dot(px, py, colour)
        # A tick is the end of its day, so the work runs out during the day
        # after the last tick it has passed.
        end = TODAY + timedelta(days=math.ceil(SCOPE / rate))
        svg.line(right - 420, ly, right - 396, ly, colour, width=2.5)
        svg.text(right - 388, ly + 4, f"{name}: done {label(end)}")

    # Panel 2: what has landed, per day and in total.
    top, bottom = 390, 590
    svg.text(left, 350, "Points landed per day, and the running total", size=16,
             weight="bold")
    svg.text(left, 368, "hatched: sized afterwards from git log, "
             "not estimated before the work started", size=12, fill=MUTED)
    svg.add('<defs><pattern id="hatch" width="6" height="6" '
            'patternUnits="userSpaceOnUse" patternTransform="rotate(45)">'
            f'<rect width="6" height="6" fill="{FORECAST_C}"/>'
            '<line x1="0" y1="0" x2="0" y2="6" stroke="#ffffff" stroke-width="2"/>'
            '</pattern></defs>')
    n = len(LANDED)
    slot = (right - left) / n
    bmax, cmax = 300, 2400
    yb = lambda v: bottom - (bottom - top) * v / bmax
    yc = lambda v: bottom - (bottom - top) * v / cmax
    for v in range(0, bmax + 1, 50):
        svg.line(left, yb(v), right, yb(v), GRID)
        svg.text(left - 8, yb(v) + 4, str(v), anchor="end", fill=MUTED)
    for v in range(0, cmax + 1, 400):
        svg.text(right + 8, yc(v) + 4, str(v), fill=LINE_C[1])
    svg.text(18, (top + bottom) / 2, "a day", fill=MUTED, anchor="middle")
    svg.text(w - 18, (top + bottom) / 2, "total", fill=LINE_C[1], anchor="middle")
    total, pts = 0, []
    for i, (d, p) in enumerate(LANDED):
        cx = left + slot * (i + 0.5)
        after = p if d in BACKFILLED else AFTERWARDS.get(d, 0)
        est = p - after
        if est:
            svg.rect(cx - slot * 0.32, yb(est), slot * 0.64, yb(0) - yb(est), ACTIVE_C)
        if after:
            svg.rect(cx - slot * 0.32, yb(p), slot * 0.64, yb(est) - yb(p), "url(#hatch)")
        svg.text(cx, yb(p) - 5, str(p), anchor="middle", size=10)
        svg.text(cx, bottom + 18, label(d), anchor="middle", fill=MUTED, size=9)
        total += p
        pts.append((cx, yc(total)))
    svg.polyline(pts, LINE_C[1], width=2.5)
    for px, py in pts:
        svg.dot(px, py, LINE_C[1])
    svg.text(pts[-1][0] - 8, pts[-1][1] - 8, f"≈ {total}", anchor="end",
             fill=LINE_C[1], weight="bold")
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
    top = 82
    h = top + rows * row_h + 3 * sec_h + 50
    svg = Svg(w, h, "Ferrix Gantt")
    svg.text(24, 28, "Ferrix: done, in progress, and a forecast", size=16,
             weight="bold")
    svg.text(24, 46, f"The forecast is one queue at {FORECAST_RATE} points a day, "
             "built rows first, then shortest first: the size of the work, not a plan; "
             "hollow rows have no session on them",
             fill=MUTED)
    svg.text(24, 60, "In progress: solid is done; the faint bar is the wait for the queue "
             "below (built rows first, then shortest first), the light dashed bar "
             "the work itself; a dotted bar to the edge is unsized work",
             fill=MUTED)
    x = lambda days: left + (right - left) * days / span
    bottom = h - 40
    for i in range(span + 1):
        d = start_day + timedelta(days=i)
        svg.line(x(i), top, x(i), bottom, GRID if i % 7 else "#afb8c1")
        if i % 2 == 0 and i < span:
            svg.text(x(i) + 2, bottom + 16, label(d), fill=MUTED, size=11)
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
        return (span if items is None
                else max(sched[n][1] for n in items) + qd)
    # Drawn by start day, ties by where the remainder ends; the data's order
    # and the forecast queue's order (the status table's) are not changed.
    for name, a, items in sorted(ACTIVE, key=lambda r: (r[1], remainder_end(r[2]))):
        bar(name, day(a), qd, ACTIVE_C)
        by = yy - row_h
        if items is None:
            svg.rect(x(qd), by + 4, x(span) - x(qd), row_h - 8, "#ddf4ff", ACTIVE_C, "1 3")
            svg.text(x(qd) + 6, by + row_h / 2 + 4, "remaining unknown (unsized)",
                     fill=ACTIVE_C, size=11)
        else:
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
                     f"{left_pts} pts left, ~{label(start_day + timedelta(days=math.ceil(e)))}",
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
