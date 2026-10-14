#!/usr/bin/env python3
"""Hold the boundary of the certified item.

`tools/common/data/certification-item.json` says which kernel files and library crates are inside the thing
four assurance ratings attach to. That file is the scope of every artifact in
`docs/certification`: the Security Target's TOE, the hazard analysis's safety
item, the traceability matrix, the coverage obligation. A boundary that lives
only in a document is a boundary that has already moved.

So this gate asserts five things.

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

  5. **The profile rule** (docs/BUILD-FLAGS.md rule 3, since M0). The flag
     file, tools/common/data/build-flags.toml, defines each level as its full
     set of targets, and "covers" compares them standard by standard, so
     `present` and `sc2` are incomparable, which is tested. Each value's
     `allows` is closed under that comparison. Each profile in
     tools/common/data/profiles/ sets every flag (rule 4) to a declared value
     its level allows -- a below-level value only by a dated exception, in
     `present` alone (BF1) -- permits only declared boot options, records the
     build settings the tree really has, and links into ring 0 only
     components at its level or above: each ring and crate here names its
     `level`. `max` and `fast` are checked statically and reported not yet
     buildable, their component levels as what must hold once they build.
     reference_configuration is held to present.toml.

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
import tomllib
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


def _kernel_crates_in_manifest() -> dict[str, dict]:
    """The manifest's `kernel_crates`, by package name: the kernel binary and
    the library crates it is split into (docs/KERNEL-CRATES.md). Read once,
    here, because where a crate's root file sits decides module paths for
    every tool that loads this file."""
    try:
        data = json.loads(MANIFEST.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return {
        name: entry
        for name, entry in data.get("kernel_crates", {}).items()
        if not name.startswith("//")
    }


KERNEL_CRATES = _kernel_crates_in_manifest()
# Files that are a crate's root, whose `mod x;` is `x.rs` beside them and
# whose module path is the crate's: `main.rs` and `lib.rs`, and each kernel
# crate's root beside `main.rs`.
ROOT_FILES = {"main.rs", "lib.rs"} | {e["root"] for e in KERNEL_CRATES.values() if "root" in e}
# A path through one of the kernel's own crates, `ferrix_kernel_core::mm`,
# names the module `crate::mm` names: the four crates share one tree of
# modules by position, each re-exporting the ones below it.
KERNEL_CRATE_NAMES = {name.replace("-", "_") for name in KERNEL_CRATES}


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
    if parts[-1] == "mod" or (len(parts) == 1 and rel in ROOT_FILES):
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


def _leading_colons(toks: list[str], at: int) -> bool:
    """Whether the `::` at `toks[at]` begins a path rather than joins two
    segments: nothing a path segment could end with stands before it."""
    if at == 0:
        return True
    before = toks[at - 1]
    return not (IDENT.match(before) or before in (">", ")", "]", "*", "self", "super", "Self", "crate"))


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
            and (prev not in ("::", ".") or (prev == "::" and _leading_colons(toks, i - 1)))
            and i + 1 < n
            and toks[i + 1] == "::"
            and (prev != "$" or tok == "crate")
        ):
            # `::ferrix_kernel_load::fs`: a path from the crates' root, which
            # the resolver reads as through the crate named next.
            segs = ["::", _name(tok)] if prev == "::" else [_name(tok)]
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

    def __init__(self, modules: dict[ModPath, Module], crate_names: set[str] | None = None):
        self.modules = modules
        self.crate_names = KERNEL_CRATE_NAMES if crate_names is None else crate_names

    def resolve(self, module: ModPath, segs: list[str], is_use: bool = False,
                guard: frozenset = frozenset()):
        """(landing module, whether every segment named a module), or None
        when the path does not start in this crate."""
        if segs[0] == "::" and len(segs) > 1 and segs[1] in self.crate_names:
            segs = segs[1:]
        first, rest = segs[0], list(segs[1:])
        if first == "crate" or first in self.crate_names:
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
    # A path through one of the kernel's own crates (`ferrix_kernel_load::fs`)
    # is a path into the kernel's one tree of modules, and lands in the ring
    # of the file it names, as `crate::fs` would.
    resolver = Resolver(modules, KERNEL_CRATE_NAMES | {
        name.replace("-", "_") for name in manifest.get("kernel_crates", {}) if not name.startswith("//")
    })
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


# --- link-time hooks ----------------------------------------------------------
#
# The core may reach code above it without naming it in two ways: a pointer the
# composition root registers at bring-up (`hooks.rs`), or a function the core
# declares in an `unsafe extern "Rust"` block and the composition root defines
# under `#[unsafe(no_mangle)]`, which the linker binds -- a direct call, where
# the registered pointer is an indirect one (`docs/OPAQUE-KERNEL.md` §9.7, "as
# built", 2 and 3). The resolver above sees neither as an edge, since no path
# is written. So this rule holds the second kind to a list that can only be
# changed by editing the manifest: every such declaration in a core or item
# file is named in `composition_root.hooks`, the name is defined once, in the
# composition root, and its body is one call of the function the entry says
# it forwards to -- no logic of the item's lives in the root.

_EXTERN_RUST = re.compile(r'\bextern\s+"\s*"\s*\{')
_FN_NAME = re.compile(r"\bfn\s+([A-Za-z_]\w*)")
_NO_MANGLE = re.compile(
    r"#\s*\[\s*unsafe\s*\(\s*no_mangle\s*\)\s*\]"
    r"(?:\s*#\s*\[[^\]]*\])*\s*(?:pub(?:\s*\([^)]*\))?\s+)?(?:extern\s+\"\s*\"\s+)?"
    r"fn\s+([A-Za-z_]\w*)"
)
_CALL = re.compile(r"^([A-Za-z_]\w*(?:::[A-Za-z_]\w*)*)\s*\((?:[^(){};]|\([^(){};]*\))*\)$")


def _closing(masked: str, open_at: int) -> int:
    """The offset of the `}` that closes the `{` at `open_at`."""
    depth = 0
    for at in range(open_at, len(masked)):
        if masked[at] == "{":
            depth += 1
        elif masked[at] == "}":
            depth -= 1
            if depth == 0:
                return at
    return len(masked)


def hook_problems(ring_of: dict[str, str], read, hooks: list[dict], root: str | None) -> list[str]:
    """What breaks the link-time hook rule, over `ring_of`'s files."""
    listed = {entry["name"]: entry for entry in hooks}
    declared: dict[str, str] = {}
    defined: dict[str, list[tuple[str, str]]] = defaultdict(list)
    problems: list[str] = []
    for rel in sorted(ring_of):
        source = read(rel)
        masked = rustlex.mask(source)
        for block in _EXTERN_RUST.finditer(masked):
            quote = masked.index('"', block.start())
            if source[quote:quote + 6] != '"Rust"':
                continue
            open_at = block.end() - 1
            body = masked[open_at:_closing(masked, open_at)]
            for name in _FN_NAME.findall(body):
                if ring_of[rel] not in ("core", "item"):
                    continue
                declared[name] = rel
                if name not in listed:
                    problems.append(
                        f"{rel}: `{name}` is declared in an extern \"Rust\" block of the "
                        f"{ring_of[rel]} and is not in composition_root.hooks"
                    )
        for found in _NO_MANGLE.finditer(masked):
            name = found.group(1)
            open_at = masked.find("{", found.end())
            body = masked[open_at + 1:_closing(masked, open_at)].strip()
            defined[name].append((rel, body))
    for name, sites in sorted(defined.items()):
        if name not in listed and name not in declared:
            continue
        for rel, body in sites:
            if rel != root:
                problems.append(f"{rel}: hook `{name}` is defined outside the composition root ({root})")
                continue
            call = _CALL.match(body)
            want = listed.get(name, {}).get("forwards_to")
            if call is None:
                problems.append(f"{rel}: hook `{name}`'s body is not a single forwarding call")
            elif want is not None and call.group(1) != want:
                problems.append(
                    f"{rel}: hook `{name}` forwards to `{call.group(1)}`, and "
                    f"composition_root.hooks says `{want}`"
                )
        if len(sites) > 1:
            problems.append(f"hook `{name}` is defined {len(sites)} times")
    for name, entry in sorted(listed.items()):
        if name not in declared:
            problems.append(f"composition_root.hooks names `{name}`, which no core or item file declares")
        elif entry.get("declared_in") not in (None, declared[name]):
            problems.append(
                f"composition_root.hooks says `{name}` is declared in {entry['declared_in']}; "
                f"it is in {declared[name]}"
            )
        if name not in defined:
            problems.append(f"composition_root.hooks names `{name}`, which the composition root does not define")
    return problems


# --- the kernel's crates (docs/KERNEL-CRATES.md) ------------------------------

# A checks-only item: `pub` so a raised check in a higher crate can reach it,
# and not interface for product code there (ledger line 1531, D6).
CHECKS_ONLY = re.compile(r"//\s*CHECKS-ONLY:")
CHECKS_ONLY_ITEM = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:const|unsafe|async|extern\s+\"[^\"]*\")\s+)*"
    r"(?:fn|struct|enum|union|trait|type|const|static|mod)\s+(?:mut\s+)?(?:r#)?([^\W\d]\w*)"
)
DISALLOWED_ALLOW = re.compile(r"#\s*(!?)\s*\[\s*(?:allow|expect)\s*\([^\]]*clippy::disallowed_methods")


def kernel_crate_files(entries: dict[str, dict], read, exists, listing: list[str]):
    """`{file: [packages whose module tree reaches it]}` for every kernel file,
    each crate walked from its root by `mod` declarations, inline ones
    included; and the declarations that name no file."""
    reached: dict[str, list[str]] = {rel: [] for rel in listing}
    problems: list[str] = []
    roots = ROOT_FILES | {e["root"] for e in entries.values() if e.get("root")}
    for package, entry in sorted(entries.items()):
        root = entry.get("root")
        if not root or not exists(root):
            problems.append(f"kernel_crates.{package}: its root {root!r} is not a kernel file")
            continue
        seen: set[str] = set()
        queue = [root]
        while queue:
            rel = queue.pop()
            if rel in seen:
                continue
            seen.add(rel)
            reached.setdefault(rel, []).append(package)
            masked = rustlex.mask(read(rel))
            stem = rel[:-3]
            parts = stem.split("/")
            base = "/".join(parts[:-1]) if parts[-1] == "mod" or rel in roots else stem
            toks = [t for t, _ in rustlex.tokens(masked)]
            stack: list[tuple[str, int]] = []
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
                    directory = "/".join([p for p in [base] + [s for s, _ in stack] if p])
                    prefix = f"{directory}/{name}" if directory else name
                    found = [c for c in (f"{prefix}.rs", f"{prefix}/mod.rs") if exists(c)]
                    if not found:
                        problems.append(f"{rel}: `mod {name};` ({package}) has no file this gate can find")
                        continue
                    queue.append(found[0])
    return reached, problems


def checks_only_items(files: list[str], read, problems: list[str] | None = None) -> dict[tuple[ModPath, str], str]:
    """`{(module, item): file}` for every item marked `// CHECKS-ONLY:`: the
    marker on a line of its own at the file's own module scope, then the item
    after its attributes and doc comments. A marker the gate cannot place --
    no item follows it, or it stands inside braces (an inline module, an
    `impl`, a function), where the item is not the file module's -- is put in
    `problems` (ledger line 1565, (2)(a)): a mark that marks nothing would
    let the item it meant be named from product code unseen."""
    out = {}
    for rel in files:
        source = read(rel)
        masked = rustlex.mask(source)
        lines = source.split("\n")
        offset = 0
        starts = []
        for line in lines:
            starts.append(offset)
            offset += len(line) + 1
        for number, line in enumerate(lines):
            if not CHECKS_ONLY.search(line):
                continue
            before = masked[: starts[number]]
            if before.count("{") != before.count("}"):
                if problems is not None:
                    problems.append(f"{rel}:{number + 1}: a CHECKS-ONLY marker inside braces (an inline "
                                    f"module, an impl or a function): the gate cannot place its item")
                continue
            found = None
            for after in lines[number + 1 :]:
                stripped = after.strip()
                if not stripped or stripped.startswith(("#[", "///", "//")):
                    continue
                found = CHECKS_ONLY_ITEM.match(after)
                break
            if found is None:
                if problems is not None:
                    problems.append(f"{rel}:{number + 1}: a CHECKS-ONLY marker followed by no item the "
                                    f"gate can place (fn, struct, enum, union, trait, type, const, static, mod)")
                continue
            out[(module_of_file(rel), found.group(1))] = rel
    return out


def kernel_crate_problems(manifest: dict, files: list[str], ring_of: dict[str, str], read, exists,
                          members: dict[str, str]) -> list[str]:
    """The kernel's crates against their rings (ledger lines 1477 and 1531):

    * every package under src/kernel is in `kernel_crates`, and every entry is
      a package there (`members`: package -> directory);
    * each file is reached by exactly one crate's module tree;
    * a file's crate holds its ring: an entry's `rings`, or the crate's own
      root (the composition root), or `raised_checks`, which may name only
      check files, and none that no longer needs it;
    * a checks-only item is named by no product file, and only a raised check
      allows clippy's `disallowed_methods` for a whole file."""
    entries = {k: v for k, v in manifest.get("kernel_crates", {}).items() if not k.startswith("//")}
    problems: list[str] = []
    if "ferrix-kernel" not in entries:
        problems.append("kernel_crates: no entry for ferrix-kernel, the kernel binary")
    for package, directory in sorted(members.items()):
        entry = entries.get(package)
        if entry is None:
            problems.append(f"{directory}: package {package} is under src/kernel and not in kernel_crates")
        elif entry.get("path") != directory:
            problems.append(f"kernel_crates.{package}: path {entry.get('path')!r}, the workspace has {directory!r}")
    for package in sorted(entries):
        if package not in members:
            problems.append(f"kernel_crates.{package}: no such package under src/kernel")

    reached, found = kernel_crate_files(entries, read, exists, files)
    problems += found
    raised = {
        e["file"]: e for e in manifest.get("raised_checks", {}).get("entries", [])
    }
    for rel in sorted(reached):
        crates = reached[rel]
        if not crates:
            problems.append(f"{rel}: reached by no crate's module tree, so built by none")
            continue
        if len(crates) > 1:
            problems.append(f"{rel}: reached by {len(crates)} crates ({', '.join(crates)}): "
                            f"each would build its own copy, statics and locks included")
            continue
        crate = crates[0]
        ring = ring_of.get(rel)
        holds = set(entries.get(crate, {}).get("rings", []))
        own_root = entries.get(crate, {}).get("root") == rel
        entry = raised.get(rel)
        if entry is not None:
            if not is_test_file(rel, manifest):
                problems.append(f"raised_checks: {rel} is product code; only a check may be raised")
                continue
            if entry.get("crate") != crate and not (ring in holds or own_root):
                problems.append(f"raised_checks: {rel} names {entry.get('crate')}, it is reached by {crate}")
                continue
        if ring not in holds and not own_root:
            if entry is None:
                problems.append(f"{rel}: {ring} ring, reached by {crate}, which holds "
                                f"{sorted(holds) or 'only its root'}")
            continue
        if entry is not None:
            problems.append(f"raised_checks: {rel} is in its ring's crate ({crate}); the entry is stale")
    for rel in sorted(raised):
        if rel not in reached:
            problems.append(f"raised_checks: {rel} is no kernel file; the entry is stale")

    # Checks-only items, and the allows that let a check call their methods.
    marked = checks_only_items(files, read, problems)
    modules, refs, _ = build(files, read)
    problems += kernel_paths_unresolved(refs, modules, {p.replace("-", "_") for p in entries})
    if marked:
        resolver = Resolver(modules, {p.replace("-", "_") for p in entries})
        for ref in refs:
            if is_test_file(ref.file, manifest):
                continue
            landed = resolver.resolve(ref.module, ref.segments, ref.is_use)
            if landed is None:
                continue
            for name in ref.segments:
                where = marked.get((landed[0], name))
                if where is not None:
                    problems.append(f"{ref.file}:{ref.line}: names {key_of(landed[0])}::{name}, "
                                    f"checks-only ({where}), from product code")
    for rel in files:
        masked_lines = read(rel).split("\n")
        for number, line in enumerate(masked_lines, 1):
            allow = DISALLOWED_ALLOW.search(line)
            if not allow:
                continue
            whole_file = allow.group(1) == "!"
            if whole_file and rel not in raised:
                problems.append(f"{rel}:{number}: allows clippy::disallowed_methods for the whole file; "
                                f"only a raised check may")
            elif not whole_file and rel not in raised and rel not in set(marked.values()):
                problems.append(f"{rel}:{number}: allows clippy::disallowed_methods outside a raised check "
                                f"and outside the module of a checks-only item")
            elif not whole_file and "reason" not in line:
                problems.append(f"{rel}:{number}: allows clippy::disallowed_methods without a reason")
    return problems


def kernel_paths_unresolved(refs, modules, crate_names: set[str]) -> list[str]:
    """Paths written through one of the kernel's crate names that resolve to
    nothing: never skipped (ledger line 1531, D1)."""
    resolver = Resolver(modules, crate_names)
    out = []
    for ref in refs:
        first = ref.segments[1] if ref.segments[0] == "::" and len(ref.segments) > 1 else ref.segments[0]
        if first not in crate_names:
            continue
        rest = ref.segments[ref.segments.index(first) + 1:]
        if not rest:
            # `use ferrix_kernel_item::*;`, the crate's root itself.
            continue
        if resolver.member((), rest[0], (), len(rest) == 1 and not ref.is_use, frozenset()) is None:
            out.append(f"{ref.file}:{ref.line}: {'::'.join(ref.segments)} names nothing in the kernel's crates")
    return out


def kernel_dependency_problems(manifest: dict, cargo_of, workspace: dict) -> list[str]:
    """A kernel crate that holds only the core or only the item may depend on
    kernel crates and classified crates of its ring or below, and on the
    allowlist; nothing of a higher ring, nothing in no ring (ledger 1531, D2)."""
    entries = {k: v for k, v in manifest.get("kernel_crates", {}).items() if not k.startswith("//")}
    classified = crate_entries(manifest)
    allow = crate_allowlist(manifest)
    problems = []
    for package, entry in sorted(entries.items()):
        rings = entry.get("rings", [])
        if len(rings) != 1 or rings[0] not in ("core", "item"):
            continue
        ring = rings[0]
        cargo = cargo_of(package)
        if cargo is None:
            problems.append(f"{package}: cannot read its manifest")
            continue
        if cargo.get("lints", {}).get("workspace") is not True:
            problems.append(f"{package}: its manifest does not inherit the workspace lints")
        if "build" in cargo.get("package", {}) or entry.get("build_script"):
            problems.append(f"{package}: a kernel library crate has no build script")
        for dep in _dependency_names(cargo, workspace):
            other = entries.get(dep)
            if other is not None:
                theirs = other.get("rings", [])
                if len(theirs) != 1 or RING_ORDER.get(theirs[0], 9) > RING_ORDER[ring]:
                    problems.append(f"{package} -> {dep}: {_a(ring)} kernel crate depends on one holding {theirs}")
            elif dep in classified:
                if RING_ORDER.get(classified[dep]["ring"], 9) > RING_ORDER[ring]:
                    problems.append(f"{package} -> {dep}: {_a(ring)} kernel crate depends on "
                                    f"{_a(classified[dep]['ring'])} crate")
            elif dep not in allow:
                problems.append(f"{package} -> {dep}: {_a(ring)} kernel crate depends on a crate in no ring "
                                f"and not in crates.infrastructure_allowlist")
    return problems


def kernel_members() -> dict[str, str]:
    """`{package: directory}` for the workspace's members at or under src/kernel."""
    import tomllib

    with (ROOT / "Cargo.toml").open("rb") as handle:
        members = tomllib.load(handle).get("workspace", {}).get("members", [])
    out = {}
    for member in members:
        if member == "src/kernel" or member.startswith("src/kernel/"):
            with (ROOT / member / "Cargo.toml").open("rb") as handle:
                out[tomllib.load(handle)["package"]["name"]] = member
    return out


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
    failures += _hook_self_test()
    failures += _kernel_self_test()
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


# The hook rule's cases: a tree that keeps it, and the three ways to break it
# the consultant asked to see fail (ledger line 415, H1), each on its own.
_HOOK_RINGS = {"main.rs": "load", "trap.rs": "core", "up.rs": "load"}
_HOOK_LIST = [{"name": "hook_up", "declared_in": "trap.rs", "forwards_to": "up::go"}]
_HOOK_TREE = {
    "main.rs": "mod trap; mod up;\n#[unsafe(no_mangle)]\nfn hook_up(a: &[u64; 6]) -> bool {\n"
               "    up::go(a)\n}\n",
    "trap.rs": "// SAFETY: (LINK) x\nunsafe extern \"Rust\" {\n    safe fn hook_up(a: &[u64; 6]) -> bool;\n}\n"
               "unsafe extern \"C\" {\n    fn stub();\n}\n",
    "up.rs": "pub fn go(a: &[u64; 6]) -> bool { a[0] == 1 }\n",
}
_HOOK_BREAKS = [
    ("an undeclared hook in the core",
     {"trap.rs": _HOOK_TREE["trap.rs"] + "unsafe extern \"Rust\" { fn sneak(); }\n"},
     "not in composition_root.hooks"),
    ("a definition outside the composition root",
     {"main.rs": "mod trap; mod up;\n",
      "up.rs": _HOOK_TREE["up.rs"] + "#[unsafe(no_mangle)]\nfn hook_up(a: &[u64; 6]) -> bool { go(a) }\n"},
     "outside the composition root"),
    ("a definition that does more than forward",
     {"main.rs": "mod trap; mod up;\n#[unsafe(no_mangle)]\nfn hook_up(a: &[u64; 6]) -> bool {\n"
                 "    let b = up::go(a);\n    b || a[1] == 0\n}\n"},
     "not a single forwarding call"),
]


def _hook_self_test() -> list[str]:
    failures = []
    found = hook_problems(_HOOK_RINGS, _HOOK_TREE.get, _HOOK_LIST, "main.rs")
    if found:
        failures.append(f"hooks: a tree that keeps the rule was refused: {found}")
    for what, change, expect in _HOOK_BREAKS:
        tree = {**_HOOK_TREE, **change}
        found = hook_problems(_HOOK_RINGS, tree.get, _HOOK_LIST, "main.rs")
        if not any(expect in problem for problem in found):
            failures.append(f"hooks: {what} passed ({found})")
    return failures


# The kernel split's rules (ledger line 1531, D7) on a tree of four crates
# shaped as docs/KERNEL-CRATES.md lays them out; each break must be refused.
_KERNEL_TREE = {
    "main.rs": "mod stages_check;\nfn main() {}\n",
    "kernel_core.rs": "pub mod mm;\npub mod object;\n",
    "kernel_item.rs": "pub use ferrix_kernel_core::{mm, object};\npub mod syscall;\n",
    "kernel_load.rs": "pub use ferrix_kernel_item::*;\npub mod fs;\n"
                      "pub mod object { pub use ferrix_kernel_core::object::*; pub mod check; }\n"
                      "pub mod syscall { pub use ferrix_kernel_item::syscall::*; pub mod eventfd; }\n",
    "mm.rs": "// CHECKS-ONLY: object/check.rs\n#[doc(hidden)]\npub fn secret() {}\npub fn frame() {}\n",
    "object/mod.rs": "pub fn make() { crate::mm::frame(); }\n",
    "object/check.rs": "#![allow(clippy::disallowed_methods, reason = \"a raised check\")]\n"
                       "fn c() { crate::mm::secret(); crate::fs::open(); }\n",
    "syscall/mod.rs": "pub use crate::mm::*;\npub fn dispatch() {}\n",
    "syscall/eventfd.rs": "fn e() { super::dispatch(); }\n",
    "fs.rs": "pub fn open() { ferrix_kernel_core::mm::frame(); }\n",
    "stages_check.rs": "fn s() { crate::fs::open(); ferrix_kernel_load::object::make(); }\n",
}
_KERNEL_MANIFEST = {
    "rings": {
        "core": {"members": ["kernel_core.rs", "mm.rs", "object/**"]},
        "item": {"members": ["kernel_item.rs", "main.rs", "syscall/mod.rs", "stages_check.rs"]},
        "load": {"members": ["kernel_load.rs", "fs.rs", "syscall/eventfd.rs", "x.rs"]},
    },
    "test_file_patterns": ["check.rs", "*_check.rs"],
    "kernel_crates": {
        "ferrix-kernel": {"path": "k", "root": "main.rs", "rings": []},
        "ferrix-kernel-core": {"path": "k/core", "root": "kernel_core.rs", "rings": ["core"]},
        "ferrix-kernel-item": {"path": "k/item", "root": "kernel_item.rs", "rings": ["item"]},
        "ferrix-kernel-load": {"path": "k/load", "root": "kernel_load.rs", "rings": ["load"]},
    },
    "raised_checks": {"entries": [
        {"file": "object/check.rs", "crate": "ferrix-kernel-load"},
        {"file": "stages_check.rs", "crate": "ferrix-kernel"},
    ]},
}
_KERNEL_MEMBERS = {"ferrix-kernel": "k", "ferrix-kernel-core": "k/core",
                   "ferrix-kernel-item": "k/item", "ferrix-kernel-load": "k/load"}


def _kernel_break(tree: dict | None = None, raised: list | None = None, members: dict | None = None):
    files = dict(_KERNEL_TREE, **(tree or {}))
    files = {k: v for k, v in files.items() if v is not None}
    manifest = dict(_KERNEL_MANIFEST)
    if raised is not None:
        manifest["raised_checks"] = {"entries": raised}
    ring_of, _, _ = classify(manifest, sorted(files))
    roots = {e["root"] for e in manifest["kernel_crates"].values()}
    added = roots - ROOT_FILES
    ROOT_FILES.update(added)
    try:
        return kernel_crate_problems(manifest, sorted(files), ring_of, files.__getitem__,
                                     files.__contains__, members or _KERNEL_MEMBERS)
    finally:
        ROOT_FILES.difference_update(added)


_KERNEL_BREAKS = [
    ("four roots: a file reached by two crates",
     {"tree": {"kernel_core.rs": "pub mod mm;\npub mod object;\npub mod fs;\n"}}, "reached by 2 crates"),
    ("four roots: a file reached by none",
     {"tree": {"kernel_load.rs": _KERNEL_TREE["kernel_load.rs"].replace("pub mod fs;\n", "")}},
     "reached by no crate"),
    ("crate equals ring: a load file in the core's crate",
     {"tree": {"kernel_core.rs": "pub mod mm;\npub mod object;\npub mod x;\n", "x.rs": ""}},
     "load ring, reached by ferrix-kernel-core"),
    ("a raised check back in its ring's crate is stale",
     {"tree": {"object/mod.rs": "pub mod check;\npub fn make() {}\n",
               "kernel_load.rs": _KERNEL_TREE["kernel_load.rs"].replace(" pub mod check;", "")}},
     "the entry is stale"),
    ("a raised entry naming no file is stale",
     {"raised": _KERNEL_MANIFEST["raised_checks"]["entries"] + [{"file": "gone_check.rs", "crate": "ferrix-kernel"}]},
     "is no kernel file"),
    ("product code is never raised",
     {"raised": _KERNEL_MANIFEST["raised_checks"]["entries"] + [{"file": "fs.rs", "crate": "ferrix-kernel-load"}]},
     "is product code"),
    ("a checks-only item named from product code",
     {"tree": {"fs.rs": "pub fn open() { crate::mm::secret(); }\n"}}, "checks-only"),
    ("a checks-only item named through a re-export chain",
     {"tree": {"fs.rs": "pub fn open() { crate::syscall::secret(); }\n"}}, "checks-only"),
    ("a path through a kernel crate that names nothing",
     {"tree": {"fs.rs": "pub fn open() { ferrix_kernel_core::nothing::here(); }\n"}}, "names nothing"),
    ("a whole-file allow of disallowed_methods outside a raised check",
     {"tree": {"fs.rs": "#![allow(clippy::disallowed_methods)]\npub fn open() {}\n"}}, "for the whole file"),
    ("an item allow of disallowed_methods in a product module",
     {"tree": {"fs.rs": "#[allow(clippy::disallowed_methods, reason = \"x\")]\npub fn open() {}\n"}},
     "outside a raised check"),
    ("an allow of disallowed_methods without a reason",
     {"tree": {"mm.rs": _KERNEL_TREE["mm.rs"] + "#[allow(clippy::disallowed_methods)]\nfn f() {}\n"}},
     "without a reason"),
    ("a CHECKS-ONLY marker followed by no item",
     {"tree": {"mm.rs": _KERNEL_TREE["mm.rs"] + "// CHECKS-ONLY: object/check.rs\nmacro_rules! m { () => {} }\n"}},
     "followed by no item"),
    ("a CHECKS-ONLY marker on an item inside an inline module",
     {"tree": {"mm.rs": _KERNEL_TREE["mm.rs"] + "pub mod inner {\n    // CHECKS-ONLY: object/check.rs\n"
                                                "    pub fn hidden() {}\n}\n"}},
     "inside braces"),
    ("a CHECKS-ONLY marker on an item inside an impl",
     {"tree": {"mm.rs": _KERNEL_TREE["mm.rs"] + "pub struct S;\nimpl S {\n    // CHECKS-ONLY: object/check.rs\n"
                                                "    pub fn hidden(&self) {}\n}\n"}},
     "inside braces"),
    ("a package under src/kernel not in kernel_crates",
     {"members": dict(_KERNEL_MEMBERS, **{"ferrix-kernel-extra": "k/extra"})}, "not in kernel_crates"),
]

# The dependency rule for the core's and the item's kernel crates (D2).
_KDEP_MANIFEST = dict(_KERNEL_MANIFEST, crates={
    "members": {"ferrix-low": {"ring": "core", "path": "-"}, "ferrix-high": {"ring": "load", "path": "-"}},
    "infrastructure_allowlist": {"ferrix-sync": "why"},
})
_KDEP_CASES = [
    ("a core library", "ferrix-kernel-core", {"ferrix-low": "1", "ferrix-sync": "1"}, None),
    ("the core, from the item", "ferrix-kernel-item", {"ferrix-kernel-core": "1"}, None),
    ("the item, from the core", "ferrix-kernel-core", {"ferrix-kernel-item": "1"}, "depends on one holding"),
    ("a load library, from the core", "ferrix-kernel-core", {"ferrix-high": "1"}, "depends on a load crate"),
    ("a crate in no ring", "ferrix-kernel-item", {"ferrix-other": "1"}, "in no ring"),
]


def _kernel_upward_self_test() -> list[str]:
    """The upward-reference rule sees a path through a kernel crate's name
    (ledger line 1565, (2)(b)): a core file and an item file each naming
    `ferrix_kernel_load::fs`, the load's, are refused."""
    failures = []
    manifest = dict(_KERNEL_MANIFEST, dependency_rules=[
        {"from": "core", "may_not_reference": ["item", "load"]},
        {"from": "item", "may_not_reference": ["load"]},
    ], crates={"members": {}})
    for what, rel, text, want in [
        ("a core file", "mm.rs", _KERNEL_TREE["mm.rs"] + "fn up() { ferrix_kernel_load::fs::open(); }\n", "core"),
        ("an item file", "syscall/mod.rs",
         _KERNEL_TREE["syscall/mod.rs"] + "fn up() { ::ferrix_kernel_load::fs::open(); }\n", "item"),
    ]:
        files = dict(_KERNEL_TREE, **{rel: text})
        ring_of, _, _ = classify(manifest, sorted(files))
        added = {e["root"] for e in manifest["kernel_crates"].values()} - ROOT_FILES
        ROOT_FILES.update(added)
        try:
            modules, refs, _ = build(sorted(files), files.__getitem__)
            edges = find_edges(manifest, ring_of, modules, refs)
        finally:
            ROOT_FILES.difference_update(added)
        if not any(e.file == rel and e.from_ring == want and e.to_ring == "load" for e in edges):
            failures.append(f"kernel crates: {what} naming ferrix_kernel_load::fs passed the upward rule "
                            f"({[(e.file, e.references) for e in edges]})")
    return failures


def _kernel_self_test() -> list[str]:
    failures = _kernel_upward_self_test()
    kept = _kernel_break()
    if kept:
        failures.append(f"kernel crates: a tree that keeps the rules was refused: {kept}")
    for what, change, expect in _KERNEL_BREAKS:
        found = _kernel_break(**change)
        if not any(expect in problem for problem in found):
            failures.append(f"kernel crates: {what} passed ({found})")
    for what, package, deps, expect in _KDEP_CASES:
        manifests = {p: {"lints": {"workspace": True}, "package": {"name": p}} for p in _KERNEL_MEMBERS}
        manifests[package] = dict(manifests[package], dependencies=deps)
        found = [p for p in kernel_dependency_problems(_KDEP_MANIFEST, manifests.get, {})
                 if p.startswith(package)]
        if (expect is None) != (not found) or (expect and not any(expect in p for p in found)):
            failures.append(f"kernel crates: dependency {what}: {found or 'passed'}")
    bare = {p: {"package": {"name": p}} for p in _KERNEL_MEMBERS}
    if not any("workspace lints" in p for p in kernel_dependency_problems(_KDEP_MANIFEST, bare.get, {})):
        failures.append("kernel crates: a manifest without the workspace lints passed")
    return failures


# --- the profile rule (docs/BUILD-FLAGS.md rule 3) ---------------------------

FLAG_FILE = ROOT / "tools" / "common" / "data" / "build-flags.toml"
PROFILE_DIR = ROOT / "tools" / "common" / "data" / "profiles"
ARCH_TRIPLES = {"x86_64": "x86_64-unknown-none", "aarch64": "aarch64-unknown-none-softfloat",
                "armv7a": "armv7a-none-eabi"}
BUILD_RS_ARCH = {"x86_64": "x86_64", "aarch64": "aarch64", "armv7a": "arm"}
BUILT = ("yes", "stand-in", "no")


def covers(levels: dict, standards: dict, a: str, b: str) -> bool:
    """Whether level `a` covers level `b`: for every standard in b's set, a
    names a target at least as high on that standard's scale (BF10)."""
    mine = levels.get(a, {}).get("targets", {})
    for standard, target in levels.get(b, {}).get("targets", {}).items():
        scale = standards.get(standard, {}).get("scale", [])
        if standard not in mine or target not in scale or mine[standard] not in scale:
            return False
        if scale.index(mine[standard]) < scale.index(target):
            return False
    return True


def flag_file_problems(flags: dict) -> list[str]:
    """The flag file's own consistency: levels as target sets on declared
    scales, the present/sc2 pair incomparable, and each value's allows
    closed under coverage, its build state and its requirements."""
    problems = []
    standards, levels = flags.get("standards", {}), flags.get("levels", {})
    for name, level in levels.items():
        for standard, target in level.get("targets", {}).items():
            if target not in standards.get(standard, {}).get("scale", []):
                problems.append(f"level {name}: {standard} = {target!r} is on no declared scale")
    for needed in ("none", "present", "sc2", "sc4"):
        if needed not in levels:
            problems.append(f"level {needed} is not declared")
    if covers(levels, standards, "present", "sc2") or covers(levels, standards, "sc2", "present"):
        problems.append("levels present and sc2 must be incomparable, standard by standard (BF10), "
                        "and one covers the other")
    for higher, lower in (("sc4", "present"), ("sc4", "sc2"), ("present", "none"), ("sc2", "none")):
        if higher in levels and lower in levels and not covers(levels, standards, higher, lower):
            problems.append(f"level {higher} must cover {lower}")
    declared = {flag: set(entry.get("values", {})) for flag, entry in flags.get("flags", {}).items()}
    for flag, entry in flags.get("flags", {}).items():
        values = entry.get("values", {})
        if len(values) < 2:
            problems.append(f"{flag}: a flag has two values at least")
        for value, spec in values.items():
            where = f"{flag}={value}"
            allows = spec.get("allows", [])
            for level in allows:
                if level not in levels:
                    problems.append(f"{where} allows the undeclared level {level}")
            for level in allows:
                for other in levels:
                    if other not in allows and covers(levels, standards, level, other):
                        problems.append(f"{where} allows {level} but not {other}, which {level} covers")
            built = spec.get("built")
            if built not in BUILT:
                problems.append(f"{where}: built is one of {', '.join(BUILT)}")
            if built == "stand-in":
                instead = spec.get("stands_in")
                if values.get(instead, {}).get("built") != "yes":
                    problems.append(f"{where}: a stand-in names a built value of its flag in stands_in")
            if built == "no" and spec.get("lands") == "today":
                problems.append(f"{where}: a value that lands today is built")
            if not spec.get("lands"):
                problems.append(f"{where} names no lands")
            for pair in spec.get("requires", []) + spec.get("conflicts", []):
                other, _, other_value = pair.partition("=")
                if other_value not in declared.get(other, set()):
                    problems.append(f"{where} names {pair}, which is not declared")
    return problems


def profile_problems(name: str, profile: dict, flags: dict) -> tuple[list[str], list[str]]:
    """A profile file against the flag file: (problems, the reasons it does not
    build yet). Every flag set (rule 4), each value declared and allowed at
    the profile's level or by a dated exception in `present` alone (rule 3,
    BF1), requirements met, and the boot options it permits declared, with
    `ferrix.checks` running and no test-only option in a certified one."""
    problems, waiting = [], []
    standards, levels = flags.get("standards", {}), flags.get("levels", {})
    declared = flags.get("flags", {})
    level = profile.get("level")
    if profile.get("profile") != name:
        problems.append(f"its profile is {profile.get('profile')!r}, not its file's name")
    if level not in levels:
        problems.append(f"level {level!r} is not declared")
    values = profile.get("flags", {})
    for flag in declared:
        if flag not in values:
            problems.append(f"does not set {flag} (rule 4: every flag, explicitly)")
    exceptions = {(e.get("flag"), e.get("value")) for e in profile.get("exceptions", [])}
    if exceptions and name != "present":
        problems.append("only present may carry dated exceptions (BF1)")
    for exception in profile.get("exceptions", []):
        if not exception.get("ends") or not exception.get("why"):
            problems.append(f"exception {exception.get('flag')}={exception.get('value')} names no end or reason")
    for flag, value in values.items():
        spec = declared.get(flag, {}).get("values", {}).get(value)
        if spec is None:
            problems.append(f"{flag}={value} is not declared in build-flags.toml")
            continue
        for pair in spec.get("requires", []):
            other, _, other_value = pair.partition("=")
            if values.get(other) != other_value:
                problems.append(f"{flag}={value} requires {pair}")
        for pair in spec.get("conflicts", []):
            other, _, other_value = pair.partition("=")
            if values.get(other) == other_value:
                problems.append(f"{flag}={value} conflicts with {pair}")
        allowed = level in spec.get("allows", [])
        if not allowed and (flag, value) in exceptions and name == "present":
            allowed = True
        if not allowed:
            problems.append(f"{flag}={value} does not allow level {level} (it allows "
                            f"{', '.join(spec.get('allows', []))}): refused (rule 3)")
        if spec.get("built") == "no" or (spec.get("built") == "stand-in" and level != "none"):
            waiting.append(f"{flag}={value} lands at {spec.get('lands')}")
    for flag, value in exceptions:
        spec = declared.get(flag, {}).get("values", {}).get(value, {})
        if values.get(flag) != value or level in spec.get("allows", []):
            problems.append(f"exception {flag}={value} excuses nothing this profile uses")
    registry = flags.get("boot_options", {})
    for option, permitted in profile.get("boot_options", {}).items():
        entry = registry.get(option)
        if entry is None:
            problems.append(f"permits the boot option {option}, which the registry does not declare")
            continue
        if entry.get("kind") == "flag":
            if permitted is not True:
                problems.append(f"{option} is a bare word: permit it as true")
        elif not isinstance(permitted, list) or not set(permitted) <= set(entry.get("values", [])) | {"*"} \
                or ("*" in permitted and "*" not in entry.get("values", [])):
            problems.append(f"{option}: permits values the registry does not declare")
        if level != "none" and entry.get("test_only"):
            problems.append(f"a certified profile permits the test-only option {option}")
    if level != "none" and profile.get("boot_options", {}).get("ferrix.checks", ["run"]) != ["run"]:
        problems.append("a certified profile permits ferrix.checks only with the checks running")
    for arch in profile.get("claim_architectures", []):
        if arch not in ARCH_TRIPLES:
            problems.append(f"claims the unknown architecture {arch}")
    return problems, waiting


def build_record_problems(profile: dict) -> list[str]:
    """The profile's build record against the tree (BF4): the toolchain, the
    release profile's codegen, the kernel targets' rustflags and build.rs's
    link arguments."""
    problems = []
    build = profile.get("build", {})
    with (ROOT / "rust-toolchain.toml").open("rb") as handle:
        channel = tomllib.load(handle).get("toolchain", {}).get("channel")
    if build.get("toolchain") != channel:
        problems.append(f"build.toolchain {build.get('toolchain')} is not rust-toolchain.toml's {channel}")
    with (ROOT / "Cargo.toml").open("rb") as handle:
        cargo_profiles = tomllib.load(handle).get("profile", {})
    release = cargo_profiles.get(build.get("cargo_profile"), {})
    for key, value in build.get("codegen", {}).items():
        if release.get(key) != value:
            problems.append(f"build.codegen.{key} = {value!r}, Cargo.toml's profile says {release.get(key)!r}")
    with (ROOT / ".cargo" / "config.toml").open("rb") as handle:
        targets = tomllib.load(handle).get("target", {})
    for arch, triple in ARCH_TRIPLES.items():
        if build.get("rustflags", {}).get(arch) != targets.get(triple, {}).get("rustflags"):
            problems.append(f"build.rustflags.{arch} is not .cargo/config.toml's [target.{triple}]")
    script = (ROOT / "src" / "kernel" / "build.rs").read_text(encoding="utf-8")
    for arch, key in BUILD_RS_ARCH.items():
        found = re.search(rf'"{key}"\s*=>\s*&\[([^\]]*)\]', script)
        args = re.findall(r'"([^"]*)"', found.group(1)) if found else None
        if build.get("link", {}).get(arch, {}).get("args") != args:
            problems.append(f"build.link.{arch}.args is not what src/kernel/build.rs links {arch} with")
    return problems


def component_problems(name: str, profile: dict, flags: dict, manifest: dict) -> list[str]:
    """Rule 3's second half: each component linked into ring 0 under the
    profile is at its level or above. Core and item are always linked; a ring
    with `in_ring0_when` only when one of those flag values is chosen, and
    then a dated exception for that value covers it if it is in the load
    ring, and never if it is in the core or the item. A crate's own `level`
    and `in_ring0_when` override its ring's."""
    standards, levels = flags.get("standards", {}), flags.get("levels", {})
    level = profile.get("level")
    values = profile.get("flags", {})
    exceptions = {f"{e.get('flag')}={e.get('value')}" for e in profile.get("exceptions", [])}
    problems = []
    rings = manifest["rings"]
    components = [(f"ring {ring}", ring, entry.get("level"), entry.get("in_ring0_when", []))
                  for ring, entry in rings.items()]
    components += [(f"crate {crate}", entry.get("ring"),
                    entry.get("level", rings.get(entry.get("ring"), {}).get("level")),
                    entry.get("in_ring0_when", rings.get(entry.get("ring"), {}).get("in_ring0_when", [])))
                   for crate, entry in manifest["crates"]["members"].items()]
    for what, ring, component_level, when in components:
        chosen = [pair for pair in when if values.get(pair.partition("=")[0]) == pair.partition("=")[2]]
        if when and not chosen:
            continue
        if component_level not in levels:
            problems.append(f"{what} names no declared level")
        elif not covers(levels, standards, component_level, level):
            # The exception excuses the uncertified load a value brings into
            # ring 0, never a component the item claims: a core or item one
            # below the level is refused whatever brought it in.
            if ring == "load" and any(pair in exceptions for pair in chosen):
                continue
            problems.append(f"{what} is at level {component_level}, below the profile's {level}, "
                            f"and is linked into ring 0 (rule 3)")
    return problems


def reference_problems(manifest: dict, present: dict) -> list[str]:
    """reference_configuration is the `present` profile's (BF1), not a second
    copy that can drift from it."""
    reference = manifest.get("reference_configuration", {})
    build = present.get("build", {})
    pairs = [
        ("kernel_profile", reference.get("kernel_profile"), "present"),
        ("toolchain", reference.get("toolchain"), build.get("toolchain")),
        ("profile", reference.get("profile"), build.get("cargo_profile")),
        ("architectures", reference.get("architectures"), present.get("claim_architectures")),
        ("mitigations", reference.get("mitigations"), present.get("flags", {}).get("mitigations")),
        ("driver_updates", reference.get("driver_updates"), present.get("image", {}).get("driver_updates")),
        ("devmgr", [reference.get("devmgr")], present.get("boot_options", {}).get("ferrix.devmgr")),
    ]
    return [f"reference_configuration.{key} is {mine!r}, profiles/present.toml says {theirs!r}"
            for key, mine, theirs in pairs if mine != theirs]


def judge_profile(name: str, profile: dict, flags: dict, manifest: dict) -> tuple[list[str], list[str]]:
    """One profile under the rule: (problems, notes). A component below the
    profile's level is a note only while some value of the profile is refused
    by `lands` -- nothing it would link is built then -- and a failure as soon
    as every value builds, by the `lands` state alone, never by a later edit
    (ledger 1039, condition 4)."""
    found, waiting = profile_problems(name, profile, flags)
    components = component_problems(name, profile, flags, manifest)
    if waiting:
        return found, [f"profile {name}: valid statically, not yet buildable ({'; '.join(waiting)})"] + \
            [f"profile {name}: once buildable, {p}" for p in components]
    return found + components, [f"profile {name}: level {profile.get('level')}, buildable"]


def profile_rule(manifest: dict) -> tuple[list[str], list[str]]:
    """Every check above over the tree's files: (problems, notes)."""
    with FLAG_FILE.open("rb") as handle:
        flags = tomllib.load(handle)
    problems = [f"build-flags.toml: {p}" for p in flag_file_problems(flags)]
    notes = []
    profiles = {}
    for path in sorted(PROFILE_DIR.glob("*.toml")):
        with path.open("rb") as handle:
            profiles[path.stem] = tomllib.load(handle)
    for needed in ("present", "desktop", "fast", "max"):
        if needed not in profiles:
            problems.append(f"profiles/{needed}.toml is missing")
    for name, profile in profiles.items():
        found, said = judge_profile(name, profile, flags, manifest)
        found += build_record_problems(profile)
        notes += said
        problems += [f"profiles/{name}.toml: {p}" for p in found]
    if "present" in profiles:
        problems += reference_problems(manifest, profiles["present"])
    return problems, notes


def _profile_self_test() -> list[str]:
    """The rule's cases, on files written here: the incomparable pair, a
    below-level value, an exception outside present, a QM component."""
    failures = []
    flags = {
        "standards": {"s": {"scale": ["1", "2"]}, "t": {"scale": ["1", "2"]}, "u": {"scale": ["1"]}},
        "levels": {
            "none": {"targets": {}},
            "present": {"targets": {"s": "2", "u": "1"}},
            "sc2": {"targets": {"s": "2", "t": "2"}},
            "sc4": {"targets": {"s": "2", "t": "2", "u": "1"}},
        },
        "flags": {"f": {"values": {
            "a": {"allows": ["sc4", "present", "sc2", "none"], "lands": "today", "built": "yes"},
            "b": {"allows": ["sc2", "none"], "lands": "M3", "built": "yes"},
            "c": {"allows": ["none"], "lands": "M0", "built": "stand-in", "stands_in": "a"},
        }}},
        "boot_options": {"ferrix.checks": {"values": ["run", "skip"]}},
    }
    if flag_file_problems(flags):
        failures.append(f"a consistent flag file was refused: {flag_file_problems(flags)}")
    levels, standards = flags["levels"], flags["standards"]
    if covers(levels, standards, "present", "sc2") or covers(levels, standards, "sc2", "present"):
        failures.append("present and sc2 compare, where each lacks a standard of the other")
    if not covers(levels, standards, "sc4", "present") or not covers(levels, standards, "sc4", "sc2"):
        failures.append("sc4 does not cover present and sc2")
    broken = json.loads(json.dumps(flags))
    broken["levels"]["sc2"]["targets"]["u"] = "1"
    if not any("incomparable" in p for p in flag_file_problems(broken)):
        failures.append("a level table where sc2 covers present was accepted")
    def profile(level, value, exceptions=(), name="p"):
        return {"profile": name, "level": level, "flags": {"f": value},
                "exceptions": [dict(e) for e in exceptions], "boot_options": {"ferrix.checks": ["run"]}}
    if profile_problems("p", profile("sc2", "b"), flags)[0]:
        failures.append("a value allowing sc2 was refused at sc2")
    if not profile_problems("p", profile("present", "b"), flags)[0]:
        failures.append("a value allowing sc2 was accepted at present, which sc2 does not cover")
    if not profile_problems("p", profile("sc4", "c"), flags)[0]:
        failures.append("a below-level value was accepted in a certified profile")
    excepted = [{"flag": "f", "value": "c", "ends": "M5", "why": "w"}]
    if profile_problems("present", profile("present", "c", excepted, "present"), flags)[0]:
        failures.append("present's dated exception was not honoured")
    if not profile_problems("p", profile("present", "c", excepted), flags)[0]:
        failures.append("an exception outside present was honoured")
    manifest = {"rings": {"item": {"level": "present"}, "load": {"level": "none", "in_ring0_when": ["f=c"]}},
                "crates": {"members": {"k": {"ring": "item", "level": "none", "in_ring0_when": ["f=c"]}}}}
    found = component_problems("present", profile("present", "c", excepted, "present"), flags, manifest)
    if found != ["crate k is at level none, below the profile's present, and is linked into ring 0 (rule 3)"]:
        failures.append(f"a QM crate in the item, or the excepted load, was judged wrong: {found}")
    # A profile whose values all build today, with its item below its level,
    # FAILS: the note "once buildable" holds only while `lands` refuses one of
    # its values (ledger 1039, condition 4). The same profile with one value
    # not built yet is a note, and no problem.
    below = {"rings": {"item": {"level": "present"}}, "crates": {"members": {}}}
    problems, notes = judge_profile("p", profile("sc2", "a"), flags, below)
    if problems != ["ring item is at level present, below the profile's sc2, and is linked into ring 0 (rule 3)"]:
        failures.append(f"a buildable profile over components below its level did not fail: {problems}, {notes}")
    waiting = json.loads(json.dumps(flags))
    waiting["flags"]["f"]["values"]["a"]["built"] = "no"
    problems, notes = judge_profile("p", profile("sc2", "a"), waiting, below)
    if problems or not any("once buildable, ring item" in n for n in notes):
        failures.append(f"a profile refused by lands was failed, not noted: {problems}, {notes}")
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

    failures = self_test() + _profile_self_test()
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

    rule_problems, rule_notes = profile_rule(manifest)
    if rule_problems:
        print(
            f"item-boundary: {len(rule_problems)} problem(s) with the build flags and profiles\n"
            f"  (docs/BUILD-FLAGS.md rule 3, the profile rule, and rules 4 and 7):",
            file=sys.stderr,
        )
        for problem in rule_problems:
            print(f"    {problem}", file=sys.stderr)
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

    kernel_read = lambda rel: (KERNEL_SRC / rel).read_text(encoding="utf-8", errors="replace")  # noqa: E731
    kernel_problems = kernel_crate_problems(
        manifest, files, ring_of, kernel_read, lambda rel: (KERNEL_SRC / rel).is_file(), kernel_members()
    )
    import tomllib

    with (ROOT / "Cargo.toml").open("rb") as handle:
        workspace_deps = tomllib.load(handle).get("workspace", {}).get("dependencies", {})
    kernel_entries = {k: v for k, v in manifest.get("kernel_crates", {}).items() if not k.startswith("//")}

    def kernel_cargo(package: str):
        path = kernel_entries.get(package, {}).get("path")
        if path is None or not (ROOT / path / "Cargo.toml").is_file():
            return None
        with (ROOT / path / "Cargo.toml").open("rb") as handle:
            return tomllib.load(handle)

    kernel_problems += kernel_dependency_problems(manifest, kernel_cargo, workspace_deps)
    if kernel_problems:
        print(
            f"item-boundary: {len(kernel_problems)} problem(s) with the kernel's crates\n"
            f"  (docs/KERNEL-CRATES.md; the certification consultant's ledger lines\n"
            f"  1477 and 1531): each file in exactly one crate, the crate its ring's\n"
            f"  or a listed raised check, checks-only items out of product code:",
            file=sys.stderr,
        )
        for problem in kernel_problems:
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

    hook_found = hook_problems(
        ring_of,
        lambda rel: (KERNEL_SRC / rel).read_text(encoding="utf-8", errors="replace"),
        root.get("hooks", []),
        root.get("file"),
    )
    if hook_found:
        print(
            f"item-boundary: {len(hook_found)} problem(s) with the link-time hooks.\n"
            f"  A core or item file may declare a function in an extern \"Rust\"\n"
            f"  block only if composition_root.hooks names it, and the composition\n"
            f"  root alone defines it, as one call of what the entry forwards to:",
            file=sys.stderr,
        )
        for problem in hook_found:
            print(f"    {problem}", file=sys.stderr)
        status = 1

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
        f"{len(root_edges)} composition-root edge(s), "
        f"{len(root.get('hooks', []))} link-time hook(s)"
    )
    for note in crate_notes:
        print(f"  {note}")
    for note in rule_notes:
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
