#!/usr/bin/env python3
"""Hold the certified item to fallible allocation (finding F-23).

The kernel's allocator returns null when memory has run out, and every
ordinary `Box`, `Arc`, `Vec` or map turns that null into the allocation error
handler, which stops the machine. So in the certified item -- the `core` and
`item` rings of `tools/common/data/certification-item.json`, product code only -- every
allocation goes through `src/kernel/src/fallible.rs`, which reports failure as an
error its caller can return. This gate is what keeps that true: it finds calls
to the standard library's allocating APIs in the item's source and fails when
one is not argued.

What it looks for
-----------------

Token patterns in source whose comments and literals `tools/common/check/rustlex.py` has
blanked:

  constructors  `Box::new`, `Arc::new`, `Arc::new_cyclic`, `Rc::new`, their
                `from`, `pin`, `new_uninit`, `new_zeroed`, the `_slice` forms
                of those two, `default` (a boxed or shared value is allocated
                even when it is a default) and `Arc::make_mut` and
                `unwrap_or_clone` (both clone the value when it is shared);
                `with_capacity`, `from` and `from_iter` of `Vec`, `String`,
                `VecDeque` and the maps. Each with or without a turbofish:
                `Box::<T>::new` is `Box::new`. `Vec::new()` and friends are
                not flagged: an empty collection allocates nothing.
  macros        `vec!`, `format!`.
  methods       every method of `alloc`'s collections that can grow one --
                `push`, `insert`, `insert_str`, `extend`, `append`, `resize`,
                `entry`, `collect`, `to_vec`, `to_string`, `sort` (a stable
                sort allocates a buffer), and the rest in `METHODS` below.
  `Default` and `Clone` that allocate
                A type whose `Default` allocates -- an `impl Default` whose
                body makes a flagged call, or a `#[derive(Default)]` over a
                field that is a `Box`, `Arc` or `Rc`, or of such a type -- is
                found in *every* kernel file, the load included, and then a
                `Type::default()` call of it in a scanned file is flagged, and
                so is a `#[derive(Default)]` there over a field of it. A
                `#[derive(Clone)]` in a scanned file over a field that is a
                `Box`, `Vec`, `String`, `VecDeque` or map is flagged too: the
                `.clone()` it makes possible is invisible below. This is how
                `syscall::signal::Signals::default` stopped the kernel on any
                `fork` or `process_create` without the gate seeing it (finding
                F-23's hole, fixed 2026-09-26).

What it cannot see, said here because a gate whose blind spots travel
separately is worse than none:

  * **Types.** A method is matched by name. `CpuSet::insert` sets a bit and is
    flagged like `BTreeSet::insert`; such a site carries a `NOALLOC:` comment
    saying why, and the comment is what a reviewer checks.
  * **`.clone()`** of a `Vec`, `String`, `Box` or map allocates; of an `Arc` it
    does not. The two cannot be told apart without types, so `clone` is not
    flagged at all. The item's clones were audited by hand when this gate was
    written (docs/certification/MEMORY-AND-TIMING.md section 1).
  * **Conversions.** `.into()` from a `&str` to a `String`, a slice to a `Vec`,
    and `?` converting into a boxed error allocate invisibly.
  * **Callees.** A call into `src/lib/` or into the uncertified load that
    allocates is not the item's source and is not scanned here -- except
    the load files in `REACHED` below, which the item's own native calls run
    to make a process or a thread, and which are scanned as the item is.
    The rest of the load those calls reach is listed in
    docs/certification/MEMORY-AND-TIMING.md section 1.
  * **`write!` into a `String`**, and `str::replace`, which share their names
    with calls that allocate nothing.

Markers
-------

A flagged call is accepted when the line it is on, or the block of `//`
comments directly above that line, says one of:

  `NOALLOC: <why>`       it does not allocate: a type-blind false positive, or
                         a push into capacity reserved fallibly just before.
  `FALLIBLE: <why>`      it may allocate and reports failure: a first-party
                         method named like a standard one, such as the handle
                         table's `insert` and `reserve`.
  `FATAL-ALLOC: <why>`   it allocates, and failure stops the machine by
                         design -- only at boot, before the first program runs,
                         where there is nothing to return an error to.
                         Listed with `--report`, and in MEMORY-AND-TIMING.md.

The item's library crates
-------------------------

Since 2026-10-02 the item includes two crates outside the kernel,
`ferrix-btrfs` and `ferrix-btrfs-write` (`crates` in the manifest), and their
product files are scanned with the same patterns and markers -- every file
their module trees reach except through `#[cfg(test)] mod`, whose code is host
tests. A crate cannot reach `src/kernel/src/fallible.rs`, so what counts as
fallible there is:

  * the standard library's own fallible forms, `try_reserve` and
    `try_reserve_exact` on `Vec`, `VecDeque` and `String`, which the patterns
    do not match;
  * the `ferrix-fallible` crate's functions (`try_box`, `try_push`,
    `try_with_capacity`, `try_collect` and the rest): the code the kernel's
    own toolkit is built on, and allowlisted for item crates in the manifest
    (`crates.infrastructure_allowlist`), so they do not match either;
  * a helper of the crate's own whose infallible call is made into capacity
    it has just reserved fallibly, which carries `NOALLOC:` saying so, or a
    first-party method named like a standard one, which carries `FALLIBLE:`.

There is no toolkit file exempt by name in a crate: a helper's push says why it
cannot fail at the push. `FATAL-ALLOC:` has no use in a crate, which is never
the boot.

Each crate is held as a whole, not per file: `crates` in the baseline records
its unmarked count, which may only fall, and like a file's must be re-recorded
when it has. Its `by_file` breakdown is not the ratchet; it is where the gate
looks to name the file when the total rises. The target is zero for both; the
conversion is the work of branch `btrfs-fallible` (2026-10-02), and the
baseline's own entry says so.

The ratchet
-----------

Like the other gates over the item: `tools/common/data/fallible-alloc-baseline.json`
records, per file, the flagged calls that carry no marker. A file may improve
freely; a new unmarked call fails the build, and so does an entry that has
shrunk without being re-recorded, so a converted site cannot leave an
allowance behind. The target is an empty baseline.

    python3 tools/common/check/check-fallible-alloc.py
    python3 tools/common/check/check-fallible-alloc.py --report     # every site, and the markers
    python3 tools/common/check/check-fallible-alloc.py --record     # rewrite the baseline
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
BASELINE = ROOT / "tools" / "common" / "data" / "fallible-alloc-baseline.json"

# The toolkit itself: the one place the infallible APIs are called on
# purpose, each inside a reserved section or behind a fallible reservation.
TOOLKIT = {"fallible.rs"}

# An optional turbofish after a type name: `Box::<T>::new` is `Box::new`.
TURBOFISH = r"(?:\s*::\s*<[^{}]*?>)?"
CONSTRUCTORS = re.compile(
    r"\b(?:Box|Arc|Rc)" + TURBOFISH + r"\s*::\s*(?:new|new_cyclic|new_uninit|new_zeroed"
    r"|new_uninit_slice|new_zeroed_slice|pin|from|from_iter|default|make_mut"
    r"|unwrap_or_clone)\b"
    r"|\b(?:Vec|String|VecDeque|BTreeMap|BTreeSet|BinaryHeap|HashMap|HashSet)" + TURBOFISH
    + r"\s*::\s*(?:with_capacity|from|from_iter)\b"
)
MACROS = re.compile(r"\b(?:vec|format)\s*!")
METHODS = re.compile(
    r"\.\s*(?:push|push_back|push_front|push_str|insert|insert_str|extend|extend_from_slice"
    r"|extend_from_within|append|resize|resize_with|entry|reserve|reserve_exact"
    r"|collect|to_vec|to_string|to_owned|into_boxed_slice|into_boxed_str"
    r"|shrink_to_fit|shrink_to|sort|sort_by|sort_by_key|sort_by_cached_key"
    r"|concat|join|repeat|split_off|or_insert|or_insert_with|or_default"
    r"|to_ascii_lowercase|to_ascii_uppercase|to_lowercase|to_uppercase"
    r"|into_owned|clone_from|partition|unzip)\s*(?:\(|::)"
)
PATTERNS = (CONSTRUCTORS, MACROS, METHODS)

# Load-ring files the item's own native calls run: `process_create` and
# `process_start` make a POSIX process and its first thread through them, so
# an allocation in them that cannot fail stops the kernel on an item path.
# Scanned as the item is. `Signals::default` in the first was F-23's hole.
# The last three are `process_create`'s load: the `Processes::load` hook,
# then the ELF loader it runs (consultant ledger 5546, answer A, 2026-10-31).
REACHED = {
    "syscall/signal.rs": "a process's and a thread's signal state",
    "syscall/thread.rs": "a process's first thread",
    "syscall/process.rs": "the process process_create loads and process_start starts",
    "syscall/launch.rs": "the Processes::load and ::start hooks, load_native and start_native",
    "syscall/exec.rs": "load_native_in and populate, which build the process from its image",
    "syscall/load.rs": "the ELF image's headers, segments and mapping plan",
}

# A derive list, what it is on, and the start of its body.
DERIVE = re.compile(
    r"#\[derive\(([^)]*)\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?"
    r"struct\s+(\w+)[^{;]*\{"
)
IMPL_DEFAULT = re.compile(r"\bimpl\s+(?:<[^>]*>\s*)?Default\s+for\s+(\w+)[^{;]*\{")
# A field's type, from the colon: the identifier it starts with.
FIELD = re.compile(r"(?:^|,|\{)\s*(?:pub(?:\([^)]*\))?\s+)?\w+\s*:\s*([A-Za-z_][\w:]*)")
# Fields whose `Default` allocates, and whose `Clone` does.
ALLOCATING_DEFAULT_FIELDS = {"Box", "Arc", "Rc"}
ALLOCATING_CLONE_FIELDS = {
    "Box", "Vec", "String", "VecDeque", "BTreeMap", "BTreeSet", "BinaryHeap", "HashMap", "HashSet",
}

MARKERS = {"NOALLOC:": "noalloc", "FALLIBLE:": "fallible", "FATAL-ALLOC:": "fatal"}
TEST_MODULE = re.compile(r"#\[cfg\(test\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{")


def without_test_modules(masked: str) -> str:
    """`masked` with every `#[cfg(test)] mod x { ... }` blanked, length kept."""
    out = masked
    for match in TEST_MODULE.finditer(masked):
        depth = 0
        for index in range(match.end() - 1, len(masked)):
            if masked[index] == "{":
                depth += 1
            elif masked[index] == "}":
                depth -= 1
                if depth == 0:
                    span = out[match.start() : index + 1]
                    out = out[: match.start()] + re.sub(r"[^\n]", " ", span) + out[index + 1 :]
                    break
    return out


def marker_for(lines: list[str], line: int) -> str | None:
    """The marker covering 1-based `line`: on it, or in the comments above."""
    candidates = []
    if 0 < line <= len(lines):
        candidates.append(lines[line - 1])
    above = line - 1
    while above >= 1 and lines[above - 1].strip().startswith("//"):
        candidates.append(lines[above - 1])
        above -= 1
    for text in candidates:
        comment = text[text.find("//") :] if "//" in text else ""
        for marker, kind in MARKERS.items():
            if marker in comment:
                return kind
    return None


def body_at(masked: str, opening: int) -> str:
    """The text between the `{` just before `opening` and its partner."""
    depth = 1
    index = opening
    while depth and index < len(masked):
        if masked[index] == "{":
            depth += 1
        elif masked[index] == "}":
            depth -= 1
        index += 1
    return masked[opening : index - 1]


def field_types(body: str) -> set[str]:
    """The last path segment of the type each field of a struct body starts with."""
    return {match.group(1).split("::")[-1] for match in FIELD.finditer(body)}


def allocating_defaults(sources: list[str]) -> set[str]:
    """Types whose `Default` allocates, across `sources`: an `impl Default`
    whose body makes a flagged call, or a derive over a field that is a box,
    a shared pointer, or of a type already found -- to a fixed point."""
    found: set[str] = set()
    derived: list[tuple[str, set[str]]] = []
    for source in sources:
        masked = without_test_modules(rustlex.mask(source))
        for match in IMPL_DEFAULT.finditer(masked):
            body = body_at(masked, match.end())
            if any(pattern.search(body) for pattern in PATTERNS):
                found.add(match.group(1))
        for match in DERIVE.finditer(masked):
            if re.search(r"\bDefault\b", match.group(1)):
                derived.append((match.group(2), field_types(body_at(masked, match.end()))))
    grew = True
    while grew:
        grew = False
        for name, fields in derived:
            if name not in found and fields & (ALLOCATING_DEFAULT_FIELDS | found):
                found.add(name)
                grew = True
    return found


def scan_source(source: str, defaults: frozenset[str] = frozenset()) -> list[dict]:
    """Every flagged call in `source`, with its line, text and marker.

    `defaults` names the types whose `Default` allocates
    ([`allocating_defaults`] over the whole kernel)."""
    masked = without_test_modules(rustlex.mask(source))
    lines = source.splitlines()
    found = []

    def flag(offset: int, call: str) -> None:
        line = masked.count("\n", 0, offset) + 1
        found.append({"line": line, "call": call, "marker": marker_for(lines, line)})

    for pattern in PATTERNS:
        for match in pattern.finditer(masked):
            call = re.sub(r"\s+", "", match.group())
            flag(match.start(), re.sub(r"::<.*?>(?=::)", "", call).rstrip("(:"))
    if defaults:
        calls = re.compile(r"\b(" + "|".join(sorted(defaults)) + r")" + TURBOFISH + r"\s*::\s*default\s*\(")
        for match in calls.finditer(masked):
            flag(match.start(), f"{match.group(1)}::default")
    for match in DERIVE.finditer(masked):
        traits = match.group(1)
        fields = field_types(body_at(masked, match.end()))
        if re.search(r"\bDefault\b", traits) and fields & (ALLOCATING_DEFAULT_FIELDS | defaults):
            flag(match.start(), f"derive(Default) {match.group(2)}")
        if re.search(r"\bClone\b", traits) and fields & ALLOCATING_CLONE_FIELDS:
            flag(match.start(), f"derive(Clone) {match.group(2)}")
    return sorted(found, key=lambda site: site["line"])


def load_gate():
    spec = importlib.util.spec_from_file_location(
        "boundary", ROOT / "tools" / "common" / "check" / "check-item-boundary.py"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def crate_sites(gate, manifest: dict) -> dict[str, tuple[str, list[dict]]]:
    """`{path: (package, sites)}` over the item crates' product files, keyed by
    the path from the repository root. Their `Default`s are found across the
    item crates only: a kernel type of the same name is not theirs."""
    files = gate.item_crate_product_files(manifest)
    sources = {path: (package, (ROOT / path).read_text(encoding="utf-8", errors="replace"))
               for package, path in files}
    defaults = frozenset(allocating_defaults([source for _, source in sources.values()]))
    return {path: (package, scan_source(source, defaults))
            for path, (package, source) in sorted(sources.items())}


def crate_counts(sites: dict[str, tuple[str, list[dict]]], manifest_crates) -> dict[str, int]:
    """Unmarked calls per item crate, every item crate present."""
    counts = {package: 0 for package in manifest_crates}
    for package, found in sites.values():
        counts[package] = counts.get(package, 0) + sum(1 for s in found if s["marker"] is None)
    return counts


def item_crate_names(gate, manifest: dict) -> list[str]:
    return sorted(name for name, entry in gate.crate_entries(manifest).items()
                  if entry["ring"] in ("core", "item"))


def item_sites() -> dict[str, list[dict]]:
    """Flagged calls in every product-code file of the certified item, and in
    the load files the item's own calls run ([`REACHED`])."""
    gate = load_gate()
    manifest = gate.load_manifest()
    ring_of, _, _ = gate.classify(manifest, gate.kernel_files())
    sources = {
        rel: (KERNEL_SRC / rel).read_text(encoding="utf-8", errors="replace")
        for rel in ring_of
        if not gate.is_test_file(rel, manifest)
    }
    defaults = frozenset(allocating_defaults(list(sources.values())))
    missing = sorted(rel for rel in REACHED if ring_of.get(rel) != "load")
    if missing:
        raise SystemExit(
            f"fallible-alloc: REACHED names {', '.join(missing)}, which is not a load-ring "
            "file any more: move it out of the list, or say where it went"
        )
    out: dict[str, list[dict]] = {}
    for rel, source in sorted(sources.items()):
        if (ring_of[rel] == "load" and rel not in REACHED) or rel in TOOLKIT:
            continue
        sites = scan_source(source, defaults)
        if sites:
            out[rel] = sites
    return out


def unmarked_counts(sites: dict[str, list[dict]]) -> dict[str, int]:
    counts = {}
    for rel, found in sites.items():
        count = sum(1 for site in found if site["marker"] is None)
        if count:
            counts[rel] = count
    return counts


def report(sites: dict[str, list[dict]], crates: dict[str, tuple[str, list[dict]]]) -> None:
    for package in sorted({package for package, _ in crates.values()}):
        mine = {path: found for path, (owner, found) in crates.items() if owner == package}
        unmarked_here = unmarked_counts(mine)
        print(f"fallible-alloc: crate {package}: {sum(unmarked_here.values())} unmarked "
              f"allocating call(s) in {len(unmarked_here)} file(s)")
        for path, count in sorted(unmarked_here.items(), key=lambda item: -item[1]):
            calls: dict[str, int] = {}
            for site in mine[path]:
                if site["marker"] is None:
                    calls[site["call"]] = calls.get(site["call"], 0) + 1
            detail = ", ".join(f"{c} {n}" for c, n in sorted(calls.items(), key=lambda c: -c[1]))
            print(f"  {count:>4}  {path}: {detail}")
    unmarked = unmarked_counts(sites)
    total = sum(unmarked.values())
    noalloc = sum(1 for found in sites.values() for s in found if s["marker"] == "noalloc")
    fallible = sum(1 for found in sites.values() for s in found if s["marker"] == "fallible")
    fatal = [
        (rel, site) for rel, found in sites.items() for site in found if site["marker"] == "fatal"
    ]
    print(
        f"fallible-alloc: {total} unmarked allocating call(s) in {len(unmarked)} file(s); "
        f"{noalloc} marked NOALLOC, {fallible} FALLIBLE, {len(fatal)} FATAL-ALLOC"
    )
    for rel, count in sorted(unmarked.items(), key=lambda item: -item[1]):
        calls: dict[str, int] = {}
        for site in sites[rel]:
            if site["marker"] is None:
                calls[site["call"]] = calls.get(site["call"], 0) + 1
        detail = ", ".join(f"{call} {n}" for call, n in sorted(calls.items(), key=lambda c: -c[1]))
        print(f"  {count:>4}  {rel}: {detail}")
    if fatal:
        print("  fatal by design, at boot only:")
        for rel, site in fatal:
            print(f"    {rel}:{site['line']}  {site['call']}")


# --- self-test ---------------------------------------------------------------

_SELF_TEST = r"""
fn a() {
    let v = Vec::new();
    v.push(1);
    let s = format!("{}", 1);
    // NOALLOC: a bit set, not a tree.
    cpus.insert(3);
    let b = Box::new(1); // FATAL-ALLOC: boot only
    let text = "Arc::new(x) and v.push(y) in a string";
    // Arc::new in a comment
    let a = crate::fallible::try_arc(1)?;
    let c: Vec<_> = it.collect();
    let d = it.collect::<Vec<_>>();
    let e = alloc::vec![0; 4];
}

#[cfg(test)]
mod tests {
    fn t() { let v = vec![1]; v.push(2); }
}
"""

_SELF_EXPECT = [
    (4, "push", None),
    (5, "format!", None),
    (7, "insert", "noalloc"),
    (8, "Box::new", "fatal"),
    (12, "collect", None),
    (13, "collect", None),
    (14, "vec!", None),
]


# The forms added when `Signals::default` was found to have got past: a load
# file (`_SELF_LOAD`) whose `Default`s allocate, and item code (`_SELF_ITEM`)
# that reaches them, or allocates in ways the first patterns missed.
_SELF_LOAD = r"""
struct Queue { pending: u64, origins: Box<[u8]> }
impl Default for Queue {
    fn default() -> Self { Queue { pending: 0, origins: vec![0; 64].into_boxed_slice() } }
}
#[derive(Debug, Default)]
pub(crate) struct Signals { shared: Queue, alarm: u64 }
#[derive(Default)]
struct Plain { count: u64, name: Option<Box<u8>> }
impl Default for Quiet {
    fn default() -> Self { Quiet { count: 0 } }
}
"""

_SELF_ITEM = r"""
fn a() {
    let s = Signals::default();
    let q = crate::syscall::signal::Queue::default();
    let p = Plain::default();
    let b = Box::<[u8; 4]>::new([0; 4]);
    let v = Vec::<Vec<u8>>::with_capacity(4);
    let d: Box<u64> = Box::default();
    let m = Arc::make_mut(&mut shared);
    text.insert_str(0, "x");
    let u = Box::<[u8]>::new_uninit_slice(4);
}
#[derive(Clone, Debug)]
struct Held { ranges: Vec<u64>, count: u64 }
#[derive(Clone, Copy)]
struct Small { count: u64 }
#[derive(Default)]
struct Holder { signals: Signals }
"""

_SELF_ITEM_EXPECT = [
    (3, "Signals::default", None),
    (4, "Queue::default", None),
    (6, "Box::new", None),
    (7, "Vec::with_capacity", None),
    (8, "Box::default", None),
    (9, "Arc::make_mut", None),
    (10, "insert_str", None),
    (11, "Box::new_uninit_slice", None),
    (13, "derive(Clone) Held", None),
    (17, "derive(Default) Holder", None),
]


def self_test() -> list[str]:
    failures = [f"lexer: {f}" for f in rustlex.self_test()]
    got = [(site["line"], site["call"].lstrip("."), site["marker"]) for site in scan_source(_SELF_TEST)]
    if got != _SELF_EXPECT:
        failures.append(f"scan: got {got}, expected {_SELF_EXPECT}")
    defaults = allocating_defaults([_SELF_LOAD])
    if defaults != {"Queue", "Signals"}:
        failures.append(f"defaults: got {sorted(defaults)}, expected Queue and Signals")
    got = [
        (site["line"], site["call"].lstrip("."), site["marker"])
        for site in scan_source(_SELF_ITEM, frozenset(defaults))
    ]
    if got != _SELF_ITEM_EXPECT:
        failures.append(f"reach: got {got}, expected {_SELF_ITEM_EXPECT}")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", action="store_true")
    parser.add_argument("--record", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    failures = self_test()
    if failures:
        for failure in failures:
            print(f"fallible-alloc: self-test: {failure}", file=sys.stderr)
        return 1
    if args.self_test:
        print("fallible-alloc: self-test passes")
        return 0

    sites = item_sites()
    gate = load_gate()
    manifest = gate.load_manifest()
    crates = crate_sites(gate, manifest)
    if args.report:
        report(sites, crates)
        return 0

    current = unmarked_counts(sites)
    current_crates = crate_counts(crates, item_crate_names(gate, manifest))
    old_crates = json.loads(BASELINE.read_text()).get("crates", {}) if BASELINE.exists() else {}
    # Per file inside each crate: not the ratchet, which is the crate's total,
    # but where to look when the total rises.
    per_file: dict[str, dict[str, int]] = {}
    for path, (package, found) in crates.items():
        count = sum(1 for site in found if site["marker"] is None)
        if count:
            per_file.setdefault(package, {})[path] = count
    if args.record:
        BASELINE.write_text(
            json.dumps(
                {
                    "//": [
                        "Calls to allocating standard-library APIs in the certified",
                        "item's product code that carry no NOALLOC or FATAL-ALLOC",
                        "marker, per file, as tools/common/check/check-fallible-alloc.py counts",
                        "them. A debt register for finding F-23, not an allowance:",
                        "a count may only fall, and the gate fails when one rises,",
                        "when a new file appears, or when one has fallen without",
                        "being re-recorded. The target is an empty map.",
                    ],
                    "files": dict(sorted(current.items())),
                    "//crates": [
                        "The item's library crates (certification-item.json, crates),",
                        "held whole: the unmarked calls in each one's product code.",
                        "The same ratchet as the files: a count may only fall, and",
                        "one that has fallen must be re-recorded. Each entry's own",
                        "note says who is burning it down; the target is zero.",
                    ],
                    "crates": {
                        package: {
                            "//": old_crates.get(package, {}).get(
                                "//", "Target zero. Nobody is named as converting it yet."
                            ),
                            "count": count,
                            "by_file": dict(sorted(per_file.get(package, {}).items())),
                        }
                        for package, count in sorted(current_crates.items())
                    },
                },
                indent=2,
                ensure_ascii=False,
            )
            + "\n"
        )
        print(f"fallible-alloc: recorded {sum(current.values())} call(s) in {len(current)} file(s), "
              f"{sum(current_crates.values())} in {len(current_crates)} item crate(s)")
        return 0

    if not BASELINE.exists():
        print("fallible-alloc: no baseline; run with --record and commit it.", file=sys.stderr)
        return 1
    baseline = json.loads(BASELINE.read_text())["files"]

    status = 0
    grown = [(rel, baseline.get(rel, 0), now) for rel, now in current.items() if now > baseline.get(rel, 0)]
    shrunk = [(rel, was, current.get(rel, 0)) for rel, was in baseline.items() if current.get(rel, 0) < was]
    if grown:
        print(
            "fallible-alloc: infallible allocation added to the certified item.\n"
            "  Use src/kernel/src/fallible.rs, or argue the site with NOALLOC: or\n"
            "  FATAL-ALLOC: (see this script's docstring):",
            file=sys.stderr,
        )
        for rel, was, now in grown:
            print(f"    {rel}: {was} -> {now}", file=sys.stderr)
            for site in sites.get(rel, []):
                if site["marker"] is None:
                    print(f"      line {site['line']}: {site['call']}", file=sys.stderr)
        status = 1
    if shrunk:
        print(
            "fallible-alloc: fewer unmarked calls than the baseline records.\n"
            "  Re-record so the converted sites leave no allowance behind:",
            file=sys.stderr,
        )
        for rel, was, now in shrunk:
            print(f"    {rel}: {was} -> {now}", file=sys.stderr)
        status = 1

    crate_baseline = {package: entry["count"] for package, entry in old_crates.items()}
    grown = [(p, crate_baseline.get(p), now) for p, now in current_crates.items()
             if crate_baseline.get(p) is None or now > crate_baseline[p]]
    shrunk = [(p, was, current_crates.get(p, 0)) for p, was in crate_baseline.items()
              if current_crates.get(p, 0) < was or p not in current_crates]
    if grown:
        print(
            "fallible-alloc: infallible allocation added to an item crate, or a crate\n"
            "  not in the baseline. A crate cannot use src/kernel/src/fallible.rs:\n"
            "  use try_reserve or ferrix-fallible, or argue the site with NOALLOC:\n"
            "  (see this script's docstring, \"The item's library crates\"):",
            file=sys.stderr,
        )
        for package, was, now in grown:
            print(f"    {package}: {was if was is not None else 'unrecorded'} -> {now}", file=sys.stderr)
            recorded = old_crates.get(package, {}).get("by_file", {})
            rose = [path for path, count in per_file.get(package, {}).items()
                    if count > recorded.get(path, 0)]
            for path in rose if was is not None else []:
                print(f"      {path}: {recorded.get(path, 0)} -> {per_file[package][path]}",
                      file=sys.stderr)
                for site in crates[path][1]:
                    if site["marker"] is None:
                        print(f"        line {site['line']}: {site['call']}", file=sys.stderr)
        status = 1
    if shrunk:
        print(
            "fallible-alloc: fewer unmarked calls in an item crate than the baseline\n"
            "  records. Re-record (--record) so the converted sites leave no\n"
            "  allowance behind:",
            file=sys.stderr,
        )
        for package, was, now in shrunk:
            print(f"    {package}: {was} -> {now}", file=sys.stderr)
        status = 1

    fatal = sum(1 for found in sites.values() for s in found if s["marker"] == "fatal")
    in_crates = ", ".join(f"{p} {n}" for p, n in sorted(current_crates.items()))
    print(f"fallible-alloc: item crates, unmarked allocating calls: {in_crates}")
    print(
        f"fallible-alloc: {sum(current.values())} unmarked allocating call(s) in the item and "
        f"the {len(REACHED)} load files it reaches ({len(current)} file(s)), {fatal} fatal by design"
    )
    return status


if __name__ == "__main__":
    sys.exit(main())
