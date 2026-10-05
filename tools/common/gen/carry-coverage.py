#!/usr/bin/env python3
"""Carry the coverage evidence's line anchors across a change to the kernel.

The evidence in docs/certification -- coverage-<arch>.json,
coverage-residual-<arch>.json and coverage-argued-<arch>.json -- names
statements by file and line, as measured on one tree. Any change that moves a
line in the item leaves those anchors pointing at the wrong statement, and
`gen-coverage-justification.py --check` then fails, as it should: an argument
must not drift onto another statement.

This carries the anchors from the tree the evidence was last written on to the
working tree, file by file, through a diff:

    python3 tools/common/gen/carry-coverage.py            # from the commit that last wrote the evidence
    python3 tools/common/gen/carry-coverage.py --from REV  # refused unless REV's kernel is that commit's
    python3 tools/common/gen/carry-coverage.py --check     # fail if anything was left uncarried
    python3 tools/common/gen/gen-coverage-justification.py   # then regenerate the pages
    python3 tools/common/gen/gen-coverage-justification.py --check

It carries, it does not measure. A line the change left untouched keeps its
place in the residual and its argument, at its new number. A line the change
edited or removed is dropped: it is new code now, unmeasured until the next
run of `cargo xtask coverage`, and an argument written for the old text is not
evidence for the new one. What is dropped is printed, so the landing can say
so. The figures in coverage-<arch>.json stay as measured, and a renamed file
keeps its figures under its new name; the checks' own reached statements there
(its `verification` map, which TRACEABILITY.md reads) are carried like the
residual, an edited line dropped and so no longer counted as reached.
"""

import argparse
import difflib
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
CERT = ROOT / "docs" / "certification"
ARCHES = ("x86_64", "aarch64", "armv7a")
SRC = "src/kernel/src/"


def git(*args, check=True):
    done = subprocess.run(["git", "-C", str(ROOT), *args], capture_output=True, text=True)
    if check and done.returncode:
        sys.exit(f"carry-coverage: git {' '.join(args)}: {done.stderr.strip()}")
    return done


def renames(base):
    """kernel/src-relative old path -> new path, for files the change renamed."""
    out = {}
    listing = git("diff", "-M", "--name-status", base, "--", SRC).stdout
    for row in listing.splitlines():
        fields = row.split("\t")
        if fields[0].startswith("R") and len(fields) == 3:
            out[fields[1][len(SRC):]] = fields[2][len(SRC):]
    return out


def line_map(old, new):
    """Old line -> new line, for lines the change left exactly as they were."""
    out = {}
    matcher = difflib.SequenceMatcher(None, old, new, autojunk=False)
    for tag, i1, i2, j1, _ in matcher.get_opcodes():
        if tag == "equal":
            for k in range(i2 - i1):
                out[i1 + k + 1] = j1 + k + 1
    return out


def parse_lines(spec):
    out = []
    for part in str(spec).split(","):
        part = part.strip()
        if "-" in part:
            low, high = part.split("-")
            out.extend(range(int(low), int(high) + 1))
        elif part:
            out.append(int(part))
    return out


def format_lines(numbers):
    numbers = sorted(numbers)
    parts, i = [], 0
    while i < len(numbers):
        j = i
        while j + 1 < len(numbers) and numbers[j + 1] == numbers[j] + 1:
            j += 1
        parts.append(str(numbers[i]) if i == j else f"{numbers[i]}-{numbers[j]}")
        i = j + 1
    return ", ".join(parts)


class Carrier:
    def __init__(self, base):
        self.base = base
        self.moved = renames(base)
        self.maps = {}

    def carry(self, path):
        """(new path, line map or None for unchanged, or (new path, {}) if gone)."""
        if path in self.maps:
            return self.maps[path]
        new = self.moved.get(path, path)
        old = git("show", f"{self.base}:{SRC}{path}", check=False)
        here = ROOT / SRC / new
        if old.returncode or not here.is_file():
            result = (new, {})
        else:
            before, after = old.stdout, here.read_text()
            result = (new, None if before == after else line_map(before.splitlines(), after.splitlines()))
        self.maps[path] = result
        return result


class Files:
    """The evidence on disk, read and written in place."""

    def exists(self, name):
        return (CERT / name).is_file()

    def read(self, name):
        return (CERT / name).read_text()

    def write(self, name, text):
        (CERT / name).write_text(text)


class Memory(Files):
    """The evidence as on disk, with writes kept in memory: a dry run."""

    def __init__(self):
        self.written = {}

    def read(self, name):
        return self.written[name] if name in self.written else super().read(name)

    def write(self, name, text):
        self.written[name] = text

    def read_disk(self, name):
        return super().read(name)


def carry_residual(carrier, arch, report, store):
    evidence = f"coverage-residual-{arch}.json"
    residual = json.loads(store.read(evidence))
    files = {}
    for name, entry in residual["files"].items():
        new, mapping = carrier.carry(name)
        lines = entry["lines"]
        if mapping is not None:
            kept = [mapping[line] for line in lines if line in mapping]
            gone = [line for line in lines if line not in mapping]
            if gone:
                report.append(f"{arch} residual {name}: {len(gone)} line(s) edited or removed, "
                              f"unmeasured until the next run: {format_lines(gone)}")
            lines = kept
        if lines:
            files[new] = {**entry, "lines": lines}
    residual["files"] = files
    residual["unreached"] = sum(len(entry["lines"]) for entry in files.values())
    store.write(evidence, json.dumps(residual, indent=2) + "\n")


def carry_arguments(carrier, arch, report, store):
    evidence = f"coverage-argued-{arch}.json"
    if not store.exists(evidence):
        return
    argued = json.loads(store.read(evidence))
    kept = []
    for argument in argued["arguments"]:
        new, mapping = carrier.carry(argument["file"])
        lines = parse_lines(argument["lines"])
        if mapping is None:
            kept.append({**argument, "file": new})
            continue
        moved = [mapping[line] for line in lines if line in mapping]
        gone = [line for line in lines if line not in mapping]
        if gone:
            report.append(f"{arch} argument {argument['file']}:{argument['lines']} "
                          f"({argument['category']}): line(s) {format_lines(gone)} edited or removed, "
                          f"{'argument kept for the rest' if moved else 'argument dropped'}")
        if moved:
            kept.append({**argument, "file": new, "lines": format_lines(moved)})
    argued["arguments"] = kept
    store.write(evidence, json.dumps(argued, indent=2, ensure_ascii=False) + "\n")


def carry_figures(carrier, arch, store):
    evidence = f"coverage-{arch}.json"
    figures = json.loads(store.read(evidence))
    figures["files"] = {carrier.carry(name)[0]: entry for name, entry in figures["files"].items()}
    # The checks' own reached statements (TRACEABILITY.md). A line the change
    # edited is dropped: unmeasured, so never counted as reached.
    if "verification" in figures:
        checks = {}
        for name, entry in figures["verification"].items():
            new, mapping = carrier.carry(name)
            lines = parse_lines(entry["reached"])
            if mapping is not None:
                lines = [mapping[line] for line in lines if line in mapping]
            checks[new] = {**entry, "reached": format_lines(lines)}
        figures["verification"] = dict(sorted(checks.items()))
    store.write(evidence, json.dumps(figures, indent=2) + "\n")


EVIDENCE = tuple(f"coverage-{kind}{arch}.json" for arch in ARCHES for kind in ("", "residual-", "argued-"))


def carry_all(base, store):
    carrier = Carrier(base)
    report = []
    for arch in ARCHES:
        carry_residual(carrier, arch, report, store)
        carry_arguments(carrier, arch, report, store)
        carry_figures(carrier, arch, store)
    return carrier, report


def shallow_boundary():
    """The commits a shallow clone (CI's) was cut at. Its oldest commit seems
    to write every file, so what it really wrote cannot be told there."""
    shallow = Path(git("rev-parse", "--git-path", "shallow").stdout.strip())
    if not shallow.is_absolute():
        shallow = ROOT / shallow
    return set(shallow.read_text().split()) if shallow.is_file() else set()


def writers(head="HEAD"):
    """Each evidence file and the commit that last wrote it. Each file is
    judged from its own: a later edit of one file by hand moves only that
    file's base, never another's past a kernel change nobody carried."""
    return {
        name: git("log", "-1", "--format=%H", head, "--", f"docs/certification/{name}").stdout.strip()
        for name in EVIDENCE
    }


def carry_from_writers(store, head="HEAD"):
    """Carry each evidence file from the commit that last wrote it into
    `store`. Returns {name: base}, and the report of what was dropped."""
    bases = writers(head)
    report = []
    for base in sorted({b for b in bases.values() if b}):
        scratch = Memory()
        _, found = carry_all(base, scratch)
        mine = {name for name, b in bases.items() if b == base}
        for name in mine:
            if name in scratch.written:
                store.write(name, scratch.written[name])
        # The report's lines start "<arch> residual" or "<arch> argument".
        prefixes = tuple(
            f"{name[len('coverage-residual-'):-5]} residual " if name.startswith("coverage-residual-")
            else f"{name[len('coverage-argued-'):-5]} argument "
            for name in mine
            if name.startswith(("coverage-residual-", "coverage-argued-"))
        )
        report += [line for line in found if line.startswith(prefixes)]
    return bases, report


def stale(head="HEAD"):
    """The evidence files a carry from the commit that last wrote each would
    change: anchors a later change to the kernel moved and nobody carried
    (finding F-62). Empty when every anchor is where its statement is, or
    when the working tree has rewritten the evidence since `head`, which is
    checked once it is committed. One limit stays: an edit of an evidence
    file by hand resets that file's own base."""
    paths = [f"docs/certification/{name}" for name in EVIDENCE]
    if git("diff", "--quiet", head, "--", *paths, check=False).returncode:
        return []
    boundary = shallow_boundary()
    store = Memory()
    bases, _ = carry_from_writers(store, head)
    return [
        (name, bases[name])
        for name, text in sorted(store.written.items())
        if bases[name] and bases[name] not in boundary and text != store.read_disk(name)
    ]


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--from", dest="base", help="the tree the evidence was written on "
                        "(default: for each file, the commit that last wrote it)")
    parser.add_argument("--check", action="store_true", help="fail if a carry from the commit that "
                        "last wrote each evidence file would change it; write nothing")
    options = parser.parse_args()
    if options.check:
        found = stale()
        for name, base in found:
            print(f"carry-coverage: {name}: anchors not carried since {base[:12]}, the commit "
                  f"that last wrote it", file=sys.stderr)
        if found:
            print("carry-coverage: a change moved lines the evidence names; carry them with\n"
                  "  python3 tools/common/gen/carry-coverage.py", file=sys.stderr)
            return 1
        print("carry-coverage: every anchor carried")
        return 0
    bases = writers()
    if options.base:
        # The evidence names lines of the kernel as it was when each file was
        # last written. A base with another kernel would carry anchors from
        # the wrong lines and keep them silently (finding F-62: 61135f4ba
        # carried from a tree two uncarried landings had already moved).
        for name, written in bases.items():
            if git("diff", "--quiet", options.base, "--", f"docs/certification/{name}", check=False).returncode:
                sys.exit(f"carry-coverage: {name} changed since {options.base}; carry from the commit that last wrote it")
            if written and git("diff", "--quiet", written, options.base, "--", SRC, check=False).returncode:
                sys.exit(f"carry-coverage: the kernel at {options.base[:12]} is not the one {name} was last "
                         f"written on ({written[:12]}); carry from {written[:12]}")
        carrier, report = carry_all(options.base, Files())
        changed = sum(1 for _, mapping in carrier.maps.values() if mapping is not None)
        for line in report:
            print(line)
        print(f"carry-coverage: carried from {options.base[:12]}, {changed} file(s) of the evidence changed, "
              f"{len(report)} anchor(s) dropped as unmeasured")
        return 0
    store = Memory()
    _, report = carry_from_writers(store)
    for name, text in store.written.items():
        Files().write(name, text)
    for line in report:
        print(line)
    print(f"carry-coverage: carried each evidence file from the commit that last wrote it "
          f"({len({b for b in bases.values() if b})} base(s)), {len(report)} anchor(s) dropped as unmeasured")
    return 0

if __name__ == "__main__":
    sys.exit(main())
