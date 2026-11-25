#!/usr/bin/env python3
"""The SOUP register, generated from the dependency graph that actually ships.

IEC 62304 calls third-party code in a device SOUP -- software of unknown
provenance -- and §5.3.3, §7.1.2 and §8.1.2 require every item of it to be
identified by title, manufacturer and version, with its published anomaly list
evaluated against the device's hazards. For a Class C item that evaluation is
the difference between a dependency and a finding.

Two facts, both measured here rather than asserted, and a third, section 1a,
that is named from the toolchain pin and the sources' `extern crate alloc`
and measures nothing of the libraries it names:

  1. **The certified item links no external crate.** `ferrix-kernel` and
     `ferrix-boot` link none on any of the three architectures. Everything in
     the shipping graph is first-party code in this repository, developed under
     the lifecycle the rest of docs/certification describes. The graph is not
     the whole image: the toolchain's `core` and `compiler_builtins` are in
     every image and `alloc` in each whose crates say `extern crate alloc`
     (section 1a names them), and they are no crate's
     dependency, so section 1a names them from the pin (FINDINGS.md F-119)
     and the count leaves them out. That is the single
     most valuable sentence in this file, so the gate below re-establishes it
     on every run instead of letting it be a thing that used to be true.

  2. **The host-side crates are tools, not SOUP.** The 21 external crates in
     `Cargo.lock` build, test and package the item; none is in it. They are
     classified in TOOLS.md under EN 50716 §6.7 and DO-330 instead, which asks
     a different and more tractable question -- can this tool introduce an
     undetected error into the product -- than 62304 asks of SOUP.

    python3 tools/common/gen/gen-soup.py            # write docs/certification/SOUP.md
    python3 tools/common/gen/gen-soup.py --check    # fail if it is stale or SOUP appeared
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent.parent
OUTPUT = ROOT / "docs" / "certification" / "SOUP.md"

# The shipping graph: every binary that is part of the certified item, against
# every target it is built for. A crate that appears in none of these is not in
# the device, whatever else it is in.
SHIPPING = [
    ("ferrix-kernel", "x86_64-unknown-none"),
    ("ferrix-kernel", "aarch64-unknown-none-softfloat"),
    ("ferrix-kernel", "armv7a-none-eabi"),
    ("ferrix-boot", "x86_64-unknown-uefi"),
    ("ferrix-boot", "aarch64-unknown-uefi"),
    ("ferrix-boot", "armv7-unknown-linux-musleabi"),
]


# The kernel's manifest keys a table on a build flag, which Cargo decides from
# rustc's command line (docs/microkernel/CORE-WITHOUT-EXTENSIONS.md section 2):
# the kernel's graph is read with the flags of the reference configuration,
# `present`, as xtask gives them to rustc (personality=kernel, linux_abi=on),
# the graph with every part in it (ledger 9212 M4).
KERNEL_CFGS = '["--cfg","ferrix_personality=\\"kernel\\"","--cfg","ferrix_linux_abi=\\"on\\""]'


def shipping_externals() -> dict[tuple[str, str], list[str]]:
    """External crates reachable from each shipping binary."""
    found: dict[tuple[str, str], list[str]] = {}
    for package, target in SHIPPING:
        flags = ["--config", f"target.{target}.rustflags={KERNEL_CFGS}"] if package == "ferrix-kernel" else []
        result = subprocess.run(
            ["cargo", "tree", "-p", package, "--edges", "normal",
             "--prefix", "none", "--target", target, *flags],
            cwd=ROOT, capture_output=True, text=True,
        )
        if result.returncode != 0:
            # A tree that errors must not read as "no dependencies found":
            # that turns a broken manifest into an empty SOUP register, which
            # is the exact failure this file exists to prevent.
            raise SystemExit(
                f"gen-soup: cargo tree failed for {package} on {target}:\n"
                f"{result.stderr.strip()}"
            )
        names = {
            line.split()[0]
            for line in result.stdout.splitlines()
            if line.strip() and not line.startswith(" ")
        }
        found[(package, target)] = sorted(n for n in names if not n.startswith("ferrix-"))
        FIRST_PARTY[(package, target)] = sorted(n for n in names if n.startswith("ferrix-"))
    return found


# The first-party crates of each shipping graph, filled by shipping_externals:
# section 1a reads their sources for which toolchain libraries they link.
FIRST_PARTY: dict[tuple[str, str], list[str]] = {}

ALLOC_DECLARED = re.compile(
    r"^[ \t]*(?:#\[[^\]]*\][ \t]*)*(?:pub(?:\([^)]*\))?\s+)?extern\s+crate\s+alloc\b",
    re.MULTILINE,
)


def crate_directories() -> dict[str, Path]:
    """Each workspace package's directory, by name."""
    blob = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT, capture_output=True, text=True, check=True,
    ).stdout
    return {p["name"]: Path(p["manifest_path"]).parent for p in json.loads(blob)["packages"]}


def declares_alloc(directory: Path) -> bool:
    """Whether a crate's product source says `extern crate alloc`.

    A `no_std` crate links `alloc` only by saying so. Test files (`tests.rs`,
    anything under a `tests` directory) and a declaration under
    `#[cfg(test)]` are host-test code and do not count.
    """
    for path in sorted((directory / "src").rglob("*.rs")):
        if path.name == "tests.rs" or "tests" in path.relative_to(directory).parts:
            continue
        text = path.read_text(encoding="utf-8")
        for match in ALLOC_DECLARED.finditer(text):
            before = text[:match.start()].rstrip().splitlines()
            if before and before[-1].strip() == "#[cfg(test)]":
                continue
            return True
    return False


def links_alloc() -> dict[tuple[str, str], bool]:
    """For each shipping image, whether any crate of its graph links `alloc`."""
    directories = crate_directories()
    answer = {}
    for key, names in FIRST_PARTY.items():
        missing = [n for n in names if n not in directories]
        if missing:
            raise SystemExit(f"gen-soup: no directory for {', '.join(missing)}")
        answer[key] = any(declares_alloc(directories[n]) for n in names)
    return answer


TOOLCHAIN_LIBRARIES = ("core", "alloc", "compiler_builtins")


def alloc_packages() -> str:
    """The binaries that link `alloc`, read from the sources, for prose.

    The sentences beside the table say which images link `alloc`; they are
    built from the same answer as the table, so they cannot disagree with it.
    """
    names = sorted({package for (package, _), linked in links_alloc().items() if linked})
    return ", ".join(f"`{n}`" for n in names) or "no image"


def pinned_toolchain() -> str:
    """The channel rust-toolchain.toml pins, which ships the libraries."""
    text = (ROOT / "rust-toolchain.toml").read_text(encoding="utf-8")
    match = re.search(r'^channel\s*=\s*"([^"]+)"', text, re.MULTILINE)
    if not match:
        raise SystemExit("gen-soup: rust-toolchain.toml pins no channel")
    return match.group(1)


def toolchain_libraries() -> list[str]:
    """Section 1a: the code in the item that the Cargo graph does not show.

    `core`, `alloc` and `compiler_builtins` are no crate's dependency, so
    `cargo tree` never names them, and a register read off the graph alone
    says the item holds nothing third-party while every image links `core`
    and `compiler_builtins`, and every image whose graph declares it links
    `alloc` (F-119). The version is named from the pin and `alloc`'s images
    from the sources; nothing of the libraries themselves is measured:
    which of their functions an image links is F-119's closing work, and
    this section says so rather than counting.
    """
    channel = pinned_toolchain()
    with_alloc = links_alloc()
    lines = [
        "## 1a. The toolchain's libraries, linked into the item",
        "",
        "The Cargo graph does not show the Rust standard libraries. Every",
        "image above links `core` and `compiler_builtins`, and every image",
        "one of whose crates says `extern crate alloc` links `alloc`: today",
        f"that is {alloc_packages()}, and a binary that gained or lost the",
        "line would change this table and fail `--check`. They reach an",
        "image two ways. What is not",
        "generic is linked as the pinned toolchain ships it, compiled, for",
        "the target. What is generic (`Vec<T>`, `BTreeMap<K, V>`, the",
        "iterators) is compiled by this build, from the toolchain's source,",
        "into the item's own crates. Both are software in the item that",
        "this repository did not write, and neither is counted in the",
        "figure above. The version is named from the pin; nothing of the",
        "libraries is measured here:",
        "",
        "| Library | From | Images that link it |",
        "|---|---|---|",
    ]
    every = sorted(FIRST_PARTY)
    for name in TOOLCHAIN_LIBRARIES:
        images = [key for key in every if name != "alloc" or with_alloc[key]]
        shown = ", ".join(f"`{package}` on `{target}`" for package, target in images) or "none"
        lines.append(f"| `{name}` | rustc {channel} (`rust-toolchain.toml`) | {shown} |")
    lines += [
        "",
        "None of the item's gates reads them, no coverage figure measures",
        "them, and their anomaly lists are not evaluated, as IEC 62304 asks",
        "of SOUP. Which of their functions each image links, of each of the",
        "two kinds, each archive's digest, and",
        "whether a qualified library or the item's own measurement stands",
        "behind them, are what FINDINGS.md F-119 asks for; a qualified",
        "compiler (F-17) does not answer it.",
        "",
    ]
    return lines


def host_crates() -> list[dict]:
    """Every external crate in the workspace graph, with its metadata."""
    blob = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--all-features"],
        cwd=ROOT, capture_output=True, text=True, check=True,
    ).stdout
    meta = json.loads(blob)
    members = set(meta["workspace_members"])
    return sorted(
        (
            {
                "name": p["name"],
                "version": p["version"],
                "license": p.get("license") or "(unstated)",
                "repository": p.get("repository") or "",
            }
            for p in meta["packages"]
            if p["id"] not in members and p.get("source")
        ),
        key=lambda p: p["name"],
    )


def render(ship: dict[tuple[str, str], list[str]], host: list[dict]) -> str:
    lines = [
        "# SOUP register",
        "",
        "*Generated by `tools/common/gen/gen-soup.py`. Do not edit: run the script.*",
        "",
        "IEC 62304 §5.3.3 requires every item of software of unknown provenance",
        "in the device to be identified, and §7.1.2 requires its published",
        "anomaly list to be evaluated against the item's hazards -- which are",
        "not yet analysed (FINDINGS.md, F-20). This register is generated from the",
        "dependency graph rather than maintained by hand, because a register",
        "that is edited separately from the manifest it describes is a register",
        "that is wrong.",
        "",
        "## 1. SOUP in the certified item",
        "",
        "| Binary | Target | External crates |",
        "|---|---|---:|",
    ]
    total = 0
    for (package, target), names in ship.items():
        total += len(names)
        shown = ", ".join(names) if names else "**none**"
        lines.append(f"| `{package}` | `{target}` | {shown} |")

    lines += [
        "",
        f"**External crates in the item: {total}.** The toolchain's own",
        "libraries are in these images and are not crates of the graph:",
        "`core` and `compiler_builtins` in every one, `alloc` in the images of",
        f"{alloc_packages()}; section 1a lists them (FINDINGS.md, F-119).",
        "",
    ]
    if total == 0:
        lines += [
            "The item links no external crate. Every line in",
            "the shipping graph is first-party code in this repository, written",
            "against the design in `docs/ARCHITECTURE.md` and the model in",
            "`docs/sysml/`, under the conventions and gates this repository",
            "enforces on every commit. That is a lifecycle in practice; it is",
            "not yet one written down in the terms §5 asks for (F-28).",
            "",
            "This is worth stating plainly because it is the position Linux",
            "structurally cannot reach: a Linux-based medical device classifies",
            "the kernel itself as SOUP, since §5 lifecycle evidence cannot be",
            "produced retroactively for code developed elsewhere. The obligation",
            "that dominates a Class C submission built on Linux is, here, the",
            "toolchain's three libraries of section 1a and nothing else.",
            "",
            "It is not an accident and it is not free. `deny.toml` keeps the",
            "dependency budget near zero on purpose -- *code that runs in ring 0",
            "with no memory protection above it is code we want to have read* --",
            "and the cost is that `src/lib/` contains a hand-written ACPI parser, a",
            "device-tree parser, a btrfs implementation and a TCP stack.",
            "",
        ]
    else:
        lines += [
            "Each crate above is SOUP and needs, per §5.3.3, a documented",
            "functional and performance specification, and per §7.1.2 an",
            "evaluation of its published anomaly list against this item's",
            "hazards. **That evaluation is not yet written.**",
            "",
        ]

    lines += toolchain_libraries()

    lines += [
        "## 2. Host-side crates — tools, not SOUP",
        "",
        f"The {len(host)} external crates below are in `Cargo.lock` and build,",
        "test or package the item without being part of it. 62304 does not",
        "reach them; they are classified as tools in [TOOLS.md](TOOLS.md) under",
        "EN 50716 §6.7 and DO-330, where the question is whether a defect could",
        "put an undetected error into the product.",
        "",
        "| Crate | Version | Licence |",
        "|---|---|---|",
    ]
    for crate in host:
        lines.append(f"| `{crate['name']}` | {crate['version']} | {crate['license']} |")

    lines += [
        "",
        "Advisories against all of them are checked on every run of",
        "`cargo deny check` (`deny.toml`), whose `advisories.ignore` list is",
        "empty on purpose and whose `yanked` setting is `deny`.",
        "",
    ]
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail if stale")
    args = parser.parse_args()

    ship = shipping_externals()
    rendered = render(ship, host_crates())

    leaked = {k: v for k, v in ship.items() if v}
    if leaked:
        print(
            "gen-soup: the certified item has acquired SOUP. An external crate\n"
            "  in ring 0 is third-party code inside the trusted base, and IEC\n"
            "  62304 §7.1.2 wants its anomaly list evaluated against every\n"
            "  hazard before it ships:",
            file=sys.stderr,
        )
        for (package, target), names in leaked.items():
            print(f"    {package} [{target}]: {', '.join(names)}", file=sys.stderr)
        return 1

    if args.check:
        if not OUTPUT.exists():
            print(f"gen-soup: {OUTPUT.relative_to(ROOT)} is missing", file=sys.stderr)
            return 1
        if OUTPUT.read_text(encoding="utf-8") != rendered:
            print(
                f"gen-soup: {OUTPUT.relative_to(ROOT)} is stale. Run\n"
                f"  python3 tools/common/gen/gen-soup.py",
                file=sys.stderr,
            )
            return 1
        print("gen-soup: register current, no external crate in the item; the toolchain's libraries listed (F-119)")
        return 0

    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_text(rendered, encoding="utf-8")
    print(f"gen-soup: wrote {OUTPUT.relative_to(ROOT)}, no external crate in the item; the toolchain's libraries listed (F-119)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
