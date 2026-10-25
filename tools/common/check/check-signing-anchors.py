#!/usr/bin/env python3
"""Validate the signing anchors, and refuse a development anchor where it
must not be (docs/SIGNING.md §6, §6.4; the consultant's D1, ledger 3731).

    python3 tools/common/check/check-signing-anchors.py               # the file
    python3 tools/common/check/check-signing-anchors.py --scan PATH.. # files
    python3 tools/common/check/check-signing-anchors.py --self-test

The file, `tools/common/data/signing-anchors.toml`, must hold:

* only `[release.<name>]` and `[development.<name>]` tables, each with a
  kind (slot, config or driver), an anchor id and a public key;
* release ids 0 or 1 and development ids 32768 (0x8000) and up, every
  (kind, id) once;
* each public key 52 bytes of hex: `L` 1 or 2, an LMS type of the product
  set (M24_H10, H15, H20) and `LMOTS_SHA256_N24_W4` (§2.1);
* no key in both sections, and no key twice.

It also refuses a workspace package that depends on the soft signer as
more than a dev-dependency, and any path in a kernel package's normal and
build dependency closure (`cargo metadata`, every platform) to the
verifier or the signer (§5.1, §5.2); xtask refuses a kernel image that
holds either's name besides (`tools/common/xtask/src/signing.rs`).

It also validates the hardware-family registry,
`tools/common/data/hardware-families.toml` (§1.4): every id once, in its
arch's range, one generic family per arch, and a recognition rule each.

`--scan` refuses every file that holds a development key's tree root `T`
(its last 24 bytes, which any compiled anchor table carries whatever its
layout): what a claimed image's files must never hold. xtask runs the same refusal on the
files it puts into a claimed profile's image (`tools/common/xtask/src/
signing.rs`); this is the same rule for any file at hand, a release
artifact included.

`--self-test` plants a development key in a scratch file and requires the
refusal, and plants each malformed anchor and requires its refusal.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import subprocess
import sys
import tempfile
import datetime
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[3]
ANCHORS = ROOT / "tools/common/data/signing-anchors.toml"
FAMILIES = ROOT / "tools/common/data/hardware-families.toml"
ARCHES = {"x86_64": 0x01, "aarch64": 0x02, "armv7a": 0x03}
KINDS = {"slot", "config", "driver"}
PRODUCT_LMS = {0x0B, 0x0C, 0x0D}
LMOTS_N24_W4 = 0x07
DEVELOPMENT_MIN = 0x8000
# The CI development keys' block of ids (docs/SIGNING.md §6.5; ledger 4005 (6)).
CI_MIN = 0x9000
# The one workflow that holds a CI key, the secret's name and its environment.
CI_WORKFLOW = ".github/workflows/signing-ci.yml"
CI_SECRET = "FERRIX_CI_SIGNING_KEY"
CI_ENVIRONMENT = "signing-ci"
# How long a CI key lives: strictly less than the signature logs' artifacts
# are kept (90 days), so the audit covers every log since the key was made
# (4005 (4); 4188 (4)).
CI_DAYS = 85


def problems_of(data: dict) -> list[str]:
    out = []
    seen_ids = set()
    keys: dict[bytes, str] = {}
    for section in data:
        if section not in ("release", "development"):
            out.append(f"[{section}] is neither release nor development")
    for section in ("release", "development"):
        for name, anchor in data.get(section, {}).items():
            where = f"[{section}.{name}]"
            kind = anchor.get("kind")
            ident = anchor.get("anchor_id")
            hexkey = anchor.get("public_key", "")
            if kind not in KINDS:
                out.append(f"{where}: kind {kind!r} is not slot, config or driver")
            if not isinstance(ident, int):
                out.append(f"{where}: anchor_id is not an integer")
                continue
            if section == "release" and ident not in (0, 1):
                out.append(f"{where}: a release anchor id is 0 or 1, not {ident}")
            if section == "development" and not DEVELOPMENT_MIN <= ident <= 0xFFFF:
                out.append(f"{where}: a development anchor id is 32768 to 65535, not {ident}")
            if (kind, ident) in seen_ids:
                out.append(f"{where}: kind {kind} id {ident} is given twice")
            seen_ids.add((kind, ident))
            if not isinstance(hexkey, str) or not re.fullmatch(r"(?:[0-9a-f]{2})*", hexkey):
                out.append(f"{where}: public_key is not lower-case hex without spaces")
                continue
            key = bytes.fromhex(hexkey)
            if len(key) != 52:
                out.append(f"{where}: public_key is {len(key)} bytes, not 52")
                continue
            levels = int.from_bytes(key[0:4], "big")
            lms = int.from_bytes(key[4:8], "big")
            ots = int.from_bytes(key[8:12], "big")
            if levels not in (1, 2) or lms not in PRODUCT_LMS or ots != LMOTS_N24_W4:
                out.append(f"{where}: L {levels}, LMS type {lms:#x}, LM-OTS type {ots:#x} is not the product set")
            if key in keys:
                out.append(f"{where}: the same key as {keys[key]}")
            keys[key] = where
            if section == "development":
                out += ci_anchor_problems(where, anchor)
    return out


def ci_anchor_problems(where: str, anchor: dict, today: datetime.date | None = None) -> list[str]:
    """A CI key's entry (ledger 4005 (6), (4)): ids at or above 0x9000 are
    CI's alone, made "CI", for the one workflow, created and expiring no more
    than CI_DAYS apart, and not expired."""
    today = today or datetime.date.today()
    ident = anchor.get("anchor_id", 0)
    ci = anchor.get("made") == "CI"
    out = []
    if ci != (ident >= CI_MIN):
        out.append(f"{where}: anchor ids {CI_MIN} (0x9000) and up are CI keys, made = \"CI\", and only they")
    if not ci:
        return out
    if anchor.get("workflow") != CI_WORKFLOW:
        out.append(f"{where}: a CI key's workflow is {CI_WORKFLOW}, not {anchor.get('workflow')!r}")
    try:
        created = datetime.date.fromisoformat(anchor.get("created", ""))
        expires = datetime.date.fromisoformat(anchor.get("expires", ""))
    except ValueError:
        return out + [f"{where}: a CI key names its created and expires dates (YYYY-MM-DD)"]
    if (expires - created).days > CI_DAYS or expires < created:
        out.append(f"{where}: a CI key lives at most {CI_DAYS} days, the logs' retention")
    if today > expires:
        out.append(f"{where}: the CI key expired on {expires}: rotate it (docs/SIGNING.md 6.5)")
    return out


def workflow_problems(files: dict[str, str]) -> list[str]:
    """The CI key's workflow rules (ledger 4005 (1), (2), (5), (7)), over the
    workflow files' texts by path: only CI_WORKFLOW names the secret or the
    environment; it runs on pushes to develop and main only; one job holds
    the environment, with no matrix; the secret is one step's env, and
    nothing traces commands."""
    out = []
    for path, text in sorted(files.items()):
        if path != CI_WORKFLOW and (CI_SECRET in text or CI_ENVIRONMENT in text):
            out.append(f"{path}: names the CI key or its environment; only {CI_WORKFLOW} may (4005 (7))")
    text = files.get(CI_WORKFLOW)
    if text is None:
        return out + [f"{CI_WORKFLOW} is missing, and a CI key names it"]
    code = "\n".join(line.split("#", 1)[0].rstrip() for line in text.splitlines())
    for trigger in ("pull_request", "pull_request_target", "workflow_run", "workflow_dispatch", "schedule", "repository_dispatch", "workflow_call"):
        if re.search(rf"^\s*{trigger}\s*:|[\[,]\s*{trigger}\s*[\],]", code, re.M):
            out.append(f"{CI_WORKFLOW}: the trigger {trigger} is a path to the key; only pushes to develop and main may run it (4005 (2))")
    # An allowlist (4188 (1)): one top-level `on:`, whose block is exactly a
    # push to develop and main, the next non-blank line at column 0.
    lines = code.splitlines()
    tops = [n for n, line in enumerate(lines) if re.match(r"""^(on|"on"|'on'|true|yes|"true"|'true'):""", line)]
    block = []
    if len(tops) == 1 and lines[tops[0]] == "on:":
        for line in lines[tops[0] + 1:]:
            if not line.strip():
                continue
            if not line.startswith(" "):
                break
            block.append(line)
    if block != ["  push:", "    branches: [develop, main]"]:
        out.append(f"{CI_WORKFLOW}: `on:` must be exactly a push to [develop, main], and nothing else (4005 (2); 4188 (1))")
    if not re.search(r"^defaults:\n  run:\n    shell: bash$", code, re.M) or re.search(r"^\s+shell:(?! bash$)", code, re.M):
        out.append(f"{CI_WORKFLOW}: every step runs under `shell: bash` (-eo pipefail), set in defaults.run (4188 (3))")
    for line in code.splitlines():
        used = re.search(r"\buses:\s*(\S+)", line)
        if used and not re.fullmatch(r"[\w.-]+/[\w.-]+(/[\w./-]+)?@[0-9a-f]{40}", used.group(1)):
            out.append(f"{CI_WORKFLOW}: `{used.group(1)}` is not pinned to a commit (4188 (2))")
    if re.search(r"toJSON\(\s*secrets", code) or re.search(r"\bsecrets\s*\[", code):
        out.append(f"{CI_WORKFLOW}: no toJSON(secrets) or secrets[...]: only the one step's env names a secret (4188 (2))")
    if len(re.findall(r"\bsecrets\.", code)) != 1:
        out.append(f"{CI_WORKFLOW}: exactly one `secrets.` reference, the signing step's env (4188 (2))")
    if len(re.findall(rf"^\s+environment:\s*{CI_ENVIRONMENT}\s*$", code, re.M)) != 1:
        out.append(f"{CI_WORKFLOW}: exactly one job has environment {CI_ENVIRONMENT} (4005 (1))")
    if re.search(r"^\s+(strategy|matrix):", code, re.M):
        out.append(f"{CI_WORKFLOW}: no matrix: one holder per run attempt (4005 (1))")
    uses = [line for line in code.splitlines() if CI_SECRET in line]
    if len(uses) != 1 or not any(re.fullmatch(rf"\s{{10}}{CI_SECRET}: \$\{{\{{ secrets\.{CI_SECRET} \}}\}}", u) for u in uses):
        out.append(f"{CI_WORKFLOW}: the key is one step's env, `{CI_SECRET}: ${{{{ secrets.{CI_SECRET} }}}}`, and named nowhere else (4005 (5))")
    if re.search(r"set -x|set -o xtrace|ACTIONS_STEP_DEBUG|ACTIONS_RUNNER_DEBUG", code):
        out.append(f"{CI_WORKFLOW}: no command tracing or debug logging (4005 (5))")
    return out


def workflow_files() -> dict[str, str]:
    return {
        str(p.relative_to(ROOT)): p.read_text()
        for p in sorted((ROOT / ".github/workflows").glob("*.y*ml"))
    }


def family_problems(data: dict) -> list[str]:
    """The hardware-family registry (SIGNING.md §1.4): every id once, its
    high byte its arch's, exactly one generic family per arch with low byte
    0x01, and a recognition rule for each."""
    out = []
    ids = {}
    generic = {}
    for section in data:
        if section != "family":
            out.append(f"[{section}] is not [family.*]")
    for name, fam in data.get("family", {}).items():
        where = f"[family.{name}]"
        ident, arch = fam.get("id"), fam.get("arch")
        if arch not in ARCHES:
            out.append(f"{where}: arch {arch!r} is not x86_64, aarch64 or armv7a")
            continue
        if not isinstance(ident, int) or not 0 < ident <= 0xFFFF:
            out.append(f"{where}: id is not an integer of 1 to 65535")
            continue
        if ident >> 8 != ARCHES[arch]:
            out.append(f"{where}: id {ident:#06x} is not in {arch}'s range {ARCHES[arch]:#04x}xx")
        if ident in ids:
            out.append(f"{where}: id {ident:#06x} is also {ids[ident]}'s")
        ids[ident] = name
        if not fam.get("recognised_by"):
            out.append(f"{where}: no recognised_by rule")
        if fam.get("generic") is True:
            if ident & 0xFF != 0x01:
                out.append(f"{where}: a generic family's low byte is 0x01")
            if arch in generic:
                out.append(f"{where}: {arch} has a second generic family besides {generic[arch]}")
            generic[arch] = name
    for arch in ARCHES:
        if arch not in generic:
            out.append(f"{arch} has no generic family")
    return out


SIGNER = "ferrix-softsign"
VERIFIER = {"ferrix-sigverify", "ferrix-softsign"}
DEP_TABLES = ("dependencies", "build-dependencies")


def normal_dependencies(manifest: dict) -> set[str]:
    """Every package a manifest names as a normal or build dependency,
    target-specific ones included (dev-dependencies are host tests)."""
    names = set()
    tables = [manifest] + list(manifest.get("target", {}).values())
    for table in tables:
        for kind in DEP_TABLES:
            for key, value in table.get(kind, {}).items():
                names.add(value.get("package", key) if isinstance(value, dict) else key)
    return names


def closure_problems(graph: dict[str, list[str]], kernel: list[str], names: dict[str, str]) -> list[str]:
    """Walk every kernel package's normal and build dependency closure
    (`graph`: package id to the ids it depends on, dev edges left out) and
    refuse any path to the verifier or the signer, naming it (ledger 3843
    (1))."""
    out = []
    for start in kernel:
        seen = {start: None}
        queue = [start]
        while queue:
            node = queue.pop(0)
            for dep in graph.get(node, []):
                if dep in seen:
                    continue
                seen[dep] = node
                if names.get(dep) in VERIFIER:
                    path, at = [], dep
                    while at is not None:
                        path.append(names.get(at, at))
                        at = seen[at]
                    out.append("a kernel package reaches " + names[dep] + ": " + " -> ".join(reversed(path)))
                    continue
                queue.append(dep)
    return out


def metadata_graph() -> tuple[dict[str, list[str]], list[str], dict[str, str]]:
    """The workspace's resolved graph from `cargo metadata` (no build):
    normal and build edges only, every platform's."""
    # Offline first, as a check should be; where the registry's cache lacks a
    # crate the lock file names (a fresh CI runner), resolving needs the
    # index, so cargo is asked again online. The graph is the lock file's
    # either way.
    command = ["cargo", "metadata", "--format-version", "1"]
    ran = subprocess.run(
        command + ["--offline"], cwd=ROOT, capture_output=True, text=True,
    )
    if ran.returncode != 0:
        ran = subprocess.run(command, cwd=ROOT, capture_output=True, text=True)
    if ran.returncode != 0:
        sys.exit(f"check-signing-anchors: cargo metadata failed:\n{ran.stderr}")
    meta = json.loads(ran.stdout)
    names = {p["id"]: p["name"] for p in meta["packages"]}
    kernel = [
        p["id"] for p in meta["packages"]
        if pathlib.Path(p["manifest_path"]).resolve().is_relative_to(ROOT / "src/kernel")
    ]
    graph = {}
    for node in meta["resolve"]["nodes"]:
        graph[node["id"]] = [
            dep["pkg"] for dep in node.get("deps", [])
            if any(kind.get("kind") in (None, "build") for kind in dep.get("dep_kinds", []))
        ]
    return graph, kernel, names


def dependency_problems(manifests: dict[str, dict]) -> list[str]:
    """No package depends on the soft signer but as a dev-dependency (no
    image can link it, docs/SIGNING.md §5.1). The kernel's rule, no path at
    all to the verifier or the signer, is [closure_problems]."""
    out = []
    for path, manifest in sorted(manifests.items()):
        deps = normal_dependencies(manifest)
        if SIGNER in deps:
            out.append(f"{path} depends on {SIGNER}, the soft signer, as more than a dev-dependency")
    return out


def workspace_manifests() -> dict[str, dict]:
    root = tomllib.loads((ROOT / "Cargo.toml").read_text())
    out = {}
    for member in root.get("workspace", {}).get("members", []):
        path = ROOT / member / "Cargo.toml"
        if path.exists():
            out[f"{member}/Cargo.toml"] = tomllib.loads(path.read_text())
    return out


def development_keys(data: dict) -> list[tuple[str, bytes]]:
    """Each development key's tree root `T`, its last 24 bytes: the part
    no anchor table can leave out, whatever layout it gives `L`, the type
    codes and `I` (ledger 3843 (3)). S4's tables keep `T` contiguous."""
    out = []
    for name, anchor in data.get("development", {}).items():
        try:
            key = bytes.fromhex(anchor.get("public_key", ""))
        except ValueError:
            continue
        if len(key) == 52:
            out.append((name, key[28:]))
    return out


def scan(paths: list[pathlib.Path], keys: list[tuple[str, bytes]]) -> list[str]:
    hits = []
    for path in paths:
        files = [p for p in path.rglob("*") if p.is_file()] if path.is_dir() else [path]
        for f in files:
            data = f.read_bytes()
            for name, key in keys:
                if key in data:
                    hits.append(f"{f} holds the development key {name}")
    return hits


def self_test() -> int:
    data = tomllib.loads(ANCHORS.read_text())
    keys = development_keys(data)
    with tempfile.TemporaryDirectory(prefix="check-signing-anchors-") as scratch:
        clean = pathlib.Path(scratch) / "clean.img"
        clean.write_bytes(b"\0" * 4096)
        planted = pathlib.Path(scratch) / "planted.img"
        planted.write_bytes(b"\0" * 100 + keys[0][1] + b"\0" * 100)
        if len(keys[0][1]) != 24:
            print("signing-anchors: self-test: the search is not for T alone", file=sys.stderr)
            return 1
        if scan([clean], keys) or len(scan([planted], keys)) != 1:
            print("signing-anchors: self-test: a planted development key was NOT refused", file=sys.stderr)
            return 1
    bad = {
        "a key with spaces": {"development": {"x": {"kind": "slot", "anchor_id": 32768, "public_key": "00 " * 52}}},
        "a non-product type code": {"development": {"x": {"kind": "slot", "anchor_id": 32768, "public_key": "00000001" + "0000000e" + "00000007" + "00" * 40}}},
        "a stray table": {"other": {}},
        "release id 2": {"release": {"x": {"kind": "slot", "anchor_id": 2, "public_key": "00" * 52}}},
        "development id 1": {"development": {"x": {"kind": "slot", "anchor_id": 1, "public_key": "00" * 52}}},
        "unknown kind": {"development": {"x": {"kind": "boot", "anchor_id": 32768, "public_key": "00" * 52}}},
        "short key": {"development": {"x": {"kind": "slot", "anchor_id": 32768, "public_key": "00" * 51}}},
    }
    for why, case in bad.items():
        if not problems_of(case):
            print(f"signing-anchors: self-test: {why} was NOT refused", file=sys.stderr)
            return 1
    families = tomllib.loads(FAMILIES.read_text())
    for why, tamper in {
        "a duplicated id": lambda f: f["family"].__setitem__("dup", dict(f["family"]["x86_64-n150"])),
        "an id outside its arch": lambda f: f["family"]["x86_64-n150"].__setitem__("id", 0x0202),
        "a second generic family": lambda f: f["family"]["x86_64-n150"].__setitem__("generic", True),
    }.items():
        copy = tomllib.loads(FAMILIES.read_text())
        tamper(copy)
        if not family_problems(copy):
            print(f"signing-anchors: self-test: {why} in the family registry was NOT refused", file=sys.stderr)
            return 1
    if family_problems(families):
        print("signing-anchors: self-test: the real family registry is refused", file=sys.stderr)
        return 1
    planted = {
        "src/user/system/native/devmgr/Cargo.toml": {"dependencies": {"ferrix-softsign": {"path": "x"}}},
        "src/lib/crypto/sigverify/Cargo.toml": {"dev-dependencies": {"ferrix-softsign": {"path": "x"}}},
    }
    if len(dependency_problems(planted)) != 1:
        print("signing-anchors: self-test: a planted signer dependency was NOT refused", file=sys.stderr)
        return 1
    names = {"k": "ferrix-kernel-load", "lib": "ferrix-something", "v": "ferrix-sigverify", "s": "ferrix-sha256"}
    indirect = {"k": ["lib"], "lib": ["s", "v"]}
    found = closure_problems(indirect, ["k"], names)
    if found != ["a kernel package reaches ferrix-sigverify: ferrix-kernel-load -> ferrix-something -> ferrix-sigverify"]:
        print(f"signing-anchors: self-test: an indirect kernel path to the verifier was NOT refused ({found})", file=sys.stderr)
        return 1
    if closure_problems({"k": ["lib"], "lib": ["s"]}, ["k"], names):
        print("signing-anchors: self-test: a clean kernel closure was refused", file=sys.stderr)
        return 1
    shared = {"release": {"a": dict(data["development"]["slot"], anchor_id=0)}, "development": data["development"]}
    if not any("the same key" in p for p in problems_of(shared)):
        print("signing-anchors: self-test: a development key in release was NOT refused", file=sys.stderr)
        return 1
    ci_anchor = {"kind": "slot", "anchor_id": 0x9000, "public_key": "00" * 52, "made": "CI",
                 "created": "2026-10-25", "expires": "2027-01-18", "workflow": CI_WORKFLOW}
    on_day = datetime.date(2026, 11, 1)
    if ci_anchor_problems("[x]", ci_anchor, on_day):
        print("signing-anchors: self-test: a good CI anchor was refused", file=sys.stderr)
        return 1
    for why, edit, day in [
        ("a CI key below 0x9000", {"anchor_id": 0x8003}, on_day),
        ("a non-CI key at 0x9000", {"made": "nazuna"}, on_day),
        ("another workflow", {"workflow": ".github/workflows/ci.yml"}, on_day),
        ("a life past 85 days", {"expires": "2027-01-19"}, on_day),
        ("an expired key", {}, datetime.date(2027, 1, 19)),
        ("no dates", {"created": "soon"}, on_day),
    ]:
        if not ci_anchor_problems("[x]", dict(ci_anchor, **edit), day):
            print(f"signing-anchors: self-test: {why} was NOT refused", file=sys.stderr)
            return 1
    good = (ROOT / CI_WORKFLOW).read_text() if (ROOT / CI_WORKFLOW).exists() else None
    if good is None or workflow_problems({CI_WORKFLOW: good}):
        print("signing-anchors: self-test: the real CI workflow is refused or missing", file=sys.stderr)
        return 1
    for why, files in {
        "a second workflow naming the key": {CI_WORKFLOW: good, ".github/workflows/x.yml": f"env:\n  A: ${{{{ secrets.{CI_SECRET} }}}}\n"},
        "a renamed workflow": {".github/workflows/signing.yml": good},
        "a pull_request trigger": {CI_WORKFLOW: good.replace("on:\n  push:", "on:\n  pull_request:\n  push:")},
        "a workflow_dispatch trigger": {CI_WORKFLOW: good.replace("    branches: [develop, main]\n", "    branches: [develop, main]\n  workflow_dispatch:\n")},
        "another branch": {CI_WORKFLOW: good.replace("[develop, main]", "[develop, main, '**']")},
        "a matrix": {CI_WORKFLOW: good.replace("    runs-on: ubuntu-latest\n", "    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        n: [1, 2]\n", 1)},
        "a second job in the environment": {CI_WORKFLOW: good + f"  again:\n    runs-on: ubuntu-latest\n    environment: {CI_ENVIRONMENT}\n    steps: []\n"},
        "the key in a job's env": {CI_WORKFLOW: good.replace("    environment: signing-ci\n", f"    environment: signing-ci\n    env:\n      X: ${{{{ secrets.{CI_SECRET} }}}}\n")},
        "set -x": {CI_WORKFLOW: good.replace("        run: |\n          target/release/ferrix-softsign ci-sign", "        run: |\n          set -x\n          target/release/ferrix-softsign ci-sign")},
        "pull_request_target after push": {CI_WORKFLOW: good.replace("    branches: [develop, main]\n", "    branches: [develop, main]\n  pull_request_target:\n")},
        "tags after branches": {CI_WORKFLOW: good.replace("    branches: [develop, main]\n", "    branches: [develop, main]\n    tags: ['*']\n")},
        "a second top-level on": {CI_WORKFLOW: good + "\n'on':\n  pull_request:\n"},
        "toJSON(secrets)": {CI_WORKFLOW: good.replace("          GH_TOKEN: ${{ github.token }}\n", "          GH_TOKEN: ${{ github.token }}\n          ALL: ${{ toJSON(secrets) }}\n")},
        "another secret": {CI_WORKFLOW: good.replace("          GH_TOKEN: ${{ github.token }}\n", "          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}\n")},
        "an action by tag": {CI_WORKFLOW: good.replace("actions/checkout@d23441a48e516b6c34aea4fa41551a30e30af803", "actions/checkout@v6")},
        "no bash default": {CI_WORKFLOW: good.replace("defaults:\n  run:\n    shell: bash\n", "")},
        "a step's own shell": {CI_WORKFLOW: good.replace("      - name: Sign\n", "      - name: Sign\n        shell: sh\n")},
    }.items():
        if not workflow_problems(files):
            print(f"signing-anchors: self-test: {why} was NOT refused", file=sys.stderr)
            return 1
    print("signing-anchors: self-test passed: a planted development key root, each malformed anchor and family, a planted signer dependency and an indirect kernel path to the verifier are refused")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--scan", nargs="+", type=pathlib.Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    data = tomllib.loads(ANCHORS.read_text())
    has_ci = any(a.get("made") == "CI" for a in data.get("development", {}).values())
    problems = (
        problems_of(data)
        + (workflow_problems(workflow_files()) if has_ci else [])
        + family_problems(tomllib.loads(FAMILIES.read_text()))
        + dependency_problems(workspace_manifests())
        + closure_problems(*metadata_graph())
    )
    if args.scan:
        problems += scan(args.scan, development_keys(data))
    for p in problems:
        print(f"signing-anchors: {p}", file=sys.stderr)
    if problems:
        return 1
    releases = len(data.get("release", {}))
    developments = len(data.get("development", {}))
    scanned = f"; {len(args.scan)} path(s) scanned, no development key" if args.scan else ""
    families = len(tomllib.loads(FAMILIES.read_text()).get("family", {}))
    print(f"signing-anchors: {releases} release and {developments} development anchor(s) and {families} hardware families, valid{scanned}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
