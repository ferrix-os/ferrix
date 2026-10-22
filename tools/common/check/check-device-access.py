#!/usr/bin/env python3
"""Hold the seam: the kernel enumerates devices, and drives none.

`docs/ARCHITECTURE.md` §1 and §7 make a structural claim. Buses are enumerated
in the kernel, because that needs ACPI, a device tree and privileged access;
but the thing that pokes a device's registers lives in ring 3, behind an
`IoMapping` and an IOMMU domain. That claim is what makes a userspace driver
worth having -- and it is the kind of claim that decays one convenient access
at a time, each of which looked reasonable on its own.

So this is an allow-list, not a threshold, and it works like
`tools/common/check/check-asm-budget.py`: adding a site is a deliberate act somebody had
to argue for in a diff.

What counts as a site
---------------------

Three different things wear the same spelling in Rust, and conflating them
would make this gate noise:

  register   A device register: memory-mapped I/O, or an x86 port. Reading one
             can have side effects -- reading a 16550's receive buffer consumes
             a character. These are what the seam is about, and an entry of
             this kind is refused outside the directories §1 and §7 permit.

  shared     Ordinary RAM that a device or another process also reads: an
             IOMMU's tables, a virtio configuration space, the block and net
             rings in their VMOs. Volatile because something outside this
             processor observes it, not because it is a register. Permitted
             anywhere an entry argues for it, since this is the ring protocol
             §7 is built on.

  memory     Ordinary RAM, volatile only so the compiler cannot elide an access
             the check is making on purpose: a poisoned page read back, a page
             table word, a stack walk. Nothing to do with devices at all.

The gate cannot tell the three apart by reading the source -- they are the same
call -- so the allow-list says which each file is, and the diff that adds one
is where that is argued. What the gate does enforce is that the claim was made
at all, that it stays inside its budget, and that a `register` entry sits only
where the architecture says a register may be touched.

Four rules:

  1. A file under `src/kernel/` containing a device access must be listed in
     `tools/common/data/device-access-allowlist.json`, with a `kind` and a `reason`.
  2. Each entry declares a `max_sites` budget the file must stay under.
  3. A stale entry fails, the way `#[expect]` does: a file that no longer
     contains one must lose its exemption rather than keep it warm.
  4. A `register` entry must be under one of the permitted paths. This is the
     rule that carries the weight; the rest keep it honest.

Usage:  python3 tools/common/check/check-device-access.py [--json]
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys

ROOT_DIR = "src/kernel"

# `ptr.read_volatile()`, `core::ptr::write_volatile(ptr, v)` and every spelling
# between. The compiler is being told the access must really happen. Matched on
# the bare name, so a path-qualified call counts: the lookbehind excludes
# identifier characters and not `:`, or `cpu::outb(...)` would slip past.
VOLATILE = re.compile(r"(?<![A-Za-z0-9_])(read_volatile|write_volatile)\s*\(")
# The x86 port primitives, at their definition and at every call, qualified or
# not -- the callers are what make a file a register site, not the wrapper.
PORT_IO = re.compile(r"(?<![A-Za-z0-9_])(inb|inw|inl|outb|outw|outl)\s*\(")
# A port instruction inside an `asm!` string: `out dx, al`, `in al, dx`.
PORT_ASM = re.compile(r'"\s*(in|out)\s+[a-z]')
# A line that is only a comment carries no access, whatever it talks about.
# The `*` branch is a block comment's continuation line, which is why it must
# be followed by space or end: `*byte = ... read_volatile(..)` is a pointer
# dereference, and reading it as a comment would hide a ring access entirely.
COMMENT = re.compile(r"^\s*(//|/\*|\*\s|\*/|\*$)")

KINDS = ("register", "shared", "memory")


def sites(source: str) -> int:
    """Device-access sites in one Rust file."""
    total = 0
    for line in source.splitlines():
        if COMMENT.match(line):
            continue
        total += len(VOLATILE.findall(line))
        total += len(PORT_IO.findall(line))
        total += len(PORT_ASM.findall(line))
    return total


def survey(root: pathlib.Path) -> dict[str, int]:
    """Sites per file, for every Rust file under the kernel."""
    measured: dict[str, int] = {}
    directory = root / ROOT_DIR
    for path in sorted(directory.rglob("*.rs")):
        if "target" in path.parts or not path.is_file():
            continue
        count = sites(path.read_text(encoding="utf-8"))
        if count:
            measured[path.relative_to(root).as_posix()] = count
    return measured


def permitted(relative: str, paths: list[str]) -> bool:
    """Whether a register may be touched here, per ARCHITECTURE.md §1 and §7."""
    return any(relative == path or relative.startswith(path) for path in paths)


def check(measured: dict[str, int], policy: dict) -> list[str]:
    allowed = {entry["file"]: entry for entry in policy["files"]}
    paths = policy["register_paths"]
    problems: list[str] = []

    for relative, count in sorted(measured.items()):
        entry = allowed.get(relative)
        if entry is None:
            problems.append(
                f"{relative}: makes {count} device access(es) but is not in "
                "tools/common/data/device-access-allowlist.json.\n"
                "  Add an entry saying which kind it is and why, or move the "
                "access to the driver that owns the device."
            )
            continue
        if entry["kind"] not in KINDS:
            problems.append(
                f"{relative}: kind {entry['kind']!r} is not one of "
                f"{', '.join(KINDS)}"
            )
        if count > entry["max_sites"]:
            problems.append(
                f"{relative}: {count} device accesses exceeds its budget of "
                f"{entry['max_sites']}"
            )
        # Rule 4, the one with teeth.
        if entry["kind"] == "register" and not permitted(relative, paths):
            problems.append(
                f"{relative}: touches device registers outside the paths "
                "ARCHITECTURE.md §1 and §7 permit.\n"
                "  The kernel enumerates devices and drives none: this belongs "
                "in a ring-3 driver behind an IoMapping."
            )

    for relative in sorted(allowed):
        if relative not in measured:
            problems.append(
                f"{relative}: allow-listed for device access but makes none; "
                "remove the entry rather than leaving it warm"
            )

    registers = sum(
        count
        for relative, count in measured.items()
        if allowed.get(relative, {}).get("kind") == "register"
    )
    cap = policy["max_register_sites"]
    if registers > cap:
        problems.append(
            f"the kernel touches device registers at {registers} sites, over "
            f"the cap of {cap} in tools/common/data/device-access-allowlist.json.\n"
            "  That list is meant to be finished. A new device is a ring-3 "
            "driver, not another site here."
        )

    return problems


# ferrix.msrlog is read-only (L.x86_64.187, the consultant's condition 5 of
# ledger line 3075): no line of its files may write an MSR, by the kernel's
# helper or by the instruction. Comments included, so a write cannot hide
# behind a doc line either.
READ_ONLY_MSR_FILES = ("src/kernel/src/arch/x86_64/msrlog.rs", "src/kernel/src/arch/x86_64/msrlog/")
MSR_WRITE = re.compile(r"\bwrite_msr\w*|\bwrmsr\b|\bwrmsrns\b")


def read_only_msr_problems(root: pathlib.Path) -> list[str]:
    problems = []
    files = [root / READ_ONLY_MSR_FILES[0], *sorted((root / READ_ONLY_MSR_FILES[1]).glob("*.rs"))]
    for path in files:
        if not path.exists():
            continue
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            if MSR_WRITE.search(line):
                problems.append(f"{path.relative_to(root)}:{number}: ferrix.msrlog writes an MSR; it must only read (L.x86_64.187)")
    return problems


# The power core writes one register (L.x86_64.192, the consultant's
# condition 11 of ledger line 3130; docs/POWERCTL.md): the kernel's one
# checked MSR write, `cpu::write_msr_checked`, is called only from
# powerctl.rs, with the constant VR_CURRENT_CONFIG defined there once as
# 0x601, and from the write's boot check in cpu/check.rs, with only its
# absent-index constant ABSENT. In cpu.rs nothing but its definition names it
# with an argument list (ledger 3090, C2). powerctl.rs writes an MSR no other
# way, and the files under powerctl/ -- the decisions, the fake register file
# and the checks -- reach no MSR at all, read or write. cpu.rs's checked write
# holds exactly one `wrmsr`, at its fixup label; cpu.rs's other `wrmsr`, the
# unchecked write_msr, is the only other in that file.
KERNEL = "src/kernel/src"
CPU_FILE = "src/kernel/src/arch/x86_64/cpu.rs"
POWER_FILE = "src/kernel/src/arch/x86_64/powerctl.rs"
POWER_DIR = "src/kernel/src/arch/x86_64/powerctl/"
CHECK_FILE = "src/kernel/src/arch/x86_64/cpu/check.rs"
CHECKED_WRITE_CALL = re.compile(r"\bwrite_msr_checked\b(\s*\(\s*([A-Za-z0-9_:]*))?")
CHECKED_WRITE_ARGUMENT = {POWER_FILE: "VR_CURRENT_CONFIG", CHECK_FILE: "ABSENT"}
OTHER_MSR_WRITE = re.compile(r"\bwrite_msr\b|\bwrmsr\b|\bwrmsrns\b")
ANY_MSR_ACCESS = re.compile(r"\b(read|write)_msr\w*|\brdmsr\b|\bwrmsr\w*\b")
ASM_WRMSR = re.compile(r'"wrmsr"')


def checked_write_problems(root: pathlib.Path) -> list[str]:
    problems = []
    for path in sorted((root / KERNEL).rglob("*.rs")):
        relative = path.relative_to(root).as_posix()
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            for match in CHECKED_WRITE_CALL.finditer(line):
                if relative == CPU_FILE:
                    if match.group(1) and not re.search(r"\bfn write_msr_checked\(", line):
                        problems.append(f"{relative}:{number}: write_msr_checked called inside cpu.rs (L.x86_64.192)")
                    continue
                wanted = CHECKED_WRITE_ARGUMENT.get(relative)
                if wanted is None:
                    problems.append(f"{relative}:{number}: write_msr_checked outside the power core (L.x86_64.192)")
                elif match.group(2) != wanted:
                    problems.append(f"{relative}:{number}: write_msr_checked of {match.group(2) or 'no register'}, not {wanted} (L.x86_64.192)")
    power = root / POWER_FILE
    if power.exists():
        text = power.read_text(encoding="utf-8")
        for number, line in enumerate(text.splitlines(), 1):
            if OTHER_MSR_WRITE.search(line):
                problems.append(f"{POWER_FILE}:{number}: the power core writes an MSR other than through write_msr_checked (L.x86_64.192)")
        if re.findall(r"const VR_CURRENT_CONFIG: u32 = (\w+);", text) != ["0x601"]:
            problems.append(f"{POWER_FILE}: VR_CURRENT_CONFIG must be defined once, as 0x601 (L.x86_64.192)")
    for path in sorted((root / POWER_DIR).glob("*.rs")):
        relative = path.relative_to(root).as_posix()
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            if ANY_MSR_ACCESS.search(line):
                problems.append(f"{relative}:{number}: the power core's decisions, fake or checks reach an MSR (L.x86_64.207)")
    cpu = (root / CPU_FILE).read_text(encoding="utf-8")
    body = re.search(r"fn ferrix_wrmsr_checked\(.*?\n}\n", cpu, re.S)
    if body is None or len(ASM_WRMSR.findall(body.group(0))) != 1 or not re.search(r'"ferrix_wrmsr_checked_insn:",\s*"wrmsr",', body.group(0)):
        problems.append(f"{CPU_FILE}: ferrix_wrmsr_checked must hold exactly one wrmsr, at ferrix_wrmsr_checked_insn (L.x86_64.192)")
    if len(ASM_WRMSR.findall(cpu)) != 2:
        problems.append(f"{CPU_FILE}: {len(ASM_WRMSR.findall(cpu))} wrmsr; only write_msr's and the checked write's (L.x86_64.192)")
    return problems


def main() -> int:
    parser = argparse.ArgumentParser()
    _ = parser.add_argument("--json", action="store_true", help="machine-readable report")
    args = parser.parse_args()

    root = pathlib.Path(__file__).resolve().parent.parent.parent.parent
    policy = json.loads(
        (root / "tools" / "common" / "data" / "device-access-allowlist.json").read_text(encoding="utf-8")
    )
    allowed = {entry["file"]: entry for entry in policy["files"]}

    measured = survey(root)
    problems = check(measured, policy) + read_only_msr_problems(root) + checked_write_problems(root)

    by_kind = {kind: 0 for kind in KINDS}
    for relative, count in measured.items():
        entry = allowed.get(relative)
        if entry and entry["kind"] in by_kind:
            by_kind[entry["kind"]] += count

    if args.json:
        print(
            json.dumps(
                {
                    "sites": measured,
                    "by_kind": by_kind,
                    "max_register_sites": policy["max_register_sites"],
                    "problems": problems,
                },
                indent=2,
            )
        )
    else:
        print(
            f"device-access: {by_kind['register']} register "
            f"({policy['max_register_sites']} allowed), "
            f"{by_kind['shared']} shared, {by_kind['memory']} memory"
        )
        for relative, count in sorted(measured.items(), key=lambda item: -item[1]):
            entry = allowed.get(relative)
            kind = entry["kind"] if entry else "NOT ALLOW-LISTED"
            budget = f"/{entry['max_sites']}" if entry else ""
            print(f"    {count:5}{budget:>6}  {kind:<9}  {relative}")

    if problems:
        print(file=sys.stderr)
        for problem in problems:
            print(problem, file=sys.stderr)
        print(file=sys.stderr)
        print(
            "The kernel enumerates devices and drives none.\n"
            "See docs/ARCHITECTURE.md §1 and §7.",
            file=sys.stderr,
        )
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
