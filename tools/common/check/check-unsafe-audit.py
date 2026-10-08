#!/usr/bin/env python3
"""Assert that every `unsafe` in the tree carries a justification, and that
every one in the certified item says which obligation it discharges.

This is the gate that stands in for `unsafe_code = "deny"`, which the Starling
workspace this policy is ported from can hold and a kernel cannot: writing a
page table entry, storing to an MMIO register or moving a CPU system register
*is* the program here. So unsafe is not forbidden, it is made expensive --
every block has to say why it is sound, at the block.

Three rules over the whole tree:

  1. Every `unsafe { ... }` block is preceded by a `// SAFETY:` comment.
  2. Every `unsafe impl` is preceded by a `// SAFETY:` comment.
  3. Every `unsafe fn` has a `/// # Safety` section in its own doc comment,
     stating the contract a caller has to meet -- except one implementing a
     trait method, whose contract belongs to the trait. Restating
     `GlobalAlloc::alloc`'s requirements on our impl of it would add noise and
     invite the copy to drift from the original; the `unsafe impl` line still
     needs its `// SAFETY:` comment, and that is where the claim that this type
     upholds the contract belongs.

Clippy's `undocumented_unsafe_blocks` and `missing_safety_doc` enforce (1) and
(3) as well, and the workspace denies both. This script exists anyway because
those two are nursery/pedantic lints: a clippy release that softens or renames
one would silently retire the rule, and nobody would notice until an audit.
A script in CI cannot be softened by someone else's release.

The obligation id (finding F-26)
--------------------------------

Documented is not traced. An assurance argument needs each unsafe site in the
certified item -- the `core` and `item` rings of
`tools/common/data/certification-item.json`, self-tests included, since they run in
the same image -- to name *why unsafe exists there*: which of a small closed set
of obligations it discharges, and through that, which assumed safety
requirement, failure mode or assumption of use of
`docs/certification/SAFETY-MANUAL.md` it serves. The set is the
`unsafe_obligations` table of `tools/common/data/safety-requirements.json`;
`check-safety-requirements.py` holds it to the manual and to its evidence.

The id is written in parentheses straight after the `SAFETY:` that clippy
looks for, and the prose stays as it was:

    // SAFETY: (DEVICE) `at` is inside a window the caller mapped as
    // device memory ...
    unsafe { core::ptr::read_volatile(at as *const u32) }

and on an `unsafe fn`, at the start of its `# Safety` section's first line:

    /// # Safety
    ///
    /// (TRANSLATE) The caller guarantees the new tables map what runs next.

Not `SAFETY(DEVICE):`, which reads better: clippy's
`undocumented_unsafe_blocks` looks for the text `SAFETY:` and refuses a block
whose comment spells it any other way (measured, rustc 1.97.1). One site may
name two ids, `(FRAME, DMA)`, when its one operation meets both; the per-id
counts count it under each.

Since 2026-10-02 the item also includes two library crates, `ferrix-btrfs`
and `ferrix-btrfs-write` (`crates` in the manifest). Their product files --
what their module trees reach except through `#[cfg(test)] mod`; their tests
are host tests and never in the image -- are item files here, named by their
path from the repository root. Both are `#![forbid(unsafe_code)]` today, so
they hold no site; the scope is so that the first one is traced.

Two more rules follow:

  4. An id anywhere in the tree must be one the registry defines. An id that
     resolves to nothing traces to nothing.
  5. In the item, a site with no id is debt, recorded per file in
     `tools/common/data/unsafe-trace-baseline.json`. A file may only improve: a new
     untagged site fails, and so does a count that has fallen without being
     re-recorded, so a tagged site leaves no allowance behind. The target is
     an empty baseline.

The report prints the unsafe-block count per crate and the item's sites per
obligation id. Those numbers are meant to be looked at in a diff: unsafe
growing is not a failure, but it should never grow without somebody noticing.

Usage:
    python3 tools/common/check/check-unsafe-audit.py
    python3 tools/common/check/check-unsafe-audit.py --report    # the untagged sites, by file
    python3 tools/common/check/check-unsafe-audit.py --record    # rewrite the baseline
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent.parent.parent
ROOTS = ("src/kernel", "src/boot/common/uefi", "src/lib", "src/user/system/native", "tools/common/xtask")
REGISTER = ROOT / "tools" / "common" / "data" / "safety-requirements.json"
BASELINE = ROOT / "tools" / "common" / "data" / "unsafe-trace-baseline.json"
KERNEL_SRC = ROOT / "src" / "kernel" / "src"

# `unsafe {` opening a block, but not `unsafe fn`, `unsafe impl`, `unsafe trait`
# or `unsafe extern`. Also matches the `unsafe` in `unsafe { ... }` used as an
# expression on the right of `=`.
UNSAFE_BLOCK = re.compile(r"(?<![\w:])unsafe\s*\{")
UNSAFE_IMPL = re.compile(r"(?<![\w:])unsafe\s+impl\b")
# `unsafe extern "Rust" {`: a link-time hook's declarations, whose types the
# compiler does not check against their definitions (the `LINK` obligation).
# The `extern "C"` blocks naming assembly are not counted here.
UNSAFE_EXTERN_RUST = re.compile(r'(?<![\w:])unsafe\s+extern\s+"Rust"')
UNSAFE_FN = re.compile(
    r"(?<![\w:])(?:pub(?:\([^)]*\))?\s+)?unsafe\s+(?:extern\s+\"[^\"]*\"\s+)?fn\s+(\w+)"
)
# `#[unsafe(naked)]`: a function whose body is assembly the compiler adds
# nothing to -- no prologue, no epilogue -- so its ABI toward its callers is
# the author's claim, not the compiler's (the consultant's Q8-C1).
UNSAFE_NAKED = re.compile(r"#\[\s*unsafe\s*\(\s*naked\s*\)\s*\]")
# `impl Trait for Type {`, whose methods implement someone else's contract.
TRAIT_IMPL = re.compile(r"^\s*(?:unsafe\s+)?impl\s*(?:<[^>]*>)?\s+[^;{]*\bfor\b[^;{]*\{")
SAFETY_COMMENT = re.compile(r"//\s*SAFETY:", re.IGNORECASE)
SAFETY_DOC = re.compile(r"///\s*#+\s*Safety\b", re.IGNORECASE)
# An obligation id list: `(DEVICE)` or `(FRAME, DMA)`.
ID_LIST = r"\(\s*([A-Z][A-Z0-9]*(?:-[A-Z0-9]+)*(?:\s*,\s*[A-Z][A-Z0-9]*(?:-[A-Z0-9]+)*)*)\s*\)"
# On a `// SAFETY:` comment: the list straight after the colon.
COMMENT_IDS = re.compile(r"//\s*SAFETY:\s*" + ID_LIST)
# On the first line of an `unsafe fn`'s `# Safety` section.
DOC_IDS = re.compile(r"^\s*///\s*(?:SAFETY:\s*)?" + ID_LIST)
# Lines that may sit between a SAFETY comment and the thing it covers:
# attributes, comments, blank lines and closing delimiters.
INTERVENING = re.compile(r"^\s*(#\[|#!\[|\)|\}|//|$)")
# How far back to look. A SAFETY comment more than a few lines from what it
# covers is not documenting it any more.
LOOKBACK = 8


def balanced_lines(lines: list[str], start: int) -> int:
    """Index of the line closing the block opened on line `start`."""
    depth = 0
    for index in range(start, len(lines)):
        line = lines[index]
        depth += line.count("{") - line.count("}")
        if index > start or "{" in line:
            if depth <= 0:
                return index
    return len(lines) - 1


def trait_impl_spans(lines: list[str]) -> list[tuple[int, int]]:
    """Line ranges covered by `impl Trait for Type { ... }`."""
    spans = []
    for index, line in enumerate(lines):
        if TRAIT_IMPL.match(line):
            spans.append((index, balanced_lines(lines, index)))
    return spans


def continuation(line: str) -> bool:
    """True if `line` is the middle of a statement rather than a whole one.

    rustfmt wraps `let x = unsafe { ... }` so that the binding and the `unsafe`
    land on different lines, which puts the SAFETY comment above the
    *statement* rather than above the block. Clippy accepts that shape --
    `accept-comment-above-statement` is on by default -- so this must too, or
    the script is stricter than the lint it exists to backstop, and the fix
    would be to un-format the code.
    """
    stripped = line.strip()
    return bool(stripped) and not stripped.endswith((";", "{", "}"))


def safety_line(lines: list[str], index: int) -> int | None:
    """The line of the `// SAFETY:` comment covering the construct on line
    `index`, or `None` if nothing covers it."""
    # `let x = /* SAFETY: ... */ unsafe { ... }` is unusual, but a trailing
    # `// SAFETY:` on the same line is a shape people write.
    if SAFETY_COMMENT.search(lines[index]):
        return index

    for scan in range(index - 1, max(index - 1 - LOOKBACK, -1), -1):
        line = lines[scan]
        if SAFETY_COMMENT.search(line):
            return scan
        # Comments, attributes and blank lines are always crossable; a partial
        # statement is crossable because the comment above it covers the whole
        # statement. A completed statement is where the search stops.
        if not INTERVENING.match(line) and not continuation(line):
            return None
    return None


def safety_section(lines: list[str], index: int) -> int | None:
    """The line of the `/// # Safety` heading in the doc comment above line
    `index`, or `None` if it has none."""
    scan = index - 1
    saw_doc = False
    while scan >= 0:
        line = lines[scan].strip()
        if line.startswith("///"):
            saw_doc = True
            if SAFETY_DOC.search(lines[scan]):
                return scan
        elif line.startswith("#[") or line.startswith("#!["):
            pass
        elif line.startswith("//") and not saw_doc:
            # A `// SAFETY:` comment on an attribute between the doc comment
            # and the item, as a naked function's has.
            pass
        elif line == "" and not saw_doc:
            pass
        else:
            return None
        scan -= 1
    return None


def split_ids(text: str) -> list[str]:
    return [part.strip() for part in text.split(",")]


def comment_ids(line: str) -> list[str]:
    match = COMMENT_IDS.search(line)
    return split_ids(match.group(1)) if match else []


def section_ids(lines: list[str], heading: int, index: int) -> list[str]:
    """The ids on the first non-blank line of the `# Safety` section that
    starts at `heading` and ends at the item on line `index`."""
    for scan in range(heading + 1, index):
        text = lines[scan].strip()
        if not text.startswith("///"):
            continue
        if text.lstrip("/").strip() == "":
            continue
        match = DOC_IDS.match(lines[scan])
        return split_ids(match.group(1)) if match else []
    return []


class Site:
    """One unsafe construct: a block, an impl, or an `unsafe fn` that needs a
    `# Safety` section."""

    def __init__(self, line: int, kind: str, ids: list[str], text: str):
        self.line = line  # 1-based
        self.kind = kind
        self.ids = ids
        self.text = text


def scan(source: str, label: str = "") -> tuple[list[str], list[Site], int]:
    """Problems, sites and the unsafe-block count of one source file."""
    if "unsafe" not in source:
        return [], [], 0
    lines = source.splitlines()
    problems: list[str] = []
    sites: list[Site] = []
    blocks = 0
    impls = trait_impl_spans(lines)

    for index, line in enumerate(lines):
        stripped = line.strip()
        # A `//` comment line is not code; `///` doc text mentioning unsafe is
        # not code either.
        if stripped.startswith("//"):
            continue

        if UNSAFE_BLOCK.search(line):
            blocks += 1
            covering = safety_line(lines, index)
            if covering is None:
                problems.append(f"{label}:{index + 1}: unsafe block with no `// SAFETY:` comment")
            else:
                sites.append(Site(index + 1, "block", comment_ids(lines[covering]), stripped))

        if UNSAFE_EXTERN_RUST.search(line):
            covering = safety_line(lines, index)
            if covering is None:
                problems.append(f"{label}:{index + 1}: unsafe extern \"Rust\" with no `// SAFETY:` comment")
            else:
                sites.append(Site(index + 1, "extern", comment_ids(lines[covering]), stripped))

        if UNSAFE_NAKED.search(line):
            covering = safety_line(lines, index)
            if covering is None:
                problems.append(f"{label}:{index + 1}: #[unsafe(naked)] with no `// SAFETY:` comment")
            else:
                sites.append(Site(index + 1, "naked", comment_ids(lines[covering]), stripped))

        if UNSAFE_IMPL.search(line):
            covering = safety_line(lines, index)
            if covering is None:
                problems.append(f"{label}:{index + 1}: unsafe impl with no `// SAFETY:` comment")
            else:
                sites.append(Site(index + 1, "impl", comment_ids(lines[covering]), stripped))

        match = UNSAFE_FN.search(line)
        in_trait_impl = any(begin <= index <= end for begin, end in impls)
        if match and not in_trait_impl:
            heading = safety_section(lines, index)
            if heading is None:
                problems.append(
                    f"{label}:{index + 1}: `unsafe fn {match.group(1)}` "
                    "has no `/// # Safety` section"
                )
            else:
                sites.append(Site(index + 1, "fn", section_ids(lines, heading, index), stripped))

    return problems, sites, blocks


def load_obligations() -> dict[str, dict]:
    register = json.loads(REGISTER.read_text(encoding="utf-8"))
    return {entry["id"]: entry for entry in register.get("unsafe_obligations", [])}


def item_files() -> tuple[dict[str, str], dict[str, str]]:
    """Every kernel file in the `core` or `item` ring, relative to
    src/kernel/src, with its ring; and every product file of an item crate,
    relative to the repository, with its crate."""
    spec = importlib.util.spec_from_file_location(
        "boundary", ROOT / "tools" / "common" / "check" / "check-item-boundary.py"
    )
    gate = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(gate)
    manifest = gate.load_manifest()
    ring_of, _, _ = gate.classify(manifest, gate.kernel_files())
    kernel = {rel: ring for rel, ring in ring_of.items() if ring in ("core", "item")}
    crates = {path: package for package, path in gate.item_crate_product_files(manifest)}
    return kernel, crates


# --- self-test ---------------------------------------------------------------

_SELF_TEST = r"""
fn a() {
    // SAFETY: (DEVICE) inside the window.
    unsafe { read() };
    // SAFETY: nothing traced.
    unsafe { write() };
    let x = unsafe { f() }; // SAFETY: (FRAME, DMA) both.
    // SAFETY: (SYSREG) a register, and a long
    // explanation over two lines.
    let value =
        unsafe { g() };
}

// SAFETY: (SHARED) one accessor.
unsafe impl Sync for Cell {}

/// Does a thing.
///
/// # Safety
///
/// (TRANSLATE) The caller guarantees the tables.
pub(crate) unsafe fn install() {}

/// # Safety
/// The caller guarantees nothing traced.
unsafe fn plain() {}

impl GlobalAlloc for A {
    unsafe fn alloc(&self) {}
}

// SAFETY: (LINK) tied to one alias on both sides.
unsafe extern "Rust" {
    safe fn hook() -> bool;
}

/// # Safety
///
/// (CONTEXT) The caller's stacks.
// SAFETY: (CONTEXT) the whole body, to the ABI.
#[unsafe(naked)]
unsafe extern "C" fn switch() {}
"""

_SELF_EXPECT = [
    (4, "block", ["DEVICE"]),
    (6, "block", []),
    (7, "block", ["FRAME", "DMA"]),
    (11, "block", ["SYSREG"]),
    (15, "impl", ["SHARED"]),
    (22, "fn", ["TRANSLATE"]),
    (26, "fn", []),
    (33, "extern", ["LINK"]),
    (41, "naked", ["CONTEXT"]),
    (42, "fn", ["CONTEXT"]),
]


def self_test() -> list[str]:
    problems, sites, blocks = scan(_SELF_TEST, "self-test")
    failures = [f"unexpected problem {problem}" for problem in problems]
    got = [(site.line, site.kind, site.ids) for site in sites]
    if got != _SELF_EXPECT:
        failures.append(f"got {got}, expected {_SELF_EXPECT}")
    if blocks != 4:
        failures.append(f"counted {blocks} blocks, expected 4")
    return failures


# --- the gate ------------------------------------------------------------------


def record(untagged: dict[str, int]) -> None:
    BASELINE.write_text(
        json.dumps(
            {
                "//": [
                    "Unsafe sites in the certified item -- the core and item rings,",
                    "self-tests included -- whose SAFETY comment or # Safety section",
                    "names no obligation id, per file, as tools/common/check/check-unsafe-audit.py",
                    "counts them. A debt register for finding F-26, not an allowance:",
                    "a count may only fall, and the gate fails when one rises, when a",
                    "new file appears, or when one has fallen without being",
                    "re-recorded. The target is an empty map.",
                ],
                "files": dict(sorted(untagged.items())),
            },
            indent=2,
        )
        + "\n"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", action="store_true")
    parser.add_argument("--record", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    failures = self_test()
    if failures:
        for failure in failures:
            print(f"unsafe-audit: self-test: {failure}", file=sys.stderr)
        return 1
    if args.self_test:
        print("unsafe-audit: self-test passes")
        return 0

    obligations = load_obligations()
    item, item_crate_files = item_files()
    crate_sites: dict[str, int] = {package: 0 for package in item_crate_files.values()}
    problems: list[str] = []
    unknown: list[str] = []
    per_crate: dict[str, int] = {}
    per_id: dict[str, int] = {name: 0 for name in obligations}
    untagged: dict[str, list[Site]] = {}
    item_sites = 0

    for name in ROOTS:
        directory = ROOT / name
        if not directory.is_dir():
            continue
        for path in sorted(directory.rglob("*.rs")):
            if "target" in path.parts:
                continue
            # `/` between the parts on every host, as the manifest and the
            # baseline name files.
            label = path.relative_to(ROOT).as_posix()
            found, sites, blocks = scan(path.read_text(encoding="utf-8"), label)
            problems.extend(found)
            if blocks:
                crate = path.relative_to(ROOT).parent.as_posix().split("src")[0].rstrip("/")
                per_crate[crate] = per_crate.get(crate, 0) + blocks
            for site in sites:
                for ident in site.ids:
                    if ident not in obligations:
                        unknown.append(f"{label}:{site.line}: ({ident}) is no obligation id")
            rel = path.relative_to(KERNEL_SRC).as_posix() if path.is_relative_to(KERNEL_SRC) else None
            if label in item_crate_files:
                # An item crate's product file, named from the repository root.
                rel = label
                crate_sites[item_crate_files[label]] += len(sites)
            if rel in item or rel in item_crate_files:
                item_sites += len(sites)
                for site in sites:
                    for ident in site.ids:
                        if ident in per_id:
                            per_id[ident] += 1
                missing = [site for site in sites if not site.ids]
                if missing:
                    untagged[rel] = missing

    if problems:
        for problem in problems:
            print(problem, file=sys.stderr)
        print(file=sys.stderr)
        print("Every unsafe construct must state why it is sound, at the site:", file=sys.stderr)
        print(file=sys.stderr)
        print("    // SAFETY: (FRAME) `phys` came from the frame allocator, so it is", file=sys.stderr)
        print("    // inside the physmap and 4 KiB aligned.", file=sys.stderr)
        print("    unsafe { core::ptr::write_bytes(virt, 0, PAGE_SIZE) };", file=sys.stderr)
        print(file=sys.stderr)
        print("See docs/RELIABILITY.md.", file=sys.stderr)
        return 1

    status = 0
    if unknown:
        print(
            "unsafe-audit: an obligation id the registry does not define. The ids\n"
            "  are the unsafe_obligations of tools/common/data/safety-requirements.json,\n"
            "  tabled in docs/certification/SAFETY-MANUAL.md:",
            file=sys.stderr,
        )
        for line in unknown:
            print(f"    {line}", file=sys.stderr)
        status = 1

    counts = {rel: len(sites) for rel, sites in untagged.items()}
    if args.report:
        for rel, sites in sorted(untagged.items()):
            print(f"{len(sites):5}  {rel}")
            for site in sites:
                print(f"         {site.line}: [{site.kind}] {site.text[:90]}")
        print(f"unsafe-audit: {sum(counts.values())} of {item_sites} item site(s) name no obligation")
        return status
    if args.record:
        record(counts)
        print(f"unsafe-audit: recorded {sum(counts.values())} untagged site(s) in {len(counts)} file(s)")
        return status

    if not BASELINE.exists():
        print("unsafe-audit: no baseline; run with --record and commit it.", file=sys.stderr)
        return 1
    baseline = json.loads(BASELINE.read_text(encoding="utf-8"))["files"]
    grown = [(rel, baseline.get(rel, 0), now) for rel, now in counts.items() if now > baseline.get(rel, 0)]
    shrunk = [(rel, was, counts.get(rel, 0)) for rel, was in baseline.items() if counts.get(rel, 0) < was]
    if grown:
        print(
            "unsafe-audit: an unsafe site in the certified item names no obligation.\n"
            "  Start its SAFETY comment `// SAFETY: (ID)`, or its # Safety section\n"
            "  `/// (ID)`, with the id of the obligation it discharges\n"
            "  (docs/certification/SAFETY-MANUAL.md, the unsafe obligations):",
            file=sys.stderr,
        )
        for rel, was, now in grown:
            print(f"    {rel}: {was} -> {now}", file=sys.stderr)
            for site in untagged.get(rel, []):
                print(f"      line {site.line}: {site.text[:80]}", file=sys.stderr)
        status = 1
    if shrunk:
        print(
            "unsafe-audit: fewer untagged sites than the baseline records.\n"
            "  Re-record (--record) so the tagged sites leave no allowance behind:",
            file=sys.stderr,
        )
        for rel, was, now in shrunk:
            print(f"    {rel}: {was} -> {now}", file=sys.stderr)
        status = 1

    total = sum(per_crate.values())
    print(f"unsafe-audit: {total} unsafe block(s), all documented")
    for crate in sorted(per_crate):
        print(f"    {per_crate[crate]:5}  {crate}")
    tagged = item_sites - sum(counts.values())
    print(
        f"unsafe-audit: {item_sites} unsafe site(s) in the certified item, {tagged} traced "
        f"to an obligation, {sum(counts.values())} in the baseline ({len(counts)} file(s))"
    )
    for ident, count in sorted(per_id.items(), key=lambda item: (-item[1], item[0])):
        print(f"    {count:5}  ({ident})")
    print("unsafe-audit: of them in the item's crates: "
          + ", ".join(f"{package} {count}" for package, count in sorted(crate_sites.items())))
    return status


if __name__ == "__main__":
    sys.exit(main())
