#!/usr/bin/env python3
"""A kernel built without the Linux personality compiles no extension
(docs/microkernel/CORE-WITHOUT-EXTENSIONS.md; ledger 9212 M7 (d), 9361 K1).

Reads Cargo's graph, builds nothing. For each kernel target, `cargo tree` of
the binary, `ferrix-kernel`, with the flags its manifest keys on given to
rustc as xtask gives them (`--config target.<t>.rustflags`, CORE-WITHOUT-
EXTENSIONS §2):

1. **`personality=user` and `personality=none`** (the `fast` and `max`
   kernels' graph, and the `linux_abi=off` build's) name no package whose
   manifest is under the parts' place (`kernel_source_roots.parts_place`,
   the extensions) but, with `board_glue=on`, the board glue, by name
   (`BOARD_GLUE`); with `board_glue=off` none at all (CORE-WITHOUT-EXTENSIONS
   E1b; ledger 9212 Q1.4).
2. **`personality=kernel`** with `board_glue=on` names every package of the
   binary's keyed tables: each table is read as a dependency where its flag
   holds, not as none.
3. **The Linux-only libraries an `off` tree still names** are data here
   (`LINUX_ONLY`), each with the step of the plan that removes it. A listed
   library the `off` tree no longer names fails until its line goes, so the
   list only shrinks; a package the listed ones bring into the tree (`cargo
   tree --prune`) that is not listed fails.
4. **The libraries of the `off` tree that depend on ferrix-linux-abi
   directly**, other than the kernel's own crates and the listed ones, are
   data too (`LINUX_ABI_USERS`), each with what it names of it and the step
   that would take it out: they keep the library in an `off` tree after E6
   has left the core's and the item's manifests. A new one fails, and a
   listed one that no longer depends on it fails until its line goes.

`--self-test` runs the rules on planted trees; the controls are the load, and
a board crate, put back as a plain dependency of the binary, run by `gate.sh
control` over `cargo xtask check`.

    python3 tools/common/check/check-core-without-extensions.py
    python3 tools/common/check/check-core-without-extensions.py --self-test
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import kernel_roots  # noqa: E402  (the parts' place, docs/microkernel/CRATE-SPLIT.md S0)

ROOT = Path(__file__).resolve().parent.parent.parent.parent
BINARY = "ferrix-kernel"
TARGETS = ("x86_64-unknown-none", "aarch64-unknown-none-softfloat", "armv7a-none-eabi")
KEY = 'cfg(ferrix_personality = "kernel")'
BOARD_KEY = 'cfg(ferrix_board_glue = "on")'
KEYS = (KEY, BOARD_KEY)
# The builds without the Linux personality in ring 0 (9361 K1), as
# (personality, linux_abi, board_glue): each combination the flag file allows.
WITHOUT = (("user", "on", "on"), ("user", "on", "off"), ("none", "off", "on"), ("none", "off", "off"))
WITH = ("kernel", "on", "on")
# The board glue: a kernel without the Linux personality may name it where
# board_glue=on, and no other extension (ledger 9212 Q1.1, Q1.4).
BOARD_GLUE = ("ferrix-kernel-board-gs201-usb", "ferrix-kernel-board-stm32mp1")
# The libraries that exist only for the Linux personality and that an `off`
# tree still names, each with the step that removes it (ledger 9212 O2: E6 is
# per library, in the landing that removes its last name).
LINUX_ONLY = {
    "ferrix-cgroupfs": "E4 (object/job.rs's Limit and controllers), then E6",
    "ferrix-vfs": "E2 (Errno), E4 (panic/catalog.rs), E5 (OpenFile), then E6",
    "ferrix-vdso": "E3 (the vDSO and counter_nanos), then E6",
    "ferrix-linux-abi": "E2 (Errno), E3 (the architecture's Linux code), E5 (Syscall), then E6",
}
# The libraries of an `off` tree that name ferrix-linux-abi directly, besides
# the kernel's crates and LINUX_ONLY: what each names of it, and the step that
# takes it out. The plan (CORE-WITHOUT-EXTENSIONS section 3) did not count
# them; found by this check's first run, 2026-11-13.
LINUX_ABI_USERS = {
    "ferrix-blkring": "errno::Errno (bell.rs): E2, ferrix-errno",
    "ferrix-native-abi": "errno::Errno and nr (status.rs): E2 for Errno; nr has no step yet (a plan row)",
    "ferrix-ustack": "types::AT_* (the auxiliary vector's keys): no step yet (a plan row)",
    "ferrix-virtio": "input (event codes, input.rs's re-export): no step yet (a plan row)",
}
KERNEL_CRATES = ("ferrix-kernel", "ferrix-kernel-core", "ferrix-kernel-item")
LINE = re.compile(r"^(\S+) v\S+(?: \((.*)\))?")


def rustflags(target: str, personality: str, linux_abi: str, board_glue: str) -> str:
    """xtask's `manifest_cfg_configs`, for one target."""
    return (f'target.{target}.rustflags=["--cfg","ferrix_personality=\\"{personality}\\"",'
            f'"--cfg","ferrix_linux_abi=\\"{linux_abi}\\"","--cfg","ferrix_board_glue=\\"{board_glue}\\""]')


def tree(target: str, personality: str, linux_abi: str, board_glue: str, prune: tuple[str, ...] = (),
         invert: str | None = None) -> dict[str, str]:
    """`cargo tree`'s packages, by name: each package's directory, relative to the
    repository (empty for one not in it). A tree that errors fails loudly: an
    empty answer would read as a kernel without extensions."""
    command = ["cargo", "tree", "--locked", "--target", target, "--edges", "normal", "--prefix", "none",
               "--format", "{p}", "--config", rustflags(target, personality, linux_abi, board_glue)]
    command += ["-i", invert, "-p", BINARY, "--depth", "1"] if invert else ["-p", BINARY]
    for name in prune:
        command += ["--prune", name]
    result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True)
    if result.returncode != 0:
        raise SystemExit(f"core-without-extensions: cargo tree failed for {target}, personality={personality}, "
                         f"linux_abi={linux_abi}, board_glue={board_glue}:\n{result.stderr.strip()}")
    found = {}
    for line in result.stdout.splitlines():
        match = LINE.match(line.strip())
        if match is None:
            continue
        path = match.group(2) or ""
        try:
            path = Path(path).resolve().relative_to(ROOT).as_posix() if path.startswith("/") else ""
        except ValueError:
            path = ""
        found[match.group(1)] = path
    return found


def without_problems(label: str, packages: dict[str, str], place: str, glue: bool) -> list[str]:
    """Rule 1: no package under the parts' place but, where `glue`, the board glue."""
    allowed = BOARD_GLUE if glue else ()
    return [f"{label}: names {name} ({path}), an extension; a kernel without the Linux personality "
            f"compiles {'none but the board glue' if glue else 'none'}"
            for name, path in sorted(packages.items())
            if (path == place or path.startswith(place + "/")) and name not in allowed]


def with_problems(label: str, packages: dict[str, str], keyed: dict[str, list[str]]) -> list[str]:
    """Rule 2: each keyed table is a dependency where its flag holds."""
    return [f"{label}: does not name {name}, which the binary's table keyed {key} lists"
            for key, names in keyed.items() for name in sorted(set(names) - set(packages))]


def linux_only_problems(label: str, packages: dict[str, str], brought: set[str],
                        linux_abi_users: set[str]) -> list[str]:
    """Rules 3 and 4 over an `off` tree: `brought` is what the tree loses when the
    listed libraries are pruned; `linux_abi_users` the packages that depend on
    ferrix-linux-abi directly."""
    problems = [f"{label}: no longer names {name}: take its line out of LINUX_ONLY (the list only shrinks)"
                for name in sorted(set(LINUX_ONLY) - set(packages))]
    problems += [f"{label}: names {name}, which the Linux-only libraries bring and LINUX_ONLY does not list"
                 for name in sorted(brought - set(LINUX_ONLY))]
    users = linux_abi_users - set(LINUX_ONLY) - set(KERNEL_CRATES) - {"ferrix-linux-abi"}
    problems += [f"{label}: names {name}, which depends on ferrix-linux-abi and is in neither LINUX_ONLY nor "
                 "LINUX_ABI_USERS: a library keeping the Linux ABI in the off tree"
                 for name in sorted(users - set(LINUX_ABI_USERS))]
    if "ferrix-linux-abi" in packages:
        problems += [f"{label}: {name} no longer depends on ferrix-linux-abi: take its line out of "
                     "LINUX_ABI_USERS (the list only shrinks)"
                     for name in sorted(set(LINUX_ABI_USERS) - users)]
    return problems


def keyed_dependencies() -> dict[str, list[str]]:
    with (ROOT / "src" / "kernel" / "Cargo.toml").open("rb") as handle:
        cargo = tomllib.load(handle)
    workspace = {}
    with (ROOT / "Cargo.toml").open("rb") as handle:
        workspace = tomllib.load(handle).get("workspace", {}).get("dependencies", {})
    tables = {}
    for table in KEYS:
        names = []
        for key, spec in cargo.get("target", {}).get(table, {}).get("dependencies", {}).items():
            package = key
            if isinstance(spec, dict) and spec.get("workspace"):
                root = workspace.get(key, {})
                package = root.get("package", key) if isinstance(root, dict) else key
            if isinstance(spec, dict):
                package = spec.get("package", package)
            names.append(package)
        tables[table] = names
    return tables


def self_test() -> list[str]:
    failures = []
    place = "src/kernel/parts-fixture"
    clean = {"ferrix-kernel": "src/kernel", "ferrix-kernel-core": "src/kernel/core",
             "ferrix-kernel-board-stm32mp1": f"{place}/board/stm32mp1", "ferrix-sync": "src/lib/kernel/sync"}
    if without_problems("t", clean, place, True):
        failures.append(f"a tree with the board glue alone was refused: {without_problems('t', clean, place, True)}")
    if not any("ferrix-kernel-board-stm32mp1" in problem for problem in without_problems("t", clean, place, False)):
        failures.append("the board glue in a board_glue=off tree passed")
    for name, path in [("ferrix-kernel-load", f"{place}/personality/linux"), ("ferrix-kernel-x", place)]:
        if not any(name in problem for problem in without_problems("t", {**clean, name: path}, place, True)):
            failures.append(f"an extension in a tree without the personality passed: {name}")
    if with_problems("t", {**clean, "ferrix-kernel-load": place}, {KEY: ["ferrix-kernel-load"]}):
        failures.append("a keyed table read as a dependency was refused")
    if not with_problems("t", clean, {KEY: ["ferrix-kernel-load"]}):
        failures.append("a keyed table read as no dependency passed")
    off = {**clean, **{name: "src/lib/x" for name in LINUX_ONLY}}
    users = {"ferrix-kernel-core", "ferrix-vfs", *LINUX_ABI_USERS}
    if linux_only_problems("t", off, set(LINUX_ONLY), users):
        failures.append(f"the listed off tree was refused: {linux_only_problems('t', off, set(LINUX_ONLY), users)}")
    gone = dict(off)
    del gone["ferrix-cgroupfs"]
    for what, args, expect in [
        ("a listed library the tree lost", (gone, set(LINUX_ONLY) - {"ferrix-cgroupfs"}, users), "no longer names"),
        ("a library the listed ones bring", ({**off, "ferrix-new": "src/lib/n"}, set(LINUX_ONLY) | {"ferrix-new"},
                                             users), "LINUX_ONLY does not list"),
        ("a new library depending on ferrix-linux-abi", ({**off, "ferrix-new": "src/lib/n"}, set(LINUX_ONLY),
                                                         users | {"ferrix-new"}), "LINUX_ABI_USERS: a library"),
        ("a listed user that no longer depends on it", (off, set(LINUX_ONLY), users - {"ferrix-ustack"}),
         "ferrix-ustack no longer depends"),
    ]:
        if not any(expect in problem for problem in linux_only_problems("t", *args)):
            failures.append(f"the Linux-only rule passed {what}")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    failures = self_test()
    if failures:
        for failure in failures:
            print(f"core-without-extensions: self-test: {failure}", file=sys.stderr)
        return 1
    if args.self_test:
        print("core-without-extensions: self-tests pass")
        return 0
    place = kernel_roots.parts_place()
    keyed = keyed_dependencies()
    problems = []
    for table, names in keyed.items():
        if not names:
            problems.append(f"src/kernel/Cargo.toml: no table keyed {table}")
    summary = []
    for target in TARGETS:
        for flags in WITHOUT:
            personality, linux_abi, board_glue = flags
            label = f"{target} personality={personality} linux_abi={linux_abi} board_glue={board_glue}"
            packages = tree(target, *flags)
            problems += without_problems(label, packages, place, board_glue == "on")
            parts = sorted(name for name, path in packages.items() if path.startswith(place + "/"))
            summary.append(f"{label}: {len(packages)} packages, of the parts' place only {', '.join(parts) or 'none'}")
            if linux_abi == "off":
                pruned = tree(target, *flags, prune=tuple(LINUX_ONLY))
                brought = set(packages) - set(pruned)
                users = set(tree(target, *flags, invert="ferrix-linux-abi")) \
                    if "ferrix-linux-abi" in packages else set()
                problems += linux_only_problems(label, packages, brought, users)
                still = sorted(set(LINUX_ONLY) & set(packages))
                summary.append(f"{label}: Linux-only libraries still named: {', '.join(still) or 'none'}")
        label = f"{target} personality={WITH[0]} linux_abi={WITH[1]} board_glue={WITH[2]}"
        problems += with_problems(label, tree(target, *WITH), keyed)
    if problems:
        print(f"core-without-extensions: {len(problems)} problem(s):", file=sys.stderr)
        for problem in problems:
            print(f"    {problem}", file=sys.stderr)
        return 1
    for line in summary:
        print(f"core-without-extensions: {line}")
    print(f"core-without-extensions: every personality=kernel tree names the "
          f"{sum(len(names) for names in keyed.values())} packages of the binary's keyed tables; no tree without "
          f"the personality names an extension but, with board_glue=on, the board glue")
    return 0


if __name__ == "__main__":
    sys.exit(main())
