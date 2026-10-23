#!/usr/bin/env python3
"""Turn Linux's processor feature tables into the names `/proc/cpuinfo` prints.

A program that reads `/proc/cpuinfo` -- Steam's hardware survey, a game's
launcher, a fetch tool -- looks for `avx2` or `sse4_2` in the `flags` line
on x86-64, and for `asimd` in `Features` on Arm. Those spellings are Linux's,
and they are not the instruction set manuals': `pni` for SSE3, `lahf_lm`,
`dts` for the debug store. So the names come from Linux's own tables, not
from memory, and this script is what copies them:

* `arch/x86/include/asm/cpufeatures.h`: each `X86_FEATURE_*` bit, and the
  quoted name its comment starts with. A bit whose comment has no quoted
  name is not printed, as `mkcapflags.sh` decides.
* `arch/x86/kernel/cpu/cpuid-deps.c`: which feature goes when another does.
* `arch/x86/kernel/cpu/scattered.c`: the bits Linux gathers from leaves it
  has no whole word for.
* `arch/arm64/include/uapi/asm/hwcap.h`, `arch/arm64/include/asm/hwcap.h`
  and `arch/arm64/kernel/cpuinfo.c`: AArch64's `Features` names, by
  `AT_HWCAP` and `AT_HWCAP2` bit.
* `arch/arm/kernel/setup.c`: ARMv7-A's, by bit.

The tree is read from `--linux`, a Linux source tree; the tables were last
written from v7.0, the kernel the N150's inspection Linux runs. The output is
committed, and `--check` regenerates it into memory and compares.

Usage:
    python3 tools/common/gen/gen-cpuinfo-names.py --linux DIR           # write
    python3 tools/common/gen/gen-cpuinfo-names.py --linux DIR --check   # stale?
"""

import argparse
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent.parent.parent
OUT = ROOT / "src" / "lib" / "fs" / "common" / "procfs" / "src" / "cpuinfo" / "names.rs"

# The x86 features the renderer names in its own logic, beyond the tables.
NAMED = [
    "ACC_POWER",
    "ARAT",
    "LM",
    "3DNOW",
    "3DNOWPREFETCH",
    "AMX_TILE",
    "AMD_IBPB",
    "AMD_IBRS",
    "AMD_SSBD",
    "AMD_STIBP",
    "APIC",
    "ARCH_PERFMON",
    "AUTOIBRS",
    "AVX",
    "AVX512F",
    "CONSTANT_TSC",
    "CPUID",
    "EXTD_APICID",
    "FSGSBASE",
    "IBPB",
    "IBRS",
    "IBRS_ENHANCED",
    "INTEL_PPIN",
    "INTEL_STIBP",
    "LA57",
    "NONSTOP_TSC",
    "NOPL",
    "PKU",
    "RAPL",
    "REP_GOOD",
    "SEV",
    "SEV_ES",
    "SEV_SNP",
    "SME",
    "SPEC_CTRL",
    "SPEC_CTRL_SSBD",
    "SSBD",
    "STIBP",
    "TSC",
    "TSC_KNOWN_FREQ",
    "VIRT_SSBD",
    "VMMCALL",
    "XSAVE",
    "XTOPOLOGY",
]

REGISTERS = {"CPUID_EAX": 0, "CPUID_EBX": 1, "CPUID_ECX": 2, "CPUID_EDX": 3}


def x86_features(linux):
    """Every `X86_FEATURE_*`: name -> (bit, printed name or None)."""
    text = (linux / "arch/x86/include/asm/cpufeatures.h").read_text()
    words = int(re.search(r"#define NCAPINTS\s+(\d+)", text).group(1))
    features = {}
    pattern = re.compile(
        r"#define X86_FEATURE_(\w+)\s+\(\s*(\d+)\s*\*\s*32\s*\+\s*(\d+)\s*\)\s*(?:/\*\s*(.*))?"
    )
    for line in text.splitlines():
        found = pattern.match(line)
        if not found:
            continue
        name, word, bit, comment = found.groups()
        printed = None
        if comment:
            quoted = re.match(r'"([^"]*)"', comment)
            if quoted and quoted.group(1):
                printed = quoted.group(1)
        features[name] = (int(word) * 32 + int(bit), printed)
    return words, features


def x86_dependencies(linux, features):
    text = (linux / "arch/x86/kernel/cpu/cpuid-deps.c").read_text()
    table = text[text.index("cpuid_deps[]") :]
    table = table[: table.index("};")]
    pairs = []
    for feature, depends in re.findall(r"\{\s*X86_FEATURE_(\w+),\s*X86_FEATURE_(\w+)\s*,?\s*\}", table):
        pairs.append((features[feature][0], features[depends][0], feature, depends))
    return pairs


def x86_scattered(linux, features):
    text = (linux / "arch/x86/kernel/cpu/scattered.c").read_text()
    table = text[text.index("cpuid_bits[]") :]
    table = table[: table.index("};")]
    rows = []
    pattern = r"\{\s*X86_FEATURE_(\w+),\s*(CPUID_E[A-D]X),\s*(\d+),\s*(0x[0-9a-fA-F]+),\s*(\d+)\s*\}"
    for feature, register, bit, leaf, subleaf in re.findall(pattern, table):
        rows.append(
            (features[feature][0], REGISTERS[register], int(bit), int(leaf, 16), int(subleaf), feature)
        )
    return rows


def aarch64_names(linux):
    """`Features` names by (word, bit): word 0 is `AT_HWCAP`, 1 `AT_HWCAP2`."""
    uapi = (linux / "arch/arm64/include/uapi/asm/hwcap.h").read_text()
    bits = {}
    for word, name, shift in re.findall(r"#define HWCAP(\d?)_(\w+)\s+\(1(?:UL)?\s*<<\s*(\d+)\)", uapi):
        bits[(word or "1", name)] = int(shift)
    kernel = (linux / "arch/arm64/include/asm/hwcap.h").read_text()
    which = {}
    for name, word, cap in re.findall(r"#define KERNEL_HWCAP_(\w+)\s+__khwcap(\d?)_feature\((\w+)\)", kernel):
        which[name] = (word or "1", cap)
    cpuinfo = (linux / "arch/arm64/kernel/cpuinfo.c").read_text()
    table = cpuinfo[cpuinfo.index("hwcap_str[] = {") :]
    table = table[: table.index("};")]
    names = []
    for name, printed in re.findall(r"\[KERNEL_HWCAP_(\w+)\]\s*=\s*\"(\w+)\"", table):
        word, cap = which[name]
        if word not in ("1", "2"):
            continue  # AT_HWCAP3 and on: Ferrix passes no such word.
        names.append((int(word) - 1, bits[(word, cap)], printed))
    names.sort()
    return names


def arm_names(linux):
    text = (linux / "arch/arm/kernel/setup.c").read_text()
    tables = []
    for array in ("hwcap_str[] = {", "hwcap2_str[] = {"):
        table = text[text.index(array) :]
        table = table[: table.index("};")]
        tables.append(re.findall(r"\"(\w+)\"", table))
    return tables


def rust_name(name):
    """A Rust constant's name for `X86_FEATURE_<name>`: `3DNOW` is `THREEDNOW`."""
    return "THREE" + name[1:] if name.startswith("3") else name


def render(linux):
    words, features = x86_features(linux)
    dependencies = x86_dependencies(linux, features)
    scattered = x86_scattered(linux, features)
    flags = sorted((bit, printed) for bit, printed in features.values() if printed)
    arm64 = aarch64_names(linux)
    arm_hwcap, arm_hwcap2 = arm_names(linux)

    out = []
    out.append("//! Linux's names for processor features, as `/proc/cpuinfo` prints them.")
    out.append("//!")
    out.append("//! Written by `tools/common/gen/gen-cpuinfo-names.py` from Linux v7.0's")
    out.append("//! `cpufeatures.h`, `cpuid-deps.c`, `scattered.c`, arm64's `hwcap.h` and")
    out.append("//! `cpuinfo.c`, and arm's `setup.c`. Do not edit; run the script.")
    out.append("")
    out.append("/// Linux's `NCAPINTS`: 32-bit words of x86 capability bits.")
    out.append(f"pub(super) const X86_WORDS: usize = {words};")
    out.append("")
    out.append("/// The x86 capability bits `/proc/cpuinfo`'s `flags` names, in the order")
    out.append("/// it prints them: (word * 32 + bit, name).")
    out.append("pub(super) const X86_FLAGS: &[(u16, &str)] = &[")
    for bit, printed in flags:
        out.append(f'    ({bit}, "{printed}"),')
    out.append("];")
    out.append("")
    out.append("/// `cpuid_deps`: (feature, the feature it needs). Clearing the second")
    out.append("/// clears the first.")
    out.append("pub(super) const X86_DEPENDENCIES: &[(u16, u16)] = &[")
    for feature, depends, fname, dname in dependencies:
        out.append(f"    ({feature}, {depends}), // {fname.lower()} needs {dname.lower()}")
    out.append("];")
    out.append("")
    out.append("/// `cpuid_bits`, the features Linux scatters into its own words:")
    out.append("/// (feature, register -- 0 `EAX` to 3 `EDX` --, bit, leaf, subleaf).")
    out.append("pub(super) const X86_SCATTERED: &[(u16, u8, u8, u32, u32)] = &[")
    for feature, register, bit, leaf, subleaf, fname in scattered:
        out.append(f"    ({feature}, {register}, {bit}, {leaf:#x}, {subleaf}), // {fname.lower()}")
    out.append("];")
    out.append("")
    out.append("/// The x86 features the renderer names in its own logic.")
    out.append("pub(super) mod feature {")
    for name in sorted(NAMED, key=rust_name):
        bit = features[name][0]
        out.append(f"    /// `X86_FEATURE_{name}`, word {bit // 32} bit {bit % 32}.")
        out.append(f"    pub(in super::super) const {rust_name(name)}: u16 = {bit};")
    out.append("}")
    out.append("")
    out.append("/// AArch64's `Features` names: (0 for `AT_HWCAP`, 1 for `AT_HWCAP2`, bit,")
    out.append("/// name), in the order Linux prints them.")
    out.append("pub(super) const AARCH64_FEATURES: &[(u8, u8, &str)] = &[")
    for word, bit, printed in arm64:
        out.append(f'    ({word}, {bit}, "{printed}"),')
    out.append("];")
    out.append("")
    out.append("/// ARMv7-A's `Features` names for `AT_HWCAP`, by bit.")
    out.append("pub(super) const ARM_HWCAP: &[&str] = &[")
    for printed in arm_hwcap:
        out.append(f'    "{printed}",')
    out.append("];")
    out.append("")
    out.append("/// ARMv7-A's `Features` names for `AT_HWCAP2`, by bit.")
    out.append("pub(super) const ARM_HWCAP2: &[&str] = &[")
    for printed in arm_hwcap2:
        out.append(f'    "{printed}",')
    out.append("];")
    return rustfmt("\n".join(out) + "\n")


def rustfmt(text):
    """`text` as `cargo fmt` leaves it, so that `--check` and the formatting
    gate agree on one file."""
    return subprocess.run(
        ["rustfmt", "--edition", "2024", "--emit", "stdout", "--quiet"],
        input=text,
        capture_output=True,
        text=True,
        check=True,
        cwd=ROOT,
    ).stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--linux", required=True, type=pathlib.Path, help="a Linux source tree")
    parser.add_argument("--check", action="store_true", help="fail if the committed file is stale")
    args = parser.parse_args()
    text = render(args.linux)
    if args.check:
        if not OUT.exists() or OUT.read_text() != text:
            print(f"{OUT.relative_to(ROOT)} is stale: run {sys.argv[0]} --linux {args.linux}")
            return 1
        return 0
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(text)
    print(f"wrote {OUT.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
