#!/usr/bin/env python3
"""Three of docs/BUILD-FLAGS.md's checks that read source: where build flags
are read, which boot options are, and the kernel's own table of flags.

1. **Flags are read in the kernel's crates alone** (rule 6 b). A library built
   without a `cfg` takes its other branch without a word, which `--check-cfg`
   cannot see, so no crate under src/lib may name a `ferrix_` cfg at all, and
   no crate outside the kernel's may name a declared flag's cfg
   (`ferrix_<flag>`, `ferrix_profile`, `ferrix_level`), nor read one through
   a build script's `CARGO_CFG_FERRIX_*`. The kernel's crates are the four
   under src/kernel (`kernel_crates` in certification-item.json), and their
   one build script, src/kernel/build.rs, is where the flags become `cfg`s
   for each of them. That they all build with it, and nothing else does, is
   check-item-boundary.py's rule (ledger line 3282, (K1)).

2. **No boot option is read that the registry does not declare** (BF4). Every
   read of the kernel command line in src/kernel and src/boot -- `.option(K)`,
   `.flag(K)`, `option_in(_, K)`, `flag_in(_, K)` -- must name, as a string
   literal, a `const` of one in the same file, or `module::CONST` of the
   reader file named for that module, an option
   tools/common/data/build-flags.toml declares under `boot_options`. A read
   through a local function the check cannot see as a literal must be listed
   under `boot_option_forwarders`, and then each call of that function is a
   read. The library files that read it for the loader or the kernel are
   named in LIBRARY_READERS. A read the check cannot name fails it: it never
   reports an absence it did not establish.

   And the general scan (ledger 2208 (2)): every `"ferrix.` string literal,
   and every literal that is a registry key, in src/lib and src/boot outside
   their tests (tests.rs, tests/, inline #[cfg(test)] modules) is a declared
   option, in a LIBRARY_READERS file, or on NOT_OPTIONS with its reason. It
   cannot see a new option without the `ferrix.` prefix that a library reads.

3. **The kernel's flag table follows the flag file**: `build_flags.rs` names
   the profiles, the levels and every flag with every value, in the flag
   file's order, each `cfg!` filed under its own word. The audit record
   packs places in those lists, and AUDIT.md decodes them by the file. And
   `build_flag_word!` (in the scheduler selection's arms file, which the
   core re-exports) names the same values in the same order: each of the
   kernel's crates expands it, and the binary requires their words equal
   (ledger line 3282, (K4)).

4. **The scheduler's flags are read in the arms file alone**
   (docs/SCHED-CLASSES.md §9a; the consultant's design review of step 3,
   ledger line 20, D6, D7, D9, D10): no `ferrix_sched_*` cfg anywhere in the
   kernel but src/kernel/src/sched_select.rs; there, each declared value of
   each `sched_*` flag has exactly one arm, a `Value` const naming it with
   its place in the flag file's list (or the identity tier alias of an
   `absent` one), or a `compile_error!` naming it when it is not built,
   and no arm names an undeclared value; `build_flags.rs` takes each
   `sched_*` flag from that const. Each profile's `modes` is exactly the
   modes whose tiers it has present, and a profile with a claim is
   `sched_dispatch = "fixed"`.

The first two have a planted negative control (a `ferrix_` cfg in a library; a reader
of an undeclared option), run with `gate.sh control` over `cargo xtask
test-flags`.

    python3 tools/common/check/check-build-flags.py
    python3 tools/common/check/check-build-flags.py --self-test
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import rustlex  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent.parent.parent
FLAGS = ROOT / "tools" / "common" / "data" / "build-flags.toml"
KERNEL = "src/kernel/"
READERS = ("src/kernel", "src/boot")
# Library files that read the command line on the loader's or the kernel's
# behalf, through a function of their own the loader or kernel calls:
# `ferrix_bootinfo::pstore::switch` reads `ferrix.pstore` for the loader. A
# library's own reads are not otherwise scanned (the libraries also hold the
# parsers and their tests), so a new one is named here.
LIBRARY_READERS = ("src/lib/proto/bootinfo/src/pstore.rs",)
QUALIFIED = re.compile(r"(?:[a-z_][a-z0-9_]*::)*([a-z_][a-z0-9_]*)::([A-Z][A-Z0-9_]*)")

# A cfg naming something: `cfg(`, `cfg!(`, `cfg_attr(`, and a build script's
# `rustc-cfg=` / `rustc-check-cfg=` and `CARGO_CFG_` reads, which live in
# strings.
CFG_OPEN = re.compile(r"\bcfg(?:_attr)?\s*!?\s*\(")
FERRIX_IDENT = re.compile(r"\bferrix_[a-z0-9_]+")
IN_STRINGS = re.compile(r"rustc-(?:check-)?cfg=\s*(?:cfg\()?\s*(ferrix_[a-z0-9_]+)|CARGO_CFG_(FERRIX_[A-Z0-9_]+)")

READ = re.compile(
    r"\.(?:option|flag)\s*\(\s*(?P<a>[^()]*?)\s*\)"
    r"|\b(?:option_in|flag_in)\s*\((?:[^(),]|\([^()]*\))*,\s*(?P<b>[^()]*?)\s*\)"
)
STRING = re.compile(r'^"((?:[^"\\]|\\.)*)"$')
CONST = re.compile(r'\bconst\s+([A-Z][A-Z0-9_]*)\s*:\s*&(?:\'static\s+)?str\s*=\s*"((?:[^"\\]|\\.)*)"\s*;')


def comments_blanked(source: str) -> str:
    """The source with comments blanked and literals kept, length kept."""
    out, pos = [], 0
    for kind, start, end in rustlex.spans(source):
        out.append(source[pos:start])
        text = source[start:end]
        out.append(re.sub(r"[^\n]", " ", text) if kind == "comment" else text)
        pos = end
    out.append(source[pos:])
    return "".join(out)


def closing(text: str, open_at: int) -> int:
    depth = 0
    for index in range(open_at, len(text)):
        if text[index] == "(":
            depth += 1
        elif text[index] == ")":
            depth -= 1
            if depth == 0:
                return index
    return len(text)


def cfg_names(source: str) -> list[tuple[int, str]]:
    """Every `ferrix_` name a cfg in `source` names, with its line."""
    code = comments_blanked(source)
    masked = rustlex.mask(source)
    found = []
    for match in CFG_OPEN.finditer(masked):
        start = match.end() - 1
        inside = masked[start : closing(masked, start) + 1]
        for name in FERRIX_IDENT.finditer(inside):
            found.append((masked.count("\n", 0, start) + 1, name.group()))
    for match in IN_STRINGS.finditer(code):
        name = match.group(1) or match.group(2).lower()
        found.append((code.count("\n", 0, match.start()) + 1, name))
    return found


def flag_cfg_problems(rust_files: list[tuple[str, str]], flags: set[str]) -> list[str]:
    """Rule 6 b over (path, source) pairs."""
    declared = {f"ferrix_{flag}" for flag in flags} | {"ferrix_profile", "ferrix_level"}
    problems = []
    for rel, source in rust_files:
        if rel.startswith(KERNEL):
            continue
        for line, name in cfg_names(source):
            if rel.startswith("src/lib/"):
                problems.append(f"{rel}:{line}: a library names the cfg `{name}`; flags are read in the kernel's crates alone")
            elif name in declared or name.startswith("ferrix_mitigations"):
                problems.append(f"{rel}:{line}: names the build flag `{name}` outside the kernel's crates")
    return problems


def option_reads(rel: str, source: str, forwarder: str | None, imported: dict[str, set[str]] | None = None,
                 modules: dict[tuple[str, str], set[str]] | None = None) -> list[tuple[int, str | None, str]]:
    """(line, option or None when it cannot be named, the call) for each read.

    A name that is no `const` of the file is looked up among the libraries'
    `pub const`s (`imported`), when exactly one literal answers to it. A
    path `module::NAME` is looked up among the `const`s of the reader files
    named `module.rs` or `module/mod.rs` (`modules`), when exactly one
    literal answers to it.
    """
    code = comments_blanked(source)
    consts = {name: value for name, value in CONST.findall(code)}
    for name, values in (imported or {}).items():
        if name not in consts and len(values) == 1 and re.search(rf"\b{name}\b", code):
            consts[name] = next(iter(values))
    reads = []
    for match in READ.finditer(code):
        argument = (match.group("a") if match.group("a") is not None else match.group("b")).strip()
        line = code.count("\n", 0, match.start()) + 1
        literal = STRING.match(argument)
        if literal:
            reads.append((line, literal.group(1), match.group()))
        elif argument in consts:
            reads.append((line, consts[argument], match.group()))
        elif (qualified := QUALIFIED.fullmatch(argument)) and len(
            (modules or {}).get((qualified.group(1), qualified.group(2)), set())
        ) == 1:
            reads.append((line, next(iter(modules[(qualified.group(1), qualified.group(2))])), match.group()))
        elif forwarder is not None and re.fullmatch(r"[a-z_][a-z0-9_]*", argument):
            continue  # the forwarder's own body; its calls are the reads
        else:
            reads.append((line, None, match.group()))
    if forwarder is not None:
        for match in re.finditer(rf"(?<![\w.]){re.escape(forwarder)}\s*\(\s*\"((?:[^\"\\]|\\.)*)\"\s*\)", code):
            reads.append((code.count("\n", 0, match.start()) + 1, match.group(1), match.group()))
    return reads


def library_consts(rust_files: list[tuple[str, str]]) -> dict[str, set[str]]:
    """Every `pub const NAME: &str = "..."` in src/lib, by name."""
    found: dict[str, set[str]] = {}
    for rel, source in rust_files:
        if rel.startswith("src/lib/") and "pub const" in source:
            for name, value in CONST.findall(comments_blanked(source)):
                found.setdefault(name, set()).add(value)
    return found


def module_consts(rust_files: list[tuple[str, str]]) -> dict[tuple[str, str], set[str]]:
    """Every `const NAME: &str = "..."` of a reader file, by (its module's
    name, NAME): the file's stem, or its directory's for a `mod.rs`."""
    found: dict[tuple[str, str], set[str]] = {}
    for rel, source in rust_files:
        if not rel.startswith(READERS):
            continue
        parts = rel.removesuffix(".rs").split("/")
        module = parts[-2] if parts[-1] == "mod" else parts[-1]
        for name, value in CONST.findall(comments_blanked(source)):
            found.setdefault((module, name), set()).add(value)
    return found


def option_problems(rust_files: list[tuple[str, str]], registry: dict, forwarders: dict) -> tuple[list[str], dict[str, int]]:
    problems, counts = [], {}
    imported = library_consts(rust_files)
    modules = module_consts(rust_files)
    for rel, source in rust_files:
        if not rel.startswith(READERS) and rel not in LIBRARY_READERS:
            continue
        for line, option, call in option_reads(rel, source, forwarders.get(rel), imported, modules):
            if option is None:
                problems.append(f"{rel}:{line}: `{call}` reads a boot option this check cannot name")
            elif option not in registry:
                problems.append(f"{rel}:{line}: `{call}` reads `{option}`, which build-flags.toml's boot_options does not declare")
            else:
                counts[option] = counts.get(option, 0) + 1
    return problems, counts


# The general scan (the consultant, ledger 2208 (2)): every `"ferrix.`
# literal, and every literal that is a registry key, in a library or loader
# file outside its tests is a declared option, a literal of a file in
# LIBRARY_READERS, or a name below that is no boot option, each with why.
LITERAL_SCAN = ("src/lib/", "src/boot/")
NOT_OPTIONS = {
    "ferrix.slot": "the loader's A/B slot word on the command line it writes, read by ring-3 bootctl from /proc/cmdline; no kernel or loader read (docs/AB-BOOT.md)",
    "ferrix.slot.boot": "as ferrix.slot: the slot booted, written by the loader for bootctl",
    "ferrix.slot.seq": "as ferrix.slot: the boot control record's sequence, written by the loader for bootctl",
    "ferrix.slot.disk": "as ferrix.slot: the disk the slots are on, written by the loader for bootctl",
    "ferrix.slot.sig": "as ferrix.slot: the anchor whose signature let the slot start, or none, written by the loader after its other words (docs/SIGNING.md §10.6)",
    "ferrix.devmgr.update": "drvupdate-proto's service name, not a command-line option",
    "ferrix.devmgr.events": "devmgr-events-proto's abstract socket name, not a command-line option",
    "ferrix.test": "native-abi's test name, not a command-line option",
}
# The bare prefix `"ferrix."` (the Pixel 7 loader keeps every word that
# starts with it) names no option.
FERRIX_LITERAL = re.compile(r"ferrix\.[A-Za-z0-9_][A-Za-z0-9_.-]*")
TEST_MODULE = re.compile(r"#\[cfg\(test\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{")


def test_regions(masked: str) -> list[tuple[int, int]]:
    """The spans of the inline `#[cfg(test)] mod name { ... }` blocks."""
    regions = []
    for match in TEST_MODULE.finditer(masked):
        depth, at = 0, match.end() - 1
        while at < len(masked):
            if masked[at] == "{":
                depth += 1
            elif masked[at] == "}":
                depth -= 1
                if depth == 0:
                    break
            at += 1
        regions.append((match.start(), at + 1))
    return regions


def is_test_file(rel: str) -> bool:
    return "/tests/" in rel or rel.endswith("tests.rs") or "/benches/" in rel or "/examples/" in rel


def option_literals(source: str, registry: dict) -> list[tuple[int, str]]:
    """(line, name) for each `"ferrix.` literal and each literal that is a
    registry key, outside the source's inline test modules."""
    masked = rustlex.mask(source)
    regions = test_regions(masked)
    found = []
    for kind, start, end in rustlex.spans(source):
        if kind != "string" or any(a <= start < b for a, b in regions):
            continue
        text = source[start:end]
        body = text[text.index('"') + 1 : text.rindex('"')]
        named = FERRIX_LITERAL.match(body)
        name = named.group().rstrip(".") if named else (body if body in registry else None)
        if name:
            found.append((source.count("\n", 0, start) + 1, name))
    return found


def literal_problems(rust_files: list[tuple[str, str]], registry: dict) -> list[str]:
    """The general scan: a literal naming an option no registry entry, reader
    file or NOT_OPTIONS reason accounts for."""
    problems = []
    for rel, source in rust_files:
        if not rel.startswith(LITERAL_SCAN) or is_test_file(rel) or rel in LIBRARY_READERS:
            continue
        for line, name in option_literals(source, registry):
            if name not in registry and name not in NOT_OPTIONS:
                problems.append(f"{rel}:{line}: the literal `{name}` is no declared boot option, no reader file's "
                                "and on no NOT_OPTIONS list (ledger 2208 (2))")
    return problems


KERNEL_TABLE = "src/kernel/src/build_flags.rs"
TABLE_ENTRY = re.compile(r'name:\s*"([a-z0-9_]+)"|cfg!\(\s*ferrix_([a-z0-9_]+)\s*=\s*"([a-z0-9_]+)"\s*\)\s*,\s*"([a-z0-9_]+)"'
                         r'|Values::Selected\(\s*ferrix_kernel_sched_select::([A-Z_]+)\s*\)')


def kernel_table_problems(source: str, flag_file: dict, profiles: list[str]) -> list[str]:
    """The kernel's own table (build_flags.rs) names the profile, the level
    and every flag of the flag file with every value, in the file's order,
    each value's `cfg!` matching its word: the audit record's packing is
    places in these lists, which AUDIT.md decodes by the flag file."""
    table: list[tuple[str, list[str]]] = []
    problems = []
    for match in TABLE_ENTRY.finditer(comments_blanked(source)):
        if match.group(1):
            table.append((match.group(1), []))
        elif match.group(5) and table:
            # A scheduler flag: its value is the arms file's (D6), checked there.
            want = list(flag_file.get("flags", {}).get(table[-1][0], {}).get("values", {}))
            if not table[-1][0].startswith("sched_") or match.group(5) != SCHED_CONSTS.get(table[-1][0]):
                problems.append(f"{KERNEL_TABLE}: {table[-1][0]} is taken from the arms file's {match.group(5)}")
            table[-1][1].extend(want)
        elif table:
            flag, value, word = match.group(2), match.group(3), match.group(4)
            if flag != table[-1][0] or value != word:
                problems.append(f"{KERNEL_TABLE}: cfg!(ferrix_{flag} = \"{value}\") is filed as {table[-1][0]}={word}")
            table[-1][1].append(value)
    wanted = [("profile", profiles), ("level", list(flag_file.get("levels", {})))]
    wanted += [(flag, list(entry.get("values", {}))) for flag, entry in flag_file.get("flags", {}).items()]
    if [name for name, _ in table] != [name for name, _ in wanted]:
        problems.append(f"{KERNEL_TABLE}'s flags are {[n for n, _ in table]}, the flag file's {[n for n, _ in wanted]}")
    for (name, values), (_, want) in zip(table, wanted):
        if name != "profile" and values != want:
            problems.append(f"{KERNEL_TABLE}: {name}'s values are {values}, the flag file's {want}")
        if name == "profile" and sorted(values) != sorted(want):
            problems.append(f"{KERNEL_TABLE}: the profiles are {values}, the files' {want}")
    return problems


KERNEL_WORD = "src/kernel/src/sched_select.rs"
WORD_ENTRY = re.compile(r'cfg!\(\s*ferrix_([a-z0-9_]+)\s*=\s*"([a-z0-9_]+)"\s*\)')


def flag_word_problems(source: str, flag_file: dict, profiles: list[str]) -> list[str]:
    """The core's `build_flag_word!` (ledger line 3282, (K4)) names the
    profile, the level and every flag of the flag file with every value, in
    the file's order (the profiles in any), and no more than 64: each of the
    kernel's crates expands it, and the binary requires the words equal, so
    a value it missed is one the crates could differ in unseen."""
    blanked = comments_blanked(source)
    start = blanked.find("macro_rules! build_flag_word")
    if start < 0:
        return [f"{KERNEL_WORD} defines no build_flag_word! macro"]
    end = blanked.find("\n}", start)
    found = [(m.group(1), m.group(2)) for m in WORD_ENTRY.finditer(blanked[start:end])]
    wanted = [("profile", profiles), ("level", list(flag_file.get("levels", {})))]
    wanted += [(flag, list(entry.get("values", {}))) for flag, entry in flag_file.get("flags", {}).items()]
    problems = []
    if len(found) > 64:
        problems.append(f"{KERNEL_WORD}: build_flag_word! names {len(found)} values, more than a word holds")
    names = []
    for flag, _ in found:
        if not names or names[-1] != flag:
            names.append(flag)
    if names != [name for name, _ in wanted]:
        problems.append(f"{KERNEL_WORD}: build_flag_word!'s flags are {names}, the flag file's "
                        f"{[name for name, _ in wanted]}")
    for name, want in wanted:
        values = [value for flag, value in found if flag == name]
        if (sorted(values) != sorted(want)) if name == "profile" else (values != want):
            problems.append(f"{KERNEL_WORD}: build_flag_word!'s {name} values are {values}, the flag file's {want}")
    return problems



# The scheduler's flags (docs/SCHED-CLASSES.md §9a; ledger line 20, D6, D7):
# the arms file is the only file reading a `sched_*` cfg, with one arm a
# declared value. `SCHED_CONSTS` is the const each flag's arm defines.
ARMS = KERNEL_WORD
SCHED_CONSTS = {
    "sched_dispatch": "DISPATCH",
    "sched_windows": "WINDOWS",
    "sched_deadline": "DEADLINE",
    "sched_realtime": "REALTIME",
    "sched_fair": "FAIR",
}
ARM = re.compile(r'#\[cfg\(\s*ferrix_(sched_[a-z0-9_]+)\s*=\s*"([a-z0-9_]+)"\s*\)\]\s*'
                 r'(?:pub\s+const\s+([A-Z_]+)\s*:\s*Value\s*=\s*Value\s*\{\s*name:\s*"([a-z0-9_]+)"\s*,\s*'
                 r'place:\s*(\d+)\s*,\s*present:\s*(true|false)\s*,?\s*\}\s*;'
                 r'|(compile_error!)\(\s*"([^"]*)"\s*\)\s*;'
                 r'|pub\s+type\s+[A-Za-z]+(?:<[A-Z]>)?\s*=\s*[^;]+;)')


# Stack's nesting in the arms file, outermost first, as its tier aliases name
# it; the flag file declares the tier flags in the same order, which
# check-boot-line.py composes the classes line by (ledger line 23 (3)).
TIER_ALIASES = {"Windows": "sched_windows", "Deadline": "sched_deadline", "Realtime": "sched_realtime",
                "Fair": "sched_fair"}
STACK = re.compile(r"pub\s+type\s+Stack\s*=\s*([^;]+);")


def tier_order_problems(source: str, flag_file: dict) -> list[str]:
    """Stack's tiers, outermost first, are the flag file's tier flags in
    their declared order."""
    match = STACK.search(comments_blanked(source))
    if match is None:
        return [f"{ARMS}: no `pub type Stack`"]
    nesting = [TIER_ALIASES.get(alias, alias) for alias in re.findall(r"[A-Za-z_]+", match.group(1))]
    declared = [flag for flag in flag_file.get("flags", {}) if flag.startswith("sched_") and flag != "sched_dispatch"]
    if nesting != declared:
        return [f"{ARMS}: Stack nests {nesting}, and the flag file declares the tiers {declared}; "
                "the classes line is composed in the flag file's order"]
    return []


def sched_cfg_problems(rust_files: list[tuple[str, str]]) -> list[str]:
    """No `ferrix_sched_*` cfg in the kernel outside the arms file (D6)."""
    problems = []
    for rel, source in rust_files:
        if not rel.startswith(KERNEL) or rel == ARMS:
            continue
        for line, name in cfg_names(source):
            if name.startswith("ferrix_sched_"):
                problems.append(f"{rel}:{line}: names `{name}`; only the scheduler selection's arms file "
                                f"({ARMS}) reads a scheduler flag (SCHED-CLASSES.md §9a, D6)")
    return problems


def arms_problems(source: str, flag_file: dict) -> list[str]:
    """Each declared value of each `sched_*` flag has exactly one `Value`
    arm, naming it at its place, or, unbuilt, one `compile_error!` naming
    it; no arm names an undeclared value (D6, D7)."""
    blanked = comments_blanked(source)
    start = blanked.find("macro_rules! build_flag_word")
    if start >= 0:
        end = blanked.find("\n}", start)
        blanked = blanked[:start] + " " * (end - start) + blanked[end:]
    sched = {flag: entry for flag, entry in flag_file.get("flags", {}).items() if flag.startswith("sched_")}
    problems = []
    seen: dict[tuple[str, str], int] = {}
    for match in ARM.finditer(blanked):
        flag, value = match.group(1), match.group(2)
        values = list(sched.get(flag, {}).get("values", {}))
        if flag != "sched_dispatch" and flag not in sched:
            problems.append(f"{ARMS}: an arm for `{flag}`, which the flag file does not declare")
            continue
        if value not in values:
            problems.append(f"{ARMS}: an arm for {flag}={value}, which the flag file does not declare")
            continue
        built = sched[flag]["values"][value].get("built") == "yes"
        if match.group(3):
            seen[(flag, value)] = seen.get((flag, value), 0) + 1
            const, name, place, present = match.group(3), match.group(4), int(match.group(5)), match.group(6)
            if const != SCHED_CONSTS.get(flag) or name != value or place != values.index(value) \
                    or present != ("false" if value == "absent" else "true"):
                problems.append(f"{ARMS}: {flag}={value}'s arm names {const} {{ {name}, {place}, {present} }}, "
                                f"not {SCHED_CONSTS.get(flag)} {{ {value}, {values.index(value)}, "
                                f"{'false' if value == 'absent' else 'true'} }}")
            if not built:
                problems.append(f"{ARMS}: {flag}={value} is not built, and its arm is not a compile_error! (D7)")
        elif match.group(7):
            seen[(flag, value)] = seen.get((flag, value), 0) + 1
            if built:
                problems.append(f"{ARMS}: {flag}={value} is built, and its arm is a compile_error!")
            elif f"{flag}={value}" not in match.group(8):
                problems.append(f"{ARMS}: {flag}={value}'s compile_error! does not name it")
    for flag, entry in sched.items():
        for value in entry.get("values", {}):
            count = seen.get((flag, value), 0)
            if count != 1:
                problems.append(f"{ARMS}: {flag}={value} has {count} arms, not one (D6)")
    return problems


# The modes and their tiers (D9) are read from the kernel's own map,
# `mode_available`'s match in the arms file, so the profile rule and the
# kernel's refusal cannot drift apart (ledger line 21 (2)). An arm is
# `SchedMode::<Mode> => <expression>`, the expression of `<TIER>.present`
# joined by `&&` and `||`.
MODE_ARM = re.compile(r"SchedMode::([A-Za-z]+)\s*=>\s*([^,]+),")
TIER_OF = {const: flag for flag, const in SCHED_CONSTS.items()}


def mode_map(source: str) -> dict[str, tuple[str, set[str]]]:
    """{mode: (a Python expression over `present`, the tiers it names)} from
    the arms file's `mode_available`."""
    blanked = comments_blanked(source)
    start = blanked.find("fn mode_available")
    if start < 0:
        return {}
    body = blanked[start : blanked.find("\n}", start)]
    modes = {}
    for mode, expression in MODE_ARM.findall(body):
        names = set(re.findall(r"\b([A-Z]+)\.present\b", expression))
        python = re.sub(r"\b([A-Z]+)\.present\b", lambda m: repr(TIER_OF.get(m.group(1), m.group(1))) + " in present",
                        expression).replace("&&", " and ").replace("||", " or ")
        if re.fullmatch(r"[\s'a-z_()]*(?:(?:in present|and|or)[\s'a-z_()]*)*", python) is None:
            raise ValueError(f"{ARMS}: mode_available's arm for {mode} is not tiers joined by && and ||: {expression}")
        modes[mode] = (python, {TIER_OF.get(name, name) for name in names})
    return modes


def mode_problems(profiles: dict[str, dict], modes: dict[str, tuple[str, set[str]]]) -> list[str]:
    """Each profile's `modes` is the set of modes whose tiers it has present,
    both ways, never empty, and every tier it has present is one an allowed
    mode uses (D9, S12, ledger line 21 (1)); a profile with a claim is
    `sched_dispatch = "fixed"` (D10)."""
    problems = []
    if not modes:
        return [f"{ARMS}: no mode_available, so the modes cannot be read"]
    for name, profile in sorted(profiles.items()):
        flags = profile.get("flags", {})
        present = {flag for flag, value in flags.items()
                   if flag.startswith("sched_") and flag != "sched_dispatch" and value != "absent"}
        have = {mode for mode, (needs, _) in modes.items() if eval(needs, {}, {"present": present})}  # noqa: S307
        listed = set(profile.get("modes", []))
        if not listed:
            problems.append(f"profiles/{name}.toml: modes is empty or missing; a profile allows a mode at least")
        for mode in sorted(listed - set(modes)):
            problems.append(f"profiles/{name}.toml: modes names {mode}, which is no mode")
        for mode in sorted((listed & set(modes)) - have):
            problems.append(f"profiles/{name}.toml: allows {mode}, whose tiers are absent")
        for mode in sorted(have - listed):
            problems.append(f"profiles/{name}.toml: has {mode}'s tiers present and leaves {mode} out of modes "
                            "(compiled code no mode uses)")
        used = set().union(*(modes[mode][1] for mode in listed & set(modes))) if listed & set(modes) else set()
        for tier in sorted(present - used):
            problems.append(f"profiles/{name}.toml: {tier}={flags[tier]} is present and no mode it allows uses "
                            "it (compiled code no mode uses)")
        if profile.get("level") != "none" and flags.get("sched_dispatch") != "fixed":
            problems.append(f"profiles/{name}.toml: a profile with a claim is sched_dispatch=fixed, "
                            f"not {flags.get('sched_dispatch')} (S10, D10)")
    return problems


# max+'s rule (docs/SCHED-CLASSES.md section 9c; the consultant's design
# review, ledger line 24, E1, E2, E13): every flag and image key names its
# `barest` value, the one that links the least code and leaves the least to
# decide at run time among the values that allow sc4 (the countermeasure
# floor, E11); only that value may allow sc4_plus, which is sc4's targets
# with EAL 7 and a target alone; and a profile at sc4_plus takes the barest
# value of every flag and key, names one literal value for every boot
# option it permits, and claims a target.
TOP = "sc4_plus"
# The flags and keys whose barest value E11 keeps as a countermeasure or its
# check: never pending, so never waived (ledger line 25 (1)).
COUNTERMEASURES = {"mitigations", "checks", "ssbd", "program_barrier", "smt", "slot_signing"}


def barest_problems(flag_file: dict, profiles: dict[str, dict]) -> list[str]:
    problems = []
    # A barest value still waiting on its evidence (E3) allows neither sc4
    # nor sc4_plus yet: it is excused while an sc4_plus profile lists it as
    # pending, and only then.
    awaited = {(e.get("flag"), e.get("value")) for profile in profiles.values() if profile.get("level") == TOP
               for e in profile.get("pending", [])}
    tables = [("flag", name, entry) for name, entry in flag_file.get("flags", {}).items()]
    tables += [("image key", name, entry) for name, entry in flag_file.get("image_keys", {}).items()]
    barest = {}
    for kind, name, entry in tables:
        values = entry.get("values", {})
        chosen = entry.get("barest")
        if chosen not in values:
            problems.append(f"build-flags.toml: {kind} {name} names no barest value among its own (E13)")
            continue
        barest[name] = chosen
        if "sc4" not in values[chosen].get("allows", []) and (name, chosen) not in awaited:
            problems.append(f"build-flags.toml: {kind} {name}'s barest value {chosen} does not allow sc4 (E13)")
        for value, spec in values.items():
            allows = spec.get("allows", [])
            if TOP in allows and "sc4" not in allows:
                problems.append(f"build-flags.toml: {name}={value} allows {TOP} and not sc4, which it covers (E2)")
            if TOP in allows and value != chosen:
                problems.append(f"build-flags.toml: {name}={value} allows {TOP} and is not the barest, {chosen} (E13)")
    for profile_name, profile in sorted(profiles.items()):
        if profile.get("level") != TOP:
            continue
        where = f"profiles/{profile_name}.toml"
        if not str(profile.get("claim", "")).startswith("target"):
            problems.append(f"{where}: a profile at {TOP} claims a target and no more (E1)")
        pending = {(e.get("flag"), e.get("value")) for e in profile.get("pending", [])}
        for flag, value in sorted(pending):
            if flag in COUNTERMEASURES:
                problems.append(f"{where}: pending {flag}={value} would waive a countermeasure's floor (E11)")
        chosen_values = {**profile.get("flags", {}), **profile.get("image", {})}
        for name, value in sorted(chosen_values.items()):
            if name in barest and value != barest[name]:
                problems.append(f"{where}: {name}={value} is not its barest value, {barest[name]} (E13)")
        for kind, name, entry in tables:
            chosen = barest.get(name)
            spec = entry.get("values", {}).get(chosen, {})
            if chosen is not None and TOP not in spec.get("allows", []) and (name, chosen) not in pending:
                problems.append(f"{where}: {name}={chosen} does not allow {TOP} and is not listed as pending (E3)")
        for option, permitted in sorted(profile.get("boot_options", {}).items()):
            if permitted is not True and (not isinstance(permitted, list) or len(permitted) != 1 or "*" in permitted):
                problems.append(f"{where}: boot option {option} names {permitted}, not one literal value (E13)")
    return problems


# The audit record's fields (D8; ledger line 21 (4)): two bits a flag in the
# first word, the scheduler's flags left out; in the second the profile and
# the level four bits each, `sched_dispatch` two, each tier three. A value
# past its field would alias another in the record, so the flag file may not
# outgrow them, nor either word its 32 bits.
def audit_field_problems(flag_file: dict, profiles: list[str]) -> list[str]:
    flags = flag_file.get("flags", {})
    problems = []
    width = {flag: 2 if flag in ("sched_dispatch",) or not flag.startswith("sched_") else 3 for flag in flags}
    for flag, entry in flags.items():
        count = len(entry.get("values", {}))
        if count > 1 << width[flag]:
            problems.append(f"build-flags.toml: {flag} has {count} values, more than its {width[flag]}-bit "
                            "audit field holds")
    for what, count in (("profiles", len(profiles)), ("levels", len(flag_file.get("levels", {})))):
        if count > 16:
            problems.append(f"{what}: {count}, more than the audit record's four bits hold")
    first = sum(width[f] for f in flags if not f.startswith("sched_"))
    second = 8 + sum(width[f] for f in flags if f.startswith("sched_"))
    for which, bits in (("first", first), ("second", second)):
        if bits > 32:
            problems.append(f"the audit record's {which} word needs {bits} bits, more than 32")
    return problems


# Rule 6e (docs/KERNEL-CRATES.md section 6; ledger 3558 (11), 3623 (R3)): a
# removable crate's flag selects code in the composition root alone. Besides
# main.rs, only the flag lists that name every declared value by rule 6 may
# name it: build_flags.rs's table and sched_select.rs's flag word.
MANIFEST = ROOT / "tools" / "common" / "data" / "certification-item.json"
COMPOSITION_ROOT = "src/kernel/src/main.rs"


def removable_flags() -> set[str]:
    import json
    with MANIFEST.open(encoding="utf-8") as handle:
        removable = json.load(handle).get("composition_root", {}).get("removable", {})
    return {entry["flag"] for name, entry in removable.items() if not name.startswith("//") and "flag" in entry}


def composition_root_problems(rust_files: list[tuple[str, str]], flags: set[str]) -> list[str]:
    """Rule 6e over (path, source) pairs."""
    names = {f"ferrix_{flag}" for flag in flags}
    allowed = {COMPOSITION_ROOT, KERNEL_TABLE, KERNEL_WORD}
    problems = []
    for rel, source in rust_files:
        if not rel.startswith(KERNEL) or rel in allowed:
            continue
        for line, name in cfg_names(source):
            if name in names:
                problems.append(f"{rel}:{line}: names `{name}`, a removable crate's flag; only the composition "
                                f"root ({COMPOSITION_ROOT}) selects code by it")
    return problems


def image_key_problems(flag_file: dict, profiles: dict[str, dict]) -> list[str]:
    """Each profile's `image` table against the flag file's `image_keys`
    (ledger 3623 (R17)): every key set, each value declared and allowed at
    the profile's level, and what a value requires among the profile's flags."""
    keys = flag_file.get("image_keys", {})
    problems = []
    for name, profile in sorted(profiles.items()):
        image = profile.get("image", {})
        level = profile.get("level")
        flags = profile.get("flags", {})
        for key in sorted(set(keys) - set(image)):
            problems.append(f"profiles/{name}.toml: image key {key} is not set (rule 4)")
        for key, value in sorted(image.items()):
            declared = keys.get(key, {}).get("values", {})
            if key not in keys:
                problems.append(f"profiles/{name}.toml: image key {key} is not declared in build-flags.toml")
                continue
            if value not in declared:
                problems.append(f"profiles/{name}.toml: image {key}={value} is not a declared value")
                continue
            if level not in declared[value].get("allows", []):
                problems.append(f"profiles/{name}.toml: image {key}={value} does not allow level {level}")
            for need in declared[value].get("requires", []):
                flag, _, wanted = need.partition("=")
                if flags.get(flag) != wanted:
                    problems.append(f"profiles/{name}.toml: image {key}={value} requires {need}, "
                                    f"the profile has {flag}={flags.get(flag)}")
    return problems


def rust_files() -> list[tuple[str, str]]:
    files = []
    for top in ("src", "tools"):
        for path in sorted((ROOT / top).rglob("*.rs")):
            rel = path.relative_to(ROOT).as_posix()
            if "/target/" in f"/{rel}" or rel.startswith("tools/common/xtask/"):
                continue
            try:
                files.append((rel, path.read_text(encoding="utf-8", errors="replace")))
            except OSError:
                continue
    return files


def self_test() -> list[str]:
    failures = []
    files = [
        ("src/lib/x/src/lib.rs", '#[cfg(ferrix_mitigations = "off")]\nfn a() {}\n'),
        ("src/lib/y/src/lib.rs", '// cfg(ferrix_checks = "off") in a comment\nfn b() {}\n'),
        ("src/kernel/src/k.rs", 'const X: bool = cfg!(ferrix_checks = "off");\n'),
        ("src/user/z/build.rs", 'fn main() { println!("cargo::rustc-check-cfg=cfg(ferrix_auth_sabotage)"); }\n'),
        ("src/user/w/src/main.rs", 'fn w() { let _ = std::env::var("CARGO_CFG_FERRIX_CHECKS"); }\n'),
    ]
    problems = flag_cfg_problems(files, {"checks", "mitigations"})
    if not any("src/lib/x" in p for p in problems):
        failures.append("a library's ferrix_ cfg was not found")
    if any("src/lib/y" in p or "src/kernel" in p or "src/user/z" in p for p in problems):
        failures.append(f"a comment, the kernel's own cfg or a non-flag cfg outside src/lib was refused: {problems}")
    if not any("src/user/w" in p for p in problems):
        failures.append("a build script's read of a flag was not found")
    reader = (
        "src/kernel/src/r.rs",
        'const OPTION: &str = "ferrix.checks";\n'
        'fn r(v: &V) { v.option(OPTION); v.flag("nosmp"); option_in(t.bootargs()?, OPTION); v.flag(name); v.option("ferrix.new"); }\n',
    )
    problems, counts = option_problems([reader], {"ferrix.checks": {}, "nosmp": {}}, {})
    if counts != {"ferrix.checks": 2, "nosmp": 1}:
        failures.append(f"declared reads were counted wrong: {counts}")
    if not any("ferrix.new" in p for p in problems) or not any("cannot name" in p for p in problems):
        failures.append(f"an undeclared or unnamed read was not refused: {problems}")
    qualified = [
        ("src/kernel/src/arch/apic.rs", 'pub(crate) const OPTION: &str = "ferrix.apic";\n'),
        ("src/kernel/src/arch/mod.rs", 'fn m(v: &V) { v.option(apic::OPTION); v.option(other::OPTION); }\n'),
        ("src/lib/proto/bootinfo/src/pstore.rs", 'pub const OPTION: &str = "ferrix.pstore";\nfn s(c: &str) { crate::option_in(c, OPTION); }\n'),
    ]
    problems, counts = option_problems(qualified, {"ferrix.apic": {}, "ferrix.pstore": {}}, {})
    if counts != {"ferrix.apic": 1, "ferrix.pstore": 1} or not any("other::OPTION" in p for p in problems):
        failures.append(f"a module's const or a library reader was not followed, or an unknown path passed: {problems} {counts}")
    scanned = [
        ("src/lib/x/src/lib.rs", 'pub const A: &str = "ferrix.newopt";\nconst B: &str = "ferrix.slot.boot";\n'
                                 'const C: &str = "ferrix.pstore=on";\nfn f(w: &str) -> bool { w.starts_with("ferrix.") }\n#[cfg(test)]\nmod tests {\n    const D: &str = "ferrix.intest";\n}\n'),
        ("src/lib/x/src/tests.rs", 'const E: &str = "ferrix.tested";\n'),
    ]
    found = literal_problems(scanned, {"ferrix.pstore": {}})
    if len(found) != 1 or "ferrix.newopt" not in found[0]:
        failures.append(f"the general literal scan missed an undeclared option or refused a declared, listed or test one: {found}")
    forwarded = ("src/kernel/src/f.rs", 'let flag = |name: &str| view.flag(name) || flag_in(args, name);\nif flag("noactlr") {}\n')
    problems, counts = option_problems([forwarded], {"noactlr": {}}, {"src/kernel/src/f.rs": "flag"})
    if problems or counts != {"noactlr": 1}:
        failures.append(f"a forwarder's read was not followed: {problems} {counts}")
    word_file = {"levels": {"none": {}, "sc4": {}}, "flags": {"checks": {"values": {"on": {}, "off": {}}}}}
    word = (
        'macro_rules! build_flag_word {\n    () => { f(&[cfg!(ferrix_profile = "none"), cfg!(ferrix_level = "none"), '
        'cfg!(ferrix_level = "sc4"), cfg!(ferrix_checks = "on"), cfg!(ferrix_checks = "off")]) };\n}\n'
    )
    if flag_word_problems(word, word_file, ["none"]):
        failures.append(f"a complete flag word was refused: {flag_word_problems(word, word_file, ['none'])}")
    for what, broken in [
        ("a missing value", word.replace(', cfg!(ferrix_checks = "off")', "")),
        ("a reordered flag", word.replace('cfg!(ferrix_level = "none"), cfg!(ferrix_level = "sc4"), ', "")
         .replace('cfg!(ferrix_checks = "off")]', 'cfg!(ferrix_checks = "off"), cfg!(ferrix_level = "none"), '
                  'cfg!(ferrix_level = "sc4")]')),
        ("no macro", "fn nothing() {}\n"),
    ]:
        if not flag_word_problems(broken, word_file, ["none"]):
            failures.append(f"the flag word check passed {what}")
    rooted = [
        ("src/kernel/src/main.rs", '#[cfg(ferrix_runtime_settings = "live")]\nconst A: u8 = 0;\n'),
        ("src/kernel/src/build_flags.rs", 'const B: bool = cfg!(ferrix_runtime_settings = "live");\n'),
        ("src/kernel/src/fs/x.rs", '#[cfg(ferrix_runtime_settings = "live")]\nfn x() {}\n'),
        ("src/kernel/src/fs/y.rs", '#[cfg(ferrix_linux_abi = "on")]\nfn y() {}\n'),
    ]
    found = composition_root_problems(rooted, {"runtime_settings"})
    if len(found) != 1 or "fs/x.rs" not in found[0]:
        failures.append(f"rule 6e missed a removable flag's cfg outside the root, or refused the root or another flag: {found}")
    sched_file = {"flags": {"sched_fair": {"values": {"eevdf": {"built": "yes"}, "absent": {"built": "yes"}}},
                            "sched_realtime": {"values": {"fifo_rr": {"built": "no"}, "absent": {"built": "yes"}}}}}
    arms = (
        '#[cfg(ferrix_sched_realtime = "fifo_rr")]\ncompile_error!("sched_realtime=fifo_rr is not built yet");\n'
        '#[cfg(ferrix_sched_realtime = "absent")]\npub const REALTIME: Value = Value { name: "absent", place: 1, present: false };\n'
        '#[cfg(ferrix_sched_realtime = "absent")]\npub type Realtime<L> = L;\n'
        '#[cfg(ferrix_sched_fair = "eevdf")]\npub const FAIR: Value = Value { name: "eevdf", place: 0, present: true };\n'
        '#[cfg(ferrix_sched_fair = "absent")]\npub const FAIR: Value = Value { name: "absent", place: 1, present: false };\n'
    )
    if arms_problems(arms, sched_file):
        failures.append(f"a complete arms file was refused: {arms_problems(arms, sched_file)}")
    for what, broken in [
        ("a declared value with no arm", arms.replace('#[cfg(ferrix_sched_fair = "absent")]\npub const FAIR: Value = '
                                                      'Value { name: "absent", place: 1, present: false };\n', "")),
        ("a value with two arms", arms + '#[cfg(ferrix_sched_fair = "eevdf")]\npub const FAIR: Value = '
                                         'Value { name: "eevdf", place: 0, present: true };\n'),
        ("an unbuilt value given code", arms.replace('compile_error!("sched_realtime=fifo_rr is not built yet");',
                                                     'pub const REALTIME: Value = Value { name: "fifo_rr", place: 0, present: true };')),
        ("a value at the wrong place", arms.replace('name: "eevdf", place: 0', 'name: "eevdf", place: 1')),
        ("an undeclared value", arms + '#[cfg(ferrix_sched_fair = "bore")]\npub const FAIR: Value = '
                                       'Value { name: "bore", place: 2, present: true };\n'),
    ]:
        if not arms_problems(broken, sched_file):
            failures.append(f"the arms check passed {what}")
    tiers_file = {"flags": {"sched_dispatch": {}, "sched_windows": {}, "sched_fair": {}}}
    if tier_order_problems("pub type Stack = Windows<Fair>;\n", tiers_file):
        failures.append("a Stack nested in the flag file's order was refused")
    if not tier_order_problems("pub type Stack = Fair<Windows>;\n", tiers_file):
        failures.append("a Stack nested against the flag file's order passed")
    found = sched_cfg_problems([
        ("src/kernel/src/sched_select.rs", '#[cfg(ferrix_sched_fair = "eevdf")]\nconst A: u8 = 0;\n'),
        ("src/kernel/src/sched/x.rs", 'const B: bool = cfg!(ferrix_sched_fair = "eevdf");\n'),
        ("src/kernel/src/sched/y.rs", 'const C: bool = cfg!(ferrix_checks = "on");\n'),
    ])
    if len(found) != 1 or "sched/x.rs" not in found[0]:
        failures.append(f"the scheduler-flag rule missed a planted read or refused the arms file or another flag: {found}")
    arms_modes = mode_map(
        "pub const fn mode_available(mode: SchedMode) -> bool {\n    match mode {\n"
        "        SchedMode::Throughput => FAIR.present,\n"
        "        SchedMode::SoftRt => REALTIME.present && FAIR.present,\n"
        "        SchedMode::HardRt => DEADLINE.present || WINDOWS.present,\n    }\n}\n")
    if set(arms_modes) != {"Throughput", "SoftRt", "HardRt"} or arms_modes["SoftRt"][1] != {"sched_realtime", "sched_fair"}:
        failures.append(f"mode_available's match was misread: {arms_modes}")
    good_modes = {"p": {"level": "present", "modes": ["Throughput"],
                        "flags": {"sched_dispatch": "fixed", "sched_fair": "eevdf", "sched_windows": "absent"}}}
    if mode_problems(good_modes, arms_modes):
        failures.append(f"a profile whose modes match its tiers was refused: {mode_problems(good_modes, arms_modes)}")
    for what, profile, expect in [
        ("a mode whose tiers are absent", {"level": "none", "modes": ["Throughput", "HardRt"],
                                           "flags": {"sched_dispatch": "fixed", "sched_fair": "eevdf"}}, "tiers are absent"),
        ("a present tier no mode uses", {"level": "none", "modes": ["Throughput"],
                                         "flags": {"sched_dispatch": "fixed", "sched_fair": "eevdf",
                                                   "sched_windows": "arinc"}}, "leaves HardRt"),
        ("a claimed profile that is switchable", {"level": "sc2", "modes": ["Throughput"],
                                                  "flags": {"sched_dispatch": "switchable", "sched_fair": "eevdf"}}, "S10"),
        ("a tier no allowed mode uses", {"level": "none", "modes": [],
                                         "flags": {"sched_dispatch": "fixed", "sched_realtime": "fifo_rr",
                                                   "sched_fair": "absent"}}, "no mode it allows uses"),
        ("an empty modes list", {"level": "none", "modes": [],
                                 "flags": {"sched_dispatch": "fixed", "sched_fair": "absent"}}, "is empty"),
    ]:
        if not any(expect in p for p in mode_problems({"p": profile}, arms_modes)):
            failures.append(f"the modes rule passed {what}")
    top = {"flags": {"f": {"barest": "b", "values": {"a": {"allows": ["sc4", "none"]},
                                                   "b": {"allows": ["sc4_plus", "sc4", "none"]}}}},
           "image_keys": {"k": {"barest": "x", "values": {"x": {"allows": ["sc4_plus", "sc4"]}, "y": {"allows": ["sc4"]}}}}}
    good = {"p": {"level": "sc4_plus", "claim": "target: EAL 7", "flags": {"f": "b"}, "image": {"k": "x"},
                  "boot_options": {"ferrix.checks": ["run"]}}}
    if barest_problems(top, good):
        failures.append(f"a barest profile and flag file were refused: {barest_problems(top, good)}")
    unnamed = json.loads(json.dumps(top))
    del unnamed["flags"]["f"]["barest"]
    wider = json.loads(json.dumps(top))
    wider["flags"]["f"]["values"]["a"]["allows"].insert(0, "sc4_plus")
    above = json.loads(json.dumps(top))
    above["flags"]["f"]["values"]["b"]["allows"] = ["sc4_plus", "none"]
    for what, flag_file, profiles, expect in [
        ("a flag naming no barest value", unnamed, {}, "names no barest"),
        ("a value not the barest allowing sc4_plus", wider, {}, "is not the barest"),
        ("a value allowing sc4_plus and not sc4", above, {}, "(E2)"),
        ("a held claim at sc4_plus", top, {"p": dict(good["p"], claim="EAL 7")}, "(E1)"),
        ("a value of an sc4_plus profile not the barest", top, {"p": dict(good["p"], flags={"f": "a"})}, "not its barest"),
        ("a barest value below sc4 not pending", json.loads(json.dumps(top).replace('["sc4_plus", "sc4", "none"]', '["none"]')),
         {}, "does not allow sc4"),
        ("a countermeasure pending", top, {"p": dict(good["p"], pending=[{"flag": "smt", "value": "off"}])},
         "countermeasure's floor"),
        ("a free boot option", top, {"p": dict(good["p"], boot_options={"console": ["*"]})}, "one literal"),
        ("a barest value not allowing sc4_plus unlisted", json.loads(json.dumps(top).replace('["sc4_plus", "sc4", "none"]', '["sc4", "none"]')),
         good, "not listed as pending"),
    ]:
        if not any(expect in p for p in barest_problems(flag_file, profiles)):
            failures.append(f"max+'s rule passed {what}")
    fields = {"levels": {"none": {}}, "flags": {"a": {"values": {str(n): {} for n in range(4)}},
                                                "sched_fair": {"values": {str(n): {} for n in range(8)}}}}
    if audit_field_problems(fields, ["p"]):
        failures.append(f"flags that fit their audit fields were refused: {audit_field_problems(fields, ['p'])}")
    fields["flags"]["a"]["values"]["4"] = {}
    fields["flags"]["sched_fair"]["values"]["8"] = {}
    if len(audit_field_problems(fields, ["p"])) != 2:
        failures.append("a two-bit flag with five values or a tier with nine passed the audit-field rule")
    wide = {"levels": {}, "flags": {f"f{n}": {"values": {"a": {}, "b": {}}} for n in range(17)}}
    if not any("first word" in p for p in audit_field_problems(wide, [])):
        failures.append("seventeen two-bit flags passed the audit word's 32 bits")
    keys = {"image_keys": {"w": {"values": {"absent": {"allows": ["sc4", "none"]},
                                            "present": {"allows": ["none"], "requires": ["rs=live"]}}}}}
    good = {"p": {"level": "none", "flags": {"rs": "live"}, "image": {"w": "present"}}}
    if image_key_problems(keys, good):
        failures.append(f"an image key that keeps the rule was refused: {image_key_problems(keys, good)}")
    for what, profile, expect in [
        ("present without its required flag", {"level": "none", "flags": {"rs": "fixed"}, "image": {"w": "present"}}, "requires"),
        ("present at a level it does not allow", {"level": "sc4", "flags": {"rs": "live"}, "image": {"w": "present"}}, "does not allow"),
        ("an unset key", {"level": "none", "flags": {}, "image": {}}, "is not set"),
    ]:
        if not any(expect in p for p in image_key_problems(keys, {"p": profile})):
            failures.append(f"the image-key rule passed {what}")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    failures = self_test()
    if failures:
        for failure in failures:
            print(f"build-flags: self-test: {failure}", file=sys.stderr)
        return 1
    if args.self_test:
        print("build-flags: self-tests pass")
        return 0

    with FLAGS.open("rb") as handle:
        flag_file = tomllib.load(handle)
    files = rust_files()
    status = 0
    cfg = flag_cfg_problems(files, set(flag_file.get("flags", {})))
    if cfg:
        print(
            f"build-flags: {len(cfg)} build-flag cfg(s) outside the kernel's crates (BUILD-FLAGS.md rule 6 b):",
            file=sys.stderr,
        )
        for problem in cfg:
            print(f"    {problem}", file=sys.stderr)
        status = 1
    profiles = sorted(path.stem for path in (ROOT / "tools/common/data/profiles").glob("*.toml")) + ["none"]
    table = kernel_table_problems((ROOT / KERNEL_TABLE).read_text(encoding="utf-8"), flag_file, profiles)
    table += flag_word_problems((ROOT / KERNEL_WORD).read_text(encoding="utf-8"), flag_file, profiles)
    if table:
        print("build-flags: the kernel's flag table does not follow the flag file:", file=sys.stderr)
        for problem in table:
            print(f"    {problem}", file=sys.stderr)
        status = 1
    arms_source = (ROOT / ARMS).read_text(encoding="utf-8")
    sched = sched_cfg_problems(files) + arms_problems(arms_source, flag_file)
    sched += tier_order_problems(arms_source, flag_file)
    if sched:
        print("build-flags: a scheduler flag read outside its arms, or the arms do not follow the flag file "
              "(SCHED-CLASSES.md §9a):", file=sys.stderr)
        for problem in sched:
            print(f"    {problem}", file=sys.stderr)
        status = 1
    rooted = composition_root_problems(files, removable_flags())
    if rooted:
        print("build-flags: a removable crate's flag read outside the composition root (rule 6e):", file=sys.stderr)
        for problem in rooted:
            print(f"    {problem}", file=sys.stderr)
        status = 1
    profile_files = {}
    for path in sorted((ROOT / "tools/common/data/profiles").glob("*.toml")):
        with path.open("rb") as handle:
            profile_files[path.stem] = tomllib.load(handle)
    images = image_key_problems(flag_file, profile_files)
    images += mode_problems(profile_files, mode_map((ROOT / ARMS).read_text(encoding="utf-8")))
    images += audit_field_problems(flag_file, profiles)
    images += barest_problems(flag_file, profile_files)
    if images:
        print("build-flags: the profiles' image keys, modes or dispatch do not follow the rules:", file=sys.stderr)
        for problem in images:
            print(f"    {problem}", file=sys.stderr)
        status = 1
    registry = flag_file.get("boot_options", {})
    options, counts = option_problems(files, registry, flag_file.get("boot_option_forwarders", {}))
    if options:
        print(
            f"build-flags: {len(options)} boot-option read(s) the registry does not declare (BF4):",
            file=sys.stderr,
        )
        for problem in options:
            print(f"    {problem}", file=sys.stderr)
        status = 1
    literals = literal_problems(files, registry)
    if literals:
        print(f"build-flags: {len(literals)} option-like literal(s) outside the registry (the general scan):", file=sys.stderr)
        for problem in literals:
            print(f"    {problem}", file=sys.stderr)
        status = 1
    unread = sorted(set(registry) - set(counts))
    if unread:
        print(f"build-flags: declared boot option(s) nothing reads: {', '.join(unread)}", file=sys.stderr)
        status = 1
    if status == 0:
        print(
            f"build-flags: no build-flag cfg outside the kernel's crates; {sum(counts.values())} "
            f"boot-option reads, all of the {len(registry)} declared options; the libraries' and "
            f"loaders' option-like literals all accounted for"
        )
    return status


if __name__ == "__main__":
    sys.exit(main())
