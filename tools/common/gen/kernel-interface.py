#!/usr/bin/env python3
"""The kernel's crate interface: every `pub` in the core's and the item's files.

Since the kernel split (docs/KERNEL-CRATES.md, step S3) the kernel is four
crates, and nothing in the kernel was `pub` before it: an item is `pub` in a
core or item file only because a crate above it (the item, the load or the
binary) uses it. So the `pub` items of those files *are* the interface the
certified item offers what runs above it, and this script lists them, per
crate and per module, in docs/generated/KERNEL-INTERFACE.md -- the item, its
kind, which crates above name it, and whether it is checks-only (ledger line
1531, D5). A new `pub` in the core is then a line in that document's diff,
which a reviewer reads as a change to the item's interface.

It also writes the checks-only methods into `.clippy.toml`'s
`disallowed-methods` block (D6): a `pub` method or foreign function of a core
or item file marked `#[doc(hidden)]` is one, made `pub` only for a check raised
above its crate. Free items are marked with `// CHECKS-ONLY:` instead, which
check-item-boundary.py places and enforces.

    python3 tools/common/gen/kernel-interface.py           # write both
    python3 tools/common/gen/kernel-interface.py --check   # fail on drift
    python3 tools/common/gen/kernel-interface.py --self-test

`--check` fails when

  * the document or the `.clippy.toml` block is not what this script writes
    (a `pub` added, removed or moved without regenerating: D5 (a));
  * a `pub` item in a core or item file is named by no crate above it, and
    is not in the signature of one that is (dead interface: in a library a
    `pub` item never warns as unused, so it would rot where `pub(crate)`
    warns; D5 (b));
  * a `pub` item that is not checks-only names a checks-only type in its
    signature or a field's type, or a kernel trait is implemented for one
    (ledger line 1630, F2.2): the gate refuses product code that names a
    checks-only type, but a value reaches code by inference without its type
    being named, so whether product code can obtain one is checked here.

"Named" is by the item-boundary gate's resolver for paths (every module and
re-export a path passes through counts) and by token for what paths cannot
show: methods, fields and macros, named when a file above holds the name.
"""

from __future__ import annotations

import argparse
import importlib.util
import re
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent.parent
CHECK = ROOT / "tools" / "common" / "check"
sys.path.insert(0, str(CHECK))
import rustlex  # noqa: E402

_SPEC = importlib.util.spec_from_file_location("check_item_boundary", CHECK / "check-item-boundary.py")
gate = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(gate)

DOC = ROOT / "docs" / "generated" / "KERNEL-INTERFACE.md"
CLIPPY = ROOT / ".clippy.toml"
BEGIN = "# BEGIN kernel-interface.py"
END = "# END kernel-interface.py"
RANK = {"core": 0, "item": 1, "load": 2, "bin": 3}
ITEM_KINDS = {"fn", "struct", "enum", "union", "trait", "type", "const", "static", "mod"}
TYPE_KINDS = {"struct", "enum", "union", "type", "trait"}
ISAS = {"x86_64", "aarch64", "armv7a", "arm_common"}


class Entry:
    """One `pub` declaration."""

    def __init__(self, file, module, name, kind, container, line, signature, hidden):
        self.file = file
        self.module = module          # module path, inline modules included
        self.name = name
        self.kind = kind              # fn, struct, ..., method, field, foreign fn, re-export, macro
        self.container = container    # the impl's or struct's type, or None
        self.line = line
        self.signature = signature    # identifier tokens of its signature
        self.hidden = hidden          # #[doc(hidden)] on it
        self.checks_only = False
        self.named_by: set[str] = set()       # crates above naming it
        self.named_by_checks_only = True      # every naming file is a check

    def key(self):
        return (self.module, self.container or "", self.name, self.kind)


def _skip_group(toks, i, opening, closing):
    depth = 0
    while i < len(toks):
        if toks[i] == opening:
            depth += 1
        elif toks[i] == closing:
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return i


def _use_names(toks, i):
    """The names a `use` tree starting at toks[i] binds; (names, end index)."""
    names = []
    leaves: list = []
    end = gate._use_tree(toks, i, [], leaves)
    for segs, alias, glob in leaves:
        if glob:
            names.append("*")
        elif alias not in (None, "_"):
            names.append(alias)
        elif segs:
            names.append(segs[-1] if segs[-1] != "self" else (segs[-2] if len(segs) > 1 else "self"))
    return names, end


def parse(rel: str, source: str) -> list[Entry]:
    """Every `pub` declaration in one file."""
    masked = rustlex.mask(source)
    pairs = rustlex.tokens(masked)
    toks = [t for t, _ in pairs]
    offs = [o for _, o in pairs]
    lines = source.split("\n")

    def line_of(i):
        return masked.count("\n", 0, offs[i]) + 1

    def hidden_before(i):
        # `#[doc(hidden)]` among the attributes right above the token's line.
        number = line_of(i) - 2
        while number >= 0:
            text = lines[number].strip()
            if text == "#[doc(hidden)]":
                return True
            if text.startswith(("#[", "///", "//")) or text.endswith((",", ")]")) or text == ")]":
                number -= 1
                continue
            return False
        return False

    out: list[Entry] = []
    base = gate.module_of_file(rel)
    # Context stack: (kind, name, depth at which it closes).
    stack: list[tuple[str, str | None, int]] = []
    depth = 0
    pending: tuple[str, str | None] | None = None
    i = 0
    n = len(toks)

    def module_path():
        return base + tuple(name for kind, name, _ in stack if kind == "mod")

    def inside():
        return stack[-1][0] if stack else "mod"

    def container():
        for kind, name, _ in reversed(stack):
            if kind in ("impl", "struct", "enum", "union", "trait", "extern"):
                return name
        return None

    def signature(start, word):
        # Identifier tokens up to the body or the end of the declaration; a
        # constant's or static's value is not its signature.
        sig = []
        j = start
        angle = 0
        while j < n:
            t = toks[j]
            if t == "<":
                angle += 1
            elif t == ">":
                angle -= 1
            if t in ("{", ";") and angle <= 0:
                break
            if t == "=" and angle <= 0 and word in ("const", "static"):
                break
            if rustlex_ident(t):
                sig.append(t)
            j += 1
        return sig

    while i < n:
        t = toks[i]
        if t == "{":
            depth += 1
            if pending is not None:
                stack.append((pending[0], pending[1], depth - 1))
                pending = None
            else:
                stack.append(("block", None, depth - 1))
            i += 1
            continue
        if t == "}":
            depth -= 1
            if stack and stack[-1][2] == depth:
                stack.pop()
            i += 1
            continue
        if t == ";" and pending is not None:
            pending = None
        ctx = inside()
        if t == "mod" and i + 2 < n and gate.IDENT.match(toks[i + 1]) and toks[i + 2] == "{":
            pending = ("mod", toks[i + 1])
        elif t == "impl" and ctx in ("mod",):
            # `impl<..> Trait for Type {` or `impl<..> Type {`: the type is the
            # last path segment before `{` or `where`, after `for` if any.
            j = i + 1
            if j < n and toks[j] == "<":
                j = _skip_group(toks, j, "<", ">") + 1
            seen = []
            trait = None
            while j < n and toks[j] not in ("{", "where"):
                if toks[j] == "for":
                    trait = seen[-1] if seen else None
                    seen = []
                elif toks[j] == "<":
                    j = _skip_group(toks, j, "<", ">")
                elif gate.IDENT.match(toks[j]):
                    seen.append(toks[j])
                j += 1
            pending = ("impl", (seen[-1] if seen else "?") + (f" as {trait}" if trait else ""))
        elif t in ("struct", "enum", "union", "trait") and i + 1 < n and gate.IDENT.match(toks[i + 1]) \
                and ctx == "mod":
            pending = (t, toks[i + 1])
        elif t == "extern" and ctx == "mod" and \
                next((x for x in toks[i + 1: i + 5] if x != '"'), None) == "{":
            # `extern "C" {`: the ABI string is masked to its quotes.
            pending = ("extern", None)
        elif t == "fn" and ctx in ("mod", "impl", "trait", "block"):
            # A body: everything inside is no one's interface.
            j = i + 1
            while j < n and toks[j] not in ("{", ";"):
                if toks[j] == "<":
                    j = _skip_group(toks, j, "<", ">")
                elif toks[j] == "(":
                    j = _skip_group(toks, j, "(", ")")
                j += 1
            if j < n and toks[j] == "{":
                end = _skip_group(toks, j, "{", "}")
                i = end + 1
                continue
        if t == "pub" and (i + 1 < n and toks[i + 1] != "("):
            j = i + 1
            ctx = inside()
            if ctx == "block":
                i += 1
                continue
            while j < n and toks[j] in ("const", "unsafe", "async", "extern", "default") or \
                    (j < n and toks[j - 1] == "extern" and toks[j] not in ITEM_KINDS):
                j += 1
            word = toks[j] if j < n else ""
            here = module_path()
            hid = hidden_before(i)
            if word == "use":
                names, end = _use_names(toks, j + 1)
                for name in names:
                    out.append(Entry(rel, here, name, "re-export", None, line_of(i), [], hid))
                i = end
                continue
            if word in ITEM_KINDS and j + 1 < n:
                name_at = j + 2 if toks[j + 1] == "mut" else j + 1
                name = gate._name(toks[name_at])
                if word == "const" and name == "fn":
                    word, name = "fn", gate._name(toks[name_at + 1])
                    name_at += 1
                kind = word
                if ctx == "impl" and word in ("fn", "const", "type"):
                    kind = "method" if word == "fn" else f"associated {word}"
                elif ctx == "extern":
                    kind = "foreign fn" if word == "fn" else "foreign static"
                sig = signature(name_at + 1, word)
                if word == "enum":
                    # Every variant's fields are as public as the enum.
                    k = name_at + 1
                    while k < n and toks[k] != "{":
                        k += 1
                    end = _skip_group(toks, k, "{", "}")
                    sig += [t for t in toks[k:end] if rustlex_ident(t)]
                out.append(Entry(rel, here, name, kind, container() if ctx != "mod" else None,
                                 line_of(i), sig, hid))
            elif ctx in ("struct", "union", "enum") and gate.IDENT.match(word) and j + 1 < n and toks[j + 1] == ":":
                k = j + 2
                sig = []
                level = 0
                while k < n:
                    if toks[k] in ("<", "(", "["):
                        level += 1
                    elif toks[k] in (">", ")", "]"):
                        level -= 1
                    if level <= 0 and toks[k] in (",", "}"):
                        break
                    if rustlex_ident(toks[k]):
                        sig.append(toks[k])
                    k += 1
                out.append(Entry(rel, here, gate._name(word), "field", container(), line_of(i), sig, hid))
            elif ctx in ("struct", "enum") and toks[i - 1] in ("(", ","):
                out.append(Entry(rel, here, "(tuple field)", "field", container(), line_of(i), [], hid))
        if t == "macro_rules" and i + 2 < n and toks[i + 1] == "!" and i >= 4 \
                and toks[i - 4: i] == ["#", "[", "macro_export", "]"]:
            # Its body's paths, `$crate::console::write`, expand in the crates
            # above: what they name is reached through the macro.
            k = i + 3
            while k < n and toks[k] not in ("{", "(", "["):
                k += 1
            closing = {"{": "}", "(": ")", "[": "]"}.get(toks[k] if k < n else "{", "}")
            end = _skip_group(toks, k, toks[k] if k < n else "{", closing)
            out.append(Entry(rel, (), gate._name(toks[i + 2]), "macro", None, line_of(i),
                             [t for t in toks[k:end] if rustlex_ident(t)], False))
        i += 1
    return out


def rustlex_ident(tok: str) -> bool:
    return bool(gate.IDENT.match(tok))


class Recording(gate.Resolver):
    """The gate's resolver, keeping every (module, name) a path passes."""

    def __init__(self, modules, crate_names):
        super().__init__(modules, crate_names)
        self.seen: set[tuple[tuple, str]] = set()

    def member(self, at, name, origin, value, guard):
        # Every binding of the name, not only the first that resolves: the
        # architecture facade binds one name once per architecture under
        # `cfg`, which this resolver does not read, and each is used on its
        # own architecture.
        mod = self.modules.get(at)
        key = (at, name)
        if mod is not None and key not in guard:
            for path, _public in mod.bindings.get(name, ()):
                # A re-export of another crate's item resolves to nothing in
                # the kernel, and is used all the same.
                self.seen.add(key)
                self.resolve(at, path, True, guard | {key})
        found = super().member(at, name, origin, value, guard)
        if found is not None:
            self.seen.add((at, name))
        return found


def crates_of(manifest, files, read, exists):
    entries = {k: v for k, v in manifest.get("kernel_crates", {}).items() if not k.startswith("//")}
    reached, _ = gate.kernel_crate_files(entries, read, exists, files)
    rank_of = {}
    for package, entry in entries.items():
        rings = entry.get("rings", [])
        rank_of[package] = RANK[rings[0]] if len(rings) == 1 else RANK["bin"]
    ring_name = {v: k for k, v in RANK.items()}
    crate = {}
    for rel, crates in reached.items():
        if len(crates) == 1:
            crate[rel] = ring_name[rank_of[crates[0]]]
    return crate


def analyse(manifest, files, read, exists):
    """(entries of core and item files, crate of each file, problems)."""
    crate = crates_of(manifest, files, read, exists)
    entries: list[Entry] = []
    for rel in files:
        if crate.get(rel) in ("core", "item"):
            entries += parse(rel, read(rel))
    marked = gate.checks_only_items(files, read)
    types_marked = set()
    for e in entries:
        if (e.module, e.name) in marked and e.kind in ITEM_KINDS:
            e.checks_only = True
            if e.kind in TYPE_KINDS:
                types_marked.add(e.name)
        if e.kind in ("method", "foreign fn") and e.hidden:
            e.checks_only = True
    # Who names what, from above.
    modules, refs, _ = gate.build(files, read)
    names = {p.replace("-", "_") for p in manifest.get("kernel_crates", {}) if not p.startswith("//")}
    path_named: dict[tuple, dict[str, set[bool]]] = defaultdict(lambda: defaultdict(set))
    for ref in refs:
        mine = crate.get(ref.file)
        if mine is None:
            continue
        rec = Recording(modules, names)
        rec.resolve(ref.module, ref.segments, ref.is_use)
        for module, name in rec.seen:
            path_named[(module, name)][mine].add(gate.is_test_file(ref.file, manifest))
    token_named: dict[str, dict[str, set[bool]]] = defaultdict(lambda: defaultdict(set))
    for rel in files:
        mine = crate.get(rel)
        if mine is None:
            continue
        test = gate.is_test_file(rel, manifest)
        for tok, _ in rustlex.tokens(rustlex.mask(read(rel))):
            if gate.IDENT.match(tok):
                token_named[tok][mine].add(test)
    for e in entries:
        mine = RANK[crate[e.file]]
        if e.kind in ("method", "field", "macro", "associated const", "associated type", "foreign fn",
                      "foreign static"):
            source = token_named.get(e.name, {})
        elif set(e.module) & ISAS:
            # One architecture's twin of a name the facade binds per
            # architecture: the resolver follows one binding's path to its
            # end, so the name is looked for as a token too.
            source = dict(path_named.get((e.module, e.name), {}))
            for other, tests in token_named.get(e.name, {}).items():
                source[other] = source.get(other, set()) | tests
        else:
            source = path_named.get((e.module, e.name), {})
        for other, tests in source.items():
            if RANK[other] > mine:
                e.named_by.add(other)
                if False in tests:
                    e.named_by_checks_only = False
    problems = []
    # Reached: named above, or in the signature of something reached.
    reached = {id(e) for e in entries if e.named_by}
    by_name = defaultdict(list)
    for e in entries:
        by_name[e.name].append(e)
    changed = True
    while changed:
        changed = False
        for e in entries:
            if id(e) not in reached:
                continue
            for tok in e.signature + ([e.container] if e.container and e.kind == "field" else []):
                for other in by_name.get(tok.split(" ")[0], ()):
                    if (other.kind in TYPE_KINDS or e.kind == "macro") and id(other) not in reached:
                        reached.add(id(other))
                        changed = True
    for e in entries:
        if e.kind in ("re-export",) and e.name == "*":
            continue
        if id(e) not in reached and e.name != "(tuple field)":
            problems.append(f"{e.file}:{e.line}: pub {e.kind} {e.name}"
                            f"{f' of {e.container}' if e.container else ''} is named by no crate above "
                            f"{crate[e.file]}: dead interface (ledger line 1531, D5 (b)); make it pub(crate)")
    # F2.2: a checks-only type reachable from product API.
    for e in entries:
        if e.checks_only:
            continue
        # A field or associated item of a checks-only type is reached only
        # through a value of it (F2.3).
        if e.container and e.container.split(" ")[0] in types_marked:
            continue
        for tok in e.signature:
            if tok in types_marked and tok != e.name:
                problems.append(f"{e.file}:{e.line}: pub {e.kind} {e.name} names checks-only type {tok} "
                                f"in its signature: product code could obtain one (ledger line 1630, F2.2)")
    kernel_traits = {e.name for e in entries if e.kind == "trait"}
    for rel in files:
        if crate.get(rel) not in ("core", "item", "load"):
            continue
        toks = [t for t, _ in rustlex.tokens(rustlex.mask(read(rel)))]
        for i, t in enumerate(toks):
            if t == "impl":
                j = i + 1
                window = []
                while j < len(toks) and toks[j] not in ("{", ";", "where"):
                    window.append(toks[j])
                    j += 1
                if "for" in window:
                    at = window.index("for")
                    trait = [w for w in window[:at] if gate.IDENT.match(w)]
                    target = [w for w in window[at + 1:] if gate.IDENT.match(w)]
                    if trait and trait[-1] in kernel_traits and any(x in types_marked for x in target):
                        problems.append(f"{rel}: impl {trait[-1]} for checks-only "
                                        f"{[x for x in target if x in types_marked][0]}: a kernel trait "
                                        f"would hand product code one (ledger line 1630, F2.2)")
    return entries, crate, problems, types_marked


def clippy_paths(entries, crate):
    out = []
    for e in entries:
        if not e.checks_only or e.kind not in ("method", "foreign fn"):
            continue
        root = "ferrix_kernel_core" if crate[e.file] == "core" else "ferrix_kernel_item"
        parts = [root, *e.module]
        if e.kind == "method":
            parts.append(e.container.split(" ")[0])
        parts.append(e.name)
        out.append(("::".join(parts), e.file))
    return sorted(set(out))


def clippy_block(entries, crate):
    lines = [BEGIN, "disallowed-methods = ["]
    for path, rel in clippy_paths(entries, crate):
        lines.append(f'    {{ path = "{path}", reason = "checks-only, defined in {rel}", allow-invalid = true }},')
    lines += ["]", END]
    return "\n".join(lines)


def render(entries, crate, types_marked) -> str:
    out = [
        "# The kernel's crate interface",
        "",
        "Generated by `tools/common/gen/kernel-interface.py`; `cargo xtask check` fails",
        "when it differs from the tree. Do not edit by hand.",
        "",
        "Every `pub` item of a core or item file of the kernel (docs/KERNEL-CRATES.md):",
        "since the split, the interface the certified item offers what runs above it,",
        "and nothing else, since nothing in the kernel was `pub` before it. *Named by*",
        "is the crates above that name the item (item, load, binary), \"(checks)\" when",
        "every file that does is a check. *Checks-only* items were made `pub` for a",
        "check raised above its crate alone: product code may not name them",
        "(`// CHECKS-ONLY:`, held by check-item-boundary.py) or call them (methods,",
        "`.clippy.toml`'s `disallowed-methods`). A field of a checks-only type is `pub`",
        "for raised checks, and the type is unobtainable from product API: no pub item",
        "that is not checks-only names it (ledger line 1630, F2). A module named only by",
        "checks above is `pub` for raised checks (F3).",
        "",
    ]
    total = defaultdict(int)
    for e in entries:
        total[crate[e.file]] += 1
    out.append(f"{total['core']} declarations in the core's files, {total['item']} in the item's; "
               f"{sum(1 for e in entries if e.checks_only)} checks-only.")
    out.append("")
    groups = defaultdict(list)
    for e in entries:
        groups[(crate[e.file], e.module)].append(e)
    for which in ("core", "item"):
        out += [f"## ferrix-kernel-{which}", ""]
        for module in sorted(m for c, m in groups if c == which):
            rows = sorted(groups[(which, module)], key=lambda e: (e.container or "", e.kind, e.name, e.line))
            out += [f"### `{'::'.join(module) or 'crate root'}`", "",
                    "| Item | Kind | Named by | Note |", "|---|---|---|---|"]
            for e in rows:
                what = f"{e.container.split(' ')[0]}::{e.name}" if e.container else e.name
                named = ", ".join(sorted(e.named_by, key=RANK.get)) or "-"
                if e.named_by and e.named_by_checks_only:
                    named += " (checks)"
                note = []
                if e.checks_only:
                    note.append("checks-only")
                if e.kind == "field" and e.container in types_marked:
                    note.append("pub for raised checks; type unobtainable from product API")
                if e.kind == "mod" and e.named_by and e.named_by_checks_only:
                    note.append("pub for raised checks")
                out.append(f"| `{what}` | {e.kind} | {named} | {'; '.join(note)} |")
            out.append("")
    return "\n".join(out).rstrip("\n") + "\n"


def write_clippy(block: str, check: bool) -> bool:
    text = CLIPPY.read_text(encoding="utf-8")
    start = text.find(BEGIN)
    end = text.find(END)
    if start < 0 or end < 0:
        print(f"kernel-interface: {CLIPPY} has no {BEGIN} ... {END} block", file=sys.stderr)
        return False
    new = text[:start] + block + text[end + len(END):]
    if check:
        return new == text
    CLIPPY.write_text(new, encoding="utf-8")
    return True


# --- self-test ---------------------------------------------------------------

_FIX = {
    "main.rs": "mod stages_check;\nfn main() {}\n",
    "kernel_core.rs": "pub mod mm;\n",
    "kernel_item.rs": "pub use ferrix_kernel_core::*;\npub mod syscall;\n",
    "kernel_load.rs": "pub use ferrix_kernel_item::*;\npub mod fs;\n",
    "mm.rs": "pub fn frame() -> Frame { Frame }\npub struct Frame;\n"
             "// CHECKS-ONLY: fs.rs\n#[doc(hidden)]\npub struct Secret {\n    pub inner: u8,\n}\n"
             "// CHECKS-ONLY: fs.rs\n#[doc(hidden)]\npub fn secret() -> Secret { Secret { inner: 0 } }\n"
             "impl Secret {\n    pub fn again(&self) -> Secret { Secret { inner: self.inner } }\n}\n"
             "pub struct Node;\nimpl Node {\n    #[doc(hidden)]\n    pub fn probe(&self) {}\n}\n",
    "syscall/mod.rs": "pub fn dispatch() { crate::mm::frame(); }\n",
    "fs.rs": "pub fn open() { crate::syscall::dispatch(); let _ = crate::mm::secret().again().inner; "
             "crate::mm::Node.probe(); }\n",
    "stages_check.rs": "fn s() {}\n",
}
_MANIFEST = {
    "rings": {"core": {"members": ["kernel_core.rs", "mm.rs"]},
              "item": {"members": ["kernel_item.rs", "main.rs", "syscall/mod.rs", "stages_check.rs"]},
              "load": {"members": ["kernel_load.rs", "fs.rs"]}},
    "test_file_patterns": ["check.rs", "*_check.rs"],
    "kernel_crates": {
        "ferrix-kernel": {"path": "k", "root": "main.rs", "rings": []},
        "ferrix-kernel-core": {"path": "k/core", "root": "kernel_core.rs", "rings": ["core"]},
        "ferrix-kernel-item": {"path": "k/item", "root": "kernel_item.rs", "rings": ["item"]},
        "ferrix-kernel-load": {"path": "k/load", "root": "kernel_load.rs", "rings": ["load"]},
    },
}
_CASES = [
    ("a product pub fn returning a checks-only type",
     {"mm.rs": _FIX["mm.rs"] + "pub fn leak() -> Secret { secret() }\n",
      "fs.rs": _FIX["fs.rs"] + "fn l() { crate::mm::leak(); }\n"}, "names checks-only type Secret"),
    ("a product pub field of a checks-only type",
     {"mm.rs": _FIX["mm.rs"] + "pub struct Holder {\n    pub held: Secret,\n}\n",
      "fs.rs": _FIX["fs.rs"] + "fn l(_h: crate::mm::Holder) {}\n"}, "names checks-only type Secret"),
    ("a kernel trait implemented for a checks-only type",
     {"mm.rs": _FIX["mm.rs"] + "pub trait Give {}\nimpl Give for Secret {}\n",
      "fs.rs": _FIX["fs.rs"] + "fn l(_g: &dyn crate::mm::Give) {}\n"}, "impl Give for checks-only Secret"),
    ("a pub item no crate above names",
     {"mm.rs": _FIX["mm.rs"] + "pub fn unused() {}\n"}, "pub fn unused is named by no crate above"),
]


def _run_fixture(files):
    added = {e["root"] for e in _MANIFEST["kernel_crates"].values()} - gate.ROOT_FILES
    gate.ROOT_FILES.update(added)
    try:
        return analyse(_MANIFEST, sorted(files), files.__getitem__, files.__contains__)
    finally:
        gate.ROOT_FILES.difference_update(added)


def self_test() -> list[str]:
    failures = []
    entries, crate, problems, types_marked = _run_fixture(dict(_FIX))
    if problems:
        failures.append(f"a clean fixture was refused: {problems}")
    names = {(e.name, e.kind, e.checks_only) for e in entries}
    for want in [("secret", "fn", True), ("Secret", "struct", True), ("probe", "method", True),
                 ("inner", "field", False), ("frame", "fn", False)]:
        if want not in names:
            failures.append(f"the fixture's {want} was not listed: {sorted(names)}")
    if ("ferrix_kernel_core::mm::Node::probe", "mm.rs") not in clippy_paths(entries, crate):
        failures.append(f"the checks-only method is not in the clippy list: {clippy_paths(entries, crate)}")
    for what, change, expect in _CASES:
        _, _, found, _ = _run_fixture(dict(_FIX, **change))
        if not any(expect in p for p in found):
            failures.append(f"{what} passed: {found}")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    failures = self_test()
    if failures:
        for failure in failures:
            print(f"kernel-interface: self-test: {failure}", file=sys.stderr)
        return 1
    if args.self_test:
        print("kernel-interface: self-tests pass")
        return 0
    manifest = gate.load_manifest()
    files = gate.kernel_files()
    read = lambda rel: (gate.KERNEL_SRC / rel).read_text(encoding="utf-8")  # noqa: E731
    entries, crate, problems, types_marked = analyse(manifest, files, read,
                                                     lambda rel: (gate.KERNEL_SRC / rel).is_file())
    text = render(entries, crate, types_marked)
    block = clippy_block(entries, crate)
    status = 0
    for problem in problems:
        print(f"kernel-interface: {problem}", file=sys.stderr)
        status = 1
    if args.check:
        if not DOC.is_file() or DOC.read_text(encoding="utf-8") != text:
            print(f"kernel-interface: {DOC.relative_to(ROOT)} is not what the tree's pub items say; "
                  f"run tools/common/gen/kernel-interface.py", file=sys.stderr)
            status = 1
        if not write_clippy(block, check=True):
            print(f"kernel-interface: .clippy.toml's checks-only methods are not the tree's; "
                  f"run tools/common/gen/kernel-interface.py", file=sys.stderr)
            status = 1
        if status == 0:
            print(f"kernel-interface: {len(entries)} pub declarations, "
                  f"{sum(1 for e in entries if e.checks_only)} checks-only, as listed")
        return status
    DOC.write_text(text, encoding="utf-8")
    write_clippy(block, check=False)
    print(f"kernel-interface: wrote {DOC.relative_to(ROOT)} ({len(entries)} declarations)")
    return status


if __name__ == "__main__":
    sys.exit(main())
