#!/usr/bin/env python3
"""The gate's half of docs/BUILD-FLAGS.md rule 6: compare a kernel's `build`
boot line with the profile file it was built from.

Every Ferrix kernel prints, early in its boot, one line computed from the
`cfg`s it was compiled with (`src/kernel/src/build_flags.rs`):

    build    profile=present level=present personality=kernel fs=kernel ...

This script reads that line from a boot's serial output and compares it,
word for word, with the profile file in tools/common/data/profiles/ that the
run named. It reads the profile with Python's own TOML parser, never anything
xtask printed or resolved (BF3 a): the resolver that chose the `cfg`s is a T2
tool, and this comparison is what keeps it one. The line must name exactly
the profile, its level and every flag the flag file declares, each with the
profile's value. A build changed by `--set` prints `profile=none level=none`
and differs, so it is refused (BF3 c); with --no-claim the run is an ad-hoc
one by its own say, and the line must be the profile's with exactly those
`--set`s applied, `profile=none level=none` and `no claim`, and passes as a
run that carries no claim.

`desktop`'s line must also name its light values -- program_barrier=session,
ssbd=on_request and runtime_trades=game_mode -- so they show on every desktop
boot (DF8), whatever the profile file says.

On a match it prints the kernel's sha256 beside the profile, which ties the
image the run booted to the comparison it passed.

    check-boot-line.py --profile present --arch x86_64 --log SERIAL --kernel ELF
    check-boot-line.py --profile present ... --no-claim --set mitigations=off
    check-boot-line.py --self-test
"""

from __future__ import annotations

import argparse
import hashlib
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent.parent
FLAGS = ROOT / "tools" / "common" / "data" / "build-flags.toml"
PROFILES = ROOT / "tools" / "common" / "data" / "profiles"

# The kernel's line: `build` then the words, wherever the console put it.
LINE = re.compile(r"(?:^|\s)build\s+(profile=\S+(?:\s+\S+)*)\s*$")
NO_CLAIM = "; no claim"

# DF8: the light values every desktop boot names.
DESKTOP_LIGHT = {
    "program_barrier": "session",
    "ssbd": "on_request",
    "runtime_trades": "game_mode",
}


def expected(profile: str, sets: list[tuple[str, str]], flag_file: dict, profile_file: dict) -> dict[str, str]:
    """The words the line must hold, from the files alone."""
    if profile_file.get("profile") != profile:
        raise ValueError(f"profiles/{profile}.toml names itself `{profile_file.get('profile')}`")
    values = profile_file.get("flags", {})
    words = {"profile": profile, "level": profile_file.get("level", "?")}
    for flag in flag_file.get("flags", {}):
        if flag not in values:
            raise ValueError(f"profile {profile} does not set {flag}")
        words[flag] = values[flag]
    changed = False
    for flag, value in sets:
        if flag not in words or flag in ("profile", "level"):
            raise ValueError(f"--set {flag}: no such flag")
        if words[flag] != value:
            words[flag] = value
            changed = True
    if changed:
        words["profile"] = "none"
        words["level"] = "none"
    return words


def found(lines: list[str]) -> tuple[dict[str, str], bool] | None:
    """The last `build` line's words, and whether it said `no claim`."""
    for line in reversed(lines):
        match = LINE.search(line.rstrip())
        if not match:
            continue
        text = match.group(1)
        claim_none = text.endswith(NO_CLAIM)
        if claim_none:
            text = text[: -len(NO_CLAIM)]
        words: dict[str, str] = {}
        for word in text.split():
            name, sep, value = word.partition("=")
            if not sep or name in words:
                return {"?": word}, claim_none
            words[name] = value
        return words, claim_none
    return None


def compare(want: dict[str, str], got: dict[str, str], claim_none: bool, no_claim: bool) -> list[str]:
    """What differs, as `name=got (profile: want)`; empty when the line matches."""
    problems = []
    for name, value in want.items():
        if got.get(name) != value:
            problems.append(f"{name}={got.get(name, '(absent)')} (expected {value})")
    for name in got:
        if name not in want:
            problems.append(f"{name}={got[name]} (no such flag)")
    if got.get("profile") == "none" and not no_claim:
        problems.append(
            "an ad-hoc build, which carries no claim: the gate refuses it as its profile's run "
            "(docs/BUILD-FLAGS.md rule 5, BF3 c; --no-claim boots it as an ad-hoc run)"
        )
    if claim_none != (want.get("profile") == "none"):
        problems.append("`no claim` is said exactly when profile=none")
    if got.get("profile") == "desktop" or want.get("profile") == "desktop":
        for name, value in DESKTOP_LIGHT.items():
            if got.get(name) != value:
                problems.append(f"desktop's line does not name its light value {name}={value} (DF8)")
    return problems


def self_test() -> list[str]:
    failures = []
    flag_file = {"flags": {"a": {}, "b": {}}}
    prof = {"profile": "p", "level": "l", "flags": {"a": "x", "b": "y"}}
    want = expected("p", [], flag_file, prof)
    line = ["  1.0 | Ferrix", "  1.1 |   build    profile=p level=l a=x b=y"]
    got, none = found(line) or ({}, False)
    if compare(want, got, none, False):
        failures.append("a matching line was refused")
    got, none = found(["  build    profile=none level=none a=z b=y ; no claim"]) or ({}, False)
    if not compare(want, got, none, False):
        failures.append("an ad-hoc line was accepted as the profile's")
    adhoc = expected("p", [("a", "z")], flag_file, prof)
    if compare(adhoc, got, none, True):
        failures.append("an ad-hoc line was refused under --no-claim with its own --set")
    got, none = found(["  build    profile=p level=l a=x"]) or ({}, False)
    if not compare(want, got, none, False):
        failures.append("a line missing a flag was accepted")
    desk = {"profile": "desktop", "level": "none", "flags": {"a": "x", "b": "y"}}
    got, none = found(["  build    profile=desktop level=none a=x b=y"]) or ({}, False)
    if not compare(expected("desktop", [], flag_file, desk), got, none, False):
        failures.append("a desktop line without its light values was accepted")
    if found(["no line here"]) is not None:
        failures.append("a line was found where there is none")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--profile", default="present")
    parser.add_argument("--arch", default="?")
    parser.add_argument("--log", type=Path)
    parser.add_argument("--kernel", type=Path)
    parser.add_argument("--set", action="append", default=[], dest="sets")
    parser.add_argument("--no-claim", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    failures = self_test()
    if failures:
        for failure in failures:
            print(f"boot-line: self-test: {failure}", file=sys.stderr)
        return 1
    if args.self_test:
        print("boot-line: self-tests pass")
        return 0
    if args.log is None:
        parser.error("--log is required")

    sets = []
    for pair in args.sets:
        flag, sep, value = pair.partition("=")
        if not sep:
            parser.error(f"--set wants flag=value, not {pair}")
        sets.append((flag.strip(), value.strip()))
    path = PROFILES / f"{args.profile}.toml"
    try:
        with FLAGS.open("rb") as handle:
            flag_file = tomllib.load(handle)
        with path.open("rb") as handle:
            profile_file = tomllib.load(handle)
        want = expected(args.profile, sets, flag_file, profile_file)
    except (OSError, ValueError, tomllib.TOMLDecodeError) as error:
        print(f"boot-line: REFUSED: {args.arch}: {error}", file=sys.stderr)
        return 1

    lines = args.log.read_text(encoding="utf-8", errors="replace").splitlines()
    result = found(lines)
    if result is None:
        print(f"boot-line: REFUSED: {args.arch}: the kernel printed no `build` line ({args.log})", file=sys.stderr)
        return 1
    got, claim_none = result
    problems = compare(want, got, claim_none, args.no_claim)
    if problems:
        print(
            f"boot-line: REFUSED: {args.arch}: the kernel's build line differs from profile "
            f"{args.profile} ({path.relative_to(ROOT)}): " + "; ".join(problems),
            file=sys.stderr,
        )
        return 1
    digest = hashlib.sha256(args.kernel.read_bytes()).hexdigest() if args.kernel else "(no kernel given)"
    claim = (
        "an ad-hoc run, no claim"
        if want["profile"] == "none"
        else f"profile {args.profile} (level {want['level']})"
    )
    print(
        f"boot-line: {args.arch}: the kernel's build line matches {claim}, word for word; "
        f"kernel sha256 {digest}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
