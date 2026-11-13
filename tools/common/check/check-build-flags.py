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

   And over the manifests (rules 1 and 6 b, ledger 9212 M1 and M5;
   docs/microkernel/CORE-WITHOUT-EXTENSIONS.md section 2): a manifest may key
   a dependency table on a flag's cfg and on nothing else of Cargo's. No
   kernel manifest declares `[features]` or an `optional` dependency, their
   count held to certification-item.json's `kernel_cargo_features`; a table
   keyed `cfg(ferrix_<flag> = "<value>")` stands only in a manifest that
   record's `keyed_dependency_tables` names, with a key it lists, naming a
   declared flag and value that xtask gives rustc (`MANIFEST_FLAGS`, whose
   three copies -- xtask's cargo.rs, src/kernel/build.rs and
   check-ferrocene-rust.py -- must agree). Cargo checks no such key itself.

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
   the core's `build_flag_word!` names the same values in the same order:
   each of the kernel's crates expands it, and the binary requires the four
   words equal (ledger line 3282, (K4)).

The first two have a planted negative control (a `ferrix_` cfg in a library; a reader
of an undeclared option), run with `gate.sh control` over `cargo xtask
test-flags`; the manifest rule's is a table keyed on a flag planted in a
src/lib manifest, over `cargo xtask check`.

    python3 tools/common/check/check-build-flags.py
    python3 tools/common/check/check-build-flags.py --self-test
"""

from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import rustlex  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent.parent.parent
FLAGS = ROOT / "tools" / "common" / "data" / "build-flags.toml"
import kernel_roots  # noqa: E402  (docs/microkernel/CRATE-SPLIT.md S0)

# src/kernel/ and every kernel source root outside it (kernel_source_roots).
KERNEL = ("src/kernel/", *(r + "/" for r in kernel_roots.outside("src/kernel")))
READERS = ("src/kernel", "src/boot", *kernel_roots.outside("src/kernel", "src/boot"))
# Library files that read the command line on the loader's or the kernel's
# behalf, through a function of their own the loader or kernel calls:
# `ferrix_bootinfo::pstore::switch` reads `ferrix.pstore` for the loader, and
# `ferrix_eventlog::badframes::choice` reads `ferrix.badframes` for a
# `bad_frames = choice` loader (docs/MACHINE-CHECK-RESTART.md §10). A
# library's own reads are not otherwise scanned (the libraries also hold the
# parsers and their tests), so a new one is named here.
LIBRARY_READERS = ("src/lib/proto/bootinfo/src/pstore.rs", "src/lib/proto/eventlog/src/badframes.rs")
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
            if rel.startswith("src/user/system/native/linuxd/") and name == "ferrix_runtime_settings":
                # linuxd is the second composition root of one source, its crates
                # the kernel's built again (docs/microkernel/M5-LIFT.md §1).
                continue
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
TABLE_ENTRY = re.compile(r'name:\s*"([a-z0-9_]+)"|cfg!\(\s*ferrix_([a-z0-9_]+)\s*=\s*"([a-z0-9_]+)"\s*\)\s*,\s*"([a-z0-9_]+)"')


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


KERNEL_WORD = "src/kernel/src/kernel_core.rs"
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



# Rule 6e (docs/KERNEL-CRATES.md section 6; ledger 3558 (11), 3623 (R3)): a
# removable crate's flag selects code in the composition root alone. Besides
# main.rs, only the flag lists that name every declared value by rule 6 may
# name it: build_flags.rs's table and kernel_core.rs's flag word, as
# lift_core.rs's is under linuxd (docs/microkernel/M5-LIFT.md §1).
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
    allowed = {COMPOSITION_ROOT, KERNEL_TABLE, KERNEL_WORD, "src/kernel/src/lift_core.rs"}
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
        level = profile.get("target_level") or profile.get("level")
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


def settable_problems(registry: dict, profiles: dict[str, dict]) -> list[str]:
    """The boot words a Settings page may set (ledger 5579 (8)): `settable`
    only on a word that is neither test-only nor free-form (`*`, which B5's
    closed list never takes); and `ferrix.badframes`, the bad-frame switch
    (docs/MACHINE-CHECK-RESTART.md §10), permitted only by a profile whose
    image key `bad_frames` is `choice`, since a `persist` loader compiles no
    read of it."""
    problems = []
    for name, entry in sorted(registry.items()):
        if not entry.get("settable"):
            continue
        if entry.get("test_only"):
            problems.append(f"boot option {name} is settable and test-only")
        if "*" in entry.get("values", []):
            problems.append(f"boot option {name} is settable and takes any word (`*`)")
    for name, profile in sorted(profiles.items()):
        permits = profile.get("boot_options", {})
        if "ferrix.badframes" in permits and profile.get("image", {}).get("bad_frames") != "choice":
            problems.append(f"profiles/{name}.toml permits ferrix.badframes, but its bad_frames is not choice")
    return problems


# Rules 1 and 6 b over the manifests (ledger 9212 M1, M5; 9361 K1;
# docs/microkernel/CORE-WITHOUT-EXTENSIONS.md section 2). A manifest may key a
# dependency table on a flag's cfg and on nothing else of Cargo's: no kernel
# manifest declares `[features]` or an `optional` dependency
# (`kernel_cargo_features`), and since a flag a manifest keys on is given to
# rustc for every crate built for a kernel target, only the manifests the
# record names may hold such a table, each with the keys it lists, each key one
# declared flag and value of the flags xtask gives rustc.
DEPENDENCY_TABLES = ("dependencies", "build-dependencies", "dev-dependencies")
KEYED = re.compile(r'^cfg\(\s*ferrix_([a-z0-9_]+)\s*=\s*"([^"]*)"\s*\)$')
# The three places that list the flags given to rustc, which must agree:
# xtask's cargo.rs, the build script that refuses a disagreement, and the
# Ferrocene step, which builds the kernel crates outside xtask.
MANIFEST_FLAG_LISTS = (
    ("tools/common/xtask/src/cargo.rs", re.compile(r"const MANIFEST_FLAGS: \[&str; \d+\] = \[([^\]]*)\]")),
    ("src/kernel/build.rs", re.compile(r"const MANIFEST_FLAGS: \[&str; \d+\] = \[([^\]]*)\]")),
    ("tools/common/check/check-ferrocene-rust.py", re.compile(r"^MANIFEST_FLAGS = \(([^)]*)\)", re.M)),
)


def manifest_flag_lists(read) -> tuple[dict[str, tuple[str, ...]], list[str]]:
    """Each place's list of the flags given to rustc, by file; a list not found is a problem."""
    lists, problems = {}, []
    for rel, pattern in MANIFEST_FLAG_LISTS:
        found = pattern.search(read(rel))
        if found is None:
            problems.append(f"{rel}: no MANIFEST_FLAGS list found")
            continue
        lists[rel] = tuple(re.findall(r'"([a-z0-9_]+)"', found.group(1)))
    if len(set(lists.values())) > 1:
        problems.append("the flags given to rustc differ between " +
                        "; ".join(f"{rel} {list(names)}" for rel, names in lists.items()))
    return lists, problems


def kernel_manifest(rel: str) -> bool:
    return rel.startswith(KERNEL)


def manifest_problems(manifests: list[tuple[str, dict]], reference: dict, flag_file: dict,
                      given: set[str]) -> tuple[list[str], int]:
    """Rules 1 and 6 b over (path, parsed manifest) pairs: (problems, the kernel
    manifests' feature count). `reference` is certification-item.json's
    reference_configuration; `given` the flags xtask gives rustc."""
    record = {rel: keys for rel, keys in reference.get("keyed_dependency_tables", {}).items()
              if not rel.startswith("//")}
    declared = flag_file.get("flags", {})
    problems: list[str] = []
    features = 0
    seen: dict[str, set[str]] = {}
    for rel, cargo in manifests:
        if kernel_manifest(rel):
            if "features" in cargo:
                features += max(1, len(cargo["features"]))
                problems.append(f"{rel}: declares [features]; a kernel manifest declares none, a build flag "
                                "is a --cfg (rule 1, kernel_cargo_features)")
            tables = [(name, cargo.get(name, {})) for name in DEPENDENCY_TABLES]
            for key, target in cargo.get("target", {}).items():
                tables += [(f"target.'{key}'.{name}", target.get(name, {})) for name in DEPENDENCY_TABLES]
            for table, entries in tables:
                for name, spec in entries.items():
                    if isinstance(spec, dict) and spec.get("optional"):
                        features += 1
                        problems.append(f"{rel}: [{table}] {name} is optional, which makes a Cargo feature "
                                        "(rule 1, kernel_cargo_features)")
        for key in cargo.get("target", {}):
            if "ferrix_" not in key:
                continue
            seen.setdefault(rel, set()).add(key)
            if key not in record.get(rel, []):
                problems.append(f"{rel}: a dependency table keyed {key}; only the manifests "
                                "certification-item.json's keyed_dependency_tables names may key a table "
                                "on a build flag, with the keys it lists (rule 6 b)")
            match = KEYED.match(key)
            if match is None:
                problems.append(f"{rel}: the key {key} is not one flag's cfg, cfg(ferrix_<flag> = \"<value>\")")
                continue
            flag, value = match.groups()
            if value not in declared.get(flag, {}).get("values", {}):
                problems.append(f"{rel}: the key {key} names an undeclared flag or value (build-flags.toml)")
            elif flag not in given:
                problems.append(f"{rel}: the key {key} names {flag}, which xtask does not give rustc "
                                f"(MANIFEST_FLAGS: {', '.join(sorted(given))})")
    for rel, keys in sorted(record.items()):
        for key in keys:
            if key not in seen.get(rel, set()):
                problems.append(f"certification-item.json: keyed_dependency_tables names {rel}'s {key}, "
                                "which that manifest does not hold")
    wanted = reference.get("kernel_cargo_features")
    if features != wanted:
        problems.append(f"the kernel's manifests make {features} Cargo feature(s); "
                        f"certification-item.json's kernel_cargo_features is {wanted}")
    return problems, features


def manifests() -> list[tuple[str, dict]]:
    """Every manifest this repository tracks (a component's are its own
    repository's, CONVENTIONS.md), parsed."""
    import subprocess
    listed = subprocess.run(["git", "ls-files", "-z", "--", "Cargo.toml", "*/Cargo.toml"], cwd=ROOT,
                            capture_output=True, check=True).stdout.decode().split("\0")
    found = []
    for rel in sorted(name for name in listed if name):
        with (ROOT / rel).open("rb") as handle:
            found.append((rel, tomllib.load(handle)))
    return found


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
    bad = {"image_keys": {"bad_frames": {"values": {"persist": {"allows": ["sc4", "present", "none"]},
                                                     "choice": {"allows": ["none"]}}}}}
    for what, profile, expect in [
        ("bad_frames=choice at a certified level", {"level": "present", "flags": {}, "image": {"bad_frames": "choice"}}, "does not allow"),
        ("bad_frames unset", {"level": "none", "flags": {}, "image": {}}, "is not set"),
    ]:
        if not any(expect in p for p in image_key_problems(bad, {"p": profile})):
            failures.append(f"the image-key rule passed {what}")
    registry = {"ferrix.badframes": {"values": ["persist", "reset", "off"], "settable": True},
                "t": {"values": ["a"], "settable": True, "test_only": True},
                "w": {"values": ["*"], "settable": True}}
    found = settable_problems(registry, {
        "desktop": {"image": {"bad_frames": "choice"}, "boot_options": {"ferrix.badframes": ["off"]}},
        "present": {"image": {"bad_frames": "persist"}, "boot_options": {"ferrix.badframes": ["off"]}},
    })
    for expect in ["t is settable and test-only", "w is settable and takes any word",
                   "present.toml permits ferrix.badframes"]:
        if not any(expect in p for p in found):
            failures.append(f"the settable rule missed: {expect}")
    if any("desktop" in p for p in found):
        failures.append(f"the settable rule refused desktop's ferrix.badframes: {found}")
    failures += _manifest_self_test()
    return failures


def _manifest_self_test() -> list[str]:
    """Rules 1 and 6 b over manifests (ledger 9212 M1, M5)."""
    failures = []
    flag_file = {"flags": {"personality": {"values": {"user": {}, "kernel": {}, "none": {}}},
                           "linux_abi": {"values": {"on": {}, "off": {}}},
                           "checks": {"values": {"on": {}, "off": {}}}}}
    key = 'cfg(ferrix_personality = "kernel")'
    reference = {"kernel_cargo_features": 0,
                 "keyed_dependency_tables": {"//": ["why"], "src/kernel/Cargo.toml": [key]}}
    given = {"personality", "linux_abi"}
    binary = ("src/kernel/Cargo.toml", {"dependencies": {"a": {}}, "target": {key: {"dependencies": {"b": {}}}}})
    plain = [binary,
             ("src/lib/x/Cargo.toml", {"target": {'cfg(target_arch = "x86_64")': {"dependencies": {"c": {}}}}}),
             ("src/user/y/Cargo.toml", {"features": {"default": []}, "dependencies": {"d": {"optional": True}}})]
    found, count = manifest_problems(plain, reference, flag_file, given)
    if found or count:
        failures.append(f"manifests: a tree that keeps rules 1 and 6 b was refused: {found}")
    for what, extra, expect in [
        ("[features] in a kernel manifest", ("src/kernel/item/Cargo.toml", {"features": {"f": []}}),
         "declares [features]"),
        ("an optional dependency in a kernel manifest",
         ("src/kernel/core/Cargo.toml", {"target": {"cfg(x)": {"dependencies": {"e": {"optional": True}}}}}),
         "is optional"),
        ("a keyed table in a library's manifest",
         ("src/lib/x/Cargo.toml", {"target": {'cfg(ferrix_linux_abi = "on")': {"dependencies": {}}}}),
         "only the manifests"),
        ("a keyed table in an unlisted kernel manifest",
         ("src/kernel/core/Cargo.toml", {"target": {'cfg(ferrix_linux_abi = "on")': {"dependencies": {}}}}),
         "only the manifests"),
        ("a key naming an undeclared value",
         ("src/lib/x/Cargo.toml", {"target": {'cfg(ferrix_linux_abi = "maybe")': {"dependencies": {}}}}),
         "undeclared flag or value"),
        ("a key naming a flag xtask does not give rustc",
         ("src/lib/x/Cargo.toml", {"target": {'cfg(ferrix_checks = "on")': {"dependencies": {}}}}),
         "does not give rustc"),
        ("a key that is not one flag's cfg",
         ("src/lib/x/Cargo.toml", {"target": {'cfg(any(ferrix_checks = "on", unix))': {"dependencies": {}}}}),
         "is not one flag's cfg"),
    ]:
        found, _ = manifest_problems(plain + [extra], reference, flag_file, given)
        if not any(expect in problem for problem in found):
            failures.append(f"manifests: {what} passed ({found})")
    found, _ = manifest_problems([("src/kernel/Cargo.toml", {"dependencies": {}})], reference, flag_file, given)
    if not any("does not hold" in problem for problem in found):
        failures.append(f"manifests: a record naming a key its manifest lacks passed ({found})")
    texts = {
        "tools/common/xtask/src/cargo.rs": 'pub(crate) const MANIFEST_FLAGS: [&str; 2] = ["personality", "linux_abi"];\n',
        "src/kernel/build.rs": 'const MANIFEST_FLAGS: [&str; 2] = ["personality", "linux_abi"];\n',
        "tools/common/check/check-ferrocene-rust.py": 'MANIFEST_FLAGS = ("personality", "linux_abi")\n',
    }
    lists, found = manifest_flag_lists(texts.__getitem__)
    if found or set(lists.values()) != {("personality", "linux_abi")}:
        failures.append(f"manifests: three equal MANIFEST_FLAGS lists were refused: {found}")
    texts["src/kernel/build.rs"] = 'const MANIFEST_FLAGS: [&str; 1] = ["personality"];\n'
    _, found = manifest_flag_lists(texts.__getitem__)
    if not any("differ" in problem for problem in found):
        failures.append(f"manifests: two MANIFEST_FLAGS lists that differ passed ({found})")
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
    import json
    reference = json.loads(MANIFEST.read_text(encoding="utf-8")).get("reference_configuration", {})
    lists, keyed = manifest_flag_lists(lambda rel: (ROOT / rel).read_text(encoding="utf-8"))
    given = set(next(iter(lists.values()), ()))
    found, features = manifest_problems(manifests(), reference, flag_file, given)
    keyed += found
    if keyed:
        print("build-flags: a manifest breaks rule 1 or 6 b (a Cargo feature, or a table keyed on a flag "
              "outside the record):", file=sys.stderr)
        for problem in keyed:
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
    if images:
        print("build-flags: the profiles' image keys do not follow the flag file:", file=sys.stderr)
        for problem in images:
            print(f"    {problem}", file=sys.stderr)
        status = 1
    registry = flag_file.get("boot_options", {})
    settable = settable_problems(registry, profile_files)
    if settable:
        print("build-flags: the settable boot words do not follow the rule (ledger 5579 (8)):", file=sys.stderr)
        for problem in settable:
            print(f"    {problem}", file=sys.stderr)
        status = 1
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
            f"build-flags: no build-flag cfg outside the kernel's crates; {features} Cargo feature(s) in the "
            f"kernel's manifests; tables keyed on a flag only where the record names them "
            f"({', '.join(sorted(given))} given to rustc); {sum(counts.values())} "
            f"boot-option reads, all of the {len(registry)} declared options; the libraries' and "
            f"loaders' option-like literals all accounted for"
        )
    return status


if __name__ == "__main__":
    sys.exit(main())
