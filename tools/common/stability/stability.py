#!/usr/bin/env python3
"""The nightly stability suite's records, summary and trend (docs/STABILITY.md §4).

Three commands:

  run     run one job's command (once, or --repeat N times), keep each log,
          time it, and write one JSON record and a section of the job summary;
  report  read every record of a run and the last night's history, write the
          run's summary with the change since the night before, and the
          history with this night added;
  verdict print "red" or "green" for a history's newest night, and the rows
          that made it red, for the workflow's rolling issue.

The record of a job that needs a part not landed yet says `skipped` and why;
it is never red. Only the standard library, so the workflow needs no install.
"""

from __future__ import annotations

import argparse
import datetime
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

# The lines that are a stop, first match wins as the job's first failure.
STOP = re.compile(r"(FX-\d{4}|FERRIX-PANIC|kernel panic|panicked at|watch: (cpu|task) )")
# Lines that say why a row failed, when no stop line did.
FAILED = re.compile(r"(failed|error:|Error:|did not|timed out)", re.IGNORECASE)
# How many nights the history keeps.
NIGHTS = 120
# A line of a test's result, as the stability runner prints it:
# `stability: test <name> <pass|fail|skip|broken|timeout> <status> <ms>`.
TEST_LINE = re.compile(r"stability: test (\S+) (pass|fail|skip|broken|timeout)\b")


def now_utc() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def first_failure(text: str) -> str:
    """The line that best says why a run failed: a stop, else a failure line."""
    lines = text.splitlines()
    for line in lines:
        if STOP.search(line):
            return line.strip()[:400]
    for line in reversed(lines):
        if FAILED.search(line):
            return line.strip()[:400]
    return (lines[-1].strip() if lines else "no output")[:400]


def summary_append(text: str) -> None:
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if path:
        with open(path, "a", encoding="utf-8") as out:
            out.write(text + "\n")
    else:
        print(text)


def tests_from(path: Path | None, text: str) -> dict[str, str]:
    """Per-test results: from a JSON file the row wrote, else from the log."""
    if path and path.is_file():
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
            return {str(k): str(v) for k, v in data.get("tests", {}).items()}
        except (OSError, ValueError):
            pass
    found: dict[str, str] = {}
    for match in TEST_LINE.finditer(text):
        found[match.group(1)] = match.group(2)
    return found


def cmd_run(args: argparse.Namespace) -> int:
    out = Path(args.out)
    logs = out / "logs"
    logs.mkdir(parents=True, exist_ok=True)
    name = f"{args.job}-{args.arch}"
    record: dict = {
        "job": args.job,
        "arch": args.arch,
        "sha": os.environ.get("GITHUB_SHA", ""),
        "started": now_utc(),
        "command": " ".join(args.command),
    }
    if args.skip:
        record.update(status="skipped", seconds=0, counts={}, first_failure="", why=args.skip)
        write_record(out, name, record)
        summary_append(f"### {args.job} ({args.arch}): skipped\n\n{args.skip}\n")
        return 0
    if not args.command:
        print("stability.py run: no command after --", file=sys.stderr)
        return 2
    passes = fails = 0
    first = ""
    began = time.monotonic()
    texts = []
    for attempt in range(1, args.repeat + 1):
        log = logs / (f"{name}-{attempt}.log" if args.repeat > 1 else f"{name}.log")
        started = time.monotonic()
        with open(log, "w", encoding="utf-8", errors="replace") as sink:
            proc = subprocess.run(args.command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                  text=True, errors="replace", check=False)
            sink.write(proc.stdout)
        sys.stdout.write(proc.stdout[-20000:])
        texts.append(proc.stdout)
        took = time.monotonic() - started
        if proc.returncode == 0:
            passes += 1
        else:
            fails += 1
            if not first:
                where = f"run {attempt}: " if args.repeat > 1 else ""
                first = where + first_failure(proc.stdout)
                if args.keep:
                    for keep in args.keep:
                        src = Path(keep)
                        if src.is_file():
                            (logs / f"{name}-{attempt}-{src.name}").write_bytes(src.read_bytes())
        print(f"stability: {name} run {attempt}/{args.repeat}: "
              f"{'pass' if proc.returncode == 0 else 'fail'} in {took:.0f} s", flush=True)
    seconds = round(time.monotonic() - began)
    text = "\n".join(texts)
    counts = {"pass": passes, "fail": fails}
    tests = tests_from(Path(args.tests) if args.tests else None, text)
    if tests:
        # A job whose failures are data (LTP, kselftest): the counts are its
        # tests', and only the command's own failure -- a stop -- is red.
        counts = {}
        for result in tests.values():
            counts[result] = counts.get(result, 0) + 1
    status = "fail" if fails else "pass"
    if args.data and fails and not STOP.search(text):
        # The row said "failed" because some tests failed: data, not red.
        status = "pass"
        first = ""
    record.update(status=status, seconds=seconds, counts=counts, first_failure=first)
    if tests:
        record["tests"] = tests
    write_record(out, name, record)
    shown = ", ".join(f"{k} {v}" for k, v in sorted(counts.items()))
    summary_append(f"### {args.job} ({args.arch}): {status}\n\n{shown}; {seconds} s"
                   + (f"\n\nFirst failure: `{first}`" if first else "") + "\n")
    return 0 if status == "pass" or args.never_fail else 1


def write_record(out: Path, name: str, record: dict) -> None:
    (out / f"record-{name}.json").write_text(json.dumps(record, indent=1) + "\n", encoding="utf-8")


def load_history(path: Path) -> list[dict]:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        return data.get("nights", []) if isinstance(data, dict) else []
    except (OSError, ValueError):
        return []


def key(record: dict) -> str:
    return f"{record.get('job')}/{record.get('arch')}"


def cmd_report(args: argparse.Namespace) -> int:
    records = []
    for path in sorted(Path(args.records).rglob("record-*.json")):
        try:
            records.append(json.loads(path.read_text(encoding="utf-8")))
        except (OSError, ValueError) as error:
            print(f"stability.py: skipping {path}: {error}", file=sys.stderr)
    history = load_history(Path(args.history))
    before = {key(r): r for r in history[-1]["records"]} if history else {}
    night = {
        "date": now_utc(),
        "sha": os.environ.get("GITHUB_SHA", ""),
        "run": os.environ.get("GITHUB_RUN_ID", ""),
        "records": records,
    }
    history = (history + [night])[-NIGHTS:]
    Path(args.out).write_text(json.dumps({"nights": history}, indent=1) + "\n", encoding="utf-8")

    rows = ["| Job | Arch | Status | Counts | Time | Since last night | First failure |",
            "|---|---|---|---|---|---|---|"]
    regressions = []
    for record in sorted(records, key=key):
        old = before.get(key(record))
        counts = ", ".join(f"{k} {v}" for k, v in sorted(record.get("counts", {}).items()))
        change = "new"
        if old:
            change = "same" if old.get("status") == record.get("status") else \
                f"{old.get('status')} → {record.get('status')}"
            old_tests, new_tests = old.get("tests", {}), record.get("tests", {})
            if new_tests:
                lost = sorted(t for t, r in new_tests.items()
                              if r != "pass" and old_tests.get(t) == "pass")
                won = sorted(t for t, r in new_tests.items()
                             if r == "pass" and old_tests.get(t) not in (None, "pass"))
                change = f"{len(lost)} newly failing, {len(won)} newly passing"
                regressions += [f"{key(record)} {t}" for t in lost]
        first = record.get("first_failure") or record.get("why", "")
        rows.append(f"| {record.get('job')} | {record.get('arch')} | {record.get('status')} "
                    f"| {counts} | {record.get('seconds', 0)} s | {change} "
                    f"| {first.replace('|', '/')[:200]} |")
    trend = trend_rows(history)
    text = "## Nightly stability\n\n" + "\n".join(rows) + "\n"
    if regressions:
        text += "\n### Tests that passed last night and fail tonight\n\n" + \
            "\n".join(f"* `{r}`" for r in regressions[:200]) + "\n"
    if trend:
        text += "\n### The last nights\n\n" + trend + "\n"
    summary_append(text)
    if args.markdown:
        Path(args.markdown).write_text(text, encoding="utf-8")
    return 0


def trend_rows(history: list[dict]) -> str:
    """One line per job: its status over the last 14 nights, newest last."""
    nights = history[-14:]
    jobs = sorted({key(r) for n in nights for r in n.get("records", [])})
    mark = {"pass": "P", "fail": "F", "skipped": "-"}
    lines = ["| Job | Nights (oldest → newest) |", "|---|---|"]
    for job in jobs:
        cells = []
        for n in nights:
            got = next((r for r in n.get("records", []) if key(r) == job), None)
            cells.append(mark.get(got.get("status"), "?") if got else ".")
        lines.append(f"| {job} | `{''.join(cells)}` |")
    return "\n".join(lines) if jobs else ""


def cmd_verdict(args: argparse.Namespace) -> int:
    history = load_history(Path(args.history))
    if not history:
        print("green")
        return 0
    red = [r for r in history[-1]["records"] if r.get("status") == "fail"]
    print("red" if red else "green")
    for record in red:
        print(f"- {key(record)}: {record.get('first_failure', '')}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="cmd", required=True)
    run = sub.add_parser("run", help="run one job and write its record")
    run.add_argument("--job", required=True)
    run.add_argument("--arch", required=True)
    run.add_argument("--out", default="stability")
    run.add_argument("--repeat", type=int, default=1)
    run.add_argument("--skip", help="record the job as skipped, with this reason")
    run.add_argument("--tests", help="a JSON file of per-test results the row wrote")
    run.add_argument("--keep", action="append", help="a file to keep beside a failed run's log")
    run.add_argument("--data", action="store_true",
                     help="test failures are data: only a stop is red (LTP, kselftest)")
    run.add_argument("--never-fail", action="store_true", help="exit 0 whatever the record says")
    run.add_argument("command", nargs=argparse.REMAINDER)
    report = sub.add_parser("report", help="the run's summary and the trend")
    report.add_argument("--records", required=True)
    report.add_argument("--history", required=True)
    report.add_argument("--out", required=True)
    report.add_argument("--markdown")
    verdict = sub.add_parser("verdict", help="red or green, for the rolling issue")
    verdict.add_argument("--history", required=True)
    args = parser.parse_args()
    if args.cmd == "run":
        if args.command and args.command[0] == "--":
            args.command = args.command[1:]
        return cmd_run(args)
    if args.cmd == "report":
        return cmd_report(args)
    return cmd_verdict(args)


if __name__ == "__main__":
    sys.exit(main())
