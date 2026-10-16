#!/usr/bin/env python3
"""Measure where Ferrix's technical debt is, from git and the tree.

    python3 tools/common/gen/debt-report.py [--days 30] [--top 15] [--flakes-log PATH]

prints a Markdown report of the signals docs/TECH-DEBT.md §2 defines, over
`main`'s first-parent history of the last --days days:

  1. hotspots: changes x lines per file, with the share of its changes made
     by fix commits (a message naming a fix, flake, race, hang, stall,
     deadlock, bug or FX- panic number);
  2. coupling: pairs of files, one of them code, that change together in
     most of the commits the rarer of the two is in;
  3. landing breadth: files per commit, median and 90th percentile;
  4. flake sources: docs/backlog/flakes.md's items by where they fail, and
     the batch runner's flakes.log, when it exists (fleet/batch/, outside the
     tree), by gate;
  5. ratchets: the entry counts of tools/common/data's debt registers now
     and --days ago, which should only go down;
  6. waits on time: sleep calls in tests and test drivers, and the share of
     the kernel's source that is boot-time self-checks.

It reads and writes nothing else, uses the standard library only, and is
for the product owner's weekly round (docs/TECH-DEBT.md §3): the numbers
are heuristics to point at files, not verdicts on them. A fix commit is
told by words in its message, so the fix share is approximate.
"""

import argparse
import collections
import json
import os
import re
import statistics
import subprocess
import sys
from datetime import date, timedelta
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent.parent
CODE = (".rs", ".py", ".sh", ".c", ".h", ".S", ".s")
FIX = re.compile(r"\b(fix|fixe[sd]|fixing|flake|flaky|race|hang|hung|stall|deadlock|"
                 r"regression|bug|lost wake|revert|FX-\d{4})\b", re.I)
# Files every landing regenerates or appends to: reported as coupling, kept
# out of the hotspot table, where their size says nothing about their code.
GENERATED = re.compile(r"^(docs/generated/|docs/certification/coverage-|docs/img/)")
REGISTERS = {
    "complexity-baseline.json": ("functions",),
    "traceability-baseline.json": ("unverified",),
    "fallible-alloc-baseline.json": ("files", "crates"),
    "unsafe-trace-baseline.json": ("files",),
    "asm-allowlist.json": ("files",),
    "device-access-allowlist.json": ("files",),
}
SLEEP = re.compile(r"thread::sleep|sleep\(\s*Duration|time\.sleep\(")


def git(*args):
    return subprocess.run(["git", "-C", str(ROOT), *args], check=True,
                          capture_output=True, text=True).stdout


def commits(since):
    """[(is_fix, [files])] of main's first parent since the date."""
    out = git("log", "--first-parent", "main", f"--since={since}",
              "--format=\x01%B\x02", "--name-only")
    result = []
    for block in out.split("\x01")[1:]:
        message, _, names = block.partition("\x02")
        result.append((bool(FIX.search(message)), [f for f in names.split("\n") if f]))
    return result


def lines_of(path):
    try:
        with open(ROOT / path, "rb") as f:
            return sum(1 for _ in f)
    except OSError:
        return 0


def hotspots(log, top):
    changes = collections.Counter()
    fixes = collections.Counter()
    for is_fix, files in log:
        for f in files:
            changes[f] += 1
            fixes[f] += is_fix
    rows = []
    for f, n in changes.items():
        if not f.endswith(CODE) or GENERATED.match(f):
            continue
        size = lines_of(f)
        if size:
            rows.append((n * size, n, fixes[f], size, f))
    rows.sort(reverse=True)
    out = ["## 1. Hotspots: changes x lines", "",
           "| Score | Changes | Fix share | Lines | File |", "|---:|---:|---:|---:|---|"]
    for score, n, fx, size, f in rows[:top]:
        out.append(f"| {score:,} | {n} | {100 * fx // n}% | {size:,} | `{f}` |")
    return out


def coupling(log, top, min_together=8):
    alone = collections.Counter()
    pairs = collections.Counter()
    for _, files in log:
        files = sorted(set(files))
        if len(files) > 40:  # a relayout or a bulk regeneration says nothing
            continue
        alone.update(files)
        for i, a in enumerate(files):
            for b in files[i + 1:]:
                pairs[(a, b)] += 1
    rows = []
    for (a, b), n in pairs.items():
        if n < min_together or not (a.endswith(CODE) or b.endswith(CODE)):
            continue
        share = n / min(alone[a], alone[b])
        if share >= 0.5:
            rows.append((n, share, a, b))
    rows.sort(reverse=True)
    out = ["## 2. Coupling: files that change together", "",
           f"Pairs, one of them code, in at least {min_together} commits together and in at least half of the "
           "commits the rarer of the two is in. Generated documents always change together; section 3 counts them.", "",
           "| Together | Share | File | File |", "|---:|---:|---|---|"]
    for n, share, a, b in rows[:top]:
        out.append(f"| {n} | {share:.0%} | `{a}` | `{b}` |")
    return out


def breadth(log):
    sizes = sorted(len(files) for _, files in log if files)
    if not sizes:
        return ["## 3. Landing breadth", "", "No commits in the window."]
    p90 = sizes[min(len(sizes) - 1, int(len(sizes) * 0.9))]
    generated = sum(1 for _, files in log if any(GENERATED.match(f) for f in files))
    return ["## 3. Landing breadth", "",
            f"{len(sizes)} commits: median {statistics.median(sizes):g} files, 90th percentile {p90}; "
            f"{generated} ({100 * generated // len(sizes)}%) touch a generated document or coverage file."]


def flake_sources(flakes_log):
    out = ["## 4. Flake sources", ""]
    path = ROOT / "docs/backlog/flakes.md"
    if path.exists():
        items = re.split(r"\n### ", path.read_text(encoding="utf-8"))[1:]
        kinds = collections.Counter()
        loaded = 0
        for item in items:
            s = item.lower()
            if "done; removed from the backlog" in s:
                continue
            if re.search(r"self-check|check\.rs|stages_check|ferrix-panic|stage \d+'s", s):
                kinds["kernel boot self-check"] += 1
            elif re.search(r"hyprix|waybar|compositor|two_clients|screenshot", s):
                kinds["compositor or desktop test"] += 1
            elif re.search(r"xtask|gateway|qmp|harness|\bci\b", s):
                kinds["xtask or host harness"] += 1
            else:
                kinds["other"] += 1
            if re.search(r"load (of )?(about )?\d\d", s):
                loaded += 1
        total = sum(kinds.values())
        out.append(f"`docs/backlog/flakes.md`: {total} open items; "
                   + ", ".join(f"{k} {v}" for k, v in kinds.most_common())
                   + f"; {loaded} record a host load of 10 or more.")
    if flakes_log and Path(flakes_log).exists():
        gates = collections.Counter()
        for line in Path(flakes_log).read_text(encoding="utf-8").splitlines():
            m = re.match(r"\S+ batch \S+ \S+ (.*?) \| log ", line)
            if m:
                gates[m.group(1)] += 1
        out += ["", f"`{flakes_log}`: {sum(gates.values())} flaky gate runs a batch passed on the rerun."]
        out += [f"* {n} x `{g}`" for g, n in gates.most_common(10)]
    return out


def count_register(text, keys):
    try:
        data = json.loads(text)
    except ValueError:
        return None
    n = 0
    for k in keys:
        v = data.get(k)
        if isinstance(v, (dict, list)):
            n += sum(len(x) if isinstance(x, (dict, list)) and k == "unverified" else 1 for x in (v.values() if isinstance(v, dict) else v))
    return n


def ratchets(days):
    old = git("rev-list", "-1", "--first-parent", f"--before={date.today() - timedelta(days=days)}", "main").strip()
    out = ["## 5. Ratchets: the debt registers", "",
           f"Entries now and on `main` {days} days ago ({old[:9] or 'none'}); each should only go down.", "",
           "| Register | Then | Now |", "|---|---:|---:|"]
    for name, keys in REGISTERS.items():
        rel = f"tools/common/data/{name}"
        now_path = ROOT / rel
        now = count_register(now_path.read_text(encoding="utf-8"), keys) if now_path.exists() else None
        then = None
        if old:
            try:
                then = count_register(git("show", f"{old}:{rel}"), keys)
            except subprocess.CalledProcessError:
                then = None
        out.append(f"| `{name}` | {'—' if then is None else then} | {'—' if now is None else now} |")
    return out


def waits_on_time(top):
    files = git("ls-files", "*.rs", "*.py").split()
    per = collections.Counter()
    for f in files:
        if not re.search(r"(^|/)(tests?|xtask|compositor)(/|\.rs$|_)|test", f):
            continue
        try:
            text = (ROOT / f).read_text(encoding="utf-8", errors="ignore")
        except OSError:
            continue
        n = len(SLEEP.findall(text))
        if n:
            per[f] = n
    kernel = [f for f in files if f.startswith("src/kernel/") and f.endswith(".rs")]
    check = [f for f in kernel if "check" in os.path.basename(f)]
    k_lines = sum(lines_of(f) for f in kernel)
    c_lines = sum(lines_of(f) for f in check)
    out = ["## 6. Waits on time", "",
           f"{sum(per.values())} sleep calls in {len(per)} test files or drivers. Most sleeps are waits on time, not on state, and those flake under load.", ""]
    out += [f"* {n} in `{f}`" for f, n in per.most_common(top)]
    out += ["", f"Kernel boot self-checks: {len(check)} `*check*.rs` files, {c_lines:,} of the kernel's "
            f"{k_lines:,} lines ({100 * c_lines // max(k_lines, 1)}%), compiled in and run at every boot."]
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--days", type=int, default=30)
    ap.add_argument("--top", type=int, default=15)
    ap.add_argument("--flakes-log", default=os.path.expanduser("~/.local/share/ferrix/fleet/batch/flakes.log"))
    a = ap.parse_args()
    since = (date.today() - timedelta(days=a.days)).isoformat()
    log = commits(since)
    head = git("rev-parse", "--short=9", "main").strip()
    parts = [[f"# Technical debt report, {date.today()}", "",
              f"`main` {head}, the {len(log)} first-parent commits since {since}; "
              "what the signals mean and what to do with them: `docs/TECH-DEBT.md`."],
             hotspots(log, a.top), coupling(log, a.top), breadth(log),
             flake_sources(a.flakes_log), ratchets(a.days), waits_on_time(a.top)]
    sys.stdout.write("\n\n".join("\n".join(p) for p in parts) + "\n")


if __name__ == "__main__":
    main()
