#!/usr/bin/env python3
"""Hold the item's requirements to their parents, their code and their checks.

Findings F-14, F-15 and F-16 of docs/certification/FINDINGS.md: tens of
thousands of lines of in-kernel self-test assert rich properties, and nothing
said which requirement each assertion was evidence *for*. This gate is the
chain docs/certification/IMPLEMENTATION.md W-8 designs, in three levels:

    system      O.*, ASR-*, G.*, P.* SECURITY-TARGET.md 4.1, SAFETY-MANUAL.md 2,
                                  docs/sysml/01-requirements.sysml
    high level  H.<AREA>.<n>      what a subsystem of the item promises at its
                                  interface; docs/sysml/13-item-requirements.sysml
    low level   L.<module>.<n>    what one unit of code does; one model file
                                  per subsystem as they are written

A high- or low-level requirement is a SysML `requirement` typed
`ItemHighLevel` or `ItemLowLevel`, with the id in angle brackets and four
attributes, each a string or a parenthesised list of strings:

    requirement <'H.MEM.4'> writableOrExecutable : ItemHighLevel {
        attribute :>> statement = "No mapping ... shall be both writable and executable.";
        attribute :>> criterion = "Every one of the N mappings swept ...";
        attribute :>> parent = ("O.WXN", "ASR-2");
    }

`unit` (low level only) names the code, `path::function` or
`path::Type::method` relative to src/kernel/src -- `mm::zero_frame`,
`object::quota::Quota::charge` -- and must be the item's product code. A
library crate the manifest's `crates` puts in the `core` or `item` ring is
named the same way from its src/, led by the crate's name:
`ferrix_btrfs::volume::Volume::read_node`. Its product files are the ones
check-item-boundary.py's `item_crate_product_files` lists, and they are
sorted for the "no unintended function" report with the kernel's.

Verification is named where the check is, in a doc line on the function:

    /// Verifies: L.mm.4, L.mm.5
    fn tables_wait_for_their_shootdown() -> Result<(), &'static str> {

(an id may be written in backticks, as clippy's `doc_markdown` asks of one
with an underscore in xtask, which denies it)

on a function in one of three places, and nowhere else:

  * an in-kernel check, in a src/kernel/src file the manifest's
    `test_file_patterns` call verification (check.rs, *_check.rs, ...);
  * a host test under src/lib/, a function carrying `#[test]`;
  * an xtask gate, the function in tools/common/xtask/src that implements it (the one a
    `cargo xtask test-...` subcommand calls).

The gate fails when

  * a `Verifies:` line names an id no requirement defines, names none, is
    not on a function, is on a function outside those three places, or is
    written in any other comment form;
  * a high- or low-level requirement has a malformed or duplicate id, no
    `statement` (or one without *shall*), no `criterion`, no `parent` or a
    parent that does not exist at the level above, or -- at the low level --
    no `unit` or one that does not resolve to a function of the item;
  * a requirement has no verifier and is not in
    tools/common/data/traceability-baseline.json, or the baseline lists one that is
    now verified or no longer exists. A requirement enters the baseline only
    by `--record`, in a diff somebody reviews; verifying one and not
    re-recording fails, so an allowance cannot outlive its reason;
  * docs/certification/TRACEABILITY.md is not what this script writes
    (`--check`);
  * a high- or low-level id that is not defined at the merge-base with
    `main` lies in no range main's tools/common/data/requirement-reservations.json
    held there, or that file has a malformed entry, a range overlapping
    another, or a range holding an id the model defines (a reservation is
    released in the commit that writes its ids; docs/CONVENTIONS.md).

It sorts the item's product functions for DO-178C's "no unintended function"
question: those a low-level requirement names as its unit; *accessors* -- one
statement or one expression, no branch point and no `unsafe` -- whose
behaviour is the requirement of the function they serve, and which need none
of their own (`is_accessor`); check code that still lives in a product file,
listed with a reason in tools/common/data/traceability-units.json; and the rest,
which no requirement names. The last list moves with every function anybody
adds, so it is printed (`--report`), never committed. It fails only for a
subsystem that file lists as `complete`, whose low-level requirements are all
written: there a function no requirement names is a ratchet failure. The gate
also fails on a check-code entry that names no product function, or one a
requirement names.

The run-time half of the chain: a requirement is verified on an architecture
only if its check's lines were executed in that architecture's coverage run.
`tools/common/gen/coverage-report.py` records, per check file, the statements the
suite reached, in the `verification` map of coverage-<arch>.json. The matrix
reads a kernel check as *reached* when any statement of its function was,
*not reached* when none was, *not built* when its file is another
architecture's (src/kernel/src/arch/<isa>/) or the run's line table does not hold
it, and *not measured* when the evidence predates the map. A host test or an
xtask gate is not a per-architecture run and says so.

    python3 tools/common/check/check-traceability.py            # check, and write the matrix
    python3 tools/common/check/check-traceability.py --check    # check, and fail if the matrix is stale
    python3 tools/common/check/check-traceability.py --record   # rewrite the baseline
    python3 tools/common/check/check-traceability.py --report   # also list every unnamed unit
    python3 tools/common/check/check-traceability.py --self-test
"""

from __future__ import annotations

import argparse
import dataclasses
import importlib.util
import json
import os
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(ROOT / "tools" / "common" / "gen"))

import rustlex  # noqa: E402  (after the path insert)
from sysml import load, parse_text  # noqa: E402
from sysml.parser import split_top_level  # noqa: E402

KERNEL_SRC = ROOT / "src" / "kernel" / "src"
LIBS = ROOT / "src" / "lib"
XTASK_SRC = ROOT / "tools" / "common" / "xtask" / "src"
MODEL_DIR = ROOT / "docs" / "sysml"
CERT = ROOT / "docs" / "certification"
OUTPUT = CERT / "TRACEABILITY.md"
BASELINE = ROOT / "tools" / "common" / "data" / "traceability-baseline.json"
UNIT_RULES = ROOT / "tools" / "common" / "data" / "traceability-units.json"
SECURITY_TARGET = CERT / "SECURITY-TARGET.md"
SAFETY_REGISTER = ROOT / "tools" / "common" / "data" / "safety-requirements.json"
RESERVATIONS = ROOT / "tools" / "common" / "data" / "requirement-reservations.json"

ARCHES = ("x86_64", "aarch64", "armv7a")
ARCH_LABEL = {"x86_64": "x86-64", "aarch64": "AArch64", "armv7a": "ARMv7-A"}
# Directories of src/kernel/src built for only some architectures. `#[cfg(target_arch)]`
# is allowed only under arch/ (a house rule a gate enforces), so a file's path
# is what decides which kernels contain it.
ARCH_ONLY = {
    "arch/x86_64/": {"x86_64"},
    "arch/aarch64/": {"aarch64"},
    "arch/armv7a/": {"armv7a"},
    "arch/arm_common/": {"aarch64", "armv7a"},
    "arch/arm_common.rs": {"aarch64", "armv7a"},
}

LEVELS = {"ItemHighLevel": "high", "ItemLowLevel": "low"}
ID_FORM = {
    "high": re.compile(r"H\.[A-Z]+\.[1-9]\d*\Z"),
    "low": re.compile(r"L\.[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)*\.[1-9]\d*\Z"),
}
ANY_ID = re.compile(r"[A-Z][A-Za-z0-9+_-]*(?:\.[A-Za-z0-9_]+)*\Z")
SHALL = re.compile(r"\bshall\b")

VERIFIES = re.compile(r"^[ \t]*///[ \t]*Verifies:(.*)$")
# Any other comment that looks like a tag: `// Verifies:`, `//! Verifies:`,
# `/** Verifies:`. Each is a tag somebody meant and the gate would not read, so
# it is refused rather than ignored. (Prose such as "/// Verifies that ..." has
# no colon and is left alone.)
LOOKS_LIKE_A_TAG = re.compile(r"^[ \t]*(?://+!?|/\*+!?|\*)[ \t]*Verifies[ \t]*:")
FN_LINE = re.compile(
    r"^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?(?:const[ \t]+)?(?:async[ \t]+)?"
    r"(?:unsafe[ \t]+)?(?:extern[ \t]+\"[^\"]*\"[ \t]+)?fn[ \t]+([A-Za-z_][A-Za-z0-9_]*)"
)


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


complexity = _load("complexity", HERE / "check-complexity.py")
boundary = complexity.load_gate()


# --- requirements ------------------------------------------------------------


@dataclasses.dataclass
class Requirement:
    id: str
    level: str
    name: str
    statement: str
    criterion: str
    parents: list[str]
    units: list[str]
    source: str
    line: int
    area: str = ""


def _string(value: str) -> str:
    value = value.strip()
    if len(value) >= 2 and value[0] == '"' and value[-1] == '"':
        return re.sub(r"\\(.)", r"\1", value[1:-1]).strip()
    return value


def _strings(value: str) -> list[str]:
    value = value.strip()
    if value.startswith("(") and value.endswith(")"):
        value = value[1:-1]
    return [_string(part) for part in split_top_level(value) if _string(part)]


def _level_of(element) -> str | None:
    for reference in (element.typed_by, element.specializes):
        if reference:
            level = LEVELS.get(reference.split("::")[-1].split(",")[0].strip())
            if level:
                return level
    return None


def requirements_of(elements) -> tuple[list[Requirement], set[str], list[str]]:
    """The item's requirements among `elements`, every short name, and problems.

    `elements` is every element of a model, walked. The short names returned
    are all of them, the system-level `G.*` included, which is what a
    duplicate is checked against.
    """
    found: list[Requirement] = []
    problems: list[str] = []
    seen: dict[str, str] = {}
    definitions = set()
    for element in elements:
        where = f"{element.source}:{element.line}"
        if element.kind == "requirement" and element.is_definition:
            definitions.add(element.name)
        if element.short_name:
            if element.short_name in seen:
                problems.append(
                    f"{where}: id {element.short_name} is already defined at {seen[element.short_name]}"
                )
            else:
                seen[element.short_name] = where
        if element.kind != "requirement" or element.is_definition:
            continue
        level = _level_of(element)
        if level is None:
            if re.match(r"[HL]\.", element.short_name):
                problems.append(
                    f"{where}: {element.short_name} has an item requirement's id but is not "
                    f"typed ItemHighLevel or ItemLowLevel"
                )
            continue
        requirement = Requirement(
            id=element.short_name,
            level=level,
            name=element.name,
            statement=_string(element.attribute_value("statement")),
            criterion=_string(element.attribute_value("criterion")),
            parents=_strings(element.attribute_value("parent")),
            units=_strings(element.attribute_value("unit")),
            source=element.source,
            line=element.line,
        )
        node = element.parent
        while node is not None and node.kind != "package":
            node = node.parent
        requirement.area = node.name if node is not None else ""
        found.append(requirement)
    if found:
        for needed in LEVELS:
            if needed not in definitions:
                problems.append(f"the model uses {needed} but defines no `requirement def {needed}`")
    return found, set(seen), problems


def system_ids(model_ids: set[str]) -> set[str]:
    """The ids a high-level requirement may name as its parent: the goals
    and the design rules the model defines (G.*, P.*), the security
    objectives and the assumed safety requirements."""
    ids = {short for short in model_ids if re.match(r"[GP](?:\.|\+|\Z)", short)}
    text = SECURITY_TARGET.read_text(encoding="utf-8")
    section = re.search(r"^### 4\.1 .*?$(.*?)^### ", text, re.S | re.M)
    if section:
        ids |= set(re.findall(r"^\|\s*(O\.[A-Z]+)\s*\|", section.group(1), re.M))
    register = json.loads(SAFETY_REGISTER.read_text(encoding="utf-8"))
    ids |= {entry["id"] for entry in register.get("assumed_safety_requirements", [])}
    return ids


def check_requirements(requirements: list[Requirement], parents_allowed: set[str], units) -> list[str]:
    """Every requirement's own fields. `units(unit)` returns a problem or None."""
    problems: list[str] = []
    high = {r.id for r in requirements if r.level == "high"}
    for r in requirements:
        where = f"{r.source}:{r.line}: {r.id or r.name}"
        if not r.id:
            problems.append(f"{where}: no id in angle brackets")
        elif not ID_FORM[r.level].match(r.id):
            form = "H.<AREA>.<n>" if r.level == "high" else "L.<module>.<n>"
            problems.append(f"{where}: a {r.level}-level id is written {form}")
        if not r.statement:
            problems.append(f"{where}: no statement")
        elif not SHALL.search(r.statement):
            problems.append(f"{where}: the statement says no 'shall'")
        if not r.criterion:
            problems.append(f"{where}: no criterion")
        if not r.parents:
            problems.append(f"{where}: no parent")
        allowed = parents_allowed if r.level == "high" else high
        above = "a system-level id (O.*, ASR-*, G.*, P.*)" if r.level == "high" else "a high-level id"
        for parent in r.parents:
            if parent not in allowed:
                problems.append(f"{where}: parent {parent} is not {above} that exists")
        if r.level == "low":
            if not r.units:
                problems.append(f"{where}: no unit")
            for unit in r.units:
                problem = units(unit)
                if problem:
                    problems.append(f"{where}: unit {unit}: {problem}")
        elif r.units:
            problems.append(f"{where}: a unit is named at the low level, not the high")
    return problems


# --- units -------------------------------------------------------------------


def functions_in(source: str) -> list[tuple[str, int, int, str]]:
    """`(name, first line, last line, impl type or "")` for every function."""
    masked = rustlex.mask(source)
    impls: list[tuple[int, int, str]] = []
    # An `impl` block starts a line. `impl` anywhere else is a type in a
    # signature -- `change: impl FnOnce(&mut HandleTable) -> R` -- and taking
    # it for a block gave every function after it the wrong owner.
    for match in re.finditer(r"^[ \t]*(?:unsafe[ \t]+)?impl\b", masked, re.M):
        body = complexity.body_of(masked, match.end())
        if body is None:
            continue
        code, end = body
        header = masked[match.end() : end - len(code) + 1]
        impls.append((match.start(), end, _impl_type(header)))
    found = []
    for match in complexity.FN.finditer(masked):
        body = complexity.body_of(masked, match.end())
        if body is None:
            continue
        _, end = body
        owner = ""
        for start, stop, name in impls:
            if start < match.start() < stop:
                owner = name
        found.append(
            (
                match.group(1),
                masked.count("\n", 0, match.start()) + 1,
                masked.count("\n", 0, end) + 1,
                owner,
            )
        )
    return found


def is_accessor(source: str, first: int) -> bool:
    """Whether the function whose `fn` is on line `first` is an accessor.

    The rule for the "no unintended function" report (IMPLEMENTATION.md W-8,
    the pilot): a function whose body is one statement or one expression, with
    no branch point as `check-complexity.py` counts them (`if`, `match` arms,
    loops, `&&`, `||`, `?`) and no `unsafe`, needs no low-level requirement of
    its own. A getter, a setter, a constructor of a literal, a one-call
    delegate, a `Debug` or `From` impl of that shape: what it does is what the
    requirement of the function it serves or calls says, and a check of that
    requirement runs it. It is counted apart in the report, not silently
    dropped, and a requirement may still name one as its unit.
    """
    masked = rustlex.mask(source)
    offsets = [0]
    for line in masked.split("\n"):
        offsets.append(offsets[-1] + len(line) + 1)
    if first - 1 >= len(offsets):
        return False
    match = complexity.FN.match(masked, offsets[first - 1])
    if match is None:
        return False
    body = complexity.body_of(masked, match.end())
    if body is None:
        return False
    code = body[0][1:-1]
    if complexity.BRANCH.search(code) or re.search(r"(?<![\w])unsafe(?![\w])", code):
        return False
    statements = 0
    depth = 0
    current = ""
    for char in code:
        if char in "([{":
            depth += 1
        elif char in ")]}":
            depth -= 1
        if char == ";" and depth == 0:
            statements += 1 if current.strip() else 0
            current = ""
        else:
            current += char
    statements += 1 if current.strip() else 0
    return statements <= 1


def _impl_type(header: str) -> str:
    """The self type of `impl<T> Trait for Type<T> where ... {`."""
    header = header.split("{")[0]
    header = re.split(r"\bwhere\b", header)[0].strip()
    if header.startswith("<"):
        depth = 0
        for index, char in enumerate(header):
            depth += {"<": 1, ">": -1}.get(char, 0)
            if depth == 0:
                header = header[index + 1 :]
                break
    if re.search(r"\bfor\b", header):
        header = re.split(r"\bfor\b", header)[-1]
    header = header.strip().lstrip("&").strip()
    header = re.sub(r"^(?:mut\s+|dyn\s+|'\w+\s+)+", "", header)
    path = header.split("<")[0].strip()
    return path.split("::")[-1].strip()


def item_crate_modules() -> dict[tuple[str, ...], str]:
    """`{(crate, *module): repository-relative file}` for the product files of
    every `core` and `item` library crate in the manifest: `ferrix-btrfs`'s
    src/volume.rs is `("ferrix_btrfs", "volume")`. Empty for a manifest, or a
    boundary gate, that classifies no crates."""
    listing = getattr(boundary, "item_crate_product_files", None)
    if listing is None:
        return {}
    manifest = boundary.load_manifest()
    entries = manifest.get("crates", {}).get("members", {})
    out: dict[tuple[str, ...], str] = {}
    for package, path in listing(manifest):
        src = f"{entries[package]['path']}/src/"
        name = package.replace("-", "_")
        out[(name,) + boundary.module_of_file(path.removeprefix(src))] = path
    return out


class Units:
    """Resolve `path::function` against src/kernel/src and the item's library
    crates, product code of the item only.

    A kernel file is keyed by its path under src/kernel/src (`mm.rs`), a
    crate's by its path from the repository's root (`src/lib/fs/btrfs/src/
    volume.rs`), which no kernel path can be."""

    def __init__(
        self,
        files: dict[str, str] | None = None,
        product: set[str] | None = None,
        crates: dict[tuple[str, ...], str] | None = None,
    ):
        self._files = files
        self._product = product
        self._crates = crates
        self._functions: dict[str, list] = {}

    def crates(self) -> dict[tuple[str, ...], str]:
        if self._crates is None:
            self._crates = item_crate_modules()
        return self._crates

    def files(self) -> dict[str, str]:
        if self._files is None:
            self._files = {rel: "" for rel in boundary.kernel_files()}
            self._files.update({rel: "" for rel in self.crates().values()})
        return self._files

    def product(self) -> set[str]:
        if self._product is None:
            manifest = boundary.load_manifest()
            kernel = [rel for rel in self.files() if rel not in set(self.crates().values())]
            ring_of, _, _ = boundary.classify(manifest, kernel)
            self._product = {
                rel
                for rel, ring in ring_of.items()
                if ring in ("core", "item") and not boundary.is_test_file(rel, manifest)
            } | set(self.crates().values())
        return self._product

    def source(self, rel: str) -> str:
        text = self.files().get(rel, "")
        if not text:
            base = ROOT if rel in set(self.crates().values()) else KERNEL_SRC
            text = (base / rel).read_text(encoding="utf-8", errors="replace")
            self.files()[rel] = text
        return text

    def functions(self, rel: str):
        if rel not in self._functions:
            self._functions[rel] = functions_in(self.source(rel))
        return self._functions[rel]

    def module_of(self, rel: str) -> tuple[str, ...]:
        """The module path a unit names `rel` by."""
        for module, path in self.crates().items():
            if path == rel:
                return module
        return boundary.module_of_file(rel)

    def module_files(self) -> dict[tuple[str, ...], str]:
        crate_files = set(self.crates().values())
        out = {boundary.module_of_file(rel): rel for rel in self.files() if rel not in crate_files}
        out.update(self.crates())
        return out

    def __call__(self, unit: str) -> str | None:
        segments = [s for s in unit.strip().split("::") if s]
        if segments and segments[0] == "crate":
            segments = segments[1:]
        if len(segments) < 1:
            return "is empty"
        modules = self.module_files()
        for cut in range(len(segments) - 1, -1, -1):
            module = tuple(segments[:cut])
            if module in modules:
                rel, rest = modules[module], segments[cut:]
                break
        else:
            return "names no module of src/kernel/src or of an item crate"
        if rel not in self.product():
            return f"{rel} is not the item's product code"
        if len(rest) == 1:
            owners = [owner for name, _, _, owner in self.functions(rel) if name == rest[0]]
            if "" in owners:
                return None
            if owners:
                return f"{rest[0]} in {rel} is a method of {owners[0]}; write {'::'.join(module + (owners[0], rest[0]))}"
            return f"no function {rest[0]} in {rel}"
        if len(rest) == 2:
            for name, _, _, owner in self.functions(rel):
                if name == rest[1] and owner == rest[0]:
                    return None
            return f"no method {rest[1]} of {rest[0]} in {rel}"
        return f"{'::'.join(rest)} is not a function or a type's method in {rel}"

    def names(self) -> list[str]:
        """Every product function of the item, as a unit would name it."""
        return [unit for unit, _, _ in self.located()]

    def located(self) -> list[tuple[str, str, int]]:
        """Every product function of the item: `(unit, file, fn line)`."""
        out = []
        for rel in sorted(self.product()):
            module = "::".join(self.module_of(rel))
            for name, first, _, owner in self.functions(rel):
                parts = [p for p in (module, owner, name) if p]
                out.append(("::".join(parts), rel, first))
        return out

    def accessor(self, rel: str, first: int) -> bool:
        return is_accessor(self.source(rel), first)


@dataclasses.dataclass
class Coverage:
    """How the item's product functions stand against the low level."""

    named: list[str]
    accessors: list[str]
    check_code: list[str]
    unnamed: list[str]


def read_unit_rules() -> dict:
    if not UNIT_RULES.exists():
        return {"complete": [], "check_code_in_product": {}}
    data = json.loads(UNIT_RULES.read_text(encoding="utf-8"))
    return {
        "complete": list(data.get("complete", [])),
        "check_code_in_product": dict(data.get("check_code_in_product", {})),
    }


def classify_units(located, named: set[str], accessor, rules: dict) -> tuple[Coverage, list[str]]:
    """Sort every product function, and what is wrong with the rules.

    `located` is `Units.located()`, `accessor(rel, line)` the accessor rule,
    `rules` tools/common/data/traceability-units.json. A subsystem listed
    `complete` has every low-level requirement written, so a function of it
    that no requirement names, that is no accessor and that is not listed as
    check code fails: the "no unintended function" report is a ratchet there.
    """
    problems: list[str] = []
    check_code: dict[str, str] = rules["check_code_in_product"]
    coverage = Coverage([], [], [], [])
    seen: set[str] = set()
    for unit, rel, first in located:
        seen.add(unit)
        if unit in named:
            coverage.named.append(unit)
        elif unit in check_code:
            coverage.check_code.append(unit)
        elif accessor(rel, first):
            coverage.accessors.append(unit)
        else:
            coverage.unnamed.append(unit)
    for unit in sorted(check_code):
        if unit not in seen:
            problems.append(
                f"{UNIT_RULES.relative_to(ROOT)} lists {unit} as check code, and the item has no "
                f"such product function: take it out"
            )
        elif unit in named:
            problems.append(f"{unit} is listed as check code and named as a requirement's unit")
        if not str(check_code[unit]).strip():
            problems.append(f"{UNIT_RULES.relative_to(ROOT)}: {unit} is listed with no reason")
    for module in rules["complete"]:
        for unit in coverage.unnamed:
            if unit == module or unit.startswith(module + "::"):
                problems.append(
                    f"{unit} is a function of {module}, whose low-level requirements are complete, "
                    f"and no requirement names it: write one (or add it as a unit of the one it "
                    f"serves), or say why it is check code in {UNIT_RULES.relative_to(ROOT)}"
                )
    return coverage, problems


# --- verifiers ---------------------------------------------------------------


@dataclasses.dataclass
class Verifier:
    kind: str  # "kernel", "host" or "gate"
    file: str  # repository-relative
    function: str
    line: int  # the fn line
    ids: list[str]


def scan_verifiers(source: str, rel: str, kind: str) -> tuple[list[Verifier], list[str]]:
    """Every `/// Verifies:` in one file, and what is wrong with the rest.

    `kind` is "kernel" for a kernel check file, "host" for a file under src/lib/,
    "gate" for tools/common/xtask/src, and "none" for a file where no tag may be.
    """
    found: list[Verifier] = []
    problems: list[str] = []
    lines = source.split("\n")
    offsets = [0]
    for line in lines:
        offsets.append(offsets[-1] + len(line) + 1)
    comments = {start for k, start, _ in rustlex.spans(source) if k == "comment"}

    for index, line in enumerate(lines):
        where = f"{rel}:{index + 1}"
        tag = VERIFIES.match(line)
        start = offsets[index] + (line.index("//") if "//" in line else 0)
        if tag is None:
            if LOOKS_LIKE_A_TAG.match(line) and start in comments:
                problems.append(f"{where}: write a tag as `/// Verifies: <ids>` on the check's function")
            continue
        if start not in comments:
            continue  # inside a string literal
        # An id may be written in backticks, as clippy's `doc_markdown` asks of
        # one with an underscore (`L.x86_64.1`) in a crate that denies it.
        ids = [part.strip().strip("`") for part in tag.group(1).split(",")]
        if not any(ids):
            problems.append(f"{where}: `Verifies:` names no requirement")
            continue
        bad = [i for i in ids if not ANY_ID.match(i)]
        if bad:
            hint = (
                " (a wrapped Verifies tag? write one `/// Verifies:` line per line of ids)"
                if "" in bad
                else ""
            )
            problems.append(f"{where}: not a requirement id: {', '.join(repr(b) for b in bad)}{hint}")
            continue
        attributes: list[str] = []
        cursor = index + 1
        depth = 0
        while cursor < len(lines):
            text = lines[cursor].strip()
            if depth > 0:
                attributes[-1] += text
                depth += text.count("[") - text.count("]")
            elif text.startswith("///"):
                pass
            elif text.startswith("#["):
                attributes.append(text)
                depth = text.count("[") - text.count("]")
            else:
                break
            cursor += 1
        function = FN_LINE.match(lines[cursor]) if cursor < len(lines) else None
        if function is None:
            problems.append(f"{where}: `Verifies:` is not on a function")
            continue
        if kind == "none":
            problems.append(
                f"{where}: `Verifies:` on {function.group(1)}, which is not a check: tags go on a "
                f"kernel check file's function, a host #[test] under src/lib/, or an xtask gate"
            )
            continue
        if kind == "host" and not any(re.sub(r"\s", "", a).startswith("#[test]") for a in attributes):
            problems.append(f"{where}: `Verifies:` on {function.group(1)}, which is not a #[test]")
            continue
        found.append(Verifier(kind, rel, function.group(1), cursor + 1, ids))
    return found, problems


def all_verifiers() -> tuple[list[Verifier], list[str]]:
    manifest = boundary.load_manifest()
    verifiers: list[Verifier] = []
    problems: list[str] = []
    sources: list[tuple[Path, str]] = []
    for rel in boundary.kernel_files():
        kind = "kernel" if boundary.is_test_file(rel, manifest) else "none"
        sources.append((KERNEL_SRC / rel, kind))
    for path in sorted(LIBS.rglob("*.rs")):
        if "target" not in path.relative_to(LIBS).parts:
            sources.append((path, "host"))
    for path in sorted(XTASK_SRC.rglob("*.rs")):
        sources.append((path, "gate"))
    for path, kind in sources:
        source = path.read_text(encoding="utf-8", errors="replace")
        if "Verifies" not in source and "verifies" not in source:
            continue
        found, wrong = scan_verifiers(source, path.relative_to(ROOT).as_posix(), kind)
        verifiers += found
        problems += wrong
    return verifiers, problems


# --- the run-time half -------------------------------------------------------


def parse_lines(spec: str) -> set[int]:
    out: set[int] = set()
    for part in str(spec).split(","):
        part = part.strip()
        if "-" in part:
            low, high = part.split("-")
            out.update(range(int(low), int(high) + 1))
        elif part:
            out.add(int(part))
    return out


def built_for(rel: str) -> set[str]:
    for prefix, arches in ARCH_ONLY.items():
        if rel == prefix or rel.startswith(prefix):
            return arches
    return set(ARCHES)


def reach(verifier: Verifier, arch: str, evidence: dict | None, spans) -> str:
    """How one check fared on one architecture's coverage run."""
    if verifier.kind == "host":
        return "host test"
    if verifier.kind == "gate":
        return "xtask gate"
    rel = verifier.file.removeprefix("src/kernel/src/")
    if arch not in built_for(rel):
        return "not built"
    if evidence is None:
        return "not measured"
    entry = evidence.get(rel)
    if entry is None:
        return "not built"
    reached = parse_lines(entry.get("reached", ""))
    for name, first, last, _ in spans(rel):
        if name == verifier.function and first == verifier.line:
            return "reached" if any(first <= n <= last for n in reached) else "not reached"
    return "not reached"


RANK = ("reached", "not reached", "not measured", "xtask gate", "host test", "not built")


def best(verdicts: list[str]) -> str:
    for verdict in RANK:
        if verdict in verdicts:
            return verdict
    return "—"


def coverage_evidence() -> dict[str, dict | None]:
    out: dict[str, dict | None] = {}
    for arch in ARCHES:
        path = CERT / f"coverage-{arch}.json"
        data = json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}
        out[arch] = data.get("verification")
    return out


# --- the ratchet -------------------------------------------------------------


def ratchet(unverified: set[str], baseline: set[str], defined: set[str]) -> list[str]:
    problems = []
    for rid in sorted(unverified - baseline):
        problems.append(
            f"{rid} has no check naming it and is not in the baseline: tag the check that "
            f"verifies it with `/// Verifies: {rid}`, or --record it as written but unverified"
        )
    for rid in sorted(baseline - unverified):
        why = "is now verified" if rid in defined else "is no longer a requirement"
        problems.append(f"the baseline lists {rid}, which {why}: --record so no allowance is left behind")
    return problems


def read_baseline() -> set[str] | None:
    if not BASELINE.exists():
        return None
    return set(json.loads(BASELINE.read_text(encoding="utf-8"))["unverified"])


def write_baseline(unverified: set[str]) -> None:
    BASELINE.write_text(
        json.dumps(
            {
                "//": [
                    "High- and low-level requirements of the certified item that are",
                    "written and that no check names yet with `/// Verifies:`, as",
                    "tools/common/check/check-traceability.py finds them. A debt register",
                    "for finding F-14, not an allowance: an entry leaves when a check",
                    "is tagged, and the gate fails until it is re-recorded; one enters",
                    "only when a requirement is written unverified, in a reviewed diff.",
                    "The target is an empty list.",
                ],
                "unverified": sorted(unverified, key=_id_key),
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )


def _id_key(rid: str):
    return [int(p) if p.isdigit() else p for p in rid.split(".")]


# --- reservations ------------------------------------------------------------
#
# Unlanded branches each took "the next free id on main" and took the same
# ids three times on 2026-10-01. So a new id is written only inside a range
# reserved on main first, in tools/common/data/requirement-reservations.json,
# and the reservation is released -- its entry deleted or shrunk -- in the
# commit that writes the ids, so it lands with them.
#
# Two refusals follow. A requirement id not defined at the merge-base with
# `main` (new on this branch) must lie in a range that main's reservations
# file held at that merge-base: a branch cannot reserve for itself. And the
# file in the tree may hold no range that overlaps another, or that holds an
# id the tree's model defines -- on main that is an id main already uses; on
# a branch it is one the branch wrote and has not released.


@dataclasses.dataclass
class Reservation:
    text: str  # as written, "L.object.106-112"
    prefix: str  # "L.object"
    low: int
    high: int
    branch: str

    def holds(self, rid: str) -> bool:
        prefix, _, number = rid.rpartition(".")
        return prefix == self.prefix and number.isdigit() and self.low <= int(number) <= self.high


ENTRY_FIELDS = ("ids", "owner", "branch", "date", "purpose")
DATE = re.compile(r"\d{4}-\d{2}-\d{2}\Z")


def parse_reservations(data: dict, where: str) -> tuple[list[Reservation], list[str]]:
    """The ranges of one reservations file, and what is wrong with its entries."""
    found: list[Reservation] = []
    problems: list[str] = []
    entries = data.get("reservations")
    if not isinstance(entries, list):
        return found, [f"{where}: no `reservations` list"]
    for index, entry in enumerate(entries):
        label = f"{where}: reservation {index + 1}"
        if not isinstance(entry, dict):
            problems.append(f"{label}: not an object")
            continue
        branch = str(entry.get("branch", "")).strip()
        if branch:
            label += f" ({branch})"
        for field in ENTRY_FIELDS:
            if not entry.get(field):
                problems.append(f"{label}: no `{field}`")
        if entry.get("date") and not DATE.match(str(entry["date"])):
            problems.append(f"{label}: date {entry['date']!r} is not YYYY-MM-DD")
        ids = entry.get("ids") or []
        for text in ids if isinstance(ids, list) else [ids]:
            prefix, _, numbers = str(text).rpartition(".")
            low, dash, high = numbers.partition("-")
            high = high if dash else low
            level = "high" if prefix.startswith("H.") else "low"
            if not (
                low.isdigit()
                and high.isdigit()
                and ID_FORM[level].match(f"{prefix}.{low}")
                and ID_FORM[level].match(f"{prefix}.{high}")
            ):
                problems.append(f"{label}: {text!r} is not an id or a range like L.object.106-112")
            elif int(low) > int(high):
                problems.append(f"{label}: {text!r} runs backwards")
            else:
                found.append(Reservation(str(text), prefix, int(low), int(high), branch))
    return found, problems


def check_reservations(reserved: list[Reservation], defined: set[str], where: str) -> list[str]:
    """Ranges that overlap each other, or hold an id the model already defines."""
    problems: list[str] = []
    for index, a in enumerate(reserved):
        for b in reserved[index + 1 :]:
            if a.prefix == b.prefix and a.low <= b.high and b.low <= a.high:
                problems.append(
                    f"{where}: {a.text} ({a.branch}) overlaps {b.text} ({b.branch}): one id, one owner"
                )
        taken = sorted((rid for rid in defined if a.holds(rid)), key=_id_key)
        if taken:
            problems.append(
                f"{where}: {a.text} ({a.branch}) holds {', '.join(taken)}, which the model already "
                f"defines: release the reservation -- delete or shrink its entry -- in the commit "
                f"that writes the ids, or reserve a range main does not use"
            )
    return problems


def unreserved(new: dict[str, str], reserved: list[Reservation]) -> list[str]:
    """New ids (id -> where it is written) that no reservation on main holds."""
    return [
        f"{where}: {rid} is new on this branch, and no range in main's "
        f"{RESERVATIONS.relative_to(ROOT).as_posix()} holds it: reserve it there in a "
        f"docs-only landing first, then rebase (docs/CONVENTIONS.md)"
        for rid, where in sorted(new.items(), key=lambda item: _id_key(item[0]))
        if not any(r.holds(rid) for r in reserved)
    ]


def _git(*args: str) -> str | None:
    try:
        done = subprocess.run(
            ["git", "-C", str(ROOT), *args], capture_output=True, text=True, encoding="utf-8"
        )
    except OSError:
        return None
    return done.stdout if done.returncode == 0 else None


def main_bases() -> list[str]:
    """The merge-bases of HEAD with `main`, newest only.

    `FERRIX_MAIN_REF` names the ref when set; otherwise both `main` and
    `origin/main` are asked, and a base that is an ancestor of the other is
    dropped, so a stale one of the two does not count. On `main` itself the
    base is HEAD and nothing is new.
    """
    refs = [os.environ["FERRIX_MAIN_REF"]] if os.environ.get("FERRIX_MAIN_REF") else ["main", "origin/main"]
    bases: set[str] = set()
    for ref in refs:
        if _git("rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}") is None:
            continue
        base = _git("merge-base", "HEAD", ref)
        if base:
            bases.add(base.strip())
    return sorted(
        b for b in bases if not any(c != b and _git("merge-base", "--is-ancestor", b, c) is not None for c in bases)
    )


def ids_at(commit: str) -> set[str]:
    """Every short name the model defines at `commit`."""
    listing = _git("ls-tree", "--name-only", commit, f"{MODEL_DIR.relative_to(ROOT).as_posix()}/") or ""
    ids: set[str] = set()
    for path in listing.splitlines():
        if path.endswith(".sysml"):
            root, _, _ = parse_text(path, _git("show", f"{commit}:{path}") or "")
            ids |= {element.short_name for element in root.walk() if element.short_name}
    return ids


def reservations_at(commit: str) -> list[Reservation]:
    text = _git("show", f"{commit}:{RESERVATIONS.relative_to(ROOT).as_posix()}")
    if text is None:
        return []
    try:
        return parse_reservations(json.loads(text), "main")[0]
    except ValueError:
        return []


def reservation_problems(requirements: list[Requirement], model_ids: set[str]) -> tuple[list[str], str]:
    """Both refusals against the tree and `main`, and a line saying what ran."""
    where = RESERVATIONS.relative_to(ROOT).as_posix()
    if not RESERVATIONS.exists():
        return [f"no {where}: it is committed, with an empty `reservations` list when none is held"], ""
    try:
        data = json.loads(RESERVATIONS.read_text(encoding="utf-8"))
    except ValueError as error:
        return [f"{where}: not JSON: {error}"], ""
    reserved, problems = parse_reservations(data, where)
    problems += check_reservations(reserved, model_ids, where)
    bases = main_bases()
    if not bases:
        return problems, (
            f"{len(reserved)} range(s) reserved; no `main` to compare with (FERRIX_MAIN_REF, main, "
            f"origin/main), so new ids were not checked against them"
        )
    model = MODEL_DIR.relative_to(ROOT).as_posix()
    unchanged = all(_git("diff", "--quiet", base, "--", model) is not None for base in bases) and not _git(
        "ls-files", "--others", "--exclude-standard", "--", model
    )
    on_main = model_ids if unchanged else set().union(*(ids_at(base) for base in bases))
    new = {
        r.id: f"{r.source}:{r.line}"
        for r in requirements
        if r.id and r.id not in on_main and ID_FORM[r.level].match(r.id)
    }
    problems += unreserved(new, [r for base in bases for r in reservations_at(base)])
    return problems, (
        f"{len(reserved)} range(s) reserved; {len(new)} id(s) new against main at "
        f"{', '.join(base[:9] for base in bases)}"
    )


# --- the matrix --------------------------------------------------------------


def _cell(text: str) -> str:
    return text.replace("|", "\\|").replace("\n", " ")


def render(
    requirements: list[Requirement],
    by_id: dict[str, list[Verifier]],
    verdicts: dict[str, dict[str, str]],
    baseline: set[str],
    parents_named: dict[str, list[str]],
    system: set[str],
    evidence: dict[str, dict | None],
    coverage: Coverage | None = None,
    rules: dict | None = None,
) -> str:
    coverage = coverage or Coverage([], [], [], [])
    rules = rules or {"complete": [], "check_code_in_product": {}}
    high = [r for r in requirements if r.level == "high"]
    low = [r for r in requirements if r.level == "low"]
    out: list[str] = []
    w = out.append
    w("# Traceability")
    w("")
    w("<!-- Generated by tools/common/check/check-traceability.py from docs/sysml/, the")
    w("     `/// Verifies:` tags on the checks and docs/certification/coverage-*.json.")
    w("     Do not edit: run the script, which `cargo xtask check` runs with --check. -->")
    w("")
    w(
        "The certified item's requirements in three levels, each naming its parent, "
        "and the checks that name each one. The system level is the Security "
        "Target's objectives (SECURITY-TARGET.md §4.1), the safety manual's assumed "
        "safety requirements (SAFETY-MANUAL.md §2) and the goal "
        "(`docs/sysml/01-requirements.sysml`). The high level (`H.*`) is what a "
        "subsystem of the item promises at its interface "
        "(`docs/sysml/13-item-requirements.sysml`); the low level (`L.*`) is what "
        "one unit of code does. IMPLEMENTATION.md W-8 is the design."
    )
    w("")
    w(
        "A requirement is verified on an architecture when a check names it "
        "(`/// Verifies:` on the check's function) and that check's statements were "
        "executed in the architecture's coverage run. The columns per architecture "
        "say: *reached*, *not reached*, *not built* (the check is another "
        "architecture's), *not measured* (the coverage evidence was written before "
        "it recorded the checks' own statements), or that the verifier is a host "
        "test or an xtask gate rather than a boot. The boot's `FERRIX-BOOT-OK`, "
        "the third link, is what `cargo xtask test-boot` gates on each architecture."
    )
    w("")
    measured = [ARCH_LABEL[a] for a in ARCHES if evidence.get(a) is not None]
    w(
        "Coverage evidence recording the checks: "
        + (", ".join(measured) if measured else "none yet")
        + "."
    )
    w("")
    w("## Summary")
    w("")
    w("| Level | Written | Named by a check | Unverified, in the baseline |")
    w("|---|---:|---:|---:|")
    for label, group in (("High (`H.*`)", high), ("Low (`L.*`)", low)):
        named = sum(1 for r in group if by_id.get(r.id))
        listed = sum(1 for r in group if r.id in baseline)
        w(f"| {label} | {len(group)} | {named} | {listed} |")
    w("")
    units = sorted({u for r in low for u in r.units})
    w(
        f"{len(units)} functions of the item are named as a low-level requirement's "
        "unit. Of the item's product functions, the gate counts those a "
        "requirement names, the *accessors* -- one statement or one expression, "
        "no branch point and no `unsafe`, whose behaviour is the requirement of "
        "the function they serve -- the check code that still lives in product "
        "files (listed below), and the rest, which no requirement names. That "
        "last list changes with every function written, so it is printed by "
        "`--report`, not kept here; in a subsystem whose low-level requirements "
        "are complete it must be empty, and the gate fails otherwise."
    )
    w("")
    w("| Product functions | Count |")
    w("|---|---:|")
    w(f"| Named by a low-level requirement | {len(coverage.named)} |")
    w(f"| Accessors, covered by the requirement they serve | {len(coverage.accessors)} |")
    w(f"| Check code in a product file | {len(coverage.check_code)} |")
    w(f"| Named by none | {len(coverage.unnamed)} |")
    w("")
    complete = rules["complete"]
    w(
        "Subsystems whose low-level requirements are complete: "
        + (", ".join(f"`{m}`" for m in complete) if complete else "none yet")
        + "."
    )
    w("")
    if rules["check_code_in_product"]:
        w("### Check code in product files")
        w("")
        w(
            "Functions that are checks, or serve only checks, and live in a product "
            "file, so that no `Verifies:` tag may go on them and the item's size "
            "counts them. Each is to move into a check file; until it does it is "
            "listed here, in `tools/common/data/traceability-units.json`, and not "
            "reported as a function no requirement names."
        )
        w("")
        w("| Function | Why it is check code |")
        w("|---|---|")
        for unit, why in sorted(rules["check_code_in_product"].items()):
            w(f"| `{unit}` | {_cell(str(why))} |")
        w("")

    w("## From the system level")
    w("")
    w("Each system-level requirement, and the high-level requirements that name it as their parent.")
    w("")
    w("| System | Decomposed into |")
    w("|---|---|")
    for sid in sorted(system, key=_id_key):
        if sid.startswith("G"):
            if sid not in parents_named:
                continue
        children = parents_named.get(sid, [])
        w(f"| {sid} | {', '.join(f'`{c}`' for c in children) if children else '—'} |")
    w("")

    def table(group: list[Requirement], with_unit: bool) -> None:
        head = ["Id", "Statement", "Criterion", "Parent"]
        if with_unit:
            head.append("Unit")
        head += ["Verified by"] + [ARCH_LABEL[a] for a in ARCHES]
        w("| " + " | ".join(head) + " |")
        w("|" + "---|" * len(head))
        for r in group:
            checks = by_id.get(r.id, [])
            named = ", ".join(f"`{v.file}::{v.function}`" for v in checks) or (
                "*baselined*" if r.id in baseline else "—"
            )
            row = [f"`{r.id}`", _cell(r.statement), _cell(r.criterion), ", ".join(r.parents)]
            if with_unit:
                row.append(", ".join(f"`{u}`" for u in r.units))
            row.append(named)
            row += [verdicts[r.id][a] if checks else "—" for a in ARCHES]
            w("| " + " | ".join(row) + " |")
        w("")

    w("## High-level requirements")
    w("")
    if not high:
        w("None written yet.")
        w("")
    areas: dict[str, list[Requirement]] = defaultdict(list)
    for r in high:
        areas[r.area].append(r)
    for area, group in areas.items():
        prefix = group[0].id.split(".")[1] if group[0].id.count(".") >= 2 else ""
        w(f"### {area}" + (f" (`H.{prefix}`)" if prefix else ""))
        w("")
        table(group, with_unit=False)

    w("## Low-level requirements")
    w("")
    if not low:
        w("None written yet: IMPLEMENTATION.md W-8 steps 3 and 4 write them, subsystem by subsystem.")
        w("")
    areas = defaultdict(list)
    for r in low:
        areas[r.area].append(r)
    for area, group in areas.items():
        w(f"### {area}")
        w("")
        table(group, with_unit=True)

    w("## Checks and what they verify")
    w("")
    everyone = sorted({(v.file, v.function, v.kind, tuple(v.ids)) for vs in by_id.values() for v in vs})
    if not everyone:
        w("No check names a requirement yet.")
    else:
        w("| Check | Kind | Verifies |")
        w("|---|---|---|")
        for file, function, kind, ids in everyone:
            w(f"| `{file}::{function}` | {kind} | {', '.join(ids)} |")
    w("")
    return "\n".join(out)


# --- self-test ---------------------------------------------------------------

_MODEL = """
package T {
    requirement def ItemHighLevel;
    requirement def ItemLowLevel;
    package Memory {
        requirement <'H.MEM.1'> good : ItemHighLevel {
            attribute :>> statement = "A frame shall be zeroed; #PF is fine here.";
            attribute :>> criterion = "Every one of the N frames reads zero.";
            attribute :>> parent = ("O.SCRUB", "ASR-5");
        }
        requirement <'H.MEM.2'> noCriterion : ItemHighLevel {
            attribute :>> statement = "It shall.";
            attribute :>> parent = "O.SCRUB";
        }
        requirement <'H.MEM.3'> noShall : ItemHighLevel {
            attribute :>> statement = "It does.";
            attribute :>> criterion = "c";
            attribute :>> parent = "O.NOPE";
        }
        requirement <'H.mem.4'> badId : ItemHighLevel {
            attribute :>> statement = "It shall.";
            attribute :>> criterion = "c";
            attribute :>> parent = "O.SCRUB";
        }
        requirement <'H.MEM.1'> duplicate : ItemHighLevel {
            attribute :>> statement = "It shall.";
            attribute :>> criterion = "c";
            attribute :>> parent = "O.SCRUB";
        }
    }
    package Mm {
        requirement <'L.mm.1'> resolves : ItemLowLevel {
            attribute :>> statement = "zero_frame shall zero.";
            attribute :>> criterion = "c";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("mm::zero_frame", "mm::Frames::take");
        }
        requirement <'L.mm.2'> unresolved : ItemLowLevel {
            attribute :>> statement = "It shall.";
            attribute :>> criterion = "c";
            attribute :>> parent = ("H.MEM.1", "O.SCRUB");
            attribute :>> unit = ("mm::gone", "fs::write", "mm::take", "mm::check::sweep");
        }
        requirement <'L.mm.3'> noUnit : ItemLowLevel {
            attribute :>> statement = "It shall.";
            attribute :>> criterion = "c";
            attribute :>> parent = "H.MEM.1";
        }
        requirement <'H.MEM.9'> notTyped : Other;
    }
}
"""

_MODEL_EXPECT = [
    "id H.MEM.1 is already defined",
    "H.MEM.9 has an item requirement's id but is not typed",
    "H.MEM.2: no criterion",
    "H.MEM.3: the statement says no 'shall'",
    "H.MEM.3: parent O.NOPE is not a system-level id",
    "H.mem.4: a high-level id is written H.<AREA>.<n>",
    "L.mm.2: parent O.SCRUB is not a high-level id",
    "L.mm.2: unit mm::gone: no function gone in mm.rs",
    "L.mm.2: unit fs::write: fs.rs is not the item's product code",
    "L.mm.2: unit mm::take: take in mm.rs is a method of Frames; write mm::Frames::take",
    "L.mm.2: unit mm::check::sweep: mm/check.rs is not the item's product code",
    "L.mm.3: no unit",
]

_KERNEL = {
    "mm.rs": (
        "pub fn zero_frame(f: u64) { let s = \"fn fake() {}\"; }\n"
        "pub struct Frames;\n"
        "impl<'a> Frames {\n    pub(crate) fn take(&self) -> u64 { 0 }\n}\n"
    ),
    "mm/check.rs": "fn sweep() {}\n",
    "fs.rs": "pub fn write() {}\n",
}

_CHECK = """\
/// A check.
///
/// Verifies: H.MEM.1, L.mm.1
#[expect(clippy::too_many_lines, reason = "AUDIT: one scenario, read top to bottom")]
#[cfg_attr(
    test,
    allow(dead_code)
)]
pub(crate) fn frames_read_zero() -> Result<(), &'static str> {
    let doc = "
/// Verifies: H.NOT.1";
    Ok(())
}

/// Verifies: H.MEM.1
struct NotAFunction;

// Verifies: H.MEM.1
fn plain_comment() {}

/// Verifies:
fn names_nothing() {}

/// Verifies: mem one
fn not_an_id() {}
"""

_CHECK_EXPECT_FOUND = [("frames_read_zero", 9, ["H.MEM.1", "L.mm.1"])]
_CHECK_EXPECT_PROBLEMS = [
    "check.rs:15: `Verifies:` is not on a function",
    "check.rs:18: write a tag as `/// Verifies: <ids>` on the check's function",
    "check.rs:21: `Verifies:` names no requirement",
    "check.rs:24: not a requirement id: 'mem one'",
]

_HOST = """\
#[cfg(test)]
mod tests {
    /// Verifies: H.MEM.1
    #[test]
    fn a_test() {}

    /// Verifies: H.MEM.1
    fn a_helper() {}
}
"""


_UNITS = """\
impl Process {
    pub(crate) fn with_handles<R>(&self, change: impl FnOnce(&mut Table) -> R) -> R {
        change(&mut self.handles.lock())
    }
    pub(crate) fn job(&self) -> u32 {
        if self.a { 1 } else { 2 }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        release(self.pid);
    }
}
fn two(a: u32) -> u32 {
    let b = a + 1;
    b * 2
}
fn guarded(a: Option<u32>) -> Option<u32> {
    Some(a? + 1)
}
fn raw(p: *const u8) -> u8 {
    unsafe { *p }
}
fn literal() -> Slot {
    Slot {
        a: [0; 4],
        b: None,
    }
}
"""

# name -> (owner, accessor)
_UNITS_EXPECT = {
    "with_handles": ("Process", True),
    "job": ("Process", False),
    "drop": ("Process", True),
    "two": ("", False),
    "guarded": ("", False),
    "raw": ("", False),
    "literal": ("", True),
}


def self_test() -> list[str]:
    failures = [f"lexer: {f}" for f in rustlex.self_test()]

    parents = system_ids({"G.1", "P.2", "X.3", "PX.4"})
    if not {"G.1", "P.2"} <= parents or {"X.3", "PX.4"} & parents:
        failures.append(f"system ids: goals and design rules only, got {sorted(parents)}")

    for name, first, _, owner in functions_in(_UNITS):
        want = _UNITS_EXPECT.get(name)
        got = (owner, is_accessor(_UNITS, first))
        if want != got:
            failures.append(f"units: {name} read as (owner, accessor) {got}, expected {want}")

    located = [
        ("m::named", "m.rs", 1),
        ("m::getter", "m.rs", 2),
        ("m::check_it", "m.rs", 3),
        ("m::forgotten", "m.rs", 4),
        ("n::elsewhere", "n.rs", 1),
    ]
    rules = {
        "complete": ["m"],
        "check_code_in_product": {"m::check_it": "a boot check", "m::gone": "moved", "m::named": "x"},
    }
    coverage, problems = classify_units(located, {"m::named"}, lambda rel, line: line == 2, rules)
    if (coverage.named, coverage.accessors, coverage.check_code, coverage.unnamed) != (
        ["m::named"],
        ["m::getter"],
        ["m::check_it"],
        ["m::forgotten", "n::elsewhere"],
    ):
        failures.append(f"classify: {coverage}")
    wanted = ["lists m::gone as check code", "m::named is listed as check code", "m::forgotten is a function of m"]
    if len(problems) != 3 or not all(any(w in p for p in problems) for w in wanted):
        failures.append(f"classify: problems {problems}")

    root, _, unparsed = parse_text("t.sysml", _MODEL)
    if unparsed:
        failures.append(f"model: {len(unparsed)} unparsed declarations")
    elements = list(root.walk())
    for element in elements:
        element.source = "t.sysml"
    requirements, ids, problems = requirements_of(elements)
    units = Units(dict(_KERNEL), product={"mm.rs"}, crates={})
    problems += check_requirements(requirements, {"O.SCRUB", "ASR-5"}, units)
    for expected in _MODEL_EXPECT:
        if not any(expected in p for p in problems):
            failures.append(f"model: expected a problem {expected!r}; got {problems}")
    if len(problems) != len(_MODEL_EXPECT):
        failures.append(f"model: {len(problems)} problems, expected {len(_MODEL_EXPECT)}: {problems}")
    good = [r for r in requirements if r.name == "good"]
    if not good or good[0].parents != ["O.SCRUB", "ASR-5"] or "#PF" not in good[0].statement:
        failures.append(f"model: H.MEM.1 read as {good}")
    if [r.area for r in requirements if r.id == "L.mm.1"] != ["Mm"]:
        failures.append("model: L.mm.1's area is not its package")

    crate_file = "src/lib/x/src/volume.rs"
    crated = Units(
        {**_KERNEL, crate_file: "pub struct V;\nimpl V {\n    pub fn read(&self) -> u8 { 0 }\n}\n"},
        product={"mm.rs", crate_file},
        crates={("ferrix_x", "volume"): crate_file},
    )
    got = {
        unit: crated(unit)
        for unit in ("ferrix_x::volume::V::read", "ferrix_x::volume::read", "ferrix_y::volume::V::read")
    }
    if (
        got["ferrix_x::volume::V::read"] is not None
        or "is a method of V" not in str(got["ferrix_x::volume::read"])
        or "names no module" not in str(got["ferrix_y::volume::V::read"])
        or "ferrix_x::volume::V::read" not in crated.names()
        or "mm::zero_frame" not in crated.names()
    ):
        failures.append(f"crate units: {got}, names {crated.names()}")

    found, problems = scan_verifiers(_CHECK, "check.rs", "kernel")
    got = [(v.function, v.line, v.ids) for v in found]
    if got != _CHECK_EXPECT_FOUND:
        failures.append(f"verifiers: found {got}, expected {_CHECK_EXPECT_FOUND}")
    if problems != _CHECK_EXPECT_PROBLEMS:
        failures.append(f"verifiers: problems {problems}, expected {_CHECK_EXPECT_PROBLEMS}")
    found, problems = scan_verifiers(_HOST, "libs/x/src/lib.rs", "host")
    if [v.function for v in found] != ["a_test"] or len(problems) != 1 or "not a #[test]" not in problems[0]:
        failures.append(f"host: found {[v.function for v in found]}, problems {problems}")
    found, problems = scan_verifiers(
        "/// Verifies: `L.x86_64.1`, H.MEM.1\nfn a_gate() {}\n", "tools/common/xtask/src/x.rs", "gate"
    )
    if [v.ids for v in found] != [["L.x86_64.1", "H.MEM.1"]] or problems:
        failures.append(f"backticked id: found {[v.ids for v in found]}, problems {problems}")
    found, problems = scan_verifiers("/// Verifies: H.MEM.1\nfn product() {}\n", "mm.rs", "none")
    if found or len(problems) != 1 or "which is not a check" not in problems[0]:
        failures.append(f"product file: found {found}, problems {problems}")

    problems = ratchet({"H.A.1", "H.A.2"}, {"H.A.1", "H.A.3", "H.A.4"}, {"H.A.1", "H.A.2", "H.A.3"})
    wanted = ["H.A.2 has no check", "H.A.3, which is now verified", "H.A.4, which is no longer"]
    if len(problems) != 3 or not all(any(w in p for p in problems) for w in wanted):
        failures.append(f"ratchet: {problems}")

    spans = functions_in(_CHECK)
    checker = Verifier("kernel", "src/kernel/src/mm/check.rs", "frames_read_zero", 9, ["H.MEM.1"])
    spans_of = lambda rel: spans  # noqa: E731
    cases = [
        ("x86_64", None, "not measured"),
        ("x86_64", {"mm/check.rs": {"reached": "1-3, 11"}}, "reached"),
        ("x86_64", {"mm/check.rs": {"reached": "1-3, 14"}}, "not reached"),
        ("x86_64", {}, "not built"),
    ]
    for arch, evidence, want in cases:
        if reach(checker, arch, evidence, spans_of) != want:
            failures.append(f"reach: {evidence} gave {reach(checker, arch, evidence, spans_of)}, expected {want}")
    armed = Verifier("kernel", "src/kernel/src/arch/x86_64/trap/check.rs", "frames_read_zero", 9, [])
    if reach(armed, "aarch64", {"x": {}}, spans_of) != "not built":
        failures.append("reach: an x86-64 check counted on AArch64")
    if best(["not built", "reached", "not reached"]) != "reached":
        failures.append("best: a reached check does not win")
    if _impl_type("<T: Copy> fmt::Debug for Table<T> where T: Sized ") != "Table":
        failures.append(f"impl type: {_impl_type('<T: Copy> fmt::Debug for Table<T> where T: Sized ')}")

    entry = {"owner": "os-1", "branch": "b1", "date": "2026-10-01", "purpose": "p"}
    reserved, problems = parse_reservations(
        {
            "reservations": [
                {**entry, "ids": ["L.object.106-112", "H.TRAP.17", "L.x86_64.3"]},
                {**entry, "branch": "b2", "ids": ["L.object.110-113", "L.mm.9-4", "X.y.1", "L.mm.1-"]},
                {"ids": ["L.sched.3"], "date": "1 Oct"},
            ]
        },
        "r.json",
    )
    if [(r.prefix, r.low, r.high) for r in reserved] != [
        ("L.object", 106, 112),
        ("H.TRAP", 17, 17),
        ("L.x86_64", 3, 3),
        ("L.object", 110, 113),
        ("L.sched", 3, 3),
    ]:
        failures.append(f"reservations: read {reserved}")
    wanted = [
        "reservation 2 (b2): 'L.mm.9-4' runs backwards",
        "reservation 2 (b2): 'X.y.1' is not an id",
        "reservation 2 (b2): 'L.mm.1-' is not an id",
        "reservation 3: no `owner`",
        "reservation 3: no `branch`",
        "reservation 3: no `purpose`",
        "reservation 3: date '1 Oct' is not YYYY-MM-DD",
    ]
    if len(problems) != len(wanted) or not all(any(w in p for p in problems) for w in wanted):
        failures.append(f"reservations: problems {problems}")
    problems = check_reservations(reserved, {"L.object.105", "L.object.106", "L.objects.107", "H.TRAP.1"}, "r.json")
    wanted = [
        "L.object.106-112 (b1) overlaps L.object.110-113 (b2)",
        "L.object.106-112 (b1) holds L.object.106, which the model already defines",
    ]
    if len(problems) != len(wanted) or not all(any(w in p for p in problems) for w in wanted):
        failures.append(f"reservation overlaps: {problems}")
    problems = unreserved(
        {"L.object.112": "a:1", "L.object.114": "a:2", "H.TRAP.17": "a:3", "H.TRAP.18": "a:4", "L.objects.106": "a:5"},
        reserved,
    )
    wanted = ["a:2: L.object.114 is new", "a:4: H.TRAP.18 is new", "a:5: L.objects.106 is new"]
    if len(problems) != len(wanted) or not all(any(w in p for p in problems) for w in wanted):
        failures.append(f"unreserved ids: {problems}")
    return failures


# --- main --------------------------------------------------------------------


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--check", action="store_true", help="fail if TRACEABILITY.md is stale")
    parser.add_argument("--record", action="store_true", help="rewrite the baseline")
    parser.add_argument("--report", action="store_true", help="list every unnamed unit")
    parser.add_argument("--self-test", action="store_true", help="run the crafted cases only")
    args = parser.parse_args()

    failures = self_test()
    if failures:
        for failure in failures:
            print(f"traceability: self-test: {failure}", file=sys.stderr)
        return 1
    if args.self_test:
        print("traceability: self-test passes (model, units, tags, ratchet, reach, reservations)")
        return 0

    model = load(sorted(MODEL_DIR.glob("*.sysml")), ROOT)
    requirements, model_ids, problems = requirements_of(list(model.walk()))
    system = system_ids(model_ids)
    units = Units()
    problems += check_requirements(requirements, system, units)

    verifiers, wrong = all_verifiers()
    problems += wrong
    defined = {r.id for r in requirements}
    wrong, reserved_line = reservation_problems(requirements, model_ids)
    problems += wrong
    known = defined | model_ids
    by_id: dict[str, list[Verifier]] = defaultdict(list)
    for verifier in verifiers:
        for rid in verifier.ids:
            if rid not in known:
                problems.append(
                    f"{verifier.file}:{verifier.line}: {verifier.function} verifies {rid}, "
                    f"which no requirement defines"
                )
            else:
                by_id[rid].append(verifier)

    unverified = {r.id for r in requirements if not by_id.get(r.id)}
    if args.record:
        if problems:
            for problem in problems:
                print(f"traceability: {problem}", file=sys.stderr)
            print("traceability: not recorded while the register has problems", file=sys.stderr)
            return 1
        write_baseline(unverified)
        print(f"traceability: recorded {len(unverified)} unverified requirement(s)")
        return 0
    baseline = read_baseline()
    if baseline is None:
        problems.append(f"no {BASELINE.relative_to(ROOT)}; run with --record and commit it")
        baseline = set()
    else:
        problems += ratchet(unverified, baseline, defined)

    evidence = coverage_evidence()
    kernel_spans = lambda rel: functions_in(  # noqa: E731
        (KERNEL_SRC / rel).read_text(encoding="utf-8", errors="replace")
    )
    cache: dict[str, list] = {}

    def spans(rel: str):
        if rel not in cache:
            cache[rel] = kernel_spans(rel)
        return cache[rel]

    verdicts = {
        r.id: {a: best([reach(v, a, evidence[a], spans) for v in by_id.get(r.id, [])]) for a in ARCHES}
        for r in requirements
    }
    parents_named: dict[str, list[str]] = defaultdict(list)
    for r in requirements:
        if r.level == "high":
            for parent in r.parents:
                parents_named[parent].append(r.id)
    named = {u for r in requirements if r.level == "low" for u in r.units}
    rules = read_unit_rules()
    coverage, wrong = classify_units(units.located(), named, units.accessor, rules)
    problems += wrong
    text = render(
        requirements, by_id, verdicts, baseline, parents_named, system, evidence, coverage, rules
    )

    status = 0
    if problems:
        for problem in problems:
            print(f"traceability: {problem}", file=sys.stderr)
        status = 1
    if args.check:
        current = OUTPUT.read_text(encoding="utf-8") if OUTPUT.exists() else ""
        if current != text:
            print(
                f"traceability: {OUTPUT.relative_to(ROOT)} is stale; run "
                f"python3 tools/common/check/check-traceability.py",
                file=sys.stderr,
            )
            status = 1
    elif status == 0:
        OUTPUT.write_text(text, encoding="utf-8")

    if args.report:
        for unit in coverage.unnamed:
            print(f"  unnamed  {unit}")
        for unit in coverage.accessors:
            print(f"  accessor {unit}")
    high = sum(1 for r in requirements if r.level == "high")
    low = len(requirements) - high
    everything = (
        len(coverage.named) + len(coverage.accessors) + len(coverage.check_code) + len(coverage.unnamed)
    )
    print(
        f"traceability: {high} high-level and {low} low-level requirement(s), "
        f"{len(requirements) - len(unverified)} named by a check, {len(unverified)} in the baseline; "
        f"{len(verifiers)} tagged check(s)"
    )
    print(
        f"traceability: of {everything} product function(s) of the item, {len(coverage.named)} named "
        f"by a low-level requirement, {len(coverage.accessors)} accessors, "
        f"{len(coverage.check_code)} check code; {len(coverage.unnamed)} named by none "
        f"(failed only in a complete subsystem: {', '.join(rules['complete']) or 'none yet'}; "
        f"--report lists them)"
    )
    if reserved_line:
        print(f"traceability: {reserved_line}")
    return status


if __name__ == "__main__":
    sys.exit(main())
