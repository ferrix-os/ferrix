#!/usr/bin/env python3
"""Hold ARMv7-A's ASID sequences and F-67's remap order to their words.

docs/OPAQUE-KERNEL.md section 9.13. Three things no boot under QEMU can show,
because QEMU empties its TLB at every change of ASID and every TTBCR write,
and no emulator shows a TLB conflict:

1. The instruction order of the three register sequences in
   src/kernel/src/arch/armv7a/cpu.rs (L.armv7a.13, 14, 15): the install
   writes TTBR0 and synchronizes before it may clear EPD0, and holds no cache
   or TLB operation; the park and the flush set EPD0 and synchronize before
   TTBR0 becomes 0; the flush completes its invalidations before it returns.
   Each block must be exactly the instructions below.
2. F-67 (L.user.125): in src/kernel/src/user/space.rs every remap -- a
   `forget_in` followed within a few lines by a `mm::map_in` -- calls
   `break_page` between the two, there are exactly three, and `break_page`
   adds the page to the shootdown and calls `arch::break_before_make` on it.
3. `forget()` on a space's ASID tag is called from check code only (the
   second reading's A13).

Exit 0 and one line when all hold; otherwise each problem and exit 1.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
CPU = ROOT / "src/kernel/src/arch/armv7a/cpu.rs"
SPACE = ROOT / "src/kernel/src/user/space.rs"
KERNEL = ROOT / "src/kernel/src"

PARK = [
    "mrc p15, 0, {scratch}, c2, c0, 2",
    "orr {scratch}, {scratch}, #{epd0}",
    "mcr p15, 0, {scratch}, c2, c0, 2",
    "isb",
    "mcrr p15, 0, {zero}, {zero}, c2",
    "isb",
]

SEQUENCES = {
    "install_ttbr0": [
        "mcrr p15, 0, {low}, {high}, c2",
        "isb",
        "mrc p15, 0, {scratch}, c2, c0, 2",
        "tst {scratch}, #{epd0}",
        "beq 2f",
        "bic {scratch}, {scratch}, #{epd0}",
        "mcr p15, 0, {scratch}, c2, c0, 2",
        "isb",
        "2:",
    ],
    "park_ttbr0": PARK,
    "flush_for_new_generation": PARK
    + [
        "mcr p15, 0, {zero}, c8, c7, 0",
        "mcr p15, 0, {zero}, c7, c5, 6",
        "cmp {icache}, #0",
        "beq 2f",
        "mcr p15, 0, {zero}, c7, c5, 0",
        "2:",
        "dsb nsh",
        "isb",
    ],
}

# Coprocessor 15 operations on c7 (caches, predictor) or c8 (TLB).
MAINTENANCE = re.compile(r"\bmcr\s+p15,\s*0,\s*[^,]+,\s*c[78],")


def function_body(text: str, name: str) -> str | None:
    """The text of `fn name` up to the next item at column 0."""
    match = re.search(r"^pub\(crate\) (?:unsafe )?fn " + re.escape(name) + r"\b", text, re.M)
    if match is None:
        return None
    end = text.find("\n}\n", match.end())
    return text[match.start() : end if end >= 0 else len(text)]


def asm_lines(body: str) -> list[list[str]]:
    """Each `asm!` block's instruction strings, in order."""
    blocks = []
    for block in re.finditer(r"asm!\((.*?)\n\s*\);", body, re.S):
        blocks.append(re.findall(r'^\s*"([^"]*)",\s*$', block.group(1), re.M))
    return blocks


def check_sequences(problems: list[str]) -> int:
    text = CPU.read_text(encoding="utf-8")
    for name, expected in SEQUENCES.items():
        body = function_body(text, name)
        if body is None:
            problems.append(f"cpu.rs: no `fn {name}`")
            continue
        blocks = asm_lines(body)
        if len(blocks) != 1:
            problems.append(f"cpu.rs: `{name}` must be one asm! block, found {len(blocks)}")
            continue
        found = [" ".join(line.split()) for line in blocks[0]]
        if found != expected:
            problems.append(
                f"cpu.rs: `{name}`'s instructions are not its sequence:\n"
                f"    expected {expected}\n    found    {found}"
            )
        if name == "install_ttbr0" and any(MAINTENANCE.search(line) for line in found):
            problems.append("cpu.rs: `install_ttbr0` holds a cache or TLB operation")
    return len(SEQUENCES)


def check_remaps(problems: list[str]) -> int:
    lines = SPACE.read_text(encoding="utf-8").splitlines()
    remaps = 0
    for index, line in enumerate(lines):
        if "self.forget_in(" not in line:
            continue
        window = lines[index + 1 : index + 12]
        made = next((offset for offset, text in enumerate(window) if "mm::map_in(" in text), None)
        if made is None:
            continue
        remaps += 1
        if not any("break_page(&mut pages," in text for text in window[:made]):
            problems.append(
                f"space.rs:{index + 1}: a remap writes its new entry without "
                "break_page after the forget_in (F-67, L.user.125)"
            )
    helper = re.search(
        r"^fn break_page\(pages: &mut TlbPages, page: u64\) \{\n(.*?)\n\}",
        "\n".join(lines),
        re.M | re.S,
    )
    body = [line.strip() for line in helper.group(1).splitlines()] if helper else []
    if body != ["pages.add(page);", "arch::break_before_make(page);"]:
        problems.append(
            "space.rs: `break_page` is not `pages.add(page)` then `arch::break_before_make(page)`"
        )
    if remaps != 3:
        problems.append(f"space.rs: {remaps} remaps found, where the three paths are expected")
    return remaps


def check_forget_callers(problems: list[str]) -> int:
    callers = 0
    for path in KERNEL.rglob("*.rs"):
        text = path.read_text(encoding="utf-8")
        if "address_space_tag().forget()" not in text:
            continue
        callers += 1
        if not (path.name == "check.rs" or path.name.endswith("_check.rs")):
            problems.append(f"{path.relative_to(ROOT)}: forgets a space's ASID tag outside check code")
    return callers


def main() -> int:
    problems: list[str] = []
    sequences = check_sequences(problems)
    remaps = check_remaps(problems)
    callers = check_forget_callers(problems)
    if problems:
        for problem in problems:
            print(f"armv7a-asid: {problem}")
        return 1
    print(
        f"armv7a-asid: {sequences} register sequences in order, {remaps} remaps break "
        f"before they make, {callers} file(s) forgetting a tag, all check code"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
