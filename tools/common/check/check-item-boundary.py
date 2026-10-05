#!/usr/bin/env python3
"""Hold the boundary of the certified item.

`tools/common/data/certification-item.json` says which kernel files and library crates are inside the thing
four assurance ratings attach to. That file is the scope of every artifact in
`docs/certification`: the Security Target's TOE, the hazard analysis's safety
item, the traceability matrix, the coverage obligation. A boundary that lives
only in a document is a boundary that has already moved.

So this gate asserts four things.

  1. **Every kernel source file is classified.** A file in no ring fails the
     build. Without this the item grows by accretion -- somebody writes
     `src/kernel/src/thing.rs`, nobody decides whether it is trusted, and the
     answer defaults to whatever the reader assumes. A new file should cost
     one line of JSON and the thought that goes with it.

  2. **Rings do not reach upward.** The core may not name the item, and
     neither may name the uncertified load. This is the claim that makes a
     small item meaningful: if the trusted core calls into the filesystem,
     then the filesystem is in the trusted core no matter what a document
     says. This is the check that can actually fail, and the one worth having.

  3. **Known violations do not grow.** Today's breaches are listed in the
     manifest with finding ids. The list may shrink without ceremony; it may
     not grow without editing this file's input, which is a diff somebody has
     to argue for. Stale entries fail too, so a fixed violation cannot leave
     a permanent exemption behind.

  4. **Library crates are classified whole and held at their manifests.**
     Since 2026-10-02 the item includes two crates outside the kernel,
     `ferrix-btrfs` and `ferrix-btrfs-write`, under `crates` in the manifest.
     A classified crate's files are product or test code by its module tree
     (a file reached only through `#[cfg(test)] mod` is a host test; one
     the tree does not reach fails). An item or core crate's shipping
     dependencies -- `[dependencies]` and `[build-dependencies]`, target-
     specific ones included, never `[dev-dependencies]` -- must be crates of
     its own ring or below, or in `crates.infrastructure_allowlist`. An
     allowlisted crate's own dependencies are held to the same rule only
     once an item crate depends on it -- the walk starts at the item crates
     -- so one no item crate uses may gain a load dependency unremarked.
     Each allowlist entry must
     still be named by a core or item kernel file, which is its reason. And
     a kernel file that names a classified crate (`ferrix_btrfs::..`) is an
     edge to that crate's ring, under the same rules as rule 2. The
     item-scoped gates (complexity, fallible allocation, the unsafe trace)
     read the item crates' product files through `item_crate_product_files`.

Product code and in-kernel test code are counted apart. A standard asks
different questions of each, and conflating them flatters the item's size
while hiding how much verification exists.

How a reference is found
------------------------

Until 2026-09-26 this gate matched the text `crate::a::b` and nothing else. It
saw a fraction of the edges, and so every count it reported was a lower bound.
Three routes were invisible: a nested group (`use crate::syscall::{exec,
process}` names two modules, and the gate saw `crate::syscall`); a module-
relative path (`super::process::`, `self::fd::`, or plain `exec::` in a file
that declares `mod exec;` or imports it with `use`); and a re-export (`pub use
process::current;` in an item file, and every caller of it). Worse, the regular
expression that stripped strings first mis-paired quotes after any `\\`-newline
continuation, so whole stretches of code were read as string.

It now resolves names the way the compiler does, minus type checking:

  * `tools/common/check/rustlex.py` masks comments and literals exactly, so only code is
    read;
  * each file's module path comes from where it sits (`main.rs` is the root,
    `a/mod.rs` and `a.rs` are `a`, `a/b.rs` is `a::b`), and inline `mod x { }`
    blocks nest inside it;
  * every `use` tree is expanded however deeply it nests, and each leaf binds a
    name in its module;
  * every path in code -- and every `use` path -- is resolved from its first
    segment: `crate`, `self`, `super`, a name bound by `use`, or a child module
    the same module declares. Anything else (an external crate, a local type, a
    generic) is not a module of this crate and is not an edge;
  * a path is followed through `use` bindings in the modules it passes through,
    so a re-export is charged to the module that *defines* the item. The
    re-exporting `use` is itself an edge of the file that holds it.

A reference's key in the register is the module the path lands in: the file
that defines what it names. A `use` inside a function body is treated as if it
were at module scope, which can only over-report.

What it still cannot see: a dependency with no name written in the file -- a
value of a load-ring type handed to an item file by a function that names it,
a method called on that value, a trait object -- since that needs the type
checker; and a path assembled by a macro from pieces. What is left unseen is
coupling through the type system, not through the spelling.

**A `mod x;` declaration is not an edge.** It is containment -- the module tree
has to be rooted somewhere, and a parent declaring its child says where the
child sits, not that the parent's code runs the child's. `syscall/mod.rs`
declares 30 load-ring modules and `main.rs` every top-level module in the
kernel; those declarations are counted in `--report` so they stay visible, and
are not debt. What the declaring file's *code* does with the child is resolved
like any other path and counted in full.

**The composition root.** `main.rs` is in the item ring (it is bring-up), and
it is also the crate root, where the load is composed with the item: told to
register into the item's interfaces, brought up in order, and self-checked at
boot. Its edges into the load are listed apart from the debt register, under
`composition_root` in the manifest, with the same ratchet: they may shrink, may
not grow unrecorded, and a stale one fails. They are not filed against a
finding because composing the load is what the item's design asks main.rs to
do; they are not silently allowed because an assessor has to be able to read
every one of them (docs/certification/ITEM.md, section 2).

Run it directly, or as part of `cargo xtask check`:

    python3 tools/common/check/check-item-boundary.py
    python3 tools/common/check/check-item-boundary.py --report     # sizes and every edge
    python3 tools/common/check/check-item-boundary.py --self-test  # the resolver's cases

Every run starts with the self-tests of the lexer and of the resolver, so a
change that breaks either fails the gate rather than quietly measuring less.
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import re
import sys
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import rustlex  # noqa: E402  (after the path insert)

ROOT = Path(__file__).resolve().parent.parent.parent.parent
MANIFEST = ROOT / "tools" / "common" / "data" / "certification-item.json"
KERNEL_SRC = ROOT / "src" / "kernel" / "src"

IDENT = re.compile(r"(?:r#)?[^\W\d]\w*\Z")
# Keywords that define a named item, which a path's last segment may name.
DEFINES = {"fn", "struct", "enum", "union", "trait", "type", "const", "static"}
# Constructs that would make a file's module path something other than where it
# sits, or paste in code this gate never reads. The kernel has none; if one
# appears the gate says it cannot measure the file rather than measuring less.
UNREADABLE = re.compile(r"#\s*\[\s*path\s*=|(?<![\w:])include\s*!")


def load_manifest() -> dict:
    with MANIFEST.open(encoding="utf-8") as handle:
        return json.load(handle)


def kernel_files() -> list[str]:
    """Every kernel source file, as a path relative to src/kernel/src, with `/`
    between its parts on every host: the manifest's patterns and the module
    tree are written that way."""
    return sorted(
        path.relative_to(KERNEL_SRC).as_posix() for path in KERNEL_SRC.rglob("*.rs")
    )


def matches(pattern: str, rel: str) -> bool:
    """`arch/**` matches anything under arch/; `mm.rs` matches exactly."""
    if pattern.endswith("/**"):
        return rel.startswith(pattern[:-2])
    return fnmatch.fnmatch(rel, pattern)


def is_test_file(rel: str, manifest: dict) -> bool:
    name = rel.rsplit("/", 1)[-1]
    for pattern in manifest["test_file_patterns"]:
        if "/" in pattern:
            if matches(pattern, rel):
                return True
        elif fnmatch.fnmatch(name, pattern):
            return True
    return False


def classify(manifest: dict, files: list[str]) -> tuple[dict[str, str], list[str], list[tuple[str, list[str]]]]:
    """Map each file to its ring. Returns (ring_of, unclassified, ambiguous)."""
    ring_of: dict[str, str] = {}
    unclassified: list[str] = []
    ambiguous: list[tuple[str, list[str]]] = []

    for rel in files:
        hits = [
            name
            for name, ring in manifest["rings"].items()
            if any(matches(p, rel) for p in ring["members"])
        ]
        if not hits:
            unclassified.append(rel)
        elif len(hits) > 1:
            ambiguous.append((rel, hits))
        else:
            ring_of[rel] = hits[0]

    return ring_of, unclassified, ambiguous


def stale_members(manifest: dict, files: list[str]) -> list[tuple[str, str]]:
    """(ring, pattern) for each ring member that matches no kernel file. A
    pattern left behind by a move classifies nothing today and would silently
    classify whatever is next created under its name."""
    return [
        (name, pattern)
        for name, ring in manifest["rings"].items()
        for pattern in ring["members"]
        if not any(matches(pattern, rel) for rel in files)
    ]


# --- the module tree ---------------------------------------------------------

ModPath = tuple[str, ...]


# Plain classes rather than dataclasses: check-complexity.py and
# coverage-report.py load this file with importlib, under a module name that is
# not in sys.modules, and a dataclass needs to find its own module there.
class Module:
    """One module: where it is written, and the names its scope holds."""

    def __init__(self, path: ModPath, file: str):
        self.path = path
        self.file = file
        # Child modules declared here, by `mod x;` or `mod x { }`.
        self.children: set[str] = set()
        # `mod x;` declarations only: children whose code is in another file.
        self.file_children: set[str] = set()
        # Names bound by `use`, each to the path(s) written, relative to here,
        # and whether the `use` was `pub` (in any form) and so visible outside.
        self.bindings: dict[str, list[tuple[list[str], bool]]] = defaultdict(list)
        self.globs: list[tuple[list[str], bool]] = []
        # Items defined at this module's own scope: a `fn`, a type, a constant.
        self.defined: set[str] = set()


class Reference:
    """A path written in `file`, inside `module`."""

    def __init__(self, file: str, module: ModPath, segments: list[str], line: int, is_use: bool):
        self.file = file
        self.module = module
        self.segments = segments
        self.line = line
        self.is_use = is_use


def module_of_file(rel: str) -> ModPath:
    """`main.rs`/`lib.rs` -> (), `a/mod.rs` and `a.rs` -> (a,), `a/b.rs` -> (a, b)."""
    parts = rel[:-3].split("/")
    if parts[-1] == "mod" or (len(parts) == 1 and parts[0] in ("main", "lib")):
        parts = parts[:-1]
    return tuple(parts)


def _name(token: str) -> str:
    return token[2:] if token.startswith("r#") else token


class ParseError(ValueError):
    pass


def _use_tree(toks: list[str], i: int, prefix: list[str], leaves: list) -> int:
    """Parse one use tree at `toks[i]`; append (path, alias, glob) leaves."""
    segs = list(prefix)
    if not segs and toks[i] == "::":
        segs = ["::"]
        i += 1
    while True:
        tok = toks[i]
        if tok == "{":
            i += 1
            while toks[i] != "}":
                if toks[i] == ",":
                    i += 1
                    continue
                i = _use_tree(toks, i, segs, leaves)
            return i + 1
        if tok == "*":
            leaves.append((segs, None, True))
            return i + 1
        if IDENT.match(tok):
            segs = segs + [_name(tok)]
            i += 1
            if toks[i] == "::":
                i += 1
                continue
            alias = None
            if toks[i] == "as":
                alias = _name(toks[i + 1])
                i += 2
            leaves.append((segs, alias, False))
            return i
        raise ParseError(f"unexpected {tok!r} in a use tree")


def parse_file(rel: str, source: str, modules: dict[ModPath, Module], refs: list[Reference]) -> None:
    """Add this file's modules, declarations, bindings and references."""
    masked = rustlex.mask(source)
    pairs = rustlex.tokens(masked)
    toks = [t for t, _ in pairs]
    offsets = [o for _, o in pairs]
    n = len(toks)

    def line_of(index: int) -> int:
        return masked.count("\n", 0, offsets[index]) + 1

    def module(path: ModPath) -> Module:
        if path not in modules:
            modules[path] = Module(path, rel)
        return modules[path]

    file_module = module_of_file(rel)
    module(file_module).file = rel
    # (module, the brace depth at which its scope closes); a module's own
    # items sit one deeper than that.
    stack: list[tuple[ModPath, int]] = [(file_module, -1)]
    depth = 0
    i = 0
    while i < n:
        tok = toks[i]
        prev = toks[i - 1] if i else ""
        here = stack[-1][0]

        if tok == "{":
            depth += 1
        elif tok == "}":
            depth -= 1
            if len(stack) > 1 and depth == stack[-1][1]:
                stack.pop()
        elif tok == "mod" and prev not in ("::", ".") and i + 2 < n and IDENT.match(toks[i + 1]):
            name = _name(toks[i + 1])
            after = toks[i + 2]
            if after in (";", "{"):
                module(here).children.add(name)
                if after == ";":
                    module(here).file_children.add(name)
                else:
                    module(here + (name,))
                    stack.append((here + (name,), depth))
                    depth += 1
                i += 3
                continue
        elif (
            tok in DEFINES
            and i + 1 < n
            and IDENT.match(toks[i + 1])
            and depth == stack[-1][1] + 1
        ):
            # Only at the module's own scope: a method in an `impl` is not a
            # member of the module, and a `fn` inside a function is nobody's.
            module(here).defined.add(_name(toks[i + 1]))
        elif tok == "macro_rules" and i + 2 < n and toks[i + 1] == "!":
            module(here).defined.add(_name(toks[i + 2]))
        elif tok == "use" and prev not in ("::", ".", "$"):
            public = prev == "pub" or (prev == ")" and _opens_pub(toks, i - 1))
            leaves: list = []
            try:
                end = _use_tree(toks, i + 1, [], leaves)
            except (ParseError, IndexError) as error:
                raise ParseError(f"{rel}:{line_of(i)}: cannot parse use: {error}") from None
            for segs, alias, glob in leaves:
                if segs and segs[-1] == "self":
                    segs = segs[:-1]
                if not segs:
                    continue
                if glob:
                    module(here).globs.append((segs, public))
                else:
                    bound = alias if alias is not None else segs[-1]
                    if bound != "_":
                        module(here).bindings[bound].append((segs, public))
                refs.append(Reference(rel, here, segs, line_of(i), True))
            i = end
            continue
        elif (
            IDENT.match(tok)
            and prev not in ("::", ".")
            and i + 1 < n
            and toks[i + 1] == "::"
            and (prev != "$" or tok == "crate")
        ):
            segs = [_name(tok)]
            j = i + 1
            while j + 1 < n and toks[j] == "::" and IDENT.match(toks[j + 1]):
                segs.append(_name(toks[j + 1]))
                j += 2
            refs.append(Reference(rel, here, segs, line_of(i), False))
            i = j
            continue
        i += 1


class Resolver:
    """Resolve a path, from the module it is written in, to a module.

    Two of Rust's rules matter enough to model. Names live in two namespaces,
    so `process::kill(..)` is the function `kill` defined in `process` even
    though `process` also imports a *module* called `kill`: the last segment of
    an expression path prefers an item the module defines, and a segment with
    more after it prefers a module. And a plain `use` is private, so a path
    from outside a module follows only its `pub use` re-exports.
    """

    def __init__(self, modules: dict[ModPath, Module]):
        self.modules = modules

    def resolve(self, module: ModPath, segs: list[str], is_use: bool = False,
                guard: frozenset = frozenset()):
        """(landing module, whether every segment named a module), or None
        when the path does not start in this crate."""
        first, rest = segs[0], list(segs[1:])
        if first == "crate":
            start: ModPath = ()
        elif first == "self":
            start = module
        elif first == "super":
            start = module[:-1]
            while rest and rest[0] == "super":
                start, rest = start[:-1], rest[1:]
        elif first in ("::", "Self"):
            return None
        else:
            # Looked up in the module's own scope, where every `use` is
            # visible. A path has more after its first segment unless it is
            # a one-segment `use`, which names what it imports.
            found = self.member(module, first, module, not rest and not is_use, guard)
            if found is None:
                return None
            start, whole = found
            if not whole:
                return start, False
        return self.walk(module, start, rest, is_use, guard)

    def walk(self, origin: ModPath, at: ModPath, rest: list[str], is_use: bool,
             guard: frozenset):
        for index, seg in enumerate(rest):
            if seg == "self":
                continue
            last = index == len(rest) - 1
            found = self.member(at, seg, origin, last and not is_use, guard)
            if found is None:
                return at, False
            at, whole = found
            if not whole:
                return at, False
        return at, True

    def member(self, at: ModPath, name: str, origin: ModPath, value: bool,
               guard: frozenset):
        """`name` as a member of module `at`, seen from module `origin`.

        `value` is true for the last segment of an expression path, which is
        a function, constant or type before it is a module.
        """
        mod = self.modules.get(at)
        if mod is None:
            return None
        if value and name in mod.defined:
            return at, False
        if name in mod.children:
            return at + (name,), True
        # Private imports are visible in the module and below it.
        inside = origin[: len(at)] == at
        key = (at, name)
        if key not in guard:
            for path, public in mod.bindings.get(name, ()):
                if public or inside:
                    found = self.resolve(at, path, True, guard | {key})
                    if found is not None:
                        return found
        if name in mod.defined:
            return at, False
        glob_key = (at, "*")
        if glob_key not in guard:
            for path, public in mod.globs:
                if not (public or inside):
                    continue
                target = self.resolve(at, path, True, guard | {glob_key})
                if target is not None and target[1]:
                    found = self.member(target[0], name, at, value, guard | {glob_key})
                    if found is not None:
                        return found
        return None

    def file_of(self, path: ModPath) -> str | None:
        while True:
            mod = self.modules.get(path)
            if mod is not None:
                return mod.file
            if not path:
                return None
            path = path[:-1]


def _opens_pub(toks: list[str], close: int) -> bool:
    """Whether the `)` at `close` ends a `pub(...)` visibility."""
    depth = 0
    for index in range(close, -1, -1):
        if toks[index] == ")":
            depth += 1
        elif toks[index] == "(":
            depth -= 1
            if depth == 0:
                return index > 0 and toks[index - 1] == "pub"
    return False


def key_of(path: ModPath) -> str:
    return "::".join(path) if path else "crate"


def build(files: list[str], read) -> tuple[dict[ModPath, Module], list[Reference], list[str]]:
    """Parse every file; `read(rel)` gives its source. Returns problems too."""
    modules: dict[ModPath, Module] = {}
    refs: list[Reference] = []
    problems: list[str] = []
    for rel in files:
        source = read(rel)
        if UNREADABLE.search(rustlex.mask(source)):
            problems.append(f"{rel}: #[path] or include! -- its module path or code is not where this gate looks")
        try:
            parse_file(rel, source, modules, refs)
        except (ParseError, rustlex.LexError) as error:
            problems.append(f"{rel}: {error}")
    known = set(files)
    for mod in modules.values():
        for child in mod.file_children:
            base = "/".join(mod.path + (child,))
            if not ({f"{base}.rs", f"{base}/mod.rs"} & known):
                problems.append(f"{mod.file}: `mod {child};` has no file this gate can find")
    return modules, refs, problems


class Edge:
    """A reference that breaks a dependency rule, at its first site."""

    def __init__(self, file: str, from_ring: str, references: str, to_ring: str, line: int):
        self.file = file
        self.from_ring = from_ring
        self.references = references
        self.to_ring = to_ring
        self.line = line


def find_edges(manifest: dict, ring_of: dict[str, str], modules, refs) -> list[Edge]:
    """Every reference that breaks a dependency rule, first site per target."""
    forbidden = {
        rule["from"]: set(rule["may_not_reference"])
        for rule in manifest["dependency_rules"]
    }
    resolver = Resolver(modules)
    # A path that starts at a classified library crate lands in its ring.
    crates = {lib_name(name): entry["ring"] for name, entry in crate_entries(manifest).items()}
    edges: dict[tuple[str, str], Edge] = {}
    for ref in refs:
        ring = ring_of.get(ref.file)
        if ring not in forbidden or is_test_file(ref.file, manifest):
            # In-kernel tests drive the thing they test from inside the kernel;
            # a core test that reaches the filesystem to build a fixture is not
            # the core depending on the filesystem. Counted in VERIFICATION.md
            # instead, where the question is what the test reaches.
            continue
        found = resolver.resolve(ref.module, ref.segments, ref.is_use)
        if found is None:
            first = ref.segments[1] if ref.segments[0] == "::" and len(ref.segments) > 1 else ref.segments[0]
            if crates.get(first) in forbidden[ring]:
                key = (ref.file, f"crate {first}")
                if key not in edges:
                    edges[key] = Edge(ref.file, ring, key[1], crates[first], ref.line)
            continue
        target_file = resolver.file_of(found[0])
        target_ring = ring_of.get(target_file) if target_file else None
        if target_ring in forbidden[ring]:
            key = (ref.file, key_of(found[0]))
            if key not in edges:
                edges[key] = Edge(ref.file, ring, key[1], target_ring, ref.line)
    return sorted(edges.values(), key=lambda e: (e.file, e.references))


def containment(ring_of: dict[str, str], modules) -> list[tuple[str, str, str]]:
    """`mod x;` declarations of a higher-ring file from a lower-ring one."""
    order = {"core": 0, "item": 1, "load": 2}
    resolver = Resolver(modules)
    out = []
    for mod in modules.values():
        ring = ring_of.get(mod.file)
        for child in sorted(mod.file_children):
            child_file = resolver.file_of(mod.path + (child,))
            child_ring = ring_of.get(child_file) if child_file else None
            if ring and child_ring and order[child_ring] > order[ring]:
                out.append((mod.file, key_of(mod.path + (child,)), child_ring))
    return sorted(out)


def measure(manifest: dict, ring_of: dict[str, str]):
    files = sorted(ring_of)
    modules, refs, problems = build(
        files, lambda rel: (KERNEL_SRC / rel).read_text(encoding="utf-8", errors="replace")
    )
    return modules, find_edges(manifest, ring_of, modules, refs), problems


def line_counts(manifest: dict, ring_of: dict[str, str]) -> dict[str, dict[str, int]]:
    counts: dict[str, dict[str, int]] = defaultdict(lambda: {"product": 0, "test": 0})
    for rel, ring in ring_of.items():
        lines = sum(
            1 for _ in (KERNEL_SRC / rel).open(encoding="utf-8", errors="replace")
        )
        kind = "test" if is_test_file(rel, manifest) else "product"
        counts[ring][kind] += lines
    return counts


def split(manifest: dict, edges: list[Edge]) -> tuple[list[Edge], list[Edge]]:
    """(debt, composition root's edges)."""
    root = manifest.get("composition_root", {}).get("file")
    return (
        [e for e in edges if e.file != root],
        [e for e in edges if e.file == root],
    )


def report(manifest: dict, ring_of: dict[str, str], modules, edges: list[Edge]) -> None:
    counts = line_counts(manifest, ring_of)
    print("item-boundary: sizes, in lines")
    total_product = total_test = 0
    for ring in ("core", "item", "load"):
        product = counts[ring]["product"]
        test = counts[ring]["test"]
        total_product += product
        total_test += test
        print(f"  {ring:<6} {product:>7} product  {test:>7} test")

    # The item is the core plus its own ring: nested, not disjoint.
    certified = counts["core"]["product"] + counts["item"]["product"]
    print(f"  {'':<6} {'-' * 7}")
    print(f"  certified item (core+item): {certified} lines of product code")
    print(f"  uncertified load:           {counts['load']['product']} lines")
    print(f"  in-kernel verification:     {total_test} lines")
    share = 100.0 * certified / (total_product or 1)
    print(f"  the item is {share:.1f}% of the kernel's product code")

    crates = crate_line_counts(manifest)
    if crates:
        print("\nitem-boundary: library crates, in lines (tests are host tests)")
        for package, entry in sorted(crates.items(), key=lambda c: (RING_ORDER[c[1]["ring"]], c[0])):
            print(f"  {entry['ring']:<6} {entry['product']:>7} product  {entry['test']:>7} test  {package}")
        in_item = sum(e["product"] for e in crates.values() if e["ring"] in ("core", "item"))
        print(f"  certified item with its crates: {certified + in_item} lines of product code")

    debt, root = split(manifest, edges)
    for title, group in (("upward reference(s)", debt), ("composition-root edge(s)", root)):
        if not group:
            continue
        by_file: dict[str, list[Edge]] = defaultdict(list)
        for edge in group:
            by_file[edge.file].append(edge)
        print(f"\nitem-boundary: {len(group)} {title} in {len(by_file)} file(s)")
        for rel in sorted(by_file):
            ring = by_file[rel][0].from_ring
            print(f"  {ring:<5} {rel}")
            for edge in by_file[rel]:
                print(f"          -> {edge.references} ({edge.to_ring}), first at line {edge.line}")

    held = containment(ring_of, modules)
    if held:
        print(f"\nitem-boundary: {len(held)} `mod` declaration(s) of a higher ring "
              f"(containment, not counted as edges)")
        by_file = defaultdict(list)
        for rel, child, ring in held:
            by_file[rel].append(child)
        for rel in sorted(by_file):
            print(f"  {rel}: {len(by_file[rel])}")


# --- library crates ----------------------------------------------------------
#
# The rings above are kernel files. A library crate is classified whole, under
# `crates` in the manifest, because a crate is what Cargo links and what a
# dependency names. Three things are asked of one:
#
#   * which of its files are product code: the ones its module tree reaches
#     from `src/lib.rs` other than through a `#[cfg(test)] mod`, which are
#     host-test code. A file under `src/` the tree does not reach fails, as an
#     unclassified kernel file does: nobody has said what it is;
#   * that an item or core crate depends only on its own ring and the rings
#     below, or on an allowlisted infrastructure crate, whose own dependencies
#     are held to the same rule once an item crate depends on it, and not
#     before ([`crate_dependency_problems`]);
#   * that a kernel file naming a crate is held to `dependency_rules` like a
#     kernel file naming a module ([`find_edges`]).

RING_ORDER = {"core": 0, "item": 1, "load": 2}
# `#[cfg(test)]` exactly. `cfg(any(test, ..))` compiles into some product
# build, so a module under it is product code and is read as such.
CFG_TEST = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
DEPENDENCY_TABLES = ("dependencies", "build-dependencies")


def crate_entries(manifest: dict) -> dict[str, dict]:
    """The manifest's classified crates, by package name."""
    return manifest.get("crates", {}).get("members", {})


def crate_allowlist(manifest: dict) -> dict[str, str]:
    """Allowlisted infrastructure crates, by package name, with the reason."""
    allow = manifest.get("crates", {}).get("infrastructure_allowlist", {})
    return {name: why for name, why in allow.items() if not name.startswith("//")}


def _a(ring: str) -> str:
    return f"an {ring}" if ring[:1] in "aeiou" else f"a {ring}"


def lib_name(package: str) -> str:
    return package.replace("-", "_")


def crate_module_files(src_dir: Path, fake: dict[str, str] | None = None):
    """`{path under src_dir: "product" | "test"}` for every file the module tree
    reaches from `lib.rs`, and the problems met on the way.

    A `mod x;` is followed to `x.rs` or `x/mod.rs` beside a `lib.rs` or a
    `mod.rs`, and under `<stem>/` beside any other file; an inline `mod a { }`
    puts its declarations one directory deeper. A declaration with
    `#[cfg(test)]` among the attributes in front of it makes its file, and
    everything below that file, test code."""
    if fake is not None:
        read, exists, listing = fake.__getitem__, fake.__contains__, sorted(fake)
    else:
        def read(rel: str) -> str:
            return (src_dir / rel).read_text(encoding="utf-8", errors="replace")

        def exists(rel: str) -> bool:
            return (src_dir / rel).is_file()

        listing = sorted(p.relative_to(src_dir).as_posix() for p in src_dir.rglob("*.rs")) \
            if src_dir.is_dir() else []
    kinds: dict[str, str] = {}
    problems: list[str] = []
    queue = [("lib.rs", "product")]
    while queue:
        rel, kind = queue.pop()
        if rel in kinds:
            continue
        kinds[rel] = kind
        source = read(rel)
        masked = rustlex.mask(source)
        if UNREADABLE.search(masked):
            problems.append(f"{rel}: #[path] or include! -- its module tree is not where this gate looks")
        stem = rel[:-3]
        parts = stem.split("/")
        base = "/".join(parts[:-1]) if parts[-1] in ("lib", "mod") else stem
        pairs = rustlex.tokens(masked)
        toks = [t for t, _ in pairs]
        stack: list[tuple[str, int]] = []  # (inline module name, depth it closes at)
        depth = 0
        for i, tok in enumerate(toks):
            if tok == "{":
                depth += 1
            elif tok == "}":
                depth -= 1
                if stack and depth == stack[-1][1]:
                    stack.pop()
            elif tok == "mod" and (i == 0 or toks[i - 1] not in ("::", ".")) and i + 2 < len(toks) \
                    and IDENT.match(toks[i + 1]) and toks[i + 2] in (";", "{"):
                name = _name(toks[i + 1])
                if toks[i + 2] == "{":
                    stack.append((name, depth))
                    continue
                # The attributes in front of the declaration: back over `pub`,
                # `pub(..)` and `#[..]` groups to the previous item's end.
                start = pairs[i][1]
                head = masked[: start]
                cut = max(head.rfind(";"), head.rfind("}"), head.rfind("{"))
                attrs = head[cut + 1 :]
                test = kind == "test" or bool(CFG_TEST.search(attrs))
                directory = "/".join([p for p in [base] + [s for s, _ in stack] if p])
                prefix = f"{directory}/{name}" if directory else name
                found = [c for c in (f"{prefix}.rs", f"{prefix}/mod.rs") if exists(c)]
                if not found:
                    problems.append(f"{rel}: `mod {name};` has no file this gate can find")
                    continue
                queue.append((found[0], "test" if test else "product"))
    for rel in listing:
        if rel not in kinds:
            problems.append(f"{rel}: under src/ but not in the crate's module tree, so neither product nor test")
    return kinds, problems


def crate_files(manifest: dict, rings: tuple[str, ...] = ("core", "item", "load")):
    """`[(package, ring, path relative to the repository, kind)]` for every file
    of every classified crate in `rings`, and the problems met."""
    out = []
    problems = []
    for package, entry in sorted(crate_entries(manifest).items()):
        if entry["ring"] not in rings:
            continue
        src_dir = ROOT / entry["path"] / "src"
        kinds, found = crate_module_files(src_dir)
        problems += [f"{entry['path']}/src/{p}" for p in found]
        for rel, kind in sorted(kinds.items()):
            out.append((package, entry["ring"], f"{entry['path']}/src/{rel}", kind))
    return out, problems


def item_crate_product_files(manifest: dict) -> list[tuple[str, str]]:
    """`[(package, path relative to the repository)]`: the product code of the
    `core` and `item` crates, which the item-scoped gates read beside the
    kernel's item files. Fails loudly rather than reading less."""
    files, problems = crate_files(manifest, ("core", "item"))
    if problems:
        raise SystemExit("item-boundary: cannot list the item crates' files:\n  " + "\n  ".join(problems))
    return [(package, path) for package, _, path, kind in files if kind == "product"]


def _dependency_names(cargo: dict, workspace: dict) -> list[str]:
    """Package names in a manifest's shipping dependency tables, target-specific
    ones included; a `package = ` rename is followed, here or in the workspace."""
    tables = [cargo.get(name, {}) for name in DEPENDENCY_TABLES]
    for target in cargo.get("target", {}).values():
        tables += [target.get(name, {}) for name in DEPENDENCY_TABLES]
    names = []
    for table in tables:
        for key, spec in table.items():
            package = key
            if isinstance(spec, dict):
                if spec.get("workspace"):
                    root_spec = workspace.get(key, {})
                    if isinstance(root_spec, dict):
                        package = root_spec.get("package", key)
                package = spec.get("package", package)
            names.append(package)
    return sorted(set(names))


def crate_dependency_problems(manifest: dict, cargo_of, workspace: dict) -> tuple[list[str], dict]:
    """Break the crate-level rule: an item or core crate depending on a crate
    of a higher ring, an unclassified one, or one reaching such a crate through
    the allowlist. `cargo_of(package)` gives a parsed Cargo.toml or None;
    `workspace` is the root's `[workspace.dependencies]`. Returns the problems
    and, per allowlisted crate, the item crates that use it."""
    entries = crate_entries(manifest)
    allow = crate_allowlist(manifest)
    problems = []
    used: dict[str, set[str]] = {name: set() for name in allow}
    for package, entry in sorted(entries.items()):
        ring = entry["ring"]
        if ring not in RING_ORDER:
            problems.append(f"{package}: ring {ring!r} is none of {', '.join(RING_ORDER)}")
            continue
        if ring == "load":
            continue
        # The crate itself, then every allowlisted crate it reaches.
        pending = [(package, package)]
        seen = set()
        while pending:
            holder, via = pending.pop()
            if holder in seen:
                continue
            seen.add(holder)
            cargo = cargo_of(holder)
            if cargo is None:
                problems.append(f"{package}: cannot read the manifest of {holder}")
                continue
            for dep in _dependency_names(cargo, workspace):
                chain = f"{package} -> {dep}" if holder == package else f"{package} -> {via} -> {dep}"
                target = entries.get(dep)
                if target is not None:
                    if RING_ORDER.get(target["ring"], 9) > RING_ORDER[ring]:
                        problems.append(f"{chain}: {_a(ring)} crate depends on {_a(target['ring'])} crate")
                elif dep in allow:
                    used[dep].add(package)
                    pending.append((dep, dep if holder == package else via))
                else:
                    problems.append(
                        f"{chain}: {_a(ring)} crate depends on a crate in no ring and not "
                        f"in crates.infrastructure_allowlist"
                    )
    return problems, used


def read_cargo(package: str, workspace: dict, entries: dict):
    """A package's parsed Cargo.toml, found through the manifest or the
    workspace's dependency table; None for a crate outside the tree."""
    import tomllib

    path = entries.get(package, {}).get("path")
    if path is None:
        spec = workspace.get(package)
        path = spec.get("path") if isinstance(spec, dict) else None
    if path is None or not (ROOT / path / "Cargo.toml").is_file():
        return None
    with (ROOT / path / "Cargo.toml").open("rb") as handle:
        return tomllib.load(handle)


def check_crates(manifest: dict, ring_of: dict[str, str]) -> tuple[list[str], list[str]]:
    """(problems, notes) of the crate-level rules over the real tree."""
    import tomllib

    with (ROOT / "Cargo.toml").open("rb") as handle:
        workspace = tomllib.load(handle).get("workspace", {}).get("dependencies", {})
    entries = crate_entries(manifest)
    problems = []
    for package, entry in sorted(entries.items()):
        cargo = read_cargo(package, workspace, entries)
        name = (cargo or {}).get("package", {}).get("name")
        if name != package:
            problems.append(f"{entry['path']}: Cargo.toml names {name!r}, the manifest {package!r}")
    _, found = crate_files(manifest)
    problems += found
    deps, used = crate_dependency_problems(
        manifest, lambda package: read_cargo(package, workspace, entries), workspace
    )
    problems += deps
    # An allowlist entry's reason is that the core already relies on it, so
    # some core or item kernel file has to name it still.
    trusted = [rel for rel, ring in ring_of.items() if ring in ("core", "item")]
    notes = []
    for package, why in sorted(crate_allowlist(manifest).items()):
        if not why.strip():
            problems.append(f"crates.infrastructure_allowlist: {package} has no reason")
        named = re.compile(rf"(?<![\w:]){lib_name(package)}\s*::")
        if not any(
            named.search(rustlex.mask((KERNEL_SRC / rel).read_text(encoding="utf-8", errors="replace")))
            for rel in trusted
        ):
            problems.append(
                f"crates.infrastructure_allowlist: no core or item kernel file names {package} any "
                f"more, so the reason it is allowed has gone; remove it or classify it"
            )
        users = ", ".join(sorted(used.get(package, ()))) or "no item crate yet"
        notes.append(f"{package} (allowlisted; used by {users})")
    return problems, notes


def crate_line_counts(manifest: dict) -> dict[str, dict[str, int]]:
    """`{package: {"ring", "product", "test"}}` in lines, as for kernel files."""
    files, _ = crate_files(manifest)
    counts: dict[str, dict] = {}
    for package, ring, path, kind in files:
        entry = counts.setdefault(package, {"ring": ring, "product": 0, "test": 0})
        entry[kind] += sum(1 for _ in (ROOT / path).open(encoding="utf-8", errors="replace"))
    return counts


# --- self-test ---------------------------------------------------------------

_CRATE = {
    "main.rs": "mod a; mod exec; mod sys; mod shared;\n"
               "fn main() { exec::run(); }\n",
    "a.rs": "use crate::sys::{inner::{deep, deeper as d2}, other};\n"
            "fn f() { deep::x(); }\n",
    "exec.rs": "pub fn run() {}\n",
    "shared.rs": "pub fn helper() {}\n",
    "sys/mod.rs": "pub mod inner; pub mod other; pub mod load; mod caller;\n"
                  "pub use load::Thing;\n"
                  "use self::other as o;\n"
                  "fn g() { load::go(); \"crate::exec::fake\"; }\n"
                  "mod inline { fn h() { super::super::shared::helper(); } }\n",
    "sys/inner/mod.rs": "pub mod deep; pub mod deeper;\n",
    "sys/inner/deep.rs": "pub fn x() {}\n",
    "sys/inner/deeper.rs": "fn y() { super::super::load::go(); }\n",
    "sys/other.rs": "// crate::exec in a comment\nfn z() { let c = '\"'; crate::sys::Thing::new(); }\n",
    # `shared` is both a module it imports and a function it defines; a call
    # from outside is the function, so the edge is to sys::load.
    "sys/load.rs": "use crate::shared;\npub struct Thing;\npub fn shared() {}\n"
                   "impl Thing { fn exec() {} }\n",
    "sys/caller.rs": "fn c() { super::load::shared(); crate::sys::inline::gone(); }\n",
}

# file -> the set of module keys it must resolve an edge to
_EXPECT = {
    "main.rs": {"exec"},
    "a.rs": {"sys::inner::deep", "sys::inner::deeper", "sys::other"},
    "sys/mod.rs": {"sys::load", "sys::other", "shared"},
    "sys/inner/deeper.rs": {"sys::load"},
    # through the `pub use` in sys/mod.rs, to where Thing is defined
    "sys/other.rs": {"sys::load"},
    "sys/load.rs": {"shared"},
    "sys/caller.rs": {"sys::load", "sys::inline"},
}


def self_test() -> list[str]:
    failures = [f"lexer: {f}" for f in rustlex.self_test()]
    files = sorted(_CRATE)
    modules, refs, problems = build(files, lambda rel: _CRATE[rel])
    failures += [f"build: {p}" for p in problems]
    resolver = Resolver(modules)
    seen: dict[str, set[str]] = defaultdict(set)
    for ref in refs:
        found = resolver.resolve(ref.module, ref.segments, ref.is_use)
        if found is not None and resolver.file_of(found[0]) != ref.file:
            seen[ref.file].add(key_of(found[0]))
    for rel, want in _EXPECT.items():
        if seen[rel] != want:
            failures.append(f"resolver: {rel} names {sorted(seen[rel])}, expected {sorted(want)}")
    if modules.get(("sys", "inline")) is None or modules[("sys", "inline")].file != "sys/mod.rs":
        failures.append("resolver: inline module sys::inline not placed in sys/mod.rs")
    failures += _crate_self_test()
    rings = {"rings": {"load": {"members": ["fs/**", "render/**", "net.rs"]}}}
    stale = stale_members(rings, ["fs/pipe.rs", "interfaces/render/mod.rs", "net.rs"])
    if stale != [("load", "render/**")]:
        failures.append(f"stale members: got {stale}, expected [('load', 'render/**')]")
    return failures


# A crate's module tree: product files, a test module in its own file and in a
# directory below it, an inline module declaring a file, a `cfg(any(test,..))`
# module that is product code, and a file nothing declares.
_CRATE_TREE = {
    "lib.rs": "pub mod a;\n#[cfg(test)]\nmod tests;\nmod inline { pub mod deep; }\n"
              "#[cfg(any(test, feature = \"x\"))]\npub(crate) mod maybe;\n",
    "a.rs": "/// docs\n#[cfg(test)]\n#[allow(dead_code)]\npub(crate) mod tests;\nfn f() {}\n",
    "a/tests.rs": "mod more;\n",
    "a/tests/more.rs": "",
    "tests.rs": "",
    "inline/deep.rs": "",
    "maybe.rs": "",
    "orphan.rs": "",
}
_CRATE_KINDS = {
    "lib.rs": "product", "a.rs": "product", "a/tests.rs": "test", "a/tests/more.rs": "test",
    "tests.rs": "test", "inline/deep.rs": "product", "maybe.rs": "product",
}

# The crate-level dependency rule. Each case is the dependencies of a pretend
# `ferrix-btrfs-write`, and whether the rule must refuse it.
_DEP_MANIFEST = {
    "crates": {
        "members": {
            "ferrix-btrfs": {"ring": "item", "path": "-"},
            "ferrix-btrfs-write": {"ring": "item", "path": "-"},
            "ferrix-btrfs-vfs": {"ring": "load", "path": "-"},
            "ferrix-low": {"ring": "core", "path": "-"},
        },
        "infrastructure_allowlist": {"ferrix-fallible": "why", "ferrix-sync": "why"},
    }
}
_DEP_CASES = [
    ("the reader, item", {"dependencies": {"ferrix-btrfs": {"workspace": True}}}, False),
    ("a core crate", {"dependencies": {"ferrix-low": "0.1"}}, False),
    ("an allowlisted crate", {"dependencies": {"ferrix-fallible": {"workspace": True}}}, False),
    ("a load crate", {"dependencies": {"ferrix-btrfs-vfs": {"workspace": True}}}, True),
    ("a crate in no ring", {"dependencies": {"ferrix-vfs": {"workspace": True}}}, True),
    ("a load crate under a target", {"target": {"cfg(x)": {"dependencies": {"ferrix-btrfs-vfs": "1"}}}}, True),
    ("a load crate to build with", {"build-dependencies": {"ferrix-btrfs-vfs": "1"}}, True),
    ("a load crate renamed", {"dependencies": {"vfs": {"package": "ferrix-btrfs-vfs"}}}, True),
    ("a load crate to test with", {"dev-dependencies": {"ferrix-btrfs-vfs": "1"}}, False),
    ("the load through the allowlist", {"dependencies": {"ferrix-sync": "1"}}, True),
]


def _crate_self_test() -> list[str]:
    failures = []
    kinds, problems = crate_module_files(Path("-"), _CRATE_TREE)
    if kinds != _CRATE_KINDS:
        failures.append(f"crate tree: got {kinds}, expected {_CRATE_KINDS}")
    if problems != ["orphan.rs: under src/ but not in the crate's module tree, so neither product nor test"]:
        failures.append(f"crate tree: problems {problems}")
    for what, cargo, refused in _DEP_CASES:
        manifests = {
            "ferrix-btrfs": {}, "ferrix-btrfs-vfs": {}, "ferrix-low": {}, "ferrix-fallible": {},
            "ferrix-btrfs-write": cargo,
            # Allowlisted, and reaching the load: the allowlist is no door.
            "ferrix-sync": {"dependencies": {"ferrix-btrfs-vfs": "1"}},
        }
        problems, _ = crate_dependency_problems(_DEP_MANIFEST, manifests.get, {})
        mine = [p for p in problems if p.startswith("ferrix-btrfs-write ")]
        if bool(mine) != refused:
            failures.append(f"crate rule: {what}: {'passed' if not mine else mine}")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--report",
        action="store_true",
        help="print sizes and every edge without failing",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run the lexer's and resolver's cases and nothing else",
    )
    args = parser.parse_args()

    failures = self_test()
    if failures:
        for failure in failures:
            print(f"item-boundary: self-test: {failure}", file=sys.stderr)
        return 1
    if args.self_test:
        print("item-boundary: lexer and resolver self-tests pass")
        return 0

    manifest = load_manifest()
    files = kernel_files()
    ring_of, unclassified, ambiguous = classify(manifest, files)

    status = 0

    if unclassified:
        print(
            f"item-boundary: {len(unclassified)} kernel file(s) in no ring.\n"
            f"  Add each to a ring in {MANIFEST.relative_to(ROOT)}, which is a\n"
            f"  decision about whether it is trusted, not a formality:",
            file=sys.stderr,
        )
        for rel in unclassified:
            print(f"    {rel}", file=sys.stderr)
        status = 1

    if ambiguous:
        print(
            f"item-boundary: {len(ambiguous)} file(s) match more than one ring:",
            file=sys.stderr,
        )
        for rel, hits in ambiguous:
            print(f"    {rel}: {', '.join(hits)}", file=sys.stderr)
        status = 1

    stale = stale_members(manifest, files)
    if stale:
        print(
            f"item-boundary: {len(stale)} ring member(s) match no kernel file.\n"
            f"  Remove each from {MANIFEST.relative_to(ROOT)}, or name the file it\n"
            f"  meant: a pattern nothing matches would classify the next file\n"
            f"  made under its name without anybody deciding it:",
            file=sys.stderr,
        )
        for ring, pattern in stale:
            print(f"    {ring}: {pattern}", file=sys.stderr)
        status = 1

    modules, edges, problems = measure(manifest, ring_of)
    crate_problems, crate_notes = check_crates(manifest, ring_of)
    if crate_problems:
        print(
            f"item-boundary: {len(crate_problems)} problem(s) with the classified crates.\n"
            f"  An item crate may depend only on item and core crates and on\n"
            f"  crates.infrastructure_allowlist; every file of a classified crate\n"
            f"  has to be product or test code by its module tree:",
            file=sys.stderr,
        )
        for problem in crate_problems:
            print(f"    {problem}", file=sys.stderr)
        status = 1

    if problems:
        print(
            f"item-boundary: {len(problems)} file(s) this gate cannot read\n"
            f"  completely, so their edges would go unseen. Fix the gate or the\n"
            f"  file; do not let it report an absence it did not establish:",
            file=sys.stderr,
        )
        for problem in problems:
            print(f"    {problem}", file=sys.stderr)
        status = 1

    if args.report:
        report(manifest, ring_of, modules, edges)
        return status

    debt, root_edges = split(manifest, edges)
    status |= ratchet(
        "upward reference",
        "The certified\n"
        "  item may not depend on the uncertified load above it; a core\n"
        "  that reaches into a filesystem has put the filesystem in the\n"
        "  core, whatever docs/certification/ITEM.md says:",
        {(e.file, e.references): e for e in debt},
        {(x["file"], x["references"]) for x in manifest["known_violations"]["entries"]},
        "known_violations",
        ring_of,
    )
    root = manifest.get("composition_root", {})
    status |= ratchet(
        "composition-root edge",
        "The crate root\n"
        "  may compose the load with the item; it may not grow item logic\n"
        "  that depends on the load. Record the module under\n"
        "  composition_root only if the call is registration, bring-up or a\n"
        "  boot check, and say so in the commit:",
        {(e.file, e.references): e for e in root_edges},
        {(root.get("file"), target) for target in root.get("load_modules", [])},
        "composition_root.load_modules",
        ring_of,
    )

    counts = line_counts(manifest, ring_of)
    crates = crate_line_counts(manifest)
    item_crates = {p: e for p, e in crates.items() if e["ring"] in ("core", "item")}
    in_crates = sum(e["product"] for e in item_crates.values())
    certified = counts["core"]["product"] + counts["item"]["product"] + in_crates
    print(
        f"item-boundary: {certified} lines of product code in the item "
        f"({counts['core']['product']} of it core, {in_crates} in "
        f"{len(item_crates)} crate(s): {', '.join(sorted(item_crates))}), "
        f"{counts['load']['product']} uncertified in the kernel, "
        f"{len(debt)} known upward reference(s), "
        f"{len(root_edges)} composition-root edge(s)"
    )
    for note in crate_notes:
        print(f"  {note}")
    return status


def ratchet(what: str, why: str, found: dict, known: set, where: str,
            ring_of: dict[str, str]) -> int:
    """Fail on an edge not recorded in `known`, and on a recorded one gone."""
    new = sorted(set(found) - known)
    stale = sorted(known - set(found))
    status = 0
    if new:
        print(f"item-boundary: {len(new)} new {what}(s). {why}", file=sys.stderr)
        for rel, target in new:
            edge = found[(rel, target)]
            print(f"    {ring_of[rel]:<5} {rel}:{edge.line} -> {target} ({edge.to_ring})",
                  file=sys.stderr)
        status = 1
    if stale:
        print(
            f"item-boundary: {len(stale)} recorded {what}(s) no longer occur.\n"
            f"  Remove them from {where} so a fixed breach cannot\n"
            f"  leave a permanent exemption behind:",
            file=sys.stderr,
        )
        for rel, target in stale:
            print(f"    {rel} -> {target}", file=sys.stderr)
        status = 1
    return status


if __name__ == "__main__":
    sys.exit(main())
