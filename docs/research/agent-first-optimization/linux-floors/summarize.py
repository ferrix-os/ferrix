#!/usr/bin/env python3
"""Summarize po7-linux rounds from the raw logs in logs/po7-linux.

Linux: per boot, the median over its 5 repeats of each percentile.
Ferrix: bench-ipc's own lines (one run of 20,000 samples per boot), ns.
seL4: per boot, the median over its runs of call-replyrecv-roundtrip.
All in ns (guest TSC 4,400 MHz; seL4 ticks / its tsc_mhz).
Across rounds: the median of the per-round figures, with min..max, and the
median of per-round ratios.
"""
import glob, os, re, statistics, sys

L = os.path.expanduser(sys.argv[1] if len(sys.argv) > 1 else "~/.local/share/ferrix/logs/po7-linux")
KV = re.compile(r"(\w+)=([0-9.]+)")


def kv(line):
    return {k: float(v) for k, v in KV.findall(line)}


def meta_load(path):
    try:
        text = open(path + ".meta").read()
    except OSError:
        return None, None
    m = re.search(r"loadavg=([0-9.]+)", text)
    s = re.search(r"busy=(\d+)%", text)
    return (float(m.group(1)) if m else None), (int(s.group(1)) if s else None)


def linux(path):
    out = {}
    mhz = 4400.0
    for line in open(path, errors="replace"):
        if line.startswith("LBINFO tsc_mhz"):
            mhz = float(line.split()[-1])
        if line.startswith("LB name="):
            name = line.split()[1][5:]
            d = kv(line)
            out.setdefault(name, []).append(d)
    res = {}
    for name, reps in out.items():
        res[name] = {p: statistics.median(r[p] for r in reps) * 1000 / mhz for p in ("min", "p50", "p90", "p99")}
    return res


def ferrix(path):
    res = {}
    for line in open(path, errors="replace"):
        m = re.search(r"x86_64: ipc-bench (floor|call|domain-call|trip) (n=.*)", line)
        if m:
            d = kv(m.group(2))
            res[m.group(1)] = {p: d[p] for p in ("min", "p50", "p90", "p99")}
    m = None
    for line in open(path, errors="replace"):
        m = re.search(r"host load ([0-9.]+) before.*cpu\d+ (\d+)% busy", line) or m
    return res, (float(m.group(1)), int(m.group(2))) if m else (None, None)


def sel4(path):
    text = re.sub(r"\x1b\[[0-9;]*m", "", open(path, errors="replace").read())
    mhz = float(re.search(r"tsc_mhz=(\d+)", text).group(1))
    rows = [kv(l) for l in text.splitlines() if "call-replyrecv-roundtrip" in l]
    return {"roundtrip": {p: statistics.median(r[p] for r in rows) * 1000 / mhz for p in ("min", "p50", "p90", "p99")}}, mhz


rounds = sorted({int(m.group(1)) for f in glob.glob(L + "/r*-*.log") if (m := re.match(r"r(\d+)-", os.path.basename(f)))})
table = {}  # (system, case) -> list of per-round dicts
loads = {}
for r in rounds:
    for cfg in ("default", "off"):
        p = f"{L}/r{r}-linux-{cfg}.log"
        if os.path.exists(p):
            for case, d in linux(p).items():
                table.setdefault((f"Linux {cfg}", case), {})[r] = d
            loads.setdefault(f"Linux {cfg}", []).append(meta_load(p))
    for cfg in ("on", "off"):
        p = f"{L}/r{r}-ferrix-{cfg}.log"
        if os.path.exists(p):
            res, load = ferrix(p)
            for case, d in res.items():
                table.setdefault((f"Ferrix mitigations {cfg}", case), {})[r] = d
            loads.setdefault(f"Ferrix mitigations {cfg}", []).append(load)
    for b in ("matched-nopcid", "norsb-nopcid"):
        p = f"{L}/r{r}-sel4-{b}.log"
        if os.path.exists(p):
            res, _ = sel4(p)
            for case, d in res.items():
                table.setdefault((f"seL4 {b}", case), {})[r] = d
            loads.setdefault(f"seL4 {b}", []).append(meta_load(p))


def fmt(vals):
    return f"{statistics.median(vals):,.0f} ({min(vals):,.0f}–{max(vals):,.0f})"


ref_f = table.get(("Ferrix mitigations on", "domain-call"), {})
ref_s = table.get(("seL4 matched-nopcid", "roundtrip"), {})


def ratio(per_round, ref):
    rs = [per_round[r]["p50"] / ref[r]["p50"] for r in per_round if r in ref]
    return f"{statistics.median(rs):.2f} ({min(rs):.2f}–{max(rs):.2f})" if rs else "–"


print(f"rounds: {rounds}")
print("| system | case | p50 ns | p90 ns | p99 ns | min ns | ÷ Ferrix domain-call | ÷ seL4 matched |")
print("|---|---|---|---|---|---|---|---|")
for (system, case), per_round in table.items():
    v = list(per_round.values())
    print(f"| {system} | {case} | {fmt([d['p50'] for d in v])} | {fmt([d['p90'] for d in v])} | "
          f"{fmt([d['p99'] for d in v])} | {fmt([d['min'] for d in v])} | {ratio(per_round, ref_f)} | {ratio(per_round, ref_s)} |")
print()
print("| system | 1-min load at start (median, range) | SMT sibling busy % (range) |")
print("|---|---|---|")
for system, ls in loads.items():
    l = [x for x, _ in ls if x is not None]
    s = [y for _, y in ls if y is not None]
    print(f"| {system} | {statistics.median(l):.2f} ({min(l):.2f}–{max(l):.2f}) | {min(s)}–{max(s)} |" if l and s else f"| {system} | ? | ? |")
