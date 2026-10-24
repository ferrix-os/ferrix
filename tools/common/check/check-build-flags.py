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
   the core's `build_flag_word!` names the same values in the same order:
   each of the kernel's crates expands it, and the binary requires the four
   words equal (ledger line 3282, (K4)).

The first two have a planted negative control (a `ferrix_` cfg in a library; a reader
of an undeclared option), run with `gate.sh control` over `cargo xtask
test-flags`.

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
