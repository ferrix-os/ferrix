#!/usr/bin/env python3
"""Statement and branch coverage of host-tested item crates, from their host
tests (finding F-56's gap, for the crates this lists).

`coverage-report.py` and `decision-coverage.py` measure the kernel's boots.
A crate the item holds that no kernel links -- `ferrix-sigverify` and
`ferrix-sha256`, product signing's verifier (docs/SIGNING.md §8.5) -- is
proved by its host tests, so its coverage is measured there:

    python3 tools/common/gen/host-coverage.py            # measure, write
    python3 tools/common/gen/host-coverage.py --check    # the gate

Measuring builds the crates' unit tests with rustc's source-based coverage
(`-C instrument-coverage`) and branch coverage (`-Zcoverage-options=branch`,
which is unstable, so the measurement uses the nightly toolchain and its
`llvm-tools` component: the product is built with the pinned toolchain from
the same source, and the measurement stands for it on that ground), runs
them, merges the profiles with that toolchain's `llvm-profdata`, and exports
per-line and per-branch counts with its `llvm-cov`. Test code (`tests.rs`,
`vectors.rs`) is left out; only the crates' product files are counted.

The result, `docs/certification/host-coverage.json`, holds per file: the
SHA-256 of the source measured, the lines and branches counted and reached,
and every unreached line and branch outcome. An unreached one must be argued
in the same file's `argued` map (`"file:line": "why"`), as
COVERAGE-RESIDUAL.md argues the kernel's.

`--check` (in `cargo xtask check`, no build) fails when a measured file's
source, or any test input (the crates' tests and vectors and the soft
signer), no longer hashes as measured (the evidence is stale: measure again),
when a crate's product file was not measured, or when an unreached line or
branch is not argued. So adding an untested branch to either crate fails the
gate until the evidence is measured again and the new gap is tested or
argued. `--self-test` shows the staleness check fires on a changed source.

Limits, stated: llvm-cov's branch coverage counts the two outcomes of each
`if`, `while` and `match` guard condition, and of each `&&`/`||` operand; it
is condition coverage of those, not MC/DC, which is not owed before M7
(ledger 3738, S1-e). A `?` is not counted as a branch.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[3]
EVIDENCE = ROOT / "docs/certification/host-coverage.json"
CRATES = {
    "ferrix-boot-policy": "src/lib/proto/boot-policy",
    "ferrix-sha256": "src/lib/crypto/sha256",
    "ferrix-sigverify": "src/lib/crypto/sigverify",
    "ferrix-slots": "src/lib/proto/slots",
}
TEST_FILES = {"tests.rs", "vectors.rs"}
TOOLCHAIN = "nightly"
SIGNER = "tools/common/release/softsign/src"


def product_files() -> list[str]:
    out = []
    for path in CRATES.values():
        for f in sorted((ROOT / path / "src").glob("*.rs")):
            if f.name not in TEST_FILES:
                out.append(str(f.relative_to(ROOT)))
    return out


def test_inputs() -> list[str]:
    """What the measurement ran: the crates' test files, their vectors and
    the soft signer whose manifests the tests verify. A change to any of
    them makes the evidence stale as a product change does, so a deleted
    test cannot leave the coverage it gave on record (ledger 3803 (3))."""
    out = []
    for path in CRATES.values():
        src = ROOT / path / "src"
        out += [str(f.relative_to(ROOT)) for f in sorted(src.glob("*.rs")) if f.name in TEST_FILES]
        out += [str(f.relative_to(ROOT)) for f in sorted(src.glob("cavp/*.rsp"))]
    out += [str(f.relative_to(ROOT)) for f in sorted((ROOT / SIGNER).glob("*.rs"))]
    return out


def sha256_of(rel: str) -> str:
    return hashlib.sha256((ROOT / rel).read_bytes()).hexdigest()


def llvm_bin(name: str) -> pathlib.Path:
    sysroot = subprocess.run(
        ["rustc", f"+{TOOLCHAIN}", "--print", "sysroot"], check=True, capture_output=True, text=True
    ).stdout.strip()
    host = "x86_64-unknown-linux-gnu"
    path = pathlib.Path(sysroot) / "lib/rustlib" / host / "bin" / name
    if not path.exists():
        sys.exit(f"host-coverage: {path} is missing: rustup +{TOOLCHAIN} component add llvm-tools")
    return path


def measure() -> dict:
    target = ROOT / "target/host-coverage"
    with tempfile.TemporaryDirectory(prefix="host-coverage-") as scratch:
        env = dict(os.environ)
        env["CARGO_TARGET_DIR"] = str(target)
        env["RUSTFLAGS"] = "-C instrument-coverage -Zcoverage-options=branch"
        env["LLVM_PROFILE_FILE"] = f"{scratch}/%p-%m.profraw"
        packages = [a for crate in CRATES for a in ("-p", crate)]
        built = subprocess.run(
            ["cargo", f"+{TOOLCHAIN}", "test", *packages, "--lib", "--no-run", "--message-format=json"],
            cwd=ROOT, env=env, check=True, capture_output=True, text=True,
        ).stdout
        executables = []
        for line in built.splitlines():
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if message.get("reason") == "compiler-artifact" and message.get("executable"):
                executables.append(message["executable"])
        if len(executables) != len(CRATES):
            sys.exit(f"host-coverage: expected {len(CRATES)} test binaries, got {executables}")
        subprocess.run(
            ["cargo", f"+{TOOLCHAIN}", "test", *packages, "--lib"], cwd=ROOT, env=env, check=True
        )
        profdata = pathlib.Path(scratch) / "merged.profdata"
        raws = sorted(str(p) for p in pathlib.Path(scratch).glob("*.profraw"))
        subprocess.run([str(llvm_bin("llvm-profdata")), "merge", "-sparse", *raws, "-o", str(profdata)], check=True)
        objects = [executables[0]] + [a for e in executables[1:] for a in ("-object", e)]
        exported = subprocess.run(
            [str(llvm_bin("llvm-cov")), "export", "-format=text", f"-instr-profile={profdata}", *objects],
            check=True, capture_output=True, text=True,
        ).stdout
    data = json.loads(exported)["data"][0]["files"]
    wanted = set(product_files())
    files = {}
    for entry in data:
        rel = os.path.relpath(entry["filename"], ROOT)
        if rel not in wanted:
            continue
        lines: dict[int, int] = {}
        for seg in entry["segments"]:
            line, _col, count, has_count, is_region_entry, is_gap = seg[:6]
            if has_count and is_region_entry and not is_gap:
                lines[line] = max(lines.get(line, 0), count)
        missed_lines = sorted(line for line, count in lines.items() if count == 0)
        # One branch per source position: an inlined or generic copy adds
        # its counts to the same condition.
        outcomes: dict[tuple[int, int], list[int]] = {}
        for b in entry.get("branches", []):
            line, col, _el, _ec, true_count, false_count = b[:6]
            seen = outcomes.setdefault((line, col), [0, 0])
            seen[0] += true_count
            seen[1] += false_count
        branches = [(line, col, t, f) for (line, col), (t, f) in sorted(outcomes.items())]
        missed_branches = []
        for line, col, t, f in branches:
            if t == 0:
                missed_branches.append(f"{line}:{col}:true")
            if f == 0:
                missed_branches.append(f"{line}:{col}:false")
        files[rel] = {
            "sha256": sha256_of(rel),
            "lines": len(lines),
            "lines_hit": len(lines) - len(missed_lines),
            "branch_outcomes": 2 * len(branches),
            "branch_outcomes_hit": sum((t > 0) + (f > 0) for _l, _c, t, f in branches),
            "unreached_lines": missed_lines,
            "unreached_branches": missed_branches,
        }
    # A file the export does not name holds no counted code (a crate root
    # of `mod` and `use` lines): it is measured as such.
    for rel in sorted(wanted - set(files)):
        files[rel] = {
            "sha256": sha256_of(rel),
            "lines": 0,
            "lines_hit": 0,
            "branch_outcomes": 0,
            "branch_outcomes_hit": 0,
            "unreached_lines": [],
            "unreached_branches": [],
        }
    return files


def load() -> dict:
    if not EVIDENCE.exists():
        return {"files": {}, "argued": {}}
    return json.loads(EVIDENCE.read_text())


def check(evidence: dict) -> list[str]:
    problems = []
    files = evidence.get("files", {})
    argued = evidence.get("argued", {})
    for rel in product_files():
        if rel not in files:
            problems.append(f"{rel} was not measured: run python3 tools/common/gen/host-coverage.py")
            continue
        if files[rel]["sha256"] != sha256_of(rel):
            problems.append(f"{rel} changed since it was measured: run python3 tools/common/gen/host-coverage.py")
            continue
        for line in files[rel]["unreached_lines"]:
            if f"{rel}:{line}" not in argued:
                problems.append(f"{rel}:{line} is not reached by the host tests and not argued")
        for branch in files[rel]["unreached_branches"]:
            if f"{rel}:{branch}" not in argued:
                problems.append(f"{rel}:{branch} (a branch outcome) is not reached by the host tests and not argued")
    for rel in files:
        if rel not in product_files():
            problems.append(f"{rel} is measured but is no product file of the crates")
    inputs = evidence.get("test_inputs", {})
    for rel in test_inputs():
        if inputs.get(rel) != sha256_of(rel):
            problems.append(f"{rel}, a test input, changed since the measurement: run python3 tools/common/gen/host-coverage.py")
    for rel in inputs:
        if rel not in test_inputs():
            problems.append(f"{rel}, a test input of the measurement, is gone: run python3 tools/common/gen/host-coverage.py")
    return problems


def self_test() -> int:
    evidence = load()
    rel = product_files()[0]
    tampered = json.loads(json.dumps(evidence))
    tampered["files"][rel]["sha256"] = "0" * 64
    if not any("changed since it was measured" in p for p in check(tampered)):
        print("host-coverage: self-test: a stale file was NOT refused", file=sys.stderr)
        return 1
    tampered = json.loads(json.dumps(evidence))
    test = test_inputs()[0]
    tampered["test_inputs"][test] = "0" * 64
    if not any("a test input, changed" in p for p in check(tampered)):
        print("host-coverage: self-test: a changed test input was NOT refused", file=sys.stderr)
        return 1
    tampered = json.loads(json.dumps(evidence))
    tampered["files"][rel]["unreached_lines"] = tampered["files"][rel]["unreached_lines"] + [999999]
    if not any(":999999 is not reached" in p for p in check(tampered)):
        print("host-coverage: self-test: an unargued unreached line was NOT refused", file=sys.stderr)
        return 1
    print("host-coverage: self-test passed: a stale file, a changed test input and an unargued gap are refused")
    return 0


def summary(evidence: dict) -> str:
    files = evidence["files"]
    lines = sum(f["lines"] for f in files.values())
    hit = sum(f["lines_hit"] for f in files.values())
    outcomes = sum(f["branch_outcomes"] for f in files.values())
    reached = sum(f["branch_outcomes_hit"] for f in files.values())
    return (
        f"host-coverage: {len(files)} file(s) of {', '.join(CRATES)}: lines {hit}/{lines}, "
        f"branch outcomes {reached}/{outcomes}, {len(evidence.get('argued', {}))} argued"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if args.check:
        evidence = load()
        problems = check(evidence)
        for p in problems:
            print(f"host-coverage: {p}", file=sys.stderr)
        if problems:
            return 1
        print(summary(evidence))
        return 0
    if shutil.which("cargo") is None:
        sys.exit("host-coverage: cargo is not on PATH")
    old = load()
    evidence = {
        "//": [
            "Statement and branch coverage of ferrix-sha256 and ferrix-sigverify under their",
            "host tests, written by tools/common/gen/host-coverage.py (its docstring says how",
            "and its limits). `argued` holds each unreached line or branch outcome with why;",
            "the gate (--check) fails a stale file or an unargued gap.",
        ],
        "toolchain": subprocess.run(
            ["rustc", f"+{TOOLCHAIN}", "--version"], check=True, capture_output=True, text=True
        ).stdout.strip(),
        "files": measure(),
        "test_inputs": {rel: sha256_of(rel) for rel in test_inputs()},
        "argued": old.get("argued", {}),
    }
    EVIDENCE.write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n")
    print(summary(evidence))
    for p in check(evidence):
        print(f"host-coverage: {p}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
