#!/usr/bin/env python3
"""Build the certified item with the Rust that Ferrocene's newest release is.

The compiler that will build the certified kernel is Ferrocene's
(docs/MICROKERNEL.md section 6.5, *The toolchain*), and Ferrocene trails
upstream Rust: its newest release, 26.05.0, is Rust 1.95.0, while the tree
pins 1.99.1 (rust-toolchain.toml). A language feature or a `core` API that
was stabilised in 1.96 to 1.99 compiles under the pin and not under
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
    `--mitigations on`), in a target directory of its own. A profile
    carrying a value whose code refuses itself on a target by
    `compile_error!` (`NO_CODE_ON`: `schedule=windows` on ARMv7-A, so
    `max`) skips that target, says so, and fails if the refusal is gone.
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
import tomllib
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
# A value whose code refuses itself on a target by `compile_error!`, so a
# profile carrying it is type-checked on the other targets alone: the key,
# the target, and the file whose refusal must still say so (the exclusion
# goes when the refusal does). `schedule=windows` has no ARMv7-A code (K9
# section 4, ledger 7549 WY.13): `max` is checked on x86-64 and AArch64.
# The refusal is its `cfg` line and the `compile_error!` line under it,
# matched together (a refusal moved under another `cfg` ends the excuse);
# the architecture must lie outside the value's `built_on` in
# build-flags.toml; and xtask's check.rs carries the same entry for its
# kernel lint (ledger 10948 N1, N2).
NO_CODE_ON = (
    ("schedule", "windows", "armv7a", "armv7a-none-eabi", "src/kernel/src/arch/armv7a/mod.rs",
     '#[cfg(ferrix_schedule = "windows")]\n'
     'compile_error!("schedule=windows is not built for ARMv7-A'),
)
FLAG_FILE = "tools/common/data/build-flags.toml"
XTASK_CHECK = "tools/common/xtask/src/check.rs"
XTASK_ARCH = {"x86_64": "X86_64", "aarch64": "AArch64", "armv7a": "Armv7a"}


def no_code_problems(flags_text: str, xtask_text: str, tree) -> list[str]:
    """What breaks `NO_CODE_ON`'s excuses: each entry's refusal gone from
    `tree(path)`, its architecture in the value's `built_on`, or xtask's
    entry missing. Empty when every excuse holds."""
    declared = tomllib.loads(flags_text)["flags"]
    problems = []
    for flag, value, arch, target, source, refusal in NO_CODE_ON:
        if refusal not in tree(source):
            problems.append(f"{flag}={value} is excused on {target} for a refusal {source} "
                            f"no longer holds (its cfg line and compile_error! together)")
        built_on = declared.get(flag, {}).get("values", {}).get(value, {}).get("built_on")
        # An absent built_on names no architecture here: such a value is in no
        # profile that builds, so the excuse is inert until it is given one.
        if built_on is not None and arch in built_on:
            problems.append(f"{flag}={value} is excused on {target}, an architecture its built_on "
                            f"in {FLAG_FILE} names ({built_on})")
        rust = f'("{flag}", "{value}", Arch::{XTASK_ARCH[arch]})'
        if rust not in xtask_text:
            problems.append(f"{XTASK_CHECK}'s NO_CODE_ON lacks {rust}: the kernel lint would differ")
    return problems
# The flags a kernel manifest keys a dependency table on: xtask's cargo.rs
# MANIFEST_FLAGS and build.rs's list, by name (check-build-flags.py holds the
# three equal).
MANIFEST_FLAGS = ("personality", "linux_abi", "board_glue")

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
    chosen_flags = dict(word.split("=", 1) for word in os.environ.get("FERRIX_BUILD_FLAGS", "").split())
    targets = list(KERNEL_TARGETS)
    problems = no_code_problems((ROOT / FLAG_FILE).read_text(encoding="utf-8"),
                                (ROOT / XTASK_CHECK).read_text(encoding="utf-8"),
                                lambda path: (ROOT / path).read_text(encoding="utf-8"))
    if problems:
        for problem in problems:
            say(f"FAILED: {problem}")
        return 1
    for flag, value, _, target, source, _ in NO_CODE_ON:
        if chosen_flags.get(flag) != value:
            continue
        targets.remove(target)
        say(f"{target}: not type-checked, {flag}={value} refuses itself there ({source})")
    say("targets: " + ", ".join(targets))

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
    for target in targets:
        arguments += ["--target", target]
    # The flags a kernel manifest keys a dependency table on, as rustc's cfgs
    # (docs/microkernel/CORE-WITHOUT-EXTENSIONS.md section 2; xtask's
    # cargo.rs gives its own builds the same): src/kernel/build.rs refuses a
    # build where they differ from FERRIX_BUILD_FLAGS.
    cfgs = ",".join(f'"--cfg","ferrix_{flag}=\\"{chosen_flags[flag]}\\""'
                    for flag in MANIFEST_FLAGS if flag in chosen_flags)
    if cfgs:
        for target in targets:
            arguments += ["--config", f"target.{target}.rustflags=[{cfgs}]"]
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
            f" [{times} of the {len(targets)} targets' builds]")
    if status != 0 or errors:
        say(f"FAILED: {len(errors)} distinct error(s) under Rust {RUST}, Ferrocene {FERROCENE_RELEASE}'s "
            f"(cargo exited {status}); a feature newer than Ferrocene's is in the item's build")
        return 1
    skipped = [target for target in KERNEL_TARGETS if target not in targets]
    said = f", {', '.join(skipped)} not checked (NO_CODE_ON)" if skipped else ""
    say(f"PASSED: the kernel and the item's crates build with Rust {RUST} on "
        f"{len(targets)} targets ({', '.join(targets)}{said}; {warnings} warning(s), not failed)")
    return 0


def self_test() -> int:
    """`NO_CODE_ON`'s judge on planted texts (ledger 10948 N1-N3): the tree's
    own holds; each plant -- the refusal's text gone, its cfg line changed
    with the sentence kept, the architecture given to `built_on`, xtask's
    entry gone -- is a problem, named."""
    source = NO_CODE_ON[0][4]
    flags = (ROOT / FLAG_FILE).read_text(encoding="utf-8")
    xtask = (ROOT / XTASK_CHECK).read_text(encoding="utf-8")
    tree_text = (ROOT / source).read_text(encoding="utf-8")
    real = {source: tree_text}
    assert no_code_problems(flags, xtask, real.__getitem__) == [], "the tree's own excuse"
    refusal = NO_CODE_ON[0][5]
    cfg_line, sentence = refusal.split("\n", 1)
    gone = {source: tree_text.replace(sentence, "compile_error!(\"elsewhere")}
    moved = {source: tree_text.replace(refusal, "#[cfg(all(ferrix_schedule = \"windows\", ferrix_checks = \"on\"))]\n" + sentence)}
    for name, tree in (("refusal gone", gone), ("cfg changed", moved)):
        problems = no_code_problems(flags, xtask, tree.__getitem__)
        assert len(problems) == 1 and "no longer holds" in problems[0], (name, problems)
    flag, value = NO_CODE_ON[0][0], NO_CODE_ON[0][1]
    line = re.compile(rf'^(values\.{value} = \{{.*?built = "[a-z]+", )(built_on = \[[^]]*\], )?', re.M)
    section = flags.index(f"[flags.{flag}]")
    head, tail = flags[:section], flags[section:]
    widened = head + line.sub(r'\1built_on = ["aarch64", "armv7a"], ', tail, count=1)
    assert widened != flags, "the planted built_on found no line of the value"
    problems = no_code_problems(widened, xtask, real.__getitem__)
    assert len(problems) == 1 and "built_on" in problems[0], problems
    problems = no_code_problems(flags, xtask.replace("Arch::Armv7a)]", "Arch::X86_64)]"), real.__getitem__)
    assert len(problems) == 1 and "NO_CODE_ON lacks" in problems[0], problems
    say("self-test: NO_CODE_ON's judge holds the tree's excuse and names each of 4 plants")
    return 0


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        sys.exit(self_test())
    sys.exit(main())
