#!/usr/bin/env python3
"""List the sel4test tests an image's programs carry, and whether each is enabled.

    list-tests.py <sel4test-driver ELF> <sel4test-tests ELF>

sel4test keeps each DEFINE_TEST as a `struct testcase` (name inline,
description, function, type, enabled) in the `_test_case` section; `enabled`
is fixed at build time by the test's configuration condition. Prints `<name> on|off
<description>`, then the counts. Needs pyelftools (the build venv has it).
"""
import struct
import sys

from elftools.elf.elffile import ELFFile


def string_at(elf, addr):
    for seg in elf.iter_segments():
        if seg["p_type"] == "PT_LOAD" and seg["p_vaddr"] <= addr < seg["p_vaddr"] + seg["p_filesz"]:
            data = seg.data()[addr - seg["p_vaddr"]:]
            return data[:data.index(b"\0")].decode()
    return "?"


def tests(path):
    with open(path, "rb") as f:
        elf = ELFFile(f)
        symtab = elf.get_section_by_name(".symtab")
        sec = elf.get_section_by_name("_test_case")
        if sec is None:
            return []
        idx = [i for i, s in enumerate(elf.iter_sections()) if s.name == "_test_case"][0]
        data, base = sec.data(), sec["sh_addr"]
        out = []
        for sym in symtab.iter_symbols():
            if sym["st_shndx"] == idx and sym.name.startswith("TEST_"):
                # struct testcase (AArch32): char name[48]; description,
                # function, test_type, enabled: four words; 64 bytes.
                off = sym["st_value"] - base
                raw = data[off:off + 48]
                name = raw[:raw.index(b"\0")].decode() if b"\0" in raw else raw.decode()
                desc, _fn, _type, enabled = struct.unpack_from("<4I", data, off + 48)
                out.append((name, bool(enabled), string_at(elf, desc)))
        return out


def main():
    allt = []
    for p in sys.argv[1:]:
        allt += tests(p)
    allt.sort()
    for name, on, desc in allt:
        print(f"{name} {'on' if on else 'off'} {desc}")
    print(f"# {sum(1 for t in allt if t[1])} enabled, {sum(1 for t in allt if not t[1])} disabled")


if __name__ == "__main__":
    main()
