#!/usr/bin/env python3
"""Bound the complexity, length and recursion of the certified item.

EN 50716 requires a coding standard with metrics, and IEC 62304 §5.5.3 wants
unit verification against stated acceptance criteria. Eleven gates enforce
other properties of this tree -- every `unsafe` block documented, every panic
exemption justified, the assembly budget, the device-access seam, the crate
layering, the item boundary -- and none of them bounded how complicated a
single function may be. This is finding F-25.

Scope is the certified item as `tools/common/data/certification-item.json` defines it:
the `core` and `item` rings, product code only. The uncertified load above them
carries no assurance claim and is measured but not enforced, so the numbers can
be compared.

Since 2026-10-02 that includes the item's library crates, `ferrix-btrfs` and
`ferrix-btrfs-write` (`crates` in the manifest): every product file their module
trees reach, with any inline `#[cfg(test)] mod` blanked, since tests are
verification. Their functions are keyed by the path from the repository root
(`src/lib/fs/btrfs/format/src/tree.rs::search`), the kernel's by the path under
`src/kernel/src`, so the two cannot collide. Recursion matters more there, not
less: a B-tree walk is the natural place to write it, and on a kernel stack
with no guard page it has to be bounded by the tree's level, not by the data.

Three measures
--------------

  complexity  Branch points plus one: `if`, `else if`, `match` arms, `while`,
              `for`, `loop`, `&&`, `||`, `?`. **This is an approximation.**
              Real cyclomatic complexity needs a parser and a control-flow
              graph; this counts tokens in comment-and-string-stripped source.
              It is stable, it is monotonic in the thing it approximates, and
              it is wrong in detail -- a `match` on an enum with twelve
              unreachable arms scores the same as twelve real branches.
              Stated here because a number whose caveats travel separately is
              worse than no number.

  lines       Statement lines in the body, blank and comment lines excluded.
              `clippy::too_many_lines` already denies over 100 crate-wide; this
              records the item's distribution so the cap can be ratcheted down
              rather than merely not exceeded.

  recursion   A function whose body names itself. Direct recursion only:
              mutual recursion through two functions, and any recursion through
              a trait object or a function pointer, are **not** detected. A
              kernel with no guard page under its stack has a real reason to
              care, so the limitation matters and is not hidden.

The ratchet
-----------

Like `tools/common/check/check-item-boundary.py`: measure, record what exists, refuse
growth. `tools/common/data/complexity-baseline.json` holds every function in the item
over a threshold, with its score. An entry may improve or disappear freely; a
new one, or an existing one getting worse, fails the build. Stale entries fail
too, so a simplified function cannot leave a permanent allowance behind.

    python3 tools/common/check/check-complexity.py
    python3 tools/common/check/check-complexity.py --report     # the distribution
    python3 tools/common/check/check-complexity.py --record     # rewrite the baseline
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import rustlex  # noqa: E402  (after the path insert)

ROOT = Path(__file__).resolve().parent.parent.parent.parent
KERNEL_SRC = ROOT / "src" / "kernel" / "src"
BASELINE = ROOT / "tools" / "common" / "data" / "complexity-baseline.json"

# Only functions at or above these are recorded; below them the tree is not
# interesting and a baseline listing every small function would be unreadable.
COMPLEXITY_FLOOR = 15
LINES_FLOOR = 60

FN = re.compile(
    r"^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?(?:default[ \t]+)?(?:const[ \t]+)?"
    r"(?:async[ \t]+)?(?:unsafe[ \t]+)?(?:extern[ \t]+\"[^\"]*\"[ \t]+)?"
    r"fn[ \t]+([A-Za-z_][A-Za-z0-9_]*)",
    re.MULTILINE,
)

BRANCH = re.compile(
    r"(?<![A-Za-z0-9_])(?:if|while|for|loop)(?![A-Za-z0-9_])|&&|\|\||\?[^?]|=>"
)


def strip(source: str) -> str:
    """Source with comments and literal contents blanked, length preserved.

    `tools/common/check/rustlex.py` does the work, and why it has to is its docstring:
    the regular expressions this function used before mis-paired quotes after
    a `\\`-newline continuation, so `main.rs::say_booted` scored 102 lines
    and swallowed `register_load`, and the word "for" in a panic message
    counted as a loop.
    """
    return rustlex.mask(source)


def body_of(text: str, start: int) -> tuple[str, int] | None:
    """The braced body that follows the signature beginning at `start`.

    `None` when the signature has no body: a trait method, or a declaration in
    an `unsafe extern "C"` block, which ends in `;`. Distinguishing the two
    matters more than it sounds. Taking the next `{` unconditionally gave every
    `extern` declaration the *following* item's body -- which is how the
    assembly symbol `ferrix_switch`, declared beside the `naked_asm!` that
    defines it, came out looking like a recursive function with borrowed
    complexity and length scores.

    The scan tracks `(` and `[` depth so that a `;` inside a parameter's array
    type -- `fn f(bytes: [u8; 4])` -- is not mistaken for the end of a
    declaration.
    """
    depth = 0
    open_at = -1
    for index in range(start, len(text)):
        char = text[index]
        if char in "([":
            depth += 1
        elif char in ")]":
            depth -= 1
        elif depth == 0:
            if char == ";":
                return None
            if char == "{":
                open_at = index
                break
    if open_at < 0:
        return None
    depth = 0
    for index in range(open_at, len(text)):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return text[open_at : index + 1], index
    return None


def measure(path: Path) -> list[dict]:
    """Every function in `path`, with its three scores."""
    return measure_source(path.read_text(encoding="utf-8", errors="replace"))


TEST_MODULE = re.compile(r"#\[cfg\(test\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{")


def without_test_modules(text: str) -> str:
    """`text` with every inline `#[cfg(test)] mod x { ... }` blanked, length kept."""
    for match in TEST_MODULE.finditer(text):
        end = body_of(text, match.end() - 1)
        if end is not None:
            stop = end[1] + 1
            text = text[: match.start()] + re.sub(r"[^\n]", " ", text[match.start() : stop]) + text[stop:]
    return text


def measure_source(source: str) -> list[dict]:
    """Every function in `source`, with its three scores."""
    text = without_test_modules(strip(source))
    found: list[dict] = []

    for match in FN.finditer(text):
        name = match.group(1)
        body = body_of(text, match.end())
        if body is None:
            continue  # a trait method signature with no body
        code, _ = body

        lines = sum(1 for line in code.splitlines() if line.strip() not in ("", "{", "}"))
        complexity = len(BRANCH.findall(code)) + 1
        # An *unqualified* call to its own name. The negative lookbehind for
        # `:` and `.` is what makes this measure worth anything: the
        # architecture facade is full of `fn flush_tlb` whose body is
        # `aarch64::flush_tlb(..)`, and matching a bare name called those 124
        # forwarding shims recursive. Method calls (`self.name()`) are excluded
        # for the same reason -- a different receiver is a different function.
        calls_itself = re.search(
            rf"(?<![A-Za-z0-9_:.]){re.escape(name)}\s*\(", code[1:]
        )
        # `drop(x)` inside a `Drop::drop` body is `core::mem::drop`, which is
        # the one name in the language whose unqualified call inside a function
        # of the same name is never a call to that function.
        if name == "drop":
            calls_itself = None

        found.append(
            {
                "name": name,
                "line": text.count("\n", 0, match.start()) + 1,
                "complexity": complexity,
                "lines": lines,
                "recursive": bool(calls_itself),
            }
        )

    return found


def load_gate():
    spec = importlib.util.spec_from_file_location(
        "boundary", ROOT / "tools" / "common" / "check" / "check-item-boundary.py"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def item_functions() -> list[dict]:
    """Every product-code function in the certified item."""
    gate = load_gate()
    manifest = gate.load_manifest()
    ring_of, _, _ = gate.classify(manifest, gate.kernel_files())

    out: list[dict] = []
    for rel, ring in sorted(ring_of.items()):
        if ring == "load" or gate.is_test_file(rel, manifest):
            continue
        for fn in measure(KERNEL_SRC / rel):
            fn["file"] = rel
            fn["ring"] = ring
            out.append(fn)
    for package, path in gate.item_crate_product_files(manifest):
        for fn in measure(ROOT / path):
            fn["file"] = path
            fn["ring"] = gate.crate_entries(manifest)[package]["ring"]
            fn["crate"] = package
            out.append(fn)
    return out


def over_floor(functions: list[dict]) -> dict[str, dict]:
    """The ones worth recording, keyed `file::name`."""
    return {
        f"{fn['file']}::{fn['name']}": {
            "complexity": fn["complexity"],
            "lines": fn["lines"],
            "recursive": fn["recursive"],
        }
        for fn in functions
        if fn["complexity"] >= COMPLEXITY_FLOOR
        or fn["lines"] >= LINES_FLOOR
        or fn["recursive"]
    }


def report(functions: list[dict]) -> None:
    for package in sorted({fn["crate"] for fn in functions if "crate" in fn}):
        mine = [fn for fn in functions if fn.get("crate") == package]
        over = over_floor(mine)
        print(f"complexity: crate {package}: {len(mine)} functions, {len(over)} over a floor, "
              f"{sum(1 for fn in mine if fn['recursive'])} directly recursive")
        for key, fn in sorted(over.items()):
            mark = ", recursive" if fn["recursive"] else ""
            print(f"    {fn['complexity']:>4}  {fn['lines']:>4} lines{mark}  {key}")
    print(f"complexity: {len(functions)} functions in the certified item")
    worst = sorted(functions, key=lambda f: -f["complexity"])[:12]
    print("  worst by approximate cyclomatic complexity:")
    for fn in worst:
        print(f"    {fn['complexity']:>4}  {fn['lines']:>4} lines  "
              f"{fn['file']}::{fn['name']}")
    recursive = [f for f in functions if f["recursive"]]
    print(f"  directly recursive: {len(recursive)}")
    for fn in recursive:
        print(f"    {fn['file']}::{fn['name']} (line {fn['line']})")
    buckets = [0, 0, 0, 0]
    for fn in functions:
        index = min(fn["complexity"] // 10, 3)
        buckets[index] += 1
    print(f"  complexity 1-9: {buckets[0]}, 10-19: {buckets[1]}, "
          f"20-29: {buckets[2]}, 30+: {buckets[3]}")


# The shapes that measured wrong before the lexer was shared: a `\\`-newline
# continuation in a message (`say_booted`, whose mis-paired quotes hid the
# function after it), a char literal holding a double quote, loop words inside
# strings, and an assembly label in a raw string named like its function.
_SELF_TEST = r"""
fn say_booted() {
    if checks::run() {
        println!("{SUCCESS_MARKER} stages 1-12");
    } else {
        println!(
            "{UNCHECKED_MARKER} stages 1-12 brought up, the self-checks of 2 to 12 skipped as \
             ferrix.checks=skip asks for while loop if"
        );
    }
}

fn register_load(view: &BootView<'_>) {
    let quote = '"';
    if a && b {
        panic!("for while loop if && || ?");
    }
}

extern "C" {
    fn ferrix_switch();
}

fn ferrix_switch_body() {
    naked_asm!(r#"
    ferrix_switch_body:
        ret "for" if
    "#);
}

#[cfg(test)]
mod tests {
    fn only_a_test() { if a { only_a_test() } }
}
"""

_SELF_EXPECT = {
    # name: (complexity, lines, recursive)
    "say_booted": (2, 7, False),
    "register_load": (3, 3, False),
    "ferrix_switch_body": (1, 2, False),
}


def self_test() -> list[str]:
    failures = [f"lexer: {f}" for f in rustlex.self_test()]
    got = {
        fn["name"]: (fn["complexity"], fn["lines"], fn["recursive"])
        for fn in measure_source(_SELF_TEST)
    }
    for name, want in _SELF_EXPECT.items():
        if got.get(name) != want:
            failures.append(f"measure: {name} is {got.get(name)}, expected {want}")
    if "only_a_test" in got:
        failures.append("measure: a function in a #[cfg(test)] module was measured")
    if "ferrix_switch" in got:
        failures.append("measure: an extern declaration was measured as a function")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", action="store_true")
    parser.add_argument("--record", action="store_true")
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run the lexer's and the measure's cases and nothing else",
    )
    args = parser.parse_args()

    failures = self_test()
    if failures:
        for failure in failures:
            print(f"complexity: self-test: {failure}", file=sys.stderr)
        return 1
    if args.self_test:
        print("complexity: lexer and measure self-tests pass")
        return 0

    functions = item_functions()

    if args.report:
        report(functions)
        return 0

    current = over_floor(functions)

    if args.record:
        BASELINE.write_text(
            json.dumps(
                {
                    "//": [
                        "Functions in the certified item at or above the floors in",
                        "tools/common/check/check-complexity.py, with the scores they had when",
                        "recorded. A debt register, not an allowance: an entry may",
                        "improve or vanish freely, and the gate fails when one gets",
                        "worse, when a new one appears, or when a stale entry is",
                        "left behind.",
                        "",
                        "Complexity here is an approximation counting branch tokens,",
                        "not a control-flow graph. The script's docstring says where",
                        "it is wrong.",
                    ],
                    "complexity_floor": COMPLEXITY_FLOOR,
                    "lines_floor": LINES_FLOOR,
                    "functions": dict(sorted(current.items())),
                },
                indent=2,
            )
            + "\n"
        )
        print(f"complexity: recorded {len(current)} function(s)")
        return 0

    if not BASELINE.exists():
        print(
            "complexity: no baseline. Run `python3 tools/common/check/check-complexity.py "
            "--record` and commit it.",
            file=sys.stderr,
        )
        return 1

    baseline = json.loads(BASELINE.read_text())["functions"]
    status = 0

    worse, appeared = [], []
    for key, now in sorted(current.items()):
        was = baseline.get(key)
        if was is None:
            appeared.append((key, now))
        elif (
            now["complexity"] > was["complexity"]
            or now["lines"] > was["lines"]
            or (now["recursive"] and not was["recursive"])
        ):
            worse.append((key, was, now))

    stale = sorted(set(baseline) - set(current))

    if appeared:
        print(
            f"complexity: {len(appeared)} function(s) newly over the floor "
            f"(complexity {COMPLEXITY_FLOOR}, {LINES_FLOOR} lines, or recursive).\n"
            f"  Simplify, or record it deliberately with --record:",
            file=sys.stderr,
        )
        for key, now in appeared:
            mark = ", recursive" if now["recursive"] else ""
            print(f"    {key}: complexity {now['complexity']}, "
                  f"{now['lines']} lines{mark}", file=sys.stderr)
        status = 1

    if worse:
        print(f"complexity: {len(worse)} function(s) got worse:", file=sys.stderr)
        for key, was, now in worse:
            print(f"    {key}: complexity {was['complexity']}->{now['complexity']}, "
                  f"lines {was['lines']}->{now['lines']}", file=sys.stderr)
        status = 1

    if stale:
        print(
            f"complexity: {len(stale)} baseline entry(ies) no longer apply.\n"
            f"  Re-record so a simplified function leaves no allowance behind:",
            file=sys.stderr,
        )
        for key in stale:
            print(f"    {key}", file=sys.stderr)
        status = 1

    recursive = sum(1 for fn in functions if fn["recursive"])
    print(
        f"complexity: {len(functions)} functions in the item, "
        f"{len(current)} over a floor, {recursive} directly recursive"
    )
    return status


if __name__ == "__main__":
    sys.exit(main())
