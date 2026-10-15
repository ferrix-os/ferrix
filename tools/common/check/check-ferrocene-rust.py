#!/usr/bin/env python3
"""Build the certified item with the Rust that Ferrocene's newest release is.

The compiler that will build the certified kernel is Ferrocene's
(docs/MICROKERNEL.md section 6.5, *The toolchain*), and Ferrocene trails
upstream Rust: its newest release, 26.05.0, is Rust 1.95.0, while the tree
pins 1.97.1 (rust-toolchain.toml). A language feature or a `core` API that
was stabilised in 1.96 or 1.97 compiles under the pin and not under
Ferrocene, and it would be found the day the evidence build is made, by
which time it is in every ring. This gate finds it the day it is written:
it type-checks the kernel crate and the item's library crates for every
kernel target with that Rust (docs/roadmap/microkernel-m0.md, E).

**This is upstream Rust 1.95.0, not Ferrocene's compiler.** Ferrocene's
binaries are downloaded only with a paid account (`releases.ferrocene.dev`
answers with a login; docs/microkernel/RESEARCH-CHECK.md section 9), which
is the customer's to buy. Ferrocene 26.05.0 is built from upstream 1.95.0
(its `release/1.95` branch's `src/version` says `1.95.0`; the release post
says it "includes changes from Rust 1.93.0, 1.93.1, 1.94.0, 1.94.1 and
1.95.0"), so for the question this gate asks -- which language and library
features exist -- the two agree. What it cannot catch is anything
Ferrocene changes in its fork; that, and the qualified build itself, wait
on the subscription, and when it exists this gate runs Ferrocene's
compiler through criticalup in place of `rustup run`.

What it checks
--------------

  * `cargo check` with Rust 1.95.0 of `ferrix-kernel` and of every library
    crate `tools/common/data/certification-item.json` puts in the `core` or
    `item` ring or on its infrastructure allow-list, for
    x86_64-unknown-none, aarch64-unknown-none-softfloat and
    armv7a-none-eabi, in the reference configuration (no Cargo features,
    `--mitigations on`), in a target directory of its own.
  * **Nightly features.** The step clears `RUSTC_BOOTSTRAP`, so a
    `#![feature]` is the stable compiler's error E0554; and before any
    compiling it lists every `#![feature]` attribute and every
    `RUSTC_BOOTSTRAP` in those crates' sources and manifests and in
    `.cargo/config.toml`, and fails on any.

Every error is reported with the ring of the file it is in. The kernel is
one crate, so its core and item files cannot be compiled apart from its load
files: an error in a load file fails this gate too, and says it is the
load's.

It does not fail on warnings: lints differ between compiler versions (1.95's
dead-code analysis leaves an `#[expect(dead_code)]` in sched/work.rs
unfulfilled), and the pinned compiler's clippy is the lint gate. Warnings are
counted on the last line.

The toolchain is fetched through rustup when it is missing
(`--profile minimal` and the three targets); without rustup the step fails
rather than being skipped.

Run by `cargo xtask check` (and alone as `cargo xtask check-ferrocene`).
Exit 0 when everything builds, 1 when anything does not.
"""

from __future__ import annotations

import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent.parent
MANIFEST = ROOT / "tools" / "common" / "data" / "certification-item.json"

# The newest Ferrocene release and the upstream Rust it is built from
# (docs/microkernel/RESEARCH-CHECK.md section 4; 26.08, with Rust 1.96 to
# 1.97, was still "upcoming" in its release notes on 2026-10-14). Moved
# together, by a commit of its own, when Ferrocene releases.
FERROCENE_RELEASE = "26.05.0"
RUST = "1.95.0"

# The kernel's targets, as rust-toolchain.toml and tools/common/xtask's
# `Arch::kernel_target` name them. Ferrocene 26.05 qualifies
# `aarch64-unknown-none` alone of the three's families; the other two are
# checked because the arch/ code differs per target and a feature can hide
# behind a `cfg`.
KERNEL_TARGETS = (
    "x86_64-unknown-none",
    "aarch64-unknown-none-softfloat",
    "armv7a-none-eabi",
)

# What would make the build use another compiler, other flags or nightly
# features than the ones this gate means.
CLEARED_ENV = (
    "RUSTC_BOOTSTRAP",
    "RUSTC",
    "RUSTDOC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
    "CARGO_BUILD_RUSTC",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    "RUSTUP_TOOLCHAIN",
    "CARGO",
)

FEATURE = re.compile(r"#!\s*\[\s*feature\s*\(")


def say(line: str) -> None:
    print(f"ferrocene-rust: {line}", flush=True)


def load_boundary():
    spec = importlib.util.spec_from_file_location(
        "boundary", ROOT / "tools" / "common" / "check" / "check-item-boundary.py"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def packages(manifest: dict) -> list[tuple[str, str]]:
    """`(package, directory)` for the kernel's crates and the item's library
    crates. The kernel's are `kernel_crates`, which the item-boundary gate
    holds equal to the packages under src/kernel (docs/KERNEL-CRATES.md)."""
    kernel = {
        name: entry for name, entry in manifest.get("kernel_crates", {}).items()
        if not name.startswith("//")
    }
    if "ferrix-kernel" not in kernel:
        say("certification-item.json's kernel_crates names no ferrix-kernel; nothing to check")
        sys.exit(1)
    found = [(name, entry["path"]) for name, entry in sorted(kernel.items())]
    crates = manifest["crates"]
    for name, crate in crates["members"].items():
        if name.startswith("//"):
            continue
        if crate["ring"] in ("core", "item"):
            found.append((name, crate["path"]))
    for name in crates.get("infrastructure_allowlist", {}):
        if name.startswith("//"):
            continue
        directory = next(
            (str(p.parent.relative_to(ROOT).as_posix())
             for p in (ROOT / "src" / "lib").rglob("Cargo.toml")
             if re.search(rf'(?m)^name\s*=\s*"{re.escape(name)}"', p.read_text(encoding="utf-8"))),
            None,
        )
        if directory is None:
            say(f"cannot find the infrastructure crate {name} under src/lib")
            sys.exit(1)
        found.append((name, directory))
    return found


def nightly_uses(dirs: list[str]) -> list[str]:
    """Every `#![feature]` and `RUSTC_BOOTSTRAP` in the crates and the config."""
    hits = []
    files = [ROOT / ".cargo" / "config.toml"]
    for directory in dirs:
        base = ROOT / directory
        files.append(base / "Cargo.toml")
        files.extend(sorted(base.rglob("*.rs")))
    for path in files:
        if not path.is_file() or "target" in path.relative_to(ROOT).parts:
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        for number, line in enumerate(text.splitlines(), 1):
            stripped = line.lstrip()
            if stripped.startswith("//") or (path.suffix == ".toml" and stripped.startswith("#")):
                continue
            if "RUSTC_BOOTSTRAP" in line or (path.suffix == ".rs" and FEATURE.search(line)):
                hits.append(f"{path.relative_to(ROOT).as_posix()}:{number}: {stripped}")
    return hits


def run(arguments: list[str], **kwargs) -> subprocess.CompletedProcess:
    return subprocess.run(arguments, stdin=subprocess.DEVNULL, **kwargs)


def ensure_toolchain(rustup: str, env: dict) -> None:
    listed = run([rustup, "target", "list", "--installed", "--toolchain", RUST],
                 env=env, capture_output=True, text=True)
    if listed.returncode != 0:
        say(f"Rust {RUST} is not installed: fetching it with rustup (--profile minimal)")
        arguments = [rustup, "toolchain", "install", RUST, "--profile", "minimal", "--no-self-update"]
        for target in KERNEL_TARGETS:
            arguments += ["--target", target]
        if run(arguments, env=env).returncode != 0:
            say(f"FAILED: rustup could not install Rust {RUST}")
            sys.exit(1)
        return
    have = set(listed.stdout.split())
    missing = [t for t in KERNEL_TARGETS if t not in have]
    if missing:
        say(f"Rust {RUST} lacks {', '.join(missing)}: adding with rustup")
        if run([rustup, "target", "add", "--toolchain", RUST, *missing], env=env).returncode != 0:
            say(f"FAILED: rustup could not add {', '.join(missing)} to Rust {RUST}")
            sys.exit(1)


def ring_of_file(path: str, kernel_rings: dict[str, str], crate_rings: list[tuple[str, str]]) -> str:
    if path.startswith("src/kernel/src/"):
        return kernel_rings.get(path[len("src/kernel/src/"):], "unclassified") + " ring"
    for directory, ring in crate_rings:
        if path.startswith(directory.rstrip("/") + "/"):
            return ring
    return "outside the item's crates"


def main() -> int:
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    boundary = load_boundary()
    kernel_rings, _, _ = boundary.classify(manifest, boundary.kernel_files())
    chosen = packages(manifest)
    crate_rings = []
    for name, directory in chosen[1:]:
        crate = manifest["crates"]["members"].get(name)
        crate_rings.append((directory, f"{crate['ring']} crate {name}" if crate else f"infrastructure crate {name}"))

    say(f"Rust {RUST}, the upstream release Ferrocene {FERROCENE_RELEASE} is built from "
        f"(not Ferrocene's qualified binary, which needs the customer's subscription)")
    say("packages: " + ", ".join(name for name, _ in chosen))
    say("targets: " + ", ".join(KERNEL_TARGETS))

    nightly = nightly_uses([directory for _, directory in chosen])
    if nightly:
        for hit in nightly:
            say(f"nightly: {hit}")
        say(f"FAILED: {len(nightly)} nightly-only feature use(s) or RUSTC_BOOTSTRAP in the item's crates; "
            f"Ferrocene is a stable compiler")
        return 1

    rustup = shutil.which("rustup")
    if rustup is None:
        say(f"FAILED: rustup is not on PATH, and this step needs it to run Rust {RUST}")
        return 1
    env = {k: v for k, v in os.environ.items() if k not in CLEARED_ENV}
    ensure_toolchain(rustup, env)

    target_dir = Path(os.environ.get("CARGO_TARGET_DIR") or ROOT / "target") / "ferrocene-rust"
    arguments = [rustup, "run", RUST, "cargo", "check", "--locked", "--ignore-rust-version",
                 "--message-format=json", "--target-dir", str(target_dir)]
    for name, _ in chosen:
        arguments += ["-p", name]
    for target in KERNEL_TARGETS:
        arguments += ["--target", target]
    process = subprocess.Popen(arguments, cwd=ROOT, env=env, stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, text=True, encoding="utf-8", errors="replace")
    errors: dict[tuple[str, str], int] = {}
    warnings = 0
    assert process.stdout is not None
    for line in process.stdout:
        if not line.startswith("{"):
            print(line, end="")
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            print(line, end="")
            continue
        if message.get("reason") != "compiler-message":
            continue
        diagnostic = message["message"]
        target = message.get("target", {}).get("name", "?")
        level = diagnostic.get("level", "")
        if level == "warning":
            warnings += 1
            continue
        if not level.startswith("error"):
            continue
        spans = [s for s in diagnostic.get("spans", []) if s.get("is_primary")]
        where = f"{spans[0]['file_name']}:{spans[0]['line_start']}" if spans else f"(no location, {target})"
        key = (where, diagnostic.get("message", ""))
        if key not in errors:
            print(diagnostic.get("rendered") or diagnostic.get("message", ""), end="", flush=True)
        errors[key] = errors.get(key, 0) + 1
    status = process.wait()

    for (where, text), times in errors.items():
        path = where.rsplit(":", 1)[0]
        say(f"{where} ({ring_of_file(path, kernel_rings, crate_rings)}): {text}"
            f" [{times} of the {len(KERNEL_TARGETS)} targets' builds]")
    if status != 0 or errors:
        say(f"FAILED: {len(errors)} distinct error(s) under Rust {RUST}, Ferrocene {FERROCENE_RELEASE}'s "
            f"(cargo exited {status}); a feature newer than Ferrocene's is in the item's build")
        return 1
    say(f"PASSED: the kernel and the item's crates build with Rust {RUST} on "
        f"{len(KERNEL_TARGETS)} targets ({warnings} warning(s), not failed)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
